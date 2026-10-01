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
/// First Claude Code version whose permission requests name the MCP server (`mcp_server.source`,
/// step 4 tools); step 4 is verified against 2.1.286.
pub const MCP_MIN_VERSION: (u32, u32, u32) = (2, 1, 274);

/// A `Starting` agent without any hook event for this long gets [`STARTING_HINT_TEXT`] as detail.
pub const STARTING_HINT_AFTER: Duration = Duration::from_secs(15);
/// Detail shown while an agent is presumably waiting for an answer in its terminal (trust dialog).
pub const STARTING_HINT_TEXT: &str = "Venter på svar i terminalen (fx godkendelse af mappen)";

// --- Tickets (step 3) ---

/// File name of the ticket store in the app data dir.
pub const TICKETS_FILE: &str = "tickets.json";
/// Current `schemaVersion` of `tickets.json`.
pub const TICKETS_SCHEMA_VERSION: u32 = 1;
/// Directory of the ticket files, relative to the agent's cwd. Always `/`-separated; join it
/// component by component (see `tickets::prompt::ticket_dir`).
pub const TICKET_DIR: &str = ".mira-bots/tickets";
/// Length of a ticket's short id (first hex chars of the uuid without dashes).
pub const TICKET_SHORT_ID_LEN: usize = 8;
/// Maximum title length (chars) accepted when a ticket is created or updated.
pub const TICKET_TITLE_MAX_CHARS: usize = 200;
/// Maximum title length (chars) in the line typed into the agent's terminal.
pub const TICKET_LINE_TITLE_MAX_CHARS: usize = 120;
/// Maximum body length (chars).
pub const TICKET_BODY_MAX_CHARS: usize = 20_000;
/// Pause between an agent becoming idle and typing the next ticket line.
pub const DISPATCH_DELAY_MS: u64 = 750;
/// Pause between the typed line and the separate Enter (`\r`).
pub const ENTER_DELAY_MS: u64 = 150;
/// How long to wait for a delivery confirmation before Enter is sent once more.
pub const CONFIRM_TIMEOUT_MS: u64 = 3_000;
/// How long to wait after the extra Enter before the delivery counts as failed.
pub const RETRY_TIMEOUT_MS: u64 = 5_000;
/// Spawn with a ticket: how long after the first idle to wait for confirmation of the positional
/// prompt before falling back to an ordinary PTY dispatch.
pub const SPAWN_CONFIRM_TIMEOUT_MS: u64 = 8_000;
/// Agent detail after an unconfirmed delivery.
pub const DELIVERY_FAILED_TEXT: &str = "Kunne ikke aflevere ticket, se terminalen";
/// Agent detail after a `StopFailure` while a ticket was in progress.
pub const TURN_FAILED_TEXT: &str = "Turn fejlede, prøv igen eller skriv i terminalen";
/// History note when queued/in-progress tickets go back to the backlog at startup.
pub const RESTART_NOTE: &str = "app genstartet";

// --- Agent tools (step 4, C4.8) ---

/// `true` restores step 3: a Stop moves the agent's inProgress ticket to review (done with
/// skipReview). `false` (step 4): the ticket stays in progress with `issue: notSubmitted` until
/// the agent calls `mira_submit_for_review` or the user moves it.
pub const AUTO_REVIEW_ON_STOP: bool = false;

