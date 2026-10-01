//! Step 3: tickets, per-agent queues, review and the dispatcher that types tickets into idle
//! agents' terminals.
//!
//! Dependency direction: `tickets` → `agent`, `hooks::status`, `events`, `config` (and the
//! tool-frame types of `pipe::protocol`, step 4). Nothing in `agent`, `pipe` or `hooks` knows
//! `tickets`: the app glue hands the pipe handler a closure over [`tools::ToolsCtx`].
//!
//! This file holds the app glue (plan B.8): [`TicketsCtx`] (shared service + manager links +
//! emits + the dispatcher's channel) and [`ManagerPort`] (the dispatcher's view of the agents).
//!
//! Locks: the service lock and the manager lock are never held at the same time (service first,
//! released, then manager), never across an `.await`, and never while emitting or sending.

pub mod dispatcher;
pub mod model;
pub mod prompt;
pub mod service;
pub mod state;
pub mod store;
pub mod tools;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;

use crate::agent::{now_ms, AgentManager};
use crate::config::{NOT_SUBMITTED_TEXT, TURN_FAILED_TEXT};
use crate::events::{EmitFn, AGENTS_CHANGED, TICKETS_CHANGED};
use dispatcher::{AgentPort, AgentSnapshot, DispatchMsg, TicketsHost};
use model::TicketError;
use service::{TicketLinks, TicketService};
use store::JsonFileStore;

/// History note when an agent's process ended on its own.
pub const AGENT_EXITED_NOTE: &str = "agent afsluttet";
/// History note when the user stopped or removed the agent.
pub const AGENT_STOPPED_NOTE: &str = "agent stoppet";
/// History note when a turn ended without `mira_submit_for_review` (step 4).
pub const NOT_SUBMITTED_NOTE: &str = "turn afsluttet uden aflevering";

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn emit_json<T: Serialize>(emit: &EmitFn, name: &str, payload: &T) {
    match serde_json::to_value(payload) {
        Ok(v) => emit(name, v),
        Err(e) => log::error!("serialize {name}: {e}"),
    }
}

/// Startup: loads `path` (a corrupt or unknown-version file is renamed to `.broken-<ts>` and
/// reported as the warning) and moves queued and in-progress tickets back to the backlog with
/// "app genstartet" — their agents did not survive the restart. Review/done stay as they are.
// TODO(windows-verify): after a restart, queued/in-progress tickets are in the backlog with
// "app genstartet"; done/review are untouched (plan D.35).
pub fn load_tickets(path: PathBuf, now: u64) -> (TicketService, Option<String>) {
    let (service, warning) =
        TicketService::load_and_recover(Box::new(JsonFileStore::new(path.clone())), now);
    log::info!("tickets: {} loaded from {}", service.len(), path.display());
    if let Some(w) = &warning {
        log::warn!("tickets: {w}");
    }
    (service, warning)
}

/// Shared ticket state of the app (in `AppState`, the pipe glue and the dispatcher).
pub struct TicketsCtx {
    pub service: Mutex<TicketService>,
    pub manager: Arc<Mutex<AgentManager>>,
    /// The dispatcher task's inbox.
    pub dispatch_tx: UnboundedSender<DispatchMsg>,
    pub emit: EmitFn,
}

impl TicketsCtx {
    pub fn new(
        service: TicketService,
        manager: Arc<Mutex<AgentManager>>,
        dispatch_tx: UnboundedSender<DispatchMsg>,
        emit: EmitFn,
    ) -> Self {
        TicketsCtx {
            service: Mutex::new(service),
            manager,
            dispatch_tx,
            emit,
        }
    }

    /// Runs `f` under the service lock (which also saves). On success: syncs every agent's
    /// ticket link, emits `tickets-changed` (full list without history) and, if a link changed,
    /// `agents-changed`. On error nothing is emitted. Does NOT notify the dispatcher; callers
    /// use [`Self::notify`] for the agents whose queue they touched.
    pub fn mutate<T>(
        &self,
        f: impl FnOnce(&mut TicketService) -> Result<T, TicketError>,
    ) -> Result<T, String> {
        self.mutate_if(f, |_| true)
    }

