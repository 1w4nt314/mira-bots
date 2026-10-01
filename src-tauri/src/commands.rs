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
use crate::agent::workdir::{ensure_dir, next_agent_dir};
use crate::agent::{
    now_ms, AgentError, AgentInfo, AgentManager, AgentRole, EventSink, SeatKind, SpawnContext,
    SpawnRequest,
};
use crate::config::{AUTO_REVIEW_ON_STOP, MAX_STAFF_AGENTS, MAX_WORK_AGENTS, STARTING_HINT_AFTER};
use crate::diagnostics::{version_fields, Diagnostics, HookStats, VersionProbe};
use crate::events::{AgentOutputPayload, WorkplaceSelection, AGENTS_CHANGED, WORKPLACE_SELECT};
use crate::hooks::settings::write_settings_json;
use crate::hooks::status::AgentStatus;
use crate::island::{self, IslandState};
use crate::mcp;
use crate::permissions::{Decision, PendingPermissions, PermissionRequestInfo};
use crate::tickets::dispatcher::DispatchMsg;
use crate::tickets::model::{Ticket, TicketError, TicketPatch, TicketState, TicketSummary};
use crate::tickets::{prompt, TicketsCtx, AGENT_STOPPED_NOTE};
use crate::workplace;

/// Locations resolved once in `setup`. The `claude` binary is not cached: it is looked up again
/// on every `get_app_info` and `spawn_agent` (cheap), so installing it while the app runs works.
#[derive(Clone, Debug)]
pub struct AppPaths {
    /// `mira-hook` binary; `None` disables spawning (`AgentError::HookExeNotFound`).
    pub hook_exe: Option<PathBuf>,
    /// `<data_dir>/settings.json` (hooks + permissions), passed to `claude --settings`.
    pub settings_json: PathBuf,
    /// `mira-mcp` binary; `None`: agents are spawned without `--mcp-config` and
    /// `--append-system-prompt-file` (no tools).
    pub mcp_exe: Option<PathBuf>,
    /// `<data_dir>/mcp.json`, passed to `claude --mcp-config` (only with `mcp_exe`).
    pub mcp_config: PathBuf,
    /// `<data_dir>/system-prompt.md`, passed to `claude --append-system-prompt-file` (only with
    /// `mcp_exe`).
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
        let (claude_version, claude_version_note, claude_code_args_supported) =
            version_fields(&lock(&self.claude_version));
        Diagnostics {
            claude_path: path_string(&find_claude()),
            claude_version,
            claude_version_note,
            claude_code_args_supported,
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
/// (`<agents_root>/<prefix>-<nn>`), created on disk.
pub fn resolve_cwd(
    cwd: Option<String>,
    agents_root: &std::path::Path,
    role: AgentRole,
    manager: &Mutex<AgentManager>,
) -> Result<PathBuf, AgentError> {
    match cwd {
        Some(s) if !s.trim().is_empty() => Ok(PathBuf::from(s)),
        _ => {
            let taken = lock(manager).cwds();
            let dir = next_agent_dir(agents_root, role, &taken);
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
pub const WORKPLACE_TABS: [&str; 3] = ["permissions", "diagnostics", "tickets"];

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

pub fn ticket_delete(t: &TicketsCtx, id: &str) -> Result<(), String> {
    t.mutate(|s| s.delete(id))
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
    let old = assignee_of(t, id)?;
    let live = old.as_deref().is_some_and(|a| agent_live(&t.manager, a));
    let now = now_ms();
    let tk = t.mutate(|s| s.set_state(id, target, note, live, now))?;
    t.notify(old.into_iter().chain(tk.assignee_agent_id.clone()));
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

/// Everything a spawn needs that can fail before the agent exists, in this order: hook exe, pipe
/// ready, claude, settings.json (+ mcp.json and system-prompt.md when mira-mcp exists), then the
/// working folder (so a refused spawn never creates a default folder).
pub fn prepare_spawn(
    state: &AppState,
    cwd: Option<String>,
    role: AgentRole,
) -> Result<(SpawnContext, PathBuf), AgentError> {
    let hook_exe = state
        .paths
        .hook_exe
        .as_ref()
        .ok_or(AgentError::HookExeNotFound)?;
    check_pipe_ready(&state.pipe_ready)?;
    let claude = find_claude().ok_or(AgentError::ClaudeNotFound)?;
    // Rewrite the files before every spawn (idempotent) so a moved exe is picked up.
    let settings_json =
        write_settings_json(&state.paths.data_dir, hook_exe).map_err(AgentError::Io)?;
    let (mcp_config, system_prompt) = match &state.paths.mcp_exe {
        Some(mcp_exe) => (
            Some(mcp::write_mcp_json(&state.paths.data_dir, mcp_exe).map_err(AgentError::Io)?),
            Some(mcp::write_system_prompt(&state.paths.data_dir).map_err(AgentError::Io)?),
        ),
        // Without the MCP server the system prompt would ask for tools that do not exist.
        None => (None, None),
    };
    let ctx = SpawnContext {
        claude,
        settings_json,
        mcp_config,
        system_prompt,
        pipe_name: state.paths.pipe_name.clone(),
    };
    let cwd = resolve_cwd(cwd, &state.paths.agents_root, role, &state.manager)?;
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
        "spawned agent {} ({}) in {} role={:?} seat={:?} session={} pid={:?}",
        info.id,
        info.name,
        info.cwd,
        info.role,
        info.seat_kind,
        info.session_id,
        info.pid
    );
    state.emit_agents(app);
    schedule_starting_hint(app.clone(), Arc::clone(&state.manager), info.id.clone());
    Ok(info)
}

/// The shared core of `spawn_agent` and `spawn_agent_with_ticket`.
pub fn spawn_core(
    app: &AppHandle,
    state: &AppState,
    cwd: Option<String>,
    prompt: Option<String>,
    role: Option<AgentRole>,
    seat_kind: Option<SeatKind>,
) -> Result<AgentInfo, String> {
    let role = role.unwrap_or_default();
    let (ctx, cwd) = prepare_spawn(state, cwd, role)?;
    let req = SpawnRequest {
        cwd,
        prompt,
        role,
        seat_kind: seat_kind.unwrap_or_default(),
    };
    spawn_prepared(app, state, &ctx, req)
}

/// `cwd` null/blank → default folder; `role` defaults to `none`, `seat_kind` to `work`.
/// After [`STARTING_HINT_AFTER`] without a hook event, the agent gets the Starting hint.
#[tauri::command]
pub fn spawn_agent(
    app: AppHandle,
    state: State<'_, AppState>,
    cwd: Option<String>,
    prompt: Option<String>,
    role: Option<AgentRole>,
    seat_kind: Option<SeatKind>,
) -> Result<AgentInfo, String> {
    spawn_core(&app, &state, cwd, prompt, role, seat_kind)
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
    cwd: Option<String>,
    role: Option<AgentRole>,
    seat_kind: Option<SeatKind>,
) -> Result<AgentInfo, String> {
    let ticket = ticket_for_spawn(&state.tickets, &ticket_id)?;
    let role = role.unwrap_or_default();
    let (ctx, cwd) = prepare_spawn(&state, cwd, role)?;
    let file = prompt::write_ticket_file(&cwd, &ticket, now_ms())
        .map_err(|e| format!("Kunne ikke skrive ticket-fil: {e}"))?;
    let req = SpawnRequest {
        cwd,
        prompt: Some(prompt::line_for(&ticket)),
        role,
        seat_kind: seat_kind.unwrap_or_default(),
    };
    let info = match spawn_prepared(&app, &state, &ctx, req) {
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

#[tauri::command]
pub fn quit_app(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    lock(&state.manager).kill_all();
    app.exit(0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
            max_staff_agents: 2,
            agents_root: "/h/mira-bots/agents".into(),
        };
        assert_eq!(
            serde_json::to_value(&info).unwrap(),
            json!({"claudePath":null,"hookExe":"/h","settingsJson":"/d/settings.json",
                   "pipeName":"pipe","maxAgents":5,"version":"0.1.0","pipeReady":false,
                   "maxStaffAgents":2,"agentsRoot":"/h/mira-bots/agents"})
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
            },
            island: IslandState::default(),
            pipe_ready: Arc::new(AtomicBool::new(true)),
            sink: Arc::new(|_| {}),
            hook_stats: Arc::new(HookStats::default()),
            claude_version: Arc::new(Mutex::new(VersionProbe::Ok("2.1.286 (Claude Code)".into()))),
            workplace_select: Mutex::new(None),
            tickets: t.ctx,
            tickets_warning: Some("tickets.json kunne ikke læses".into()),
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
        let info = state.app_info();
        assert_eq!(info.max_staff_agents, 2);
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
    fn only_known_workplace_tabs_are_accepted() {
        for tab in [
            None,
            Some("permissions"),
            Some("diagnostics"),
            Some("tickets"),
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
            resolve_cwd(Some("/w/x".into()), &root, AgentRole::None, &m).unwrap(),
            PathBuf::from("/w/x")
        );
        let first = resolve_cwd(Some("  ".into()), &root, AgentRole::None, &m).unwrap();
        assert_eq!(first, root.join("bot-01"));
        assert!(first.is_dir());
        lock(&m).insert_fake("s", &first.to_string_lossy());
        assert_eq!(
            resolve_cwd(None, &root, AgentRole::None, &m).unwrap(),
            root.join("bot-02")
        );
        assert_eq!(
            resolve_cwd(None, &root, AgentRole::Researcher, &m).unwrap(),
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
}