/// Successful `mira_create_ticket` calls allowed per agent per rolling window.
pub const CREATE_TICKET_RATE_LIMIT: usize = 20;
/// The rate-limit window (ms).
pub const CREATE_TICKET_RATE_WINDOW_MS: u64 = 3_600_000;
/// Maximum `mira_submit_for_review` summary (chars).
pub const TICKET_SUMMARY_MAX_CHARS: usize = 2_000;
/// Maximum `mira_update_status` note (chars); longer notes are cut.
pub const AGENT_NOTE_MAX_CHARS: usize = 120;
/// Agent detail when a turn ended without `mira_submit_for_review`.
pub const NOT_SUBMITTED_TEXT: &str = "Turn afsluttet uden aflevering";
/// Env override for the mira-mcp exe location.
pub const MCP_EXE_ENV: &str = "MIRA_MCP_EXE";
/// The MCP server's name in mcp.json (same as `mira_mcp::SERVER_NAME`).
pub const MCP_SERVER_NAME: &str = "mira-bots";
/// Claude Code's name prefix for the server's tools (`mcp__<server>__`).
pub const MCP_TOOL_PREFIX: &str = "mcp__mira-bots__";
/// The app's own Claude Code settings file in the app data dir (hooks + permissions), passed
/// with `--settings`.
pub const SETTINGS_FILE: &str = "settings.json";
/// Step 1–3 name of that file; removed when settings.json is written.
pub const LEGACY_HOOKS_FILE: &str = "hooks.json";
/// MCP config in the app data dir, passed with `--mcp-config`.
pub const MCP_CONFIG_FILE: &str = "mcp.json";
/// System prompt addition in the app data dir, passed with `--append-system-prompt-file`.
pub const SYSTEM_PROMPT_FILE: &str = "system-prompt.md";

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
    fn ticket_limits_match_the_plan() {
        assert_eq!(TICKETS_FILE, "tickets.json");
        assert_eq!(TICKETS_SCHEMA_VERSION, 1);
        assert_eq!(TICKET_DIR, ".mira-bots/tickets");
        assert_eq!(TICKET_SHORT_ID_LEN, 8);
        assert_eq!(TICKET_TITLE_MAX_CHARS, 200);
        assert_eq!(TICKET_LINE_TITLE_MAX_CHARS, 120);
        assert_eq!(TICKET_BODY_MAX_CHARS, 20_000);
        assert_eq!(DISPATCH_DELAY_MS, 750);
        assert_eq!(ENTER_DELAY_MS, 150);
        assert_eq!(CONFIRM_TIMEOUT_MS, 3_000);
        assert_eq!(RETRY_TIMEOUT_MS, 5_000);
        assert_eq!(SPAWN_CONFIRM_TIMEOUT_MS, 8_000);
        assert_eq!(
            DELIVERY_FAILED_TEXT,
            "Kunne ikke aflevere ticket, se terminalen"
        );
        assert_eq!(
            TURN_FAILED_TEXT,
            "Turn fejlede, prøv igen eller skriv i terminalen"
        );
        assert_eq!(RESTART_NOTE, "app genstartet");
    }

    #[test]
    fn agent_tool_limits_match_the_plan() {
        assert_eq!(CREATE_TICKET_RATE_LIMIT, 20);
        assert_eq!(CREATE_TICKET_RATE_WINDOW_MS, 3_600_000);
        assert_eq!(TICKET_SUMMARY_MAX_CHARS, 2_000);
        assert_eq!(AGENT_NOTE_MAX_CHARS, 120);
        assert_eq!(NOT_SUBMITTED_TEXT, "Turn afsluttet uden aflevering");
    }

    #[test]
    fn step4_constants_match_the_plan() {
        const { assert!(!AUTO_REVIEW_ON_STOP) };
        assert_eq!(MCP_EXE_ENV, "MIRA_MCP_EXE");
        assert_eq!(MCP_SERVER_NAME, "mira-bots");
        assert_eq!(MCP_TOOL_PREFIX, "mcp__mira-bots__");
        assert_eq!(SETTINGS_FILE, "settings.json");
        assert_eq!(LEGACY_HOOKS_FILE, "hooks.json");
        assert_eq!(MCP_CONFIG_FILE, "mcp.json");
        assert_eq!(SYSTEM_PROMPT_FILE, "system-prompt.md");
    }

    #[test]
    fn mcp_names_match_the_mcp_server() {
        assert_eq!(MCP_TOOL_PREFIX, format!("mcp__{MCP_SERVER_NAME}__"));
        assert_eq!(mira_mcp::SERVER_NAME, MCP_SERVER_NAME);
        assert_eq!(mira_mcp::PIPE_ENV, PIPE_ENV);
        assert_eq!(mira_mcp::AGENT_ID_ENV, AGENT_ID_ENV);
    }

    #[test]
    fn agent_id_env_matches_the_hook_exe() {
        assert_eq!(AGENT_ID_ENV, "MIRA_AGENT_ID");
        assert_eq!(AGENT_ID_ENV, mira_hook::AGENT_ID_ENV);
    }
}
