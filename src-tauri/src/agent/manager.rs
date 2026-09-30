//! Registry of agents. Knows nothing about Tauri: output and exits go to an [`EventSink`].
//!
//! Lives in `Arc<std::sync::Mutex<AgentManager>>`; hold the lock briefly and never while emitting.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use super::pty::{self, PtyHandle, SpawnSpec};
use super::ring_buffer::RingBuffer;
use super::{now_ms, AgentError};
use crate::config::{
    AGENT_ID_ENV, DEFAULT_TOOL_WHITELIST, MAX_STAFF_AGENTS, OUTPUT_RING_CAPACITY, PIPE_ENV,
    PTY_COLS, PTY_ROWS, STARTING_HINT_AFTER, STARTING_HINT_TEXT,
};
use crate::hooks::status::AgentStatus;

/// uuid v4 as a string.
pub type AgentId = String;

/// Visual role of an agent (step 2: only the figure and the default folder name depend on it).
/// Wire: `"none"|"coder"|"researcher"|"reviewer"|"koord"`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum AgentRole {
    #[default]
    None,
    Coder,
    Researcher,
    Reviewer,
    Koord,
}

impl AgentRole {
    /// Prefix of the default folder name (`bot-01`, `coder-02`, …).
    pub fn prefix(&self) -> &'static str {
        match self {
            AgentRole::None => "bot",
            AgentRole::Coder => "coder",
            AgentRole::Researcher => "researcher",
            AgentRole::Reviewer => "reviewer",
            AgentRole::Koord => "koord",
        }
    }
}

/// Which row of seats an agent occupies; each has its own limit. Wire: `"work"|"staff"`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SeatKind {
    #[default]
    Work,
    Staff,
}

/// How a hook frame was matched to an agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchVia {
    /// By the frame-level `agent_id` (`MIRA_AGENT_ID`).
    AgentId,
    /// By `session_id` (case-insensitive).
    Session,
}

/// Result of [`AgentManager::match_frame`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameMatch {
    pub agent_id: AgentId,
    pub via: MatchVia,
    /// The agent's session id was changed to the frame's (e.g. after `/clear`).
    pub rebound: bool,
}

/// Identity of an agent that is about to be spawned (ids are generated before the spawn, because
/// the agent id goes into the child's environment).
#[derive(Clone, Debug)]
pub(crate) struct AgentMeta {
    pub id: AgentId,
    pub session_id: String,
    pub role: AgentRole,
    pub seat_kind: SeatKind,
}

/// Key of the session map: session ids are compared case-insensitively (research2 §2).
fn session_key(session_id: &str) -> String {
    session_id.to_ascii_lowercase()
}

/// Wire shape of an agent (C.2 `AgentInfo`), camelCase.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentInfo {
    pub id: AgentId,
    pub session_id: String,
    /// Last component of `cwd`.
    pub name: String,
    pub cwd: String,
    pub status: AgentStatus,
    pub detail: Option<String>,
    pub pid: Option<u32>,
    /// Unix ms.
    pub created_at: u64,
    /// Unix ms.
    pub last_event_at: u64,
    pub role: AgentRole,
    pub seat_kind: SeatKind,
}

/// What the manager reports from its PTY threads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SinkEvent {
    /// `seq` is the ring buffer's byte counter after this chunk.
    Output {
        agent_id: AgentId,
        seq: u64,
        bytes: Vec<u8>,
    },
    /// The child exited. The receiver should call [`AgentManager::mark_exited`].
    Exited {
        agent_id: AgentId,
        code: Option<i32>,
    },
}

/// Called from PTY threads (never with the manager lock held by the caller's thread).
pub type EventSink = Arc<dyn Fn(SinkEvent) + Send + Sync>;

#[derive(Clone, Debug)]
pub struct SpawnRequest {
    pub cwd: PathBuf,
    /// Optional first prompt, passed as claude's positional argument after the flags. A prompt
    /// that starts with `-` (after leading whitespace) is refused with
    /// [`AgentError::InvalidPrompt`] instead of being rewritten, since claude would parse it as a
    /// flag. Empty/whitespace-only prompts are omitted.
    pub prompt: Option<String>,
    pub role: AgentRole,
    pub seat_kind: SeatKind,
}

/// Whether `prompt` would be parsed as a flag by claude (see [`SpawnRequest::prompt`]).
fn prompt_looks_like_flag(prompt: Option<&str>) -> bool {
    prompt.is_some_and(|p| p.trim_start().starts_with('-'))
}

#[derive(Clone, Debug)]
pub struct SpawnContext {
    pub claude: PathBuf,
    pub hooks_json: PathBuf,
    pub pipe_name: String,
}

pub struct Agent {
    pub info: AgentInfo,
    pty: Option<PtyHandle>,
    pub output: Arc<Mutex<RingBuffer>>,
    pub whitelist: Vec<String>,
}

pub struct AgentManager {
    agents: HashMap<AgentId, Agent>,
    /// Lower-cased session id → agent.
    by_session: HashMap<String, AgentId>,
    max_work: usize,
    max_staff: usize,
}

fn is_exited(s: &AgentStatus) -> bool {
    matches!(s, AgentStatus::Exited { .. })
}

fn name_for(cwd: &Path) -> String {
    cwd.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| cwd.to_string_lossy().into_owned())
}

