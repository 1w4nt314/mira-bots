//! Refreshing the inbox (step 6c, plan A.5/punkt 9): every source is fetched on the
//! `mira-inbox` thread, one refresh at a time (single flight), merged into `inbox.json` and
//! announced with `inbox-changed` (list + status per source).
//!
//! The frontend drives it (timer, focus, "Opdatér"; B4); the backend enforces per source the
//! minimum interval (folder 60 s, GitHub 120 s) and the back-off after failures (120 → 240 →
//! 480 → 900 s; some GitHub errors wait for a manual refresh). A manual refresh overrides both.
//! The one backend timer is the watch's (step 6d, `watch::runtime`): only while a project
//! keeps watch, at most every 120 s, always with `RefreshReason::Timer` (so the minimum interval
//! and back-off above apply) and never through `run_refresh` directly.
//!
//! Locks: `fetch` and file moves run without any lock; each merge takes `inbox_lock` (then the
//! inbox document's own lock, then — reading the tickets — the service lock), as Start does.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use super::folder::{folder_dir_for, inbox_dirs, move_with_retry};
use super::github::build_github_sources;
use super::source::{InboxStatus, Source, SourceError, SourceErrorKind, SourceStatus};
use super::write_back::locate_folder_file;
use super::InboxItem;
use crate::agent::now_ms;
use crate::config::{shared_items_note, INBOX_DONE_DIR, INBOX_MOVE_FAILED_NOTE, INBOX_STARTED_DIR};
use crate::tickets::model::{TicketState, WriteBackState};
use crate::tickets::TicketsCtx;

/// Status text of a source whose fetch panicked.
pub const INTERNAL_ERROR_TEXT: &str = "intern fejl";
/// `refresh` could not start its thread.
pub const REFRESH_FAILED_TEXT: &str = "Indbakken kunne ikke opdateres";

/// Why the frontend asks (C6c.6). Only `manual` overrides the minimum interval and back-off.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RefreshReason {
    Startup,
    Timer,
    Focus,
    Manual,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The refresh state of a `TicketsCtx` (in memory only).
#[derive(Default)]
pub struct InboxRuntime {
    refreshing: AtomicBool,
    status: Mutex<InboxStatus>,
    /// The source being fetched (a panic marks it failed).
    current: Mutex<Option<String>>,
}

impl InboxRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_refreshing(&self) -> bool {
        self.refreshing.load(Ordering::Acquire)
    }

    /// The status as the UI sees it.
    pub fn status(&self) -> InboxStatus {
        let mut st = lock(&self.status).clone();
        st.refreshing = self.is_refreshing();
        st
    }

    fn with_source<T>(&self, key: &str, f: impl FnOnce(&mut SourceStatus) -> T) -> Option<T> {
        lock(&self.status)
            .sources
            .iter_mut()
            .find(|s| s.id == key)
            .map(f)
    }
}

/// The sources of this refresh: the folder sources that exist (root and projects), then one
/// GitHub source per `(repo, labels)` of the projects' `project.json` (`github`; an invalid one
/// is only a note there). Reads folders and project files, never runs `gh`.
pub fn build_sources(ctx: &TicketsCtx) -> Vec<Box<dyn Source>> {
    let root = ctx.workspace.root();
    let projects = crate::projects::list_projects(root);
    let kinds = ctx.workspace.config().playbook_kinds();
    let github: Vec<(String, crate::checks::GithubConfig)> = projects
        .iter()
        .filter_map(|p| {
            let f = ctx.project_files.read(Path::new(&p.path)).ok().flatten()?;
            Some((p.id.clone(), f.github?))
        })
        .collect();
    inbox_dirs(root, &projects, &kinds)
        .into_iter()
        .map(|s| Box::new(s) as Box<dyn Source>)
        .chain(
            build_github_sources(&ctx.gh, &github)
                .into_iter()
                .map(|s| Box::new(s) as Box<dyn Source>),
        )
        .collect()
}

/// Starts a refresh on the `mira-inbox` thread (C6c.1): `Ok(false)` when one is running already
/// (single flight), `Ok(true)` when started. `inbox-changed` is emitted at the start
/// (`refreshing: true`) and at the end.
pub fn refresh(ctx: &Arc<TicketsCtx>, reason: RefreshReason) -> Result<bool, String> {
    refresh_with(ctx, reason, build_sources)
}

