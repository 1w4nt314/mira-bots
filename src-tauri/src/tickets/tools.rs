//! The app side of the agents' MCP tools (plan4 punkt 7): every tool frame from `mira-mcp`
//! ends here. This is the security boundary for what an agent can do to the tickets:
//!
//! 1. the frame's `agent_id` must be a live agent (unknown or exited → "Ukendt agent");
//! 2. the tool must be one of the five;
//! 3. the arguments are read again with the same limits (mira-mcp validated them, but the app
//!    does not rely on that), titles/notes made one-line, bodies cleaned;
//! 4. ownership: an agent can only submit its own ticket in progress;
//! 5. `mira_create_ticket` is rate-limited per agent (in memory; reset at app restart).
//!
//! All ticket changes go through [`TicketsCtx::mutate`]/`read`, so saving, agent links and
//! `tickets-changed`/`agents-changed` emits work exactly as for the UI. Locks: the manager lock
//! and the service lock are taken one after the other, never together; the rate-limit lock is
//! taken alone. Titles, bodies, summaries and notes are never logged.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::{json, Map, Value};

use super::model::{Ticket, TicketError};
use super::prompt::{clean_body, one_line};
use super::service::TicketService;
use super::{emit_json, TicketsCtx};
use crate::config::{
    AGENT_NOTE_MAX_CHARS, CREATE_TICKET_RATE_LIMIT, CREATE_TICKET_RATE_WINDOW_MS,
    NOT_SUBMITTED_TEXT, TURN_FAILED_TEXT,
};
use crate::events::AGENTS_CHANGED;
use crate::hooks::status::AgentStatus;
use crate::pipe::protocol::{ToolFrame, ToolResult};

pub const UNKNOWN_AGENT: &str = "Ukendt agent";
pub const NOTE_ERROR: &str = "note skal være en tekst på 1–120 tegn";
pub const UNKNOWN_FILTER: &str = "Ukendt filter";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Shared by every pipe connection (one `Arc` in the tool handler closure).
pub struct ToolsCtx {
    tickets: Arc<TicketsCtx>,
    /// Per agent: Unix ms of its successful creates inside the rate-limit window.
    created: Mutex<HashMap<String, VecDeque<u64>>>,
}

/// An optional string argument; `Err(type_error)` when present but not a string.
fn opt_str<'a>(
    args: &'a Map<String, Value>,
    key: &str,
    type_error: &str,
) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(type_error.to_string()),
    }
}

/// `{"id","shortId","title","state","skipReview"}` (C4.4).
fn created_json(t: &Ticket) -> Value {
    json!({
        "id": t.id,
        "shortId": t.short_id(),
        "title": t.title,
        "state": t.state,
        "skipReview": t.skip_review,
    })
}

impl ToolsCtx {
    pub fn new(tickets: Arc<TicketsCtx>) -> Self {
        ToolsCtx {
            tickets,
            created: Mutex::new(HashMap::new()),
        }
    }

    /// Answers one tool frame. Never panics; every error is Danish text for the model.
    pub fn handle_tool(&self, frame: ToolFrame, now: u64) -> ToolResult {
        let outcome = self.run(&frame, now);
        if let Err(e) = &outcome {
            log::debug!(
                "tool {} agent={:?} refused: {e}",
                frame.tool,
                frame.agent_id
            );
        }
        ToolResult {
            request_id: frame.request_id,
            outcome,
        }
    }

    fn run(&self, frame: &ToolFrame, now: u64) -> Result<Value, String> {
        let agent_id = self.live_agent(frame.agent_id.as_deref())?;
        let empty = Map::new();
        let args = match &frame.args {
            Value::Object(m) => m,
            Value::Null => &empty,
            _ => return Err("Argumenterne skal være et objekt".into()),
        };
        match frame.tool.as_str() {
            "mira_create_ticket" => self.create(&agent_id, args, now),
            "mira_list_tickets" => self.list(&agent_id, args),
            "mira_get_ticket" => self.get(args),
            "mira_submit_for_review" => self.submit(&agent_id, args, now),
            "mira_update_status" => self.update_status(&agent_id, args, now),
            other => Err(format!("Ukendt værktøj: {other}")),
        }
    }

