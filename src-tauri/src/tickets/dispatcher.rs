//! The dispatcher: delivers the next queued ticket to an idle agent by writing the ticket file and
//! typing one line plus a separate Enter into its terminal, then waits for a confirmation
//! (plan A.4).
//!
//! `handle` is synchronous and processes one message at a time; every delay is a message the
//! dispatcher schedules to itself through [`Timers`], so tests run without a runtime
//! ([`FakeTimers`]). Each delivery sequence carries a token; timers with an outdated token are
//! ignored, so a cancelled sequence is never revived.
//!
//! Ports the app glue implements (plan B.8): [`AgentPort`] over the agent manager and
//! [`TicketsHost`] over the shared ticket service (`TicketsCtx::mutate`, which saves, syncs the
//! agents' ticket links and emits).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use super::model::{Ticket, TicketError, TicketIssue, TicketState};
use super::prompt;
use super::service::TicketService;
use crate::agent::now_ms;
use crate::config::{
    CONFIRM_TIMEOUT_MS, DELIVERY_FAILED_TEXT, DISPATCH_DELAY_MS, ENTER_DELAY_MS, RETRY_TIMEOUT_MS,
    SPAWN_CONFIRM_TIMEOUT_MS, TURN_FAILED_TEXT,
};
use crate::events::StatusEvent;
use crate::hooks::status::AgentStatus;

/// History note when the delivery was not confirmed after the retry.
pub const DELIVERY_UNCONFIRMED_NOTE: &str = "levering ikke bekræftet";
/// History note on `StopFailure` while a ticket was in progress.
pub const TURN_FAILED_NOTE: &str = "StopFailure";
/// History note when the terminal could not be written.
pub const TERMINAL_GONE_NOTE: &str = "Terminalen er væk";

/// What the dispatcher needs to know about an agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSnapshot {
    pub name: String,
    pub cwd: PathBuf,
    pub status: AgentStatus,
    /// Current detail text (to clear [`DELIVERY_FAILED_TEXT`]/[`TURN_FAILED_TEXT`] after a
    /// successful delivery).
    pub detail: Option<String>,
}

/// Access to the agents (implemented by `ManagerPort` over `Arc<Mutex<AgentManager>>`; each call
/// takes the manager lock briefly).
pub trait AgentPort: Send {
    /// `None` when the agent does not exist.
    fn snapshot(&self, id: &str) -> Option<AgentSnapshot>;
    fn write_input(&self, id: &str, bytes: &[u8]) -> Result<(), String>;
    /// Sets the agent's detail text; `false` when unknown or exited.
    fn set_detail(&self, id: &str, detail: Option<String>) -> bool;
}

/// Access to the shared ticket service (implemented for the app's `TicketsCtx`).
pub trait TicketsHost: Send {
    /// Runs `f` under the service lock; on success the implementation syncs the agents' ticket
    /// links and emits `tickets-changed` (+ `agents-changed`), without holding any lock.
    fn mutate<T>(
        &self,
        f: impl FnOnce(&mut TicketService) -> Result<T, TicketError>,
    ) -> Result<T, String>;
    /// Read-only access under the service lock; no emits.
    fn read<T>(&self, f: impl FnOnce(&TicketService) -> T) -> T;
}

/// Schedules `msg` to be fed back into the dispatcher after `delay_ms`.
pub trait Timers: Send {
    fn schedule(&mut self, delay_ms: u64, msg: DispatchMsg);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerKind {
    DispatchDelay,
    SendEnter,
    Confirm,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchMsg {
    /// The agent is idle (SessionStart, Stop, StopFailure, idle notification).
    AgentIdle { agent_id: String },
    /// A busy status (thinking/reading/editing/running/waiting for permission).
    AgentBusy { agent_id: String },
    /// `UserPromptSubmit`; `prompt` is never logged.
    PromptSubmitted {
        agent_id: String,
        prompt: Option<String>,
    },
    /// Stop (`failed: false`) or StopFailure (`failed: true`).
    TurnEnded { agent_id: String, failed: bool },
    /// The agent's queue changed (assign, reorder, reject, …).
    QueueChanged { agent_id: String },
    /// The agent stopped/exited/was removed (its tickets are already released).
    AgentGone { agent_id: String },
    /// `spawn_agent_with_ticket`: the line went in as the positional prompt.
    SpawnedWithTicket { agent_id: String, ticket_id: String },
    /// "Send igen" from the UI.
    Redispatch { ticket_id: String },
    Timer {
        agent_id: String,
        token: u64,
        kind: TimerKind,
    },
}

/// Maps a status notification from the pipe handler to dispatcher messages.
pub fn messages_for(ev: &StatusEvent) -> Vec<DispatchMsg> {
    let agent_id = ev.agent_id.clone();
    match ev.hook_event_name.as_str() {
        "UserPromptSubmit" => vec![DispatchMsg::PromptSubmitted {
            agent_id,
            prompt: ev.prompt.clone(),
        }],
        "Stop" | "StopFailure" => vec![
            DispatchMsg::TurnEnded {
                agent_id: agent_id.clone(),
                failed: ev.hook_event_name == "StopFailure",
            },
            DispatchMsg::AgentIdle { agent_id },
        ],
        _ => match &ev.status {
            Some(AgentStatus::Idle) => vec![DispatchMsg::AgentIdle { agent_id }],
            Some(
                AgentStatus::Thinking
                | AgentStatus::Reading
                | AgentStatus::Editing
                | AgentStatus::Running
                | AgentStatus::WaitingPermission,
            ) => vec![DispatchMsg::AgentBusy { agent_id }],
            _ => Vec::new(),
        },
    }
}

/// Per-agent delivery state (plan A.4).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Delivery {
    Free,
    /// Waiting `DISPATCH_DELAY_MS` before typing the queue head.
    Delaying {
        token: u64,
    },
    /// Line typed, Enter pending.
    Typed {
        ticket_id: String,
        token: u64,
    },
    /// Enter sent, waiting for `UserPromptSubmit` or a busy status.
    Waiting {
        ticket_id: String,
        token: u64,
        retried: bool,
        /// Came from `AwaitingSession`: a timeout falls back to an ordinary PTY dispatch.
        from_spawn: bool,
    },
    /// Spawned with the ticket line as positional prompt; waiting for the session to start.
    AwaitingSession {
        ticket_id: String,
        token: u64,
    },
}

pub struct Dispatcher<H, P, T> {
    host: H,
    port: P,
    timers: T,
    deliveries: HashMap<String, Delivery>,
    next_token: u64,
}

impl<H: TicketsHost, P: AgentPort, T: Timers> Dispatcher<H, P, T> {
    pub fn new(host: H, port: P, timers: T) -> Self {
        Dispatcher {
            host,
            port,
            timers,
            deliveries: HashMap::new(),
            next_token: 0,
        }
    }

