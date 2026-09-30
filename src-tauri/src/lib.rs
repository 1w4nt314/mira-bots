pub mod agent;
pub mod commands;
pub mod config;
pub mod diagnostics;
pub mod events;
pub mod hooks;
pub mod island;
pub mod permissions;
pub mod pipe;
pub mod workplace;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager, RunEvent, WindowEvent};
use tauri_plugin_log::{FileOpenStrategy, RotationStrategy, Target, TargetKind, TimezoneStrategy};

use agent::claude_path::find_claude;
use agent::workdir::agents_root;
use agent::{AgentManager, EventSink, SinkEvent};
use commands::{AppPaths, AppState};
use config::{
    CLAUDE_VERSION_TIMEOUT, HOOK_EXE_ENV, LOG_FILE_STEM, LOG_KEEP_FILES, LOG_LEVEL_ENV,
    LOG_MAX_FILE_SIZE, MAX_WORK_AGENTS,
};
use diagnostics::{log_level_from_env, probe_claude_version, HookStats, VersionProbe};
use events::{AgentOutputPayload, AGENTS_CHANGED, AGENT_OUTPUT};
use hooks::settings::write_hooks_json;
use island::IslandState;
use permissions::PendingPermissions;
use pipe::handler::HandlerCtx;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Candidate locations of the hook exe, in lookup order:
/// the `MIRA_HOOK_EXE` override, the installed resource dir (`resources/` first, then the dir
/// itself), the directory of the running exe and, in debug builds, the workspace's
/// `target/debug|release` (dev run from the repo).
fn hook_exe_candidates(
    env_override: Option<PathBuf>,
    resource_dir: Option<&Path>,
    exe_dir: Option<&Path>,
    workspace_dir: Option<&Path>,
) -> Vec<PathBuf> {
    let name = format!("mira-hook{}", std::env::consts::EXE_SUFFIX);
    let mut v = Vec::new();
    v.extend(env_override);
    if let Some(r) = resource_dir {
        v.push(r.join("resources").join(&name));
        v.push(r.join(&name));
    }
    if let Some(d) = exe_dir {
        v.push(d.join(&name));
    }
    if let Some(w) = workspace_dir {
        v.push(w.join("target").join("debug").join(&name));
        v.push(w.join("target").join("release").join(&name));
    }
    v
}

/// Finds `mira-hook`: `MIRA_HOOK_EXE` → `resource_dir()/resources/` → next to the app exe /
/// workspace `target/` (dev). The first candidate that exists wins.
// TODO(windows-verify): after an NSIS install the hook exe is found under
// resource_dir()/resources/ (plan D.10).
pub fn find_hook_exe(app: &AppHandle) -> Option<PathBuf> {
    let env_override = std::env::var_os(HOOK_EXE_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let resource_dir = app.path().resource_dir().ok();
    let exe = std::env::current_exe().ok();
    let exe_dir = exe.as_deref().and_then(Path::parent);
    // Compile-time workspace root; only meaningful when running from the source checkout.
    let workspace = cfg!(debug_assertions)
        .then(|| Path::new(env!("CARGO_MANIFEST_DIR")).parent())
        .flatten();
    hook_exe_candidates(env_override, resource_dir.as_deref(), exe_dir, workspace)
        .into_iter()
        .find(|p| p.is_file())
}

/// Binds the manager's PTY thread events to Tauri: output becomes `agent-output` (base64, only to
/// the workplace window; the island never shows terminal output) and an exit marks the agent
/// exited, releases its pending permission requests and emits `agents-changed`.
pub fn tauri_sink(
    app: AppHandle,
    manager: Arc<Mutex<AgentManager>>,
    pending: Arc<Mutex<PendingPermissions>>,
) -> EventSink {
    Arc::new(move |event| match event {
        SinkEvent::Output {
            agent_id,
            seq,
            bytes,
        } => {
            let payload = AgentOutputPayload {
                agent_id,
                seq,
                data_base64: BASE64.encode(bytes),
            };
            // TODO(windows-verify): emit_to a missing workplace window neither spams the log nor
            // loses data (the ring buffer covers it) (plan D.21).
            if let Err(e) = app.emit_to(workplace::LABEL, AGENT_OUTPUT, payload) {
                log::debug!("emit {AGENT_OUTPUT}: {e}");
            }
        }
        SinkEvent::Exited { agent_id, code } => {
            let exited = lock(&manager).mark_exited(&agent_id, code);
            let known = exited.is_some();
            // Drop the PTY (ConPTY ClosePseudoConsole may block) only after the lock is released.
            drop(exited);
            // Handlers waiting on these answer `none` and emit permission-resolved themselves.
            lock(&pending).remove_for_agent(&agent_id);
            if known {
                log::info!("agent {agent_id} exited (code {code:?})");
                let list = lock(&manager).list();
                if let Err(e) = app.emit(AGENTS_CHANGED, &list) {
                    log::error!("emit {AGENTS_CHANGED}: {e}");
                }
            }
        }
    })
}

/// The file logger (plus stdout in debug builds). Level from `MIRA_LOG` (default info); the
/// chatty windowing/HTTP crates are capped at warn.
// TODO(windows-verify): the log file is %LOCALAPPDATA%\dk.mira.bots\logs\mira-bots.log, a new one
// per start (at most 3 old ones kept), and a panic ends up in it; MIRA_LOG=debug gives one line per
// hook frame (plan D.18).
fn log_plugin(level: log::LevelFilter) -> tauri::plugin::TauriPlugin<tauri::Wry> {
    let mut targets = vec![Target::new(TargetKind::LogDir {
        file_name: Some(LOG_FILE_STEM.into()),
    })];
    if cfg!(debug_assertions) {
        targets.push(Target::new(TargetKind::Stdout));
    }
    tauri_plugin_log::Builder::new()
        .clear_targets()
        .targets(targets)
        .level(level)
        .level_for("tao", log::LevelFilter::Warn)
        .level_for("wry", log::LevelFilter::Warn)
        .level_for("hyper", log::LevelFilter::Warn)
        .max_file_size(LOG_MAX_FILE_SIZE)
        .rotation_strategy(RotationStrategy::KeepSome(LOG_KEEP_FILES))
        .file_open_strategy(FileOpenStrategy::Rotate)
        .timezone_strategy(TimezoneStrategy::UseLocal)
        .build()
}

/// Panics go to the log file (a Windows GUI app has no stderr). Runs before the release
/// profile's abort.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        log::error!("panic: {info}");
        log::logger().flush();
    }));
}

