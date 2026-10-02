//! Locating the `claude` binary. Never runs a shell itself (the login-shell PATH comes from
//! `login_env`).

use std::ffi::OsString;
use std::path::PathBuf;

use super::login_env;
use crate::config::CLAUDE_PATH_ENV;

#[cfg(windows)]
const CLAUDE_FILE: &str = "claude.exe";
#[cfg(not(windows))]
const CLAUDE_FILE: &str = "claude";

/// (1) `MIRA_CLAUDE_PATH` if it points at a file; (2) the first existing [`home_candidates`]
/// entry; (3) first `claude.exe` (Windows) / `claude` (Unix) on `login_env::path()` (unix: the
/// login shell's PATH merged with the process PATH). `.cmd`/`.ps1` shims are never used because
/// they cannot be started without a shell.
pub fn find_claude() -> Option<PathBuf> {
    find_claude_in(
        std::env::var_os(CLAUDE_PATH_ENV),
        home_candidates(home_dir()),
        login_env::path(),
    )
}

#[cfg(windows)]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

#[cfg(not(windows))]
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Well-known install locations, in lookup order. Windows: `%USERPROFILE%\.local\bin\claude.exe`.
#[cfg(windows)]
pub fn home_candidates(home: Option<PathBuf>) -> Vec<PathBuf> {
    home.map(|h| h.join(".local").join("bin").join(CLAUDE_FILE))
        .into_iter()
        .collect()
}

/// Well-known install locations, in lookup order (plan7 A.3): native installer
/// (`~/.local/bin`), Homebrew (Apple Silicon, then Intel/Linux prefix), npm with a user prefix,
/// Volta, fnm (macOS, then Linux data dir) and every nvm version, newest first. npm is never
/// asked for its global prefix (one process per lookup); other prefixes come from the
/// login-shell PATH.
#[cfg(not(windows))]
pub fn home_candidates(home: Option<PathBuf>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(h) = &home {
        out.push(h.join(".local").join("bin").join(CLAUDE_FILE));
    }
    out.push(PathBuf::from("/opt/homebrew/bin").join(CLAUDE_FILE));
    out.push(PathBuf::from("/usr/local/bin").join(CLAUDE_FILE));
    if let Some(h) = &home {
        out.push(h.join(".npm-global").join("bin").join(CLAUDE_FILE));
        out.push(h.join(".volta").join("bin").join(CLAUDE_FILE));
        let fnm_default = ["aliases", "default", "bin", CLAUDE_FILE];
        out.push(fnm_default.iter().fold(
            h.join("Library").join("Application Support").join("fnm"),
            |p, s| p.join(s),
        ));
        out.push(
            fnm_default
                .iter()
                .fold(h.join(".local").join("share").join("fnm"), |p, s| p.join(s)),
        );
        out.extend(
            nvm_versions(&h.join(".nvm").join("versions").join("node"))
                .into_iter()
                .map(|v| v.join("bin").join(CLAUDE_FILE)),
        );
    }
    out
}

/// The version directories under `~/.nvm/versions/node`, newest first: sorted descending by
/// `(major, minor, patch)` parsed from `v…`; names that do not parse come last, descending.
#[cfg(not(windows))]
type NodeVersion = (u64, u64, u64);

#[cfg(not(windows))]
fn nvm_versions(dir: &std::path::Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut versions: Vec<(Option<NodeVersion>, String, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            (parse_node_version(&name), name, e.path())
        })
        .collect();
    versions.sort_by(|a, b| (&b.0, &b.1).cmp(&(&a.0, &a.1)));
    versions.into_iter().map(|(_, _, p)| p).collect()
}

/// `"v22.3.1"` → `(22, 3, 1)`; missing minor/patch count as 0.
#[cfg(not(windows))]
fn parse_node_version(name: &str) -> Option<NodeVersion> {
    let mut parts = name.strip_prefix('v')?.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().map_or(Some(0), |p| p.parse().ok())?;
    let patch = parts.next().map_or(Some(0), |p| p.parse().ok())?;
    Some((major, minor, patch))
}