/// [`refresh`] with given sources (tests; B3 may wrap [`build_sources`]).
pub fn refresh_with<F>(
    ctx: &Arc<TicketsCtx>,
    reason: RefreshReason,
    build: F,
) -> Result<bool, String>
where
    F: FnOnce(&TicketsCtx) -> Vec<Box<dyn Source>> + Send + 'static,
{
    let rt = &ctx.inbox_rt;
    if rt.refreshing.swap(true, Ordering::AcqRel) {
        return Ok(false);
    }
    ctx.emit_inbox();
    let spawned = ctx.spawn_inbox_thread("mira-inbox", move |me| {
        let run = catch_unwind(AssertUnwindSafe(|| {
            let sources = build(me);
            run_refresh(me, reason, sources, now_ms());
        }));
        if let Err(panic) = run {
            let what = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            log::error!("inbox: the refresh panicked: {what}");
            if let Some(key) = lock(&me.inbox_rt.current).clone() {
                let e = SourceError::new(SourceErrorKind::Internal, INTERNAL_ERROR_TEXT);
                me.inbox_rt.with_source(&key, |s| s.failed(now_ms(), &e));
            }
        }
        finish(me);
    });
    if !spawned {
        finish(ctx);
        return Err(REFRESH_FAILED_TEXT.into());
    }
    Ok(true)
}

/// The end of every refresh (also after a panic): not refreshing, the time, `inbox-changed`.
fn finish(ctx: &TicketsCtx) {
    *lock(&ctx.inbox_rt.current) = None;
    lock(&ctx.inbox_rt.status).last_refresh_at = Some(now_ms());
    ctx.inbox_rt.refreshing.store(false, Ordering::Release);
    ctx.emit_inbox();
}

/// One refresh, synchronous (on the refresh thread): the status list follows `sources`; each
/// due source is fetched without a lock and merged under `inbox_lock`; then items whose ticket
/// exists are repaired (`reconcile`), gone items pruned, and files that could not be moved are
/// tried again.
pub fn run_refresh(
    ctx: &TicketsCtx,
    reason: RefreshReason,
    sources: Vec<Box<dyn Source>>,
    now: u64,
) {
    let rt = &ctx.inbox_rt;
    let manual = reason == RefreshReason::Manual;
    {
        let mut st = lock(&rt.status);
        let old = std::mem::take(&mut st.sources);
        st.sources = sources
            .iter()
            .map(|s| {
                let id = s.id();
                let mut e = old
                    .iter()
                    .find(|o| o.id == id.key)
                    .cloned()
                    .unwrap_or_else(|| SourceStatus::new(&id, s.label(), s.project()));
                e.label = s.label();
                e.project = s.project();
                e
            })
            .collect();
    }
    for source in &sources {
        let id = source.id();
        let due = rt
            .with_source(&id.key, |s| s.due(now, source.min_interval_ms(), manual))
            .unwrap_or(false);
        if !due {
            continue;
        }
        rt.with_source(&id.key, |s| s.last_attempt_at = Some(now));
        *lock(&rt.current) = Some(id.key.clone());
        match source.fetch() {
            Ok(fetched) => {
                let applied = {
                    let _serial = ctx.lock_inbox_serial();
                    ctx.inbox_mutate_quiet(|i| i.apply(&id, fetched.clone(), now))
                };
                match applied {
                    Ok(a) => {
                        log::debug!(
                            "inbox: {} listed {} (+{} ~{} -{})",
                            id.key,
                            fetched.items.len(),
                            a.added,
                            a.updated,
                            a.gone
                        );
                        rt.with_source(&id.key, |s| {
                            s.succeeded(now, &fetched);
                            if a.elsewhere > 0 {
                                s.notes.push(shared_items_note(a.elsewhere));
                            }
                        });
                    }
                    Err(e) => {
                        let e = SourceError::new(SourceErrorKind::Internal, e);
                        rt.with_source(&id.key, |s| s.failed(now, &e));
                    }
                }
            }
            Err(e) => {
                log::info!("inbox: fetching {} failed ({:?})", id.key, e.kind);
                rt.with_source(&id.key, |s| s.failed(now, &e));
            }
        }
        *lock(&rt.current) = None;
    }
    {
        let _serial = ctx.lock_inbox_serial();
        let pairs: Vec<(String, String)> = ctx.read(|s| {
            s.list()
                .into_iter()
                .filter_map(|t| {
                    t.external
                        .filter(|e| !e.inherited)
                        .map(|e| (e.external_id, t.id))
                })
                .collect()
        });
        match ctx.inbox_mutate_quiet(|i| i.reconcile(&pairs)) {
            Ok(n) if n > 0 => log::info!("inbox: {n} item(s) repaired to started"),
            Ok(_) => {}
            Err(e) => log::warn!("inbox: reconcile failed: {e}"),
        }
        if let Err(e) = ctx.inbox_mutate_quiet(|i| i.prune(now)) {
            log::warn!("inbox: prune failed: {e}");
        }
    }
    retry_moves(ctx);
}

