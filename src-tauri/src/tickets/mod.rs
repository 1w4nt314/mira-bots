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
pub mod playbook;
pub mod prompt;
pub mod reports;
pub mod service;
pub mod state;
pub mod store;
pub mod tools;

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

use serde::Serialize;
use tokio::sync::mpsc::UnboundedSender;

use crate::agent::roles::Role;
use crate::agent::{now_ms, AgentInfo, AgentManager};
use crate::checks::{self, CheckRunner, ChecksReport, ProcessChecks, ProjectFileReader};
use crate::config::{
    worktree_created_note, CHANGES_REPORT_TITLE, NOT_SUBMITTED_TEXT, REPORT_BODY_MAX_CHARS,
    REPORT_TITLE_MAX_CHARS, TURN_FAILED_TEXT,
};
use crate::events::{EmitFn, AGENTS_CHANGED, TICKETS_CHANGED};
use crate::git::{self, GitRunner};
use crate::hooks::status::AgentStatus;
use crate::inbox::{InboxError, InboxService};
use crate::notices::{derive_ticket_notices, notice_for_exit, NoticesCtx};
use crate::workspace::WorkspaceReader;
use dispatcher::{
    AgentPort, AgentSnapshot, DispatchMsg, RestartForTicket, RestartPort, TicketsHost,
};
use model::{
    ChecksState, GitMode, ReportAuthor, Ticket, TicketActor, TicketChecks, TicketError, TicketGit,
    TicketId, TicketReport, TicketState, TicketSummary, WorkspaceRules,
};
use prompt::{clean_body, one_line};
use reports::ReportStore;
pub use service::RejectReturn;
use service::{
    checkable, needs_changes_report, relation_effects, RelationEffects, ReviewCounts, TicketLinks,
    TicketService, REVIEWER_REMOVED_NOTE,
};
use store::JsonFileStore;

/// History note when an agent's process ended on its own.
pub const AGENT_EXITED_NOTE: &str = "agent afsluttet";
/// History note when the user stopped or removed the agent.
pub const AGENT_STOPPED_NOTE: &str = "agent stoppet";
/// History note when a turn ended without `mira_submit_for_review` (step 4).
pub const NOT_SUBMITTED_NOTE: &str = "turn afsluttet uden aflevering";
/// `assign_reviewer` on a ticket in review without an assignee (a finished flow parent, step 6b).
pub const FLOW_REVIEW_IS_USERS: &str =
    "Et afsluttet forløb uden ejer reviewes af dig: godkend eller afvis det selv";
/// History note (by the app) when the checks thread panicked (review6b N20): the run is
/// `skipped`, so the ticket never stays `pending`.
pub const CHECKS_ABORTED_NOTE: &str = "tjek afbrudt af en intern fejl";
/// History note (by the app) when the «Ændringer» report of a review entry failed (review6b
/// W4); the error on one line, clipped to 200 chars.
pub fn changes_failed_note(err: &str) -> String {
    let e: String = one_line(err).chars().take(200).collect();
    format!("Ændringer kunne ikke læses: {e}")
}
/// `assign_reviewer` with a reviewer while the project checks run and `checksGate` is on
/// (review6b W7): a failing check would move the ticket away from that reviewer. The card's
/// reviewer menu shows the same text (`src/lib/tickets.ts` `CHECKS_RUNNING_REVIEWER`).
pub const CHECKS_RUNNING_REVIEWER: &str = "Tjek kører; vælg reviewer når det er færdigt";

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

/// A report's title and body checked and cleaned for [`TicketsCtx::add_report`]: title one line,
/// 1–[`REPORT_TITLE_MAX_CHARS`]; body without controls, trimmed, 1–[`REPORT_BODY_MAX_CHARS`].
pub fn validate_report(title: &str, body: &str) -> Result<(String, String), String> {
    let title = one_line(title);
    if title.is_empty() {
        return Err("Titel må ikke være tom".into());
    }
    if title.chars().count() > REPORT_TITLE_MAX_CHARS {
        return Err(format!(
            "Titlen er for lang (maks {REPORT_TITLE_MAX_CHARS} tegn)"
        ));
    }
    let body = clean_body(body).trim().to_string();
    if body.is_empty() {
        return Err("Rapporten må ikke være tom".into());
    }
    if body.chars().count() > REPORT_BODY_MAX_CHARS {
        return Err(format!(
            "Rapporten er for lang (maks {REPORT_BODY_MAX_CHARS} tegn)"
        ));
    }
    Ok((title, body))
}

/// `get_report` result (C5.4 `ReportContent`).
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct ReportContent {
    pub report: TicketReport,
    pub body: String,
}

/// The held inbox serial lock ([`TicketsCtx::lock_inbox_serial`]). In tests it also marks the
/// thread, so file system reads can check that they never run under it (review6c W3).
pub struct InboxSerialGuard<'a> {
    _guard: MutexGuard<'a, ()>,
}

#[cfg(test)]
thread_local! {
    static INBOX_SERIAL_HELD: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

impl Drop for InboxSerialGuard<'_> {
    fn drop(&mut self) {
        #[cfg(test)]
        INBOX_SERIAL_HELD.with(|h| h.set(h.get().saturating_sub(1)));
    }
}

/// Plan A.1/F (review6c W3): `gh` and file system work never run under `inbox_lock`. Tests
/// panic when `what` is read while this thread holds it; release builds do nothing.
pub fn assert_not_under_inbox_lock(what: &str) {
    #[cfg(test)]
    INBOX_SERIAL_HELD.with(|h| assert_eq!(h.get(), 0, "{what} read under inbox_lock"));
    #[cfg(not(test))]
    let _ = what;
}

/// Shared ticket state of the app (in `AppState`, the pipe glue and the dispatcher).
pub struct TicketsCtx {
    pub service: Mutex<TicketService>,
    pub manager: Arc<Mutex<AgentManager>>,
    /// The dispatcher task's inbox.
    pub dispatch_tx: UnboundedSender<DispatchMsg>,
    pub emit: EmitFn,
    /// Report files under `<app_data>/tickets`.
    pub reports: ReportStore,
    /// Serialises report writes (sequence number → file → metadata). Taken before, never
    /// inside, the service lock.
    report_lock: Mutex<()>,
    /// The workspace file reader (plan4b A.4), shared with `AppState`.
    pub workspace: Arc<WorkspaceReader>,
    /// git for the ticket worktrees and the «Ændringer» report (step 6b; `SystemGit` in the app).
    pub git: Arc<dyn GitRunner>,
    /// Serialises git preparation and the «Ændringer» report (check + git + save), so two paths
    /// never prepare the same worktree or add the report twice. Taken before, never inside, the
    /// service and report locks; git runs under it but never under the service lock.
    git_lock: Mutex<()>,
    /// Review entries `(ticket id, review_entry_at)` whose «Ændringer» report failed (review6b
    /// W4): not tried again for that entry, so a broken worktree or a hanging git does not run
    /// (and hold `git_lock`) on every `route_reviews`. In memory only; taken briefly, never
    /// around another lock.
    changes_failed: Mutex<HashSet<(String, u64)>>,
    /// Runs the project checks (step 6b; [`ProcessChecks`] in the app, a fake in tests).
    pub checks: Arc<dyn CheckRunner>,
    /// `<project>/.mira-bots/project.json` with an `(mtime, len)` cache.
    pub project_files: ProjectFileReader,
    /// This context's own `Arc` (set by [`Self::shared`]): the checks thread holds it.
    me: OnceLock<Weak<TicketsCtx>>,
    /// The checks threads started so far (tests join them).
    #[cfg(test)]
    check_threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
    /// The inbox document (step 6c, plan A.1). Held only around one [`InboxService`] call
    /// ([`Self::inbox_read`]/[`Self::inbox_mutate`]), never together with the service lock.
    inbox: Mutex<InboxService>,
    /// Serialises Start/refresh/write-back steps that touch both documents (step 6c). Taken
    /// before, never inside, the service lock; `gh` and file moves never run under it.
    inbox_lock: Mutex<()>,
    /// Refresh state (single flight, status per source; step 6c).
    pub inbox_rt: crate::inbox::InboxRuntime,
    /// The inbox threads started so far (`mira-inbox`, `mira-writeback`; tests join them).
    #[cfg(test)]
    inbox_threads: Mutex<Vec<std::thread::JoinHandle<()>>>,
    /// The GitHub CLI (step 6c B3; `SystemGh` in the app, `FakeGh` in tests). Never run under
    /// any lock of this context.
    pub gh: Arc<dyn crate::gh::GhRunner>,
    /// The app data folder: `tmp/` holds the `--body-file`s of the GitHub write back.
    pub data_dir: PathBuf,
    /// Serialises the GitHub write backs of this process and remembers the last write call
    /// (≥ 1 s apart, research §5.4). Its own lock: no other lock is held under it, and `gh` runs
    /// under it on purpose.
    pub(crate) write_back_lock: Mutex<Option<std::time::Instant>>,
    /// Beskedkøen (trin 6d, plan A.8; `AppState.notices` er samme `Arc`). Fodres af
    /// [`Self::mutate_if`] og [`Self::notice_exit`]; dens lås tages aldrig sammen med en anden.
    pub notices: Arc<NoticesCtx>,
}

