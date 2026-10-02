//! Tauri commands (contracts C.1 + C2.1 + C3.2) and the managed [`AppState`].
//!
//! All commands except `open_workplace`/`close_workplace` (async: window operations) are
//! synchronous and return `Result<T, String>`; errors are Danish, user-facing text. Locks are
//! held briefly and never while emitting.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_opener::OpenerExt;

use crate::agent::claude_path::find_claude;
use crate::agent::manager::{build_restart_spec, RestartSession};
use crate::agent::roles::prefix_for;
use crate::agent::workdir::{ensure_dir, next_agent_name};
use crate::agent::{
    now_ms, AgentError, AgentInfo, AgentManager, EventSink, SeatKind, SpawnContext, SpawnRequest,
};
use crate::app_settings::{self, AppSettings};
use crate::config::{
    moving_text, DEFAULT_PROFILE_ID, MOVED_NOTE, SETTINGS_FILE, STARTING_HINT_AFTER,
    SYSTEM_PROMPT_FILE,
};
use crate::diagnostics::{version_fields, Diagnostics, HookStats, VersionProbe};
use crate::events::{AgentOutputPayload, WorkplaceSelection, AGENTS_CHANGED, WORKPLACE_SELECT};
use crate::hooks::settings::write_profile_settings;
use crate::hooks::status::AgentStatus;
use crate::island::{self, IslandState};
use crate::mcp;
use crate::permissions::{Decision, PendingPermissions, PermissionRequestInfo};
use crate::profiles::model::{
    model_is_valid, new_custom_id, validate_overrides, AgentProfile, Effort, ProfileError,
    ProfileSnapshot, SpawnOverrides,
};
use crate::profiles::prompt::{profile_files_dir, write_profile_prompt};
use crate::profiles::ProfilesCtx;
use crate::projects::{self, AssignmentProject, Project, ProjectError, ProjectId, ProjectRef};
use crate::tickets::dispatcher::DispatchMsg;
use crate::tickets::model::{
    ReportAuthor, ReviewAssignment, Ticket, TicketError, TicketPatch, TicketReport, TicketState,
    TicketSummary, WorkspaceRules,
};
use crate::tickets::tools::{ticket_has_project, SpawnByProfile, SPAWN_UNAVAILABLE};
use crate::tickets::{prompt, ReportContent, TicketsCtx, AGENT_EXITED_NOTE, AGENT_STOPPED_NOTE};
use crate::workplace;
use crate::workspace::WorkspaceReader;

/// Locations resolved once in `setup`. The `claude` binary is not cached: it is looked up again
/// on every `get_app_info` and `spawn_agent` (cheap), so installing it while the app runs works.
#[derive(Clone, Debug)]
pub struct AppPaths {
    /// `mira-hook` binary; `None` disables spawning (`AgentError::HookExeNotFound`).
    pub hook_exe: Option<PathBuf>,
    /// `<data_dir>/settings.json` (hooks + permissions): written at startup for diagnostics and
    /// as a fallback; agents get their profile's file instead (`profile_files_dir`).
    pub settings_json: PathBuf,
    /// `mira-mcp` binary; `None`: agents are spawned without `--mcp-config` and
    /// `--append-system-prompt-file` (no tools).
    pub mcp_exe: Option<PathBuf>,
    /// `<data_dir>/mcp.json`, passed to `claude --mcp-config` (only with `mcp_exe`).
    pub mcp_config: PathBuf,
    /// `<data_dir>/system-prompt.md`: the common part of the system prompt (diagnostics); agents
    /// get their profile's file instead.
    pub system_prompt: PathBuf,
    /// Named pipe (Windows) or socket path (Linux dev) the hook exe connects to.
    pub pipe_name: String,
    /// App data directory (`%APPDATA%\dk.mira.bots` on Windows).
    pub data_dir: PathBuf,
    /// `<app_log_dir>/mira-bots.log` (`%LOCALAPPDATA%\dk.mira.bots\logs` on Windows); `None` if
    /// the log dir could not be resolved.
    pub log_file: Option<PathBuf>,
    /// The projects root (`<home>/mira-bots/projects` or the app setting, plan4b A.1): project
    /// folders, the profile store and the workspace file. Fixed while the app runs.
    pub projects_root: PathBuf,
    /// `<data_dir>/app-settings.json` (the projects root setting; applies after a restart).
    pub app_settings: PathBuf,
    /// `<data_dir>/tickets.json`.
    pub tickets_file: PathBuf,
    /// `<projects_root>/.mira-bots/profiles`: the profile store (one JSON file per profile).
    pub profiles_dir: PathBuf,
    /// `<data_dir>/profiles`: the rendered per-profile files (`<id>/settings.json`,
    /// `<id>/system-prompt.md`), passed to claude with `--settings` and
    /// `--append-system-prompt-file`.
    pub profile_files_dir: PathBuf,
}

/// Managed state shared by commands, the pipe handler and the PTY threads.
pub struct AppState {
    pub manager: Arc<Mutex<AgentManager>>,
    pub pending: Arc<Mutex<PendingPermissions>>,
    pub paths: AppPaths,
    pub island: IslandState,
    /// Set by the pipe server once the pipe/socket exists; cleared if the server stops for good.
    /// While false, `spawn_agent` refuses to start agents (their hooks would reach nothing).
    pub pipe_ready: Arc<AtomicBool>,
    /// Receives PTY output/exit from the manager's threads (see `lib.rs::tauri_sink`).
    pub sink: EventSink,
    /// Hook frame counters, shared with the pipe handler.
    pub hook_stats: Arc<HookStats>,
    /// Result of the one-shot background `claude --version` probe.
    pub claude_version: Arc<Mutex<VersionProbe>>,
    /// Agent/tab to select when a newly created workplace window asks
    /// (`take_workplace_selection`).
    pub workplace_select: Mutex<Option<WorkplaceSelection>>,
    /// Tickets (service, manager links, emits, dispatcher inbox).
    pub tickets: Arc<TicketsCtx>,
    /// Warning from loading `tickets.json` (corrupt file renamed etc.), for Diagnostics.
    pub tickets_warning: Option<String>,
    /// Agent profiles (store + `profiles-changed`).
    pub profiles: Arc<ProfilesCtx>,
    /// `<projects_root>/mira-bots.workspace.json`, read on demand (shared with `TicketsCtx`).
    pub workspace: Arc<WorkspaceReader>,
    /// Profiles copied from the step 1–5 agents root at this start (Diagnostik).
    pub profiles_migrated: usize,
}

/// `AppInfo` (C.1), camelCase.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub claude_path: Option<String>,
    pub hook_exe: Option<String>,
    /// The app's settings.json (`hooksJson` up to step 3).
    pub settings_json: String,
    pub pipe_name: String,
    /// `rules.max_work_agents`.
    pub max_agents: usize,
    pub version: String,
    /// Whether the pipe server is listening (see [`AppState::pipe_ready`]).
    pub pipe_ready: bool,
    /// `rules.max_staff_agents`.
    pub max_staff_agents: usize,
    pub projects_root: String,
    /// The effective workspace rules (plan4b A.4).
    pub rules: WorkspaceRules,
}

/// `get_agent_output` result: same shape as the `agent-output` event payload.
pub type AgentOutputSnapshot = AgentOutputPayload;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn path_string(p: &Option<PathBuf>) -> Option<String> {
    p.as_ref().map(|p| p.to_string_lossy().into_owned())
}

impl AppState {
    /// Recomputes the `claude` lookup on every call.
    pub fn app_info(&self) -> AppInfo {
        let rules = self.workspace.snapshot().rules;
        AppInfo {
            claude_path: path_string(&find_claude()),
            hook_exe: path_string(&self.paths.hook_exe),
            settings_json: self.paths.settings_json.to_string_lossy().into_owned(),
            pipe_name: self.paths.pipe_name.clone(),
            max_agents: rules.max_work_agents,
            version: env!("CARGO_PKG_VERSION").to_string(),
            pipe_ready: self.pipe_ready.load(Ordering::Acquire),
            max_staff_agents: rules.max_staff_agents,
            projects_root: self.paths.projects_root.to_string_lossy().into_owned(),
            rules,
        }
    }

    /// Everything the Diagnostics panel shows (C2.3). Recomputes the `claude` lookup.
    pub fn diagnostics(&self) -> Diagnostics {
        let (
            claude_version,
            claude_version_note,
            claude_code_args_supported,
            claude_code_mcp_supported,
        ) = version_fields(&lock(&self.claude_version));
        let ws = self.workspace.snapshot();
        Diagnostics {
            claude_path: path_string(&find_claude()),
            claude_version,
            claude_version_note,
            claude_code_args_supported,
            claude_code_mcp_supported,
            hook_exe: path_string(&self.paths.hook_exe),
            settings_path: self.paths.settings_json.to_string_lossy().into_owned(),
            settings_exists: self.paths.settings_json.is_file(),
            mcp_exe: path_string(&self.paths.mcp_exe),
            mcp_config_path: self.paths.mcp_config.to_string_lossy().into_owned(),
            mcp_config_exists: self.paths.mcp_config.is_file(),
            system_prompt_path: self.paths.system_prompt.to_string_lossy().into_owned(),
            tool_calls: self.hook_stats.tool_calls(),
            tool_errors: self.hook_stats.tool_errors(),
            last_tool_call: self.hook_stats.last_tool_call(),
            auto_review_on_stop: ws.rules.auto_review_on_stop,
            pipe_name: self.paths.pipe_name.clone(),
            pipe_ready: self.pipe_ready.load(Ordering::Acquire),
            pipe_note: pipe_note(&self.paths.pipe_name),
            frames_received: self.hook_stats.received(),
            frames_unknown_session: self.hook_stats.unknown(),
            last_hook_event: self.hook_stats.last_event(),
            log_path: path_string(&self.paths.log_file),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            platform: crate::platform::name().into(),
            projects_root: self.paths.projects_root.to_string_lossy().into_owned(),
            running_agents: lock(&self.manager).running_count(),
            tickets_path: self.paths.tickets_file.to_string_lossy().into_owned(),
            tickets_warning: self.tickets_warning.clone(),
            tickets_read_only: self.tickets.read(|s| s.is_read_only()),
            tickets_total: self.tickets.read(|s| s.len()),
            profiles_path: self.paths.profiles_dir.to_string_lossy().into_owned(),
            profiles_loaded: self.profiles.read(|s| s.len()),
            profiles_warning: self.profiles.read(|s| s.warning().map(str::to_string)),
            review_assignments_open: self.tickets.read(|s| s.review_assignments().len()),
            tickets_escalated: self.tickets.read(|s| s.escalated_count()),
            reports_total: self.tickets.read(|s| s.report_count()),
            workspace_file_path: self.workspace.path().to_string_lossy().into_owned(),
            workspace_file_exists: ws.file_exists,
            workspace_warning: ws.warning,
            projects_total: crate::projects::list_projects(&self.paths.projects_root).len(),
            profiles_migrated: self.profiles_migrated,
        }
    }

    /// Emits the full agent list as `agents-changed`.
    pub fn emit_agents(&self, app: &AppHandle) {
        emit_agent_list(app, &self.manager);
    }
}

/// Emits the full agent list as `agents-changed` (lock released before emitting).
pub fn emit_agent_list(app: &AppHandle, manager: &Mutex<AgentManager>) {
    let list = lock(manager).list();
    if let Err(e) = app.emit(AGENTS_CHANGED, &list) {
        log::error!("emit {AGENTS_CHANGED}: {e}");
    }
}

/// A spawn on a work seat without a project (and without a ticket that has one).
pub const WORK_SEAT_NEEDS_PROJECT: &str =
    "En arbejdsplads kræver et projekt — vælg et projekt til agenten";

/// Where a new agent runs (plan4b C4b.3): its folder, name and project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    pub cwd: PathBuf,
    pub name: String,
    pub project: Option<ProjectId>,
}

/// `maxAgentsPerProject` (0 = unlimited): refuses when `max` live work agents (other than
/// `except`) already run in `project`.
pub fn check_project_limit(
    manager: &Mutex<AgentManager>,
    project: &str,
    max: usize,
    except: Option<&str>,
) -> Result<(), AgentError> {
    if max == 0 {
        return Ok(());
    }
    let n = lock(manager)
        .live_work_in_project(project)
        .iter()
        .filter(|a| Some(a.id.as_str()) != except)
        .count();
    if n >= max {
        return Err(AgentError::ProjectLimit {
            project: project.to_string(),
            max,
        });
    }
    Ok(())
}

/// The folder, name and project of a new agent (plan4b A.1, C4b.4): a staff seat runs in the
/// projects root without a project; a work seat needs `project`, which is realised (a `New` is
/// created only with `may_create`), then `maxAgentsPerProject` is checked. The name is the next
/// free `<prefix>-<nn>` among all known agents, whatever the folder.
// TODO(windows-verify): a work agent starts in <root>\<project>, a staff agent in <root>; two
// agents in one project are coder-01 and coder-02 (plan4b D.79).
pub fn resolve_placement(
    state: &AppState,
    profile: &AgentProfile,
    seat: SeatKind,
    project: Option<&ProjectRef>,
    may_create: bool,
) -> Result<Placement, String> {
    let root = &state.paths.projects_root;
    let (cwd, project) = match seat {
        SeatKind::Staff => {
            ensure_dir(root).map_err(AgentError::Io)?;
            (root.clone(), None)
        }
        SeatKind::Work => {
            let r = project.ok_or(WORK_SEAT_NEEDS_PROJECT)?;
            let p = projects::realize(root, r, may_create)?;
            let max = state.workspace.rules().max_agents_per_project;
            check_project_limit(&state.manager, &p.id, max, None)?;
            (PathBuf::from(&p.path), Some(p.id))
        }
    };
    let prefix = prefix_for(&profile.roles, profile.is_specialist());
    let name = next_agent_name(prefix, &lock(&state.manager).names());
    Ok(Placement { cwd, name, project })
}

/// Takes (and clears) the pending workplace selection.
pub fn take_selection(slot: &Mutex<Option<WorkplaceSelection>>) -> Option<WorkplaceSelection> {
    lock(slot).take()
}

/// Sidebar tabs `open_workplace` may select.
pub const WORKPLACE_TABS: [&str; 4] = ["permissions", "diagnostics", "tickets", "agents"];

/// `None` (no tab) or one of [`WORKPLACE_TABS`].
pub fn check_tab(tab: Option<&str>) -> Result<(), String> {
    match tab {
        Some(t) if !WORKPLACE_TABS.contains(&t) => Err(format!("Ukendt fane: {t}")),
        _ => Ok(()),
    }
}

/// Seat kinds `open_workplace` may open the spawn dialog for.
pub const WORKPLACE_SPAWN_KINDS: [&str; 2] = ["work", "staff"];

/// `None` (no spawn dialog) or one of [`WORKPLACE_SPAWN_KINDS`].
pub fn check_spawn(spawn: Option<&str>) -> Result<(), String> {
    match spawn {
        Some(s) if !WORKPLACE_SPAWN_KINDS.contains(&s) => Err(format!("Ukendt pladstype: {s}")),
        _ => Ok(()),
    }
}

/// Diagnostik's `pipeNote`: unix: the too-long-socket-path text (the server did not start);
/// Windows: `None`.
fn pipe_note(name: &str) -> Option<String> {
    #[cfg(unix)]
    {
        crate::pipe::unix_socket::check_length(std::path::Path::new(name)).err()
    }
    #[cfg(windows)]
    {
        let _ = name;
        None
    }
}

/// Refuses to start agents while the pipe server is not listening.
pub fn check_pipe_ready(ready: &AtomicBool) -> Result<(), AgentError> {
    if ready.load(Ordering::Acquire) {
        Ok(())
    } else {
        Err(AgentError::PipeNotReady)
    }
}

/// Answers a pending request from the UI. Only resolves the request (and extends the
/// whitelist): the pipe handler emits `permission-resolved` itself when it receives the decision.
pub fn respond(
    manager: &Mutex<AgentManager>,
    pending: &Mutex<PendingPermissions>,
    request_id: &str,
    allow: bool,
    always: bool,
) -> Result<(), String> {
    let decision = if allow {
        Decision::Allow
    } else {
        Decision::Deny
    };
    let info = lock(pending)
        .resolve(request_id, decision)
        .ok_or_else(|| "Anmodningen er udløbet".to_string())?;
    if allow && always {
        if let Err(e) = lock(manager).whitelist_add(&info.agent_id, &info.tool_name) {
            log::warn!("whitelist_add for agent {}: {e}", info.agent_id);
        }
    }
    Ok(())
}

/// Kills the agent and releases its pending permission requests (the handlers answer `none` and
/// emit `permission-resolved` themselves).
pub fn stop(
    manager: &Mutex<AgentManager>,
    pending: &Mutex<PendingPermissions>,
    agent_id: &str,
) -> Result<AgentInfo, String> {
    let info = lock(manager).stop(agent_id)?;
    lock(pending).remove_for_agent(agent_id);
    Ok(info)
}

// ---- tickets (C3.2): logic without Tauri types, so it is unit tested ----

