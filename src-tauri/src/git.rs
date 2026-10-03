//! git per ticket (step 6b, plan6b punkt 9, research6b §1.4/§2): app-managed worktrees
//! `<project>/.mira-bots/wt/<short id>` on branch `ticket/<short id>` — never `claude -w`.
//!
//! Every git call goes through [`GitRunner`] (`SystemGit` over [`crate::proc`] with
//! [`GIT_TIMEOUT_MS`]; `FakeGit` in tests). Arguments come only from the app: branch names only
//! from `^[0-9a-f]{8}$` short ids ([`branch_name`]), paths computed here, the base from the
//! workspace file (validated there) or from git's own output ([`valid_ref`]). No shell, no agent
//! text, no network (`fetch`/`push`/`ls-remote` are never run), no `--force`, no `branch -D`.
//! The user's `.gitignore` and `.git/info/exclude` are never touched; the app writes only
//! `.mira-bots/.gitignore` ([`crate::tickets::prompt::ensure_mira_gitignore`]) before the first
//! `worktree add`.
// TODO(windows-verify): `git: worktree` creates `<project>\.mira-bots\wt\<id>`, `git worktree
// list` shows `C:/…` paths and a rejected ticket reuses its worktree; a deep path without
// `core.longpaths` gives the note "git: …Filename too long…" and the delivery continues
// (plan6b D.99/D.106).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::config::{GIT_TIMEOUT_MS, WORKTREE_DIR};
use crate::proc::{CommandSpec, ProcRunner, SystemProc};

/// History note when no git executable was found (the delivery continues without git).
pub const GIT_NOT_FOUND_NOTE: &str = "git ikke fundet — git: off";
/// History note when the ticket's short id cannot be a branch name.
pub const INVALID_SHORT_NOTE: &str = "git: ugyldigt kort-id; ingen branch";
/// Output kept per git command (chars; the tail).
const GIT_OUTPUT_MAX_CHARS: usize = 200_000;
/// Timeout of the `git --version` probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest base ref accepted from git's output.
const REF_MAX_CHARS: usize = 200;
/// `diff --stat` in the «Ændringer» report is cut after this many chars.
const STAT_MAX_CHARS: usize = 15_000;
/// Commits listed in the «Ændringer» report.
const COMMITS_MAX: usize = 100;
/// Attempts (and the pause between them) of `worktree remove` (a virus scanner or indexer may
/// hold a new folder on Windows, research6b §8).
const REMOVE_ATTEMPTS: usize = 3;
const REMOVE_PAUSE: Duration = Duration::from_millis(200);

/// One git command's result.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitOut {
    /// The exit code (-1 when a signal ended git).
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl GitOut {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
}

/// Runs `git <args>` in `repo`. `Err` when git could not run at all (missing, timeout).
pub trait GitRunner: Send + Sync {
    fn run(&self, repo: &Path, args: &[&str]) -> Result<GitOut, String>;
}

/// The real git: [`find_git`] (or a given executable) through a [`ProcRunner`], timeout
/// [`GIT_TIMEOUT_MS`], children in [`crate::proc::registry`].
pub struct SystemGit {
    exe: Option<PathBuf>,
    proc: Arc<dyn ProcRunner>,
}

impl SystemGit {
    /// Looks git up on first use ([`find_git`], cached).
    pub fn new() -> Self {
        SystemGit {
            exe: None,
            proc: Arc::new(SystemProc {
                registry: Some(crate::proc::registry()),
            }),
        }
    }

    /// A given executable and runner (tests).
    pub fn with(exe: Option<PathBuf>, proc: Arc<dyn ProcRunner>) -> Self {
        SystemGit { exe, proc }
    }
}

impl Default for SystemGit {
    fn default() -> Self {
        Self::new()
    }
}

impl GitRunner for SystemGit {
    fn run(&self, repo: &Path, args: &[&str]) -> Result<GitOut, String> {
        let exe = match &self.exe {
            Some(e) => e.clone(),
            None => find_git().ok_or_else(|| GIT_NOT_FOUND_NOTE.to_string())?,
        };
        let spec = CommandSpec::new(exe, git_args(args, cfg!(windows)));
        let c = self.proc.run(
            &spec,
            repo,
            &git_env(),
            Duration::from_millis(GIT_TIMEOUT_MS),
            GIT_OUTPUT_MAX_CHARS,
        )?;
        if c.timed_out {
            return Err(format!(
                "git: timeout efter {} s (git {})",
                GIT_TIMEOUT_MS / 1000,
                args.first().copied().unwrap_or("")
            ));
        }
        Ok(GitOut {
            code: c.code.unwrap_or(-1),
            stdout: c.stdout,
            stderr: c.stderr,
        })
    }
}

/// The argument list: on Windows prefixed with `-c core.longpaths=true` (worktree paths under
/// `.mira-bots\wt\` easily pass 260 chars with `node_modules`; research6b §8).
pub fn git_args(args: &[&str], windows: bool) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len() + 2);
    if windows {
        out.push("-c".to_string());
        out.push("core.longpaths=true".to_string());
    }
    out.extend(args.iter().map(|a| a.to_string()));
    out
}

