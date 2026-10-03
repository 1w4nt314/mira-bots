//! Project checks (step 6b, plan6b punkt 14, A.3, C6b.4): the user's
//! `<project>/.mira-bots/project.json` (`checks` and `gitBase`), a [`CheckRunner`] that runs one
//! check line in a folder (the real one, [`ProcessChecks`], through [`crate::proc`]; a fake in
//! tests), [`run_checks`] (sequential, every check runs) and the text of the app's «Tjek»
//! report.
//!
//! The command lines come only from the user's file (agents cannot edit it: deny rules, plan
//! A.5). A line is passed to the shell as one argument; no agent text ever reaches the command
//! line or the environment.
// TODO(windows-verify): `"run": "npm test"` runs through `cmd /D /S /C` without a window; the
// «Tjek» report shows the exit code and seconds; æ/ø/å in the output (OEM code page: lossy)
// (plan6b D.97).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::config::{
    CHECKS_MAX, CHECKS_REPORT_TITLE, CHECK_OUTPUT_MAX_CHARS, CHECK_TIMEOUT_DEFAULT_SEC,
    CHECK_TIMEOUT_MAX_SEC, PROJECT_FILE, REPORT_BODY_MAX_CHARS,
};
use crate::proc::{self, ProcRunner, SystemProc};
use crate::watch::config::{parse_watch, probe_playbook, WatchConfig};

/// Most chars of a check's `name` (one line).
pub const CHECK_NAME_MAX_CHARS: usize = 40;
/// Most chars of a check's `run` (one line).
pub const CHECK_RUN_MAX_CHARS: usize = 1_000;

/// One check of the project file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    /// The shell line, verbatim from the user's file.
    pub run: String,
    /// Clamped to `1..=CHECK_TIMEOUT_MAX_SEC`, default [`CHECK_TIMEOUT_DEFAULT_SEC`].
    pub timeout_sec: u64,
}

/// `<project>/.mira-bots/project.json`, validated.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectFile {
    pub checks: Vec<Check>,
    /// The base branch for the ticket branches of this project (wins over the workspace's).
    pub git_base: Option<String>,
    /// What was adjusted or ignored (clamped `timeoutSec`, an invalid `gitBase`, an invalid
    /// `github`); logged.
    pub notes: Vec<String>,
    /// The project's GitHub issues source (step 6c, C6c.2); `None`: absent or invalid (a note).
    pub github: Option<GithubConfig>,
    /// Vagt-tilstand (trin 6d, C6d.2); `None`: fraværende eller ikke et objekt (en note). Vagten
    /// er kun til når `enabled` er `true`.
    pub watch: Option<WatchConfig>,
}

/// `project.json` → `github` (step 6c, plan punkt 13): the repo whose open issues the inbox
/// lists (`labels`: all of them, AND), and what Done writes back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GithubConfig {
    /// `owner/name` ([`crate::gh::valid_repo`]).
    pub repo: String,
    /// At most [`GITHUB_LABELS_MAX`], one line, 1–[`GITHUB_LABEL_MAX_CHARS`] chars each.
    pub labels: Vec<String>,
    /// Always `"open"` in 6c.
    pub state: String,
    pub write_back: WriteBackConfig,
}

/// `github.writeBack` (default: nothing is written; a comment is public in a public repo).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WriteBackConfig {
    pub comment: bool,
    pub close: bool,
}

/// Most labels of `github.labels`.
pub const GITHUB_LABELS_MAX: usize = 10;
/// Longest label of `github.labels`.
pub const GITHUB_LABEL_MAX_CHARS: usize = 50;

/// Prefix of every note that switches the GitHub source off.
pub const GITHUB_IGNORED_PREFIX: &str = "project.json: github ignoreres: ";

/// Parses `github` (plan punkt 13). `Err(reason)`: the source is off and the caller adds the
/// note [`GITHUB_IGNORED_PREFIX`]`reason`; `notes` gets the milder ones (an ignored `state`).
fn parse_github(v: &Value, notes: &mut Vec<String>) -> Result<GithubConfig, String> {
    let Value::Object(g) = v else {
        return Err("skal være et objekt".into());
    };
    let repo = match g.get("repo") {
        Some(Value::String(r)) if crate::gh::valid_repo(r.trim()) => r.trim().to_string(),
        Some(Value::String(r)) => {
            return Err(format!(
                "repo «{}» skal have formen ejer/navn",
                crate::tickets::prompt::one_line(r)
            ))
        }
        _ => return Err("repo mangler (formen ejer/navn)".into()),
    };
    let labels_rule = format!(
        "labels skal være en liste med højst {GITHUB_LABELS_MAX} tekster på 1–{GITHUB_LABEL_MAX_CHARS} tegn"
    );
    let labels = match g.get("labels") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(list)) if list.len() <= GITHUB_LABELS_MAX => list
            .iter()
            .map(|l| one_line_text(l, GITHUB_LABEL_MAX_CHARS))
            .collect::<Option<Vec<String>>>()
            .ok_or_else(|| labels_rule.clone())?,
        Some(_) => return Err(labels_rule),
    };
    // Review6c W5: gh's `--label` splits on commas (two labels, AND) and a leading `-` reads
    // like an option; such a label turns the source off.
    if let Some(bad) = labels
        .iter()
        .find(|l| l.contains(',') || l.starts_with('-'))
    {
        return Err(format!(
            "label «{}» må ikke indeholde komma eller starte med «-»",
            crate::tickets::prompt::one_line(bad)
        ));
    }
    match g.get("state") {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) if s.trim() == "open" => {}
        Some(other) => notes.push(format!(
            "project.json: github.state «{}» ignoreres (kun \"open\")",
            crate::tickets::prompt::one_line(&match other {
                Value::String(s) => s.clone(),
                v => v.to_string(),
            })
        )),
    }
    let write_back = match g.get("writeBack") {
        None | Some(Value::Null) => WriteBackConfig::default(),
        Some(Value::Object(w)) => {
            let flag = |k: &str| match w.get(k) {
                None | Some(Value::Null) => Ok(false),
                Some(Value::Bool(b)) => Ok(*b),
                Some(_) => Err(format!("writeBack.{k} skal være true eller false")),
            };
            WriteBackConfig {
                comment: flag("comment")?,
                close: flag("close")?,
            }
        }
        Some(_) => return Err("writeBack skal være et objekt".into()),
    };
    Ok(GithubConfig {
        repo,
        labels,
        state: "open".into(),
        write_back,
    })
}

