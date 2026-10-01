//! Step 3: tickets, per-agent queues, review and the dispatcher that types tickets into idle
//! agents' terminals.
//!
//! Dependency direction: `tickets` → `agent`, `hooks::status`, `events`, `config` (and the
//! tool-frame types of `pipe::protocol`, step 4). Nothing in `agent`, `pipe` or `hooks` knows
//! `tickets`: the app glue hands the pipe handler a closure over [`tools::ToolsCtx`].
//!
//! This file holds the app glue (plan B.8): [`TicketsCtx`] (shared service + manager links +
//! emits + the dispatcher's channel) and [`ManagerPort`] (the dispatcher's view of the agents).
//!
//! Locks: the service lock and the manager lock are never held at the same time (service first,
//! released, then manager), never across an `.await`, and never while emitting or sending.

pub mod dispatcher;
pub mod model;
pub mod prompt;
pub mod reports;
pub mod service;
pub mod state;
pub mod store;
pub mod tools;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;

use crate::agent::roles::Role;
use crate::agent::{now_ms, AgentManager};
use crate::config::{
    MAX_REVIEW_ROUNDS, NOT_SUBMITTED_TEXT, REPORT_BODY_MAX_CHARS, REPORT_TITLE_MAX_CHARS,
    TURN_FAILED_TEXT,
};
use crate::events::{EmitFn, AGENTS_CHANGED, TICKETS_CHANGED};
use crate::hooks::status::AgentStatus;
use dispatcher::{AgentPort, AgentSnapshot, DispatchMsg, TicketsHost};
use model::{
    ReportAuthor, Ticket, TicketActor, TicketError, TicketReport, TicketState, TicketSummary,
};
use prompt::{clean_body, one_line};
use reports::ReportStore;
use service::{ReviewCounts, TicketLinks, TicketService, REVIEWER_REMOVED_NOTE};
use store::JsonFileStore;

/// History note when an agent's process ended on its own.
pub const AGENT_EXITED_NOTE: &str = "agent afsluttet";
/// History note when the user stopped or removed the agent.
pub const AGENT_STOPPED_NOTE: &str = "agent stoppet";
/// History note when a turn ended without `mira_submit_for_review` (step 4).
pub const NOT_SUBMITTED_NOTE: &str = "turn afsluttet uden aflevering";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn emit_json<T: Serialize>(emit: &EmitFn, name: &str, payload: &T) {
    match serde_json::to_value(payload) {
        Ok(v) => emit(name, v),
        Err(e) => log::error!("serialize {name}: {e}"),
    }
}

/// Startup: loads `path` (a corrupt or unknown-version file is renamed to `.broken-<ts>` and
/// reported as the warning) and moves queued and in-progress tickets back to the backlog with
/// "app genstartet" — their agents did not survive the restart. Review/done stay as they are.
// TODO(windows-verify): after a restart, queued/in-progress tickets are in the backlog with
// "app genstartet"; done/review are untouched (plan D.35).
pub fn load_tickets(path: PathBuf, now: u64) -> (TicketService, Option<String>) {
    let (service, warning) =
        TicketService::load_and_recover(Box::new(JsonFileStore::new(path.clone())), now);
    log::info!("tickets: {} loaded from {}", service.len(), path.display());
    if let Some(w) = &warning {
        log::warn!("tickets: {w}");
    }
    (service, warning)
}

/// A report's title and body checked and cleaned for [`TicketsCtx::add_report`]: title one line,
/// 1–[`REPORT_TITLE_MAX_CHARS`]; body without controls, trimmed, 1–[`REPORT_BODY_MAX_CHARS`].
pub fn validate_report(title: &str, body: &str) -> Result<(String, String), String> {
    let title = one_line(title);
    if title.is_empty() {
        return Err("Titel må ikke være tom".into());
    }
    if title.chars().count() > REPORT_TITLE_MAX_CHARS {
        return Err(format!(
            "Titlen er for lang (maks {REPORT_TITLE_MAX_CHARS} tegn)"
        ));
    }
    let body = clean_body(body).trim().to_string();
    if body.is_empty() {
        return Err("Rapporten må ikke være tom".into());
    }
    if body.chars().count() > REPORT_BODY_MAX_CHARS {
        return Err(format!(
            "Rapporten er for lang (maks {REPORT_BODY_MAX_CHARS} tegn)"
        ));
    }
    Ok((title, body))
}

/// `get_report` result (C5.4 `ReportContent`).
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct ReportContent {
    pub report: TicketReport,
    pub body: String,
}

/// Shared ticket state of the app (in `AppState`, the pipe glue and the dispatcher).
pub struct TicketsCtx {
    pub service: Mutex<TicketService>,
    pub manager: Arc<Mutex<AgentManager>>,
    /// The dispatcher task's inbox.
    pub dispatch_tx: UnboundedSender<DispatchMsg>,
    pub emit: EmitFn,
    /// Report files under `<app_data>/tickets`.
    pub reports: ReportStore,
    /// Serialises report writes (sequence number → file → metadata). Taken before, never
    /// inside, the service lock.
    report_lock: Mutex<()>,
}

impl TicketsCtx {
    pub fn new(
        service: TicketService,
        manager: Arc<Mutex<AgentManager>>,
        dispatch_tx: UnboundedSender<DispatchMsg>,
        emit: EmitFn,
        reports_root: PathBuf,
    ) -> Self {
        TicketsCtx {
            service: Mutex::new(service),
            manager,
            dispatch_tx,
            emit,
            reports: ReportStore::new(reports_root),
            report_lock: Mutex::new(()),
        }
    }

    /// Runs `f` under the service lock (which also saves). On success: syncs every agent's
    /// ticket link, emits `tickets-changed` (full list without history) and, if a link changed,
    /// `agents-changed`. On error nothing is emitted. Does NOT notify the dispatcher; callers
    /// use [`Self::notify`] for the agents whose queue they touched.
    pub fn mutate<T>(
        &self,
        f: impl FnOnce(&mut TicketService) -> Result<T, TicketError>,
    ) -> Result<T, String> {
        self.mutate_if(f, |_| true)
    }

    /// `mutate`, but emits only when `changed(&result)` (a no-op mutation stays silent).
    fn mutate_if<T>(
        &self,
        f: impl FnOnce(&mut TicketService) -> Result<T, TicketError>,
        changed: impl FnOnce(&T) -> bool,
    ) -> Result<T, String> {
        let (result, list, links, reviews) = {
            let mut svc = lock(&self.service);
            let result = f(&mut svc).map_err(String::from)?;
            if !changed(&result) {
                return Ok(result);
            }
            (result, svc.list(), svc.links(), svc.open_review_counts())
        };
        let agents_changed = self.apply_links(&links, &reviews);
        emit_json(&self.emit, TICKETS_CHANGED, &list);
        if agents_changed {
            self.emit_agents();
        }
        Ok(result)
    }

    /// Read-only access under the service lock.
    pub fn read<T>(&self, f: impl FnOnce(&TicketService) -> T) -> T {
        f(&lock(&self.service))
    }