/// Folder items whose file could not be moved at Start or Done (`moved == false`): the move is
/// tried again (to `done/` when the ticket is Done, else `started/`), without any lock. A Done
/// ticket whose write back failed only because of the move becomes `done`.
fn retry_moves(ctx: &TicketsCtx) {
    let pending: Vec<InboxItem> = ctx.inbox_read(|i| i.pending_moves());
    for item in pending {
        let (Some(path), Some(ticket_id)) = (item.path.as_deref(), item.ticket_id.as_deref())
        else {
            continue;
        };
        let Some(dir) = folder_dir_for(ctx.workspace.root(), &item.source_id) else {
            continue;
        };
        let ticket = ctx.read(|s| s.get(ticket_id));
        let done = ticket
            .as_ref()
            .is_some_and(|t| t.state == TicketState::Done);
        let moved = if done {
            match locate_folder_file(&dir, path) {
                Some(file) => move_with_retry(&file, &dir.join(INBOX_DONE_DIR)).is_ok(),
                None => in_place(&dir.join(INBOX_DONE_DIR), path),
            }
        } else if in_place(&dir, path) {
            move_with_retry(&dir.join(path), &dir.join(INBOX_STARTED_DIR)).is_ok()
        } else {
            in_place(&dir.join(INBOX_STARTED_DIR), path)
        };
        if !moved {
            continue;
        }
        if let Err(e) = ctx.inbox_mutate_quiet(|i| i.set_moved(&item.id, true)) {
            log::debug!("inbox: item {} not updated: {e}", item.id);
            continue;
        }
        log::info!("inbox: the file of item {} was moved on retry", item.id);
        if let Some(t) = ticket.filter(|_| done) {
            let Some(mut wb) = t.external.as_ref().map(|e| e.write_back.clone()) else {
                continue;
            };
            if wb.comment == WriteBackState::Failed
                && wb.last_error.as_deref() == Some(INBOX_MOVE_FAILED_NOTE)
            {
                wb.comment = WriteBackState::Done;
                wb.last_error = None;
                let now = now_ms();
                if let Err(e) = ctx.mutate(|s| s.set_write_back(&t.id, wb, now)) {
                    log::debug!("inbox: write back of {} not updated: {e}", t.short_id());
                }
            }
        }
    }
}

/// `dir/path` is a regular file.
fn in_place(dir: &Path, path: &str) -> bool {
    std::fs::symlink_metadata(dir.join(path)).is_ok_and(|m| m.file_type().is_file())
}

#[cfg(test)]
mod tests {
    use super::super::source::{Fetched, SourceId};
    use super::super::test_support::{folder_item, FolderEnv};
    use super::super::InboxState;
    use super::*;
    use crate::events::INBOX_CHANGED;
    use crate::tickets::model::ExternalKind;
    use serde_json::json;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;

    /// A source that counts its fetches and answers with `answer`.
    struct Fake {
        key: &'static str,
        fetches: Arc<AtomicUsize>,
        answer: Result<Fetched, SourceError>,
        gate: Option<Mutex<mpsc::Receiver<()>>>,
        panics: bool,
    }

    impl Fake {
        fn ok(fetches: &Arc<AtomicUsize>) -> Self {
            Fake {
                key: "folder:fake",
                fetches: Arc::clone(fetches),
                answer: Ok(Fetched {
                    complete: true,
                    ..Fetched::default()
                }),
                gate: None,
                panics: false,
            }
        }
    }

