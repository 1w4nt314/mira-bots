pub mod agent;
pub mod app_settings;
pub mod checks;
pub mod commands;
pub mod config;
pub mod diagnostics;
pub mod events;
pub mod git;
pub mod hooks;
pub mod island;
pub mod mcp;
pub mod permissions;
pub mod pipe;
pub mod platform;
pub mod proc;
pub mod profiles;
pub mod projects;
pub mod tickets;
pub mod workplace;
pub mod workspace;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde_json::Value;
use tauri::{AppHandle, Emitter, Manager, RunEvent, WindowEvent};
use tauri_plugin_log::{FileOpenStrategy, RotationStrategy, Target, TargetKind, TimezoneStrategy};

use agent::claude_path::find_claude;
use agent::workdir::{ensure_dir, legacy_agents_root, projects_root};
use agent::{now_ms, AgentManager, EventSink, SinkEvent};
use app_settings::AppSettings;
use commands::{AppPaths, AppState};
use config::{
    CLAUDE_VERSION_TIMEOUT, HOOK_EXE_ENV, LOG_FILE_STEM, LOG_KEEP_FILES, LOG_LEVEL_ENV,
    LOG_MAX_FILE_SIZE, MCP_CONFIG_FILE, MCP_EXE_ENV, PROFILE_FILES_DIR, REPORTS_DIR, SETTINGS_FILE,
    SYSTEM_PROMPT_FILE, TICKETS_FILE, WORKSPACE_FILE,
};
use diagnostics::{log_level_from_env, probe_claude_version, HookStats, VersionProbe};
use events::{AgentOutputPayload, EmitFn, StatusEvent, AGENTS_CHANGED, AGENT_OUTPUT};
use hooks::settings::write_settings_json;
use island::IslandState;
use permissions::PendingPermissions;
use pipe::handler::{HandlerCtx, StatusObserver, ToolHandler};
use profiles::store::{migrate_legacy_profiles, profiles_dir, ProfileStore};
use profiles::ProfilesCtx;
use tickets::dispatcher::{self, messages_for, DispatchMsg, Dispatcher, RealTimers};
use tickets::tools::ToolsCtx;
use tickets::{ManagerPort, TicketsCtx, AGENT_EXITED_NOTE};
use workspace::WorkspaceReader;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Candidate locations of a bundled helper exe `name` (without suffix: `mira-hook`,
/// `mira-mcp`), in lookup order: the env override, the installed resource dir (`resources/`
/// first, then the dir itself), the directory of the running exe and, in debug builds, the
/// workspace's `target/debug|release` (dev run from the repo).
fn exe_candidates(
    name: &str,
    env_override: Option<PathBuf>,
    resource_dir: Option<&Path>,
    exe_dir: Option<&Path>,
    workspace_dir: Option<&Path>,
) -> Vec<PathBuf> {
    let name = format!("{name}{}", std::env::consts::EXE_SUFFIX);
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

/// The first existing `name` exe: `<env_var>` → `resource_dir()/resources/` → next to the app
/// exe / workspace `target/` (dev).
fn find_exe(app: &AppHandle, name: &str, env_var: &str) -> Option<PathBuf> {
    let env_override = std::env::var_os(env_var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let resource_dir = app.path().resource_dir().ok();
    let exe = std::env::current_exe().ok();
    let exe_dir = exe.as_deref().and_then(Path::parent);
    // Compile-time workspace root; only meaningful when running from the source checkout.
    let workspace = cfg!(debug_assertions)
        .then(|| Path::new(env!("CARGO_MANIFEST_DIR")).parent())
        .flatten();
    exe_candidates(
        name,
        env_override,
        resource_dir.as_deref(),
        exe_dir,
        workspace,
    )
    .into_iter()
    .find(|p| p.is_file())
}

/// Finds `mira-hook`: `MIRA_HOOK_EXE` → `resource_dir()/resources/` → next to the app exe /
/// workspace `target/` (dev). The first candidate that exists wins.
// TODO(windows-verify): after an NSIS install the hook exe is found under
// resource_dir()/resources/ (plan D.10).
pub fn find_hook_exe(app: &AppHandle) -> Option<PathBuf> {
    find_exe(app, "mira-hook", HOOK_EXE_ENV)
}

/// Finds `mira-mcp` the same way (`MIRA_MCP_EXE` override). `None`: agents get no tools.
// TODO(windows-verify): after an NSIS install mira-mcp.exe is found under
// resource_dir()/resources/ next to mira-hook.exe (plan4 D.39, D.48).
pub fn find_mcp_exe(app: &AppHandle) -> Option<PathBuf> {
    find_exe(app, "mira-mcp", MCP_EXE_ENV)
}

/// Binds the manager's PTY thread events to Tauri: output becomes `agent-output` (base64, only to
/// the workplace window; the island never shows terminal output) and an exit marks the agent
/// exited, releases its pending permission requests and its tickets (back to the backlog, any
/// delivery cancelled) and emits `agents-changed`.
pub fn tauri_sink(
    app: AppHandle,
    manager: Arc<Mutex<AgentManager>>,
    pending: Arc<Mutex<PendingPermissions>>,
    tickets: Arc<TicketsCtx>,
) -> EventSink {
    Arc::new(move |event| match event {
        SinkEvent::Output {
            agent_id,
            seq,
            bytes,
            ..
        } => {
            let payload = AgentOutputPayload {
                agent_id,
                seq,
                data_base64: BASE64.encode(bytes),
            };
            // emit_to only scopes by window label: a JS listener registered with listen() (target
            // Any, the @tauri-apps/api default) in another window would receive this too. It works
            // because the island never listens on `agent-output`; a future island listener must
            // filter on its own (or be given a window-scoped listen), or the IPC load doubles.
            // TODO(windows-verify): emit_to a missing workplace window neither spams the log nor
            // loses data (the ring buffer covers it) (plan D.21).
            if let Err(e) = app.emit_to(workplace::LABEL, AGENT_OUTPUT, payload) {
                log::debug!("emit {AGENT_OUTPUT}: {e}");
            }
        }
        SinkEvent::Exited {
            agent_id,
            gen,
            code,
        } => {
            // `None` too for the exit of a child replaced by a restart (older generation); that
            // exit must not touch the new session (its permission requests, tickets, status).
            let (exited, replaced) = {
                let mut m = lock(&manager);
                let replaced = m.pty_gen(&agent_id).is_some_and(|g| g != gen);
                (m.mark_exited(&agent_id, gen, code), replaced)
            };
            let known = exited.is_some();
            // Drop the PTY (ConPTY ClosePseudoConsole may block) only after the lock is released.
            drop(exited);
            if replaced {
                log::debug!("agent {agent_id}: old child (gen {gen}) exited after a restart");
                return;
            }
            // Handlers waiting on these answer `none` and emit permission-resolved themselves.
            lock(&pending).remove_for_agent(&agent_id);
            if known {
                log::info!("agent {agent_id} exited (code {code:?})");
                if let Err(e) = tickets.release_agent(&agent_id, AGENT_EXITED_NOTE) {
                    log::warn!("releasing the tickets of agent {agent_id} failed: {e}");
                }
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

/// File name of the emergency log in the system temp dir.
const EMERGENCY_LOG_FILE: &str = "mira-bots-panic.log";

/// Emergency file for failures that may happen before (or because of) the log plugin: a Windows
/// GUI app has no stderr, so without this an early failure is a silent exit.
fn emergency_log_path() -> PathBuf {
    std::env::temp_dir().join(EMERGENCY_LOG_FILE)
}

/// One emergency line: `[<unix seconds>.<millis>] <message>` (no chrono dependency; the log file
/// has the local time once it exists).
fn format_emergency_line(since_epoch: std::time::Duration, message: &str) -> String {
    format!(
        "[{}.{:03}] {}\n",
        since_epoch.as_secs(),
        since_epoch.subsec_millis(),
        message
    )
}

/// Appends one timestamped line to `path`, creating the file if needed.
fn write_emergency_line(path: &Path, message: &str) -> std::io::Result<()> {
    use std::io::Write;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(format_emergency_line(now, message).as_bytes())
}

/// Best effort: a failure to write the emergency file has nowhere left to be reported.
fn emergency_log(message: &str) {
    let _ = write_emergency_line(&emergency_log_path(), message);
}

/// Panics go to the log (once the logger is up; `log` is a no-op before that) and always to the
/// emergency file. Installed before the Tauri builder so it also covers plugin initialisation.
/// Runs before the release profile's abort.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        log::error!("panic: {info}");
        log::logger().flush();
        emergency_log(&format!("panic: {info}"));
        // Release builds abort right after this hook, so the socket would stay behind.
        #[cfg(unix)]
        pipe::unix_socket::cleanup_registered();
    }));
}

/// The home folder from Tauri, else `USERPROFILE`/`HOME`, else `fallback`.
fn resolve_home(app: &AppHandle, fallback: &Path) -> PathBuf {
    app.path()
        .home_dir()
        .ok()
        .or_else(|| {
            ["USERPROFILE", "HOME"]
                .iter()
                .filter_map(std::env::var_os)
                .find(|v| !v.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| fallback.to_path_buf())
}

/// The projects root (plan4b A.1): the app setting when set and not blank, otherwise
/// `<home>/mira-bots/projects`. Created if missing (a failure is only logged).
// TODO(macos-verify): profiles in ~/mira-bots/projects/.mira-bots/profiles/, app data in
// ~/Library/Application Support/dk.mira.bots/ (settings.json, mcp.json, tickets.json, profiles/)
// and the log in ~/Library/Logs/dk.mira.bots/mira-bots.log (plan7 M.13)
fn resolve_projects_root(home: &Path, settings: &AppSettings) -> PathBuf {
    let root = settings
        .projects_root()
        .unwrap_or_else(|| projects_root(home));
    if let Err(e) = ensure_dir(&root) {
        log::warn!("could not create the projects root {}: {e}", root.display());
    }
    root
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
    // The log plugin is already initialised (plugins run before setup); the panic hook was
    // installed in run().
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
    let mcp_exe = find_mcp_exe(&handle);
    match &mcp_exe {
        Some(p) => log::info!("mcp exe: {}", p.display()),
        None => log::warn!("mira-mcp not found; agents get no tools (set MIRA_MCP_EXE)"),
    }
    // The login shell's PATH (unix; at most LOGIN_SHELL_TIMEOUT) before the first claude lookup
    // and before the version probe; children get it through process::spawn_env_extra.
    agent::login_env::init();
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
    let home = resolve_home(&handle, &data_dir);
    let settings = app_settings::load(&data_dir);
    let projects_root = resolve_projects_root(&home, &settings);
    log::info!("projects root: {}", projects_root.display());
    let workspace = Arc::new(WorkspaceReader::new(projects_root.join(WORKSPACE_FILE)));
    let first_snapshot = workspace.snapshot();
    log::info!(
        "workspace file: {} ({})",
        workspace.path().display(),
        if first_snapshot.file_exists {
            "findes"
        } else {
            "mangler"
        }
    );

    // settings.json (and mcp.json / system-prompt.md) are rewritten before each spawn too;
    // writing them now makes the paths exist early. Without a hook exe a placeholder path is
    // written; spawn_agent refuses to start agents.
    let hook_for_file = hook_exe
        .clone()
        .unwrap_or_else(|| PathBuf::from("mira-hook-not-found"));
    let settings_json = match write_settings_json(&data_dir, &hook_for_file) {
        Ok(p) => {
            log::info!("settings.json written: {}", p.display());
            p
        }
        Err(e) => {
            log::error!(
                "could not write settings.json in {}: {e}",
                data_dir.display()
            );
            data_dir.join(SETTINGS_FILE)
        }
    };
    // mcp.json only when mira-mcp exists (it would name a missing program otherwise).
    let mcp_config = match &mcp_exe {
        Some(exe) => match mcp::write_mcp_json(&data_dir, exe) {
            Ok(p) => {
                log::info!("mcp.json written: {}", p.display());
                p
            }
            Err(e) => {
                log::error!("could not write mcp.json in {}: {e}", data_dir.display());
                data_dir.join(MCP_CONFIG_FILE)
            }
        },
        None => data_dir.join(MCP_CONFIG_FILE),
    };
    let system_prompt = match mcp::write_system_prompt(&data_dir) {
        Ok(p) => {
            log::info!("system-prompt.md written: {}", p.display());
            p
        }
        Err(e) => {
            log::error!(
                "could not write system-prompt.md in {}: {e}",
                data_dir.display()
            );
            data_dir.join(SYSTEM_PROMPT_FILE)
        }
    };

    let mut agent_manager = AgentManager::new(first_snapshot.rules.max_work_agents);
    agent_manager.set_limits(
        first_snapshot.rules.max_work_agents,
        first_snapshot.rules.max_staff_agents,
    );
    let manager = Arc::new(Mutex::new(agent_manager));
    let pending = Arc::new(Mutex::new(PendingPermissions::new()));
    let emit_handle = handle.clone();
    let emit: EmitFn = Arc::new(move |name: &str, payload: Value| {
        if let Err(e) = emit_handle.emit(name, payload) {
            log::debug!("emit {name}: {e}");
        }
    });

    // Profiles: one JSON file each under <projects_root>/.mira-bots/profiles; the built-ins are
    // generated on first start. The rendered per-profile files are written at spawn/save. The
    // first time, the step 1–5 profiles are copied from <home>/mira-bots/agents (never moved).
    // TODO(windows-verify): on the first start after the upgrade %USERPROFILE%\mira-bots\projects\
    // exists, the seven profiles are copied from agents\.mira-bots\profiles\ (source untouched) and
    // Diagnostik shows the root, the workspace file and "Profiler kopieret ved start: 7" (plan4b D.77).
    let profiles_dir = profiles_dir(&projects_root);
    let legacy_profiles = profiles::store::profiles_dir(&legacy_agents_root(&home));
    let profiles_migrated = match migrate_legacy_profiles(&legacy_profiles, &profiles_dir) {
        Ok(0) => 0,
        Ok(n) => {
            log::info!(
                "{n} profiles copied from {} to {}",
                legacy_profiles.display(),
                profiles_dir.display()
            );
            n
        }
        Err(e) => {
            log::warn!(
                "could not copy the profiles from {}: {e}",
                legacy_profiles.display()
            );
            0
        }
    };
    let profile_store = ProfileStore::load(profiles_dir.clone(), now_ms());
    match profile_store.warning() {
        Some(w) => log::warn!("profiles in {}: {w}", profiles_dir.display()),
        None => log::info!(
            "{} profiles loaded from {}",
            profile_store.len(),
            profiles_dir.display()
        ),
    }
    let profiles = Arc::new(ProfilesCtx::new(profile_store, Arc::clone(&emit)));

    let tickets_file = data_dir.join(TICKETS_FILE);
    let (service, tickets_warning) = tickets::load_tickets(tickets_file.clone(), now_ms());
    let (dispatch_tx, dispatch_rx) = tokio::sync::mpsc::unbounded_channel::<DispatchMsg>();
    // `shared`: the project checks run on their own thread with the context (step 6b).
    let tickets = TicketsCtx::new(
        service,
        Arc::clone(&manager),
        dispatch_tx.clone(),
        Arc::clone(&emit),
        data_dir.join(REPORTS_DIR),
        Arc::clone(&workspace),
        // git is looked up (and probed) on first use, not at startup.
        Arc::new(git::SystemGit::new()),
    )
    .shared();
    let sink = tauri_sink(
        handle.clone(),
        Arc::clone(&manager),
        Arc::clone(&pending),
        Arc::clone(&tickets),
    );
    // Hook frames → dispatcher messages. Frames that arrive before the dispatcher task runs wait
    // in the channel.
    let observer: StatusObserver = {
        let tx = dispatch_tx.clone();
        Arc::new(move |ev: StatusEvent| {
            for m in messages_for(&ev) {
                // Fails only at shutdown, when the dispatcher is gone.
                let _ = tx.send(m);
            }
        })
    };
    tauri::async_runtime::spawn(dispatcher::run(
        dispatch_rx,
        Dispatcher::new(
            Arc::clone(&tickets),
            // Step 6b: a fresh session per ticket restarts through the UI's restart path; the
            // closure looks the state up per call (like the tools' SpawnPort below).
            ManagerPort::new(Arc::clone(&manager), Arc::clone(&emit))
                .with_restart(commands::restart_port(handle.clone())),
            RealTimers::new(dispatch_tx),
        ),
    ));

    // Tool frames from mira-mcp → the agents' ticket tools (the pipe knows nothing about tickets).
    // mira_spawn_agent goes through the UI's spawn path and limits (plan5 punkt 13); the
    // closure looks the state up per call, so frames before `manage` get "not available".
    let tool_handler: ToolHandler = {
        let tools = Arc::new(ToolsCtx::new(Arc::clone(&tickets), Arc::clone(&profiles)));
        let spawn_handle = handle.clone();
        tools.set_spawn_port(Arc::new(move |req| {
            commands::spawn_for_tool(&spawn_handle, req)
        }));
        Arc::new(move |frame| tools.handle_tool(frame, now_ms()))
    };

    let pipe_ready = Arc::new(AtomicBool::new(false));
    let pipe_error = Arc::new(Mutex::new(None));
    let hook_stats = Arc::new(HookStats::default());
    pipe::server::start(
        pipe_name.clone(),
        HandlerCtx {
            manager: Arc::clone(&manager),
            pending: Arc::clone(&pending),
            emit,
            stats: Arc::clone(&hook_stats),
            observer: Some(observer),
            tools: Some(tool_handler),
        },
        Arc::clone(&pipe_ready),
        Arc::clone(&pipe_error),
    );

    let data_dir_for_profiles = data_dir.join(PROFILE_FILES_DIR);
    let app_settings_path = app_settings::settings_path(&data_dir);
    app.manage(AppState {
        manager,
        pending,
        paths: AppPaths {
            hook_exe,
            settings_json,
            mcp_exe,
            mcp_config,
            system_prompt,
            pipe_name,
            data_dir,
            log_file,
            app_settings: app_settings_path,
            projects_root,
            tickets_file,
            profiles_dir,
            profile_files_dir: data_dir_for_profiles,
        },
        island: IslandState::default(),
        pipe_ready,
        pipe_error,
        sink,
        hook_stats,
        claude_version,
        workplace_select: Mutex::new(None),
        tickets,
        tickets_warning,
        profiles,
        workspace,
        profiles_migrated,
    });

    // After `manage`: the exit handler's `kill_all` needs `AppState` (review7 W5).
    #[cfg(unix)]
    platform::signals::install(app.handle().clone());

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
    // Before anything that can fail or panic, so even plugin initialisation leaves a trace.
    install_panic_hook();
    let level = log_level_from_env(std::env::var(LOG_LEVEL_ENV).ok().as_deref());
    let built = tauri::Builder::default()
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
            commands::close_workplace,
            commands::take_workplace_selection,
            commands::open_agent_folder,
            commands::open_log_dir,
            commands::list_tickets,
            commands::get_ticket,
            commands::create_ticket,
            commands::update_ticket,
            commands::delete_ticket,
            commands::assign_ticket,
            commands::unassign_ticket,
            commands::reorder_queue,
            commands::set_ticket_state,
            commands::approve_ticket,
            commands::reject_ticket,
            commands::redispatch_ticket,
            commands::spawn_agent_with_ticket,
            commands::request_submission,
            commands::list_profiles,
            commands::get_profile,
            commands::save_profile,
            commands::delete_profile,
            commands::reset_builtin_profile,
            commands::set_agent_model,
            commands::set_agent_effort,
            commands::add_report,
            commands::get_report,
            commands::open_report_dir,
            commands::assign_reviewer,
            commands::list_review_assignments,
            commands::list_projects,
            commands::create_project,
            commands::open_project_folder,
            commands::set_projects_root,
            commands::move_agent_to_project,
            commands::ticket_start_playbook,
        ])
        .build(tauri::generate_context!());
    // Plugin setup (the log plugin creates its directory and installs the global logger) runs
    // inside build(). The Builder is consumed on failure and cannot be retried without the log
    // plugin, so report to the emergency file and exit with a non-zero code: never silently.
    let mut app = match built {
        Ok(app) => app,
        Err(e) => {
            let message = format!("error while building mira-bots: {e}");
            log::error!("{message}");
            emergency_log(&message);
            std::process::exit(1);
        }
    };

    // After build (the windows exist but the event loop has not started) and before run: the
    // runtime is still owned by `App`, so the policy is set on the event loop before launch and
    // the app never starts as a Regular app (no Dock flash, no focus steal; plan7 A.5).
    platform::apply_activation_policy(&mut app);
    app.run(|app_handle, event| {
        if matches!(event, RunEvent::ExitRequested { .. } | RunEvent::Exit) {
            if let Some(state) = app_handle.try_state::<AppState>() {
                lock(&state.manager).kill_all();
            }
            // Step 6b: git and project-check children still running (with their trees).
            // TODO(windows-verify): closing the app mid-check leaves no process behind (plan6b D.107).
            proc::registry().kill_running();
        }
        // The process exits without dropping the pipe server task (and its SocketGuard).
        #[cfg(unix)]
        if matches!(event, RunEvent::Exit) {
            pipe::unix_socket::cleanup_registered();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emergency_line_has_timestamp_and_message() {
        let line = format_emergency_line(std::time::Duration::from_millis(1_234_567), "boom");
        assert_eq!(line, "[1234.567] boom\n");
    }

    #[test]
    fn emergency_file_is_created_and_appended_to() {
        let dir = std::env::temp_dir().join(format!("mira-bots-emergency-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mira-bots-panic.log");
        let _ = std::fs::remove_file(&path);
        write_emergency_line(&path, "first").unwrap();
        write_emergency_line(&path, "second").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with('[') && lines[0].ends_with("] first"));
        assert!(lines[1].ends_with("] second"));
        // A path that cannot be opened reports an error instead of panicking.
        assert!(write_emergency_line(&dir.join("no/such/dir/x.log"), "x").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn emergency_log_lives_in_the_temp_dir() {
        assert_eq!(
            emergency_log_path(),
            std::env::temp_dir().join(EMERGENCY_LOG_FILE)
        );
    }

    #[test]
    fn exe_candidates_follow_the_lookup_order() {
        for name in ["mira-hook", "mira-mcp"] {
            let c = exe_candidates(
                name,
                Some(PathBuf::from("/override/h")),
                Some(Path::new("/res")),
                Some(Path::new("/app")),
                Some(Path::new("/ws")),
            );
            let n = format!("{name}{}", std::env::consts::EXE_SUFFIX);
            assert_eq!(c[0], PathBuf::from("/override/h"));
            assert_eq!(c[1], Path::new("/res").join("resources").join(&n));
            assert_eq!(c[2], Path::new("/res").join(&n));
            assert_eq!(c[3], Path::new("/app").join(&n));
            assert_eq!(c[4], Path::new("/ws/target/debug").join(&n));
            assert_eq!(c[5], Path::new("/ws/target/release").join(&n));
            assert_eq!(c.len(), 6);
        }
    }

    /// The Tauri command list (plan4b punkt 9: 44 → 49; plan7 punkt 8: → 50; plan6b punkt 6:
    /// → 51). Counted from the source so a command added without a handler (or the other way
    /// round) is noticed.
    #[test]
    fn generate_handler_lists_51_commands() {
        let src = include_str!("lib.rs");
        let start = src.find("generate_handler![").expect("handler list");
        let list = &src[start..start + src[start..].find("])").expect("end of list")];
        let names: Vec<&str> = list
            .lines()
            .filter_map(|l| l.trim().strip_prefix("commands::"))
            .map(|l| l.trim_end_matches(','))
            .collect();
        assert_eq!(names.len(), 51, "{names:?}");
        for n in [
            "ticket_start_playbook",
            "close_workplace",
            "list_projects",
            "create_project",
            "open_project_folder",
            "set_projects_root",
            "move_agent_to_project",
        ] {
            assert!(names.contains(&n), "{n}");
        }
        let mut unique = names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), names.len(), "duplicates");
    }

    /// plan7 punkt 8: the macOS keys in tauri.conf.json and the platform file merged on macOS.
    #[test]
    fn tauri_configs_carry_the_macos_settings() {
        let main: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).expect("tauri.conf.json");
        let island = main["app"]["windows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["label"] == island::LABEL)
            .expect("island window");
        assert_eq!(island["acceptFirstMouse"], true);
        assert_eq!(main["app"]["macOSPrivateApi"], true);
        // The Windows bundle targets stay as they were.
        assert_eq!(
            main["bundle"]["targets"],
            serde_json::json!(["nsis", "msi"])
        );

        let mac: serde_json::Value = serde_json::from_str(include_str!("../tauri.macos.conf.json"))
            .expect("tauri.macos.conf.json");
        assert_eq!(mac["bundle"]["targets"], serde_json::json!(["app", "dmg"]));
        let mac_bundle = &mac["bundle"]["macOS"];
        assert_eq!(mac_bundle["minimumSystemVersion"], "13.0");
        assert_eq!(mac_bundle["signingIdentity"], "-");
        assert_eq!(mac_bundle["hardenedRuntime"], false);
    }

    #[test]
    fn exe_candidates_skip_missing_sources() {
        assert!(exe_candidates("mira-hook", None, None, None, None).is_empty());
        assert!(exe_candidates("mira-mcp", None, None, None, None).is_empty());
    }
}
