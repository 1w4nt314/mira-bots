//! Vagtens motor (trin 6d, plan6d punkt 16, A.2, C6d.4): en ren [`WatchEngine`] som
//! `Dispatcher`. Alle bivirkninger går gennem porten [`WatchPort`], der samler [`Clock`],
//! [`InboxPort`], [`StarterPort`], [`AgentsPort`], [`Notifier`] og [`StatePort`]. Motoren læser
//! aldrig filer, tager ingen låse og kender hverken Tauri eller `AppState`; skallen
//! (`watch::runtime`) bygger [`TickPlan`] (project.json, workspace, app-indstillinger) **før**
//! ethvert indbakke-kald og giver motoren en port.
//!
//! Pr. tick og pr. aktivt vagt-projekt (i projektlistens rækkefølge) højst ét vellykket start:
//! kandidaterne (nye emner i projektet, ældste først) gennemgås i rækkefølge — `stopping` →
//! stop alt; dublet → park; ingen/ukendt playbook → park; bemanding (en levende agent eller
//! plads til at starte en arbejdsagent; aldrig en stabsagent) → park; budget (reservation under
//! én kort lås) → park + besked og ingen flere reservationer i projektet; ellers start. Et emne
//! der forsvandt (`Gone`) annullerer reservationen uden fejl; en reel fejl annullerer og tæller
//! (tre i træk stopper vagten for projektet).

use std::collections::{BTreeMap, BTreeSet, HashSet};

use serde::{Deserialize, Serialize};

use crate::agent::{AgentInfo, Role};
use crate::app_settings::AppSettings;
use crate::config::{
    watch_start_failed_text, watch_tripped_reason, watch_unknown_playbook_text,
    watch_wait_budget_text, WATCH_REASON_CANNOT_SPAWN, WATCH_REASON_NOT_ENABLED,
    WATCH_REASON_PAUSED, WATCH_REASON_PROJECT_PAUSED, WATCH_REASON_WS_OFF, WATCH_TRIP_AFTER,
    WATCH_WAIT_DUPLICATE, WATCH_WAIT_NO_PLAYBOOK, WATCH_WAIT_PLANNER, WATCH_WAIT_SEAT,
};
use crate::notices::{budget_notice, tripped_notice, Notice, NoticeKey};
use crate::projects::same_id;
use crate::tickets::model::short_id;
use crate::tickets::playbook::pick_agent;
use crate::watch::budget::{local_minute, Caps, Ring, Verdict};
use crate::watch::config::{
    in_quiet, pick_playbook, quiet_text, PlaybookRule, WatchConfig, WorkspaceWatch,
};
use crate::watch::state::WatchState;

// ---- porte ----

/// Vægur og lokal tid (injiceret; tests har et manuelt ur).
pub trait Clock {
    fn now_ms(&self) -> u64;
    /// Lokal tid − UTC i sekunder ved `now_ms`.
    fn offset_secs(&self, now_ms: u64) -> i64;
}

/// Indbakken set fra vagten.
pub trait InboxPort {
    /// Emnerne vagten må overveje i `project` (C6d.4): `state == new`, ikke `gone`, projektet
    /// er `project` (`same_id`) og ingen tvetydige kandidat-projekter; ældste `seenAt` først.
    /// `started`/`dismissed`/`gone` kommer aldrig med.
    fn candidates(&self, project: &str) -> Vec<Candidate>;
    /// Ligner `title` en åben ticket i `project` (Start-dialogens "Ligner ticket …")?
    fn is_duplicate(&self, project: &str, title: &str) -> bool;
}

/// Bemanding og selve startet.
pub trait StarterPort {
    /// Kan playbookens trin bemandes (A.5)? `watch_free` = `watch.maxAgents` minus projektets
    /// levende vagt-agenter.
    fn staffing(&self, project: &str, playbook: &str, watch_free: usize) -> Staffing;
    /// Starter emnet som ticket med forløbet (`inbox_start` + `start_playbook_with`).
    fn start(&self, req: WatchStart) -> Result<Started, StartFailure>;
}

/// Agenterne.
pub trait AgentsPort {
    /// Delmængden af `ids` der stadig lever (ikke `Exited`).
    fn live_of(&self, ids: &HashSet<String>) -> HashSet<String>;
}

/// Beskeder (køen; OS-toast senere bag samme port).
pub trait Notifier {
    fn notify(&self, key: NoticeKey, notice: Notice);
}

/// `watch-state.json` (budget, fejl, stop). Én kort lås pr. kald; intet andet kald sker under
/// den.
pub trait StatePort {
    fn with_state<T>(&self, f: &mut dyn FnMut(&mut WatchState) -> T) -> T;
}

/// Alle porte samlet, plus om appen lukker.
pub trait WatchPort: Clock + InboxPort + StarterPort + AgentsPort + Notifier + StatePort {
    fn stopping(&self) -> bool;
}

/// Et nyt emne vagten kan starte.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub item_id: String,
    pub title: String,
    pub labels: Vec<String>,
    /// Mappe-emnets frontmatter `kind:` (valideret); vinder over `byLabel`.
    pub ticket_kind: Option<String>,
    pub seen_at: u64,
}

/// Svar fra [`StarterPort::staffing`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Staffing {
    Ok,
    /// Et trin kræver en stabsrolle uden levende agent (vagten starter aldrig stabsagenter).
    MissingStaff(Role),
    /// Arbejdspladser, projektloft eller `watch.maxAgents` slår ikke til.
    NoSeat,
}

/// Et start (motoren → porten).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchStart {
    pub item_id: String,
    pub project: String,
    pub playbook: String,
    /// Højst så mange agenter må den omsluttede spawn-port starte.
    pub max_agents_left: usize,
}

/// Et vellykket start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Started {
    pub ticket_id: String,
    /// Agenter startet for forløbet (tælles som vagt-agenter).
    pub spawned: Vec<String>,
    pub notes: Vec<String>,
}

/// Et start der ikke lykkedes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartFailure {
    /// Emnet er ikke længere nyt (startet/afvist/væk, allerede startet som ticket, eller appen
    /// lukker): ingen ticket, ingen fejl — reservationen annulleres.
    Gone,
    /// En reel fejl uden ticket: reservationen annulleres, og fejlen tælles.
    Real(String),
    /// Ticketen blev oprettet, men forløbet kunne ikke rulles ud: budgettet er brugt
    /// (reservationen bekræftes), og fejlen tælles.
    AfterTicket { ticket_id: String, error: String },
}

// ---- plan ----

/// Et projekts `project.json`-del som skallen læste den (før ethvert indbakke-kald).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectWatchFile {
    pub id: String,
    /// `None`: ingen `watch` (eller ingen `project.json`).
    pub cfg: Option<WatchConfig>,
    /// `project.json: watch…`-noterne (Diagnostik).
    pub notes: Vec<String>,
}

/// Et aktivt vagt-projekt (research §5.3: alle betingelser sande).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveProject {
    pub id: String,
    pub cfg: WatchConfig,
    pub ws: WorkspaceWatch,
    /// Playbook-navnene i workspace.
    pub playbooks: Vec<String>,
}

/// Et projekt i view'et (fil med `watch` eller i `watchOff`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShownProject {
    pub id: String,
    pub cfg: Option<WatchConfig>,
    /// I `watchOff` ("Hold vagt" slået fra).
    pub paused: bool,
    pub notes: Vec<String>,
}

