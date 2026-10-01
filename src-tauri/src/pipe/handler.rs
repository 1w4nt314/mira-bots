//! One pipe connection = one frame. A hook frame: update status, emit events and, for
//! PermissionRequest, answer with one decision line. A tool frame (mira-mcp, step 4): run the
//! injected [`ToolHandler`] and answer with one `tool_result` line; nothing else happens.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use super::protocol::{self, FrameError, Incoming, ToolFrame, ToolResult};
use crate::agent::{now_ms, AgentManager};
use crate::config::{MAX_PIPE_LINE, PERMISSION_APP_DEADLINE};
use crate::diagnostics::{HookStats, LastHookEvent, LastToolCall};
use crate::events::{
    HookEventPayload, PermissionResolvedPayload, StatusEvent, AGENTS_CHANGED, HOOK_EVENT,
    PERMISSION_REQUEST, PERMISSION_RESOLVED,
};
use crate::hooks::event::{self, summarize_tool_input, HookEvent};
use crate::hooks::status::{self, status_for_tool, AgentStatus};
use crate::permissions::{Decision, PendingPermissions, PermissionRequestInfo};

pub use crate::events::EmitFn;

/// Told about every hook frame that matched an agent, after its status was applied (the app
/// glue forwards it to the ticket dispatcher; the handler knows nothing about tickets). Called
/// without any lock held; must not block.
pub type StatusObserver = Arc<dyn Fn(StatusEvent) + Send + Sync>;

/// Answers a tool frame from mira-mcp (the app glue binds it to `tickets::tools`; the handler
/// knows nothing about tickets). Called synchronously on the connection's task; it takes only
/// short locks and must not block. Must answer with the frame's `request_id`.
pub type ToolHandler = Arc<dyn Fn(ToolFrame) -> ToolResult + Send + Sync>;

/// Answer when no [`ToolHandler`] is installed.
pub const TOOLS_UNAVAILABLE: &str = "Værktøjer er ikke tilgængelige i appen";

