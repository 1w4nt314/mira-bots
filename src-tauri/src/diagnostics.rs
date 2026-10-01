//! Diagnostics: log level from the environment, the `claude --version` probe, hook counters and
//! the `get_diagnostics` payload (C2.3). Pure logic; no Tauri types.

use std::ffi::OsStr;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use log::LevelFilter;
use serde::Serialize;

use crate::config::{HOOK_ARGS_MIN_VERSION, MCP_MIN_VERSION};

/// `MIRA_LOG` value → level. `trace|debug|info|warn|error` (any case); anything else → `Info`.
pub fn log_level_from_env(value: Option<&str>) -> LevelFilter {
    match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        Some("trace") => LevelFilter::Trace,
        Some("debug") => LevelFilter::Debug,
        Some("info") => LevelFilter::Info,
        Some("warn") => LevelFilter::Warn,
        Some("error") => LevelFilter::Error,
        _ => LevelFilter::Info,
    }
}

/// State of the one-shot background `claude --version` probe.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum VersionProbe {
    /// Still running (or not started yet).
    #[default]
    Pending,
    /// No claude binary was found at startup.
    NotFound,
    /// First line of `claude --version`, e.g. `2.1.211 (Claude Code)`.
    Ok(String),
    /// The probe failed (timeout, non-zero exit, could not start); Danish text.
    Failed(String),
}

/// Parses `2.1.211 (Claude Code)` → `(2, 1, 211)`: first whitespace token of the first non-empty
/// line, optional leading `v`, three dot-separated numbers; a suffix after the third number
/// (`-beta`) is ignored.
pub fn parse_claude_version(stdout: &str) -> Option<(u32, u32, u32)> {
    let line = stdout.lines().find(|l| !l.trim().is_empty())?;
    let token = line.split_whitespace().next()?;
    let token = token.strip_prefix('v').unwrap_or(token);
    let mut parts = token.splitn(3, '.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let rest = parts.next()?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let patch = digits.parse().ok()?;
    Some((major, minor, patch))
}

/// Whether hooks.json's exec-form `args` is understood by this Claude Code version.
pub fn supports_hook_args(v: (u32, u32, u32)) -> bool {
    v >= HOOK_ARGS_MIN_VERSION
}

/// Whether the agent tools (step 4: `mcp_server.source` in permission requests) are understood by
/// this Claude Code version.
pub fn supports_mcp_tools(v: (u32, u32, u32)) -> bool {
    v >= MCP_MIN_VERSION
}

/// `(claudeVersion, claudeVersionNote, claudeCodeArgsSupported, claudeCodeMcpSupported)` for the
/// diagnostics payload.
pub fn version_fields(
    probe: &VersionProbe,
) -> (Option<String>, Option<String>, Option<bool>, Option<bool>) {
    match probe {
        VersionProbe::Pending => (None, Some("kører stadig".into()), None, None),
        VersionProbe::NotFound => (None, Some("ikke fundet".into()), None, None),
        VersionProbe::Failed(e) => (None, Some(e.clone()), None, None),
        VersionProbe::Ok(v) => match parse_claude_version(v) {
            Some(parsed) => (
                Some(v.clone()),
                None,
                Some(supports_hook_args(parsed)),
                Some(supports_mcp_tools(parsed)),
            ),
            None => (
                Some(v.clone()),
                Some("kunne ikke læse versionsnummeret".into()),
                None,
                None,
            ),
        },
    }
}

fn timeout_text(timeout: Duration) -> String {
    if timeout.subsec_millis() == 0 {
        format!("timeout efter {} s", timeout.as_secs())
    } else {
        format!("timeout efter {} ms", timeout.as_millis())
    }
}

/// Runs `program args…` without a console window, waits at most `timeout` (polling every 50 ms;
/// killed on timeout) and returns the trimmed first line of stdout. Blocking.
pub fn run_version_command<S: AsRef<OsStr>>(
    program: &Path,
    args: &[S],
    timeout: Duration,
) -> Result<String, String> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // TODO(windows-verify): no console window flashes when the probe runs (plan D.17).
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().map_err(|e| format!("kunne ikke starte: {e}"))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(timeout_text(timeout));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("ventede forgæves: {e}")),
        }
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut stdout);
    }
    if let Some(mut err) = child.stderr.take() {
        let _ = err.read_to_string(&mut stderr);
    }
    if !status.success() {
        let code = status
            .code()
            .map_or_else(|| "?".to_string(), |c| c.to_string());
        return Err(format!("exit {code}: {}", stderr.trim()));
    }
    stdout
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "tomt output".to_string())
}

