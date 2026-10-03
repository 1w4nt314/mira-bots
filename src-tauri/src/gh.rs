//! GitHub CLI (`gh`) for the inbox (step 6c, plan6c A.4, punkt 12, C6c.1/C6c.3).
//!
//! Every call goes through [`GhRunner`] (`SystemGh` over [`crate::proc`]; `FakeGh` in tests).
//! Arguments are separate `OsString`s from the app only (never a shell, never `raw_arg`): a repo
//! that passed [`valid_repo`], an issue number `u64 > 0`, fixed flags. Free text (the comment)
//! only travels through `--body-file <app_data>/tmp/wb-<short>-<ms>.md`, which is always the last
//! argument. The environment disables prompts, update notices, telemetry, colours, the pager and
//! a forced TTY ([`gh_env`]); `cwd` is the app data folder (never a repo: `--repo` is explicit).
//! A clipped answer is an error (half a JSON list is never parsed).
//!
//! [`find_gh`] mirrors `find_git`, but a miss is only cached for [`GH_NONE_TTL`]: `gh` installed
//! while the app runs is found within a minute. The app never reads or stores a token: no
//! `GH_TOKEN`/`GH_CONFIG_DIR` is set, `gh auth status` never gets `--show-token`, and its lines
//! containing `Token:` are dropped ([`auth_text`]).
// TODO(windows-verify): `winget install GitHub.cli` after the app started: Diagnostik finds
// `%ProgramFiles%\GitHub CLI\gh.exe` within 60 s; "Tjek gh-login" shows the account without a
// token (plan6c D.108).

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::config::{GH_AUTH_TIMEOUT_MS, GH_OUTPUT_MAX_CHARS, GH_TIMEOUT_MS};
use crate::diagnostics::VersionProbe;
use crate::proc::{Captured, CommandSpec, ProcRunner, SystemProc};

/// A miss of [`find_gh`] is looked up again after this long.
pub const GH_NONE_TTL: Duration = Duration::from_secs(60);
/// Timeout of `gh --version` (probe and Diagnostik).
pub const GH_VERSION_TIMEOUT: Duration = Duration::from_secs(5);
/// Older versions are not tried (Diagnostik note; not enforced).
pub const GH_MIN_TESTED: (u32, u32, u32) = (2, 40, 0);
/// Diagnostik note for an older `gh`.
pub const GH_OLD_NOTE: &str = "ældre end 2.40.0 — ikke afprøvet";
/// Longest repo (`owner/name`).
pub const REPO_MAX_CHARS: usize = 100;
/// Longest `Other` text taken from gh's stderr.
const OTHER_MAX_CHARS: usize = 300;
/// Longest text of [`auth_text`].
pub const AUTH_TEXT_MAX_CHARS: usize = 1_000;

/// The state of the `gh --version` probe (same shape as the claude probe).
pub type GhProbe = VersionProbe;

/// One `gh` call: the arguments (from the app only), an optional body file (appended as
/// `--body-file <path>`, last) and the timeout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GhCall {
    pub args: Vec<String>,
    pub body_file: Option<PathBuf>,
    pub timeout: Duration,
}

impl GhCall {
    /// `gh <args>` with the normal timeout ([`GH_TIMEOUT_MS`]).
    pub fn new<I, S>(args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        GhCall {
            args: args.into_iter().map(Into::into).collect(),
            body_file: None,
            timeout: Duration::from_millis(GH_TIMEOUT_MS),
        }
    }

    /// The full argument list as it reaches gh (`--body-file <path>` last).
    pub fn argv(&self) -> Vec<OsString> {
        let mut v: Vec<OsString> = self.args.iter().map(OsString::from).collect();
        if let Some(f) = &self.body_file {
            v.push("--body-file".into());
            v.push(f.as_os_str().to_owned());
        }
        v
    }
}

/// A successful call's output.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GhOut {
    pub stdout: String,
    pub stderr: String,
}

/// Why a call failed (research6c §1.3; the Danish texts are [`error_text`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GhError {
    GhMissing,
    Timeout,
    NotLoggedIn,
    BadCredentials,
    RepoNotFound,
    RateLimited,
    IssuesDisabled,
    Network,
    TooLarge,
    BadJson(String),
    Other(String),
}

/// Runs `gh`. `exec` gives the raw result (`Err` only when gh could not start: [`GhError::GhMissing`]);
/// `run` classifies it ([`classify`]).
pub trait GhRunner: Send + Sync {
    fn exec(&self, call: &GhCall) -> Result<Captured, GhError>;

    fn run(&self, call: &GhCall) -> Result<GhOut, GhError> {
        classify(&self.exec(call)?)
    }
}

/// The real gh: [`find_gh`] (or a given executable) through a [`ProcRunner`], children in
/// [`crate::proc::registry`], `cwd` the app data folder.
pub struct SystemGh {
    exe: Option<PathBuf>,
    proc: Arc<dyn ProcRunner>,
    cwd: PathBuf,
}

impl SystemGh {
    /// Looks gh up on each call ([`find_gh`], cached).
    pub fn new(cwd: PathBuf) -> Self {
        SystemGh {
            exe: None,
            proc: Arc::new(SystemProc {
                registry: Some(crate::proc::registry()),
            }),
            cwd,
        }
    }

    /// A given executable and runner (tests).
    pub fn with(exe: Option<PathBuf>, proc: Arc<dyn ProcRunner>, cwd: PathBuf) -> Self {
        SystemGh { exe, proc, cwd }
    }
}

