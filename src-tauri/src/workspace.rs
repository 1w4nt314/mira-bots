//! The workspace file `<projects root>/mira-bots.workspace.json` (plan4b A.4, C4b.2): optional
//! rules that override the defaults from `config.rs`. Read on demand (spawn, assignment, tool
//! call, Diagnostik) through [`WorkspaceReader`], which re-parses only when the file's
//! `(mtime, len)` changed; no file watcher (research4b §4).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde::Deserialize;

use crate::config::{MAX_REVIEW_ROUNDS_MAX, WORKSPACE_FILE};
use crate::tickets::model::{GitMode, WorkspaceRules};
use crate::tickets::playbook::{builtin_playbooks, validate_playbooks, Playbook};

/// Number of work seats (= `WORK_SEATS` in seats.ts); `maxWorkAgents` is clamped to it.
pub const MAX_WORK_SEATS: usize = 5;
/// Number of staff seats (= `STAFF_SEATS`); `maxStaffAgents` is clamped to it.
pub const MAX_STAFF_SEATS: usize = 3;
/// Upper bound of `userInputGraceMs`.
pub const USER_INPUT_GRACE_MAX_MS: u64 = 60_000;
/// Most chars of `gitBase`.
pub const GIT_BASE_MAX_CHARS: usize = 100;
/// Note for `"git": "branch"` until branch mode exists (plan6b punkt 11, 6b-2): off is used.
pub const GIT_BRANCH_LATER_NOTE: &str = "git: branch kommer i et senere trin; off bruges";
/// Note for `"cleanupWorktreesOnDone": true` until it is enforced (plan6b punkt 13, 6b-2).
pub const CLEANUP_LATER_NOTE: &str =
    "cleanupWorktreesOnDone håndhæves først i et senere trin (worktrees fjernes ikke automatisk)";

/// The file as written by the user; every field is optional. Unknown keys are ignored, a wrong
/// type rejects the whole file (defaults + warning).
#[derive(Deserialize, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct WorkspaceFile {
    pub max_work_agents: Option<usize>,
    pub max_staff_agents: Option<usize>,
    pub max_review_rounds: Option<u32>,
    pub review_by_default: Option<bool>,
    pub auto_review_on_stop: Option<bool>,
    pub user_input_grace_ms: Option<u64>,
    pub agents_may_create_projects: Option<bool>,
    pub max_agents_per_project: Option<usize>,
    /// Read, not enforced yet (profile defaults stay in effect).
    pub defaults: Option<serde_json::Value>,
    /// Belongs in the app settings; only noted.
    pub projects_root: Option<serde_json::Value>,
    // ---- step 6b ----
    /// Validated by hand in [`effective`]: a bad playbook gives a note, never a rejected file.
    pub playbooks: Option<serde_json::Value>,
    /// `"off"|"branch"|"worktree"`; anything else gives a note and `off`.
    pub git: Option<String>,
    pub git_base: Option<String>,
    pub checks_gate: Option<bool>,
    pub auto_spawn_for_playbook: Option<bool>,
    pub fresh_session_per_ticket: Option<bool>,
    pub cleanup_worktrees_on_done: Option<bool>,
}

/// The non-`Copy` part of the workspace settings (step 6b): the playbooks (built-in ones merged
/// with the file's, the file wins per name) and the configured git base branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceConfig {
    pub playbooks: BTreeMap<String, Playbook>,
    pub git_base: Option<String>,
}

impl Default for WorkspaceConfig {
    /// The built-in playbooks, no git base.
    fn default() -> Self {
        WorkspaceConfig {
            playbooks: builtin_playbooks(),
            git_base: None,
        }
    }
}

impl WorkspaceConfig {
    /// The playbook names, sorted (`AppInfo.playbookKinds`).
    pub fn playbook_kinds(&self) -> Vec<String> {
        self.playbooks.keys().cloned().collect()
    }
}

