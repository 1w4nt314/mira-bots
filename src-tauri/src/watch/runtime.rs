//! Vagtens skal (trin 6d, plan6d punkt 17, A.2, A.11, C6d.3): [`WatchRuntime`] i `AppState`,
//! timeren ([`start`]), ét tick ([`tick`]) og [`AppPort`], der giver motoren de rigtige
//! indgange (`ctx.inbox_start` + `start_playbook_with` + en omsluttet `SpawnPort` foran
//! `spawn_for_tool`).
//!
//! Låse (C6d.3): ingen `WatchRuntime`-lås holdes under et port-kald, under `project.json`-,
//! workspace- eller projektlæsning eller under ventetiden på en hentning. Hukommelsen (`mem`)
//! kopieres ud under en kort lås før motoren kører og skrives tilbage under en ny kort lås
//! bagefter; `tick_busy` sikrer at kun ét tick kører ad gangen. Budgettet reserveres og
//! bekræftes/annulleres i hver sin korte lås på `watch-state.json` (motoren).
//!
//! Exit (A.11): `shutdown()` sætter `stopping` og afbryder tasken (rammer ved næste `.await`);
//! et tick der allerede kører på en blocking-tråd, tjekker `stopping` før ventetiden, før hvert
//! start og før hvert spawn, og `AgentManager.closing` afviser spawn efter `kill_all()`.

use std::collections::HashSet;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tauri::async_runtime::JoinHandle;
use tauri::{AppHandle, Emitter, Manager};

use crate::agent::claude_path::find_claude;
use crate::agent::{now_ms, AgentError, AgentInfo, Role, SeatKind};
use crate::commands::AppState;
use crate::config::{
    watch_playbook_failed_note, watch_started_note, INBOX_ISSUE_CLOSED, INBOX_ITEM_GONE,
    WATCH_MAX_AGENTS_TEXT, WATCH_NO_STAFF_SPAWN, WATCH_REFRESH_MIN_MS, WATCH_REFRESH_WAIT_MAX_MS,
    WATCH_STOP_POLL_MS, WATCH_TICK_SECS,
};
use crate::events::WATCH_CHANGED;
use crate::hooks::status::AgentStatus;
use crate::inbox::source::{Retry, SourceErrorKind};
use crate::inbox::{InboxItemSummary, InboxState, InboxStatus, RefreshReason, StartRequest};
use crate::notices::{derive_waiting_notices, Notice, NoticeKey};
use crate::projects::{same_id, ProjectRef};
use crate::tickets::model::{short_id, TicketError};
use crate::tickets::playbook::{start_playbook_with, StartOpts, StartedBy};
use crate::tickets::tools::{SpawnByProfile, SpawnPort};
use crate::tickets::TicketsCtx;
use crate::watch::engine::{
    active_projects, build_view, can_start_anything, short_error, staffing_for, AgentsPort,
    Candidate, Clock, InboxPort, Notifier, ProjectWatchFile, SeatRoom, SourceHealth, Staffing,
    StartFailure, Started, StarterPort, StatePort, TickPlan, WatchEngine, WatchMemory, WatchPort,
    WatchStart, WatchView,
};
use crate::watch::state::{WatchState, WatchStateFile};
use crate::workspace::WorkspaceSnapshot;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Vagtens tilstand i `AppState`. Alle låse her tages kort og aldrig sammen med en anden lås
/// eller under et kald ud af modulet.
pub struct WatchRuntime {
    stopping: AtomicBool,
    tick_busy: AtomicBool,
    handle: Mutex<Option<JoinHandle<()>>>,
    /// `watch-state.json` (budget, fejl, stop).
    pub state: WatchStateFile,
    /// Ventende emner og vagt-agenter (kopieres ud/ind omkring motoren).
    mem: Mutex<WatchMemory>,
    /// Det senest publicerede view (`None` før første beregning).
    view: Mutex<Option<WatchView>>,
    /// Hvornår vagten sidst bad om en hentning.
    last_refresh_ask: Mutex<Option<u64>>,
    /// Advarslen fra indlæsningen af `watch-state.json` (Diagnostik).
    warning: Option<String>,
}