    /// Writes the links into the manager (agents without tickets get `None`/0, without reviews
    /// 0). Returns whether any agent changed. Takes only the manager lock.
    fn apply_links(&self, links: &TicketLinks, reviews: &ReviewCounts) -> bool {
        let mut m = lock(&self.manager);
        let mut changed = false;
        for id in m.ids() {
            let (current, len) = links.get(&id).cloned().unwrap_or_default();
            changed |= m.set_ticket_link(&id, current, len);
            changed |= m.set_review_link(&id, reviews.get(&id).copied().unwrap_or(0));
        }
        changed
    }

    /// Re-syncs the agents' ticket links and open review counts from the service (e.g. after a
    /// new agent appeared) and emits `agents-changed` if anything changed. Returns whether it did.
    pub fn sync_links(&self) -> bool {
        let (links, reviews) = self.read(|s| (s.links(), s.open_review_counts()));
        let changed = self.apply_links(&links, &reviews);
        if changed {
            self.emit_agents();
        }
        changed
    }

    fn emit_agents(&self) {
        let list = lock(&self.manager).list();
        emit_json(&self.emit, AGENTS_CHANGED, &list);
    }

    /// Sets the agent's detail (no-op for the same text) and emits `agents-changed` after the
    /// manager lock is released. `only_if`: change only while the current detail passes.
    pub fn set_agent_detail(
        &self,
        agent_id: &str,
        detail: Option<String>,
        only_if: impl FnOnce(Option<&str>) -> bool,
    ) {
        let list = {
            let mut m = lock(&self.manager);
            let current = m.get(agent_id).and_then(|a| a.detail);
            if current == detail || !only_if(current.as_deref()) {
                return;
            }
            if !m.set_detail(agent_id, detail) {
                return;
            }
            m.list()
        };
        emit_json(&self.emit, AGENTS_CHANGED, &list);
    }

    /// Clears the agent's detail when it is the "turn ended without submitting" or "turn failed"
    /// hint ([`NOT_SUBMITTED_TEXT`]/[`TURN_FAILED_TEXT`]): used when its ticket leaves in-progress
    /// (submit, manual move), so the hint does not outlive the ticket. Other texts stay.
    pub fn clear_stale_detail(&self, agent_id: &str) {
        self.set_agent_detail(
            agent_id,
            None,
            |d| matches!(d, Some(d) if d == NOT_SUBMITTED_TEXT || d == TURN_FAILED_TEXT),
        );
    }

    /// Review 5c W4: `from_agent`'s ticket in progress left it (handed to `to_name`, or back to
    /// the backlog with `None`). Its detail becomes "Ticket <short> givet videre" (whatever the
    /// actor; it replaces a stale "ikke afleveret"/"turn fejlede" hint and goes like the other
    /// hints: with the next status or delivery). `by_someone_else` (the user took it, not the
    /// agent itself): the dispatcher also types a "Du skal stoppe …" line once the agent is
    /// idle. Call before [`Self::notify`], so the line goes before the agent's next delivery.
    pub fn handed_over(
        &self,
        from_agent: &str,
        ticket: &Ticket,
        to_name: Option<&str>,
        by_someone_else: bool,
    ) {
        if from_agent.is_empty() {
            return;
        }
        let short = ticket.short_id();
        // Review 5c N8: an agent that handed the ticket on itself keeps its own status text; only a
        // hand-over by someone else replaces it (the agent has not been told yet).
        if by_someone_else {
            self.set_agent_detail(
                from_agent,
                Some(prompt::handed_over_detail(&short, to_name.is_some())),
                |_| true,
            );
            self.send(DispatchMsg::HandedOver {
                agent_id: from_agent.to_string(),
                ticket_id: ticket.id.clone(),
                to_name: to_name.map(str::to_string),
            });
        }
    }

    /// Tells the dispatcher that these agents' queues changed (each id once).
    pub fn notify<I, S>(&self, agent_ids: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let unique: BTreeSet<String> = agent_ids.into_iter().map(Into::into).collect();
        for agent_id in unique {
            self.send(DispatchMsg::QueueChanged { agent_id });
        }
    }

    /// Sends a message to the dispatcher. Fails only after the dispatcher task is gone
    /// (shutdown); that is logged, not returned.
    pub fn send(&self, msg: DispatchMsg) -> bool {
        match self.dispatch_tx.send(msg) {
            Ok(()) => true,
            // Not the message itself: a PromptSubmitted carries prompt text, which is never logged.
            Err(_) => {
                log::warn!("dispatcher is not running; message dropped");
                false
            }
        }
    }

    /// The agent stopped, exited or was removed: its assigned/inProgress/rejected tickets go to
    /// the backlog with `note`, and any delivery sequence for it is cancelled (`AgentGone`).
    // TODO(windows-verify): after Stop/Remove of an agent with a queue, all its tickets are in the
    // backlog with the note, no more input reaches its terminal and `queueLength` is 0 (plan D.37).
    ///
    /// As a reviewer it also loses its review assignments (the tickets stay in review with the
    /// note "reviewer <note>") and the reviews are routed again (plan5 A.6).
    // TODO(windows-verify): a reviewer stopped mid-review leaves the ticket in Review without a
    // reviewer, and it is routed to another reviewer if one exists (plan5 D.56).
    pub fn release_agent(&self, agent_id: &str, note: &str) -> Result<usize, String> {
        let now = now_ms();
        let released = self.mutate_if(|s| s.release_agent(agent_id, note, now), |v| !v.is_empty());
        self.send(DispatchMsg::AgentGone {
            agent_id: agent_id.to_string(),
        });
        let reviews = self.mutate_if(
            |s| s.release_reviewer(agent_id, note, now),
            |v| !v.is_empty(),
        );
        match &reviews {
            Ok(ids) if !ids.is_empty() => log::info!(
                "agent {agent_id}: {} review(s) without reviewer ({note})",
                ids.len()
            ),
            Ok(_) => {}
            Err(e) => log::warn!("agent {agent_id}: releasing its reviews failed: {e}"),
        }
        self.route_reviews();
        let released = released?;
        if !released.is_empty() {
            log::info!(
                "agent {agent_id}: {} ticket(s) back to the backlog ({note})",
                released.len()
            );
        }
        Ok(released.len())
    }

    // ---- review routing (plan5 A.6) ----

