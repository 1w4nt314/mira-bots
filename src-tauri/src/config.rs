//! Compile-time configuration. No user settings (steps 1–2).

use std::time::Duration;

use crate::tickets::model::GitMode;

/// Maximum number of simultaneously running work agents (seat kind `work`).
pub const MAX_WORK_AGENTS: usize = 5;
// TODO(windows-verify): D.69 (third staff seat: UI, mira_spawn_agent and the limit text)
/// Maximum number of simultaneously running staff agents (seat kind `staff`).
pub const MAX_STAFF_AGENTS: usize = 3;

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

// --- Profiles, roles, model/effort, reviews, reports (step 5, C5.8) ---

/// Rounds of review rejection before the ticket is escalated to the user.
pub const MAX_REVIEW_ROUNDS: u32 = 3;
/// Env var carrying the agent's roles (`coder,reviewer`; empty = none) to claude and mira-mcp.
/// Same value as `mira_mcp::ROLES_ENV`.
pub const ROLES_ENV: &str = "MIRA_AGENT_ROLES";
/// Profile store, relative to the projects root (`/`-separated; join component by component).
pub const PROFILES_DIR: &str = ".mira-bots/profiles";
/// Rendered per-profile files under the app data dir: `<id>/settings.json`, `<id>/system-prompt.md`.
pub const PROFILE_FILES_DIR: &str = "profiles";
/// Profile used when a spawn names none.
pub const DEFAULT_PROFILE_ID: &str = "coder";
/// The built-in profiles, in list order.
pub const BUILTIN_PROFILE_IDS: [&str; 7] = [
    "coder",
    "researcher",
    "reviewer",
    "coordinator",
    "planner",
    "debugger",
    "specialist",
];
/// Model aliases Claude Code accepts for `--model` (research5 Q1).
pub const MODEL_ALIASES: [&str; 9] = [
    "default",
    "best",
    "fable",
    "sonnet",
    "opus",
    "haiku",
    "sonnet[1m]",
    "opus[1m]",
    "opusplan",
];
/// Maximum length of a full model id.
pub const MODEL_ID_MAX_CHARS: usize = 64;
/// Maximum profile name length (chars, after trimming).
pub const PROFILE_NAME_MAX_CHARS: usize = 60;
/// Maximum `promptAppend` length (chars).
pub const PROMPT_APPEND_MAX_CHARS: usize = 4_000;
/// Report files under the app data dir: `<ticketId>/reports/<nn>-<slug>.md`.
pub const REPORTS_DIR: &str = "tickets";
pub const REPORT_TITLE_MAX_CHARS: usize = 120;
pub const REPORT_BODY_MAX_CHARS: usize = 20_000;
pub const REPORTS_PER_TICKET_MAX: usize = 20;
pub const REPORT_ON_SUBMIT_TITLE: &str = "Rapport ved aflevering";
/// Review files, relative to the reviewer's cwd.
pub const REVIEW_DIR: &str = ".mira-bots/reviews";
pub const REVIEW_DELIVERY_MAX_ATTEMPTS: u32 = 3;
pub const REVIEW_NOTE_MAX_CHARS: usize = 2_000;
/// Agent detail while it restarts with `--resume` (model/effort change).
pub const RESTARTING_TEXT: &str = "Genstarter med nye indstillinger";

/// Agent detail while it restarts in another project ("Flyt til projekt…", plan4b A.3).
pub fn moving_text(project: &str) -> String {
    format!("Flytter til «{project}»…")
}

/// Agent detail while it restarts with a fresh session (or in the ticket's worktree) before
/// a ticket delivery (step 6b, plan A.7).
pub fn fresh_text(short: &str) -> String {
    format!("Ny session til ticket {short}…")
}
/// Whether the per-profile settings get a `statusLine` pointing at the hook exe (live
/// model/effort). `false` leaves only PostModelSwitch and the requested values.
pub const STATUSLINE_ENABLED: bool = true;
/// `hook_event_name` the hook exe gives a statusLine invocation.
pub const STATUSLINE_EVENT: &str = "StatusLine";

// --- Projects and the workspace file (step 4b) ---

