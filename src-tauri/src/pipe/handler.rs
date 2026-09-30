//! One pipe connection = one hook invocation: read one frame, update status, emit events and,
//! for PermissionRequest, answer with one decision line.

use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

use super::protocol;
use crate::agent::{now_ms, AgentManager};
use crate::config::{MAX_PIPE_LINE, PERMISSION_APP_DEADLINE};
use crate::events::{
    HookEventPayload, PermissionResolvedPayload, AGENTS_CHANGED, HOOK_EVENT, PERMISSION_REQUEST,
    PERMISSION_RESOLVED,
};
use crate::hooks::event::{self, summarize_tool_input, HookEvent};
use crate::hooks::status::{self, status_for_tool, AgentStatus};
use crate::permissions::{Decision, PendingPermissions, PermissionRequestInfo};

/// Emits a Tauri event (`name`, JSON payload). In the app this wraps `app.emit`.
pub type EmitFn = Arc<dyn Fn(&str, Value) + Send + Sync>;

#[derive(Clone)]
pub struct HandlerCtx {
    pub manager: Arc<Mutex<AgentManager>>,
    pub pending: Arc<Mutex<PendingPermissions>>,
    pub emit: EmitFn,
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
    fn set_status(&self, agent_id: &str, status: AgentStatus, detail: Option<String>) {
        let changed = lock(&self.manager)
            .set_status(agent_id, status, detail)
            .is_some();
        if changed {
            self.emit_agents();
        }
    }
}

/// Reads one `\n`-terminated line of at most `MAX_PIPE_LINE` bytes (newline included).
async fn read_frame_line<R: AsyncRead + Unpin>(r: &mut BufReader<R>) -> Option<String> {
    let mut buf = Vec::new();
    let n = r
        .take(MAX_PIPE_LINE as u64 + 1)
        .read_until(b'\n', &mut buf)
        .await
        .ok()?;
    if n == 0 || buf.len() > MAX_PIPE_LINE {
        return None;
    }
    String::from_utf8(buf).ok()
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

/// Handles one connection end to end. Never panics on bad input; just closes.
///
/// Emits: `hook-event` for every valid frame; `agents-changed` on status changes;
/// for PermissionRequests that go to the UI, `permission-request` and later exactly one
/// `permission-resolved` (so `respond_permission` must NOT emit `permission-resolved` itself;
/// it only calls `PendingPermissions::resolve`).
pub async fn handle_connection<S>(stream: S, ctx: HandlerCtx)
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let mut reader = BufReader::new(stream);
    let Some(line) = read_frame_line(&mut reader).await else {
        log::debug!("pipe: empty, oversized or non-UTF-8 line; closing");
        return;
    };
    let ev = match protocol::parse_frame(&line)
        .map_err(|e| e.to_string())
        .and_then(|v| event::parse(&v).map_err(|e| format!("invalid hook event: {e}")))
    {
        Ok(ev) => ev,
        Err(e) => {
            log::warn!("pipe: {e}");
            return;
        }
    };
    let mut stream = reader.into_inner();
    let is_permission = ev.hook_event_name == "PermissionRequest";

    let agent_id = lock(&ctx.manager).agent_id_for_session(&ev.session_id);
    if let Some(id) = &agent_id {
        let t = status::apply(&ev);
        if let Some(status) = t.status {
            ctx.set_status(id, status, t.detail);
        }
    } else {
        log::debug!(
            "pipe: {} for unknown session {}",
            ev.hook_event_name,
            ev.session_id
        );
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
        Some(id) => decide_permission(&ev, &id, &ctx).await,
        None => Decision::None,
    };
    // TODO(windows-verify): the reply line reaches the hook exe before the server end of the
    // named pipe is dropped (tokio's flush does not call FlushFileBuffers) (plan D.7/D.8).
    write_reply(&mut stream, decision).await;
}

/// Whitelist → allow now; UI not ready → none now; otherwise wait for the UI up to
/// `PERMISSION_APP_DEADLINE` (108 s), then none.
// TODO(windows-verify): while this waits, Claude Code's own terminal dialog is NOT shown, and it
// does appear after a `none` answer (research §1f, plan D.8).
async fn decide_permission(ev: &HookEvent, agent_id: &str, ctx: &HandlerCtx) -> Decision {
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
        return Decision::Allow;
    }
    if !lock(&ctx.pending).is_ui_ready() {
        // The terminal takes over; status stays WaitingPermission.
        return Decision::None;
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

    let decision = tokio::select! {
        d = &mut rx => d.unwrap_or(Decision::None),
        _ = tokio::time::sleep(PERMISSION_APP_DEADLINE) => {
            // Expire it. If the UI resolved it in the same instant, its answer is already in rx.
            lock(&ctx.pending).resolve(&request_id, Decision::None);
            rx.try_recv().unwrap_or(Decision::None)
        }
    };
    ctx.emit(
        PERMISSION_RESOLVED,
        &PermissionResolvedPayload {
            request_id,
            decision: decision.as_str().to_string(),
        },
    );
    apply_decision_status(ctx, agent_id, &tool_name, &summary, decision);
    decision
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
        Decision::Allow => ctx.set_status(agent_id, status_for_tool(tool_name), detail),
        Decision::Deny => ctx.set_status(agent_id, AgentStatus::Thinking, None),
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

    /// Sends one line and returns everything the handler wrote before closing.
    async fn round_trip(h: &Harness, line: &str) -> String {
        let (mut client, task) = h.start();
        client.write_all(line.as_bytes()).await.unwrap();
        let mut out = String::new();
        client.read_to_string(&mut out).await.unwrap();
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
        let out = round_trip(&h, &mira_hook::payload::to_frame(&p)).await;
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
        task.await.unwrap();
        assert_eq!(reply_decision(&out), "none");
    }
}