#[derive(Clone)]
pub struct HandlerCtx {
    pub manager: Arc<Mutex<AgentManager>>,
    pub pending: Arc<Mutex<PendingPermissions>>,
    pub emit: EmitFn,
    /// Frame counters for `get_diagnostics` (same `Arc` as `AppState::hook_stats`).
    pub stats: Arc<HookStats>,
    /// See [`StatusObserver`]; `None` in tests that do not care.
    pub observer: Option<StatusObserver>,
    /// See [`ToolHandler`]; `None` answers every tool frame with [`TOOLS_UNAVAILABLE`].
    pub tools: Option<ToolHandler>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl HandlerCtx {
    fn emit<T: serde::Serialize>(&self, name: &str, payload: &T) {
        match serde_json::to_value(payload) {
            Ok(v) => (self.emit)(name, v),
            Err(e) => log::error!("serialize {name}: {e}"),
        }
    }

    fn emit_agents(&self) {
        let list = lock(&self.manager).list();
        self.emit(AGENTS_CHANGED, &list);
    }

    /// Sets status (lock released before emitting) and emits `agents-changed` if it changed.
    /// Returns whether it changed (i.e. whether `agents-changed` was emitted).
    fn set_status(&self, agent_id: &str, status: AgentStatus, detail: Option<String>) -> bool {
        let changed = lock(&self.manager)
            .set_status(agent_id, status, detail)
            .is_some();
        if changed {
            self.emit_agents();
        }
        changed
    }
}

/// After the reply is written, how long to wait for the hook exe to close its end (it does so
/// right after reading the line). Proves delivery before the server end is dropped.
const REPLY_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Reads one `\n`-terminated line of at most `MAX_PIPE_LINE` bytes (newline included).
/// Logs (debug) why a line is rejected.
async fn read_frame_line<R: AsyncRead + Unpin>(r: &mut BufReader<R>) -> Option<String> {
    let mut buf = Vec::new();
    let n = match r
        .take(MAX_PIPE_LINE as u64 + 1)
        .read_until(b'\n', &mut buf)
        .await
    {
        Ok(n) => n,
        Err(e) => {
            log::debug!("pipe: read failed after {} bytes: {e}; closing", buf.len());
            return None;
        }
    };
    if n == 0 {
        log::debug!("pipe: client closed without sending a frame");
        return None;
    }
    if buf.len() > MAX_PIPE_LINE {
        log::debug!(
            "pipe: frame rejected: read {} bytes without a newline (limit {MAX_PIPE_LINE}); closing",
            buf.len()
        );
        return None;
    }
    match String::from_utf8(buf) {
        Ok(s) => Some(s),
        Err(e) => {
            log::debug!(
                "pipe: frame of {} bytes is not UTF-8; closing",
                e.as_bytes().len()
            );
            None
        }
    }
}

async fn write_reply<W: AsyncWrite + Unpin>(w: &mut W, decision: Decision) {
    let line = protocol::render_reply(decision, None);
    let res = async {
        w.write_all(line.as_bytes()).await?;
        w.flush().await?;
        w.shutdown().await
    }
    .await;
    if let Err(e) = res {
        log::debug!("pipe reply ({}) not delivered: {e}", decision.as_str());
    }
}

/// Writes the `tool_result` line and shuts down the write half.
async fn write_tool_result<W: AsyncWrite + Unpin>(w: &mut W, result: &ToolResult) {
    let line = protocol::render_tool_result(result);
    let res = async {
        w.write_all(line.as_bytes()).await?;
        w.flush().await?;
        w.shutdown().await
    }
    .await;
    if let Err(e) = res {
        log::debug!("pipe tool_result {} not delivered: {e}", result.request_id);
    }
}

/// Reads (and discards) until the client closes its end, for at most [`REPLY_DRAIN_TIMEOUT`].
/// The hook exe closes right after reading the reply line, so EOF here means it got the line;
/// only then is the server end dropped (Windows may discard unread bytes on `CloseHandle`).
async fn drain_until_closed<R: AsyncRead + Unpin>(r: &mut R) {
    let mut scratch = [0u8; 256];
    let drained = tokio::time::timeout(REPLY_DRAIN_TIMEOUT, async {
        loop {
            match r.read(&mut scratch).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    })
    .await;
    if drained.is_err() {
        log::debug!("pipe: client did not close within {REPLY_DRAIN_TIMEOUT:?} after the reply");
    }
}

/// Handles one connection end to end. Never panics on bad input; just closes.
///
/// The frame is matched to an agent by its frame-level `agent_id` first, then by `session_id`
/// ([`AgentManager::match_frame`]); a rebound session id is reported with `agents-changed`.
///
/// Emits: `hook-event` for every valid frame; `agents-changed` on status changes, session rebinds
/// and when the Starting hint is cleared;
/// for PermissionRequests that go to the UI, `permission-request` and later exactly one
/// `permission-resolved` (so `respond_permission` must NOT emit `permission-resolved` itself;
/// it only calls `PendingPermissions::resolve`).
pub async fn handle_connection<S>(stream: S, ctx: HandlerCtx)
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let mut reader = BufReader::new(stream);
    let Some(line) = read_frame_line(&mut reader).await else {
        return;
    };
    let hook = match protocol::parse_frame(&line) {
        Ok(Incoming::Hook(f)) => f,
        Ok(Incoming::Tool(f)) => {
            handle_tool_frame(f, &ctx, reader.into_inner()).await;
            return;
        }
        Err(FrameError::Kind(kind)) => {
            log::debug!("pipe: ignoring frame of kind {kind:?}; closing");
            return;
        }
        Err(e) => {
            log::warn!("pipe: {e}");
            return;
        }
    };
    let (hint, ev) = match event::parse(&hook.event)
        .map(|ev| (hook.agent_id, ev))
        .map_err(|e| format!("invalid hook event: {e}"))
    {
        Ok(v) => v,
        Err(e) => {
            log::warn!("pipe: {e}");
            return;
        }
    };
    let mut stream = reader.into_inner();
    let is_permission = ev.hook_event_name == "PermissionRequest";

    // One lock: match (and possibly rebind) plus clearing the Starting hint. No emit under it.
    let (found, hint_cleared) = {
        let mut m = lock(&ctx.manager);
        let found = m.match_frame(hint.as_deref(), &ev.session_id);
        let cleared = found
            .as_ref()
            .is_some_and(|f| m.clear_starting_hint(&f.agent_id));
        (found, cleared)
    };
    ctx.stats.record(
        LastHookEvent {
            name: ev.hook_event_name.clone(),
            session_id: ev.session_id.clone(),
            agent_id: hint.clone(),
            at: now_ms(),
        },
        found.is_some(),
    );
    log::debug!(
        "hook {} session={} agent_hint={hint:?} -> {found:?}",
        ev.hook_event_name,
        ev.session_id
    );
    let agent_id = found.as_ref().map(|f| f.agent_id.clone());
    if let Some(f) = &found {
        if f.rebound {
            log::info!("agent {} rebound to session {}", f.agent_id, ev.session_id);
        }
        let t = status::apply(&ev);
        let implied = t.status.clone();
        let emitted = match t.status {
            Some(status) => ctx.set_status(&f.agent_id, status, t.detail),
            None => false,
        };
        // The UI must see the new sessionId / the cleared hint even without a status change.
        if (f.rebound || hint_cleared) && !emitted {
            ctx.emit_agents();
        }
        // Also when the status did not change: Stop/StopFailure/UserPromptSubmit matter to the
        // dispatcher on their own (turn ended, delivery confirmed).
        if let Some(observe) = &ctx.observer {
            observe(StatusEvent {
                agent_id: f.agent_id.clone(),
                hook_event_name: ev.hook_event_name.clone(),
                prompt: ev
                    .prompt
                    .clone()
                    .filter(|_| ev.hook_event_name == "UserPromptSubmit"),
                status: implied,
            });
        }
    }
    ctx.emit(
        HOOK_EVENT,
        &HookEventPayload {
            agent_id: agent_id.clone(),
            session_id: ev.session_id.clone(),
            hook_event_name: ev.hook_event_name.clone(),
            tool_name: ev.tool_name.clone(),
            received_at: now_ms(),
        },
    );

    if !is_permission {
        return;
    }
    let decision = match agent_id {
        Some(id) => decide_permission(&ev, &id, &ctx, &mut stream).await,
        None => Some(Decision::None),
    };
    let Some(decision) = decision else {
        // The hook exe went away while we waited; nobody to answer.
        return;
    };
    write_reply(&mut stream, decision).await;
    // TODO(windows-verify): the reply line reaches the hook exe before the server end of the
    // named pipe is dropped (tokio's flush does not call FlushFileBuffers; we wait for the
    // client's EOF instead) (plan D.7/D.8).
    drain_until_closed(&mut stream).await;
}

/// A tool frame: ask the [`ToolHandler`], record the call, write one `tool_result` line and wait
/// for mira-mcp to close (it does so right after reading the line). No `hook-event`, no status
/// change, no observer call. Arguments and results are never logged.
async fn handle_tool_frame<S>(frame: ToolFrame, ctx: &HandlerCtx, mut stream: S)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let tool = frame.tool.clone();
    let agent_id = frame.agent_id.clone();
    let request_id = frame.request_id.clone();
    let mut result = match &ctx.tools {
        Some(handle) => handle(frame),
        None => ToolResult {
            request_id: request_id.clone(),
            outcome: Err(TOOLS_UNAVAILABLE.to_string()),
        },
    };
    // The reply must carry the frame's id, whatever the handler did.
    result.request_id = request_id;
    let ok = result.outcome.is_ok();
    ctx.stats.record_tool(LastToolCall {
        tool: tool.clone(),
        agent_id: agent_id.clone(),
        ok,
        at: now_ms(),
    });
    log::info!(
        "tool {tool} agent={agent_id:?} -> {}",
        if ok { "ok" } else { "error" }
    );
    write_tool_result(&mut stream, &result).await;
    // TODO(windows-verify): mira-mcp gets the line before the server end is dropped (plan4 D.45).
    drain_until_closed(&mut stream).await;
}