    /// `mutate`, but emits only when `changed(&result)` (a no-op mutation stays silent).
    fn mutate_if<T>(
        &self,
        f: impl FnOnce(&mut TicketService) -> Result<T, TicketError>,
        changed: impl FnOnce(&T) -> bool,
    ) -> Result<T, String> {
        let (result, list, links) = {
            let mut svc = lock(&self.service);
            let result = f(&mut svc).map_err(String::from)?;
            if !changed(&result) {
                return Ok(result);
            }
            (result, svc.list(), svc.links())
        };
        let agents_changed = self.apply_links(&links);
        emit_json(&self.emit, TICKETS_CHANGED, &list);
        if agents_changed {
            self.emit_agents();
        }
        Ok(result)
    }

    /// Read-only access under the service lock.
    pub fn read<T>(&self, f: impl FnOnce(&TicketService) -> T) -> T {
        f(&lock(&self.service))
    }

    /// Writes the links into the manager (agents without tickets get `None`/0). Returns whether
    /// any agent changed. Takes only the manager lock.
    fn apply_links(&self, links: &TicketLinks) -> bool {
        let mut m = lock(&self.manager);
        let mut changed = false;
        for id in m.ids() {
            let (current, len) = links.get(&id).cloned().unwrap_or_default();
            changed |= m.set_ticket_link(&id, current, len);
        }
        changed
    }

    /// Re-syncs the agents' ticket links from the service (e.g. after a new agent appeared) and
    /// emits `agents-changed` if anything changed. Returns whether it did.
    pub fn sync_links(&self) -> bool {
        let links = self.read(TicketService::links);
        let changed = self.apply_links(&links);
        if changed {
            self.emit_agents();
        }
        changed
    }

    fn emit_agents(&self) {
        let list = lock(&self.manager).list();
        emit_json(&self.emit, AGENTS_CHANGED, &list);
    }

    /// Sets the agent's detail (no-op for the same text) and emits `agents-changed` after the
    /// manager lock is released. `only_if`: change only while the current detail passes.
    pub fn set_agent_detail(
        &self,
        agent_id: &str,
        detail: Option<String>,
        only_if: impl FnOnce(Option<&str>) -> bool,
    ) {
        let list = {
            let mut m = lock(&self.manager);
            let current = m.get(agent_id).and_then(|a| a.detail);
            if current == detail || !only_if(current.as_deref()) {
                return;
            }
            if !m.set_detail(agent_id, detail) {
                return;
            }
            m.list()
        };
        emit_json(&self.emit, AGENTS_CHANGED, &list);
    }

    /// Clears the agent's detail when it is the "turn ended without submitting" or "turn failed"
    /// hint ([`NOT_SUBMITTED_TEXT`]/[`TURN_FAILED_TEXT`]): used when its ticket leaves in-progress
    /// (submit, manual move), so the hint does not outlive the ticket. Other texts stay.
    pub fn clear_stale_detail(&self, agent_id: &str) {
        self.set_agent_detail(
            agent_id,
            None,
            |d| matches!(d, Some(d) if d == NOT_SUBMITTED_TEXT || d == TURN_FAILED_TEXT),
        );
    }

    /// Tells the dispatcher that these agents' queues changed (each id once).
    pub fn notify<I, S>(&self, agent_ids: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let unique: BTreeSet<String> = agent_ids.into_iter().map(Into::into).collect();
        for agent_id in unique {
            self.send(DispatchMsg::QueueChanged { agent_id });
        }
    }

    /// Sends a message to the dispatcher. Fails only after the dispatcher task is gone
    /// (shutdown); that is logged, not returned.
    pub fn send(&self, msg: DispatchMsg) -> bool {
        match self.dispatch_tx.send(msg) {
            Ok(()) => true,
            // Not the message itself: a PromptSubmitted carries prompt text, which is never logged.
            Err(_) => {
                log::warn!("dispatcher is not running; message dropped");
                false
            }
        }
    }