impl GhRunner for SystemGh {
    fn exec(&self, call: &GhCall) -> Result<Captured, GhError> {
        let exe = match &self.exe {
            Some(e) => e.clone(),
            None => find_gh().ok_or(GhError::GhMissing)?,
        };
        let spec = CommandSpec::new(exe, call.argv());
        self.proc
            .run(
                &spec,
                &self.cwd,
                &gh_env(),
                call.timeout,
                GH_OUTPUT_MAX_CHARS,
            )
            .map_err(|e| {
                log::warn!("gh: {e}");
                GhError::GhMissing
            })
    }
}

/// The environment of every gh call (C6c.3): the login PATH (Unix) and no prompt, no update
/// notices, no telemetry, no spinner, no colours, no pager, no forced TTY (an empty
/// `GH_FORCE_TTY` switches a user's global one off).
pub fn gh_env() -> Vec<(String, String)> {
    let mut env = crate::agent::process::spawn_env_extra();
    for (k, v) in [
        ("GH_PROMPT_DISABLED", "1"),
        ("GH_NO_UPDATE_NOTIFIER", "1"),
        ("GH_NO_EXTENSION_UPDATE_NOTIFIER", "1"),
        ("GH_TELEMETRY", "false"),
        ("DO_NOT_TRACK", "1"),
        ("GH_FORCE_TTY", ""),
        ("GH_SPINNER_DISABLED", "1"),
        ("NO_COLOR", "1"),
        ("GH_PAGER", "cat"),
    ] {
        env.push((k.to_string(), v.to_string()));
    }
    env
}

/// The last `max` chars of `s`.
fn tail(s: &str, max: usize) -> String {
    let n = s.chars().count();
    s.chars().skip(n.saturating_sub(max)).collect()
}

/// Classifies a finished call (C6c.3, research §1.3, in this order): timeout; exit 4 (not
/// logged in); then, for a failed call only — a successful `close` prints the issue title on
/// stderr, which must never be read as an error — `HTTP 401`, `Could not resolve to a
/// Repository`, `rate limit`/`HTTP 429`, `has disabled issues`, a `Post "…`/`Get "…` line without
/// `HTTP ` (offline); then a clipped answer (never parsed); then any other failure (the last
/// 300 chars of stderr on one line); else `Ok`.
pub fn classify(c: &Captured) -> Result<GhOut, GhError> {
    if c.timed_out {
        return Err(GhError::Timeout);
    }
    if c.code == Some(4) {
        return Err(GhError::NotLoggedIn);
    }
    let failed = c.code != Some(0);
    if failed {
        let err = c.stderr.as_str();
        if err.contains("HTTP 401") {
            return Err(GhError::BadCredentials);
        }
        if err.contains("Could not resolve to a Repository") {
            return Err(GhError::RepoNotFound);
        }
        if err.to_lowercase().contains("rate limit") || err.contains("HTTP 429") {
            return Err(GhError::RateLimited);
        }
        if err.contains("has disabled issues") {
            return Err(GhError::IssuesDisabled);
        }
        let first = err.trim_start();
        if (first.starts_with("Post \"") || first.starts_with("Get \"")) && !err.contains("HTTP ") {
            return Err(GhError::Network);
        }
    }
    if c.clipped {
        return Err(GhError::TooLarge);
    }
    if failed {
        let text = crate::tickets::prompt::one_line(&c.stderr);
        let text = if text.is_empty() {
            match c.code {
                Some(code) => format!("exit {code}"),
                None => "afbrudt".to_string(),
            }
        } else {
            tail(&text, OTHER_MAX_CHARS)
        };
        return Err(GhError::Other(text));
    }
    Ok(GhOut {
        stdout: c.stdout.clone(),
        stderr: c.stderr.clone(),
    })
}

/// The Danish text of an error (C6c.5); `repo` names the repo in RepoNotFound/IssuesDisabled.
/// RateLimited without the clock time (a source adds "prøver igen kl. hh:mm",
/// [`crate::config::rate_limited_note`]).
pub fn error_text(e: &GhError, repo: &str) -> String {
    match e {
        GhError::GhMissing => {
            "gh ikke fundet — Indbakke fra GitHub er slået fra (mappe-kilden virker)".into()
        }
        GhError::NotLoggedIn => "gh er ikke logget ind — kør gh auth login i en terminal".into(),
        GhError::BadCredentials => "GitHub afviste login (401) — kør gh auth login igen".into(),
        GhError::RepoNotFound => {
            format!("repoet «{repo}» findes ikke, eller gh har ikke adgang")
        }
        GhError::RateLimited => "GitHub: rate limit".into(),
        GhError::IssuesDisabled => format!("issues er slået fra i «{repo}»"),
        GhError::Network => "ingen forbindelse til GitHub".into(),
        GhError::Timeout => "gh svarede ikke inden for 30 s".into(),
        GhError::TooLarge => "svaret fra gh var for stort".into(),
        GhError::BadJson(_) => "svaret fra gh kunne ikke læses".into(),
        GhError::Other(t) => format!("gh: {t}"),
    }
}

/// `owner/name` (C6c.3): two components of `[A-Za-z0-9_.-]`, at most [`REPO_MAX_CHARS`], no `.`
/// or `..` component and none starting with `-` (it could be read as a flag).
pub fn valid_repo(s: &str) -> bool {
    if s.is_empty() || s.chars().count() > REPO_MAX_CHARS {
        return false;
    }
    let parts: Vec<&str> = s.split('/').collect();
    parts.len() == 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && *p != "."
                && *p != ".."
                && !p.starts_with('-')
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        })
}

/// `url` is exactly `https://github.com/<repo>/issues/<number>` (the repo compared without case:
/// GitHub answers with its own spelling). Only such URLs are stored and opened.
pub fn issue_url_ok(url: &str, repo: &str, number: u64) -> bool {
    let Some(rest) = url.strip_prefix("https://github.com/") else {
        return false;
    };
    let want = format!("{repo}/issues/{number}");
    number > 0
        && valid_repo(repo)
        && rest.eq_ignore_ascii_case(&want)
        && rest.ends_with(&format!("/issues/{number}"))
}