    fn token(&mut self) -> u64 {
        self.next_token += 1;
        self.next_token
    }

    fn state(&self, agent_id: &str) -> &Delivery {
        self.deliveries.get(agent_id).unwrap_or(&Delivery::Free)
    }

    fn set(&mut self, agent_id: &str, d: Delivery) {
        if d == Delivery::Free {
            self.deliveries.remove(agent_id);
        } else {
            self.deliveries.insert(agent_id.to_string(), d);
        }
    }

    fn schedule(&mut self, agent_id: &str, token: u64, kind: TimerKind, delay_ms: u64) {
        self.timers.schedule(
            delay_ms,
            DispatchMsg::Timer {
                agent_id: agent_id.to_string(),
                token,
                kind,
            },
        );
    }

    pub fn handle(&mut self, msg: DispatchMsg) {
        match msg {
            DispatchMsg::AgentIdle { agent_id } => self.on_idle(&agent_id),
            DispatchMsg::AgentBusy { agent_id } => self.on_busy(&agent_id),
            DispatchMsg::PromptSubmitted { agent_id, prompt } => {
                self.on_prompt(&agent_id, prompt.as_deref())
            }
            DispatchMsg::TurnEnded { agent_id, failed } => self.on_turn_ended(&agent_id, failed),
            DispatchMsg::QueueChanged { agent_id } => self.consider(&agent_id),
            DispatchMsg::AgentGone { agent_id } => {
                self.deliveries.remove(&agent_id);
            }
            DispatchMsg::SpawnedWithTicket {
                agent_id,
                ticket_id,
            } => {
                let token = self.token();
                self.set(&agent_id, Delivery::AwaitingSession { ticket_id, token });
            }
            DispatchMsg::Redispatch { ticket_id } => self.redispatch(&ticket_id),
            DispatchMsg::Timer {
                agent_id,
                token,
                kind,
            } => self.on_timer(&agent_id, token, kind),
        }
    }

    /// Starts a delivery sequence if the agent is free, idle and has a queued ticket.
    // TODO(windows-verify): 750 ms after Stop the input field is ready, and the extra Enter on
    // retry neither sends an empty prompt nor closes a dialog (plan D.29).
    fn consider(&mut self, agent_id: &str) {
        if *self.state(agent_id) != Delivery::Free || self.ready_ticket(agent_id).is_none() {
            return;
        }
        let token = self.token();
        self.schedule(agent_id, token, TimerKind::DispatchDelay, DISPATCH_DELAY_MS);
        self.set(agent_id, Delivery::Delaying { token });
    }

    /// The agent (idle, not exited) and its next ticket, if a delivery may start now.
    fn ready_ticket(&self, agent_id: &str) -> Option<(AgentSnapshot, Ticket)> {
        let snap = self.port.snapshot(agent_id)?;
        if snap.status != AgentStatus::Idle {
            return None;
        }
        let ticket = self.host.read(|s| s.next_for_agent(agent_id))?;
        Some((snap, ticket))
    }

    fn on_idle(&mut self, agent_id: &str) {
        match self.state(agent_id).clone() {
            Delivery::Free => self.consider(agent_id),
            Delivery::AwaitingSession { ticket_id, token } => {
                // The session is up; the positional prompt should be submitted soon.
                self.schedule(
                    agent_id,
                    token,
                    TimerKind::Confirm,
                    SPAWN_CONFIRM_TIMEOUT_MS,
                );
                self.set(
                    agent_id,
                    Delivery::Waiting {
                        ticket_id,
                        token,
                        retried: true,
                        from_spawn: true,
                    },
                );
            }
            // Mid-sequence: no double dispatch.
            _ => {}
        }
    }

    fn on_busy(&mut self, agent_id: &str) {
        if let Delivery::Waiting { ticket_id, .. } | Delivery::AwaitingSession { ticket_id, .. } =
            self.state(agent_id).clone()
        {
            self.confirm(agent_id, &ticket_id);
        }
    }

    fn on_prompt(&mut self, agent_id: &str, prompt: Option<&str>) {
        let ticket_id = match self.state(agent_id) {
            Delivery::Waiting { ticket_id, .. }
            | Delivery::AwaitingSession { ticket_id, .. }
            | Delivery::Typed { ticket_id, .. } => ticket_id.clone(),
            _ => return,
        };
        let expected = format!("Ticket {}", super::model::short_id(&ticket_id));
        match prompt {
            Some(p) if p.trim_start().starts_with(&expected) => self.confirm(agent_id, &ticket_id),
            Some(_) => log::debug!("dispatch {agent_id}: submitted prompt is not the ticket line"),
            None => {
                log::warn!("dispatch {agent_id}: UserPromptSubmit without prompt; counting it as delivered");
                self.confirm(agent_id, &ticket_id);
            }
        }
    }

    /// Delivery confirmed: the ticket goes in progress (or gets a "sent again" entry).
    fn confirm(&mut self, agent_id: &str, ticket_id: &str) {
        self.set(agent_id, Delivery::Free);
        let snap = self.port.snapshot(agent_id);
        let name = snap
            .as_ref()
            .map_or_else(|| agent_id.to_string(), |s| s.name.clone());
        let now = now_ms();
        if let Err(e) = self
            .host
            .mutate(|s| s.mark_dispatched(ticket_id, &name, now))
        {
            log::warn!("dispatch {agent_id}: confirming ticket {ticket_id} failed: {e}");
        }
        if snap.is_some_and(|s| {
            matches!(
                s.detail.as_deref(),
                Some(DELIVERY_FAILED_TEXT | TURN_FAILED_TEXT)
            )
        }) {
            self.port.set_detail(agent_id, None);
        }
    }