/// Whether the agent exists and has not exited.
pub fn agent_live(manager: &Mutex<AgentManager>, agent_id: &str) -> bool {
    lock(manager)
        .get(agent_id)
        .is_some_and(|a| !matches!(a.status, AgentStatus::Exited { .. }))
}

/// The ticket's current assignee (if any).
fn assignee_of(t: &TicketsCtx, id: &str) -> Result<Option<String>, String> {
    t.read(|s| s.get(id))
        .map(|tk| tk.assignee_agent_id)
        .ok_or_else(|| TicketError::NotFound.into())
}

pub fn ticket_create(
    t: &TicketsCtx,
    title: &str,
    body: &str,
    skip_review: bool,
    project: Option<ProjectRef>,
) -> Result<TicketSummary, String> {
    let now = now_ms();
    t.mutate(|s| s.create_in(title, body, skip_review, project, now))
        .map(|tk| TicketSummary::from(&tk))
}

pub fn ticket_update(
    t: &TicketsCtx,
    id: &str,
    patch: TicketPatch,
) -> Result<TicketSummary, String> {
    let now = now_ms();
    t.mutate(|s| s.update(id, patch, now))
        .map(|tk| TicketSummary::from(&tk))
}

/// Deletes the ticket and its report folder.
pub fn ticket_delete(t: &TicketsCtx, id: &str) -> Result<(), String> {
    t.delete_ticket(id)
}

/// backlog/rejected → the agent's queue (at the end). The agent must exist and not have exited.
/// A ticket in progress is handed over (step 5c): it leaves its agent ("overdraget fra … til
/// …") and goes last in the new agent's queue.
///
/// Step 4b (plan4b A.2): a work agent only takes tickets of its own project
/// ([`projects::assignment_target`]); a ticket with `{"new": name}` matching the agent's
/// project becomes that project in the same save. Staff seats take any ticket.
// TODO(windows-verify): a ticket of project A dropped on an agent in B is refused with the
// WrongProject text; one without a project on a work agent asks "Hvilket projekt?" (plan4b D.82).
pub fn ticket_assign(t: &TicketsCtx, id: &str, agent_id: &str) -> Result<TicketSummary, String> {
    ticket_assign_in(t, id, agent_id, None)
}

/// [`ticket_assign`] with the project picked in "Hvilket projekt?" (step 4b batch 3): only for a
/// ticket without a project ("Ticketen har allerede projekt «x»" otherwise, as for the agents'
/// `project` on `mira_assign_ticket`); it is checked against the agent like the ticket's own
/// project, created when new (the user may create projects) and set in the same save.
pub fn ticket_assign_in(
    t: &TicketsCtx,
    id: &str,
    agent_id: &str,
    project: Option<ProjectRef>,
) -> Result<TicketSummary, String> {
    let info = lock(&t.manager)
        .get(agent_id)
        .filter(|a| !matches!(a.status, AgentStatus::Exited { .. }))
        .ok_or(TicketError::AgentNotLive)?;
    let before = t.read(|s| s.get(id)).ok_or(TicketError::NotFound)?;
    if let (Some(cur), Some(_)) = (&before.project, &project) {
        return Err(ticket_has_project(cur));
    }
    let effective = project.as_ref().or(before.project.as_ref());
    let target = projects::assignment_target(
        effective,
        info.seat_kind,
        info.project.as_deref(),
        &info.name,
    )?;
    let now = now_ms();
    let root = t.workspace.root();
    let set = match (&project, target) {
        // Picked now: always stored (in the spelling on disk).
        (Some(p), _) => Some(projects::realize(root, p, true)?.id),
        (None, AssignmentProject::Unchanged) => None,
        // The user may create projects: a missing folder is created.
        (None, AssignmentProject::Set(p)) => {
            Some(projects::realize(root, &ProjectRef::New { new: p }, true)?.id)
        }
    };
    if before.state != TicketState::InProgress {
        let tk = t.mutate(|s| s.assign_in(id, agent_id, set, now))?;
        t.notify([agent_id]);
        return Ok(TicketSummary::from(&tk));
    }
    let old = before.assignee_agent_id.unwrap_or_default();
    let (from_name, to_name) = {
        let m = lock(&t.manager);
        let name = |a: &str| m.get(a).map_or_else(|| a.to_string(), |i| i.name);
        (name(&old), name(agent_id))
    };
    let tk = t.mutate(|s| s.handoff_in(id, agent_id, None, (&from_name, &to_name), set, now))?;
    // The user took it from the old agent: tell it to stop (review 5c W4).
    t.handed_over(&old, &tk, Some(&to_name), true);
    t.notify([old.as_str(), agent_id]);
    Ok(TicketSummary::from(&tk))
}

/// assigned → backlog; a ticket in progress is put back too ("lagt tilbage", step 5c).
pub fn ticket_unassign(t: &TicketsCtx, id: &str) -> Result<TicketSummary, String> {
    let before = t.read(|s| s.get(id)).ok_or(TicketError::NotFound)?;
    let old = before.assignee_agent_id;
    let now = now_ms();
    let tk = t.mutate(|s| s.unassign(id, now))?;
    if before.state == TicketState::InProgress {
        if let Some(agent) = old.as_deref() {
            // Taken from a working agent by the user: tell it to stop (review 5c W4).
            t.handed_over(agent, &tk, None, true);
        }
    }
    t.notify(old);
    Ok(TicketSummary::from(&tk))
}

pub fn ticket_reorder(
    t: &TicketsCtx,
    agent_id: &str,
    ticket_ids: &[String],
) -> Result<Vec<TicketSummary>, String> {
    let q = t.mutate(|s| s.reorder(agent_id, ticket_ids))?;
    t.notify([agent_id]);
    Ok(q)
}

/// Manual move (C3.3). `agent_live` refers to the ticket's assignee.
pub fn ticket_set_state(
    t: &TicketsCtx,
    id: &str,
    target: TicketState,
    note: Option<String>,
) -> Result<TicketSummary, String> {
    let before = t.read(|s| s.get(id)).ok_or(TicketError::NotFound)?;
    if before.state == TicketState::Review && target == TicketState::Rejected {
        // Dragging to "Afvist" is a rejection: same rules as the reject button (W1).
        return ticket_reject(t, id, note.as_deref().unwrap_or_default());
    }
    let old = before.assignee_agent_id;
    let live = old.as_deref().is_some_and(|a| agent_live(&t.manager, a));
    let now = now_ms();
    let tk = t.mutate(|s| s.set_state(id, target, note, live, now))?;
    // The ticket left in-progress: the agent's "no submission"/"turn failed" hint is stale (N3).
    if before.state == TicketState::InProgress && tk.state != TicketState::InProgress {
        if let Some(agent) = old.as_deref() {
            t.clear_stale_detail(agent);
        }
    }
    t.notify(old.into_iter().chain(tk.assignee_agent_id.clone()));
    // "Send til review" by hand: find a reviewer (plan5 A.6).
    if tk.state == TicketState::Review {
        t.route_reviews();
    }
    Ok(TicketSummary::from(&tk))
}

pub fn ticket_approve(t: &TicketsCtx, id: &str) -> Result<TicketSummary, String> {
    let now = now_ms();
    t.mutate(|s| s.approve(id, now))
        .map(|tk| TicketSummary::from(&tk))
}

/// review → rejected → first in the same agent's queue (agent live and still in the ticket's
/// project) or the backlog.
pub fn ticket_reject(t: &TicketsCtx, id: &str, note: &str) -> Result<TicketSummary, String> {
    if note.trim().is_empty() {
        return Err(TicketError::NeedsNote.into());
    }
    let old = assignee_of(t, id)?;
    // W1: back to the sender only while it is live and still in the ticket's project.
    let to = t.reject_return(id);
    let now = now_ms();
    let tk = t.mutate(|s| s.reject(id, note.trim(), to, now))?;
    t.notify(old);
    t.route_reviews();
    Ok(TicketSummary::from(&tk))
}

/// "Send igen": hands the ticket to the dispatcher, which checks whether it can go now.
pub fn ticket_redispatch(t: &TicketsCtx, id: &str) -> Result<(), String> {
    assignee_of(t, id)?;
    if t.send(DispatchMsg::Redispatch {
        ticket_id: id.to_string(),
    }) {
        Ok(())
    } else {
        Err("Ticket-afsendelsen kører ikke — genstart mira-bots".into())
    }
}

/// "Bed om aflevering": the ticket must be in progress with a live agent; the dispatcher then
/// types the nudge line (C4.7) once the agent is idle and no delivery runs (otherwise it only
/// logs).
pub fn ticket_request_submission(t: &TicketsCtx, id: &str) -> Result<(), String> {
    let tk = t.read(|s| s.get(id)).ok_or(TicketError::NotFound)?;
    if tk.state != TicketState::InProgress {
        return Err(TicketError::NotInProgress.into());
    }
    let live = tk
        .assignee_agent_id
        .as_deref()
        .is_some_and(|a| agent_live(&t.manager, a));
    if !live {
        return Err(TicketError::AgentNotLive.into());
    }
    if t.send(DispatchMsg::RequestSubmission {
        ticket_id: tk.id.clone(),
    }) {
        Ok(())
    } else {
        Err("Ticket-afsendelsen kører ikke — genstart mira-bots".into())
    }
}

/// Only backlog tickets and rejected tickets without an agent can start a new agent.
pub fn ticket_for_spawn(t: &TicketsCtx, id: &str) -> Result<Ticket, String> {
    let tk = t.read(|s| s.get(id)).ok_or(TicketError::NotFound)?;
    match (tk.state, &tk.assignee_agent_id) {
        (TicketState::Backlog, _) | (TicketState::Rejected, None) => Ok(tk),
        (from, _) => Err(TicketError::IllegalTransition {
            from,
            to: TicketState::Assigned,
        }
        .into()),
    }
}

/// After a spawn with the ticket line as positional prompt: the dispatcher waits for the session
/// (`SpawnedWithTicket` first, so a quick `SessionStart` cannot start a second delivery), then the
/// ticket becomes the new agent's queue head.
pub fn ticket_attach_spawned(
    t: &TicketsCtx,
    ticket_id: &str,
    agent_id: &str,
) -> Result<(), String> {
    t.send(DispatchMsg::SpawnedWithTicket {
        agent_id: agent_id.to_string(),
        ticket_id: ticket_id.to_string(),
    });
    let now = now_ms();
    t.mutate(|s| s.assign(ticket_id, agent_id, now))
        .map(|_| ())
        .map_err(|e| format!("Agenten startede, men ticketen kunne ikke tildeles: {e}"))
}

#[tauri::command]
pub fn ui_ready(state: State<'_, AppState>) -> Result<(), String> {
    lock(&state.pending).set_ui_ready();
    Ok(())
}

#[tauri::command]
pub fn get_app_info(state: State<'_, AppState>) -> Result<AppInfo, String> {
    Ok(state.app_info())
}

#[tauri::command]
pub fn list_agents(state: State<'_, AppState>) -> Result<Vec<AgentInfo>, String> {
    Ok(lock(&state.manager).list())
}

#[tauri::command]
pub fn get_diagnostics(state: State<'_, AppState>) -> Result<Diagnostics, String> {
    Ok(state.diagnostics())
}

/// The profile `profile_id` names (default [`DEFAULT_PROFILE_ID`]); unknown → "Profilen findes
/// ikke".
pub fn resolve_profile(
    profiles: &ProfilesCtx,
    profile_id: Option<&str>,
) -> Result<AgentProfile, String> {
    let id = profile_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_PROFILE_ID);
    profiles
        .get(id)
        .ok_or_else(|| ProfileError::NotFound.into())
}

/// The spawn request for `profile` with `overrides` (validated: `model_is_valid`, effort enum).
/// `seat_kind` defaults to the profile's `defaultSeat`; a staff seat needs a profile with a staff
/// role ([`AgentProfile::check_seat`], 5c B).
pub fn spawn_request(
    profile: &AgentProfile,
    overrides: Option<SpawnOverrides>,
    cwd: PathBuf,
    prompt: Option<String>,
    seat_kind: Option<SeatKind>,
) -> Result<SpawnRequest, String> {
    let overrides = validate_overrides(overrides.unwrap_or_default())?;
    let seat_kind = seat_kind.unwrap_or(profile.default_seat);
    profile.check_seat(seat_kind)?;
    Ok(SpawnRequest {
        cwd,
        name: None,
        project: None,
        prompt,
        seat_kind,
        profile: profile.snapshot(&overrides),
    })
}

/// Writes the profile's `settings.json` and `system-prompt.md` under `<data_dir>/profiles/<id>/`
/// (the hook exe placeholder when it was not found). Returns both paths.
pub fn write_profile_files(
    paths: &AppPaths,
    profile: &AgentProfile,
    rules: &WorkspaceRules,
) -> std::io::Result<(PathBuf, PathBuf)> {
    let hook = paths
        .hook_exe
        .clone()
        .unwrap_or_else(|| PathBuf::from("mira-hook-not-found"));
    let settings = write_profile_settings(&paths.data_dir, &hook, profile)?;
    let prompt = write_profile_prompt(&paths.data_dir, profile, rules)?;
    Ok((settings, prompt))
}

/// Everything a claude start needs that can fail, in this order: hook exe, pipe ready, claude,
/// then the profile's files (rewritten from `profile` so edits apply to the next start, together
/// with mcp.json when mira-mcp exists). Without `profile` (deleted since the agent started) the
/// files already on disk are used, if any.
pub fn spawn_context(
    state: &AppState,
    profile_id: &str,
    profile: Option<&AgentProfile>,
) -> Result<SpawnContext, String> {
    let hook_exe = state
        .paths
        .hook_exe
        .as_ref()
        .ok_or(AgentError::HookExeNotFound)?;
    check_pipe_ready(&state.pipe_ready)?;
    let claude = find_claude().ok_or(AgentError::ClaudeNotFound)?;
    let io = |e: std::io::Error| String::from(AgentError::Io(e));
    let (settings_json, prompt_file) = match profile {
        Some(p) => {
            let settings =
                write_profile_settings(&state.paths.data_dir, hook_exe, p).map_err(io)?;
            let rules = state.workspace.rules();
            let prompt = write_profile_prompt(&state.paths.data_dir, p, &rules).map_err(io)?;
            (settings, prompt)
        }
        None => {
            let dir = profile_files_dir(&state.paths.data_dir, profile_id);
            let settings = dir.join(SETTINGS_FILE);
            if !settings.is_file() {
                return Err(ProfileError::NotFound.into());
            }
            (settings, dir.join(SYSTEM_PROMPT_FILE))
        }
    };
    let (mcp_config, system_prompt) = match &state.paths.mcp_exe {
        Some(mcp_exe) => (
            Some(mcp::write_mcp_json(&state.paths.data_dir, mcp_exe).map_err(io)?),
            Some(prompt_file).filter(|p| p.is_file()),
        ),
        // Without the MCP server the system prompt would ask for tools that do not exist.
        None => (None, None),
    };
    Ok(SpawnContext {
        claude,
        settings_json,
        mcp_config,
        system_prompt,
        pipe_name: state.paths.pipe_name.clone(),
    })
}

/// Sets the manager's seat limits from the workspace rules (read before every spawn, plan4b
/// A.4) and returns the rules.
pub fn apply_limits(state: &AppState) -> WorkspaceRules {
    let rules = state.workspace.rules();
    lock(&state.manager).set_limits(rules.max_work_agents, rules.max_staff_agents);
    rules
}

/// The checks of a spawn in the order of plan4b C4b.4: seat limits from the workspace rules,
/// [`spawn_context`] (hook/pipe/claude/profile files), the seat limit, then the placement (a
/// refused spawn never creates a project folder: it is realised last).
pub fn prepare_spawn(
    state: &AppState,
    profile: &AgentProfile,
    seat: SeatKind,
    project: Option<&ProjectRef>,
    may_create: bool,
) -> Result<(SpawnContext, Placement), String> {
    apply_limits(state);
    let ctx = spawn_context(state, &profile.id, Some(profile))?;
    lock(&state.manager).can_spawn(seat)?;
    let placement = resolve_placement(state, profile, seat, project, may_create)?;
    Ok((ctx, placement))
}

/// [`spawn_request`] in `placement` (folder, name, project).
fn placed_request(
    profile: &AgentProfile,
    overrides: Option<SpawnOverrides>,
    placement: Placement,
    prompt: Option<String>,
    seat: SeatKind,
) -> Result<SpawnRequest, String> {
    let mut req = spawn_request(profile, overrides, placement.cwd, prompt, Some(seat))?;
    req.name = Some(placement.name);
    req.project = placement.project;
    Ok(req)
}