/// Command line for one interactive claude session:
/// `claude --settings <hooks.json> --session-id <uuid> [prompt]`, env `MIRA_BOTS_PIPE` and
/// `MIRA_AGENT_ID`. No `-p`, no `--permission-mode`, no `--setting-sources`.
pub fn build_spawn_spec(
    req: &SpawnRequest,
    ctx: &SpawnContext,
    session_id: &str,
    agent_id: &str,
) -> SpawnSpec {
    let mut args = vec![
        "--settings".to_string(),
        ctx.hooks_json.to_string_lossy().into_owned(),
        "--session-id".to_string(),
        session_id.to_string(),
    ];
    if let Some(p) = req.prompt.as_ref().filter(|p| !p.trim().is_empty()) {
        args.push(p.clone());
    }
    SpawnSpec {
        program: ctx.claude.clone(),
        args,
        cwd: req.cwd.clone(),
        // TODO(windows-verify): MIRA_BOTS_PIPE set on claude.exe is inherited by the hook
        // processes claude starts (plan D.6).
        // TODO(windows-verify): MIRA_AGENT_ID is inherited by mira-hook.exe too, the frame carries
        // `agent_id`, and after `/clear` the agent's sessionId follows (plan D.15).
        env: vec![
            (PIPE_ENV.to_string(), ctx.pipe_name.clone()),
            (AGENT_ID_ENV.to_string(), agent_id.to_string()),
        ],
        cols: PTY_COLS,
        rows: PTY_ROWS,
    }
}

impl AgentManager {
    /// `max_work` work seats and [`MAX_STAFF_AGENTS`] staff seats.
    pub fn new(max_work: usize) -> Self {
        Self::with_limits(max_work, MAX_STAFF_AGENTS)
    }

    pub fn with_limits(max_work: usize, max_staff: usize) -> Self {
        Self {
            agents: HashMap::new(),
            by_session: HashMap::new(),
            max_work,
            max_staff,
        }
    }

    /// Agents that have not exited, all seat kinds.
    pub fn running_count(&self) -> usize {
        self.agents
            .values()
            .filter(|a| !is_exited(&a.info.status))
            .count()
    }

    fn running_in(&self, seat: SeatKind) -> usize {
        self.agents
            .values()
            .filter(|a| a.info.seat_kind == seat && !is_exited(&a.info.status))
            .count()
    }

    /// Only non-exited agents of the same seat kind count.
    fn check_limit(&self, seat: SeatKind) -> Result<(), AgentError> {
        let max = match seat {
            SeatKind::Work => self.max_work,
            SeatKind::Staff => self.max_staff,
        };
        if self.running_in(seat) >= max {
            return Err(AgentError::LimitReached(seat));
        }
        Ok(())
    }

    /// Working folders of every known agent (exited included), for
    /// [`super::workdir::next_agent_dir`].
    pub fn cwds(&self) -> Vec<PathBuf> {
        self.agents
            .values()
            .map(|a| PathBuf::from(&a.info.cwd))
            .collect()
    }

    /// Starts `claude` in `req.cwd`. Checks, in order: seat limit, prompt does not start with
    /// `-`, cwd is a directory, claude binary exists.
    pub fn spawn(
        &mut self,
        req: SpawnRequest,
        ctx: &SpawnContext,
        sink: EventSink,
    ) -> Result<AgentInfo, AgentError> {
        self.check_limit(req.seat_kind)?;
        if prompt_looks_like_flag(req.prompt.as_deref()) {
            return Err(AgentError::InvalidPrompt);
        }
        if !req.cwd.is_dir() {
            return Err(AgentError::InvalidCwd);
        }
        if !ctx.claude.is_file() {
            return Err(AgentError::ClaudeNotFound);
        }
        let meta = AgentMeta {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: uuid::Uuid::new_v4().to_string(),
            role: req.role,
            seat_kind: req.seat_kind,
        };
        let spec = build_spawn_spec(&req, ctx, &meta.session_id, &meta.id);
        self.spawn_spec(spec, meta, sink)
    }

    /// Spawns an arbitrary [`SpawnSpec`] as an agent (used by `spawn`, and directly by tests).
    pub(crate) fn spawn_spec(
        &mut self,
        spec: SpawnSpec,
        meta: AgentMeta,
        sink: EventSink,
    ) -> Result<AgentInfo, AgentError> {
        self.check_limit(meta.seat_kind)?;
        let AgentMeta {
            id,
            session_id,
            role,
            seat_kind,
        } = meta;
        let output = Arc::new(Mutex::new(RingBuffer::new(OUTPUT_RING_CAPACITY)));

        let on_output = {
            let output = Arc::clone(&output);
            let sink = Arc::clone(&sink);
            let agent_id = id.clone();
            move |bytes: &[u8]| {
                let seq = {
                    let mut rb = output.lock().unwrap_or_else(|p| p.into_inner());
                    rb.push(bytes);
                    rb.seq()
                };
                sink(SinkEvent::Output {
                    agent_id: agent_id.clone(),
                    seq,
                    bytes: bytes.to_vec(),
                });
            }
        };
        let on_exit = {
            let agent_id = id.clone();
            move |code: Option<i32>| sink(SinkEvent::Exited { agent_id, code })
        };
        let handle = pty::spawn(&spec, on_output, on_exit)?;

        let now = now_ms();
        let info = AgentInfo {
            id: id.clone(),
            session_id: session_id.clone(),
            name: name_for(&spec.cwd),
            cwd: spec.cwd.to_string_lossy().into_owned(),
            status: AgentStatus::Starting,
            detail: None,
            pid: handle.pid(),
            created_at: now,
            last_event_at: now,
            role,
            seat_kind,
        };
        self.insert(info.clone(), Some(handle), output);
        Ok(info)
    }