impl WatchRuntime {
    pub fn new(state: WatchStateFile, warning: Option<String>) -> Self {
        WatchRuntime {
            stopping: AtomicBool::new(false),
            tick_busy: AtomicBool::new(false),
            handle: Mutex::new(None),
            state,
            mem: Mutex::new(WatchMemory::default()),
            view: Mutex::new(None),
            last_refresh_ask: Mutex::new(None),
            warning,
        }
    }

    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }

    /// Exit-handleren, **før** `kill_all()` (ved både `ExitRequested` og `Exit`): ingen nye
    /// starter eller spawns; timer-tasken afbrydes. Idempotent.
    pub fn shutdown(&self) {
        if !self.stopping.swap(true, Ordering::AcqRel) {
            log::info!("watch: stopper (appen lukker)");
        }
        if let Some(h) = lock(&self.handle).take() {
            h.abort();
        }
    }

    /// Gemmer timer-tasken (efter `manage`). Lukker appen allerede, afbrydes den straks.
    pub fn set_handle(&self, h: JoinHandle<()>) {
        if self.is_stopping() {
            h.abort();
            return;
        }
        *lock(&self.handle) = Some(h);
        // `shutdown` kan være kommet imellem tjekket og gemningen.
        if self.is_stopping() {
            if let Some(h) = lock(&self.handle).take() {
                h.abort();
            }
        }
    }

    /// Det senest publicerede view.
    pub fn view(&self) -> Option<WatchView> {
        lock(&self.view).clone()
    }

    /// Antal aktive vagt-projekter i det senest publicerede view (Diagnostik).
    pub fn active(&self) -> usize {
        lock(&self.view).as_ref().map_or(0, |v| v.active)
    }

    pub fn warning(&self) -> Option<&str> {
        self.warning.as_deref()
    }

    pub fn state_path(&self) -> &Path {
        self.state.path()
    }

    /// "Genstart vagt": nulstiller fejl og stop for `project` og glemmer projektets fejlede
    /// emner, så de prøves igen ved næste tick (review6d N11). `true` når det var stoppet. Et
    /// tick der kører netop nu, kan skrive sin kopi af hukommelsen tilbage bagefter; så venter
    /// emnerne deres time ud (fail-closed).
    pub fn reset_trip(&self, project: &str) -> bool {
        let was = self.state.with(|s| s.reset_trip(project));
        lock(&self.mem).forget_failed(project);
        was
    }

    fn mem_copy(&self) -> WatchMemory {
        lock(&self.mem).clone()
    }

    fn store_mem(&self, mem: WatchMemory) {
        *lock(&self.mem) = mem;
    }

    fn store_view(&self, view: WatchView) {
        *lock(&self.view) = Some(view);
    }
}

// ---- rene hjælpere ----

/// Den omsluttede spawn-ports regel (A.5, C6d.5): appen lukker → afvis; ingen arbejdsrolle
/// (stabsrolle eller ukendt profil) → afvis; vagtens agentloft brugt op → afvis.
pub fn watch_spawn_guard(role: Option<Role>, left: usize, stopping: bool) -> Result<(), String> {
    if stopping {
        return Err(AgentError::Closing.to_string());
    }
    if !role.is_some_and(Role::is_work) {
        return Err(WATCH_NO_STAFF_SPAWN.to_string());
    }
    if left == 0 {
        return Err(WATCH_MAX_AGENTS_TEXT.to_string());
    }
    Ok(())
}

/// Spawn-porten vagten giver `start_playbook_with`: [`watch_spawn_guard`] før hvert kald (med
/// `stopping` læst lige før), altid en arbejdsplads, højst `left` agenter, og hvert spawnet id
/// registreres i `spawned`.
pub fn watch_spawn_port(
    left: usize,
    stopping: impl Fn() -> bool + Send + Sync + 'static,
    spawn: impl Fn(SpawnByProfile) -> Result<AgentInfo, String> + Send + Sync + 'static,
    spawned: Arc<Mutex<Vec<String>>>,
) -> SpawnPort {
    let left = AtomicUsize::new(left);
    Arc::new(move |mut req: SpawnByProfile| {
        let role = Role::parse(&req.profile_id);
        watch_spawn_guard(role, left.load(Ordering::Acquire), stopping())?;
        // Vagten sidder aldrig på en stabsplads, uanset profilens standardplads.
        req.seat_kind = Some(SeatKind::Work);
        let info = spawn(req)?;
        // Kaldene kommer ét ad gangen fra `start_playbook_with`; vagten (> 0) er tjekket ovenfor.
        left.store(
            left.load(Ordering::Acquire).saturating_sub(1),
            Ordering::Release,
        );
        lock(&spawned).push(info.id.clone());
        log::info!(
            "watch: agent {} startet ({})",
            info.id,
            role.map_or("?", Role::as_str)
        );
        Ok(info)
    })
}

/// Fejlen fra `inbox_start` → [`StartFailure`]: emnet er væk, allerede startet som ticket,
/// eller issuen blev lukket mellem hentning og start (review6d W1) → `Gone` (ingen fejl, intet
/// budget); `gh` eller netværket svigtede (`gh` mangler, er logget ud eller afvist, rate limit,
/// ingen forbindelse, timeout — [`crate::gh::error_text`]) → `Source` (ingen fejltælling,
/// review6d W3); alt andet er reelt, som én kort linje ([`short_error`], review6d N2).
///
/// Et lukket issue forbliver `new` i indbakken (der findes ingen vej til at markere ét emne
/// `gone` uden en komplet hentning); næste komplette hentning markerer det `gone`.
pub fn classify_start_error(e: &str) -> StartFailure {
    use crate::gh::{error_text, GhError};
    let started = TicketError::ExternalAlreadyStarted(String::new()).to_string();
    if e == INBOX_ITEM_GONE || e == INBOX_ISSUE_CLOSED || e.starts_with(started.trim_end()) {
        return StartFailure::Gone;
    }
    // Review6d N19: GitHub svarer 5xx på `gh issue view` → kilden, ikke emnet.
    if e.starts_with("gh: ") && e.contains("HTTP 5") {
        return StartFailure::Source(short_error(e));
    }
    let source = [
        GhError::GhMissing,
        GhError::NotLoggedIn,
        GhError::BadCredentials,
        GhError::RateLimited,
        GhError::Network,
        GhError::Timeout,
    ]
    .iter()
    .any(|g| e == error_text(g, ""));
    if source {
        StartFailure::Source(short_error(e))
    } else {
        StartFailure::Real(short_error(e))
    }
}