    /// The agent id if it names an agent that has not exited (manager lock, briefly).
    fn live_agent(&self, agent_id: Option<&str>) -> Result<String, String> {
        let id = agent_id.filter(|s| !s.is_empty()).ok_or(UNKNOWN_AGENT)?;
        let live = lock(&self.tickets.manager)
            .get(id)
            .is_some_and(|a| !matches!(a.status, AgentStatus::Exited { .. }));
        if live {
            Ok(id.to_string())
        } else {
            Err(UNKNOWN_AGENT.into())
        }
    }

    /// Sets the agent's detail (no-op for the same text) and emits `agents-changed` after the
    /// manager lock is released. `only_if`: change only while the current detail passes.
    fn set_detail(
        &self,
        agent_id: &str,
        detail: Option<String>,
        only_if: impl FnOnce(Option<&str>) -> bool,
    ) {
        let list = {
            let mut m = lock(&self.tickets.manager);
            let current = m.get(agent_id).and_then(|a| a.detail);
            if current == detail || !only_if(current.as_deref()) {
                return;
            }
            if !m.set_detail(agent_id, detail) {
                return;
            }
            m.list()
        };
        emit_json(&self.tickets.emit, AGENTS_CHANGED, &list);
    }

    /// Creates within the window, after dropping expired entries.
    fn recent_creates(&self, agent_id: &str, now: u64) -> usize {
        let mut map = lock(&self.created);
        let q = map.entry(agent_id.to_string()).or_default();
        while q
            .front()
            .is_some_and(|t| now.saturating_sub(*t) >= CREATE_TICKET_RATE_WINDOW_MS)
        {
            q.pop_front();
        }
        q.len()
    }