/// Starts the agent in an already resolved folder, emits `agents-changed` and schedules the
/// Starting hint.
fn spawn_prepared(
    app: &AppHandle,
    state: &AppState,
    ctx: &SpawnContext,
    req: SpawnRequest,
) -> Result<AgentInfo, String> {
    let info = lock(&state.manager).spawn(req, ctx, Arc::clone(&state.sink))?;
    log::info!(
        "spawned agent {} ({}) in {} profile={} roles={:?} model={:?} effort={:?} seat={:?} session={} pid={:?}",
        info.id,
        info.name,
        info.cwd,
        info.profile_id,
        info.roles,
        info.model,
        info.effort,
        info.seat_kind,
        info.session_id,
        info.pid
    );
    state.emit_agents(app);
    schedule_starting_hint(app.clone(), Arc::clone(&state.manager), info.id.clone());
    // A new reviewer may take reviews that were waiting (plan5 A.6).
    state.tickets.route_reviews();
    Ok(info)
}

/// The shared core of `spawn_agent` and `mira_spawn_agent`. `may_create`: whether a
/// `{"new": …}` project may be created (always for the user; `agentsMayCreateProjects` for an
/// agent).
#[allow(clippy::too_many_arguments)]
pub fn spawn_core(
    app: &AppHandle,
    state: &AppState,
    profile_id: Option<String>,
    overrides: Option<SpawnOverrides>,
    project: Option<ProjectRef>,
    prompt: Option<String>,
    seat_kind: Option<SeatKind>,
    may_create: bool,
) -> Result<AgentInfo, String> {
    let profile = resolve_profile(&state.profiles, profile_id.as_deref())?;
    // Validate the overrides and the seat before anything is written or a folder created.
    validate_overrides(overrides.clone().unwrap_or_default())?;
    let seat = seat_kind.unwrap_or(profile.default_seat);
    profile.check_seat(seat)?;
    let (ctx, placement) = prepare_spawn(state, &profile, seat, project.as_ref(), may_create)?;
    let req = placed_request(&profile, overrides, placement, prompt, seat)?;
    spawn_prepared(app, state, &ctx, req)
}

/// `profileId` null → `coder`; `overrides` replace the profile's model/effort for this agent;
/// `project` (a work seat needs one; `{"new": name}` creates the folder; ignored on a staff
/// seat, which runs in the projects root); `seatKind` null → the profile's `defaultSeat`. After [`STARTING_HINT_AFTER`] without a hook event, the agent gets the
/// Starting hint.
// TODO(windows-verify): a spawn from a profile starts claude with the profile's settings file
// and `--model`/`--effort` when set; the TUI header shows them (plan5 D.50).
#[tauri::command]
pub fn spawn_agent(
    app: AppHandle,
    state: State<'_, AppState>,
    profile_id: Option<String>,
    overrides: Option<SpawnOverrides>,
    project: Option<ProjectRef>,
    prompt: Option<String>,
    seat_kind: Option<SeatKind>,
) -> Result<AgentInfo, String> {
    spawn_core(
        &app, &state, profile_id, overrides, project, prompt, seat_kind, true,
    )
}

/// Like `spawn_agent`, with a backlog ticket: the ticket file is written in the agent's folder
/// first, the ticket line is the positional prompt, and the ticket becomes the agent's queue
/// head (it goes in progress once the dispatcher sees the prompt submitted). On a failed spawn
/// the file is removed again and the ticket is untouched.
// TODO(windows-verify): "Start med ticket" starts the agent with the line as positional prompt and
// UserPromptSubmit (or a busy status) confirms it within 8 s after SessionStart; with the trust
// dialog first, the clock only starts at SessionStart (plan D.32).
#[tauri::command]
pub fn spawn_agent_with_ticket(
    app: AppHandle,
    state: State<'_, AppState>,
    ticket_id: String,
    profile_id: Option<String>,
    overrides: Option<SpawnOverrides>,
    project: Option<ProjectRef>,
    seat_kind: Option<SeatKind>,
) -> Result<AgentInfo, String> {
    spawn_with_ticket_core(
        &app, &state, &ticket_id, profile_id, overrides, project, seat_kind, true,
    )
}

/// The project a spawn with `ticket` on `seat` runs in (plan4b punkt 9): a work seat takes the
/// ticket's project when it has one, otherwise `project` ([`TicketError::ProjectRequired`]
/// when both are missing); a staff seat has none.
pub fn spawn_project(
    ticket: &Ticket,
    seat: SeatKind,
    project: Option<ProjectRef>,
) -> Result<Option<ProjectRef>, String> {
    match seat {
        SeatKind::Staff => Ok(None),
        SeatKind::Work => ticket
            .project
            .clone()
            .or(project)
            .map(Some)
            .ok_or_else(|| TicketError::ProjectRequired.into()),
    }
}

/// Whether a spawn with `ticket` may create its project folder (W3). The ticket's own
/// `{"new": …}` was authorised when the ticket got it (by the user, or by an agent the
/// workspace allowed to), so it is always realised; `may_create` (`agentsMayCreateProjects` for
/// `mira_spawn_agent`) only governs a `project` the caller passes for a ticket without one.
pub fn spawn_may_create(ticket: &Ticket, may_create: bool) -> bool {
    may_create || matches!(ticket.project, Some(ProjectRef::New { .. }))
}

/// The ticket of a spawn on a work seat gets the (realised) project of the placement before its
/// file is written, so the file shows it (plan4b punkt 9). Unchanged on a staff seat.
pub fn ticket_in_placement(
    t: &TicketsCtx,
    ticket: Ticket,
    placement: &Placement,
) -> Result<Ticket, String> {
    let Some(p) = &placement.project else {
        return Ok(ticket);
    };
    let realized = Some(ProjectRef::Existing(p.clone()));
    if ticket.project == realized {
        return Ok(ticket);
    }
    let now = now_ms();
    t.mutate(|s| s.set_project(&ticket.id, realized, now))
}

/// The delivery of the first ticket of a new agent (plan4b A.5/A.6): `## Delt projekt` when
/// other live work agents already run in the project (real work deliveries only), the project
/// list for a coordination task.
pub fn first_delivery(
    state: &AppState,
    seat: SeatKind,
    roles: &[crate::agent::Role],
    project: Option<&str>,
    rules: &WorkspaceRules,
) -> prompt::TicketDelivery {
    let delivery = prompt::TicketDelivery::for_agent(seat, roles);
    if !delivery.is_work() {
        let ids = projects::list_projects(&state.paths.projects_root)
            .into_iter()
            .map(|p| p.id)
            .collect();
        return delivery.with_projects(ids, rules.agents_may_create_projects);
    }
    match project {
        Some(p) => {
            let others = lock(&state.manager)
                .live_work_in_project(p)
                .into_iter()
                .map(|a| a.name)
                .collect();
            delivery.with_shared(p, others)
        }
        None => delivery,
    }
}

/// The shared core of `spawn_agent_with_ticket` and `mira_spawn_agent` with `firstTicketId`.
/// A ticket without a project (or with `{"new": …}`) gets the agent's project before the file
/// is written; if the spawn then fails the project stays (an empty folder may remain; plan4b
/// A.2).
#[allow(clippy::too_many_arguments)]
pub fn spawn_with_ticket_core(
    app: &AppHandle,
    state: &AppState,
    ticket_id: &str,
    profile_id: Option<String>,
    overrides: Option<SpawnOverrides>,
    project: Option<ProjectRef>,
    seat_kind: Option<SeatKind>,
    may_create: bool,
) -> Result<AgentInfo, String> {
    let mut ticket = ticket_for_spawn(&state.tickets, ticket_id)?;
    let profile = resolve_profile(&state.profiles, profile_id.as_deref())?;
    validate_overrides(overrides.clone().unwrap_or_default())?;
    let seat = seat_kind.unwrap_or(profile.default_seat);
    profile.check_seat(seat)?;
    let may_create = spawn_may_create(&ticket, may_create);
    let project = spawn_project(&ticket, seat, project)?;
    let (ctx, placement) = prepare_spawn(state, &profile, seat, project.as_ref(), may_create)?;
    ticket = ticket_in_placement(&state.tickets, ticket, &placement)?;
    let rules = state.workspace.rules();
    // On a staff seat the first ticket is a coordination task (5c C.1).
    let delivery = first_delivery(
        state,
        seat,
        &profile.roles,
        placement.project.as_deref(),
        &rules,
    );
    let file = prompt::write_ticket_file(&placement.cwd, &ticket, now_ms(), &delivery)
        .map_err(|e| format!("Kunne ikke skrive ticket-fil: {e}"))?;
    let line = prompt::line_for(&ticket, &delivery);
    let req = placed_request(&profile, overrides, placement, Some(line), seat)?;
    let info = match spawn_prepared(app, state, &ctx, req) {
        Ok(info) => info,
        Err(e) => {
            if let Err(rm) = std::fs::remove_file(&file) {
                log::debug!("removing {} after a failed spawn: {rm}", file.display());
            }
            return Err(e);
        }
    };
    ticket_attach_spawned(&state.tickets, &ticket.id, &info.id)?;
    log::info!(
        "agent {} started with ticket {}",
        info.id,
        ticket.short_id()
    );
    Ok(lock(&state.manager).get(&info.id).unwrap_or(info))
}

/// `mira_spawn_agent` (the tool's [`crate::tickets::tools::SpawnPort`]): the same path and seat
/// limits as the UI. With `first_ticket_id` (full or short id of a backlog ticket) it is
/// `spawn_agent_with_ticket`. No overrides; `project` as in the UI (the ticket's project wins).
pub fn spawn_for_tool(app: &AppHandle, req: SpawnByProfile) -> Result<AgentInfo, String> {
    let state = app
        .try_state::<AppState>()
        .ok_or_else(|| SPAWN_UNAVAILABLE.to_string())?;
    // An agent may create a project only when the workspace allows it (plan4b A.2).
    let may_create = state.workspace.rules().agents_may_create_projects;
    match req.first_ticket_id {
        Some(id) => {
            let ticket = state
                .tickets
                .read(|s| s.get_by_any_id(&id))
                .ok_or(TicketError::NotFound)?;
            spawn_with_ticket_core(
                app,
                &state,
                &ticket.id,
                Some(req.profile_id),
                None,
                req.project,
                req.seat_kind,
                may_create,
            )
        }
        None => spawn_core(
            app,
            &state,
            Some(req.profile_id),
            None,
            req.project,
            None,
            req.seat_kind,
            may_create,
        ),
    }
}

// ---- model/effort change = restart with --resume (plan5 A.5) ----

/// The gate of `set_agent_model`/`set_agent_effort`, in this order: the agent exists and has not
/// exited ("Agenten kører ikke"), it is Idle without a ticket in progress ("Agenten arbejder").
pub fn check_restartable(info: Option<&AgentInfo>) -> Result<AgentInfo, AgentError> {
    let info = info.ok_or(AgentError::NotRunning)?;
    if matches!(info.status, AgentStatus::Exited { .. }) {
        return Err(AgentError::NotRunning);
    }
    if info.status != AgentStatus::Idle || info.current_ticket_id.is_some() {
        return Err(AgentError::Working);
    }
    Ok(info.clone())
}

/// The model/effort a restart asks for. A model change keeps the current effort (if it is a
/// known level); an effort change keeps the current model (observed or requested; `None` gives
/// `--model default`). `model: Some(None)` = back to the default model.
pub fn restart_values(
    info: &AgentInfo,
    model: Option<Option<String>>,
    effort: Option<Effort>,
) -> (Option<String>, Option<Effort>) {
    let model = match model {
        Some(m) => m,
        None => info.model.clone(),
    };
    let effort = effort.or_else(|| info.effort.as_deref().and_then(Effort::parse));
    (model, effort)
}

/// The resume request for `info` with the new values (same cwd, seat and profile snapshot).
pub fn restart_request(
    info: &AgentInfo,
    model: Option<String>,
    effort: Option<Effort>,
) -> SpawnRequest {
    let project = info.project.clone();
    restart_request_in(info, model, effort, PathBuf::from(&info.cwd), project)
}

/// [`restart_request`] in another folder and project ("Flyt til projekt…", plan4b A.3); the
/// name stays.
pub fn restart_request_in(
    info: &AgentInfo,
    model: Option<String>,
    effort: Option<Effort>,
    cwd: PathBuf,
    project: Option<String>,
) -> SpawnRequest {
    SpawnRequest {
        cwd,
        name: Some(info.name.clone()),
        project,
        prompt: None,
        seat_kind: info.seat_kind,
        profile: ProfileSnapshot {
            profile_id: info.profile_id.clone(),
            profile_name: info.profile_name.clone(),
            roles: info.roles.clone(),
            specialist: info.specialist,
            model,
            effort,
        },
    }
}

/// Gate → profile files → kill + restart under the same agent id → `AgentRestarting` to the
/// dispatcher → `agents-changed`. Pending permission requests of the old session are released.
/// `--resume` only when the session has had a turn; otherwise a fresh session with a new id
/// (no transcript exists yet, `--resume` would fail; review5 N1).
fn restart_agent(
    app: &AppHandle,
    state: &AppState,
    agent_id: &str,
    model: Option<Option<String>>,
    effort: Option<Effort>,
) -> Result<AgentInfo, String> {
    restart_with(app, state, agent_id, model, effort, None)
}

/// [`restart_agent`], optionally in another folder and project (`moved`; plan4b A.3).
fn restart_with(
    app: &AppHandle,
    state: &AppState,
    agent_id: &str,
    model: Option<Option<String>>,
    effort: Option<Effort>,
    moved: Option<(PathBuf, String)>,
) -> Result<AgentInfo, String> {
    let info = check_restartable(lock(&state.manager).get(agent_id).as_ref())?;
    let (model, effort) = restart_values(&info, model, effort);
    let profile = state.profiles.get(&info.profile_id);
    let ctx = spawn_context(state, &info.profile_id, profile.as_ref())?;
    let req = match &moved {
        Some((cwd, project)) => restart_request_in(
            &info,
            model.clone(),
            effort,
            cwd.clone(),
            Some(project.clone()),
        ),
        None => restart_request(&info, model.clone(), effort),
    };
    lock(&state.pending).remove_for_agent(agent_id);
    let (result, old_pty, resumed) = {
        let mut m = lock(&state.manager);
        // Checked again under the lock: the agent may have started working meanwhile.
        if let Err(e) = check_restartable(m.get(agent_id).as_ref()) {
            return Err(e.into());
        }
        // Decided under the same lock as the restart (a turn may have ended meanwhile).
        let session = m.restart_session(agent_id).ok_or(AgentError::NotRunning)?;
        let resumed = matches!(session, RestartSession::Resume(_));
        let spec = build_restart_spec(&req, &ctx, &session, agent_id);
        let (result, old_pty) = m.restart(
            agent_id,
            spec,
            &session,
            model.clone(),
            effort.map(|e| e.as_str().to_string()),
            Arc::clone(&state.sink),
        );
        if let (Ok(_), Some((_, project))) = (&result, &moved) {
            // W2: say where it goes instead of "nye indstillinger".
            m.set_start_text(agent_id, moving_text(project));
        }
        (result, old_pty, resumed)
    };
    // Close the old pseudo terminal only after the manager lock is released (it may block).
    drop(old_pty);
    state.tickets.send(DispatchMsg::AgentRestarting {
        agent_id: agent_id.to_string(),
    });
    match result {
        Ok(info) => {
            log::info!(
                "agent {agent_id} restarted ({}) with model={} effort={}",
                if resumed { "resume" } else { "fresh session" },
                model.as_deref().unwrap_or("default"),
                effort.map_or("-", Effort::as_str)
            );
            let info = match &moved {
                Some((_, project)) => {
                    let mut m = lock(&state.manager);
                    m.set_project(agent_id, Some(project.clone()));
                    log::info!("agent {agent_id} moved to project {project}");
                    m.get(agent_id).unwrap_or(info)
                }
                None => info,
            };
            // W2: a move into a git project shows the trust dialog; the hint points at it.
            schedule_starting_hint(
                app.clone(),
                Arc::clone(&state.manager),
                agent_id.to_string(),
            );
            state.emit_agents(app);
            Ok(info)
        }
        Err(e) => {
            log::warn!("restart of agent {agent_id} failed: {e}");
            if let Err(e) = state.tickets.release_agent(agent_id, AGENT_EXITED_NOTE) {
                log::warn!("releasing the tickets of agent {agent_id} failed: {e}");
            }
            state.emit_agents(app);
            Err(e.into())
        }
    }
}

/// Restarts the agent (`--resume`, or a fresh session without a turn yet) with the new model (`null` → `--model default`). Only when
/// it is Idle without a ticket in progress.
#[tauri::command]
pub fn set_agent_model(
    app: AppHandle,
    state: State<'_, AppState>,
    agent_id: String,
    model: Option<String>,
) -> Result<AgentInfo, String> {
    check_restartable(lock(&state.manager).get(&agent_id).as_ref())?;
    let model = model
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty());
    if model.as_deref().is_some_and(|m| !model_is_valid(m)) {
        return Err(ProfileError::UnknownModel.into());
    }
    restart_agent(&app, &state, &agent_id, Some(model), None)
}

/// Restarts the agent (like `set_agent_model`) with the new effort (a concrete level; no flag can reset
/// it to the model's default).
#[tauri::command]
pub fn set_agent_effort(
    app: AppHandle,
    state: State<'_, AppState>,
    agent_id: String,
    effort: String,
) -> Result<AgentInfo, String> {
    check_restartable(lock(&state.manager).get(&agent_id).as_ref())?;
    let effort = Effort::parse(effort.trim()).ok_or(ProfileError::UnknownEffort)?;
    restart_agent(&app, &state, &agent_id, None, Some(effort))
}