/// `gh version 2.102.0 (2026-09-30)` → `(2, 102, 0)`.
pub fn parse_gh_version(first_line: &str) -> Option<(u32, u32, u32)> {
    let rest = first_line.trim().strip_prefix("gh version ")?;
    let token = rest.split_whitespace().next()?;
    let token = token.strip_prefix('v').unwrap_or(token);
    let mut parts = token.splitn(3, '.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let digits: String = parts
        .next()?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    Some((major, minor, digits.parse().ok()?))
}

/// `(ghVersion, ghVersionNote)` for Diagnostik: the version number (`"2.102.0"`), and a note
/// "ikke fundet" | "kører stadig" | the probe's error | [`GH_OLD_NOTE`].
pub fn version_fields(probe: &GhProbe) -> (Option<String>, Option<String>) {
    match probe {
        VersionProbe::Pending => (None, Some("kører stadig".into())),
        VersionProbe::NotFound => (None, Some("ikke fundet".into())),
        VersionProbe::Failed(e) => (None, Some(e.clone())),
        VersionProbe::Ok(line) => match parse_gh_version(line) {
            Some(v) => (
                Some(format!("{}.{}.{}", v.0, v.1, v.2)),
                (v < GH_MIN_TESTED).then(|| GH_OLD_NOTE.to_string()),
            ),
            None => (
                Some(line.clone()),
                Some("kunne ikke læse versionsnummeret".into()),
            ),
        },
    }
}

/// The text of `gh auth status` for the "Tjek gh-login" button: stdout then stderr, without
/// every line that mentions a token, at most [`AUTH_TEXT_MAX_CHARS`].
pub fn auth_text(c: &Captured) -> String {
    let lines: Vec<&str> = c
        .stdout
        .lines()
        .chain(c.stderr.lines())
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty())
        .filter(|l| !l.to_lowercase().contains("token:"))
        .collect();
    lines.join("\n").chars().take(AUTH_TEXT_MAX_CHARS).collect()
}

/// `check_gh_auth` (C6c.2 `GhAuthResult`).
#[derive(serde::Serialize, Clone, Debug, PartialEq, Eq)]
pub struct GhAuthResult {
    pub ok: bool,
    pub text: String,
}

/// `gh auth status --hostname github.com` (15 s; never `--show-token`), only on a click.
pub fn check_auth(gh: &dyn GhRunner) -> GhAuthResult {
    let mut call = GhCall::new(["auth", "status", "--hostname", "github.com"]);
    call.timeout = Duration::from_millis(GH_AUTH_TIMEOUT_MS);
    match gh.exec(&call) {
        Ok(c) if c.timed_out => GhAuthResult {
            ok: false,
            text: format!("gh svarede ikke inden for {} s", GH_AUTH_TIMEOUT_MS / 1000),
        },
        Ok(c) => GhAuthResult {
            ok: c.success(),
            text: auth_text(&c),
        },
        Err(e) => GhAuthResult {
            ok: false,
            text: error_text(&e, ""),
        },
    }
}

// ---- finding gh ----

type GhCache = Mutex<Option<(Instant, Option<PathBuf>)>>;

fn cache() -> &'static GhCache {
    static CACHE: GhCache = Mutex::new(None);
    &CACHE
}

fn lock_cache(c: &GhCache) -> MutexGuard<'_, Option<(Instant, Option<PathBuf>)>> {
    c.lock().unwrap_or_else(|p| p.into_inner())
}

/// Whether the cached entry must be looked up again: none yet, or a miss older than `ttl`.
fn stale(entry: &Option<(Instant, Option<PathBuf>)>, now: Instant, ttl: Duration) -> bool {
    match entry {
        None => true,
        Some((_, Some(_))) => false,
        Some((at, None)) => now.saturating_duration_since(*at) >= ttl,
    }
}

/// [`find_gh`] with an explicit cache, clock and lookup (tests). A hit is kept for the process,
/// a miss for `ttl`. The lookup runs under the cache lock (two callers never probe twice).
pub(crate) fn cached_lookup(
    cache: &GhCache,
    now: Instant,
    ttl: Duration,
    lookup: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    let mut entry = lock_cache(cache);
    if stale(&entry, now, ttl) {
        let found = lookup();
        *entry = Some((now, found));
    }
    entry.as_ref().and_then(|(_, p)| p.clone())
}

/// The gh executable: PATH (the login PATH on Unix), then the usual install folders; each
/// candidate is probed with `gh --version` (5 s, `gh version …`). A hit is cached for the
/// process, a miss for [`GH_NONE_TTL`]. Blocking (never call it under a lock of the app).
pub fn find_gh() -> Option<PathBuf> {
    cached_lookup(cache(), Instant::now(), GH_NONE_TTL, || {
        let path = crate::agent::login_env::path();
        let found = gh_candidates(
            path.as_deref(),
            std::env::var_os("ProgramFiles").as_deref(),
            std::env::var_os("ProgramFiles(x86)").as_deref(),
            std::env::var_os("LOCALAPPDATA").as_deref(),
        )
        .into_iter()
        .filter(|c| c.is_file())
        .find(|c| match probe_version(c) {
            Ok(_) => true,
            Err(e) => {
                log::warn!("gh: probing {} failed: {e}", c.display());
                false
            }
        });
        match &found {
            Some(g) => log::info!("gh: using {}", g.display()),
            None => log::info!("gh: not found; the GitHub inbox is off"),
        }
        found
    })
}