    fn insert(&mut self, info: AgentInfo, pty: Option<PtyHandle>, output: Arc<Mutex<RingBuffer>>) {
        self.by_session
            .insert(session_key(&info.session_id), info.id.clone());
        self.agents.insert(
            info.id.clone(),
            Agent {
                info,
                pty,
                output,
                whitelist: DEFAULT_TOOL_WHITELIST
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            },
        );
    }

    fn agent_mut(&mut self, id: &str) -> Result<&mut Agent, AgentError> {
        self.agents.get_mut(id).ok_or(AgentError::NotFound)
    }

    /// Kills the child and marks it `Exited{code: None}` right away; the waiter thread's
    /// `SinkEvent::Exited` later fills in the real code via [`Self::mark_exited`].
    pub fn stop(&mut self, id: &str) -> Result<AgentInfo, AgentError> {
        let agent = self.agent_mut(id)?;
        if let Some(pty) = agent.pty.as_mut() {
            if let Err(e) = pty.kill() {
                // Usually "already exited"; the status below is what matters.
                log::debug!("kill agent {id}: {e}");
            }
        }
        if !is_exited(&agent.info.status) {
            agent.info.status = AgentStatus::Exited { code: None };
            agent.info.detail = None;
            agent.info.last_event_at = now_ms();
        }
        Ok(agent.info.clone())
    }

    /// Records the exit reported by the waiter thread and takes the PTY out of the agent.
    /// Returns the updated info and the PTY handle, which the caller must drop **after** releasing
    /// the manager lock: dropping it closes the pseudo terminal (ConPTY `ClosePseudoConsole` can
    /// block until output is drained), which also lets a ConPTY reader thread finish.
    pub fn mark_exited(
        &mut self,
        id: &str,
        code: Option<i32>,
    ) -> Option<(AgentInfo, Option<PtyHandle>)> {
        let agent = self.agents.get_mut(id)?;
        // Keep a known code if stop() raced ahead with None; otherwise take the reported one.
        let keep =
            matches!(agent.info.status, AgentStatus::Exited { code: Some(_) }) && code.is_none();
        if !keep {
            agent.info.status = AgentStatus::Exited { code };
        }
        agent.info.detail = None;
        agent.info.last_event_at = now_ms();
        let pty = agent.pty.take();
        Some((agent.info.clone(), pty))
    }

    /// Kills every child (app exit / `quit_app`). Idempotent; errors are only logged.
    pub fn kill_all(&mut self) {
        for (id, agent) in &mut self.agents {
            if let Some(pty) = agent.pty.as_mut() {
                if let Err(e) = pty.kill() {
                    log::debug!("kill_all: agent {id}: {e}");
                }
            }
        }
    }

    /// Removes an exited agent. Returns its PTY handle if it still had one (stopped, waiter not
    /// yet reported); like with [`Self::mark_exited`], drop it after releasing the manager lock.
    pub fn remove(&mut self, id: &str) -> Result<Option<PtyHandle>, AgentError> {
        let agent = self.agents.get(id).ok_or(AgentError::NotFound)?;
        if !is_exited(&agent.info.status) {
            return Err(AgentError::StillRunning);
        }
        let session_id = agent.info.session_id.clone();
        let removed = self.agents.remove(id);
        self.by_session.remove(&session_key(&session_id));
        Ok(removed.and_then(|mut a| a.pty.take()))
    }

    pub fn write_input(&mut self, id: &str, bytes: &[u8]) -> Result<(), AgentError> {
        match self.agent_mut(id)?.pty.as_mut() {
            Some(pty) => pty.write(bytes),
            None => Err(AgentError::NotFound),
        }
    }

    pub fn resize(&mut self, id: &str, cols: u16, rows: u16) -> Result<(), AgentError> {
        match self.agent_mut(id)?.pty.as_ref() {
            Some(pty) => pty.resize(cols, rows),
            None => Err(AgentError::NotFound),
        }
    }

    /// `(seq, bytes)` of the agent's retained output.
    pub fn output_snapshot(&self, id: &str) -> Result<(u64, Vec<u8>), AgentError> {
        let agent = self.agents.get(id).ok_or(AgentError::NotFound)?;
        let rb = agent.output.lock().unwrap_or_else(|p| p.into_inner());
        Ok(rb.snapshot())
    }

    /// Sets status and detail from a hook transition. An exited agent never comes back to life
    /// (late hook events after exit are ignored). Returns the updated info.
    pub fn set_status(
        &mut self,
        id: &str,
        status: AgentStatus,
        detail: Option<String>,
    ) -> Option<AgentInfo> {
        let agent = self.agents.get_mut(id)?;
        if is_exited(&agent.info.status) && !is_exited(&status) {
            return None;
        }
        agent.info.status = status;
        agent.info.detail = detail;
        agent.info.last_event_at = now_ms();
        Some(agent.info.clone())
    }

    /// Case-insensitive.
    pub fn agent_id_for_session(&self, session_id: &str) -> Option<AgentId> {
        self.by_session.get(&session_key(session_id)).cloned()
    }