    /// Gives every ticket in review without a reviewer (and not escalated) to a reviewer, or
    /// escalates it when it reached [`MAX_REVIEW_ROUNDS`]. Candidates: live agents with the
    /// reviewer role other than the sender; the one with the fewest open reviews wins (tie: the
    /// oldest, then the id). No candidate: the ticket waits for the user as before. Idempotent;
    /// called after every way into review and whenever reviewers come or go. Returns the number
    /// of tickets routed.
    // TODO(windows-verify): a coder submits → the review file is in the reviewer's
    // .mira-bots\reviews\ and the line is typed when the reviewer is idle (plan5 D.54); three
    // rejections give "Eskaleret" and no new review line (plan5 D.55).
    pub fn route_reviews(&self) -> usize {
        let pending = self.read(TicketService::unrouted_reviews);
        if pending.is_empty() {
            return 0;
        }
        let reviewers = lock(&self.manager).reviewers();
        let mut routed = 0;
        for t in pending {
            let now = now_ms();
            if t.review_round >= MAX_REVIEW_ROUNDS {
                match self.mutate_if(|s| s.escalate(&t.id, now), Option::is_some) {
                    Ok(Some(_)) => log::info!(
                        "review: ticket {} escalated after {} rounds",
                        t.short_id(),
                        t.review_round
                    ),
                    Ok(None) => {}
                    Err(e) => log::warn!("review: escalating {} failed: {e}", t.short_id()),
                }
                continue;
            }
            let counts = self.read(TicketService::open_review_counts);
            let best = reviewers
                .iter()
                .filter(|r| t.assignee_agent_id.as_deref() != Some(r.id.as_str()))
                .min_by(|a, b| {
                    let load =
                        |r: &crate::agent::AgentInfo| counts.get(&r.id).copied().unwrap_or(0);
                    (load(a), a.created_at, &a.id).cmp(&(load(b), b.created_at, &b.id))
                });
            let Some(r) = best else {
                log::info!(
                    "review: no reviewer for ticket {}; waiting for the user",
                    t.short_id()
                );
                continue;
            };
            match self.mutate_if(
                |s| s.route_review(&t.id, &r.id, &r.name, now),
                Option::is_some,
            ) {
                Ok(Some(_)) => {
                    routed += 1;
                    log::info!("review: ticket {} -> reviewer {}", t.short_id(), r.id);
                    self.send(DispatchMsg::ReviewAssigned {
                        reviewer_agent_id: r.id.clone(),
                    });
                }
                Ok(None) => {}
                Err(e) => log::warn!("review: routing {} failed: {e}", t.short_id()),
            }
        }
        routed
    }

    /// `assign_reviewer` (C5.4): the ticket must be in review. `Some(agent)`: a live agent with
    /// the reviewer role other than the sender replaces any current reviewer (an escalation is
    /// cleared for this round; the round count is unchanged). `None`: the reviewer is removed
    /// ("reviewer fjernet") and the ticket is routed again, unless it reached
    /// [`MAX_REVIEW_ROUNDS`]: then it stays escalated (no new escalation note, review5 N5).
    pub fn assign_reviewer(
        &self,
        ticket_id: &str,
        agent_id: Option<&str>,
    ) -> Result<TicketSummary, String> {
        let t = self
            .read(|s| s.get(ticket_id))
            .ok_or(TicketError::NotFound)?;
        if t.state != TicketState::Review {
            return Err(TicketError::NotInReview.into());
        }
        let now = now_ms();
        match agent_id {
            Some(agent) => {
                let info = lock(&self.manager)
                    .get(agent)
                    .filter(|a| !matches!(a.status, AgentStatus::Exited { .. }))
                    .ok_or(TicketError::AgentNotLive)?;
                if !info.roles.contains(&Role::Reviewer) {
                    return Err(TicketError::NotAReviewer.into());
                }
                if t.assignee_agent_id.as_deref() == Some(agent) {
                    return Err(TicketError::SenderCannotReview.into());
                }
                let tk = self.mutate(|s| s.set_reviewer(&t.id, agent, &info.name, now))?;
                self.send(DispatchMsg::ReviewAssigned {
                    reviewer_agent_id: agent.to_string(),
                });
                Ok(TicketSummary::from(&tk))
            }
            None => {
                self.mutate_if(
                    |s| s.clear_reviewer(&t.id, REVIEWER_REMOVED_NOTE, TicketActor::User, now),
                    Option::is_some,
                )?;
                self.route_reviews();
                self.read(|s| s.get(&t.id))
                    .map(|tk| TicketSummary::from(&tk))
                    .ok_or_else(|| TicketError::NotFound.into())
            }
        }
    }

    // ---- reports (plan5 A.8) ----

    /// Adds a report to ticket `ticket_id` (full id): validates ([`validate_report`], ticket
    /// exists, at most 20), writes the file, then the metadata; a failed metadata save removes
    /// the file again. Ownership is the caller's business (tools: own ticket or review).
    // TODO(windows-verify): mira_add_report writes
    // %APPDATA%\dk.mira.bots\tickets\<id>\reports\01-<slug>.md (plan5 D.58).
    pub fn add_report(
        &self,
        ticket_id: &str,
        author: ReportAuthor,
        title: &str,
        body: &str,
    ) -> Result<TicketReport, String> {
        let (title, body) = validate_report(title, body)?;
        let _guard = lock(&self.report_lock);
        let seq = self.read(|s| s.next_report_seq(ticket_id))?;
        let (path, size) = self
            .reports
            .write(ticket_id, seq, &title, &body)
            .map_err(|e| format!("Kunne ikke gemme rapporten: {e}"))?;
        let now = now_ms();
        let report = TicketReport {
            id: format!("{seq:02}"),
            title,
            author,
            created_at: now,
            path: path.clone(),
            size,
        };
        if let Err(e) = self.mutate(|s| s.add_report_meta(ticket_id, report.clone(), now)) {
            if let Err(rm) = self.reports.remove(ticket_id, &path) {
                log::warn!("removing report file after a failed save: {rm}");
            }
            return Err(e);
        }
        log::info!(
            "report {} added to ticket {ticket_id} ({size} bytes)",
            report.id
        );
        Ok(report)
    }

    /// A report's metadata and text (`ticket_id`: full or short id).
    pub fn get_report(&self, ticket_id: &str, report_id: &str) -> Result<ReportContent, String> {
        let t: Ticket = self
            .read(|s| s.get_by_any_id(ticket_id))
            .ok_or(TicketError::NotFound)?;
        let report = t
            .reports
            .iter()
            .find(|r| r.id == report_id.trim())
            .cloned()
            .ok_or(TicketError::ReportNotFound)?;
        let body = self.reports.read(&t.id, &report.path).map_err(|e| {
            log::warn!("reading report {} of ticket {}: {e}", report.id, t.id);
            String::from(TicketError::ReportNotFound)
        })?;
        Ok(ReportContent { report, body })
    }

    /// Deletes the ticket (service rules) and then its report folder (a failure there is logged).
    pub fn delete_ticket(&self, id: &str) -> Result<(), String> {
        self.mutate(|s| s.delete(id))?;
        if let Err(e) = self.reports.remove_ticket_dir(id) {
            log::warn!("removing the report folder of ticket {id} failed: {e}");
        }
        Ok(())
    }
}

impl TicketsHost for Arc<TicketsCtx> {
    fn reroute_reviews(&self) {
        TicketsCtx::route_reviews(self);
    }

    fn mutate<T>(
        &self,
        f: impl FnOnce(&mut TicketService) -> Result<T, TicketError>,
    ) -> Result<T, String> {
        TicketsCtx::mutate(self, f)
    }

    fn read<T>(&self, f: impl FnOnce(&TicketService) -> T) -> T {
        TicketsCtx::read(self, f)
    }
}