    fn create(&self, agent_id: &str, args: &Map<String, Value>, now: u64) -> Result<Value, String> {
        let title = one_line(opt_str(args, "title", "Titel må ikke være tom")?.unwrap_or_default());
        let body =
            clean_body(opt_str(args, "body", "body skal være en tekst")?.unwrap_or_default());
        let skip_review = match args.get("skipReview") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err("skipReview skal være true eller false".into()),
        };
        if self.recent_creates(agent_id, now) >= CREATE_TICKET_RATE_LIMIT {
            return Err(TicketError::RateLimited.into());
        }
        let t = self
            .tickets
            .mutate(|s| s.create_by_agent(&title, body.trim(), skip_review, now))?;
        lock(&self.created)
            .entry(agent_id.to_string())
            .or_default()
            .push_back(now);
        log::info!(
            "agent {agent_id} created ticket {} (title {} chars, body {} chars)",
            t.short_id(),
            t.title.chars().count(),
            t.body.chars().count()
        );
        Ok(created_json(&t))
    }

    fn list(&self, agent_id: &str, args: &Map<String, Value>) -> Result<Value, String> {
        let filter = opt_str(args, "filter", UNKNOWN_FILTER)?
            .map(str::trim)
            .unwrap_or("mine");
        let tickets = match filter {
            "mine" => self.tickets.read(|s| s.list_for_agent(agent_id)),
            "backlog" => self.tickets.read(TicketService::backlog),
            "all" => self.tickets.read(TicketService::list),
            _ => return Err(UNKNOWN_FILTER.into()),
        };
        Ok(json!({"filter": filter, "tickets": tickets}))
    }

    fn get(&self, args: &Map<String, Value>) -> Result<Value, String> {
        let id = opt_str(args, "id", "id skal være en tekst på 1–64 tegn")?.unwrap_or_default();
        let t = self
            .tickets
            .read(|s| s.get_by_any_id(id))
            .ok_or_else(|| String::from(TicketError::NotFound))?;
        let mut v = serde_json::to_value(&t).map_err(|e| e.to_string())?;
        if let Value::Object(m) = &mut v {
            m.insert("shortId".into(), Value::String(t.short_id()));
        }
        Ok(v)
    }

    fn submit(&self, agent_id: &str, args: &Map<String, Value>, now: u64) -> Result<Value, String> {
        let summary_error = "summary skal være en tekst på 1–2000 tegn";
        let summary = opt_str(args, "summary", summary_error)?.unwrap_or_default();
        let ticket_id = opt_str(args, "ticketId", "ticketId skal være en tekst på 1–64 tegn")?
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let t = self
            .tickets
            .mutate(|s| s.submit_by_agent(agent_id, ticket_id, summary, now))?;
        // The queue may move on at the next idle.
        self.tickets.notify([agent_id]);
        self.set_detail(
            agent_id,
            None,
            |d| matches!(d, Some(d) if d == NOT_SUBMITTED_TEXT || d == TURN_FAILED_TEXT),
        );
        log::info!(
            "agent {agent_id} submitted ticket {} -> {} (summary {} chars)",
            t.short_id(),
            t.state.as_str(),
            t.summary.as_deref().map_or(0, |s| s.chars().count())
        );
        Ok(json!({
            "id": t.id,
            "shortId": t.short_id(),
            "state": t.state,
            "summary": t.summary,
        }))
    }

    fn update_status(
        &self,
        agent_id: &str,
        args: &Map<String, Value>,
        now: u64,
    ) -> Result<Value, String> {
        let note = one_line(opt_str(args, "note", NOTE_ERROR)?.unwrap_or_default());
        if note.is_empty() {
            return Err(NOTE_ERROR.into());
        }
        let note: String = note.chars().take(AGENT_NOTE_MAX_CHARS).collect();
        let note = note.trim_end().to_string();
        self.set_detail(agent_id, Some(note.clone()), |_| true);
        let t = self
            .tickets
            .mutate_if(|s| s.note_by_agent(agent_id, &note, now), Option::is_some)?;
        Ok(json!({"ok": true, "ticketId": t.map(|t| t.id)}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentManager;
    use crate::events::TICKETS_CHANGED;
    use crate::tickets::dispatcher::DispatchMsg;
    use crate::tickets::model::{TicketActor, TicketIssue, TicketSource, TicketState};
    use crate::tickets::test_support::{test_ctx, TestCtx};

    struct T {
        tc: TestCtx,
        tools: ToolsCtx,
        a: String,
        b: String,
    }

    fn setup() -> T {
        let mut m = AgentManager::new(5);
        let a = m.insert_fake("s-a", "/w/a");
        let b = m.insert_fake("s-b", "/w/b");
        let tc = test_ctx(Arc::new(Mutex::new(m)));
        let tools = ToolsCtx::new(Arc::clone(&tc.ctx));
        T { tc, tools, a, b }
    }

    impl T {
        fn call(
            &self,
            agent: Option<&str>,
            tool: &str,
            args: Value,
            now: u64,
        ) -> Result<Value, String> {
            let r = self.tools.handle_tool(
                ToolFrame {
                    agent_id: agent.map(str::to_string),
                    request_id: "req-1".into(),
                    tool: tool.into(),
                    args,
                },
                now,
            );
            assert_eq!(r.request_id, "req-1");
            r.outcome
        }

        /// A user ticket assigned to and in progress for `agent`.
        fn in_progress(&self, agent: &str, title: &str, skip: bool) -> Ticket {
            let c = &self.tc.ctx;
            let t = c.mutate(|s| s.create(title, "b", skip, 1)).unwrap();
            c.mutate(|s| s.assign(&t.id, agent, 2)).unwrap();
            c.mutate(|s| s.mark_dispatched(&t.id, agent, 3)).unwrap()
        }

        fn detail(&self, agent: &str) -> Option<String> {
            self.tc
                .ctx
                .manager
                .lock()
                .unwrap()
                .get(agent)
                .unwrap()
                .detail
        }

        fn ticket(&self, id: &str) -> Ticket {
            self.tc.ctx.read(|s| s.get(id)).unwrap()
        }
    }

    #[test]
    fn unknown_missing_or_exited_agent_is_refused() {
        let t = setup();
        for agent in [None, Some(""), Some("nope")] {
            assert_eq!(
                t.call(agent, "mira_list_tickets", json!({}), 1),
                Err(UNKNOWN_AGENT.into())
            );
        }
        t.tc.ctx.manager.lock().unwrap().stop(&t.a).unwrap();
        assert_eq!(
            t.call(Some(&t.a), "mira_create_ticket", json!({"title":"x"}), 1),
            Err(UNKNOWN_AGENT.into())
        );
        assert!(t.tc.ctx.read(TicketService::is_empty));
        // The agent check comes before the tool name.
        assert_eq!(
            t.call(Some("nope"), "mira_assign", json!({}), 1),
            Err(UNKNOWN_AGENT.into())
        );
        assert_eq!(
            t.call(Some(&t.b), "mira_assign", json!({}), 1),
            Err("Ukendt værktøj: mira_assign".into())
        );
    }

    #[test]
    fn create_puts_an_agent_ticket_in_the_backlog() {
        let t = setup();
        let r = t
            .call(
                Some(&t.a),
                "mira_create_ticket",
                json!({"title":" Følg\nop\u{0} på login ","body":"a\u{0}b\r\nc","skipReview":true}),
                10,
            )
            .unwrap();
        let id = r["id"].as_str().unwrap();
        let tk = t.ticket(id);
        assert_eq!(
            r,
            json!({"id":id,"shortId":tk.short_id(),"title":"Følg op på login","state":"backlog","skipReview":true})
        );
        assert_eq!(tk.source, TicketSource::Agent);
        assert_eq!(tk.state, TicketState::Backlog);
        assert_eq!(tk.assignee_agent_id, None);
        assert_eq!(tk.body, "ab\nc");
        assert_eq!(tk.history[0].by, TicketActor::Agent);
        let lists = t.tc.emitted(TICKETS_CHANGED);
        assert_eq!(lists.len(), 1);
        assert_eq!(lists[0][0]["source"], "agent");
    }

    #[test]
    fn create_validates_like_the_ui() {
        let t = setup();
        let err = |args: Value| {
            t.call(Some(&t.a), "mira_create_ticket", args, 1)
                .unwrap_err()
        };
        assert_eq!(err(json!({})), "Titel må ikke være tom");
        assert_eq!(err(json!({"title":"\n\t "})), "Titel må ikke være tom");
        assert_eq!(err(json!({"title":5})), "Titel må ikke være tom");
        assert_eq!(
            err(json!({"title":"x".repeat(201)})),
            "Titlen er for lang (maks 200 tegn)"
        );
        assert_eq!(
            err(json!({"title":"x","body":"y".repeat(20_001)})),
            "Teksten er for lang (maks 20000 tegn)"
        );
        assert_eq!(
            err(json!({"title":"x","skipReview":"ja"})),
            "skipReview skal være true eller false"
        );
        assert!(t.tc.ctx.read(TicketService::is_empty));
    }

    #[test]
    fn create_is_rate_limited_per_agent_in_a_rolling_hour() {
        let t = setup();
        let now = 1_000_000;
        for i in 0..CREATE_TICKET_RATE_LIMIT {
            t.call(
                Some(&t.a),
                "mira_create_ticket",
                json!({"title":format!("t{i}")}),
                now + i as u64,
            )
            .unwrap();
        }
        assert_eq!(
            t.call(
                Some(&t.a),
                "mira_create_ticket",
                json!({"title":"21"}),
                now + 100
            ),
            Err("For mange tickets oprettet den seneste time (maks 20)".into())
        );
        assert_eq!(t.tc.ctx.read(TicketService::len), 20);
        // Another agent has its own budget.
        assert!(t
            .call(
                Some(&t.b),
                "mira_create_ticket",
                json!({"title":"b"}),
                now + 100
            )
            .is_ok());
        // The window has passed for the first create only: room for exactly one more.
        assert!(t
            .call(
                Some(&t.a),
                "mira_create_ticket",
                json!({"title":"x"}),
                now + CREATE_TICKET_RATE_WINDOW_MS
            )
            .is_ok());
        assert!(t
            .call(
                Some(&t.a),
                "mira_create_ticket",
                json!({"title":"y"}),
                now + CREATE_TICKET_RATE_WINDOW_MS
            )
            .is_err());
        // Everything expired.
        assert!(t
            .call(
                Some(&t.a),
                "mira_create_ticket",
                json!({"title":"z"}),
                now + 3_600_001 + 100
            )
            .is_ok());
    }

    #[test]
    fn failed_creates_do_not_use_the_budget() {
        let t = setup();
        for _ in 0..30 {
            assert!(t
                .call(Some(&t.a), "mira_create_ticket", json!({"title":""}), 5)
                .is_err());
        }
        assert!(t
            .call(Some(&t.a), "mira_create_ticket", json!({"title":"ok"}), 5)
            .is_ok());
    }

    #[test]
    fn submit_without_a_ticket_in_progress() {
        let t = setup();
        assert_eq!(
            t.call(
                Some(&t.a),
                "mira_submit_for_review",
                json!({"summary":"done"}),
                5
            ),
            Err("Du har ingen ticket i gang".into())
        );
    }

    #[test]
    fn submit_of_someone_elses_ticket_is_refused() {
        let t = setup();
        let theirs = t.in_progress(&t.b, "theirs", false);
        assert_eq!(
            t.call(
                Some(&t.a),
                "mira_submit_for_review",
                json!({"summary":"done","ticketId":theirs.short_id()}),
                5
            ),
            Err("Ticketen er tildelt en anden agent".into())
        );
        assert_eq!(t.ticket(&theirs.id).state, TicketState::InProgress);
        assert_eq!(
            t.call(
                Some(&t.a),
                "mira_submit_for_review",
                json!({"summary":"done","ticketId":"ffffffff"}),
                5
            ),
            Err("Ticketen findes ikke".into())
        );
    }

    #[test]
    fn submit_moves_to_review_clears_issue_and_detail_and_wakes_the_queue() {
        let mut t = setup();
        let tk = t.in_progress(&t.a, "fix", false);
        t.tc.ctx.mutate(|s| s.mark_not_submitted(&t.a, 4)).unwrap();
        t.tc.ctx
            .manager
            .lock()
            .unwrap()
            .set_detail(&t.a, Some(NOT_SUBMITTED_TEXT.into()));
        t.tc.clear();
        t.tc.sent();

        let r = t
            .call(
                Some(&t.a),
                "mira_submit_for_review",
                json!({"summary":"Rettet login"}),
                5,
            )
            .unwrap();
        assert_eq!(
            r,
            json!({"id":tk.id,"shortId":tk.short_id(),"state":"review","summary":"Rettet login"})
        );
        let now = t.ticket(&tk.id);
        assert_eq!(now.state, TicketState::Review);
        assert_eq!(now.summary.as_deref(), Some("Rettet login"));
        assert_eq!(now.issue, None);
        assert_eq!(now.history.last().unwrap().by, TicketActor::Agent);
        assert_eq!(
            t.tc.sent(),
            vec![DispatchMsg::QueueChanged {
                agent_id: t.a.clone()
            }]
        );
        assert_eq!(t.detail(&t.a), None);
        assert_eq!(t.tc.emitted(TICKETS_CHANGED).len(), 1);
        assert!(!t.tc.emitted(AGENTS_CHANGED).is_empty());
    }

    #[test]
    fn submit_keeps_an_unrelated_detail_and_handles_skip_review() {
        let t = setup();
        let tk = t.in_progress(&t.a, "quick", true);
        t.tc.ctx
            .manager
            .lock()
            .unwrap()
            .set_detail(&t.a, Some("Kører tests".into()));
        let r = t
            .call(
                Some(&t.a),
                "mira_submit_for_review",
                json!({"summary":"ok","ticketId":tk.id}),
                5,
            )
            .unwrap();
        assert_eq!(r["state"], "done");
        assert_eq!(t.detail(&t.a).as_deref(), Some("Kører tests"));
    }

    #[test]
    fn submit_summary_limits() {
        let t = setup();
        t.in_progress(&t.a, "x", false);
        for bad in [
            json!({}),
            json!({"summary":"  "}),
            json!({"summary":"x".repeat(2_001)}),
            json!({"summary":7}),
        ] {
            assert_eq!(
                t.call(Some(&t.a), "mira_submit_for_review", bad, 5),
                Err("summary skal være en tekst på 1–2000 tegn".into())
            );
        }
        assert!(t
            .call(
                Some(&t.a),
                "mira_submit_for_review",
                json!({"summary":"x".repeat(2_000)}),
                5
            )
            .is_ok());
    }

    #[test]
    fn list_filters() {
        let t = setup();
        let mine = t.in_progress(&t.a, "mine-now", false);
        let queued =
            t.tc.ctx
                .mutate(|s| s.create("mine-next", "", false, 4))
                .unwrap();
        t.tc.ctx.mutate(|s| s.assign(&queued.id, &t.a, 5)).unwrap();
        t.in_progress(&t.b, "theirs", false);
        t.tc.ctx.mutate(|s| s.create("free", "", false, 6)).unwrap();

        let titles = |v: &Value| -> Vec<String> {
            v["tickets"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["title"].as_str().unwrap().to_string())
                .collect()
        };
        let r = t
            .call(Some(&t.a), "mira_list_tickets", json!({}), 7)
            .unwrap();
        assert_eq!(r["filter"], "mine");
        assert_eq!(titles(&r), vec!["mine-now", "mine-next"]);
        assert_eq!(r["tickets"][0]["id"], json!(mine.id));
        assert!(r["tickets"][0].get("body").is_none());
        assert!(r["tickets"][0].get("history").is_none());
        let r = t
            .call(
                Some(&t.a),
                "mira_list_tickets",
                json!({"filter":"backlog"}),
                7,
            )
            .unwrap();
        assert_eq!(titles(&r), vec!["free"]);
        let r = t
            .call(Some(&t.a), "mira_list_tickets", json!({"filter":"all"}), 7)
            .unwrap();
        assert_eq!(titles(&r).len(), 4);
        assert_eq!(
            t.call(
                Some(&t.a),
                "mira_list_tickets",
                json!({"filter":"others"}),
                7
            ),
            Err(UNKNOWN_FILTER.into())
        );
    }

    #[test]
    fn get_by_short_or_full_id() {
        let t = setup();
        let tk = t.in_progress(&t.b, "theirs", false);
        let r = t
            .call(
                Some(&t.a),
                "mira_get_ticket",
                json!({"id":tk.short_id()}),
                5,
            )
            .unwrap();
        assert_eq!(r["id"], json!(tk.id));
        assert_eq!(r["shortId"], json!(tk.short_id()));
        assert_eq!(r["body"], "b");
        assert_eq!(r["state"], "inProgress");
        assert!(r["history"].is_array());
        let r = t
            .call(Some(&t.a), "mira_get_ticket", json!({"id":tk.id}), 5)
            .unwrap();
        assert_eq!(r["id"], json!(tk.id));
        assert_eq!(
            t.call(Some(&t.a), "mira_get_ticket", json!({"id":"nope"}), 5),
            Err("Ticketen findes ikke".into())
        );
    }

    #[test]
    fn update_status_sets_detail_and_notes_the_ticket() {
        let t = setup();
        // Without a ticket: detail only.
        let r = t
            .call(
                Some(&t.a),
                "mira_update_status",
                json!({"note":"Læser koden"}),
                5,
            )
            .unwrap();
        assert_eq!(r, json!({"ok":true,"ticketId":null}));
        assert_eq!(t.detail(&t.a).as_deref(), Some("Læser koden"));
        assert!(t.tc.emitted(TICKETS_CHANGED).is_empty());
        assert_eq!(t.tc.emitted(AGENTS_CHANGED).len(), 1);

        let tk = t.in_progress(&t.a, "x", false);
        t.tc.clear();
        let long = format!("Kører\ntests {}", "ø".repeat(200));
        let r = t
            .call(Some(&t.a), "mira_update_status", json!({"note":long}), 6)
            .unwrap();
        assert_eq!(r, json!({"ok":true,"ticketId":tk.id}));
        let detail = t.detail(&t.a).unwrap();
        assert_eq!(detail.chars().count(), AGENT_NOTE_MAX_CHARS);
        assert!(detail.starts_with("Kører tests øø"));
        let last = t.ticket(&tk.id).history.last().cloned().unwrap();
        assert_eq!(
            (last.by, last.to),
            (TicketActor::Agent, TicketState::InProgress)
        );
        assert_eq!(last.note.as_deref(), Some(detail.as_str()));
        assert_eq!(t.tc.emitted(TICKETS_CHANGED).len(), 1);

        for bad in [json!({}), json!({"note":" \n "}), json!({"note":1})] {
            assert_eq!(
                t.call(Some(&t.a), "mira_update_status", bad, 7),
                Err(NOTE_ERROR.into())
            );
        }
    }

    #[test]
    fn args_must_be_an_object() {
        let t = setup();
        assert_eq!(
            t.call(Some(&t.a), "mira_list_tickets", json!([1]), 1),
            Err("Argumenterne skal være et objekt".into())
        );
    }

    #[test]
    fn not_submitted_issue_is_set_by_the_service_api() {
        // Sanity check for the fixture used above.
        let t = setup();
        let tk = t.in_progress(&t.a, "x", false);
        t.tc.ctx.mutate(|s| s.mark_not_submitted(&t.a, 4)).unwrap();
        assert_eq!(t.ticket(&tk.id).issue, Some(TicketIssue::NotSubmitted));
    }
}