/// Hvad et tick skal arbejde med.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TickPlan {
    pub projects: Vec<ActiveProject>,
    /// `watchPaused` ("Stop vagten").
    pub paused: bool,
    /// `(projekt, årsag)` for de viste projekter der ikke er aktive.
    pub inactive: Vec<(String, String)>,
    pub shown: Vec<ShownProject>,
    pub ws: WorkspaceWatch,
}

/// Er `id` i `set` (uden hensyn til store/små bogstaver)?
fn contains_id<'a>(mut set: impl Iterator<Item = &'a String>, id: &str) -> bool {
    set.any(|x| same_id(x, id))
}

/// De aktive vagt-projekter (research §5.3): workspace `watch.enabled` ∧ `!watchPaused` ∧
/// `project.json` `watch.enabled == true` ∧ projektet ∉ `watchOff` ∧ ikke stoppet ∧ agenter kan
/// startes (`pipe_ready` ∧ claude ∧ hook). Den første betingelse der svigter, giver årsagen
/// (C6d.5). Ren.
pub fn active_projects(
    ws: &WorkspaceWatch,
    settings: &AppSettings,
    files: &[ProjectWatchFile],
    tripped: &BTreeSet<String>,
    can_spawn: bool,
    playbooks: &[String],
) -> TickPlan {
    let mut plan = TickPlan {
        paused: settings.watch_paused,
        ws: *ws,
        ..TickPlan::default()
    };
    for f in files {
        let off = contains_id(settings.watch_off.iter(), &f.id);
        if f.cfg.is_none() && !off {
            continue;
        }
        plan.shown.push(ShownProject {
            id: f.id.clone(),
            cfg: f.cfg.clone(),
            paused: off,
            notes: f.notes.clone(),
        });
        let reason = match &f.cfg {
            None => Some(WATCH_REASON_NOT_ENABLED.to_string()),
            Some(c) if !c.enabled => Some(WATCH_REASON_NOT_ENABLED.to_string()),
            Some(_) if !ws.enabled => Some(WATCH_REASON_WS_OFF.to_string()),
            Some(_) if settings.watch_paused => Some(WATCH_REASON_PAUSED.to_string()),
            Some(_) if off => Some(WATCH_REASON_PROJECT_PAUSED.to_string()),
            Some(_) if contains_id(tripped.iter(), &f.id) => {
                Some(watch_tripped_reason(WATCH_TRIP_AFTER))
            }
            Some(_) if !can_spawn => Some(WATCH_REASON_CANNOT_SPAWN.to_string()),
            Some(_) => None,
        };
        match (reason, &f.cfg) {
            (None, Some(cfg)) => plan.projects.push(ActiveProject {
                id: f.id.clone(),
                cfg: cfg.clone(),
                ws: *ws,
                playbooks: playbooks.to_vec(),
            }),
            (Some(r), _) => plan.inactive.push((f.id.clone(), r)),
            (None, None) => {}
        }
    }
    plan
}

// ---- hukommelse og view ----

/// Hvorfor et emne venter (wire camelCase).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum WaitReason {
    Budget,
    Seat,
    Planner,
    Duplicate,
    Playbook,
    Failed,
}

impl WaitReason {
    pub fn as_str(self) -> &'static str {
        match self {
            WaitReason::Budget => "budget",
            WaitReason::Seat => "seat",
            WaitReason::Planner => "planner",
            WaitReason::Duplicate => "duplicate",
            WaitReason::Playbook => "playbook",
            WaitReason::Failed => "failed",
        }
    }
}

/// Et parkeret emne (C6d.2 `waiting`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Waiting {
    pub reason: WaitReason,
    /// Badge-teksten (C6d.5).
    pub text: String,
    /// Budget: hvornår det tidligst er frit (UTC-ms).
    pub next_at: Option<u64>,
    pub project: String,
}

/// Vagtens hukommelse (i runtime; aldrig på disk).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WatchMemory {
    /// Emne-id → hvorfor det venter.
    pub waiting: BTreeMap<String, Waiting>,
    /// Agent-id → projekt for de agenter vagten har startet (beskæres hvert tick til dem der
    /// lever).
    pub agents: BTreeMap<String, String>,
    pub last_tick_at: Option<u64>,
}

impl WatchMemory {
    /// Levende vagt-agenter i `project` (efter seneste beskæring).
    pub fn agents_in(&self, project: &str) -> usize {
        self.agents.values().filter(|p| same_id(p, project)).count()
    }
}

/// Brugt/loft for time og dag.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BudgetView {
    pub used_hour: u32,
    pub cap_hour: u32,
    pub used_day: u32,
    pub cap_day: u32,
}

/// Ét projekt i `WatchView` (C6d.2/C6d.6).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WatchProjectView {
    pub id: String,
    /// `project.json` har `watch.enabled: true` ("Hold vagt" kan slås til).
    pub enabled: bool,
    /// I `watchOff`.
    pub paused: bool,
    pub active: bool,
    /// Hvorfor projektet ikke er aktivt (C6d.5); `None` når det er.
    pub reason: Option<String>,
    pub tripped: bool,
    pub tripped_at: Option<u64>,
    pub tripped_reason: Option<String>,
    pub used_hour: u32,
    pub cap_hour: u32,
    pub used_day: u32,
    pub cap_day: u32,
    /// Levende vagt-agenter i projektet.
    pub agents: usize,
    pub max_agents: usize,
    /// Hvornår budgettet tidligst er frit, når det venter nu.
    pub next_free_at: Option<u64>,
    pub quiet: Option<String>,
    pub in_quiet: bool,
    /// Kort beskrivelse af reglen (`bug`, `byLabel (2) → task`); `None`: ingen playbook.
    pub playbook: Option<String>,
    pub notes: Vec<String>,
}

/// `get_watch`/`set_watch`/`restart_watch` og eventet `watch-changed` (C6d.2).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WatchView {
    pub paused: bool,
    /// Antal aktive vagt-projekter.
    pub active: usize,
    pub last_tick_at: Option<u64>,
    /// Summen over alle projekter mod workspace-lofterne.
    pub global: BudgetView,
    pub projects: Vec<WatchProjectView>,
    /// Emne-id → hvorfor det venter.
    pub waiting: BTreeMap<String, Waiting>,
}

/// `HH:MM` i lokal tid (offset ved `now`; ±1 t over en sommertidsovergang, dokumenteret).
pub fn hhmm(ms: u64, off: i64) -> String {
    let m = local_minute(ms, off);
    format!("{:02}:{:02}", m / 60, m % 60)
}

/// Reglen som kort tekst til Diagnostik.
pub fn playbook_text(rule: &PlaybookRule) -> Option<String> {
    match rule {
        PlaybookRule::None => None,
        PlaybookRule::Fixed(n) => Some(n.clone()),
        PlaybookRule::ByLabel { by_label, default } => Some(format!(
            "byLabel ({}) → {}",
            by_label.len(),
            default
                .as_deref()
                .unwrap_or(crate::watch::config::NO_PLAYBOOK)
        )),
    }
}

fn caps_of(cfg: &WatchConfig, ws: &WorkspaceWatch) -> Caps {
    Caps {
        per_hour: cfg.max_per_hour,
        per_day: cfg.max_per_day,
        ws_per_hour: ws.max_per_hour,
        ws_per_day: ws.max_per_day,
    }
}

/// `(brugt i timen, brugt i dag)` for en ring (på en kopi; rører ikke tilstanden).
fn used(ring: &Ring, now: u64, off: i64) -> (u32, u32) {
    let mut r = ring.clone();
    r.normalise(now, off);
    (r.starts.len() as u32, r.day_count)
}