/// The cached gh path without a lookup (Diagnostik; never blocks on a probe).
pub fn known_gh() -> Option<PathBuf> {
    lock_cache(cache()).as_ref().and_then(|(_, p)| p.clone())
}

/// Whether [`find_gh`] would look again now (no lookup yet, or an expired miss).
pub fn gh_lookup_due() -> bool {
    stale(&lock_cache(cache()), Instant::now(), GH_NONE_TTL)
}

/// `gh --version` (5 s, the gh environment): the first line when it starts with `gh version`.
pub fn probe_version(exe: &Path) -> Result<String, String> {
    let spec = CommandSpec::new(exe, ["--version"]);
    let c = crate::proc::run_capture(
        &spec,
        &std::env::temp_dir(),
        &gh_env(),
        GH_VERSION_TIMEOUT,
        10_000,
        Some(crate::proc::registry()),
    )?;
    if c.timed_out {
        return Err(format!("timeout efter {} s", GH_VERSION_TIMEOUT.as_secs()));
    }
    let first = c
        .stdout
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .to_string();
    if !c.success() {
        let code = c.code.map_or_else(|| "?".into(), |c| c.to_string());
        return Err(format!("exit {code}: {}", c.stderr.trim()));
    }
    if first.starts_with("gh version") {
        Ok(first)
    } else {
        Err(format!("uventet svar «{first}»"))
    }
}

/// Runs the version probe once on the `gh-version` thread (lookup included) and stores the
/// result in `slot` (startup, and Diagnostik after an expired miss). Only one at a time.
pub fn start_gh_probe(slot: Arc<Mutex<GhProbe>>) {
    static RUNNING: AtomicBool = AtomicBool::new(false);
    if RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    *slot.lock().unwrap_or_else(|p| p.into_inner()) = VersionProbe::Pending;
    let spawned = std::thread::Builder::new()
        .name("gh-version".into())
        .spawn(move || {
            let result = match find_gh() {
                None => VersionProbe::NotFound,
                Some(exe) => match probe_version(&exe) {
                    Ok(v) => {
                        log::info!("gh --version: {v}");
                        VersionProbe::Ok(v)
                    }
                    Err(e) => VersionProbe::Failed(e),
                },
            };
            *slot.lock().unwrap_or_else(|p| p.into_inner()) = result;
            RUNNING.store(false, Ordering::Release);
        });
    if let Err(e) = spawned {
        log::warn!("could not start the gh version probe: {e}");
        RUNNING.store(false, Ordering::Release);
    }
}

/// Candidates in order, without duplicates (research6c §1.7; never `.cmd`/`.ps1`): every PATH
/// entry + `gh`/`gh.exe`, then Windows `%ProgramFiles%\GitHub CLI\gh.exe` (else `C:\Program
/// Files\…`), `%ProgramFiles(x86)%\GitHub CLI\gh.exe`, `%LOCALAPPDATA%\Programs\GitHub CLI\gh.exe`;
/// Unix `/opt/homebrew/bin/gh`, `/usr/local/bin/gh`, `/usr/bin/gh`.
pub fn gh_candidates(
    path: Option<&OsStr>,
    program_files: Option<&OsStr>,
    program_files_x86: Option<&OsStr>,
    local_app_data: Option<&OsStr>,
) -> Vec<PathBuf> {
    candidates_for(
        cfg!(windows),
        path,
        program_files,
        program_files_x86,
        local_app_data,
    )
}

fn candidates_for(
    windows: bool,
    path: Option<&OsStr>,
    program_files: Option<&OsStr>,
    program_files_x86: Option<&OsStr>,
    local_app_data: Option<&OsStr>,
) -> Vec<PathBuf> {
    let exe = if windows { "gh.exe" } else { "gh" };
    let mut out: Vec<PathBuf> = path
        .map(|p| {
            std::env::split_paths(p)
                .filter(|d| !d.as_os_str().is_empty())
                .map(|d| d.join(exe))
                .collect()
        })
        .unwrap_or_default();
    let set = |v: Option<&OsStr>| v.filter(|v| !v.is_empty()).map(PathBuf::from);
    if windows {
        let cli = |base: PathBuf| base.join("GitHub CLI").join("gh.exe");
        out.push(cli(
            set(program_files).unwrap_or_else(|| PathBuf::from(r"C:\Program Files"))
        ));
        if let Some(x86) = set(program_files_x86) {
            out.push(cli(x86));
        }
        if let Some(local) = set(local_app_data) {
            out.push(cli(local.join("Programs")));
        }
    } else {
        for p in ["/opt/homebrew/bin/gh", "/usr/local/bin/gh", "/usr/bin/gh"] {
            out.push(PathBuf::from(p));
        }
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert(p.clone()));
    out
}

// ---- local clock (the rate-limit text) ----

/// `hh:mm` of Unix ms `ms` in the local time zone (Unix: `localtime_r`; Windows: the current
/// bias from `GetTimeZoneInformation`).
pub fn local_hhmm(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let offset = local_offset_secs(secs);
    let day = (secs + offset).rem_euclid(86_400);
    format!("{:02}:{:02}", day / 3600, (day % 3600) / 60)
}

#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // `tm_gmtoff` is a `c_long`
fn local_offset_secs(secs: i64) -> i64 {
    let t: libc::time_t = secs as libc::time_t;
    // SAFETY: `tm` is a plain C struct (zeroed is valid); localtime_r only writes into it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&t, &mut tm).is_null() };
    if ok {
        tm.tm_gmtoff as i64
    } else {
        0
    }
}

