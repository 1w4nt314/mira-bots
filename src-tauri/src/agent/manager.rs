//! Registry of agents. Knows nothing about Tauri: output and exits go to an [`EventSink`].
//!
//! Lives in `Arc<std::sync::Mutex<AgentManager>>`; hold the lock briefly and never while emitting.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::Serialize;

use super::pty::{self, PtyHandle, SpawnSpec};
use super::ring_buffer::RingBuffer;
use super::{now_ms, AgentError};
use crate::config::{DEFAULT_TOOL_WHITELIST, OUTPUT_RING_CAPACITY, PIPE_ENV, PTY_COLS, PTY_ROWS};
use crate::hooks::status::AgentStatus;

/// uuid v4 as a string.
pub type AgentId = String;

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
    pub prompt: Option<String>,
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
    by_session: HashMap<String, AgentId>,
    max_agents: usize,
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
/// `claude --settings <hooks.json> --session-id <uuid> [prompt]`, env `MIRA_BOTS_PIPE`.
/// No `-p`, no `--permission-mode`, no `--setting-sources`.
pub fn build_spawn_spec(req: &SpawnRequest, ctx: &SpawnContext, session_id: &str) -> SpawnSpec {
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
        env: vec![(PIPE_ENV.to_string(), ctx.pipe_name.clone())],
        cols: PTY_COLS,
        rows: PTY_ROWS,
    }
}

impl AgentManager {
    pub fn new(max_agents: usize) -> Self {
        Self {
            agents: HashMap::new(),
            by_session: HashMap::new(),
            max_agents,
        }
    }

    fn running_count(&self) -> usize {
        self.agents
            .values()
            .filter(|a| !is_exited(&a.info.status))
            .count()
    }

    fn check_limit(&self) -> Result<(), AgentError> {
        if self.running_count() >= self.max_agents {
            return Err(AgentError::LimitReached);
        }
        Ok(())
    }

    /// Starts `claude` in `req.cwd`. Checks, in order: agent limit, cwd is a directory,
    /// claude binary exists.
    pub fn spawn(
        &mut self,
        req: SpawnRequest,
        ctx: &SpawnContext,
        sink: EventSink,
    ) -> Result<AgentInfo, AgentError> {
        self.check_limit()?;
        if !req.cwd.is_dir() {
            return Err(AgentError::InvalidCwd);
        }
        if !ctx.claude.is_file() {
            return Err(AgentError::ClaudeNotFound);
        }
        let session_id = uuid::Uuid::new_v4().to_string();
        let spec = build_spawn_spec(&req, ctx, &session_id);
        self.spawn_spec(spec, session_id, sink)
    }

