//! Locating the `claude` binary. Never runs a shell.

use std::ffi::OsString;
use std::path::PathBuf;

use crate::config::CLAUDE_PATH_ENV;

#[cfg(windows)]
const CLAUDE_FILE: &str = "claude.exe";
#[cfg(not(windows))]
const CLAUDE_FILE: &str = "claude";

/// (1) `MIRA_CLAUDE_PATH` if it points at a file; (2) Windows: `%USERPROFILE%\.local\bin\claude.exe`;
/// (3) first `claude.exe` (Windows) / `claude` (Unix) on `PATH`. `.cmd`/`.ps1` shims are never used
/// because they cannot be started without a shell.
pub fn find_claude() -> Option<PathBuf> {
    find_claude_in(
        std::env::var_os(CLAUDE_PATH_ENV),
        home_candidate(),
        std::env::var_os("PATH"),
    )
}

#[cfg(windows)]
fn home_candidate() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(|home| {
        PathBuf::from(home)
            .join(".local")
            .join("bin")
            .join("claude.exe")
    })
}

#[cfg(not(windows))]
fn home_candidate() -> Option<PathBuf> {
    None
}

/// Pure lookup used by [`find_claude`], with every input injected (testable).
pub fn find_claude_in(
    env_override: Option<OsString>,
    home_candidate: Option<PathBuf>,
    path_var: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(p) = env_override.filter(|p| !p.is_empty()).map(PathBuf::from) {
        if p.is_file() {
            return Some(p);
        }
        log::warn!("{CLAUDE_PATH_ENV}={} is not a file; ignoring", p.display());
    }
    if let Some(p) = home_candidate.filter(|p| p.is_file()) {
        return Some(p);
    }
    path_var.and_then(|path| search_path(&path))
}

fn search_path(path_var: &OsString) -> Option<PathBuf> {
    std::env::split_paths(path_var)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(CLAUDE_FILE))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("mira-claude-path-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn env_override_wins_when_file_exists() {
        let d = temp_dir("env");
        let f = d.join("my-claude");
        std::fs::write(&f, b"").unwrap();
        let other = temp_dir("path");
        std::fs::write(other.join(CLAUDE_FILE), b"").unwrap();
        let found = find_claude_in(
            Some(f.clone().into_os_string()),
            None,
            Some(other.clone().into_os_string()),
        );
        assert_eq!(found, Some(f));
        let _ = std::fs::remove_dir_all(d);
        let _ = std::fs::remove_dir_all(other);
    }

    #[test]
    fn missing_env_override_falls_through() {
        let d = temp_dir("fallthrough");
        let found = find_claude_in(
            Some(d.join("nope").into_os_string()),
            None,
            Some(OsString::new()),
        );
        assert_eq!(found, None);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn home_candidate_before_path() {
        let home = temp_dir("home");
        let h = home.join(CLAUDE_FILE);
        std::fs::write(&h, b"").unwrap();
        let p = temp_dir("path2");
        std::fs::write(p.join(CLAUDE_FILE), b"").unwrap();
        let found = find_claude_in(None, Some(h.clone()), Some(p.clone().into_os_string()));
        assert_eq!(found, Some(h));
        let _ = std::fs::remove_dir_all(home);
        let _ = std::fs::remove_dir_all(p);
    }

    #[test]
    fn path_scan_takes_first_existing_and_skips_shims() {
        let empty = temp_dir("empty");
        let shim = temp_dir("shim");
        std::fs::write(shim.join("claude.cmd"), b"").unwrap();
        std::fs::write(shim.join("claude.ps1"), b"").unwrap();
        let real = temp_dir("real");
        std::fs::write(real.join(CLAUDE_FILE), b"").unwrap();
        let later = temp_dir("later");
        std::fs::write(later.join(CLAUDE_FILE), b"").unwrap();
        let path = std::env::join_paths([&empty, &shim, &real, &later]).unwrap();
        assert_eq!(
            find_claude_in(None, None, Some(path)),
            Some(real.join(CLAUDE_FILE))
        );
        let only_shims = std::env::join_paths([&empty, &shim]).unwrap();
        assert_eq!(find_claude_in(None, None, Some(only_shims)), None);
        for d in [empty, shim, real, later] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn a_directory_named_claude_is_not_a_match() {
        let d = temp_dir("dir");
        std::fs::create_dir_all(d.join(CLAUDE_FILE)).unwrap();
        assert_eq!(
            find_claude_in(None, None, Some(d.clone().into_os_string())),
            None
        );
        let _ = std::fs::remove_dir_all(d);
    }
}