/// Kilden `source_id` i indbakkens status (review6d W3; ingen `gh`-kald). En ukendt kilde er ok.
pub fn source_health_of(status: &InboxStatus, source_id: &str) -> SourceHealth {
    let Some(s) = status.sources.iter().find(|s| s.id == source_id) else {
        return SourceHealth::default();
    };
    SourceHealth {
        error: (!s.ok).then(|| {
            s.error
                .clone()
                .unwrap_or_else(|| "kilden kunne ikke hentes".to_string())
        }),
        waits_for_user: !s.ok && s.error_kind.map(SourceErrorKind::retry) == Some(Retry::Manual),
        next_retry_at: s.next_retry_at,
        last_fetch_at: s.last_fetch_at,
    }
}

/// Kandidaterne i `project` (C6d.4) af indbakkens liste (som allerede er uden `gone`): kun
/// `new`, projektet entydigt `project`; ældste `seenAt` først (stabil: ellers listens
/// rækkefølge).
pub fn watch_candidates(items: &[InboxItemSummary], project: &str) -> Vec<Candidate> {
    let mut v: Vec<&InboxItemSummary> = items
        .iter()
        .filter(|i| {
            i.state == InboxState::New
                && i.candidates.is_empty()
                && i.project.as_deref().is_some_and(|p| same_id(p, project))
        })
        .collect();
    v.sort_by_key(|i| i.seen_at);
    v.into_iter()
        .map(|i| Candidate {
            item_id: i.id.clone(),
            title: i.title.clone(),
            labels: i.labels.clone(),
            ticket_kind: i.ticket_kind.clone(),
            seen_at: i.seen_at,
            source_id: i.source_id.clone(),
        })
        .collect()
}

/// Kandidaterne fra indbakke-dokumentet (kun dets egen korte lås; ingen filer).
pub fn candidates_in(ctx: &TicketsCtx, project: &str) -> Vec<Candidate> {
    watch_candidates(&ctx.inbox_read(|i| i.list()), project)
}

/// Start-dialogens dubletregel (`duplicate_of`) for vagten (kun service-låsen; ingen filer).
pub fn is_duplicate_in(ctx: &TicketsCtx, project: &str, title: &str) -> bool {
    let tickets = ctx.read(|s| s.list());
    crate::inbox::ipc::duplicate_of(&tickets, Some(project), title).is_some()
}

/// Skal vagten bede om en hentning nu? Højst hvert [`WATCH_REFRESH_MIN_MS`], både målt fra
/// sidste hentning (uanset hvem der bad om den) og fra vagtens sidste forespørgsel.
pub fn should_refresh(last_refresh_at: Option<u64>, last_ask: Option<u64>, now: u64) -> bool {
    let old = |t: Option<u64>| t.is_none_or(|t| now.saturating_sub(t) >= WATCH_REFRESH_MIN_MS);
    old(last_refresh_at) && old(last_ask)
}

/// `project.json`-noterne om `watch` (Diagnostik).
fn watch_notes(notes: &[String]) -> Vec<String> {
    notes
        .iter()
        .filter(|n| n.starts_with("project.json: watch"))
        .cloned()
        .collect()
}

/// Starter emnet som vagten (C6d.4, A.6): `inbox_start` med `skip_review: false`, playbooken
/// som `kind` og projektet sat; derefter `start_playbook_with(StartedBy::Watch, spawn,
/// StartOpts::watch())` og en System-note på forælderen. Ingen lås holdes af kalderen.
pub fn start_for_watch(
    ctx: &Arc<TicketsCtx>,
    req: &WatchStart,
    spawn: &SpawnPort,
) -> Result<Started, StartFailure> {
    let ticket = ctx
        .inbox_start(StartRequest {
            item_id: req.item_id.clone(),
            kind: Some(req.playbook.clone()),
            project: Some(ProjectRef::Existing(req.project.clone())),
            skip_review: false,
        })
        .map_err(|e| classify_start_error(&e))?;
    let now = now_ms();
    // Review6d N4: ingen `tickets-changed` når noten allerede er den sidste.
    let note = |text: String| {
        if let Err(e) = ctx.mutate_if(
            |s| s.note_by_system(&ticket.id, &text, now),
            Option::is_some,
        ) {
            log::warn!(
                "watch: note på ticket {} fejlede: {e}",
                short_id(&ticket.id)
            );
        }
    };
    match start_playbook_with(
        ctx,
        &ticket.id,
        StartedBy::Watch,
        Some(spawn),
        StartOpts::watch(),
    ) {
        Ok(pb) => {
            note(watch_started_note(&req.playbook));
            Ok(Started {
                ticket_id: ticket.id.clone(),
                spawned: pb.spawned,
                notes: pb.notes,
            })
        }
        Err(e) => {
            let e = short_error(&e);
            note(watch_playbook_failed_note(&e));
            Err(StartFailure::AfterTicket {
                ticket_id: ticket.id.clone(),
                error: e,
            })
        }
    }
}

// ---- porten ----

/// Den rigtige port: kun `AppState`'s egne indgange. Ingen `WatchRuntime`-lås under noget kald.
pub struct AppPort<'a> {
    app: &'a AppHandle,
    state: &'a AppState,
    /// Workspace-filen som tick'et læste den før ethvert indbakke-kald.
    ws: WorkspaceSnapshot,
}

impl<'a> AppPort<'a> {
    pub fn new(app: &'a AppHandle, state: &'a AppState, ws: WorkspaceSnapshot) -> Self {
        AppPort { app, state, ws }
    }
}

impl Clock for AppPort<'_> {
    fn now_ms(&self) -> u64 {
        now_ms()
    }
    fn offset_secs(&self, now_ms: u64) -> i64 {
        crate::gh::local_offset_secs((now_ms / 1000) as i64)
    }
}

