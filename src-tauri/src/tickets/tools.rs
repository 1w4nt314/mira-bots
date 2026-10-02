//! The app side of the agents' MCP tools (plan4 punkt 7, plan5 punkt 13): every tool frame from
//! `mira-mcp` ends here. This is the security boundary for what an agent can do:
//!
//! 1. the frame's `agent_id` must be a live agent (unknown or exited → "Ukendt agent");
//! 2. the tool must be one of the seventeen ([`mira_mcp::tools::TOOL_NAMES`]);
//! 3. the agent's roles (fixed at spawn, from the manager) must allow it
//!    ([`mira_mcp::tools::is_allowed`], the one role matrix) → "Din rolle tillader ikke dette
//!    værktøj", whatever mira-mcp showed;
//! 4. the arguments are read again with the same limits (mira-mcp validated them, but the app
//!    does not rely on that), titles/notes made one-line, bodies cleaned;
//! 5. ownership: an agent submits and reports only on its own ticket (or, as reviewer, on the
//!    review it was given); a reviewer approves/rejects only tickets in review it was assigned,
//!    never its own submission; a ticket in progress is handed on (`mira_handoff_ticket`, or
//!    the coordinator's `mira_assign_ticket`/`mira_unassign_ticket`) only by its assignee;
//! 6. `mira_create_ticket` is rate-limited per agent (in memory; reset at app restart);
//! 7. `mira_spawn_agent` goes through the same spawn path and seat limits as the UI
//!    ([`SpawnPort`]);
//! 8. projects (step 4b): a work agent only gets tickets of its own project
//!    ([`crate::projects::assignment_target`]), and an agent creates a project
//!    (`{"new": …}`) only when the workspace file allows it (`agentsMayCreateProjects`).
//!
//! All ticket changes go through [`TicketsCtx::mutate`]/`read`, so saving, agent links and
//! `tickets-changed`/`agents-changed` emits work exactly as for the UI. Locks: the manager lock,
//! the service lock and the profile lock are taken one after the other, never together; the
//! rate-limit lock is taken alone. Titles, bodies, summaries, notes and report texts are never
//! logged.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use mira_mcp::tools as mcp_tools;
use serde_json::{json, Map, Value};

use super::model::{ReportAuthor, Ticket, TicketError, TicketReport, TicketState};
use super::prompt::{clean_body, one_line};
use super::service::TicketService;
use super::{validate_report, TicketsCtx};
use crate::agent::roles::{wire_names, Role};
use crate::agent::{AgentInfo, SeatKind};
use crate::config::{
    AGENT_NOTE_MAX_CHARS, CREATE_TICKET_RATE_LIMIT, CREATE_TICKET_RATE_WINDOW_MS,
    REPORT_ON_SUBMIT_TITLE, REVIEW_NOTE_MAX_CHARS,
};
use crate::hooks::status::AgentStatus;
use crate::pipe::protocol::{ToolFrame, ToolResult};
use crate::profiles::ProfilesCtx;
use crate::projects::{self, AssignmentProject, ProjectError, ProjectId, ProjectRef};

pub const UNKNOWN_AGENT: &str = "Ukendt agent";
pub const NOTE_ERROR: &str = "note skal være en tekst på 1–120 tegn";
pub const UNKNOWN_FILTER: &str = "Ukendt filter";
/// The agent's roles do not allow the tool (plan5 C5.5; same text as mira-mcp's).
pub const ROLE_DENIED: &str = mcp_tools::ROLE_DENIED;
/// `assignTo` on `mira_create_ticket` without the coordinator role.
pub const ONLY_COORDINATOR_ASSIGNS: &str = "Kun koordinator-rollen må tildele";
/// No [`SpawnPort`] (tests, or the app is still starting).
pub const SPAWN_UNAVAILABLE: &str = "Start af agenter er ikke tilgængelig";
/// `mira_list_tickets` shows at most this many characters of each ticket's summary (then "…");
/// `mira_get_ticket` has the full text. Keeps a list of any realistic length far below
/// mira-mcp's `MAX_REPLY` (N1); the UI's `tickets-changed` still carries the full summary.
pub const LIST_SUMMARY_MAX_CHARS: usize = 160;

/// `mira_spawn_agent`'s request to the app's spawn path (`commands::spawn_for_tool`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnByProfile {
    pub profile_id: String,
    /// `None`: the profile's `defaultSeat`.
    pub seat_kind: Option<SeatKind>,
    /// A backlog ticket (full or short id) the new agent starts with.
    pub first_ticket_id: Option<String>,
    /// The project of a work seat (the ticket's project wins; plan4b punkt 9).
    pub project: Option<ProjectRef>,
}

/// Starts an agent like the UI does (same checks and seat limits); set in `setup` as a closure
/// over the `AppHandle`. Errors are the UI's Danish texts.
pub type SpawnPort = Arc<dyn Fn(SpawnByProfile) -> Result<AgentInfo, String> + Send + Sync>;

/// `s` cut to `max` characters with a trailing "…" (unchanged when it fits).
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Shared by every pipe connection (one `Arc` in the tool handler closure).
pub struct ToolsCtx {
    tickets: Arc<TicketsCtx>,
    profiles: Arc<ProfilesCtx>,
    spawn: Mutex<Option<SpawnPort>>,
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

/// An optional id argument, trimmed; blank counts as absent.
fn opt_id<'a>(args: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, String> {
    Ok(
        opt_str(args, key, &format!("{key} skal være en tekst på 1–64 tegn"))?
            .map(str::trim)
            .filter(|s| !s.is_empty()),
    )
}

/// A required id argument, trimmed.
fn req_id<'a>(args: &'a Map<String, Value>, key: &str) -> Result<&'a str, String> {
    opt_id(args, key)?.ok_or_else(|| format!("{key} skal være en tekst på 1–64 tegn"))
}

/// `{"id","shortId","title","state","skipReview","project"}` (C4.4 + step 4b).
fn created_json(t: &Ticket) -> Value {
    json!({
        "id": t.id,
        "shortId": t.short_id(),
        "title": t.title,
        "state": t.state,
        "skipReview": t.skip_review,
        "project": t.project,
    })
}

/// The `project` argument (same forms as mira-mcp accepts: an id or `{"new": "<name>"}`;
/// `null` = absent), checked against the folder-name rules.
pub fn parse_project(args: &Map<String, Value>) -> Result<Option<ProjectRef>, String> {
    let name = |v: &Value| -> Result<String, String> {
        let s = v.as_str().ok_or(mcp_tools::PROJECT_ERROR)?.trim();
        if s.is_empty() {
            return Err(mcp_tools::PROJECT_ERROR.into());
        }
        Ok(projects::validate_project_id(s)?)
    };
    match args.get("project") {
        None | Some(Value::Null) => Ok(None),
        Some(v @ Value::String(_)) => Ok(Some(ProjectRef::Existing(name(v)?))),
        Some(Value::Object(m)) if m.len() == 1 => {
            let new = m.get("new").ok_or(mcp_tools::PROJECT_ERROR)?;
            Ok(Some(ProjectRef::New { new: name(new)? }))
        }
        Some(_) => Err(mcp_tools::PROJECT_ERROR.into()),
    }
}

/// `project` on `mira_assign_ticket`/`mira_handoff_ticket` for a ticket that already has one.
pub fn ticket_has_project(p: &ProjectRef) -> String {
    format!("Ticketen har allerede projekt «{}»", p.name())
}

/// `project` on `mira_handoff_ticket` without `agentId` (back to the backlog).
pub const PROJECT_NEEDS_TARGET: &str =
    "project bruges kun sammen med agentId (ticketen gives videre til en agent)";

/// The status kind as a plain string (`"idle"`, `"exited"`, …).
fn status_kind(s: &AgentStatus) -> Value {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.get("kind").cloned())
        .unwrap_or(Value::Null)
}

/// A review note: one line, at most [`REVIEW_NOTE_MAX_CHARS`].
fn review_note(args: &Map<String, Value>) -> Result<String, String> {
    let note = one_line(opt_str(args, "note", "note skal være en tekst")?.unwrap_or_default());
    if note.chars().count() > REVIEW_NOTE_MAX_CHARS {
        return Err(format!("note må højst være {REVIEW_NOTE_MAX_CHARS} tegn"));
    }
    Ok(note)
}

impl ToolsCtx {
    pub fn new(tickets: Arc<TicketsCtx>, profiles: Arc<ProfilesCtx>) -> Self {
        ToolsCtx {
            tickets,
            profiles,
            spawn: Mutex::new(None),
            created: Mutex::new(HashMap::new()),
        }
    }

    /// Installs the spawn path used by `mira_spawn_agent`.
    pub fn set_spawn_port(&self, port: SpawnPort) {
        *lock(&self.spawn) = Some(port);
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
        let agent = self.live_agent(frame.agent_id.as_deref())?;
        let empty = Map::new();
        let args = match &frame.args {
            Value::Object(m) => m,
            Value::Null => &empty,
            _ => return Err("Argumenterne skal være et objekt".into()),
        };
        let tool = frame.tool.as_str();
        if !mcp_tools::is_known(tool) {
            return Err(format!("Ukendt værktøj: {tool}"));
        }
        // The security boundary for role-bound tools (plan5 A.2): the roles the agent was
        // spawned with, whatever mira-mcp listed.
        if !mcp_tools::is_allowed(tool, &wire_names(&agent.roles)) {
            log::info!(
                "agent {} refused {tool}: not allowed for its roles",
                agent.id
            );
            return Err(ROLE_DENIED.into());
        }
        let id = agent.id.as_str();
        match tool {
            mcp_tools::CREATE_TICKET => self.create(&agent, args, now),
            mcp_tools::LIST_TICKETS => self.list(id, args),
            mcp_tools::GET_TICKET => self.get(args),
            mcp_tools::SUBMIT_FOR_REVIEW => self.submit(id, args, now),
            mcp_tools::UPDATE_STATUS => self.update_status(id, args, now),
            mcp_tools::GET_WORKSPACE_RULES => self.workspace_rules(),
            mcp_tools::LIST_PROJECTS => Ok(self.list_projects()),
            mcp_tools::ADD_REPORT => self.add_report(id, args),
            mcp_tools::GET_REPORT => self.get_report(args),
            mcp_tools::APPROVE_TICKET => self.approve(&agent, args, now),
            mcp_tools::REJECT_TICKET => self.reject(&agent, args, now),
            mcp_tools::ASSIGN_TICKET => self.assign(&agent, args, now),
            mcp_tools::UNASSIGN_TICKET => self.unassign(&agent, args, now),
            mcp_tools::HANDOFF_TICKET => self.handoff(&agent, args, now),
            mcp_tools::SPAWN_AGENT => self.spawn_agent(id, args),
            mcp_tools::LIST_AGENTS => Ok(self.list_agents()),
            mcp_tools::LIST_PROFILES => Ok(self.list_profiles()),
            other => Err(format!("Ukendt værktøj: {other}")),
        }
    }

