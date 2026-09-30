//! Tauri commands (contract C.1) and the managed [`AppState`].
//!
//! All commands are synchronous and return `Result<T, String>`; errors are Danish, user-facing
//! text. Locks are held briefly and never while emitting.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::agent::claude_path::find_claude;
use crate::agent::{AgentError, AgentInfo, AgentManager, EventSink, SpawnContext, SpawnRequest};
use crate::config::MAX_WORK_AGENTS;
use crate::events::{AgentOutputPayload, AGENTS_CHANGED};
use crate::hooks::settings::write_hooks_json;
use crate::island::{self, IslandState};
use crate::permissions::{Decision, PendingPermissions, PermissionRequestInfo};

/// Locations resolved once in `setup`.
#[derive(Clone, Debug)]
pub struct AppPaths {
    /// `claude` binary, if found at startup (looked up again at spawn time when `None`).
    pub claude: Option<PathBuf>,
    /// `mira-hook` binary; `None` disables spawning (`AgentError::HookExeNotFound`).
    pub hook_exe: Option<PathBuf>,
    /// `<data_dir>/hooks.json`, passed to `claude --settings`.
    pub hooks_json: PathBuf,
    /// Named pipe (Windows) or socket path (Linux dev) the hook exe connects to.
    pub pipe_name: String,
    /// App data directory (`%APPDATA%\dk.mira.bots` on Windows).
    pub data_dir: PathBuf,
}

/// Managed state shared by commands, the pipe handler and the PTY threads.
pub struct AppState {
    pub manager: Arc<Mutex<AgentManager>>,
    pub pending: Arc<Mutex<PendingPermissions>>,
    pub paths: AppPaths,
    pub island: IslandState,
    /// Receives PTY output/exit from the manager's threads (see `lib.rs::tauri_sink`).
    pub sink: EventSink,
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
    pub fn app_info(&self) -> AppInfo {
        AppInfo {
            claude_path: path_string(&self.paths.claude),
            hook_exe: path_string(&self.paths.hook_exe),
            hooks_json: self.paths.hooks_json.to_string_lossy().into_owned(),
            pipe_name: self.paths.pipe_name.clone(),
            max_agents: MAX_WORK_AGENTS,
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Emits the full agent list as `agents-changed`.
    pub fn emit_agents(&self, app: &AppHandle) {
        let list = lock(&self.manager).list();
        if let Err(e) = app.emit(AGENTS_CHANGED, &list) {
            log::error!("emit {AGENTS_CHANGED}: {e}");
        }
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
pub fn spawn_agent(
    app: AppHandle,
    state: State<'_, AppState>,
    cwd: String,
    prompt: Option<String>,
) -> Result<AgentInfo, String> {
    let hook_exe = state
        .paths
        .hook_exe
        .as_ref()
        .ok_or(AgentError::HookExeNotFound)?;
    let claude = state
        .paths
        .claude
        .clone()
        .or_else(find_claude)
        .ok_or(AgentError::ClaudeNotFound)?;
    // Rewrite hooks.json before every spawn (idempotent) so a moved hook exe is picked up.
    let hooks_json = write_hooks_json(&state.paths.data_dir, hook_exe).map_err(AgentError::Io)?;
    let ctx = SpawnContext {
        claude,
        hooks_json,
        pipe_name: state.paths.pipe_name.clone(),
    };
    let req = SpawnRequest {
        cwd: PathBuf::from(cwd),
        prompt,
    };
    let info = lock(&state.manager).spawn(req, &ctx, Arc::clone(&state.sink))?;
    state.emit_agents(&app);
    Ok(info)
}

#[tauri::command]
pub fn stop_agent(
    app: AppHandle,
    state: State<'_, AppState>,
    agent_id: String,
) -> Result<(), String> {
    stop(&state.manager, &state.pending, &agent_id)?;
    state.emit_agents(&app);
    Ok(())
}

#[tauri::command]
pub fn remove_agent(
    app: AppHandle,
    state: State<'_, AppState>,
    agent_id: String,
) -> Result<(), String> {
    lock(&state.manager).remove(&agent_id)?;
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
        };
        assert_eq!(
            serde_json::to_value(&info).unwrap(),
            json!({"claudePath":null,"hookExe":"/h","hooksJson":"/d/hooks.json",
                   "pipeName":"pipe","maxAgents":5,"version":"0.1.0"})
        );
    }
}