/// `gitBase`: trimmed, 1–[`GIT_BASE_MAX_CHARS`] chars, no whitespace and no leading `-` (it ends
/// up as a git argument); otherwise `None`.
pub(crate) fn valid_git_base(v: &str) -> Option<String> {
    let v = v.trim();
    let ok = !v.is_empty()
        && v.chars().count() <= GIT_BASE_MAX_CHARS
        && !v.starts_with('-')
        && !v.chars().any(char::is_whitespace);
    ok.then(|| v.to_string())
}

/// Clamps `v` into `lo..=hi`, noting a change as "`<key> v er sat ned/op til c (<why>)`".
fn clamp_noted<T: Ord + Copy + std::fmt::Display>(
    key: &str,
    v: T,
    lo: T,
    hi: T,
    why: &str,
    notes: &mut Vec<String>,
) -> T {
    let c = v.clamp(lo, hi);
    if c != v {
        let dir = if c < v { "ned" } else { "op" };
        notes.push(format!("{key} {v} er sat {dir} til {c} ({why})"));
    }
    c
}

/// The effective rules and config for `file` plus notes for the user (clamped values, unknown
/// `git` values, dropped playbooks, fields that are read but not enforced yet, `projectsRoot` in
/// the wrong file).
pub fn effective(file: &WorkspaceFile) -> (WorkspaceRules, WorkspaceConfig, Vec<String>) {
    let mut r = WorkspaceRules::defaults();
    let mut c = WorkspaceConfig::default();
    let mut notes = Vec::new();
    if let Some(v) = file.max_work_agents {
        r.max_work_agents = clamp_noted(
            "maxWorkAgents",
            v,
            1,
            MAX_WORK_SEATS,
            "antal pladser",
            &mut notes,
        );
    }
    if let Some(v) = file.max_staff_agents {
        r.max_staff_agents = clamp_noted(
            "maxStaffAgents",
            v,
            1,
            MAX_STAFF_SEATS,
            "antal stabspladser",
            &mut notes,
        );
    }
    if let Some(v) = file.max_review_rounds {
        r.max_review_rounds = clamp_noted(
            "maxReviewRounds",
            v,
            1,
            MAX_REVIEW_ROUNDS_MAX,
            "1–10",
            &mut notes,
        );
    }
    if let Some(v) = file.review_by_default {
        r.review_by_default = v;
    }
    if let Some(v) = file.auto_review_on_stop {
        r.auto_review_on_stop = v;
    }
    if let Some(v) = file.user_input_grace_ms {
        r.user_input_grace_ms = clamp_noted(
            "userInputGraceMs",
            v,
            0,
            USER_INPUT_GRACE_MAX_MS,
            "højst 60000",
            &mut notes,
        );
    }
    if let Some(v) = file.agents_may_create_projects {
        r.agents_may_create_projects = v;
    }
    if let Some(v) = file.max_agents_per_project {
        r.max_agents_per_project = v;
    }
    let empty_defaults = |v: &serde_json::Value| v.as_object().is_some_and(|o| o.is_empty());
    if file.defaults.as_ref().is_some_and(|v| !empty_defaults(v)) {
        notes.push(format!(
            "defaults i {WORKSPACE_FILE} læses først i et senere trin (nu profilens værdier)"
        ));
    }
    if file.projects_root.is_some() {
        notes.push("projectsRoot hører til i appens indstillinger og ignoreres".to_string());
    }
    // ---- step 6b ----
    if let Some(v) = &file.git {
        match GitMode::parse(v) {
            // plan6b punkt 11 is deferred (6b-2): branch mode behaves as off, with a note.
            Some(GitMode::Branch) => notes.push(GIT_BRANCH_LATER_NOTE.to_string()),
            Some(m) => r.git = m,
            None => notes.push(format!(
                "git «{v}» er ukendt (off, branch eller worktree); off bruges"
            )),
        }
    }
    if let Some(v) = &file.git_base {
        c.git_base = valid_git_base(v);
        if c.git_base.is_none() {
            notes.push(format!(
                "gitBase «{v}» ignoreres (1–{GIT_BASE_MAX_CHARS} tegn uden mellemrum)"
            ));
        }
    }
    if let Some(v) = file.checks_gate {
        r.checks_gate = v;
    }
    if let Some(v) = file.auto_spawn_for_playbook {
        r.auto_spawn_for_playbook = v;
    }
    if let Some(v) = file.fresh_session_per_ticket {
        r.fresh_session_per_ticket = v;
    }
    if let Some(v) = file.cleanup_worktrees_on_done {
        r.cleanup_worktrees_on_done = v;
        // plan6b punkt 13 is deferred (6b-2): read, not enforced yet.
        if v {
            notes.push(CLEANUP_LATER_NOTE.to_string());
        }
    }
    if let Some(v) = &file.playbooks {
        // Built-in names the file does not mention stay.
        c.playbooks.extend(validate_playbooks(v, &mut notes));
    }
    (r, c, notes)
}