    // TODO(windows-verify): Stop moves the ticket to review (done with skipReview) right after the
    // answer; Esc mid-turn gives no Stop (the ticket stays in progress); StopFailure shows the
    // "Turn fejlede" hint and "Send igen" works (plan D.33).
    fn on_turn_ended(&mut self, agent_id: &str, failed: bool) {
        let now = now_ms();
        if failed {
            let Some(t) = self.host.read(|s| s.current_for_agent(agent_id)) else {
                return;
            };
            let r = self.host.mutate(|s| {
                s.set_issue(
                    &t.id,
                    Some(TicketIssue::TurnFailed),
                    Some(TURN_FAILED_NOTE.into()),
                    now,
                )
            });
            if let Err(e) = r {
                log::warn!("dispatch {agent_id}: marking the turn failure failed: {e}");
            }
            self.port
                .set_detail(agent_id, Some(TURN_FAILED_TEXT.to_string()));
        } else {
            if let Err(e) = self.host.mutate(|s| s.complete_turn(agent_id, now)) {
                log::warn!("dispatch {agent_id}: completing the turn failed: {e}");
            }
            self.consider(agent_id);
        }
    }

    fn on_timer(&mut self, agent_id: &str, token: u64, kind: TimerKind) {
        let state = self.state(agent_id).clone();
        match (state, kind) {
            (Delivery::Delaying { token: t }, TimerKind::DispatchDelay) if t == token => {
                self.set(agent_id, Delivery::Free);
                if let Some((snap, ticket)) = self.ready_ticket(agent_id) {
                    self.type_ticket(agent_id, &snap, &ticket, token);
                }
            }
            (
                Delivery::Typed {
                    ticket_id,
                    token: t,
                },
                TimerKind::SendEnter,
            ) if t == token => {
                if self.write_enter(agent_id, &ticket_id) {
                    self.schedule(agent_id, token, TimerKind::Confirm, CONFIRM_TIMEOUT_MS);
                    self.set(
                        agent_id,
                        Delivery::Waiting {
                            ticket_id,
                            token,
                            retried: false,
                            from_spawn: false,
                        },
                    );
                }
            }
            (
                Delivery::Waiting {
                    ticket_id,
                    token: t,
                    retried,
                    from_spawn,
                },
                TimerKind::Confirm,
            ) if t == token => {
                if !retried {
                    // Enter may have been swallowed (popup, invisible chars): press it once more.
                    if self.write_enter(agent_id, &ticket_id) {
                        self.schedule(agent_id, token, TimerKind::Confirm, RETRY_TIMEOUT_MS);
                        self.set(
                            agent_id,
                            Delivery::Waiting {
                                ticket_id,
                                token,
                                retried: true,
                                from_spawn,
                            },
                        );
                    }
                } else if from_spawn {
                    log::info!(
                        "dispatch {agent_id}: positional prompt not confirmed; typing the ticket instead"
                    );
                    self.set(agent_id, Delivery::Free);
                    self.consider(agent_id);
                } else {
                    self.delivery_failed(agent_id, &ticket_id);
                }
            }
            _ => log::debug!("dispatch {agent_id}: ignoring stale {kind:?} timer"),
        }
    }

    /// Writes the ticket file and types the line; Enter follows after `ENTER_DELAY_MS`.
    fn type_ticket(&mut self, agent_id: &str, snap: &AgentSnapshot, ticket: &Ticket, token: u64) {
        let now = now_ms();
        if let Err(e) = prompt::write_ticket_file(&snap.cwd, ticket, now) {
            log::warn!("dispatch {agent_id}: writing the ticket file failed: {e}");
            self.send_to_backlog(
                agent_id,
                &ticket.id,
                &format!("Kunne ikke skrive ticket-fil: {e}"),
            );
            return;
        }
        let line = prompt::line_for(ticket);
        if let Err(e) = self.port.write_input(agent_id, line.as_bytes()) {
            log::warn!("dispatch {agent_id}: typing the ticket line failed: {e}");
            self.send_to_backlog(agent_id, &ticket.id, TERMINAL_GONE_NOTE);
            return;
        }
        log::info!("dispatch {agent_id}: typed ticket {}", ticket.short_id());
        self.schedule(agent_id, token, TimerKind::SendEnter, ENTER_DELAY_MS);
        self.set(
            agent_id,
            Delivery::Typed {
                ticket_id: ticket.id.clone(),
                token,
            },
        );
    }

    /// Sends `\r` on its own. On failure the ticket goes to the backlog and `false` is returned.
    // TODO(windows-verify): a separate `\r` write submits the typed line under ConPTY (plan D.28).
    fn write_enter(&mut self, agent_id: &str, ticket_id: &str) -> bool {
        match self.port.write_input(agent_id, b"\r") {
            Ok(()) => true,
            Err(e) => {
                log::warn!("dispatch {agent_id}: sending Enter failed: {e}");
                self.send_to_backlog(agent_id, ticket_id, TERMINAL_GONE_NOTE);
                false
            }
        }
    }

    fn send_to_backlog(&mut self, agent_id: &str, ticket_id: &str, note: &str) {
        self.set(agent_id, Delivery::Free);
        let now = now_ms();
        if let Err(e) = self.host.mutate(|s| s.to_backlog(ticket_id, note, now)) {
            log::warn!("dispatch {agent_id}: moving ticket {ticket_id} to the backlog failed: {e}");
        }
    }

    /// No confirmation after the retry: the ticket stays first in the queue with an issue.
    fn delivery_failed(&mut self, agent_id: &str, ticket_id: &str) {
        self.set(agent_id, Delivery::Free);
        log::warn!("dispatch {agent_id}: delivery of ticket {ticket_id} not confirmed");
        let now = now_ms();
        let r = self.host.mutate(|s| {
            s.set_issue(
                ticket_id,
                Some(TicketIssue::DeliveryFailed),
                Some(DELIVERY_UNCONFIRMED_NOTE.into()),
                now,
            )
        });
        if let Err(e) = r {
            log::warn!("dispatch {agent_id}: marking the delivery failure failed: {e}");
        }
        self.port
            .set_detail(agent_id, Some(DELIVERY_FAILED_TEXT.to_string()));
    }

