//! Registry of agents. Knows nothing about Tauri: output and exits go to an [`EventSink`].
//!
//! Lives in `Arc<std::sync::Mutex<AgentManager>>`; hold the lock briefly and never while emitting.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use super::process;
use super::pty::{self, PtyHandle, SpawnSpec};
use super::ring_buffer::RingBuffer;
use super::roles::{self, Role};
use super::{now_ms, AgentError};
use crate::config::{
    AGENT_ID_ENV, DEFAULT_TOOL_WHITELIST, MAX_STAFF_AGENTS, OUTPUT_RING_CAPACITY, PIPE_ENV,
    PTY_COLS, PTY_ROWS, QUIT_KILL_BUDGET, RESTARTING_TEXT, ROLES_ENV, STARTING_HINT_AFTER,
    STARTING_HINT_TEXT,
};
use crate::hooks::status::AgentStatus;
use crate::profiles::model::ProfileSnapshot;

/// uuid v4 as a string.
pub type AgentId = String;

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
    pub profile: ProfileSnapshot,
    pub seat_kind: SeatKind,
    /// `AgentInfo.name` (plan4b A.1: `<prefix>-<nn>`, independent of the folder).
    pub name: String,
    /// `AgentInfo.project`.
    pub project: Option<String>,
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
    /// `<prefix>-<nn>` from the profile's roles (plan4b A.1); falls back to the last component
    /// of `cwd` when the spawn named none.
    pub name: String,
    pub cwd: String,
    pub status: AgentStatus,
    pub detail: Option<String>,
    pub pid: Option<u32>,
    /// Unix ms.
    pub created_at: u64,
    /// Unix ms.
    pub last_event_at: u64,
    /// The profile snapshot taken at spawn (plan5 A.1); roles never change during a session.
    pub profile_id: String,
    pub profile_name: String,
    pub roles: Vec<Role>,
    pub specialist: bool,
    /// The requested model (spawn/restart), overwritten by the observed one (statusLine,
    /// PostModelSwitch); `None` = Claude Code's default. See `model_observed`.
    pub model: Option<String>,
    /// Like `model`: requested effort, overwritten by the observed `effort.level`.
    pub effort: Option<String>,
    /// Whether `model` was observed from the session rather than requested.
    pub model_observed: bool,
    /// Open review assignments of this agent (set by the tickets glue, batch 2; 0 until then).
    pub open_reviews: usize,
    pub seat_kind: SeatKind,
    /// The agent's `inProgress` ticket. Only set through [`AgentManager::set_ticket_link`]
    /// (from the tickets glue); the manager knows nothing else about tickets.
    pub current_ticket_id: Option<String>,
    /// Number of queued (`assigned`) tickets; see `current_ticket_id`.
    pub queue_length: usize,
    /// The project folder the agent works in (work seat, plan4b A.1); `None` for staff agents
    /// (cwd = the projects root) and agents started outside a project.
    pub project: Option<String>,
}

/// What the manager reports from its PTY threads. `gen` is the agent's PTY generation the
/// event comes from: a restart (`--resume`) starts a new generation, and events of an older one
/// are ignored ([`AgentManager::mark_exited`]; output of an old child is never even reported).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SinkEvent {
    /// `seq` is the ring buffer's byte counter after this chunk.
    Output {
        agent_id: AgentId,
        gen: u64,
        seq: u64,
        bytes: Vec<u8>,
    },
    /// The child exited. The receiver should call [`AgentManager::mark_exited`].
    Exited {
        agent_id: AgentId,
        gen: u64,
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
    pub seat_kind: SeatKind,
    /// Profile id/name, roles, specialist and the requested model/effort.
    pub profile: ProfileSnapshot,
    /// The agent's name; `None` → the last component of `cwd`.
    pub name: Option<String>,
    /// The agent's project (`AgentInfo.project`).
    pub project: Option<String>,
}

/// Whether `prompt` would be parsed as a flag by claude (see [`SpawnRequest::prompt`]).
fn prompt_looks_like_flag(prompt: Option<&str>) -> bool {
    prompt.is_some_and(|p| p.trim_start().starts_with('-'))
}

#[derive(Clone, Debug)]
pub struct SpawnContext {
    pub claude: PathBuf,
    /// The profile's settings.json (`<app_data>/profiles/<id>/settings.json`: hooks,
    /// permissions, model, effortLevel, statusLine), passed with `--settings`.
    pub settings_json: PathBuf,
    /// mcp.json, passed with `--mcp-config`; `None` when mira-mcp was not found.
    pub mcp_config: Option<PathBuf>,
    /// The profile's system-prompt.md, passed with `--append-system-prompt-file`; `None`
    /// without mira-mcp (the prompt asks for tools that would not exist).
    pub system_prompt: Option<PathBuf>,
    pub pipe_name: String,
}

pub struct Agent {
    pub info: AgentInfo,
    pty: Option<PtyHandle>,
    /// Current PTY generation (see [`SinkEvent`]); shared with the reader closure of the child,
    /// which drops output once a newer generation runs.
    pty_gen: Arc<AtomicU64>,
    pub output: Arc<Mutex<RingBuffer>>,
    pub whitelist: Vec<String>,
    /// When the user last typed into the terminal (`write_user_input`, ms since epoch). The
    /// ticket dispatcher waits a grace period after it so it never types into a half-written
    /// prompt. The dispatcher's own writes (`write_input`) do not touch it.
    last_user_input_at: Option<u64>,
    /// The current session has had a turn (a `UserPromptSubmit` or `Stop` was seen for it), so
    /// Claude Code has written its transcript and `--resume` can find it. Reset when the
    /// session id changes (`/clear`, a fresh restart). See [`AgentManager::restart_session`].
    has_conversation: bool,
    /// Set by a `--resume` restart (ms since epoch); a non-zero exit shortly after it, before the
    /// session started, means the conversation could not be resumed ([`RESTART_FAILED_TEXT`]).
    resume_started_at: Option<u64>,
    /// When the current child was started (spawn or restart, ms since epoch): the Starting hint
    /// counts from here ([`AgentManager::apply_starting_hint`]).
    started_at: u64,
    /// The detail a restart showed while `Starting` ([`RESTARTING_TEXT`] or the move text); the
    /// Starting hint may replace it. `None` after a plain spawn.
    start_text: Option<String>,
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

/// The flags shared by spawn and resume: `--settings <profile settings> [--mcp-config <mcp.json>]
/// [--append-system-prompt-file <profile prompt>] [--model <m>] [--effort <e>]`.
fn base_args(ctx: &SpawnContext, model: Option<&str>, effort: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "--settings".to_string(),
        ctx.settings_json.to_string_lossy().into_owned(),
    ];
    // TODO(windows-verify): the order below starts claude with a positional prompt and loads
    // the MCP server (plan4 D.39, D.43).
    if let Some(m) = &ctx.mcp_config {
        args.push("--mcp-config".to_string());
        args.push(m.to_string_lossy().into_owned());
    }
    if let Some(p) = &ctx.system_prompt {
        args.push("--append-system-prompt-file".to_string());
        args.push(p.to_string_lossy().into_owned());
    }
    if let Some(m) = model {
        args.push("--model".to_string());
        args.push(m.to_string());
    }
    if let Some(e) = effort {
        args.push("--effort".to_string());
        args.push(e.to_string());
    }
    args
}

fn spec_with(
    req: &SpawnRequest,
    ctx: &SpawnContext,
    args: Vec<String>,
    agent_id: &str,
) -> SpawnSpec {
    SpawnSpec {
        program: ctx.claude.clone(),
        args,
        cwd: req.cwd.clone(),
        // TODO(windows-verify): MIRA_BOTS_PIPE set on claude.exe is inherited by the hook
        // processes claude starts (plan D.6).
        // TODO(windows-verify): MIRA_AGENT_ID is inherited by mira-hook.exe too, the frame carries
        // `agent_id`, and after `/clear` the agent's sessionId follows (plan D.15).
        // TODO(windows-verify): MIRA_AGENT_ROLES reaches mira-mcp.exe (`${VAR}` in mcp.json), so
        // its tools/list is filtered (plan5 D.51).
        env: vec![
            (PIPE_ENV.to_string(), ctx.pipe_name.clone()),
            (AGENT_ID_ENV.to_string(), agent_id.to_string()),
            (ROLES_ENV.to_string(), roles::join_list(&req.profile.roles)),
        ],
        cols: PTY_COLS,
        rows: PTY_ROWS,
    }
}