/// `claude --version` with a timeout (see [`run_version_command`]).
// TODO(windows-verify): the probe finishes in < 5 s and its output parses (plan D.17).
pub fn probe_claude_version(claude: &Path, timeout: Duration) -> Result<String, String> {
    run_version_command(claude, &["--version"], timeout)
}

/// The last valid hook frame the pipe handler saw.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LastHookEvent {
    pub name: String,
    pub session_id: String,
    /// Frame-level agent id (`MIRA_AGENT_ID`) as sent by the hook, if any.
    pub agent_id: Option<String>,
    /// Unix ms.
    pub at: u64,
}

/// The last tool frame (`mira-mcp`) the pipe handler answered. No arguments, ever.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LastToolCall {
    pub tool: String,
    pub agent_id: Option<String>,
    /// Whether the answer was `ok: true`.
    pub ok: bool,
    /// Unix ms.
    pub at: u64,
}

/// Counters updated by the pipe handler, shared with `AppState` (one `Arc`).
#[derive(Debug, Default)]
pub struct HookStats {
    /// Every successfully parsed hook frame.
    pub frames_received: AtomicU64,
    /// Parsed hook frames that matched no agent (neither by agent id nor by session id).
    pub frames_unknown_session: AtomicU64,
    pub last: Mutex<Option<LastHookEvent>>,
    /// Every answered tool frame (step 4).
    pub tool_calls: AtomicU64,
    /// Tool frames answered with `ok: false`.
    pub tool_errors: AtomicU64,
    pub last_tool: Mutex<Option<LastToolCall>>,
}

impl HookStats {
    /// Records one parsed frame.
    pub fn record(&self, last: LastHookEvent, matched: bool) {
        self.frames_received.fetch_add(1, Ordering::Relaxed);
        if !matched {
            self.frames_unknown_session.fetch_add(1, Ordering::Relaxed);
        }
        *self.last.lock().unwrap_or_else(|p| p.into_inner()) = Some(last);
    }

    pub fn received(&self) -> u64 {
        self.frames_received.load(Ordering::Relaxed)
    }

    pub fn unknown(&self) -> u64 {
        self.frames_unknown_session.load(Ordering::Relaxed)
    }