/// `<project_dir>/.mira-bots/project.json`, joined component by component.
pub fn project_file_path(project_dir: &Path) -> PathBuf {
    PROJECT_FILE
        .split('/')
        .fold(project_dir.to_path_buf(), |p, part| p.join(part))
}

fn one_line_text(v: &Value, max: usize) -> Option<String> {
    let s = v.as_str()?.trim();
    let ok = !s.is_empty() && s.chars().count() <= max && !s.chars().any(char::is_control);
    ok.then(|| s.to_string())
}

/// Parses and validates the file's text (plan A.3): an object; `checks` a list of at most
/// [`CHECKS_MAX`] objects with `name` (1–40 chars, one line), `run` (1–1000 chars, one line) and
/// an optional `timeoutSec` (whole seconds, clamped to 1–3600 with a note, default 600);
/// `gitBase` optional (an invalid value is ignored with a note); `github` and `watch` (step 6d)
/// are validated with notes, never a rejected file. Any other error rejects the whole file with
/// a text starting "project.json: ".
pub fn parse_project_file(text: &str) -> Result<ProjectFile, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("project.json: {e}"))?;
    let Value::Object(map) = v else {
        return Err("project.json: skal være et JSON-objekt".into());
    };
    let mut out = ProjectFile::default();
    match map.get("checks") {
        None | Some(Value::Null) => {}
        Some(Value::Array(list)) => {
            if list.len() > CHECKS_MAX {
                return Err(format!(
                    "project.json: højst {CHECKS_MAX} tjek (filen har {})",
                    list.len()
                ));
            }
            for (i, c) in list.iter().enumerate() {
                let Value::Object(c) = c else {
                    return Err(format!("project.json: checks[{i}] skal være et objekt"));
                };
                let name = c
                    .get("name")
                    .and_then(|n| one_line_text(n, CHECK_NAME_MAX_CHARS))
                    .ok_or_else(|| {
                        format!(
                            "project.json: checks[{i}].name skal være 1–{CHECK_NAME_MAX_CHARS} tegn på én linje"
                        )
                    })?;
                let run = c
                    .get("run")
                    .and_then(|r| one_line_text(r, CHECK_RUN_MAX_CHARS))
                    .ok_or_else(|| {
                        format!(
                            "project.json: checks[{i}].run skal være 1–{CHECK_RUN_MAX_CHARS} tegn på én linje"
                        )
                    })?;
                let timeout_sec = match c.get("timeoutSec") {
                    None | Some(Value::Null) => CHECK_TIMEOUT_DEFAULT_SEC,
                    Some(t) => {
                        let Some(t) = t.as_u64() else {
                            return Err(format!(
                                "project.json: checks[{i}].timeoutSec skal være et helt antal sekunder"
                            ));
                        };
                        let c = t.clamp(1, CHECK_TIMEOUT_MAX_SEC);
                        if c != t {
                            let dir = if c < t { "ned" } else { "op" };
                            out.notes.push(format!(
                                "project.json: checks[{i}].timeoutSec {t} er sat {dir} til {c} (1–{CHECK_TIMEOUT_MAX_SEC})"
                            ));
                        }
                        c
                    }
                };
                out.checks.push(Check {
                    name,
                    run,
                    timeout_sec,
                });
            }
        }
        Some(_) => return Err("project.json: checks skal være en liste".into()),
    }
    match map.get("gitBase") {
        None | Some(Value::Null) => {}
        Some(Value::String(s)) => match crate::workspace::valid_git_base(s) {
            Some(b) => out.git_base = Some(b),
            None => out.notes.push(format!(
                "project.json: gitBase «{}» ignoreres (1–{} tegn uden mellemrum)",
                s.trim(),
                crate::workspace::GIT_BASE_MAX_CHARS
            )),
        },
        Some(_) => out
            .notes
            .push("project.json: gitBase skal være en tekst; den ignoreres".into()),
    }
    match map.get("github") {
        None | Some(Value::Null) => {}
        Some(g) => match parse_github(g, &mut out.notes) {
            Ok(c) => out.github = Some(c),
            Err(reason) => out.notes.push(format!("{GITHUB_IGNORED_PREFIX}{reason}")),
        },
    }
    match map.get("watch") {
        None | Some(Value::Null) => {}
        // Andet pas over teksten: `byLabel` i filens rækkefølge (plan6d A.4).
        Some(w) => out.watch = parse_watch(w, probe_playbook(text).as_ref(), &mut out.notes),
    }
    Ok(out)
}