    /// The agent stopped, exited or was removed: its assigned/inProgress/rejected tickets go to
    /// the backlog with `note`, and any delivery sequence for it is cancelled (`AgentGone`).
    // TODO(windows-verify): after Stop/Remove of an agent with a queue, all its tickets are in the
    // backlog with the note, no more input reaches its terminal and `queueLength` is 0 (plan D.37).
    pub fn release_agent(&self, agent_id: &str, note: &str) -> Result<usize, String> {
        let now = now_ms();
        let released = self.mutate_if(|s| s.release_agent(agent_id, note, now), |v| !v.is_empty());
        self.send(DispatchMsg::AgentGone {
            agent_id: agent_id.to_string(),
        });
        let released = released?;
        if !released.is_empty() {
            log::info!(
                "agent {agent_id}: {} ticket(s) back to the backlog ({note})",
                released.len()
            );
        }
        Ok(released.len())
    }
}

impl TicketsHost for Arc<TicketsCtx> {
    fn mutate<T>(
        &self,
        f: impl FnOnce(&mut TicketService) -> Result<T, TicketError>,
    ) -> Result<T, String> {
        TicketsCtx::mutate(self, f)
    }

    fn read<T>(&self, f: impl FnOnce(&TicketService) -> T) -> T {
        TicketsCtx::read(self, f)
    }
}

/// The dispatcher's [`AgentPort`] over the real manager. Each call takes the manager lock
/// briefly; a detail change is announced with `agents-changed` (after the lock is released).
#[derive(Clone)]
pub struct ManagerPort {
    pub manager: Arc<Mutex<AgentManager>>,
    pub emit: EmitFn,
}

impl ManagerPort {
    pub fn new(manager: Arc<Mutex<AgentManager>>, emit: EmitFn) -> Self {
        ManagerPort { manager, emit }
    }
}

impl AgentPort for ManagerPort {
    fn snapshot(&self, id: &str) -> Option<AgentSnapshot> {
        let (a, last_user_input_at) = {
            let m = lock(&self.manager);
            (m.get(id)?, m.last_user_input_at(id))
        };
        Some(AgentSnapshot {
            name: a.name,
            cwd: a.cwd.into(),
            status: a.status,
            detail: a.detail,
            last_user_input_at,
        })
    }

    fn write_input(&self, id: &str, bytes: &[u8]) -> Result<(), String> {
        lock(&self.manager)
            .write_input(id, bytes)
            .map_err(|e| e.to_string())
    }

    fn set_detail(&self, id: &str, detail: Option<String>) -> bool {
        let list = {
            let mut m = lock(&self.manager);
            if !m.set_detail(id, detail) {
                return false;
            }
            m.list()
        };
        emit_json(&self.emit, AGENTS_CHANGED, &list);
        true
    }
}