impl InboxPort for AppPort<'_> {
    fn candidates(&self, project: &str) -> Vec<Candidate> {
        candidates_in(&self.state.tickets, project)
    }
    fn is_duplicate(&self, project: &str, title: &str) -> bool {
        is_duplicate_in(&self.state.tickets, project, title)
    }
    fn source_health(&self, source_id: &str) -> SourceHealth {
        source_health_of(&self.state.tickets.inbox_rt.status(), source_id)
    }
}

impl StarterPort for AppPort<'_> {
    fn staffing(&self, project: &str, playbook: &str, watch_free: usize) -> Staffing {
        let Some(pb) = self.ws.config.playbooks.get(playbook) else {
            return Staffing::NoSeat;
        };
        let roles: Vec<Role> = pb.steps.iter().map(|s| s.role).collect();
        let (agents, room) = {
            let m = lock(&self.state.manager);
            let room = SeatRoom {
                work_free: m.free_seats(SeatKind::Work),
                in_project: m.live_work_in_project(project).len(),
                max_per_project: self.ws.rules.max_agents_per_project,
            };
            (m.list(), room)
        };
        staffing_for(&roles, &agents, project, room, watch_free)
    }

    fn start(&self, req: WatchStart) -> Result<Started, StartFailure> {
        if self.stopping() {
            return Err(StartFailure::Gone);
        }
        let spawned: Arc<Mutex<Vec<String>>> = Arc::default();
        let (a, b) = (self.app.clone(), self.app.clone());
        let port = watch_spawn_port(
            req.max_agents_left,
            move || {
                a.try_state::<AppState>()
                    .is_none_or(|s| s.watch.is_stopping())
            },
            move |r| crate::commands::spawn_for_tool(&b, r),
            Arc::clone(&spawned),
        );
        let mut started = start_for_watch(&self.state.tickets, &req, &port)?;
        for id in lock(&spawned).iter() {
            if !started.spawned.contains(id) {
                started.spawned.push(id.clone());
            }
        }
        Ok(started)
    }
}

impl AgentsPort for AppPort<'_> {
    fn live_of(&self, ids: &HashSet<String>) -> HashSet<String> {
        live_of(self.state, ids)
    }
}

impl Notifier for AppPort<'_> {
    fn notify(&self, key: NoticeKey, notice: Notice) {
        self.state.notices.push_all(vec![(key, notice)], now_ms());
    }
}

impl StatePort for AppPort<'_> {
    fn with_state<T>(&self, f: &mut dyn FnMut(&mut WatchState) -> T) -> T {
        self.state.watch.state.with(|s| f(s))
    }
}

impl WatchPort for AppPort<'_> {
    fn stopping(&self) -> bool {
        self.state.watch.is_stopping()
    }
}

/// Delmængden af `ids` der lever (managerens korte lås).
fn live_of(state: &AppState, ids: &HashSet<String>) -> HashSet<String> {
    lock(&state.manager)
        .list()
        .into_iter()
        .filter(|a| !matches!(a.status, AgentStatus::Exited { .. }) && ids.contains(&a.id))
        .map(|a| a.id)
        .collect()
}

// ---- plan, view, tick, timer ----

/// Planen for et tick: workspace-filen, app-indstillingerne (kort lås, kopi), projektlisten og
/// hvert projekts `project.json`, stoppede projekter og om agenter kan startes. Læses **før**
/// ethvert indbakke-kald; ingen lås holdes under fil-læsningen.
pub fn build_plan(state: &AppState) -> (TickPlan, WorkspaceSnapshot) {
    let ws = state.workspace.snapshot();
    let settings = lock(&state.settings).clone();
    let files: Vec<ProjectWatchFile> = crate::projects::list_projects(&state.paths.projects_root)
        .into_iter()
        .map(
            |p| match state.tickets.project_files.read(Path::new(&p.path)) {
                Ok(Some(f)) => ProjectWatchFile {
                    id: p.id,
                    cfg: f.watch,
                    notes: watch_notes(&f.notes),
                },
                Ok(None) => ProjectWatchFile {
                    id: p.id,
                    ..ProjectWatchFile::default()
                },
                Err(e) => ProjectWatchFile {
                    id: p.id,
                    cfg: None,
                    notes: vec![e],
                },
            },
        )
        .collect();
    let tripped = state.watch.state.read(WatchState::tripped);
    // claude slås kun op når et projekt overhovedet har vagten slået til.
    let wanted = files
        .iter()
        .any(|f| f.cfg.as_ref().is_some_and(|c| c.enabled));
    let can_spawn = wanted
        && state.pipe_ready.load(Ordering::Acquire)
        && state.paths.hook_exe.is_some()
        && find_claude().is_some();
    let playbooks = ws.config.playbook_kinds();
    let plan = active_projects(
        &ws.config.watch,
        &settings,
        &files,
        &tripped,
        can_spawn,
        &playbooks,
    );
    (plan, ws)
}

/// View uden sweep (efter "Hold vagt"/"Stop vagten"/"Genstart vagt" og før første tick): planen
/// og tilstanden læses på ny, hukommelsen kun som kopi (skrives ikke tilbage).
pub fn compute_view(state: &AppState) -> WatchView {
    let now = now_ms();
    let off = crate::gh::local_offset_secs((now / 1000) as i64);
    let (plan, _) = build_plan(state);
    let mut mem = state.watch.mem_copy();
    let ids: HashSet<String> = mem.agents.keys().cloned().collect();
    let live = live_of(state, &ids);
    mem.agents.retain(|id, _| live.contains(id));
    mem.waiting.retain(|_, w| {
        w.failed_until_later(now) || plan.projects.iter().any(|p| same_id(&p.id, &w.project))
    });
    let st = state.watch.state.read(WatchState::clone);
    build_view(&plan, &st, &mem, now, off)
}

