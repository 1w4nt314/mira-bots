//! Tauri commands (contracts C.1 + C2.1 + C3.2) and the managed [`AppState`].
//!
//! All commands except `open_workplace` (async: window creation) are synchronous and return
//! `Result<T, String>`; errors are Danish, user-facing text. Locks are held briefly and never
//! while emitting.

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
use crate::agent::workdir::{ensure_dir, next_agent_dir};
use crate::agent::{
    now_ms, AgentError, AgentInfo, AgentManager, EventSink, SeatKind, SpawnContext, SpawnRequest,
};
use crate::config::{
    AUTO_REVIEW_ON_STOP, DEFAULT_PROFILE_ID, MAX_STAFF_AGENTS, MAX_WORK_AGENTS, SETTINGS_FILE,
    STARTING_HINT_AFTER, SYSTEM_PROMPT_FILE,
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
use crate::tickets::dispatcher::DispatchMsg;
use crate::tickets::model::{
    ReportAuthor, ReviewAssignment, Ticket, TicketError, TicketPatch, TicketReport, TicketState,
    TicketSummary, WorkspaceRules,
};
use crate::tickets::tools::{SpawnByProfile, SPAWN_UNAVAILABLE};
use crate::tickets::{prompt, ReportContent, TicketsCtx, AGENT_EXITED_NOTE, AGENT_STOPPED_NOTE};
use crate::workplace;

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
    /// `<home>/mira-bots/agents`: parent of the default agent folders (created on first use).
    pub agents_root: PathBuf,
    /// `<data_dir>/tickets.json`.
    pub tickets_file: PathBuf,
    /// `<agents_root>/.mira-bots/profiles`: the profile store (one JSON file per profile).
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
    pub max_agents: usize,
    pub version: String,
    /// Whether the pipe server is listening (see [`AppState::pipe_ready`]).
    pub pipe_ready: bool,
    pub max_staff_agents: usize,
    pub agents_root: String,
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
        AppInfo {
            claude_path: path_string(&find_claude()),
            hook_exe: path_string(&self.paths.hook_exe),
            settings_json: self.paths.settings_json.to_string_lossy().into_owned(),
            pipe_name: self.paths.pipe_name.clone(),
            max_agents: MAX_WORK_AGENTS,
            version: env!("CARGO_PKG_VERSION").to_string(),
            pipe_ready: self.pipe_ready.load(Ordering::Acquire),
            max_staff_agents: MAX_STAFF_AGENTS,
            agents_root: self.paths.agents_root.to_string_lossy().into_owned(),
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
            auto_review_on_stop: AUTO_REVIEW_ON_STOP,
            pipe_name: self.paths.pipe_name.clone(),
            pipe_ready: self.pipe_ready.load(Ordering::Acquire),
            frames_received: self.hook_stats.received(),
            frames_unknown_session: self.hook_stats.unknown(),
            last_hook_event: self.hook_stats.last_event(),
            log_path: path_string(&self.paths.log_file),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            agents_root: self.paths.agents_root.to_string_lossy().into_owned(),
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

/// Explicit folder if given and non-blank, otherwise the next free default folder
/// (`<agents_root>/<prefix>-<nn>`, prefix from the profile's roles), created on disk.
pub fn resolve_cwd(
    cwd: Option<String>,
    agents_root: &std::path::Path,
    prefix: &str,
    manager: &Mutex<AgentManager>,
) -> Result<PathBuf, AgentError> {
    match cwd {
        Some(s) if !s.trim().is_empty() => Ok(PathBuf::from(s)),
        _ => {
            let taken = lock(manager).cwds();
            let dir = next_agent_dir(agents_root, prefix, &taken);
            ensure_dir(&dir)?;
            Ok(dir)
        }
    }
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
) -> Result<TicketSummary, String> {
    let now = now_ms();
    t.mutate(|s| s.create(title, body, skip_review, now))
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
pub fn ticket_assign(t: &TicketsCtx, id: &str, agent_id: &str) -> Result<TicketSummary, String> {
    if !agent_live(&t.manager, agent_id) {
        return Err(TicketError::AgentNotLive.into());
    }
    let now = now_ms();
    let tk = t.mutate(|s| s.assign(id, agent_id, now))?;
    t.notify([agent_id]);
    Ok(TicketSummary::from(&tk))
}

pub fn ticket_unassign(t: &TicketsCtx, id: &str) -> Result<TicketSummary, String> {
    let old = assignee_of(t, id)?;
    let now = now_ms();
    let tk = t.mutate(|s| s.unassign(id, now))?;
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

/// review → rejected → first in the same agent's queue (agent live) or the backlog.
pub fn ticket_reject(t: &TicketsCtx, id: &str, note: &str) -> Result<TicketSummary, String> {
    if note.trim().is_empty() {
        return Err(TicketError::NeedsNote.into());
    }
    let old = assignee_of(t, id)?;
    let live = old.as_deref().is_some_and(|a| agent_live(&t.manager, a));
    let now = now_ms();
    let tk = t.mutate(|s| s.reject(id, note.trim(), live, now))?;
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
/// `seat_kind` defaults to the profile's `defaultSeat`.
pub fn spawn_request(
    profile: &AgentProfile,
    overrides: Option<SpawnOverrides>,
    cwd: PathBuf,
    prompt: Option<String>,
    seat_kind: Option<SeatKind>,
) -> Result<SpawnRequest, String> {
    let overrides = validate_overrides(overrides.unwrap_or_default())?;
    Ok(SpawnRequest {
        cwd,
        prompt,
        seat_kind: seat_kind.unwrap_or(profile.default_seat),
        profile: profile.snapshot(&overrides),
    })
}

/// Writes the profile's `settings.json` and `system-prompt.md` under `<data_dir>/profiles/<id>/`
/// (the hook exe placeholder when it was not found). Returns both paths.
pub fn write_profile_files(
    paths: &AppPaths,
    profile: &AgentProfile,
) -> std::io::Result<(PathBuf, PathBuf)> {
    let hook = paths
        .hook_exe
        .clone()
        .unwrap_or_else(|| PathBuf::from("mira-hook-not-found"));
    let settings = write_profile_settings(&paths.data_dir, &hook, profile)?;
    let prompt = write_profile_prompt(&paths.data_dir, profile, &WorkspaceRules::current())?;
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
            let prompt = write_profile_prompt(&state.paths.data_dir, p, &WorkspaceRules::current())
                .map_err(io)?;
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

/// [`spawn_context`] for `profile`, then the working folder (prefix from the profile's roles; a
/// refused spawn never creates a default folder).
pub fn prepare_spawn(
    state: &AppState,
    profile: &AgentProfile,
    cwd: Option<String>,
) -> Result<(SpawnContext, PathBuf), String> {
    let ctx = spawn_context(state, &profile.id, Some(profile))?;
    let prefix = prefix_for(&profile.roles, profile.is_specialist());
    let cwd = resolve_cwd(cwd, &state.paths.agents_root, prefix, &state.manager)?;
    Ok((ctx, cwd))
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

/// The shared core of `spawn_agent` (and, from batch 2, `mira_spawn_agent`).
pub fn spawn_core(
    app: &AppHandle,
    state: &AppState,
    profile_id: Option<String>,
    overrides: Option<SpawnOverrides>,
    cwd: Option<String>,
    prompt: Option<String>,
    seat_kind: Option<SeatKind>,
) -> Result<AgentInfo, String> {
    let profile = resolve_profile(&state.profiles, profile_id.as_deref())?;
    // Validate the overrides before anything is written or a folder created.
    validate_overrides(overrides.clone().unwrap_or_default())?;
    let (ctx, cwd) = prepare_spawn(state, &profile, cwd)?;
    let req = spawn_request(&profile, overrides, cwd, prompt, seat_kind)?;
    spawn_prepared(app, state, &ctx, req)
}

/// `profileId` null → `coder`; `overrides` replace the profile's model/effort for this agent;
/// `cwd` null/blank → default folder (prefix from the roles); `seatKind` null → the profile's
/// `defaultSeat`. After [`STARTING_HINT_AFTER`] without a hook event, the agent gets the
/// Starting hint.
// TODO(windows-verify): a spawn from a profile starts claude with the profile's settings file
// and `--model`/`--effort` when set; the TUI header shows them (plan5 D.50).
#[tauri::command]
pub fn spawn_agent(
    app: AppHandle,
    state: State<'_, AppState>,
    profile_id: Option<String>,
    overrides: Option<SpawnOverrides>,
    cwd: Option<String>,
    prompt: Option<String>,
    seat_kind: Option<SeatKind>,
) -> Result<AgentInfo, String> {
    spawn_core(&app, &state, profile_id, overrides, cwd, prompt, seat_kind)
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
    cwd: Option<String>,
    seat_kind: Option<SeatKind>,
) -> Result<AgentInfo, String> {
    spawn_with_ticket_core(
        &app, &state, &ticket_id, profile_id, overrides, cwd, seat_kind,
    )
}

/// The shared core of `spawn_agent_with_ticket` and `mira_spawn_agent` with `firstTicketId`.
pub fn spawn_with_ticket_core(
    app: &AppHandle,
    state: &AppState,
    ticket_id: &str,
    profile_id: Option<String>,
    overrides: Option<SpawnOverrides>,
    cwd: Option<String>,
    seat_kind: Option<SeatKind>,
) -> Result<AgentInfo, String> {
    let ticket = ticket_for_spawn(&state.tickets, ticket_id)?;
    let profile = resolve_profile(&state.profiles, profile_id.as_deref())?;
    validate_overrides(overrides.clone().unwrap_or_default())?;
    let (ctx, cwd) = prepare_spawn(state, &profile, cwd)?;
    let file = prompt::write_ticket_file(&cwd, &ticket, now_ms())
        .map_err(|e| format!("Kunne ikke skrive ticket-fil: {e}"))?;
    let req = spawn_request(
        &profile,
        overrides,
        cwd,
        Some(prompt::line_for(&ticket)),
        seat_kind,
    )?;
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
/// `spawn_agent_with_ticket`. Default folder, no overrides.
pub fn spawn_for_tool(app: &AppHandle, req: SpawnByProfile) -> Result<AgentInfo, String> {
    let state = app
        .try_state::<AppState>()
        .ok_or_else(|| SPAWN_UNAVAILABLE.to_string())?;
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
                None,
                req.seat_kind,
            )
        }
        None => spawn_core(
            app,
            &state,
            Some(req.profile_id),
            None,
            None,
            None,
            req.seat_kind,
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
    SpawnRequest {
        cwd: PathBuf::from(&info.cwd),
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
    let info = check_restartable(lock(&state.manager).get(agent_id).as_ref())?;
    let (model, effort) = restart_values(&info, model, effort);
    let profile = state.profiles.get(&info.profile_id);
    let ctx = spawn_context(state, &info.profile_id, profile.as_ref())?;
    let req = restart_request(&info, model.clone(), effort);
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
fn refresh_profile_files(paths: &AppPaths, profile: &AgentProfile) {
    if let Err(e) = write_profile_files(paths, profile) {
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
    refresh_profile_files(&state.paths, &saved);
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
    refresh_profile_files(&state.paths, &p);
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
/// ("permissions" | "diagnostics" | "tickets") in it. Async on purpose: creating a window from a
/// synchronous command deadlocks on Windows (research2 §5).
// TODO(windows-verify): the "n i review" chip in the non-focusable island opens the workplace on
// the Tickets tab, both when the window is created and when it is already open (plan D.36).
#[tauri::command]
pub async fn open_workplace(
    app: AppHandle,
    state: State<'_, AppState>,
    agent_id: Option<String>,
    tab: Option<String>,
) -> Result<(), String> {
    check_tab(tab.as_deref())?;
    let selection = WorkplaceSelection { agent_id, tab };
    *lock(&state.workplace_select) = Some(selection.clone());
    let created =
        workplace::open_or_focus(&app).map_err(|e| format!("Kunne ikke åbne Workplace: {e}"))?;
    log::info!(
        "workplace {} (select {:?}, tab {:?})",
        if created { "created" } else { "focused" },
        selection.agent_id,
        selection.tab
    );
    if !created && (selection.agent_id.is_some() || selection.tab.is_some()) {
        // A new window fetches the selection itself via take_workplace_selection. The slot is
        // kept here too, in case the existing window is still loading and misses the event.
        if let Err(e) = app.emit_to(workplace::LABEL, WORKPLACE_SELECT, &selection) {
            log::debug!("emit {WORKPLACE_SELECT}: {e}");
        }
    }
    Ok(())
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
) -> Result<TicketSummary, String> {
    ticket_create(&state.tickets, &title, &body, skip_review)
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
) -> Result<TicketSummary, String> {
    ticket_assign(&state.tickets, &id, &agent_id)
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
            agents_root: "/h/mira-bots/agents".into(),
        };
        assert_eq!(
            serde_json::to_value(&info).unwrap(),
            json!({"claudePath":null,"hookExe":"/h","settingsJson":"/d/settings.json",
                   "pipeName":"pipe","maxAgents":5,"version":"0.1.0","pipeReady":false,
                   "maxStaffAgents":3,"agentsRoot":"/h/mira-bots/agents"})
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
                agents_root: dir.join("agents"),
                tickets_file: dir.join("tickets.json"),
                profiles_dir: profiles_dir(&dir.join("agents")),
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
                ProfileStore::load(profiles_dir(&dir.join("agents")), 1),
                Arc::new(|_, _| {}),
            )),
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
        assert_eq!((d.frames_received, d.frames_unknown_session), (1, 1));
        assert_eq!(d.last_hook_event.unwrap().name, "Stop");
        assert!(d.log_path.unwrap().ends_with("mira-bots.log"));
        assert_eq!(d.running_agents, 1, "the stopped agent does not count");
        assert!(d.agents_root.ends_with("agents"));
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
        assert_eq!(info.agents_root, d.agents_root);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn workplace_selection_is_taken_once() {
        let sel = WorkplaceSelection {
            agent_id: Some("a1".into()),
            tab: Some("tickets".into()),
        };
        let slot = Mutex::new(Some(sel.clone()));
        assert_eq!(take_selection(&slot), Some(sel));
        assert_eq!(take_selection(&slot), None);
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
        let (settings, prompt) = write_profile_files(&state.paths, &saved).unwrap();
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
        let tk = ticket_create(&t.ctx, "  Opgave  ", "", false).unwrap();
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
            let tk = ticket_create(&t.ctx, "x", "", false).unwrap();
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
        let other = ticket_create(&t.ctx, "y", "", false).unwrap();
        ticket_assign(&t.ctx, &other.id, &live).unwrap();
        lock(&t.ctx.manager).set_detail(&live, Some(NOT_SUBMITTED_TEXT.into()));
        ticket_set_state(&t.ctx, &other.id, TicketState::Backlog, None).unwrap();
        assert_eq!(detail(&t).as_deref(), Some(NOT_SUBMITTED_TEXT));
        // Back to the backlog from in-progress clears it too.
        ticket_set_state(&t.ctx, &id, TicketState::Backlog, None).unwrap();
        assert_eq!(detail(&t), None);
    }

    #[test]
    fn set_ticket_state_maps_errors_to_danish_text() {
        let (mut t, live, _) = tickets_setup();
        let tk = ticket_create(&t.ctx, "x", "", false).unwrap();
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
    fn reject_requeues_for_a_live_agent_and_approve_finishes() {
        let (mut t, live, dead) = tickets_setup();
        let mk = |t: &TestCtx, agent: &str| {
            let tk = ticket_create(&t.ctx, "x", "", false).unwrap();
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
                let tk = ticket_create(&t.ctx, &format!("t{i}"), "", false).unwrap();
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
        let fresh = ticket_create(&t.ctx, "ny", "", false).unwrap();
        assert_eq!(ticket_for_spawn(&t.ctx, &fresh.id).unwrap().id, fresh.id);
    }

    #[test]
    fn a_spawned_ticket_waits_for_the_session_and_heads_the_queue() {
        let (mut t, live, _) = tickets_setup();
        let tk = ticket_create(&t.ctx, "x", "", false).unwrap();
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
            let tk = ticket_create(&t.ctx, "x", "", false).unwrap();
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
        let backlog = ticket_create(&t.ctx, "y", "", false).unwrap();
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

    #[test]
    fn resolve_cwd_uses_explicit_or_next_default_folder() {
        let base = std::env::temp_dir().join(format!("mira-cwd-{}", uuid::Uuid::new_v4()));
        let root = base.join("agents");
        let m = Mutex::new(AgentManager::new(5));
        assert_eq!(
            resolve_cwd(Some("/w/x".into()), &root, "bot", &m).unwrap(),
            PathBuf::from("/w/x")
        );
        let first = resolve_cwd(Some("  ".into()), &root, "bot", &m).unwrap();
        assert_eq!(first, root.join("bot-01"));
        assert!(first.is_dir());
        lock(&m).insert_fake("s", &first.to_string_lossy());
        assert_eq!(
            resolve_cwd(None, &root, "bot", &m).unwrap(),
            root.join("bot-02")
        );
        assert_eq!(
            resolve_cwd(None, &root, "researcher", &m).unwrap(),
            root.join("researcher-01")
        );
        std::fs::remove_dir_all(&base).unwrap();
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
        let tk = ticket_create(&t.ctx, "x", "", false).unwrap();
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