/// Env for git: the login PATH (Unix, [`crate::agent::process::spawn_env_extra`]) and never a
/// credential prompt.
fn git_env() -> Vec<(String, String)> {
    let mut env = crate::agent::process::spawn_env_extra();
    env.push(("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()));
    env
}

/// The git executable: PATH (the login PATH on Unix), then the usual install folders; each
/// candidate is probed with `git --version` (5 s). Cached for the process; `None` = git off.
pub fn find_git() -> Option<PathBuf> {
    static GIT: OnceLock<Option<PathBuf>> = OnceLock::new();
    GIT.get_or_init(|| {
        let path = crate::agent::login_env::path();
        let local = std::env::var_os("LOCALAPPDATA");
        let found = git_candidates(path.as_deref(), local.as_deref())
            .into_iter()
            .filter(|c| c.is_file())
            .find(|c| probe(c));
        match &found {
            Some(g) => log::info!("git: using {}", g.display()),
            None => log::warn!("git: not found; git per ticket is off"),
        }
        found
    })
    .clone()
}

fn probe(exe: &Path) -> bool {
    match crate::diagnostics::run_version_command(exe, &["--version"], PROBE_TIMEOUT) {
        Ok(v) if v.starts_with("git version") => true,
        Ok(v) => {
            log::warn!("git: {} answered «{v}»", exe.display());
            false
        }
        Err(e) => {
            log::warn!("git: probing {} failed: {e}", exe.display());
            false
        }
    }
}

/// Candidates in order, without duplicates: every PATH entry + `git`/`git.exe`, then Windows
/// `C:\Program Files\Git\cmd\git.exe`, `%LOCALAPPDATA%\Programs\Git\cmd\git.exe`,
/// `C:\Program Files (x86)\Git\cmd\git.exe`; Unix `/usr/bin/git` (macOS: the Xcode tools shim),
/// `/usr/local/bin/git`, `/opt/homebrew/bin/git`.
pub fn git_candidates(path: Option<&OsStr>, local_app_data: Option<&OsStr>) -> Vec<PathBuf> {
    let exe = if cfg!(windows) { "git.exe" } else { "git" };
    let mut out: Vec<PathBuf> = path
        .map(|p| {
            std::env::split_paths(p)
                .filter(|d| !d.as_os_str().is_empty())
                .map(|d| d.join(exe))
                .collect()
        })
        .unwrap_or_default();
    #[cfg(windows)]
    {
        out.push(PathBuf::from(r"C:\Program Files\Git\cmd\git.exe"));
        if let Some(l) = local_app_data.filter(|l| !l.is_empty()) {
            out.push(
                PathBuf::from(l)
                    .join("Programs")
                    .join("Git")
                    .join("cmd")
                    .join("git.exe"),
            );
        }
        out.push(PathBuf::from(r"C:\Program Files (x86)\Git\cmd\git.exe"));
    }
    #[cfg(unix)]
    {
        let _ = local_app_data;
        for p in [
            "/usr/bin/git",
            "/usr/local/bin/git",
            "/opt/homebrew/bin/git",
        ] {
            out.push(PathBuf::from(p));
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

/// `<dir>/.git` exists — a folder, or a file (`gitdir: …`) in a worktree or submodule.
pub fn is_git_repo(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// `ticket/<short>` when `short` is exactly 8 lowercase hex digits, else `None`.
pub fn branch_name(short: &str) -> Option<String> {
    let ok = short.len() == 8
        && short
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    ok.then(|| format!("ticket/{short}"))
}

/// `<project>/.mira-bots/wt/<short>`, joined component by component.
pub fn worktree_dir(project: &Path, short: &str) -> PathBuf {
    WORKTREE_DIR
        .split('/')
        .fold(project.to_path_buf(), |p, part| p.join(part))
        .join(short)
}

/// The project folder of an app worktree: `P` for `P/.mira-bots/wt/<short>` (the inverse of
/// [`worktree_dir`], `<short>` a valid [`branch_name`] id); `None` for any other folder. Step 6b:
/// an agent that sits in a ticket's worktree goes back to its project folder for a ticket
/// without one.
pub fn worktree_project(dir: &Path) -> Option<PathBuf> {
    let short = dir.file_name()?.to_str()?;
    branch_name(short)?;
    let mut project = dir.parent()?;
    for part in WORKTREE_DIR.rsplit('/') {
        if project.file_name()?.to_str()? != part {
            return None;
        }
        project = project.parent()?;
    }
    Some(project.to_path_buf())
}

/// A ref name the app may pass to git as an argument: 1–200 chars, no whitespace or control
/// characters, not starting with `-` (it would be an option).
pub fn valid_ref(s: &str) -> bool {
    !s.is_empty()
        && s.chars().count() <= REF_MAX_CHARS
        && !s.starts_with('-')
        && !s.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// git's error as "git: {line}": the first `fatal:`/`error:` line of stderr (git prints progress
/// such as "Preparing worktree …" before the error), else its first non-empty line (else stdout's,
/// else the exit code).
pub fn error_text(out: &GitOut) -> String {
    let lines = || {
        out.stderr
            .lines()
            .chain(out.stdout.lines())
            .map(str::trim)
            .filter(|l| !l.is_empty())
    };
    let line = lines()
        .find(|l| l.starts_with("fatal:") || l.starts_with("error:"))
        .or_else(|| lines().next())
        .map_or_else(|| format!("exit {}", out.code), str::to_string);
    format!("git: {line}")
}

/// Runs and requires exit 0 ([`error_text`] otherwise).
fn run_ok(git: &dyn GitRunner, dir: &Path, args: &[&str]) -> Result<GitOut, String> {
    let out = git.run(dir, args)?;
    if out.ok() {
        Ok(out)
    } else {
        Err(error_text(&out))
    }
}

/// The base of a new ticket branch (research6b §2): `configured` (project.json → workspace
/// `gitBase`) → `origin/HEAD` without `origin/` (`symbolic-ref`, no network) → the current branch
/// → the current commit (`rev-parse HEAD`; a detached HEAD) → `HEAD`.
pub fn resolve_base(git: &dyn GitRunner, repo: &Path, configured: Option<&str>) -> String {
    if let Some(c) = configured.map(str::trim).filter(|c| valid_ref(c)) {
        return c.to_string();
    }
    let first_line = |args: &[&str]| -> Option<String> {
        let out = git.run(repo, args).ok().filter(GitOut::ok)?;
        let line = out.stdout.lines().next()?.trim().to_string();
        valid_ref(&line).then_some(line)
    };
    if let Some(r) = first_line(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]) {
        let r = r.strip_prefix("origin/").unwrap_or(&r).to_string();
        if valid_ref(&r) {
            return r;
        }
    }
    first_line(&["branch", "--show-current"])
        .or_else(|| first_line(&["rev-parse", "--verify", "HEAD"]))
        .unwrap_or_else(|| "HEAD".to_string())
}

/// A path as text for comparisons: `\` → `/`, no trailing `/`, a drive letter lowercased.
pub fn normalize_path_str(s: &str) -> String {
    let mut p = s.replace('\\', "/");
    while p.len() > 1 && p.ends_with('/') {
        p.pop();
    }
    let b = p.as_bytes();
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        p = format!("{}{}", (b[0] as char).to_ascii_lowercase(), &p[1..]);
    }
    p
}

/// Whether two existing folders are the same: textually ([`same_path`]) or, when the text
/// differs, by canonical path (git reports worktrees canonically: `/private/var` for `/var` on
/// macOS, long names for 8.3 names on Windows).
pub fn same_folder(a: &Path, b: &Path) -> bool {
    if same_path(&a.to_string_lossy(), &b.to_string_lossy()) {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Whether `a` and `b` name the same folder textually ([`normalize_path_str`]; Windows also
/// ignores case).
pub fn same_path(a: &str, b: &str) -> bool {
    let (a, b) = (normalize_path_str(a), normalize_path_str(b));
    if cfg!(windows) {
        a.eq_ignore_ascii_case(&b)
    } else {
        a == b
    }
}

/// The folder of the worktree that has `branch` checked out, from `worktree list --porcelain`
/// (records separated by blank lines: `worktree <path>`, `HEAD …`, `branch refs/heads/<b>`,
/// `prunable …`). Prunable records are skipped.
pub fn parse_worktree_list(porcelain: &str, branch: &str) -> Option<PathBuf> {
    let want = format!("refs/heads/{branch}");
    let mut path: Option<&str> = None;
    let mut has_branch = false;
    let mut prunable = false;
    let mut found = None;
    for line in porcelain.lines().chain(std::iter::once("")) {
        if line.trim().is_empty() {
            if let (Some(p), true, false) = (path, has_branch, prunable) {
                found = Some(PathBuf::from(p));
                break;
            }
            (path, has_branch, prunable) = (None, false, false);
        } else if let Some(p) = line.strip_prefix("worktree ") {
            path = Some(p);
        } else if let Some(b) = line.strip_prefix("branch ") {
            has_branch = b.trim() == want;
        } else if line == "prunable" || line.starts_with("prunable ") {
            prunable = true;
        }
    }
    found
}

/// [`parse_worktree_list`] over `git worktree list --porcelain` in `repo`.
pub fn existing_worktree(
    git: &dyn GitRunner,
    repo: &Path,
    branch: &str,
) -> Result<Option<PathBuf>, String> {
    let out = run_ok(git, repo, &["worktree", "list", "--porcelain"])?;
    Ok(parse_worktree_list(&out.stdout, branch))
}

/// Path argument for git (UTF-8 only; the app's own paths).
fn path_arg(p: &Path) -> Result<&str, String> {
    p.to_str()
        .ok_or_else(|| format!("git: stien {} er ikke gyldig UTF-8", p.display()))
}

/// The ticket's worktree (research6b §1.4): `.mira-bots/.gitignore` first (else the new folder
/// makes the user's tree dirty), `worktree prune` (a manually deleted folder), an existing
/// worktree with the branch is reused, an existing branch is checked out without `-b`, else
/// `worktree add <dir> -b ticket/<short> <base>`. Errors are history notes ("git: …").
pub fn prepare_worktree(
    git: &dyn GitRunner,
    repo: &Path,
    short: &str,
    base: &str,
) -> Result<PathBuf, String> {
    let branch = branch_name(short).ok_or_else(|| INVALID_SHORT_NOTE.to_string())?;
    if !valid_ref(base) {
        return Err(format!("git: ugyldig base «{base}»; ingen branch"));
    }
    crate::tickets::prompt::ensure_mira_gitignore(repo)
        .map_err(|e| format!("git: kunne ikke skrive .mira-bots/.gitignore: {e}"))?;
    run_ok(git, repo, &["worktree", "prune"])?;
    let dir = worktree_dir(repo, short);
    if let Some(found) = existing_worktree(git, repo, &branch)? {
        // Git lists the canonical path; the ticket keeps the app's own spelling of the same
        // folder so later comparisons with the agent's cwd stay textual.
        return Ok(if same_folder(&found, &dir) {
            dir
        } else {
            found
        });
    }
    let dir_arg = path_arg(&dir)?;
    let listed = run_ok(git, repo, &["branch", "--list", &branch])?;
    if listed.stdout.trim().is_empty() {
        run_ok(
            git,
            repo,
            &["worktree", "add", dir_arg, "-b", &branch, base],
        )?;
    } else {
        run_ok(git, repo, &["worktree", "add", dir_arg, &branch])?;
    }
    Ok(dir)
}

/// Lines of `status --porcelain` that count as changes: app folders (`?? .mira-bots/`,
/// `?? .claude/`) and ignored entries (`!!`) do not.
pub fn count_changes(porcelain: &str) -> usize {
    porcelain
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|l| !l.starts_with("!!"))
        .filter(|l| {
            let path = l.get(3..).unwrap_or("").trim_matches('"');
            !(l.starts_with("??")
                && (path.starts_with(".mira-bots/") || path.starts_with(".claude/")))
        })
        .count()
}

/// The «Ændringer» report's data.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangeSummary {
    pub branch: String,
    pub base: String,
    /// `git diff --stat base...branch` (from the merge base).
    pub stat: String,
    /// `git log --oneline base..branch`, newest first.
    pub commits: Vec<String>,
    /// Changed files in the worktree that are not committed.
    pub uncommitted: usize,
}

/// Diff stat (`base...branch`), commits (`base..branch`) and, with a worktree, its uncommitted
/// files (research6b §2: three dots for the diff, two for the log).
pub fn change_summary(
    git: &dyn GitRunner,
    repo: &Path,
    wt: Option<&Path>,
    base: &str,
    branch: &str,
) -> Result<ChangeSummary, String> {
    if !valid_ref(base) || !valid_ref(branch) {
        return Err(format!("git: ugyldig base/branch «{base}»/«{branch}»"));
    }
    let three = format!("{base}...{branch}");
    let two = format!("{base}..{branch}");
    let stat = run_ok(git, repo, &["diff", "--stat", &three])?.stdout;
    let log = run_ok(git, repo, &["log", "--oneline", &two])?.stdout;
    let uncommitted = match wt {
        Some(dir) => count_changes(&run_ok(git, dir, &["status", "--porcelain"])?.stdout),
        None => 0,
    };
    Ok(ChangeSummary {
        branch: branch.to_string(),
        base: base.to_string(),
        stat: stat.trim_end().to_string(),
        commits: log
            .lines()
            .map(str::trim_end)
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string)
            .collect(),
        uncommitted,
    })
}

/// The «Ændringer» report text (plan6b C6b.4).
pub fn render_changes(s: &ChangeSummary) -> String {
    let commits = if s.commits.is_empty() {
        "(ingen)".to_string()
    } else {
        let mut lines: Vec<String> = s
            .commits
            .iter()
            .take(COMMITS_MAX)
            .map(|c| format!("- {c}"))
            .collect();
        if s.commits.len() > COMMITS_MAX {
            lines.push(format!("… og {} flere", s.commits.len() - COMMITS_MAX));
        }
        lines.join("\n")
    };
    let stat = if s.stat.trim().is_empty() {
        "(ingen)".to_string()
    } else if s.stat.chars().count() > STAT_MAX_CHARS {
        let head: String = s.stat.chars().take(STAT_MAX_CHARS).collect();
        format!("{head}\n… (klippet)")
    } else {
        s.stat.clone()
    };
    let suffix = if s.uncommitted > 0 {
        " (de indgår ikke i diffen)"
    } else {
        ""
    };
    format!(
        "Branch {} fra {}\n\n## Commits ({})\n{commits}\n\n## Ændrede filer\n{stat}\n\n## Ikke committet\n{} fil(er) i arbejdsmappen er ikke committet{suffix}",
        s.branch,
        s.base,
        s.commits.len(),
        s.uncommitted
    )
}

/// `git worktree remove <dir>` (never `--force`; the branch stays), up to three attempts 200 ms
/// apart. For `cleanupWorktreesOnDone` (6b-2).
pub fn remove_worktree(git: &dyn GitRunner, repo: &Path, dir: &Path) -> Result<(), String> {
    let dir_arg = path_arg(dir)?;
    let mut last = String::new();
    for attempt in 0..REMOVE_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(REMOVE_PAUSE);
        }
        match git.run(repo, &["worktree", "remove", dir_arg]) {
            Ok(out) if out.ok() => return Ok(()),
            Ok(out) => last = error_text(&out),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Scripted git for tests: the first reply whose argument prefix matches answers (default: exit
/// 0 without output); every call is logged with its folder.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::sync::Mutex;

    type Reply = Result<GitOut, String>;

    #[derive(Default)]
    pub(crate) struct FakeGit {
        replies: Mutex<Vec<(Vec<String>, Reply)>>,
        log: Mutex<Vec<(PathBuf, Vec<String>)>>,
    }

    impl FakeGit {
        pub(crate) fn new() -> Self {
            Self::default()
        }

        /// Answers commands starting with `prefix` with `code`/`stdout`/`stderr`.
        pub(crate) fn reply(&self, prefix: &[&str], code: i32, stdout: &str, stderr: &str) {
            self.replies.lock().unwrap().push((
                prefix.iter().map(|s| s.to_string()).collect(),
                Ok(GitOut {
                    code,
                    stdout: stdout.into(),
                    stderr: stderr.into(),
                }),
            ));
        }

        /// Commands starting with `prefix` fail to run (`Err`).
        pub(crate) fn fail(&self, prefix: &[&str], err: &str) {
            self.replies.lock().unwrap().push((
                prefix.iter().map(|s| s.to_string()).collect(),
                Err(err.into()),
            ));
        }

        /// The argument lists so far.
        pub(crate) fn calls(&self) -> Vec<Vec<String>> {
            self.log
                .lock()
                .unwrap()
                .iter()
                .map(|(_, a)| a.clone())
                .collect()
        }

        /// The calls with their folders.
        pub(crate) fn calls_in(&self) -> Vec<(PathBuf, Vec<String>)> {
            self.log.lock().unwrap().clone()
        }
    }

    impl GitRunner for FakeGit {
        fn run(&self, repo: &Path, args: &[&str]) -> Result<GitOut, String> {
            let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            self.log
                .lock()
                .unwrap()
                .push((repo.to_path_buf(), args.clone()));
            self.replies
                .lock()
                .unwrap()
                .iter()
                .find(|(p, _)| args.starts_with(p))
                .map_or_else(|| Ok(GitOut::default()), |(_, r)| r.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeGit;
    use super::*;
    use crate::config::MIRA_GITIGNORE;
    use std::fs;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mira-git-{tag}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn branch_name_requires_8_hex() {
        assert_eq!(branch_name("ab12cd34").as_deref(), Some("ticket/ab12cd34"));
        assert_eq!(branch_name("00000000").as_deref(), Some("ticket/00000000"));
        for bad in [
            "AB12CD34",
            "ab12cd3",
            "ab12cd345",
            "ab12cd3g",
            "ab-2cd34",
            "",
            "../../x",
            "ab12cd3é",
        ] {
            assert_eq!(branch_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn same_folder_sees_through_symlinks_and_spelling() {
        let tmp = std::env::temp_dir().join(format!("mira-sf-{}", std::process::id()));
        let real = tmp.join("real");
        fs::create_dir_all(&real).unwrap();
        // Same spelling: textual.
        assert!(same_folder(&real, &real));
        // Different spelling of the same folder (trailing separator, `.` component).
        assert!(same_folder(&real, &tmp.join("real").join(".")));
        // Different folders.
        assert!(!same_folder(&real, &tmp));
        // Missing folders fall back to the textual comparison only.
        assert!(!same_folder(&tmp.join("nope"), &tmp.join("nope2")));
        #[cfg(unix)]
        {
            let link = tmp.join("link");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            assert!(same_folder(&real, &link), "a symlink names the same folder");
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn worktree_dir_is_under_mira_bots_wt() {
        let p = worktree_dir(Path::new("/r/proj"), "ab12cd34");
        assert_eq!(
            p,
            Path::new("/r/proj")
                .join(".mira-bots")
                .join("wt")
                .join("ab12cd34")
        );
    }

    #[test]
    fn worktree_project_inverts_worktree_dir() {
        let proj = Path::new("/r/proj");
        let wt = worktree_dir(proj, "ab12cd34");
        assert_eq!(worktree_project(&wt), Some(proj.to_path_buf()));
        for other in [
            proj.to_path_buf(),
            proj.join(".mira-bots").join("wt"),
            proj.join(".mira-bots").join("wt").join("not-hex!"),
            proj.join(".mira-bots").join("xx").join("ab12cd34"),
            proj.join("other").join("wt").join("ab12cd34"),
            PathBuf::from("ab12cd34"),
        ] {
            assert_eq!(worktree_project(&other), None, "{}", other.display());
        }
    }

    #[test]
    fn valid_ref_refuses_options_and_whitespace() {
        for ok in ["main", "origin/release-1.2", "0123abcd", "HEAD"] {
            assert!(valid_ref(ok), "{ok}");
        }
        for bad in ["", "-x", "--force", "a b", "a\tb", "a\nb", &"x".repeat(201)] {
            assert!(!valid_ref(bad), "{bad}");
        }
    }

    #[test]
    fn git_args_prefix_longpaths_on_windows_only() {
        assert_eq!(git_args(&["status"], false), strs(&["status"]));
        assert_eq!(
            git_args(&["worktree", "add", "x"], true),
            strs(&["-c", "core.longpaths=true", "worktree", "add", "x"])
        );
    }

    #[test]
    fn git_candidates_start_with_path_and_have_no_duplicates() {
        let exe = if cfg!(windows) { "git.exe" } else { "git" };
        let a = std::env::temp_dir().join("a");
        let b = std::env::temp_dir().join("b");
        let path = std::env::join_paths([&a, &b, &a]).unwrap();
        let c = git_candidates(Some(&path), Some(OsStr::new("L")));
        assert_eq!(c[0], a.join(exe));
        assert_eq!(c[1], b.join(exe));
        let unique: std::collections::HashSet<_> = c.iter().collect();
        assert_eq!(unique.len(), c.len());
        #[cfg(unix)]
        assert!(c.contains(&PathBuf::from("/usr/bin/git")));
        #[cfg(windows)]
        assert!(c.contains(&PathBuf::from(r"L\Programs\Git\cmd\git.exe")));
        assert!(!git_candidates(None, None).is_empty());
    }

    #[test]
    fn is_git_repo_accepts_a_git_file() {
        let d = tmp("isrepo");
        assert!(!is_git_repo(&d));
        fs::write(d.join(".git"), "gitdir: /elsewhere/.git/worktrees/x\n").unwrap();
        assert!(is_git_repo(&d));
        fs::remove_file(d.join(".git")).unwrap();
        fs::create_dir(d.join(".git")).unwrap();
        assert!(is_git_repo(&d));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn resolve_base_order() {
        let repo = Path::new("/r");
        let g = FakeGit::new();
        g.reply(&["symbolic-ref"], 0, "origin/trunk\n", "");
        g.reply(&["branch", "--show-current"], 0, "feature\n", "");
        // Configured wins without any git call.
        assert_eq!(resolve_base(&g, repo, Some(" develop ")), "develop");
        assert!(g.calls().is_empty());
        // An invalid configured value is skipped.
        assert_eq!(resolve_base(&g, repo, Some("-x")), "trunk");
        assert_eq!(
            g.calls()[0],
            strs(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        );

        // No origin/HEAD (exit 128): the current branch.
        let g = FakeGit::new();
        g.reply(
            &["symbolic-ref"],
            128,
            "",
            "fatal: ref refs/remotes/origin/HEAD is not a symbolic ref",
        );
        g.reply(&["branch", "--show-current"], 0, "main\n", "");
        assert_eq!(resolve_base(&g, repo, None), "main");

        // Detached HEAD (empty current branch): the commit.
        let g = FakeGit::new();
        g.reply(&["symbolic-ref"], 128, "", "fatal");
        g.reply(&["branch", "--show-current"], 0, "\n", "");
        g.reply(&["rev-parse"], 0, "0123456789abcdef\n", "");
        assert_eq!(resolve_base(&g, repo, None), "0123456789abcdef");

        // Nothing works (git missing, empty repo): HEAD.
        let g = FakeGit::new();
        g.fail(&[], GIT_NOT_FOUND_NOTE);
        assert_eq!(resolve_base(&g, repo, None), "HEAD");
    }

    #[test]
    fn existing_worktree_parses_porcelain_with_windows_paths() {
        let porcelain = "worktree C:/Users/x/proj\nHEAD 1111\nbranch refs/heads/main\n\n\
                         worktree C:/Users/x/proj/.mira-bots/wt/ab12cd34\nHEAD 2222\nbranch refs/heads/ticket/ab12cd34\n\n\
                         worktree C:/Users/x/proj/.mira-bots/wt/00000000\nHEAD 3333\nbranch refs/heads/ticket/00000000\nprunable gitdir file points to non-existent location\n\n\
                         worktree /tmp/detached\nHEAD 4444\ndetached\n";
        assert_eq!(
            parse_worktree_list(porcelain, "ticket/ab12cd34"),
            Some(PathBuf::from("C:/Users/x/proj/.mira-bots/wt/ab12cd34"))
        );
        assert_eq!(parse_worktree_list(porcelain, "ticket/00000000"), None);
        assert_eq!(parse_worktree_list(porcelain, "ticket/ffffffff"), None);
        // A prefix of another branch does not match; the last record needs no blank line.
        assert_eq!(parse_worktree_list(porcelain, "ticket/ab12"), None);
        assert_eq!(
            parse_worktree_list("worktree /a\nbranch refs/heads/x", "x"),
            Some(PathBuf::from("/a"))
        );
        assert!(same_path(
            r"C:\Users\x\proj\.mira-bots\wt\ab12cd34\",
            "c:/Users/x/proj/.mira-bots/wt/ab12cd34"
        ));
        assert!(!same_path("/a/b", "/a/c"));
        assert_eq!(normalize_path_str("/"), "/");
    }

    #[test]
    fn prepare_worktree_sequences_prune_list_add_with_or_without_b() {
        let repo = tmp("seq");
        let dir = worktree_dir(&repo, "ab12cd34");
        let dir_s = dir.to_str().unwrap();

        // New branch: prune → list → branch --list (empty) → add -b.
        let g = FakeGit::new();
        assert_eq!(
            prepare_worktree(&g, &repo, "ab12cd34", "main").unwrap(),
            dir
        );
        assert_eq!(
            g.calls(),
            vec![
                strs(&["worktree", "prune"]),
                strs(&["worktree", "list", "--porcelain"]),
                strs(&["branch", "--list", "ticket/ab12cd34"]),
                strs(&["worktree", "add", dir_s, "-b", "ticket/ab12cd34", "main"]),
            ]
        );
        assert!(g.calls_in().iter().all(|(d, _)| d == &repo));
        // The app's .gitignore was written (before the add).
        assert_eq!(
            fs::read_to_string(repo.join(".mira-bots").join(".gitignore")).unwrap(),
            MIRA_GITIGNORE
        );

        // Existing branch: add without -b.
        let g = FakeGit::new();
        g.reply(&["branch", "--list"], 0, "  ticket/ab12cd34\n", "");
        prepare_worktree(&g, &repo, "ab12cd34", "main").unwrap();
        assert_eq!(
            g.calls().last().unwrap(),
            &strs(&["worktree", "add", dir_s, "ticket/ab12cd34"])
        );

        // Already checked out in a worktree: reused, nothing added.
        let g = FakeGit::new();
        g.reply(
            &["worktree", "list"],
            0,
            "worktree /elsewhere/wt\nHEAD 1\nbranch refs/heads/ticket/ab12cd34\n",
            "",
        );
        assert_eq!(
            prepare_worktree(&g, &repo, "ab12cd34", "main").unwrap(),
            PathBuf::from("/elsewhere/wt")
        );
        assert_eq!(g.calls().len(), 2);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn prepare_worktree_errors_are_notes() {
        let repo = tmp("err");
        let g = FakeGit::new();
        assert_eq!(
            prepare_worktree(&g, &repo, "ABCDEF01", "main").unwrap_err(),
            INVALID_SHORT_NOTE
        );
        assert!(g.calls().is_empty());
        assert_eq!(
            prepare_worktree(&g, &repo, "ab12cd34", "--evil").unwrap_err(),
            "git: ugyldig base «--evil»; ingen branch"
        );
        assert!(g.calls().is_empty());

        let g = FakeGit::new();
        g.reply(
            &["worktree", "add"],
            128,
            "",
            "Preparing worktree\nfatal: invalid reference: main\n",
        );
        // The fatal line, not git's progress line before it.
        assert_eq!(
            prepare_worktree(&g, &repo, "ab12cd34", "main").unwrap_err(),
            "git: fatal: invalid reference: main"
        );
        let g = FakeGit::new();
        g.reply(&["worktree", "add"], 1, "", "\nsomething odd\nmore\n");
        assert_eq!(
            prepare_worktree(&g, &repo, "ab12cd34", "main").unwrap_err(),
            "git: something odd"
        );
        let g = FakeGit::new();
        g.reply(
            &["worktree", "add"],
            128,
            "",
            "\nfatal: 'x' is a missing but locked worktree\n",
        );
        assert_eq!(
            prepare_worktree(&g, &repo, "ab12cd34", "main").unwrap_err(),
            "git: fatal: 'x' is a missing but locked worktree"
        );
        let g = FakeGit::new();
        g.fail(&[], GIT_NOT_FOUND_NOTE);
        assert_eq!(
            prepare_worktree(&g, &repo, "ab12cd34", "main").unwrap_err(),
            GIT_NOT_FOUND_NOTE
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn count_changes_ignores_mira_and_claude_dirs() {
        let p = " M src/a.rs\n?? .mira-bots/\n?? .claude/\n!! target/\n?? new.txt\nA  b.rs\n\n";
        assert_eq!(count_changes(p), 3);
        assert_eq!(count_changes(""), 0);
    }

    #[test]
    fn change_summary_uses_three_dots_for_diff_and_two_for_log() {
        let g = FakeGit::new();
        g.reply(
            &["diff"],
            0,
            " a.txt | 2 ++\n 1 file changed, 2 insertions(+)\n",
            "",
        );
        g.reply(&["log"], 0, "bbbb Second\naaaa First\n", "");
        g.reply(&["status"], 0, " M a.txt\n?? .mira-bots/\n", "");
        let wt = Path::new("/r/.mira-bots/wt/ab12cd34");
        let s = change_summary(&g, Path::new("/r"), Some(wt), "main", "ticket/ab12cd34").unwrap();
        assert_eq!(
            g.calls_in(),
            vec![
                (
                    PathBuf::from("/r"),
                    strs(&["diff", "--stat", "main...ticket/ab12cd34"])
                ),
                (
                    PathBuf::from("/r"),
                    strs(&["log", "--oneline", "main..ticket/ab12cd34"])
                ),
                (wt.to_path_buf(), strs(&["status", "--porcelain"])),
            ]
        );
        assert_eq!(s.commits, strs(&["bbbb Second", "aaaa First"]));
        assert_eq!(s.uncommitted, 1);
        assert!(s.stat.ends_with("2 insertions(+)"));
        // Without a worktree there is no status call.
        let g = FakeGit::new();
        let s = change_summary(&g, Path::new("/r"), None, "main", "ticket/ab12cd34").unwrap();
        assert_eq!((g.calls().len(), s.uncommitted), (2, 0));
        // A failing command is an error.
        let g = FakeGit::new();
        g.reply(
            &["diff"],
            128,
            "",
            "fatal: bad revision 'main...ticket/ab12cd34'",
        );
        assert_eq!(
            change_summary(&g, Path::new("/r"), None, "main", "ticket/ab12cd34").unwrap_err(),
            "git: fatal: bad revision 'main...ticket/ab12cd34'"
        );
    }

    #[test]
    fn render_changes_lists_stat_commits_and_uncommitted() {
        let s = ChangeSummary {
            branch: "ticket/ab12cd34".into(),
            base: "main".into(),
            stat: " a.txt | 2 ++\n 1 file changed, 2 insertions(+)".into(),
            commits: strs(&["bbbb Second", "aaaa First"]),
            uncommitted: 2,
        };
        assert_eq!(
            render_changes(&s),
            "Branch ticket/ab12cd34 fra main\n\n## Commits (2)\n- bbbb Second\n- aaaa First\n\n\
             ## Ændrede filer\n a.txt | 2 ++\n 1 file changed, 2 insertions(+)\n\n\
             ## Ikke committet\n2 fil(er) i arbejdsmappen er ikke committet (de indgår ikke i diffen)"
        );
        let empty = ChangeSummary {
            branch: "ticket/ab12cd34".into(),
            base: "main".into(),
            ..ChangeSummary::default()
        };
        assert_eq!(
            render_changes(&empty),
            "Branch ticket/ab12cd34 fra main\n\n## Commits (0)\n(ingen)\n\n## Ændrede filer\n(ingen)\n\n\
             ## Ikke committet\n0 fil(er) i arbejdsmappen er ikke committet"
        );
        let big = ChangeSummary {
            stat: "x".repeat(STAT_MAX_CHARS + 10),
            commits: (0..COMMITS_MAX + 3).map(|i| format!("c{i}")).collect(),
            ..empty
        };
        let text = render_changes(&big);
        assert!(text.contains("… og 3 flere") && text.contains("… (klippet)"));
        assert!(text.chars().count() < crate::config::REPORT_BODY_MAX_CHARS);
    }

    #[test]
    fn remove_worktree_retries_and_never_forces() {
        let g = FakeGit::new();
        g.reply(
            &["worktree", "remove"],
            128,
            "",
            "fatal: '/w' contains modified or untracked files, use --force to delete it",
        );
        let e = remove_worktree(&g, Path::new("/r"), Path::new("/w")).unwrap_err();
        assert!(e.starts_with("git: fatal: '/w' contains modified"), "{e}");
        assert_eq!(g.calls().len(), REMOVE_ATTEMPTS);
        assert!(g
            .calls()
            .iter()
            .all(|c| !c.contains(&"--force".to_string())));
        let g = FakeGit::new();
        remove_worktree(&g, Path::new("/r"), Path::new("/w")).unwrap();
        assert_eq!(g.calls(), vec![strs(&["worktree", "remove", "/w"])]);
    }

    #[test]
    fn system_git_goes_through_the_proc_runner() {
        use crate::proc::Captured;
        use std::sync::Mutex;

        #[derive(Default)]
        struct Rec(Mutex<Vec<crate::proc::ProcCall>>);
        impl ProcRunner for Rec {
            fn run(
                &self,
                spec: &CommandSpec,
                cwd: &Path,
                env: &[(String, String)],
                timeout: Duration,
                _tail: usize,
            ) -> Result<Captured, String> {
                self.0
                    .lock()
                    .unwrap()
                    .push((spec.clone(), cwd.into(), env.to_vec(), timeout));
                let timed_out = spec.args.iter().any(|a| a == "slow");
                Ok(Captured {
                    code: (!timed_out).then_some(0),
                    timed_out,
                    stdout: "out".into(),
                    ..Captured::default()
                })
            }
        }
        let rec = Arc::new(Rec::default());
        let git = SystemGit::with(Some(PathBuf::from("/usr/bin/git")), rec.clone());
        let out = git
            .run(Path::new("/repo"), &["status", "--porcelain"])
            .unwrap();
        assert_eq!((out.code, out.stdout.as_str()), (0, "out"));
        let e = git.run(Path::new("/repo"), &["slow"]).unwrap_err();
        assert_eq!(e, "git: timeout efter 60 s (git slow)");
        let calls = rec.0.lock().unwrap();
        let (spec, cwd, env, timeout) = &calls[0];
        assert_eq!(spec.program, PathBuf::from("/usr/bin/git"));
        assert_eq!(spec.raw_arg, None);
        let args: Vec<String> = spec
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args, git_args(&["status", "--porcelain"], cfg!(windows)));
        assert_eq!(cwd, &PathBuf::from("/repo"));
        assert_eq!(*timeout, Duration::from_millis(GIT_TIMEOUT_MS));
        assert!(env.contains(&("GIT_TERMINAL_PROMPT".to_string(), "0".to_string())));
    }

    // ---- against a real repository (skipped when git is missing) ----

    /// `git <args>` in `dir` for the test setup (no signing, fixed identity), must succeed.
    fn sh_git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new(find_git().unwrap())
            .args([
                "-c",
                "user.name=mira",
                "-c",
                "user.email=mira@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn real_repo() -> Option<PathBuf> {
        find_git()?;
        let repo = tmp("real");
        sh_git(&repo, &["init", "-q"]);
        fs::write(repo.join("readme.txt"), "hej\n").unwrap();
        sh_git(&repo, &["add", "readme.txt"]);
        sh_git(&repo, &["commit", "-q", "-m", "init"]);
        Some(repo)
    }

    #[test]
    fn real_git_base_without_remote_and_with_origin_head() {
        let Some(repo) = real_repo() else {
            eprintln!("git not found; skipped");
            return;
        };
        let git = SystemGit::new();
        assert!(is_git_repo(&repo));
        assert_eq!(resolve_base(&git, &repo, None), "main");
        assert_eq!(resolve_base(&git, &repo, Some("develop")), "develop");
        // origin/HEAD set locally (no network): wins over the current branch.
        sh_git(
            &repo,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/trunk",
            ],
        );
        assert_eq!(resolve_base(&git, &repo, None), "trunk");
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn real_git_worktree_roundtrip() {
        let Some(repo) = real_repo() else {
            eprintln!("git not found; skipped");
            return;
        };
        let git = SystemGit::new();
        // An old app's .gitignore is upgraded before the first worktree add.
        fs::create_dir_all(repo.join(".mira-bots")).unwrap();
        fs::write(repo.join(".mira-bots").join(".gitignore"), "*\n").unwrap();

        let wt = prepare_worktree(&git, &repo, "ab12cd34", "main").unwrap();
        assert_eq!(wt, worktree_dir(&repo, "ab12cd34"));
        assert!(wt.join("readme.txt").is_file());
        assert!(wt.join(".git").is_file(), "a worktree's .git is a file");
        assert!(is_git_repo(&wt));
        assert_eq!(
            fs::read_to_string(repo.join(".mira-bots").join(".gitignore")).unwrap(),
            MIRA_GITIGNORE
        );
        // The user's tree stays clean (the worktree folder is ignored).
        assert_eq!(sh_git(&repo, &["status", "--porcelain"]), "");
        assert!(!repo.join(".gitignore").exists());
        assert_eq!(
            sh_git(&wt, &["branch", "--show-current"]).trim(),
            "ticket/ab12cd34"
        );
        // Again: the checked-out worktree is reused.
        assert!(same_path(
            &prepare_worktree(&git, &repo, "ab12cd34", "main")
                .unwrap()
                .to_string_lossy(),
            &wt.to_string_lossy()
        ));

        // A commit on the ticket branch, one on main after branching, one uncommitted file.
        fs::write(wt.join("a.txt"), "a\n").unwrap();
        sh_git(&wt, &["add", "a.txt"]);
        sh_git(&wt, &["commit", "-q", "-m", "Tilføj a"]);
        fs::write(repo.join("main.txt"), "m\n").unwrap();
        sh_git(&repo, &["add", "main.txt"]);
        sh_git(&repo, &["commit", "-q", "-m", "main videre"]);
        fs::write(wt.join("b.txt"), "b\n").unwrap();
        let s = change_summary(&git, &repo, Some(&wt), "main", "ticket/ab12cd34").unwrap();
        assert_eq!(s.commits.len(), 1);
        assert!(s.commits[0].ends_with(" Tilføj a"), "{:?}", s.commits);
        assert!(
            s.stat.contains("a.txt") && !s.stat.contains("main.txt"),
            "{}",
            s.stat
        );
        assert_eq!(s.uncommitted, 1);

        // remove refuses untracked files (no --force); after cleaning it works, the branch stays.
        assert!(remove_worktree(&git, &repo, &wt).is_err());
        fs::remove_file(wt.join("b.txt")).unwrap();
        remove_worktree(&git, &repo, &wt).unwrap();
        assert!(!wt.exists());
        assert!(sh_git(&repo, &["branch", "--list", "ticket/ab12cd34"]).contains("ticket/ab12cd34"));

        // The existing branch is checked out again without -b (its commit is there).
        let wt2 = prepare_worktree(&git, &repo, "ab12cd34", "main").unwrap();
        assert!(wt2.join("a.txt").is_file());

        // A manually deleted folder: prune makes the next add work.
        fs::remove_dir_all(&wt2).unwrap();
        let wt3 = prepare_worktree(&git, &repo, "ab12cd34", "main").unwrap();
        assert!(wt3.join("a.txt").is_file());

        // An invalid short id never reaches git.
        assert_eq!(
            prepare_worktree(&git, &repo, "AB12CD34", "main").unwrap_err(),
            INVALID_SHORT_NOTE
        );
        // A base that does not exist: a note with git's first stderr line.
        let e = prepare_worktree(&git, &repo, "00000001", "nope").unwrap_err();
        assert!(e.starts_with("git: "), "{e}");
        let _ = fs::remove_dir_all(&repo);
    }
}