/// Command line for one interactive claude session (C5.11):
/// `claude --settings <profile settings.json> [--mcp-config <mcp.json>]
/// [--append-system-prompt-file <profile system-prompt.md>] [--model <m>] [--effort <e>]
/// --session-id <uuid> [prompt]`, env `MIRA_BOTS_PIPE`, `MIRA_AGENT_ID` and `MIRA_AGENT_ROLES`
/// (`coder,reviewer`; empty for no roles). `--model`/`--effort` only when the snapshot has them.
/// No `-p`, no `--permission-mode`, no `--setting-sources`, no `--strict-mcp-config` (it would
/// drop the user's own MCP servers), no `ANTHROPIC_MODEL`/`CLAUDE_CODE_EFFORT_LEVEL`.
///
/// `--mcp-config` is variadic: a value directly after its path would be read as one more config
/// file ("MCP config file not found: …/<prompt>", research4 Q2). So its path is always followed
/// by another flag, and `--session-id` always stands last before the positional prompt.
pub fn build_spawn_spec(
    req: &SpawnRequest,
    ctx: &SpawnContext,
    session_id: &str,
    agent_id: &str,
) -> SpawnSpec {
    let effort = req.profile.effort.map(|e| e.as_str());
    let mut args = base_args(ctx, req.profile.model.as_deref(), effort);
    args.push("--session-id".to_string());
    args.push(session_id.to_string());
    if let Some(p) = req.prompt.as_ref().filter(|p| !p.trim().is_empty()) {
        args.push(p.clone());
    }
    spec_with(req, ctx, args, agent_id)
}

/// How a restart continues the agent's session (plan5 A.5, review5 N1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestartSession {
    /// `--resume <session id>`: the session has had a turn, so its transcript exists.
    Resume(String),
    /// `--session-id <new uuid>`: no turn yet, so no transcript; `--resume` would fail with
    /// "No conversation found". The agent gets this new session id.
    Fresh(String),
}

/// Detail of an agent whose `--resume` restart exited with an error before its session
/// started (review5 N1).
pub const RESTART_FAILED_TEXT: &str = "Genstart fejlede: samtalen kunne ikke genoptages";

/// How long after a `--resume` restart an error exit counts as [`RESTART_FAILED_TEXT`].
pub const RESUME_FAIL_WINDOW_MS: u64 = 15_000;

/// Model value of a restart when none is requested: a resumed session would otherwise keep the
/// transcript's model (C5.11b).
pub const RESUME_DEFAULT_MODEL: &str = "default";

/// Command line for a restart with new model/effort (C5.11b): the flags of
/// [`build_spawn_spec`] up to the effort, with `--model` always present (the snapshot's model or
/// `default`), and `--resume <session-id>` last; never a positional prompt. Same env.
///
/// TODO(windows-verify): the restarted agent keeps its transcript, SessionStart (source
/// `resume`) turns it Idle and PostModelSwitch confirms the model; no process of the old session
/// is left (plan5 D.53).
pub fn build_resume_spec(
    req: &SpawnRequest,
    ctx: &SpawnContext,
    session_id: &str,
    agent_id: &str,
) -> SpawnSpec {
    build_restart_spec(
        req,
        ctx,
        &RestartSession::Resume(session_id.to_string()),
        agent_id,
    )
}

/// Command line for a restart (C5.11b, review5 N1): like [`build_resume_spec`], but a
/// [`RestartSession::Fresh`] session ends in `--session-id <new uuid>` instead of `--resume`
/// (same profile files, `--model` always present, never a positional prompt). Same env.
pub fn build_restart_spec(
    req: &SpawnRequest,
    ctx: &SpawnContext,
    session: &RestartSession,
    agent_id: &str,
) -> SpawnSpec {
    let model = req.profile.model.as_deref().unwrap_or(RESUME_DEFAULT_MODEL);
    let effort = req.profile.effort.map(|e| e.as_str());
    let mut args = base_args(ctx, Some(model), effort);
    let (flag, session_id) = match session {
        RestartSession::Resume(id) => ("--resume", id),
        RestartSession::Fresh(id) => ("--session-id", id),
    };
    args.push(flag.to_string());
    args.push(session_id.clone());
    spec_with(req, ctx, args, agent_id)
}