    /// Spawns an arbitrary [`SpawnSpec`] as an agent (used by `spawn`, and directly by tests).
    pub(crate) fn spawn_spec(
        &mut self,
        spec: SpawnSpec,
        session_id: String,
        sink: EventSink,
    ) -> Result<AgentInfo, AgentError> {
        self.check_limit()?;
        let id: AgentId = uuid::Uuid::new_v4().to_string();
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
        };
        self.insert(info.clone(), Some(handle), output);
        Ok(info)
    }

    fn insert(&mut self, info: AgentInfo, pty: Option<PtyHandle>, output: Arc<Mutex<RingBuffer>>) {
        self.by_session
            .insert(info.session_id.clone(), info.id.clone());
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

    /// Records the exit reported by the waiter thread and drops the PTY (closing the pseudo
    /// terminal, which also lets a ConPTY reader thread finish). Returns the updated info.
    pub fn mark_exited(&mut self, id: &str, code: Option<i32>) -> Option<AgentInfo> {
        let agent = self.agents.get_mut(id)?;
        // Keep a known code if stop() raced ahead with None; otherwise take the reported one.
        let keep =
            matches!(agent.info.status, AgentStatus::Exited { code: Some(_) }) && code.is_none();
        if !keep {
            agent.info.status = AgentStatus::Exited { code };
        }
        agent.info.detail = None;
        agent.info.last_event_at = now_ms();
        agent.pty = None;
        Some(agent.info.clone())
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

    /// Removes an exited agent.
    pub fn remove(&mut self, id: &str) -> Result<(), AgentError> {
        let agent = self.agents.get(id).ok_or(AgentError::NotFound)?;
        if !is_exited(&agent.info.status) {
            return Err(AgentError::StillRunning);
        }
        let session_id = agent.info.session_id.clone();
        self.agents.remove(id);
        self.by_session.remove(&session_id);
        Ok(())
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

    pub fn agent_id_for_session(&self, session_id: &str) -> Option<AgentId> {
        self.by_session.get(session_id).cloned()
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

    /// Test helper: an agent without a PTY.
    #[cfg(test)]
    pub fn insert_fake(&mut self, session_id: &str, cwd: &str) -> AgentId {
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
        };
        self.insert(info, None, Arc::new(Mutex::new(RingBuffer::new(1024))));
        id
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
        };
        let spec = build_spawn_spec(&req, &ctx(PathBuf::from("/bin/claude")), "sid");
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
            vec![("MIRA_BOTS_PIPE".to_string(), "pipe-x".to_string())]
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
            };
            let spec = build_spawn_spec(&req, &ctx(PathBuf::from("c")), "s");
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
        };
        let c = ctx(PathBuf::from("/nope/claude"));
        assert!(matches!(
            m.spawn(req(), &c, null_sink()),
            Err(AgentError::LimitReached)
        ));
        m.mark_exited(&ids[0], Some(0));
        // Past the limit now; fails on the next check instead.
        assert!(matches!(
            m.spawn(req(), &c, null_sink()),
            Err(AgentError::InvalidCwd)
        ));
        assert_eq!(
            AgentError::LimitReached.to_string(),
            "Loft på 5 agenter nået"
        );
    }

    #[test]
    fn spawn_rejects_missing_claude() {
        let mut m = AgentManager::new(5);
        let req = SpawnRequest {
            cwd: std::env::temp_dir(),
            prompt: None,
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
            m.mark_exited(&id, Some(3)).unwrap().status,
            AgentStatus::Exited { code: Some(3) }
        );
        // A later None does not erase a known code.
        assert_eq!(
            m.mark_exited(&id, None).unwrap().status,
            AgentStatus::Exited { code: Some(3) }
        );
        m.remove(&id).unwrap();
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
        ] {
            assert!(v.get(key).is_some(), "{key}");
        }
        assert_eq!(v["status"], json!({"kind":"starting"}));
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
                .spawn_spec(sh("echo hi; exit 3", vec![]), "sess".into(), sink)
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
            let exited = m.mark_exited(&info.id, Some(3)).unwrap();
            assert_eq!(exited.status, AgentStatus::Exited { code: Some(3) });
        }

        #[test]
        fn pipe_env_reaches_the_child() {
            let mut m = AgentManager::new(5);
            let (sink, events) = collecting_sink();
            let env = vec![(PIPE_ENV.to_string(), "/tmp/mira-bots-42.sock".to_string())];
            m.spawn_spec(
                sh("printf '<%s>' \"$MIRA_BOTS_PIPE\"", env),
                "s".into(),
                sink,
            )
            .unwrap();
            assert_eq!(wait_for_exit(&events), Some(0));
            assert!(wait_for_output(&events, "<").contains("</tmp/mira-bots-42.sock>"));
        }

        #[test]
        fn write_input_resize_and_stop() {
            let mut m = AgentManager::new(5);
            let (sink, events) = collecting_sink();
            let info = m
                .spawn_spec(
                    sh("read line; echo got:$line; sleep 30", vec![]),
                    "s".into(),
                    sink,
                )
                .unwrap();
            m.resize(&info.id, 100, 40).unwrap();
            m.write_input(&info.id, b"ping\n").unwrap();
            assert!(wait_for_output(&events, "got:ping").contains("got:ping"));
            let stopped = m.stop(&info.id).unwrap();
            assert_eq!(stopped.status, AgentStatus::Exited { code: None });
            wait_for_exit(&events);
            m.remove(&info.id).unwrap();
        }

        #[test]
        fn running_limit_blocks_spawn_until_one_stops() {
            let mut m = AgentManager::new(2);
            let (sink, events) = collecting_sink();
            let a = m
                .spawn_spec(sh("sleep 30", vec![]), "a".into(), sink.clone())
                .unwrap();
            m.spawn_spec(sh("sleep 30", vec![]), "b".into(), sink.clone())
                .unwrap();
            assert!(matches!(
                m.spawn_spec(sh("true", vec![]), "c".into(), sink.clone()),
                Err(AgentError::LimitReached)
            ));
            m.stop(&a.id).unwrap();
            let c = m.spawn_spec(sh("true", vec![]), "c".into(), sink).unwrap();
            assert_eq!(c.status, AgentStatus::Starting);
            for info in m.list() {
                let _ = m.stop(&info.id);
            }
            let _ = events;
        }
    }
}