/// Pure lookup used by [`find_claude`], with every input injected (testable).
pub fn find_claude_in(
    env_override: Option<OsString>,
    home_candidates: Vec<PathBuf>,
    path_var: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(p) = env_override.filter(|p| !p.is_empty()).map(PathBuf::from) {
        if p.is_file() {
            return Some(p);
        }
        log::warn!("{CLAUDE_PATH_ENV}={} is not a file; ignoring", p.display());
    }
    if let Some(p) = home_candidates.into_iter().find(|p| p.is_file()) {
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
            vec![],
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
            vec![],
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
        let found = find_claude_in(None, vec![h.clone()], Some(p.clone().into_os_string()));
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
            find_claude_in(None, vec![], Some(path)),
            Some(real.join(CLAUDE_FILE))
        );
        let only_shims = std::env::join_paths([&empty, &shim]).unwrap();
        assert_eq!(find_claude_in(None, vec![], Some(only_shims)), None);
        for d in [empty, shim, real, later] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn a_directory_named_claude_is_not_a_match() {
        let d = temp_dir("dir");
        std::fs::create_dir_all(d.join(CLAUDE_FILE)).unwrap();
        assert_eq!(
            find_claude_in(None, vec![], Some(d.clone().into_os_string())),
            None
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn home_candidates_without_home_have_no_home_paths() {
        let c = home_candidates(None);
        #[cfg(windows)]
        assert!(c.is_empty());
        #[cfg(not(windows))]
        assert_eq!(
            c,
            vec![
                PathBuf::from("/opt/homebrew/bin/claude"),
                PathBuf::from("/usr/local/bin/claude")
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_home_candidate_is_the_native_installer() {
        let home = PathBuf::from(r"C:\Users\x");
        assert_eq!(
            home_candidates(Some(home.clone())),
            vec![home.join(".local").join("bin").join("claude.exe")]
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_home_candidates_in_plan_order() {
        let home = temp_dir("order");
        let c = home_candidates(Some(home.clone()));
        let h = |rel: &str| home.join(rel);
        assert_eq!(c[0], h(".local/bin/claude"));
        assert_eq!(c[1], PathBuf::from("/opt/homebrew/bin/claude"));
        assert_eq!(c[2], PathBuf::from("/usr/local/bin/claude"));
        assert_eq!(c[3], h(".npm-global/bin/claude"));
        assert_eq!(c[4], h(".volta/bin/claude"));
        assert_eq!(
            c[5],
            h("Library/Application Support/fnm/aliases/default/bin/claude")
        );
        assert_eq!(c[6], h(".local/share/fnm/aliases/default/bin/claude"));
        // No ~/.nvm in the fake home: nothing more.
        assert_eq!(c.len(), 7);
        let _ = std::fs::remove_dir_all(home);
    }

    #[cfg(unix)]
    #[test]
    fn nvm_versions_newest_first() {
        let home = temp_dir("nvm");
        let node = home.join(".nvm/versions/node");
        for v in ["v20.10.0", "v22.3.1", "v9.0.0", "system"] {
            std::fs::create_dir_all(node.join(v).join("bin")).unwrap();
        }
        // A stray file is not a version directory.
        std::fs::write(node.join("v99.0.0"), b"").unwrap();
        let c = home_candidates(Some(home.clone()));
        assert_eq!(
            c[7..].to_vec(),
            vec![
                node.join("v22.3.1/bin/claude"),
                node.join("v20.10.0/bin/claude"),
                node.join("v9.0.0/bin/claude"),
                node.join("system/bin/claude"),
            ]
        );
        assert_eq!(parse_node_version("v22.3.1"), Some((22, 3, 1)));
        assert_eq!(parse_node_version("v18"), Some((18, 0, 0)));
        assert_eq!(parse_node_version("22.3.1"), None);
        assert_eq!(parse_node_version("vx.1"), None);
        let _ = std::fs::remove_dir_all(home);
    }

    #[cfg(unix)]
    #[test]
    fn first_existing_candidate_wins() {
        let home = temp_dir("first");
        let npm = home.join(".npm-global/bin/claude");
        std::fs::create_dir_all(npm.parent().unwrap()).unwrap();
        std::fs::write(&npm, b"").unwrap();
        let older = home.join(".nvm/versions/node/v20.0.0/bin/claude");
        std::fs::create_dir_all(older.parent().unwrap()).unwrap();
        std::fs::write(&older, b"").unwrap();
        // Only the fake-home entries: /opt/homebrew and /usr/local may exist on the machine.
        let in_home: Vec<PathBuf> = home_candidates(Some(home.clone()))
            .into_iter()
            .filter(|p| p.starts_with(&home))
            .collect();
        let on_path = temp_dir("first-path");
        std::fs::write(on_path.join(CLAUDE_FILE), b"").unwrap();
        assert_eq!(
            find_claude_in(None, in_home, Some(on_path.clone().into_os_string())),
            Some(npm)
        );
        let _ = std::fs::remove_dir_all(home);
        let _ = std::fs::remove_dir_all(on_path);
    }
}