// ---- projects (plan4b C4b.4) ----

/// The gate of `move_agent_to_project`, in the order of C4b.4 (without the restart, so it is
/// unit tested): running, idle, no ticket in progress → work seat → empty queue (or `force`) →
/// the project exists (a `New` is created: the user may) → not the agent's own → the
/// `maxAgentsPerProject` limit (the agent itself not counted). Returns the agent and the
/// project.
pub fn move_gate(
    state: &AppState,
    agent_id: &str,
    project: &ProjectRef,
    force: bool,
) -> Result<(AgentInfo, Project), String> {
    let info = check_restartable(lock(&state.manager).get(agent_id).as_ref())?;
    if info.seat_kind != SeatKind::Work {
        return Err(AgentError::StaffHasNoProject.into());
    }
    if info.queue_length > 0 && !force {
        return Err(AgentError::QueueNotEmpty(info.queue_length).into());
    }
    let p = projects::realize(&state.paths.projects_root, project, true)?;
    if info
        .project
        .as_deref()
        .is_some_and(|cur| projects::same_id(cur, &p.id))
    {
        return Err(AgentError::SameProject(p.id).into());
    }
    let max = state.workspace.rules().max_agents_per_project;
    check_project_limit(&state.manager, &p.id, max, Some(agent_id))?;
    Ok((info, p))
}

/// "Flyt til projekt…" (plan4b A.3): restarts the agent with `--resume` in the project's
/// folder (the conversation is kept, research4b §2). Only on a work seat, idle without a ticket
/// in progress; queued tickets go to the backlog with [`MOVED_NOTE`] when `force`, otherwise the
/// move is refused.
// TODO(windows-verify): the agent restarts with --resume in the new folder, the conversation is
// kept, the trust dialog comes for a git project, and the next ticket file lands in the new
// folder (plan4b D.81).
// TODO(macos-verify): "Flyt til projekt…" and "Skift model" restart with --resume: the old process
// group dies, the new one starts, the conversation is kept, the trust dialog shows in a git project
// (plan7 M.17).
#[tauri::command]
pub fn move_agent_to_project(
    app: AppHandle,
    state: State<'_, AppState>,
    agent_id: String,
    project: ProjectRef,
    force: bool,
) -> Result<AgentInfo, String> {
    let (info, p) = move_gate(&state, &agent_id, &project, force)?;
    if info.queue_length > 0 {
        state.tickets.release_queue(&agent_id, MOVED_NOTE)?;
    }
    restart_with(
        &app,
        &state,
        &agent_id,
        None,
        None,
        Some((PathBuf::from(&p.path), p.id)),
    )
}

/// The project folders under the projects root.
#[tauri::command]
pub fn list_projects(state: State<'_, AppState>) -> Result<Vec<Project>, String> {
    Ok(projects::list_projects(&state.paths.projects_root))
}

/// Creates a project folder (the user may always).
#[tauri::command]
pub fn create_project(state: State<'_, AppState>, name: String) -> Result<Project, String> {
    let p = projects::create_project(&state.paths.projects_root, name.trim())?;
    log::info!("project {} created", p.id);
    Ok(p)
}

/// The folder `open_project_folder` opens: the root (`None`, created if missing) or an existing
/// project.
pub fn project_folder(root: &std::path::Path, project: Option<&str>) -> Result<PathBuf, String> {
    match project.map(str::trim).filter(|s| !s.is_empty()) {
        None => {
            ensure_dir(root).map_err(|e| format!("Kunne ikke oprette projektroden: {e}"))?;
            Ok(root.to_path_buf())
        }
        Some(id) => projects::find_project(root, id)
            .map(|p| PathBuf::from(p.path))
            .ok_or_else(|| ProjectError::NotFound(id.to_string()).into()),
    }
}

/// Opens the projects root (`project` null) or a project folder in the file manager.
#[tauri::command]
pub fn open_project_folder(
    app: AppHandle,
    state: State<'_, AppState>,
    project: Option<String>,
) -> Result<(), String> {
    let dir = project_folder(&state.paths.projects_root, project.as_deref())?;
    app.opener()
        .open_path(dir.to_string_lossy().into_owned(), None::<&str>)
        .map_err(|e| format!("Kunne ikke åbne mappen: {e}"))
}

/// [`store_projects_root`] with a relative path.
pub const PROJECTS_ROOT_NOT_ABSOLUTE: &str = "Projektroden skal være en absolut sti";

/// Stores a new projects root in `app-settings.json` (created if missing). It applies after a
/// restart of mira-bots (plan4b A.1); returns the stored path.
pub fn store_projects_root(data_dir: &std::path::Path, path: &str) -> Result<String, String> {
    let path = path.trim();
    if path.is_empty() {
        return Err("Vælg en mappe til projektroden".into());
    }
    let dir = PathBuf::from(path);
    // N5: a relative path would resolve against whatever the working directory is next start.
    if !dir.is_absolute() {
        return Err(PROJECTS_ROOT_NOT_ABSOLUTE.into());
    }
    if !dir.is_dir() {
        std::fs::create_dir_all(&dir).map_err(|e| format!("Mappen kunne ikke oprettes: {e}"))?;
    }
    let settings = AppSettings {
        projects_root: Some(path.to_string()),
    };
    app_settings::save(data_dir, &settings)
        .map_err(|e| format!("Indstillingen kunne ikke gemmes: {e}"))?;
    Ok(path.to_string())
}

// TODO(windows-verify): the path is stored in %APPDATA%\dk.mira.bots\app-settings.json and
// only used after a restart (plan4b D.86).
// TODO(macos-verify): the folder picker ("Vælg projektrod…") opens in front of the workplace and
// returns a path; a project root with spaces in its name works (hooks exec form, quoted statusLine)
// (plan7 M.16).
#[tauri::command]
pub fn set_projects_root(state: State<'_, AppState>, path: String) -> Result<String, String> {
    let stored = store_projects_root(&state.paths.data_dir, &path)?;
    log::info!("projects root set to {stored} (applies after a restart)");
    Ok(stored)
}

// ---- profiles (plan5 C5.4) ----

/// `save_profile` without Tauri: an empty id becomes a new `custom-<8 hex>`; validation and the
/// file in the store; `profiles-changed`. The caller writes the per-profile files.
pub fn profile_save(
    profiles: &ProfilesCtx,
    mut profile: AgentProfile,
    now: u64,
) -> Result<AgentProfile, String> {
    if profile.id.trim().is_empty() {
        profile.id = new_custom_id();
    }
    profiles
        .mutate(|s| s.save(profile, now))
        .map_err(Into::into)
}

/// Rewrites the per-profile files after a save/reset (failures are only logged: the next spawn
/// writes them again).
fn refresh_profile_files(state: &AppState, profile: &AgentProfile) {
    if let Err(e) = write_profile_files(&state.paths, profile, &state.workspace.rules()) {
        log::warn!("could not write the files of profile {}: {e}", profile.id);
    }
}

#[tauri::command]
pub fn list_profiles(state: State<'_, AppState>) -> Result<Vec<AgentProfile>, String> {
    Ok(state.profiles.list())
}

#[tauri::command]
pub fn get_profile(state: State<'_, AppState>, id: String) -> Result<AgentProfile, String> {
    state
        .profiles
        .get(&id)
        .ok_or_else(|| ProfileError::NotFound.into())
}

/// Validates (C5.6) and stores the profile; `kind` is derived from the id. Running agents keep
/// their snapshot; the change applies to the next spawn.
#[tauri::command]
pub fn save_profile(
    state: State<'_, AppState>,
    profile: AgentProfile,
) -> Result<AgentProfile, String> {
    let saved = profile_save(&state.profiles, profile, now_ms())?;
    log::info!("profile {} saved", saved.id);
    refresh_profile_files(&state, &saved);
    Ok(saved)
}

/// Only custom profiles.
#[tauri::command]
pub fn delete_profile(state: State<'_, AppState>, id: String) -> Result<(), String> {
    state.profiles.mutate(|s| s.delete(&id))?;
    log::info!("profile {id} deleted");
    Ok(())
}

/// Only built-in profiles: writes the default again.
#[tauri::command]
pub fn reset_builtin_profile(
    state: State<'_, AppState>,
    id: String,
) -> Result<AgentProfile, String> {
    let p = state.profiles.mutate(|s| s.reset_builtin(&id, now_ms()))?;
    log::info!("profile {id} reset");
    refresh_profile_files(&state, &p);
    Ok(p)
}

/// After [`STARTING_HINT_AFTER`], sets the Starting hint if the agent is still waiting for its
/// first hook event, and emits `agents-changed`.
// TODO(windows-verify): the trust dialog is what holds the first hook back; after "Yes" in the
// terminal, SessionStart arrives and the agent turns Idle (the hint disappears) (plan D.16).
fn schedule_starting_hint(app: AppHandle, manager: Arc<Mutex<AgentManager>>, id: String) {
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(STARTING_HINT_AFTER).await;
        let hinted = lock(&manager).apply_starting_hint(&id, now_ms()).is_some();
        if hinted {
            log::info!("agent {id}: no hook event after {STARTING_HINT_AFTER:?}; showing hint");
            emit_agent_list(&app, &manager);
        }
    });
}

#[tauri::command]
pub fn stop_agent(
    app: AppHandle,
    state: State<'_, AppState>,
    agent_id: String,
) -> Result<(), String> {
    stop(&state.manager, &state.pending, &agent_id)?;
    log::info!("stopped agent {agent_id}");
    if let Err(e) = state.tickets.release_agent(&agent_id, AGENT_STOPPED_NOTE) {
        log::warn!("releasing the tickets of agent {agent_id} failed: {e}");
    }
    state.emit_agents(&app);
    Ok(())
}

#[tauri::command]
pub fn remove_agent(
    app: AppHandle,
    state: State<'_, AppState>,
    agent_id: String,
) -> Result<(), String> {
    let pty = lock(&state.manager).remove(&agent_id)?;
    // Close the pseudo terminal only after the manager lock is released (it may block).
    drop(pty);
    // Usually a no-op (released when it exited); covers a remove racing the exit report.
    if let Err(e) = state.tickets.release_agent(&agent_id, AGENT_STOPPED_NOTE) {
        log::warn!("releasing the tickets of agent {agent_id} failed: {e}");
    }
    state.emit_agents(&app);
    Ok(())
}

#[tauri::command]
pub fn write_agent_input(
    state: State<'_, AppState>,
    agent_id: String,
    data: String,
    user_initiated: bool,
) -> Result<(), String> {
    write_terminal_input(&state.manager, &agent_id, data.as_bytes(), user_initiated)
}

/// The user's own typing is recorded so the ticket dispatcher does not type into it; the
/// terminal's automatic replies (`user_initiated` false) are written without a timestamp.
fn write_terminal_input(
    manager: &Mutex<AgentManager>,
    agent_id: &str,
    bytes: &[u8],
    user_initiated: bool,
) -> Result<(), String> {
    let mut m = lock(manager);
    if user_initiated {
        m.write_user_input(agent_id, bytes)
    } else {
        m.write_input(agent_id, bytes)
    }
    .map_err(Into::into)
}

#[tauri::command]
pub fn resize_agent_pty(
    state: State<'_, AppState>,
    agent_id: String,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    lock(&state.manager)
        .resize(&agent_id, cols, rows)
        .map_err(Into::into)
}

#[tauri::command]
pub fn get_agent_output(
    state: State<'_, AppState>,
    agent_id: String,
) -> Result<AgentOutputSnapshot, String> {
    let (seq, bytes) = lock(&state.manager).output_snapshot(&agent_id)?;
    Ok(AgentOutputPayload {
        agent_id,
        seq,
        data_base64: BASE64.encode(bytes),
    })
}

#[tauri::command]
pub fn list_pending_permissions(
    state: State<'_, AppState>,
) -> Result<Vec<PermissionRequestInfo>, String> {
    Ok(lock(&state.pending).list())
}

#[tauri::command]
pub fn respond_permission(
    state: State<'_, AppState>,
    request_id: String,
    allow: bool,
    always: bool,
) -> Result<(), String> {
    respond(&state.manager, &state.pending, &request_id, allow, always)
}

/// Resizes and re-centres the island. `width`/`height` are logical px and may be fractional
/// (they come from DOM measurements).
#[tauri::command]
pub fn resize_island(
    app: AppHandle,
    state: State<'_, AppState>,
    width: f64,
    height: f64,
) -> Result<(), String> {
    let w = width.round().clamp(1.0, f64::from(island::MAX_SIZE.0)) as u32;
    let h = height.round().clamp(1.0, f64::from(island::MAX_SIZE.1)) as u32;
    let window = app
        .get_webview_window(island::LABEL)
        .ok_or_else(|| "Island-vinduet findes ikke".to_string())?;
    *lock(&state.island.last) = (w, h);
    island::place(&window, w, h).map_err(|e| format!("Kunne ikke placere islanden: {e}"))
}

/// Opens (or focuses) the workplace window and selects `agent_id` and/or the sidebar `tab`
/// ("permissions" | "diagnostics" | "tickets") in it; `spawn` ("work" | "staff") makes it open the
/// "Ny agent" dialog for that seat kind (the island's "+ Ny agent"). Async on purpose: creating a window from a
/// synchronous command deadlocks on Windows (research2 §5).
// TODO(windows-verify): the "n i review" chip in the non-focusable island opens the workplace on
// the Tickets tab, both when the window is created and when it is already open (plan D.36).
#[tauri::command]
pub async fn open_workplace(
    app: AppHandle,
    state: State<'_, AppState>,
    agent_id: Option<String>,
    tab: Option<String>,
    spawn: Option<String>,
) -> Result<(), String> {
    check_tab(tab.as_deref())?;
    check_spawn(spawn.as_deref())?;
    let selection = WorkplaceSelection {
        agent_id,
        tab,
        spawn,
    };
    *lock(&state.workplace_select) = Some(selection.clone());
    let created =
        workplace::open_or_focus(&app).map_err(|e| format!("Kunne ikke åbne Workplace: {e}"))?;
    log::info!(
        "workplace {} (select {:?}, tab {:?}, spawn {:?})",
        if created { "created" } else { "focused" },
        selection.agent_id,
        selection.tab,
        selection.spawn
    );
    if !created
        && (selection.agent_id.is_some() || selection.tab.is_some() || selection.spawn.is_some())
    {
        // A new window fetches the selection itself via take_workplace_selection. The slot is
        // kept here too, in case the existing window is still loading and misses the event.
        if let Err(e) = app.emit_to(workplace::LABEL, WORKPLACE_SELECT, &selection) {
            log::debug!("emit {WORKPLACE_SELECT}: {e}");
        }
    }
    Ok(())
}

/// Closes the workplace window (Cmd+W on macOS). `true` when a window was open. Async like
/// `open_workplace`: window operations from a synchronous command can deadlock on Windows.
#[tauri::command]
pub async fn close_workplace(app: AppHandle) -> Result<bool, String> {
    let closed = workplace::close(&app).map_err(|e| format!("Kunne ikke lukke Workplace: {e}"))?;
    log::info!("workplace close requested (window open: {closed})");
    Ok(closed)
}

#[tauri::command]
pub fn take_workplace_selection(
    state: State<'_, AppState>,
) -> Result<Option<WorkplaceSelection>, String> {
    Ok(take_selection(&state.workplace_select))
}

/// Opens the agent's working folder in the file manager (opener plugin, called from Rust: no JS
/// capability needed).
// TODO(windows-verify): opens Explorer on the right folder (plan D.22).
// TODO(macos-verify): "Åbn mappe" opens Finder on the folder (plan7 M.13).
#[tauri::command]
pub fn open_agent_folder(
    app: AppHandle,
    state: State<'_, AppState>,
    agent_id: String,
) -> Result<(), String> {
    let cwd = lock(&state.manager)
        .get(&agent_id)
        .map(|a| a.cwd)
        .ok_or_else(|| AgentError::NotFound.to_string())?;
    app.opener()
        .open_path(cwd, None::<&str>)
        .map_err(|e| format!("Kunne ikke åbne mappen: {e}"))
}

/// Opens the log folder (created first if needed).
// TODO(windows-verify): opens Explorer on %LOCALAPPDATA%\dk.mira.bots\logs (plan D.18/D.22).
// TODO(macos-verify): "Åbn logmappe" opens Finder on ~/Library/Logs/dk.mira.bots (plan7 M.13).
#[tauri::command]
pub fn open_log_dir(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let dir = match state
        .paths
        .log_file
        .as_deref()
        .and_then(std::path::Path::parent)
    {
        Some(d) => d.to_path_buf(),
        None => app
            .path()
            .app_log_dir()
            .map_err(|e| format!("Kunne ikke finde logmappen: {e}"))?,
    };
    std::fs::create_dir_all(&dir).map_err(|e| format!("Kunne ikke oprette logmappen: {e}"))?;
    app.opener()
        .open_path(dir.to_string_lossy().into_owned(), None::<&str>)
        .map_err(|e| format!("Kunne ikke åbne mappen: {e}"))
}