/// What the reader found: the effective rules, notes, a warning when the file could not be
/// read/parsed (the defaults apply then) and whether the file exists.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceSnapshot {
    pub rules: WorkspaceRules,
    /// Playbooks and git base (step 6b).
    pub config: WorkspaceConfig,
    pub notes: Vec<String>,
    pub warning: Option<String>,
    pub file_exists: bool,
}

impl WorkspaceSnapshot {
    fn defaults(file_exists: bool, warning: Option<String>) -> Self {
        WorkspaceSnapshot {
            rules: WorkspaceRules::defaults(),
            config: WorkspaceConfig::default(),
            notes: Vec::new(),
            warning,
            file_exists,
        }
    }
}

fn read_warning(e: impl std::fmt::Display) -> String {
    format!("{WORKSPACE_FILE} kunne ikke læses: {e}; standardværdierne bruges")
}

/// Parses the file's text into a snapshot (warning + defaults on a parse error).
fn parse(text: &str) -> WorkspaceSnapshot {
    match serde_json::from_str::<WorkspaceFile>(text) {
        Ok(file) => {
            let (rules, config, notes) = effective(&file);
            WorkspaceSnapshot {
                rules,
                config,
                notes,
                warning: None,
                file_exists: true,
            }
        }
        Err(e) => WorkspaceSnapshot::defaults(true, Some(read_warning(e))),
    }
}

type CacheEntry = (Option<SystemTime>, u64, WorkspaceSnapshot);

/// Reads the workspace file on demand with an `(mtime, len)` cache. Only locks its own mutex
/// (never calls into the ticket service or the agent manager), so it is safe under other locks.
pub struct WorkspaceReader {
    path: PathBuf,
    cache: Mutex<Option<CacheEntry>>,
}

impl WorkspaceReader {
    pub fn new(path: PathBuf) -> Self {
        WorkspaceReader {
            path,
            cache: Mutex::new(None),
        }
    }

    /// `<projects root>/mira-bots.workspace.json`.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The projects root (the file's folder).
    pub fn root(&self) -> &Path {
        self.path.parent().unwrap_or_else(|| Path::new(""))
    }

