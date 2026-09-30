//! The island window: a borderless, transparent, always-on-top strip that sits top-centred on
//! the primary monitor and is resized by the frontend (collapsed 240x8 <-> expanded bar).
//!
//! Hover is handled with JS events (`mouseenter`/`mouseleave`) in a permanent 8 px window, so
//! there is no Win32 code, no cursor polling and no click-through state in step 1.

use std::sync::Mutex;

use tauri::{PhysicalPosition, PhysicalSize, WebviewWindow};

/// Window label in `tauri.conf.json` and in the capability file.
pub const LABEL: &str = "island";

/// Collapsed size in logical px (width, height).
pub const COLLAPSED: (u32, u32) = (240, 8);
/// Upper bound for the expanded size in logical px (width, height).
pub const MAX_SIZE: (u32, u32) = (960, 420);

/// Last size (logical px) given to [`place`]; used to re-place after a scale-factor change.
pub struct IslandState {
    pub last: Mutex<(u32, u32)>,
}

impl Default for IslandState {
    fn default() -> Self {
        Self {
            last: Mutex::new(COLLAPSED),
        }
    }
}

/// Clamps a requested logical size to `1..=MAX_SIZE`.
pub fn clamp_size(w: u32, h: u32) -> (u32, u32) {
    (w.clamp(1, MAX_SIZE.0), h.clamp(1, MAX_SIZE.1))
}

/// Physical size and top-centred position for a logical size inside a monitor work area.
///
/// `wa_pos`/`wa_size` are the monitor's work area (the part not covered by the taskbar), so a
/// taskbar at the top pushes the island down instead of hiding it.
pub fn compute_placement(
    wa_pos: (i32, i32),
    wa_size: (u32, u32),
    scale: f64,
    logical: (u32, u32),
) -> (PhysicalSize<u32>, PhysicalPosition<i32>) {
    let (w, h) = clamp_size(logical.0, logical.1);
    let pw = ((f64::from(w) * scale).round() as u32).max(1);
    let ph = ((f64::from(h) * scale).round() as u32).max(1);
    let x = wa_pos.0 + (wa_size.0 as i32 - pw as i32) / 2;
    let y = wa_pos.1;
    (PhysicalSize::new(pw, ph), PhysicalPosition::new(x, y))
}

/// Sizes the window to `logical_w` x `logical_h` (clamped to [`MAX_SIZE`]) and centres it at the
/// top of the primary monitor's work area.
// TODO(windows-verify): work_area() + scale_factor() give a correct top-centred placement at
// 125 % / 150 % scaling and with the taskbar at the top (plan D.3).
// TODO(windows-verify): tauri.conf.json sets focus:false + focusable:false; clicking Tillad/Afvis
// must not steal focus from the terminal. If it does, add WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW via
// the `windows` crate (Coucou pattern) (plan D.1).
// TODO(windows-verify): the transparent window must not flash white at startup; if it does, try
// additionalBrowserArgs / WS_EX_NOREDIRECTIONBITMAP (plan D.2).
pub fn place(window: &WebviewWindow, logical_w: u32, logical_h: u32) -> tauri::Result<()> {
    let (w, h) = clamp_size(logical_w, logical_h);
    let monitor = match window.primary_monitor()? {
        Some(m) => Some(m),
        None => window.current_monitor()?,
    };
    let Some(monitor) = monitor else {
        // No monitor info: just size the window.
        return window.set_size(tauri::LogicalSize::new(f64::from(w), f64::from(h)));
    };
    let wa = monitor.work_area();
    let (size, pos) = compute_placement(
        (wa.position.x, wa.position.y),
        (wa.size.width, wa.size.height),
        monitor.scale_factor(),
        (w, h),
    );
    window.set_size(size)?;
    window.set_position(pos)?;
    // Cheap, and keeps the island above other always-on-top windows that appeared later.
    window.set_always_on_top(true)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapsed_is_centred_at_100_percent() {
        let (size, pos) = compute_placement((0, 0), (1920, 1040), 1.0, COLLAPSED);
        assert_eq!((size.width, size.height), (240, 8));
        assert_eq!((pos.x, pos.y), (840, 0));
    }

    #[test]
    fn scale_factor_scales_size_before_centring() {
        let (size, pos) = compute_placement((0, 0), (2560, 1400), 1.5, COLLAPSED);
        assert_eq!((size.width, size.height), (360, 12));
        assert_eq!((pos.x, pos.y), (1100, 0));
        let (size, _) = compute_placement((0, 0), (1920, 1080), 1.25, COLLAPSED);
        assert_eq!((size.width, size.height), (300, 10));
    }

    #[test]
    fn work_area_offset_is_respected() {
        // Taskbar on top (48 px) and a second monitor to the left.
        let (_, pos) = compute_placement((-1920, 48), (1920, 1032), 1.0, (240, 8));
        assert_eq!((pos.x, pos.y), (-1920 + 840, 48));
    }

    #[test]
    fn size_is_clamped() {
        assert_eq!(clamp_size(5000, 5000), MAX_SIZE);
        assert_eq!(clamp_size(0, 0), (1, 1));
        let (size, _) = compute_placement((0, 0), (1920, 1080), 1.0, (5000, 5000));
        assert_eq!((size.width, size.height), MAX_SIZE);
    }

    #[test]
    fn default_state_starts_collapsed() {
        assert_eq!(*IslandState::default().last.lock().unwrap(), COLLAPSED);
    }
}
