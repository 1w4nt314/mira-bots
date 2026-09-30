//! Work agents: one interactive `claude` per agent, running in a PTY.

pub mod claude_path;
pub mod manager;
pub mod pty;
pub mod ring_buffer;

pub use manager::{
    AgentId, AgentInfo, AgentManager, EventSink, SinkEvent, SpawnContext, SpawnRequest,
};

use crate::config::MAX_WORK_AGENTS;

/// Errors from agent operations. The messages are user-facing (Danish) because commands pass
/// them straight to the UI.
#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("Loft på {} agenter nået", MAX_WORK_AGENTS)]
    LimitReached,
    #[error("Mappen findes ikke eller er ikke en mappe")]
    InvalidCwd,
    #[error("Fandt ikke claude — installer Claude Code eller sæt MIRA_CLAUDE_PATH")]
    ClaudeNotFound,
    #[error("Agenten findes ikke")]
    NotFound,
    /// `remove` on an agent that has not exited (C.1 `remove_agent`).
    #[error("Agenten kører stadig")]
    StillRunning,
    #[error("Terminalfejl: {0}")]
    Pty(String),
    #[error("I/O-fejl: {0}")]
    Io(#[from] std::io::Error),
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
