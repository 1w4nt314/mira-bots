//! The `workplace` window: a normal, decorated window created on demand from Rust (never in
//! `tauri.conf.json`). Closing it destroys it; the next `open_workplace` builds a new one.

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

/// Window label; must match `capabilities/default.json` and the React router.
pub const LABEL: &str = "workplace";
/// Initial inner size (logical px).
pub const SIZE: (f64, f64) = (1100.0, 720.0);
/// Minimum inner size (logical px).
pub const MIN_SIZE: (f64, f64) = (820.0, 540.0);

/// Focuses the workplace window (unminimize + show + focus) or creates it. Returns whether it was
/// created. Must only be called from an async command or a separate thread: building a window in
/// a synchronous command or event handler deadlocks on Windows (research2 §5).
// TODO(windows-verify): creating the window from the async command does not deadlock; closing it
// does not end the app; reopening works; invoke/listen work in it (capability on the label)
// (plan D.19). Focus is taken even though the island is not focusable (plan D.24).
pub fn open_or_focus(app: &AppHandle) -> tauri::Result<bool> {
    if let Some(window) = app.get_webview_window(LABEL) {
        if window.is_minimized()? {
            window.unminimize()?;
        }
        window.show()?;
        window.set_focus()?;
        return Ok(false);
    }
    WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("index.html".into()))
        .title("mira-bots")
        .inner_size(SIZE.0, SIZE.1)
        .min_inner_size(MIN_SIZE.0, MIN_SIZE.1)
        .resizable(true)
        .decorations(true)
        .center()
        .build()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_and_sizes_match_the_plan() {
        assert_eq!(LABEL, "workplace");
        assert_eq!(SIZE, (1100.0, 720.0));
        assert_eq!(MIN_SIZE, (820.0, 540.0));
        assert_ne!(LABEL, crate::island::LABEL);
    }

    #[test]
    fn capability_covers_both_windows() {
        let cap: serde_json::Value =
            serde_json::from_str(include_str!("../capabilities/default.json")).unwrap();
        let windows: Vec<&str> = cap["windows"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(windows.contains(&LABEL));
        assert!(windows.contains(&crate::island::LABEL));
    }
}