    /// The current snapshot: cached while the file's mtime and length are unchanged, otherwise
    /// read and parsed again. A missing file gives the defaults without a warning.
    // TODO(windows-verify): a save from Notepad/VS Code (new mtime, maybe the same length) is
    // read at the next spawn/assignment; invalid JSON shows the warning (plan4b D.83).
    pub fn snapshot(&self) -> WorkspaceSnapshot {
        crate::tickets::assert_not_under_inbox_lock("the workspace file");
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let meta = match std::fs::metadata(&self.path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                *cache = None;
                return WorkspaceSnapshot::defaults(false, None);
            }
            Err(e) => {
                *cache = None;
                let w = read_warning(e);
                log::warn!("{w}");
                return WorkspaceSnapshot::defaults(true, Some(w));
            }
        };
        let key = (meta.modified().ok(), meta.len());
        if let Some((mtime, len, snap)) = cache.as_ref() {
            if (*mtime, *len) == key {
                return snap.clone();
            }
        }
        let snap = match std::fs::read_to_string(&self.path) {
            Ok(text) => parse(&text),
            Err(e) => WorkspaceSnapshot::defaults(true, Some(read_warning(e))),
        };
        if let Some(w) = &snap.warning {
            log::warn!("{w}");
        }
        for n in &snap.notes {
            log::info!("workspace file: {n}");
        }
        *cache = Some((key.0, key.1, snap.clone()));
        snap
    }

    /// The effective rules of [`Self::snapshot`].
    pub fn rules(&self) -> WorkspaceRules {
        self.snapshot().rules
    }

    /// The playbooks and git base of [`Self::snapshot`] (step 6b).
    pub fn config(&self) -> WorkspaceConfig {
        self.snapshot().config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("mira-ws-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        fn reader(&self) -> WorkspaceReader {
            WorkspaceReader::new(self.0.join(WORKSPACE_FILE))
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn file(json: &str) -> WorkspaceFile {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn empty_file_gives_the_defaults() {
        let (r, c, notes) = effective(&file("{}"));
        assert_eq!(r, WorkspaceRules::defaults());
        assert_eq!(c, WorkspaceConfig::default());
        assert_eq!(c.playbook_kinds(), ["bug", "feature"]);
        assert_eq!(c.git_base, None);
        assert!(notes.is_empty());
        assert_eq!(file("{}"), WorkspaceFile::default());
    }

    #[test]
    fn partial_file_overrides_only_its_fields() {
        let (r, _, notes) = effective(&file(r#"{"maxWorkAgents": 2, "autoReviewOnStop": true}"#));
        assert_eq!(
            r,
            WorkspaceRules {
                max_work_agents: 2,
                auto_review_on_stop: true,
                ..WorkspaceRules::defaults()
            }
        );
        assert!(notes.is_empty());
    }

    #[test]
    fn all_fields() {
        let (r, c, notes) = effective(&file(
            r#"{"maxWorkAgents": 3, "maxStaffAgents": 2, "maxReviewRounds": 4,
                "reviewByDefault": false, "autoReviewOnStop": true, "userInputGraceMs": 1000,
                "agentsMayCreateProjects": true, "maxAgentsPerProject": 2, "defaults": {},
                "somethingNew": [1, 2], "playbooks": {}, "git": "worktree", "gitBase": "main",
                "checksGate": false, "autoSpawnForPlaybook": true,
                "freshSessionPerTicket": false, "cleanupWorktreesOnDone": true}"#,
        ));
        assert_eq!(
            r,
            WorkspaceRules {
                max_work_agents: 3,
                max_staff_agents: 2,
                max_review_rounds: 4,
                review_by_default: false,
                auto_review_on_stop: true,
                user_input_grace_ms: 1000,
                agents_may_create_projects: true,
                max_agents_per_project: 2,
                git: GitMode::Worktree,
                checks_gate: false,
                auto_spawn_for_playbook: true,
                fresh_session_per_ticket: false,
                cleanup_worktrees_on_done: true,
                ..WorkspaceRules::defaults()
            }
        );
        assert_eq!(
            c,
            WorkspaceConfig {
                git_base: Some("main".into()),
                ..WorkspaceConfig::default()
            }
        );
        assert_eq!(notes, [CLEANUP_LATER_NOTE], "{notes:?}");
    }

    #[test]
    fn values_are_clamped_with_a_note() {
        let (r, _, notes) = effective(&file(r#"{"maxWorkAgents": 9}"#));
        assert_eq!(r.max_work_agents, 5);
        assert_eq!(notes, ["maxWorkAgents 9 er sat ned til 5 (antal pladser)"]);
        let (r, _, notes) = effective(&file(r#"{"maxWorkAgents": 0}"#));
        assert_eq!(r.max_work_agents, 1);
        assert_eq!(notes, ["maxWorkAgents 0 er sat op til 1 (antal pladser)"]);
        let (r, _, notes) = effective(&file(r#"{"maxStaffAgents": 4}"#));
        assert_eq!(r.max_staff_agents, 3);
        assert_eq!(notes.len(), 1);
        let (r, _, notes) = effective(&file(r#"{"userInputGraceMs": 90000}"#));
        assert_eq!(r.user_input_grace_ms, 60_000);
        assert_eq!(notes.len(), 1);
        let (r, _, _) = effective(&file(
            r#"{"userInputGraceMs": 0, "maxAgentsPerProject": 0}"#,
        ));
        assert_eq!((r.user_input_grace_ms, r.max_agents_per_project), (0, 0));
    }

    #[test]
    fn read_but_not_enforced_fields_give_notes() {
        // Step 6b: maxReviewRounds is enforced now (no note within 1–10).
        let (r, _, notes) = effective(&file(r#"{"maxReviewRounds": 5}"#));
        assert_eq!(r.max_review_rounds, 5);
        assert!(notes.is_empty(), "{notes:?}");
        let (_, _, notes) = effective(&file(r#"{"defaults": {"model": "opus"}}"#));
        assert_eq!(
            notes,
            ["defaults i mira-bots.workspace.json læses først i et senere trin (nu profilens værdier)"]
        );
        let (r, _, notes) = effective(&file(r#"{"projectsRoot": "C:\\x"}"#));
        assert_eq!(r, WorkspaceRules::defaults());
        assert_eq!(
            notes,
            ["projectsRoot hører til i appens indstillinger og ignoreres"]
        );
    }

    #[test]
    fn max_review_rounds_is_clamped_1_to_10() {
        for (v, want, note) in [
            (0, 1, Some("maxReviewRounds 0 er sat op til 1 (1–10)")),
            (1, 1, None),
            (3, 3, None),
            (10, 10, None),
            (11, 10, Some("maxReviewRounds 11 er sat ned til 10 (1–10)")),
            (
                1000,
                10,
                Some("maxReviewRounds 1000 er sat ned til 10 (1–10)"),
            ),
        ] {
            let (r, _, notes) = effective(&file(&format!(r#"{{"maxReviewRounds": {v}}}"#)));
            assert_eq!(r.max_review_rounds, want, "{v}");
            assert_eq!(notes, note.into_iter().collect::<Vec<_>>(), "{v}");
        }
        // No trace of the step 5 note.
        let (_, _, notes) = effective(&file(r#"{"maxReviewRounds": 7}"#));
        assert!(notes.iter().all(|n| !n.contains("senere trin")));
    }

    #[test]
    fn git_mode_parses_and_unknown_gives_note() {
        for (v, want) in [
            ("off", GitMode::Off),
            ("worktree", GitMode::Worktree),
            ("Worktree", GitMode::Worktree),
        ] {
            let (r, _, notes) = effective(&WorkspaceFile {
                git: Some(v.into()),
                ..WorkspaceFile::default()
            });
            assert_eq!(r.git, want, "{v}");
            assert!(notes.is_empty(), "{v}: {notes:?}");
        }
        // Branch mode is deferred (6b-2): off with a note.
        for v in ["branch", " BRANCH "] {
            let (r, _, notes) = effective(&WorkspaceFile {
                git: Some(v.into()),
                ..WorkspaceFile::default()
            });
            assert_eq!(r.git, GitMode::Off, "{v}");
            assert_eq!(notes, ["git: branch kommer i et senere trin; off bruges"]);
        }
        let (r, _, notes) = effective(&file(r#"{"git": "foo"}"#));
        assert_eq!(r.git, GitMode::Off);
        assert_eq!(
            notes,
            ["git «foo» er ukendt (off, branch eller worktree); off bruges"]
        );
        let (r, _, notes) = effective(&file(r#"{"git": ""}"#));
        assert_eq!((r.git, notes.len()), (GitMode::Off, 1));
        // A wrong JSON type rejects the whole file, like every other typed field.
        assert!(parse(r#"{"git": true}"#).warning.is_some());
    }

    #[test]
    fn git_base_is_trimmed_and_validated() {
        let (_, c, notes) = effective(&file(r#"{"gitBase": "  develop "}"#));
        assert_eq!((c.git_base.as_deref(), notes.len()), (Some("develop"), 0));
        let (_, c, _) = effective(&file(r#"{"gitBase": "origin/release-1.2"}"#));
        assert_eq!(c.git_base.as_deref(), Some("origin/release-1.2"));
        for bad in ["", "   ", "my branch", "--force", &"b".repeat(101)] {
            let (_, c, notes) = effective(&WorkspaceFile {
                git_base: Some(bad.into()),
                ..WorkspaceFile::default()
            });
            assert_eq!(c.git_base, None, "{bad}");
            assert_eq!(
                notes,
                [format!(
                    "gitBase «{bad}» ignoreres (1–100 tegn uden mellemrum)"
                )]
            );
        }
        let (_, c, notes) = effective(&WorkspaceFile {
            git_base: Some("b".repeat(100)),
            ..WorkspaceFile::default()
        });
        assert!(c.git_base.is_some() && notes.is_empty());
    }

    #[test]
    fn bools_6b_are_read() {
        let d = WorkspaceRules::defaults();
        assert_eq!(
            (
                d.git,
                d.checks_gate,
                d.auto_spawn_for_playbook,
                d.fresh_session_per_ticket,
                d.cleanup_worktrees_on_done
            ),
            (GitMode::Off, true, false, true, false)
        );
        let (r, _, notes) = effective(&file(
            r#"{"checksGate": false, "autoSpawnForPlaybook": true,
                "freshSessionPerTicket": false, "cleanupWorktreesOnDone": true}"#,
        ));
        assert_eq!(notes, [CLEANUP_LATER_NOTE]);
        assert_eq!(
            (
                r.checks_gate,
                r.auto_spawn_for_playbook,
                r.fresh_session_per_ticket,
                r.cleanup_worktrees_on_done
            ),
            (false, true, false, true)
        );
        let (r, _, _) = effective(&file(r#"{"freshSessionPerTicket": true}"#));
        assert!(r.fresh_session_per_ticket && r.checks_gate);
        assert!(parse(r#"{"checksGate": "yes"}"#).warning.is_some());
    }

    #[test]
    fn playbooks_merge_over_builtins_and_bad_ones_are_dropped_with_notes() {
        let (_, c, notes) = effective(&file(
            r#"{"playbooks": {
                "docs": {"steps": [{"role": "researcher", "title": "Skriv: {title}"}]},
                "feature": {"steps": [{"role": "coder", "title": "Byg: {title}"}]},
                "bug": {"steps": [{"role": "tester", "title": "x"}]},
                "UPPER": {"steps": [{"role": "coder", "title": "x"}]},
                "empty": {"steps": []}
            }, "maxWorkAgents": 2}"#,
        ));
        // The file wins for "feature"; the invalid "bug" leaves the built-in one; "docs" is new.
        assert_eq!(c.playbook_kinds(), ["bug", "docs", "feature"]);
        assert_eq!(c.playbooks["feature"].steps.len(), 1);
        assert_eq!(c.playbooks["bug"], builtin_playbooks()["bug"]);
        assert_eq!(
            c.playbooks["docs"].steps[0].role,
            crate::agent::roles::Role::Researcher
        );
        assert_eq!(
            notes,
            [
                "playbooks.UPPER ignoreres: ugyldigt navn",
                "playbooks.bug: ukendt rolle «tester»",
                "playbooks.empty ignoreres: steps skal være en liste med 1–6 trin",
            ]
        );
        // Never a rejected file: the other fields still apply, no warning.
        let s = parse(r#"{"maxWorkAgents": 2, "playbooks": {"x": {"steps": [{"role": 7}]}}}"#);
        assert_eq!((s.warning, s.rules.max_work_agents), (None, 2));
        assert_eq!(s.notes, ["playbooks.x ignoreres: trin 1 mangler role"]);
        assert_eq!(s.config.playbook_kinds(), ["bug", "feature"]);
        // Not an object: a note, the built-ins; `null` counts as absent.
        let (_, c, notes) = effective(&file(r#"{"playbooks": ["feature"]}"#));
        assert_eq!(c, WorkspaceConfig::default());
        assert_eq!(notes, ["playbooks ignoreres: skal være et objekt"]);
        let (_, c, notes) = effective(&file(r#"{"playbooks": null}"#));
        assert_eq!((c, notes.len()), (WorkspaceConfig::default(), 0));
    }

    #[test]
    fn playbook_named_task_is_ignored() {
        let (_, c, notes) = effective(&file(
            r#"{"playbooks": {"task": {"steps": [{"role": "coder", "title": "x"}]}}}"#,
        ));
        assert!(!c.playbooks.contains_key("task"));
        assert_eq!(c.playbook_kinds(), ["bug", "feature"]);
        assert_eq!(notes, ["playbooks.task ignoreres: ugyldigt navn"]);
    }

    #[test]
    fn wrong_type_or_invalid_json_rejects_the_file() {
        let s = parse(r#"{"maxWorkAgents": "5"}"#);
        assert_eq!(s.rules, WorkspaceRules::defaults());
        assert!(s.file_exists);
        let w = s.warning.unwrap();
        assert!(
            w.starts_with("mira-bots.workspace.json kunne ikke læses: "),
            "{w}"
        );
        assert!(w.ends_with("; standardværdierne bruges"), "{w}");
        let s = parse("{ not json");
        assert_eq!(s.rules, WorkspaceRules::defaults());
        assert!(s.warning.is_some());
        let s = parse(r#"{"maxWorkAgents": -1}"#);
        assert!(s.warning.is_some());
    }

    #[test]
    fn reader_missing_file_gives_defaults_without_warning() {
        let d = TempDir::new();
        let r = d.reader();
        assert_eq!(r.root(), d.0.as_path());
        assert!(r.path().ends_with(WORKSPACE_FILE));
        let s = r.snapshot();
        assert_eq!(s, WorkspaceSnapshot::defaults(false, None));
        assert_eq!(r.rules(), WorkspaceRules::defaults());
    }

    #[test]
    fn reader_caches_on_mtime_and_len_and_rereads_on_change() {
        let d = TempDir::new();
        let r = d.reader();
        let path = r.path().to_path_buf();
        fs::write(&path, r#"{"maxWorkAgents": 2}"#).unwrap();
        let t0 = SystemTime::now() - Duration::from_secs(100);
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(t0)
            .unwrap();
        let s = r.snapshot();
        assert!(s.file_exists && s.warning.is_none());
        assert_eq!(s.rules.max_work_agents, 2);
        assert_eq!(r.config(), WorkspaceConfig::default());

        // Same length and mtime: the cache answers (the new text is not read).
        fs::write(&path, r#"{"maxWorkAgents": 3}"#).unwrap();
        let f = fs::File::options().write(true).open(&path).unwrap();
        f.set_modified(t0).unwrap();
        assert_eq!(r.rules().max_work_agents, 2);

        // Same length, new mtime: read again.
        f.set_modified(t0 + Duration::from_secs(10)).unwrap();
        drop(f);
        assert_eq!(r.rules().max_work_agents, 3);

        // Invalid content: warning + defaults, cached.
        fs::write(&path, "{").unwrap();
        let s = r.snapshot();
        assert!(s.warning.is_some());
        assert_eq!(s.rules, WorkspaceRules::defaults());
        assert_eq!(r.snapshot(), s);

        // Deleted: defaults again, no warning.
        fs::remove_file(&path).unwrap();
        assert_eq!(r.snapshot(), WorkspaceSnapshot::defaults(false, None));
    }
}
