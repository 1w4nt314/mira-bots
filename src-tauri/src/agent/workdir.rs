//! Default working folders for agents started without an explicit folder:
//! `<home>/mira-bots/agents/<prefix>-<nn>`, the prefix from the profile's roles
//! ([`super::roles::prefix_for`]).

use std::io;
use std::path::{Path, PathBuf};

/// `<home>/mira-bots/agents`.
pub fn agents_root(home: &Path) -> PathBuf {
    home.join("mira-bots").join("agents")
}

/// Path components split on both `/` and `\`, so a Windows-style path compares equal to the same
/// path built with [`Path::join`] on any host. Empty and `.` components are dropped.
fn components(p: &Path) -> Vec<String> {
    p.to_string_lossy()
        .split(['/', '\\'])
        .filter(|c| !c.is_empty() && *c != ".")
        .map(str::to_string)
        .collect()
}

/// Component-wise, ASCII-case-insensitive path equality (Windows paths are case-insensitive; on
/// other hosts a false "equal" only skips a number).
fn same_path(a: &Path, b: &Path) -> bool {
    let (a, b) = (components(a), components(b));
    a.len() == b.len() && a.iter().zip(&b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

/// First `root/<prefix>-<nn>` (from `01`) that is not the cwd of an agent in `taken` (all agents
/// the manager still knows, exited included). Folders that already exist on disk are reused on
/// purpose, so trust granted in an earlier run still applies.
// TODO(windows-verify): %USERPROFILE%\mira-bots\agents\bot-01 is created, the agent is named
// `bot-01`, and after a restart `bot-01` is reused when free (plan D.25).
pub fn next_agent_dir(root: &Path, prefix: &str, taken: &[PathBuf]) -> PathBuf {
    (1u32..)
        .map(|n| root.join(format!("{prefix}-{n:02}")))
        .find(|candidate| !taken.iter().any(|t| same_path(t, candidate)))
        .expect("an unbounded range always yields a free folder")
}

/// Creates the folder (and its parents) if it does not exist.
pub fn ensure_dir(p: &Path) -> io::Result<()> {
    std::fs::create_dir_all(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        agents_root(Path::new("/home/u"))
    }

    #[test]
    fn agents_root_is_under_home() {
        assert_eq!(
            agents_root(Path::new("/home/u")),
            Path::new("/home/u/mira-bots/agents")
        );
    }

    #[test]
    fn next_agent_dir_picks_the_first_free_number() {
        let r = root();
        assert_eq!(next_agent_dir(&r, "bot", &[]), r.join("bot-01"));
        assert_eq!(
            next_agent_dir(&r, "bot", &[r.join("bot-01")]),
            r.join("bot-02")
        );
        // A hole is filled first.
        assert_eq!(
            next_agent_dir(&r, "bot", &[r.join("bot-01"), r.join("bot-03")]),
            r.join("bot-02")
        );
        // Prefix; other prefixes' folders do not count.
        assert_eq!(
            next_agent_dir(&r, "coder", &[r.join("bot-01")]),
            r.join("coder-01")
        );
        assert_eq!(next_agent_dir(&r, "koord", &[]), r.join("koord-01"));
        // Agents elsewhere do not count.
        assert_eq!(
            next_agent_dir(&r, "bot", &[PathBuf::from("/w/bot-01")]),
            r.join("bot-01")
        );
    }

    #[test]
    fn windows_style_taken_paths_compare_by_component() {
        let r = PathBuf::from(r"C:\Users\Ana Bo\mira-bots\agents");
        let taken = [PathBuf::from(r"C:\Users\Ana Bo\mira-bots\agents\bot-01")];
        assert_eq!(
            components(&next_agent_dir(&r, "bot", &taken)),
            components(&r.join("bot-02"))
        );
        let taken = [PathBuf::from(r"c:\users\ana bo\mira-bots\agents\BOT-01\")];
        assert!(components(&next_agent_dir(&r, "bot", &taken))
            .last()
            .is_some_and(|c| c == "bot-02"));
    }

    #[test]
    fn ensure_dir_creates_nested_folders() {
        let base = std::env::temp_dir().join(format!("mira-workdir-{}", uuid::Uuid::new_v4()));
        let dir = next_agent_dir(&agents_root(&base), "reviewer", &[]);
        ensure_dir(&dir).unwrap();
        ensure_dir(&dir).unwrap();
        assert!(dir.is_dir());
        assert!(dir.ends_with("mira-bots/agents/reviewer-01"));
        std::fs::remove_dir_all(&base).unwrap();
    }
}