    /// "Send igen": the queue head or the ticket in progress, to an idle agent with no delivery
    /// running. Clears the issue and types the ticket right away.
    fn redispatch(&mut self, ticket_id: &str) {
        let Some(t) = self.host.read(|s| s.get(ticket_id)) else {
            log::info!("redispatch: ticket {ticket_id} not found");
            return;
        };
        let Some(agent_id) = t.assignee_agent_id.clone() else {
            log::info!("redispatch: ticket {} has no agent", t.short_id());
            return;
        };
        let eligible = match t.state {
            TicketState::InProgress => true,
            TicketState::Assigned => {
                self.host
                    .read(|s| s.next_for_agent(&agent_id))
                    .map(|n| n.id)
                    == Some(t.id.clone())
            }
            _ => false,
        };
        let snap = self.port.snapshot(&agent_id);
        let idle = snap.as_ref().is_some_and(|s| s.status == AgentStatus::Idle);
        if !eligible || !idle || *self.state(&agent_id) != Delivery::Free {
            log::info!("redispatch: ticket {} is not deliverable now", t.short_id());
            return;
        }
        let now = now_ms();
        let ticket = match self
            .host
            .mutate(|s| s.set_issue(ticket_id, None, None, now))
        {
            Ok(t) => t,
            Err(e) => {
                log::warn!("redispatch: clearing the issue failed: {e}");
                return;
            }
        };
        let token = self.token();
        if let Some(snap) = snap {
            self.type_ticket(&agent_id, &snap, &ticket, token);
        }
    }
}

/// The dispatcher task: one message at a time until all senders are gone.
pub async fn run<H: TicketsHost, P: AgentPort, T: Timers>(
    mut rx: UnboundedReceiver<DispatchMsg>,
    mut d: Dispatcher<H, P, T>,
) {
    while let Some(m) = rx.recv().await {
        d.handle(m);
    }
}

/// Timers on the Tauri async runtime; the message comes back through the dispatcher's channel.
pub struct RealTimers {
    tx: UnboundedSender<DispatchMsg>,
}

impl RealTimers {
    pub fn new(tx: UnboundedSender<DispatchMsg>) -> Self {
        RealTimers { tx }
    }
}

impl Timers for RealTimers {
    fn schedule(&mut self, delay_ms: u64, msg: DispatchMsg) {
        let tx = self.tx.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            // The receiver is gone only at shutdown.
            let _ = tx.send(msg);
        });
    }
}

/// Manual clock for tests: `advance` returns what came due, in (time, schedule order).
/// Clones share the clock, so a test keeps one while the dispatcher owns another.
#[doc(hidden)]
#[derive(Clone, Default)]
pub struct FakeTimers {
    inner: Arc<Mutex<FakeClock>>,
}

#[derive(Default)]
struct FakeClock {
    now: u64,
    seq: u64,
    due: Vec<(u64, u64, DispatchMsg)>,
}

impl FakeTimers {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, FakeClock> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn now(&self) -> u64 {
        self.lock().now
    }

    pub fn pending(&self) -> usize {
        self.lock().due.len()
    }

    /// Removes and returns the earliest message due at or before `until`, moving the clock to
    /// its due time. Messages scheduled while handling it are seen by the next call.
    pub fn pop_due(&self, until: u64) -> Option<DispatchMsg> {
        let mut c = self.lock();
        let i = c
            .due
            .iter()
            .enumerate()
            .filter(|(_, (at, _, _))| *at <= until)
            .min_by_key(|(_, (at, seq, _))| (*at, *seq))
            .map(|(i, _)| i)?;
        let (at, _, msg) = c.due.remove(i);
        c.now = c.now.max(at);
        Some(msg)
    }

    /// Moves the clock forward by `ms` and returns every message due by then.
    pub fn advance(&self, ms: u64) -> Vec<DispatchMsg> {
        let until = self.now() + ms;
        let mut out = Vec::new();
        while let Some(m) = self.pop_due(until) {
            out.push(m);
        }
        self.lock().now = until;
        out
    }
}