/// `<home>/mira-bots/agents`, with home from Tauri, else `USERPROFILE`/`HOME`, else `fallback`.
fn resolve_agents_root(app: &AppHandle, fallback: &Path) -> PathBuf {
    let home = app
        .path()
        .home_dir()
        .ok()
        .or_else(|| {
            ["USERPROFILE", "HOME"]
                .iter()
                .filter_map(std::env::var_os)
                .find(|v| !v.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| fallback.to_path_buf());
    agents_root(&home)
}

/// Runs `claude --version` once on its own thread and stores the result in `slot`.
fn start_version_probe(slot: Arc<Mutex<VersionProbe>>) {
    let Some(claude) = find_claude() else {
        *lock(&slot) = VersionProbe::NotFound;
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("claude-version".into())
        .spawn(move || {
            let result = probe_claude_version(&claude, CLAUDE_VERSION_TIMEOUT);
            match &result {
                Ok(v) => log::info!("claude --version: {v}"),
                Err(e) => log::warn!("claude --version failed: {e}"),
            }
            *lock(&slot) = match result {
                Ok(v) => VersionProbe::Ok(v),
                Err(e) => VersionProbe::Failed(e),
            };
        });
    if let Err(e) = spawned {
        log::warn!("could not start the claude version probe: {e}");
    }
}

fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    // The log plugin is already initialised (plugins run before setup).
    install_panic_hook();
    log::info!(
        "mira-bots {} starting; log level {}",
        env!("CARGO_PKG_VERSION"),
        log::max_level()
    );
    let handle = app.handle().clone();

    let data_dir = app.path().app_data_dir().unwrap_or_else(|e| {
        let fallback = std::env::temp_dir().join("mira-bots");
        log::warn!(
            "app_data_dir unavailable ({e}); using {}",
            fallback.display()
        );
        fallback
    });
    let pipe_name = pipe::protocol::pipe_name(std::process::id());
    let hook_exe = find_hook_exe(&handle);
    match &hook_exe {
        Some(p) => log::info!("hook exe: {}", p.display()),
        None => log::warn!("mira-hook not found; agents cannot be started (set MIRA_HOOK_EXE)"),
    }
    // Only logged: the lookup is repeated on every get_app_info/spawn_agent.
    match find_claude() {
        Some(p) => log::info!("claude: {}", p.display()),
        None => log::warn!("claude not found (set MIRA_CLAUDE_PATH)"),
    }
    let claude_version = Arc::new(Mutex::new(VersionProbe::Pending));
    start_version_probe(Arc::clone(&claude_version));

    let log_file = match app.path().app_log_dir() {
        Ok(d) => Some(d.join(format!("{LOG_FILE_STEM}.log"))),
        Err(e) => {
            log::warn!("app_log_dir unavailable: {e}");
            None
        }
    };
    if let Some(f) = &log_file {
        log::info!("log file: {}", f.display());
    }
    let agents_root = resolve_agents_root(&handle, &data_dir);
    log::info!("default agent folders under {}", agents_root.display());

    // hooks.json is rewritten before each spawn too; writing it now makes the path exist early.
    // Without a hook exe a placeholder path is written; spawn_agent refuses to start agents.
    let hook_for_file = hook_exe
        .clone()
        .unwrap_or_else(|| PathBuf::from("mira-hook-not-found"));
    let hooks_json = match write_hooks_json(&data_dir, &hook_for_file) {
        Ok(p) => {
            log::info!("hooks.json written: {}", p.display());
            p
        }
        Err(e) => {
            log::error!("could not write hooks.json in {}: {e}", data_dir.display());
            data_dir.join("hooks.json")
        }
    };

    let manager = Arc::new(Mutex::new(AgentManager::new(MAX_WORK_AGENTS)));
    let pending = Arc::new(Mutex::new(PendingPermissions::new()));
    let sink = tauri_sink(handle.clone(), Arc::clone(&manager), Arc::clone(&pending));

    let pipe_ready = Arc::new(AtomicBool::new(false));
    let hook_stats = Arc::new(HookStats::default());
    let emit_handle = handle.clone();
    pipe::server::start(
        pipe_name.clone(),
        HandlerCtx {
            manager: Arc::clone(&manager),
            pending: Arc::clone(&pending),
            emit: Arc::new(move |name: &str, payload: Value| {
                if let Err(e) = emit_handle.emit(name, payload) {
                    log::debug!("emit {name}: {e}");
                }
            }),
            stats: Arc::clone(&hook_stats),
        },
        Arc::clone(&pipe_ready),
    );

    app.manage(AppState {
        manager,
        pending,
        paths: AppPaths {
            hook_exe,
            hooks_json,
            pipe_name,
            data_dir,
            log_file,
            agents_root,
        },
        island: IslandState::default(),
        pipe_ready,
        sink,
        hook_stats,
        claude_version,
        workplace_select: Mutex::new(None),
    });

    if let Some(window) = app.get_webview_window(island::LABEL) {
        let (w, h) = island::COLLAPSED;
        if let Err(e) = island::place(&window, w, h) {
            log::warn!("could not place the island: {e}");
        }
    } else {
        log::error!("island window not found");
    }
    Ok(())
}

pub fn run() {
    let level = log_level_from_env(std::env::var(LOG_LEVEL_ENV).ok().as_deref());
    let app = tauri::Builder::default()
        // Sets the global logger; nothing else may (env_logger was removed for this reason).
        .plugin(log_plugin(level))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(setup)
        // Only the island reacts here. Closing the workplace window just destroys it: Tauri
        // raises ExitRequested only when the last window is gone, and the island cannot be closed.
        .on_window_event(|window, event| {
            if window.label() != island::LABEL {
                return;
            }
            if let WindowEvent::ScaleFactorChanged { .. } = event {
                let app = window.app_handle();
                let (Some(state), Some(win)) = (
                    app.try_state::<AppState>(),
                    app.get_webview_window(island::LABEL),
                ) else {
                    return;
                };
                let (w, h) = *lock(&state.island.last);
                if let Err(e) = island::place(&win, w, h) {
                    log::warn!("re-placing the island failed: {e}");
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::ui_ready,
            commands::get_app_info,
            commands::list_agents,
            commands::spawn_agent,
            commands::stop_agent,
            commands::remove_agent,
            commands::write_agent_input,
            commands::resize_agent_pty,
            commands::get_agent_output,
            commands::list_pending_permissions,
            commands::respond_permission,
            commands::resize_island,
            commands::quit_app,
            commands::get_diagnostics,
            commands::open_workplace,
            commands::take_workplace_selection,
            commands::open_agent_folder,
            commands::open_log_dir,
        ])
        .build(tauri::generate_context!())
        .expect("error while building mira-bots");

    app.run(|app_handle, event| {
        if matches!(event, RunEvent::ExitRequested { .. } | RunEvent::Exit) {
            if let Some(state) = app_handle.try_state::<AppState>() {
                lock(&state.manager).kill_all();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_exe_candidates_follow_the_lookup_order() {
        let c = hook_exe_candidates(
            Some(PathBuf::from("/override/h")),
            Some(Path::new("/res")),
            Some(Path::new("/app")),
            Some(Path::new("/ws")),
        );
        let n = format!("mira-hook{}", std::env::consts::EXE_SUFFIX);
        assert_eq!(c[0], PathBuf::from("/override/h"));
        assert_eq!(c[1], Path::new("/res").join("resources").join(&n));
        assert_eq!(c[2], Path::new("/res").join(&n));
        assert_eq!(c[3], Path::new("/app").join(&n));
        assert_eq!(c[4], Path::new("/ws/target/debug").join(&n));
        assert_eq!(c[5], Path::new("/ws/target/release").join(&n));
        assert_eq!(c.len(), 6);
    }

    #[test]
    fn hook_exe_candidates_skip_missing_sources() {
        assert!(hook_exe_candidates(None, None, None, None).is_empty());
    }
}