/// Default projects root folder name: `<home>/mira-bots/projects`.
pub const PROJECTS_DIR_NAME: &str = "projects";
/// Workspace rules file directly in the projects root.
pub const WORKSPACE_FILE: &str = "mira-bots.workspace.json";
/// The app's own settings (`{"projectsRoot": "<path>"}`) in the app data dir.
pub const APP_SETTINGS_FILE: &str = "app-settings.json";
/// Maximum project id length (chars).
pub const PROJECT_ID_MAX_CHARS: usize = 64;
/// Maximum length (chars) of `<root>/<id>`, so the agents' paths stay well below MAX_PATH.
pub const PROJECT_PATH_MAX_CHARS: usize = 200;
/// How long after the user typed in a terminal the dispatcher holds a delivery back (default of
/// the workspace rule `userInputGraceMs`).
pub const USER_INPUT_GRACE_MS: u64 = 5000;
/// History note when a moved agent's queue goes back to the backlog.
pub const MOVED_NOTE: &str = "agenten flyttede til et andet projekt";
/// Default of `maxAgentsPerProject` (0 = unlimited).
pub const MAX_AGENTS_PER_PROJECT: usize = 0;
/// Default of `reviewByDefault`.
pub const REVIEW_BY_DEFAULT: bool = true;
/// Default of `agentsMayCreateProjects`.
pub const AGENTS_MAY_CREATE_PROJECTS: bool = false;

// --- forløb: parent tickets and dependencies (step 6a) ---

/// Most `blockedBy` entries per ticket.
pub const BLOCKED_BY_MAX: usize = 10;
/// History note when a parent is submitted while it still has open children (`Wait`); the
/// service appends ` (<n>)`.
pub const WAITING_NOTE: &str = "venter på del-tickets";
/// History note when a waiting parent is resumed after the wake line was typed.
pub const WOKEN_NOTE: &str = "vækket: del-ticket godkendt";
/// History note when the last open child of a parent is gone (Done or deleted).
pub const CHILDREN_DONE_NOTE: &str = "alle del-tickets er afsluttet";
/// History note on a child whose parent was deleted (its `parentId` is cleared).
pub const PARENT_DELETED_NOTE: &str = "forælder slettet";
/// Agent detail when the wake line of a waiting parent was not confirmed (no UserPromptSubmit
/// with the line) in [`WAKE_MAX_ATTEMPTS`] attempts (review 6a W1).
pub const WAKE_UNCONFIRMED_TEXT: &str = "Vækning ikke bekræftet, se terminalen";
/// Attempts at typing the same wake line before the dispatcher gives up and shows
/// [`WAKE_UNCONFIRMED_TEXT`]: the first one and one retry at the next idle (review 6a W1).
pub const WAKE_MAX_ATTEMPTS: u8 = 2;

// --- playbooks, project checks, git per ticket, fresh session (step 6b, plan6b C6b.1/C6b.2) ---

/// The project file with the checks, relative to the project folder (`/`-separated).
pub const PROJECT_FILE: &str = ".mira-bots/project.json";
/// App-managed worktrees, relative to the project folder: `<project>/.mira-bots/wt/<short id>`.
pub const WORKTREE_DIR: &str = ".mira-bots/wt";
/// Content of `<folder>/.mira-bots/.gitignore`: everything app-owned is ignored except the
/// user's `project.json` (a file that is exactly `*\n` is upgraded to this).
pub const MIRA_GITIGNORE: &str = "*\n!project.json\n";
/// `timeoutSec` of a check when the project file gives none.
pub const CHECK_TIMEOUT_DEFAULT_SEC: u64 = 600;
/// Upper bound of `timeoutSec`.
pub const CHECK_TIMEOUT_MAX_SEC: u64 = 3_600;
/// Most checks per project file.
pub const CHECKS_MAX: usize = 10;
/// Output kept per failed check (the tail, chars).
pub const CHECK_OUTPUT_MAX_CHARS: usize = 8_000;
/// Most steps per playbook.
pub const PLAYBOOK_STEPS_MAX: usize = 6;
/// Upper bound of `maxReviewRounds` in the workspace file (lower bound 1).
pub const MAX_REVIEW_ROUNDS_MAX: u32 = 10;
/// Timeout of one git command run by the app.
pub const GIT_TIMEOUT_MS: u64 = 60_000;
/// How long the dispatcher waits for a restart before a ticket delivery before it gives up.
pub const RESTART_TIMEOUT_MS: u64 = 60_000;
/// Default of the workspace rule `git`.
pub const GIT_DEFAULT: GitMode = GitMode::Off;
/// Default of `checksGate`: a failed check rejects the ticket before review.
pub const CHECKS_GATE: bool = true;
/// Default of `autoSpawnForPlaybook`.
pub const AUTO_SPAWN_FOR_PLAYBOOK: bool = false;
/// Default of `freshSessionPerTicket`.
pub const FRESH_SESSION_PER_TICKET: bool = true;
/// Default of `cleanupWorktreesOnDone`.
pub const CLEANUP_WORKTREES_ON_DONE: bool = false;
/// History note on a child created by a playbook: "oprettet af forløb {parent}: trin {i}/{n}".
pub fn playbook_created_note(parent_short: &str, step: usize, steps: usize) -> String {
    format!("oprettet af forløb {parent_short}: trin {step}/{steps}")
}
/// History note when the last child of a playbook parent without assignee is done and the
/// parent goes to review (or Done).
pub const FLOW_DONE_NOTE: &str = "forløb afsluttet: alle del-tickets godkendt";
/// History note when the app restarted while a ticket's checks were running.
pub const CHECKS_INTERRUPTED_NOTE: &str = "tjek afbrudt af genstart";
/// Prefix of the rejection note when the app rejects a ticket because a check failed.
pub const CHECKS_REJECT_PREFIX: &str = "afvist af appen";
/// Title (prefix) of the app's check report.
pub const CHECKS_REPORT_TITLE: &str = "Tjek";
/// Title of the app's report with the ticket's git changes.
pub const CHANGES_REPORT_TITLE: &str = "Ændringer";