/// Gemmer view'et og sender `watch-changed` (efter at låsen er sluppet).
fn publish(app: &AppHandle, rt: &WatchRuntime, view: &WatchView) {
    rt.store_view(view.clone());
    if let Err(e) = app.emit(WATCH_CHANGED, view) {
        log::debug!("emit {WATCH_CHANGED}: {e}");
    }
}

/// [`compute_view`] + publicér (IPC: `get_watch` før første tick, `set_watch`,
/// `restart_watch`).
pub fn refresh_view(app: &AppHandle) -> WatchView {
    let Some(state) = app.try_state::<AppState>() else {
        return WatchView::default();
    };
    let view = compute_view(&state);
    publish(app, &state.watch, &view);
    view
}

/// Venter (højst [`WATCH_REFRESH_WAIT_MAX_MS`], i skridt af [`WATCH_STOP_POLL_MS`]) til
/// hentningen er færdig; afbrydes af `stopping`. Ingen lås holdes.
fn wait_for_refresh(state: &AppState) {
    let deadline = Instant::now() + Duration::from_millis(WATCH_REFRESH_WAIT_MAX_MS);
    while state.tickets.inbox_rt.is_refreshing() {
        if state.watch.is_stopping() || Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(WATCH_STOP_POLL_MS));
    }
}

/// Beder om en hentning (`RefreshReason::Timer`: kildernes mindsteinterval og back-off gælder)
/// når den sidste er mindst 120 s gammel, og venter på den.
fn maybe_refresh(state: &AppState, now: u64) {
    let rt = &state.watch;
    let last = state.tickets.inbox_rt.status().last_refresh_at;
    let ask = *lock(&rt.last_refresh_ask);
    if should_refresh(last, ask, now) {
        *lock(&rt.last_refresh_ask) = Some(now);
        match state.tickets.inbox_refresh(RefreshReason::Timer) {
            Ok(true) => log::debug!("watch: indbakken hentes"),
            Ok(false) => log::debug!("watch: en hentning kører allerede"),
            Err(e) => log::warn!("watch: indbakken kunne ikke hentes: {e}"),
        }
    }
    if !rt.is_stopping() {
        wait_for_refresh(state);
    }
}

/// Ét tick (på en blocking-tråd). Højst ét ad gangen (`tick_busy`); en panik logges og
/// frigiver `tick_busy` — kun i debug og tests (review6d W4): release har `panic = "abort"`
/// (rodens `Cargo.toml`), så dér ender en panik processen som enhver anden panik i appen.
/// Hukommelsen kopieres ud og ind, og `watch-state.json` skrives atomisk, så ingen af dem er
/// nogensinde halvt skrevet.
pub fn tick(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let rt = &state.watch;
    if rt.is_stopping() {
        return;
    }
    if rt.tick_busy.swap(true, Ordering::AcqRel) {
        log::debug!("watch: forrige tick kører stadig");
        return;
    }
    if catch_unwind(AssertUnwindSafe(|| run_tick(app, &state))).is_err() {
        log::error!("watch: tick panicked");
    }
    rt.tick_busy.store(false, Ordering::Release);
}

fn run_tick(app: &AppHandle, state: &AppState) {
    let rt = &state.watch;
    // (1) Konfigurationen, før ethvert indbakke-kald.
    let (plan, ws) = build_plan(state);
    let now = now_ms();
    let port = AppPort::new(app, state, ws);
    let off = port.offset_secs(now);
    // (2) Husholdning: tilladelser og trust-dialoger der har ventet over et minut.
    let pending = lock(&state.pending).list();
    let agents = lock(&state.manager).list();
    state
        .notices
        .push_all(derive_waiting_notices(&pending, &agents, now), now);
    // (3) Intet aktivt vagt-projekt: kun view (ingen hentning, ingen sweep).
    let mut mem = rt.mem_copy();
    if plan.projects.is_empty() {
        let ids: HashSet<String> = mem.agents.keys().cloned().collect();
        let live = port.live_of(&ids);
        mem.agents.retain(|id, _| live.contains(id));
        // Review6d N11: et fejlet start husker sin time også gennem en pause.
        mem.waiting.retain(|_, w| w.failed_until_later(now));
        mem.last_tick_at = Some(now);
        let st = rt.state.read(WatchState::clone);
        let view = build_view(&plan, &st, &mem, now, off);
        rt.store_mem(mem);
        publish(app, rt, &view);
        return;
    }
    // (4) Hentning (højst hvert 120 s) og ventetid — kun når mindst ét projekt kan starte nu
    // (ellers ingen `gh`-kald); `stopping` før og under.
    if rt.is_stopping() {
        return;
    }
    if rt.state.read(|s| can_start_anything(&plan, s, now, off)) {
        maybe_refresh(state, now);
    }
    if rt.is_stopping() {
        return;
    }
    // (5) Motoren, også når alt venter på budgettet: så får de ventende emner deres
    // "venter på budget (næste: HH:MM)" og brugeren én besked pr. time (intet startes, for
    // reservationen siger nej). Kun indbakke-dokumentet læses; intet netværk.
    //
    // Review6d N8: en panik i motoren må ikke tabe hukommelsen (fx agent-id'er fra et start
    // lige før panikken); den gemmes også da, og panikken logges som én linje. Det gælder kun
    // debug og tests (review6d W4): i release (`panic = "abort"`) ender processen.
    let view = WatchEngine::new(port).tick_guarded(&mut mem, &plan);
    rt.store_mem(mem);
    if let Some(view) = view {
        publish(app, rt, &view);
    }
}