/// Reads `<project_dir>/.mira-bots/project.json` without a cache: a missing file is `Ok(None)`,
/// an unreadable or invalid one `Err` (see [`parse_project_file`]).
pub fn read_project_file(project_dir: &Path) -> Result<Option<ProjectFile>, String> {
    match std::fs::read_to_string(project_file_path(project_dir)) {
        Ok(text) => parse_project_file(&text).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("project.json: {e}")),
    }
}

type ProjectCacheEntry = (Option<SystemTime>, u64, Result<ProjectFile, String>);

/// [`read_project_file`] with a per-folder `(mtime, len)` cache, like the workspace file
/// ([`crate::workspace::WorkspaceReader`]). Only locks its own mutex (safe under other locks,
/// though callers do not hold any).
#[derive(Default)]
pub struct ProjectFileReader {
    cache: Mutex<HashMap<PathBuf, ProjectCacheEntry>>,
}

impl ProjectFileReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// The project file of `project_dir`: cached while its mtime and length are unchanged.
    pub fn read(&self, project_dir: &Path) -> Result<Option<ProjectFile>, String> {
        crate::tickets::assert_not_under_inbox_lock("project.json");
        let path = project_file_path(project_dir);
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                cache.remove(&path);
                return Ok(None);
            }
            Err(e) => {
                cache.remove(&path);
                return Err(format!("project.json: {e}"));
            }
        };
        let key = (meta.modified().ok(), meta.len());
        if let Some((mtime, len, parsed)) = cache.get(&path) {
            if (*mtime, *len) == key {
                return parsed.clone().map(Some);
            }
        }
        let parsed = match std::fs::read_to_string(&path) {
            Ok(text) => parse_project_file(&text),
            Err(e) => Err(format!("project.json: {e}")),
        };
        match &parsed {
            Ok(f) => {
                for n in &f.notes {
                    log::info!("{}: {n}", path.display());
                }
            }
            Err(e) => log::warn!("{}: {e}", path.display()),
        }
        cache.insert(path, (key.0, key.1, parsed.clone()));
        parsed.map(Some)
    }
}

/// How one check ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    /// Ended on its own (a signal on Unix is code -1). `output`: the tail (stdout and stderr).
    Exit {
        code: i32,
        output: String,
        clipped: bool,
        elapsed_ms: u64,
    },
    /// Killed (with its tree) after `timeout_sec`.
    TimedOut {
        output: String,
        clipped: bool,
        elapsed_ms: u64,
    },
    /// Could not be started.
    SpawnFailed(String),
}

/// Runs one check line in a folder. Blocking; called off every lock (the checks thread).
pub trait CheckRunner: Send + Sync {
    fn run(&self, check: &Check, cwd: &Path) -> RunOutcome;
}

/// The real runner: [`proc::shell_command`] (Unix `$SHELL -lc <run>`, Windows
/// `cmd /D /S /C "<run>"`) with the login PATH ([`crate::agent::process::spawn_env_extra`]),
/// the check's timeout, the last [`CHECK_OUTPUT_MAX_CHARS`] chars of output, and the children in
/// [`proc::registry`] (killed at app exit).
pub struct ProcessChecks {
    proc: Arc<dyn ProcRunner>,
}

impl ProcessChecks {
    pub fn new() -> Self {
        ProcessChecks {
            proc: Arc::new(SystemProc {
                registry: Some(proc::registry()),
            }),
        }
    }

    /// A given process runner (tests).
    pub fn with(proc: Arc<dyn ProcRunner>) -> Self {
        ProcessChecks { proc }
    }
}

impl Default for ProcessChecks {
    fn default() -> Self {
        Self::new()
    }
}

impl CheckRunner for ProcessChecks {
    fn run(&self, check: &Check, cwd: &Path) -> RunOutcome {
        let spec = proc::shell_command(&check.run);
        let env = crate::agent::process::spawn_env_extra();
        match self.proc.run(
            &spec,
            cwd,
            &env,
            Duration::from_secs(check.timeout_sec),
            CHECK_OUTPUT_MAX_CHARS,
        ) {
            Err(e) => RunOutcome::SpawnFailed(e),
            Ok(c) if c.timed_out => RunOutcome::TimedOut {
                output: c.output,
                clipped: c.clipped,
                elapsed_ms: c.elapsed_ms,
            },
            Ok(c) => RunOutcome::Exit {
                code: c.code.unwrap_or(-1),
                output: c.output,
                clipped: c.clipped,
                elapsed_ms: c.elapsed_ms,
            },
        }
    }
}

/// One line of the «Tjek» report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckLine {
    pub name: String,
    pub ok: bool,
    /// "Tjek: {name} → OK (exit 0, 3 s)" etc. (C6b.4).
    pub text: String,
    /// Why it failed, for the gate note: "exit {code}", "timeout" or "kunne ikke starte".
    pub reason: Option<String>,
    /// The output tail of a failed check (exit ≠ 0 or timeout); `None` for passed checks.
    pub output: Option<String>,
}