// ---- ticket commands (C3.2) ----

#[tauri::command]
pub fn list_tickets(state: State<'_, AppState>) -> Result<Vec<TicketSummary>, String> {
    Ok(state.tickets.read(|s| s.list()))
}

#[tauri::command]
pub fn get_ticket(state: State<'_, AppState>, id: String) -> Result<Ticket, String> {
    state
        .tickets
        .read(|s| s.get(&id))
        .ok_or_else(|| TicketError::NotFound.into())
}

#[tauri::command]
pub fn create_ticket(
    state: State<'_, AppState>,
    title: String,
    body: String,
    skip_review: bool,
    project: Option<ProjectRef>,
) -> Result<TicketSummary, String> {
    ticket_create(&state.tickets, &title, &body, skip_review, project)
}

#[tauri::command]
pub fn update_ticket(
    state: State<'_, AppState>,
    id: String,
    patch: TicketPatch,
) -> Result<TicketSummary, String> {
    ticket_update(&state.tickets, &id, patch)
}

#[tauri::command]
pub fn delete_ticket(state: State<'_, AppState>, id: String) -> Result<(), String> {
    ticket_delete(&state.tickets, &id)
}

#[tauri::command]
pub fn assign_ticket(
    state: State<'_, AppState>,
    id: String,
    agent_id: String,
    project: Option<ProjectRef>,
) -> Result<TicketSummary, String> {
    ticket_assign_in(&state.tickets, &id, &agent_id, project)
}

#[tauri::command]
pub fn unassign_ticket(state: State<'_, AppState>, id: String) -> Result<TicketSummary, String> {
    ticket_unassign(&state.tickets, &id)
}

#[tauri::command]
pub fn reorder_queue(
    state: State<'_, AppState>,
    agent_id: String,
    ticket_ids: Vec<String>,
) -> Result<Vec<TicketSummary>, String> {
    ticket_reorder(&state.tickets, &agent_id, &ticket_ids)
}

#[tauri::command]
pub fn set_ticket_state(
    // Named `app` here because the contract's argument is called `state` (C3.2); Tauri resolves
    // `State<…>` by type, not by name.
    app: State<'_, AppState>,
    id: String,
    state: TicketState,
    note: Option<String>,
) -> Result<TicketSummary, String> {
    ticket_set_state(&app.tickets, &id, state, note)
}

#[tauri::command]
pub fn approve_ticket(state: State<'_, AppState>, id: String) -> Result<TicketSummary, String> {
    ticket_approve(&state.tickets, &id)
}

#[tauri::command]
pub fn reject_ticket(
    state: State<'_, AppState>,
    id: String,
    note: String,
) -> Result<TicketSummary, String> {
    ticket_reject(&state.tickets, &id, &note)
}

#[tauri::command]
pub fn redispatch_ticket(state: State<'_, AppState>, id: String) -> Result<(), String> {
    ticket_redispatch(&state.tickets, &id)
}

#[tauri::command]
pub fn request_submission(state: State<'_, AppState>, ticket_id: String) -> Result<(), String> {
    ticket_request_submission(&state.tickets, &ticket_id)
}

// ---- reports and review assignment (plan5 C5.4) ----

/// The user adds a report (author "user").
#[tauri::command]
pub fn add_report(
    state: State<'_, AppState>,
    ticket_id: String,
    title: String,
    body: String,
) -> Result<TicketReport, String> {
    state
        .tickets
        .add_report(&ticket_id, ReportAuthor::user(), &title, &body)
}

#[tauri::command]
pub fn get_report(
    state: State<'_, AppState>,
    ticket_id: String,
    report_id: String,
) -> Result<ReportContent, String> {
    state.tickets.get_report(&ticket_id, &report_id)
}