/// Starts `spec` in a PTY as generation `gen` of agent `id`. Output is pushed into `output` and
/// reported only while `gen_cell` still holds `gen`; the exit is always reported with `gen`.
fn start_child(
    spec: &SpawnSpec,
    id: &str,
    gen: u64,
    gen_cell: &Arc<AtomicU64>,
    output: &Arc<Mutex<RingBuffer>>,
    sink: EventSink,
) -> Result<PtyHandle, AgentError> {
    let on_output = {
        let output = Arc::clone(output);
        let sink = Arc::clone(&sink);
        let gen_cell = Arc::clone(gen_cell);
        let agent_id = id.to_string();
        move |bytes: &[u8]| {
            if gen_cell.load(Ordering::SeqCst) != gen {
                return;
            }
            let seq = {
                let mut rb = output.lock().unwrap_or_else(|p| p.into_inner());
                rb.push(bytes);
                rb.seq()
            };
            sink(SinkEvent::Output {
                agent_id: agent_id.clone(),
                gen,
                seq,
                bytes: bytes.to_vec(),
            });
        }
    };
    let on_exit = {
        let agent_id = id.to_string();
        move |code: Option<i32>| {
            sink(SinkEvent::Exited {
                agent_id,
                gen,
                code,
            })
        }
    };
    pty::spawn(spec, on_output, on_exit)
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
            return Err(AgentError::LimitReached { seat, max });
        }
        Ok(())
    }

    /// The seat-limit check of [`Self::spawn`], so callers can refuse before they create
    /// anything (a project folder, a ticket file; plan4b C4b.4).
    pub fn can_spawn(&self, seat: SeatKind) -> Result<(), AgentError> {
        self.check_limit(seat)
    }

    /// Changes the seat limits (from the workspace rules, plan4b A.4); running agents are never
    /// stopped, a lower limit only blocks new spawns.
    pub fn set_limits(&mut self, work: usize, staff: usize) {
        self.max_work = work;
        self.max_staff = staff;
    }

    /// Working folders of every known agent (exited included).
    pub fn cwds(&self) -> Vec<PathBuf> {
        self.agents
            .values()
            .map(|a| PathBuf::from(&a.info.cwd))
            .collect()
    }

    /// Names of every known agent (exited included), for
    /// [`super::workdir::next_agent_name`].
    pub fn names(&self) -> Vec<String> {
        self.agents.values().map(|a| a.info.name.clone()).collect()
    }

    /// Sets the agent's project (after a move, plan4b A.3). `false` for unknown or exited
    /// agents.
    pub fn set_project(&mut self, id: &str, project: Option<String>) -> bool {
        match self.agents.get_mut(id) {
            Some(a) if !is_exited(&a.info.status) => {
                a.info.project = project;
                true
            }
            _ => false,
        }
    }

    /// Live (not exited) agents on a work seat in `project` (ASCII-case-insensitive), oldest
    /// first.
    pub fn live_work_in_project(&self, project: &str) -> Vec<AgentInfo> {
        self.list()
            .into_iter()
            .filter(|a| {
                !is_exited(&a.status)
                    && a.seat_kind == SeatKind::Work
                    && a.project
                        .as_deref()
                        .is_some_and(|p| crate::projects::same_id(p, project))
            })
            .collect()
    }

    /// Whether a live agent with the coordinator role runs (any seat; plan4b A.5).
    pub fn has_live_coordinator(&self) -> bool {
        self.agents
            .values()
            .any(|a| !is_exited(&a.info.status) && a.info.roles.contains(&Role::Coordinator))
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
            profile: req.profile.clone(),
            seat_kind: req.seat_kind,
            name: req.name.clone().unwrap_or_else(|| name_for(&req.cwd)),
            project: req.project.clone(),
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
            profile,
            seat_kind,
            name,
            project,
        } = meta;
        let output = Arc::new(Mutex::new(RingBuffer::new(OUTPUT_RING_CAPACITY)));
        let gen_cell = Arc::new(AtomicU64::new(0));
        let handle = start_child(&spec, &id, 0, &gen_cell, &output, sink)?;

        let now = now_ms();
        let info = AgentInfo {
            id: id.clone(),
            session_id: session_id.clone(),
            name,
            cwd: spec.cwd.to_string_lossy().into_owned(),
            status: AgentStatus::Starting,
            detail: None,
            pid: handle.pid(),
            created_at: now,
            last_event_at: now,
            profile_id: profile.profile_id,
            profile_name: profile.profile_name,
            roles: profile.roles,
            specialist: profile.specialist,
            model: profile.model,
            effort: profile.effort.map(|e| e.as_str().to_string()),
            model_observed: false,
            open_reviews: 0,
            seat_kind,
            current_ticket_id: None,
            queue_length: 0,
            project,
        };
        self.insert(info.clone(), Some(handle), output, gen_cell);
        Ok(info)
    }

    /// Restarts a live agent with a new command line (`--resume`, model/effort change; plan5
    /// A.5): same id, seat, name, session id and ring buffer. The cwd becomes `spec.cwd` (the same
    /// folder for a model/effort change, another project's folder for a move, plan4b A.3). The PTY generation is bumped first,
    /// so the old child's exit and output are ignored; then the old child is killed and the new
    /// one started. The agent shows `Starting` with [`RESTARTING_TEXT`] until its SessionStart;
    /// `model`/`effort` become the requested values (`model_observed = false`).
    ///
    /// With [`RestartSession::Fresh`] the agent takes the new session id (session map updated).
    ///
    /// Returns the result and the old PTY handle, which the caller must drop after releasing the
    /// manager lock (see [`Self::mark_exited`]). If the new child cannot be started the agent is
    /// marked exited (the caller releases its tickets).
    pub fn restart(
        &mut self,
        id: &str,
        spec: SpawnSpec,
        session: &RestartSession,
        model: Option<String>,
        effort: Option<String>,
        sink: EventSink,
    ) -> (Result<AgentInfo, AgentError>, Option<PtyHandle>) {
        let Some(agent) = self.agents.get_mut(id) else {
            return (Err(AgentError::NotFound), None);
        };
        if is_exited(&agent.info.status) {
            return (Err(AgentError::NotFound), None);
        }
        let gen = agent.pty_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let mut old = agent.pty.take();
        if let Some(pty) = old.as_mut() {
            if let Err(e) = pty.kill() {
                log::debug!("restart: kill agent {id}: {e}");
            }
        }
        let now = now_ms();
        match start_child(&spec, id, gen, &agent.pty_gen, &agent.output, sink) {
            Ok(handle) => {
                agent.info.cwd = spec.cwd.to_string_lossy().into_owned();
                agent.info.pid = handle.pid();
                agent.pty = Some(handle);
                agent.info.status = AgentStatus::Starting;
                agent.info.detail = Some(RESTARTING_TEXT.to_string());
                agent.info.last_event_at = now;
                agent.started_at = now;
                agent.start_text = Some(RESTARTING_TEXT.to_string());
                agent.info.model = model;
                agent.info.effort = effort;
                agent.info.model_observed = false;
                match session {
                    RestartSession::Resume(_) => agent.resume_started_at = Some(now),
                    RestartSession::Fresh(new_id) => {
                        agent.resume_started_at = None;
                        agent.has_conversation = false;
                        let old_id = std::mem::replace(&mut agent.info.session_id, new_id.clone());
                        let info = agent.info.clone();
                        let old_key = session_key(&old_id);
                        if self.by_session.get(&old_key).map(String::as_str) == Some(id) {
                            self.by_session.remove(&old_key);
                        }
                        self.by_session.insert(session_key(new_id), id.to_string());
                        return (Ok(info), old);
                    }
                }
                (Ok(agent.info.clone()), old)
            }
            Err(e) => {
                agent.info.status = AgentStatus::Exited { code: None };
                agent.info.detail = None;
                agent.info.pid = None;
                agent.info.last_event_at = now;
                agent.info.current_ticket_id = None;
                agent.info.queue_length = 0;
                (Err(e), old)
            }
        }
    }

    /// How a restart of agent `id` continues its session (review5 N1): `--resume` only when the
    /// session has had a turn, otherwise a fresh session with a new uuid. `None` for unknown
    /// agents.
    pub fn restart_session(&self, id: &str) -> Option<RestartSession> {
        let a = self.agents.get(id)?;
        Some(if a.has_conversation {
            RestartSession::Resume(a.info.session_id.clone())
        } else {
            RestartSession::Fresh(uuid::Uuid::new_v4().to_string())
        })
    }

    /// The agent's session had a turn (`UserPromptSubmit` or `Stop`); see
    /// [`Agent::has_conversation`]. Unknown agents are ignored.
    pub fn mark_conversation(&mut self, id: &str) {
        if let Some(a) = self.agents.get_mut(id) {
            a.has_conversation = true;
        }
    }

    /// See [`Agent::has_conversation`] (`false` for unknown agents).
    pub fn has_conversation(&self, id: &str) -> bool {
        self.agents.get(id).is_some_and(|a| a.has_conversation)
    }

    /// The agent's current PTY generation (`None` for unknown agents).
    pub fn pty_gen(&self, id: &str) -> Option<u64> {
        self.agents
            .get(id)
            .map(|a| a.pty_gen.load(Ordering::SeqCst))
    }

    /// Live model/effort from the session (statusLine, PostModelSwitch; plan5 A.4). `None` in a
    /// field leaves it alone; a given model sets `model_observed`. Returns whether anything
    /// changed (unknown/exited agents: `false`), so the caller emits only on a change.
    pub fn set_live_model_effort(
        &mut self,
        id: &str,
        model: Option<String>,
        effort: Option<String>,
    ) -> bool {
        let Some(a) = self.agents.get_mut(id) else {
            return false;
        };
        if is_exited(&a.info.status) {
            return false;
        }
        let mut changed = false;
        if let Some(m) = model.filter(|m| !m.is_empty()) {
            if a.info.model.as_deref() != Some(m.as_str()) || !a.info.model_observed {
                a.info.model = Some(m);
                a.info.model_observed = true;
                changed = true;
            }
        }
        if let Some(e) = effort.filter(|e| !e.is_empty()) {
            if a.info.effort.as_deref() != Some(e.as_str()) {
                a.info.effort = Some(e);
                changed = true;
            }
        }
        changed
    }

    /// Live (not exited) agents with `role`, oldest first.
    pub fn with_role(&self, role: Role) -> Vec<AgentInfo> {
        self.list()
            .into_iter()
            .filter(|a| !is_exited(&a.status) && a.roles.contains(&role))
            .collect()
    }

    /// Live agents with the reviewer role (review routing, batch 2).
    pub fn reviewers(&self) -> Vec<AgentInfo> {
        self.with_role(Role::Reviewer)
    }

    fn insert(
        &mut self,
        info: AgentInfo,
        pty: Option<PtyHandle>,
        output: Arc<Mutex<RingBuffer>>,
        pty_gen: Arc<AtomicU64>,
    ) {
        self.by_session
            .insert(session_key(&info.session_id), info.id.clone());
        let started_at = info.created_at;
        self.agents.insert(
            info.id.clone(),
            Agent {
                info,
                pty,
                pty_gen,
                output,
                whitelist: DEFAULT_TOOL_WHITELIST
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
                last_user_input_at: None,
                has_conversation: false,
                resume_started_at: None,
                started_at,
                start_text: None,
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
        // The tickets glue releases the agent's tickets right after; never show a stale queue.
        agent.info.current_ticket_id = None;
        agent.info.queue_length = 0;
        Ok(agent.info.clone())
    }

    /// Records the exit reported by the waiter thread and takes the PTY out of the agent.
    /// Returns the updated info and the PTY handle, which the caller must drop **after** releasing
    /// the manager lock: dropping it closes the pseudo terminal (ConPTY `ClosePseudoConsole` can
    /// block until output is drained), which also lets a ConPTY reader thread finish.
    ///
    /// `gen` is the generation from [`SinkEvent::Exited`]: the exit of a child replaced by a
    /// restart is ignored (`None`).
    pub fn mark_exited(
        &mut self,
        id: &str,
        gen: u64,
        code: Option<i32>,
    ) -> Option<(AgentInfo, Option<PtyHandle>)> {
        let agent = self.agents.get_mut(id)?;
        if agent.pty_gen.load(Ordering::SeqCst) != gen {
            return None;
        }
        let now = now_ms();
        // A `--resume` restart that died with an error before its SessionStart (still Starting
        // with the restart text): the conversation could not be resumed (review5 N1).
        let resume_failed =
            code != Some(0)
                && agent.info.status == AgentStatus::Starting
                && agent.info.detail.as_deref().is_some_and(|d| {
                    agent.start_text.as_deref() == Some(d) || d == STARTING_HINT_TEXT
                })
                && agent
                    .resume_started_at
                    .is_some_and(|t| now.saturating_sub(t) <= RESUME_FAIL_WINDOW_MS);
        // Keep a known code if stop() raced ahead with None; otherwise take the reported one.
        let keep =
            matches!(agent.info.status, AgentStatus::Exited { code: Some(_) }) && code.is_none();
        if !keep {
            agent.info.status = AgentStatus::Exited { code };
        }
        agent.info.detail = resume_failed.then(|| RESTART_FAILED_TEXT.to_string());
        agent.resume_started_at = None;
        agent.info.last_event_at = now;
        agent.info.current_ticket_id = None;
        agent.info.queue_length = 0;
        let pty = agent.pty.take();
        Some((agent.info.clone(), pty))
    }

    /// Kills every child (app exit / `quit_app`). Idempotent; errors are only logged.
    /// Blocks at most [`QUIT_KILL_BUDGET`] on unix (waits for the process groups, then SIGKILL);
    /// returns at once on Windows. Safe under the manager lock: the waiter threads set the exit
    /// flags before they report (and need this lock).
    pub fn kill_all(&mut self) {
        let mut children = Vec::new();
        for (id, agent) in &mut self.agents {
            if let Some(pty) = agent.pty.as_mut() {
                if let Err(e) = pty.kill() {
                    log::debug!("kill_all: agent {id}: {e}");
                }
                if let Some(pid) = pty.pid() {
                    children.push((pid, pty.exit_flag()));
                }
            }
        }
        process::finish_all(&children, QUIT_KILL_BUDGET);
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

    /// Input typed by the user in the terminal panel (`write_agent_input`). Records the time
    /// first (the attempt counts, even if the write then fails), then writes like `write_input`.
    pub fn write_user_input(&mut self, id: &str, bytes: &[u8]) -> Result<(), AgentError> {
        let agent = self.agent_mut(id)?;
        agent.last_user_input_at = Some(now_ms());
        match agent.pty.as_mut() {
            Some(pty) => pty.write(bytes),
            None => Err(AgentError::NotFound),
        }
    }

    /// See [`Agent::last_user_input_at`]; `None` for unknown agents and before any user input.
    pub fn last_user_input_at(&self, id: &str) -> Option<u64> {
        self.agents.get(id).and_then(|a| a.last_user_input_at)
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

    /// Sets the detail text alone (dispatcher hints such as "Kunne ikke aflevere ticket");
    /// status is unchanged. Refused (`false`) for unknown and exited agents.
    pub fn set_detail(&mut self, id: &str, detail: Option<String>) -> bool {
        match self.agents.get_mut(id) {
            Some(a) if !is_exited(&a.info.status) => {
                a.info.detail = detail;
                a.info.last_event_at = now_ms();
                true
            }
            _ => false,
        }
    }

    /// Sets the agent's ticket link (`currentTicketId`, `queueLength`). Returns whether anything
    /// changed (unknown agent: `false`). Exited agents are accepted so a release can zero them.
    pub fn set_ticket_link(&mut self, id: &str, current: Option<String>, len: usize) -> bool {
        match self.agents.get_mut(id) {
            Some(a) if a.info.current_ticket_id != current || a.info.queue_length != len => {
                a.info.current_ticket_id = current;
                a.info.queue_length = len;
                true
            }
            _ => false,
        }
    }

    /// Sets the agent's open review count (`openReviews`, from the tickets glue). Returns whether
    /// it changed (unknown agent: `false`).
    pub fn set_review_link(&mut self, id: &str, open_reviews: usize) -> bool {
        match self.agents.get_mut(id) {
            Some(a) if a.info.open_reviews != open_reviews => {
                a.info.open_reviews = open_reviews;
                true
            }
            _ => false,
        }
    }

    /// Ids of every known agent (exited included).
    pub fn ids(&self) -> Vec<AgentId> {
        self.agents.keys().cloned().collect()
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
                // A new session (e.g. `/clear`) has no transcript until its first turn.
                agent.has_conversation = false;
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

    /// Sets [`STARTING_HINT_TEXT`] as detail when the agent is still `Starting`
    /// [`STARTING_HINT_AFTER`] after its child was started (spawn or restart; no hook event yet,
    /// usually the trust dialog). The detail must be empty or the restart's own text
    /// ([`RESTARTING_TEXT`], the move text): a move into a git project shows the trust dialog
    /// too (W2). `last_event_at` is left alone. Returns the updated info if the hint was set.
    pub fn apply_starting_hint(&mut self, id: &str, now_ms: u64) -> Option<AgentInfo> {
        let agent = self.agents.get_mut(id)?;
        let due = now_ms.saturating_sub(agent.started_at) >= STARTING_HINT_AFTER.as_millis() as u64;
        let replaceable = match agent.info.detail.as_deref() {
            None => true,
            Some(d) => agent.start_text.as_deref() == Some(d),
        };
        if agent.info.status != AgentStatus::Starting || !replaceable || !due {
            return None;
        }
        agent.info.detail = Some(STARTING_HINT_TEXT.to_string());
        Some(agent.info.clone())
    }

    /// Replaces the restart text of a restarted agent that is still `Starting` with `text` (the
    /// move text, plan4b A.3); the Starting hint may later replace it in turn. Returns whether it
    /// was set.
    pub fn set_start_text(&mut self, id: &str, text: String) -> bool {
        match self.agents.get_mut(id) {
            Some(a)
                if a.info.status == AgentStatus::Starting
                    && a.start_text.is_some()
                    && a.info.detail == a.start_text =>
            {
                a.info.detail = Some(text.clone());
                a.start_text = Some(text);
                true
            }
            _ => false,
        }
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

    /// Test helper: an agent without a PTY (no roles, work seat).
    #[cfg(test)]
    pub fn insert_fake(&mut self, session_id: &str, cwd: &str) -> AgentId {
        self.insert_fake_with(session_id, cwd, &[], SeatKind::Work)
    }

    /// Test helper: an agent without a PTY. On a work seat it is in project `p` (every work agent
    /// has a project from step 4b on).
    #[cfg(test)]
    pub fn insert_fake_with(
        &mut self,
        session_id: &str,
        cwd: &str,
        roles: &[Role],
        seat_kind: SeatKind,
    ) -> AgentId {
        let project = (seat_kind == SeatKind::Work).then_some("p");
        self.insert_fake_in(session_id, cwd, roles, seat_kind, project)
    }

    /// Test helper: an agent without a PTY in `project`.
    #[cfg(test)]
    pub fn insert_fake_in(
        &mut self,
        session_id: &str,
        cwd: &str,
        roles: &[Role],
        seat_kind: SeatKind,
        project: Option<&str>,
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
            profile_id: "test".into(),
            profile_name: "Test".into(),
            roles: roles.to_vec(),
            specialist: roles.len() != 1,
            model: None,
            effort: None,
            model_observed: false,
            open_reviews: 0,
            seat_kind,
            current_ticket_id: None,
            queue_length: 0,
            project: project.map(str::to_string),
        };
        self.insert(
            info,
            None,
            Arc::new(Mutex::new(RingBuffer::new(1024))),
            Arc::new(AtomicU64::new(0)),
        );
        id
    }

    /// Test helper: moves the agent's creation time into the past.
    #[cfg(test)]
    pub fn backdate(&mut self, id: &str, ms: u64) {
        if let Some(a) = self.agents.get_mut(id) {
            a.info.created_at = a.info.created_at.saturating_sub(ms);
            a.started_at = a.started_at.saturating_sub(ms);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::model::Effort;
    use serde_json::{json, Value};

    fn ctx(claude: PathBuf) -> SpawnContext {
        SpawnContext {
            claude,
            settings_json: PathBuf::from("/data/settings.json"),
            mcp_config: None,
            system_prompt: None,
            pipe_name: "pipe-x".into(),
        }
    }

    fn ctx_with_mcp(claude: PathBuf) -> SpawnContext {
        SpawnContext {
            mcp_config: Some(PathBuf::from("/data/mcp.json")),
            system_prompt: Some(PathBuf::from("/data/system-prompt.md")),
            ..ctx(claude)
        }
    }

    fn work_req(prompt: Option<&str>) -> SpawnRequest {
        SpawnRequest {
            cwd: PathBuf::from("/w/demo"),
            name: None,
            project: None,
            prompt: prompt.map(str::to_string),
            profile: ProfileSnapshot::default(),
            seat_kind: SeatKind::Work,
        }
    }

    #[test]
    fn spawn_spec_with_mcp_server_and_system_prompt() {
        let spec = build_spawn_spec(
            &work_req(Some("fix it")),
            &ctx_with_mcp(PathBuf::from("/bin/claude")),
            "sid",
            "aid",
        );
        assert_eq!(
            spec.args,
            [
                "--settings",
                "/data/settings.json",
                "--mcp-config",
                "/data/mcp.json",
                "--append-system-prompt-file",
                "/data/system-prompt.md",
                "--session-id",
                "sid",
                "fix it"
            ]
        );
        assert_eq!(
            spec.env,
            vec![
                ("MIRA_BOTS_PIPE".to_string(), "pipe-x".to_string()),
                ("MIRA_AGENT_ID".to_string(), "aid".to_string()),
                ("MIRA_AGENT_ROLES".to_string(), String::new()),
            ]
        );
    }

    /// For every combination: `--mcp-config <path>` is followed by a flag (never the prompt),
    /// `--session-id <sid>` is the last flag before the prompt, no `--strict-mcp-config`.
    #[test]
    fn spawn_spec_never_puts_the_prompt_after_mcp_config() {
        let claude = PathBuf::from("/bin/claude");
        for mcp in [false, true] {
            for prompt_file in [false, true] {
                for prompt in [None, Some("fix it"), Some("Ticket abc: x")] {
                    let mut c = ctx(claude.clone());
                    if mcp {
                        c.mcp_config = Some(PathBuf::from("/data/mcp.json"));
                    }
                    if prompt_file {
                        c.system_prompt = Some(PathBuf::from("/data/system-prompt.md"));
                    }
                    let a = build_spawn_spec(&work_req(prompt), &c, "sid", "aid").args;
                    assert!(!a.iter().any(|x| x == "--strict-mcp-config" || x == "--"));
                    if let Some(i) = a.iter().position(|x| x == "--mcp-config") {
                        assert!(a[i + 2].starts_with("--"), "{a:?}");
                    }
                    assert_eq!(a.iter().any(|x| x == "--mcp-config"), mcp);
                    assert_eq!(
                        a.iter().any(|x| x == "--append-system-prompt-file"),
                        prompt_file
                    );
                    let flags = if prompt.is_some() { 3 } else { 2 };
                    assert_eq!(a[a.len() - flags], "--session-id", "{a:?}");
                    assert_eq!(a[a.len() - flags + 1], "sid");
                    if let Some(p) = prompt {
                        assert_eq!(a.last().unwrap(), p);
                    }
                    assert_eq!(a[0], "--settings");
                }
            }
        }
    }

    fn null_sink() -> EventSink {
        Arc::new(|_| {})
    }

    #[test]
    fn spawn_spec_command_line() {
        let req = SpawnRequest {
            cwd: PathBuf::from("/w/demo"),
            name: None,
            project: None,
            prompt: Some("fix it".into()),
            profile: ProfileSnapshot::default(),
            seat_kind: SeatKind::Work,
        };
        let spec = build_spawn_spec(&req, &ctx(PathBuf::from("/bin/claude")), "sid", "aid");
        assert_eq!(spec.program, PathBuf::from("/bin/claude"));
        assert_eq!(
            spec.args,
            [
                "--settings",
                "/data/settings.json",
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
                ("MIRA_AGENT_ROLES".to_string(), String::new()),
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
                name: None,
                project: None,
                prompt,
                profile: ProfileSnapshot::default(),
                seat_kind: SeatKind::Work,
            };
            let spec = build_spawn_spec(&req, &ctx(PathBuf::from("c")), "s", "a");
            assert_eq!(spec.args.len(), 4);
        }
    }

    fn profile_req(model: Option<&str>, effort: Option<Effort>, roles: &[Role]) -> SpawnRequest {
        SpawnRequest {
            cwd: PathBuf::from("/w/demo"),
            name: None,
            project: None,
            prompt: Some("fix it".into()),
            seat_kind: SeatKind::Work,
            profile: ProfileSnapshot {
                profile_id: "reviewer".into(),
                profile_name: "Reviewer".into(),
                roles: roles.to_vec(),
                specialist: false,
                model: model.map(str::to_string),
                effort,
            },
        }
    }

    fn profile_ctx() -> SpawnContext {
        SpawnContext {
            claude: PathBuf::from("/bin/claude"),
            settings_json: PathBuf::from("/data/profiles/reviewer/settings.json"),
            mcp_config: Some(PathBuf::from("/data/mcp.json")),
            system_prompt: Some(PathBuf::from("/data/profiles/reviewer/system-prompt.md")),
            pipe_name: "pipe-x".into(),
        }
    }

    #[test]
    fn spawn_spec_flag_order_with_model_and_effort() {
        let req = profile_req(
            Some("claude-opus-5-5[1m]"),
            Some(Effort::Max),
            &[Role::Reviewer],
        );
        let spec = build_spawn_spec(&req, &profile_ctx(), "sid", "aid");
        assert_eq!(
            spec.args,
            [
                "--settings",
                "/data/profiles/reviewer/settings.json",
                "--mcp-config",
                "/data/mcp.json",
                "--append-system-prompt-file",
                "/data/profiles/reviewer/system-prompt.md",
                "--model",
                "claude-opus-5-5[1m]",
                "--effort",
                "max",
                "--session-id",
                "sid",
                "fix it"
            ]
        );
        // Without mira-mcp: no MCP/prompt flags, model/effort still there.
        let mut c = profile_ctx();
        c.mcp_config = None;
        c.system_prompt = None;
        let a = build_spawn_spec(&req, &c, "sid", "aid").args;
        assert_eq!(
            a,
            [
                "--settings",
                "/data/profiles/reviewer/settings.json",
                "--model",
                "claude-opus-5-5[1m]",
                "--effort",
                "max",
                "--session-id",
                "sid",
                "fix it"
            ]
        );
        // Only one of them.
        let a = build_spawn_spec(&profile_req(None, Some(Effort::Low), &[]), &c, "s", "a").args;
        assert_eq!(a[2..], ["--effort", "low", "--session-id", "s", "fix it"]);
        let a = build_spawn_spec(&profile_req(Some("haiku"), None, &[]), &c, "s", "a").args;
        assert_eq!(a[2..], ["--model", "haiku", "--session-id", "s", "fix it"]);
    }

    #[test]
    fn spawn_spec_without_model_effort() {
        let spec = build_spawn_spec(&profile_req(None, None, &[]), &profile_ctx(), "sid", "aid");
        assert!(!spec.args.iter().any(|a| a == "--model" || a == "--effort"));
        assert_eq!(spec.args.len(), 9);
        assert_eq!(spec.args[6..], ["--session-id", "sid", "fix it"]);
        for bad in ["--resume", "--strict-mcp-config", "-p"] {
            assert!(!spec.args.iter().any(|a| a == bad), "{bad}");
        }
        // Never the env overrides that would hide the user's own /model and --effort.
        assert!(spec
            .env
            .iter()
            .all(|(k, _)| k != "ANTHROPIC_MODEL" && k != "CLAUDE_CODE_EFFORT_LEVEL"));
    }

    #[test]
    fn spawn_spec_env_has_roles() {
        let env = |roles: &[Role]| {
            build_spawn_spec(
                &profile_req(None, None, roles),
                &profile_ctx(),
                "sid",
                "aid",
            )
            .env
        };
        assert_eq!(
            env(&[Role::Reviewer, Role::Coder]),
            vec![
                ("MIRA_BOTS_PIPE".to_string(), "pipe-x".to_string()),
                ("MIRA_AGENT_ID".to_string(), "aid".to_string()),
                ("MIRA_AGENT_ROLES".to_string(), "coder,reviewer".to_string()),
            ]
        );
        assert_eq!(
            env(&[])[2],
            ("MIRA_AGENT_ROLES".to_string(), String::new()),
            "no roles: empty string, not a missing variable"
        );
        assert_eq!(
            env(&Role::ALL)[2].1,
            "coder,researcher,reviewer,coordinator,planner,debugger"
        );
    }

    #[test]
    fn resume_spec_uses_resume_and_no_prompt() {
        let req = profile_req(Some("sonnet"), Some(Effort::Xhigh), &[Role::Reviewer]);
        let spec = build_resume_spec(&req, &profile_ctx(), "sid-1", "aid");
        assert_eq!(
            spec.args,
            [
                "--settings",
                "/data/profiles/reviewer/settings.json",
                "--mcp-config",
                "/data/mcp.json",
                "--append-system-prompt-file",
                "/data/profiles/reviewer/system-prompt.md",
                "--model",
                "sonnet",
                "--effort",
                "xhigh",
                "--resume",
                "sid-1"
            ]
        );
        assert!(!spec
            .args
            .iter()
            .any(|a| a == "--session-id" || a == "fix it"));
        assert_eq!(
            spec.env,
            build_spawn_spec(&req, &profile_ctx(), "x", "aid").env
        );
        assert_eq!(spec.cwd, PathBuf::from("/w/demo"));
        // No model requested: `--model default` (otherwise the transcript's model would stay).
        let a = build_resume_spec(&profile_req(None, None, &[]), &profile_ctx(), "s", "a").args;
        assert_eq!(a[6..], ["--model", "default", "--resume", "s"]);
        // `--mcp-config <path>` is followed by a flag here too.
        let i = a.iter().position(|x| x == "--mcp-config").unwrap();
        assert!(a[i + 2].starts_with("--"));
    }

    #[test]
    fn live_model_only_emits_on_change() {
        let mut m = AgentManager::new(5);
        let id = m.insert_fake("s", "/w/a");
        assert!(m.set_live_model_effort(&id, Some("claude-opus-5-5".into()), Some("high".into())));
        assert!(!m.set_live_model_effort(&id, Some("claude-opus-5-5".into()), Some("high".into())));
        assert!(!m.set_live_model_effort(&id, None, None));
        assert!(!m.set_live_model_effort(&id, Some(String::new()), None));
        // Effort alone; model untouched.
        assert!(m.set_live_model_effort(&id, None, Some("xhigh".into())));
        let a = m.get(&id).unwrap();
        assert_eq!(
            (a.model.as_deref(), a.effort.as_deref(), a.model_observed),
            (Some("claude-opus-5-5"), Some("xhigh"), true)
        );
        assert!(m.set_live_model_effort(&id, Some("claude-sonnet-5-5".into()), None));
        assert!(!m.set_live_model_effort("nope", Some("x".into()), None));
        m.stop(&id).unwrap();
        assert!(!m.set_live_model_effort(&id, Some("claude-haiku".into()), None));
    }

    #[test]
    fn reviewers_are_live_agents_with_the_role() {
        let mut m = AgentManager::new(5);
        let r1 = m.insert_fake_with("a", "/w/a", &[Role::Reviewer], SeatKind::Staff);
        let r2 = m.insert_fake_with("b", "/w/b", &[Role::Coder, Role::Reviewer], SeatKind::Work);
        let gone = m.insert_fake_with("c", "/w/c", &[Role::Reviewer], SeatKind::Staff);
        m.insert_fake_with("d", "/w/d", &[Role::Coder], SeatKind::Work);
        m.stop(&gone).unwrap();
        let mut ids: Vec<String> = m.reviewers().into_iter().map(|a| a.id).collect();
        ids.sort();
        let mut want = vec![r1, r2];
        want.sort();
        assert_eq!(ids, want);
        assert_eq!(m.with_role(Role::Coordinator).len(), 0);
    }

    #[test]
    fn exits_of_an_older_generation_are_ignored() {
        let mut m = AgentManager::new(5);
        let id = m.insert_fake("s", "/w/a");
        assert_eq!(m.pty_gen(&id), Some(0));
        assert!(
            m.mark_exited(&id, 1, Some(0)).is_none(),
            "a future gen is not ours"
        );
        assert!(matches!(m.get(&id).unwrap().status, AgentStatus::Starting));
        assert!(m.mark_exited(&id, 0, Some(0)).is_some());
        assert_eq!(m.pty_gen("nope"), None);
    }

    #[test]
    fn limit_counts_only_non_exited_agents() {
        let mut m = AgentManager::new(5);
        let ids: Vec<_> = (0..5)
            .map(|i| m.insert_fake(&format!("s{i}"), "/w/a"))
            .collect();
        let req = || SpawnRequest {
            cwd: PathBuf::from("/definitely/not/a/dir"),
            name: None,
            project: None,
            prompt: None,
            profile: ProfileSnapshot::default(),
            seat_kind: SeatKind::Work,
        };
        let c = ctx(PathBuf::from("/nope/claude"));
        assert!(matches!(
            m.spawn(req(), &c, null_sink()),
            Err(AgentError::LimitReached {
                seat: SeatKind::Work,
                max: 5
            })
        ));
        m.mark_exited(&ids[0], 0, Some(0));
        // Past the limit now; fails on the next check instead.
        assert!(matches!(
            m.spawn(req(), &c, null_sink()),
            Err(AgentError::InvalidCwd)
        ));
        assert_eq!(
            AgentError::LimitReached {
                seat: SeatKind::Work,
                max: 5
            }
            .to_string(),
            "Loft på 5 arbejdspladser nået"
        );
        assert_eq!(
            AgentError::LimitReached {
                seat: SeatKind::Staff,
                max: 3
            }
            .to_string(),
            "Loft på 3 stabspladser nået"
        );
    }

    fn doomed(seat_kind: SeatKind) -> SpawnRequest {
        // Passes the limit check, then fails on the cwd check (nothing is started).
        SpawnRequest {
            cwd: PathBuf::from("/definitely/not/a/dir"),
            name: None,
            project: None,
            prompt: None,
            profile: ProfileSnapshot::default(),
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
            Err(AgentError::LimitReached {
                seat: SeatKind::Work,
                max: 5
            })
        ));
        // Five work agents do not block staff.
        assert!(matches!(
            m.spawn(doomed(SeatKind::Staff), &c, null_sink()),
            Err(AgentError::InvalidCwd)
        ));
        let s0 = m.insert_fake_with("s0", "/w/s", &[Role::Coordinator], SeatKind::Staff);
        m.insert_fake_with("s1", "/w/s", &[Role::Reviewer], SeatKind::Staff);
        m.insert_fake_with("s2", "/w/s", &[Role::Planner], SeatKind::Staff);
        assert!(matches!(
            m.spawn(doomed(SeatKind::Staff), &c, null_sink()),
            Err(AgentError::LimitReached {
                seat: SeatKind::Staff,
                max: 3
            })
        ));
        m.mark_exited(&s0, 0, Some(0));
        assert!(matches!(
            m.spawn(doomed(SeatKind::Staff), &c, null_sink()),
            Err(AgentError::InvalidCwd)
        ));
        // Staff agents never count against the work limit.
        let mut m = AgentManager::with_limits(1, 2);
        m.insert_fake_with("s0", "/w/s", &[], SeatKind::Staff);
        m.insert_fake_with("s1", "/w/s", &[], SeatKind::Staff);
        assert!(matches!(
            m.spawn(doomed(SeatKind::Work), &c, null_sink()),
            Err(AgentError::InvalidCwd)
        ));
        assert_eq!(m.running_count(), 2);
    }

    #[test]
    fn step4b_agent_errors_are_danish() {
        let table = [
            (
                AgentError::LimitReached {
                    seat: SeatKind::Work,
                    max: 2,
                },
                "Loft på 2 arbejdspladser nået",
            ),
            (
                AgentError::LimitReached {
                    seat: SeatKind::Staff,
                    max: 1,
                },
                "Loft på 1 stabspladser nået",
            ),
            (
                AgentError::QueueNotEmpty(2),
                "Agenten har 2 tickets i kø — flyt dem først, eller bekræft at de lægges i Backlog",
            ),
            (
                AgentError::ProjectLimit {
                    project: "p".into(),
                    max: 1,
                },
                "Loft på 1 agenter i projektet «p» nået",
            ),
            (
                AgentError::SameProject("p".into()),
                "Agenten står allerede i projekt «p»",
            ),
            (
                AgentError::StaffHasNoProject,
                "Stabsagenter står i projektroden og kan ikke flyttes",
            ),
        ];
        for (e, text) in table {
            assert_eq!(e.to_string(), text);
        }
    }

    #[test]
    fn set_limits_changes_the_check() {
        let mut m = AgentManager::with_limits(1, 3);
        let c = ctx(PathBuf::from("/nope/claude"));
        m.insert_fake("w0", "/w/a");
        assert!(matches!(
            m.spawn(doomed(SeatKind::Work), &c, null_sink()),
            Err(AgentError::LimitReached {
                seat: SeatKind::Work,
                max: 1
            })
        ));
        m.set_limits(2, 3);
        assert!(matches!(
            m.spawn(doomed(SeatKind::Work), &c, null_sink()),
            Err(AgentError::InvalidCwd)
        ));
        m.set_limits(2, 1);
        m.insert_fake_with("s0", "/w", &[Role::Coordinator], SeatKind::Staff);
        assert_eq!(
            m.spawn(doomed(SeatKind::Staff), &c, null_sink())
                .unwrap_err()
                .to_string(),
            "Loft på 1 stabspladser nået"
        );
    }

    #[test]
    fn names_projects_and_coordinator() {
        let mut m = AgentManager::new(5);
        let a = m.insert_fake_in("a", "/r/p", &[Role::Coder], SeatKind::Work, Some("p"));
        let b = m.insert_fake_in("b", "/r/P", &[Role::Coder], SeatKind::Work, Some("P"));
        m.insert_fake_in("c", "/r/q", &[Role::Coder], SeatKind::Work, Some("q"));
        m.insert_fake_with("d", "/r", &[Role::Reviewer], SeatKind::Staff);
        let mut names = m.names();
        names.sort();
        assert_eq!(names, ["P", "p", "q", "r"]);
        assert_eq!(m.get(&a).unwrap().project.as_deref(), Some("p"));
        let in_p: Vec<_> = m
            .live_work_in_project("p")
            .into_iter()
            .map(|i| i.id)
            .collect();
        assert_eq!(in_p.len(), 2);
        assert!(in_p.contains(&a) && in_p.contains(&b));
        assert!(m.live_work_in_project("none").is_empty());
        assert!(!m.has_live_coordinator());

        // set_project: live agents only.
        assert!(m.set_project(&b, Some("q".into())));
        assert_eq!(m.live_work_in_project("Q").len(), 2);
        assert!(!m.set_project("nope", None));
        m.mark_exited(&a, 0, Some(0));
        assert!(!m.set_project(&a, None));
        assert!(m.live_work_in_project("p").is_empty());
        assert_eq!(m.names().len(), 4, "exited agents keep their names");

        let k = m.insert_fake_with("k", "/r", &[Role::Coordinator], SeatKind::Staff);
        assert!(m.has_live_coordinator());
        m.mark_exited(&k, 0, Some(0));
        assert!(!m.has_live_coordinator());
    }

    #[test]
    fn seat_kind_serde_lowercase() {
        for (seat, s) in [(SeatKind::Work, "work"), (SeatKind::Staff, "staff")] {
            assert_eq!(serde_json::to_value(seat).unwrap(), json!(s));
            assert_eq!(serde_json::from_value::<SeatKind>(json!(s)).unwrap(), seat);
        }
        assert!(serde_json::from_value::<SeatKind>(json!("Work")).is_err());
        assert_eq!(SeatKind::default(), SeatKind::Work);
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
                name: None,
                project: None,
                prompt: Some(prompt.into()),
                profile: ProfileSnapshot::default(),
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
                name: None,
                project: None,
                prompt: prompt.map(str::to_string),
                profile: ProfileSnapshot::default(),
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
            name: None,
            project: None,
            prompt: None,
            profile: ProfileSnapshot::default(),
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
            m.mark_exited(&id, 0, Some(3)).unwrap().0.status,
            AgentStatus::Exited { code: Some(3) }
        );
        // A later None does not erase a known code.
        assert_eq!(
            m.mark_exited(&id, 0, None).unwrap().0.status,
            AgentStatus::Exited { code: Some(3) }
        );
        assert!(m.mark_exited("nope", 0, None).is_none());
        assert!(m.remove(&id).unwrap().is_none(), "fake agent has no PTY");
        assert!(m.list().is_empty());
        assert_eq!(m.agent_id_for_session("s"), None);
        assert!(matches!(m.remove(&id), Err(AgentError::NotFound)));
    }

    #[test]
    fn agent_info_serializes_new_fields() {
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
            "profileId",
            "profileName",
            "roles",
            "specialist",
            "model",
            "effort",
            "modelObserved",
            "openReviews",
            "seatKind",
            "currentTicketId",
            "queueLength",
            "project",
        ] {
            assert!(v.get(key).is_some(), "{key}");
        }
        // Test fakes on a work seat are in project "p" (insert_fake_with).
        assert_eq!(v["project"], "p");
        assert_eq!(v["currentTicketId"], Value::Null);
        assert_eq!(v["queueLength"], 0);
        m.set_ticket_link(&id, Some("t1".into()), 2);
        let v = serde_json::to_value(m.get(&id).unwrap()).unwrap();
        assert_eq!(
            (v["currentTicketId"].clone(), v["queueLength"].clone()),
            (json!("t1"), json!(2))
        );
        assert_eq!(v["status"], json!({"kind":"starting"}));
        assert!(v.get("role").is_none(), "the step-4 field is gone");
        assert_eq!(v["roles"], json!([]));
        assert_eq!(v["seatKind"], "work");
        let id = m.insert_fake_with(
            "s2",
            "/w/x",
            &[Role::Researcher, Role::Coordinator],
            SeatKind::Staff,
        );
        m.set_live_model_effort(&id, Some("claude-opus-5-5".into()), Some("high".into()));
        let v = serde_json::to_value(m.get(&id).unwrap()).unwrap();
        assert_eq!(
            (v["roles"].clone(), v["seatKind"].clone()),
            (json!(["researcher", "coordinator"]), json!("staff"))
        );
        assert_eq!(
            (
                v["model"].clone(),
                v["effort"].clone(),
                v["modelObserved"].clone(),
                v["openReviews"].clone(),
                v["specialist"].clone(),
                v["profileId"].clone(),
                v["profileName"].clone()
            ),
            (
                json!("claude-opus-5-5"),
                json!("high"),
                json!(true),
                json!(0),
                json!(true),
                json!("test"),
                json!("Test")
            )
        );
    }

    #[test]
    fn set_ticket_link_reports_changes_and_stop_clears_it() {
        let mut m = AgentManager::new(5);
        let id = m.insert_fake("s", "/w/demo");
        assert!(m.set_ticket_link(&id, Some("t1".into()), 2));
        assert!(!m.set_ticket_link(&id, Some("t1".into()), 2), "unchanged");
        assert!(m.set_ticket_link(&id, None, 2));
        assert!(!m.set_ticket_link("nope", None, 1));
        assert_eq!(m.ids(), vec![id.clone()]);
        let stopped = m.stop(&id).unwrap();
        assert_eq!((stopped.current_ticket_id, stopped.queue_length), (None, 0));
        // An exited agent can still be zeroed (no-op here) and set (harmless).
        assert!(!m.set_ticket_link(&id, None, 0));
        let id2 = m.insert_fake("s2", "/w/b");
        m.set_ticket_link(&id2, Some("t2".into()), 1);
        let (info, _) = m.mark_exited(&id2, 0, Some(0)).unwrap();
        assert_eq!((info.current_ticket_id, info.queue_length), (None, 0));
    }

    #[test]
    fn only_user_input_records_its_time() {
        let mut m = AgentManager::new(5);
        let id = m.insert_fake("s", "/w/demo");
        assert_eq!(m.last_user_input_at(&id), None);
        // The dispatcher's path (no PTY in the fake, so the write itself fails).
        assert!(m.write_input(&id, b"Ticket x").is_err());
        assert_eq!(m.last_user_input_at(&id), None);
        let before = now_ms();
        assert!(m.write_user_input(&id, b"h").is_err());
        assert!(m.last_user_input_at(&id).is_some_and(|t| t >= before));
        assert!(m.write_user_input("nope", b"h").is_err());
        assert_eq!(m.last_user_input_at("nope"), None);
    }

    #[test]
    fn set_detail_is_refused_for_exited_and_unknown_agents() {
        let mut m = AgentManager::new(5);
        let id = m.insert_fake("s", "/w/demo");
        m.set_status(&id, AgentStatus::Idle, None).unwrap();
        assert!(m.set_detail(&id, Some("hint".into())));
        let a = m.get(&id).unwrap();
        assert_eq!(
            (a.status, a.detail.as_deref()),
            (AgentStatus::Idle, Some("hint"))
        );
        assert!(m.set_detail(&id, None));
        assert_eq!(m.get(&id).unwrap().detail, None);
        assert!(!m.set_detail("nope", Some("x".into())));
        m.stop(&id).unwrap();
        assert!(!m.set_detail(&id, Some("x".into())));
        assert_eq!(m.get(&id).unwrap().detail, None);
    }

    /// review5 N1: `--resume` only after a turn (UserPromptSubmit/Stop) of the current session;
    /// a new session id (`/clear`) starts over.
    #[test]
    fn restart_session_resumes_only_after_a_turn() {
        let mut m = AgentManager::new(5);
        let id = m.insert_fake("sess-1", "/w/a");
        assert!(m.restart_session("nope").is_none());
        let RestartSession::Fresh(new_id) = m.restart_session(&id).unwrap() else {
            panic!("no turn yet: fresh session");
        };
        assert_ne!(new_id, "sess-1");
        assert!(uuid::Uuid::parse_str(&new_id).is_ok());
        m.mark_conversation(&id);
        assert_eq!(
            m.restart_session(&id),
            Some(RestartSession::Resume("sess-1".into()))
        );
        // `/clear`: the frame rebinds the agent to a new session without a transcript.
        m.match_frame(Some(&id), "sess-2").unwrap();
        assert!(!m.has_conversation(&id));
        assert!(matches!(
            m.restart_session(&id),
            Some(RestartSession::Fresh(_))
        ));
        // A frame of the same session does not reset it.
        m.mark_conversation(&id);
        m.match_frame(Some(&id), "SESS-2").unwrap();
        assert!(m.has_conversation(&id));
    }

    #[test]
    fn restart_spec_fresh_vs_resume() {
        let req = profile_req(Some("haiku"), Some(Effort::Low), &[]);
        let c = profile_ctx();
        let resume = build_restart_spec(&req, &c, &RestartSession::Resume("s-1".into()), "aid");
        assert_eq!(resume.args, build_resume_spec(&req, &c, "s-1", "aid").args);
        let fresh = build_restart_spec(&req, &c, &RestartSession::Fresh("s-2".into()), "aid");
        let n = fresh.args.len();
        assert_eq!(fresh.args[n - 2..], ["--session-id", "s-2"]);
        assert_eq!(
            fresh.args[..n - 2],
            resume.args[..n - 2],
            "same profile flags"
        );
        assert!(!fresh.args.iter().any(|a| a == "--resume"));
        assert_eq!(fresh.env, resume.env);
        // No model requested: --model default, like a resume.
        let fresh = build_restart_spec(
            &profile_req(None, None, &[]),
            &c,
            &RestartSession::Fresh("s".into()),
            "a",
        );
        assert!(fresh.args.windows(2).any(|w| w == ["--model", "default"]));
    }

    #[cfg(unix)]
    mod unix_pty {
        use super::*;
        use crate::config::moving_text;
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
                profile: ProfileSnapshot::default(),
                seat_kind: SeatKind::Work,
                name: "bot-01".into(),
                project: None,
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
            let (exited, pty) = m.mark_exited(&info.id, 0, Some(3)).unwrap();
            assert_eq!(exited.status, AgentStatus::Exited { code: Some(3) });
            assert!(pty.is_some(), "the PTY handle is handed to the caller");
            assert_eq!(
                m.get(&info.id).unwrap().status,
                AgentStatus::Exited { code: Some(3) }
            );
            // Dropped here, outside any manager lock; a second report has nothing left to hand.
            drop(pty);
            let (_, again) = m.mark_exited(&info.id, 0, None).unwrap();
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
                name: None,
                project: None,
                prompt: None,
                profile: ProfileSnapshot::default(),
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
                Err(AgentError::LimitReached {
                    seat: SeatKind::Work,
                    max: 2
                })
            ));
            m.stop(&a.id).unwrap();
            let c = m.spawn_spec(sh("true", vec![]), meta("c"), sink).unwrap();
            assert_eq!(c.status, AgentStatus::Starting);
            for info in m.list() {
                let _ = m.stop(&info.id);
            }
            let _ = events;
        }

        /// W2: a move shows the move text instead of the restart text, and 15 s after the
        /// restart (not the spawn) without a hook event the Starting hint replaces it.
        #[test]
        fn move_restart_shows_the_move_text_then_the_starting_hint() {
            let mut m = AgentManager::new(5);
            let (sink, _events) = collecting_sink();
            let info = m
                .spawn_spec(sh("sleep 30", vec![]), meta("sess-mv"), sink.clone())
                .unwrap();
            m.set_status(&info.id, AgentStatus::Idle, None).unwrap();
            m.backdate(&info.id, 60_000);
            // Only a restarted agent has a start text to replace.
            assert!(!m.set_start_text(&info.id, moving_text("shop")));
            let before = now_ms();
            let (res, old) = m.restart(
                &info.id,
                sh("sleep 30", vec![]),
                &RestartSession::Resume("sess-mv".into()),
                None,
                None,
                sink,
            );
            res.unwrap();
            drop(old);
            assert!(m.set_start_text(&info.id, moving_text("shop")));
            let now = m.get(&info.id).unwrap();
            assert_eq!(now.detail.as_deref(), Some("Flytter til «shop»…"));
            // Counted from the restart, not from the (backdated) spawn.
            assert!(m.apply_starting_hint(&info.id, before + 14_000).is_none());
            let hinted = m.apply_starting_hint(&info.id, now_ms() + 15_000).unwrap();
            assert_eq!(hinted.detail.as_deref(), Some(STARTING_HINT_TEXT));
            assert_eq!(hinted.status, AgentStatus::Starting);
            // The first hook event clears it like after a spawn.
            assert!(m.clear_starting_hint(&info.id));
            // A plain restart (model change): the restart text is replaced as well.
            let (sink2, _e2) = collecting_sink();
            let (res, old) = m.restart(
                &info.id,
                sh("sleep 30", vec![]),
                &RestartSession::Resume("sess-mv".into()),
                None,
                None,
                sink2,
            );
            res.unwrap();
            drop(old);
            assert_eq!(
                m.get(&info.id).unwrap().detail.as_deref(),
                Some(RESTARTING_TEXT)
            );
            assert!(m.apply_starting_hint(&info.id, now_ms() + 15_000).is_some());
            // Another detail (e.g. set by a hook) is never replaced.
            m.set_status(&info.id, AgentStatus::Starting, Some("x".into()))
                .unwrap();
            assert!(m.apply_starting_hint(&info.id, now_ms() + 60_000).is_none());
            m.stop(&info.id).unwrap();
        }

        /// A restart keeps id, session, cwd, creation time and the ring buffer; the old child's
        /// exit (generation 0) is ignored, the new child's output lands in the same buffer.
        #[test]
        fn restart_keeps_id_and_ignores_old_exit() {
            let mut m = AgentManager::new(5);
            let (sink, events) = collecting_sink();
            let info = m
                .spawn_spec(
                    sh("echo first; sleep 30", vec![]),
                    meta("sess-r"),
                    sink.clone(),
                )
                .unwrap();
            assert!(wait_for_output(&events, "first").contains("first"));
            m.set_status(&info.id, AgentStatus::Idle, None).unwrap();
            let moved = std::env::temp_dir().join(format!("mira-restart-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&moved).unwrap();
            let mut second = sh("echo second; sleep 30", vec![]);
            second.cwd = moved.clone();
            let (res, old) = m.restart(
                &info.id,
                second,
                &RestartSession::Resume("sess-r".into()),
                Some("opus".into()),
                Some("high".into()),
                sink.clone(),
            );
            let after = res.unwrap();
            drop(old);
            assert_eq!(after.id, info.id);
            assert_eq!(after.session_id, "sess-r");
            assert_eq!(after.created_at, info.created_at);
            assert_eq!(after.name, info.name);
            assert_eq!(after.cwd, moved.to_string_lossy());
            assert_ne!(after.cwd, info.cwd);
            assert_eq!(after.status, AgentStatus::Starting);
            assert_eq!(after.detail.as_deref(), Some(RESTARTING_TEXT));
            assert_ne!(after.pid, info.pid);
            assert_eq!(
                (
                    after.model.as_deref(),
                    after.effort.as_deref(),
                    after.model_observed
                ),
                (Some("opus"), Some("high"), false)
            );
            assert_eq!(m.pty_gen(&info.id), Some(1));
            // The killed first child reports its exit with generation 0: ignored.
            let t = Instant::now();
            let old_gen = loop {
                let found = events.lock().unwrap().iter().find_map(|e| match e {
                    SinkEvent::Exited { gen, .. } => Some(*gen),
                    _ => None,
                });
                if let Some(g) = found {
                    break g;
                }
                assert!(
                    t.elapsed() < Duration::from_secs(10),
                    "no exit of the old child"
                );
                std::thread::sleep(Duration::from_millis(20));
            };
            assert_eq!(old_gen, 0);
            assert!(m.mark_exited(&info.id, old_gen, None).is_none());
            assert_eq!(m.get(&info.id).unwrap().status, AgentStatus::Starting);
            // Same ring buffer: both outputs.
            wait_for_output(&events, "second");
            let (_, bytes) = m.output_snapshot(&info.id).unwrap();
            let text = String::from_utf8_lossy(&bytes);
            assert!(text.contains("first") && text.contains("second"), "{text}");
            // Exited agents cannot be restarted.
            m.stop(&info.id).unwrap();
            let resume = RestartSession::Resume("sess-r".into());
            let (res, _) = m.restart(
                &info.id,
                sh("true", vec![]),
                &resume,
                None,
                None,
                sink.clone(),
            );
            assert!(matches!(res, Err(AgentError::NotFound)));
            let (res, _) = m.restart("nope", sh("true", vec![]), &resume, None, None, sink);
            assert!(matches!(res, Err(AgentError::NotFound)));
            assert_eq!(m.list().len(), 1);
            let _ = std::fs::remove_dir_all(&moved);
        }

        /// The exit code of generation `gen` (waits up to 10 s).
        fn wait_for_gen_exit(events: &Arc<Mutex<Vec<SinkEvent>>>, want: u64) -> Option<i32> {
            let t = Instant::now();
            loop {
                let found = events.lock().unwrap().iter().find_map(|e| match e {
                    SinkEvent::Exited { gen, code, .. } if *gen == want => Some(*code),
                    _ => None,
                });
                if let Some(code) = found {
                    return code;
                }
                assert!(
                    t.elapsed() < Duration::from_secs(10),
                    "no exit of gen {want}"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        /// review5 N1: a fresh restart takes the new session id (session map follows); a
        /// `--resume` restart that dies with an error before its SessionStart gets
        /// [`RESTART_FAILED_TEXT`], a later error exit does not.
        #[test]
        fn fresh_restart_rebinds_and_failed_resume_says_so() {
            let mut m = AgentManager::new(5);
            let (sink, events) = collecting_sink();
            let info = m
                .spawn_spec(sh("sleep 30", vec![]), meta("sess-a"), sink.clone())
                .unwrap();
            m.set_status(&info.id, AgentStatus::Idle, None).unwrap();
            let fresh = RestartSession::Fresh("sess-new".into());
            let (res, old) = m.restart(
                &info.id,
                sh("sleep 30", vec![]),
                &fresh,
                None,
                None,
                sink.clone(),
            );
            drop(old);
            assert_eq!(res.unwrap().session_id, "sess-new");
            assert_eq!(m.agent_id_for_session("SESS-NEW"), Some(info.id.clone()));
            assert_eq!(m.agent_id_for_session("sess-a"), None);
            assert!(!m.has_conversation(&info.id));

            // The session had a turn → --resume; the resumed child exits 1 right away.
            m.mark_conversation(&info.id);
            m.set_status(&info.id, AgentStatus::Idle, None).unwrap();
            let resume = m.restart_session(&info.id).unwrap();
            assert_eq!(resume, RestartSession::Resume("sess-new".into()));
            let (res, old) = m.restart(
                &info.id,
                sh("exit 1", vec![]),
                &resume,
                None,
                None,
                sink.clone(),
            );
            drop(old);
            res.unwrap();
            let code = wait_for_gen_exit(&events, 2);
            assert_eq!(code, Some(1));
            let (exited, _pty) = m.mark_exited(&info.id, 2, code).unwrap();
            assert_eq!(exited.status, AgentStatus::Exited { code: Some(1) });
            assert_eq!(exited.detail.as_deref(), Some(RESTART_FAILED_TEXT));

            // After its SessionStart (Idle) an error exit is an ordinary exit.
            let other = m
                .spawn_spec(sh("sleep 30", vec![]), meta("sess-b"), sink.clone())
                .unwrap();
            m.set_status(&other.id, AgentStatus::Idle, None).unwrap();
            let (res, old) = m.restart(
                &other.id,
                sh("sleep 0.3; exit 1", vec![]),
                &RestartSession::Resume("sess-b".into()),
                None,
                None,
                sink.clone(),
            );
            drop(old);
            res.unwrap();
            m.set_status(&other.id, AgentStatus::Idle, None).unwrap();
            let code = wait_for_gen_exit(&events, 1);
            let (exited, _pty) = m.mark_exited(&other.id, 1, code).unwrap();
            assert_eq!(exited.detail, None);
        }

        #[test]
        fn kill_all_ends_every_group_within_the_quit_budget() {
            let mut m = AgentManager::new(5);
            let (sink, events) = collecting_sink();
            // SIGTERM is ignored by the shell and (inherited) by its child: only SIGKILL helps.
            let script = r#"trap "" TERM; sleep 30"#;
            let a = m
                .spawn_spec(sh(script, vec![]), meta("q-a"), sink.clone())
                .unwrap();
            let b = m.spawn_spec(sh(script, vec![]), meta("q-b"), sink).unwrap();
            std::thread::sleep(Duration::from_millis(200));
            let t = Instant::now();
            m.kill_all();
            let took = t.elapsed();
            assert!(
                took >= QUIT_KILL_BUDGET - Duration::from_millis(100)
                    && took < QUIT_KILL_BUDGET + Duration::from_secs(1),
                "{took:?}"
            );
            let deadline = Instant::now() + Duration::from_secs(3);
            for pid in [a.pid.unwrap(), b.pid.unwrap()] {
                while !process::pid_is_dead(pid) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(20));
                }
                assert!(process::pid_is_dead(pid), "agent pid {pid} survived quit");
            }
            let exits = || {
                events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|e| matches!(e, SinkEvent::Exited { .. }))
                    .count()
            };
            while exits() < 2 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(exits(), 2);
            // A second call (RunEvent::Exit after ExitRequested) returns at once.
            let t = Instant::now();
            m.kill_all();
            assert!(t.elapsed() < Duration::from_millis(200));
        }
    }
}