impl TicketsCtx {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        service: TicketService,
        manager: Arc<Mutex<AgentManager>>,
        dispatch_tx: UnboundedSender<DispatchMsg>,
        emit: EmitFn,
        reports_root: PathBuf,
        workspace: Arc<WorkspaceReader>,
        git: Arc<dyn GitRunner>,
        inbox: InboxService,
    ) -> Self {
        // The reports live in `<app_data>/tickets`: their parent is the app data folder.
        let data_dir = reports_root
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| reports_root.clone());
        TicketsCtx {
            service: Mutex::new(service),
            manager,
            dispatch_tx,
            notices: Arc::new(NoticesCtx::new(Arc::clone(&emit))),
            emit,
            reports: ReportStore::new(reports_root),
            report_lock: Mutex::new(()),
            workspace,
            git,
            git_lock: Mutex::new(()),
            changes_failed: Mutex::new(HashSet::new()),
            checks: Arc::new(ProcessChecks::new()),
            project_files: ProjectFileReader::new(),
            me: OnceLock::new(),
            #[cfg(test)]
            check_threads: Mutex::new(Vec::new()),
            inbox: Mutex::new(inbox),
            inbox_lock: Mutex::new(()),
            inbox_rt: crate::inbox::InboxRuntime::new(),
            #[cfg(test)]
            inbox_threads: Mutex::new(Vec::new()),
            gh: Arc::new(crate::gh::SystemGh::new(data_dir.clone())),
            data_dir,
            write_back_lock: Mutex::new(None),
        }
    }

    /// Replaces the GitHub CLI runner (tests).
    #[cfg(test)]
    pub fn with_gh(mut self, gh: Arc<dyn crate::gh::GhRunner>) -> Self {
        self.gh = gh;
        self
    }

    /// Replaces the app data folder (tests).
    #[cfg(test)]
    pub fn with_data_dir(mut self, dir: PathBuf) -> Self {
        self.data_dir = dir;
        self
    }

    /// Takes the inbox serial lock (step 6c): before, never inside, the service lock. No file
    /// system read runs under it ([`assert_not_under_inbox_lock`], review6c W3).
    pub fn lock_inbox_serial(&self) -> InboxSerialGuard<'_> {
        let guard = lock(&self.inbox_lock);
        #[cfg(test)]
        INBOX_SERIAL_HELD.with(|h| h.set(h.get() + 1));
        InboxSerialGuard { _guard: guard }
    }

    /// Read-only access to the inbox document (its own short lock).
    pub fn inbox_read<T>(&self, f: impl FnOnce(&InboxService) -> T) -> T {
        f(&lock(&self.inbox))
    }

    /// Runs `f` under the inbox document's lock (the service saves); on success emits
    /// `inbox-changed` (list + status) after the lock is released. Never called with the service
    /// lock held (the payload reads the tickets).
    pub fn inbox_mutate<T>(
        &self,
        f: impl FnOnce(&mut InboxService) -> Result<T, InboxError>,
    ) -> Result<T, String> {
        let r = self.inbox_mutate_quiet(f)?;
        self.emit_inbox();
        Ok(r)
    }

    /// [`Self::inbox_mutate`] without the emit (the refresh emits once at its end).
    pub fn inbox_mutate_quiet<T>(
        &self,
        f: impl FnOnce(&mut InboxService) -> Result<T, InboxError>,
    ) -> Result<T, String> {
        f(&mut lock(&self.inbox)).map_err(String::from)
    }

    /// Runs `f` with this context's `Arc` on a thread named `name` (step 6c: `mira-inbox`,
    /// `mira-writeback`). `false` when the context is not [`Self::shared`] or the thread could
    /// not start (nothing ran). A panic on the thread is caught and logged.
    pub fn spawn_inbox_thread(
        &self,
        name: &str,
        f: impl FnOnce(&Arc<TicketsCtx>) + Send + 'static,
    ) -> bool {
        let Some(me) = self.me.get().and_then(Weak::upgrade) else {
            log::warn!("inbox: {name} needs the shared context");
            return false;
        };
        let thread_name = name.to_string();
        let spawned = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&me))).is_err() {
                    log::error!("inbox: the {thread_name} thread panicked");
                }
            });
        match spawned {
            Ok(_handle) => {
                #[cfg(test)]
                lock(&self.inbox_threads).push(_handle);
                true
            }
            Err(e) => {
                log::warn!("inbox: could not start {name}: {e}");
                false
            }
        }
    }

    /// Waits for every inbox thread started so far, also those they start (tests).
    #[cfg(test)]
    pub fn join_inbox_threads(&self) {
        loop {
            let Some(h) = lock(&self.inbox_threads).pop() else {
                return;
            };
            h.join().expect("inbox thread");
        }
    }

    /// Tickets that just became Done (step 6c, plan A.2: the write-back hook). Runs after the
    /// mutation's emits without any lock held — but possibly while the caller holds
    /// `inbox_lock` (Start), so it never takes that lock itself: a folder ticket that never
    /// wrote back gets a `mira-writeback` thread ([`crate::inbox::write_back::folder_write_back`];
    /// GitHub: [`crate::inbox::write_back::github_write_back`], which does nothing unless the
    /// project's `github.writeBack.comment` is true).
    fn on_done(&self, ids: &[TicketId]) {
        use crate::inbox::write_back::{folder_write_back, github_write_back, wants_write_back};
        for id in ids {
            let Some(e) = self.read(|s| s.get(id)).and_then(|t| t.external) else {
                continue;
            };
            if !wants_write_back(&e) {
                continue;
            }
            match e.kind {
                model::ExternalKind::Folder => {
                    let id = id.clone();
                    self.spawn_inbox_thread("mira-writeback", move |me| {
                        if let Err(err) = folder_write_back(me, &id) {
                            log::info!(
                                "inbox: no write back for ticket {}: {err}",
                                model::short_id(&id)
                            );
                        }
                    });
                }
                model::ExternalKind::Github => {
                    let id = id.clone();
                    self.spawn_inbox_thread("mira-writeback", move |me| {
                        match github_write_back(me, &id, false) {
                            Ok(wb) => log::debug!(
                                "inbox: write back of {}: {:?}",
                                model::short_id(&id),
                                wb.comment
                            ),
                            Err(err) => log::info!(
                                "inbox: no write back for ticket {}: {err}",
                                model::short_id(&id)
                            ),
                        }
                    });
                }
            }
        }
    }

    /// The context in an `Arc` that knows itself, so the project checks can run on their own
    /// thread (step 6b). Without it the checks run inline.
    pub fn shared(self) -> Arc<Self> {
        let me = Arc::new(self);
        let _ = me.me.set(Arc::downgrade(&me));
        me
    }

    /// Replaces the check runner (tests).
    #[cfg(test)]
    pub fn with_check_runner(mut self, checks: Arc<dyn CheckRunner>) -> Self {
        self.checks = checks;
        self
    }

    /// Waits for every checks thread started so far (tests).
    #[cfg(test)]
    pub fn join_checks(&self) {
        loop {
            let Some(h) = lock(&self.check_threads).pop() else {
                return;
            };
            h.join().expect("checks thread");
        }
    }

    /// Runs `f` under the service lock (which also saves). On success: syncs every agent's
    /// ticket link, emits `tickets-changed` (full list without history) and, if a link changed,
    /// `agents-changed`. On error nothing is emitted. Callers use [`Self::notify`] for the agents
    /// whose queue they touched; the only notifications sent from here are the relation effects
    /// (step 6a, [`Self::mutate_if`]).
    pub fn mutate<T>(
        &self,
        f: impl FnOnce(&mut TicketService) -> Result<T, TicketError>,
    ) -> Result<T, String> {
        self.mutate_if(f, |_| true)
    }

    /// `mutate`, but emits only when `changed(&result)` (a no-op mutation stays silent).
    ///
    /// Step 6a hook (plan A.5): a relations snapshot is taken before and after `f` (under the
    /// lock); after the lock is released and the emits are done, [`relation_effects`] decides
    /// whom to wake ([`Self::after_relations`]). So every way a child becomes Done (reviewer,
    /// user, manual move, skipReview) or a ticket is deleted reaches the same path without
    /// changing the callers.
    ///
    /// Trin 6d-krog (plan A.8): en [`crate::notices::NoticeSnap`]-liste tages også før og efter
    /// `f`; efter `tickets-changed`-emittet (ingen lås holdt) giver [`derive_ticket_notices`]
    /// beskederne (eskaleret, forløbsforælder i review, tilbagemelding fejlet), og køen fjerner
    /// dubletter.
    pub(crate) fn mutate_if<T>(
        &self,
        f: impl FnOnce(&mut TicketService) -> Result<T, TicketError>,
        changed: impl FnOnce(&T) -> bool,
    ) -> Result<T, String> {
        let (result, list, links, reviews, before, after, notes_before, notes_after) = {
            let mut svc = lock(&self.service);
            let before = svc.relations_snapshot();
            let notes_before = svc.notice_snapshot();
            let result = f(&mut svc).map_err(String::from)?;
            if !changed(&result) {
                return Ok(result);
            }
            let after = svc.relations_snapshot();
            let notes_after = svc.notice_snapshot();
            (
                result,
                svc.list(),
                svc.links(),
                svc.open_review_counts(),
                before,
                after,
                notes_before,
                notes_after,
            )
        };
        let agents_changed = self.apply_links(&links, &reviews);
        emit_json(&self.emit, TICKETS_CHANGED, &list);
        if agents_changed {
            self.emit_agents();
        }
        let now = now_ms();
        let notices = derive_ticket_notices(&notes_before, &notes_after, now);
        if !notices.is_empty() {
            self.notices.push_all(notices, now);
        }
        let fx = relation_effects(&before, &after);
        if !fx.is_empty() {
            self.after_relations(fx);
        }
        Ok(result)
    }

    /// Acts on a mutation's relation effects (plan A.5), without any lock held: the assignees
    /// of woken parents and of unblocked tickets get `QueueChanged` (the dispatcher's
    /// `consider` then types the wake line or delivers the freed ticket), and a backlog parent
    /// whose last open child went away gets [`crate::config::CHILDREN_DONE_NOTE`] (a history
    /// entry only: its own effects are empty, so this does not recurse).
    fn after_relations(&self, fx: RelationEffects) {
        self.on_done(&fx.done);
        for (parent, agent) in &fx.woken {
            log::info!(
                "forløb: forælder {} vækkes hos {agent}",
                model::short_id(parent)
            );
        }
        for (ticket, agent) in &fx.freed {
            log::info!(
                "forløb: ticket {} er ikke længere blokeret (agent {agent})",
                model::short_id(ticket)
            );
        }
        self.notify(fx.wake.iter().chain(&fx.unblocked).cloned());
        for p in &fx.children_done {
            let now = now_ms();
            // Step 6b (plan A.2): a flow parent goes to review for the user (never routed).
            match self.mutate_if(|s| s.finish_flow(p, now), Option::is_some) {
                Ok(Some(t)) => {
                    log::info!(
                        "forløb: {} afsluttet; {} til brugeren",
                        model::short_id(p),
                        t.state.label_da()
                    );
                    continue;
                }
                Ok(None) => {}
                Err(e) => {
                    log::warn!("forløb: finishing {} failed: {e}", model::short_id(p));
                    continue;
                }
            }
            match self.mutate_if(|s| s.note_children_done(p, now), Option::is_some) {
                Ok(Some(_)) => log::info!(
                    "forløb: alle del-tickets til {} er afsluttet (ingen ejer)",
                    model::short_id(p)
                ),
                Ok(None) => {}
                Err(e) => log::warn!(
                    "forløb: noting the children of {} failed: {e}",
                    model::short_id(p)
                ),
            }
        }
    }

    /// Read-only access under the service lock.
    pub fn read<T>(&self, f: impl FnOnce(&TicketService) -> T) -> T {
        f(&lock(&self.service))
    }

    /// Writes the links into the manager (agents without tickets get `None`/0, without reviews
    /// 0). Returns whether any agent changed. Takes only the manager lock.
    fn apply_links(&self, links: &TicketLinks, reviews: &ReviewCounts) -> bool {
        let mut m = lock(&self.manager);
        let mut changed = false;
        for id in m.ids() {
            let (current, len) = links.get(&id).cloned().unwrap_or_default();
            changed |= m.set_ticket_link(&id, current, len);
            changed |= m.set_review_link(&id, reviews.get(&id).copied().unwrap_or(0));
        }
        changed
    }

    /// Re-syncs the agents' ticket links and open review counts from the service (e.g. after a
    /// new agent appeared) and emits `agents-changed` if anything changed. Returns whether it did.
    pub fn sync_links(&self) -> bool {
        let (links, reviews) = self.read(|s| (s.links(), s.open_review_counts()));
        let changed = self.apply_links(&links, &reviews);
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

    /// Where the rejected ticket `id` goes (W1): first in the sender's queue only while the
    /// sender is live and may still take the ticket — it may have moved to another project
    /// ("Flyt til projekt…") after submitting. Read before the rejection.
    pub fn reject_return(&self, id: &str) -> RejectReturn {
        let Some(tk) = self.read(|s| s.get_by_any_id(id)) else {
            return RejectReturn::Backlog;
        };
        let Some(sender) = tk.assignee_agent_id.as_deref() else {
            return RejectReturn::Backlog;
        };
        let m = lock(&self.manager);
        match m.get(sender) {
            Some(a) if !matches!(a.status, AgentStatus::Exited { .. }) => {
                // Review 4b R2-N1: a ticket without a project (older tickets.json) was never
                // moved away from its sender, so it goes back to it like before step 4b.
                let ok = matches!(
                    crate::projects::assignment_target(
                        tk.project.as_ref(),
                        a.seat_kind,
                        a.project.as_deref(),
                        &a.name,
                    ),
                    Ok(_) | Err(TicketError::ProjectRequired)
                );
                if ok {
                    RejectReturn::Sender
                } else {
                    RejectReturn::Moved
                }
            }
            _ => RejectReturn::Backlog,
        }
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

    /// Review 5c W4: `from_agent`'s ticket in progress left it (handed to `to_name`, or back to
    /// the backlog with `None`). Its detail becomes "Ticket <short> givet videre" (whatever the
    /// actor; it replaces a stale "ikke afleveret"/"turn fejlede" hint and goes like the other
    /// hints: with the next status or delivery). `by_someone_else` (the user took it, not the
    /// agent itself): the dispatcher also types a "Du skal stoppe …" line once the agent is
    /// idle. Call before [`Self::notify`], so the line goes before the agent's next delivery.
    pub fn handed_over(
        &self,
        from_agent: &str,
        ticket: &Ticket,
        to_name: Option<&str>,
        by_someone_else: bool,
    ) {
        if from_agent.is_empty() {
            return;
        }
        let short = ticket.short_id();
        // Review 5c N8: an agent that handed the ticket on itself keeps its own status text; only a
        // hand-over by someone else replaces it (the agent has not been told yet).
        if by_someone_else {
            self.set_agent_detail(
                from_agent,
                Some(prompt::handed_over_detail(&short, to_name.is_some())),
                |_| true,
            );
            self.send(DispatchMsg::HandedOver {
                agent_id: from_agent.to_string(),
                ticket_id: ticket.id.clone(),
                to_name: to_name.map(str::to_string),
            });
        }
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
    ///
    /// As a reviewer it also loses its review assignments (the tickets stay in review with the
    /// note "reviewer <note>") and the reviews are routed again (plan5 A.6).
    // TODO(windows-verify): a reviewer stopped mid-review leaves the ticket in Review without a
    // reviewer, and it is routed to another reviewer if one exists (plan5 D.56).
    pub fn release_agent(&self, agent_id: &str, note: &str) -> Result<usize, String> {
        let now = now_ms();
        let released = self.mutate_if(|s| s.release_agent(agent_id, note, now), |v| !v.is_empty());
        self.send(DispatchMsg::AgentGone {
            agent_id: agent_id.to_string(),
        });
        let reviews = self.mutate_if(
            |s| s.release_reviewer(agent_id, note, now),
            |v| !v.is_empty(),
        );
        match &reviews {
            Ok(ids) if !ids.is_empty() => log::info!(
                "agent {agent_id}: {} review(s) without reviewer ({note})",
                ids.len()
            ),
            Ok(_) => {}
            Err(e) => log::warn!("agent {agent_id}: releasing its reviews failed: {e}"),
        }
        self.route_reviews();
        let released = released?;
        if !released.is_empty() {
            log::info!(
                "agent {agent_id}: {} ticket(s) back to the backlog ({note})",
                released.len()
            );
        }
        Ok(released.len())
    }

    /// Beskeden "agent afsluttede med ticket i gang" (trin 6d, plan A.8 b) efter at
    /// [`Self::release_agent`] gav `released` tickets tilbage (intet ved 0). `agent` er den
    /// afsluttede agents info (dens `last_event_at` er afslutningen). Svarer hvor mange beskeder
    /// der kom ind.
    pub fn notice_exit(&self, agent: &AgentInfo, released: usize) -> usize {
        let now = now_ms();
        match notice_for_exit(agent, released, now) {
            Some(n) => self.notices.push_all(vec![n], now),
            None => 0,
        }
    }

    /// "Flyt til projekt…" with `force` (plan4b A.3): the agent's queued tickets go to the
    /// backlog with `note`; the agent keeps running (restarted in the new folder), so its
    /// delivery state and reviews are left alone. Returns how many tickets moved.
    pub fn release_queue(&self, agent_id: &str, note: &str) -> Result<usize, String> {
        let now = now_ms();
        let released =
            self.mutate_if(|s| s.release_agent(agent_id, note, now), |v| !v.is_empty())?;
        if !released.is_empty() {
            log::info!(
                "agent {agent_id}: {} queued ticket(s) back to the backlog ({note})",
                released.len()
            );
            self.notify([agent_id]);
        }
        Ok(released.len())
    }

    // ---- review routing (plan5 A.6) ----

    /// Gives every ticket in review without a reviewer (and not escalated) to a reviewer, or
    /// escalates it when it reached the workspace's `maxReviewRounds`. Step 6b (plan A.3), per
    /// ticket in this order: the «Ændringer» report · escalate (no checks; the user decides) ·
    /// start the project checks ([`Self::ensure_checks`]) · wait while they run and
    /// `checksGate` is on · route. Candidates: live agents with the
    /// reviewer role other than the sender; the one with the fewest open reviews wins (tie: the
    /// oldest, then the id). No candidate: the ticket waits for the user as before. Idempotent;
    /// called after every way into review and whenever reviewers come or go. Returns the number
    /// of tickets routed.
    // TODO(windows-verify): a coder submits → the review file is in the reviewer's
    // .mira-bots\reviews\ and the line is typed when the reviewer is idle (plan5 D.54); three
    // rejections give "Eskaleret" and no new review line (plan5 D.55).
    pub fn route_reviews(&self) -> usize {
        let pending = self.read(TicketService::unrouted_reviews);
        if pending.is_empty() {
            return 0;
        }
        let rules = self.workspace.rules();
        let (max, gate) = (rules.max_review_rounds, rules.checks_gate);
        let reviewers = lock(&self.manager).reviewers();
        let mut routed = 0;
        for t in pending {
            // Step 6b (plan A.6): the app's «Ændringer» report before routing or escalation.
            if t.git.is_some() {
                self.attach_changes(&t.id);
            }
            let now = now_ms();
            if t.review_round >= max {
                match self.mutate_if(|s| s.escalate(&t.id, max, now), Option::is_some) {
                    Ok(Some(_)) => log::info!(
                        "review: ticket {} escalated after {} rounds",
                        t.short_id(),
                        t.review_round
                    ),
                    Ok(None) => {}
                    Err(e) => log::warn!("review: escalating {} failed: {e}", t.short_id()),
                }
                continue;
            }
            // Step 6b (plan A.3): the project checks start here (on their own thread); with
            // `checksGate` the ticket waits for them, and the thread routes it when they pass.
            let checks = self.ensure_checks(&t);
            if gate && checks.is_some_and(|c| c.state == ChecksState::Pending) {
                log::info!(
                    "review: ticket {} waits for its project checks",
                    t.short_id()
                );
                continue;
            }
            let counts = self.read(TicketService::open_review_counts);
            let best = reviewers
                .iter()
                .filter(|r| t.assignee_agent_id.as_deref() != Some(r.id.as_str()))
                .min_by(|a, b| {
                    let load =
                        |r: &crate::agent::AgentInfo| counts.get(&r.id).copied().unwrap_or(0);
                    (load(a), a.created_at, &a.id).cmp(&(load(b), b.created_at, &b.id))
                });
            let Some(r) = best else {
                log::info!(
                    "review: no reviewer for ticket {}; waiting for the user",
                    t.short_id()
                );
                continue;
            };
            match self.mutate_if(
                |s| s.route_review(&t.id, &r.id, &r.name, now),
                Option::is_some,
            ) {
                Ok(Some(_)) => {
                    routed += 1;
                    log::info!("review: ticket {} -> reviewer {}", t.short_id(), r.id);
                    self.send(DispatchMsg::ReviewAssigned {
                        reviewer_agent_id: r.id.clone(),
                    });
                }
                Ok(None) => {}
                Err(e) => log::warn!("review: routing {} failed: {e}", t.short_id()),
            }
        }
        routed
    }

    // ---- git per ticket (step 6b, plan A.4/A.6) ----

    /// The ticket's git branch at delivery (plan A.4): a ticket that already has one keeps it (a
    /// rejected ticket goes back to its branch/worktree). Otherwise only with the workspace rule
    /// `git: worktree`, for a ticket with an existing project and without children: the base is
    /// resolved (workspace `gitBase` → origin/HEAD → current branch → commit) and the worktree
    /// `<project>/.mira-bots/wt/<short>` on `ticket/<short>` is prepared and saved as
    /// `ticket.git`. A project that is not a repository, a missing git or a failing git command
    /// is a history note by the system and `None`: the delivery always continues without git.
    /// Blocking (git, at most `GIT_TIMEOUT_MS` per command); never called under a lock.
    pub fn prepare_ticket_git(&self, ticket: &Ticket) -> Option<TicketGit> {
        if ticket.git.is_some() {
            return ticket.git.clone();
        }
        if self.workspace.rules().git != GitMode::Worktree {
            return None;
        }
        let project = ticket.project.as_ref().and_then(|p| p.id())?;
        if !self.read(|s| s.children(&ticket.id)).is_empty() {
            return None;
        }
        let _guard = lock(&self.git_lock);
        // Another path may have prepared it while this one waited.
        let current = self.read(|s| s.get(&ticket.id))?;
        if current.git.is_some() {
            return current.git;
        }
        let short = ticket.short_id();
        let Some(found) = crate::projects::find_project(self.workspace.root(), project) else {
            log::warn!("git: ticket {short}: project «{project}» not found; no git");
            return None;
        };
        let repo = PathBuf::from(&found.path);
        let prepared = if git::is_git_repo(&repo) {
            // project.json `gitBase` → workspace `gitBase` → what the repository says.
            let configured = self
                .project_files
                .read(&repo)
                .ok()
                .flatten()
                .and_then(|f| f.git_base)
                .or(self.workspace.config().git_base);
            let base = git::resolve_base(self.git.as_ref(), &repo, configured.as_deref());
            git::prepare_worktree(self.git.as_ref(), &repo, &short, &base).map(|wt| TicketGit {
                mode: GitMode::Worktree,
                branch: git::branch_name(&short).unwrap_or_default(),
                base,
                repo: repo.to_string_lossy().into_owned(),
                worktree: Some(wt.to_string_lossy().into_owned()),
            })
        } else {
            Err(format!(
                "git: projektet «{}» er ikke et git-repo; ingen branch",
                found.id
            ))
        };
        let now = now_ms();
        match prepared {
            Ok(info) => match self.mutate(|s| s.set_git(&ticket.id, Some(info.clone()), now)) {
                Ok(_) => {
                    log::info!(
                        "git: ticket {short} on {} in {}",
                        info.branch,
                        info.worktree.as_deref().unwrap_or(&info.repo)
                    );
                    // Step 6d (A.10): the timeline's "worktree oprettet" note, only here (this
                    // branch is reached once: `current.git` was `None`).
                    let note = worktree_created_note(&info.branch);
                    if let Err(e) = self.mutate_if(
                        |s| s.note_by_system(&ticket.id, &note, now),
                        Option::is_some,
                    ) {
                        log::warn!("git: noting the worktree on ticket {short} failed: {e}");
                    }
                    Some(info)
                }
                Err(e) => {
                    log::warn!("git: saving ticket {short}'s branch failed: {e}");
                    None
                }
            },
            Err(note) => {
                log::warn!("git: ticket {short}: {note}; delivering without git");
                if let Err(e) = self.mutate_if(
                    |s| s.note_by_system(&ticket.id, &note, now),
                    Option::is_some,
                ) {
                    log::warn!("git: noting on ticket {short} failed: {e}");
                }
                None
            }
        }
    }

    /// Adds the app's «Ændringer» report (diff stat `base...branch`, commits `base..branch`,
    /// uncommitted files in the worktree; author System) to a ticket in review with git info,
    /// once per review entry ([`needs_changes_report`]). A failure (git or the report) is tried
    /// once per review entry: it is logged, noted on the ticket by the app ([`changes_failed_note`])
    /// and remembered in `changes_failed` (review6b W4). Returns whether a report was added.
    pub fn attach_changes(&self, ticket_id: &str) -> bool {
        let _guard = lock(&self.git_lock);
        let Some(t) = self.read(|s| s.get(ticket_id)) else {
            return false;
        };
        if !needs_changes_report(&t) {
            return false;
        }
        let Some(g) = t.git.as_ref() else {
            return false;
        };
        let entry = (t.id.clone(), service::review_entry_at(&t));
        if lock(&self.changes_failed).contains(&entry) {
            return false;
        }
        let short = t.short_id();
        let failed = |e: &str| {
            lock(&self.changes_failed).insert(entry.clone());
            let note = changes_failed_note(e);
            if let Err(e) = self.mutate_if(
                |s| s.note_by_system(&t.id, &note, now_ms()),
                Option::is_some,
            ) {
                log::warn!("git: noting the changes failure on ticket {short} failed: {e}");
            }
        };
        let summary = git::change_summary(
            self.git.as_ref(),
            Path::new(&g.repo),
            g.worktree.as_deref().map(Path::new),
            &g.base,
            &g.branch,
        );
        let body = match summary {
            Ok(s) => git::render_changes(&s),
            Err(e) => {
                log::warn!("git: changes of ticket {short} could not be read: {e}");
                failed(&e.to_string());
                return false;
            }
        };
        match self.add_report(&t.id, ReportAuthor::system(), CHANGES_REPORT_TITLE, &body) {
            Ok(r) => {
                log::info!("git: report {} «Ændringer» added to ticket {short}", r.id);
                true
            }
            Err(e) => {
                log::warn!("git: adding the changes report to ticket {short} failed: {e}");
                failed(&e);
                false
            }
        }
    }

    // ---- project checks (step 6b, plan A.3) ----

    /// The folder of the ticket's existing project.
    fn project_dir(&self, t: &Ticket) -> Option<PathBuf> {
        let id = t.project.as_ref().and_then(|p| p.id())?;
        crate::projects::find_project(self.workspace.root(), id).map(|p| PathBuf::from(p.path))
    }

    /// The project's checks as "name: `run`" lines for the review file (empty: none, no project
    /// or an unreadable file).
    pub fn project_checks(&self, project: Option<&str>) -> Vec<String> {
        let Some(dir) = project
            .and_then(|p| crate::projects::find_project(self.workspace.root(), p))
            .map(|p| PathBuf::from(p.path))
        else {
            return Vec::new();
        };
        match self.project_files.read(&dir) {
            Ok(Some(f)) => f
                .checks
                .iter()
                .map(|c| format!("{}: `{}`", c.name, c.run))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// The checks state of a ticket in review for [`Self::route_reviews`]: the stored one, or —
    /// for a [`checkable`] ticket without one — the checks are started now: an unreadable
    /// project file gives the report "Tjek: project.json kunne ikke læses" and `skipped` (a
    /// configuration error never sends a ticket back), no file or no checks give `skipped`,
    /// otherwise `pending` and a "mira-checks" thread runs them in the ticket's worktree (else
    /// the project folder) and hands the result to [`Self::finish_checks`]. `None`: not
    /// checkable. Never runs a check under a lock.
    pub fn ensure_checks(&self, t: &Ticket) -> Option<TicketChecks> {
        if t.checks.is_some() {
            return t.checks.clone();
        }
        let sender = t.assignee_agent_id.as_deref().and_then(|a| {
            lock(&self.manager)
                .get(a)
                .filter(|i| !matches!(i.status, AgentStatus::Exited { .. }))
        });
        let children = self.read(|s| s.children(&t.id).len());
        if !checkable(t, sender.as_ref(), children) {
            return None;
        }
        let short = t.short_id();
        let now = now_ms();
        let dir = self.project_dir(t);
        let file = match &dir {
            Some(d) => self.project_files.read(d),
            None => Ok(None),
        };
        let checks = match file {
            Ok(Some(f)) if !f.checks.is_empty() => f.checks,
            Ok(_) => return self.skip_checks(t, now),
            Err(e) => {
                log::warn!("checks: ticket {short}: {e}");
                let (title, body) = checks::unreadable_report(&e);
                if let Err(e) = self.add_report(&t.id, ReportAuthor::system(), &title, &body) {
                    log::warn!("checks: adding the report to ticket {short} failed: {e}");
                }
                return self.skip_checks(t, now);
            }
        };
        let round = t.review_round;
        // Only one path starts them (route_reviews may run on several threads at once).
        let started = self.mutate_if(
            |s| match s.get(&t.id) {
                Some(c) if c.state == TicketState::Review && c.checks.is_none() => {
                    s.start_checks(&t.id, round, now).map(Some)
                }
                _ => Ok(None),
            },
            Option::is_some,
        );
        let started = match started {
            Ok(Some(tk)) => tk,
            Ok(None) => return self.read(|s| s.get(&t.id)).and_then(|c| c.checks),
            Err(e) => {
                log::warn!("checks: starting the checks of ticket {short} failed: {e}");
                return None;
            }
        };
        let cwd = t
            .git
            .as_ref()
            .and_then(|g| g.worktree.as_deref())
            .map(PathBuf::from)
            .filter(|w| w.is_dir())
            .or(dir)
            .unwrap_or_default();
        log::info!(
            "checks: ticket {short}: {} check(s) in {}",
            checks.len(),
            cwd.display()
        );
        self.spawn_checks(t.id.clone(), round, now, checks, cwd);
        started.checks
    }

    /// `skipped` for this review entry (only while the ticket has no checks result yet).
    fn skip_checks(&self, t: &Ticket, now: u64) -> Option<TicketChecks> {
        let r = self.mutate_if(
            |s| match s.get(&t.id) {
                Some(c) if c.state == TicketState::Review && c.checks.is_none() => {
                    s.skip_checks(&t.id, now).map(Some)
                }
                _ => Ok(None),
            },
            Option::is_some,
        );
        match r {
            Ok(Some(tk)) => tk.checks,
            Ok(None) => self.read(|s| s.get(&t.id)).and_then(|c| c.checks),
            Err(e) => {
                log::warn!("checks: skipping for ticket {} failed: {e}", t.short_id());
                None
            }
        }
    }

    /// Runs the checks on a "mira-checks" thread (inline when this context is not
    /// [`Self::shared`]) and finishes with [`Self::finish_checks`]. A panic on the thread is
    /// caught (review6b N20): the run ends `skipped` via [`Self::abort_checks`]. Release builds
    /// use `panic = "abort"` (workspace Cargo.toml), so there the process ends as before and
    /// `load_and_recover` clears the pending run at the next start (review6b N23).
    fn spawn_checks(
        &self,
        id: String,
        round: u32,
        started_at: u64,
        list: Vec<checks::Check>,
        cwd: PathBuf,
    ) {
        let Some(me) = self.me.get().and_then(Weak::upgrade) else {
            let report = checks::run_checks(self.checks.as_ref(), &list, &cwd);
            self.finish_checks(&id, round, started_at, &report);
            return;
        };
        let thread_id = id.clone();
        let spawned = std::thread::Builder::new()
            .name("mira-checks".into())
            .spawn(move || {
                let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let report = checks::run_checks(me.checks.as_ref(), &list, &cwd);
                    me.finish_checks(&thread_id, round, started_at, &report);
                }));
                if let Err(panic) = run {
                    let what = panic
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_default();
                    log::error!(
                        "checks: the checks thread of ticket {} panicked: {what}",
                        model::short_id(&thread_id)
                    );
                    me.abort_checks(&thread_id, round, started_at);
                }
            });
        match spawned {
            Ok(_handle) => {
                #[cfg(test)]
                lock(&self.check_threads).push(_handle);
            }
            Err(e) => {
                log::warn!("checks: could not start the checks thread: {e}");
                // Nothing ran: the ticket must not wait forever.
                self.skip_checks_run(&id, round, started_at);
            }
        }
    }

    /// Marks this run (still `pending`) `skipped`; `true` when it was stored.
    fn skip_checks_run(&self, id: &str, round: u32, started_at: u64) -> bool {
        let now = now_ms();
        let r = self.mutate_if(
            |s| match s.get(id) {
                Some(t) if same_checks_run(&t, round, started_at) => s
                    .set_checks(
                        id,
                        TicketChecks {
                            state: ChecksState::Skipped,
                            failed: None,
                            round,
                            started_at,
                        },
                        now,
                    )
                    .map(Some),
                _ => Ok(None),
            },
            Option::is_some,
        );
        match r {
            Ok(stored) => stored.is_some(),
            Err(e) => {
                log::warn!(
                    "checks: skipping the run of ticket {} failed: {e}",
                    model::short_id(id)
                );
                false
            }
        }
    }

    /// The checks thread panicked (review6b N20): like a run that never started, the run ends
    /// `skipped` (an internal error never sends a ticket back) with [`CHECKS_ABORTED_NOTE`] by
    /// the app, and the reviews are routed as after [`Self::finish_checks`]. A run that
    /// already got its result (the panic came later) is left as it is.
    fn abort_checks(&self, id: &str, round: u32, started_at: u64) {
        if self.skip_checks_run(id, round, started_at) {
            if let Err(e) = self.mutate_if(
                |s| s.note_by_system(id, CHECKS_ABORTED_NOTE, now_ms()),
                Option::is_some,
            ) {
                log::warn!(
                    "checks: noting on ticket {} failed: {e}",
                    model::short_id(id)
                );
            }
        }
        self.route_reviews();
    }

    /// The checks of `id` ended (on the checks thread; no lock held). A result for another run
    /// (the ticket left review, was submitted again, or the app restarted) is dropped. Otherwise:
    /// the «Tjek» report (author: the app), `passed`/`failed`, and — when a check failed,
    /// `checksGate` is on and the ticket is not escalated — the app rejects it ("Tjek fejlede:
    /// … Se rapport nn."; the round counts towards `maxReviewRounds`) and the sender is told.
    /// Then the reviews are routed.
    pub fn finish_checks(&self, id: &str, round: u32, started_at: u64, report: &ChecksReport) {
        let current = self.read(|s| s.get(id));
        let Some(t) = current.filter(|t| same_checks_run(t, round, started_at)) else {
            log::info!(
                "checks: stale result for ticket {} dropped",
                model::short_id(id)
            );
            // Left review while running (approved, rejected, moved): no "running" state stays.
            let now = now_ms();
            if let Err(e) = self.mutate_if(
                |s| s.clear_checks_run(id, round, started_at, now),
                Option::is_some,
            ) {
                log::debug!("checks: clearing the stale run failed: {e}");
            }
            return;
        };
        let short = t.short_id();
        let (title, body) = checks::render_checks_report(report);
        let report_id = match self.add_report(id, ReportAuthor::system(), &title, &body) {
            Ok(r) => Some(r.id),
            Err(e) => {
                log::warn!("checks: adding the report to ticket {short} failed: {e}");
                None
            }
        };
        let state = if report.failed.is_some() {
            ChecksState::Failed
        } else {
            ChecksState::Passed
        };
        let now = now_ms();
        let stored = self.mutate_if(
            |s| match s.get(id) {
                Some(t) if same_checks_run(&t, round, started_at) => s
                    .set_checks(
                        id,
                        TicketChecks {
                            state,
                            failed: report.failed.clone(),
                            round,
                            started_at,
                        },
                        now,
                    )
                    .map(Some),
                _ => Ok(None),
            },
            Option::is_some,
        );
        let t = match stored {
            Ok(Some(t)) => t,
            Ok(None) => {
                log::info!("checks: ticket {short} moved on while storing; result dropped");
                return;
            }
            Err(e) => {
                log::warn!("checks: storing the result of ticket {short} failed: {e}");
                return;
            }
        };
        log::info!("checks: ticket {short}: {title}");
        let gate = self.workspace.rules().checks_gate;
        if let (true, false, Some(line)) = (gate, t.escalated, report.first_failure()) {
            let note = checks::gate_note(line, report_id.as_deref());
            let sender = t.assignee_agent_id.clone();
            let to = self.reject_return(id);
            match self.mutate(|s| s.reject_by_system(id, &note, to, now)) {
                Ok(tk) => {
                    log::info!(
                        "checks: ticket {short} rejected by the app (round {})",
                        tk.review_round
                    );
                    self.notify(sender);
                }
                Err(e) => log::warn!("checks: rejecting ticket {short} failed: {e}"),
            }
        }
        self.route_reviews();
    }

    /// `assign_reviewer` (C5.4): the ticket must be in review. `Some(agent)`: a live agent with
    /// the reviewer role other than the sender replaces any current reviewer (an escalation is
    /// cleared for this round; the round count is unchanged). `None`: the reviewer is removed
    /// ("reviewer fjernet") and the ticket is routed again, unless it reached
    /// the workspace's `maxReviewRounds`: then it stays escalated (no new escalation note,
    /// review5 N5).
    pub fn assign_reviewer(
        &self,
        ticket_id: &str,
        agent_id: Option<&str>,
    ) -> Result<TicketSummary, String> {
        let t = self
            .read(|s| s.get(ticket_id))
            .ok_or(TicketError::NotFound)?;
        if t.state != TicketState::Review {
            return Err(TicketError::NotInReview.into());
        }
        // Step 6b (plan A.2/F): a finished flow parent has no sender; only the user decides.
        if t.assignee_agent_id.is_none() {
            return Err(FLOW_REVIEW_IS_USERS.into());
        }
        let now = now_ms();
        match agent_id {
            Some(agent) => {
                let pending = t
                    .checks
                    .as_ref()
                    .is_some_and(|c| c.state == ChecksState::Pending);
                if pending && self.workspace.rules().checks_gate {
                    return Err(CHECKS_RUNNING_REVIEWER.into());
                }
                let info = lock(&self.manager)
                    .get(agent)
                    .filter(|a| !matches!(a.status, AgentStatus::Exited { .. }))
                    .ok_or(TicketError::AgentNotLive)?;
                if !info.roles.contains(&Role::Reviewer) {
                    return Err(TicketError::NotAReviewer.into());
                }
                if t.assignee_agent_id.as_deref() == Some(agent) {
                    return Err(TicketError::SenderCannotReview.into());
                }
                let tk = self.mutate(|s| s.set_reviewer(&t.id, agent, &info.name, now))?;
                self.send(DispatchMsg::ReviewAssigned {
                    reviewer_agent_id: agent.to_string(),
                });
                Ok(TicketSummary::from(&tk))
            }
            None => {
                let max = self.workspace.rules().max_review_rounds;
                self.mutate_if(
                    |s| s.clear_reviewer(&t.id, REVIEWER_REMOVED_NOTE, TicketActor::User, max, now),
                    Option::is_some,
                )?;
                self.route_reviews();
                self.read(|s| s.get(&t.id))
                    .map(|tk| TicketSummary::from(&tk))
                    .ok_or_else(|| TicketError::NotFound.into())
            }
        }
    }

    // ---- reports (plan5 A.8) ----

    /// Adds a report to ticket `ticket_id` (full id): validates ([`validate_report`], ticket
    /// exists, at most 20), writes the file, then the metadata; a failed metadata save removes
    /// the file again. Ownership is the caller's business (tools: own ticket or review).
    // TODO(windows-verify): mira_add_report writes
    // %APPDATA%\dk.mira.bots\tickets\<id>\reports\01-<slug>.md (plan5 D.58).
    pub fn add_report(
        &self,
        ticket_id: &str,
        author: ReportAuthor,
        title: &str,
        body: &str,
    ) -> Result<TicketReport, String> {
        let (title, body) = validate_report(title, body)?;
        let _guard = lock(&self.report_lock);
        let seq = self.read(|s| s.next_report_seq(ticket_id))?;
        let (path, size) = self
            .reports
            .write(ticket_id, seq, &title, &body)
            .map_err(|e| format!("Kunne ikke gemme rapporten: {e}"))?;
        let now = now_ms();
        let report = TicketReport {
            id: format!("{seq:02}"),
            title,
            author,
            created_at: now,
            path: path.clone(),
            size,
        };
        if let Err(e) = self.mutate(|s| s.add_report_meta(ticket_id, report.clone(), now)) {
            if let Err(rm) = self.reports.remove(ticket_id, &path) {
                log::warn!("removing report file after a failed save: {rm}");
            }
            return Err(e);
        }
        log::info!(
            "report {} added to ticket {ticket_id} ({size} bytes)",
            report.id
        );
        Ok(report)
    }

    /// A report's metadata and text (`ticket_id`: full or short id).
    pub fn get_report(&self, ticket_id: &str, report_id: &str) -> Result<ReportContent, String> {
        let t: Ticket = self
            .read(|s| s.get_by_any_id(ticket_id))
            .ok_or(TicketError::NotFound)?;
        let report = t
            .reports
            .iter()
            .find(|r| r.id == report_id.trim())
            .cloned()
            .ok_or(TicketError::ReportNotFound)?;
        let body = self.reports.read(&t.id, &report.path).map_err(|e| {
            log::warn!("reading report {} of ticket {}: {e}", report.id, t.id);
            String::from(TicketError::ReportNotFound)
        })?;
        Ok(ReportContent { report, body })
    }

    /// Deletes the ticket (service rules) and then its report folder (a failure there is logged).
    pub fn delete_ticket(&self, id: &str) -> Result<(), String> {
        self.mutate(|s| s.delete(id))?;
        if let Err(e) = self.reports.remove_ticket_dir(id) {
            log::warn!("removing the report folder of ticket {id} failed: {e}");
        }
        Ok(())
    }
}

/// The ticket is still in review with the pending checks run `(round, started_at)`.
fn same_checks_run(t: &Ticket, round: u32, started_at: u64) -> bool {
    t.state == TicketState::Review
        && t.checks.as_ref().is_some_and(|c| {
            c.state == ChecksState::Pending && c.round == round && c.started_at == started_at
        })
}

impl TicketsHost for Arc<TicketsCtx> {
    fn reroute_reviews(&self) {
        TicketsCtx::route_reviews(self);
    }

    fn mutate<T>(
        &self,
        f: impl FnOnce(&mut TicketService) -> Result<T, TicketError>,
    ) -> Result<T, String> {
        TicketsCtx::mutate(self, f)
    }

    fn mutate_if<T>(
        &self,
        f: impl FnOnce(&mut TicketService) -> Result<T, TicketError>,
        changed: impl FnOnce(&T) -> bool,
    ) -> Result<T, String> {
        TicketsCtx::mutate_if(self, f, changed)
    }

    fn read<T>(&self, f: impl FnOnce(&TicketService) -> T) -> T {
        TicketsCtx::read(self, f)
    }

    fn rules(&self) -> WorkspaceRules {
        self.workspace.rules()
    }

    fn project_ids(&self) -> Vec<String> {
        crate::projects::list_projects(self.workspace.root())
            .into_iter()
            .map(|p| p.id)
            .collect()
    }

    fn prepare_git(&self, ticket: &Ticket, _cwd: &Path) -> Option<TicketGit> {
        TicketsCtx::prepare_ticket_git(self, ticket)
    }

    fn project_checks(&self, project: Option<&str>) -> Vec<String> {
        TicketsCtx::project_checks(self, project)
    }
}

/// The dispatcher's [`AgentPort`] over the real manager. Each call takes the manager lock
/// briefly; a detail change is announced with `agents-changed` (after the lock is released).
/// `restart` is the app's restart path for a fresh session per ticket (step 6b, plan A.7;
/// `commands::restart_port`, set in `lib.rs`); without it `restart_fresh` is refused and the
/// ticket is typed into the current session.
#[derive(Clone)]
pub struct ManagerPort {
    pub manager: Arc<Mutex<AgentManager>>,
    pub emit: EmitFn,
    pub restart: Option<RestartPort>,
}

impl ManagerPort {
    pub fn new(manager: Arc<Mutex<AgentManager>>, emit: EmitFn) -> Self {
        ManagerPort {
            manager,
            emit,
            restart: None,
        }
    }

    /// Installs the restart path (step 6b).
    pub fn with_restart(mut self, restart: RestartPort) -> Self {
        self.restart = Some(restart);
        self
    }
}

impl AgentPort for ManagerPort {
    fn has_live_coordinator(&self) -> bool {
        lock(&self.manager).has_live_coordinator()
    }

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
            seat_kind: a.seat_kind,
            roles: a.roles,
            project: a.project,
            session_id: a.session_id,
        })
    }

    fn restart_fresh(&self, req: RestartForTicket) -> Result<(), String> {
        match &self.restart {
            // No manager lock is held here: the restart takes it itself.
            Some(restart) => restart(req),
            None => Err("genstart er ikke tilgængelig".into()),
        }
    }

    fn write_input(&self, id: &str, bytes: &[u8]) -> Result<(), String> {
        lock(&self.manager)
            .write_input(id, bytes)
            .map_err(|e| e.to_string())
    }

    fn peers_in_project(&self, id: &str) -> Vec<String> {
        let m = lock(&self.manager);
        let Some(me) = m.get(id) else {
            return Vec::new();
        };
        let Some(project) = me.project.as_deref() else {
            return Vec::new();
        };
        m.live_work_in_project(project)
            .into_iter()
            .filter(|a| a.id != id)
            .map(|a| a.name)
            .collect()
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

    /// [`test_ctx`] with a given inbox (step 6c).
    pub fn test_ctx_with_inbox(manager: Arc<Mutex<AgentManager>>, inbox: InboxService) -> TestCtx {
        build_test_ctx(
            manager,
            Arc::new(crate::git::fake::FakeGit::new()),
            Arc::new(crate::checks::ProcessChecks::new()),
            inbox,
        )
    }

    pub fn test_ctx(manager: Arc<Mutex<AgentManager>>) -> TestCtx {
        test_ctx_with_git(manager, Arc::new(crate::git::fake::FakeGit::new()))
    }

    /// [`test_ctx_with`] with a scripted git and the real check runner.
    pub fn test_ctx_with_git(
        manager: Arc<Mutex<AgentManager>>,
        git: Arc<dyn crate::git::GitRunner>,
    ) -> TestCtx {
        test_ctx_with(manager, git, Arc::new(crate::checks::ProcessChecks::new()))
    }

    /// [`test_ctx`] with a given git and check runner (step 6b). The workspace file (absent: the
    /// defaults) is `ctx.workspace.path()`; its folder is the projects root.
    pub fn test_ctx_with(
        manager: Arc<Mutex<AgentManager>>,
        git: Arc<dyn crate::git::GitRunner>,
        checks: Arc<dyn crate::checks::CheckRunner>,
    ) -> TestCtx {
        let inbox = InboxService::new(
            Box::new(crate::inbox::MemoryInboxStore::new()),
            crate::inbox::InboxDoc::default(),
        );
        build_test_ctx(manager, git, checks, inbox)
    }

    /// [`test_ctx_with_inbox`] with a scripted `gh` (step 6c B3).
    pub fn test_ctx_with_gh(
        manager: Arc<Mutex<AgentManager>>,
        gh: Arc<dyn crate::gh::GhRunner>,
        inbox: InboxService,
    ) -> TestCtx {
        build_test_ctx_gh(
            manager,
            Arc::new(crate::git::fake::FakeGit::new()),
            Arc::new(crate::checks::ProcessChecks::new()),
            inbox,
            gh,
        )
    }

    fn build_test_ctx(
        manager: Arc<Mutex<AgentManager>>,
        git: Arc<dyn crate::git::GitRunner>,
        checks: Arc<dyn crate::checks::CheckRunner>,
        inbox: InboxService,
    ) -> TestCtx {
        // A gh without answers: no test ever runs the real one.
        let gh = Arc::new(crate::gh::fake::FakeGh::new());
        build_test_ctx_gh(manager, git, checks, inbox, gh)
    }

    fn build_test_ctx_gh(
        manager: Arc<Mutex<AgentManager>>,
        git: Arc<dyn crate::git::GitRunner>,
        checks: Arc<dyn crate::checks::CheckRunner>,
        inbox: InboxService,
        gh: Arc<dyn crate::gh::GhRunner>,
    ) -> TestCtx {
        let store = MemoryStore::new();
        let svc = TicketService::new(Box::new(store.clone()), TicketDoc::default());
        let (tx, rx) = unbounded_channel();
        let events: Events = Arc::default();
        let sink = Arc::clone(&events);
        let emit: EmitFn = Arc::new(move |n: &str, v: Value| {
            sink.lock().unwrap().push((n.to_string(), v));
        });
        let data_dir = std::env::temp_dir().join(format!("mira-data-{}", uuid::Uuid::new_v4()));
        let reports_root = data_dir.join(crate::config::REPORTS_DIR);
        // A workspace file that does not exist: the defaults.
        let workspace = Arc::new(WorkspaceReader::new(
            std::env::temp_dir()
                .join(format!("mira-ws-{}", uuid::Uuid::new_v4()))
                .join(crate::config::WORKSPACE_FILE),
        ));
        TestCtx {
            ctx: TicketsCtx::new(svc, manager, tx, emit, reports_root, workspace, git, inbox)
                .with_check_runner(checks)
                .with_gh(gh)
                .with_data_dir(data_dir)
                .shared(),
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

    /// Step 6b: the port tells the agent's session id (review6b W3/W5) and passes a restart to
    /// the installed path (refused without one); it holds no manager lock while calling it.
    #[test]
    fn manager_port_session_id_and_restart_fresh() {
        let m = Arc::new(Mutex::new(AgentManager::new(5)));
        let id =
            lock(&m).insert_fake_with("s-1", "/w/a", &[Role::Coder], crate::agent::SeatKind::Work);
        let emit: EmitFn = Arc::new(|_, _| {});
        let port = ManagerPort::new(Arc::clone(&m), emit);
        assert_eq!(port.snapshot(&id).unwrap().session_id, "s-1");
        let req = RestartForTicket {
            agent_id: id.clone(),
            cwd: Some(PathBuf::from("/w/a/.mira-bots/wt/ab12cd34")),
            force_fresh: true,
            ticket_short: "ab12cd34".into(),
        };
        assert!(port.restart_fresh(req.clone()).is_err());
        let seen: Arc<Mutex<Vec<RestartForTicket>>> = Arc::default();
        let (seen2, m2) = (Arc::clone(&seen), Arc::clone(&m));
        let port = port.with_restart(Arc::new(move |r: RestartForTicket| {
            // The manager lock is free during the call.
            assert!(m2.try_lock().is_ok());
            seen2.lock().unwrap().push(r);
            Ok(())
        }));
        assert_eq!(port.restart_fresh(req.clone()), Ok(()));
        assert_eq!(*seen.lock().unwrap(), vec![req]);
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
        let ctx = TicketsCtx::new(
            svc,
            Arc::clone(&m),
            tx,
            Arc::clone(&emit),
            std::env::temp_dir().join("mira-unused-reports"),
            Arc::new(WorkspaceReader::new(
                std::env::temp_dir()
                    .join(format!("mira-ws-{}", uuid::Uuid::new_v4()))
                    .join(crate::config::WORKSPACE_FILE),
            )),
            Arc::new(crate::git::fake::FakeGit::new()),
            InboxService::new(
                Box::new(crate::inbox::MemoryInboxStore::new()),
                crate::inbox::InboxDoc::default(),
            ),
        )
        .shared();
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
        use crate::agent::{EventSink, SeatKind, SinkEvent};
        use crate::config::{PTY_COLS, PTY_ROWS};
        use crate::profiles::model::ProfileSnapshot;
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
                    // A work role: the plain ticket line (review 5c W1).
                    profile: ProfileSnapshot {
                        roles: vec![Role::Coder],
                        ..ProfileSnapshot::default()
                    },
                    seat_kind: SeatKind::Work,
                    name: "bot-01".into(),
                    project: None,
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

    // ---- step 5: review routing and reports ----

    use crate::agent::roles::Role;
    use crate::agent::SeatKind;

    /// Agents: a1 (coder), r1 and r2 (reviewers, r1 older).
    fn review_setup() -> (TestCtx, Arc<Mutex<AgentManager>>, String, String, String) {
        let mut m = AgentManager::new(5);
        let a1 = m.insert_fake_with("s-a1", "/w/a1", &[Role::Coder], SeatKind::Work);
        let r1 = m.insert_fake_with("s-r1", "/w/r1", &[Role::Reviewer], SeatKind::Staff);
        let r2 = m.insert_fake_with("s-r2", "/w/r2", &[Role::Reviewer], SeatKind::Staff);
        m.backdate(&r1, 1_000);
        let m = Arc::new(Mutex::new(m));
        (test_ctx(Arc::clone(&m)), m, a1, r1, r2)
    }

    /// A ticket submitted by `agent` (in review, unrouted).
    fn submitted(t: &TestCtx, agent: &str, title: &str) -> String {
        let c = &t.ctx;
        let tk = c.mutate(|s| s.create(title, "b", false, 1)).unwrap();
        c.mutate(|s| s.assign(&tk.id, agent, 2)).unwrap();
        c.mutate(|s| s.mark_dispatched(&tk.id, agent, 3)).unwrap();
        c.mutate(|s| s.submit_by_agent(agent, None, "klar", 4))
            .unwrap();
        tk.id
    }

    fn reviewer_of(t: &TestCtx, id: &str) -> Option<String> {
        t.ctx.read(|s| s.get(id)).unwrap().reviewer_agent_id
    }

    #[test]
    fn route_picks_reviewer_with_fewest_open_reviews() {
        let (mut t, _m, a1, r1, r2) = review_setup();
        let t1 = submitted(&t, &a1, "x");
        assert_eq!(t.ctx.route_reviews(), 1);
        assert_eq!(
            reviewer_of(&t, &t1).as_deref(),
            Some(r1.as_str()),
            "tie: oldest"
        );
        let t2 = submitted(&t, &a1, "y");
        let t3 = submitted(&t, &a1, "z");
        assert_eq!(t.ctx.route_reviews(), 2);
        assert_eq!(reviewer_of(&t, &t2).as_deref(), Some(r2.as_str()));
        assert_eq!(reviewer_of(&t, &t3).as_deref(), Some(r1.as_str()));
        assert_eq!(t.ctx.route_reviews(), 0, "idempotent");
        let sent = t.sent();
        assert_eq!(
            sent.iter()
                .filter(|m| matches!(m, DispatchMsg::ReviewAssigned { .. }))
                .count(),
            3
        );
        assert!(sent.contains(&DispatchMsg::ReviewAssigned {
            reviewer_agent_id: r2.clone()
        }));
        let hist = t.ctx.read(|s| s.get(&t1)).unwrap().history;
        assert_eq!(
            hist.last().unwrap().note.as_deref(),
            Some("review tildelt r1")
        );
    }

    #[test]
    fn route_never_picks_the_sender() {
        let (t, m, a1, r1, _r2) = review_setup();
        // r2 gone; r1 submits its own work: no other reviewer → waits.
        let _ = a1;
        let r2 = lock(&m).reviewers()[1].id.clone();
        lock(&m).stop(&r2).unwrap();
        let mine = submitted(&t, &r1, "egen");
        assert_eq!(t.ctx.route_reviews(), 0);
        assert_eq!(reviewer_of(&t, &mine), None);
    }

    #[test]
    fn route_with_no_reviewer_leaves_ticket_for_user() {
        let (m, ids) = manager_with(1);
        let t = test_ctx(m);
        let id = submitted(&t, &ids[0], "x");
        assert_eq!(t.ctx.route_reviews(), 0);
        let tk = t.ctx.read(|s| s.get(&id)).unwrap();
        assert_eq!(
            (tk.state, tk.reviewer_agent_id, tk.escalated),
            (TicketState::Review, None, false)
        );
        // The user can still approve as before.
        t.ctx.mutate(|s| s.approve(&id, 9)).unwrap();
    }

    #[test]
    fn route_escalates_at_three_rounds() {
        let (mut t, _m, a1, r1, _r2) = review_setup();
        let id = submitted(&t, &a1, "x");
        for round in 0..t.ctx.workspace.rules().max_review_rounds {
            assert_eq!(t.ctx.route_reviews(), 1, "round {round}");
            let rev = reviewer_of(&t, &id).unwrap();
            t.ctx
                .mutate(|s| s.reject_by_agent(&rev, "r", &id, "mere", RejectReturn::Sender, 10))
                .unwrap();
            t.ctx.mutate(|s| s.mark_dispatched(&id, "a1", 11)).unwrap();
            t.ctx
                .mutate(|s| s.submit_by_agent(&a1, None, "igen", 12))
                .unwrap();
        }
        t.sent();
        assert_eq!(t.ctx.route_reviews(), 0);
        let tk = t.ctx.read(|s| s.get(&id)).unwrap();
        assert_eq!(
            (tk.review_round, tk.escalated, tk.reviewer_agent_id),
            (3, true, None)
        );
        assert_eq!(
            tk.history.last().unwrap().note.as_deref(),
            Some("eskaleret efter 3 runder")
        );
        assert!(
            t.sent().is_empty(),
            "no review line for an escalated ticket"
        );
        // The user can pick a reviewer by hand.
        let s = t.ctx.assign_reviewer(&id, Some(&r1)).unwrap();
        assert_eq!(
            (s.escalated, s.reviewer_agent_id.as_deref()),
            (false, Some(r1.as_str()))
        );
        assert_eq!(
            t.sent(),
            vec![DispatchMsg::ReviewAssigned {
                reviewer_agent_id: r1.clone()
            }]
        );
        // "Fjern reviewer" (review5 N5): escalated again at once, without routing and without a
        // second escalation note; the round count stays.
        let escalations = |t: &TestCtx| {
            t.ctx
                .read(|s| s.get(&id))
                .unwrap()
                .history
                .iter()
                .filter(|h| h.note.as_deref() == Some("eskaleret efter 3 runder"))
                .count()
        };
        assert_eq!(escalations(&t), 1);
        let s = t.ctx.assign_reviewer(&id, None).unwrap();
        assert_eq!(
            (s.escalated, s.reviewer_agent_id.as_deref(), s.review_round),
            (true, None, 3)
        );
        assert_eq!(escalations(&t), 1);
        assert!(t.sent().is_empty(), "no review line");
        let tk = t.ctx.read(|s| s.get(&id)).unwrap();
        assert_eq!(
            tk.history.last().unwrap().note.as_deref(),
            Some("reviewer fjernet")
        );
        assert_eq!(t.ctx.route_reviews(), 0);
    }

    /// Step 6b: `maxReviewRounds` from the workspace file decides when routing escalates (and
    /// the escalation note names it); "Fjern reviewer" keeps the escalation at that round.
    #[test]
    fn route_reviews_escalates_at_workspace_max() {
        let (mut t, _m, a1, r1, _r2) = review_setup();
        let path = t.ctx.workspace.path().to_path_buf();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"maxReviewRounds": 1}"#).unwrap();
        assert_eq!(t.ctx.workspace.rules().max_review_rounds, 1);
        let id = submitted(&t, &a1, "x");
        assert_eq!(t.ctx.route_reviews(), 1);
        let rev = reviewer_of(&t, &id).unwrap();
        t.ctx
            .mutate(|s| s.reject_by_agent(&rev, "r", &id, "mere", RejectReturn::Sender, 10))
            .unwrap();
        t.ctx.mutate(|s| s.mark_dispatched(&id, "a1", 11)).unwrap();
        t.ctx
            .mutate(|s| s.submit_by_agent(&a1, None, "igen", 12))
            .unwrap();
        t.sent();
        assert_eq!(t.ctx.route_reviews(), 0);
        let tk = t.ctx.read(|s| s.get(&id)).unwrap();
        assert_eq!((tk.review_round, tk.escalated), (1, true));
        assert_eq!(
            tk.history.last().unwrap().note.as_deref(),
            Some("eskaleret efter 1 runder")
        );
        assert!(t.sent().is_empty(), "no review line");
        t.ctx.assign_reviewer(&id, Some(&r1)).unwrap();
        let s = t.ctx.assign_reviewer(&id, None).unwrap();
        assert_eq!((s.escalated, s.reviewer_agent_id), (true, None));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn assign_reviewer_waits_for_running_checks_with_the_gate() {
        // Review6b W7: a manual reviewer while the checks run could lose the ticket to the gate.
        let (t, _m, a1, _r1, r2) = review_setup();
        let id = submitted(&t, &a1, "x");
        t.ctx.mutate(|s| s.start_checks(&id, 0, 5)).unwrap();
        assert_eq!(
            t.ctx.assign_reviewer(&id, Some(&r2)),
            Err(CHECKS_RUNNING_REVIEWER.to_string())
        );
        // Removing the reviewer (routing again) is still allowed.
        assert!(t.ctx.assign_reviewer(&id, None).is_ok());
        // Without the gate a failing check only shows a badge: the choice is allowed.
        let path = t.ctx.workspace.path().to_path_buf();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"checksGate": false}"#).unwrap();
        t.ctx.assign_reviewer(&id, Some(&r2)).unwrap();
        assert_eq!(reviewer_of(&t, &id).as_deref(), Some(r2.as_str()));
    }

    #[test]
    fn assign_reviewer_rules() {
        let (t, m, a1, r1, r2) = review_setup();
        let id = submitted(&t, &a1, "x");
        assert_eq!(
            t.ctx.assign_reviewer(&id, Some(&a1)),
            Err("Agenten er ikke reviewer".into())
        );
        assert_eq!(
            t.ctx.assign_reviewer(&id, Some("nope")),
            Err("Agenten kører ikke".into())
        );
        let other = submitted(&t, &r1, "egen");
        assert_eq!(
            t.ctx.assign_reviewer(&other, Some(&r1)),
            Err("Afsenderen kan ikke reviewe sin egen ticket".into())
        );
        let backlog = t.ctx.mutate(|s| s.create("b", "", false, 1)).unwrap();
        assert_eq!(
            t.ctx.assign_reviewer(&backlog.id, Some(&r1)),
            Err("Ticketen er ikke i review".into())
        );
        t.ctx.assign_reviewer(&id, Some(&r2)).unwrap();
        assert_eq!(reviewer_of(&t, &id).as_deref(), Some(r2.as_str()));
        // None: removed and routed again (r1 has the fewest open reviews).
        let s = t.ctx.assign_reviewer(&id, None).unwrap();
        assert_eq!(s.reviewer_agent_id.as_deref(), Some(r1.as_str()));
        let notes: Vec<_> = t
            .ctx
            .read(|s| s.get(&id))
            .unwrap()
            .history
            .into_iter()
            .filter_map(|h| h.note)
            .collect();
        assert!(notes.contains(&"reviewer fjernet".to_string()));
        drop(m);
    }

    #[test]
    fn reviewer_exit_releases_assignments_and_reroutes() {
        let (mut t, m, a1, r1, r2) = review_setup();
        let id = submitted(&t, &a1, "x");
        t.ctx.route_reviews();
        assert_eq!(reviewer_of(&t, &id).as_deref(), Some(r1.as_str()));
        t.sent();
        lock(&m).stop(&r1).unwrap();
        t.ctx.release_agent(&r1, AGENT_STOPPED_NOTE).unwrap();
        assert_eq!(reviewer_of(&t, &id).as_deref(), Some(r2.as_str()));
        let notes: Vec<_> = t
            .ctx
            .read(|s| s.get(&id))
            .unwrap()
            .history
            .into_iter()
            .filter_map(|h| h.note)
            .collect();
        assert!(
            notes.contains(&"reviewer agent stoppet".to_string()),
            "{notes:?}"
        );
        let sent = t.sent();
        assert!(sent.contains(&DispatchMsg::AgentGone {
            agent_id: r1.clone()
        }));
        assert!(sent.contains(&DispatchMsg::ReviewAssigned {
            reviewer_agent_id: r2.clone()
        }));
        // The last reviewer goes too: the ticket stays in review for the user.
        lock(&m).stop(&r2).unwrap();
        t.ctx.release_agent(&r2, AGENT_EXITED_NOTE).unwrap();
        let tk = t.ctx.read(|s| s.get(&id)).unwrap();
        assert_eq!(
            (tk.state, tk.reviewer_agent_id),
            (TicketState::Review, None)
        );
    }

    #[test]
    fn open_reviews_link_is_synced() {
        let (t, m, a1, r1, _r2) = review_setup();
        submitted(&t, &a1, "x");
        t.clear();
        t.ctx.route_reviews();
        assert_eq!(lock(&m).get(&r1).unwrap().open_reviews, 1);
        let agents = t.emitted(AGENTS_CHANGED);
        assert_eq!(agents.len(), 1);
        assert_eq!(agent_json(&agents[0], &r1)["openReviews"], 1);
        let id = t.ctx.read(|s| s.review_assignments())[0].ticket_id.clone();
        t.ctx.mutate(|s| s.approve(&id, 20)).unwrap();
        assert_eq!(lock(&m).get(&r1).unwrap().open_reviews, 0);
    }

    #[test]
    fn add_report_writes_file_and_metadata() {
        let (m, ids) = manager_with(1);
        let t = test_ctx(m);
        let tk = t.ctx.mutate(|s| s.create("x", "", false, 1)).unwrap();
        t.clear();
        let r = t
            .ctx
            .add_report(
                &tk.id,
                ReportAuthor::agent(&ids[0]),
                " Første\nrapport ",
                "# Hej\r\næøå\u{0}\n",
            )
            .unwrap();
        assert_eq!((r.id.as_str(), r.title.as_str()), ("01", "Første rapport"));
        assert_eq!(r.path, "reports/01-foerste-rapport.md");
        let file = t
            .ctx
            .reports
            .dir_for(&tk.id)
            .unwrap()
            .join("01-foerste-rapport.md");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "# Hej\næøå");
        assert_eq!(r.size, "# Hej\næøå".len() as u64);
        let lists = t.emitted(TICKETS_CHANGED);
        assert_eq!(lists.len(), 1);
        assert_eq!(lists[0][0]["reportCount"], 1);
        let got = t.ctx.get_report(&tk.short_id(), "01").unwrap();
        assert_eq!((got.report, got.body.as_str()), (r.clone(), "# Hej\næøå"));
        let v = serde_json::to_value(t.ctx.get_report(&tk.id, "01").unwrap()).unwrap();
        assert_eq!(v["body"], "# Hej\næøå");
        assert_eq!(v["report"]["id"], "01");
        let r2 = t
            .ctx
            .add_report(&tk.id, ReportAuthor::user(), "To", "b")
            .unwrap();
        assert_eq!(r2.id, "02");
        assert_eq!(
            t.ctx.get_report(&tk.id, "09").unwrap_err(),
            "Rapporten findes ikke"
        );
        let _ = std::fs::remove_dir_all(t.ctx.reports.root());
    }

    #[test]
    fn report_limits() {
        let (m, _) = manager_with(0);
        let t = test_ctx(m);
        let tk = t.ctx.mutate(|s| s.create("x", "", false, 1)).unwrap();
        let add =
            |title: &str, body: &str| t.ctx.add_report(&tk.id, ReportAuthor::user(), title, body);
        assert_eq!(add(" \n ", "b").unwrap_err(), "Titel må ikke være tom");
        assert_eq!(
            add(&"t".repeat(121), "b").unwrap_err(),
            "Titlen er for lang (maks 120 tegn)"
        );
        assert_eq!(add("t", " \n").unwrap_err(), "Rapporten må ikke være tom");
        assert_eq!(
            add("t", &"b".repeat(20_001)).unwrap_err(),
            "Rapporten er for lang (maks 20000 tegn)"
        );
        assert_eq!(
            t.ctx
                .add_report("nope", ReportAuthor::user(), "t", "b")
                .unwrap_err(),
            "Ticketen findes ikke"
        );
        assert!(add(&"t".repeat(120), &"b".repeat(20_000)).is_ok());
        for _ in 1..20 {
            add("t", "b").unwrap();
        }
        assert_eq!(
            add("t", "b").unwrap_err(),
            "Ticketen har allerede 20 rapporter"
        );
        let dir = t.ctx.reports.dir_for(&tk.id).unwrap();
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 20);
        let _ = std::fs::remove_dir_all(t.ctx.reports.root());
    }

    #[test]
    fn get_report_refuses_path_traversal() {
        let (m, _) = manager_with(0);
        let t = test_ctx(m);
        let tk = t.ctx.mutate(|s| s.create("x", "", false, 1)).unwrap();
        let r = t
            .ctx
            .add_report(&tk.id, ReportAuthor::user(), "t", "b")
            .unwrap();
        // A tampered path in tickets.json is never followed.
        let mut bad = r.clone();
        bad.id = "02".into();
        bad.path = "reports/../../../secret.txt".into();
        t.ctx.mutate(|s| s.add_report_meta(&tk.id, bad, 2)).unwrap();
        assert_eq!(
            t.ctx.get_report(&tk.id, "02").unwrap_err(),
            "Rapporten findes ikke"
        );
        assert!(t.ctx.get_report(&tk.id, "01").is_ok());
        let _ = std::fs::remove_dir_all(t.ctx.reports.root());
    }

    #[test]
    fn delete_ticket_removes_report_dir() {
        let (m, _) = manager_with(0);
        let t = test_ctx(m);
        let tk = t.ctx.mutate(|s| s.create("x", "", false, 1)).unwrap();
        t.ctx
            .add_report(&tk.id, ReportAuthor::user(), "t", "b")
            .unwrap();
        let dir = t.ctx.reports.root().join(&tk.id);
        assert!(dir.is_dir());
        t.ctx.delete_ticket(&tk.id).unwrap();
        assert!(!dir.exists());
        assert!(t.ctx.read(|s| s.get(&tk.id)).is_none());
        // A ticket without reports deletes fine too.
        let tk2 = t.ctx.mutate(|s| s.create("y", "", false, 1)).unwrap();
        t.ctx.delete_ticket(&tk2.id).unwrap();
        let _ = std::fs::remove_dir_all(t.ctx.reports.root());
    }

    #[test]
    fn manager_port_and_host_see_projects_and_rules() {
        use crate::agent::SeatKind;
        let m = Arc::new(Mutex::new(AgentManager::new(5)));
        let (a, b, c) = {
            let mut g = lock(&m);
            let a = g.insert_fake_in("a", "/r/p", &[Role::Coder], SeatKind::Work, Some("p"));
            let b = g.insert_fake_in("b", "/r/P", &[Role::Coder], SeatKind::Work, Some("P"));
            let c = g.insert_fake_with("c", "/r", &[Role::Coordinator], SeatKind::Staff);
            (a, b, c)
        };
        let port = ManagerPort::new(Arc::clone(&m), Arc::new(|_, _| {}));
        assert_eq!(port.peers_in_project(&a), ["P"]);
        assert_eq!(port.peers_in_project(&b), ["p"]);
        assert!(port.peers_in_project(&c).is_empty(), "staff: no project");
        assert!(port.peers_in_project("nope").is_empty());
        assert_eq!(port.snapshot(&a).unwrap().project.as_deref(), Some("p"));

        // The host reads the (missing) workspace file: defaults, no projects.
        let t = test_ctx(Arc::clone(&m));
        assert_eq!(t.ctx.rules(), WorkspaceRules::defaults());
        assert!(t.ctx.project_ids().is_empty());
        let root = t.ctx.workspace.root().to_path_buf();
        std::fs::create_dir_all(root.join("beta")).unwrap();
        std::fs::create_dir_all(root.join("Alpha")).unwrap();
        std::fs::write(
            t.ctx.workspace.path(),
            r#"{"userInputGraceMs": 0, "autoReviewOnStop": true}"#,
        )
        .unwrap();
        assert_eq!(t.ctx.project_ids(), ["Alpha", "beta"]);
        let r = t.ctx.rules();
        assert!(r.auto_review_on_stop && r.user_input_grace_ms == 0);
        std::fs::remove_dir_all(&root).unwrap();
    }

    // ---- step 6a: the relation hook in mutate_if (plan A.5) ----

    /// A parent in progress for `k` with one child per `child_agents` entry, each submitted to
    /// review by its agent; then the parent is submitted and waits. Returns (parent, children).
    fn waiting_family(t: &TestCtx, k: &str, child_agents: &[&str]) -> (Ticket, Vec<Ticket>) {
        let c = &t.ctx;
        let p = c.mutate(|s| s.create("Forælder", "", false, 1)).unwrap();
        c.mutate(|s| s.assign(&p.id, k, 2)).unwrap();
        c.mutate(|s| s.mark_dispatched(&p.id, "k", 3)).unwrap();
        let mut kids = Vec::new();
        for (i, a) in child_agents.iter().enumerate() {
            let ch = c
                .mutate(|s| {
                    s.create_by_agent_related(
                        &format!("Del {i}"),
                        "",
                        false,
                        None,
                        None,
                        Some(p.id.clone()),
                        vec![],
                        None,
                        4,
                    )
                })
                .unwrap();
            c.mutate(|s| s.assign(&ch.id, a, 5)).unwrap();
            c.mutate(|s| s.mark_dispatched(&ch.id, a, 6)).unwrap();
            kids.push(c.mutate(|s| s.submit_by_agent(a, None, "klar", 7)).unwrap());
        }
        let p = c
            .mutate(|s| s.submit_by_agent(k, None, "fordelt", 8))
            .unwrap();
        assert_eq!(p.state, TicketState::Waiting);
        (p, kids)
    }

    #[test]
    fn approve_of_child_notifies_parent_assignee() {
        let (m, ids) = manager_with(3);
        let mut t = test_ctx(Arc::clone(&m));
        let (k, a, b) = (ids[0].as_str(), ids[1].as_str(), ids[2].as_str());
        let (p, kids) = waiting_family(&t, k, &[a, b]);
        t.sent();
        // The reviewer's/user's approval of one child: the parent's assignee is told (once).
        t.ctx.mutate(|s| s.approve(&kids[0].id, 10)).unwrap();
        assert_eq!(
            t.sent(),
            vec![DispatchMsg::QueueChanged {
                agent_id: k.to_string()
            }]
        );
        let due = t.ctx.read(|s| s.due_wake_for(k)).expect("wake due");
        assert_eq!(due.parent.id, p.id);
        assert_eq!(due.open_left, 1);
        assert_eq!(due.newly_done.len(), 1);
        // The last child via the user's manual move (Review → Done): told again, nothing open.
        t.ctx
            .mutate(|s| s.set_state(&kids[1].id, TicketState::Done, None, true, 11))
            .unwrap();
        assert_eq!(
            t.sent(),
            vec![DispatchMsg::QueueChanged {
                agent_id: k.to_string()
            }]
        );
        let due = t.ctx.read(|s| s.due_wake_for(k)).unwrap();
        assert_eq!(due.open_left, 0, "the 'aflever' variant");
        assert_eq!(due.newly_done.len(), 2);
        // The parent stays waiting until the dispatcher typed the line (P2).
        assert_eq!(
            t.ctx.read(|s| s.get(&p.id)).unwrap().state,
            TicketState::Waiting
        );
    }

    #[test]
    fn rejected_child_does_not_wake_and_deleted_last_child_does() {
        let (m, ids) = manager_with(2);
        let mut t = test_ctx(Arc::clone(&m));
        let (k, a) = (ids[0].as_str(), ids[1].as_str());
        let (p, kids) = waiting_family(&t, k, &[a]);
        t.sent();
        // Rejected (back to the backlog): still open, nobody woken.
        t.ctx
            .mutate(|s| s.reject(&kids[0].id, "mangler", RejectReturn::Backlog, 10))
            .unwrap();
        assert!(t.sent().is_empty());
        // Deleted: no open child left → the parent's assignee is told.
        t.ctx.delete_ticket(&kids[0].id).unwrap();
        assert_eq!(
            t.sent(),
            vec![DispatchMsg::QueueChanged {
                agent_id: k.to_string()
            }]
        );
        let due = t.ctx.read(|s| s.due_wake_for(k)).unwrap();
        assert_eq!((due.parent.id, due.open_left), (p.id, 0));
        assert!(due.newly_done.is_empty());
    }

    #[test]
    fn unblocked_ticket_notifies_its_agent() {
        let (m, ids) = manager_with(3);
        let mut t = test_ctx(Arc::clone(&m));
        let (x, a, b) = (ids[0].as_str(), ids[1].as_str(), ids[2].as_str());
        let c = &t.ctx;
        let blocker = c.mutate(|s| s.create("Plan", "", false, 1)).unwrap();
        c.mutate(|s| s.assign(&blocker.id, x, 2)).unwrap();
        c.mutate(|s| s.mark_dispatched(&blocker.id, "x", 3))
            .unwrap();
        c.mutate(|s| s.submit_by_agent(x, None, "plan", 4)).unwrap();
        let mk = |agent: &str, title: &str| {
            let q = c
                .mutate(|s| {
                    s.create_by_agent_related(
                        title,
                        "",
                        false,
                        None,
                        None,
                        None,
                        vec![blocker.id.clone()],
                        None,
                        5,
                    )
                })
                .unwrap();
            c.mutate(|s| s.assign(&q.id, agent, 6)).unwrap()
        };
        let qa = mk(a, "Byg");
        // b's ticket is blocked by `blocker` and by another open ticket: stays blocked.
        let other = c.mutate(|s| s.create("Andet", "", false, 7)).unwrap();
        let qb = c
            .mutate(|s| {
                s.create_by_agent_related(
                    "Test",
                    "",
                    false,
                    None,
                    None,
                    None,
                    vec![blocker.id.clone(), other.id.clone()],
                    None,
                    8,
                )
            })
            .unwrap();
        c.mutate(|s| s.assign(&qb.id, b, 9)).unwrap();
        assert_eq!(c.read(|s| s.next_for_agent(a)), None, "blocked");
        t.sent();
        t.ctx.mutate(|s| s.approve(&blocker.id, 10)).unwrap();
        assert_eq!(
            t.sent(),
            vec![DispatchMsg::QueueChanged {
                agent_id: a.to_string()
            }]
        );
        assert_eq!(t.ctx.read(|s| s.next_for_agent(a)).unwrap().id, qa.id);
        assert_eq!(t.ctx.read(|s| s.next_for_agent(b)), None);
        // Deleting the other blocker frees b's ticket.
        t.ctx.delete_ticket(&other.id).unwrap();
        assert_eq!(
            t.sent(),
            vec![DispatchMsg::QueueChanged {
                agent_id: b.to_string()
            }]
        );
        assert_eq!(t.ctx.read(|s| s.next_for_agent(b)).unwrap().id, qb.id);
    }

    #[test]
    fn dead_assignee_parent_goes_to_backlog_with_note() {
        let (m, ids) = manager_with(2);
        let mut t = test_ctx(Arc::clone(&m));
        let (k, a) = (ids[0].as_str(), ids[1].as_str());
        let (p, kids) = waiting_family(&t, k, &[a]);
        // The coordinator stops: the waiting parent goes to the backlog (plan A.1).
        assert_eq!(t.ctx.release_agent(k, AGENT_STOPPED_NOTE), Ok(1));
        let tk = t.ctx.read(|s| s.get(&p.id)).unwrap();
        assert_eq!(
            (tk.state, tk.assignee_agent_id),
            (TicketState::Backlog, None)
        );
        assert_eq!(
            tk.history.last().unwrap().note.as_deref(),
            Some(AGENT_STOPPED_NOTE)
        );
        t.sent();
        t.clear();
        // Its last child is approved: no wake (nobody to wake), a history note instead.
        t.ctx.mutate(|s| s.approve(&kids[0].id, 20)).unwrap();
        assert!(t.sent().is_empty());
        let tk = t.ctx.read(|s| s.get(&p.id)).unwrap();
        let last = tk.history.last().unwrap();
        assert_eq!(
            last.note.as_deref(),
            Some(crate::config::CHILDREN_DONE_NOTE)
        );
        assert_eq!(last.by, TicketActor::System);
        assert_eq!(tk.state, TicketState::Backlog);
        // The approve and the note: two saves, two emits.
        assert_eq!(t.emitted(TICKETS_CHANGED).len(), 2);
        let n = tk.history.len();
        assert_eq!(t.ctx.read(|s| s.get(&p.id)).unwrap().history.len(), n);
    }

    #[test]
    fn last_child_done_sends_flow_parent_to_review() {
        use crate::agent::SeatKind;
        use crate::tickets::model::{TicketSource, TicketState as S};
        use crate::tickets::service::ChildSpec;
        let mut mgr = AgentManager::new(5);
        let a = mgr.insert_fake("s-a", "/w/a");
        let r = mgr.insert_fake_with("s-r", "/w/r", &[Role::Reviewer], SeatKind::Staff);
        let mut t = test_ctx(Arc::new(Mutex::new(mgr)));
        let c = Arc::clone(&t.ctx);
        let p = Some(crate::projects::ProjectRef::Existing("p".into()));
        let parent = c
            .mutate(|s| s.create_in("Login", "", false, p, Some("feature".into()), 1))
            .unwrap();
        let specs: Vec<ChildSpec> = (0..2)
            .map(|i| ChildSpec {
                title: format!("trin {i}"),
                body: String::new(),
                blocked_by_previous: i > 0,
                skip_review: false,
            })
            .collect();
        let kids = c
            .mutate(|s| {
                s.create_playbook_children(
                    &parent.id,
                    &specs,
                    (TicketSource::User, TicketActor::User),
                    2,
                )
            })
            .unwrap();
        let done = |id: &str, now: u64| {
            c.mutate(|s| {
                s.assign(id, &a, now)?;
                s.mark_dispatched(id, &a, now)?;
                s.submit_by_agent(&a, Some(id), "klar", now)?;
                s.approve(id, now)
            })
            .unwrap();
        };
        done(&kids[0].id, 3);
        assert_eq!(c.read(|s| s.get(&parent.id)).unwrap().state, S::Backlog);
        done(&kids[1].id, 4);
        let tk = c.read(|s| s.get(&parent.id)).unwrap();
        assert_eq!((tk.state, tk.assignee_agent_id.clone()), (S::Review, None));
        let last = tk.history.last().unwrap();
        assert_eq!(
            (last.by, last.note.as_deref()),
            (TicketActor::System, Some(crate::config::FLOW_DONE_NOTE))
        );
        // Never routed: no reviewer, no ReviewAssigned, the user decides.
        t.sent();
        assert_eq!(c.route_reviews(), 0);
        assert!(t
            .sent()
            .iter()
            .all(|m| !matches!(m, DispatchMsg::ReviewAssigned { .. })));
        assert_eq!(
            c.read(|s| s.get(&parent.id)).unwrap().reviewer_agent_id,
            None
        );
        assert_eq!(
            c.assign_reviewer(&parent.id, Some(&r)),
            Err(FLOW_REVIEW_IS_USERS.to_string())
        );
        // The user rejects: back to the backlog (nobody to return it to).
        assert_eq!(c.reject_return(&parent.id), RejectReturn::Backlog);
    }

    // ---- step 6b: git per ticket ----

    use crate::git::fake::FakeGit;
    use crate::projects::ProjectRef;

    /// A ctx with a scripted git, the workspace file `json` and the project `proj` (with a
    /// `.git` folder when `repo`).
    fn git_ctx(json: &str, repo: bool) -> (TestCtx, Arc<FakeGit>, PathBuf, Vec<String>) {
        let (m, ids) = manager_with(1);
        let fake = Arc::new(FakeGit::new());
        let t = test_ctx_with_git(m, fake.clone());
        let proj = t.ctx.workspace.root().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        if repo {
            std::fs::create_dir(proj.join(".git")).unwrap();
        }
        std::fs::write(t.ctx.workspace.path(), json).unwrap();
        (t, fake, proj, ids)
    }

    fn ticket_in(t: &TestCtx, project: Option<&str>) -> Ticket {
        let p = project.map(|p| ProjectRef::Existing(p.into()));
        t.ctx
            .mutate(|s| s.create_in("Ret login", "b", false, p, None, 1))
            .unwrap()
    }

    fn cleanup(t: &TestCtx) {
        let _ = std::fs::remove_dir_all(t.ctx.workspace.root());
    }

    #[test]
    fn prepare_git_is_off_by_default_and_needs_a_project() {
        let (t, fake, _proj, _) = git_ctx("{}", true);
        let tk = ticket_in(&t, Some("proj"));
        assert_eq!(t.ctx.prepare_ticket_git(&tk), None);
        std::fs::write(t.ctx.workspace.path(), r#"{"git": "worktree"}"#).unwrap();
        let none = ticket_in(&t, None);
        assert_eq!(t.ctx.prepare_ticket_git(&none), None);
        assert!(fake.calls().is_empty());
        let h = t.ctx.read(|s| s.get(&none.id)).unwrap().history;
        assert_eq!(h.len(), 1, "no note");
        cleanup(&t);
    }

    #[test]
    fn prepare_git_creates_worktree_and_saves_ticket_git() {
        let (t, fake, proj, _) = git_ctx(r#"{"git": "worktree", "gitBase": "develop"}"#, true);
        let tk = ticket_in(&t, Some("PROJ"));
        let short = tk.short_id();
        let g = t.ctx.prepare_ticket_git(&tk).expect("git");
        let wt = crate::git::worktree_dir(&proj, &short);
        assert_eq!(
            g,
            TicketGit {
                mode: GitMode::Worktree,
                branch: format!("ticket/{short}"),
                base: "develop".into(),
                repo: proj.to_string_lossy().into_owned(),
                worktree: Some(wt.to_string_lossy().into_owned()),
            }
        );
        let calls = fake.calls();
        assert_eq!(calls.len(), 4, "{calls:?}");
        assert_eq!(calls[0], ["worktree", "prune"]);
        assert_eq!(
            calls[3],
            [
                "worktree",
                "add",
                wt.to_str().unwrap(),
                "-b",
                &format!("ticket/{short}"),
                "develop"
            ]
        );
        assert!(proj.join(".mira-bots").join(".gitignore").is_file());
        let stored = t.ctx.read(|s| s.get(&tk.id)).unwrap();
        assert_eq!(stored.git.as_ref(), Some(&g));
        // Again (e.g. after a rejection): the stored value, no git call.
        assert_eq!(t.ctx.prepare_ticket_git(&stored), Some(g.clone()));
        assert_eq!(t.ctx.prepare_ticket_git(&tk), Some(g));
        assert_eq!(fake.calls().len(), 4);
        cleanup(&t);
    }

    #[test]
    fn prepare_ticket_git_notes_worktree_once() {
        // Step 6d (A.10): the system notes "worktree oprettet: ticket/<short>" when the
        // worktree is created; a second call (stored value) adds nothing.
        let (t, fake, _proj, _) = git_ctx(r#"{"git": "worktree", "gitBase": "develop"}"#, true);
        let tk = ticket_in(&t, Some("proj"));
        let short = tk.short_id();
        let before = t.ctx.read(|s| s.get(&tk.id)).unwrap().history.len();
        t.ctx.prepare_ticket_git(&tk).expect("git");
        let stored = t.ctx.read(|s| s.get(&tk.id)).unwrap();
        assert_eq!(stored.history.len(), before + 1);
        let last = stored.history.last().unwrap();
        assert_eq!(last.by, TicketActor::System);
        assert_eq!(
            last.note.as_deref(),
            Some(format!("worktree oprettet: ticket/{short}").as_str())
        );
        assert_eq!(
            last.note.as_deref(),
            Some(worktree_created_note(&format!("ticket/{short}")).as_str())
        );
        assert_eq!(
            (last.from, last.to),
            (Some(stored.state), stored.state),
            "state unchanged"
        );
        assert_eq!(t.ctx.prepare_ticket_git(&stored), stored.git);
        assert_eq!(t.ctx.prepare_ticket_git(&tk), stored.git);
        assert_eq!(
            t.ctx.read(|s| s.get(&tk.id)).unwrap().history.len(),
            before + 1,
            "one note only"
        );
        assert_eq!(fake.calls().len(), 4);
        cleanup(&t);
    }

    #[test]
    fn prepare_git_failure_is_a_note_and_the_delivery_goes_on() {
        let (t, fake, _proj, _) = git_ctx(r#"{"git": "worktree"}"#, true);
        fake.reply(&["symbolic-ref"], 128, "", "fatal: not a symbolic ref");
        fake.reply(&["branch", "--show-current"], 0, "main\n", "");
        fake.reply(
            &["worktree", "add"],
            128,
            "",
            "Preparing worktree (new branch 'x')\nfatal: invalid reference: main\n",
        );
        let tk = ticket_in(&t, Some("proj"));
        assert_eq!(t.ctx.prepare_ticket_git(&tk), None);
        let stored = t.ctx.read(|s| s.get(&tk.id)).unwrap();
        assert_eq!(stored.git, None);
        let last = stored.history.last().unwrap();
        assert_eq!(
            (last.note.as_deref(), last.by, last.to),
            (
                Some("git: fatal: invalid reference: main"),
                TicketActor::System,
                TicketState::Backlog
            )
        );
        // The same failure again: no duplicate note.
        assert_eq!(t.ctx.prepare_ticket_git(&tk), None);
        let n = t.ctx.read(|s| s.get(&tk.id)).unwrap().history.len();
        assert_eq!(n, stored.history.len());

        // git missing.
        let (t2, fake2, _, _) = git_ctx(r#"{"git": "worktree"}"#, true);
        fake2.fail(&[], crate::git::GIT_NOT_FOUND_NOTE);
        let tk2 = ticket_in(&t2, Some("proj"));
        assert_eq!(t2.ctx.prepare_ticket_git(&tk2), None);
        let h = t2.ctx.read(|s| s.get(&tk2.id)).unwrap().history;
        assert_eq!(
            h.last().unwrap().note.as_deref(),
            Some("git ikke fundet — git: off")
        );
        cleanup(&t);
        cleanup(&t2);
    }

    #[test]
    fn prepare_git_for_a_project_without_repo_is_a_note() {
        let (t, fake, _proj, _) = git_ctx(r#"{"git": "worktree"}"#, false);
        let tk = ticket_in(&t, Some("proj"));
        assert_eq!(t.ctx.prepare_ticket_git(&tk), None);
        assert!(fake.calls().is_empty());
        let h = t.ctx.read(|s| s.get(&tk.id)).unwrap().history;
        assert_eq!(
            h.last().unwrap().note.as_deref(),
            Some("git: projektet «proj» er ikke et git-repo; ingen branch")
        );
        // Branch mode is deferred: off (the workspace note says so), nothing happens.
        std::fs::write(t.ctx.workspace.path(), r#"{"git": "branch"}"#).unwrap();
        let tk2 = ticket_in(&t, Some("proj"));
        assert_eq!(t.ctx.prepare_ticket_git(&tk2), None);
        assert_eq!(t.ctx.read(|s| s.get(&tk2.id)).unwrap().history.len(), 1);
        cleanup(&t);
    }

    #[test]
    fn route_reviews_attaches_changes_report_once() {
        let (t, fake, proj, ids) = git_ctx(r#"{"git": "worktree"}"#, true);
        fake.reply(
            &["diff"],
            0,
            " a.txt | 1 +\n 1 file changed, 1 insertion(+)\n",
            "",
        );
        fake.reply(&["log"], 0, "abc1234 Ret login\n", "");
        fake.reply(&["status"], 0, "?? notes.txt\n?? .mira-bots/\n", "");
        let a = &ids[0];
        let tk = t
            .ctx
            .mutate(|s| {
                s.create_in(
                    "Ret",
                    "b",
                    false,
                    Some(ProjectRef::Existing("proj".into())),
                    None,
                    1,
                )
            })
            .unwrap();
        let g = t.ctx.prepare_ticket_git(&tk).unwrap();
        let before = fake.calls().len();
        t.ctx.mutate(|s| s.assign(&tk.id, a, 2)).unwrap();
        t.ctx.mutate(|s| s.mark_dispatched(&tk.id, a, 3)).unwrap();
        t.ctx
            .mutate(|s| s.submit_by_agent(a, None, "klar", 4))
            .unwrap();
        // No reviewer: nothing routed, but the report is there (author: the app).
        assert_eq!(t.ctx.route_reviews(), 0);
        let stored = t.ctx.read(|s| s.get(&tk.id)).unwrap();
        assert_eq!(stored.reports.len(), 1);
        let r = &stored.reports[0];
        assert_eq!(
            (r.title.as_str(), r.author.clone()),
            ("Ændringer", ReportAuthor::system())
        );
        let body = t.ctx.get_report(&tk.id, &r.id).unwrap().body;
        assert_eq!(
            body,
            format!(
                "Branch {} fra {}\n\n## Commits (1)\n- abc1234 Ret login\n\n## Ændrede filer\n a.txt | 1 +\n 1 file changed, 1 insertion(+)\n\n## Ikke committet\n1 fil(er) i arbejdsmappen er ikke committet (de indgår ikke i diffen)",
                g.branch, g.base
            )
        );
        let calls = fake.calls_in();
        assert_eq!(calls.len(), before + 3);
        assert_eq!(
            calls[before + 2].0,
            PathBuf::from(g.worktree.clone().unwrap())
        );
        // Routing again: no second report, no git.
        t.ctx.route_reviews();
        assert_eq!(t.ctx.read(|s| s.get(&tk.id)).unwrap().reports.len(), 1);
        assert_eq!(fake.calls().len(), before + 3);

        // Rejected and submitted again (later than the first report): a new review entry gets
        // a new report.
        let later = now_ms() + 1_000;
        t.ctx
            .mutate(|s| s.reject(&tk.id, "mere", RejectReturn::Sender, later))
            .unwrap();
        t.ctx
            .mutate(|s| s.mark_dispatched(&tk.id, a, later + 1))
            .unwrap();
        t.ctx
            .mutate(|s| s.submit_by_agent(a, None, "igen", later + 2))
            .unwrap();
        t.ctx.route_reviews();
        assert_eq!(t.ctx.read(|s| s.get(&tk.id)).unwrap().reports.len(), 2);
        assert!(proj.join(".mira-bots").join(".gitignore").is_file());
        cleanup(&t);
    }

    #[test]
    fn changes_report_failure_is_noted_once_per_review_entry() {
        let (t, fake, _proj, ids) = git_ctx(r#"{"git": "worktree"}"#, true);
        fake.reply(&["diff"], 128, "", "fatal: bad revision");
        let a = &ids[0];
        let tk = ticket_in(&t, Some("proj"));
        t.ctx.prepare_ticket_git(&tk).unwrap();
        t.ctx.mutate(|s| s.assign(&tk.id, a, 2)).unwrap();
        t.ctx.mutate(|s| s.mark_dispatched(&tk.id, a, 3)).unwrap();
        t.ctx
            .mutate(|s| s.submit_by_agent(a, None, "klar", 4))
            .unwrap();
        let before = fake.calls().len();
        assert!(!t.ctx.attach_changes(&tk.id));
        let after_first = fake.calls().len();
        assert!(after_first > before, "git was asked once");
        // Review6b W4: no second git run for the same review entry (routing, a direct call).
        assert_eq!(t.ctx.route_reviews(), 0);
        assert!(!t.ctx.attach_changes(&tk.id));
        assert_eq!(fake.calls().len(), after_first);
        let stored = t.ctx.read(|s| s.get(&tk.id)).unwrap();
        assert!(stored.reports.is_empty());
        assert_eq!(stored.state, TicketState::Review);
        // One note by the app says why.
        let notes: Vec<_> = stored
            .history
            .iter()
            .filter(|h| {
                h.by == TicketActor::System
                    && h.note
                        .as_deref()
                        .is_some_and(|n| n.starts_with("Ændringer kunne ikke læses: "))
            })
            .collect();
        assert_eq!(notes.len(), 1, "{:?}", stored.history);
        assert!(!notes[0].note.as_deref().unwrap().contains('\n'));
        // A new review entry tries again.
        let later = now_ms() + 1_000;
        t.ctx
            .mutate(|s| s.reject(&tk.id, "mere", RejectReturn::Sender, later))
            .unwrap();
        t.ctx
            .mutate(|s| s.mark_dispatched(&tk.id, a, later + 1))
            .unwrap();
        t.ctx
            .mutate(|s| s.submit_by_agent(a, None, "igen", later + 2))
            .unwrap();
        assert!(!t.ctx.attach_changes(&tk.id));
        assert!(fake.calls().len() > after_first);
        // A ticket without git never gets the report.
        let plain = submitted(&t, a, "uden git");
        assert!(!t.ctx.attach_changes(&plain));
        cleanup(&t);
    }

    // ---- step 6b: project checks and the gate (plan6b punkt 15) ----

    use crate::checks::fake::FakeChecks;
    use crate::checks::RunOutcome;

    struct ChecksSetup {
        t: TestCtx,
        fake: Arc<FakeChecks>,
        proj: PathBuf,
        coder: String,
        reviewer: String,
    }

    /// Coder a1 (work seat in `proj`), reviewer r1 (staff); workspace file `ws`; the project
    /// `proj` with `project` as its project.json (none when `None`).
    fn checks_setup(ws: &str, project: Option<&str>) -> ChecksSetup {
        let mut m = AgentManager::new(5);
        let coder = m.insert_fake_in(
            "s-a1",
            "/w/a1",
            &[Role::Coder],
            SeatKind::Work,
            Some("proj"),
        );
        let reviewer = m.insert_fake_with("s-r1", "/w/r1", &[Role::Reviewer], SeatKind::Staff);
        let fake = Arc::new(FakeChecks::new());
        let t = test_ctx_with(
            Arc::new(Mutex::new(m)),
            Arc::new(FakeGit::new()),
            fake.clone(),
        );
        let proj = t.ctx.workspace.root().join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(t.ctx.workspace.path(), ws).unwrap();
        if let Some(text) = project {
            let f = crate::checks::project_file_path(&proj);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, text).unwrap();
        }
        ChecksSetup {
            t,
            fake,
            proj,
            coder,
            reviewer,
        }
    }

    const TWO_CHECKS: &str = r#"{"checks": [{"name": "tests", "run": "npm test"}, {"name": "lint", "run": "npm run lint"}]}"#;

    /// A ticket in `proj` submitted by the coder (in review, unrouted).
    fn submit_in_proj(c: &ChecksSetup) -> Ticket {
        let ctx = &c.t.ctx;
        let p = Some(ProjectRef::Existing("proj".into()));
        let tk = ctx
            .mutate(|s| s.create_in("Ret login", "b", false, p, None, 1))
            .unwrap();
        ctx.mutate(|s| s.assign(&tk.id, &c.coder, 2)).unwrap();
        ctx.mutate(|s| s.mark_dispatched(&tk.id, &c.coder, 3))
            .unwrap();
        ctx.mutate(|s| s.submit_by_agent(&c.coder, None, "klar", 4))
            .unwrap()
    }

    fn get(c: &ChecksSetup, id: &str) -> Ticket {
        c.t.ctx.read(|s| s.get(id)).unwrap()
    }

    fn checks_state(c: &ChecksSetup, id: &str) -> Option<ChecksState> {
        get(c, id).checks.map(|k| k.state)
    }

    fn review_assigned(msgs: &[DispatchMsg]) -> usize {
        msgs.iter()
            .filter(|m| matches!(m, DispatchMsg::ReviewAssigned { .. }))
            .count()
    }

    #[test]
    fn review_without_project_file_is_skipped_and_routed() {
        let mut c = checks_setup("{}", None);
        let tk = submit_in_proj(&c);
        assert_eq!(c.t.ctx.route_reviews(), 1);
        c.t.ctx.join_checks();
        let t = get(&c, &tk.id);
        assert_eq!(t.reviewer_agent_id.as_deref(), Some(c.reviewer.as_str()));
        assert_eq!(t.checks.map(|k| k.state), Some(ChecksState::Skipped));
        assert!(t.reports.is_empty());
        assert!(c.fake.calls().is_empty());
        assert_eq!(review_assigned(&c.t.sent()), 1);

        // An empty list: skipped too. An unreadable file: a report by the app, skipped, routed.
        for (text, report) in [(r#"{"checks": []}"#, false), (r#"{"checks": 3}"#, true)] {
            let c = checks_setup("{}", Some(text));
            let tk = submit_in_proj(&c);
            assert_eq!(c.t.ctx.route_reviews(), 1, "{text}");
            let t = get(&c, &tk.id);
            assert_eq!(t.checks.map(|k| k.state), Some(ChecksState::Skipped));
            assert_eq!(t.reports.len(), usize::from(report));
            if report {
                let r = &t.reports[0];
                assert_eq!(
                    (r.title.as_str(), r.author.clone()),
                    (
                        "Tjek: project.json kunne ikke læses",
                        ReportAuthor::system()
                    )
                );
                let body = c.t.ctx.get_report(&tk.id, &r.id).unwrap().body;
                assert_eq!(body, "project.json: checks skal være en liste");
            }
            assert!(c.fake.calls().is_empty());
            cleanup(&c.t);
        }
        // A ticket without a project is not checked at all.
        let plain = submitted(&c.t, &c.coder, "uden projekt");
        c.t.ctx.route_reviews();
        assert_eq!(checks_state(&c, &plain), None);
        cleanup(&c.t);
    }

    #[test]
    fn review_with_checks_waits_for_pending_then_routes_on_pass() {
        let mut c = checks_setup("{}", Some(TWO_CHECKS));
        let tk = submit_in_proj(&c);
        c.fake.hold();
        assert_eq!(c.t.ctx.route_reviews(), 0, "waits for the checks");
        let pending = get(&c, &tk.id).checks.unwrap();
        assert_eq!((pending.state, pending.round), (ChecksState::Pending, 0));
        assert_eq!(get(&c, &tk.id).reviewer_agent_id, None);
        // Routing again while they run: still waiting, not started twice.
        assert_eq!(c.t.ctx.route_reviews(), 0);
        assert_eq!(get(&c, &tk.id).checks, Some(pending));
        assert_eq!(review_assigned(&c.t.sent()), 0);
        c.fake.release();
        c.t.ctx.join_checks();
        // Both ran, in order, in the project folder (no worktree).
        assert_eq!(
            c.fake.calls(),
            vec![
                ("tests".to_string(), c.proj.clone()),
                ("lint".to_string(), c.proj.clone())
            ]
        );
        let t = get(&c, &tk.id);
        assert_eq!(t.state, TicketState::Review);
        assert_eq!(
            t.checks.as_ref().map(|k| k.state),
            Some(ChecksState::Passed)
        );
        assert_eq!(t.reviewer_agent_id.as_deref(), Some(c.reviewer.as_str()));
        assert_eq!(t.reports.len(), 1);
        let r = &t.reports[0];
        assert_eq!(
            (r.title.as_str(), r.author.clone()),
            ("Tjek: OK", ReportAuthor::system())
        );
        let body = c.t.ctx.get_report(&tk.id, &r.id).unwrap().body;
        assert_eq!(
            body,
            "Tjek: tests → OK (exit 0, 1 s)\nTjek: lint → OK (exit 0, 1 s)"
        );
        assert_eq!(review_assigned(&c.t.sent()), 1);
        cleanup(&c.t);
    }

    #[test]
    fn a_panicking_checks_thread_skips_the_run_and_routes() {
        // Review6b N20: the ticket must never stay pending.
        let mut c = checks_setup("{}", Some(TWO_CHECKS));
        let tk = submit_in_proj(&c);
        c.fake.panic();
        assert_eq!(c.t.ctx.route_reviews(), 0, "waits for the checks");
        c.t.ctx.join_checks();
        let t = get(&c, &tk.id);
        assert_eq!(t.state, TicketState::Review, "never sent back");
        assert_eq!(
            t.checks.as_ref().map(|k| k.state),
            Some(ChecksState::Skipped)
        );
        // Noted once by the app (before routing adds its own entry).
        let notes: Vec<_> = t
            .history
            .iter()
            .filter(|h| h.note.as_deref() == Some(CHECKS_ABORTED_NOTE))
            .collect();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].by, TicketActor::System);
        assert!(t.reports.is_empty(), "no «Tjek» report");
        assert_eq!(t.reviewer_agent_id.as_deref(), Some(c.reviewer.as_str()));
        assert_eq!(review_assigned(&c.t.sent()), 1);
        cleanup(&c.t);
    }

    #[test]
    fn checks_run_in_the_ticket_worktree() {
        let c = checks_setup("{}", Some(TWO_CHECKS));
        let tk = submit_in_proj(&c);
        let wt = c.proj.join(".mira-bots").join("wt").join(tk.short_id());
        std::fs::create_dir_all(&wt).unwrap();
        let g = TicketGit {
            mode: GitMode::Worktree,
            branch: format!("ticket/{}", tk.short_id()),
            base: "main".into(),
            repo: c.proj.to_string_lossy().into_owned(),
            worktree: Some(wt.to_string_lossy().into_owned()),
        };
        c.t.ctx.mutate(|s| s.set_git(&tk.id, Some(g), 5)).unwrap();
        c.t.ctx.route_reviews();
        c.t.ctx.join_checks();
        assert!(
            c.fake.calls().iter().all(|(_, d)| d == &wt),
            "{:?}",
            c.fake.calls()
        );
        assert_eq!(c.fake.calls().len(), 2);
        cleanup(&c.t);
    }

    #[test]
    fn failed_check_rejects_by_system_with_report_reference_and_round_plus_one() {
        let mut c = checks_setup("{}", Some(TWO_CHECKS));
        c.fake.exit("tests", 1, "FAIL src/login.test.ts\n");
        let tk = submit_in_proj(&c);
        c.t.sent();
        assert_eq!(c.t.ctx.route_reviews(), 0);
        c.t.ctx.join_checks();
        let t = get(&c, &tk.id);
        // Back first in the sender's queue, rejected by the app; the round counts.
        assert_eq!(t.state, TicketState::Assigned);
        assert_eq!(t.assignee_agent_id.as_deref(), Some(c.coder.as_str()));
        assert_eq!(t.queue_position, Some(0));
        assert_eq!(t.review_round, 1);
        assert_eq!(t.reviewer_agent_id, None);
        let note = "Tjek fejlede: tests (exit 1). Se rapport 01.";
        assert_eq!(t.rejection_note.as_deref(), Some(note));
        let h = t
            .history
            .iter()
            .rev()
            .find(|h| h.to == TicketState::Rejected)
            .unwrap();
        assert_eq!(h.by, TicketActor::System);
        assert_eq!(
            h.note.as_deref(),
            Some(format!("afvist af appen: {note}").as_str())
        );
        let k = t.checks.clone().unwrap();
        assert_eq!(
            (k.state, k.failed.as_deref()),
            (ChecksState::Failed, Some("tests"))
        );
        // The report: id 01, by the app, the failing output.
        let r = &t.reports[0];
        assert_eq!(
            (r.id.as_str(), r.title.as_str(), r.author.clone()),
            ("01", "Tjek: FEJL (tests)", ReportAuthor::system())
        );
        let body = c.t.ctx.get_report(&tk.id, "01").unwrap().body;
        assert_eq!(
            body,
            "Tjek: tests → FEJL (exit 1, 2 s)\nTjek: lint → OK (exit 0, 1 s)\n\n--- tests (sidste 22 tegn) ---\nFAIL src/login.test.ts"
        );
        // The sender is told; nobody reviews.
        let sent = c.t.sent();
        assert_eq!(review_assigned(&sent), 0);
        assert!(sent
            .iter()
            .any(|m| matches!(m, DispatchMsg::QueueChanged { agent_id } if agent_id == &c.coder)));

        // Submitted again: checked anew (the round is 1 now).
        c.fake.exit("tests", 0, "");
        c.t.ctx
            .mutate(|s| s.mark_dispatched(&tk.id, &c.coder, 10))
            .unwrap();
        c.t.ctx
            .mutate(|s| s.submit_by_agent(&c.coder, None, "rettet", 11))
            .unwrap();
        assert_eq!(get(&c, &tk.id).checks, None, "Submit resets");
        c.t.ctx.route_reviews();
        c.t.ctx.join_checks();
        let t = get(&c, &tk.id);
        assert_eq!(
            t.checks.map(|k| (k.state, k.round)),
            Some((ChecksState::Passed, 1))
        );
        assert_eq!(t.reviewer_agent_id.as_deref(), Some(c.reviewer.as_str()));
        cleanup(&c.t);
    }

    #[test]
    fn failed_check_round_counts_towards_max_and_escalates() {
        let c = checks_setup(r#"{"maxReviewRounds": 1}"#, Some(TWO_CHECKS));
        c.fake.outcome(
            "lint",
            RunOutcome::TimedOut {
                output: String::new(),
                clipped: false,
                elapsed_ms: 600_000,
            },
        );
        let tk = submit_in_proj(&c);
        c.t.ctx.route_reviews();
        c.t.ctx.join_checks();
        let t = get(&c, &tk.id);
        assert_eq!(
            t.rejection_note.as_deref(),
            Some("Tjek fejlede: lint (timeout). Se rapport 01.")
        );
        assert_eq!(t.review_round, 1);
        // Back in review at the maximum: escalated, no new checks.
        c.t.ctx
            .mutate(|s| s.mark_dispatched(&tk.id, &c.coder, 10))
            .unwrap();
        c.t.ctx
            .mutate(|s| s.submit_by_agent(&c.coder, None, "igen", 11))
            .unwrap();
        let calls = c.fake.calls().len();
        assert_eq!(c.t.ctx.route_reviews(), 0);
        c.t.ctx.join_checks();
        let t = get(&c, &tk.id);
        assert!(t.escalated);
        assert_eq!(t.checks, None);
        assert_eq!(c.fake.calls().len(), calls);
        cleanup(&c.t);
    }

    #[test]
    fn gate_off_routes_with_failed_badge() {
        let mut c = checks_setup(r#"{"checksGate": false}"#, Some(TWO_CHECKS));
        c.fake.exit("lint", 2, "error");
        let tk = submit_in_proj(&c);
        c.fake.hold();
        assert_eq!(c.t.ctx.route_reviews(), 1, "routed while the checks run");
        assert_eq!(checks_state(&c, &tk.id), Some(ChecksState::Pending));
        c.fake.release();
        c.t.ctx.join_checks();
        let t = get(&c, &tk.id);
        assert_eq!(t.state, TicketState::Review);
        assert_eq!(t.reviewer_agent_id.as_deref(), Some(c.reviewer.as_str()));
        assert_eq!(t.review_round, 0);
        assert_eq!(t.rejection_note, None);
        assert_eq!(t.checks.map(|k| k.state), Some(ChecksState::Failed));
        assert_eq!(t.reports[0].title, "Tjek: FEJL (lint)");
        assert_eq!(review_assigned(&c.t.sent()), 1);
        cleanup(&c.t);
    }

    #[test]
    fn escalated_ticket_runs_no_gate() {
        let c = checks_setup(r#"{"maxReviewRounds": 1}"#, Some(TWO_CHECKS));
        let tk = submit_in_proj(&c);
        // Rejected once by the user: round 1 = the maximum.
        c.t.ctx
            .mutate(|s| s.reject(&tk.id, "nej", RejectReturn::Sender, 5))
            .unwrap();
        c.t.ctx
            .mutate(|s| s.mark_dispatched(&tk.id, &c.coder, 6))
            .unwrap();
        c.t.ctx
            .mutate(|s| s.submit_by_agent(&c.coder, None, "igen", 7))
            .unwrap();
        assert_eq!(c.t.ctx.route_reviews(), 0);
        c.t.ctx.join_checks();
        let t = get(&c, &tk.id);
        assert!(t.escalated);
        assert_eq!(t.checks, None);
        assert!(t.reports.is_empty());
        assert!(c.fake.calls().is_empty());
        cleanup(&c.t);
    }

    #[test]
    fn stale_check_result_is_dropped() {
        let c = checks_setup("{}", Some(TWO_CHECKS));
        c.fake.exit("tests", 1, "boom");
        let tk = submit_in_proj(&c);
        c.fake.hold();
        c.t.ctx.route_reviews();
        assert_eq!(checks_state(&c, &tk.id), Some(ChecksState::Pending));
        // The user approves while the checks run.
        c.t.ctx.mutate(|s| s.approve(&tk.id, 8)).unwrap();
        c.fake.release();
        c.t.ctx.join_checks();
        let t = get(&c, &tk.id);
        assert_eq!(t.state, TicketState::Done);
        assert!(t.reports.is_empty(), "no report for a stale run");
        assert_eq!(t.checks, None, "no running badge left behind");
        assert_eq!(t.rejection_note, None);

        // A result for another run (e.g. from before a restart) is dropped too.
        let tk2 = submit_in_proj(&c);
        c.t.ctx.mutate(|s| s.start_checks(&tk2.id, 0, 42)).unwrap();
        let report = crate::checks::run_checks(
            c.fake.as_ref(),
            &[crate::checks::Check {
                name: "tests".into(),
                run: "x".into(),
                timeout_sec: 1,
            }],
            &c.proj,
        );
        c.t.ctx.finish_checks(&tk2.id, 0, 41, &report);
        let t2 = get(&c, &tk2.id);
        assert_eq!(t2.state, TicketState::Review);
        assert_eq!(
            t2.checks.map(|k| (k.state, k.started_at)),
            Some((ChecksState::Pending, 42))
        );
        assert!(t2.reports.is_empty());
        // The matching run is taken.
        c.t.ctx.finish_checks(&tk2.id, 0, 42, &report);
        let t2 = get(&c, &tk2.id);
        assert_eq!(t2.state, TicketState::Assigned);
        assert_eq!(
            t2.rejection_note.as_deref(),
            Some("Tjek fejlede: tests (exit 1). Se rapport 01.")
        );
        cleanup(&c.t);
    }

    #[test]
    fn recover_resets_pending_checks_and_routing_restarts_them() {
        let c = checks_setup("{}", Some(TWO_CHECKS));
        let tk = submit_in_proj(&c);
        c.t.ctx.mutate(|s| s.start_checks(&tk.id, 0, 5)).unwrap();
        // A restart: the stored document is loaded again.
        let doc = c.t.store.doc().unwrap();
        let m = crate::tickets::store::MemoryStore::with_doc(doc);
        let (svc, _) = TicketService::load_and_recover(Box::new(m), 100);
        *lock(&c.t.ctx.service) = svc;
        let t = get(&c, &tk.id);
        assert_eq!(t.checks, None);
        assert_eq!(
            t.history.last().unwrap().note.as_deref(),
            Some(crate::config::CHECKS_INTERRUPTED_NOTE)
        );
        c.t.ctx.route_reviews();
        c.t.ctx.join_checks();
        assert_eq!(checks_state(&c, &tk.id), Some(ChecksState::Passed));
        assert_eq!(c.fake.calls().len(), 2);
        cleanup(&c.t);
    }

    #[test]
    fn project_checks_and_git_base_come_from_project_json() {
        let c = checks_setup(
            r#"{"git": "worktree", "gitBase": "develop"}"#,
            Some(r#"{"checks": [{"name": "tests", "run": "cargo test"}], "gitBase": "release"}"#),
        );
        assert_eq!(
            c.t.ctx.project_checks(Some("proj")),
            vec!["tests: `cargo test`"]
        );
        assert!(c.t.ctx.project_checks(Some("nope")).is_empty());
        assert!(c.t.ctx.project_checks(None).is_empty());
        // project.json's gitBase wins over the workspace's.
        std::fs::create_dir(c.proj.join(".git")).unwrap();
        let tk = ticket_in(&c.t, Some("proj"));
        let g = c.t.ctx.prepare_ticket_git(&tk).unwrap();
        assert_eq!(g.base, "release");
        // Without one in project.json: the workspace's.
        let f = crate::checks::project_file_path(&c.proj);
        std::fs::write(&f, r#"{"checks": []}"#).unwrap();
        let tk2 = ticket_in(&c.t, Some("proj"));
        assert_eq!(c.t.ctx.prepare_ticket_git(&tk2).unwrap().base, "develop");
        cleanup(&c.t);
    }
}

/// Trin 6d (plan punkt 14): beskederne som `mutate_if` og afslutningsvejen giver.
#[cfg(test)]
mod notice_tests {
    use super::test_support::*;
    use super::*;
    use crate::agent::SeatKind;
    use crate::events::NOTICES_CHANGED;
    use crate::notices::{Notice, NoticeKind};
    use crate::tickets::model::{TicketSource, WriteBack, WriteBackState};
    use crate::tickets::service::ChildSpec;

    fn notices(t: &TestCtx) -> Vec<Notice> {
        t.ctx.notices.payload().items
    }

    fn of_kind(t: &TestCtx, kind: NoticeKind) -> Vec<Notice> {
        notices(t).into_iter().filter(|n| n.kind == kind).collect()
    }

    /// A reviewer setup with `maxReviewRounds: 1` in the workspace file.
    fn one_round() -> (TestCtx, String, String) {
        let mut m = AgentManager::new(5);
        let a1 = m.insert_fake_with("s-a1", "/w/a1", &[Role::Coder], SeatKind::Work);
        let r1 = m.insert_fake_with("s-r1", "/w/r1", &[Role::Reviewer], SeatKind::Staff);
        let t = test_ctx(Arc::new(Mutex::new(m)));
        let path = t.ctx.workspace.path().to_path_buf();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"maxReviewRounds": 1}"#).unwrap();
        (t, a1, r1)
    }

    fn cleanup(t: &TestCtx) {
        let path = t.ctx.workspace.path().to_path_buf();
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// Rejected by its reviewer, delivered again and submitted again by `agent`.
    fn reject_and_resubmit(t: &TestCtx, id: &str, agent: &str) {
        let rev = t
            .ctx
            .read(|s| s.get(id))
            .unwrap()
            .reviewer_agent_id
            .unwrap();
        t.ctx
            .mutate(|s| s.reject_by_agent(&rev, "r", id, "mere", RejectReturn::Sender, 10))
            .unwrap();
        t.ctx.mutate(|s| s.mark_dispatched(id, agent, 11)).unwrap();
        t.ctx
            .mutate(|s| s.submit_by_agent(agent, None, "igen", 12))
            .unwrap();
    }

    #[test]
    fn escalation_emits_notices_changed_once() {
        let (t, a1, r1) = one_round();
        let c = &t.ctx;
        let tk = c
            .mutate(|s| s.create("Fejl i login", "hemmelig body", false, 1))
            .unwrap();
        c.mutate(|s| s.assign(&tk.id, &a1, 2)).unwrap();
        c.mutate(|s| s.mark_dispatched(&tk.id, &a1, 3)).unwrap();
        c.mutate(|s| s.submit_by_agent(&a1, None, "klar", 4))
            .unwrap();
        assert_eq!(c.route_reviews(), 1);
        reject_and_resubmit(&t, &tk.id, &a1);
        assert!(notices(&t).is_empty());
        t.clear();
        assert_eq!(c.route_reviews(), 0, "escalated, not routed");
        let esc = of_kind(&t, NoticeKind::Escalated);
        assert_eq!(esc.len(), 1);
        assert_eq!(
            esc[0].text,
            format!("{}: «Fejl i login» efter 1 runder", tk.short_id())
        );
        assert!(!esc[0].text.contains("hemmelig"), "never the body");
        assert_eq!(esc[0].ticket_id.as_deref(), Some(tk.id.as_str()));
        let emitted = t.emitted(NOTICES_CHANGED);
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0]["unread"], 1);
        assert_eq!(emitted[0]["items"][0]["kind"], "escalated");
        // Ten mutations that leave the escalation alone: no new notice, no emit.
        t.clear();
        for i in 0..10 {
            c.mutate(|s| s.create(&format!("andet {i}"), "", false, 20 + i))
                .unwrap();
        }
        c.mutate(|s| s.note_by_system(&tk.id, "en note", 40))
            .unwrap();
        assert_eq!(c.route_reviews(), 0);
        assert!(t.emitted(NOTICES_CHANGED).is_empty());
        assert_eq!(notices(&t).len(), 1);
        // "Fjern reviewer" escalates again in the same round: the same key, nothing new.
        c.assign_reviewer(&tk.id, Some(&r1)).unwrap();
        c.assign_reviewer(&tk.id, None).unwrap();
        assert!(c.read(|s| s.get(&tk.id)).unwrap().escalated);
        assert_eq!(of_kind(&t, NoticeKind::Escalated).len(), 1);
        // A new round (the user's reviewer rejects, the agent submits again): one new notice.
        c.assign_reviewer(&tk.id, Some(&r1)).unwrap();
        reject_and_resubmit(&t, &tk.id, &a1);
        assert_eq!(c.route_reviews(), 0);
        let tk2 = c.read(|s| s.get(&tk.id)).unwrap();
        assert_eq!((tk2.escalated, tk2.review_round), (true, 2));
        let esc = of_kind(&t, NoticeKind::Escalated);
        assert_eq!(esc.len(), 2);
        assert!(esc[0].text.ends_with("efter 2 runder"), "{}", esc[0].text);
        cleanup(&t);
    }

    #[test]
    fn flow_parent_review_emits_notice() {
        let mut mgr = AgentManager::new(5);
        let a = mgr.insert_fake("s-a", "/w/a");
        let t = test_ctx(Arc::new(Mutex::new(mgr)));
        let c = Arc::clone(&t.ctx);
        let p = Some(crate::projects::ProjectRef::Existing("web".into()));
        let parent = c
            .mutate(|s| s.create_in("Login", "", false, p, Some("feature".into()), 1))
            .unwrap();
        let spec = ChildSpec {
            title: "trin".into(),
            body: String::new(),
            blocked_by_previous: false,
            skip_review: false,
        };
        let kids = c
            .mutate(|s| {
                s.create_playbook_children(
                    &parent.id,
                    &[spec],
                    (TicketSource::User, TicketActor::User),
                    2,
                )
            })
            .unwrap();
        // The child in review (assigned): no flow notice.
        c.mutate(|s| {
            s.assign(&kids[0].id, &a, 3)?;
            s.mark_dispatched(&kids[0].id, &a, 3)?;
            s.submit_by_agent(&a, Some(&kids[0].id), "klar", 3)
        })
        .unwrap();
        assert!(of_kind(&t, NoticeKind::FlowReview).is_empty());
        t.clear();
        c.mutate(|s| s.approve(&kids[0].id, 4)).unwrap();
        let tk = c.read(|s| s.get(&parent.id)).unwrap();
        assert_eq!(
            (tk.state, tk.assignee_agent_id),
            (TicketState::Review, None)
        );
        let flow = of_kind(&t, NoticeKind::FlowReview);
        assert_eq!(flow.len(), 1);
        assert_eq!(flow[0].text, format!("{}: «Login»", parent.short_id()));
        assert_eq!(
            (flow[0].ticket_id.as_deref(), flow[0].project.as_deref()),
            (Some(parent.id.as_str()), Some("web"))
        );
        assert_eq!(t.emitted(NOTICES_CHANGED).len(), 1);
        // Further mutations while it waits for the user: nothing new.
        c.mutate(|s| s.note_by_system(&parent.id, "note", 5))
            .unwrap();
        assert_eq!(of_kind(&t, NoticeKind::FlowReview).len(), 1);
    }

    fn external(t: &TestCtx) -> Ticket {
        let e = crate::tickets::model::test_support::github_ref(7);
        t.ctx
            .mutate(|s| s.create_external("Crash", "ekstern body", false, None, None, e, 1))
            .unwrap()
    }

    fn failed(attempts: u32, err: &str) -> WriteBack {
        WriteBack {
            comment: WriteBackState::Failed,
            attempts,
            last_error: Some(err.into()),
            last_body: Some("ekstern tekst".into()),
            ..WriteBack::default()
        }
    }

    #[test]
    fn write_back_failed_emits_notice() {
        let t = test_ctx(Arc::new(Mutex::new(AgentManager::new(5))));
        let tk = external(&t);
        t.clear();
        t.ctx
            .mutate(|s| s.set_write_back(&tk.id, failed(1, "ingen forbindelse til GitHub"), 2))
            .unwrap();
        let wb = of_kind(&t, NoticeKind::WriteBackFailed);
        assert_eq!(wb.len(), 1);
        assert_eq!(
            wb[0].text,
            format!("{}: ingen forbindelse til GitHub", tk.short_id())
        );
        assert!(
            !wb[0].text.contains("ekstern"),
            "never the body or the external text"
        );
        assert_eq!(t.emitted(NOTICES_CHANGED).len(), 1);
        // "Prøv igen": in flight (attempt 2), failed again → a second notice.
        let inflight = WriteBack {
            comment: WriteBackState::Inflight,
            attempts: 2,
            ..WriteBack::default()
        };
        t.ctx
            .mutate(|s| s.set_write_back(&tk.id, inflight, 3))
            .unwrap();
        t.ctx
            .mutate(|s| s.set_write_back(&tk.id, failed(2, "GitHub: rate limit"), 4))
            .unwrap();
        assert_eq!(of_kind(&t, NoticeKind::WriteBackFailed).len(), 2);
        assert_eq!(t.emitted(NOTICES_CHANGED).len(), 2);
    }

    #[test]
    fn write_back_without_external_change_emits_nothing() {
        let t = test_ctx(Arc::new(Mutex::new(AgentManager::new(5))));
        let tk = external(&t);
        let wb = failed(1, "ingen forbindelse til GitHub");
        t.ctx
            .mutate(|s| s.set_write_back(&tk.id, wb.clone(), 2))
            .unwrap();
        t.clear();
        // The same state saved again, and an unrelated mutation: no new notice, no emit.
        t.ctx.mutate(|s| s.set_write_back(&tk.id, wb, 3)).unwrap();
        t.ctx.mutate(|s| s.create("andet", "", false, 4)).unwrap();
        assert!(t.emitted(NOTICES_CHANGED).is_empty());
        assert_eq!(notices(&t).len(), 1);
    }

    #[test]
    fn no_notice_when_kind_is_off() {
        let t = test_ctx(Arc::new(Mutex::new(AgentManager::new(5))));
        t.ctx.notices.set_off(&["writeBackFailed".into()]);
        let tk = external(&t);
        t.clear();
        t.ctx
            .mutate(|s| s.set_write_back(&tk.id, failed(1, "fejl"), 2))
            .unwrap();
        assert!(notices(&t).is_empty());
        assert!(t.emitted(NOTICES_CHANGED).is_empty());
        // Turned on again: the next attempt gives a notice (the off one was never remembered).
        t.ctx.notices.set_off(&[]);
        t.ctx
            .mutate(|s| s.set_write_back(&tk.id, failed(2, "fejl"), 3))
            .unwrap();
        assert_eq!(of_kind(&t, NoticeKind::WriteBackFailed).len(), 1);
    }

    #[test]
    fn exit_notice_with_and_without_released_tickets() {
        let mut mgr = AgentManager::new(5);
        let a = mgr.insert_fake("s-a", "/w/a");
        let m = Arc::new(Mutex::new(mgr));
        let t = test_ctx(Arc::clone(&m));
        // Nothing to release: no notice.
        let released = t.ctx.release_agent(&a, AGENT_EXITED_NOTE).unwrap();
        let info = lock(&m).get(&a).unwrap();
        assert_eq!(t.ctx.notice_exit(&info, released), 0);
        assert!(t.emitted(NOTICES_CHANGED).is_empty());
        // One ticket in progress: one notice, once.
        let tk = t.ctx.mutate(|s| s.create("Arbejde", "", false, 1)).unwrap();
        t.ctx.mutate(|s| s.assign(&tk.id, &a, 2)).unwrap();
        t.ctx.mutate(|s| s.mark_dispatched(&tk.id, &a, 3)).unwrap();
        let released = t.ctx.release_agent(&a, AGENT_EXITED_NOTE).unwrap();
        assert_eq!(released, 1);
        assert_eq!(t.ctx.notice_exit(&info, released), 1);
        assert_eq!(t.ctx.notice_exit(&info, released), 0, "same exit");
        let n = of_kind(&t, NoticeKind::AgentExited);
        assert_eq!(n.len(), 1);
        assert_eq!(
            n[0].text,
            format!("{}: 1 ticket(s) tilbage i backlog", info.name)
        );
        assert_eq!(n[0].agent_id.as_deref(), Some(a.as_str()));
    }
}