impl Timers for FakeTimers {
    fn schedule(&mut self, delay_ms: u64, msg: DispatchMsg) {
        let mut c = self.lock();
        c.seq += 1;
        let entry = (c.now + delay_ms, c.seq, msg);
        c.due.push(entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::event::parse;
    use crate::hooks::fixtures as fx;
    use crate::hooks::status::apply;
    use crate::tickets::model::{TicketActor, TicketDoc};
    use crate::tickets::store::MemoryStore;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use TicketState as S;

    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap()
    }

    /// Host over a shared service; counts successful mutations (stand-in for the emits).
    #[derive(Clone)]
    struct TestHost {
        svc: Arc<Mutex<TicketService>>,
        mutations: Arc<AtomicUsize>,
    }

    impl TicketsHost for TestHost {
        fn mutate<R>(
            &self,
            f: impl FnOnce(&mut TicketService) -> Result<R, TicketError>,
        ) -> Result<R, String> {
            let r = f(&mut lock(&self.svc)).map_err(String::from);
            if r.is_ok() {
                self.mutations.fetch_add(1, Ordering::SeqCst);
            }
            r
        }

        fn read<R>(&self, f: impl FnOnce(&TicketService) -> R) -> R {
            f(&lock(&self.svc))
        }
    }

    #[derive(Default)]
    struct PortState {
        snapshots: HashMap<String, AgentSnapshot>,
        writes: Vec<(String, Vec<u8>)>,
        fail_writes: bool,
    }

    #[derive(Clone, Default)]
    struct FakePort(Arc<Mutex<PortState>>);

    impl AgentPort for FakePort {
        fn snapshot(&self, id: &str) -> Option<AgentSnapshot> {
            lock(&self.0).snapshots.get(id).cloned()
        }

        fn write_input(&self, id: &str, bytes: &[u8]) -> Result<(), String> {
            let mut s = lock(&self.0);
            if s.fail_writes || !s.snapshots.contains_key(id) {
                return Err("Agenten findes ikke".into());
            }
            s.writes.push((id.to_string(), bytes.to_vec()));
            Ok(())
        }

        fn set_detail(&self, id: &str, detail: Option<String>) -> bool {
            match lock(&self.0).snapshots.get_mut(id) {
                Some(s) => {
                    s.detail = detail;
                    true
                }
                None => false,
            }
        }
    }

    struct Harness {
        d: Dispatcher<TestHost, FakePort, FakeTimers>,
        svc: Arc<Mutex<TicketService>>,
        port: FakePort,
        timers: FakeTimers,
        root: PathBuf,
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    impl Harness {
        fn new() -> Self {
            let svc = Arc::new(Mutex::new(TicketService::new(
                Box::new(MemoryStore::new()),
                TicketDoc::default(),
            )));
            let host = TestHost {
                svc: svc.clone(),
                mutations: Arc::default(),
            };
            let port = FakePort::default();
            let timers = FakeTimers::new();
            let root = std::env::temp_dir().join(format!("mira-dispatch-{}", uuid::Uuid::new_v4()));
            Harness {
                d: Dispatcher::new(host, port.clone(), timers.clone()),
                svc,
                port,
                timers,
                root,
            }
        }

        /// A live agent with its own cwd.
        fn agent(&self, id: &str, status: AgentStatus) {
            let cwd = self.root.join(id);
            fs::create_dir_all(&cwd).unwrap();
            lock(&self.port.0).snapshots.insert(
                id.to_string(),
                AgentSnapshot {
                    name: format!("bot-{id}"),
                    cwd,
                    status,
                    detail: None,
                },
            );
        }

        fn set_status(&self, id: &str, status: AgentStatus) {
            lock(&self.port.0).snapshots.get_mut(id).unwrap().status = status;
        }

        fn remove_agent(&self, id: &str) {
            lock(&self.port.0).snapshots.remove(id);
        }

        fn detail(&self, id: &str) -> Option<String> {
            lock(&self.port.0)
                .snapshots
                .get(id)
                .and_then(|s| s.detail.clone())
        }

        fn svc(&self) -> MutexGuard<'_, TicketService> {
            lock(&self.svc)
        }

        /// Creates a ticket and queues it for `agent`.
        fn queued(&self, agent: &str, title: &str) -> Ticket {
            let mut s = self.svc();
            let t = s.create(title, "Gør det", false, 1).unwrap();
            s.assign(&t.id, agent, 2).unwrap()
        }

        fn ticket(&self, id: &str) -> Ticket {
            self.svc().get(id).unwrap()
        }

        fn send(&mut self, m: DispatchMsg) {
            self.d.handle(m);
        }

        fn idle(&mut self, agent: &str) {
            self.send(DispatchMsg::AgentIdle {
                agent_id: agent.into(),
            });
        }

        fn submitted(&mut self, agent: &str, prompt: &str) {
            self.send(DispatchMsg::PromptSubmitted {
                agent_id: agent.into(),
                prompt: Some(prompt.into()),
            });
        }

        /// Advances the fake clock, handling every timer that comes due (including ones
        /// scheduled on the way).
        fn advance(&mut self, ms: u64) {
            let until = self.timers.now() + ms;
            while let Some(m) = self.timers.pop_due(until) {
                self.d.handle(m);
            }
            assert!(self.timers.advance(0).is_empty());
            let _ = self.timers.advance(until - self.timers.now());
        }

        fn writes(&self) -> Vec<(String, String)> {
            lock(&self.port.0)
                .writes
                .iter()
                .map(|(a, b)| (a.clone(), String::from_utf8(b.clone()).unwrap()))
                .collect()
        }

        fn ticket_file(&self, agent: &str, t: &Ticket) -> String {
            fs::read_to_string(
                prompt::ticket_dir(&self.root.join(agent)).join(format!("{}.md", t.short_id())),
            )
            .unwrap()
        }
    }

    fn line(t: &Ticket) -> String {
        prompt::line_for(t)
    }

    fn enter(agent: &str) -> (String, String) {
        (agent.to_string(), "\r".to_string())
    }

    /// Delivers `t` to `agent` up to (but not including) the confirmation.
    fn deliver_until_enter(h: &mut Harness, agent: &str) {
        h.idle(agent);
        h.advance(DISPATCH_DELAY_MS + ENTER_DELAY_MS);
    }

    // (1)
    #[test]
    fn idle_agent_gets_line_then_separate_enter() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        let t = h.queued("a1", "Ret login");
        h.idle("a1");
        h.advance(DISPATCH_DELAY_MS - 1);
        assert!(h.writes().is_empty());
        h.advance(1);
        assert_eq!(h.writes(), vec![("a1".into(), line(&t))]);
        assert!(!h.writes()[0].1.contains('\r') && !h.writes()[0].1.contains('\n'));
        assert!(h.ticket_file("a1", &t).contains("## Opgave\n\nGør det\n"));
        h.advance(ENTER_DELAY_MS);
        assert_eq!(h.writes(), vec![("a1".into(), line(&t)), enter("a1")]);
        assert_eq!(h.ticket(&t.id).state, S::Assigned);
    }

    // (2) + busy status as confirmation
    #[test]
    fn prompt_submit_or_busy_status_confirms_the_delivery() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        let t = h.queued("a1", "Ret login");
        deliver_until_enter(&mut h, "a1");
        // A different prompt (the user typed something) is no confirmation.
        h.submitted("a1", "noget andet");
        assert_eq!(h.ticket(&t.id).state, S::Assigned);
        h.submitted("a1", &format!("Ticket {}: Ret login. Læs …", t.short_id()));
        let now = h.ticket(&t.id);
        assert_eq!(now.state, S::InProgress);
        assert_eq!(
            now.history.last().unwrap().note.as_deref(),
            Some("sendt til bot-a1")
        );
        assert_eq!(h.svc().links().get("a1"), Some(&(Some(t.id.clone()), 0)));
        assert!(h.d.host.mutations.load(Ordering::SeqCst) >= 1);
        // The pending confirm timer is now stale.
        h.advance(CONFIRM_TIMEOUT_MS + RETRY_TIMEOUT_MS);
        assert_eq!(h.writes().len(), 2);

