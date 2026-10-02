//! Per-platform app setup in one place (plan7 A.1).

/// `std::env::consts::OS`: "windows" | "macos" | "linux".
pub fn name() -> &'static str {
    std::env::consts::OS
}

/// macOS: `ActivationPolicy::Accessory` (no Dock icon, no menu bar, not in Cmd+Tab; windows
/// still become key). Other platforms: nothing. Tauri's default menu stays on (Cmd+C/V may
/// depend on it). Called from `lib.rs::run` between `Builder::build` and `App::run`: there
/// `App` still owns the runtime, so tauri 2.12's `App::set_activation_policy` sets it on the
/// event loop before launch (in `setup` the runtime is already taken and the app would start
/// as Regular first).
// TODO(macos-verify): Accessory: no Dock icon, workplace still gets keyboard focus, Cmd+Q/W/C/V
// (plan7 M.2, M.6)
pub fn apply_activation_policy(app: &mut tauri::App) {
    #[cfg(target_os = "macos")]
    {
        app.set_activation_policy(tauri::ActivationPolicy::Accessory);
        log::info!("macOS activation policy: Accessory");
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = app;
    }
}

/// SIGTERM/SIGINT/SIGHUP on Unix (review7 W5). Neither tauri 2.12 nor tao installs signal
/// handlers, so `kill <pid>`, a closed terminal (SIGHUP) or Ctrl+C under `tauri dev` used to end
/// the process without `RunEvent::Exit`: the socket file stayed behind and the agent groups only
/// got the kernel's SIGHUP from the pty. Now the first signal takes the quit path
/// (`AppHandle::exit(0)` → `ExitRequested`/`Exit` → `kill_all` + `cleanup_registered`), and a
/// second signal before that has finished aborts with exit code 1, so a hanging shutdown never
/// makes the app unkillable. Windows: unchanged (no such signals; console events are not
/// handled).
// TODO(macos-verify): `kill -TERM <pid>` removes the socket and stops the agent groups (README).
#[cfg(unix)]
pub mod signals {
    /// What a received signal does, by how many have arrived so far (1-based).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum SignalAction {
        /// First signal: a normal quit (same path as the island's Quit / `quit_app`).
        Exit,
        /// A further signal while the quit is still running: leave at once.
        Abort,
    }

    pub fn action_for(signals_seen: u32) -> SignalAction {
        if signals_seen <= 1 {
            SignalAction::Exit
        } else {
            SignalAction::Abort
        }
    }

    /// Spawns the listener on Tauri's tokio runtime. Called from `setup` after `AppState` is
    /// managed, so `kill_all` in the exit handler finds the manager.
    pub fn install(app: tauri::AppHandle) {
        use tokio::signal::unix::{signal, SignalKind};
        tauri::async_runtime::spawn(async move {
            let streams = (
                signal(SignalKind::terminate()),
                signal(SignalKind::interrupt()),
                signal(SignalKind::hangup()),
            );
            let (mut term, mut int, mut hup) = match streams {
                (Ok(t), Ok(i), Ok(h)) => (t, i, h),
                (t, i, h) => {
                    let e = [t.err(), i.err(), h.err()].into_iter().flatten().next();
                    log::warn!("signal handlers not installed: {e:?}");
                    return;
                }
            };
            let mut seen = 0u32;
            loop {
                let name = tokio::select! {
                    Some(()) = term.recv() => "SIGTERM",
                    Some(()) = int.recv() => "SIGINT",
                    Some(()) = hup.recv() => "SIGHUP",
                    else => return,
                };
                seen += 1;
                match action_for(seen) {
                    SignalAction::Exit => {
                        log::info!("{name}: exiting (stopping agents, removing the socket)");
                        app.exit(0);
                    }
                    SignalAction::Abort => {
                        log::warn!("{name} during shutdown: aborting");
                        crate::pipe::unix_socket::cleanup_registered();
                        std::process::exit(1);
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_the_compile_target_os() {
        #[cfg(windows)]
        assert_eq!(name(), "windows");
        #[cfg(target_os = "macos")]
        assert_eq!(name(), "macos");
        #[cfg(target_os = "linux")]
        assert_eq!(name(), "linux");
    }

    #[cfg(unix)]
    #[test]
    fn first_signal_exits_and_a_second_aborts() {
        use signals::{action_for, SignalAction};
        assert_eq!(action_for(1), SignalAction::Exit);
        assert_eq!(action_for(2), SignalAction::Abort);
        assert_eq!(action_for(3), SignalAction::Abort);
    }
}