    pub fn last_event(&self) -> Option<LastHookEvent> {
        self.last.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Records one answered tool frame (`last.ok == false` counts as an error).
    pub fn record_tool(&self, last: LastToolCall) {
        self.tool_calls.fetch_add(1, Ordering::Relaxed);
        if !last.ok {
            self.tool_errors.fetch_add(1, Ordering::Relaxed);
        }
        *self.last_tool.lock().unwrap_or_else(|p| p.into_inner()) = Some(last);
    }

    pub fn tool_calls(&self) -> u64 {
        self.tool_calls.load(Ordering::Relaxed)
    }

    pub fn tool_errors(&self) -> u64 {
        self.tool_errors.load(Ordering::Relaxed)
    }

    pub fn last_tool_call(&self) -> Option<LastToolCall> {
        self.last_tool
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

/// `get_diagnostics` result (C2.3 + C4.1), camelCase.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostics {
    pub claude_path: Option<String>,
    pub claude_version: Option<String>,
    pub claude_version_note: Option<String>,
    pub claude_code_args_supported: Option<bool>,
    /// Claude Code >= 2.1.274 (the agent tools, step 4); `None` when the version is unknown.
    pub claude_code_mcp_supported: Option<bool>,
    pub hook_exe: Option<String>,
    /// `<app_data_dir>/settings.json` (hooks + permissions; `hooks.json` up to step 3).
    pub settings_path: String,
    pub settings_exists: bool,
    /// The mira-mcp exe; `None`: not found, agents get no tools.
    pub mcp_exe: Option<String>,
    /// `<app_data_dir>/mcp.json` (written only when mira-mcp was found).
    pub mcp_config_path: String,
    pub mcp_config_exists: bool,
    /// `<app_data_dir>/system-prompt.md`.
    pub system_prompt_path: String,
    /// Tool frames answered (all / `ok: false`) and the last one.
    pub tool_calls: u64,
    pub tool_errors: u64,
    pub last_tool_call: Option<LastToolCall>,
    /// [`crate::config::AUTO_REVIEW_ON_STOP`]: whether a Stop moves the ticket to review.
    pub auto_review_on_stop: bool,
    pub pipe_name: String,
    pub pipe_ready: bool,
    pub frames_received: u64,
    pub frames_unknown_session: u64,
    pub last_hook_event: Option<LastHookEvent>,
    pub log_path: Option<String>,
    pub app_version: String,
    pub agents_root: String,
    pub running_agents: usize,
    /// `<app_data_dir>/tickets.json`.
    pub tickets_path: String,
    /// Set when `tickets.json` could not be read at startup (renamed to `.broken-<ts>`, or
    /// unreadable → read-only).
    pub tickets_warning: Option<String>,
    /// `tickets.json` could not be read at startup (I/O error): ticket changes are disabled
    /// until the app restarts.
    pub tickets_read_only: bool,
    /// Number of tickets in memory.
    pub tickets_total: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn log_level_from_env_values() {
        assert_eq!(log_level_from_env(Some("trace")), LevelFilter::Trace);
        assert_eq!(log_level_from_env(Some("debug")), LevelFilter::Debug);
        assert_eq!(log_level_from_env(Some("info")), LevelFilter::Info);
        assert_eq!(log_level_from_env(Some("warn")), LevelFilter::Warn);
        assert_eq!(log_level_from_env(Some("error")), LevelFilter::Error);
        assert_eq!(log_level_from_env(Some("DeBuG")), LevelFilter::Debug);
        assert_eq!(log_level_from_env(Some("verbose")), LevelFilter::Info);
        assert_eq!(log_level_from_env(Some("")), LevelFilter::Info);
        assert_eq!(log_level_from_env(None), LevelFilter::Info);
    }

    #[test]
    fn parses_claude_versions() {
        assert_eq!(
            parse_claude_version("2.1.211 (Claude Code)"),
            Some((2, 1, 211))
        );
        assert_eq!(parse_claude_version("v2.1.139"), Some((2, 1, 139)));
        assert_eq!(parse_claude_version("2.1.286-beta (x)"), Some((2, 1, 286)));
        assert_eq!(parse_claude_version("\n  10.0.1\n"), Some((10, 0, 1)));
        assert_eq!(parse_claude_version("garbage"), None);
        assert_eq!(parse_claude_version("2.1"), None);
        assert_eq!(parse_claude_version(""), None);
    }

    #[test]
    fn hook_args_need_2_1_139() {
        assert!(!supports_hook_args((2, 1, 138)));
        assert!(supports_hook_args((2, 1, 139)));
        assert!(supports_hook_args((2, 2, 0)));
        assert!(supports_hook_args((3, 0, 0)));
        assert!(!supports_hook_args((1, 9, 999)));
    }

    #[test]
    fn version_fields_per_probe_state() {
        assert_eq!(
            version_fields(&VersionProbe::Pending),
            (None, Some("kører stadig".into()), None, None)
        );
        assert_eq!(
            version_fields(&VersionProbe::NotFound),
            (None, Some("ikke fundet".into()), None, None)
        );
        assert_eq!(
            version_fields(&VersionProbe::Failed("exit 1: x".into())),
            (None, Some("exit 1: x".into()), None, None)
        );
        assert_eq!(
            version_fields(&VersionProbe::Ok("2.1.100 (Claude Code)".into())),
            (
                Some("2.1.100 (Claude Code)".into()),
                None,
                Some(false),
                Some(false)
            )
        );
        // 2.1.139 has hook args but not the step 4 tools; 2.1.274 has both.
        let v = |s: &str| version_fields(&VersionProbe::Ok(s.into()));
        assert_eq!((v("2.1.139").2, v("2.1.139").3), (Some(true), Some(false)));
        assert_eq!((v("2.1.273").2, v("2.1.273").3), (Some(true), Some(false)));
        assert_eq!((v("2.1.274").2, v("2.1.274").3), (Some(true), Some(true)));
        assert_eq!(v("2.1.286 (Claude Code)").3, Some(true));
        assert_eq!((v("??").2, v("??").3), (None, None));
    }

    #[test]
    fn mcp_tools_need_2_1_274() {
        assert!(!supports_mcp_tools((2, 1, 273)));
        assert!(supports_mcp_tools((2, 1, 274)));
        assert!(supports_mcp_tools((2, 2, 0)));
        assert!(!supports_mcp_tools((1, 9, 999)));
    }

    #[cfg(unix)]
    #[test]
    fn version_command_returns_first_line() {
        let out = run_version_command(
            Path::new("/bin/sh"),
            &["-c", "echo '2.1.200 (x)'; echo second"],
            Duration::from_secs(5),
        );
        assert_eq!(out.as_deref(), Ok("2.1.200 (x)"));
        let err = run_version_command(
            Path::new("/bin/sh"),
            &["-c", "echo bad >&2; exit 3"],
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert_eq!(err, "exit 3: bad");
        assert!(run_version_command(
            Path::new("/definitely/not/here"),
            &["--version"],
            Duration::from_secs(1)
        )
        .unwrap_err()
        .starts_with("kunne ikke starte"));
    }

    #[cfg(unix)]
    #[test]
    fn version_command_times_out() {
        let t = Instant::now();
        let err = run_version_command(
            Path::new("/bin/sh"),
            &["-c", "sleep 5"],
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert_eq!(err, "timeout efter 100 ms");
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
    }

    #[test]
    fn tool_stats_count_and_remember() {
        let s = HookStats::default();
        assert_eq!((s.tool_calls(), s.tool_errors()), (0, 0));
        assert_eq!(s.last_tool_call(), None);
        s.record_tool(LastToolCall {
            tool: "mira_list_tickets".into(),
            agent_id: Some("a1".into()),
            ok: true,
            at: 5,
        });
        s.record_tool(LastToolCall {
            tool: "mira_submit_for_review".into(),
            agent_id: None,
            ok: false,
            at: 6,
        });
        assert_eq!((s.tool_calls(), s.tool_errors()), (2, 1));
        assert_eq!(s.received(), 0, "tool frames are not hook frames");
        let last = s.last_tool_call().unwrap();
        assert_eq!(
            serde_json::to_value(&last).unwrap(),
            serde_json::json!({"tool":"mira_submit_for_review","agentId":null,"ok":false,"at":6})
        );
    }

    #[test]
    fn hook_stats_count_and_remember() {
        let s = HookStats::default();
        let ev = |name: &str| LastHookEvent {
            name: name.into(),
            session_id: "s".into(),
            agent_id: None,
            at: 1,
        };
        s.record(ev("Stop"), true);
        s.record(ev("PreToolUse"), false);
        assert_eq!((s.received(), s.unknown()), (2, 1));
        assert_eq!(s.last_event().unwrap().name, "PreToolUse");
    }

    #[test]
    fn diagnostics_serialize_camel_case() {
        let d = Diagnostics {
            claude_path: Some("/c".into()),
            claude_version: Some("2.1.200 (Claude Code)".into()),
            claude_version_note: None,
            claude_code_args_supported: Some(true),
            claude_code_mcp_supported: Some(false),
            hook_exe: None,
            settings_path: "/d/settings.json".into(),
            settings_exists: true,
            mcp_exe: None,
            mcp_config_path: "/d/mcp.json".into(),
            mcp_config_exists: false,
            system_prompt_path: "/d/system-prompt.md".into(),
            tool_calls: 7,
            tool_errors: 2,
            last_tool_call: Some(LastToolCall {
                tool: "mira_submit_for_review".into(),
                agent_id: Some("a".into()),
                ok: false,
                at: 11,
            }),
            auto_review_on_stop: false,
            pipe_name: "p".into(),
            pipe_ready: true,
            frames_received: 3,
            frames_unknown_session: 1,
            last_hook_event: Some(LastHookEvent {
                name: "Stop".into(),
                session_id: "s".into(),
                agent_id: Some("a".into()),
                at: 9,
            }),
            log_path: None,
            app_version: "0.1.0".into(),
            agents_root: "/h/mira-bots/agents".into(),
            running_agents: 2,
            tickets_path: "/d/tickets.json".into(),
            tickets_warning: None,
            tickets_read_only: false,
            tickets_total: 4,
        };
        assert_eq!(
            serde_json::to_value(&d).unwrap(),
            json!({
                "claudePath": "/c",
                "claudeVersion": "2.1.200 (Claude Code)",
                "claudeVersionNote": null,
                "claudeCodeArgsSupported": true,
                "claudeCodeMcpSupported": false,
                "hookExe": null,
                "settingsPath": "/d/settings.json",
                "settingsExists": true,
                "mcpExe": null,
                "mcpConfigPath": "/d/mcp.json",
                "mcpConfigExists": false,
                "systemPromptPath": "/d/system-prompt.md",
                "toolCalls": 7,
                "toolErrors": 2,
                "lastToolCall": {"tool": "mira_submit_for_review", "agentId": "a", "ok": false, "at": 11},
                "autoReviewOnStop": false,
                "pipeName": "p",
                "pipeReady": true,
                "framesReceived": 3,
                "framesUnknownSession": 1,
                "lastHookEvent": {"name": "Stop", "sessionId": "s", "agentId": "a", "at": 9},
                "logPath": null,
                "appVersion": "0.1.0",
                "agentsRoot": "/h/mira-bots/agents",
                "runningAgents": 2,
                "ticketsPath": "/d/tickets.json",
                "ticketsWarning": null,
                "ticketsReadOnly": false,
                "ticketsTotal": 4
            })
        );
    }
}
