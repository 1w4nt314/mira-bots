//! The workspace file `<projects root>/mira-bots.workspace.json` (plan4b A.4, C4b.2): optional
//! rules that override the defaults from `config.rs`. Read on demand (spawn, assignment, tool
//! call, Diagnostik) through [`WorkspaceReader`], which re-parses only when the file's
//! `(mtime, len)` changed; no file watcher (research4b §4).

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde::Deserialize;

use crate::config::{MAX_REVIEW_ROUNDS, WORKSPACE_FILE};
use crate::tickets::model::WorkspaceRules;

/// Number of work seats (= `WORK_SEATS` in seats.ts); `maxWorkAgents` is clamped to it.
pub const MAX_WORK_SEATS: usize = 5;
/// Number of staff seats (= `STAFF_SEATS`); `maxStaffAgents` is clamped to it.
pub const MAX_STAFF_SEATS: usize = 3;
/// Upper bound of `userInputGraceMs`.
pub const USER_INPUT_GRACE_MAX_MS: u64 = 60_000;

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

/// The effective rules for `file` plus notes for the user (clamped values, fields that are read
/// but not enforced yet, `projectsRoot` in the wrong file).
pub fn effective(file: &WorkspaceFile) -> (WorkspaceRules, Vec<String>) {
    let mut r = WorkspaceRules::defaults();
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
    if file
        .max_review_rounds
        .is_some_and(|v| v != MAX_REVIEW_ROUNDS)
    {
        notes.push(format!(
            "maxReviewRounds i {WORKSPACE_FILE} læses først i et senere trin (nu {MAX_REVIEW_ROUNDS})"
        ));
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
    (r, notes)
}

/// What the reader found: the effective rules, notes, a warning when the file could not be
/// read/parsed (the defaults apply then) and whether the file exists.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceSnapshot {
    pub rules: WorkspaceRules,
    pub notes: Vec<String>,
    pub warning: Option<String>,
    pub file_exists: bool,
}

impl WorkspaceSnapshot {
    fn defaults(file_exists: bool, warning: Option<String>) -> Self {
        WorkspaceSnapshot {
            rules: WorkspaceRules::defaults(),
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
            let (rules, notes) = effective(&file);
            WorkspaceSnapshot {
                rules,
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
        let (r, notes) = effective(&file("{}"));
        assert_eq!(r, WorkspaceRules::defaults());
        assert!(notes.is_empty());
        assert_eq!(file("{}"), WorkspaceFile::default());
    }

    #[test]
    fn partial_file_overrides_only_its_fields() {
        let (r, notes) = effective(&file(r#"{"maxWorkAgents": 2, "autoReviewOnStop": true}"#));
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
        let (r, notes) = effective(&file(
            r#"{"maxWorkAgents": 3, "maxStaffAgents": 2, "maxReviewRounds": 3,
                "reviewByDefault": false, "autoReviewOnStop": true, "userInputGraceMs": 1000,
                "agentsMayCreateProjects": true, "maxAgentsPerProject": 2, "defaults": {},
                "somethingNew": [1, 2]}"#,
        ));
        assert_eq!(
            r,
            WorkspaceRules {
                max_work_agents: 3,
                max_staff_agents: 2,
                review_by_default: false,
                auto_review_on_stop: true,
                user_input_grace_ms: 1000,
                agents_may_create_projects: true,
                max_agents_per_project: 2,
                ..WorkspaceRules::defaults()
            }
        );
        assert!(notes.is_empty(), "{notes:?}");
    }

    #[test]
    fn values_are_clamped_with_a_note() {
        let (r, notes) = effective(&file(r#"{"maxWorkAgents": 9}"#));
        assert_eq!(r.max_work_agents, 5);
        assert_eq!(notes, ["maxWorkAgents 9 er sat ned til 5 (antal pladser)"]);
        let (r, notes) = effective(&file(r#"{"maxWorkAgents": 0}"#));
        assert_eq!(r.max_work_agents, 1);
        assert_eq!(notes, ["maxWorkAgents 0 er sat op til 1 (antal pladser)"]);
        let (r, notes) = effective(&file(r#"{"maxStaffAgents": 4}"#));
        assert_eq!(r.max_staff_agents, 3);
        assert_eq!(notes.len(), 1);
        let (r, notes) = effective(&file(r#"{"userInputGraceMs": 90000}"#));
        assert_eq!(r.user_input_grace_ms, 60_000);
        assert_eq!(notes.len(), 1);
        let (r, _) = effective(&file(
            r#"{"userInputGraceMs": 0, "maxAgentsPerProject": 0}"#,
        ));
        assert_eq!((r.user_input_grace_ms, r.max_agents_per_project), (0, 0));
    }

    #[test]
    fn read_but_not_enforced_fields_give_notes() {
        let (r, notes) = effective(&file(r#"{"maxReviewRounds": 5}"#));
        assert_eq!(r.max_review_rounds, 3);
        assert_eq!(
            notes,
            ["maxReviewRounds i mira-bots.workspace.json læses først i et senere trin (nu 3)"]
        );
        let (_, notes) = effective(&file(r#"{"defaults": {"model": "opus"}}"#));
        assert_eq!(
            notes,
            ["defaults i mira-bots.workspace.json læses først i et senere trin (nu profilens værdier)"]
        );
        let (r, notes) = effective(&file(r#"{"projectsRoot": "C:\\x"}"#));
        assert_eq!(r, WorkspaceRules::defaults());
        assert_eq!(
            notes,
            ["projectsRoot hører til i appens indstillinger og ignoreres"]
        );
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