/// Kan noget overhovedet startes nu? `false` når alle aktive projekter venter på budgettet
/// (stille timer, dags- eller timeloft); så springer skallen indbakke-hentningen over. Ren
/// (regner på en kopi).
pub fn can_start_anything(plan: &TickPlan, state: &WatchState, now: u64, off: i64) -> bool {
    let mut s = state.clone();
    plan.projects
        .iter()
        .any(|p| s.check(&p.id, now, off, caps_of(&p.cfg, &p.ws), p.cfg.quiet) == Verdict::Go)
}

/// `WatchView` fra planen, tilstanden og hukommelsen. Ren (regner på en kopi af tilstanden).
pub fn build_view(
    plan: &TickPlan,
    state: &WatchState,
    mem: &WatchMemory,
    now: u64,
    off: i64,
) -> WatchView {
    let mut s = state.clone();
    let (gh, gd) = used(&s.global, now, off);
    let minute = local_minute(now, off);
    let projects = plan
        .shown
        .iter()
        .map(|sp| {
            let cfg = sp.cfg.clone().unwrap_or_default();
            let caps = caps_of(&cfg, &plan.ws);
            let (cap_hour, cap_day) = caps.effective();
            let ps = s.projects.get(&sp.id).cloned().unwrap_or_default();
            let (used_hour, used_day) = used(&ps.ring, now, off);
            let next_free_at = if cfg.enabled {
                match s.check(&sp.id, now, off, caps, cfg.quiet) {
                    Verdict::Wait { next_ms, .. } => next_ms,
                    Verdict::Go => None,
                }
            } else {
                None
            };
            let reason = plan
                .inactive
                .iter()
                .find(|(id, _)| same_id(id, &sp.id))
                .map(|(_, r)| r.clone());
            WatchProjectView {
                id: sp.id.clone(),
                enabled: cfg.enabled,
                paused: sp.paused,
                active: plan.projects.iter().any(|p| same_id(&p.id, &sp.id)),
                reason,
                tripped: ps.tripped_at.is_some(),
                tripped_at: ps.tripped_at,
                tripped_reason: ps.tripped_reason.clone(),
                used_hour,
                cap_hour,
                used_day,
                cap_day,
                agents: mem.agents_in(&sp.id),
                max_agents: cfg.max_agents,
                next_free_at,
                quiet: cfg.quiet.map(quiet_text),
                in_quiet: cfg.quiet.is_some_and(|q| in_quiet(q, minute)),
                playbook: playbook_text(&cfg.playbook),
                notes: sp.notes.clone(),
            }
        })
        .collect();
    WatchView {
        paused: plan.paused,
        active: plan.projects.len(),
        last_tick_at: mem.last_tick_at,
        global: BudgetView {
            used_hour: gh,
            cap_hour: plan.ws.max_per_hour,
            used_day: gd,
            cap_day: plan.ws.max_per_day,
        },
        projects,
        waiting: mem.waiting.clone(),
    }
}

// ---- bemanding (ren; bruges af AppPort og af testenes Fake) ----

/// Pladserne et start kan bruge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeatRoom {
    /// Frie arbejdspladser (`maxWorkAgents` minus levende arbejdsagenter).
    pub work_free: usize,
    /// Levende arbejdsagenter i projektet.
    pub in_project: usize,
    /// `maxAgentsPerProject` (0 = ubegrænset).
    pub max_per_project: usize,
}

/// A.5: hvert trin skal have en levende agent ([`pick_agent`]), ellers skal rollen være en
/// arbejdsrolle, og der skal være plads til én ny agent pr. manglende rolle (som
/// `start_playbook_with` starter): frie arbejdspladser, projektloftet og `watch_free`. En
/// manglende stabsrolle vinder (vagten starter aldrig stabsagenter).
pub fn staffing_for(
    roles: &[Role],
    agents: &[AgentInfo],
    project: &str,
    room: SeatRoom,
    watch_free: usize,
) -> Staffing {
    let mut missing: BTreeSet<Role> = BTreeSet::new();
    for &role in roles {
        if pick_agent(agents, role, Some(project)).is_some() {
            continue;
        }
        if !role.is_work() {
            return Staffing::MissingStaff(role);
        }
        missing.insert(role);
    }
    let n = missing.len();
    let project_full = room.max_per_project > 0 && room.in_project + n > room.max_per_project;
    if n > 0 && (n > watch_free || n > room.work_free || project_full) {
        return Staffing::NoSeat;
    }
    Staffing::Ok
}

// ---- motoren ----

/// Den rene motor; `port` er alt den kan gøre.
pub struct WatchEngine<P: WatchPort> {
    pub port: P,
}

impl<P: WatchPort> WatchEngine<P> {
    pub fn new(port: P) -> Self {
        WatchEngine { port }
    }

    /// Ét tick (C6d.4): beskær vagt-agenterne, gennemgå de aktive projekter i rækkefølge (højst
    /// ét start pr. projekt) og byg view'et. `mem` er runtime's hukommelse.
    pub fn tick(&self, mem: &mut WatchMemory, plan: &TickPlan) -> WatchView {
        let now = self.port.now_ms();
        let off = self.port.offset_secs(now);
        let ids: HashSet<String> = mem.agents.keys().cloned().collect();
        if !ids.is_empty() {
            let live = self.port.live_of(&ids);
            mem.agents.retain(|id, _| live.contains(id));
        }
        // Emner i projekter der ikke (længere) er aktive, venter ikke på vagten.
        mem.waiting
            .retain(|_, w| plan.projects.iter().any(|p| same_id(&p.id, &w.project)));
        for p in &plan.projects {
            if self.port.stopping() || !self.sweep(mem, p, now, off) {
                log::info!("watch: appen lukker — ingen flere starter i dette tick");
                break;
            }
        }
        mem.last_tick_at = Some(now);
        let state = self.port.with_state(&mut |s| s.clone());
        build_view(plan, &state, mem, now, off)
    }