// --- macOS / unix (step 7) ---

/// Unix: time between SIGTERM and SIGKILL to an agent's process group.
pub const PROCESS_KILL_GRACE: Duration = Duration::from_secs(2);
/// Unix: how long quitting the app waits for every agent group to end before SIGKILL.
pub const QUIT_KILL_BUDGET: Duration = Duration::from_millis(1500);
/// Longest socket path accepted (`sun_path` is 104 bytes on macOS, 108 on Linux; margin).
pub const SOCKET_PATH_MAX: usize = 100;
/// Prefix of the private per-user socket directory (`mira-bots-<uid>`).
pub const SOCKET_DIR_PREFIX: &str = "mira-bots-";
/// Unix: how long the login shell may take to print its PATH.
pub const LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(5);
/// Marks the PATH in the login shell's output (banners and prompts around it are ignored).
pub const LOGIN_PATH_DELIMITER: &str = "_MIRA_PATH_DELIMITER_";
/// `TERM` for the agent's child when the app's own environment has none (Finder/Dock start).
pub const TERM_DEFAULT: &str = "xterm-256color";
/// `COLORTERM` for the agent's child when the app's own environment has none.
pub const COLORTERM_DEFAULT: &str = "truecolor";

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
        assert_eq!(MAX_STAFF_AGENTS, 3);
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
    fn step5_constants_match_the_plan() {
        assert_eq!(MAX_REVIEW_ROUNDS, 3);
        assert_eq!(ROLES_ENV, "MIRA_AGENT_ROLES");
        assert_eq!(ROLES_ENV, mira_mcp::ROLES_ENV);
        assert_eq!(PROFILES_DIR, ".mira-bots/profiles");
        assert_eq!(PROFILE_FILES_DIR, "profiles");
        assert_eq!(DEFAULT_PROFILE_ID, "coder");
        assert!(BUILTIN_PROFILE_IDS.contains(&DEFAULT_PROFILE_ID));
        assert_eq!(MODEL_ALIASES.len(), 9);
        assert_eq!(MODEL_ID_MAX_CHARS, 64);
        assert_eq!(PROFILE_NAME_MAX_CHARS, 60);
        assert_eq!(PROMPT_APPEND_MAX_CHARS, 4_000);
        assert_eq!(REPORTS_DIR, "tickets");
        assert_eq!(
            (
                REPORT_TITLE_MAX_CHARS,
                REPORT_BODY_MAX_CHARS,
                REPORTS_PER_TICKET_MAX
            ),
            (120, 20_000, 20)
        );
        assert_eq!(REPORT_ON_SUBMIT_TITLE, "Rapport ved aflevering");
        assert_eq!(REVIEW_DIR, ".mira-bots/reviews");
        assert_eq!(REVIEW_DELIVERY_MAX_ATTEMPTS, 3);
        assert_eq!(REVIEW_NOTE_MAX_CHARS, 2_000);
        assert_eq!(RESTARTING_TEXT, "Genstarter med nye indstillinger");
        assert_eq!(moving_text("shop"), "Flytter til «shop»…");
        assert_eq!(fresh_text("ab12cd34"), "Ny session til ticket ab12cd34…");
        const { assert!(STATUSLINE_ENABLED) };
        assert_eq!(STATUSLINE_EVENT, "StatusLine");
    }

    #[test]
    fn step4b_constants_match_the_plan() {
        assert_eq!(PROJECTS_DIR_NAME, "projects");
        assert_eq!(WORKSPACE_FILE, "mira-bots.workspace.json");
        assert_eq!(APP_SETTINGS_FILE, "app-settings.json");
        assert_eq!(PROJECT_ID_MAX_CHARS, 64);
        assert_eq!(PROJECT_PATH_MAX_CHARS, 200);
        assert_eq!(USER_INPUT_GRACE_MS, 5000);
        assert_eq!(MOVED_NOTE, "agenten flyttede til et andet projekt");
        assert_eq!(MAX_AGENTS_PER_PROJECT, 0);
        const { assert!(REVIEW_BY_DEFAULT) };
        const { assert!(!AGENTS_MAY_CREATE_PROJECTS) };
    }

    #[test]
    fn step6a_constants() {
        assert_eq!(BLOCKED_BY_MAX, 10);
        assert_eq!(WAITING_NOTE, "venter på del-tickets");
        assert_eq!(WOKEN_NOTE, "vækket: del-ticket godkendt");
        assert_eq!(CHILDREN_DONE_NOTE, "alle del-tickets er afsluttet");
        assert_eq!(PARENT_DELETED_NOTE, "forælder slettet");
        assert_eq!(
            WAKE_UNCONFIRMED_TEXT,
            "Vækning ikke bekræftet, se terminalen"
        );
        assert_eq!(WAKE_MAX_ATTEMPTS, 2);
        // A.1: no schema bump (an older build would discard every newer file).
        assert_eq!(TICKETS_SCHEMA_VERSION, 1);
    }

    #[test]
    fn step6b_constants() {
        assert_eq!(PROJECT_FILE, ".mira-bots/project.json");
        assert_eq!(WORKTREE_DIR, ".mira-bots/wt");
        assert_eq!(MIRA_GITIGNORE, "*\n!project.json\n");
        assert_eq!(
            (CHECK_TIMEOUT_DEFAULT_SEC, CHECK_TIMEOUT_MAX_SEC, CHECKS_MAX),
            (600, 3_600, 10)
        );
        assert_eq!(CHECK_OUTPUT_MAX_CHARS, 8_000);
        assert_eq!(PLAYBOOK_STEPS_MAX, 6);
        assert_eq!(MAX_REVIEW_ROUNDS_MAX, 10);
        const { assert!(MAX_REVIEW_ROUNDS <= MAX_REVIEW_ROUNDS_MAX) };
        assert_eq!((GIT_TIMEOUT_MS, RESTART_TIMEOUT_MS), (60_000, 60_000));
        assert_eq!(GIT_DEFAULT, GitMode::Off);
        const { assert!(CHECKS_GATE) };
        const { assert!(!AUTO_SPAWN_FOR_PLAYBOOK) };
        const { assert!(FRESH_SESSION_PER_TICKET) };
        const { assert!(!CLEANUP_WORKTREES_ON_DONE) };
        assert_eq!(
            playbook_created_note("ab12cd34", 2, 3),
            "oprettet af forløb ab12cd34: trin 2/3"
        );
        assert_eq!(
            FLOW_DONE_NOTE,
            "forløb afsluttet: alle del-tickets godkendt"
        );
        assert_eq!(CHECKS_INTERRUPTED_NOTE, "tjek afbrudt af genstart");
        assert_eq!(CHECKS_REJECT_PREFIX, "afvist af appen");
        assert_eq!(CHECKS_REPORT_TITLE, "Tjek");
        assert_eq!(CHANGES_REPORT_TITLE, "Ændringer");
    }

    #[test]
    fn step7_constants_match_the_plan() {
        assert_eq!(PROCESS_KILL_GRACE, Duration::from_secs(2));
        assert_eq!(QUIT_KILL_BUDGET, Duration::from_millis(1500));
        assert_eq!(SOCKET_PATH_MAX, 100);
        assert_eq!(SOCKET_DIR_PREFIX, "mira-bots-");
        assert_eq!(LOGIN_SHELL_TIMEOUT, Duration::from_secs(5));
        assert_eq!(LOGIN_PATH_DELIMITER, "_MIRA_PATH_DELIMITER_");
        assert_eq!(TERM_DEFAULT, "xterm-256color");
        assert_eq!(COLORTERM_DEFAULT, "truecolor");
    }

    #[test]
    fn agent_id_env_matches_the_hook_exe() {
        assert_eq!(AGENT_ID_ENV, "MIRA_AGENT_ID");
        assert_eq!(AGENT_ID_ENV, mira_hook::AGENT_ID_ENV);
    }
}