    /// Matches a hook frame to an agent: the frame-level `agent_hint` first (if that agent
    /// exists), otherwise `session_id`. On a hint match with a different, non-empty `session_id`
    /// the agent is rebound: its old `by_session` key is removed, the new one inserted and
    /// `info.session_id` updated (happens after `/clear`; harmless for exited agents).
    pub fn match_frame(
        &mut self,
        agent_hint: Option<&str>,
        session_id: &str,
    ) -> Option<FrameMatch> {
        if let Some(agent) = agent_hint.and_then(|h| self.agents.get_mut(h)) {
            let id = agent.info.id.clone();
            let rebound =
                !session_id.is_empty() && !agent.info.session_id.eq_ignore_ascii_case(session_id);
            if rebound {
                let old = std::mem::replace(&mut agent.info.session_id, session_id.to_string());
                let old_key = session_key(&old);
                if self.by_session.get(&old_key) == Some(&id) {
                    self.by_session.remove(&old_key);
                }
                self.by_session.insert(session_key(session_id), id.clone());
            }
            return Some(FrameMatch {
                agent_id: id,
                via: MatchVia::AgentId,
                rebound,
            });
        }
        self.agent_id_for_session(session_id)
            .map(|agent_id| FrameMatch {
                agent_id,
                via: MatchVia::Session,
                rebound: false,
            })
    }

    /// Sets [`STARTING_HINT_TEXT`] as detail when the agent is still `Starting` without a detail
    /// [`STARTING_HINT_AFTER`] after it was created (no hook event yet, usually the trust dialog).
    /// `last_event_at` is left alone. Returns the updated info if the hint was set.
    pub fn apply_starting_hint(&mut self, id: &str, now_ms: u64) -> Option<AgentInfo> {
        let agent = self.agents.get_mut(id)?;
        let due =
            now_ms.saturating_sub(agent.info.created_at) >= STARTING_HINT_AFTER.as_millis() as u64;
        if agent.info.status != AgentStatus::Starting || agent.info.detail.is_some() || !due {
            return None;
        }
        agent.info.detail = Some(STARTING_HINT_TEXT.to_string());
        Some(agent.info.clone())
    }

    /// Removes the Starting hint (first hook event for the agent, even one that leaves the status
    /// unchanged). Returns whether it was removed.
    pub fn clear_starting_hint(&mut self, id: &str) -> bool {
        match self.agents.get_mut(id) {
            Some(a)
                if a.info.status == AgentStatus::Starting
                    && a.info.detail.as_deref() == Some(STARTING_HINT_TEXT) =>
            {
                a.info.detail = None;
                true
            }
            _ => false,
        }
    }

    pub fn get(&self, id: &str) -> Option<AgentInfo> {
        self.agents.get(id).map(|a| a.info.clone())
    }

    /// All agents, oldest first.
    pub fn list(&self) -> Vec<AgentInfo> {
        let mut v: Vec<AgentInfo> = self.agents.values().map(|a| a.info.clone()).collect();
        v.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        v
    }

    /// Whole-name match on `tool_name` ("Bash" allows every Bash command).
    pub fn whitelist_contains(&self, id: &str, tool: &str) -> bool {
        self.agents
            .get(id)
            .is_some_and(|a| a.whitelist.iter().any(|t| t == tool))
    }

    pub fn whitelist_add(&mut self, id: &str, tool: &str) -> Result<(), AgentError> {
        let agent = self.agent_mut(id)?;
        if !agent.whitelist.iter().any(|t| t == tool) {
            agent.whitelist.push(tool.to_string());
        }
        Ok(())
    }

    /// Test helper: an agent without a PTY (role none, work seat).
    #[cfg(test)]
    pub fn insert_fake(&mut self, session_id: &str, cwd: &str) -> AgentId {
        self.insert_fake_with(session_id, cwd, AgentRole::None, SeatKind::Work)
    }

    /// Test helper: an agent without a PTY.
    #[cfg(test)]
    pub fn insert_fake_with(
        &mut self,
        session_id: &str,
        cwd: &str,
        role: AgentRole,
        seat_kind: SeatKind,
    ) -> AgentId {
        let now = now_ms();
        let id = uuid::Uuid::new_v4().to_string();
        let info = AgentInfo {
            id: id.clone(),
            session_id: session_id.to_string(),
            name: name_for(Path::new(cwd)),
            cwd: cwd.to_string(),
            status: AgentStatus::Starting,
            detail: None,
            pid: None,
            created_at: now,
            last_event_at: now,
            role,
            seat_kind,
        };
        self.insert(info, None, Arc::new(Mutex::new(RingBuffer::new(1024))));
        id
    }