/// Timeren (én task): første tick efter [`WATCH_TICK_SECS`], derefter hvert
/// [`WATCH_TICK_SECS`] (`Delay`: efter dvale ét tick, ingen byge). Hvert tick kører i
/// `spawn_blocking`; løkken slutter når appen lukker.
pub fn start(app: AppHandle) -> JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        let period = Duration::from_secs(WATCH_TICK_SECS);
        let mut iv = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        log::info!("watch: timer startet ({WATCH_TICK_SECS} s)");
        let stopping = |app: &AppHandle| {
            app.try_state::<AppState>()
                .is_some_and(|s| s.watch.is_stopping())
        };
        loop {
            iv.tick().await;
            if stopping(&app) {
                break;
            }
            let a = app.clone();
            if let Err(e) = tauri::async_runtime::spawn_blocking(move || tick(&a)).await {
                log::error!("watch: tick failed: {e}");
            }
            if stopping(&app) {
                break;
            }
        }
        log::info!("watch: timer stoppet");
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentManager;
    use crate::inbox::test_support::{folder_item, github_item, FolderEnv};
    use crate::tickets::model::TicketState;

    fn runtime() -> (WatchRuntime, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("mira-watch-rt-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (file, warning) = WatchStateFile::load(&dir);
        (WatchRuntime::new(file, warning), dir)
    }

    #[test]
    fn shutdown_is_idempotent_and_aborts_once() {
        let (rt, dir) = runtime();
        assert!(!rt.is_stopping());
        let ran = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ran);
        rt.set_handle(tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            flag.store(true, Ordering::Release);
        }));
        rt.shutdown();
        assert!(rt.is_stopping());
        assert!(
            lock(&rt.handle).is_none(),
            "the handle was taken and aborted"
        );
        rt.shutdown();
        assert!(rt.is_stopping());
        std::thread::sleep(Duration::from_millis(600));
        assert!(
            !ran.load(Ordering::Acquire),
            "the aborted task never finished"
        );
        // En handle der kommer efter shutdown, afbrydes straks og gemmes ikke.
        let late = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&late);
        rt.set_handle(tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            flag.store(true, Ordering::Release);
        }));
        assert!(lock(&rt.handle).is_none());
        std::thread::sleep(Duration::from_millis(400));
        assert!(!late.load(Ordering::Acquire));
        assert_eq!(rt.active(), 0);
        assert!(rt.view().is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn wrapped_port_refuses_staff_and_counts_down() {
        assert_eq!(
            watch_spawn_guard(Some(Role::Planner), 2, false),
            Err("vagten starter ikke stabsagenter".to_string())
        );
        for staff in [Role::Reviewer, Role::Coordinator] {
            assert!(watch_spawn_guard(Some(staff), 2, false).is_err());
        }
        assert!(
            watch_spawn_guard(None, 2, false).is_err(),
            "unknown profile"
        );
        assert_eq!(
            watch_spawn_guard(Some(Role::Coder), 0, false),
            Err("vagtens agentloft er nået".to_string())
        );
        assert_eq!(
            watch_spawn_guard(Some(Role::Coder), 2, true),
            Err("Appen lukker — ingen nye agenter".to_string())
        );
        assert_eq!(watch_spawn_guard(Some(Role::Debugger), 1, false), Ok(()));

        // Den omsluttede port: tæller ned, tvinger arbejdsplads, registrerer id'er.
        let mut m = AgentManager::new(5);
        let calls: Arc<Mutex<Vec<SpawnByProfile>>> = Arc::default();
        let infos: Vec<AgentInfo> = (0..3)
            .map(|i| {
                let id = m.insert_fake(&format!("s{i}"), "/w/x");
                m.get(&id).unwrap()
            })
            .collect();
        let log = Arc::clone(&calls);
        let next = Mutex::new(infos.clone().into_iter());
        let stop = Arc::new(AtomicBool::new(false));
        let stop_read = Arc::clone(&stop);
        let spawned: Arc<Mutex<Vec<String>>> = Arc::default();
        let port = watch_spawn_port(
            2,
            move || stop_read.load(Ordering::Acquire),
            move |r| {
                lock(&log).push(r);
                Ok(lock(&next).next().unwrap())
            },
            Arc::clone(&spawned),
        );
        let req = |profile: &str| SpawnByProfile {
            profile_id: profile.into(),
            seat_kind: None,
            first_ticket_id: None,
            project: None,
        };
        assert_eq!(
            port(req("planner")).unwrap_err(),
            "vagten starter ikke stabsagenter"
        );
        assert!(port(req("debugger")).is_ok());
        assert!(port(req("coder")).is_ok());
        assert_eq!(port(req("coder")).unwrap_err(), "vagtens agentloft er nået");
        let made = lock(&calls).clone();
        assert_eq!(made.len(), 2, "refused requests never reach spawn_for_tool");
        assert!(made.iter().all(|r| r.seat_kind == Some(SeatKind::Work)));
        assert_eq!(*lock(&spawned), [infos[0].id.clone(), infos[1].id.clone()]);
        // Appen lukker: afvist før spawn.
        let calls2: Arc<Mutex<usize>> = Arc::default();
        let c2 = Arc::clone(&calls2);
        let stop2 = Arc::clone(&stop);
        let info = infos[2].clone();
        let port = watch_spawn_port(
            5,
            move || stop2.load(Ordering::Acquire),
            move |_| {
                *lock(&c2) += 1;
                Ok(info.clone())
            },
            Arc::default(),
        );
        stop.store(true, Ordering::Release);
        assert_eq!(
            port(req("coder")).unwrap_err(),
            "Appen lukker — ingen nye agenter"
        );
        assert_eq!(*lock(&calls2), 0);
    }

    #[test]
    fn start_failure_classification() {
        assert_eq!(classify_start_error(INBOX_ITEM_GONE), StartFailure::Gone);
        let started = TicketError::ExternalAlreadyStarted("ab12cd34".into()).to_string();
        assert_eq!(classify_start_error(&started), StartFailure::Gone);
        // Review6d W1: et issue lukket mellem hentning og start er væk, ikke en fejl.
        assert_eq!(
            classify_start_error("Issuen er lukket på GitHub; den startes ikke"),
            StartFailure::Gone
        );
        assert_eq!(classify_start_error(INBOX_ISSUE_CLOSED), StartFailure::Gone);
        // Review6d N2: reelle fejl er korte og uden gh's egen stderr.
        assert_eq!(
            classify_start_error("gh: netværk\nmere"),
            StartFailure::Real(crate::config::WATCH_GH_ERROR_TEXT.into())
        );
        assert_eq!(
            classify_start_error("svaret fra gh var for stort"),
            StartFailure::Real("svaret fra gh var for stort".into())
        );
        // Review6d W3: gh/netværket svigter → kilden, ikke en fejl der tæller.
        use crate::gh::{error_text, GhError};
        for g in [
            GhError::GhMissing,
            GhError::NotLoggedIn,
            GhError::BadCredentials,
            GhError::RateLimited,
            GhError::Network,
            GhError::Timeout,
        ] {
            let e = error_text(&g, "o/r");
            assert_eq!(
                classify_start_error(&e),
                StartFailure::Source(e.clone()),
                "{g:?}"
            );
        }
        // Review6d N19: 5xx fra GitHub er kilden; andre gh-fejl er reelle.
        assert!(matches!(
            classify_start_error("gh: HTTP 502: Bad Gateway (https://api.github.com/...)"),
            StartFailure::Source(_)
        ));
        for g in [
            GhError::RepoNotFound,
            GhError::IssuesDisabled,
            GhError::TooLarge,
            GhError::BadJson("x".into()),
            GhError::Other("y".into()),
        ] {
            let e = error_text(&g, "o/r");
            assert!(
                matches!(classify_start_error(&e), StartFailure::Real(_)),
                "{g:?}"
            );
        }
    }

    #[test]
    fn source_health_from_the_inbox_status() {
        use crate::inbox::source::{SourceError, SourceId, SourceStatus};
        let gh = SourceId::github("O/R");
        let mut status = InboxStatus::default();
        let mut s = SourceStatus::new(&gh, "o/r".into(), Some("web".into()));
        s.succeeded(1_000, &crate::inbox::source::Fetched::default());
        status.sources.push(s.clone());
        // Ok og ukendt kilde: intet blokerer.
        let h = source_health_of(&status, "github:o/r");
        assert_eq!(
            h,
            SourceHealth {
                last_fetch_at: Some(1_000),
                ..SourceHealth::default()
            }
        );
        assert_eq!(
            source_health_of(&status, "folder:web"),
            SourceHealth::default()
        );
        // Logget ud: venter på "Opdatér".
        s.failed(
            2_000,
            &SourceError::new(SourceErrorKind::NotLoggedIn, "gh er ikke logget ind"),
        );
        status.sources[0] = s.clone();
        let h = source_health_of(&status, "github:o/r");
        assert_eq!(h.error.as_deref(), Some("gh er ikke logget ind"));
        assert!(h.waits_for_user);
        assert_eq!((h.next_retry_at, h.last_fetch_at), (None, Some(1_000)));
        // Rate limit: back-off med tidspunkt.
        s.failed(
            3_000,
            &SourceError::new(SourceErrorKind::RateLimited, "GitHub: rate limit"),
        );
        status.sources[0] = s;
        let h = source_health_of(&status, "github:o/r");
        assert!(!h.waits_for_user);
        assert_eq!(h.next_retry_at, Some(3_000 + 900_000));
        assert!(h.error.unwrap().starts_with("GitHub: rate limit"));
    }

    #[test]
    fn refresh_at_most_every_two_minutes() {
        let now = 10_000_000;
        assert!(should_refresh(None, None, now));
        assert!(!should_refresh(Some(now - 119_999), None, now));
        assert!(should_refresh(Some(now - 120_000), None, now));
        assert!(!should_refresh(None, Some(now - 60_000), now));
        assert!(should_refresh(
            Some(now - 300_000),
            Some(now - 120_000),
            now
        ));
        // Uret gik baglæns: ingen byge (afstanden tæller som 0).
        assert!(!should_refresh(Some(now + 5_000), None, now));
    }

    fn summary(item: &crate::inbox::InboxItem) -> InboxItemSummary {
        InboxItemSummary::from(item)
    }

    #[test]
    fn candidates_exclude_started_dismissed_gone_and_ambiguous() {
        let mut old = github_item("old", 1);
        old.seen_at = 5;
        let mut new = github_item("new", 2);
        new.seen_at = 9;
        let mut started = github_item("started", 3);
        started.state = InboxState::Started;
        let mut dismissed = github_item("dismissed", 4);
        dismissed.state = InboxState::Dismissed;
        let mut ambiguous = github_item("ambiguous", 5);
        ambiguous.project = None;
        ambiguous.candidates = vec!["web".into(), "api".into()];
        let mut both = github_item("both", 6);
        both.candidates = vec!["web".into(), "api".into()];
        let mut other = github_item("other", 7);
        other.project = Some("api".into());
        let mut upper = github_item("upper", 8);
        upper.project = Some("WEB".into());
        upper.seen_at = 9;
        let items: Vec<InboxItemSummary> = [
            &new, &started, &dismissed, &ambiguous, &both, &other, &upper, &old,
        ]
        .into_iter()
        .map(summary)
        .collect();
        let ids: Vec<String> = watch_candidates(&items, "web")
            .into_iter()
            .map(|c| c.item_id)
            .collect();
        assert_eq!(ids, ["old", "new", "upper"]);

        // Gennem dokumentet: `gone` kommer aldrig med, og hverken kandidaterne eller dublet-
        // tjekket læser filer (de kører her under `inbox_lock`, hvor en fil-læsning går i panik).
        let mut gone = github_item("gone", 9);
        gone.gone = true;
        let env = FolderEnv::new(vec![old.clone(), gone, started]);
        let ctx = &env.t.ctx;
        {
            let _serial = ctx.lock_inbox_serial();
            let ids: Vec<String> = candidates_in(ctx, "web")
                .into_iter()
                .map(|c| c.item_id)
                .collect();
            assert_eq!(ids, ["old"]);
            assert!(!is_duplicate_in(ctx, "web", "Issue 1"));
        }
        ctx.mutate(|s| {
            s.create_in(
                "Issue 1",
                "",
                false,
                Some(ProjectRef::Existing("web".into())),
                None,
                1,
            )
        })
        .unwrap();
        let _serial = ctx.lock_inbox_serial();
        assert!(is_duplicate_in(ctx, "web", "Issue 1"));
        assert!(!is_duplicate_in(ctx, "api", "Issue 1"));
    }

    #[test]
    fn watch_start_uses_review_playbook_and_project_and_is_gone_the_second_time() {
        let env = FolderEnv::new(vec![folder_item("i1", "fejl.md")]);
        env.write("fejl.md", "Trin 1");
        let ctx = &env.t.ctx;
        // `reviewByDefault: false` i workspace: børnene reviewes alligevel.
        std::fs::write(ctx.workspace.path(), r#"{"reviewByDefault": false}"#).unwrap();
        assert!(!ctx.workspace.rules().review_by_default);
        let manager = Arc::clone(&ctx.manager);
        let me = Arc::clone(ctx);
        let spawned: Arc<Mutex<Vec<String>>> = Arc::default();
        let port = watch_spawn_port(
            2,
            || false,
            move |r: SpawnByProfile| {
                let role = Role::parse(&r.profile_id).unwrap();
                let id = lock(&manager).insert_fake_in(
                    &uuid::Uuid::new_v4().to_string(),
                    "/w/web",
                    &[role],
                    SeatKind::Work,
                    Some("web"),
                );
                if let Some(first) = &r.first_ticket_id {
                    me.mutate(|s| s.assign(first, &id, 5)).unwrap();
                }
                Ok(lock(&manager).get(&id).unwrap())
            },
            Arc::clone(&spawned),
        );
        let req = WatchStart {
            item_id: "i1".into(),
            project: "web".into(),
            playbook: "bug".into(),
            max_agents_left: 2,
        };
        let st = start_for_watch(ctx, &req, &port).unwrap();
        assert_eq!(st.spawned.len(), 2, "{:?}", st.notes);
        assert_eq!(st.spawned, *lock(&spawned));
        let parent = ctx.read(|s| s.get(&st.ticket_id)).unwrap();
        assert_eq!(parent.kind.as_deref(), Some("bug"));
        assert!(!parent.skip_review);
        assert_eq!(
            parent.project.as_ref().map(|p| p.name().to_string()),
            Some("web".into())
        );
        assert_ne!(parent.state, TicketState::Done);
        assert_eq!(
            parent.history.last().and_then(|h| h.note.as_deref()),
            Some("startet af vagten (forløb «bug»)")
        );
        let children: Vec<_> = ctx.read(|s| {
            s.list()
                .into_iter()
                .filter(|t| t.parent_id.as_deref() == Some(parent.id.as_str()))
                .collect()
        });
        assert_eq!(children.len(), 2);
        for c in &children {
            let full = ctx.read(|s| s.get(&c.id)).unwrap();
            assert!(!full.skip_review, "child {} is reviewed", c.id);
        }
        let item = ctx.inbox_read(|i| i.get("i1")).unwrap();
        assert_eq!(item.state, InboxState::Started);
        // Samme emne igen (to ticks, eller tick + brugerens Start): `Gone`, ingen ny ticket.
        let n = ctx.read(|s| s.len());
        assert_eq!(start_for_watch(ctx, &req, &port), Err(StartFailure::Gone));
        assert_eq!(ctx.read(|s| s.len()), n);
    }
}