    /// Dublet → playbook → bemanding (intet budget). `Ok(playbook)` eller hvorfor emnet venter.
    fn precheck<'a>(
        &self,
        p: &'a ActiveProject,
        c: &'a Candidate,
        watch_free: usize,
    ) -> Result<&'a str, Waiting> {
        let wait = |reason, text: String| Waiting {
            reason,
            text,
            next_at: None,
            project: p.id.clone(),
        };
        if self.port.is_duplicate(&p.id, &c.title) {
            return Err(wait(WaitReason::Duplicate, WATCH_WAIT_DUPLICATE.into()));
        }
        let Some(pb) = pick_playbook(&p.cfg.playbook, c.ticket_kind.as_deref(), &c.labels) else {
            return Err(wait(WaitReason::Playbook, WATCH_WAIT_NO_PLAYBOOK.into()));
        };
        if !p.playbooks.iter().any(|n| n == pb) {
            return Err(wait(WaitReason::Playbook, watch_unknown_playbook_text(pb)));
        }
        match self.port.staffing(&p.id, pb, watch_free) {
            Staffing::Ok => Ok(pb),
            Staffing::MissingStaff(_) => Err(wait(WaitReason::Planner, WATCH_WAIT_PLANNER.into())),
            Staffing::NoSeat => Err(wait(WaitReason::Seat, WATCH_WAIT_SEAT.into())),
        }
    }

    /// Ét projekt. `false`: appen lukker (stop alt).
    fn sweep(&self, mem: &mut WatchMemory, p: &ActiveProject, now: u64, off: i64) -> bool {
        let cands = self.port.candidates(&p.id);
        let ids: BTreeSet<&str> = cands.iter().map(|c| c.item_id.as_str()).collect();
        mem.waiting
            .retain(|id, w| !same_id(&w.project, &p.id) || ids.contains(id.as_str()));
        let watch_free = p.cfg.max_agents.saturating_sub(mem.agents_in(&p.id));
        let caps = caps_of(&p.cfg, &p.ws);
        // Efter et budget-afslag reserveres intet mere i projektet; de øvrige emner får samme
        // ventetekst (efter dublet/playbook/bemanding).
        let mut budget_wait: Option<Waiting> = None;
        for c in &cands {
            if self.port.stopping() {
                return false;
            }
            let pb = match self.precheck(p, c, watch_free) {
                Ok(pb) => pb,
                Err(w) => {
                    park(mem, &c.item_id, w);
                    continue;
                }
            };
            if let Some(w) = &budget_wait {
                park(mem, &c.item_id, w.clone());
                continue;
            }
            let verdict = self
                .port
                .with_state(&mut |s| s.reserve(&p.id, now, off, caps, p.cfg.quiet));
            if let Verdict::Wait { why, next_ms } = verdict {
                let at = next_ms.map_or_else(|| "--:--".to_string(), |t| hhmm(t, off));
                if let Some((key, n)) = budget_notice(&p.id, why, &at, now) {
                    self.port.notify(key, n);
                }
                let w = Waiting {
                    reason: WaitReason::Budget,
                    text: watch_wait_budget_text(&at),
                    next_at: next_ms,
                    project: p.id.clone(),
                };
                park(mem, &c.item_id, w.clone());
                budget_wait = Some(w);
                continue;
            }
            // Reserveret ved `now`. Sidste chance for at lade være, før noget oprettes.
            if self.port.stopping() {
                self.port.with_state(&mut |s| s.cancel(&p.id, now));
                return false;
            }
            let req = WatchStart {
                item_id: c.item_id.clone(),
                project: p.id.clone(),
                playbook: pb.to_string(),
                max_agents_left: watch_free,
            };
            let item = short_id(&c.item_id);
            match self.port.start(req) {
                Ok(st) => {
                    self.port.with_state(&mut |s| s.record_success(&p.id));
                    mem.waiting.remove(&c.item_id);
                    for id in &st.spawned {
                        mem.agents.insert(id.clone(), p.id.clone());
                    }
                    log::info!(
                        "watch: {}: emne {item} startet som ticket {} ({pb}; {} agent(er) startet)",
                        p.id,
                        short_id(&st.ticket_id),
                        st.spawned.len()
                    );
                    return true;
                }
                Err(StartFailure::Gone) => {
                    self.port.with_state(&mut |s| s.cancel(&p.id, now));
                    mem.waiting.remove(&c.item_id);
                    log::info!(
                        "watch: {}: emne {item} er ikke længere nyt; næste emne",
                        p.id
                    );
                }
                Err(StartFailure::Real(e)) => {
                    let tripped = self.port.with_state(&mut |s| {
                        s.cancel(&p.id, now);
                        s.record_failure(&p.id, &e, now)
                    });
                    log::warn!("watch: {}: start af emne {item} fejlede: {e}", p.id);
                    park(
                        mem,
                        &c.item_id,
                        Waiting {
                            reason: WaitReason::Failed,
                            text: watch_start_failed_text(&e),
                            next_at: None,
                            project: p.id.clone(),
                        },
                    );
                    self.after_failure(p, tripped, now);
                    return true;
                }
                Err(StartFailure::AfterTicket { ticket_id, error }) => {
                    let tripped = self
                        .port
                        .with_state(&mut |s| s.record_failure(&p.id, &error, now));
                    mem.waiting.remove(&c.item_id);
                    log::warn!(
                        "watch: {}: ticket {} oprettet, men forløbet fejlede: {error}",
                        p.id,
                        short_id(&ticket_id)
                    );
                    self.after_failure(p, tripped, now);
                    return true;
                }
            }
        }
        true
    }

    fn after_failure(&self, p: &ActiveProject, tripped: bool, now: u64) {
        if tripped {
            log::warn!(
                "watch: {}: stoppet efter {WATCH_TRIP_AFTER} fejl i træk",
                p.id
            );
            let (key, n) = tripped_notice(&p.id, now, now);
            self.port.notify(key, n);
        }
    }
}