    /// Test helper: moves the agent's creation time into the past.
    #[cfg(test)]
    pub fn backdate(&mut self, id: &str, ms: u64) {
        if let Some(a) = self.agents.get_mut(id) {
            a.info.created_at = a.info.created_at.saturating_sub(ms);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx(claude: PathBuf) -> SpawnContext {
        SpawnContext {
            claude,
            hooks_json: PathBuf::from("/data/hooks.json"),
            pipe_name: "pipe-x".into(),
        }
    }

    fn null_sink() -> EventSink {
        Arc::new(|_| {})
    }

    #[test]
    fn spawn_spec_command_line() {
        let req = SpawnRequest {
            cwd: PathBuf::from("/w/demo"),
            prompt: Some("fix it".into()),
            role: AgentRole::None,
            seat_kind: SeatKind::Work,
        };
        let spec = build_spawn_spec(&req, &ctx(PathBuf::from("/bin/claude")), "sid", "aid");
        assert_eq!(spec.program, PathBuf::from("/bin/claude"));
        assert_eq!(
            spec.args,
            [
                "--settings",
                "/data/hooks.json",
                "--session-id",
                "sid",
                "fix it"
            ]
        );
        assert_eq!(spec.cwd, PathBuf::from("/w/demo"));
        assert_eq!(
            spec.env,
            vec![
                ("MIRA_BOTS_PIPE".to_string(), "pipe-x".to_string()),
                ("MIRA_AGENT_ID".to_string(), "aid".to_string()),
            ]
        );
        assert_eq!((spec.cols, spec.rows), (120, 30));
        for bad in [
            "-p",
            "--print",
            "--permission-mode",
            "--dangerously-skip-permissions",
        ] {
            assert!(!spec.args.iter().any(|a| a == bad));
        }
    }

    #[test]
    fn spawn_spec_omits_empty_prompt() {
        for prompt in [None, Some(String::new()), Some("   ".into())] {
            let req = SpawnRequest {
                cwd: PathBuf::from("/w"),
                prompt,
                role: AgentRole::None,
                seat_kind: SeatKind::Work,
            };
            let spec = build_spawn_spec(&req, &ctx(PathBuf::from("c")), "s", "a");
            assert_eq!(spec.args.len(), 4);
        }
    }

    #[test]
    fn limit_counts_only_non_exited_agents() {
        let mut m = AgentManager::new(5);
        let ids: Vec<_> = (0..5)
            .map(|i| m.insert_fake(&format!("s{i}"), "/w/a"))
            .collect();
        let req = || SpawnRequest {
            cwd: PathBuf::from("/definitely/not/a/dir"),
            prompt: None,
            role: AgentRole::None,
            seat_kind: SeatKind::Work,
        };
        let c = ctx(PathBuf::from("/nope/claude"));
        assert!(matches!(
            m.spawn(req(), &c, null_sink()),
            Err(AgentError::LimitReached(SeatKind::Work))
        ));
        m.mark_exited(&ids[0], Some(0));
        // Past the limit now; fails on the next check instead.
        assert!(matches!(
            m.spawn(req(), &c, null_sink()),
            Err(AgentError::InvalidCwd)
        ));
        assert_eq!(
            AgentError::LimitReached(SeatKind::Work).to_string(),
            "Loft på 5 arbejdspladser nået"
        );
        assert_eq!(
            AgentError::LimitReached(SeatKind::Staff).to_string(),
            "Loft på 2 stabspladser nået"
        );
    }

    fn doomed(seat_kind: SeatKind) -> SpawnRequest {
        // Passes the limit check, then fails on the cwd check (nothing is started).
        SpawnRequest {
            cwd: PathBuf::from("/definitely/not/a/dir"),
            prompt: None,
            role: AgentRole::Coder,
            seat_kind,
        }
    }

    #[test]
    fn staff_and_work_limits_are_separate() {
        let mut m = AgentManager::new(5);
        let c = ctx(PathBuf::from("/nope/claude"));
        for i in 0..5 {
            m.insert_fake(&format!("w{i}"), "/w/a");
        }
        assert!(matches!(
            m.spawn(doomed(SeatKind::Work), &c, null_sink()),
            Err(AgentError::LimitReached(SeatKind::Work))
        ));
        // Five work agents do not block staff.
        assert!(matches!(
            m.spawn(doomed(SeatKind::Staff), &c, null_sink()),
            Err(AgentError::InvalidCwd)
        ));
        let s0 = m.insert_fake_with("s0", "/w/s", AgentRole::Koord, SeatKind::Staff);
        m.insert_fake_with("s1", "/w/s", AgentRole::Reviewer, SeatKind::Staff);
        assert!(matches!(
            m.spawn(doomed(SeatKind::Staff), &c, null_sink()),
            Err(AgentError::LimitReached(SeatKind::Staff))
        ));
        m.mark_exited(&s0, Some(0));
        assert!(matches!(
            m.spawn(doomed(SeatKind::Staff), &c, null_sink()),
            Err(AgentError::InvalidCwd)
        ));
        // Staff agents never count against the work limit.
        let mut m = AgentManager::with_limits(1, 2);
        m.insert_fake_with("s0", "/w/s", AgentRole::None, SeatKind::Staff);
        m.insert_fake_with("s1", "/w/s", AgentRole::None, SeatKind::Staff);
        assert!(matches!(
            m.spawn(doomed(SeatKind::Work), &c, null_sink()),
            Err(AgentError::InvalidCwd)
        ));
        assert_eq!(m.running_count(), 2);
    }

    #[test]
    fn role_and_seat_kind_serde_lowercase() {
        for (role, s) in [
            (AgentRole::None, "none"),
            (AgentRole::Coder, "coder"),
            (AgentRole::Researcher, "researcher"),
            (AgentRole::Reviewer, "reviewer"),
            (AgentRole::Koord, "koord"),
        ] {
            assert_eq!(serde_json::to_value(role).unwrap(), json!(s));
            assert_eq!(serde_json::from_value::<AgentRole>(json!(s)).unwrap(), role);
        }
        for (seat, s) in [(SeatKind::Work, "work"), (SeatKind::Staff, "staff")] {
            assert_eq!(serde_json::to_value(seat).unwrap(), json!(s));
            assert_eq!(serde_json::from_value::<SeatKind>(json!(s)).unwrap(), seat);
        }
        assert!(serde_json::from_value::<AgentRole>(json!("Coder")).is_err());
        assert_eq!(AgentRole::default(), AgentRole::None);
        assert_eq!(SeatKind::default(), SeatKind::Work);
        assert_eq!(AgentRole::None.prefix(), "bot");
        assert_eq!(AgentRole::Koord.prefix(), "koord");
    }

    #[test]
    fn match_frame_hint_first_then_session() {
        let mut m = AgentManager::new(5);
        let a = m.insert_fake("sess-A", "/w/a");
        let b = m.insert_fake("sess-b", "/w/b");
        // Hint + same session (case-insensitive): no rebind.
        let r = m.match_frame(Some(&a), "SESS-a").unwrap();
        assert_eq!(
            (r.agent_id.as_str(), r.via, r.rebound),
            (a.as_str(), MatchVia::AgentId, false)
        );
        // Unknown hint falls back to the session.
        let r = m.match_frame(Some("nope"), "sess-b").unwrap();
        assert_eq!(
            (r.agent_id.as_str(), r.via, r.rebound),
            (b.as_str(), MatchVia::Session, false)
        );
        // No hint, case-insensitive session.
        assert_eq!(m.match_frame(None, "SESS-B").unwrap().agent_id, b);
        // Neither known.
        assert_eq!(m.match_frame(Some("nope"), "other"), None);
        assert_eq!(m.match_frame(None, "other"), None);
        // Empty session id never rebinds.
        let r = m.match_frame(Some(&a), "").unwrap();
        assert!(!r.rebound);
        assert_eq!(m.get(&a).unwrap().session_id, "sess-A");
    }

    #[test]
    fn match_frame_rebinds_the_session_after_clear() {
        let mut m = AgentManager::new(5);
        let a = m.insert_fake("sess-1", "/w/a");
        let r = m.match_frame(Some(&a), "New-Sess").unwrap();
        assert_eq!((r.via, r.rebound), (MatchVia::AgentId, true));
        assert_eq!(m.get(&a).unwrap().session_id, "New-Sess");
        assert_eq!(m.agent_id_for_session("new-sess"), Some(a.clone()));
        assert_eq!(m.agent_id_for_session("sess-1"), None);
        // Exited agents are rebound too; removing them clears the new key.
        m.stop(&a).unwrap();
        assert!(m.match_frame(Some(&a), "third").unwrap().rebound);
        m.remove(&a).unwrap();
        assert_eq!(m.agent_id_for_session("third"), None);
        assert_eq!(m.agent_id_for_session("new-sess"), None);
    }

    #[test]
    fn starting_hint_after_15_s_without_events() {
        let mut m = AgentManager::new(5);
        let a = m.insert_fake("s", "/w/a");
        let created = m.get(&a).unwrap().created_at;
        let last = m.get(&a).unwrap().last_event_at;
        // Not before 15 s.
        assert!(m.apply_starting_hint(&a, created + 14_999).is_none());
        let info = m.apply_starting_hint(&a, created + 15_000).unwrap();
        assert_eq!(info.detail.as_deref(), Some(STARTING_HINT_TEXT));
        assert_eq!(info.status, AgentStatus::Starting);
        assert_eq!(info.last_event_at, last, "the hint is not a hook event");
        // Only once.
        assert!(m.apply_starting_hint(&a, created + 60_000).is_none());
        // A hook transition replaces it.
        let info = m.set_status(&a, AgentStatus::Idle, None).unwrap();
        assert_eq!(info.detail, None);
        // Not when the status is no longer Starting.
        assert!(m.apply_starting_hint(&a, created + 60_000).is_none());
        // Not when a detail is already set.
        let b = m.insert_fake("s2", "/w/b");
        m.set_status(&b, AgentStatus::Starting, Some("x".into()))
            .unwrap();
        assert!(m.apply_starting_hint(&b, now_ms() + 60_000).is_none());
        assert!(m.apply_starting_hint("nope", now_ms()).is_none());
    }

    #[test]
    fn first_hook_event_clears_the_starting_hint() {
        let mut m = AgentManager::new(5);
        let a = m.insert_fake("s", "/w/a");
        assert!(!m.clear_starting_hint(&a), "no hint yet");
        m.backdate(&a, 20_000);
        assert!(m.apply_starting_hint(&a, now_ms()).is_some());
        assert!(m.clear_starting_hint(&a));
        assert_eq!(m.get(&a).unwrap().detail, None);
        assert_eq!(m.get(&a).unwrap().status, AgentStatus::Starting);
        assert!(!m.clear_starting_hint(&a));
    }

    #[test]
    fn cwds_lists_every_agent() {
        let mut m = AgentManager::new(5);
        let a = m.insert_fake("s", "/w/bot-01");
        m.insert_fake("t", "/w/bot-02");
        m.stop(&a).unwrap();
        let mut c = m.cwds();
        c.sort();
        assert_eq!(c, [PathBuf::from("/w/bot-01"), PathBuf::from("/w/bot-02")]);
    }

    #[test]
    fn spawn_rejects_prompt_that_looks_like_a_flag() {
        let mut m = AgentManager::new(5);
        let c = ctx(PathBuf::from("/nope/claude"));
        for prompt in [
            "-p hi",
            "--dangerously-skip-permissions",
            "  -x",
            "\t--print",
        ] {
            let req = SpawnRequest {
                cwd: std::env::temp_dir(),
                prompt: Some(prompt.into()),
                role: AgentRole::None,
                seat_kind: SeatKind::Work,
            };
            assert!(
                matches!(
                    m.spawn(req, &c, null_sink()),
                    Err(AgentError::InvalidPrompt)
                ),
                "{prompt:?}"
            );
        }
        // A dash later in the prompt is fine (fails on the next check instead).
        for prompt in [Some("fix -x flag"), Some("  "), None] {
            let req = SpawnRequest {
                cwd: std::env::temp_dir(),
                prompt: prompt.map(str::to_string),
                role: AgentRole::None,
                seat_kind: SeatKind::Work,
            };
            assert!(
                matches!(
                    m.spawn(req, &c, null_sink()),
                    Err(AgentError::ClaudeNotFound)
                ),
                "{prompt:?}"
            );
        }
        assert!(AgentError::InvalidPrompt
            .to_string()
            .starts_with("Prompten"));
        assert!(m.list().is_empty());
    }

    #[test]
    fn spawn_rejects_missing_claude() {
        let mut m = AgentManager::new(5);
        let req = SpawnRequest {
            cwd: std::env::temp_dir(),
            prompt: None,
            role: AgentRole::None,
            seat_kind: SeatKind::Work,
        };
        assert!(matches!(
            m.spawn(req, &ctx(PathBuf::from("/nope/claude")), null_sink()),
            Err(AgentError::ClaudeNotFound)
        ));
    }

    #[test]
    fn session_lookup_status_and_whitelist() {
        let mut m = AgentManager::new(5);
        let id = m.insert_fake("sess-1", "/w/demo");
        assert_eq!(m.agent_id_for_session("sess-1"), Some(id.clone()));
        assert_eq!(m.agent_id_for_session("other"), None);
        let info = m
            .set_status(&id, AgentStatus::Editing, Some("src/main.rs".into()))
            .unwrap();
        assert_eq!(info.status, AgentStatus::Editing);
        assert_eq!(info.name, "demo");
        assert!(m.set_status("nope", AgentStatus::Idle, None).is_none());

        assert!(!m.whitelist_contains(&id, "Bash"));
        m.whitelist_add(&id, "Bash").unwrap();
        m.whitelist_add(&id, "Bash").unwrap();
        assert!(m.whitelist_contains(&id, "Bash"));
        assert!(!m.whitelist_contains(&id, "Bas"));
        assert!(matches!(
            m.whitelist_add("nope", "Bash"),
            Err(AgentError::NotFound)
        ));
    }

    #[test]
    fn exited_agents_stay_exited_and_can_be_removed() {
        let mut m = AgentManager::new(5);
        let id = m.insert_fake("s", "/w/demo");
        assert!(matches!(m.remove(&id), Err(AgentError::StillRunning)));
        let info = m.stop(&id).unwrap();
        assert_eq!(info.status, AgentStatus::Exited { code: None });
        assert!(m.set_status(&id, AgentStatus::Thinking, None).is_none());
        assert_eq!(
            m.mark_exited(&id, Some(3)).unwrap().0.status,
            AgentStatus::Exited { code: Some(3) }
        );
        // A later None does not erase a known code.
        assert_eq!(
            m.mark_exited(&id, None).unwrap().0.status,
            AgentStatus::Exited { code: Some(3) }
        );
        assert!(m.mark_exited("nope", None).is_none());
        assert!(m.remove(&id).unwrap().is_none(), "fake agent has no PTY");
        assert!(m.list().is_empty());
        assert_eq!(m.agent_id_for_session("s"), None);
        assert!(matches!(m.remove(&id), Err(AgentError::NotFound)));
    }

    #[test]
    fn agent_info_serializes_camel_case() {
        let mut m = AgentManager::new(5);
        let id = m.insert_fake("s", "/w/demo");
        let v = serde_json::to_value(m.get(&id).unwrap()).unwrap();
        for key in [
            "id",
            "sessionId",
            "name",
            "cwd",
            "status",
            "detail",
            "pid",
            "createdAt",
            "lastEventAt",
            "role",
            "seatKind",
        ] {
            assert!(v.get(key).is_some(), "{key}");
        }
        assert_eq!(v["status"], json!({"kind":"starting"}));
        assert_eq!(v["role"], "none");
        assert_eq!(v["seatKind"], "work");
        let id = m.insert_fake_with("s2", "/w/x", AgentRole::Researcher, SeatKind::Staff);
        let v = serde_json::to_value(m.get(&id).unwrap()).unwrap();
        assert_eq!(
            (v["role"].clone(), v["seatKind"].clone()),
            (json!("researcher"), json!("staff"))
        );
    }

    #[cfg(unix)]
    mod unix_pty {
        use super::*;
        use std::time::{Duration, Instant};

        fn collecting_sink() -> (EventSink, Arc<Mutex<Vec<SinkEvent>>>) {
            let events = Arc::new(Mutex::new(Vec::new()));
            let e = Arc::clone(&events);
            (Arc::new(move |ev| e.lock().unwrap().push(ev)), events)
        }

        fn sh(script: &str, env: Vec<(String, String)>) -> SpawnSpec {
            SpawnSpec {
                program: PathBuf::from("/bin/sh"),
                args: vec!["-c".into(), script.into()],
                cwd: std::env::temp_dir(),
                env,
                cols: PTY_COLS,
                rows: PTY_ROWS,
            }
        }

        fn meta(session: &str) -> AgentMeta {
            AgentMeta {
                id: uuid::Uuid::new_v4().to_string(),
                session_id: session.to_string(),
                role: AgentRole::None,
                seat_kind: SeatKind::Work,
            }
        }

        fn wait_for_exit(events: &Arc<Mutex<Vec<SinkEvent>>>) -> Option<i32> {
            let t = Instant::now();
            while t.elapsed() < Duration::from_secs(10) {
                let found = events.lock().unwrap().iter().find_map(|e| match e {
                    SinkEvent::Exited { code, .. } => Some(*code),
                    _ => None,
                });
                if let Some(code) = found {
                    return code;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            panic!("no Exited event within 10 s");
        }

        fn output_text(events: &Arc<Mutex<Vec<SinkEvent>>>) -> String {
            let mut s = Vec::new();
            for e in events.lock().unwrap().iter() {
                if let SinkEvent::Output { bytes, .. } = e {
                    s.extend_from_slice(bytes);
                }
            }
            String::from_utf8_lossy(&s).into_owned()
        }

        /// Output can trail the exit notification slightly; wait until it contains `needle`.
        fn wait_for_output(events: &Arc<Mutex<Vec<SinkEvent>>>, needle: &str) -> String {
            let t = Instant::now();
            loop {
                let s = output_text(events);
                if s.contains(needle) || t.elapsed() > Duration::from_secs(5) {
                    return s;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        #[test]
        fn spawn_echo_reports_output_and_exit_code() {
            let mut m = AgentManager::new(5);
            let (sink, events) = collecting_sink();
            let info = m
                .spawn_spec(sh("echo hi; exit 3", vec![]), meta("sess"), sink)
                .unwrap();
            assert_eq!(info.status, AgentStatus::Starting);
            assert!(info.pid.is_some());
            assert_eq!(wait_for_exit(&events), Some(3));
            assert!(wait_for_output(&events, "hi").contains("hi"));
            let (seq, bytes) = m.output_snapshot(&info.id).unwrap();
            assert_eq!(seq as usize, bytes.len());
            assert!(String::from_utf8_lossy(&bytes).contains("hi"));
            // Every Output event carries the id and a monotonic seq.
            let seqs: Vec<u64> = events
                .lock()
                .unwrap()
                .iter()
                .filter_map(|e| match e {
                    SinkEvent::Output { agent_id, seq, .. } => {
                        assert_eq!(agent_id, &info.id);
                        Some(*seq)
                    }
                    _ => None,
                })
                .collect();
            assert!(seqs.windows(2).all(|w| w[0] < w[1]));
            let (exited, pty) = m.mark_exited(&info.id, Some(3)).unwrap();
            assert_eq!(exited.status, AgentStatus::Exited { code: Some(3) });
            assert!(pty.is_some(), "the PTY handle is handed to the caller");
            assert_eq!(
                m.get(&info.id).unwrap().status,
                AgentStatus::Exited { code: Some(3) }
            );
            // Dropped here, outside any manager lock; a second report has nothing left to hand.
            drop(pty);
            let (_, again) = m.mark_exited(&info.id, None).unwrap();
            assert!(again.is_none());
            assert!(m.remove(&info.id).unwrap().is_none());
        }

        #[test]
        fn pipe_env_reaches_the_child() {
            let mut m = AgentManager::new(5);
            let (sink, events) = collecting_sink();
            let env = vec![(PIPE_ENV.to_string(), "/tmp/mira-bots-42.sock".to_string())];
            m.spawn_spec(
                sh("printf '<%s>' \"$MIRA_BOTS_PIPE\"", env),
                meta("s"),
                sink,
            )
            .unwrap();
            assert_eq!(wait_for_exit(&events), Some(0));
            assert!(wait_for_output(&events, "<").contains("</tmp/mira-bots-42.sock>"));
        }

        #[test]
        fn agent_id_env_reaches_the_child() {
            let mut m = AgentManager::new(5);
            let (sink, events) = collecting_sink();
            let req = SpawnRequest {
                cwd: std::env::temp_dir(),
                prompt: None,
                role: AgentRole::None,
                seat_kind: SeatKind::Work,
            };
            let c = ctx(PathBuf::from("/bin/sh"));
            // The env the real spawn builds, on a shell command.
            let mut spec = build_spawn_spec(&req, &c, "sid", "agent-xyz");
            spec.args = vec!["-c".into(), "printf '<%s>' \"$MIRA_AGENT_ID\"".into()];
            let mut md = meta("s");
            md.id = "agent-xyz".into();
            let info = m.spawn_spec(spec, md, sink).unwrap();
            assert_eq!(info.id, "agent-xyz");
            assert_eq!(wait_for_exit(&events), Some(0));
            assert!(wait_for_output(&events, "<").contains("<agent-xyz>"));
        }

        #[test]
        fn write_input_resize_and_stop() {
            let mut m = AgentManager::new(5);
            let (sink, events) = collecting_sink();
            let info = m
                .spawn_spec(
                    sh("read line; echo got:$line; sleep 30", vec![]),
                    meta("s"),
                    sink,
                )
                .unwrap();
            m.resize(&info.id, 100, 40).unwrap();
            m.write_input(&info.id, b"ping\n").unwrap();
            assert!(wait_for_output(&events, "got:ping").contains("got:ping"));
            let stopped = m.stop(&info.id).unwrap();
            assert_eq!(stopped.status, AgentStatus::Exited { code: None });
            wait_for_exit(&events);
            // mark_exited was not called (no Tauri sink here), so remove hands back the PTY.
            let pty = m.remove(&info.id).unwrap();
            assert!(pty.is_some());
            assert!(m.get(&info.id).is_none());
        }

        #[test]
        fn running_limit_blocks_spawn_until_one_stops() {
            let mut m = AgentManager::new(2);
            let (sink, events) = collecting_sink();
            let a = m
                .spawn_spec(sh("sleep 30", vec![]), meta("a"), sink.clone())
                .unwrap();
            m.spawn_spec(sh("sleep 30", vec![]), meta("b"), sink.clone())
                .unwrap();
            assert!(matches!(
                m.spawn_spec(sh("true", vec![]), meta("c"), sink.clone()),
                Err(AgentError::LimitReached(SeatKind::Work))
            ));
            m.stop(&a.id).unwrap();
            let c = m.spawn_spec(sh("true", vec![]), meta("c"), sink).unwrap();
            assert_eq!(c.status, AgentStatus::Starting);
            for info in m.list() {
                let _ = m.stop(&info.id);
            }
            let _ = events;
        }
    }
}