        // Busy status instead of UserPromptSubmit.
        h.agent("a2", AgentStatus::Idle);
        let t2 = h.queued("a2", "Andet");
        deliver_until_enter(&mut h, "a2");
        h.send(DispatchMsg::AgentBusy {
            agent_id: "a2".into(),
        });
        assert_eq!(h.ticket(&t2.id).state, S::InProgress);
    }

    // (3)
    #[test]
    fn unconfirmed_delivery_retries_enter_once_then_fails() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        let t = h.queued("a1", "Ret login");
        deliver_until_enter(&mut h, "a1");
        h.advance(CONFIRM_TIMEOUT_MS - 1);
        assert_eq!(h.writes().len(), 2);
        h.advance(1);
        assert_eq!(h.writes().last(), Some(&enter("a1")));
        assert_eq!(h.writes().len(), 3);
        h.advance(RETRY_TIMEOUT_MS);
        assert_eq!(h.writes().len(), 3);
        let now = h.ticket(&t.id);
        assert_eq!((now.state, now.queue_position), (S::Assigned, Some(0)));
        assert_eq!(now.issue, Some(TicketIssue::DeliveryFailed));
        assert_eq!(
            now.history.last().unwrap().note.as_deref(),
            Some(DELIVERY_UNCONFIRMED_NOTE)
        );
        assert_eq!(h.detail("a1").as_deref(), Some(DELIVERY_FAILED_TEXT));
        assert_eq!(h.timers.pending(), 0);
        // Next idle tries again; a confirmation clears issue and detail.
        deliver_until_enter(&mut h, "a1");
        assert_eq!(h.writes().len(), 5);
        h.submitted("a1", &line(&t));
        let now = h.ticket(&t.id);
        assert_eq!((now.state, now.issue), (S::InProgress, None));
        assert_eq!(h.detail("a1"), None);
    }

    // (4)
    #[test]
    fn busy_agent_is_not_typed_into_until_idle() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Running);
        let t = h.queued("a1", "Ret login");
        h.send(DispatchMsg::QueueChanged {
            agent_id: "a1".into(),
        });
        h.idle("a1"); // status still Running in the manager
        h.advance(10_000);
        assert!(h.writes().is_empty());
        // Became busy again during the delay: the re-check stops the sequence.
        h.set_status("a1", AgentStatus::Idle);
        h.idle("a1");
        h.set_status("a1", AgentStatus::WaitingPermission);
        h.advance(DISPATCH_DELAY_MS);
        assert!(h.writes().is_empty());
        h.set_status("a1", AgentStatus::Idle);
        h.idle("a1");
        h.advance(DISPATCH_DELAY_MS);
        assert_eq!(h.writes(), vec![("a1".into(), line(&t))]);
    }

    // (5)
    #[test]
    fn turn_end_moves_the_ticket_to_review_or_done() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        let a = h.queued("a1", "A");
        let b = h.queued("a1", "B");
        h.svc()
            .update(
                &b.id,
                crate::tickets::model::TicketPatch {
                    skip_review: Some(true),
                    ..Default::default()
                },
                3,
            )
            .unwrap();
        deliver_until_enter(&mut h, "a1");
        h.submitted("a1", &line(&a));
        h.set_status("a1", AgentStatus::Idle);
        h.send(DispatchMsg::TurnEnded {
            agent_id: "a1".into(),
            failed: false,
        });
        h.idle("a1");
        let ra = h.ticket(&a.id);
        assert_eq!(ra.state, S::Review);
        assert_eq!(ra.history.last().unwrap().by, TicketActor::System);
        // The next ticket follows.
        h.advance(DISPATCH_DELAY_MS + ENTER_DELAY_MS);
        let b = h.ticket(&b.id);
        assert_eq!(h.writes()[2], ("a1".into(), line(&b)));
        h.submitted("a1", &line(&b));
        h.send(DispatchMsg::TurnEnded {
            agent_id: "a1".into(),
            failed: false,
        });
        assert_eq!(h.ticket(&b.id).state, S::Done);
    }

    // (6)
    #[test]
    fn stop_failure_keeps_the_ticket_in_progress_with_an_issue() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        let t = h.queued("a1", "A");
        h.queued("a1", "B");
        deliver_until_enter(&mut h, "a1");
        h.submitted("a1", &line(&t));
        h.send(DispatchMsg::TurnEnded {
            agent_id: "a1".into(),
            failed: true,
        });
        h.idle("a1");
        h.advance(10_000);
        let now = h.ticket(&t.id);
        assert_eq!(
            (now.state, now.issue),
            (S::InProgress, Some(TicketIssue::TurnFailed))
        );
        assert_eq!(
            now.history.last().unwrap().note.as_deref(),
            Some(TURN_FAILED_NOTE)
        );
        assert_eq!(h.detail("a1").as_deref(), Some(TURN_FAILED_TEXT));
        // No second ticket while one is in progress.
        assert_eq!(h.writes().len(), 2);
    }

    // (7)
    #[test]
    fn redispatch_of_a_failed_turn_types_again() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        let t = h.queued("a1", "A");
        deliver_until_enter(&mut h, "a1");
        h.submitted("a1", &line(&t));
        h.send(DispatchMsg::TurnEnded {
            agent_id: "a1".into(),
            failed: true,
        });
        // Not while the agent is busy.
        h.set_status("a1", AgentStatus::Thinking);
        h.send(DispatchMsg::Redispatch {
            ticket_id: t.id.clone(),
        });
        assert_eq!(h.writes().len(), 2);
        h.set_status("a1", AgentStatus::Idle);
        h.send(DispatchMsg::Redispatch {
            ticket_id: t.id.clone(),
        });
        assert_eq!(h.writes().len(), 3);
        assert_eq!(h.writes()[2], ("a1".into(), line(&t)));
        assert_eq!(h.ticket(&t.id).issue, None);
        h.advance(ENTER_DELAY_MS);
        assert_eq!(h.writes()[3], enter("a1"));
        h.submitted("a1", &line(&t));
        let now = h.ticket(&t.id);
        assert_eq!((now.state, now.issue), (S::InProgress, None));
        assert_eq!(
            now.history.last().unwrap().note.as_deref(),
            Some("sendt igen")
        );
        assert_eq!(h.detail("a1"), None);
        // A backlog ticket cannot be redispatched.
        let other = h.svc().create("x", "", false, 1).unwrap();
        h.send(DispatchMsg::Redispatch {
            ticket_id: other.id,
        });
        assert_eq!(h.writes().len(), 4);
    }

    // (8)
    #[test]
    fn rejected_ticket_goes_first_and_its_file_has_the_note() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        let a = h.queued("a1", "A");
        let b = h.queued("a1", "B");
        deliver_until_enter(&mut h, "a1");
        h.submitted("a1", &line(&a));
        h.set_status("a1", AgentStatus::Thinking);
        h.send(DispatchMsg::TurnEnded {
            agent_id: "a1".into(),
            failed: false,
        });
        assert_eq!(h.ticket(&a.id).state, S::Review);
        h.svc().reject(&a.id, "Mangler test", true, 10).unwrap();
        assert_eq!(h.ticket(&a.id).queue_position, Some(0));
        assert_eq!(h.ticket(&b.id).queue_position, Some(1));
        h.set_status("a1", AgentStatus::Idle);
        h.send(DispatchMsg::QueueChanged {
            agent_id: "a1".into(),
        });
        h.advance(DISPATCH_DELAY_MS);
        let a_now = h.ticket(&a.id);
        assert_eq!(h.writes().last(), Some(&("a1".into(), line(&a_now))));
        assert!(h
            .ticket_file("a1", &a_now)
            .contains("## Afvist: Mangler test\n"));
    }

    // (9)
    #[test]
    fn agent_gone_mid_delivery_stops_writing() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        let a = h.queued("a1", "A");
        let b = h.queued("a1", "B");
        deliver_until_enter(&mut h, "a1");
        h.svc().release_agent("a1", "agent stoppet", 20).unwrap();
        h.remove_agent("a1");
        h.send(DispatchMsg::AgentGone {
            agent_id: "a1".into(),
        });
        h.advance(CONFIRM_TIMEOUT_MS + RETRY_TIMEOUT_MS + DISPATCH_DELAY_MS);
        assert_eq!(h.writes().len(), 2);
        for id in [&a.id, &b.id] {
            let t = h.ticket(id);
            assert_eq!((t.state, t.issue), (S::Backlog, None));
        }
        assert!(h.d.deliveries.is_empty());
    }

    // (10)
    #[test]
    fn stale_timer_is_ignored() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        let t = h.queued("a1", "A");
        h.send(DispatchMsg::QueueChanged {
            agent_id: "a1".into(),
        });
        let old = match h.d.state("a1") {
            Delivery::Delaying { token } => *token,
            other => panic!("{other:?}"),
        };
        h.send(DispatchMsg::AgentGone {
            agent_id: "a1".into(),
        });
        // New sequence for the same agent.
        h.idle("a1");
        // The old timer arrives first: ignored.
        h.send(DispatchMsg::Timer {
            agent_id: "a1".into(),
            token: old,
            kind: TimerKind::DispatchDelay,
        });
        assert!(h.writes().is_empty());
        h.advance(DISPATCH_DELAY_MS);
        // Exactly one line: the old timer firing in the fake clock did nothing either.
        assert_eq!(h.writes(), vec![("a1".into(), line(&t))]);
        // Unexpected kinds for the current state are ignored too.
        let cur = match h.d.state("a1") {
            Delivery::Typed { token, .. } => *token,
            other => panic!("{other:?}"),
        };
        h.send(DispatchMsg::Timer {
            agent_id: "a1".into(),
            token: cur,
            kind: TimerKind::Confirm,
        });
        assert_eq!(h.writes().len(), 1);
    }

    // (11)
    #[test]
    fn write_failures_send_the_ticket_to_the_backlog() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        let t = h.queued("a1", "A");
        lock(&h.port.0).fail_writes = true;
        h.idle("a1");
        h.advance(DISPATCH_DELAY_MS);
        let now = h.ticket(&t.id);
        assert_eq!(
            (now.state, now.assignee_agent_id.clone()),
            (S::Backlog, None)
        );
        assert_eq!(
            now.history.last().unwrap().note.as_deref(),
            Some(TERMINAL_GONE_NOTE)
        );

        // The ticket file cannot be written (cwd is a file).
        lock(&h.port.0).fail_writes = false;
        h.agent("a2", AgentStatus::Idle);
        let bad = h.root.join("not-a-dir");
        fs::write(&bad, "x").unwrap();
        lock(&h.port.0).snapshots.get_mut("a2").unwrap().cwd = bad;
        let t2 = h.queued("a2", "B");
        h.idle("a2");
        h.advance(DISPATCH_DELAY_MS);
        let now = h.ticket(&t2.id);
        assert_eq!(now.state, S::Backlog);
        assert!(now
            .history
            .last()
            .unwrap()
            .note
            .as_deref()
            .unwrap()
            .starts_with("Kunne ikke skrive ticket-fil: "));
        assert!(h.writes().is_empty());
    }

    // (12)
    #[test]
    fn spawn_with_ticket_waits_for_the_session_then_falls_back() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Starting);
        let t = h.queued("a1", "A");
        h.send(DispatchMsg::SpawnedWithTicket {
            agent_id: "a1".into(),
            ticket_id: t.id.clone(),
        });
        h.set_status("a1", AgentStatus::Idle);
        h.idle("a1"); // SessionStart
        h.advance(SPAWN_CONFIRM_TIMEOUT_MS - 1);
        assert!(h.writes().is_empty());
        h.send(DispatchMsg::AgentBusy {
            agent_id: "a1".into(),
        });
        assert_eq!(h.ticket(&t.id).state, S::InProgress);
        h.advance(20_000);
        assert!(h.writes().is_empty());

        // No confirmation within the grace period: ordinary PTY dispatch.
        h.agent("a2", AgentStatus::Starting);
        let t2 = h.queued("a2", "B");
        h.send(DispatchMsg::SpawnedWithTicket {
            agent_id: "a2".into(),
            ticket_id: t2.id.clone(),
        });
        h.set_status("a2", AgentStatus::Idle);
        h.idle("a2");
        h.idle("a2"); // a second idle does not restart the grace period
        h.advance(SPAWN_CONFIRM_TIMEOUT_MS);
        assert!(h.writes().is_empty());
        h.advance(DISPATCH_DELAY_MS);
        assert_eq!(h.writes(), vec![("a2".into(), line(&t2))]);
    }

    // (13)
    #[test]
    fn messages_for_every_fixture_and_status() {
        let ev = |json: &str| {
            let e = parse(&serde_json::from_str(json).unwrap()).unwrap();
            StatusEvent {
                agent_id: "a".into(),
                status: apply(&e).status,
                hook_event_name: e.hook_event_name,
                prompt: e.prompt,
            }
        };
        let idle = || DispatchMsg::AgentIdle {
            agent_id: "a".into(),
        };
        let busy = || DispatchMsg::AgentBusy {
            agent_id: "a".into(),
        };
        let ended = |failed| DispatchMsg::TurnEnded {
            agent_id: "a".into(),
            failed,
        };
        let table: Vec<(&str, Vec<DispatchMsg>)> = vec![
            (fx::SESSION_START, vec![idle()]),
            (
                fx::USER_PROMPT_SUBMIT,
                vec![DispatchMsg::PromptSubmitted {
                    agent_id: "a".into(),
                    prompt: Some("fix the bug".into()),
                }],
            ),
            (fx::PRE_TOOL_USE, vec![busy()]),
            (fx::PERMISSION_REQUEST, vec![busy()]),
            (fx::PERMISSION_DENIED, vec![busy()]),
            (fx::POST_TOOL_USE, vec![busy()]),
            (fx::POST_TOOL_USE_FAILURE, vec![busy()]),
            (fx::NOTIFICATION_PERMISSION, vec![busy()]),
            (fx::NOTIFICATION_IDLE, vec![idle()]),
            (fx::NOTIFICATION_OTHER, vec![]),
            (fx::STOP, vec![ended(false), idle()]),
            (fx::STOP_FAILURE, vec![ended(true), idle()]),
            (fx::SESSION_END, vec![]),
            (fx::SUBAGENT_STOP, vec![]),
        ];
        assert_eq!(table.len(), 14);
        for (json, want) in table {
            let e = ev(json);
            assert_eq!(messages_for(&e), want, "{}", e.hook_event_name);
        }
        let status = |s: Option<AgentStatus>| StatusEvent {
            agent_id: "a".into(),
            hook_event_name: "X".into(),
            prompt: None,
            status: s,
        };
        for s in [
            AgentStatus::Thinking,
            AgentStatus::Reading,
            AgentStatus::Editing,
            AgentStatus::Running,
            AgentStatus::WaitingPermission,
        ] {
            assert_eq!(messages_for(&status(Some(s))), vec![busy()]);
        }
        for s in [
            None,
            Some(AgentStatus::Starting),
            Some(AgentStatus::Exited { code: Some(0) }),
        ] {
            assert!(messages_for(&status(s)).is_empty());
        }
        assert_eq!(messages_for(&status(Some(AgentStatus::Idle))), vec![idle()]);
    }

    // (14)
    #[test]
    fn two_agents_do_not_affect_each_other() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        h.agent("a2", AgentStatus::Idle);
        let t1 = h.queued("a1", "A");
        let t2 = h.queued("a2", "B");
        h.idle("a1");
        h.advance(400);
        h.idle("a2");
        h.advance(DISPATCH_DELAY_MS); // a1: line at 750, Enter at 900; a2: line at 1150
        h.submitted("a1", &line(&t1));
        assert_eq!(
            h.writes(),
            vec![
                ("a1".into(), line(&t1)),
                enter("a1"),
                ("a2".into(), line(&t2))
            ]
        );
        h.advance(ENTER_DELAY_MS);
        h.advance(CONFIRM_TIMEOUT_MS + RETRY_TIMEOUT_MS);
        assert_eq!(h.ticket(&t1.id).state, S::InProgress);
        assert_eq!(h.ticket(&t1.id).issue, None);
        assert_eq!(h.ticket(&t2.id).issue, Some(TicketIssue::DeliveryFailed));
        assert_eq!(h.detail("a1"), None);
        assert_eq!(h.detail("a2").as_deref(), Some(DELIVERY_FAILED_TEXT));
    }

    #[test]
    fn queue_change_triggers_dispatch_but_not_mid_sequence() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        // Idle with an empty queue: nothing to do.
        h.idle("a1");
        assert_eq!(h.timers.pending(), 0);
        let a = h.queued("a1", "A");
        h.send(DispatchMsg::QueueChanged {
            agent_id: "a1".into(),
        });
        h.advance(DISPATCH_DELAY_MS);
        h.queued("a1", "B");
        h.send(DispatchMsg::QueueChanged {
            agent_id: "a1".into(),
        });
        h.idle("a1");
        h.advance(ENTER_DELAY_MS);
        h.send(DispatchMsg::QueueChanged {
            agent_id: "a1".into(),
        });
        h.advance(CONFIRM_TIMEOUT_MS - 1);
        assert_eq!(h.writes(), vec![("a1".into(), line(&a)), enter("a1")]);
    }

    #[test]
    fn fake_timers_deliver_in_time_then_schedule_order() {
        let mut t = FakeTimers::new();
        let m = |id: &str| DispatchMsg::QueueChanged {
            agent_id: id.into(),
        };
        t.schedule(20, m("b"));
        t.schedule(10, m("a"));
        t.schedule(20, m("c"));
        assert_eq!(t.advance(5), vec![]);
        assert_eq!(t.advance(15), vec![m("a"), m("b"), m("c")]);
        assert_eq!(t.now(), 20);
        assert_eq!(t.pending(), 0);
    }

    #[tokio::test]
    async fn real_timers_and_run_loop() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut timers = RealTimers::new(tx.clone());
        timers.schedule(
            1,
            DispatchMsg::QueueChanged {
                agent_id: "x".into(),
            },
        );
        let got = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap();
        assert_eq!(
            got,
            Some(DispatchMsg::QueueChanged {
                agent_id: "x".into()
            })
        );

        let h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        let t = h.queued("a1", "A");
        let (tx2, rx2) = tokio::sync::mpsc::unbounded_channel();
        let host = TestHost {
            svc: h.svc.clone(),
            mutations: Arc::default(),
        };
        let d = Dispatcher::new(host, h.port.clone(), h.timers.clone());
        tx2.send(DispatchMsg::QueueChanged {
            agent_id: "a1".into(),
        })
        .unwrap();
        drop(tx2);
        run(rx2, d).await;
        assert_eq!(h.timers.pending(), 1);
        assert_eq!(h.ticket(&t.id).state, S::Assigned);
    }
}