#[cfg(windows)]
fn local_offset_secs(_secs: i64) -> i64 {
    // TODO(windows-verify): the rate-limit text shows the local clock (summer time included; plan6c D.114).
    #[repr(C)]
    struct SystemTime {
        parts: [u16; 8],
    }
    #[repr(C)]
    struct TimeZoneInformation {
        bias: i32,
        standard_name: [u16; 32],
        standard_date: SystemTime,
        standard_bias: i32,
        daylight_name: [u16; 32],
        daylight_date: SystemTime,
        daylight_bias: i32,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetTimeZoneInformation(info: *mut TimeZoneInformation) -> u32;
    }
    // SAFETY: the struct has the documented layout; the call only writes into it.
    let mut info: TimeZoneInformation = unsafe { std::mem::zeroed() };
    let id = unsafe { GetTimeZoneInformation(&mut info) };
    let bias = match id {
        1 => info.bias + info.standard_bias,
        2 => info.bias + info.daylight_bias,
        0 => info.bias,
        _ => 0,
    };
    -i64::from(bias) * 60
}

#[cfg(test)]
pub mod fake {
    //! [`FakeGh`]: scripted answers by argument prefix, every call recorded (with the body
    //! file's content at the time of the call).

    use super::*;

    /// Research6c bilag C.
    pub const NOT_LOGGED_IN: &str = "To get started with GitHub CLI, please run:  gh auth login\nAlternatively, populate the GH_TOKEN environment variable with a GitHub API authentication token.\n";
    pub const BAD_CREDENTIALS: &str = "HTTP 401: Bad credentials (https://api.github.com/graphql)\nTry authenticating with:  gh auth login\n";
    pub const REPO_MISSING: &str =
        "GraphQL: Could not resolve to a Repository with the name 'o/missing'. (repository)\n";
    pub const RATE_LIMIT: &str =
        "HTTP 403: API rate limit exceeded for user ID 1. (https://api.github.com/graphql)\n";
    pub const OFFLINE: &str =
        "Post \"https://api.github.com/graphql\": dial tcp: lookup api.github.com: no such host\n";

    type Answers = Vec<Result<Captured, GhError>>;

    #[derive(Default)]
    struct Inner {
        /// `(prefix, answers)`: the longest matching prefix answers; with several answers the
        /// first is used up, the last one stays.
        rules: Vec<(Vec<String>, Answers)>,
        calls: Vec<GhCall>,
        bodies: Vec<String>,
    }

    #[derive(Default)]
    pub struct FakeGh {
        inner: Mutex<Inner>,
    }

    impl FakeGh {
        pub fn new() -> Self {
            Self::default()
        }

        fn lock(&self) -> MutexGuard<'_, Inner> {
            self.inner.lock().unwrap_or_else(|p| p.into_inner())
        }

        fn push(&self, prefix: &[&str], answer: Result<Captured, GhError>) {
            let prefix: Vec<String> = prefix.iter().map(|s| s.to_string()).collect();
            let mut inner = self.lock();
            match inner.rules.iter_mut().find(|(p, _)| *p == prefix) {
                Some((_, answers)) => answers.push(answer),
                None => inner.rules.push((prefix, vec![answer])),
            }
        }

        /// Calls starting with `prefix` exit with `code` and print `stdout`/`stderr`.
        pub fn reply(&self, prefix: &[&str], code: i32, stdout: &str, stderr: &str) {
            self.push(
                prefix,
                Ok(Captured {
                    code: Some(code),
                    stdout: stdout.into(),
                    stderr: stderr.into(),
                    output: format!("{stdout}{stderr}"),
                    ..Captured::default()
                }),
            );
        }

        /// Calls starting with `prefix` give this raw result (timeout, clipped …).
        pub fn captured(&self, prefix: &[&str], c: Captured) {
            self.push(prefix, Ok(c));
        }

        /// Calls starting with `prefix` cannot start gh.
        pub fn fail(&self, prefix: &[&str]) {
            self.push(prefix, Err(GhError::GhMissing));
        }

        /// The argument lists so far (`--body-file <path>` included).
        pub fn calls(&self) -> Vec<Vec<String>> {
            self.lock()
                .calls
                .iter()
                .map(|c| {
                    c.argv()
                        .into_iter()
                        .map(|a| a.to_string_lossy().into_owned())
                        .collect()
                })
                .collect()
        }

        /// The raw calls (timeouts, body file paths).
        pub fn raw_calls(&self) -> Vec<GhCall> {
            self.lock().calls.clone()
        }