/// Whitelist → allow now; UI not ready → none now; otherwise wait for the UI up to
/// `PERMISSION_APP_DEADLINE` (108 s), then none.
///
/// While waiting, `client` is read so a hook exe that goes away (EOF or error) is noticed: the
/// request is then resolved as `none`, `permission-resolved{none}` is emitted and `None` is
/// returned (nothing is written back).
// TODO(windows-verify): while this waits, Claude Code's own terminal dialog is NOT shown, and it
// does appear after a `none` answer (research §1f, plan D.8).
async fn decide_permission<C: AsyncRead + Unpin>(
    ev: &HookEvent,
    agent_id: &str,
    ctx: &HandlerCtx,
    client: &mut C,
) -> Option<Decision> {
    let tool_name = ev.tool_name.clone().unwrap_or_default();
    let summary = summarize_tool_input(&tool_name, ev.tool_input.as_ref());

    let (whitelisted, agent_name) = {
        let m = lock(&ctx.manager);
        (
            m.whitelist_contains(agent_id, &tool_name),
            m.get(agent_id).map(|a| a.name).unwrap_or_default(),
        )
    };
    if whitelisted {
        apply_decision_status(ctx, agent_id, &tool_name, &summary, Decision::Allow);
        return Some(Decision::Allow);
    }
    if !lock(&ctx.pending).is_ui_ready() {
        // The terminal takes over; status stays WaitingPermission.
        return Some(Decision::None);
    }

    let created_at = now_ms();
    let info = PermissionRequestInfo {
        request_id: uuid::Uuid::new_v4().to_string(),
        agent_id: agent_id.to_string(),
        agent_name,
        tool_name: tool_name.clone(),
        summary: summary.clone(),
        tool_input: ev.tool_input.clone().unwrap_or(Value::Null),
        created_at,
        deadline_at: created_at + PERMISSION_APP_DEADLINE.as_millis() as u64,
    };
    let request_id = info.request_id.clone();
    let mut rx = lock(&ctx.pending).insert(info.clone());
    ctx.emit(PERMISSION_REQUEST, &info);

    let deadline = tokio::time::sleep(PERMISSION_APP_DEADLINE);
    tokio::pin!(deadline);
    let mut scratch = [0u8; 256];
    // `Some(d)`: answer `d`; `None`: the client is gone.
    let outcome = loop {
        tokio::select! {
            d = &mut rx => break Some(d.unwrap_or(Decision::None)),
            _ = &mut deadline => {
                // Expire it. If the UI resolved it in the same instant, its answer is already in rx.
                lock(&ctx.pending).resolve(&request_id, Decision::None);
                break Some(rx.try_recv().unwrap_or(Decision::None));
            }
            r = client.read(&mut scratch) => match r {
                // The hook exe sends nothing after its frame; EOF or an error means it is gone
                // (killed by Claude Code, agent stopped, dialog answered in the terminal).
                Ok(0) | Err(_) => {
                    lock(&ctx.pending).resolve(&request_id, Decision::None);
                    break None;
                }
                // Unexpected bytes: ignore and keep waiting.
                Ok(_) => {}
            },
        }
    };
    let decision = outcome.unwrap_or(Decision::None);
    ctx.emit(
        PERMISSION_RESOLVED,
        &PermissionResolvedPayload {
            request_id,
            decision: decision.as_str().to_string(),
        },
    );
    apply_decision_status(ctx, agent_id, &tool_name, &summary, decision);
    outcome
}