/// The dispatcher's [`AgentPort`] over the real manager. Each call takes the manager lock
/// briefly; a detail change is announced with `agents-changed` (after the lock is released).
#[derive(Clone)]
pub struct ManagerPort {
    pub manager: Arc<Mutex<AgentManager>>,
    pub emit: EmitFn,
}

impl ManagerPort {
    pub fn new(manager: Arc<Mutex<AgentManager>>, emit: EmitFn) -> Self {
        ManagerPort { manager, emit }
    }
}

impl AgentPort for ManagerPort {
    fn snapshot(&self, id: &str) -> Option<AgentSnapshot> {
        let (a, last_user_input_at) = {
            let m = lock(&self.manager);
            (m.get(id)?, m.last_user_input_at(id))
        };
        Some(AgentSnapshot {
            name: a.name,
            cwd: a.cwd.into(),
            status: a.status,
            detail: a.detail,
            last_user_input_at,
            seat_kind: a.seat_kind,
            roles: a.roles,
        })
    }

    fn write_input(&self, id: &str, bytes: &[u8]) -> Result<(), String> {
        lock(&self.manager)
            .write_input(id, bytes)
            .map_err(|e| e.to_string())
    }

    fn set_detail(&self, id: &str, detail: Option<String>) -> bool {
        let list = {
            let mut m = lock(&self.manager);
            if !m.set_detail(id, detail) {
                return false;
            }
            m.list()
        };
        emit_json(&self.emit, AGENTS_CHANGED, &list);
        true
    }
}