        /// The body files' contents, read when the call was made.
        pub fn bodies(&self) -> Vec<String> {
            self.lock().bodies.clone()
        }
    }

    impl GhRunner for FakeGh {
        fn exec(&self, call: &GhCall) -> Result<Captured, GhError> {
            let mut inner = self.lock();
            inner.calls.push(call.clone());
            if let Some(f) = &call.body_file {
                let body = std::fs::read_to_string(f).unwrap_or_default();
                inner.bodies.push(body);
            }
            let best = inner
                .rules
                .iter_mut()
                .filter(|(p, _)| call.args.starts_with(p))
                .max_by_key(|(p, _)| p.len());
            match best {
                Some((_, answers)) if answers.len() > 1 => answers.remove(0),
                Some((_, answers)) => answers[0].clone(),
                None => Err(GhError::Other(format!(
                    "FakeGh: intet svar til {}",
                    call.args.join(" ")
                ))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::*;
    use super::*;

    fn cap(code: Option<i32>, stdout: &str, stderr: &str) -> Captured {
        Captured {
            code,
            stdout: stdout.into(),
            stderr: stderr.into(),
            ..Captured::default()
        }
    }

    #[test]
    fn classify_table() {
        let ok = cap(Some(0), "[]", "");
        assert_eq!(
            classify(&ok),
            Ok(GhOut {
                stdout: "[]".into(),
                stderr: String::new()
            })
        );
        let table: Vec<(Captured, GhError)> = vec![
            (
                Captured {
                    timed_out: true,
                    code: None,
                    ..Captured::default()
                },
                GhError::Timeout,
            ),
            (cap(Some(4), "", NOT_LOGGED_IN), GhError::NotLoggedIn),
            (cap(Some(1), "", BAD_CREDENTIALS), GhError::BadCredentials),
            (cap(Some(1), "", REPO_MISSING), GhError::RepoNotFound),
            (cap(Some(1), "", RATE_LIMIT), GhError::RateLimited),
            (cap(Some(1), "", "HTTP 429: too many"), GhError::RateLimited),
            (
                cap(
                    Some(1),
                    "",
                    "the 'o/noissues' repository has disabled issues\n",
                ),
                GhError::IssuesDisabled,
            ),
            (cap(Some(1), "", OFFLINE), GhError::Network),
            (
                Captured {
                    code: Some(0),
                    clipped: true,
                    stdout: "\"title\":\"x\"}]".into(),
                    ..Captured::default()
                },
                GhError::TooLarge,
            ),
            (
                cap(Some(1), "", "invalid issue format: \"abc\"\n"),
                GhError::Other("invalid issue format: \"abc\"".into()),
            ),
            (cap(Some(2), "", ""), GhError::Other("exit 2".into())),
        ];
        for (c, want) in table {
            assert_eq!(classify(&c), Err(want), "{c:?}");
        }
        // A successful close prints the issue title on stderr: never an error.
        let close = cap(Some(0), "", "✓ Closed issue o/r#7 (rate limit HTTP 401)");
        assert!(classify(&close).is_ok());
        // An HTTP status in a Post line is not "offline".
        let e = classify(&cap(
            Some(1),
            "",
            "Post \"https://x\": HTTP 502 Bad Gateway",
        ));
        assert!(matches!(e, Err(GhError::Other(_))), "{e:?}");
        // Other keeps the last 300 chars on one line.
        let long = format!("{}\nslut", "x".repeat(400));
        let Err(GhError::Other(t)) = classify(&cap(Some(1), "", &long)) else {
            panic!()
        };
        assert_eq!(t.chars().count(), 300);
        assert!(t.ends_with(" slut"));
    }

    #[test]
    fn error_texts_are_danish() {
        assert_eq!(
            error_text(&GhError::NotLoggedIn, "o/r"),
            "gh er ikke logget ind — kør gh auth login i en terminal"
        );
        assert_eq!(
            error_text(&GhError::RepoNotFound, "o/r"),
            "repoet «o/r» findes ikke, eller gh har ikke adgang"
        );
        assert_eq!(
            error_text(&GhError::IssuesDisabled, "o/r"),
            "issues er slået fra i «o/r»"
        );
        assert_eq!(
            error_text(&GhError::GhMissing, ""),
            "gh ikke fundet — Indbakke fra GitHub er slået fra (mappe-kilden virker)"
        );
        assert_eq!(
            error_text(&GhError::BadCredentials, ""),
            "GitHub afviste login (401) — kør gh auth login igen"
        );
        assert_eq!(
            error_text(&GhError::Network, ""),
            "ingen forbindelse til GitHub"
        );
        assert_eq!(
            error_text(&GhError::Timeout, ""),
            "gh svarede ikke inden for 30 s"
        );
        assert_eq!(
            error_text(&GhError::TooLarge, ""),
            "svaret fra gh var for stort"
        );
        assert_eq!(
            error_text(&GhError::BadJson("x".into()), ""),
            "svaret fra gh kunne ikke læses"
        );
        assert_eq!(error_text(&GhError::Other("boom".into()), ""), "gh: boom");
    }

    /// A ProcRunner that records what would run.
    #[derive(Default)]
    struct Rec(
        Mutex<Vec<crate::proc::ProcCall>>,
        Mutex<Option<Captured>>,
        Mutex<usize>,
    );
    impl ProcRunner for Rec {
        fn run(
            &self,
            spec: &CommandSpec,
            cwd: &Path,
            env: &[(String, String)],
            timeout: Duration,
            tail: usize,
        ) -> Result<Captured, String> {
            *self.2.lock().unwrap() = tail;
            self.0
                .lock()
                .unwrap()
                .push((spec.clone(), cwd.into(), env.to_vec(), timeout));
            Ok(self.1.lock().unwrap().clone().unwrap_or(Captured {
                code: Some(0),
                stdout: "https://github.com/o/r/issues/7#issuecomment-1\n".into(),
                ..Captured::default()
            }))
        }
    }

    #[test]
    fn args_never_contain_free_text() {
        let rec = Arc::new(Rec::default());
        let data = PathBuf::from("/data");
        let gh = SystemGh::with(
            Some(PathBuf::from("/usr/bin/gh")),
            rec.clone(),
            data.clone(),
        );
        let body = data.join("tmp").join("wb-ab12cd34-5.md");
        let mut call = GhCall::new(["issue", "comment", "7", "--repo", "o/r"]);
        call.body_file = Some(body.clone());
        let out = gh.run(&call).unwrap();
        assert_eq!(
            out.stdout.trim(),
            "https://github.com/o/r/issues/7#issuecomment-1"
        );
        let calls = rec.0.lock().unwrap();
        let (spec, cwd, _env, timeout) = &calls[0];
        assert_eq!(spec.program, PathBuf::from("/usr/bin/gh"));
        assert_eq!(spec.raw_arg, None, "never a shell");
        let mut want: Vec<OsString> = ["issue", "comment", "7", "--repo", "o/r", "--body-file"]
            .iter()
            .map(OsString::from)
            .collect();
        want.push(body.into_os_string());
        assert_eq!(
            spec.args, want,
            "--body-file is last and points into app_data/tmp"
        );
        assert!(Path::new(spec.args.last().unwrap()).starts_with(data.join("tmp")));
        // Never the inline body flag (only the file).
        assert!(!spec
            .args
            .iter()
            .any(|a| a.to_string_lossy().trim_start_matches('-') == "body"));
        assert_eq!(cwd, &data);
        assert_eq!(*timeout, Duration::from_millis(GH_TIMEOUT_MS));
        assert_eq!(*rec.2.lock().unwrap(), GH_OUTPUT_MAX_CHARS);
    }

    #[test]
    fn env_sets_prompt_disabled_and_force_tty_empty() {
        let env = gh_env();
        let has = |k: &str, v: &str| env.contains(&(k.to_string(), v.to_string()));
        assert!(has("GH_PROMPT_DISABLED", "1"));
        assert!(has("GH_NO_UPDATE_NOTIFIER", "1"));
        assert!(has("GH_NO_EXTENSION_UPDATE_NOTIFIER", "1"));
        assert!(has("GH_TELEMETRY", "false"));
        assert!(has("DO_NOT_TRACK", "1"));
        assert!(has("GH_FORCE_TTY", ""));
        assert!(has("GH_SPINNER_DISABLED", "1"));
        assert!(has("NO_COLOR", "1"));
        assert!(has("GH_PAGER", "cat"));
        for k in ["GH_TOKEN", "GITHUB_TOKEN", "GH_CONFIG_DIR", "GH_HOST"] {
            assert!(!env.iter().any(|(n, _)| n == k), "{k} is never set");
        }
        // SystemGh passes it on.
        let rec = Arc::new(Rec::default());
        let gh = SystemGh::with(Some("/gh".into()), rec.clone(), "/d".into());
        gh.run(&GhCall::new(["--version"])).unwrap();
        assert_eq!(rec.0.lock().unwrap()[0].2, env);
    }

    #[test]
    fn system_gh_maps_clipped_timeout_and_spawn_failure() {
        let rec = Arc::new(Rec::default());
        let gh = SystemGh::with(Some("/gh".into()), rec.clone(), "/d".into());
        *rec.1.lock().unwrap() = Some(Captured {
            code: Some(0),
            clipped: true,
            stdout: "half".into(),
            ..Captured::default()
        });
        assert_eq!(
            gh.run(&GhCall::new(["issue", "list"])),
            Err(GhError::TooLarge)
        );
        *rec.1.lock().unwrap() = Some(Captured {
            timed_out: true,
            ..Captured::default()
        });
        assert_eq!(
            gh.run(&GhCall::new(["issue", "list"])),
            Err(GhError::Timeout)
        );

        struct NoStart;
        impl ProcRunner for NoStart {
            fn run(
                &self,
                _: &CommandSpec,
                _: &Path,
                _: &[(String, String)],
                _: Duration,
                _: usize,
            ) -> Result<Captured, String> {
                Err("kunne ikke starte gh".into())
            }
        }
        let gh = SystemGh::with(Some("/gh".into()), Arc::new(NoStart), "/d".into());
        assert_eq!(gh.run(&GhCall::new(["x"])), Err(GhError::GhMissing));
    }

    #[test]
    fn candidates_order_without_duplicates() {
        let path = std::env::join_paths(["/opt/homebrew/bin", "/usr/bin", "", "/x"]).unwrap();
        assert_eq!(
            candidates_for(false, Some(&path), None, None, None),
            [
                "/opt/homebrew/bin/gh",
                "/usr/bin/gh",
                "/x/gh",
                "/usr/local/bin/gh"
            ]
            .map(PathBuf::from)
        );
        let pf = OsString::from(r"D:\Programmer");
        let x86 = OsString::from(r"D:\Programmer (x86)");
        let local = OsString::from(r"C:\Users\a\AppData\Local");
        let cli = |b: &str| PathBuf::from(b).join("GitHub CLI").join("gh.exe");
        assert_eq!(
            candidates_for(true, None, Some(&pf), Some(&x86), Some(&local)),
            vec![
                cli(r"D:\Programmer"),
                cli(r"D:\Programmer (x86)"),
                cli(&PathBuf::from(r"C:\Users\a\AppData\Local")
                    .join("Programs")
                    .to_string_lossy()),
            ]
        );
        // Without the variables: the usual folder only.
        assert_eq!(
            candidates_for(true, None, None, Some(OsStr::new("")), None),
            vec![cli(r"C:\Program Files")]
        );
        assert!(candidates_for(true, None, None, None, None)
            .iter()
            .chain(candidates_for(false, None, None, None, None).iter())
            .all(|p| {
                let s = p.to_string_lossy();
                !s.ends_with(".cmd") && !s.ends_with(".ps1")
            }));
    }

    #[test]
    fn parse_gh_version_and_fields() {
        assert_eq!(
            parse_gh_version("gh version 2.102.0 (2026-09-30)"),
            Some((2, 102, 0))
        );
        assert_eq!(parse_gh_version("gh version 2.40.1"), Some((2, 40, 1)));
        assert_eq!(
            parse_gh_version("gh version v2.9.0-rc1 (x)"),
            Some((2, 9, 0))
        );
        assert_eq!(parse_gh_version("git version 2.40.0"), None);
        assert_eq!(parse_gh_version("gh version x"), None);
        assert_eq!(
            version_fields(&VersionProbe::Ok("gh version 2.102.0 (2026-09-30)".into())),
            (Some("2.102.0".into()), None)
        );
        assert_eq!(
            version_fields(&VersionProbe::Ok("gh version 2.39.9".into())),
            (Some("2.39.9".into()), Some(GH_OLD_NOTE.into()))
        );
        assert_eq!(
            version_fields(&VersionProbe::NotFound),
            (None, Some("ikke fundet".into()))
        );
        assert_eq!(
            version_fields(&VersionProbe::Pending),
            (None, Some("kører stadig".into()))
        );
        assert_eq!(
            version_fields(&VersionProbe::Ok("??".into())),
            (
                Some("??".into()),
                Some("kunne ikke læse versionsnummeret".into())
            )
        );
    }

    #[test]
    fn none_cache_expires() {
        let cache: GhCache = Mutex::new(None);
        let lookups = std::cell::Cell::new(0);
        let t0 = Instant::now();
        let miss = || {
            lookups.set(lookups.get() + 1);
            None
        };
        assert_eq!(cached_lookup(&cache, t0, GH_NONE_TTL, miss), None);
        assert_eq!(
            cached_lookup(&cache, t0 + Duration::from_secs(59), GH_NONE_TTL, miss),
            None
        );
        assert_eq!(lookups.get(), 1, "a miss is cached for 60 s");
        let hit = || {
            lookups.set(lookups.get() + 1);
            Some(PathBuf::from("/usr/bin/gh"))
        };
        let later = t0 + Duration::from_secs(60);
        assert_eq!(
            cached_lookup(&cache, later, GH_NONE_TTL, hit),
            Some(PathBuf::from("/usr/bin/gh"))
        );
        assert_eq!(lookups.get(), 2, "looked up again after 60 s");
        assert_eq!(
            cached_lookup(&cache, later + Duration::from_secs(3600), GH_NONE_TTL, miss),
            Some(PathBuf::from("/usr/bin/gh"))
        );
        assert_eq!(lookups.get(), 2, "a hit is kept");
    }

    #[test]
    fn repo_and_url_validation() {
        for ok in ["o/r", "My-Org/repo.name", "a_b/c-d", "x/..y"] {
            assert!(valid_repo(ok), "{ok}");
        }
        for bad in [
            "", "o", "o/r/x", "/r", "o/", "o/..", "./r", "-o/r", "o/-r", "o r/x", "o/r;x",
            "HOST/o/r", "o/ær",
        ] {
            assert!(!valid_repo(bad), "{bad}");
        }
        assert!(!valid_repo(&format!("o/{}", "r".repeat(99))));
        assert!(issue_url_ok("https://github.com/o/r/issues/7", "o/r", 7));
        assert!(issue_url_ok("https://github.com/O/R/issues/7", "o/r", 7));
        for bad in [
            "http://github.com/o/r/issues/7",
            "https://evil.com/o/r/issues/7",
            "https://github.com.evil.com/o/r/issues/7",
            "https://github.com/o/r/issues/8",
            "https://github.com/o/x/issues/7",
            "https://github.com/o/r/pull/7",
            "https://github.com/o/r/issues/7?x=1",
            "javascript:alert(1)",
        ] {
            assert!(!issue_url_ok(bad, "o/r", 7), "{bad}");
        }
        assert!(!issue_url_ok("https://github.com/o/r/issues/0", "o/r", 0));
    }

    #[test]
    fn auth_text_drops_token_lines() {
        let c = cap(
            Some(0),
            "github.com\n  ✓ Logged in to github.com account alice (keyring)\n  - Token: gho_****\n",
            "",
        );
        assert_eq!(
            auth_text(&c),
            "github.com\n  ✓ Logged in to github.com account alice (keyring)"
        );
        let fake = FakeGh::new();
        fake.reply(&["auth", "status"], 0, &c.stdout, "");
        let r = check_auth(&fake);
        assert!(r.ok);
        assert!(!r.text.contains("gho_"));
        assert_eq!(
            fake.calls(),
            vec![vec!["auth", "status", "--hostname", "github.com"]]
        );
        assert_eq!(
            fake.raw_calls()[0].timeout,
            Duration::from_millis(GH_AUTH_TIMEOUT_MS)
        );
        let fake = FakeGh::new();
        fake.reply(
            &["auth"],
            1,
            "",
            "You are not logged into any GitHub hosts. To log in, run: gh auth login\n",
        );
        let r = check_auth(&fake);
        assert!(!r.ok);
        assert!(r.text.starts_with("You are not logged into"));
        let fake = FakeGh::new();
        fake.fail(&["auth"]);
        assert_eq!(
            check_auth(&fake),
            GhAuthResult {
                ok: false,
                text: error_text(&GhError::GhMissing, "")
            }
        );
        let r = serde_json::to_value(GhAuthResult {
            ok: true,
            text: "x".into(),
        })
        .unwrap();
        assert_eq!(r, serde_json::json!({"ok": true, "text": "x"}));
    }

    #[test]
    fn fake_gh_matches_the_longest_prefix_and_uses_up_answers() {
        let f = FakeGh::new();
        f.reply(&["issue"], 0, "a", "");
        f.reply(&["issue", "view"], 0, "b", "");
        f.reply(&["issue", "view"], 0, "c", "");
        let run = |args: &[&str]| f.run(&GhCall::new(args.to_vec())).map(|o| o.stdout);
        assert_eq!(run(&["issue", "list"]), Ok("a".into()));
        assert_eq!(run(&["issue", "view", "1"]), Ok("b".into()));
        assert_eq!(run(&["issue", "view", "1"]), Ok("c".into()));
        assert_eq!(run(&["issue", "view", "1"]), Ok("c".into()));
        assert!(matches!(run(&["auth"]), Err(GhError::Other(_))));
    }

    #[test]
    fn local_clock_is_hh_mm() {
        let s = local_hhmm(1_700_000_000_000);
        assert_eq!(s.len(), 5);
        assert_eq!(&s[2..3], ":");
        assert!(s[..2].parse::<u32>().unwrap() < 24 && s[3..].parse::<u32>().unwrap() < 60);
    }
}