    /// The agent if `agent_id` names one that has not exited (manager lock, briefly).
    fn live_agent(&self, agent_id: Option<&str>) -> Result<AgentInfo, String> {
        let id = agent_id.filter(|s| !s.is_empty()).ok_or(UNKNOWN_AGENT)?;
        lock(&self.tickets.manager)
            .get(id)
            .filter(|a| !matches!(a.status, AgentStatus::Exited { .. }))
            .ok_or_else(|| UNKNOWN_AGENT.into())
    }

    /// Another agent by id, live (manager lock, briefly); "Agenten kører ikke" otherwise.
    fn target_agent(&self, agent_id: &str) -> Result<AgentInfo, String> {
        lock(&self.tickets.manager)
            .get(agent_id)
            .filter(|a| !matches!(a.status, AgentStatus::Exited { .. }))
            .ok_or_else(|| TicketError::AgentNotLive.into())
    }

    fn is_live(&self, agent_id: &str) -> bool {
        self.target_agent(agent_id).is_ok()
    }

    /// See [`TicketsCtx::set_agent_detail`].
    fn set_detail(
        &self,
        agent_id: &str,
        detail: Option<String>,
        only_if: impl FnOnce(Option<&str>) -> bool,
    ) {
        self.tickets.set_agent_detail(agent_id, detail, only_if);
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

    /// A project named by an agent: an existing one in its spelling on disk
    /// ([`ProjectError::NotFound`] otherwise); a `{"new": …}` whose folder already exists becomes
    /// that project; a really new one needs `agentsMayCreateProjects` (it is created when the
    /// ticket is assigned or spawned with).
    fn agent_project(&self, project: ProjectRef) -> Result<Option<ProjectRef>, String> {
        let root = self.tickets.workspace.root();
        match project {
            ProjectRef::Existing(id) => projects::find_project(root, &id)
                .map(|p| Some(ProjectRef::Existing(p.id)))
                .ok_or_else(|| ProjectError::NotFound(id).into()),
            ProjectRef::New { new } => match projects::find_project(root, &new) {
                Some(p) => Ok(Some(ProjectRef::Existing(p.id))),
                None if self.tickets.workspace.rules().agents_may_create_projects => {
                    Ok(Some(ProjectRef::New { new }))
                }
                None => Err(ProjectError::AgentsMayNotCreate(new).into()),
            },
        }
    }

    /// The assignment rule for a ticket with `project` going to `target` (plan4b A.2): a work
    /// agent only takes its own project; `Some(id)` when the ticket's project must become `id`
    /// (a `{"new": …}` matching the agent's project, realised under the same rule as
    /// [`Self::agent_project`]).
    fn target_project(
        &self,
        project: Option<&ProjectRef>,
        target: &AgentInfo,
    ) -> Result<Option<ProjectId>, String> {
        match projects::assignment_target(
            project,
            target.seat_kind,
            target.project.as_deref(),
            &target.name,
        )? {
            AssignmentProject::Unchanged => Ok(None),
            AssignmentProject::Set(p) => {
                let may_create = self.tickets.workspace.rules().agents_may_create_projects;
                let root = self.tickets.workspace.root();
                Ok(Some(
                    projects::realize(root, &ProjectRef::New { new: p }, may_create)?.id,
                ))
            }
        }
    }

    /// The project the ticket gets when it goes to `target`: without `given` the plain rule
    /// ([`Self::target_project`]); with `given` (`project` on `mira_assign_ticket` /
    /// `mira_handoff_ticket`) only for a ticket without a project ([`ticket_has_project`]
    /// otherwise), checked against `target` before a new folder is created.
    fn assignment_project(
        &self,
        current: Option<&ProjectRef>,
        given: Option<ProjectRef>,
        target: &AgentInfo,
    ) -> Result<Option<ProjectId>, String> {
        let Some(given) = given else {
            return self.target_project(current, target);
        };
        if let Some(p) = current {
            return Err(ticket_has_project(p));
        }
        let given = self
            .agent_project(given)?
            .ok_or(TicketError::ProjectRequired)?;
        projects::assignment_target(
            Some(&given),
            target.seat_kind,
            target.project.as_deref(),
            &target.name,
        )?;
        let may_create = self.tickets.workspace.rules().agents_may_create_projects;
        let root = self.tickets.workspace.root();
        Ok(Some(projects::realize(root, &given, may_create)?.id))
    }

    fn create(
        &self,
        agent: &AgentInfo,
        args: &Map<String, Value>,
        now: u64,
    ) -> Result<Value, String> {
        let agent_id = agent.id.as_str();
        let title = one_line(opt_str(args, "title", "Titel må ikke være tom")?.unwrap_or_default());
        let body =
            clean_body(opt_str(args, "body", "body skal være en tekst")?.unwrap_or_default());
        let rules = self.tickets.workspace.rules();
        // Step 4b: absent = the workspace's reviewByDefault.
        let skip_review = match args.get("skipReview") {
            None | Some(Value::Null) => !rules.review_by_default,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err("skipReview skal være true eller false".into()),
        };
        // assignTo: only the coordinator role, only to a live agent (plan5 C5.5).
        let target = match opt_id(args, "assignTo")? {
            Some(target) => {
                if !agent.roles.contains(&Role::Coordinator) {
                    return Err(ONLY_COORDINATOR_ASSIGNS.into());
                }
                Some(self.target_agent(target)?)
            }
            None => None,
        };
        let assign_to = target.as_ref().map(|t| t.id.clone());
        // The project: explicit (checked), else the assignTo agent's, else the creator's
        // (plan4b A.2).
        let mut project = match parse_project(args)? {
            Some(p) => self.agent_project(p)?,
            None => target
                .as_ref()
                .and_then(|t| t.project.clone())
                .or_else(|| agent.project.clone())
                .map(ProjectRef::Existing),
        };
        if let Some(t) = &target {
            if let Some(p) = self.target_project(project.as_ref(), t)? {
                project = Some(ProjectRef::Existing(p));
            }
        }
        if self.recent_creates(agent_id, now) >= CREATE_TICKET_RATE_LIMIT {
            return Err(TicketError::RateLimited.into());
        }
        let t = self.tickets.mutate(|s| {
            s.create_by_agent(
                &title,
                body.trim(),
                skip_review,
                assign_to.as_deref().map(|a| (a, agent.name.as_str())),
                project,
                now,
            )
        })?;
        lock(&self.created)
            .entry(agent_id.to_string())
            .or_default()
            .push_back(now);
        log::info!(
            "agent {agent_id} created ticket {} (title {} chars, body {} chars){}",
            t.short_id(),
            t.title.chars().count(),
            t.body.chars().count(),
            assign_to
                .as_deref()
                .map(|a| format!(" for agent {a}"))
                .unwrap_or_default()
        );
        let mut v = created_json(&t);
        if let (Some(target), Value::Object(m)) = (&assign_to, &mut v) {
            m.insert("assigneeAgentId".into(), Value::String(target.clone()));
            self.tickets.notify([target.as_str()]);
        }
        Ok(v)
    }

    fn list(&self, agent_id: &str, args: &Map<String, Value>) -> Result<Value, String> {
        let filter = opt_str(args, "filter", UNKNOWN_FILTER)?
            .map(str::trim)
            .unwrap_or("mine");
        let mut tickets = match filter {
            "mine" => self.tickets.read(|s| s.list_for_agent(agent_id)),
            "backlog" => self.tickets.read(TicketService::backlog),
            "all" => self.tickets.read(TicketService::list),
            _ => return Err(UNKNOWN_FILTER.into()),
        };
        // Step 4b: `project` = an id, or "none" for tickets without a project.
        let project = opt_id(args, "project")?;
        if let Some(p) = project {
            tickets.retain(|t| {
                if p.eq_ignore_ascii_case("none") {
                    t.project.is_none()
                } else {
                    projects::matches(t.project.as_ref(), p)
                }
            });
        }
        for t in &mut tickets {
            t.summary = t
                .summary
                .as_deref()
                .map(|s| truncate_chars(s, LIST_SUMMARY_MAX_CHARS));
        }
        let mut v = json!({"filter": filter, "tickets": tickets});
        if let (Some(p), Value::Object(m)) = (project, &mut v) {
            m.insert("project".into(), Value::String(p.to_string()));
        }
        Ok(v)
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

    /// `mira_submit_for_review`, with an optional `report` written first (plan5 C5.5: nothing
    /// is submitted when the report fails).
    // TODO(windows-verify): `mira_submit_for_review` with `report` in one call puts the ticket in
    // Review with the report visible (plan5 D.61).
    fn submit(&self, agent_id: &str, args: &Map<String, Value>, now: u64) -> Result<Value, String> {
        let summary_error = "summary skal være en tekst på 1–2000 tegn";
        let summary = opt_str(args, "summary", summary_error)?.unwrap_or_default();
        let ticket_id = opt_id(args, "ticketId")?;
        let report = opt_str(args, "report", "report skal være en tekst på 1–20000 tegn")?;
        let mut report_id = None;
        if let Some(body) = report {
            // The same checks as the submit, read-only, so a refused submit writes no report.
            let summary_ok = summary.trim().chars().count();
            if summary_ok == 0 || summary_ok > crate::config::TICKET_SUMMARY_MAX_CHARS {
                return Err(summary_error.into());
            }
            let target = self.own_submittable(agent_id, ticket_id)?;
            validate_report(REPORT_ON_SUBMIT_TITLE, body)?;
            let r = self.tickets.add_report(
                &target.id,
                ReportAuthor::agent(agent_id),
                REPORT_ON_SUBMIT_TITLE,
                body,
            )?;
            report_id = Some(r.id);
        }
        let t = self
            .tickets
            .mutate(|s| s.submit_by_agent(agent_id, ticket_id, summary, now))?;
        // The queue may move on at the next idle; a review needs a reviewer.
        self.tickets.notify([agent_id]);
        self.tickets.clear_stale_detail(agent_id);
        if t.state == TicketState::Review {
            self.tickets.route_reviews();
        }
        log::info!(
            "agent {agent_id} submitted ticket {} -> {} (summary {} chars, report {})",
            t.short_id(),
            t.state.as_str(),
            t.summary.as_deref().map_or(0, |s| s.chars().count()),
            report_id.is_some()
        );
        let mut v = json!({
            "id": t.id,
            "shortId": t.short_id(),
            "state": t.state,
            "summary": t.summary,
        });
        if let (Some(r), Value::Object(m)) = (report_id, &mut v) {
            m.insert("reportId".into(), Value::String(r));
        }
        Ok(v)
    }

    /// The ticket `mira_submit_for_review` would submit (same rules as
    /// `TicketService::submit_by_agent`), read-only.
    fn own_submittable(&self, agent_id: &str, ticket_id: Option<&str>) -> Result<Ticket, String> {
        let t = match ticket_id {
            Some(id) => {
                let t = self
                    .tickets
                    .read(|s| s.get_by_any_id(id))
                    .ok_or(TicketError::NotFound)?;
                if t.assignee_agent_id.as_deref() != Some(agent_id) {
                    return Err(TicketError::NotYours.into());
                }
                if t.state != TicketState::InProgress {
                    return Err(TicketError::NotInProgress.into());
                }
                t
            }
            None => self
                .tickets
                .read(|s| s.current_for_agent(agent_id))
                .ok_or(TicketError::NoTicketInProgress)?,
        };
        Ok(t)
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

    // ---- reports ----

    /// `mira_add_report`: on the agent's own ticket (default: its ticket in progress), or as
    /// reviewer on the ticket in review it was assigned.
    fn add_report(&self, agent_id: &str, args: &Map<String, Value>) -> Result<Value, String> {
        let title = opt_str(args, "title", "Titel må ikke være tom")?.unwrap_or_default();
        let body = opt_str(args, "body", "Rapporten må ikke være tom")?.unwrap_or_default();
        let t = match opt_id(args, "ticketId")? {
            Some(id) => self
                .tickets
                .read(|s| s.get_by_any_id(id))
                .ok_or(TicketError::NotFound)?,
            None => self
                .tickets
                .read(|s| s.current_for_agent(agent_id))
                .ok_or(TicketError::NoTicketInProgress)?,
        };
        let own = t.assignee_agent_id.as_deref() == Some(agent_id);
        let reviewing = t.reviewer_agent_id.as_deref() == Some(agent_id);
        if !own {
            if reviewing && t.state != TicketState::Review {
                return Err(TicketError::NotYourReview.into());
            }
            if !reviewing {
                return Err(TicketError::NotYours.into());
            }
        }
        let r: TicketReport =
            self.tickets
                .add_report(&t.id, ReportAuthor::agent(agent_id), title, body)?;
        let mut v = serde_json::to_value(&r).map_err(|e| e.to_string())?;
        if let Value::Object(m) = &mut v {
            m.insert("ticketId".into(), Value::String(t.id.clone()));
        }
        Ok(v)
    }

    /// `mira_get_report`: any agent may read any report.
    fn get_report(&self, args: &Map<String, Value>) -> Result<Value, String> {
        let ticket_id = req_id(args, "ticketId")?;
        let report_id = opt_str(args, "reportId", "reportId skal være en tekst på 1–8 tegn")?
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or("reportId skal være en tekst på 1–8 tegn")?;
        let c = self.tickets.get_report(ticket_id, report_id)?;
        serde_json::to_value(&c).map_err(|e| e.to_string())
    }

    // ---- reviewer ----

    // TODO(windows-verify): mira_approve_ticket sets Done, mira_reject_ticket puts the ticket
    // first in the coder's queue with round 1/3 (plan5 D.54).
    fn approve(
        &self,
        agent: &AgentInfo,
        args: &Map<String, Value>,
        now: u64,
    ) -> Result<Value, String> {
        let id = req_id(args, "id")?;
        let note = review_note(args)?;
        let t = self
            .tickets
            .mutate(|s| s.approve_by_agent(&agent.id, &agent.name, id, Some(&note), now))?;
        self.tickets
            .notify(t.assignee_agent_id.iter().map(String::as_str));
        log::info!("agent {} approved ticket {}", agent.id, t.short_id());
        Ok(json!({"id": t.id, "shortId": t.short_id(), "state": "done"}))
    }

    fn reject(
        &self,
        agent: &AgentInfo,
        args: &Map<String, Value>,
        now: u64,
    ) -> Result<Value, String> {
        let id = req_id(args, "id")?;
        let note = review_note(args)?;
        if note.is_empty() {
            return Err(TicketError::NeedsNote.into());
        }
        let sender = self
            .tickets
            .read(|s| s.get_by_any_id(id))
            .and_then(|t| t.assignee_agent_id);
        let sender_live = sender.as_deref().is_some_and(|a| self.is_live(a));
        let t = self
            .tickets
            .mutate(|s| s.reject_by_agent(&agent.id, &agent.name, id, &note, sender_live, now))?;
        self.tickets.notify(sender.iter().map(String::as_str));
        self.tickets.route_reviews();
        log::info!(
            "agent {} rejected ticket {} (round {})",
            agent.id,
            t.short_id(),
            t.review_round
        );
        // C5.5: "rejected" while it waits (first in the sender's queue), else "backlog".
        let state = if t.state == TicketState::Backlog {
            "backlog"
        } else {
            "rejected"
        };
        Ok(json!({
            "id": t.id,
            "shortId": t.short_id(),
            "state": state,
            "reviewRound": t.review_round,
            "escalated": t.escalated,
        }))
    }

    // ---- coordinator ----

    fn assign(
        &self,
        agent: &AgentInfo,
        args: &Map<String, Value>,
        now: u64,
    ) -> Result<Value, String> {
        let id = req_id(args, "id")?;
        let target = self.target_agent(req_id(args, "agentId")?)?;
        let given = parse_project(args)?;
        let ticket = self
            .tickets
            .read(|s| s.get_by_any_id(id))
            .ok_or(TicketError::NotFound)?;
        // A ticket in progress: only the coordinator's own, handed over (step 5c).
        if ticket.state == TicketState::InProgress {
            return self.hand_on(agent, id, Some(&target), given, now);
        }
        // Step 4b: a work agent only takes tickets of its own project; `project` names the
        // project of a ticket that has none.
        let set = self.assignment_project(ticket.project.as_ref(), given, &target)?;
        let t = self
            .tickets
            .mutate(|s| s.assign_by_agent(id, &target.id, &agent.name, set, now))?;
        self.tickets.notify([target.id.as_str()]);
        log::info!(
            "agent {} assigned ticket {} to agent {}",
            agent.id,
            t.short_id(),
            target.id
        );
        Ok(json!({
            "id": t.id,
            "shortId": t.short_id(),
            "state": t.state,
            "assigneeAgentId": t.assignee_agent_id,
            "queuePosition": t.queue_position,
        }))
    }

    fn unassign(
        &self,
        agent: &AgentInfo,
        args: &Map<String, Value>,
        now: u64,
    ) -> Result<Value, String> {
        let id = req_id(args, "id")?;
        let before = self.tickets.read(|s| s.get_by_any_id(id));
        // A ticket in progress: only the coordinator's own, put back (step 5c).
        if before
            .as_ref()
            .is_some_and(|t| t.state == TicketState::InProgress)
        {
            return self.hand_on(agent, id, None, None, now);
        }
        let old = before.and_then(|t| t.assignee_agent_id);
        let t = self
            .tickets
            .mutate(|s| s.unassign_by_agent(id, &agent.id, now))?;
        self.tickets.notify(old);
        log::info!("agent {} unassigned ticket {}", agent.id, t.short_id());
        Ok(json!({"id": t.id, "shortId": t.short_id(), "state": t.state}))
    }

    // ---- step 5c: handing a ticket in progress on (every role) ----

    /// `mira_handoff_ticket`: the agent's own ticket in progress (`ticketId`, default: the
    /// current one) to `agentId` (live, not itself) or, without it, back to the backlog.
    fn handoff(
        &self,
        agent: &AgentInfo,
        args: &Map<String, Value>,
        now: u64,
    ) -> Result<Value, String> {
        let ticket_id = match opt_id(args, "ticketId")? {
            Some(id) => id.to_string(),
            None => self
                .tickets
                .read(|s| s.current_for_agent(&agent.id))
                .map(|t| t.id)
                .ok_or(TicketError::NoTicketInProgress)?,
        };
        let target = opt_id(args, "agentId")?
            .map(|a| self.target_agent(a))
            .transpose()?;
        let given = parse_project(args)?;
        if given.is_some() && target.is_none() {
            return Err(PROJECT_NEEDS_TARGET.into());
        }
        self.hand_on(agent, &ticket_id, target.as_ref(), given, now)
    }

    /// Hands `agent`'s own ticket in progress to `target` (last in its queue) or back to the
    /// backlog. The service refuses someone else's ticket ("tildelt en anden agent"), a ticket
    /// not in progress and a handoff to the agent itself. Afterwards the agent has no ticket in
    /// progress: its "ikke afleveret"/"turn fejlede" hint goes, its Stop marks nothing and its
    /// queue moves on; the target's queue is woken. The ticket file in the agent's folder stays
    /// (it may still be reading it this turn; the next delivery of that ticket overwrites it).
    fn hand_on(
        &self,
        agent: &AgentInfo,
        ticket_id: &str,
        target: Option<&AgentInfo>,
        given: Option<ProjectRef>,
        now: u64,
    ) -> Result<Value, String> {
        let t = match target {
            Some(to) => {
                // Step 4b: the project rule for the new agent (only the agent's own ticket in
                // progress can be handed on; the service checks that below).
                let project = self
                    .tickets
                    .read(|s| s.get_by_any_id(ticket_id))
                    .and_then(|t| t.project);
                let set = self.assignment_project(project.as_ref(), given, to)?;
                self.tickets.mutate(|s| {
                    s.handoff_in(
                        ticket_id,
                        &to.id,
                        Some(&agent.id),
                        (&agent.name, &to.name),
                        set,
                        now,
                    )
                })?
            }
            None => self
                .tickets
                .mutate(|s| s.give_back(ticket_id, Some(&agent.id), now))?,
        };
        // The service only lets the assignee hand its own ticket on, so the sender is the
        // caller: it gets the detail text, never the "Du skal stoppe …" line (review 5c W4).
        self.tickets
            .handed_over(&agent.id, &t, target.map(|a| a.name.as_str()), false);
        self.tickets
            .notify(std::iter::once(agent.id.as_str()).chain(target.map(|a| a.id.as_str())));
        log::info!(
            "agent {} handed ticket {} on to {}",
            agent.id,
            t.short_id(),
            target.map_or("the backlog", |a| a.id.as_str())
        );
        Ok(json!({
            "id": t.id,
            "shortId": t.short_id(),
            "state": t.state,
            "assigneeAgentId": t.assignee_agent_id,
            "queuePosition": t.queue_position,
            "message": "Ticketen er ikke længere din; arbejd ikke videre på den. Afslut dit svar.",
        }))
    }

    // TODO(windows-verify): mira_spawn_agent respects the limits (a 6th work agent is refused
    // with the Danish limit text) and mira_create_ticket with assignTo queues the ticket
    // (plan5 D.57).
    fn spawn_agent(&self, agent_id: &str, args: &Map<String, Value>) -> Result<Value, String> {
        let profile_id = opt_str(
            args,
            "profileId",
            "profileId skal være en tekst på 1–40 tegn",
        )?
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("profileId skal være en tekst på 1–40 tegn")?;
        let seat_kind = match opt_str(
            args,
            "seatKind",
            "seatKind skal være \"work\" eller \"staff\"",
        )?
        .map(str::trim)
        {
            None => None,
            Some("work") => Some(SeatKind::Work),
            Some("staff") => Some(SeatKind::Staff),
            Some(_) => return Err("seatKind skal være \"work\" eller \"staff\"".into()),
        };
        let first_ticket_id = opt_id(args, "firstTicketId")?.map(str::to_string);
        let project = parse_project(args)?;
        let profile = self
            .profiles
            .get(profile_id)
            .ok_or(crate::profiles::ProfileError::NotFound)?;
        // Refused as a tool error before the port (the spawn path checks it again, 5c B).
        profile.check_seat(seat_kind.unwrap_or(profile.default_seat))?;
        let port = lock(&self.spawn).clone().ok_or(SPAWN_UNAVAILABLE)?;
        let info = port(SpawnByProfile {
            profile_id: profile_id.to_string(),
            seat_kind,
            first_ticket_id,
            project,
        })?;
        log::info!(
            "agent {agent_id} spawned agent {} from profile {}",
            info.id,
            info.profile_id
        );
        Ok(json!({
            "agentId": info.id,
            "name": info.name,
            "cwd": info.cwd,
            "profileId": info.profile_id,
            "seatKind": info.seat_kind,
            "project": info.project,
        }))
    }

    fn list_agents(&self) -> Value {
        let agents: Vec<Value> = lock(&self.tickets.manager)
            .list()
            .into_iter()
            .map(|a| {
                json!({
                    "id": a.id,
                    "name": a.name,
                    "cwd": a.cwd,
                    "profileId": a.profile_id,
                    "profileName": a.profile_name,
                    "roles": a.roles,
                    "seatKind": a.seat_kind,
                    "project": a.project,
                    "status": status_kind(&a.status),
                    "currentTicketId": a.current_ticket_id,
                    "queueLength": a.queue_length,
                    "openReviews": a.open_reviews,
                })
            })
            .collect();
        json!({ "agents": agents })
    }

    /// `mira_get_workspace_rules` (plan4b C4b.5): the effective rules from the workspace file,
    /// the projects root, the file's path, the project ids and the notes (a warning about an
    /// unreadable file first).
    fn workspace_rules(&self) -> Result<Value, String> {
        let ws = &self.tickets.workspace;
        let snap = ws.snapshot();
        let mut v = serde_json::to_value(snap.rules).map_err(|e| e.to_string())?;
        let ids: Vec<String> = projects::list_projects(ws.root())
            .into_iter()
            .map(|p| p.id)
            .collect();
        let notes: Vec<String> = snap.warning.into_iter().chain(snap.notes).collect();
        if let Value::Object(m) = &mut v {
            m.insert(
                "projectsRoot".into(),
                Value::String(ws.root().to_string_lossy().into_owned()),
            );
            m.insert(
                "workspaceFile".into(),
                Value::String(ws.path().to_string_lossy().into_owned()),
            );
            m.insert("projects".into(), json!(ids));
            m.insert("notes".into(), json!(notes));
        }
        Ok(v)
    }

    /// `mira_list_projects`: the folders under the projects root with their live work agents.
    // TODO(windows-verify): a coder sees 11 tools including mira_list_projects; the list shows
    // the folders under %USERPROFILE%\mira-bots\projects (plan4b D.85).
    fn list_projects(&self) -> Value {
        let root = self.tickets.workspace.root();
        let list = projects::list_projects(root);
        let m = lock(&self.tickets.manager);
        let projects: Vec<Value> = list
            .into_iter()
            .map(|p| {
                let agents = m.live_work_in_project(&p.id).len();
                json!({"id": p.id, "path": p.path, "agents": agents})
            })
            .collect();
        json!({
            "projectsRoot": root.to_string_lossy(),
            "projects": projects,
        })
    }

    fn list_profiles(&self) -> Value {
        let profiles: Vec<Value> = self
            .profiles
            .list()
            .into_iter()
            .map(|p| {
                json!({
                    "id": p.id,
                    "name": p.name,
                    "roles": p.roles,
                    "specialist": p.is_specialist(),
                    "defaultSeat": p.default_seat,
                    "model": p.model,
                    "effort": p.effort,
                })
            })
            .collect();
        json!({ "profiles": profiles })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentManager;
    use crate::config::NOT_SUBMITTED_TEXT;
    use crate::events::{AGENTS_CHANGED, TICKETS_CHANGED};
    use crate::tickets::dispatcher::DispatchMsg;
    use crate::tickets::model::{TicketActor, TicketIssue, TicketSource, TicketState};
    use crate::tickets::test_support::{test_ctx, TestCtx};
    use mira_mcp::tools::ALL_TOOL_NAMES;

    struct T {
        tc: TestCtx,
        tools: ToolsCtx,
        a: String,
        b: String,
        /// Reviewer (staff).
        r: String,
        /// Coordinator (staff).
        k: String,
        profiles_dir: std::path::PathBuf,
    }

    impl Drop for T {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.profiles_dir);
            let _ = std::fs::remove_dir_all(self.tc.ctx.reports.root());
            let _ = std::fs::remove_dir_all(self.tc.ctx.workspace.root());
        }
    }

    fn setup() -> T {
        let mut m = AgentManager::new(5);
        let a = m.insert_fake("s-a", "/w/a");
        let b = m.insert_fake("s-b", "/w/b");
        let r = m.insert_fake_with("s-r", "/w/r", &[Role::Reviewer], SeatKind::Staff);
        let k = m.insert_fake_with("s-k", "/w/k", &[Role::Coordinator], SeatKind::Staff);
        let tc = test_ctx(Arc::new(Mutex::new(m)));
        // The work fakes' project folder under the test root.
        std::fs::create_dir_all(tc.ctx.workspace.root().join("p")).unwrap();
        let profiles_dir =
            std::env::temp_dir().join(format!("mira-tools-profiles-{}", uuid::Uuid::new_v4()));
        let emit: crate::events::EmitFn = Arc::new(|_: &str, _: Value| {});
        let profiles = Arc::new(ProfilesCtx::new(
            crate::profiles::ProfileStore::load(profiles_dir.clone(), 1),
            emit,
        ));
        let tools = ToolsCtx::new(Arc::clone(&tc.ctx), profiles);
        T {
            tc,
            tools,
            a,
            b,
            r,
            k,
            profiles_dir,
        }
    }

    impl T {
        /// A live coder on a work seat in `project`.
        fn agent_in_project(&self, project: &str) -> String {
            std::fs::create_dir_all(self.tc.ctx.workspace.root().join(project)).unwrap();
            self.tc.ctx.manager.lock().unwrap().insert_fake_in(
                &format!("s-{project}"),
                &format!("/w/{project}"),
                &[Role::Coder],
                SeatKind::Work,
                Some(project),
            )
        }

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
            // In project "p" (the work fakes' project), so it may go to any agent.
            let p = Some(ProjectRef::Existing("p".into()));
            let t = c.mutate(|s| s.create_in(title, "b", skip, p, 1)).unwrap();
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
            json!({"id":id,"shortId":tk.short_id(),"title":"Følg op på login","state":"backlog","skipReview":true,"project":"p"})
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
        let submit = &now.history[now.history.len() - 2];
        assert_eq!(
            (submit.by, submit.to),
            (TicketActor::Agent, TicketState::Review)
        );
        // Then routed to the reviewer (plan5 A.6).
        assert_eq!(now.reviewer_agent_id.as_deref(), Some(t.r.as_str()));
        assert_eq!(now.history.last().unwrap().by, TicketActor::System);
        assert_eq!(
            t.tc.sent(),
            vec![
                DispatchMsg::QueueChanged {
                    agent_id: t.a.clone()
                },
                DispatchMsg::ReviewAssigned {
                    reviewer_agent_id: t.r.clone()
                }
            ]
        );
        assert_eq!(t.detail(&t.a), None);
        assert_eq!(t.tc.emitted(TICKETS_CHANGED).len(), 2);
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

    /// N1: a list of 150 finished tickets with 2000-character summaries stays far below
    /// mira-mcp's `MAX_REPLY` (1 MiB); summaries are cut to 160 characters + "…" and the full
    /// text is still there through `mira_get_ticket`.
    #[test]
    fn list_truncates_summaries_and_stays_below_the_reply_limit() {
        const MCP_MAX_REPLY: usize = 1 << 20; // crates/mira-mcp: MAX_REPLY
        let t = setup();
        let long = "æ".repeat(2_000);
        let mut first = String::new();
        for i in 0..150 {
            let tk = t.in_progress(&t.a, &format!("t{i}"), false);
            if i == 0 {
                first = tk.id.clone();
            }
            t.call(
                Some(&t.a),
                "mira_submit_for_review",
                json!({"summary": long}),
                10,
            )
            .unwrap();
        }
        let r = t
            .call(Some(&t.a), "mira_list_tickets", json!({"filter":"all"}), 11)
            .unwrap();
        assert_eq!(r["tickets"].as_array().unwrap().len(), 150);
        let reply =
            json!({"v":1,"kind":"tool_result","request_id":"1-1","ok":true,"result":r}).to_string();
        assert!(
            reply.len() < MCP_MAX_REPLY / 2,
            "list reply is {} bytes",
            reply.len()
        );
        let shown = r["tickets"][0]["summary"].as_str().unwrap();
        assert_eq!(shown.chars().count(), LIST_SUMMARY_MAX_CHARS + 1);
        assert!(shown.ends_with('…'));
        // The full text is unchanged in the service and in get_ticket.
        let full = t
            .call(Some(&t.a), "mira_get_ticket", json!({"id": first}), 12)
            .unwrap();
        assert_eq!(full["summary"].as_str(), Some(long.as_str()));
        assert_eq!(t.ticket(&first).summary.as_deref(), Some(long.as_str()));
    }

    #[test]
    fn truncate_chars_cuts_on_characters() {
        assert_eq!(truncate_chars("kort", 160), "kort");
        assert_eq!(truncate_chars("æøå", 3), "æøå");
        assert_eq!(truncate_chars("æøåx", 3), "æøå…");
        assert_eq!(truncate_chars("", 3), "");
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

    // ---- step 5 (plan5 punkt 13) ----

    impl T {
        fn agent_with(&self, roles: &[Role], seat: SeatKind) -> String {
            self.tc.ctx.manager.lock().unwrap().insert_fake_with(
                &uuid::Uuid::new_v4().to_string(),
                "/w/x",
                roles,
                seat,
            )
        }

        /// A ticket submitted by `sender` and routed to `self.r`.
        fn review_for_r(&self, sender: &str, title: &str) -> Ticket {
            let t = self.in_progress(sender, title, false);
            self.call(
                Some(sender),
                "mira_submit_for_review",
                json!({"summary":"klar"}),
                5,
            )
            .unwrap();
            let t = self.ticket(&t.id);
            assert_eq!(
                t.reviewer_agent_id.as_deref(),
                Some(self.r.as_str()),
                "routed"
            );
            t
        }
    }

    #[test]
    fn role_tool_matrix_is_enforced() {
        let t = setup();
        const COMMON: [&str; 11] = [
            "mira_create_ticket",
            "mira_list_tickets",
            "mira_get_ticket",
            "mira_submit_for_review",
            "mira_update_status",
            "mira_get_workspace_rules",
            "mira_add_report",
            "mira_get_report",
            "mira_handoff_ticket",
            "mira_list_agents",
            "mira_list_projects",
        ];
        let table: [(&[Role], &[&str]); 8] = [
            (&[], &[]),
            (&[Role::Coder], &[]),
            (&[Role::Researcher], &[]),
            (&[Role::Planner], &[]),
            (&[Role::Debugger], &[]),
            (
                &[Role::Reviewer],
                &["mira_approve_ticket", "mira_reject_ticket"],
            ),
            (
                &[Role::Coordinator],
                &[
                    "mira_assign_ticket",
                    "mira_unassign_ticket",
                    "mira_spawn_agent",
                    "mira_list_profiles",
                ],
            ),
            (
                &[Role::Reviewer, Role::Coordinator],
                &[
                    "mira_approve_ticket",
                    "mira_reject_ticket",
                    "mira_assign_ticket",
                    "mira_unassign_ticket",
                    "mira_spawn_agent",
                    "mira_list_profiles",
                ],
            ),
        ];
        assert_eq!(ALL_TOOL_NAMES.len(), 17);
        for (roles, extra) in table {
            let agent = t.agent_with(roles, SeatKind::Work);
            for tool in ALL_TOOL_NAMES {
                let allowed = COMMON.contains(&tool) || extra.contains(&tool);
                let r = t.call(Some(&agent), tool, json!({}), 1);
                if allowed {
                    assert_ne!(r, Err(ROLE_DENIED.into()), "{roles:?} {tool}");
                } else {
                    assert_eq!(r, Err(ROLE_DENIED.into()), "{roles:?} {tool}");
                }
            }
        }
        // An unknown tool is still "Ukendt værktøj", before the role gate.
        assert_eq!(
            t.call(Some(&t.a), "mira_nope", json!({}), 1),
            Err("Ukendt værktøj: mira_nope".into())
        );
    }

    #[test]
    fn approve_via_tool_rules() {
        let t = setup();
        let tk = t.review_for_r(&t.a, "Ret login");
        // Not a reviewer at all.
        assert_eq!(
            t.call(Some(&t.a), "mira_approve_ticket", json!({"id": tk.id}), 6),
            Err(ROLE_DENIED.into())
        );
        // A second reviewer that is not this ticket's reviewer.
        let r2 = t.agent_with(&[Role::Reviewer], SeatKind::Staff);
        assert_eq!(
            t.call(
                Some(&r2),
                "mira_approve_ticket",
                json!({"id": tk.short_id()}),
                6
            ),
            Err("Du er ikke reviewer på denne ticket".into())
        );
        // Not in review.
        let backlog = t.tc.ctx.mutate(|s| s.create("b", "", false, 1)).unwrap();
        assert_eq!(
            t.call(
                Some(&t.r),
                "mira_approve_ticket",
                json!({"id": backlog.id}),
                6
            ),
            Err("Ticketen er ikke i review".into())
        );
        // Its own submission (a reviewer that also submits work).
        let own = t.in_progress(&t.r, "Eget", false);
        t.call(
            Some(&t.r),
            "mira_submit_for_review",
            json!({"summary":"x"}),
            6,
        )
        .unwrap();
        assert_eq!(
            t.call(Some(&t.r), "mira_approve_ticket", json!({"id": own.id}), 6),
            Err("Du kan ikke reviewe din egen aflevering".into())
        );
        assert_eq!(
            t.call(Some(&t.r), "mira_approve_ticket", json!({"id": "nope"}), 6),
            Err("Ticketen findes ikke".into())
        );
        assert_eq!(
            t.call(
                Some(&t.r),
                "mira_approve_ticket",
                json!({"id": tk.id, "note": "n".repeat(2001)}),
                6
            ),
            Err("note må højst være 2000 tegn".into())
        );
        let r = t
            .call(
                Some(&t.r),
                "mira_approve_ticket",
                json!({"id": tk.short_id(), "note": "Tests\nok"}),
                7,
            )
            .unwrap();
        assert_eq!(
            r,
            json!({"id": tk.id, "shortId": tk.short_id(), "state": "done"})
        );
        let done = t.ticket(&tk.id);
        assert_eq!(done.state, TicketState::Done);
        assert_eq!(done.history.last().unwrap().by, TicketActor::Agent);
        assert!(done
            .history
            .last()
            .unwrap()
            .note
            .as_deref()
            .unwrap()
            .ends_with(": Tests ok"));
    }

    #[test]
    fn reject_via_tool_requires_note_and_requeues() {
        let mut t = setup();
        let first =
            t.tc.ctx
                .mutate(|s| s.create("først", "", false, 1))
                .unwrap();
        let tk = t.review_for_r(&t.a, "Ret login");
        t.tc.ctx.mutate(|s| s.assign(&first.id, &t.a, 6)).unwrap();
        t.tc.sent();
        assert_eq!(
            t.call(Some(&t.r), "mira_reject_ticket", json!({"id": tk.id}), 7),
            Err("Afvisning kræver en note".into())
        );
        assert_eq!(
            t.call(
                Some(&t.r),
                "mira_reject_ticket",
                json!({"id": tk.id, "note": " \n "}),
                7
            ),
            Err("Afvisning kræver en note".into())
        );
        let r = t
            .call(
                Some(&t.r),
                "mira_reject_ticket",
                json!({"id": tk.id, "note": "Mangler test"}),
                8,
            )
            .unwrap();
        assert_eq!(
            r,
            json!({"id": tk.id, "shortId": tk.short_id(), "state": "rejected", "reviewRound": 1, "escalated": false})
        );
        let now = t.ticket(&tk.id);
        assert_eq!(
            (now.state, now.queue_position),
            (TicketState::Assigned, Some(0))
        );
        assert_eq!(now.rejection_note.as_deref(), Some("Mangler test"));
        assert!(t.tc.sent().contains(&DispatchMsg::QueueChanged {
            agent_id: t.a.clone()
        }));
        assert!(t.tc.ctx.read(|s| s.review_assignments()).is_empty());
    }

    #[test]
    fn assign_to_only_for_coordinator() {
        let mut t = setup();
        assert_eq!(
            t.call(
                Some(&t.a),
                "mira_create_ticket",
                json!({"title":"x","assignTo": t.b}),
                1
            ),
            Err(ONLY_COORDINATOR_ASSIGNS.into())
        );
        assert_eq!(
            t.call(
                Some(&t.k),
                "mira_create_ticket",
                json!({"title":"x","assignTo":"nope"}),
                1
            ),
            Err("Agenten kører ikke".into())
        );
        assert!(t.tc.ctx.read(TicketService::is_empty));
        t.tc.sent();
        let r = t
            .call(
                Some(&t.k),
                "mira_create_ticket",
                json!({"title":"Del 1","assignTo": t.b}),
                2,
            )
            .unwrap();
        assert_eq!(r["state"], "assigned");
        assert_eq!(r["assigneeAgentId"], json!(t.b));
        let tk = t.ticket(r["id"].as_str().unwrap());
        assert_eq!(
            (tk.assignee_agent_id.as_deref(), tk.queue_position),
            (Some(t.b.as_str()), Some(0))
        );
        assert_eq!(
            t.tc.sent(),
            vec![DispatchMsg::QueueChanged {
                agent_id: t.b.clone()
            }]
        );

        // mira_assign_ticket / mira_unassign_ticket.
        let b2 = t
            .call(
                Some(&t.k),
                "mira_create_ticket",
                json!({"title":"Del 2","project":"p"}),
                3,
            )
            .unwrap();
        let id = b2["id"].as_str().unwrap();
        assert_eq!(
            t.call(
                Some(&t.k),
                "mira_assign_ticket",
                json!({"id": id, "agentId": "nope"}),
                4
            ),
            Err("Agenten kører ikke".into())
        );
        let r = t
            .call(
                Some(&t.k),
                "mira_assign_ticket",
                json!({"id": b2["shortId"], "agentId": t.b}),
                4,
            )
            .unwrap();
        assert_eq!(
            r,
            json!({"id": id, "shortId": b2["shortId"], "state": "assigned", "assigneeAgentId": t.b, "queuePosition": 1})
        );
        let r = t
            .call(Some(&t.k), "mira_unassign_ticket", json!({"id": id}), 5)
            .unwrap();
        assert_eq!(
            r,
            json!({"id": id, "shortId": b2["shortId"], "state": "backlog"})
        );
        let busy = t.in_progress(&t.b, "i gang", false);
        assert_eq!(
            t.call(
                Some(&t.k),
                "mira_unassign_ticket",
                json!({"id": busy.id}),
                6
            ),
            // Step 5c: only the assignee may put back a ticket in progress.
            Err("Ticketen er tildelt en anden agent".into())
        );
    }

    #[test]
    fn spawn_tool_uses_port_and_limits() {
        let t = setup();
        assert_eq!(
            t.call(
                Some(&t.k),
                "mira_spawn_agent",
                json!({"profileId":"coder"}),
                1
            ),
            Err(SPAWN_UNAVAILABLE.into())
        );
        assert_eq!(
            t.call(
                Some(&t.k),
                "mira_spawn_agent",
                json!({"profileId":"nope"}),
                1
            ),
            Err("Profilen findes ikke".into())
        );
        // A coder on a staff seat is refused as a tool error, before the port (5c B).
        assert_eq!(
            t.call(
                Some(&t.k),
                "mira_spawn_agent",
                json!({"profileId":"coder","seatKind":"staff"}),
                1
            ),
            Err("Profilen «Koder» har ingen stabsrolle (reviewer, koordinator eller planlægger) og kan ikke stå på en stabsplads".into())
        );
        let seen: Arc<Mutex<Vec<SpawnByProfile>>> = Arc::default();
        let (rec, manager) = (Arc::clone(&seen), Arc::clone(&t.tc.ctx.manager));
        t.tools.set_spawn_port(Arc::new(move |req: SpawnByProfile| {
            rec.lock().unwrap().push(req.clone());
            if req.seat_kind == Some(SeatKind::Staff) {
                return Err(crate::agent::AgentError::LimitReached {
                    seat: SeatKind::Staff,
                    max: 3,
                }
                .to_string());
            }
            let mut m = manager.lock().unwrap();
            let id = m.insert_fake("s-new", "/w/coder-01");
            Ok(m.get(&id).unwrap())
        }));
        let r = t
            .call(
                Some(&t.k),
                "mira_spawn_agent",
                json!({"profileId":"coder","firstTicketId":"abc"}),
                2,
            )
            .unwrap();
        assert_eq!(r["name"], "coder-01");
        assert_eq!(r["cwd"], "/w/coder-01");
        assert_eq!(r["seatKind"], "work");
        assert!(r["agentId"].is_string() && r.get("profileId").is_some());
        let err = t
            .call(
                Some(&t.k),
                "mira_spawn_agent",
                json!({"profileId":"reviewer","seatKind":"staff"}),
                3,
            )
            .unwrap_err();
        assert_eq!(
            err,
            crate::agent::AgentError::LimitReached {
                seat: SeatKind::Staff,
                max: 3,
            }
            .to_string()
        );
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![
                SpawnByProfile {
                    profile_id: "coder".into(),
                    seat_kind: None,
                    first_ticket_id: Some("abc".into()),
                    project: None,
                },
                SpawnByProfile {
                    profile_id: "reviewer".into(),
                    seat_kind: Some(SeatKind::Staff),
                    first_ticket_id: None,
                    project: None,
                },
            ]
        );
        assert_eq!(
            t.call(
                Some(&t.a),
                "mira_spawn_agent",
                json!({"profileId":"coder"}),
                4
            ),
            Err(ROLE_DENIED.into())
        );
    }

    #[test]
    fn list_agents_and_profiles_shape() {
        let t = setup();
        t.in_progress(&t.a, "x", false);
        let r = t
            .call(Some(&t.k), "mira_list_agents", json!({}), 1)
            .unwrap();
        let agents = r["agents"].as_array().unwrap();
        assert_eq!(agents.len(), 4);
        let a = agents.iter().find(|x| x["id"] == json!(t.a)).unwrap();
        let keys: Vec<&str> = a.as_object().unwrap().keys().map(String::as_str).collect();
        let mut want = vec![
            "id",
            "name",
            "cwd",
            "profileId",
            "profileName",
            "roles",
            "seatKind",
            "status",
            "currentTicketId",
            "queueLength",
            "openReviews",
            "project",
        ];
        let mut got = keys.clone();
        got.sort_unstable();
        want.sort_unstable();
        assert_eq!(got, want);
        assert_eq!(a["status"], "starting");
        assert!(a["currentTicketId"].is_string());
        let k = agents.iter().find(|x| x["id"] == json!(t.k)).unwrap();
        assert_eq!(
            (k["roles"].clone(), k["seatKind"].clone()),
            (json!(["coordinator"]), json!("staff"))
        );

        let r = t
            .call(Some(&t.k), "mira_list_profiles", json!({}), 1)
            .unwrap();
        let p = r["profiles"].as_array().unwrap();
        assert_eq!(p.len(), 7);
        assert_eq!(
            p[0],
            json!({"id":"coder","name":"Koder","roles":["coder"],"specialist":false,"defaultSeat":"work","model":null,"effort":null})
        );
        let spec = p.iter().find(|x| x["id"] == "specialist").unwrap();
        assert_eq!(spec["specialist"], true);
        assert_eq!(spec["roles"].as_array().unwrap().len(), 6);
    }

    #[test]
    fn workspace_rules_values() {
        let t = setup();
        let r = t
            .call(Some(&t.a), "mira_get_workspace_rules", json!({}), 1)
            .unwrap();
        let root = t.tc.ctx.workspace.root().to_string_lossy().into_owned();
        let file = t.tc.ctx.workspace.path().to_string_lossy().into_owned();
        assert_eq!(
            r,
            json!({"maxWorkAgents":5,"maxStaffAgents":3,"maxReviewRounds":3,"autoReviewOnStop":false,"createTicketRateLimit":20,"ticketBodyMaxChars":20000,"reportBodyMaxChars":20000,"reportsPerTicketMax":20,"reviewByDefault":true,"userInputGraceMs":5000,"agentsMayCreateProjects":false,"maxAgentsPerProject":0,
                   "projectsRoot":root,"workspaceFile":file,"projects":["p"],"notes":[]})
        );
    }

    // ---- step 4b: projects ----

    /// Writes the test root's workspace file.
    fn workspace_file(t: &T, body: &str) {
        std::fs::write(t.tc.ctx.workspace.path(), body).unwrap();
    }

    #[test]
    fn workspace_rules_are_read_from_the_file() {
        let t = setup();
        workspace_file(
            &t,
            r#"{"maxWorkAgents": 2, "agentsMayCreateProjects": true, "maxReviewRounds": 5}"#,
        );
        let r = t
            .call(Some(&t.a), "mira_get_workspace_rules", json!({}), 1)
            .unwrap();
        assert_eq!(r["maxWorkAgents"], 2);
        assert_eq!(r["agentsMayCreateProjects"], true);
        // Read, not enforced yet: the note says so.
        assert_eq!(r["maxReviewRounds"], 3);
        assert!(r["notes"].as_array().unwrap()[0]
            .as_str()
            .unwrap()
            .contains("maxReviewRounds"));
        std::fs::create_dir_all(t.tc.ctx.workspace.root().join("Alpha")).unwrap();
        // A broken file: the defaults and the warning first in the notes.
        workspace_file(&t, "{nope");
        let r = t
            .call(Some(&t.a), "mira_get_workspace_rules", json!({}), 2)
            .unwrap();
        assert_eq!(r["maxWorkAgents"], 5);
        assert_eq!(r["projects"], json!(["Alpha", "p"]));
        assert!(r["notes"][0]
            .as_str()
            .unwrap()
            .contains("mira-bots.workspace.json kunne ikke læses"));
    }

    #[test]
    fn create_inherits_the_creators_project() {
        let t = setup();
        let r = t
            .call(Some(&t.a), "mira_create_ticket", json!({"title": "x"}), 1)
            .unwrap();
        assert_eq!(r["project"], "p");
        assert_eq!(
            t.ticket(r["id"].as_str().unwrap()).project,
            Some(ProjectRef::Existing("p".into()))
        );
        // A staff agent without assignTo: no project.
        let r = t
            .call(Some(&t.k), "mira_create_ticket", json!({"title": "y"}), 1)
            .unwrap();
        assert_eq!(r["project"], Value::Null);
        // With assignTo: the target's project.
        let r = t
            .call(
                Some(&t.k),
                "mira_create_ticket",
                json!({"title": "z", "assignTo": t.a}),
                1,
            )
            .unwrap();
        assert_eq!(r["project"], "p");
        // An explicit project must exist (spelling from disk).
        let r = t
            .call(
                Some(&t.k),
                "mira_create_ticket",
                json!({"title": "w", "project": "P"}),
                1,
            )
            .unwrap();
        assert_eq!(r["project"], "p");
        assert_eq!(
            t.call(
                Some(&t.k),
                "mira_create_ticket",
                json!({"title": "w", "project": "nej"}),
                1
            ),
            Err("Projektet «nej» findes ikke".into())
        );
        assert_eq!(
            t.call(
                Some(&t.k),
                "mira_create_ticket",
                json!({"title": "w", "project": "CON"}),
                1
            ),
            Err("Projektnavnet «CON» er ugyldigt: er et reserveret navn i Windows".into())
        );
        // skipReview absent: the workspace's reviewByDefault.
        workspace_file(&t, r#"{"reviewByDefault": false}"#);
        let r = t
            .call(Some(&t.a), "mira_create_ticket", json!({"title": "v"}), 2)
            .unwrap();
        assert_eq!(r["skipReview"], true);
    }

    #[test]
    fn create_with_new_project_is_refused_by_default() {
        let t = setup();
        let err = t
            .call(
                Some(&t.k),
                "mira_create_ticket",
                json!({"title": "x", "project": {"new": "nyt"}}),
                1,
            )
            .unwrap_err();
        assert_eq!(
            err,
            "Agenter må ikke oprette projekter i dette workspace (agentsMayCreateProjects) — bed brugeren oprette «nyt»"
        );
        assert!(!t.tc.ctx.workspace.root().join("nyt").exists());
        // A "new" project that already exists is that project.
        let r = t
            .call(
                Some(&t.k),
                "mira_create_ticket",
                json!({"title": "x", "project": {"new": "P"}}),
                1,
            )
            .unwrap();
        assert_eq!(r["project"], "p");
    }

    #[test]
    fn create_with_new_project_when_allowed() {
        let t = setup();
        workspace_file(&t, r#"{"agentsMayCreateProjects": true}"#);
        let r = t
            .call(
                Some(&t.k),
                "mira_create_ticket",
                json!({"title": "x", "project": {"new": "nyt"}}),
                1,
            )
            .unwrap();
        // Stays "new" until it is assigned or spawned with.
        assert_eq!(r["project"], json!({"new": "nyt"}));
        assert!(!t.tc.ctx.workspace.root().join("nyt").exists());
        // Assigned to the agent in p: wrong project.
        let err = t
            .call(
                Some(&t.k),
                "mira_assign_ticket",
                json!({"id": r["id"], "agentId": t.a}),
                2,
            )
            .unwrap_err();
        assert!(err.contains("ticketen hører til «nyt»"), "{err}");
        // A staff agent takes it as it is.
        let to_staff = t
            .call(
                Some(&t.k),
                "mira_assign_ticket",
                json!({"id": r["id"], "agentId": t.r}),
                3,
            )
            .unwrap();
        assert_eq!(to_staff["state"], "assigned");
        // {"new": "p"} with assignTo to the agent in p: realised as p.
        let r = t
            .call(
                Some(&t.k),
                "mira_create_ticket",
                json!({"title": "y", "project": {"new": "p"}, "assignTo": t.a}),
                4,
            )
            .unwrap();
        assert_eq!(r["project"], "p");
    }

    #[test]
    fn assign_tool_refuses_other_project_and_no_project() {
        let t = setup();
        let q = t.agent_in_project("q");
        let r = t
            .call(
                Some(&t.k),
                "mira_create_ticket",
                json!({"title": "x", "project": "p"}),
                1,
            )
            .unwrap();
        let name = t.tc.ctx.manager.lock().unwrap().get(&q).unwrap().name;
        assert_eq!(
            t.call(
                Some(&t.k),
                "mira_assign_ticket",
                json!({"id": r["id"], "agentId": q}),
                2
            ),
            Err(format!(
                "Agenten {name} står i projekt «q»; ticketen hører til «p»"
            ))
        );
        let none = t
            .call(Some(&t.k), "mira_create_ticket", json!({"title": "y"}), 1)
            .unwrap();
        assert_eq!(
            t.call(
                Some(&t.k),
                "mira_assign_ticket",
                json!({"id": none["id"], "agentId": t.a}),
                2
            ),
            Err(TicketError::ProjectRequired.to_string())
        );
        // The same ticket to a staff agent: fine.
        assert!(t
            .call(
                Some(&t.k),
                "mira_assign_ticket",
                json!({"id": none["id"], "agentId": t.r}),
                3
            )
            .is_ok());
        // Handing a ticket in progress on to another project is refused too.
        let mine = t.in_progress(&t.a, "egen", false);
        assert_eq!(
            t.call(
                Some(&t.a),
                "mira_handoff_ticket",
                json!({"ticketId": mine.id, "agentId": q}),
                4
            ),
            Err(format!(
                "Agenten {name} står i projekt «q»; ticketen hører til «p»"
            ))
        );
    }

    #[test]
    fn list_filters_by_project() {
        let t = setup();
        for (title, project) in [("a", json!("p")), ("b", Value::Null)] {
            t.call(
                Some(&t.k),
                "mira_create_ticket",
                json!({"title": title, "project": project}),
                1,
            )
            .unwrap();
        }
        let titles = |args: Value| -> Vec<String> {
            t.call(Some(&t.k), "mira_list_tickets", args, 2).unwrap()["tickets"]
                .as_array()
                .unwrap()
                .iter()
                .map(|x| x["title"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(titles(json!({"filter": "all", "project": "P"})), ["a"]);
        assert_eq!(titles(json!({"filter": "all", "project": "none"})), ["b"]);
        assert_eq!(titles(json!({"filter": "all"})).len(), 2);
        let r = t
            .call(
                Some(&t.k),
                "mira_list_tickets",
                json!({"filter": "backlog", "project": "p"}),
                2,
            )
            .unwrap();
        assert_eq!(r["project"], "p");
        assert_eq!(r["tickets"][0]["project"], "p");
    }

    #[test]
    fn list_projects_lists_folders_and_counts() {
        let t = setup();
        std::fs::create_dir_all(t.tc.ctx.workspace.root().join("q")).unwrap();
        std::fs::create_dir_all(t.tc.ctx.workspace.root().join(".mira-bots")).unwrap();
        let r = t
            .call(Some(&t.a), "mira_list_projects", json!({}), 1)
            .unwrap();
        let root = t.tc.ctx.workspace.root();
        assert_eq!(r["projectsRoot"], json!(root.to_string_lossy()));
        assert_eq!(
            r["projects"],
            json!([
                {"id": "p", "path": root.join("p").to_string_lossy(), "agents": 2},
                {"id": "q", "path": root.join("q").to_string_lossy(), "agents": 0}
            ])
        );
        // Every role may list the projects (common tool).
        assert!(t
            .call(Some(&t.r), "mira_list_projects", json!({}), 1)
            .is_ok());
    }

    #[test]
    fn spawn_tool_passes_project() {
        let t = setup();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = Arc::clone(&seen);
        t.tools.set_spawn_port(Arc::new(move |req: SpawnByProfile| {
            s2.lock().unwrap().push(req);
            Err("nej".to_string())
        }));
        for project in [json!("p"), json!({"new": "ny"}), Value::Null] {
            let _ = t.call(
                Some(&t.k),
                "mira_spawn_agent",
                json!({"profileId": "coder", "project": project}),
                1,
            );
        }
        assert_eq!(
            t.call(
                Some(&t.k),
                "mira_spawn_agent",
                json!({"profileId": "coder", "project": {"x": 1}}),
                1
            ),
            Err(mcp_tools::PROJECT_ERROR.into())
        );
        let got: Vec<Option<ProjectRef>> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.project.clone())
            .collect();
        assert_eq!(
            got,
            [
                Some(ProjectRef::Existing("p".into())),
                Some(ProjectRef::New { new: "ny".into() }),
                None
            ]
        );
    }

    #[test]
    fn add_report_ownership() {
        let t = setup();
        // Own ticket in progress (default ticket).
        assert_eq!(
            t.call(
                Some(&t.a),
                "mira_add_report",
                json!({"title":"T","body":"b"}),
                1
            ),
            Err("Du har ingen ticket i gang".into())
        );
        let mine = t.in_progress(&t.a, "x", false);
        let r = t
            .call(
                Some(&t.a),
                "mira_add_report",
                json!({"title":"Ændringer","body":"# Hej"}),
                2,
            )
            .unwrap();
        assert_eq!(r["ticketId"], json!(mine.id));
        assert_eq!(r["id"], "01");
        assert_eq!(r["author"], json!({"kind":"agent","agentId": t.a}));
        assert_eq!(r["path"], "reports/01-aendringer.md");
        // Someone else's ticket.
        assert_eq!(
            t.call(
                Some(&t.b),
                "mira_add_report",
                json!({"ticketId": mine.id, "title":"T","body":"b"}),
                3
            ),
            Err("Ticketen er tildelt en anden agent".into())
        );
        // The reviewer on the review ticket.
        t.call(
            Some(&t.a),
            "mira_submit_for_review",
            json!({"summary":"klar"}),
            4,
        )
        .unwrap();
        assert_eq!(
            t.ticket(&mine.id).reviewer_agent_id.as_deref(),
            Some(t.r.as_str())
        );
        let r = t
            .call(
                Some(&t.r),
                "mira_add_report",
                json!({"ticketId": mine.short_id(), "title":"Review","body":"ok"}),
                5,
            )
            .unwrap();
        assert_eq!(r["id"], "02");
        // The reviewer once the ticket is back in progress: refused.
        t.tc.ctx
            .mutate(|s| s.set_state(&mine.id, TicketState::InProgress, None, true, 6))
            .unwrap();
        assert_eq!(
            t.call(
                Some(&t.r),
                "mira_add_report",
                json!({"ticketId": mine.id, "title":"T","body":"b"}),
                7
            ),
            Err("Ticketen er tildelt en anden agent".into())
        );
        assert_eq!(t.ticket(&mine.id).reports.len(), 2);
    }

    #[test]
    fn submit_with_report_stores_both() {
        let t = setup();
        let tk = t.in_progress(&t.a, "x", false);
        // A bad report: nothing is submitted.
        assert_eq!(
            t.call(
                Some(&t.a),
                "mira_submit_for_review",
                json!({"summary":"s","report":"  "}),
                2
            ),
            Err("Rapporten må ikke være tom".into())
        );
        assert_eq!(t.ticket(&tk.id).state, TicketState::InProgress);
        assert!(t.ticket(&tk.id).reports.is_empty());
        // A refused submit writes no report.
        assert_eq!(
            t.call(
                Some(&t.b),
                "mira_submit_for_review",
                json!({"summary":"s","report":"r","ticketId": tk.id}),
                2
            ),
            Err("Ticketen er tildelt en anden agent".into())
        );
        assert!(t.ticket(&tk.id).reports.is_empty());
        let r = t
            .call(
                Some(&t.a),
                "mira_submit_for_review",
                json!({"summary":"Færdig","report":"# Rapport\nalt ok"}),
                3,
            )
            .unwrap();
        assert_eq!(r["state"], "review");
        assert_eq!(r["reportId"], "01");
        let now = t.ticket(&tk.id);
        assert_eq!(now.state, TicketState::Review);
        assert_eq!(now.reports.len(), 1);
        assert_eq!(now.reports[0].title, crate::config::REPORT_ON_SUBMIT_TITLE);
        let c = t.tc.ctx.get_report(&tk.id, "01").unwrap();
        assert_eq!(c.body, "# Rapport\nalt ok");
    }

    #[test]
    fn get_report_by_any_agent() {
        let t = setup();
        let tk = t.in_progress(&t.a, "x", false);
        t.call(
            Some(&t.a),
            "mira_add_report",
            json!({"title":"T","body":"æøå"}),
            1,
        )
        .unwrap();
        for agent in [&t.a, &t.b, &t.r, &t.k] {
            let r = t
                .call(
                    Some(agent),
                    "mira_get_report",
                    json!({"ticketId": tk.short_id(), "reportId":"01"}),
                    2,
                )
                .unwrap();
            assert_eq!(r["body"], "æøå");
            assert_eq!(r["report"]["title"], "T");
        }
        assert_eq!(
            t.call(
                Some(&t.b),
                "mira_get_report",
                json!({"ticketId": tk.id, "reportId":"02"}),
                2
            ),
            Err("Rapporten findes ikke".into())
        );
        assert_eq!(
            t.call(
                Some(&t.b),
                "mira_get_report",
                json!({"ticketId": "nope", "reportId":"01"}),
                2
            ),
            Err("Ticketen findes ikke".into())
        );
        // get_ticket carries the report list.
        let g = t
            .call(Some(&t.b), "mira_get_ticket", json!({"id": tk.id}), 3)
            .unwrap();
        assert_eq!(g["reports"][0]["id"], "01");
    }

    // ---- step 5c: handing a ticket in progress on ----

    #[test]
    fn coordinator_hands_its_ticket_in_progress_on_with_assign() {
        let mut t = setup();
        let tk = t.in_progress(&t.k.clone(), "Lav en HTML-side", false);
        t.tc.ctx
            .manager
            .lock()
            .unwrap()
            .set_detail(&t.k, Some(NOT_SUBMITTED_TEXT.into()));
        t.tc.clear();
        let _ = t.tc.sent();
        let r = t
            .call(
                Some(&t.k),
                "mira_assign_ticket",
                json!({"id": tk.short_id(), "agentId": t.a}),
                5,
            )
            .unwrap();
        assert_eq!(r["state"], "assigned");
        assert_eq!(r["assigneeAgentId"], json!(t.a));
        assert_eq!(r["queuePosition"], 0);
        let now = t.ticket(&tk.id);
        assert_eq!(now.history.last().unwrap().by, TicketActor::Agent);
        assert!(now
            .history
            .last()
            .unwrap()
            .note
            .as_deref()
            .unwrap()
            .starts_with("overdraget fra "));
        // The coordinator has nothing in progress any more; both queues are woken.
        assert_eq!(t.tc.ctx.read(|s| s.current_for_agent(&t.k)), None);
        // Review 5c W4/N8: it handed the ticket on itself, so its own status text stays and no
        // stop line is typed (below: only the two QueueChanged).
        assert!(t
            .detail(&t.k)
            .as_deref()
            .is_none_or(|d| !d.contains("givet videre")));
        let mut sent = t.tc.sent();
        sent.sort_by_key(|m| format!("{m:?}"));
        let mut want = vec![
            DispatchMsg::QueueChanged {
                agent_id: t.k.clone(),
            },
            DispatchMsg::QueueChanged {
                agent_id: t.a.clone(),
            },
        ];
        want.sort_by_key(|m| format!("{m:?}"));
        assert_eq!(sent, want);
        let info = t.tc.ctx.manager.lock().unwrap().get(&t.k).unwrap();
        assert_eq!(info.current_ticket_id, None);
    }

    #[test]
    fn coordinator_puts_its_ticket_in_progress_back_with_unassign() {
        let t = setup();
        let tk = t.in_progress(&t.k.clone(), "x", false);
        let r = t
            .call(Some(&t.k), "mira_unassign_ticket", json!({"id": tk.id}), 5)
            .unwrap();
        assert_eq!(r["state"], "backlog");
        assert_eq!(
            t.ticket(&tk.id).history.last().unwrap().note.as_deref(),
            Some(crate::tickets::state::RETURNED_NOTE)
        );
        // Someone else's ticket in progress stays where it is.
        let other = t.in_progress(&t.a.clone(), "y", false);
        assert_eq!(
            t.call(
                Some(&t.k),
                "mira_unassign_ticket",
                json!({"id": other.id}),
                6
            ),
            Err("Ticketen er tildelt en anden agent".into())
        );
        assert_eq!(
            t.call(
                Some(&t.k),
                "mira_assign_ticket",
                json!({"id": other.id, "agentId": t.b}),
                6
            ),
            Err("Ticketen er tildelt en anden agent".into())
        );
        assert_eq!(t.ticket(&other.id).state, TicketState::InProgress);
    }

    #[test]
    fn assign_tool_names_the_project_of_a_ticket_without_one() {
        let t = setup();
        let q = t.agent_in_project("q");
        let create = |title: &str, project: Value, now: u64| {
            let mut args = json!({ "title": title });
            if !project.is_null() {
                args["project"] = project;
            }
            t.call(Some(&t.k), "mira_create_ticket", args, now).unwrap()
        };
        // A ticket without a project goes to the agent in p with project "P" (disk spelling).
        let x = create("x", Value::Null, 1);
        assert_eq!(x["project"], Value::Null);
        let r = t
            .call(
                Some(&t.k),
                "mira_assign_ticket",
                json!({"id": x["id"], "agentId": t.a, "project": "P"}),
                2,
            )
            .unwrap();
        assert_eq!(r["state"], "assigned");
        let x_id = x["id"].as_str().unwrap();
        assert_eq!(
            t.ticket(x_id).project,
            Some(ProjectRef::Existing("p".into()))
        );
        // Another project than the agent's: refused, the ticket keeps no project.
        let y = create("y", Value::Null, 3);
        let y_id = y["id"].as_str().unwrap();
        let name_a = t.tc.ctx.manager.lock().unwrap().get(&t.a).unwrap().name;
        assert_eq!(
            t.call(
                Some(&t.k),
                "mira_assign_ticket",
                json!({"id": y_id, "agentId": t.a, "project": "q"}),
                4
            ),
            Err(format!(
                "Agenten {name_a} står i projekt «p»; ticketen hører til «q»"
            ))
        );
        assert_eq!(t.ticket(y_id).project, None);
        assert_eq!(t.ticket(y_id).state, TicketState::Backlog);
        // A new project needs agentsMayCreateProjects; nothing is created.
        assert_eq!(
            t.call(
                Some(&t.k),
                "mira_assign_ticket",
                json!({"id": y_id, "agentId": t.a, "project": {"new": "zz"}}),
                5
            ),
            Err(ProjectError::AgentsMayNotCreate("zz".into()).to_string())
        );
        assert!(!t.tc.ctx.workspace.root().join("zz").exists());
        // An unknown project id.
        assert_eq!(
            t.call(
                Some(&t.k),
                "mira_assign_ticket",
                json!({"id": y_id, "agentId": t.a, "project": "nope"}),
                5
            ),
            Err("Projektet «nope» findes ikke".into())
        );
        // A staff target takes the project too (coordination task in project q).
        t.call(
            Some(&t.k),
            "mira_assign_ticket",
            json!({"id": y_id, "agentId": t.r, "project": "q"}),
            6,
        )
        .unwrap();
        assert_eq!(
            t.ticket(y_id).project,
            Some(ProjectRef::Existing("q".into()))
        );
        // A ticket with a project: `project` is refused, even when it is the same.
        let z = create("z", json!("q"), 7);
        for p in [json!("q"), json!("p"), json!({"new": "q"})] {
            assert_eq!(
                t.call(
                    Some(&t.k),
                    "mira_assign_ticket",
                    json!({"id": z["id"], "agentId": q, "project": p}),
                    8
                ),
                Err("Ticketen har allerede projekt «q»".into())
            );
        }
        // Without `project` it goes to the agent in q as before.
        assert!(t
            .call(
                Some(&t.k),
                "mira_assign_ticket",
                json!({"id": z["id"], "agentId": q}),
                9
            )
            .is_ok());
    }

    #[test]
    fn assign_tool_creates_a_named_new_project_when_allowed() {
        let t = setup();
        workspace_file(&t, r#"{"agentsMayCreateProjects": true}"#);
        let x = t
            .call(Some(&t.k), "mira_create_ticket", json!({"title": "x"}), 1)
            .unwrap();
        // To a work agent in p: {"new": "nyt"} does not match its project → refused, no folder.
        let err = t
            .call(
                Some(&t.k),
                "mira_assign_ticket",
                json!({"id": x["id"], "agentId": t.a, "project": {"new": "nyt"}}),
                2,
            )
            .unwrap_err();
        assert!(err.contains("ticketen hører til «nyt»"), "{err}");
        assert!(!t.tc.ctx.workspace.root().join("nyt").exists());
        // To a staff agent: created and set.
        t.call(
            Some(&t.k),
            "mira_assign_ticket",
            json!({"id": x["id"], "agentId": t.r, "project": {"new": "nyt"}}),
            3,
        )
        .unwrap();
        assert!(t.tc.ctx.workspace.root().join("nyt").is_dir());
        assert_eq!(
            t.ticket(x["id"].as_str().unwrap()).project,
            Some(ProjectRef::Existing("nyt".into()))
        );
    }

    #[test]
    fn handoff_tool_names_the_project_of_a_ticket_without_one() {
        let t = setup();
        let c = &t.tc.ctx;
        let tk = c
            .mutate(|s| s.create_in("uden", "b", false, None, 1))
            .unwrap();
        c.mutate(|s| s.assign(&tk.id, &t.a, 2)).unwrap();
        c.mutate(|s| s.mark_dispatched(&tk.id, &t.a, 3)).unwrap();
        // Without a project a work agent cannot take it.
        assert_eq!(
            t.call(
                Some(&t.a),
                "mira_handoff_ticket",
                json!({"agentId": t.b}),
                4
            ),
            Err(TicketError::ProjectRequired.to_string())
        );
        // `project` without agentId (back to the backlog) is refused.
        assert_eq!(
            t.call(
                Some(&t.a),
                "mira_handoff_ticket",
                json!({"project": "p"}),
                4
            ),
            Err(PROJECT_NEEDS_TARGET.into())
        );
        assert_eq!(t.ticket(&tk.id).state, TicketState::InProgress);
        // With the target's project: handed on and the project set.
        let r = t
            .call(
                Some(&t.a),
                "mira_handoff_ticket",
                json!({"agentId": t.b, "project": "p"}),
                5,
            )
            .unwrap();
        assert_eq!(r["assigneeAgentId"], json!(t.b));
        assert_eq!(
            t.ticket(&tk.id).project,
            Some(ProjectRef::Existing("p".into()))
        );
        // A ticket in progress with a project: `project` is refused (handoff and assign).
        let mine = t.in_progress(&t.a, "med projekt", false);
        assert_eq!(
            t.call(
                Some(&t.a),
                "mira_handoff_ticket",
                json!({"ticketId": mine.id, "agentId": t.b, "project": "p"}),
                6
            ),
            Err("Ticketen har allerede projekt «p»".into())
        );
        assert_eq!(t.ticket(&mine.id).assignee_agent_id, Some(t.a.clone()));
    }

    #[test]
    fn handoff_tool_for_every_assignee() {
        let t = setup();
        // A coder (no coordinator role) gives its own ticket to another agent.
        let tk = t.in_progress(&t.a.clone(), "Forkert agent", false);
        let r = t
            .call(
                Some(&t.a),
                "mira_handoff_ticket",
                json!({"agentId": t.b}),
                5,
            )
            .unwrap();
        assert_eq!(r["id"], json!(tk.id));
        assert_eq!(r["state"], "assigned");
        assert_eq!(r["assigneeAgentId"], json!(t.b));
        assert!(r["message"].as_str().unwrap().contains("Afslut dit svar"));
        assert_eq!(t.tc.ctx.read(|s| s.current_for_agent(&t.a)), None);
        // A Stop of the old assignee marks nothing "ikke afleveret".
        assert_eq!(
            t.tc.ctx.mutate(|s| s.mark_not_submitted(&t.a, 6)).unwrap(),
            None
        );

        // Refused: someone else's ticket, no ticket in progress, itself, a dead/unknown target.
        let mine = t.in_progress(&t.b.clone(), "b's", false);
        let _ = t.tc.ctx.mutate(|s| s.give_back(&tk.id, None, 6));
        assert_eq!(
            t.call(
                Some(&t.a),
                "mira_handoff_ticket",
                json!({"ticketId": mine.short_id(), "agentId": t.a}),
                7
            ),
            Err("Ticketen er tildelt en anden agent".into())
        );
        assert_eq!(
            t.call(Some(&t.a), "mira_handoff_ticket", json!({}), 7),
            Err("Du har ingen ticket i gang".into())
        );
        assert_eq!(
            t.call(
                Some(&t.b),
                "mira_handoff_ticket",
                json!({"agentId": t.b}),
                7
            ),
            Err("Ticketen kan ikke gives videre til den agent, der allerede har den".into())
        );
        assert_eq!(
            t.call(
                Some(&t.b),
                "mira_handoff_ticket",
                json!({"agentId": "nope"}),
                7
            ),
            Err("Agenten kører ikke".into())
        );
        assert_eq!(t.ticket(&mine.id).state, TicketState::InProgress);

        // Without agentId: back to the backlog.
        let r = t
            .call(Some(&t.b), "mira_handoff_ticket", json!({}), 8)
            .unwrap();
        assert_eq!(r["state"], "backlog");
        assert_eq!(r["assigneeAgentId"], Value::Null);

        // A ticket in review cannot be handed on.
        let rev = t.review_for_r(&t.a.clone(), "til review");
        assert_eq!(
            t.call(
                Some(&t.a),
                "mira_handoff_ticket",
                json!({"ticketId": rev.id, "agentId": t.b}),
                9
            ),
            Err("Ticketen er ikke i gang".into())
        );
    }
}