    impl Source for Fake {
        fn id(&self) -> SourceId {
            SourceId {
                kind: ExternalKind::Folder,
                key: self.key.into(),
            }
        }
        fn label(&self) -> String {
            "fake".into()
        }
        fn min_interval_ms(&self) -> u64 {
            60_000
        }
        fn fetch(&self) -> Result<Fetched, SourceError> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            if let Some(g) = &self.gate {
                g.lock().unwrap().recv().unwrap();
            }
            if self.panics {
                panic!("boom");
            }
            self.answer.clone()
        }
    }

    fn once(f: Fake) -> impl FnOnce(&TicketsCtx) -> Vec<Box<dyn Source>> + Send + 'static {
        move |_| vec![Box::new(f) as Box<dyn Source>]
    }

    fn source_status(env: &FolderEnv, key: &str) -> SourceStatus {
        env.t
            .ctx
            .inbox_rt
            .status()
            .sources
            .into_iter()
            .find(|s| s.id == key)
            .unwrap()
    }

    #[test]
    fn refresh_lists_github_issues_with_dedup_and_min_interval() {
        use crate::gh::fake::{FakeGh, RATE_LIMIT};
        let gh = Arc::new(FakeGh::new());
        gh.reply(
            &["issue", "list"],
            0,
            r#"[{"author":{"login":"alice"},"labels":[{"name":"bug"}],"number":7,"state":"OPEN",
                "title":"Crash","updatedAt":"2026-10-01T10:00:00Z","url":"https://github.com/o/r/issues/7"}]"#,
            "",
        );
        let env = FolderEnv::with_gh(Vec::new(), gh.clone());
        env.project_json(r#"{"github": {"repo": "o/r", "labels": ["bug"]}}"#);
        // A second project with the same repo and labels shares the call.
        let api = env.root.join("api");
        std::fs::create_dir_all(&api).unwrap();
        let pj = crate::checks::project_file_path(&api);
        std::fs::create_dir_all(pj.parent().unwrap()).unwrap();
        std::fs::write(&pj, r#"{"github": {"repo": "O/R", "labels": ["bug"]}}"#).unwrap();
        let ctx = &env.t.ctx;
        let t0 = 1_000_000;
        run_refresh(ctx, RefreshReason::Startup, build_sources(ctx), t0);
        assert_eq!(gh.calls().len(), 1, "one gh call for two projects");
        let p = ctx.inbox_payload();
        let it = p
            .items
            .iter()
            .find(|i| i.external_id == "github:o/r#7")
            .unwrap();
        assert_eq!(
            (it.project.as_deref(), it.candidates.clone()),
            (None, vec!["api".to_string(), "web".to_string()])
        );
        assert_eq!(it.number, Some(7));
        let st = source_status(&env, "github:o/r[bug]");
        assert!(st.ok && st.items == 1 && st.project.is_none());
        // Timer within 120 s: no call; manual: a call.
        run_refresh(ctx, RefreshReason::Timer, build_sources(ctx), t0 + 119_000);
        assert_eq!(gh.calls().len(), 1);
        run_refresh(ctx, RefreshReason::Timer, build_sources(ctx), t0 + 120_000);
        assert_eq!(gh.calls().len(), 2);
        run_refresh(ctx, RefreshReason::Manual, build_sources(ctx), t0 + 121_000);
        assert_eq!(gh.calls().len(), 3);
        // Rate limit: the status shows the clock time of the next try (900 s).
        gh.reply(&["issue", "list", "--repo"], 1, "", RATE_LIMIT);
        run_refresh(ctx, RefreshReason::Manual, build_sources(ctx), t0 + 200_000);
        let st = source_status(&env, "github:o/r[bug]");
        assert_eq!(st.error_kind, Some(SourceErrorKind::RateLimited));
        assert_eq!(st.next_retry_at, Some(t0 + 200_000 + 900_000));
        assert_eq!(
            st.error.as_deref(),
            Some(crate::config::rate_limited_note(&crate::gh::local_hhmm(t0 + 1_100_000)).as_str())
        );
        // The item stays (an error never marks anything gone).
        assert_eq!(ctx.inbox_payload().items.len(), 1);
    }

    #[test]
    fn refresh_skips_projects_with_an_invalid_github() {
        let env = FolderEnv::new(Vec::new());
        env.project_json(r#"{"github": {"repo": "ikke et repo"}}"#);
        let sources = build_sources(&env.t.ctx);
        assert!(sources.iter().all(|s| s.id().kind == ExternalKind::Folder));
    }

    #[test]
    fn refresh_scans_folders_and_emits_payload() {
        let env = FolderEnv::new(Vec::new());
        std::fs::create_dir_all(env.root.join("inbox")).unwrap();
        std::fs::write(env.root.join("inbox").join("rod.md"), "# Fra roden\nx").unwrap();
        env.write(
            "fejl-1.md",
            "---\ntitle: Fejl i login\nlabels: bug\n---\nTrin 1",
        );
        assert_eq!(refresh(&env.t.ctx, RefreshReason::Startup), Ok(true));
        env.t.ctx.join_inbox_threads();
        let events = env.t.emitted(INBOX_CHANGED);
        assert!(events.len() >= 2);
        assert_eq!(events[0]["status"]["refreshing"], json!(true));
        let last = events.last().unwrap();
        assert_eq!(last["status"]["refreshing"], json!(false));
        assert!(last["status"]["lastRefreshAt"].is_u64());
        let items = last["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        let web = items
            .iter()
            .find(|i| i["externalId"] == json!("folder:web:fejl-1.md"))
            .unwrap();
        assert_eq!(web["title"], json!("Fejl i login"));
        assert_eq!(web["labels"], json!(["bug"]));
        assert_eq!(web["project"], json!("web"));
        assert_eq!(web["hasBody"], json!(true));
        assert!(web.get("body").is_none());
        let sources = last["status"]["sources"].as_array().unwrap();
        let ids: Vec<&str> = sources.iter().map(|s| s["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["folder:_rod", "folder:web"]);
        assert_eq!(sources[1]["ok"], json!(true));
        assert_eq!(sources[1]["items"], json!(1));
        assert_eq!(sources[1]["project"], json!("web"));
        assert!(sources[1]["lastFetchAt"].is_u64());
        // The body is in the store.
        assert_eq!(
            env.item("fejl-1.md").unwrap().body.as_deref(),
            Some("Trin 1")
        );
        assert!(env.store.saves() > 0);
    }

    #[test]
    fn refresh_is_single_flight() {
        let env = FolderEnv::new(Vec::new());
        let fetches = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel();
        let mut slow = Fake::ok(&fetches);
        slow.gate = Some(Mutex::new(rx));
        assert_eq!(
            refresh_with(&env.t.ctx, RefreshReason::Manual, once(slow)),
            Ok(true)
        );
        assert!(env.t.ctx.inbox_rt.is_refreshing());
        // A second refresh while the first runs does nothing.
        assert_eq!(
            refresh_with(&env.t.ctx, RefreshReason::Manual, once(Fake::ok(&fetches))),
            Ok(false)
        );
        assert_eq!(refresh(&env.t.ctx, RefreshReason::Manual), Ok(false));
        tx.send(()).unwrap();
        env.t.ctx.join_inbox_threads();
        assert!(!env.t.ctx.inbox_rt.is_refreshing());
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
        assert_eq!(
            refresh_with(&env.t.ctx, RefreshReason::Manual, once(Fake::ok(&fetches))),
            Ok(true)
        );
        env.t.ctx.join_inbox_threads();
        assert_eq!(fetches.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn refresh_respects_min_interval_except_manual() {
        let env = FolderEnv::new(Vec::new());
        let fetches = Arc::new(AtomicUsize::new(0));
        for (reason, expected) in [
            (RefreshReason::Timer, 1),
            (RefreshReason::Timer, 1),
            (RefreshReason::Focus, 1),
            (RefreshReason::Manual, 2),
            (RefreshReason::Startup, 2),
        ] {
            assert_eq!(
                refresh_with(&env.t.ctx, reason, once(Fake::ok(&fetches))),
                Ok(true)
            );
            env.t.ctx.join_inbox_threads();
            assert_eq!(fetches.load(Ordering::SeqCst), expected, "{reason:?}");
        }
        let st = source_status(&env, "folder:fake");
        assert!(st.ok && st.last_fetch_at.is_some());
    }

    #[test]
    fn refresh_backs_off_after_failures() {
        let env = FolderEnv::new(Vec::new());
        let fetches = Arc::new(AtomicUsize::new(0));
        let failing = |kind| {
            let mut f = Fake::ok(&fetches);
            f.answer = Err(SourceError::new(kind, "ingen forbindelse til GitHub"));
            f
        };
        let ctx = &env.t.ctx;
        refresh_with(
            ctx,
            RefreshReason::Manual,
            once(failing(SourceErrorKind::Network)),
        )
        .unwrap();
        ctx.join_inbox_threads();
        let st = source_status(&env, "folder:fake");
        assert!(!st.ok);
        assert_eq!(st.error.as_deref(), Some("ingen forbindelse til GitHub"));
        assert_eq!(st.error_kind, Some(SourceErrorKind::Network));
        let first = st.next_retry_at.unwrap() - st.last_attempt_at.unwrap();
        assert_eq!(first, 120_000);
        // The timer waits for the back-off; a manual refresh does not, and the step grows.
        refresh_with(
            ctx,
            RefreshReason::Timer,
            once(failing(SourceErrorKind::Network)),
        )
        .unwrap();
        ctx.join_inbox_threads();
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
        refresh_with(
            ctx,
            RefreshReason::Manual,
            once(failing(SourceErrorKind::Network)),
        )
        .unwrap();
        ctx.join_inbox_threads();
        let st = source_status(&env, "folder:fake");
        assert_eq!((fetches.load(Ordering::SeqCst), st.fails), (2, 2));
        assert_eq!(
            st.next_retry_at.unwrap() - st.last_attempt_at.unwrap(),
            240_000
        );
        // A kind that needs the user: no retry time at all.
        refresh_with(
            ctx,
            RefreshReason::Manual,
            once(failing(SourceErrorKind::NotLoggedIn)),
        )
        .unwrap();
        ctx.join_inbox_threads();
        let st = source_status(&env, "folder:fake");
        assert_eq!(st.next_retry_at, None);
        assert!(!st.due(u64::MAX / 2, 60_000, false));
        // Success resets it.
        refresh_with(ctx, RefreshReason::Manual, once(Fake::ok(&fetches))).unwrap();
        ctx.join_inbox_threads();
        let st = source_status(&env, "folder:fake");
        assert!(st.ok && st.error.is_none() && st.error_kind.is_none() && st.fails == 0);
    }

    #[test]
    fn refresh_panic_is_caught_and_marks_the_source() {
        let env = FolderEnv::new(Vec::new());
        let fetches = Arc::new(AtomicUsize::new(0));
        let mut bad = Fake::ok(&fetches);
        bad.panics = true;
        refresh_with(&env.t.ctx, RefreshReason::Manual, once(bad)).unwrap();
        env.t.ctx.join_inbox_threads();
        assert!(!env.t.ctx.inbox_rt.is_refreshing());
        let st = source_status(&env, "folder:fake");
        assert_eq!(st.error.as_deref(), Some(INTERNAL_ERROR_TEXT));
        assert_eq!(st.error_kind, Some(SourceErrorKind::Internal));
        assert_eq!(
            env.t.emitted(INBOX_CHANGED).last().unwrap()["status"]["refreshing"],
            json!(false)
        );
    }

    #[test]
    fn fingerprint_change_updates_new_item_and_deleted_file_is_gone() {
        let env = FolderEnv::new(Vec::new());
        let file = env.write("a.md", "# Første\nx");
        let ctx = &env.t.ctx;
        let run = || {
            refresh(ctx, RefreshReason::Manual).unwrap();
            ctx.join_inbox_threads();
        };
        run();
        let before = env.item("a.md").unwrap();
        assert_eq!(before.title, "Første");
        std::fs::write(&file, "# Anden titel\nlængere tekst").unwrap();
        run();
        let after = env.item("a.md").unwrap();
        assert_eq!(
            (after.id.as_str(), after.title.as_str()),
            (before.id.as_str(), "Anden titel")
        );
        assert_ne!(after.fingerprint, before.fingerprint);
        assert_eq!(after.body.as_deref(), Some("længere tekst"));
        std::fs::remove_file(&file).unwrap();
        run();
        // Gone and new: pruned at once.
        assert!(env.item("a.md").is_none());
        assert!(ctx.inbox_payload().items.is_empty());
    }

    #[test]
    fn refresh_reconciles_started_items() {
        let env = FolderEnv::new(vec![folder_item("i1", "a.md")]);
        let ctx = &env.t.ctx;
        let ext = super::super::start::external_ref_for(&folder_item("i1", "a.md"));
        let t = ctx
            .mutate(|s| s.create_external("T", "", false, None, None, ext, 5))
            .unwrap();
        run_refresh(ctx, RefreshReason::Timer, Vec::new(), now_ms());
        let it = ctx.inbox_read(|i| i.get("i1")).unwrap();
        assert_eq!(it.state, InboxState::Started);
        assert_eq!(it.ticket_id.as_deref(), Some(t.id.as_str()));
    }
}
