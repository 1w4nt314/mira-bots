//! Work agents: one interactive `claude` per agent, running in a PTY.

pub mod claude_path;
pub mod login_env;
pub mod manager;
pub mod process;
pub mod pty;
pub mod ring_buffer;
pub mod roles;
pub mod workdir;

pub use manager::{
    build_resume_spec, build_spawn_spec, AgentId, AgentInfo, AgentManager, EventSink, FrameMatch,
    MatchVia, SeatKind, SinkEvent, SpawnContext, SpawnRequest,
};
pub use roles::Role;

/// Text of [`AgentError::LimitReached`].
fn limit_text(seat: &SeatKind, max: &usize) -> String {
    match seat {
        SeatKind::Work => format!("Loft på {max} arbejdspladser nået"),
        SeatKind::Staff => format!("Loft på {max} stabspladser nået"),
    }
}

/// Errors from agent operations. The messages are user-facing (Danish) because commands pass
/// them straight to the UI.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    /// All `max` seats of that kind are taken by non-exited agents (`max` from the workspace
    /// rules, plan4b A.4).
    #[error("{}", limit_text(.seat, .max))]
    LimitReached { seat: SeatKind, max: usize },
    #[error("Mappen findes ikke eller er ikke en mappe")]
    InvalidCwd,
    #[error("Fandt ikke claude — installer Claude Code eller sæt MIRA_CLAUDE_PATH")]
    ClaudeNotFound,
    #[error("Fandt ikke mira-hook — appen kan ikke lytte efter hook-events (sæt MIRA_HOOK_EXE)")]
    HookExeNotFound,
    /// The pipe server is not listening, so hooks would reach nothing (F2).
    #[error("Hook-forbindelsen er ikke klar — genstart mira-bots (se loggen)")]
    PipeNotReady,
    /// The initial prompt starts with `-` and would be parsed as a flag by claude.
    #[error("Prompten må ikke starte med '-' (den ville blive læst som et flag)")]
    InvalidPrompt,
    #[error("Agenten findes ikke")]
    NotFound,
    /// Model/effort change of an agent that is unknown or has exited (plan5 C5.4).
    #[error("Agenten kører ikke")]
    NotRunning,
    /// Model/effort change while the agent is not idle or has a ticket in progress (C5.6).
    #[error("Agenten arbejder")]
    Working,
    /// `remove` on an agent that has not exited (C.1 `remove_agent`).
    #[error("Agenten kører stadig")]
    StillRunning,
    #[error("Terminalfejl: {0}")]
    Pty(String),
    #[error("I/O-fejl: {0}")]
    Io(#[from] std::io::Error),
    // ---- step 4b (C4b.3) ----
    /// "Flyt til projekt…" while the agent still has queued tickets or waiting parents (without
    /// `force`; review 6a W2).
    #[error("Agenten har {0} tickets i kø eller i Venter — flyt dem først, eller bekræft at de lægges i Backlog")]
    QueueNotEmpty(usize),
    /// `maxAgentsPerProject` live work agents already run in the project.
    #[error("Loft på {max} agenter i projektet «{project}» nået")]
    ProjectLimit { project: String, max: usize },
    /// "Flyt til projekt…" to the agent's own project.
    #[error("Agenten står allerede i projekt «{0}»")]
    SameProject(String),
    #[error("Stabsagenter står i projektroden og kan ikke flyttes")]
    StaffHasNoProject,
    // ---- step 6d (A.11) ----
    /// `spawn`/`restart` after `kill_all()`: the app is closing, no new children (the watch's
    /// timer may still be running).
    #[error("Appen lukker — ingen nye agenter")]
    Closing,
}

impl From<AgentError> for String {
    fn from(e: AgentError) -> Self {
        e.to_string()
    }
}

/// Milliseconds since the Unix epoch (0 if the clock is before 1970).
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
