//! Compile-time configuration. No user settings (steps 1–2).

use std::time::Duration;

/// Maximum number of simultaneously running work agents (seat kind `work`).
pub const MAX_WORK_AGENTS: usize = 5;
/// Maximum number of simultaneously running staff agents (seat kind `staff`).
pub const MAX_STAFF_AGENTS: usize = 2;

/// Tools that are auto-allowed for every new agent (copied at spawn). Empty by default.
pub const DEFAULT_TOOL_WHITELIST: &[&str] = &[];

/// Per-agent PTY output ring buffer capacity (1 MiB).
pub const OUTPUT_RING_CAPACITY: usize = 1 << 20;

/// How long the app waits for a UI answer to a PermissionRequest before answering `none`.
/// Ordering 108 < 110 (hook exe budget) < 120 (hooks.json timeout) is deliberate.
pub const PERMISSION_APP_DEADLINE: Duration = Duration::from_secs(108);

/// hooks.json timeout (seconds) for PermissionRequest.
pub const PERMISSION_HOOK_TIMEOUT_S: u64 = 120;
/// hooks.json timeout (seconds) for all ordinary events.
pub const DEFAULT_HOOK_TIMEOUT_S: u64 = 10;
/// hooks.json timeout (seconds) for SessionEnd (Claude Code only grants ~1.5 s in total).
pub const SESSION_END_HOOK_TIMEOUT_S: u64 = 1;

/// Initial PTY size.
pub const PTY_COLS: u16 = 120;
pub const PTY_ROWS: u16 = 30;

/// Env var carrying the pipe name from the app to the hook exe (via claude).
pub const PIPE_ENV: &str = "MIRA_BOTS_PIPE";
/// Env override for the hook exe location.
pub const HOOK_EXE_ENV: &str = "MIRA_HOOK_EXE";
/// Env override for the claude binary location.
pub const CLAUDE_PATH_ENV: &str = "MIRA_CLAUDE_PATH";

/// Env var carrying the agent id from the app to the hook exe (via claude); the hook exe sends
/// it back as the frame's top-level `agent_id`. Same value as `mira_hook::AGENT_ID_ENV`.
pub const AGENT_ID_ENV: &str = "MIRA_AGENT_ID";

/// Maximum length (bytes) of one line received over the pipe.
pub const MAX_PIPE_LINE: usize = 1 << 20;

/// Env var selecting the log level (`trace|debug|info|warn|error`, default info).
pub const LOG_LEVEL_ENV: &str = "MIRA_LOG";
/// Log file stem in the app log dir: `mira-bots.log`, rotated `mira-bots_<timestamp>.log`.
pub const LOG_FILE_STEM: &str = "mira-bots";
/// Rotate the log file once it exceeds this many bytes.
pub const LOG_MAX_FILE_SIZE: u128 = 2_000_000;
/// Number of rotated log files kept.
pub const LOG_KEEP_FILES: usize = 3;

/// How long the background `claude --version` probe may take.
pub const CLAUDE_VERSION_TIMEOUT: Duration = Duration::from_secs(5);
/// First Claude Code version that supports exec-form hooks (`args`).
pub const HOOK_ARGS_MIN_VERSION: (u32, u32, u32) = (2, 1, 139);

/// A `Starting` agent without any hook event for this long gets [`STARTING_HINT_TEXT`] as detail.
pub const STARTING_HINT_AFTER: Duration = Duration::from_secs(15);
/// Detail shown while an agent is presumably waiting for an answer in its terminal (trust dialog).
pub const STARTING_HINT_TEXT: &str = "Venter på svar i terminalen (fx godkendelse af mappen)";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_ordering_is_preserved() {
        // app deadline < hook exe budget (110 s) < hooks.json timeout
        assert!(PERMISSION_APP_DEADLINE < Duration::from_secs(110));
        assert!(Duration::from_secs(110) < Duration::from_secs(PERMISSION_HOOK_TIMEOUT_S));
    }

    #[test]
    fn limits_match_the_plan() {
        assert_eq!(MAX_WORK_AGENTS, 5);
        assert_eq!(MAX_STAFF_AGENTS, 2);
        assert_eq!(STARTING_HINT_AFTER, Duration::from_secs(15));
        assert_eq!(LOG_KEEP_FILES, 3);
        assert_eq!(OUTPUT_RING_CAPACITY, 1_048_576);
        assert_eq!((PTY_COLS, PTY_ROWS), (120, 30));
        assert!(DEFAULT_TOOL_WHITELIST.is_empty());
    }

    #[test]
    fn agent_id_env_matches_the_hook_exe() {
        assert_eq!(AGENT_ID_ENV, "MIRA_AGENT_ID");
        assert_eq!(AGENT_ID_ENV, mira_hook::AGENT_ID_ENV);
    }
}