/// Shared test fixture: a `TicketsCtx` over a `MemoryStore`, a collecting emit and the
/// dispatcher's receiving end.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::tickets::model::TicketDoc;
    use crate::tickets::store::MemoryStore;
    use serde_json::Value;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

    pub type Events = Arc<Mutex<Vec<(String, Value)>>>;

    pub struct TestCtx {
        pub ctx: Arc<TicketsCtx>,
        pub rx: UnboundedReceiver<DispatchMsg>,
        pub events: Events,
        pub store: MemoryStore,
    }

    pub fn test_ctx(manager: Arc<Mutex<AgentManager>>) -> TestCtx {
        let store = MemoryStore::new();
        let svc = TicketService::new(Box::new(store.clone()), TicketDoc::default());
        let (tx, rx) = unbounded_channel();
        let events: Events = Arc::default();
        let sink = Arc::clone(&events);
        let emit: EmitFn = Arc::new(move |n: &str, v: Value| {
            sink.lock().unwrap().push((n.to_string(), v));
        });
        let reports_root =
            std::env::temp_dir().join(format!("mira-tickets-{}", uuid::Uuid::new_v4()));
        TestCtx {
            ctx: Arc::new(TicketsCtx::new(svc, manager, tx, emit, reports_root)),
            rx,
            events,
            store,
        }
    }

    impl TestCtx {
        pub fn emitted(&self, name: &str) -> Vec<Value> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
                .collect()
        }

        pub fn clear(&self) {
            self.events.lock().unwrap().clear();
        }

        pub fn sent(&mut self) -> Vec<DispatchMsg> {
            let mut v = Vec::new();
            while let Ok(m) = self.rx.try_recv() {
                v.push(m);
            }
            v
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::hooks::status::AgentStatus;
    use crate::tickets::model::{TicketActor, TicketState};
    use serde_json::{json, Value};
    use std::sync::OnceLock;

    fn manager_with(n: usize) -> (Arc<Mutex<AgentManager>>, Vec<String>) {
        let mut m = AgentManager::new(5);
        let ids = (0..n)
            .map(|i| m.insert_fake(&format!("s{i}"), &format!("/w/a{i}")))
            .collect();
        (Arc::new(Mutex::new(m)), ids)
    }

    fn agent_json<'a>(list: &'a Value, id: &str) -> &'a Value {
        list.as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == id)
            .unwrap()
    }

    #[test]
    fn mutate_syncs_links_and_emits_only_when_needed() {
        let (m, ids) = manager_with(2);
        let mut t = test_ctx(Arc::clone(&m));
        let a = ids[0].clone();

        let tk = t.ctx.mutate(|s| s.create("Fix", "", false, 1)).unwrap();
        let lists = t.emitted(TICKETS_CHANGED);
        assert_eq!(lists.len(), 1);
        assert_eq!(lists[0][0]["title"], "Fix");
        assert!(lists[0][0].get("history").is_none(), "summaries only");
        assert_eq!(lists[0][0]["historyLen"], 1);
        assert!(t.emitted(AGENTS_CHANGED).is_empty(), "links unchanged");
        t.clear();

        t.ctx.mutate(|s| s.assign(&tk.id, &a, 2)).unwrap();
        assert_eq!(t.emitted(TICKETS_CHANGED).len(), 1);
        let agents = t.emitted(AGENTS_CHANGED);
        assert_eq!(agents.len(), 1);
        assert_eq!(agent_json(&agents[0], &a)["queueLength"], 1);
        assert_eq!(agent_json(&agents[0], &ids[1])["queueLength"], 0);
        assert_eq!(lock(&m).get(&a).unwrap().queue_length, 1);
        t.clear();

        t.ctx
            .mutate(|s| s.mark_dispatched(&tk.id, "a0", 3))
            .unwrap();
        let agents = t.emitted(AGENTS_CHANGED);
        assert_eq!(agent_json(&agents[0], &a)["currentTicketId"], json!(tk.id));
        assert_eq!(agent_json(&agents[0], &a)["queueLength"], 0);
        t.clear();

        // An error emits nothing and changes nothing.
        let err = t.ctx.mutate(|s| s.approve(&tk.id, 4)).unwrap_err();
        assert!(err.starts_with("Kan ikke flytte en ticket fra"), "{err}");
        assert!(t.events.lock().unwrap().is_empty());
        // mutate never talks to the dispatcher by itself.
        assert!(t.sent().is_empty());
        assert_eq!(t.store.saves(), 3);
    }

    #[test]
    fn emits_happen_without_any_lock_held() {
        let (m, ids) = manager_with(1);
        lock(&m)
            .set_status(&ids[0], AgentStatus::Idle, None)
            .unwrap();
        let slot: Arc<OnceLock<Arc<TicketsCtx>>> = Arc::default();
        let checks = Arc::new(Mutex::new(0usize));
        let (inner, n) = (Arc::clone(&slot), Arc::clone(&checks));
        let emit: EmitFn = Arc::new(move |_: &str, _: Value| {
            let ctx = inner.get().unwrap();
            assert!(
                ctx.service.try_lock().is_ok(),
                "service lock held while emitting"
            );
            assert!(
                ctx.manager.try_lock().is_ok(),
                "manager lock held while emitting"
            );
            *n.lock().unwrap() += 1;
        });
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let svc = TicketService::new(
            Box::new(store::MemoryStore::new()),
            model::TicketDoc::default(),
        );
        let ctx = Arc::new(TicketsCtx::new(
            svc,
            Arc::clone(&m),
            tx,
            Arc::clone(&emit),
            std::env::temp_dir().join("mira-unused-reports"),
        ));
        assert!(slot.set(Arc::clone(&ctx)).is_ok());
        let tk = ctx.mutate(|s| s.create("x", "", false, 1)).unwrap();
        ctx.mutate(|s| s.assign(&tk.id, &ids[0], 2)).unwrap();
        assert!(ManagerPort::new(Arc::clone(&m), emit).set_detail(&ids[0], None));
        // tickets; tickets + agents; agents (detail).
        assert_eq!(*checks.lock().unwrap(), 4);
    }

    #[test]
    fn release_agent_moves_tickets_to_backlog_and_cancels_delivery() {
        let (m, ids) = manager_with(2);
        let mut t = test_ctx(Arc::clone(&m));
        let a = ids[0].clone();
        let ids_t: Vec<String> = (0..3)
            .map(|i| {
                let tk = t
                    .ctx
                    .mutate(|s| s.create(&format!("t{i}"), "", false, 1))
                    .unwrap();
                t.ctx.mutate(|s| s.assign(&tk.id, &a, 2)).unwrap();
                tk.id
            })
            .collect();
        t.ctx
            .mutate(|s| s.mark_dispatched(&ids_t[0], "a0", 3))
            .unwrap();
        assert_eq!(
            lock(&m).get(&a).unwrap().current_ticket_id.as_deref(),
            Some(ids_t[0].as_str())
        );
        t.clear();

        assert_eq!(t.ctx.release_agent(&a, AGENT_STOPPED_NOTE), Ok(3));
        let info = lock(&m).get(&a).unwrap();
        assert_eq!((info.current_ticket_id, info.queue_length), (None, 0));
        for id in &ids_t {
            let tk = t.ctx.read(|s| s.get(id)).unwrap();
            assert_eq!(tk.state, TicketState::Backlog);
            assert_eq!(tk.assignee_agent_id, None);
            let last = tk.history.last().unwrap();
            assert_eq!(last.by, TicketActor::System);
            assert_eq!(last.note.as_deref(), Some(AGENT_STOPPED_NOTE));
        }
        assert_eq!(t.emitted(TICKETS_CHANGED).len(), 1);
        assert_eq!(t.emitted(AGENTS_CHANGED).len(), 1);
        assert_eq!(
            t.sent(),
            vec![DispatchMsg::AgentGone {
                agent_id: a.clone()
            }]
        );
        t.clear();

        // Nothing to release: no emit, but the delivery is still cancelled.
        assert_eq!(t.ctx.release_agent(&a, AGENT_EXITED_NOTE), Ok(0));
        assert!(t.events.lock().unwrap().is_empty());
        assert_eq!(t.sent().len(), 1);
    }

    #[test]
    fn notify_sends_one_queue_changed_per_agent() {
        let (m, _) = manager_with(0);
        let mut t = test_ctx(m);
        t.ctx.notify(["b", "a", "b"]);
        t.ctx.notify(None::<String>);
        assert_eq!(
            t.sent(),
            vec![
                DispatchMsg::QueueChanged {
                    agent_id: "a".into()
                },
                DispatchMsg::QueueChanged {
                    agent_id: "b".into()
                },
            ]
        );
    }

    #[test]
    fn sync_links_picks_up_new_agents() {
        let (m, ids) = manager_with(1);
        let t = test_ctx(Arc::clone(&m));
        let tk = t.ctx.mutate(|s| s.create("x", "", false, 1)).unwrap();
        t.ctx.mutate(|s| s.assign(&tk.id, &ids[0], 2)).unwrap();
        // Simulate a manager that lost the link (e.g. set from elsewhere).
        lock(&m).set_ticket_link(&ids[0], None, 0);
        t.clear();
        assert!(t.ctx.sync_links());
        assert_eq!(lock(&m).get(&ids[0]).unwrap().queue_length, 1);
        assert_eq!(t.emitted(AGENTS_CHANGED).len(), 1);
        assert!(!t.ctx.sync_links());
    }

    #[test]
    fn load_tickets_recovers_queues_after_a_restart() {
        use crate::tickets::model::TicketDoc;
        let dir = std::env::temp_dir().join(format!("mira-load-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(crate::config::TICKETS_FILE);
        // Build a document through a service, then save it as the app would have.
        let mem = store::MemoryStore::new();
        let mut svc = TicketService::new(Box::new(mem.clone()), TicketDoc::default());
        let mut make = |title: &str| svc.create(title, "", false, 1).unwrap().id;
        let (queued, running, review, done, backlog) =
            (make("q"), make("r"), make("rv"), make("d"), make("b"));
        svc.assign(&queued, "gone", 2).unwrap();
        svc.assign(&running, "gone", 2).unwrap();
        svc.mark_dispatched(&running, "gone", 3).unwrap();
        svc.assign(&review, "gone-2", 2).unwrap();
        svc.mark_dispatched(&review, "gone-2", 3).unwrap();
        svc.complete_turn("gone-2", 4).unwrap();
        svc.assign(&done, "gone-3", 2).unwrap();
        svc.mark_dispatched(&done, "gone-3", 3).unwrap();
        svc.complete_turn("gone-3", 4).unwrap();
        svc.approve(&done, 5).unwrap();
        std::fs::write(&path, serde_json::to_vec(&mem.doc().unwrap()).unwrap()).unwrap();

        let (loaded, warning) = load_tickets(path.clone(), 10);
        assert_eq!(warning, None);
        let state = |id: &str| loaded.get(id).unwrap();
        for id in [&queued, &running] {
            let t = state(id);
            assert_eq!(t.state, TicketState::Backlog);
            assert_eq!(t.assignee_agent_id, None);
            assert_eq!(
                t.history.last().unwrap().note.as_deref(),
                Some(crate::config::RESTART_NOTE)
            );
        }
        assert_eq!(state(&review).state, TicketState::Review);
        assert_eq!(state(&review).assignee_agent_id.as_deref(), Some("gone-2"));
        assert_eq!(state(&done).state, TicketState::Done);
        assert_eq!(state(&backlog).history.len(), 1);
        // The recovery was saved: a second start finds nothing to do.
        let on_disk: TicketDoc = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(on_disk
            .tickets
            .iter()
            .all(|t| !matches!(t.state, TicketState::Assigned | TicketState::InProgress)));
        assert!(links_empty(&loaded));

        // A corrupt file starts empty with a warning.
        std::fs::write(&path, "{nope").unwrap();
        let (empty, warning) = load_tickets(path, 11);
        assert!(empty.is_empty());
        assert!(warning.unwrap().contains("broken-"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn links_empty(s: &TicketService) -> bool {
        s.links().values().all(|(c, n)| c.is_none() && *n == 0)
    }

    #[test]
    fn manager_port_snapshot_and_detail_on_fake_agents() {
        let (m, ids) = manager_with(1);
        let events: Events = Arc::default();
        let sink = Arc::clone(&events);
        let port = ManagerPort::new(
            Arc::clone(&m),
            Arc::new(move |n: &str, v: Value| sink.lock().unwrap().push((n.into(), v))),
        );
        let a = &ids[0];
        lock(&m).set_status(a, AgentStatus::Idle, None).unwrap();
        let snap = port.snapshot(a).unwrap();
        assert_eq!(snap.name, "a0");
        assert_eq!(snap.cwd, PathBuf::from("/w/a0"));
        assert_eq!(snap.status, AgentStatus::Idle);
        assert_eq!(snap.detail, None);
        assert!(port.snapshot("nope").is_none());

        assert!(port.set_detail(a, Some("hint".into())));
        assert_eq!(port.snapshot(a).unwrap().detail.as_deref(), Some("hint"));
        {
            let ev = events.lock().unwrap();
            assert_eq!(ev.len(), 1);
            assert_eq!(ev[0].0, AGENTS_CHANGED);
            assert_eq!(ev[0].1[0]["detail"], "hint");
        }
        // A fake agent has no terminal.
        assert_eq!(
            port.write_input(a, b"x").unwrap_err(),
            "Agenten findes ikke"
        );
        lock(&m).stop(a).unwrap();
        assert!(!port.set_detail(a, Some("x".into())));
        assert!(!port.set_detail("nope", None));
        assert_eq!(
            events.lock().unwrap().len(),
            1,
            "no emit for refused details"
        );
    }

    /// The real manager with a PTY child (`sh`) behind the port, driven by the real dispatcher
    /// with fake timers: the ticket line and the separate Enter reach the terminal, and a
    /// matching UserPromptSubmit puts the ticket in progress and onto the agent.
    #[cfg(unix)]
    #[test]
    fn dispatcher_types_into_a_real_pty_through_the_manager_port() {
        use crate::agent::manager::AgentMeta;
        use crate::agent::pty::SpawnSpec;
        use crate::agent::{EventSink, SeatKind, SinkEvent};
        use crate::config::{PTY_COLS, PTY_ROWS};
        use crate::profiles::model::ProfileSnapshot;
        use crate::tickets::dispatcher::{Dispatcher, FakeTimers};
        use std::time::{Duration, Instant};

        let cwd = std::env::temp_dir().join(format!("mira-port-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&cwd).unwrap();
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let o = Arc::clone(&out);
        let sink: EventSink = Arc::new(move |ev| {
            if let SinkEvent::Output { bytes, .. } = ev {
                o.lock().unwrap().extend_from_slice(&bytes);
            }
        });
        let m = Arc::new(Mutex::new(AgentManager::new(5)));
        let info = lock(&m)
            .spawn_spec(
                SpawnSpec {
                    program: PathBuf::from("/bin/sh"),
                    args: vec!["-c".into(), "read line; echo got:$line; sleep 30".into()],
                    cwd: cwd.clone(),
                    env: vec![],
                    cols: PTY_COLS,
                    rows: PTY_ROWS,
                },
                AgentMeta {
                    id: uuid::Uuid::new_v4().to_string(),
                    session_id: "s".into(),
                    // A work role: the plain ticket line (review 5c W1).
                    profile: ProfileSnapshot {
                        roles: vec![Role::Coder],
                        ..ProfileSnapshot::default()
                    },
                    seat_kind: SeatKind::Work,
                },
                sink,
            )
            .unwrap();
        let a = info.id.clone();
        lock(&m).set_status(&a, AgentStatus::Idle, None).unwrap();

        let mut t = test_ctx(Arc::clone(&m));
        let port = ManagerPort::new(Arc::clone(&m), Arc::clone(&t.ctx.emit));
        let timers = FakeTimers::new();
        let mut d = Dispatcher::new(Arc::clone(&t.ctx), port, timers.clone());
        let tk = t
            .ctx
            .mutate(|s| s.create("Ret fejlen", "", false, 1))
            .unwrap();
        t.ctx.mutate(|s| s.assign(&tk.id, &a, 2)).unwrap();
        d.handle(DispatchMsg::QueueChanged {
            agent_id: a.clone(),
        });
        // The delay, then (scheduled while handling it) the separate Enter.
        for step in [750, 150] {
            for msg in timers.advance(step) {
                d.handle(msg);
            }
        }
        let short = tk.short_id();
        let wanted = format!("got:Ticket {short}: Ret fejlen.");
        let start = Instant::now();
        while !String::from_utf8_lossy(&out.lock().unwrap()).contains(&wanted) {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "terminal output: {:?}",
                String::from_utf8_lossy(&out.lock().unwrap())
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(cwd
            .join(".mira-bots")
            .join("tickets")
            .join(format!("{short}.md"))
            .is_file());

        t.clear();
        d.handle(DispatchMsg::PromptSubmitted {
            agent_id: a.clone(),
            prompt: Some(format!("Ticket {short}: Ret fejlen. Læs filen …")),
        });
        assert_eq!(
            t.ctx.read(|s| s.get(&tk.id)).unwrap().state,
            TicketState::InProgress
        );
        let agents = t.emitted(AGENTS_CHANGED);
        assert_eq!(agent_json(&agents[0], &a)["currentTicketId"], json!(tk.id));

        // Stop: the tickets go back and the dispatcher forgets the agent.
        lock(&m).stop(&a).unwrap();
        t.ctx.release_agent(&a, AGENT_STOPPED_NOTE).unwrap();
        for msg in t.sent() {
            d.handle(msg);
        }
        assert_eq!(
            t.ctx.read(|s| s.get(&tk.id)).unwrap().state,
            TicketState::Backlog
        );
        let pty = lock(&m).remove(&a).unwrap();
        drop(pty);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    // ---- step 5: review routing and reports ----

    use crate::agent::roles::Role;
    use crate::agent::SeatKind;

    /// Agents: a1 (coder), r1 and r2 (reviewers, r1 older).
    fn review_setup() -> (TestCtx, Arc<Mutex<AgentManager>>, String, String, String) {
        let mut m = AgentManager::new(5);
        let a1 = m.insert_fake_with("s-a1", "/w/a1", &[Role::Coder], SeatKind::Work);
        let r1 = m.insert_fake_with("s-r1", "/w/r1", &[Role::Reviewer], SeatKind::Staff);
        let r2 = m.insert_fake_with("s-r2", "/w/r2", &[Role::Reviewer], SeatKind::Staff);
        m.backdate(&r1, 1_000);
        let m = Arc::new(Mutex::new(m));
        (test_ctx(Arc::clone(&m)), m, a1, r1, r2)
    }

    /// A ticket submitted by `agent` (in review, unrouted).
    fn submitted(t: &TestCtx, agent: &str, title: &str) -> String {
        let c = &t.ctx;
        let tk = c.mutate(|s| s.create(title, "b", false, 1)).unwrap();
        c.mutate(|s| s.assign(&tk.id, agent, 2)).unwrap();
        c.mutate(|s| s.mark_dispatched(&tk.id, agent, 3)).unwrap();
        c.mutate(|s| s.submit_by_agent(agent, None, "klar", 4))
            .unwrap();
        tk.id
    }

    fn reviewer_of(t: &TestCtx, id: &str) -> Option<String> {
        t.ctx.read(|s| s.get(id)).unwrap().reviewer_agent_id
    }

    #[test]
    fn route_picks_reviewer_with_fewest_open_reviews() {
        let (mut t, _m, a1, r1, r2) = review_setup();
        let t1 = submitted(&t, &a1, "x");
        assert_eq!(t.ctx.route_reviews(), 1);
        assert_eq!(
            reviewer_of(&t, &t1).as_deref(),
            Some(r1.as_str()),
            "tie: oldest"
        );
        let t2 = submitted(&t, &a1, "y");
        let t3 = submitted(&t, &a1, "z");
        assert_eq!(t.ctx.route_reviews(), 2);
        assert_eq!(reviewer_of(&t, &t2).as_deref(), Some(r2.as_str()));
        assert_eq!(reviewer_of(&t, &t3).as_deref(), Some(r1.as_str()));
        assert_eq!(t.ctx.route_reviews(), 0, "idempotent");
        let sent = t.sent();
        assert_eq!(
            sent.iter()
                .filter(|m| matches!(m, DispatchMsg::ReviewAssigned { .. }))
                .count(),
            3
        );
        assert!(sent.contains(&DispatchMsg::ReviewAssigned {
            reviewer_agent_id: r2.clone()
        }));
        let hist = t.ctx.read(|s| s.get(&t1)).unwrap().history;
        assert_eq!(
            hist.last().unwrap().note.as_deref(),
            Some("review tildelt r1")
        );
    }

    #[test]
    fn route_never_picks_the_sender() {
        let (t, m, a1, r1, _r2) = review_setup();
        // r2 gone; r1 submits its own work: no other reviewer → waits.
        let _ = a1;
        let r2 = lock(&m).reviewers()[1].id.clone();
        lock(&m).stop(&r2).unwrap();
        let mine = submitted(&t, &r1, "egen");
        assert_eq!(t.ctx.route_reviews(), 0);
        assert_eq!(reviewer_of(&t, &mine), None);
    }

    #[test]
    fn route_with_no_reviewer_leaves_ticket_for_user() {
        let (m, ids) = manager_with(1);
        let t = test_ctx(m);
        let id = submitted(&t, &ids[0], "x");
        assert_eq!(t.ctx.route_reviews(), 0);
        let tk = t.ctx.read(|s| s.get(&id)).unwrap();
        assert_eq!(
            (tk.state, tk.reviewer_agent_id, tk.escalated),
            (TicketState::Review, None, false)
        );
        // The user can still approve as before.
        t.ctx.mutate(|s| s.approve(&id, 9)).unwrap();
    }

    #[test]
    fn route_escalates_at_three_rounds() {
        let (mut t, _m, a1, r1, _r2) = review_setup();
        let id = submitted(&t, &a1, "x");
        for round in 0..MAX_REVIEW_ROUNDS {
            assert_eq!(t.ctx.route_reviews(), 1, "round {round}");
            let rev = reviewer_of(&t, &id).unwrap();
            t.ctx
                .mutate(|s| s.reject_by_agent(&rev, "r", &id, "mere", true, 10))
                .unwrap();
            t.ctx.mutate(|s| s.mark_dispatched(&id, "a1", 11)).unwrap();
            t.ctx
                .mutate(|s| s.submit_by_agent(&a1, None, "igen", 12))
                .unwrap();
        }
        t.sent();
        assert_eq!(t.ctx.route_reviews(), 0);
        let tk = t.ctx.read(|s| s.get(&id)).unwrap();
        assert_eq!(
            (tk.review_round, tk.escalated, tk.reviewer_agent_id),
            (3, true, None)
        );
        assert_eq!(
            tk.history.last().unwrap().note.as_deref(),
            Some("eskaleret efter 3 runder")
        );
        assert!(
            t.sent().is_empty(),
            "no review line for an escalated ticket"
        );
        // The user can pick a reviewer by hand.
        let s = t.ctx.assign_reviewer(&id, Some(&r1)).unwrap();
        assert_eq!(
            (s.escalated, s.reviewer_agent_id.as_deref()),
            (false, Some(r1.as_str()))
        );
        assert_eq!(
            t.sent(),
            vec![DispatchMsg::ReviewAssigned {
                reviewer_agent_id: r1.clone()
            }]
        );
        // "Fjern reviewer" (review5 N5): escalated again at once, without routing and without a
        // second escalation note; the round count stays.
        let escalations = |t: &TestCtx| {
            t.ctx
                .read(|s| s.get(&id))
                .unwrap()
                .history
                .iter()
                .filter(|h| h.note.as_deref() == Some("eskaleret efter 3 runder"))
                .count()
        };
        assert_eq!(escalations(&t), 1);
        let s = t.ctx.assign_reviewer(&id, None).unwrap();
        assert_eq!(
            (s.escalated, s.reviewer_agent_id.as_deref(), s.review_round),
            (true, None, 3)
        );
        assert_eq!(escalations(&t), 1);
        assert!(t.sent().is_empty(), "no review line");
        let tk = t.ctx.read(|s| s.get(&id)).unwrap();
        assert_eq!(
            tk.history.last().unwrap().note.as_deref(),
            Some("reviewer fjernet")
        );
        assert_eq!(t.ctx.route_reviews(), 0);
    }

    #[test]
    fn assign_reviewer_rules() {
        let (t, m, a1, r1, r2) = review_setup();
        let id = submitted(&t, &a1, "x");
        assert_eq!(
            t.ctx.assign_reviewer(&id, Some(&a1)),
            Err("Agenten er ikke reviewer".into())
        );
        assert_eq!(
            t.ctx.assign_reviewer(&id, Some("nope")),
            Err("Agenten kører ikke".into())
        );
        let other = submitted(&t, &r1, "egen");
        assert_eq!(
            t.ctx.assign_reviewer(&other, Some(&r1)),
            Err("Afsenderen kan ikke reviewe sin egen ticket".into())
        );
        let backlog = t.ctx.mutate(|s| s.create("b", "", false, 1)).unwrap();
        assert_eq!(
            t.ctx.assign_reviewer(&backlog.id, Some(&r1)),
            Err("Ticketen er ikke i review".into())
        );
        t.ctx.assign_reviewer(&id, Some(&r2)).unwrap();
        assert_eq!(reviewer_of(&t, &id).as_deref(), Some(r2.as_str()));
        // None: removed and routed again (r1 has the fewest open reviews).
        let s = t.ctx.assign_reviewer(&id, None).unwrap();
        assert_eq!(s.reviewer_agent_id.as_deref(), Some(r1.as_str()));
        let notes: Vec<_> = t
            .ctx
            .read(|s| s.get(&id))
            .unwrap()
            .history
            .into_iter()
            .filter_map(|h| h.note)
            .collect();
        assert!(notes.contains(&"reviewer fjernet".to_string()));
        drop(m);
    }

    #[test]
    fn reviewer_exit_releases_assignments_and_reroutes() {
        let (mut t, m, a1, r1, r2) = review_setup();
        let id = submitted(&t, &a1, "x");
        t.ctx.route_reviews();
        assert_eq!(reviewer_of(&t, &id).as_deref(), Some(r1.as_str()));
        t.sent();
        lock(&m).stop(&r1).unwrap();
        t.ctx.release_agent(&r1, AGENT_STOPPED_NOTE).unwrap();
        assert_eq!(reviewer_of(&t, &id).as_deref(), Some(r2.as_str()));
        let notes: Vec<_> = t
            .ctx
            .read(|s| s.get(&id))
            .unwrap()
            .history
            .into_iter()
            .filter_map(|h| h.note)
            .collect();
        assert!(
            notes.contains(&"reviewer agent stoppet".to_string()),
            "{notes:?}"
        );
        let sent = t.sent();
        assert!(sent.contains(&DispatchMsg::AgentGone {
            agent_id: r1.clone()
        }));
        assert!(sent.contains(&DispatchMsg::ReviewAssigned {
            reviewer_agent_id: r2.clone()
        }));
        // The last reviewer goes too: the ticket stays in review for the user.
        lock(&m).stop(&r2).unwrap();
        t.ctx.release_agent(&r2, AGENT_EXITED_NOTE).unwrap();
        let tk = t.ctx.read(|s| s.get(&id)).unwrap();
        assert_eq!(
            (tk.state, tk.reviewer_agent_id),
            (TicketState::Review, None)
        );
    }

    #[test]
    fn open_reviews_link_is_synced() {
        let (t, m, a1, r1, _r2) = review_setup();
        submitted(&t, &a1, "x");
        t.clear();
        t.ctx.route_reviews();
        assert_eq!(lock(&m).get(&r1).unwrap().open_reviews, 1);
        let agents = t.emitted(AGENTS_CHANGED);
        assert_eq!(agents.len(), 1);
        assert_eq!(agent_json(&agents[0], &r1)["openReviews"], 1);
        let id = t.ctx.read(|s| s.review_assignments())[0].ticket_id.clone();
        t.ctx.mutate(|s| s.approve(&id, 20)).unwrap();
        assert_eq!(lock(&m).get(&r1).unwrap().open_reviews, 0);
    }

    #[test]
    fn add_report_writes_file_and_metadata() {
        let (m, ids) = manager_with(1);
        let t = test_ctx(m);
        let tk = t.ctx.mutate(|s| s.create("x", "", false, 1)).unwrap();
        t.clear();
        let r = t
            .ctx
            .add_report(
                &tk.id,
                ReportAuthor::agent(&ids[0]),
                " Første\nrapport ",
                "# Hej\r\næøå\u{0}\n",
            )
            .unwrap();
        assert_eq!((r.id.as_str(), r.title.as_str()), ("01", "Første rapport"));
        assert_eq!(r.path, "reports/01-foerste-rapport.md");
        let file = t
            .ctx
            .reports
            .dir_for(&tk.id)
            .unwrap()
            .join("01-foerste-rapport.md");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "# Hej\næøå");
        assert_eq!(r.size, "# Hej\næøå".len() as u64);
        let lists = t.emitted(TICKETS_CHANGED);
        assert_eq!(lists.len(), 1);
        assert_eq!(lists[0][0]["reportCount"], 1);
        let got = t.ctx.get_report(&tk.short_id(), "01").unwrap();
        assert_eq!((got.report, got.body.as_str()), (r.clone(), "# Hej\næøå"));
        let v = serde_json::to_value(t.ctx.get_report(&tk.id, "01").unwrap()).unwrap();
        assert_eq!(v["body"], "# Hej\næøå");
        assert_eq!(v["report"]["id"], "01");
        let r2 = t
            .ctx
            .add_report(&tk.id, ReportAuthor::user(), "To", "b")
            .unwrap();
        assert_eq!(r2.id, "02");
        assert_eq!(
            t.ctx.get_report(&tk.id, "09").unwrap_err(),
            "Rapporten findes ikke"
        );
        let _ = std::fs::remove_dir_all(t.ctx.reports.root());
    }

    #[test]
    fn report_limits() {
        let (m, _) = manager_with(0);
        let t = test_ctx(m);
        let tk = t.ctx.mutate(|s| s.create("x", "", false, 1)).unwrap();
        let add =
            |title: &str, body: &str| t.ctx.add_report(&tk.id, ReportAuthor::user(), title, body);
        assert_eq!(add(" \n ", "b").unwrap_err(), "Titel må ikke være tom");
        assert_eq!(
            add(&"t".repeat(121), "b").unwrap_err(),
            "Titlen er for lang (maks 120 tegn)"
        );
        assert_eq!(add("t", " \n").unwrap_err(), "Rapporten må ikke være tom");
        assert_eq!(
            add("t", &"b".repeat(20_001)).unwrap_err(),
            "Rapporten er for lang (maks 20000 tegn)"
        );
        assert_eq!(
            t.ctx
                .add_report("nope", ReportAuthor::user(), "t", "b")
                .unwrap_err(),
            "Ticketen findes ikke"
        );
        assert!(add(&"t".repeat(120), &"b".repeat(20_000)).is_ok());
        for _ in 1..20 {
            add("t", "b").unwrap();
        }
        assert_eq!(
            add("t", "b").unwrap_err(),
            "Ticketen har allerede 20 rapporter"
        );
        let dir = t.ctx.reports.dir_for(&tk.id).unwrap();
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 20);
        let _ = std::fs::remove_dir_all(t.ctx.reports.root());
    }

    #[test]
    fn get_report_refuses_path_traversal() {
        let (m, _) = manager_with(0);
        let t = test_ctx(m);
        let tk = t.ctx.mutate(|s| s.create("x", "", false, 1)).unwrap();
        let r = t
            .ctx
            .add_report(&tk.id, ReportAuthor::user(), "t", "b")
            .unwrap();
        // A tampered path in tickets.json is never followed.
        let mut bad = r.clone();
        bad.id = "02".into();
        bad.path = "reports/../../../secret.txt".into();
        t.ctx.mutate(|s| s.add_report_meta(&tk.id, bad, 2)).unwrap();
        assert_eq!(
            t.ctx.get_report(&tk.id, "02").unwrap_err(),
            "Rapporten findes ikke"
        );
        assert!(t.ctx.get_report(&tk.id, "01").is_ok());
        let _ = std::fs::remove_dir_all(t.ctx.reports.root());
    }

    #[test]
    fn delete_ticket_removes_report_dir() {
        let (m, _) = manager_with(0);
        let t = test_ctx(m);
        let tk = t.ctx.mutate(|s| s.create("x", "", false, 1)).unwrap();
        t.ctx
            .add_report(&tk.id, ReportAuthor::user(), "t", "b")
            .unwrap();
        let dir = t.ctx.reports.root().join(&tk.id);
        assert!(dir.is_dir());
        t.ctx.delete_ticket(&tk.id).unwrap();
        assert!(!dir.exists());
        assert!(t.ctx.read(|s| s.get(&tk.id)).is_none());
        // A ticket without reports deletes fine too.
        let tk2 = t.ctx.mutate(|s| s.create("y", "", false, 1)).unwrap();
        t.ctx.delete_ticket(&tk2.id).unwrap();
        let _ = std::fs::remove_dir_all(t.ctx.reports.root());
    }
}
