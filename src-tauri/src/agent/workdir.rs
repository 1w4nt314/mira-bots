//! Agent folders and names (plan4b A.1): work agents run in `<projects root>/<project>`, staff
//! agents in the projects root itself (default `<home>/mira-bots/projects`). The agent's name
//! `<prefix>-<nn>` (prefix from the profile's roles, [`super::roles::prefix_for`]) is no longer
//! tied to a folder, because several agents share a project folder.

use std::io;
use std::path::{Path, PathBuf};

use crate::config::PROJECTS_DIR_NAME;

/// `<home>/mira-bots/projects`: the default projects root.
pub fn projects_root(home: &Path) -> PathBuf {
    home.join("mira-bots").join(PROJECTS_DIR_NAME)
}

/// `<home>/mira-bots/agents`: the step 1–5 agents root; only read to migrate its profiles.
pub fn legacy_agents_root(home: &Path) -> PathBuf {
    home.join("mira-bots").join("agents")
}

/// First `<prefix>-<nn>` (from `01`) that is not the name of an agent in `taken` (all agents the
/// manager still knows, exited included), compared ASCII-case-insensitively.
// TODO(windows-verify): two agents in the same project are named `coder-01` and `coder-02`
// independently of their folder (plan4b D.79).
pub fn next_agent_name(prefix: &str, taken: &[String]) -> String {
    (1u32..)
        .map(|n| format!("{prefix}-{n:02}"))
        .find(|candidate| !taken.iter().any(|t| t.eq_ignore_ascii_case(candidate)))
        .expect("an unbounded range always yields a free name")
}

/// Creates the folder (and its parents) if it does not exist.
pub fn ensure_dir(p: &Path) -> io::Result<()> {
    std::fs::create_dir_all(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn roots_are_under_home() {
        assert_eq!(
            projects_root(Path::new("/home/u")),
            Path::new("/home/u/mira-bots/projects")
        );
        assert_eq!(
            legacy_agents_root(Path::new("/home/u")),
            Path::new("/home/u/mira-bots/agents")
        );
    }

    #[test]
    fn next_agent_name_picks_the_first_free_number() {
        assert_eq!(next_agent_name("bot", &[]), "bot-01");
        assert_eq!(next_agent_name("bot", &names(&["bot-01"])), "bot-02");
        // A hole is filled first.
        assert_eq!(
            next_agent_name("bot", &names(&["bot-01", "bot-03"])),
            "bot-02"
        );
        // Other prefixes do not count.
        assert_eq!(next_agent_name("coder", &names(&["bot-01"])), "coder-01");
        assert_eq!(next_agent_name("koord", &[]), "koord-01");
        // Case-insensitive.
        assert_eq!(next_agent_name("bot", &names(&["BOT-01"])), "bot-02");
    }

    #[test]
    fn ensure_dir_creates_nested_folders() {
        let base = std::env::temp_dir().join(format!("mira-workdir-{}", uuid::Uuid::new_v4()));
        let dir = projects_root(&base).join("demo");
        ensure_dir(&dir).unwrap();
        ensure_dir(&dir).unwrap();
        assert!(dir.is_dir());
        assert!(dir.ends_with("mira-bots/projects/demo"));
        std::fs::remove_dir_all(&base).unwrap();
    }
}
