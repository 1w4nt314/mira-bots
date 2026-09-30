//! Tauri commands (contracts C.1 + C2.1) and the managed [`AppState`].
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
use crate::config::{MAX_STAFF_AGENTS, MAX_WORK_AGENTS, STARTING_HINT_AFTER};
use crate::diagnostics::{version_fields, Diagnostics, HookStats, VersionProbe};
use crate::events::{AgentOutputPayload, AGENTS_CHANGED, WORKPLACE_SELECT};
use crate::hooks::settings::write_hooks_json;
use crate::island::{self, IslandState};
use crate::permissions::{Decision, PendingPermissions, PermissionRequestInfo};
use crate::workplace;

/// Locations resolved once in `setup`. The `claude` binary is not cached: it is looked up again
/// on every `get_app_info` and `spawn_agent` (cheap), so installing it while the app runs works.
#[derive(Clone, Debug)]
pub struct AppPaths {
    /// `mira-hook` binary; `None` disables spawning (`AgentError::HookExeNotFound`).
    pub hook_exe: Option<PathBuf>,
    /// `<data_dir>/hooks.json`, passed to `claude --settings`.
    pub hooks_json: PathBuf,
    /// Named pipe (Windows) or socket path (Linux dev) the hook exe connects to.
    pub pipe_name: String,
    /// App data directory (`%APPDATA%\dk.mira.bots` on Windows).
    pub data_dir: PathBuf,
    /// `<app_log_dir>/mira-bots.log` (`%LOCALAPPDATA%\dk.mira.bots\logs` on Windows); `None` if
    /// the log dir could not be resolved.
    pub log_file: Option<PathBuf>,
    /// `<home>/mira-bots/agents`: parent of the default agent folders (created on first use).
    pub agents_root: PathBuf,
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
    /// Agent to select when a newly created workplace window asks (`take_workplace_selection`).
    pub workplace_select: Mutex<Option<String>>,
}

/// `AppInfo` (C.1), camelCase.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    pub claude_path: Option<String>,
    pub hook_exe: Option<String>,
    pub hooks_json: String,
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
            hooks_json: self.paths.hooks_json.to_string_lossy().into_owned(),
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
            hooks_json_path: self.paths.hooks_json.to_string_lossy().into_owned(),
            hooks_json_exists: self.paths.hooks_json.is_file(),
            pipe_name: self.paths.pipe_name.clone(),
            pipe_ready: self.pipe_ready.load(Ordering::Acquire),
            frames_received: self.hook_stats.received(),
            frames_unknown_session: self.hook_stats.unknown(),
            last_hook_event: self.hook_stats.last_event(),
            log_path: path_string(&self.paths.log_file),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            agents_root: self.paths.agents_root.to_string_lossy().into_owned(),
            running_agents: lock(&self.manager).running_count(),
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
pub fn take_selection(slot: &Mutex<Option<String>>) -> Option<String> {
    lock(slot).take()
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
    let role = role.unwrap_or_default();
    let seat_kind = seat_kind.unwrap_or_default();
    let hook_exe = state
        .paths
        .hook_exe
        .as_ref()
        .ok_or(AgentError::HookExeNotFound)?;
    check_pipe_ready(&state.pipe_ready)?;
    let claude = find_claude().ok_or(AgentError::ClaudeNotFound)?;
    // Rewrite hooks.json before every spawn (idempotent) so a moved hook exe is picked up.
    let hooks_json = write_hooks_json(&state.paths.data_dir, hook_exe).map_err(AgentError::Io)?;
    let ctx = SpawnContext {
        claude,
        hooks_json,
        pipe_name: state.paths.pipe_name.clone(),
    };
    let cwd = resolve_cwd(cwd, &state.paths.agents_root, role, &state.manager)?;
    let req = SpawnRequest {
        cwd,
        prompt,
        role,
        seat_kind,
    };
    let info = lock(&state.manager).spawn(req, &ctx, Arc::clone(&state.sink))?;
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
    state.emit_agents(&app);
    schedule_starting_hint(app, Arc::clone(&state.manager), info.id.clone());
    Ok(info)
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
    state.emit_agents(&app);
    Ok(())
}

