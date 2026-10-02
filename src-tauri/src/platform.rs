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
}