/// allow → status of the tool, deny → Thinking, none → unchanged (WaitingPermission).
fn apply_decision_status(
    ctx: &HandlerCtx,
    agent_id: &str,
    tool_name: &str,
    summary: &str,
    decision: Decision,
) {
    let detail = (!summary.is_empty()).then(|| summary.to_string());
    match decision {
        Decision::Allow => {
            ctx.set_status(agent_id, status_for_tool(tool_name), detail);
        }
        Decision::Deny => {
            ctx.set_status(agent_id, AgentStatus::Thinking, None);
        }
        Decision::None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentId;
    use crate::hooks::fixtures as fx;
    use serde_json::json;
    use std::time::Duration;
    use tokio::io::{duplex, DuplexStream};

    type Events = Arc<Mutex<Vec<(String, Value)>>>;

    struct Harness {
        ctx: HandlerCtx,
        events: Events,
        agent: AgentId,
    }

    fn harness(ui_ready: bool) -> Harness {
        let mut m = AgentManager::new(5);
        let agent = m.insert_fake("sess-1", "/work/demo");
        let mut pending = PendingPermissions::new();
        if ui_ready {
            pending.set_ui_ready();
        }
        let events: Events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        Harness {
            ctx: HandlerCtx {
                manager: Arc::new(Mutex::new(m)),
                pending: Arc::new(Mutex::new(pending)),
                emit: Arc::new(move |name: &str, v: Value| {
                    sink.lock().unwrap().push((name.to_string(), v))
                }),
                stats: Arc::new(HookStats::default()),
                observer: None,
                tools: None,
            },
            events,
            agent,
        }
    }

    impl Harness {
        fn status(&self) -> AgentStatus {
            self.ctx
                .manager
                .lock()
                .unwrap()
                .get(&self.agent)
                .unwrap()
                .status
        }
        fn emitted(&self, name: &str) -> Vec<Value> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
                .collect()
        }
        fn start(&self) -> (DuplexStream, tokio::task::JoinHandle<()>) {
            let (client, server) = duplex(64 * 1024);
            let h = tokio::spawn(handle_connection(server, self.ctx.clone()));
            (client, h)
        }
    }

    fn frame(event_json: &str) -> String {
        let ev: Value = serde_json::from_str(event_json).unwrap();
        format!("{}\n", json!({"v":1,"kind":"hook","event":ev}))
    }

    fn frame_with_agent(event_json: &str, agent_id: &str) -> String {
        let ev: Value = serde_json::from_str(event_json).unwrap();
        format!(
            "{}\n",
            json!({"v":1,"kind":"hook","agent_id":agent_id,"event":ev})
        )
    }

    /// Sends one line and returns everything the handler wrote before closing.
    async fn round_trip(h: &Harness, line: &str) -> String {
        let (mut client, task) = h.start();
        client.write_all(line.as_bytes()).await.unwrap();
        let mut out = String::new();
        client.read_to_string(&mut out).await.unwrap();
        // Like the hook exe: close right after reading the reply.
        drop(client);
        task.await.unwrap();
        out
    }

    fn reply_decision(out: &str) -> String {
        assert!(out.ends_with('\n'), "reply must be one line: {out:?}");
        let v: Value = serde_json::from_str(out.trim_end()).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["kind"], "decision");
        v["decision"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn a_pre_tool_use_edit_sets_editing() {
        let h = harness(true);
        let out = round_trip(&h, &frame(fx::PRE_TOOL_USE)).await;
        assert_eq!(out, "", "non-permission events get no reply");
        assert_eq!(h.status(), AgentStatus::Editing);
        let lists = h.emitted(AGENTS_CHANGED);
        assert_eq!(lists.len(), 1);
        assert_eq!(lists[0][0]["status"], json!({"kind":"editing"}));
        let hook = h.emitted(HOOK_EVENT);
        assert_eq!(hook[0]["agentId"], json!(h.agent));
        assert_eq!(hook[0]["hookEventName"], "PreToolUse");
        assert_eq!(hook[0]["toolName"], "Edit");
    }

    #[tokio::test]
    async fn b_whitelisted_tool_is_allowed_immediately() {
        let h = harness(false);
        h.ctx
            .manager
            .lock()
            .unwrap()
            .whitelist_add(&h.agent, "Bash")
            .unwrap();
        let out = round_trip(&h, &frame(fx::PERMISSION_REQUEST)).await;
        assert_eq!(reply_decision(&out), "allow");
        assert_eq!(h.status(), AgentStatus::Running);
        assert!(h.emitted(PERMISSION_REQUEST).is_empty());
        assert!(h.ctx.pending.lock().unwrap().list().is_empty());
    }

    #[tokio::test]
    async fn c_ui_not_ready_answers_none() {
        let h = harness(false);
        let out = round_trip(&h, &frame(fx::PERMISSION_REQUEST)).await;
        assert_eq!(reply_decision(&out), "none");
        assert_eq!(h.status(), AgentStatus::WaitingPermission);
        assert!(h.emitted(PERMISSION_REQUEST).is_empty());
    }

    async fn wait_for_request(h: &Harness) -> Value {
        for _ in 0..500 {
            if let Some(v) = h.emitted(PERMISSION_REQUEST).pop() {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("permission-request was not emitted");
    }

    #[tokio::test]
    async fn d_ui_allow_is_forwarded() {
        let h = harness(true);
        let (mut client, task) = h.start();
        client
            .write_all(frame(fx::PERMISSION_REQUEST).as_bytes())
            .await
            .unwrap();
        let req = wait_for_request(&h).await;
        assert_eq!(h.status(), AgentStatus::WaitingPermission);
        assert_eq!(req["agentId"], json!(h.agent));
        assert_eq!(req["agentName"], "demo");
        assert_eq!(req["toolName"], "Bash");
        assert_eq!(req["summary"], "npm test");
        assert_eq!(req["toolInput"]["command"], "npm test");
        assert_eq!(
            req["deadlineAt"].as_u64().unwrap() - req["createdAt"].as_u64().unwrap(),
            108_000
        );
        let id = req["requestId"].as_str().unwrap().to_string();
        assert_eq!(h.ctx.pending.lock().unwrap().list().len(), 1);
        h.ctx
            .pending
            .lock()
            .unwrap()
            .resolve(&id, Decision::Allow)
            .unwrap();

        let mut out = String::new();
        client.read_to_string(&mut out).await.unwrap();
        drop(client);
        task.await.unwrap();
        assert_eq!(reply_decision(&out), "allow");
        assert_eq!(h.status(), AgentStatus::Running);
        assert_eq!(
            h.emitted(PERMISSION_RESOLVED),
            vec![json!({"requestId": id, "decision": "allow"})]
        );
    }

    #[tokio::test]
    async fn d2_ui_deny_is_forwarded() {
        let h = harness(true);
        let (mut client, task) = h.start();
        client
            .write_all(frame(fx::PERMISSION_REQUEST).as_bytes())
            .await
            .unwrap();
        let id = wait_for_request(&h).await["requestId"]
            .as_str()
            .unwrap()
            .to_string();
        h.ctx
            .pending
            .lock()
            .unwrap()
            .resolve(&id, Decision::Deny)
            .unwrap();
        let mut out = String::new();
        client.read_to_string(&mut out).await.unwrap();
        drop(client);
        task.await.unwrap();
        assert_eq!(reply_decision(&out), "deny");
        assert_eq!(h.status(), AgentStatus::Thinking);
    }

    #[tokio::test]
    async fn e_unknown_session_changes_nothing() {
        let h = harness(true);
        let before = h.status();
        let ev = fx::PRE_TOOL_USE.replace("sess-1", "someone-else");
        assert_eq!(round_trip(&h, &frame(&ev)).await, "");
        assert_eq!(h.status(), before);
        assert!(h.emitted(AGENTS_CHANGED).is_empty());
        assert_eq!(h.emitted(HOOK_EVENT)[0]["agentId"], Value::Null);
        // PermissionRequest from an unknown session: `none`, nothing pending.
        let ev = fx::PERMISSION_REQUEST.replace("sess-1", "someone-else");
        assert_eq!(reply_decision(&round_trip(&h, &frame(&ev)).await), "none");
        assert!(h.ctx.pending.lock().unwrap().list().is_empty());
    }

    #[tokio::test]
    async fn f_invalid_lines_close_without_panic() {
        let h = harness(true);
        for line in [
            "garbage\n".to_string(),
            "\n".to_string(),
            "{\"v\":2,\"kind\":\"hook\",\"event\":{}}\n".to_string(),
            "{\"v\":1,\"kind\":\"hook\",\"event\":{\"hook_event_name\":\"Stop\"}}\n".to_string(),
        ] {
            assert_eq!(round_trip(&h, &line).await, "");
        }
        // Client that closes without sending anything.
        let (client, task) = h.start();
        drop(client);
        task.await.unwrap();
        assert!(h.events.lock().unwrap().is_empty());
        assert_eq!(h.status(), AgentStatus::Starting);
    }

    #[tokio::test]
    async fn f2_oversized_line_is_rejected() {
        let h = harness(true);
        let (client, server) = duplex(64 * 1024);
        let task = tokio::spawn(handle_connection(server, h.ctx.clone()));
        let (mut rd, mut wr) = tokio::io::split(client);
        let writer = tokio::spawn(async move {
            let chunk = vec![b'x'; 64 * 1024];
            // Stops with an error once the handler hangs up.
            for _ in 0..20 {
                if wr.write_all(&chunk).await.is_err() {
                    break;
                }
            }
        });
        let mut out = String::new();
        rd.read_to_string(&mut out).await.unwrap();
        task.await.unwrap();
        writer.await.unwrap();
        assert_eq!(out, "");
        assert!(h.events.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn g_end_to_end_with_hook_exe_codec() {
        let h = harness(false);
        h.ctx
            .manager
            .lock()
            .unwrap()
            .whitelist_add(&h.agent, "Bash")
            .unwrap();
        let mut p = mira_hook::payload::parse(fx::PERMISSION_REQUEST).unwrap();
        mira_hook::payload::trim(&mut p.json);
        let out = round_trip(&h, &mira_hook::payload::to_frame(&p, None)).await;
        let d = mira_hook::decision::parse_reply(&out);
        assert_eq!(d, mira_hook::decision::Decision::Allow);
        assert_eq!(
            mira_hook::decision::to_stdout(&d).unwrap(),
            r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#
        );
    }

    #[tokio::test(start_paused = true)]
    async fn h_deadline_answers_none_after_108_s() {
        let h = harness(true);
        let t0 = tokio::time::Instant::now();
        let out = round_trip(&h, &frame(fx::PERMISSION_REQUEST)).await;
        let waited = t0.elapsed();
        assert_eq!(reply_decision(&out), "none");
        assert!(waited >= PERMISSION_APP_DEADLINE, "{waited:?}");
        assert!(waited < Duration::from_secs(110), "{waited:?}");
        assert!(h.ctx.pending.lock().unwrap().list().is_empty());
        let resolved = h.emitted(PERMISSION_RESOLVED);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0]["decision"], "none");
        assert_eq!(h.status(), AgentStatus::WaitingPermission);
    }

    #[tokio::test]
    async fn agent_removed_while_waiting_answers_none() {
        let h = harness(true);
        let (mut client, task) = h.start();
        client
            .write_all(frame(fx::PERMISSION_REQUEST).as_bytes())
            .await
            .unwrap();
        wait_for_request(&h).await;
        let gone = h.ctx.pending.lock().unwrap().remove_for_agent(&h.agent);
        assert_eq!(gone.len(), 1);
        let mut out = String::new();
        client.read_to_string(&mut out).await.unwrap();
        drop(client);
        task.await.unwrap();
        assert_eq!(reply_decision(&out), "none");
    }

    #[tokio::test]
    async fn client_gone_while_waiting_resolves_none_without_reply() {
        let h = harness(true);
        let (mut client, task) = h.start();
        client
            .write_all(frame(fx::PERMISSION_REQUEST).as_bytes())
            .await
            .unwrap();
        let id = wait_for_request(&h).await["requestId"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(h.ctx.pending.lock().unwrap().list().len(), 1);
        // The hook exe is killed while the card is shown.
        drop(client);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("handler must notice the client is gone")
            .unwrap();
        assert!(h.ctx.pending.lock().unwrap().list().is_empty());
        assert_eq!(
            h.emitted(PERMISSION_RESOLVED),
            vec![json!({"requestId": id, "decision": "none"})]
        );
        assert_eq!(h.status(), AgentStatus::WaitingPermission);
        // A late click from the UI now finds nothing.
        assert!(h
            .ctx
            .pending
            .lock()
            .unwrap()
            .resolve(&id, Decision::Allow)
            .is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn reply_waits_for_client_close_at_most_2_s() {
        let h = harness(false);
        let (mut client, task) = h.start();
        client
            .write_all(frame(fx::PERMISSION_REQUEST).as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        BufReader::new(&mut client)
            .read_line(&mut line)
            .await
            .unwrap();
        assert_eq!(reply_decision(&line), "none");
        // The client keeps its end open: the handler gives up after REPLY_DRAIN_TIMEOUT.
        let t0 = tokio::time::Instant::now();
        task.await.unwrap();
        let waited = t0.elapsed();
        assert!(waited >= REPLY_DRAIN_TIMEOUT, "{waited:?}");
        assert!(
            waited <= REPLY_DRAIN_TIMEOUT + Duration::from_millis(100),
            "{waited:?}"
        );
        drop(client);
    }

    #[tokio::test]
    async fn agent_id_hint_maps_despite_unknown_session() {
        let h = harness(true);
        let ev = fx::PRE_TOOL_USE.replace("sess-1", "new-sess");
        assert_eq!(round_trip(&h, &frame_with_agent(&ev, &h.agent)).await, "");
        assert_eq!(h.status(), AgentStatus::Editing);
        let info = h.ctx.manager.lock().unwrap().get(&h.agent).unwrap();
        assert_eq!(info.session_id, "new-sess");
        let lists = h.emitted(AGENTS_CHANGED);
        assert_eq!(lists.len(), 1);
        assert_eq!(lists[0][0]["sessionId"], "new-sess");
        assert_eq!(h.emitted(HOOK_EVENT)[0]["agentId"], json!(h.agent));
        assert_eq!(h.ctx.stats.received(), 1);
        assert_eq!(h.ctx.stats.unknown(), 0);
        let last = h.ctx.stats.last_event().unwrap();
        assert_eq!(last.agent_id.as_deref(), Some(h.agent.as_str()));
        assert_eq!(last.session_id, "new-sess");

        // Without a hint, the new session id now matches…
        let ev = fx::STOP.replace("sess-1", "new-sess");
        round_trip(&h, &frame(&ev)).await;
        assert_eq!(h.status(), AgentStatus::Idle);
        // …and the old one no longer does.
        let before = h.emitted(AGENTS_CHANGED).len();
        round_trip(&h, &frame(fx::PRE_TOOL_USE)).await;
        assert_eq!(h.status(), AgentStatus::Idle);
        assert_eq!(h.emitted(AGENTS_CHANGED).len(), before);
        assert_eq!(h.ctx.stats.received(), 3);
        assert_eq!(h.ctx.stats.unknown(), 1);
    }

    #[tokio::test]
    async fn rebind_without_status_change_still_emits_agents() {
        let h = harness(true);
        h.ctx.manager.lock().unwrap().stop(&h.agent).unwrap();
        let ev = fx::PRE_TOOL_USE.replace("sess-1", "after-clear");
        round_trip(&h, &frame_with_agent(&ev, &h.agent)).await;
        let lists = h.emitted(AGENTS_CHANGED);
        assert_eq!(
            lists.len(),
            1,
            "exited: no status change, but the rebind is shown"
        );
        assert_eq!(lists[0][0]["sessionId"], "after-clear");
    }

    #[tokio::test]
    async fn unknown_hint_falls_back_to_session() {
        let h = harness(true);
        round_trip(&h, &frame_with_agent(fx::PRE_TOOL_USE, "nope")).await;
        assert_eq!(h.status(), AgentStatus::Editing);
        let info = h.ctx.manager.lock().unwrap().get(&h.agent).unwrap();
        assert_eq!(info.session_id, "sess-1", "no rebind via session");
        assert_eq!(h.emitted(HOOK_EVENT)[0]["agentId"], json!(h.agent));
        assert_eq!((h.ctx.stats.received(), h.ctx.stats.unknown()), (1, 0));
        assert_eq!(
            h.ctx.stats.last_event().unwrap().agent_id.as_deref(),
            Some("nope")
        );
    }

    #[tokio::test]
    async fn no_hint_no_session_counts_unknown() {
        let h = harness(true);
        let ev = fx::PERMISSION_REQUEST.replace("sess-1", "someone-else");
        assert_eq!(reply_decision(&round_trip(&h, &frame(&ev)).await), "none");
        assert_eq!(h.ctx.stats.received(), 1);
        assert_eq!(h.ctx.stats.unknown(), 1);
        let last = h.ctx.stats.last_event().unwrap();
        assert_eq!(last.name, "PermissionRequest");
        assert_eq!(last.session_id, "someone-else");
        assert_eq!(last.agent_id, None);
        assert!(h.ctx.pending.lock().unwrap().list().is_empty());
        // Invalid frames are not counted.
        round_trip(&h, "garbage\n").await;
        assert_eq!(h.ctx.stats.received(), 1);
    }

    #[tokio::test]
    async fn first_hook_event_clears_the_starting_hint() {
        let h = harness(true);
        {
            let mut m = h.ctx.manager.lock().unwrap();
            m.backdate(&h.agent, 20_000);
            assert!(m.apply_starting_hint(&h.agent, now_ms()).is_some());
        }
        // An event that leaves the status unchanged still removes the hint.
        let ev = r#"{"session_id":"sess-1","hook_event_name":"Notification","message":"x"}"#;
        round_trip(&h, &frame(ev)).await;
        let info = h.ctx.manager.lock().unwrap().get(&h.agent).unwrap();
        assert_eq!(info.status, AgentStatus::Starting);
        assert_eq!(info.detail, None);
        assert_eq!(h.emitted(AGENTS_CHANGED).len(), 1);
    }

    #[tokio::test]
    async fn matched_frames_reach_the_status_observer() {
        let mut h = harness(true);
        let seen: Arc<Mutex<Vec<StatusEvent>>> = Arc::default();
        let sink = Arc::clone(&seen);
        h.ctx.observer = Some(Arc::new(move |ev| sink.lock().unwrap().push(ev)));
        let take = || std::mem::take(&mut *seen.lock().unwrap());

        round_trip(&h, &frame(fx::STOP)).await;
        assert_eq!(
            take(),
            vec![StatusEvent {
                agent_id: h.agent.clone(),
                hook_event_name: "Stop".into(),
                prompt: None,
                status: Some(AgentStatus::Idle),
            }]
        );
        // A second Stop leaves the status unchanged but still reaches the observer.
        round_trip(&h, &frame(fx::STOP)).await;
        assert_eq!(take().len(), 1);

        round_trip(&h, &frame(fx::USER_PROMPT_SUBMIT)).await;
        let evs = take();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].hook_event_name, "UserPromptSubmit");
        assert_eq!(evs[0].prompt.as_deref(), Some("fix the bug"));
        assert_eq!(evs[0].status, Some(AgentStatus::Thinking));

        // A notification without a status change is reported with `status: None`.
        round_trip(&h, &frame(fx::NOTIFICATION_OTHER)).await;
        let evs = take();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].status, None);

        // Unknown session: no agent, no observer call.
        let ev = fx::STOP.replace("sess-1", "someone-else");
        round_trip(&h, &frame(&ev)).await;
        assert!(take().is_empty());
    }

    fn tool_frame(request_id: &str, tool: &str) -> String {
        format!(
            "{}\n",
            json!({"v":1,"kind":"tool","agent_id":"agent-x","request_id":request_id,"tool":tool,"args":{"filter":"mine"}})
        )
    }

    fn tool_reply(out: &str) -> Value {
        assert!(out.ends_with('\n'), "reply must be one line: {out:?}");
        assert_eq!(out.matches('\n').count(), 1, "{out:?}");
        let v: Value = serde_json::from_str(out.trim_end()).unwrap();
        assert_eq!(
            (v["v"].clone(), v["kind"].clone()),
            (json!(1), json!("tool_result"))
        );
        v
    }

    #[tokio::test]
    async fn tool_frame_is_answered_by_the_injected_handler() {
        let mut h = harness(true);
        let seen: Arc<Mutex<Vec<ToolFrame>>> = Arc::default();
        let sink = Arc::clone(&seen);
        h.ctx.tools = Some(Arc::new(move |f: ToolFrame| {
            sink.lock().unwrap().push(f.clone());
            ToolResult {
                request_id: f.request_id,
                outcome: Ok(json!({"filter":"mine","tickets":[]})),
            }
        }));
        let observed: Arc<Mutex<usize>> = Arc::default();
        let n = Arc::clone(&observed);
        h.ctx.observer = Some(Arc::new(move |_| *n.lock().unwrap() += 1));
        let before = h.status();

        let out = round_trip(&h, &tool_frame("77-1", "mira_list_tickets")).await;
        let v = tool_reply(&out);
        assert_eq!(v["request_id"], "77-1");
        assert_eq!(v["ok"], true);
        assert_eq!(v["result"], json!({"filter":"mine","tickets":[]}));

        let frames = seen.lock().unwrap().clone();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].agent_id.as_deref(), Some("agent-x"));
        assert_eq!(frames[0].tool, "mira_list_tickets");
        assert_eq!(frames[0].args, json!({"filter":"mine"}));

        assert_eq!(
            (h.ctx.stats.tool_calls(), h.ctx.stats.tool_errors()),
            (1, 0)
        );
        let last = h.ctx.stats.last_tool_call().unwrap();
        assert_eq!(
            (last.tool.as_str(), last.agent_id.as_deref(), last.ok),
            ("mira_list_tickets", Some("agent-x"), true)
        );
        // Not a hook frame: no events, no status change, no observer, no hook counters.
        assert!(h.events.lock().unwrap().is_empty());
        assert_eq!(h.status(), before);
        assert_eq!(*observed.lock().unwrap(), 0);
        assert_eq!(h.ctx.stats.received(), 0);
    }

    #[tokio::test]
    async fn tool_error_and_missing_handler_answer_ok_false() {
        let mut h = harness(true);
        let out = round_trip(&h, &tool_frame("1-1", "mira_get_ticket")).await;
        let v = tool_reply(&out);
        assert_eq!(
            v,
            json!({"v":1,"kind":"tool_result","request_id":"1-1","ok":false,"error":TOOLS_UNAVAILABLE})
        );
        assert_eq!(
            (h.ctx.stats.tool_calls(), h.ctx.stats.tool_errors()),
            (1, 1)
        );

        // A handler error; a wrong request_id from the handler is corrected.
        h.ctx.tools = Some(Arc::new(|_f: ToolFrame| ToolResult {
            request_id: "other".into(),
            outcome: Err("Ukendt agent".into()),
        }));
        let v = tool_reply(&round_trip(&h, &tool_frame("1-2", "mira_get_ticket")).await);
        assert_eq!(v["request_id"], "1-2");
        assert_eq!(
            (v["ok"].clone(), v["error"].clone()),
            (json!(false), json!("Ukendt agent"))
        );
        assert_eq!(
            (h.ctx.stats.tool_calls(), h.ctx.stats.tool_errors()),
            (2, 2)
        );
        assert!(h.events.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unknown_kind_and_bad_tool_frames_close_without_reply() {
        let mut h = harness(true);
        let called: Arc<Mutex<usize>> = Arc::default();
        let n = Arc::clone(&called);
        h.ctx.tools = Some(Arc::new(move |f: ToolFrame| {
            *n.lock().unwrap() += 1;
            ToolResult {
                request_id: f.request_id,
                outcome: Ok(json!({})),
            }
        }));
        for line in [
            "{\"v\":1,\"kind\":\"something\",\"x\":1}\n",
            "{\"v\":1,\"kind\":\"tool\",\"tool\":\"mira_list_tickets\"}\n",
            "{\"v\":2,\"kind\":\"tool\",\"request_id\":\"r\",\"tool\":\"x\"}\n",
        ] {
            assert_eq!(round_trip(&h, line).await, "", "{line}");
        }
        assert_eq!(*called.lock().unwrap(), 0);
        assert_eq!((h.ctx.stats.tool_calls(), h.ctx.stats.received()), (0, 0));
        assert!(h.events.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn tool_reply_waits_for_client_close_at_most_2_s() {
        let h = harness(true);
        let (mut client, task) = h.start();
        client
            .write_all(tool_frame("9-9", "mira_list_tickets").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        BufReader::new(&mut client)
            .read_line(&mut line)
            .await
            .unwrap();
        assert_eq!(tool_reply(&line)["request_id"], "9-9");
        let t0 = tokio::time::Instant::now();
        task.await.unwrap();
        assert!(t0.elapsed() >= REPLY_DRAIN_TIMEOUT);
        drop(client);
    }
}