/// The result of [`run_checks`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChecksReport {
    pub lines: Vec<CheckLine>,
    /// Name of the first failed check.
    pub failed: Option<String>,
}

impl ChecksReport {
    /// The first failed line.
    pub fn first_failure(&self) -> Option<&CheckLine> {
        self.lines.iter().find(|l| !l.ok)
    }
}

/// Whole seconds, rounded.
fn secs(ms: u64) -> u64 {
    (ms + 500) / 1000
}

/// One check's report line (and failure reason/output).
fn check_line(check: &Check, outcome: RunOutcome) -> CheckLine {
    let name = &check.name;
    let (ok, text, reason, output) = match outcome {
        RunOutcome::Exit {
            code: 0,
            elapsed_ms,
            ..
        } => (
            true,
            format!("Tjek: {name} → OK (exit 0, {} s)", secs(elapsed_ms)),
            None,
            None,
        ),
        RunOutcome::Exit {
            code,
            output,
            elapsed_ms,
            ..
        } => (
            false,
            format!("Tjek: {name} → FEJL (exit {code}, {} s)", secs(elapsed_ms)),
            Some(format!("exit {code}")),
            Some(output),
        ),
        RunOutcome::TimedOut { output, .. } => (
            false,
            format!(
                "Tjek: {name} → FEJL (timeout efter {} s)",
                check.timeout_sec
            ),
            Some("timeout".to_string()),
            Some(output),
        ),
        RunOutcome::SpawnFailed(e) => (
            false,
            format!(
                "Tjek: {name} → FEJL (kunne ikke starte: {})",
                crate::tickets::prompt::one_line(&e)
            ),
            Some("kunne ikke starte".to_string()),
            None,
        ),
    };
    CheckLine {
        name: name.clone(),
        ok,
        text,
        reason,
        output,
    }
}

/// Runs `checks` one after the other in `cwd`; a failure does not stop the rest. Blocking.
pub fn run_checks(runner: &dyn CheckRunner, checks: &[Check], cwd: &Path) -> ChecksReport {
    let mut report = ChecksReport::default();
    for check in checks {
        let line = check_line(check, runner.run(check, cwd));
        log::info!("checks: {} in {}", line.text, cwd.display());
        if !line.ok && report.failed.is_none() {
            report.failed = Some(line.name.clone());
        }
        report.lines.push(line);
    }
    report
}

/// Cuts `s` to at most `max` chars, ending with "…" when cut.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The «Tjek» report (C6b.4): title "Tjek: OK" or "Tjek: FEJL ({first failed})"; the body has
/// one line per check, then for every failed check with output "--- {name} (sidste {m} tegn)
/// ---" and its output tail. The body is cut to [`REPORT_BODY_MAX_CHARS`].
pub fn render_checks_report(report: &ChecksReport) -> (String, String) {
    let title = match &report.failed {
        None => format!("{CHECKS_REPORT_TITLE}: OK"),
        Some(name) => format!("{CHECKS_REPORT_TITLE}: FEJL ({name})"),
    };
    let mut body = report
        .lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    for l in report.lines.iter().filter(|l| !l.ok) {
        let Some(output) = l.output.as_deref() else {
            continue;
        };
        let output = output.trim_end();
        if output.trim().is_empty() {
            continue;
        }
        body.push_str(&format!(
            "\n\n--- {} (sidste {} tegn) ---\n{output}",
            l.name,
            output.chars().count()
        ));
    }
    (title, clip(&body, REPORT_BODY_MAX_CHARS))
}

/// The «Tjek» report for a project file that could not be read (title
/// "Tjek: project.json kunne ikke læses", body: the error).
pub fn unreadable_report(error: &str) -> (String, String) {
    (
        format!("{CHECKS_REPORT_TITLE}: project.json kunne ikke læses"),
        clip(error, REPORT_BODY_MAX_CHARS),
    )
}

/// The gate's rejection note: "Tjek fejlede: {name} ({exit {code}|timeout}). Se rapport {id}."
/// (without the reference when the report could not be added).
pub fn gate_note(line: &CheckLine, report_id: Option<&str>) -> String {
    let head = format!(
        "Tjek fejlede: {} ({}).",
        line.name,
        line.reason.as_deref().unwrap_or("fejl")
    );
    match report_id {
        Some(id) => format!("{head} Se rapport {id}."),
        None => head,
    }
}