/// Opens the ticket's report folder (created first if needed).
// TODO(windows-verify): "Åbn mappe" opens Explorer on
// %APPDATA%\dk.mira.bots\tickets\<id>\reports; deleting the ticket removes it (plan5 D.58).
#[tauri::command]
pub fn open_report_dir(
    app: AppHandle,
    state: State<'_, AppState>,
    ticket_id: String,
) -> Result<(), String> {
    state
        .tickets
        .read(|s| s.get(&ticket_id))
        .ok_or(TicketError::NotFound)?;
    let dir = state
        .tickets
        .reports
        .dir_for(&ticket_id)
        .map_err(|e| format!("Kunne ikke åbne mappen: {e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("Kunne ikke oprette mappen: {e}"))?;
    app.opener()
        .open_path(dir.to_string_lossy(), None::<&str>)
        .map_err(|e| format!("Kunne ikke åbne mappen: {e}"))
}

/// Picks (`agentId`) or removes (`null`, then routed again) the reviewer of a ticket in review.
// TODO(windows-verify): an escalated ticket can get a reviewer by hand; "Fjern reviewer" routes
// it again (plan5 D.55).
#[tauri::command]
pub fn assign_reviewer(
    state: State<'_, AppState>,
    ticket_id: String,
    agent_id: Option<String>,
) -> Result<TicketSummary, String> {
    state
        .tickets
        .assign_reviewer(&ticket_id, agent_id.as_deref().filter(|a| !a.is_empty()))
}

#[tauri::command]
pub fn list_review_assignments(
    state: State<'_, AppState>,
) -> Result<Vec<ReviewAssignment>, String> {
    Ok(state.tickets.read(|s| s.review_assignments()))
}

#[tauri::command]
pub fn quit_app(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    lock(&state.manager).kill_all();
    app.exit(0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::build_spawn_spec;
    use crate::agent::roles::Role;
    use crate::profiles::store::{profiles_dir, ProfileStore};
    use crate::tickets::test_support::{test_ctx, TestCtx};
    use serde_json::json;

    /// The project of the test fakes on work seats (`insert_fake_with`).
    fn p() -> Option<ProjectRef> {
        Some(ProjectRef::Existing("p".into()))
    }

    fn request(id: &str, agent: &str) -> PermissionRequestInfo {
        PermissionRequestInfo {
            request_id: id.into(),
            agent_id: agent.into(),
            agent_name: "demo".into(),
            tool_name: "Bash".into(),
            summary: "npm test".into(),
            tool_input: json!({"command":"npm test"}),
            created_at: 1,
            deadline_at: 108_001,
        }
    }

    fn setup() -> (Mutex<AgentManager>, Mutex<PendingPermissions>, String) {
        let mut m = AgentManager::new(5);
        let agent = m.insert_fake("sess", "/w/demo");
        (m.into(), Mutex::new(PendingPermissions::new()), agent)
    }

    #[test]
    fn terminal_replies_do_not_count_as_user_input() {
        let (m, _, agent) = setup();
        // The fake agent has no PTY, so the write itself fails; the timestamp is what matters.
        assert!(write_terminal_input(&m, &agent, b"\x1b[?1;2c", false).is_err());
        assert_eq!(lock(&m).last_user_input_at(&agent), None);
        assert!(write_terminal_input(&m, &agent, b"a", true).is_err());
        assert!(lock(&m).last_user_input_at(&agent).is_some());
    }

    #[test]
    fn respond_allow_delivers_decision_without_whitelisting() {
        let (m, p, agent) = setup();
        let mut rx = lock(&p).insert(request("r1", &agent));
        respond(&m, &p, "r1", true, false).unwrap();
        assert_eq!(rx.try_recv().unwrap(), Decision::Allow);
        assert!(!lock(&m).whitelist_contains(&agent, "Bash"));
    }

    #[test]
    fn respond_always_whitelists_the_tool_for_that_agent() {
        let (m, p, agent) = setup();
        let mut rx = lock(&p).insert(request("r1", &agent));
        respond(&m, &p, "r1", true, true).unwrap();
        assert_eq!(rx.try_recv().unwrap(), Decision::Allow);
        assert!(lock(&m).whitelist_contains(&agent, "Bash"));
        assert!(!lock(&m).whitelist_contains(&agent, "Edit"));
    }

    #[test]
    fn respond_deny_never_whitelists() {
        let (m, p, agent) = setup();
        let mut rx = lock(&p).insert(request("r1", &agent));
        respond(&m, &p, "r1", false, true).unwrap();
        assert_eq!(rx.try_recv().unwrap(), Decision::Deny);
        assert!(!lock(&m).whitelist_contains(&agent, "Bash"));
    }

    #[test]
    fn respond_to_unknown_or_answered_request_is_expired() {
        let (m, p, agent) = setup();
        assert_eq!(
            respond(&m, &p, "nope", true, false).unwrap_err(),
            "Anmodningen er udløbet"
        );
        let _rx = lock(&p).insert(request("r1", &agent));
        respond(&m, &p, "r1", false, false).unwrap();
        assert_eq!(
            respond(&m, &p, "r1", true, false).unwrap_err(),
            "Anmodningen er udløbet"
        );
    }

    #[test]
    fn stop_marks_exited_and_releases_pending_requests() {
        let (m, p, agent) = setup();
        let mut rx = lock(&p).insert(request("r1", &agent));
        let info = stop(&m, &p, &agent).unwrap();
        assert!(matches!(
            info.status,
            crate::hooks::status::AgentStatus::Exited { code: None }
        ));
        assert_eq!(rx.try_recv().unwrap(), Decision::None);
        assert!(lock(&p).list().is_empty());
        assert_eq!(stop(&m, &p, "missing").unwrap_err(), "Agenten findes ikke");
    }

    #[test]
    fn app_info_serializes_camel_case() {
        let info = AppInfo {
            claude_path: None,
            hook_exe: Some("/h".into()),
            settings_json: "/d/settings.json".into(),
            pipe_name: "pipe".into(),
            max_agents: 5,
            version: "0.1.0".into(),
            pipe_ready: false,
            max_staff_agents: 3,
            projects_root: "/h/mira-bots/projects".into(),
            rules: WorkspaceRules::defaults(),
        };
        assert_eq!(
            serde_json::to_value(&info).unwrap(),
            json!({"claudePath":null,"hookExe":"/h","settingsJson":"/d/settings.json",
                   "pipeName":"pipe","maxAgents":5,"version":"0.1.0","pipeReady":false,
                   "maxStaffAgents":3,"projectsRoot":"/h/mira-bots/projects",
                   "rules":{"maxWorkAgents":5,"maxStaffAgents":3,"maxReviewRounds":3,
                            "autoReviewOnStop":false,"createTicketRateLimit":20,
                            "ticketBodyMaxChars":20000,"reportBodyMaxChars":20000,
                            "reportsPerTicketMax":20,"reviewByDefault":true,
                            "userInputGraceMs":5000,"agentsMayCreateProjects":false,
                            "maxAgentsPerProject":0}})
        );
    }

    fn app_state(dir: &std::path::Path) -> AppState {
        let mut m = AgentManager::new(5);
        m.insert_fake("sess", "/w/demo");
        let stopped = m.insert_fake("sess-2", "/w/demo2");
        m.stop(&stopped).unwrap();
        let manager = Arc::new(Mutex::new(m));
        let t = test_ctx(Arc::clone(&manager));
        t.ctx.mutate(|s| s.create("a", "", false, 1)).unwrap();
        t.ctx.mutate(|s| s.create("b", "", false, 1)).unwrap();
        AppState {
            manager,
            pending: Arc::new(Mutex::new(PendingPermissions::new())),
            paths: AppPaths {
                hook_exe: None,
                settings_json: dir.join("settings.json"),
                mcp_exe: None,
                mcp_config: dir.join("mcp.json"),
                system_prompt: dir.join("system-prompt.md"),
                pipe_name: "pipe".into(),
                data_dir: dir.to_path_buf(),
                log_file: Some(dir.join("logs").join("mira-bots.log")),
                projects_root: dir.join("projects"),
                app_settings: dir.join("app-settings.json"),
                tickets_file: dir.join("tickets.json"),
                profiles_dir: profiles_dir(&dir.join("projects")),
                profile_files_dir: dir.join("profiles"),
            },
            island: IslandState::default(),
            pipe_ready: Arc::new(AtomicBool::new(true)),
            sink: Arc::new(|_| {}),
            hook_stats: Arc::new(HookStats::default()),
            claude_version: Arc::new(Mutex::new(VersionProbe::Ok("2.1.286 (Claude Code)".into()))),
            workplace_select: Mutex::new(None),
            tickets: t.ctx,
            tickets_warning: Some("tickets.json kunne ikke læses".into()),
            profiles: Arc::new(ProfilesCtx::new(
                ProfileStore::load(profiles_dir(&dir.join("projects")), 1),
                Arc::new(|_, _| {}),
            )),
            workspace: Arc::new(WorkspaceReader::new(
                dir.join("projects").join(crate::config::WORKSPACE_FILE),
            )),
            profiles_migrated: 0,
        }
    }

    #[test]
    fn diagnostics_are_built_from_the_state() {
        let dir = std::env::temp_dir().join(format!("mira-diag-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = app_state(&dir);
        state.hook_stats.record(
            crate::diagnostics::LastHookEvent {
                name: "Stop".into(),
                session_id: "sess".into(),
                agent_id: None,
                at: 5,
            },
            false,
        );
        let d = state.diagnostics();
        assert_eq!(d.claude_version.as_deref(), Some("2.1.286 (Claude Code)"));
        assert_eq!(d.claude_version_note, None);
        assert_eq!(d.claude_code_args_supported, Some(true));
        assert_eq!(d.claude_code_mcp_supported, Some(true));
        assert!(!d.settings_exists);
        assert!(d.settings_path.ends_with("settings.json"));
        assert_eq!(d.mcp_exe, None);
        assert!(d.mcp_config_path.ends_with("mcp.json"));
        assert!(!d.mcp_config_exists);
        assert!(d.system_prompt_path.ends_with("system-prompt.md"));
        assert_eq!(
            (d.tool_calls, d.tool_errors, d.last_tool_call),
            (0, 0, None)
        );
        assert!(!d.auto_review_on_stop);
        assert!(d.pipe_ready);
        assert_eq!(d.platform, std::env::consts::OS);
        assert_eq!(d.pipe_note, pipe_note(&d.pipe_name));
        assert_eq!((d.frames_received, d.frames_unknown_session), (1, 1));
        assert_eq!(d.last_hook_event.unwrap().name, "Stop");
        assert!(d.log_path.unwrap().ends_with("mira-bots.log"));
        assert_eq!(d.running_agents, 1, "the stopped agent does not count");
        assert!(d.projects_root.ends_with("projects"));
        assert!(d.workspace_file_path.ends_with("mira-bots.workspace.json"));
        assert!(!d.workspace_file_exists);
        assert_eq!(d.workspace_warning, None);
        assert_eq!(d.projects_total, 0);
        assert_eq!(d.profiles_migrated, 0);
        assert!(d.tickets_path.ends_with("tickets.json"));
        assert_eq!(d.tickets_total, 2);
        assert!(!d.tickets_read_only);
        assert_eq!(
            d.tickets_warning.as_deref(),
            Some("tickets.json kunne ikke læses")
        );
        std::fs::write(dir.join("settings.json"), "{}").unwrap();
        std::fs::write(dir.join("mcp.json"), "{}").unwrap();
        state
            .hook_stats
            .record_tool(crate::diagnostics::LastToolCall {
                tool: "mira_list_tickets".into(),
                agent_id: Some("a".into()),
                ok: false,
                at: 6,
            });
        *lock(&state.claude_version) = VersionProbe::Pending;
        let d = state.diagnostics();
        assert!(d.settings_exists);
        assert!(d.mcp_config_exists);
        assert_eq!((d.tool_calls, d.tool_errors), (1, 1));
        assert_eq!(d.last_tool_call.unwrap().tool, "mira_list_tickets");
        assert_eq!(d.claude_version_note.as_deref(), Some("kører stadig"));
        assert_eq!(d.claude_code_args_supported, None);
        assert_eq!(d.claude_code_mcp_supported, None);
        let info = state.app_info();
        assert_eq!(info.max_staff_agents, 3);
        assert_eq!(info.projects_root, d.projects_root);
        assert_eq!(info.rules, WorkspaceRules::defaults());

        // The workspace file decides the limits and the auto review; projects are counted.
        std::fs::write(
            dir.join("projects").join(crate::config::WORKSPACE_FILE),
            r#"{"maxWorkAgents": 2, "maxStaffAgents": 1, "autoReviewOnStop": true}"#,
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("projects").join("demo")).unwrap();
        let info = state.app_info();
        assert_eq!((info.max_agents, info.max_staff_agents), (2, 1));
        assert_eq!(info.rules.max_work_agents, 2);
        let d = state.diagnostics();
        assert!(d.workspace_file_exists && d.auto_review_on_stop);
        assert_eq!(d.projects_total, 1);
        std::fs::write(
            dir.join("projects").join(crate::config::WORKSPACE_FILE),
            "{ broken",
        )
        .unwrap();
        let d = state.diagnostics();
        assert!(d.workspace_warning.is_some());
        assert!(!d.auto_review_on_stop);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn workplace_selection_is_taken_once() {
        let sel = WorkplaceSelection {
            agent_id: Some("a1".into()),
            tab: Some("tickets".into()),
            spawn: Some("work".into()),
        };
        let slot = Mutex::new(Some(sel.clone()));
        assert_eq!(take_selection(&slot), Some(sel));
        assert_eq!(take_selection(&slot), None);
    }

    #[test]
    fn workplace_spawn_accepts_only_seat_kinds() {
        assert_eq!(check_spawn(None), Ok(()));
        assert_eq!(check_spawn(Some("work")), Ok(()));
        assert_eq!(check_spawn(Some("staff")), Ok(()));
        assert_eq!(
            check_spawn(Some("x")),
            Err("Ukendt pladstype: x".to_string())
        );
        assert!(check_spawn(Some("")).is_err());
    }

    #[test]
    fn workplace_tabs_include_agents() {
        assert_eq!(
            WORKPLACE_TABS,
            ["permissions", "diagnostics", "tickets", "agents"]
        );
    }

    #[test]
    fn diagnostics_has_profile_fields() {
        let dir = std::env::temp_dir().join(format!("mira-diagp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = app_state(&dir);
        let d = state.diagnostics();
        assert_eq!(d.profiles_loaded, 7);
        assert_eq!(d.profiles_warning, None);
        assert!(d.profiles_path.ends_with("profiles"));
        assert!(d.profiles_path.contains(".mira-bots"));
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["profilesLoaded"], 7);
        assert_eq!(v["profilesWarning"], serde_json::Value::Null);
        assert!(v["profilesPath"].is_string());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn spawn_uses_profile_defaults_and_overrides() {
        let dir = std::env::temp_dir().join(format!("mira-spawnp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = app_state(&dir);
        let reviewer = resolve_profile(&state.profiles, Some("reviewer")).unwrap();
        // Defaults: the profile's seat, no model/effort.
        let req = spawn_request(&reviewer, None, PathBuf::from("/w/r"), None, None).unwrap();
        assert_eq!(req.seat_kind, SeatKind::Staff);
        assert_eq!(req.profile.roles, [Role::Reviewer]);
        assert_eq!(
            (req.profile.profile_id.as_str(), req.profile.specialist),
            ("reviewer", false)
        );
        let ctx = SpawnContext {
            claude: PathBuf::from("/bin/claude"),
            settings_json: dir.join("profiles/reviewer/settings.json"),
            mcp_config: None,
            system_prompt: None,
            pipe_name: "p".into(),
        };
        let spec = build_spawn_spec(&req, &ctx, "sid", "aid");
        assert!(!spec.args.iter().any(|a| a == "--model" || a == "--effort"));
        assert_eq!(spec.env[2], ("MIRA_AGENT_ROLES".into(), "reviewer".into()));
        // Overrides and an explicit seat win.
        let req = spawn_request(
            &reviewer,
            Some(SpawnOverrides {
                model: Some("opus".into()),
                effort: Some(Effort::Max),
            }),
            PathBuf::from("/w/r"),
            None,
            Some(SeatKind::Work),
        )
        .unwrap();
        assert_eq!(req.seat_kind, SeatKind::Work);
        let a = build_spawn_spec(&req, &ctx, "sid", "aid").args;
        assert_eq!(
            a[2..],
            ["--model", "opus", "--effort", "max", "--session-id", "sid"]
        );
        // The profile's own model applies without an override.
        let with_model = AgentProfile {
            model: Some("claude-sonnet-5-5".into()),
            ..reviewer.clone()
        };
        let req = spawn_request(&with_model, None, PathBuf::from("/w"), None, None).unwrap();
        assert_eq!(req.profile.model.as_deref(), Some("claude-sonnet-5-5"));
        // Bad overrides are refused in Danish.
        let err = spawn_request(
            &reviewer,
            Some(SpawnOverrides {
                model: Some("bogus".into()),
                effort: None,
            }),
            PathBuf::from("/w"),
            None,
            None,
        )
        .unwrap_err();
        assert!(err.starts_with("Ukendt model"), "{err}");
        // Default profile: coder.
        assert_eq!(resolve_profile(&state.profiles, None).unwrap().id, "coder");
        assert_eq!(
            resolve_profile(&state.profiles, Some(" ")).unwrap().id,
            "coder"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn spawn_request_refuses_staff_seat_without_staff_role() {
        let dir = std::env::temp_dir().join(format!("mira-seatp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = app_state(&dir);
        let coder = resolve_profile(&state.profiles, Some("coder")).unwrap();
        let err = spawn_request(
            &coder,
            None,
            PathBuf::from("/w/c"),
            None,
            Some(SeatKind::Staff),
        )
        .unwrap_err();
        assert_eq!(
            err,
            "Profilen «Koder» har ingen stabsrolle (reviewer, koordinator eller planlægger) og kan ikke stå på en stabsplads"
        );
        // A work seat takes any profile; a staff role is enough for a staff seat.
        for id in ["coder", "reviewer", "coordinator", "planner", "specialist"] {
            let p = resolve_profile(&state.profiles, Some(id)).unwrap();
            let req = spawn_request(&p, None, PathBuf::from("/w"), None, Some(SeatKind::Work));
            assert_eq!(req.unwrap().seat_kind, SeatKind::Work, "{id}");
        }
        for id in ["reviewer", "coordinator", "planner", "specialist"] {
            let p = resolve_profile(&state.profiles, Some(id)).unwrap();
            let req = spawn_request(&p, None, PathBuf::from("/w"), None, Some(SeatKind::Staff));
            assert_eq!(req.unwrap().seat_kind, SeatKind::Staff, "{id}");
        }
        // A custom profile whose default seat is staff, without a staff role, is refused too.
        let own = AgentProfile {
            name: "Min koder".into(),
            default_seat: SeatKind::Staff,
            ..coder.clone()
        };
        let err = spawn_request(&own, None, PathBuf::from("/w"), None, None).unwrap_err();
        assert!(
            err.starts_with("Profilen «Min koder» har ingen stabsrolle"),
            "{err}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unknown_profile_is_refused() {
        let dir = std::env::temp_dir().join(format!("mira-unkp-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = app_state(&dir);
        assert_eq!(
            resolve_profile(&state.profiles, Some("custom-nope")).unwrap_err(),
            "Profilen findes ikke"
        );
        // Without the hook exe the spawn context is refused before anything is written.
        let coder = resolve_profile(&state.profiles, None).unwrap();
        let err = spawn_context(&state, "coder", Some(&coder)).unwrap_err();
        assert!(err.starts_with("Fandt ikke mira-hook"), "{err}");
        assert!(!dir.join("profiles").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn profile_save_creates_custom_ids_and_writes_files() {
        let dir = std::env::temp_dir().join(format!("mira-savep-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = app_state(&dir);
        let new = AgentProfile {
            id: String::new(),
            name: "Min debugger".into(),
            ..resolve_profile(&state.profiles, Some("debugger")).unwrap()
        };
        let saved = profile_save(&state.profiles, new, 9).unwrap();
        assert!(saved.id.starts_with("custom-"), "{}", saved.id);
        assert_eq!(saved.kind, crate::profiles::ProfileKind::Custom);
        assert_eq!(state.profiles.list().len(), 8);
        let (settings, prompt) =
            write_profile_files(&state.paths, &saved, &WorkspaceRules::defaults()).unwrap();
        assert_eq!(
            settings,
            dir.join("profiles").join(&saved.id).join("settings.json")
        );
        assert_eq!(
            prompt,
            dir.join("profiles")
                .join(&saved.id)
                .join("system-prompt.md")
        );
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(
            v["statusLine"]["command"], "mira-hook-not-found",
            "placeholder without a hook exe"
        );
        let bad = AgentProfile {
            name: " ".into(),
            ..saved
        };
        assert_eq!(
            profile_save(&state.profiles, bad, 10).unwrap_err(),
            "Navn skal være 1–60 tegn"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn set_model_refused_when_not_idle() {
        let mut m = AgentManager::new(5);
        let a = m.insert_fake("s", "/w/a");
        // Starting is not idle.
        let err = check_restartable(m.get(&a).as_ref()).unwrap_err();
        assert_eq!(err.to_string(), "Agenten arbejder");
        for busy in [
            AgentStatus::Thinking,
            AgentStatus::Running,
            AgentStatus::WaitingPermission,
        ] {
            m.set_status(&a, busy, None).unwrap();
            assert!(matches!(
                check_restartable(m.get(&a).as_ref()),
                Err(AgentError::Working)
            ));
        }
        m.set_status(&a, AgentStatus::Idle, None).unwrap();
        assert_eq!(check_restartable(m.get(&a).as_ref()).unwrap().id, a);
        m.stop(&a).unwrap();
        let err = check_restartable(m.get(&a).as_ref()).unwrap_err();
        assert_eq!(err.to_string(), "Agenten kører ikke");
        assert!(matches!(
            check_restartable(None),
            Err(AgentError::NotRunning)
        ));
    }

    #[test]
    fn set_model_refused_while_working() {
        let (t, a, _) = tickets_setup();
        let tk = t.ctx.mutate(|s| s.create("x", "", false, 1)).unwrap();
        t.ctx.mutate(|s| s.assign(&tk.id, &a, 2)).unwrap();
        lock(&t.ctx.manager)
            .set_status(&a, AgentStatus::Idle, None)
            .unwrap();
        // Idle with only a queue: allowed.
        assert!(check_restartable(lock(&t.ctx.manager).get(&a).as_ref()).is_ok());
        // Idle with a ticket in progress: refused.
        lock(&t.ctx.manager).set_ticket_link(&a, Some(tk.id.clone()), 0);
        assert!(matches!(
            check_restartable(lock(&t.ctx.manager).get(&a).as_ref()),
            Err(AgentError::Working)
        ));
    }

    #[test]
    fn restart_values_keep_the_other_setting() {
        let mut m = AgentManager::new(5);
        let a = m.insert_fake_with("s", "/w/a", &[Role::Coder], SeatKind::Staff);
        let mut info = m.get(&a).unwrap();
        info.model = Some("claude-opus-5-5".into());
        info.effort = Some("xhigh".into());
        assert_eq!(
            restart_values(&info, Some(Some("haiku".into())), None),
            (Some("haiku".into()), Some(Effort::Xhigh))
        );
        assert_eq!(
            restart_values(&info, Some(None), None),
            (None, Some(Effort::Xhigh)),
            "null model = default"
        );
        assert_eq!(
            restart_values(&info, None, Some(Effort::Low)),
            (Some("claude-opus-5-5".into()), Some(Effort::Low))
        );
        info.effort = Some("auto".into());
        assert_eq!(restart_values(&info, Some(None), None), (None, None));
        let req = restart_request(&info, None, Some(Effort::High));
        assert_eq!(
            (req.cwd, req.seat_kind, req.prompt, req.profile.roles),
            (
                PathBuf::from("/w/a"),
                SeatKind::Staff,
                None,
                vec![Role::Coder]
            )
        );
        let ctx = SpawnContext {
            claude: PathBuf::from("/bin/claude"),
            settings_json: PathBuf::from("/d/profiles/test/settings.json"),
            mcp_config: None,
            system_prompt: None,
            pipe_name: "p".into(),
        };
        let req = restart_request(&info, None, Some(Effort::High));
        let spec = build_restart_spec(&req, &ctx, &RestartSession::Resume("s".into()), &a);
        assert_eq!(
            spec.args[2..],
            ["--model", "default", "--effort", "high", "--resume", "s"]
        );
        // No turn yet: a fresh session with the same flags and the new id instead of --resume.
        let spec = build_restart_spec(&req, &ctx, &RestartSession::Fresh("n".into()), &a);
        assert_eq!(
            spec.args[2..],
            [
                "--model",
                "default",
                "--effort",
                "high",
                "--session-id",
                "n"
            ]
        );
        assert!(!spec.args.iter().any(|x| x == "--resume"));
    }

    #[test]
    fn only_known_workplace_tabs_are_accepted() {
        for tab in [
            None,
            Some("permissions"),
            Some("diagnostics"),
            Some("tickets"),
            Some("agents"),
        ] {
            assert_eq!(check_tab(tab), Ok(()));
        }
        assert_eq!(check_tab(Some("x")).unwrap_err(), "Ukendt fane: x");
    }

    // ---- tickets ----

    /// A manager with one idle agent and one exited agent, and a ticket context over it.
    fn tickets_setup() -> (TestCtx, String, String) {
        let mut m = AgentManager::new(5);
        let live = m.insert_fake("s1", "/w/live");
        m.set_status(&live, AgentStatus::Idle, None).unwrap();
        let dead = m.insert_fake("s2", "/w/dead");
        m.stop(&dead).unwrap();
        (test_ctx(Arc::new(Mutex::new(m))), live, dead)
    }

    fn queue_changed(id: &str) -> DispatchMsg {
        DispatchMsg::QueueChanged {
            agent_id: id.to_string(),
        }
    }

    #[test]
    fn assign_needs_a_live_agent_and_notifies_the_dispatcher() {
        let (mut t, live, dead) = tickets_setup();
        let tk = ticket_create(&t.ctx, "  Opgave  ", "", false, p()).unwrap();
        assert_eq!(tk.title, "Opgave");
        for agent in [dead.as_str(), "nope"] {
            assert_eq!(
                ticket_assign(&t.ctx, &tk.id, agent).unwrap_err(),
                "Agenten kører ikke"
            );
        }
        assert!(t.sent().is_empty());
        let s = ticket_assign(&t.ctx, &tk.id, &live).unwrap();
        assert_eq!(
            (s.state, s.queue_position),
            (TicketState::Assigned, Some(0))
        );
        assert_eq!(t.sent(), vec![queue_changed(&live)]);
        assert_eq!(lock(&t.ctx.manager).get(&live).unwrap().queue_length, 1);
        // Assigned tickets cannot be deleted; unassigning notifies the old agent.
        assert_eq!(
            ticket_delete(&t.ctx, &tk.id).unwrap_err(),
            "Kun tickets i backlog, done eller afvist uden agent kan slettes"
        );
        assert_eq!(
            ticket_unassign(&t.ctx, &tk.id).unwrap().state,
            TicketState::Backlog
        );
        assert_eq!(t.sent(), vec![queue_changed(&live)]);
        ticket_delete(&t.ctx, &tk.id).unwrap();
        assert_eq!(
            ticket_unassign(&t.ctx, &tk.id).unwrap_err(),
            "Ticketen findes ikke"
        );
    }

    #[test]
    fn leaving_in_progress_clears_the_stale_agent_hint() {
        use crate::config::{NOT_SUBMITTED_TEXT, TURN_FAILED_TEXT};
        let (t, live, _) = tickets_setup();
        let detail = |t: &TestCtx| lock(&t.ctx.manager).get(&live).unwrap().detail;
        let in_progress = |t: &TestCtx| {
            let tk = ticket_create(&t.ctx, "x", "", false, p()).unwrap();
            ticket_assign(&t.ctx, &tk.id, &live).unwrap();
            ticket_set_state(&t.ctx, &tk.id, TicketState::InProgress, None).unwrap();
            tk.id
        };
        for hint in [NOT_SUBMITTED_TEXT, TURN_FAILED_TEXT] {
            let id = in_progress(&t);
            lock(&t.ctx.manager).set_detail(&live, Some(hint.into()));
            t.clear();
            ticket_set_state(&t.ctx, &id, TicketState::Review, None).unwrap();
            assert_eq!(detail(&t), None, "{hint}");
            // The last `agents-changed` already carries the cleared detail.
            let last = t.emitted(AGENTS_CHANGED).pop().unwrap();
            let me = last
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["id"] == json!(live))
                .unwrap();
            assert!(me["detail"].is_null(), "{hint}");
            ticket_set_state(&t.ctx, &id, TicketState::Done, None).unwrap();
        }
        // Another detail stays; so does the hint when a different ticket is moved.
        let id = in_progress(&t);
        lock(&t.ctx.manager).set_detail(&live, Some("Kører tests".into()));
        ticket_set_state(&t.ctx, &id, TicketState::Review, None).unwrap();
        assert_eq!(detail(&t).as_deref(), Some("Kører tests"));
        let id = in_progress(&t);
        let other = ticket_create(&t.ctx, "y", "", false, p()).unwrap();
        ticket_assign(&t.ctx, &other.id, &live).unwrap();
        lock(&t.ctx.manager).set_detail(&live, Some(NOT_SUBMITTED_TEXT.into()));
        ticket_set_state(&t.ctx, &other.id, TicketState::Backlog, None).unwrap();
        assert_eq!(detail(&t).as_deref(), Some(NOT_SUBMITTED_TEXT));
        // Back to the backlog from in-progress clears it too.
        ticket_set_state(&t.ctx, &id, TicketState::Backlog, None).unwrap();
        assert_eq!(detail(&t), None);
    }

    /// Step 5c: "Tildel…" on a ticket in progress hands it over; "Fjern tildeling" puts it back.
    #[test]
    fn user_hands_over_and_puts_back_a_ticket_in_progress() {
        use crate::config::NOT_SUBMITTED_TEXT;
        let (mut t, live, dead) = tickets_setup();
        let other = {
            let mut m = lock(&t.ctx.manager);
            let id = m.insert_fake("s3", "/w/other");
            m.set_status(&id, AgentStatus::Idle, None).unwrap();
            id
        };
        let tk = ticket_create(&t.ctx, "x", "", false, p()).unwrap();
        ticket_assign(&t.ctx, &tk.id, &live).unwrap();
        ticket_set_state(&t.ctx, &tk.id, TicketState::InProgress, None).unwrap();
        lock(&t.ctx.manager).set_detail(&live, Some(NOT_SUBMITTED_TEXT.into()));
        let _ = t.sent();
        assert_eq!(
            ticket_assign(&t.ctx, &tk.id, &dead).unwrap_err(),
            "Agenten kører ikke"
        );
        assert_eq!(
            ticket_assign(&t.ctx, &tk.id, &live).unwrap_err(),
            "Ticketen kan ikke gives videre til den agent, der allerede har den"
        );
        let s = ticket_assign(&t.ctx, &tk.id, &other).unwrap();
        assert_eq!(
            (s.state, s.assignee_agent_id.as_deref(), s.queue_position),
            (TicketState::Assigned, Some(other.as_str()), Some(0))
        );
        // Review 5c W4: the user took it from `live`, so the dispatcher is told to stop it
        // (before the queues are woken), and its detail says so.
        let sent = t.sent();
        let other_name = lock(&t.ctx.manager).get(&other).unwrap().name;
        assert_eq!(
            sent[0],
            DispatchMsg::HandedOver {
                agent_id: live.clone(),
                ticket_id: tk.id.clone(),
                to_name: Some(other_name),
            }
        );
        let mut rest = sent[1..].to_vec();
        rest.sort_by_key(|m| format!("{m:?}"));
        let mut want = vec![queue_changed(&live), queue_changed(&other)];
        want.sort_by_key(|m| format!("{m:?}"));
        assert_eq!(rest, want);
        {
            let m = lock(&t.ctx.manager);
            assert_eq!(m.get(&live).unwrap().current_ticket_id, None);
            assert_eq!(
                m.get(&live).unwrap().detail,
                Some(format!(
                    "Ticket {} givet videre",
                    crate::tickets::model::short_id(&tk.id)
                )),
                "the stale hint is replaced"
            );
            assert_eq!(m.get(&other).unwrap().queue_length, 1);
        }
        let full = t.ctx.read(|s| s.get(&tk.id)).unwrap();
        let note = full.history.last().unwrap().note.clone().unwrap();
        assert!(note.starts_with("overdraget fra "), "{note}");
        // In progress with the other agent, then put back.
        ticket_set_state(&t.ctx, &tk.id, TicketState::InProgress, None).unwrap();
        let _ = t.sent();
        let b = ticket_unassign(&t.ctx, &tk.id).unwrap();
        assert_eq!((b.state, b.assignee_agent_id), (TicketState::Backlog, None));
        assert_eq!(
            t.sent(),
            vec![
                DispatchMsg::HandedOver {
                    agent_id: other.clone(),
                    ticket_id: tk.id.clone(),
                    to_name: None,
                },
                queue_changed(&other)
            ]
        );
        let full = t.ctx.read(|s| s.get(&tk.id)).unwrap();
        assert_eq!(
            full.history.last().unwrap().note.as_deref(),
            Some("lagt tilbage")
        );
        assert_eq!(
            lock(&t.ctx.manager).get(&other).unwrap().detail,
            Some(format!(
                "Ticket {} lagt tilbage",
                crate::tickets::model::short_id(&tk.id)
            ))
        );
    }

    #[test]
    fn set_ticket_state_maps_errors_to_danish_text() {
        let (mut t, live, _) = tickets_setup();
        let tk = ticket_create(&t.ctx, "x", "", false, p()).unwrap();
        assert_eq!(
            ticket_set_state(&t.ctx, &tk.id, TicketState::Done, None).unwrap_err(),
            "Kan ikke flytte en ticket fra Backlog til Done"
        );
        assert_eq!(
            ticket_set_state(&t.ctx, &tk.id, TicketState::Assigned, None).unwrap_err(),
            "Brug Tildel for at sætte en ticket i kø"
        );
        assert_eq!(
            ticket_set_state(&t.ctx, &tk.id, TicketState::Rejected, None).unwrap_err(),
            "Brug Afvis med note"
        );
        assert_eq!(
            ticket_set_state(&t.ctx, "nope", TicketState::Backlog, None).unwrap_err(),
            "Ticketen findes ikke"
        );
        // assigned → inProgress by hand ("the user gave the task himself"), then → review.
        ticket_assign(&t.ctx, &tk.id, &live).unwrap();
        t.sent();
        let s = ticket_set_state(&t.ctx, &tk.id, TicketState::InProgress, None).unwrap();
        assert_eq!(s.state, TicketState::InProgress);
        assert_eq!(t.sent(), vec![queue_changed(&live)]);
        assert_eq!(
            ticket_set_state(&t.ctx, &tk.id, TicketState::Done, None).unwrap_err(),
            "Done kræver review (eller skipReview på ticketen)"
        );
        let s = ticket_set_state(&t.ctx, &tk.id, TicketState::Review, None).unwrap();
        assert_eq!(s.state, TicketState::Review);
        // review → rejected through set_ticket_state needs a note, like reject_ticket.
        assert_eq!(
            ticket_set_state(&t.ctx, &tk.id, TicketState::Rejected, None).unwrap_err(),
            "Afvisning kræver en note"
        );
    }

    #[test]
    fn reject_after_the_sender_moved_goes_to_the_backlog() {
        // W1: submitted in "p", moved to "q", then rejected (button and drag to "Afvist").
        let (mut t, live, _) = tickets_setup();
        let mk = |t: &TestCtx| {
            let tk = ticket_create(&t.ctx, "x", "", false, p()).unwrap();
            t.ctx
                .mutate(|s| {
                    s.assign(&tk.id, &live, 1)?;
                    s.mark_dispatched(&tk.id, "bot", 2)?;
                    s.complete_turn(&live, 3)
                })
                .unwrap();
            tk.id
        };
        let a = mk(&t);
        let b = mk(&t);
        lock(&t.ctx.manager).set_project(&live, Some("q".into()));
        t.sent();
        for (id, s) in [
            (&a, ticket_reject(&t.ctx, &a, "Mangler test").unwrap()),
            (
                &b,
                ticket_set_state(
                    &t.ctx,
                    &b,
                    TicketState::Rejected,
                    Some("Mangler test".into()),
                )
                .unwrap(),
            ),
        ] {
            assert_eq!((s.state, s.assignee_agent_id), (TicketState::Backlog, None));
            let tk = t.ctx.read(|s| s.get(id)).unwrap();
            assert_eq!(tk.rejection_note.as_deref(), Some("Mangler test"));
            assert_eq!(tk.review_round, 1);
            assert_eq!(tk.history.last().unwrap().note.as_deref(), Some(MOVED_NOTE));
        }
        // Nothing was queued for the moved agent, so nothing can be delivered in "q".
        assert_eq!(t.ctx.read(|s| s.queue(&live)).len(), 0);
        // Moved back: the next rejection goes first in its queue again.
        lock(&t.ctx.manager).set_project(&live, Some("P".into()));
        let c = mk(&t);
        let s = ticket_reject(&t.ctx, &c, "igen").unwrap();
        assert_eq!(
            (s.state, s.queue_position),
            (TicketState::Assigned, Some(0))
        );
    }

    #[test]
    fn reject_requeues_for_a_live_agent_and_approve_finishes() {
        let (mut t, live, dead) = tickets_setup();
        let mk = |t: &TestCtx, agent: &str| {
            let tk = ticket_create(&t.ctx, "x", "", false, p()).unwrap();
            t.ctx
                .mutate(|s| {
                    s.assign(&tk.id, agent, 1)?;
                    s.mark_dispatched(&tk.id, "bot", 2)?;
                    s.complete_turn(agent, 3)
                })
                .unwrap();
            tk.id
        };
        let a = mk(&t, &live);
        assert_eq!(
            ticket_reject(&t.ctx, &a, "   ").unwrap_err(),
            "Afvisning kræver en note"
        );
        t.sent();
        let s = ticket_reject(&t.ctx, &a, " Mangler test ").unwrap();
        assert_eq!(
            (s.state, s.queue_position),
            (TicketState::Assigned, Some(0))
        );
        assert_eq!(s.rejection_note.as_deref(), Some("Mangler test"));
        assert_eq!(t.sent(), vec![queue_changed(&live)]);
        // The exited agent's ticket goes to the backlog instead.
        let b = mk(&t, &dead);
        assert_eq!(
            ticket_reject(&t.ctx, &b, "nej").unwrap().state,
            TicketState::Backlog
        );
        let c = mk(&t, &live);
        assert_eq!(ticket_approve(&t.ctx, &c).unwrap().state, TicketState::Done);
        assert_eq!(
            ticket_approve(&t.ctx, &c).unwrap_err(),
            "Kan ikke flytte en ticket fra Done til Done"
        );
    }

    #[test]
    fn reorder_update_redispatch_and_spawn_checks() {
        let (mut t, live, _) = tickets_setup();
        let ids: Vec<String> = (0..2)
            .map(|i| {
                let tk = ticket_create(&t.ctx, &format!("t{i}"), "", false, p()).unwrap();
                ticket_assign(&t.ctx, &tk.id, &live).unwrap();
                tk.id
            })
            .collect();
        t.sent();
        let rev: Vec<String> = ids.iter().rev().cloned().collect();
        let q = ticket_reorder(&t.ctx, &live, &rev).unwrap();
        assert_eq!(q.iter().map(|s| s.id.clone()).collect::<Vec<_>>(), rev);
        assert_eq!(t.sent(), vec![queue_changed(&live)]);
        assert_eq!(
            ticket_reorder(&t.ctx, &live, &ids[..1]).unwrap_err(),
            "Køen passer ikke"
        );
        let patch = TicketPatch {
            title: Some(String::new()),
            ..TicketPatch::default()
        };
        assert_eq!(
            ticket_update(&t.ctx, &ids[0], patch).unwrap_err(),
            "Titel må ikke være tom"
        );
        ticket_redispatch(&t.ctx, &ids[0]).unwrap();
        assert_eq!(
            t.sent(),
            vec![DispatchMsg::Redispatch {
                ticket_id: ids[0].clone()
            }]
        );
        assert_eq!(
            ticket_redispatch(&t.ctx, "nope").unwrap_err(),
            "Ticketen findes ikke"
        );
        // Only backlog (or agent-less rejected) tickets can start a new agent.
        let queued = TicketState::Assigned.label_da();
        assert_eq!(
            ticket_for_spawn(&t.ctx, &ids[0]).unwrap_err(),
            format!("Kan ikke flytte en ticket fra {queued} til {queued}")
        );
        let fresh = ticket_create(&t.ctx, "ny", "", false, p()).unwrap();
        assert_eq!(ticket_for_spawn(&t.ctx, &fresh.id).unwrap().id, fresh.id);
    }

    #[test]
    fn a_spawned_ticket_waits_for_the_session_and_heads_the_queue() {
        let (mut t, live, _) = tickets_setup();
        let tk = ticket_create(&t.ctx, "x", "", false, p()).unwrap();
        ticket_attach_spawned(&t.ctx, &tk.id, &live).unwrap();
        // The dispatcher hears about the spawn before the ticket shows up in the queue.
        assert_eq!(
            t.sent(),
            vec![DispatchMsg::SpawnedWithTicket {
                agent_id: live.clone(),
                ticket_id: tk.id.clone()
            }]
        );
        let got = t.ctx.read(|s| s.get(&tk.id)).unwrap();
        assert_eq!(got.state, TicketState::Assigned);
        assert_eq!(got.queue_position, Some(0));
        assert_eq!(lock(&t.ctx.manager).get(&live).unwrap().queue_length, 1);
    }

    #[test]
    fn request_submission_needs_an_in_progress_ticket_with_a_live_agent() {
        let (mut t, live, dead) = tickets_setup();
        let in_progress = |agent: &str| {
            let tk = ticket_create(&t.ctx, "x", "", false, p()).unwrap();
            t.ctx
                .mutate(|s| {
                    s.assign(&tk.id, agent, 2)?;
                    s.mark_dispatched(&tk.id, "bot", 3)
                })
                .unwrap()
        };
        let mine = in_progress(&live);
        let orphan = in_progress(&dead);
        t.sent();
        ticket_request_submission(&t.ctx, &mine.id).unwrap();
        assert_eq!(
            t.sent(),
            vec![DispatchMsg::RequestSubmission {
                ticket_id: mine.id.clone()
            }]
        );
        // The ticket itself is untouched by the command.
        assert_eq!(t.ctx.read(|s| s.get(&mine.id)).unwrap(), mine);
        assert_eq!(
            ticket_request_submission(&t.ctx, &orphan.id).unwrap_err(),
            "Agenten kører ikke"
        );
        // A review ticket is not in progress.
        t.ctx.mutate(|s| s.complete_turn(&live, 4)).unwrap();
        assert_eq!(
            ticket_request_submission(&t.ctx, &mine.id).unwrap_err(),
            "Ticketen er ikke i gang"
        );
        let backlog = ticket_create(&t.ctx, "y", "", false, p()).unwrap();
        assert_eq!(
            ticket_request_submission(&t.ctx, &backlog.id).unwrap_err(),
            "Ticketen er ikke i gang"
        );
        assert_eq!(
            ticket_request_submission(&t.ctx, "nope").unwrap_err(),
            "Ticketen findes ikke"
        );
        assert!(t.sent().is_empty());
    }

    fn temp_state() -> (AppState, PathBuf) {
        let dir = std::env::temp_dir().join(format!("mira-place-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("projects")).unwrap();
        (app_state(&dir), dir)
    }

    fn profile(state: &AppState, id: &str) -> AgentProfile {
        state.profiles.get(id).unwrap()
    }

    #[test]
    fn resolve_placement_by_seat_and_project() {
        let (state, dir) = temp_state();
        let root = state.paths.projects_root.clone();
        let coder = profile(&state, "coder");
        // Staff: the root, no project.
        let staff = resolve_placement(
            &state,
            &profile(&state, "reviewer"),
            SeatKind::Staff,
            None,
            false,
        )
        .unwrap();
        assert_eq!(
            (staff.cwd.clone(), staff.project.clone()),
            (root.clone(), None)
        );
        // Work without a project.
        assert_eq!(
            resolve_placement(&state, &coder, SeatKind::Work, None, true).unwrap_err(),
            WORK_SEAT_NEEDS_PROJECT
        );
        // A new project is created; names count all agents, not folders.
        let new = ProjectRef::New { new: "demo".into() };
        let a = resolve_placement(&state, &coder, SeatKind::Work, Some(&new), true).unwrap();
        assert_eq!(a.cwd, root.join("demo"));
        assert!(a.cwd.is_dir());
        assert_eq!(a.project.as_deref(), Some("demo"));
        assert_eq!(a.name, "coder-01");
        lock(&state.manager).insert_fake_in(
            "s9",
            &a.cwd.to_string_lossy(),
            &[Role::Coder],
            SeatKind::Work,
            Some("demo"),
        );
        let existing = ProjectRef::Existing("DEMO".into());
        let b = resolve_placement(&state, &coder, SeatKind::Work, Some(&existing), false).unwrap();
        assert_eq!(
            (b.cwd.clone(), b.project.as_deref()),
            (root.join("demo"), Some("demo"))
        );
        // Unknown project; an agent may not create one.
        assert_eq!(
            resolve_placement(
                &state,
                &coder,
                SeatKind::Work,
                Some(&ProjectRef::Existing("nope".into())),
                true
            )
            .unwrap_err(),
            "Projektet «nope» findes ikke"
        );
        let err = resolve_placement(
            &state,
            &coder,
            SeatKind::Work,
            Some(&ProjectRef::New {
                new: "andet".into(),
            }),
            false,
        )
        .unwrap_err();
        assert!(err.contains("agentsMayCreateProjects"), "{err}");
        assert!(!root.join("andet").exists());
        // maxAgentsPerProject from the workspace file.
        std::fs::write(
            root.join(crate::config::WORKSPACE_FILE),
            r#"{"maxAgentsPerProject": 1}"#,
        )
        .unwrap();
        assert_eq!(
            resolve_placement(&state, &coder, SeatKind::Work, Some(&existing), false).unwrap_err(),
            "Loft på 1 agenter i projektet «demo» nået"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn placement_names_follow_the_known_agents() {
        let (state, dir) = temp_state();
        let coder = profile(&state, "coder");
        let new = ProjectRef::New { new: "x".into() };
        let a = resolve_placement(&state, &coder, SeatKind::Work, Some(&new), true).unwrap();
        assert_eq!(a.name, "coder-01");
        // A fake is named after its folder: "coder-01" takes the name, not the folder.
        lock(&state.manager).insert_fake_in("s1", "/w/coder-01", &[], SeatKind::Work, Some("x"));
        let b = resolve_placement(&state, &coder, SeatKind::Work, Some(&new), true).unwrap();
        assert_eq!(
            (b.name.as_str(), b.cwd.clone()),
            ("coder-02", a.cwd.clone())
        );
        let req = placed_request(&coder, None, b, None, SeatKind::Work).unwrap();
        assert_eq!(
            (req.name.as_deref(), req.project.as_deref()),
            (Some("coder-02"), Some("x"))
        );
        // Seat limits come from the workspace file before every spawn.
        std::fs::write(
            state
                .paths
                .projects_root
                .join(crate::config::WORKSPACE_FILE),
            r#"{"maxWorkAgents": 1}"#,
        )
        .unwrap();
        let rules = apply_limits(&state);
        assert_eq!(rules.max_work_agents, 1);
        assert_eq!(
            lock(&state.manager)
                .can_spawn(SeatKind::Work)
                .unwrap_err()
                .to_string(),
            "Loft på 1 arbejdspladser nået"
        );
        assert!(lock(&state.manager).can_spawn(SeatKind::Staff).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn spawn_with_ticket_realizes_a_new_project() {
        let (state, dir) = temp_state();
        let coder = profile(&state, "coder");
        let tk = ticket_create(
            &state.tickets,
            "ny",
            "",
            false,
            Some(ProjectRef::New { new: "App".into() }),
        )
        .unwrap();
        let ticket = ticket_for_spawn(&state.tickets, &tk.id).unwrap();
        let project = spawn_project(&ticket, SeatKind::Work, None).unwrap();
        let placement =
            resolve_placement(&state, &coder, SeatKind::Work, project.as_ref(), true).unwrap();
        assert!(state.paths.projects_root.join("App").is_dir());
        let ticket = ticket_in_placement(&state.tickets, ticket, &placement).unwrap();
        assert_eq!(ticket.project, Some(ProjectRef::Existing("App".into())));
        assert_eq!(ticket.state, TicketState::Backlog);
        // The ticket file shows the project.
        let delivery = first_delivery(
            &state,
            SeatKind::Work,
            &coder.roles,
            placement.project.as_deref(),
            &WorkspaceRules::defaults(),
        );
        let file = prompt::write_ticket_file(&placement.cwd, &ticket, 0, &delivery).unwrap();
        assert!(std::fs::read_to_string(file)
            .unwrap()
            .contains("- projekt: App\n"));
        // W3: `mira_spawn_agent` with default rules (agentsMayCreateProjects false) still
        // realises the ticket's own "Nyt projekt" — the user decided it.
        let tk2 = ticket_create(
            &state.tickets,
            "andet",
            "",
            false,
            Some(ProjectRef::New {
                new: "Andet".into(),
            }),
        )
        .unwrap();
        let t2 = ticket_for_spawn(&state.tickets, &tk2.id).unwrap();
        let may_create = spawn_may_create(&t2, false);
        assert!(may_create);
        let p2 = spawn_project(&t2, SeatKind::Work, None).unwrap();
        let placed =
            resolve_placement(&state, &coder, SeatKind::Work, p2.as_ref(), may_create).unwrap();
        assert_eq!(placed.project.as_deref(), Some("Andet"));
        assert!(state.paths.projects_root.join("Andet").is_dir());
        // A project the agent passes for a ticket without one stays under the rule: refused
        // before anything exists.
        let tk3 = ticket_create(&state.tickets, "uden", "", false, None).unwrap();
        let t3 = ticket_for_spawn(&state.tickets, &tk3.id).unwrap();
        let may_create = spawn_may_create(&t3, false);
        assert!(!may_create);
        let param = Some(ProjectRef::New {
            new: "Tredje".into(),
        });
        let p3 = spawn_project(&t3, SeatKind::Work, param).unwrap();
        let err =
            resolve_placement(&state, &coder, SeatKind::Work, p3.as_ref(), may_create).unwrap_err();
        assert!(
            err.starts_with("Agenter må ikke oprette projekter"),
            "{err}"
        );
        assert!(!state.paths.projects_root.join("Tredje").exists());
        // An existing project on the ticket needs no permission either way.
        let t4 = Ticket {
            project: Some(ProjectRef::Existing("App".into())),
            ..t3
        };
        assert!(!spawn_may_create(&t4, false));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn spawn_project_takes_the_tickets_project_first() {
        let mut tk = crate::tickets::model::test_support::ticket(
            "abcdef01-0000-4000-8000-000000000001",
            TicketState::Backlog,
        );
        let param = Some(ProjectRef::Existing("b".into()));
        assert_eq!(
            spawn_project(&tk, SeatKind::Work, None).unwrap_err(),
            TicketError::ProjectRequired.to_string()
        );
        assert_eq!(
            spawn_project(&tk, SeatKind::Work, param.clone()).unwrap(),
            param
        );
        assert_eq!(
            spawn_project(&tk, SeatKind::Staff, param.clone()).unwrap(),
            None
        );
        tk.project = Some(ProjectRef::New { new: "a".into() });
        assert_eq!(
            spawn_project(&tk, SeatKind::Work, param).unwrap(),
            Some(ProjectRef::New { new: "a".into() })
        );
    }

    #[test]
    fn first_delivery_shared_only_for_work_and_projects_for_coordination() {
        let (state, dir) = temp_state();
        let rules = WorkspaceRules::defaults();
        std::fs::create_dir_all(state.paths.projects_root.join("p")).unwrap();
        // The fixture's live fake (work seat) is in project p.
        let d = first_delivery(&state, SeatKind::Work, &[Role::Coder], Some("p"), &rules);
        assert_eq!(d.shared.as_ref().unwrap().others, ["demo"]);
        let alone = first_delivery(&state, SeatKind::Work, &[Role::Coder], Some("q"), &rules);
        assert_eq!(alone.shared, None);
        // A reviewer on a work seat gets a coordination task: no shared section, the list.
        let r = first_delivery(&state, SeatKind::Work, &[Role::Reviewer], Some("p"), &rules);
        assert_eq!(r.shared, None);
        assert_eq!(r.projects.as_ref().unwrap().ids, ["p"]);
        let staff = first_delivery(&state, SeatKind::Staff, &[Role::Coordinator], None, &rules);
        assert!(!staff.projects.as_ref().unwrap().may_create);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ticket_assign_follows_the_project_rules() {
        let (state, dir) = temp_state();
        let root = state.paths.projects_root.clone();
        for p in ["a", "b"] {
            std::fs::create_dir_all(root.join(p)).unwrap();
        }
        let (in_a, in_b, staff) = {
            let mut m = lock(&state.manager);
            let a = m.insert_fake_in("sa", "/w/a", &[Role::Coder], SeatKind::Work, Some("a"));
            let b = m.insert_fake_in("sb", "/w/b", &[Role::Coder], SeatKind::Work, Some("b"));
            let s = m.insert_fake_with("ss", "/w/s", &[Role::Coordinator], SeatKind::Staff);
            (a, b, s)
        };
        let t = &state.tickets;
        // No project: refused on a work seat, fine on a staff seat.
        let none = ticket_create(t, "uden", "", false, None).unwrap();
        assert_eq!(
            ticket_assign(t, &none.id, &in_a).unwrap_err(),
            TicketError::ProjectRequired.to_string()
        );
        assert_eq!(ticket_assign(t, &none.id, &staff).unwrap().project, None);
        // Existing(a) to an agent in b.
        let ta = ticket_create(t, "a", "", false, Some(ProjectRef::Existing("a".into()))).unwrap();
        let err = ticket_assign(t, &ta.id, &in_b).unwrap_err();
        assert_eq!(err, "Agenten b står i projekt «b»; ticketen hører til «a»");
        assert_eq!(
            ticket_assign(t, &ta.id, &in_a)
                .unwrap()
                .assignee_agent_id
                .as_deref(),
            Some(in_a.as_str())
        );
        // New{"A"} on the agent in a: realised as a.
        let tn =
            ticket_create(t, "n", "", false, Some(ProjectRef::New { new: "A".into() })).unwrap();
        let assigned = ticket_assign(t, &tn.id, &in_a).unwrap();
        assert_eq!(assigned.project, Some(ProjectRef::Existing("a".into())));
        // New{"c"} on the agent in a: wrong project, nothing created.
        let tc =
            ticket_create(t, "c", "", false, Some(ProjectRef::New { new: "c".into() })).unwrap();
        assert!(ticket_assign(t, &tc.id, &in_a).unwrap_err().contains("«c»"));
        assert!(!root.join("c").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ticket_assign_with_a_picked_project() {
        let (state, dir) = temp_state();
        // The tickets context reads projects under its own workspace root.
        let root = state.tickets.workspace.root().to_path_buf();
        for p in ["a", "b"] {
            std::fs::create_dir_all(root.join(p)).unwrap();
        }
        let (in_a, staff) = {
            let mut m = lock(&state.manager);
            let a = m.insert_fake_in("sa", "/w/a", &[Role::Coder], SeatKind::Work, Some("a"));
            let s = m.insert_fake_with("ss", "/w/s", &[Role::Coordinator], SeatKind::Staff);
            (a, s)
        };
        let t = &state.tickets;
        let existing = |p: &str| Some(ProjectRef::Existing(p.into()));
        // "Hvilket projekt?" → the agent's project (any case): set and assigned in one go.
        let x = ticket_create(t, "x", "", false, None).unwrap();
        let s = ticket_assign_in(t, &x.id, &in_a, existing("A")).unwrap();
        assert_eq!(s.project, existing("a"));
        assert_eq!(s.state, TicketState::Assigned);
        // Another project than the agent's: refused, nothing stored.
        let y = ticket_create(t, "y", "", false, None).unwrap();
        let err = ticket_assign_in(t, &y.id, &in_a, existing("b")).unwrap_err();
        assert_eq!(err, "Agenten a står i projekt «a»; ticketen hører til «b»");
        let err = ticket_assign_in(t, &y.id, &in_a, Some(ProjectRef::New { new: "ny".into() }))
            .unwrap_err();
        assert!(err.contains("«ny»"), "{err}");
        assert!(!root.join("ny").exists());
        assert_eq!(t.read(|s| s.get(&y.id)).unwrap().project, None);
        // A new project to a staff agent: created (the user may) and set.
        let s =
            ticket_assign_in(t, &y.id, &staff, Some(ProjectRef::New { new: "ny".into() })).unwrap();
        assert_eq!(s.project, existing("ny"));
        assert!(root.join("ny").is_dir());
        // A ticket with a project: a picked project is refused.
        let z = ticket_create(t, "z", "", false, existing("a")).unwrap();
        assert_eq!(
            ticket_assign_in(t, &z.id, &in_a, existing("a")).unwrap_err(),
            "Ticketen har allerede projekt «a»"
        );
        assert!(ticket_assign_in(t, &z.id, &in_a, None).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn move_gate_checks_in_order() {
        let (state, dir) = temp_state();
        let root = state.paths.projects_root.clone();
        std::fs::create_dir_all(root.join("p")).unwrap();
        std::fs::create_dir_all(root.join("q")).unwrap();
        let (work, staff, busy) = {
            let mut m = lock(&state.manager);
            let w = m.insert_fake_in("mw", "/w/p", &[Role::Coder], SeatKind::Work, Some("p"));
            let s = m.insert_fake_with("ms", "/w/r", &[Role::Coordinator], SeatKind::Staff);
            let b = m.insert_fake_in("mb", "/w/p2", &[Role::Coder], SeatKind::Work, Some("q"));
            for id in [&w, &s] {
                m.set_status(id, AgentStatus::Idle, None).unwrap();
            }
            (w, s, b)
        };
        let q = ProjectRef::Existing("q".into());
        assert_eq!(
            move_gate(&state, "nope", &q, false).unwrap_err(),
            "Agenten kører ikke"
        );
        assert_eq!(
            move_gate(&state, &busy, &q, false).unwrap_err(),
            "Agenten arbejder"
        );
        assert_eq!(
            move_gate(&state, &staff, &q, false).unwrap_err(),
            AgentError::StaffHasNoProject.to_string()
        );
        // A queued ticket: refused without force.
        let tk = ticket_create(&state.tickets, "x", "", false, p()).unwrap();
        ticket_assign(&state.tickets, &tk.id, &work).unwrap();
        assert_eq!(
            move_gate(&state, &work, &q, false).unwrap_err(),
            AgentError::QueueNotEmpty(1).to_string()
        );
        // force: passes the queue check; its own project is refused (case-insensitive).
        assert_eq!(
            move_gate(&state, &work, &ProjectRef::Existing("P".into()), true).unwrap_err(),
            AgentError::SameProject("p".into()).to_string()
        );
        let (info, proj) = move_gate(&state, &work, &q, true).unwrap();
        assert_eq!((info.id.as_str(), proj.id.as_str()), (work.as_str(), "q"));
        // With force the queue goes to the backlog with the move note.
        assert_eq!(state.tickets.release_queue(&work, MOVED_NOTE).unwrap(), 1);
        let back = state.tickets.read(|s| s.get(&tk.id)).unwrap();
        assert_eq!(back.state, TicketState::Backlog);
        assert_eq!(
            back.history.last().unwrap().note.as_deref(),
            Some(MOVED_NOTE)
        );
        assert!(move_gate(&state, &work, &q, false).is_ok());
        // A new project is created by the user's move; the limit does not count the agent.
        std::fs::write(
            root.join(crate::config::WORKSPACE_FILE),
            r#"{"maxAgentsPerProject": 1}"#,
        )
        .unwrap();
        assert_eq!(
            move_gate(&state, &work, &q, false).unwrap_err(),
            "Loft på 1 agenter i projektet «q» nået"
        );
        let (_, n) =
            move_gate(&state, &work, &ProjectRef::New { new: "ny".into() }, false).unwrap();
        assert!(PathBuf::from(&n.path).is_dir());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn project_folder_and_projects_root_setting() {
        let (state, dir) = temp_state();
        let root = state.paths.projects_root.clone();
        assert_eq!(project_folder(&root, None).unwrap(), root);
        assert_eq!(
            project_folder(&root, Some("x")).unwrap_err(),
            "Projektet «x» findes ikke"
        );
        std::fs::create_dir_all(root.join("X")).unwrap();
        assert_eq!(project_folder(&root, Some("x")).unwrap(), root.join("X"));
        assert_eq!(
            store_projects_root(&dir, "  ").unwrap_err(),
            "Vælg en mappe til projektroden"
        );
        // N5: relative paths are refused; nothing is created or stored.
        for rel in ["x", "andet/rod", "./x"] {
            assert_eq!(
                store_projects_root(&dir, rel).unwrap_err(),
                PROJECTS_ROOT_NOT_ABSOLUTE
            );
        }
        assert_eq!(app_settings::load(&dir).projects_root(), None);
        let other = dir.join("andet").join("rod");
        let stored = store_projects_root(&dir, &format!(" {} ", other.display())).unwrap();
        assert_eq!(stored, other.to_string_lossy());
        assert!(other.is_dir());
        assert_eq!(app_settings::load(&dir).projects_root(), Some(other));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn spawn_is_refused_until_the_pipe_is_ready() {
        let ready = AtomicBool::new(false);
        let err = check_pipe_ready(&ready).unwrap_err();
        assert!(matches!(err, AgentError::PipeNotReady));
        assert!(String::from(err).starts_with("Hook-forbindelsen"));
        ready.store(true, Ordering::Release);
        assert!(check_pipe_ready(&ready).is_ok());
    }

    // ---- step 5: review routing from the commands, report folder on delete ----

    #[test]
    fn manual_review_routes_and_delete_removes_reports() {
        let mut m = AgentManager::new(5);
        let live = m.insert_fake("s1", "/w/live");
        m.set_status(&live, AgentStatus::Idle, None).unwrap();
        let rev = m.insert_fake_with("s2", "/w/rev", &[Role::Reviewer], SeatKind::Staff);
        let mut t = test_ctx(Arc::new(Mutex::new(m)));
        let tk = ticket_create(&t.ctx, "x", "", false, p()).unwrap();
        ticket_assign(&t.ctx, &tk.id, &live).unwrap();
        ticket_set_state(&t.ctx, &tk.id, TicketState::InProgress, None).unwrap();
        t.sent();
        // "Send til review" by hand: routed to the reviewer.
        let s = ticket_set_state(&t.ctx, &tk.id, TicketState::Review, None).unwrap();
        assert_eq!(s.state, TicketState::Review);
        let now = t.ctx.read(|s| s.get(&tk.id)).unwrap();
        assert_eq!(now.reviewer_agent_id.as_deref(), Some(rev.as_str()));
        assert!(t.sent().contains(&DispatchMsg::ReviewAssigned {
            reviewer_agent_id: rev.clone()
        }));
        // The user approves; the ticket (with a report) is deleted with its folder.
        t.ctx
            .add_report(&tk.id, ReportAuthor::user(), "Noter", "tekst")
            .unwrap();
        ticket_approve(&t.ctx, &tk.id).unwrap();
        let dir = t.ctx.reports.root().join(&tk.id);
        assert!(dir.is_dir());
        ticket_delete(&t.ctx, &tk.id).unwrap();
        assert!(!dir.exists());
        let _ = std::fs::remove_dir_all(t.ctx.reports.root());
    }
}