/// Parkerer et emne; logger kun når årsagen er ny eller skiftede (ingen titler).
fn park(mem: &mut WatchMemory, item_id: &str, w: Waiting) {
    let changed = mem
        .waiting
        .get(item_id)
        .is_none_or(|old| old.reason != w.reason || old.next_at != w.next_at);
    if changed {
        log::info!(
            "watch: {}: emne {} parkeret ({})",
            w.project,
            short_id(item_id),
            w.reason.as_str()
        );
    }
    mem.waiting.insert(item_id.to_string(), w);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentManager, SeatKind};
    use crate::config::HOUR_MS;
    use crate::notices::NoticeKind;
    use crate::watch::config::parse_quiet;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    const CEST: i64 = 7200;
    /// 2026-10-02 10:00:00 UTC (12:00 CEST).
    const T: u64 = 1_790_935_200_000;
    const MIN: u64 = 60_000;

    #[derive(Default)]
    struct FakeState {
        now: u64,
        candidates: BTreeMap<String, Vec<Candidate>>,
        duplicates: BTreeSet<String>,
        /// Playbook → rollerne i trinene.
        playbooks: BTreeMap<String, Vec<Role>>,
        agents: Vec<AgentInfo>,
        room: Option<SeatRoom>,
        answers: VecDeque<Result<Started, StartFailure>>,
        starts: Vec<WatchStart>,
        staffing_calls: Vec<(String, String, usize)>,
        notices: Vec<(NoticeKey, Notice)>,
        state: WatchState,
        live: HashSet<String>,
        stopping: bool,
        /// Sæt `stopping` efter så mange starter.
        stop_after: Option<usize>,
        next_ticket: u32,
    }

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<FakeState>>);

    impl Fake {
        fn s(&self) -> std::sync::MutexGuard<'_, FakeState> {
            self.0.lock().unwrap()
        }
        fn new() -> Self {
            let f = Fake::default();
            {
                let mut s = f.s();
                s.now = T;
                s.playbooks
                    .insert("bug".into(), vec![Role::Debugger, Role::Coder]);
                s.playbooks
                    .insert("feature".into(), vec![Role::Planner, Role::Coder]);
                s.room = Some(SeatRoom {
                    work_free: 10,
                    in_project: 0,
                    max_per_project: 0,
                });
            }
            f
        }
        fn add(&self, project: &str, id: &str, labels: &[&str], kind: Option<&str>) {
            let n = self.s().candidates.values().map(Vec::len).sum::<usize>() as u64;
            self.s()
                .candidates
                .entry(project.into())
                .or_default()
                .push(Candidate {
                    item_id: id.into(),
                    title: format!("titel {id}"),
                    labels: labels.iter().map(|s| s.to_string()).collect(),
                    ticket_kind: kind.map(str::to_string),
                    seen_at: n,
                });
        }
        fn started_items(&self) -> Vec<String> {
            self.s().starts.iter().map(|w| w.item_id.clone()).collect()
        }
        fn advance(&self, ms: u64) {
            self.s().now += ms;
        }
        fn notices_of(&self, kind: NoticeKind) -> usize {
            self.s()
                .notices
                .iter()
                .filter(|(k, _)| k.kind == kind)
                .count()
        }
    }

    impl Clock for Fake {
        fn now_ms(&self) -> u64 {
            self.s().now
        }
        fn offset_secs(&self, _now_ms: u64) -> i64 {
            CEST
        }
    }
    impl InboxPort for Fake {
        fn candidates(&self, project: &str) -> Vec<Candidate> {
            self.s()
                .candidates
                .get(project)
                .cloned()
                .unwrap_or_default()
        }
        fn is_duplicate(&self, _project: &str, title: &str) -> bool {
            self.s().duplicates.contains(title)
        }
    }
    impl StarterPort for Fake {
        fn staffing(&self, project: &str, playbook: &str, watch_free: usize) -> Staffing {
            let mut s = self.s();
            s.staffing_calls
                .push((project.into(), playbook.into(), watch_free));
            let roles = s.playbooks.get(playbook).cloned().unwrap_or_default();
            staffing_for(&roles, &s.agents, project, s.room.unwrap(), watch_free)
        }
        fn start(&self, req: WatchStart) -> Result<Started, StartFailure> {
            let mut s = self.s();
            s.starts.push(req.clone());
            if s.stop_after.is_some_and(|n| s.starts.len() >= n) {
                s.stopping = true;
            }
            let answer = s.answers.pop_front().unwrap_or_else(|| {
                Ok(Started {
                    ticket_id: String::new(),
                    spawned: vec![],
                    notes: vec![],
                })
            });
            if !matches!(answer, Err(StartFailure::Real(_))) {
                for v in s.candidates.values_mut() {
                    v.retain(|c| c.item_id != req.item_id);
                }
            }
            s.next_ticket += 1;
            let n = s.next_ticket;
            answer.map(|mut st| {
                if st.ticket_id.is_empty() {
                    st.ticket_id = format!("ticket-{n}");
                }
                st
            })
        }
    }
    impl AgentsPort for Fake {
        fn live_of(&self, ids: &HashSet<String>) -> HashSet<String> {
            let s = self.s();
            ids.iter()
                .filter(|i| s.live.contains(*i))
                .cloned()
                .collect()
        }
    }
    impl Notifier for Fake {
        fn notify(&self, key: NoticeKey, notice: Notice) {
            let mut s = self.s();
            // Som køen: en kendt nøgle kommer ikke ind igen.
            if !s.notices.iter().any(|(k, _)| *k == key) {
                s.notices.push((key, notice));
            }
        }
    }
    impl StatePort for Fake {
        fn with_state<T>(&self, f: &mut dyn FnMut(&mut WatchState) -> T) -> T {
            f(&mut self.s().state)
        }
    }
    impl WatchPort for Fake {
        fn stopping(&self) -> bool {
            self.s().stopping
        }
    }

    fn cfg(rule: PlaybookRule) -> WatchConfig {
        WatchConfig {
            enabled: true,
            playbook: rule,
            ..WatchConfig::default()
        }
    }

    fn fixed(name: &str) -> PlaybookRule {
        PlaybookRule::Fixed(name.into())
    }

    fn plan_of(projects: &[(&str, WatchConfig)]) -> TickPlan {
        let files: Vec<ProjectWatchFile> = projects
            .iter()
            .map(|(id, c)| ProjectWatchFile {
                id: id.to_string(),
                cfg: Some(c.clone()),
                notes: vec![],
            })
            .collect();
        active_projects(
            &WorkspaceWatch::default(),
            &AppSettings::default(),
            &files,
            &BTreeSet::new(),
            true,
            &["bug".to_string(), "feature".to_string()],
        )
    }

    fn engine(f: &Fake) -> WatchEngine<Fake> {
        WatchEngine::new(f.clone())
    }

    fn global_used(f: &Fake) -> usize {
        f.s().state.global.starts.len()
    }

    #[test]
    fn starts_one_item_per_project_per_tick_oldest_first() {
        let f = Fake::new();
        f.add("web", "a", &[], None);
        f.add("web", "b", &[], None);
        let plan = plan_of(&[("web", cfg(fixed("bug")))]);
        let e = engine(&f);
        let mut mem = WatchMemory::default();
        let v = e.tick(&mut mem, &plan);
        assert_eq!(f.started_items(), ["a"]);
        let st = f.s().starts[0].clone();
        assert_eq!((st.project.as_str(), st.playbook.as_str()), ("web", "bug"));
        assert_eq!(st.max_agents_left, 2);
        assert_eq!(v.projects[0].used_hour, 1);
        assert_eq!(v.global.used_hour, 1);
        assert_eq!(mem.last_tick_at, Some(T));
        f.advance(MIN);
        e.tick(&mut mem, &plan);
        assert_eq!(f.started_items(), ["a", "b"]);
    }

    #[test]
    fn second_project_starts_in_same_tick() {
        let f = Fake::new();
        f.add("web", "a", &[], None);
        f.add("api", "b", &[], None);
        let plan = plan_of(&[("web", cfg(fixed("bug"))), ("api", cfg(fixed("bug")))]);
        let v = engine(&f).tick(&mut WatchMemory::default(), &plan);
        assert_eq!(f.started_items(), ["a", "b"]);
        assert_eq!(v.active, 2);
        assert_eq!(v.global.used_hour, 2);
    }

    #[test]
    fn parks_when_hour_cap_reached_with_next_time_and_notifies_once_per_hour() {
        let f = Fake::new();
        for id in ["a", "b", "c"] {
            f.add("web", id, &[], None);
        }
        let c = WatchConfig {
            max_per_hour: 1,
            ..cfg(fixed("bug"))
        };
        let plan = plan_of(&[("web", c)]);
        let e = engine(&f);
        let mut mem = WatchMemory::default();
        e.tick(&mut mem, &plan);
        f.advance(MIN);
        let v = e.tick(&mut mem, &plan);
        assert_eq!(f.started_items(), ["a"]);
        // Begge resterende emner står med samme budget-tekst (første start + 1 t = 13:00 CEST).
        for id in ["b", "c"] {
            let w = &v.waiting[id];
            assert_eq!(w.reason, WaitReason::Budget);
            assert_eq!(w.text, "venter på budget (næste: 13:00)");
            assert_eq!(w.next_at, Some(T + HOUR_MS));
        }
        assert_eq!(v.projects[0].next_free_at, Some(T + HOUR_MS));
        assert_eq!(f.notices_of(NoticeKind::BudgetReached), 1);
        let n = f.s().notices[0].1.clone();
        assert_eq!(n.text, "web: timeloft nået — næste: 13:00");
        // Flere afslag i samme time: ingen ny besked, ingen ny post i ringen.
        f.advance(MIN);
        e.tick(&mut mem, &plan);
        assert_eq!(f.notices_of(NoticeKind::BudgetReached), 1);
        assert_eq!(global_used(&f), 1);
        // Efter en time: næste emne startes.
        f.advance(HOUR_MS);
        e.tick(&mut mem, &plan);
        assert_eq!(f.started_items(), ["a", "b"]);
    }

    #[test]
    fn parks_in_quiet_hours_without_notice() {
        let f = Fake::new();
        f.add("web", "a", &[], None);
        let c = WatchConfig {
            quiet: parse_quiet("11-13"),
            quiet_text: Some("11-13".into()),
            ..cfg(fixed("bug"))
        };
        let plan = plan_of(&[("web", c)]);
        let v = engine(&f).tick(&mut WatchMemory::default(), &plan);
        assert!(f.started_items().is_empty());
        assert_eq!(v.waiting["a"].text, "venter på budget (næste: 13:00)");
        assert!(v.projects[0].in_quiet);
        assert_eq!(v.projects[0].quiet.as_deref(), Some("11-13"));
        assert_eq!(f.s().notices.len(), 0);
        assert_eq!(global_used(&f), 0);
        assert!(!can_start_anything(&plan, &f.s().state, T, CEST));
    }

    #[test]
    fn global_cap_blocks_second_project() {
        let f = Fake::new();
        f.add("web", "a", &[], None);
        f.add("api", "b", &[], None);
        let mut plan = plan_of(&[("web", cfg(fixed("bug"))), ("api", cfg(fixed("bug")))]);
        for p in &mut plan.projects {
            p.ws.max_per_hour = 1;
        }
        plan.ws.max_per_hour = 1;
        let v = engine(&f).tick(&mut WatchMemory::default(), &plan);
        assert_eq!(f.started_items(), ["a"]);
        assert_eq!(v.waiting["b"].reason, WaitReason::Budget);
        assert_eq!(
            f.s().notices[0].1.text,
            "api: workspace-loft nået — næste: 13:00"
        );
        assert_eq!(v.global.cap_hour, 1);
    }

    #[test]
    fn skips_duplicate_with_note_no_budget() {
        let f = Fake::new();
        f.add("web", "a", &[], None);
        f.add("web", "b", &[], None);
        f.s().duplicates.insert("titel a".into());
        let v = engine(&f).tick(
            &mut WatchMemory::default(),
            &plan_of(&[("web", cfg(fixed("bug")))]),
        );
        assert_eq!(f.started_items(), ["b"]);
        assert_eq!(v.waiting["a"].reason, WaitReason::Duplicate);
        assert_eq!(v.waiting["a"].text, "mulig dublet, start manuelt");
        assert_eq!(global_used(&f), 1, "only the start of b is in the ring");
    }

    #[test]
    fn parks_without_playbook_or_unknown_name_no_budget() {
        let f = Fake::new();
        f.add("web", "a", &["question"], None);
        f.add("api", "b", &[], None);
        f.add("ops", "c", &[], Some("task"));
        let plan = plan_of(&[
            (
                "web",
                cfg(PlaybookRule::ByLabel {
                    by_label: vec![("bug".into(), "bug".into())],
                    default: None,
                }),
            ),
            ("api", cfg(fixed("docs"))),
            ("ops", cfg(PlaybookRule::None)),
        ]);
        let v = engine(&f).tick(&mut WatchMemory::default(), &plan);
        assert!(f.started_items().is_empty());
        assert_eq!(v.waiting["a"].text, "ingen playbook valgt for vagten");
        assert_eq!(
            v.waiting["b"].text,
            "playbook «docs» findes ikke i workspace"
        );
        assert_eq!(v.waiting["c"].reason, WaitReason::Playbook);
        assert_eq!(global_used(&f), 0);
        assert!(f.s().staffing_calls.is_empty());
    }

    #[test]
    fn ticket_kind_wins_over_by_label() {
        let f = Fake::new();
        f.add("web", "a", &["enhancement"], Some("bug"));
        f.s().agents = vec![];
        let rule = PlaybookRule::ByLabel {
            by_label: vec![("enhancement".into(), "feature".into())],
            default: None,
        };
        engine(&f).tick(&mut WatchMemory::default(), &plan_of(&[("web", cfg(rule))]));
        assert_eq!(f.s().starts[0].playbook, "bug");
    }

    fn agent(
        m: &mut AgentManager,
        roles: &[Role],
        seat: SeatKind,
        project: Option<&str>,
    ) -> AgentInfo {
        let id = m.insert_fake_in(
            &uuid::Uuid::new_v4().to_string(),
            "/w/x",
            roles,
            seat,
            project,
        );
        m.get(&id).unwrap()
    }

    #[test]
    fn parks_when_staff_missing_or_no_seat_no_budget() {
        // `feature` uden planlægger → planner; `maxAgents: 1` + `bug` med 0 agenter → plads.
        let f = Fake::new();
        f.add("web", "a", &[], None);
        f.add("api", "b", &[], None);
        let one = WatchConfig {
            max_agents: 1,
            ..cfg(fixed("bug"))
        };
        let plan = plan_of(&[("web", cfg(fixed("feature"))), ("api", one)]);
        let e = engine(&f);
        let mut mem = WatchMemory::default();
        let v = e.tick(&mut mem, &plan);
        assert!(f.started_items().is_empty());
        assert_eq!(v.waiting["a"].reason, WaitReason::Planner);
        assert_eq!(v.waiting["a"].text, "venter på planlægger (stabsplads)");
        assert_eq!(v.waiting["b"].reason, WaitReason::Seat);
        assert_eq!(v.waiting["b"].text, "venter på plads");
        assert_eq!(global_used(&f), 0);
        assert!(f.s().state.projects.is_empty(), "no budget entry at all");
        // En planlægger på en stabsplads → `feature` startes næste tick.
        let mut m = AgentManager::new(5);
        f.s().agents = vec![agent(&mut m, &[Role::Planner], SeatKind::Staff, None)];
        f.advance(MIN);
        e.tick(&mut mem, &plan);
        assert_eq!(f.started_items(), ["a"]);
    }

    #[test]
    fn staffing_counts_missing_roles_seats_and_project_cap() {
        let mut m = AgentManager::new(5);
        let room = SeatRoom {
            work_free: 5,
            in_project: 0,
            max_per_project: 0,
        };
        let bug = [Role::Debugger, Role::Coder];
        assert_eq!(staffing_for(&bug, &[], "web", room, 2), Staffing::Ok);
        assert_eq!(staffing_for(&bug, &[], "web", room, 1), Staffing::NoSeat);
        let tight = SeatRoom {
            work_free: 1,
            ..room
        };
        assert_eq!(staffing_for(&bug, &[], "web", tight, 2), Staffing::NoSeat);
        let capped = SeatRoom {
            in_project: 1,
            max_per_project: 2,
            ..room
        };
        assert_eq!(staffing_for(&bug, &[], "web", capped, 2), Staffing::NoSeat);
        // En levende koder i projektet: kun debuggeren mangler.
        let coder = agent(&mut m, &[Role::Coder], SeatKind::Work, Some("web"));
        assert_eq!(
            staffing_for(&bug, std::slice::from_ref(&coder), "web", room, 1),
            Staffing::Ok
        );
        // En koder i et andet projekt tæller ikke.
        let other = agent(&mut m, &[Role::Coder], SeatKind::Work, Some("api"));
        assert_eq!(
            staffing_for(&bug, std::slice::from_ref(&other), "web", room, 1),
            Staffing::NoSeat
        );
        let feature = [Role::Planner, Role::Coder];
        assert_eq!(
            staffing_for(&feature, &[coder], "web", room, 2),
            Staffing::MissingStaff(Role::Planner)
        );
    }

    #[test]
    fn gone_item_cancels_reservation_and_tries_next() {
        let f = Fake::new();
        f.add("web", "a", &[], None);
        f.add("web", "b", &[], None);
        f.s().answers.push_back(Err(StartFailure::Gone));
        let v = engine(&f).tick(
            &mut WatchMemory::default(),
            &plan_of(&[("web", cfg(fixed("bug")))]),
        );
        assert_eq!(f.started_items(), ["a", "b"]);
        assert_eq!(global_used(&f), 1, "a's reservation was cancelled");
        assert_eq!(f.s().state.projects["web"].failures, 0);
        assert!(v.waiting.is_empty());
    }

    #[test]
    fn three_real_failures_trip_once_and_notify() {
        let f = Fake::new();
        f.add("web", "a", &[], None);
        for _ in 0..3 {
            f.s()
                .answers
                .push_back(Err(StartFailure::Real("netværk væk".into())));
        }
        let e = engine(&f);
        let mut mem = WatchMemory::default();
        let plan = plan_of(&[("web", cfg(fixed("bug")))]);
        let v = e.tick(&mut mem, &plan);
        assert_eq!(v.waiting["a"].reason, WaitReason::Failed);
        assert_eq!(v.waiting["a"].text, "vagt: start fejlede: netværk væk");
        assert_eq!(global_used(&f), 0, "a failed start costs no budget");
        for _ in 0..2 {
            f.advance(MIN);
            e.tick(&mut mem, &plan);
        }
        assert_eq!(f.s().starts.len(), 3);
        let ps = f.s().state.projects["web"].clone();
        assert_eq!(ps.failures, 3);
        assert_eq!(ps.tripped_at, Some(T + 2 * MIN));
        assert_eq!(ps.tripped_reason.as_deref(), Some("netværk væk"));
        assert_eq!(f.notices_of(NoticeKind::WatchTripped), 1);
        // 4. tick: skallens plan har ikke længere projektet med.
        let tripped = f.s().state.tripped();
        let files = [ProjectWatchFile {
            id: "web".into(),
            cfg: Some(cfg(fixed("bug"))),
            notes: vec![],
        }];
        let ws = WorkspaceWatch::default();
        let names = ["bug".to_string()];
        let plan4 = active_projects(&ws, &AppSettings::default(), &files, &tripped, true, &names);
        assert!(plan4.projects.is_empty());
        f.advance(MIN);
        let v = e.tick(&mut mem, &plan4);
        assert_eq!(f.s().starts.len(), 3, "nothing started while tripped");
        assert!(v.projects[0].tripped);
        assert_eq!(
            v.projects[0].reason.as_deref(),
            Some("stoppet efter 3 fejl — tryk Genstart vagt")
        );
        // "Genstart vagt" nulstiller; næste tick starter.
        assert!(f.s().state.reset_trip("web"));
        let tripped = f.s().state.tripped();
        let plan5 = active_projects(&ws, &AppSettings::default(), &files, &tripped, true, &names);
        e.tick(&mut mem, &plan5);
        assert_eq!(f.s().starts.len(), 4);
        assert_eq!(f.s().state.projects["web"].failures, 0);
    }

    #[test]
    fn failure_after_ticket_keeps_the_reservation_and_counts() {
        let f = Fake::new();
        f.add("web", "a", &[], None);
        f.s().answers.push_back(Err(StartFailure::AfterTicket {
            ticket_id: "t1".into(),
            error: "kan ikke gemme".into(),
        }));
        let v = engine(&f).tick(
            &mut WatchMemory::default(),
            &plan_of(&[("web", cfg(fixed("bug")))]),
        );
        assert_eq!(global_used(&f), 1);
        assert_eq!(f.s().state.projects["web"].failures, 1);
        assert!(v.waiting.is_empty());
    }

    #[test]
    fn tripped_project_is_not_in_plan_until_reset() {
        let files = [ProjectWatchFile {
            id: "Web".into(),
            cfg: Some(cfg(fixed("bug"))),
            notes: vec![],
        }];
        let tripped: BTreeSet<String> = ["web".to_string()].into();
        let p = active_projects(
            &WorkspaceWatch::default(),
            &AppSettings::default(),
            &files,
            &tripped,
            true,
            &[],
        );
        assert!(p.projects.is_empty());
        assert_eq!(p.inactive[0].1, "stoppet efter 3 fejl — tryk Genstart vagt");
        let p = active_projects(
            &WorkspaceWatch::default(),
            &AppSettings::default(),
            &files,
            &BTreeSet::new(),
            true,
            &[],
        );
        assert_eq!(p.projects.len(), 1);
    }

    #[test]
    fn stopping_mid_sweep_starts_nothing_more() {
        let f = Fake::new();
        f.add("web", "a", &[], None);
        f.add("api", "b", &[], None);
        f.add("ops", "c", &[], None);
        f.s().stop_after = Some(1);
        let plan = plan_of(&[
            ("web", cfg(fixed("bug"))),
            ("api", cfg(fixed("bug"))),
            ("ops", cfg(fixed("bug"))),
        ]);
        engine(&f).tick(&mut WatchMemory::default(), &plan);
        assert_eq!(f.started_items(), ["a"]);
        assert_eq!(global_used(&f), 1, "no reservation left behind");
        // Allerede lukkende: intet start, ingen reservation.
        let f = Fake::new();
        f.add("web", "a", &[], None);
        f.s().stopping = true;
        engine(&f).tick(&mut WatchMemory::default(), &plan);
        assert!(f.started_items().is_empty());
        assert_eq!(global_used(&f), 0);
    }

    #[test]
    fn live_watch_agents_are_pruned_each_tick() {
        let f = Fake::new();
        f.add("web", "a", &[], None);
        f.add("web", "b", &[], None);
        f.s().answers.push_back(Ok(Started {
            ticket_id: "t1".into(),
            spawned: vec!["ag1".into(), "ag2".into()],
            notes: vec![],
        }));
        f.s().live = ["ag1".to_string(), "ag2".to_string()].into();
        let e = engine(&f);
        let mut mem = WatchMemory::default();
        let plan = plan_of(&[("web", cfg(fixed("bug")))]);
        let v = e.tick(&mut mem, &plan);
        assert_eq!(v.projects[0].agents, 2);
        // Begge lever: ingen vagt-plads til b (maxAgents 2).
        f.advance(MIN);
        let v = e.tick(&mut mem, &plan);
        assert_eq!(f.started_items(), ["a"]);
        assert_eq!(v.waiting["b"].reason, WaitReason::Seat);
        assert_eq!(f.s().staffing_calls.last().unwrap().2, 0);
        // Én afsluttede: den tælles ikke længere, men der mangler stadig én plads (2 roller).
        f.s().live.remove("ag1");
        f.advance(MIN);
        let v = e.tick(&mut mem, &plan);
        assert_eq!(v.projects[0].agents, 1);
        assert_eq!(f.s().staffing_calls.last().unwrap().2, 1);
        f.s().live.clear();
        f.advance(MIN);
        e.tick(&mut mem, &plan);
        assert_eq!(f.started_items(), ["a", "b"]);
        assert!(mem.agents.is_empty());
    }

    #[test]
    fn active_projects_table() {
        let on = cfg(fixed("bug"));
        let file = |c: Option<WatchConfig>| ProjectWatchFile {
            id: "web".into(),
            cfg: c,
            notes: vec!["project.json: watch.maxPerHour 90 er sat ned til 60 (1–60)".into()],
        };
        let ws_on = WorkspaceWatch::default();
        let ws_off = WorkspaceWatch {
            enabled: false,
            ..ws_on
        };
        let paused = AppSettings {
            watch_paused: true,
            ..AppSettings::default()
        };
        let off = AppSettings {
            watch_off: vec!["WEB".into()],
            ..AppSettings::default()
        };
        let disabled = WatchConfig {
            enabled: false,
            ..on.clone()
        };
        let none = AppSettings::default();
        let tripped: BTreeSet<String> = ["web".to_string()].into();
        let empty = BTreeSet::new();
        type Row<'a> = (
            &'a WorkspaceWatch,
            &'a AppSettings,
            Option<WatchConfig>,
            &'a BTreeSet<String>,
            bool,
            Option<&'a str>,
        );
        let rows: [Row; 9] = [
            (&ws_on, &none, Some(on.clone()), &empty, true, None),
            (&ws_on, &none, None, &empty, true, Some("-")),
            (
                &ws_on,
                &none,
                Some(disabled),
                &empty,
                true,
                Some(WATCH_REASON_NOT_ENABLED),
            ),
            (
                &ws_off,
                &none,
                Some(on.clone()),
                &empty,
                true,
                Some(WATCH_REASON_WS_OFF),
            ),
            (
                &ws_on,
                &paused,
                Some(on.clone()),
                &empty,
                true,
                Some(WATCH_REASON_PAUSED),
            ),
            (
                &ws_on,
                &off,
                Some(on.clone()),
                &empty,
                true,
                Some(WATCH_REASON_PROJECT_PAUSED),
            ),
            (
                &ws_on,
                &none,
                Some(on.clone()),
                &tripped,
                true,
                Some("stoppet efter 3 fejl — tryk Genstart vagt"),
            ),
            (
                &ws_on,
                &none,
                Some(on.clone()),
                &empty,
                false,
                Some(WATCH_REASON_CANNOT_SPAWN),
            ),
            // I `watchOff` uden `watch` i filen: vist (så "Hold vagt" ses), men ikke aktiv.
            (
                &ws_on,
                &off,
                None,
                &empty,
                true,
                Some(WATCH_REASON_NOT_ENABLED),
            ),
        ];
        for (i, (ws, settings, c, tr, can, want)) in rows.into_iter().enumerate() {
            let p = active_projects(ws, settings, &[file(c)], tr, can, &["bug".into()]);
            match want {
                None => {
                    assert_eq!(p.projects.len(), 1, "row {i}");
                    assert!(p.inactive.is_empty(), "row {i}");
                }
                Some("-") => {
                    assert!(p.projects.is_empty() && p.inactive.is_empty(), "row {i}");
                    assert!(p.shown.is_empty(), "row {i}: not shown at all");
                }
                Some(r) => {
                    assert!(p.projects.is_empty(), "row {i}");
                    assert_eq!(p.inactive, [("web".to_string(), r.to_string())], "row {i}");
                    assert_eq!(p.shown.len(), 1, "row {i}");
                }
            }
            assert_eq!(p.paused, settings.watch_paused);
        }
    }

    #[test]
    fn inactive_projects_start_nothing_and_lose_their_waiting() {
        let f = Fake::new();
        f.add("web", "a", &[], None);
        f.s().duplicates.insert("titel a".into());
        let e = engine(&f);
        let mut mem = WatchMemory::default();
        e.tick(&mut mem, &plan_of(&[("web", cfg(fixed("bug")))]));
        assert!(mem.waiting.contains_key("a"));
        f.s().duplicates.clear();
        let files = [ProjectWatchFile {
            id: "web".into(),
            cfg: Some(cfg(fixed("bug"))),
            notes: vec![],
        }];
        let paused = AppSettings {
            watch_paused: true,
            ..AppSettings::default()
        };
        let plan = active_projects(
            &WorkspaceWatch::default(),
            &paused,
            &files,
            &BTreeSet::new(),
            true,
            &["bug".into()],
        );
        let v = e.tick(&mut mem, &plan);
        assert!(f.started_items().is_empty());
        assert!(v.waiting.is_empty());
        assert!(v.paused && !v.projects[0].active);
        assert_eq!(v.projects[0].reason.as_deref(), Some(WATCH_REASON_PAUSED));
    }

    #[test]
    fn waiting_is_cleared_when_item_disappears() {
        let f = Fake::new();
        f.add("web", "a", &[], None);
        f.s().duplicates.insert("titel a".into());
        let e = engine(&f);
        let mut mem = WatchMemory::default();
        let plan = plan_of(&[("web", cfg(fixed("bug")))]);
        e.tick(&mut mem, &plan);
        assert_eq!(mem.waiting.len(), 1);
        // Brugeren afviste emnet: det er ikke længere en kandidat.
        f.s().candidates.clear();
        let v = e.tick(&mut mem, &plan);
        assert!(v.waiting.is_empty());
    }

    #[test]
    fn view_is_camel_case_and_lists_waiting() {
        let f = Fake::new();
        f.add("web", "a", &[], None);
        f.s().duplicates.insert("titel a".into());
        let c = WatchConfig {
            quiet: parse_quiet("23-07"),
            quiet_text: Some("23-07".into()),
            playbook: PlaybookRule::ByLabel {
                by_label: vec![
                    ("bug".into(), "bug".into()),
                    ("enhancement".into(), "feature".into()),
                ],
                default: None,
            },
            ..cfg(PlaybookRule::None)
        };
        let v = engine(&f).tick(&mut WatchMemory::default(), &plan_of(&[("web", c)]));
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "paused": false,
                "active": 1,
                "lastTickAt": T,
                "global": {"usedHour": 0, "capHour": 6, "usedDay": 0, "capDay": 20},
                "projects": [{
                    "id": "web", "enabled": true, "paused": false, "active": true,
                    "reason": null, "tripped": false, "trippedAt": null, "trippedReason": null,
                    "usedHour": 0, "capHour": 3, "usedDay": 0, "capDay": 10,
                    "agents": 0, "maxAgents": 2, "nextFreeAt": null,
                    "quiet": "23-07", "inQuiet": false, "playbook": "byLabel (2) → task",
                    "notes": []
                }],
                "waiting": {"a": {"reason": "duplicate", "text": "mulig dublet, start manuelt",
                    "nextAt": null, "project": "web"}}
            })
        );
        let text = serde_json::to_string(&v).unwrap();
        let at = |k: &str| text.find(&format!("\"{k}\":")).unwrap();
        assert!(at("paused") < at("active") && at("lastTickAt") < at("global"));
        assert!(at("global") < at("projects") && at("projects") < at("waiting"));
    }

    #[test]
    fn nothing_to_start_means_no_refresh() {
        let f = Fake::new();
        let c = WatchConfig {
            max_per_hour: 1,
            ..cfg(fixed("bug"))
        };
        let plan = plan_of(&[("web", c)]);
        assert!(can_start_anything(&plan, &f.s().state, T, CEST));
        f.s().state.reserve(
            "web",
            T,
            CEST,
            caps_of(&plan.projects[0].cfg, &plan.ws),
            None,
        );
        assert!(!can_start_anything(&plan, &f.s().state, T + MIN, CEST));
        assert!(can_start_anything(&plan, &f.s().state, T + HOUR_MS, CEST));
        assert!(!can_start_anything(
            &TickPlan::default(),
            &f.s().state,
            T,
            CEST
        ));
    }

    #[test]
    fn hhmm_and_playbook_text() {
        assert_eq!(hhmm(T, CEST), "12:00");
        assert_eq!(hhmm(T + 65 * MIN, 0), "11:05");
        assert_eq!(playbook_text(&PlaybookRule::None), None);
        assert_eq!(playbook_text(&fixed("bug")).as_deref(), Some("bug"));
        let r = PlaybookRule::ByLabel {
            by_label: vec![("bug".into(), "bug".into())],
            default: Some("feature".into()),
        };
        assert_eq!(playbook_text(&r).as_deref(), Some("byLabel (1) → feature"));
    }
}