#[tauri::command]
pub fn write_agent_input(
    state: State<'_, AppState>,
    agent_id: String,
    data: String,
) -> Result<(), String> {
    lock(&state.manager)
        .write_input(&agent_id, data.as_bytes())
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

/// Opens (or focuses) the workplace window and selects `agent_id` in it. Async on purpose:
/// creating a window from a synchronous command deadlocks on Windows (research2 §5).
#[tauri::command]
pub async fn open_workplace(
    app: AppHandle,
    state: State<'_, AppState>,
    agent_id: Option<String>,
) -> Result<(), String> {
    *lock(&state.workplace_select) = agent_id.clone();
    let created =
        workplace::open_or_focus(&app).map_err(|e| format!("Kunne ikke åbne Workplace: {e}"))?;
    log::info!(
        "workplace {} (select {agent_id:?})",
        if created { "created" } else { "focused" }
    );
    if !created {
        // A new window fetches the selection itself via take_workplace_selection. The slot is
        // kept here too, in case the existing window is still loading and misses the event.
        if let Some(id) = agent_id {
            if let Err(e) = app.emit_to(workplace::LABEL, WORKPLACE_SELECT, id) {
                log::debug!("emit {WORKPLACE_SELECT}: {e}");
            }
        }
    }
    Ok(())
}

#[tauri::command]
pub fn take_workplace_selection(state: State<'_, AppState>) -> Result<Option<String>, String> {
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

#[tauri::command]
pub fn quit_app(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    lock(&state.manager).kill_all();
    app.exit(0);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
            hooks_json: "/d/hooks.json".into(),
            pipe_name: "pipe".into(),
            max_agents: 5,
            version: "0.1.0".into(),
            pipe_ready: false,
            max_staff_agents: 2,
            agents_root: "/h/mira-bots/agents".into(),
        };
        assert_eq!(
            serde_json::to_value(&info).unwrap(),
            json!({"claudePath":null,"hookExe":"/h","hooksJson":"/d/hooks.json",
                   "pipeName":"pipe","maxAgents":5,"version":"0.1.0","pipeReady":false,
                   "maxStaffAgents":2,"agentsRoot":"/h/mira-bots/agents"})
        );
    }

    fn app_state(dir: &std::path::Path) -> AppState {
        let mut m = AgentManager::new(5);
        m.insert_fake("sess", "/w/demo");
        let stopped = m.insert_fake("sess-2", "/w/demo2");
        m.stop(&stopped).unwrap();
        AppState {
            manager: Arc::new(Mutex::new(m)),
            pending: Arc::new(Mutex::new(PendingPermissions::new())),
            paths: AppPaths {
                hook_exe: None,
                hooks_json: dir.join("hooks.json"),
                pipe_name: "pipe".into(),
                data_dir: dir.to_path_buf(),
                log_file: Some(dir.join("logs").join("mira-bots.log")),
                agents_root: dir.join("agents"),
            },
            island: IslandState::default(),
            pipe_ready: Arc::new(AtomicBool::new(true)),
            sink: Arc::new(|_| {}),
            hook_stats: Arc::new(HookStats::default()),
            claude_version: Arc::new(Mutex::new(VersionProbe::Ok("2.1.286 (Claude Code)".into()))),
            workplace_select: Mutex::new(None),
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
        assert!(!d.hooks_json_exists);
        assert!(d.pipe_ready);
        assert_eq!((d.frames_received, d.frames_unknown_session), (1, 1));
        assert_eq!(d.last_hook_event.unwrap().name, "Stop");
        assert!(d.log_path.unwrap().ends_with("mira-bots.log"));
        assert_eq!(d.running_agents, 1, "the stopped agent does not count");
        assert!(d.agents_root.ends_with("agents"));
        std::fs::write(dir.join("hooks.json"), "{}").unwrap();
        *lock(&state.claude_version) = VersionProbe::Pending;
        let d = state.diagnostics();
        assert!(d.hooks_json_exists);
        assert_eq!(d.claude_version_note.as_deref(), Some("kører stadig"));
        assert_eq!(d.claude_code_args_supported, None);
        let info = state.app_info();
        assert_eq!(info.max_staff_agents, 2);
        assert_eq!(info.agents_root, d.agents_root);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn workplace_selection_is_taken_once() {
        let slot = Mutex::new(Some("a1".to_string()));
        assert_eq!(take_selection(&slot), Some("a1".into()));
        assert_eq!(take_selection(&slot), None);
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