/// Scripted checks for tests: an outcome per check name (default: exit 0 after 1 s), every call
/// logged with its folder, and an optional hold that keeps `run` waiting until released.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::sync::Condvar;

    #[derive(Default)]
    pub(crate) struct FakeChecks {
        outcomes: Mutex<HashMap<String, RunOutcome>>,
        log: Mutex<Vec<(String, PathBuf)>>,
        held: Mutex<bool>,
        released: Condvar,
        panics: Mutex<bool>,
    }

    impl FakeChecks {
        pub(crate) fn new() -> Self {
            Self::default()
        }

        /// `name` ends with `outcome`.
        pub(crate) fn outcome(&self, name: &str, outcome: RunOutcome) {
            self.outcomes
                .lock()
                .unwrap()
                .insert(name.to_string(), outcome);
        }

        /// `name` exits with `code` and `output`.
        pub(crate) fn exit(&self, name: &str, code: i32, output: &str) {
            self.outcome(
                name,
                RunOutcome::Exit {
                    code,
                    output: output.into(),
                    clipped: false,
                    elapsed_ms: 2_000,
                },
            );
        }

        /// Every `run` waits until [`Self::release`].
        pub(crate) fn hold(&self) {
            *self.held.lock().unwrap() = true;
        }

        pub(crate) fn release(&self) {
            *self.held.lock().unwrap() = false;
            self.released.notify_all();
        }

        /// Every `run` panics (an internal error on the checks thread, review6b N20).
        pub(crate) fn panic(&self) {
            *self.panics.lock().unwrap() = true;
        }

        /// The checks run so far (name, folder), in order.
        pub(crate) fn calls(&self) -> Vec<(String, PathBuf)> {
            self.log.lock().unwrap().clone()
        }
    }

    impl CheckRunner for FakeChecks {
        fn run(&self, check: &Check, cwd: &Path) -> RunOutcome {
            {
                let mut held = self.held.lock().unwrap();
                while *held {
                    held = self.released.wait(held).unwrap();
                }
            }
            if *self.panics.lock().unwrap() {
                panic!("fake check {} panicked", check.name);
            }
            self.log
                .lock()
                .unwrap()
                .push((check.name.clone(), cwd.to_path_buf()));
            self.outcomes
                .lock()
                .unwrap()
                .get(&check.name)
                .cloned()
                .unwrap_or(RunOutcome::Exit {
                    code: 0,
                    output: String::new(),
                    clipped: false,
                    elapsed_ms: 1_000,
                })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeChecks;
    use super::*;

    fn check(name: &str, run: &str, timeout_sec: u64) -> Check {
        Check {
            name: name.into(),
            run: run.into(),
            timeout_sec,
        }
    }

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("mira-checks-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
        fn write(&self, text: &str) {
            let f = project_file_path(&self.0);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, text).unwrap();
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn read_project_file_validates_and_clamps() {
        let f = parse_project_file(
            r#"{"checks": [
                {"name": " tests ", "run": "npm test", "timeoutSec": 600},
                {"name": "lint", "run": "npm run lint"},
                {"name": "lang", "run": "x", "timeoutSec": 9000},
                {"name": "kort", "run": "y", "timeoutSec": 0}
            ], "gitBase": " develop "}"#,
        )
        .unwrap();
        assert_eq!(
            f.checks,
            vec![
                check("tests", "npm test", 600),
                check("lint", "npm run lint", CHECK_TIMEOUT_DEFAULT_SEC),
                check("lang", "x", 3_600),
                check("kort", "y", 1),
            ]
        );
        assert_eq!(f.git_base.as_deref(), Some("develop"));
        assert_eq!(
            f.notes,
            vec![
                "project.json: checks[2].timeoutSec 9000 er sat ned til 3600 (1–3600)",
                "project.json: checks[3].timeoutSec 0 er sat op til 1 (1–3600)",
            ]
        );
        // gitBase of the wrong type or form: a note, the checks stay.
        let f = parse_project_file(r#"{"checks": [], "gitBase": 3}"#).unwrap();
        assert_eq!(
            (f.git_base, f.notes),
            (
                None,
                vec!["project.json: gitBase skal være en tekst; den ignoreres".to_string()]
            )
        );
        let f = parse_project_file(r#"{"gitBase": "-x main"}"#).unwrap();
        assert_eq!(f.git_base, None);
        assert_eq!(
            f.notes,
            vec!["project.json: gitBase «-x main» ignoreres (1–100 tegn uden mellemrum)"]
        );
        assert_eq!(parse_project_file("{}").unwrap(), ProjectFile::default());

        let err = |text: &str| parse_project_file(text).unwrap_err();
        assert!(err("nej").starts_with("project.json: "));
        assert_eq!(err("[]"), "project.json: skal være et JSON-objekt");
        assert_eq!(
            err(r#"{"checks": {}}"#),
            "project.json: checks skal være en liste"
        );
        assert_eq!(
            err(r#"{"checks": [1]}"#),
            "project.json: checks[0] skal være et objekt"
        );
        assert_eq!(
            err(r#"{"checks": [{"run": "x"}]}"#),
            "project.json: checks[0].name skal være 1–40 tegn på én linje"
        );
        let long = "n".repeat(41);
        assert_eq!(
            err(&format!(
                r#"{{"checks": [{{"name": "{long}", "run": "x"}}]}}"#
            )),
            "project.json: checks[0].name skal være 1–40 tegn på én linje"
        );
        assert_eq!(
            err(r#"{"checks": [{"name": "a", "run": "x"}, {"name": "b", "run": "x\ny"}]}"#),
            "project.json: checks[1].run skal være 1–1000 tegn på én linje"
        );
        assert_eq!(
            err(r#"{"checks": [{"name": "a", "run": 3}]}"#),
            "project.json: checks[0].run skal være 1–1000 tegn på én linje"
        );
        assert_eq!(
            err(r#"{"checks": [{"name": "a", "run": "x", "timeoutSec": "10"}]}"#),
            "project.json: checks[0].timeoutSec skal være et helt antal sekunder"
        );
        assert_eq!(
            err(r#"{"checks": [{"name": "a", "run": "x", "timeoutSec": -1}]}"#),
            "project.json: checks[0].timeoutSec skal være et helt antal sekunder"
        );
        let eleven = [r#"{"name": "a", "run": "x"}"#; 11].join(",");
        assert_eq!(
            err(&format!(r#"{{"checks": [{eleven}]}}"#)),
            "project.json: højst 10 tjek (filen har 11)"
        );
    }

    #[test]
    fn github_config_parses() {
        let f = parse_project_file(
            r#"{"gitBase": "main", "github": {"repo": " owner/name ", "labels": ["bug", "help wanted"],
                "state": "open", "writeBack": {"comment": true, "close": false}}}"#,
        )
        .unwrap();
        assert_eq!(
            f.github,
            Some(GithubConfig {
                repo: "owner/name".into(),
                labels: vec!["bug".into(), "help wanted".into()],
                state: "open".into(),
                write_back: WriteBackConfig {
                    comment: true,
                    close: false
                },
            })
        );
        assert!(f.notes.is_empty(), "{:?}", f.notes);
        // Defaults: no labels, nothing written back.
        let f = parse_project_file(r#"{"github": {"repo": "o/r"}}"#).unwrap();
        let g = f.github.unwrap();
        assert_eq!(
            (g.labels.len(), g.write_back),
            (0, WriteBackConfig::default())
        );
        // Another state: a note, the source stays (open).
        let f = parse_project_file(r#"{"github": {"repo": "o/r", "state": "all"}}"#).unwrap();
        assert_eq!(f.github.unwrap().state, "open");
        assert_eq!(
            f.notes,
            vec!["project.json: github.state «all» ignoreres (kun \"open\")"]
        );
        // Without github: none.
        assert_eq!(
            parse_project_file(r#"{"checks": []}"#).unwrap().github,
            None
        );
    }

    #[test]
    fn github_invalid_repo_is_note_not_error() {
        let cases = [
            (r#"{"github": "o/r"}"#, "skal være et objekt"),
            (r#"{"github": {}}"#, "repo mangler (formen ejer/navn)"),
            (
                r#"{"github": {"repo": "not a repo"}}"#,
                "repo «not a repo» skal have formen ejer/navn",
            ),
            (
                r#"{"github": {"repo": "host/o/r"}}"#,
                "repo «host/o/r» skal have formen ejer/navn",
            ),
            (
                r#"{"github": {"repo": "o/r", "labels": "bug"}}"#,
                "labels skal være en liste med højst 10 tekster på 1–50 tegn",
            ),
            (
                r#"{"github": {"repo": "o/r", "labels": ["a\nb"]}}"#,
                "labels skal være en liste med højst 10 tekster på 1–50 tegn",
            ),
            (
                r#"{"github": {"repo": "o/r", "writeBack": true}}"#,
                "writeBack skal være et objekt",
            ),
            // Review6c W5: a comma would be two labels for gh, a leading `-` reads like an option.
            (
                r#"{"github": {"repo": "o/r", "labels": ["ok", "bug, ui"]}}"#,
                "label «bug, ui» må ikke indeholde komma eller starte med «-»",
            ),
            (
                r#"{"github": {"repo": "o/r", "labels": ["-x"]}}"#,
                "label «-x» må ikke indeholde komma eller starte med «-»",
            ),
            (
                r#"{"github": {"repo": "o/r", "labels": [" --label=evil "]}}"#,
                "label «--label=evil» må ikke indeholde komma eller starte med «-»",
            ),
        ];
        for (text, reason) in cases {
            let f = parse_project_file(&format!(
                r#"{{"checks": [{{"name": "t", "run": "x"}}], {}"#,
                &text[1..]
            ))
            .unwrap_or_else(|e| panic!("{text}: the file is kept ({e})"));
            assert_eq!(f.github, None, "{text}");
            assert_eq!(f.checks.len(), 1, "{text}: the checks stay");
            assert_eq!(f.notes, vec![format!("{GITHUB_IGNORED_PREFIX}{reason}")]);
        }
        let eleven: Vec<String> = (0..11).map(|i| format!("\"l{i}\"")).collect();
        let f = parse_project_file(&format!(
            r#"{{"github": {{"repo": "o/r", "labels": [{}]}}}}"#,
            eleven.join(",")
        ))
        .unwrap();
        assert_eq!(f.github, None);
    }

    #[test]
    fn github_close_requires_bool() {
        for wb in [r#"{"close": "yes"}"#, r#"{"comment": 1}"#] {
            let f = parse_project_file(&format!(
                r#"{{"github": {{"repo": "o/r", "writeBack": {wb}}}}}"#
            ))
            .unwrap();
            assert_eq!(f.github, None, "{wb}");
            assert!(
                f.notes[0].starts_with(GITHUB_IGNORED_PREFIX),
                "{:?}",
                f.notes
            );
            assert!(f.notes[0].ends_with("skal være true eller false"));
        }
        let f = parse_project_file(r#"{"github": {"repo": "o/r", "writeBack": {"close": true}}}"#)
            .unwrap();
        assert_eq!(
            f.github.unwrap().write_back,
            WriteBackConfig {
                comment: false,
                close: true
            }
        );
    }

    #[test]
    fn watch_config_parses_with_file_order() {
        use crate::watch::config::PlaybookRule;
        let f = parse_project_file(
            r#"{"checks": [], "watch": {"enabled": true, "playbook": {"byLabel": {"zeta": "bug", "Alpha": "feature"}, "default": "task"}, "maxPerHour": 2, "maxPerDay": 5, "maxAgents": 1, "quietHours": "22-06"}}"#,
        )
        .unwrap();
        assert!(f.notes.is_empty(), "{:?}", f.notes);
        let w = f.watch.unwrap();
        assert!(w.enabled);
        assert_eq!(
            w.playbook,
            PlaybookRule::ByLabel {
                by_label: vec![
                    ("zeta".into(), "bug".into()),
                    ("Alpha".into(), "feature".into())
                ],
                default: None
            }
        );
        assert_eq!((w.max_per_hour, w.max_per_day, w.max_agents), (2, 5, 1));
        assert_eq!(
            (w.quiet, w.quiet_text.as_deref()),
            (Some((1320, 360)), Some("22-06"))
        );
        let f = parse_project_file(r#"{"watch": {"enabled": true, "playbook": "bug"}}"#).unwrap();
        assert_eq!(f.watch.unwrap().playbook, PlaybookRule::Fixed("bug".into()));
    }

    #[test]
    fn watch_invalid_is_note_not_error() {
        let f = parse_project_file(
            r#"{"gitBase": "main", "watch": {"enabled": "ja", "maxPerHour": 90, "quietHours": "07-07", "playbook": 5}}"#,
        )
        .unwrap();
        assert_eq!(f.git_base.as_deref(), Some("main"));
        assert_eq!(
            f.notes,
            [
                "project.json: watch.enabled skal være true/false; vagten er fra",
                "project.json: watch.playbook skal være et navn eller et objekt med byLabel/default; ingen playbook valgt",
                "project.json: watch.maxPerHour 90 er sat ned til 60 (1–60)",
                "project.json: watch.quietHours «07-07» ignoreres (formen HH-HH, fx 23-07)",
            ]
        );
        let w = f.watch.unwrap();
        assert!(!w.enabled);
        assert_eq!((w.max_per_hour, w.quiet), (60, None));
        let f = parse_project_file(r#"{"watch": "on", "github": {"repo": "o/r"}}"#).unwrap();
        assert_eq!(f.watch, None);
        assert!(f.github.is_some());
        assert_eq!(
            f.notes,
            ["project.json: watch ignoreres: skal være et objekt"]
        );
    }

    #[test]
    fn watch_missing_is_none() {
        for text in [r#"{}"#, r#"{"watch": null}"#, r#"{"checks": []}"#] {
            let f = parse_project_file(text).unwrap();
            assert_eq!((f.watch, f.notes.len()), (None, 0), "{text}");
        }
    }

    #[test]
    fn read_project_file_missing_is_none() {
        let d = TempDir::new();
        assert_eq!(read_project_file(&d.0), Ok(None));
        let r = ProjectFileReader::new();
        assert_eq!(r.read(&d.0), Ok(None));
        d.write(r#"{"checks": [{"name": "t", "run": "true"}]}"#);
        assert_eq!(
            read_project_file(&d.0).unwrap().unwrap().checks,
            vec![check("t", "true", 600)]
        );
        assert_eq!(r.read(&d.0).unwrap().unwrap().checks.len(), 1);
        // Invalid → Err (cached), then fixed (other length) → read again.
        d.write("{nej");
        assert!(r.read(&d.0).unwrap_err().starts_with("project.json: "));
        assert!(r.read(&d.0).is_err());
        d.write(r#"{"checks": [], "gitBase": "main"}"#);
        let f = r.read(&d.0).unwrap().unwrap();
        assert_eq!((f.checks.len(), f.git_base.as_deref()), (0, Some("main")));
        std::fs::remove_file(project_file_path(&d.0)).unwrap();
        assert_eq!(r.read(&d.0), Ok(None));
    }

    #[test]
    fn run_checks_runs_all_and_names_first_failure() {
        let fake = FakeChecks::new();
        fake.exit("lint", 1, "warning: x\nerror: y\n");
        fake.outcome(
            "e2e",
            RunOutcome::TimedOut {
                output: "venter…".into(),
                clipped: false,
                elapsed_ms: 5_000,
            },
        );
        let checks = [
            check("tests", "a", 600),
            check("lint", "b", 600),
            check("e2e", "c", 5),
            check("build", "d", 600),
        ];
        let cwd = Path::new("/p/wt");
        let r = run_checks(&fake, &checks, cwd);
        // Sequential, in file order, all of them, in the folder.
        assert_eq!(
            fake.calls(),
            ["tests", "lint", "e2e", "build"]
                .iter()
                .map(|n| (n.to_string(), cwd.to_path_buf()))
                .collect::<Vec<_>>()
        );
        assert_eq!(r.failed.as_deref(), Some("lint"));
        assert_eq!(
            r.lines.iter().map(|l| l.ok).collect::<Vec<_>>(),
            [true, false, false, true]
        );
        assert_eq!(r.first_failure().unwrap().reason.as_deref(), Some("exit 1"));
        assert_eq!(r.lines[2].reason.as_deref(), Some("timeout"));
        // No checks: nothing runs, nothing fails.
        let empty = run_checks(&fake, &[], cwd);
        assert_eq!(empty, ChecksReport::default());
    }

    #[test]
    fn render_checks_report_formats_ok_fail_timeout_and_clips() {
        let fake = FakeChecks::new();
        fake.outcome(
            "tests",
            RunOutcome::Exit {
                code: 0,
                output: "alt godt".into(),
                clipped: false,
                elapsed_ms: 12_400,
            },
        );
        fake.outcome(
            "lint",
            RunOutcome::Exit {
                code: 2,
                output: "error: x\n".into(),
                clipped: false,
                elapsed_ms: 3_600,
            },
        );
        fake.outcome(
            "e2e",
            RunOutcome::TimedOut {
                output: "hænger".into(),
                clipped: true,
                elapsed_ms: 5_000,
            },
        );
        fake.outcome(
            "x",
            RunOutcome::SpawnFailed("kunne ikke starte sh: nej".into()),
        );
        let r = run_checks(
            &fake,
            &[
                check("tests", "a", 600),
                check("lint", "b", 600),
                check("e2e", "c", 5),
                check("x", "d", 600),
            ],
            Path::new("/p"),
        );
        let (title, body) = render_checks_report(&r);
        assert_eq!(title, "Tjek: FEJL (lint)");
        assert_eq!(
            body,
            "Tjek: tests → OK (exit 0, 12 s)\n\
             Tjek: lint → FEJL (exit 2, 4 s)\n\
             Tjek: e2e → FEJL (timeout efter 5 s)\n\
             Tjek: x → FEJL (kunne ikke starte: kunne ikke starte sh: nej)\n\
             \n\
             --- lint (sidste 8 tegn) ---\n\
             error: x\n\
             \n\
             --- e2e (sidste 6 tegn) ---\n\
             hænger"
        );
        assert_eq!(
            gate_note(r.first_failure().unwrap(), Some("03")),
            "Tjek fejlede: lint (exit 2). Se rapport 03."
        );
        assert_eq!(
            gate_note(&r.lines[2], Some("04")),
            "Tjek fejlede: e2e (timeout). Se rapport 04."
        );
        assert_eq!(
            gate_note(&r.lines[3], None),
            "Tjek fejlede: x (kunne ikke starte)."
        );
        // All passed.
        let ok = run_checks(&FakeChecks::new(), &[check("t", "a", 1)], Path::new("/p"));
        assert_eq!(
            render_checks_report(&ok),
            (
                "Tjek: OK".to_string(),
                "Tjek: t → OK (exit 0, 1 s)".to_string()
            )
        );
        // Ten failed checks with full output tails: the body is cut to the report maximum.
        let big = FakeChecks::new();
        let checks: Vec<Check> = (0..10)
            .map(|i| {
                let name = format!("c{i}");
                big.exit(&name, 1, &"ø".repeat(CHECK_OUTPUT_MAX_CHARS));
                check(&name, "x", 1)
            })
            .collect();
        let (_, body) = render_checks_report(&run_checks(&big, &checks, Path::new("/p")));
        assert_eq!(body.chars().count(), REPORT_BODY_MAX_CHARS);
        assert!(body.starts_with("Tjek: c0 → FEJL (exit 1, 2 s)"));
        assert!(body.ends_with('…'));
        assert_eq!(
            unreadable_report("project.json: checks skal være en liste"),
            (
                "Tjek: project.json kunne ikke læses".to_string(),
                "project.json: checks skal være en liste".to_string()
            )
        );
    }

    /// A real run on the host OS: `echo` passes in the folder, `exit 3` fails with its code.
    #[test]
    fn process_checks_runs_shell_line_in_cwd() {
        let d = TempDir::new();
        let runner = ProcessChecks::new();
        #[cfg(unix)]
        let echo = check("echo", "echo mira-ok:$PWD", 30);
        #[cfg(windows)]
        let echo = check("echo", "echo mira-ok:%CD%", 30);
        match runner.run(&echo, &d.0) {
            RunOutcome::Exit { code, output, .. } => {
                assert_eq!(code, 0, "{output}");
                assert!(output.contains("mira-ok:"), "{output}");
                let dir = d.0.file_name().unwrap().to_string_lossy().into_owned();
                assert!(output.contains(&dir), "cwd: {output}");
            }
            other => panic!("{other:?}"),
        }
        let fail = check("fail", "exit 3", 30);
        assert!(matches!(
            runner.run(&fail, &d.0),
            RunOutcome::Exit { code: 3, .. }
        ));
        let r = run_checks(&runner, &[echo, fail], &d.0);
        assert_eq!(r.failed.as_deref(), Some("fail"));
        assert!(r.lines[1].text.starts_with("Tjek: fail → FEJL (exit 3, "));
    }

    #[cfg(unix)]
    #[test]
    fn process_checks_times_out() {
        let d = TempDir::new();
        let started = std::time::Instant::now();
        let out = ProcessChecks::new().run(&check("sov", "echo start; sleep 5", 1), &d.0);
        assert!(started.elapsed() < Duration::from_secs(4), "killed in time");
        match out {
            RunOutcome::TimedOut { output, .. } => assert!(output.contains("start"), "{output}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn process_checks_reports_spawn_failure_from_the_runner() {
        struct Broken;
        impl ProcRunner for Broken {
            fn run(
                &self,
                _: &proc::CommandSpec,
                _: &Path,
                _: &[(String, String)],
                _: Duration,
                _: usize,
            ) -> Result<proc::Captured, String> {
                Err("kunne ikke starte sh: findes ikke".into())
            }
        }
        let r = ProcessChecks::with(Arc::new(Broken)).run(&check("a", "x", 1), Path::new("."));
        assert_eq!(
            r,
            RunOutcome::SpawnFailed("kunne ikke starte sh: findes ikke".into())
        );
    }
}