/// Shared test fixture: a `TicketsCtx` over a `MemoryStore`, a collecting emit and the
/// dispatcher's receiving end.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::tickets::model::TicketDoc;
    use crate::tickets::store::MemoryStore;
    use serde_json::Value;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

    pub type Events = Arc<Mutex<Vec<(String, Value)>>>;

    pub struct TestCtx {
        pub ctx: Arc<TicketsCtx>,
        pub rx: UnboundedReceiver<DispatchMsg>,
        pub events: Events,
        pub store: MemoryStore,
    }

    pub fn test_ctx(manager: Arc<Mutex<AgentManager>>) -> TestCtx {
        let store = MemoryStore::new();
        let svc = TicketService::new(Box::new(store.clone()), TicketDoc::default());
        let (tx, rx) = unbounded_channel();
        let events: Events = Arc::default();
        let sink = Arc::clone(&events);
        let emit: EmitFn = Arc::new(move |n: &str, v: Value| {
            sink.lock().unwrap().push((n.to_string(), v));
        });
        TestCtx {
            ctx: Arc::new(TicketsCtx::new(svc, manager, tx, emit)),
            rx,
            events,
            store,
        }
    }

    impl TestCtx {
        pub fn emitted(&self, name: &str) -> Vec<Value> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
                .collect()
        }

        pub fn clear(&self) {
            self.events.lock().unwrap().clear();
        }

        pub fn sent(&mut self) -> Vec<DispatchMsg> {
            let mut v = Vec::new();
            while let Ok(m) = self.rx.try_recv() {
                v.push(m);
            }
            v
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::hooks::status::AgentStatus;
    use crate::tickets::model::{TicketActor, TicketState};
    use serde_json::{json, Value};
    use std::sync::OnceLock;

    fn manager_with(n: usize) -> (Arc<Mutex<AgentManager>>, Vec<String>) {
        let mut m = AgentManager::new(5);
        let ids = (0..n)
            .map(|i| m.insert_fake(&format!("s{i}"), &format!("/w/a{i}")))
            .collect();
        (Arc::new(Mutex::new(m)), ids)
    }

    fn agent_json<'a>(list: &'a Value, id: &str) -> &'a Value {
        list.as_array()
            .unwrap()
            .iter()
            .find(|a| a["id"] == id)
            .unwrap()
    }

    #[test]
    fn mutate_syncs_links_and_emits_only_when_needed() {
        let (m, ids) = manager_with(2);
        let mut t = test_ctx(Arc::clone(&m));
        let a = ids[0].clone();

        let tk = t.ctx.mutate(|s| s.create("Fix", "", false, 1)).unwrap();
        let lists = t.emitted(TICKETS_CHANGED);
        assert_eq!(lists.len(), 1);
        assert_eq!(lists[0][0]["title"], "Fix");
        assert!(lists[0][0].get("history").is_none(), "summaries only");
        assert_eq!(lists[0][0]["historyLen"], 1);
        assert!(t.emitted(AGENTS_CHANGED).is_empty(), "links unchanged");
        t.clear();

        t.ctx.mutate(|s| s.assign(&tk.id, &a, 2)).unwrap();
        assert_eq!(t.emitted(TICKETS_CHANGED).len(), 1);
        let agents = t.emitted(AGENTS_CHANGED);
        assert_eq!(agents.len(), 1);
        assert_eq!(agent_json(&agents[0], &a)["queueLength"], 1);
        assert_eq!(agent_json(&agents[0], &ids[1])["queueLength"], 0);
        assert_eq!(lock(&m).get(&a).unwrap().queue_length, 1);
        t.clear();

        t.ctx
            .mutate(|s| s.mark_dispatched(&tk.id, "a0", 3))
            .unwrap();
        let agents = t.emitted(AGENTS_CHANGED);
        assert_eq!(agent_json(&agents[0], &a)["currentTicketId"], json!(tk.id));
        assert_eq!(agent_json(&agents[0], &a)["queueLength"], 0);
        t.clear();

        // An error emits nothing and changes nothing.
        let err = t.ctx.mutate(|s| s.approve(&tk.id, 4)).unwrap_err();
        assert!(err.starts_with("Kan ikke flytte en ticket fra"), "{err}");
        assert!(t.events.lock().unwrap().is_empty());
        // mutate never talks to the dispatcher by itself.
        assert!(t.sent().is_empty());
        assert_eq!(t.store.saves(), 3);
    }

    #[test]
    fn emits_happen_without_any_lock_held() {
        let (m, ids) = manager_with(1);
        lock(&m)
            .set_status(&ids[0], AgentStatus::Idle, None)
            .unwrap();
        let slot: Arc<OnceLock<Arc<TicketsCtx>>> = Arc::default();
        let checks = Arc::new(Mutex::new(0usize));
        let (inner, n) = (Arc::clone(&slot), Arc::clone(&checks));
        let emit: EmitFn = Arc::new(move |_: &str, _: Value| {
            let ctx = inner.get().unwrap();
            assert!(
                ctx.service.try_lock().is_ok(),
                "service lock held while emitting"
            );
            assert!(
                ctx.manager.try_lock().is_ok(),
                "manager lock held while emitting"
            );
            *n.lock().unwrap() += 1;
        });
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let svc = TicketService::new(
            Box::new(store::MemoryStore::new()),
            model::TicketDoc::default(),
        );
        let ctx = Arc::new(TicketsCtx::new(svc, Arc::clone(&m), tx, Arc::clone(&emit)));
        assert!(slot.set(Arc::clone(&ctx)).is_ok());
        let tk = ctx.mutate(|s| s.create("x", "", false, 1)).unwrap();
        ctx.mutate(|s| s.assign(&tk.id, &ids[0], 2)).unwrap();
        assert!(ManagerPort::new(Arc::clone(&m), emit).set_detail(&ids[0], None));
        // tickets; tickets + agents; agents (detail).
        assert_eq!(*checks.lock().unwrap(), 4);
    }

    #[test]
    fn release_agent_moves_tickets_to_backlog_and_cancels_delivery() {
        let (m, ids) = manager_with(2);
        let mut t = test_ctx(Arc::clone(&m));
        let a = ids[0].clone();
        let ids_t: Vec<String> = (0..3)
            .map(|i| {
                let tk = t
                    .ctx
                    .mutate(|s| s.create(&format!("t{i}"), "", false, 1))
                    .unwrap();
                t.ctx.mutate(|s| s.assign(&tk.id, &a, 2)).unwrap();
                tk.id
            })
            .collect();
        t.ctx
            .mutate(|s| s.mark_dispatched(&ids_t[0], "a0", 3))
            .unwrap();
        assert_eq!(
            lock(&m).get(&a).unwrap().current_ticket_id.as_deref(),
            Some(ids_t[0].as_str())
        );
        t.clear();

        assert_eq!(t.ctx.release_agent(&a, AGENT_STOPPED_NOTE), Ok(3));
        let info = lock(&m).get(&a).unwrap();
        assert_eq!((info.current_ticket_id, info.queue_length), (None, 0));
        for id in &ids_t {
            let tk = t.ctx.read(|s| s.get(id)).unwrap();
            assert_eq!(tk.state, TicketState::Backlog);
            assert_eq!(tk.assignee_agent_id, None);
            let last = tk.history.last().unwrap();
            assert_eq!(last.by, TicketActor::System);
            assert_eq!(last.note.as_deref(), Some(AGENT_STOPPED_NOTE));
        }
        assert_eq!(t.emitted(TICKETS_CHANGED).len(), 1);
        assert_eq!(t.emitted(AGENTS_CHANGED).len(), 1);
        assert_eq!(
            t.sent(),
            vec![DispatchMsg::AgentGone {
                agent_id: a.clone()
            }]
        );
        t.clear();

        // Nothing to release: no emit, but the delivery is still cancelled.
        assert_eq!(t.ctx.release_agent(&a, AGENT_EXITED_NOTE), Ok(0));
        assert!(t.events.lock().unwrap().is_empty());
        assert_eq!(t.sent().len(), 1);
    }

    #[test]
    fn notify_sends_one_queue_changed_per_agent() {
        let (m, _) = manager_with(0);
        let mut t = test_ctx(m);
        t.ctx.notify(["b", "a", "b"]);
        t.ctx.notify(None::<String>);
        assert_eq!(
            t.sent(),
            vec![
                DispatchMsg::QueueChanged {
                    agent_id: "a".into()
                },
                DispatchMsg::QueueChanged {
                    agent_id: "b".into()
                },
            ]
        );
    }

    #[test]
    fn sync_links_picks_up_new_agents() {
        let (m, ids) = manager_with(1);
        let t = test_ctx(Arc::clone(&m));
        let tk = t.ctx.mutate(|s| s.create("x", "", false, 1)).unwrap();
        t.ctx.mutate(|s| s.assign(&tk.id, &ids[0], 2)).unwrap();
        // Simulate a manager that lost the link (e.g. set from elsewhere).
        lock(&m).set_ticket_link(&ids[0], None, 0);
        t.clear();
        assert!(t.ctx.sync_links());
        assert_eq!(lock(&m).get(&ids[0]).unwrap().queue_length, 1);
        assert_eq!(t.emitted(AGENTS_CHANGED).len(), 1);
        assert!(!t.ctx.sync_links());
    }

    #[test]
    fn load_tickets_recovers_queues_after_a_restart() {
        use crate::tickets::model::TicketDoc;
        let dir = std::env::temp_dir().join(format!("mira-load-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(crate::config::TICKETS_FILE);
        // Build a document through a service, then save it as the app would have.
        let mem = store::MemoryStore::new();
        let mut svc = TicketService::new(Box::new(mem.clone()), TicketDoc::default());
        let mut make = |title: &str| svc.create(title, "", false, 1).unwrap().id;
        let (queued, running, review, done, backlog) =
            (make("q"), make("r"), make("rv"), make("d"), make("b"));
        svc.assign(&queued, "gone", 2).unwrap();
        svc.assign(&running, "gone", 2).unwrap();
        svc.mark_dispatched(&running, "gone", 3).unwrap();
        svc.assign(&review, "gone-2", 2).unwrap();
        svc.mark_dispatched(&review, "gone-2", 3).unwrap();
        svc.complete_turn("gone-2", 4).unwrap();
        svc.assign(&done, "gone-3", 2).unwrap();
        svc.mark_dispatched(&done, "gone-3", 3).unwrap();
        svc.complete_turn("gone-3", 4).unwrap();
        svc.approve(&done, 5).unwrap();
        std::fs::write(&path, serde_json::to_vec(&mem.doc().unwrap()).unwrap()).unwrap();

        let (loaded, warning) = load_tickets(path.clone(), 10);
        assert_eq!(warning, None);
        let state = |id: &str| loaded.get(id).unwrap();
        for id in [&queued, &running] {
            let t = state(id);
            assert_eq!(t.state, TicketState::Backlog);
            assert_eq!(t.assignee_agent_id, None);
            assert_eq!(
                t.history.last().unwrap().note.as_deref(),
                Some(crate::config::RESTART_NOTE)
            );
        }
        assert_eq!(state(&review).state, TicketState::Review);
        assert_eq!(state(&review).assignee_agent_id.as_deref(), Some("gone-2"));
        assert_eq!(state(&done).state, TicketState::Done);
        assert_eq!(state(&backlog).history.len(), 1);
        // The recovery was saved: a second start finds nothing to do.
        let on_disk: TicketDoc = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(on_disk
            .tickets
            .iter()
            .all(|t| !matches!(t.state, TicketState::Assigned | TicketState::InProgress)));
        assert!(links_empty(&loaded));

        // A corrupt file starts empty with a warning.
        std::fs::write(&path, "{nope").unwrap();
        let (empty, warning) = load_tickets(path, 11);
        assert!(empty.is_empty());
        assert!(warning.unwrap().contains("broken-"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn links_empty(s: &TicketService) -> bool {
        s.links().values().all(|(c, n)| c.is_none() && *n == 0)
    }

    #[test]
    fn manager_port_snapshot_and_detail_on_fake_agents() {
        let (m, ids) = manager_with(1);
        let events: Events = Arc::default();
        let sink = Arc::clone(&events);
        let port = ManagerPort::new(
            Arc::clone(&m),
            Arc::new(move |n: &str, v: Value| sink.lock().unwrap().push((n.into(), v))),
        );
        let a = &ids[0];
        lock(&m).set_status(a, AgentStatus::Idle, None).unwrap();
        let snap = port.snapshot(a).unwrap();
        assert_eq!(snap.name, "a0");
        assert_eq!(snap.cwd, PathBuf::from("/w/a0"));
        assert_eq!(snap.status, AgentStatus::Idle);
        assert_eq!(snap.detail, None);
        assert!(port.snapshot("nope").is_none());

        assert!(port.set_detail(a, Some("hint".into())));
        assert_eq!(port.snapshot(a).unwrap().detail.as_deref(), Some("hint"));
        {
            let ev = events.lock().unwrap();
            assert_eq!(ev.len(), 1);
            assert_eq!(ev[0].0, AGENTS_CHANGED);
            assert_eq!(ev[0].1[0]["detail"], "hint");
        }
        // A fake agent has no terminal.
        assert_eq!(
            port.write_input(a, b"x").unwrap_err(),
            "Agenten findes ikke"
        );
        lock(&m).stop(a).unwrap();
        assert!(!port.set_detail(a, Some("x".into())));
        assert!(!port.set_detail("nope", None));
        assert_eq!(
            events.lock().unwrap().len(),
            1,
            "no emit for refused details"
        );
    }

    /// The real manager with a PTY child (`sh`) behind the port, driven by the real dispatcher
    /// with fake timers: the ticket line and the separate Enter reach the terminal, and a
    /// matching UserPromptSubmit puts the ticket in progress and onto the agent.
    #[cfg(unix)]
    #[test]
    fn dispatcher_types_into_a_real_pty_through_the_manager_port() {
        use crate::agent::manager::AgentMeta;
        use crate::agent::pty::SpawnSpec;
        use crate::agent::{AgentRole, EventSink, SeatKind, SinkEvent};
        use crate::config::{PTY_COLS, PTY_ROWS};
        use crate::tickets::dispatcher::{Dispatcher, FakeTimers};
        use std::time::{Duration, Instant};

        let cwd = std::env::temp_dir().join(format!("mira-port-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&cwd).unwrap();
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let o = Arc::clone(&out);
        let sink: EventSink = Arc::new(move |ev| {
            if let SinkEvent::Output { bytes, .. } = ev {
                o.lock().unwrap().extend_from_slice(&bytes);
            }
        });
        let m = Arc::new(Mutex::new(AgentManager::new(5)));
        let info = lock(&m)
            .spawn_spec(
                SpawnSpec {
                    program: PathBuf::from("/bin/sh"),
                    args: vec!["-c".into(), "read line; echo got:$line; sleep 30".into()],
                    cwd: cwd.clone(),
                    env: vec![],
                    cols: PTY_COLS,
                    rows: PTY_ROWS,
                },
                AgentMeta {
                    id: uuid::Uuid::new_v4().to_string(),
                    session_id: "s".into(),
                    role: AgentRole::None,
                    seat_kind: SeatKind::Work,
                },
                sink,
            )
            .unwrap();
        let a = info.id.clone();
        lock(&m).set_status(&a, AgentStatus::Idle, None).unwrap();

        let mut t = test_ctx(Arc::clone(&m));
        let port = ManagerPort::new(Arc::clone(&m), Arc::clone(&t.ctx.emit));
        let timers = FakeTimers::new();
        let mut d = Dispatcher::new(Arc::clone(&t.ctx), port, timers.clone());
        let tk = t
            .ctx
            .mutate(|s| s.create("Ret fejlen", "", false, 1))
            .unwrap();
        t.ctx.mutate(|s| s.assign(&tk.id, &a, 2)).unwrap();
        d.handle(DispatchMsg::QueueChanged {
            agent_id: a.clone(),
        });
        // The delay, then (scheduled while handling it) the separate Enter.
        for step in [750, 150] {
            for msg in timers.advance(step) {
                d.handle(msg);
            }
        }
        let short = tk.short_id();
        let wanted = format!("got:Ticket {short}: Ret fejlen.");
        let start = Instant::now();
        while !String::from_utf8_lossy(&out.lock().unwrap()).contains(&wanted) {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "terminal output: {:?}",
                String::from_utf8_lossy(&out.lock().unwrap())
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(cwd
            .join(".mira-bots")
            .join("tickets")
            .join(format!("{short}.md"))
            .is_file());

        t.clear();
        d.handle(DispatchMsg::PromptSubmitted {
            agent_id: a.clone(),
            prompt: Some(format!("Ticket {short}: Ret fejlen. Læs filen …")),
        });
        assert_eq!(
            t.ctx.read(|s| s.get(&tk.id)).unwrap().state,
            TicketState::InProgress
        );
        let agents = t.emitted(AGENTS_CHANGED);
        assert_eq!(agent_json(&agents[0], &a)["currentTicketId"], json!(tk.id));

        // Stop: the tickets go back and the dispatcher forgets the agent.
        lock(&m).stop(&a).unwrap();
        t.ctx.release_agent(&a, AGENT_STOPPED_NOTE).unwrap();
        for msg in t.sent() {
            d.handle(msg);
        }
        assert_eq!(
            t.ctx.read(|s| s.get(&tk.id)).unwrap().state,
            TicketState::Backlog
        );
        let pty = lock(&m).remove(&a).unwrap();
        drop(pty);
        let _ = std::fs::remove_dir_all(&cwd);
    }
}
