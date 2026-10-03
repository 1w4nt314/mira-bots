//! Step 6d (plan A.8): beskeder til brugeren — eskaleringer, forløb klar til godkendelse,
//! tilladelser og trust-dialoger der venter, fejlet tilbagemelding, vagtens budget og stop, og
//! agenter der afsluttede med tickets i gang.
//!
//! Kilder: (a) [`derive_ticket_notices`] over to [`NoticeSnap`]-lister taget i
//! `TicketsCtx::mutate_if` (samme sted som `relations_snapshot`); (b) [`notice_for_exit`] fra
//! Exited-håndteringen når mindst én ticket blev frigivet; (c) [`derive_waiting_notices`] (kaldes
//! af vagtens tick i Batch 4); (d) [`budget_notice`]/[`tripped_notice`] (vagt-motoren, Batch 4).
//! Alle udledninger er rene; [`NoticesCtx`] er den eneste tilstand (i hukommelsen, ingen fil).
//!
//! Dedup: hver besked har en [`NoticeKey`] `(kind, subject, generation)`. En kendt nøgle giver
//! ingen ny besked; nøglerne glemmes efter [`NOTICE_SEEN_TTL_MS`]. Køen holder højst
//! [`NOTICES_MAX`] beskeder (de ældste ryddes). En fravalgt type (`notifyOff`) oprettes ikke.
//!
//! Indhold (plan H, "kompromitteret eksternt indhold"): teksterne bygges kun af kort-id, en
//! `one_line`-klippet (≤ 80 tegn) allerede renset ticket-titel, tal, agent-/projektnavne og faste
//! tekster — aldrig en body eller eksterne noter. Loggen får kun type og kort-id/projekt.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::agent::AgentInfo;
use crate::config::{
    HOUR_MS, NOTICES_MAX, NOTICE_SEEN_TTL_MS, STARTING_HINT_TEXT, WAITING_NOTICE_AFTER_MS,
    WATCH_TRIP_AFTER,
};
use crate::events::{EmitFn, NOTICES_CHANGED};
use crate::hooks::status::AgentStatus;
use crate::permissions::PermissionRequestInfo;
use crate::tickets::model::{short_id, TicketState};
use crate::tickets::prompt::one_line;
use crate::watch::budget::WaitWhy;

/// Højst så mange tegn af en ticket-titel i en besked (`…` markerer klippet).
pub const NOTICE_TITLE_MAX_CHARS: usize = 80;
/// Højst så mange tegn af en tilbagemeldingsfejl, et værktøjsnavn eller et agentnavn i en besked.
pub const NOTICE_PART_MAX_CHARS: usize = 120;

/// De otte beskedtyper (C6d.1; camelCase på tråden og i `notifyOff`).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum NoticeKind {
    Escalated,
    FlowReview,
    PermissionWaiting,
    TrustWaiting,
    WriteBackFailed,
    BudgetReached,
    WatchTripped,
    AgentExited,
}

impl NoticeKind {
    pub const ALL: [NoticeKind; 8] = [
        NoticeKind::Escalated,
        NoticeKind::FlowReview,
        NoticeKind::PermissionWaiting,
        NoticeKind::TrustWaiting,
        NoticeKind::WriteBackFailed,
        NoticeKind::BudgetReached,
        NoticeKind::WatchTripped,
        NoticeKind::AgentExited,
    ];

    /// Navnet på tråden (som i `notifyOff`).
    pub fn as_str(self) -> &'static str {
        match self {
            NoticeKind::Escalated => "escalated",
            NoticeKind::FlowReview => "flowReview",
            NoticeKind::PermissionWaiting => "permissionWaiting",
            NoticeKind::TrustWaiting => "trustWaiting",
            NoticeKind::WriteBackFailed => "writeBackFailed",
            NoticeKind::BudgetReached => "budgetReached",
            NoticeKind::WatchTripped => "watchTripped",
            NoticeKind::AgentExited => "agentExited",
        }
    }

    /// Typen ud fra navnet på tråden; ukendte navne (en nyere builds `notifyOff`) giver `None`.
    pub fn parse(s: &str) -> Option<NoticeKind> {
        NoticeKind::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// Kort dansk etiket (Diagnostiks "Giv besked ved:"; `NOTICE_KIND_LABEL` i TS, C6d.5).
    pub fn label_da(self) -> &'static str {
        match self {
            NoticeKind::Escalated => "eskaleret",
            NoticeKind::FlowReview => "forløb til godkendelse",
            NoticeKind::PermissionWaiting => "tilladelse venter",
            NoticeKind::TrustWaiting => "agent venter i terminalen",
            NoticeKind::WriteBackFailed => "tilbagemelding fejlede",
            NoticeKind::BudgetReached => "budget nået",
            NoticeKind::WatchTripped => "vagt stoppet",
            NoticeKind::AgentExited => "agent afsluttet",
        }
    }

    /// Beskedens titel (C6d.5).
    pub fn title_da(self) -> &'static str {
        match self {
            NoticeKind::Escalated => "Ticket eskaleret",
            NoticeKind::FlowReview => "Forløb klar til din godkendelse",
            NoticeKind::PermissionWaiting => "Tilladelse venter",
            NoticeKind::TrustWaiting => "Agent venter i terminalen",
            NoticeKind::WriteBackFailed => "Tilbagemelding fejlede",
            NoticeKind::BudgetReached => "Vagtens budget er nået",
            NoticeKind::WatchTripped => "Vagten er stoppet",
            NoticeKind::AgentExited => "Agent afsluttede med ticket i gang",
        }
    }
}

/// Én besked (C6d.2 `Notice`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Notice {
    /// uuid v4.
    pub id: String,
    pub kind: NoticeKind,
    /// Unix ms.
    pub at: u64,
    pub title: String,
    pub text: String,
    /// Fuldt ticket-id (klik → ticketen).
    pub ticket_id: Option<String>,
    pub agent_id: Option<String>,
    pub project: Option<String>,
    pub seen: bool,
}

impl Notice {
    /// En ny, ulæst besked af typen `kind` med dens faste titel.
    pub fn new(kind: NoticeKind, text: String, now: u64) -> Notice {
        Notice {
            id: uuid::Uuid::new_v4().to_string(),
            kind,
            at: now,
            title: kind.title_da().to_string(),
            text,
            ticket_id: None,
            agent_id: None,
            project: None,
            seen: false,
        }
    }

    fn ticket(mut self, id: &str, project: Option<&str>) -> Notice {
        self.ticket_id = Some(id.to_string());
        self.project = project.map(str::to_string);
        self
    }

    fn agent(mut self, id: &str, project: Option<&str>) -> Notice {
        self.agent_id = Some(id.to_string());
        self.project = project.map(str::to_string);
        self
    }

    fn project(mut self, project: &str) -> Notice {
        self.project = Some(project.to_string());
        self
    }
}

/// Dedup-nøgle (plan A.8): samme `(kind, subject, generation)` giver aldrig en besked mere
/// (så længe nøglen huskes, [`NOTICE_SEEN_TTL_MS`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NoticeKey {
    pub kind: NoticeKind,
    pub subject: String,
    pub generation: u64,
}

impl NoticeKey {
    pub fn new(kind: NoticeKind, subject: &str, generation: u64) -> NoticeKey {
        NoticeKey {
            kind,
            subject: subject.to_string(),
            generation,
        }
    }
}

/// Resultatet af `list_notices` og indholdet af `notices-changed` (C6d.2): nyeste først.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NoticesPayload {
    pub unread: usize,
    pub items: Vec<Notice>,
}

/// Køen (kun i hukommelsen): nyeste først, højst [`NOTICES_MAX`].
#[derive(Debug, Default)]
pub struct NoticeQueue {
    items: VecDeque<Notice>,
    /// Nøgle → hvornår den kom ind (Unix ms); glemmes efter [`NOTICE_SEEN_TTL_MS`].
    seen_keys: HashMap<NoticeKey, u64>,
    off: BTreeSet<NoticeKind>,
}

impl NoticeQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Lægger `notice` forrest, medmindre typen er fravalgt eller `key` er kendt; den ældste
    /// besked ryddes når der er flere end [`NOTICES_MAX`]. Svarer om den kom ind.
    pub fn push(&mut self, key: NoticeKey, notice: Notice, now: u64) -> bool {
        if self.off.contains(&key.kind) || self.off.contains(&notice.kind) {
            return false;
        }
        if self.seen_keys.contains_key(&key) {
            return false;
        }
        self.seen_keys.insert(key, now);
        self.items.push_front(notice);
        self.items.truncate(NOTICES_MAX);
        true
    }

    /// Nyeste først.
    pub fn list(&self) -> Vec<Notice> {
        self.items.iter().cloned().collect()
    }

    /// Antal ulæste beskeder.
    pub fn unread(&self) -> usize {
        self.items.iter().filter(|n| !n.seen).count()
    }

    /// Markerer beskederne med disse id'er (alle ved `None`) som læst; svarer hvor mange der
    /// ændrede sig.
    pub fn mark_seen(&mut self, ids: Option<&[String]>) -> usize {
        let mut n = 0;
        for item in self.items.iter_mut() {
            let hit = ids.is_none_or(|ids| ids.contains(&item.id));
            if hit && !item.seen {
                item.seen = true;
                n += 1;
            }
        }
        n
    }

    /// Glemmer nøgler ældre end [`NOTICE_SEEN_TTL_MS`] (selve beskederne bliver).
    pub fn prune(&mut self, now: u64) {
        self.seen_keys
            .retain(|_, at| now.saturating_sub(*at) < NOTICE_SEEN_TTL_MS);
    }

    /// Sætter de fravalgte typer (`notifyOff`; ukendte navne ignoreres). Beskeder af en type
    /// der nu er fravalgt, forlader køen (en fravalgt type vises aldrig). Svarer om køen
    /// ændrede sig.
    pub fn set_off(&mut self, kinds: &[String]) -> bool {
        self.off = kinds.iter().filter_map(|k| NoticeKind::parse(k)).collect();
        let before = self.items.len();
        let off = &self.off;
        self.items.retain(|n| !off.contains(&n.kind));
        self.items.len() != before
    }

    /// De fravalgte typer.
    pub fn off(&self) -> &BTreeSet<NoticeKind> {
        &self.off
    }

    pub fn payload(&self) -> NoticesPayload {
        NoticesPayload {
            unread: self.unread(),
            items: self.list(),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Appens beskedkø (`AppState.notices` = `TicketsCtx.notices`) og dens emit. Kø-låsen tages
/// alene (aldrig sammen med en anden lås) og slippes før hvert emit.
pub struct NoticesCtx {
    queue: Mutex<NoticeQueue>,
    emit: EmitFn,
}

impl NoticesCtx {
    pub fn new(emit: EmitFn) -> Self {
        NoticesCtx {
            queue: Mutex::new(NoticeQueue::new()),
            emit,
        }
    }

    fn emit_payload(&self, p: &NoticesPayload) {
        match serde_json::to_value(p) {
            Ok(v) => (self.emit)(NOTICES_CHANGED, v),
            Err(e) => log::error!("serialize {NOTICES_CHANGED}: {e}"),
        }
    }

    /// Lægger alle beskeder i køen (gamle nøgler ryddes først); emitter `notices-changed` én
    /// gang når mindst én kom ind. Svarer hvor mange der kom ind.
    pub fn push_all(&self, v: Vec<(NoticeKey, Notice)>, now: u64) -> usize {
        if v.is_empty() {
            return 0;
        }
        let (added, payload) = {
            let mut q = lock(&self.queue);
            q.prune(now);
            let mut added = 0;
            for (key, notice) in v {
                let (kind, what) = (notice.kind, log_subject(&key, &notice));
                if q.push(key, notice, now) {
                    log::info!("notice: {} {what}", kind.as_str());
                    added += 1;
                }
            }
            (added, (added > 0).then(|| q.payload()))
        };
        if let Some(p) = payload {
            self.emit_payload(&p);
        }
        added
    }

    /// Markerer beskeder som læst (alle ved `None`); emitter når noget ændrede sig.
    pub fn mark_seen(&self, ids: Option<&[String]>) -> NoticesPayload {
        let (changed, p) = {
            let mut q = lock(&self.queue);
            let n = q.mark_seen(ids);
            (n > 0, q.payload())
        };
        if changed {
            self.emit_payload(&p);
        }
        p
    }

    /// Sætter de fravalgte typer (`notifyOff`); emitter når beskeder forlod køen.
    pub fn set_off(&self, kinds: &[String]) -> NoticesPayload {
        let (changed, p) = {
            let mut q = lock(&self.queue);
            let changed = q.set_off(kinds);
            (changed, q.payload())
        };
        if changed {
            self.emit_payload(&p);
        }
        p
    }

    pub fn payload(&self) -> NoticesPayload {
        lock(&self.queue).payload()
    }
}

/// Hvad loggen må nævne (plan C6d.5: aldrig titler): ticketens kort-id, ellers agent-id'et,
/// ellers projektet.
fn log_subject(key: &NoticeKey, n: &Notice) -> String {
    if let Some(t) = &n.ticket_id {
        short_id(t)
    } else if let Some(a) = &n.agent_id {
        a.clone()
    } else if let Some(p) = &n.project {
        p.clone()
    } else {
        format!("({})", key.generation)
    }
}

/// `s` på én linje, højst `max` tegn (`…` markerer klippet).
pub fn clip_line(s: &str, max: usize) -> String {
    let s = one_line(s);
    if s.chars().count() <= max {
        return s;
    }
    let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
    t = t.trim_end().to_string();
    t.push('…');
    t
}

/// Den del af en ticket beskederne bruger (tages i `mutate_if` ved siden af
/// `relations_snapshot`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoticeSnap {
    pub id: String,
    pub short: String,
    /// Den (allerede validerede) titel på én linje, højst [`NOTICE_TITLE_MAX_CHARS`] tegn.
    pub title: String,
    pub state: TicketState,
    pub escalated: bool,
    pub review_round: u32,
    pub assignee: Option<String>,
    pub playbook_started_at: Option<u64>,
    pub updated_at: u64,
    pub project: Option<String>,
    /// Antal tilbagemeldingsforsøg mens kommentaren eller lukningen er `failed` (kun egen kilde,
    /// aldrig et barn med arvet kilde).
    pub wb_failed: Option<u32>,
    /// Appens egen fejltekst for den fejlede tilbagemelding (én linje, klippet).
    pub wb_error: Option<String>,
    pub external_number: Option<u64>,
}

/// Beskeder fra én mutation (plan A.8 a), ren:
/// - `Escalated`: `escalated` falsk → sand; generation `review_round` (én pr. runde);
/// - `FlowReview`: en forløbsforælder (playbook startet, ingen ejer) går i Review; generation
///   `updated_at` (én pr. indgang);
/// - `WriteBackFailed`: `wb_failed` `None` → `Some(n)` eller `Some(a)` → `Some(b > a)`;
///   generation `n` (én pr. forsøg).
///
/// En ticket der mangler i `before` (lige oprettet), tæller som "ikke eskaleret, ikke i review,
/// ikke fejlet".
pub fn derive_ticket_notices(
    before: &[NoticeSnap],
    after: &[NoticeSnap],
    now: u64,
) -> Vec<(NoticeKey, Notice)> {
    let before: HashMap<&str, &NoticeSnap> = before.iter().map(|s| (s.id.as_str(), s)).collect();
    let mut out = Vec::new();
    for a in after {
        let b = before.get(a.id.as_str()).copied();
        let project = a.project.as_deref();
        if a.escalated && !b.is_some_and(|b| b.escalated) {
            let text = format!("{}: «{}» efter {} runder", a.short, a.title, a.review_round);
            out.push((
                NoticeKey::new(NoticeKind::Escalated, &a.id, u64::from(a.review_round)),
                Notice::new(NoticeKind::Escalated, text, now).ticket(&a.id, project),
            ));
        }
        let flow_review = a.state == TicketState::Review
            && a.assignee.is_none()
            && a.playbook_started_at.is_some();
        if flow_review && !b.is_some_and(|b| b.state == TicketState::Review) {
            let text = format!("{}: «{}»", a.short, a.title);
            out.push((
                NoticeKey::new(NoticeKind::FlowReview, &a.id, a.updated_at),
                Notice::new(NoticeKind::FlowReview, text, now).ticket(&a.id, project),
            ));
        }
        if let Some(n) = a.wb_failed {
            let new_attempt = match b.and_then(|b| b.wb_failed) {
                None => true,
                Some(prev) => n > prev,
            };
            if new_attempt {
                let err = a.wb_error.as_deref().unwrap_or("ukendt fejl");
                let text = format!("{}: {err}", a.short);
                out.push((
                    NoticeKey::new(NoticeKind::WriteBackFailed, &a.id, u64::from(n)),
                    Notice::new(NoticeKind::WriteBackFailed, text, now).ticket(&a.id, project),
                ));
            }
        }
    }
    out
}

/// Husholdningsbeskeder (plan A.8 c; kaldes af vagtens tick i Batch 4), ren:
/// - `PermissionWaiting`: en anmodning ældre end [`WAITING_NOTICE_AFTER_MS`]; nøgle =
///   request-id, generation 0;
/// - `TrustWaiting`: en agent der stadig er `Starting` med [`STARTING_HINT_TEXT`] mindst
///   [`WAITING_NOTICE_AFTER_MS`] efter at dens proces startede; nøgle = agent-id, generation =
///   den start (`last_event_at`, som hintet lader være og en genstart sætter).
pub fn derive_waiting_notices(
    pending: &[PermissionRequestInfo],
    agents: &[AgentInfo],
    now: u64,
) -> Vec<(NoticeKey, Notice)> {
    let mut out = Vec::new();
    for p in pending {
        if p.created_at.saturating_add(WAITING_NOTICE_AFTER_MS) > now {
            continue;
        }
        let agent = agents.iter().find(|a| a.id == p.agent_id);
        let name = clip_line(&p.agent_name, NOTICE_PART_MAX_CHARS);
        let tool = clip_line(&p.tool_name, NOTICE_PART_MAX_CHARS);
        let text = format!("{name}: {tool} har ventet over 1 minut");
        out.push((
            NoticeKey::new(NoticeKind::PermissionWaiting, &p.request_id, 0),
            Notice::new(NoticeKind::PermissionWaiting, text, now)
                .agent(&p.agent_id, agent.and_then(|a| a.project.as_deref())),
        ));
    }
    for a in agents {
        let waiting = a.status == AgentStatus::Starting
            && a.detail.as_deref() == Some(STARTING_HINT_TEXT)
            && a.last_event_at.saturating_add(WAITING_NOTICE_AFTER_MS) <= now;
        if !waiting {
            continue;
        }
        let name = clip_line(&a.name, NOTICE_PART_MAX_CHARS);
        let text = format!("{name} venter på svar (fx godkendelse af mappen)");
        out.push((
            NoticeKey::new(NoticeKind::TrustWaiting, &a.id, a.last_event_at),
            Notice::new(NoticeKind::TrustWaiting, text, now).agent(&a.id, a.project.as_deref()),
        ));
    }
    out
}

/// Beskeden for en agent hvis afslutning frigav `released` tickets (plan A.8 b); `None` når
/// ingen blev frigivet. Nøgle = agent-id, generation = `last_event_at` (afslutningstiden).
pub fn notice_for_exit(
    agent: &AgentInfo,
    released: usize,
    now: u64,
) -> Option<(NoticeKey, Notice)> {
    if released == 0 {
        return None;
    }
    let name = clip_line(&agent.name, NOTICE_PART_MAX_CHARS);
    let text = format!("{name}: {released} ticket(s) tilbage i backlog");
    Some((
        NoticeKey::new(NoticeKind::AgentExited, &agent.id, agent.last_event_at),
        Notice::new(NoticeKind::AgentExited, text, now).agent(&agent.id, agent.project.as_deref()),
    ))
}

/// Vagtens budgetbesked (plan A.8 d; motoren kalder den i Batch 4): `next_hhmm` er det lokale
/// klokkeslæt hvor budgettet bliver frit. Højst én pr. projekt og time (generation =
/// `now / HOUR_MS`, uanset loft). `None` ved stille timer og `Off` (ingen besked).
pub fn budget_notice(
    project: &str,
    why: WaitWhy,
    next_hhmm: &str,
    now: u64,
) -> Option<(NoticeKey, Notice)> {
    let cap = match why {
        WaitWhy::Hour => "timeloft",
        WaitWhy::Day => "dagsloft",
        WaitWhy::GlobalDay | WaitWhy::GlobalHour => "workspace-loft",
        WaitWhy::Quiet | WaitWhy::Off => return None,
    };
    let text = format!("{project}: {cap} nået — næste: {next_hhmm}");
    Some((
        NoticeKey::new(NoticeKind::BudgetReached, project, now / HOUR_MS),
        Notice::new(NoticeKind::BudgetReached, text, now).project(project),
    ))
}

/// Vagten stoppede for `project` efter [`WATCH_TRIP_AFTER`] fejl i træk (plan A.8 d).
/// Generation = `tripped_at`: én gang indtil "Genstart vagt".
pub fn tripped_notice(project: &str, tripped_at: u64, now: u64) -> (NoticeKey, Notice) {
    let text = format!("{project}: {WATCH_TRIP_AFTER} fejl i træk — se Diagnostik → Projekter");
    (
        NoticeKey::new(NoticeKind::WatchTripped, project, tripped_at),
        Notice::new(NoticeKind::WatchTripped, text, now).project(project),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::SeatKind;
    use serde_json::{json, Value};
    use std::sync::Arc;

    fn snap(id: &str) -> NoticeSnap {
        NoticeSnap {
            id: id.into(),
            short: short_id(id),
            title: "Fejl i login".into(),
            state: TicketState::InProgress,
            escalated: false,
            review_round: 0,
            assignee: Some("a1".into()),
            playbook_started_at: None,
            updated_at: 1,
            project: Some("web".into()),
            wb_failed: None,
            wb_error: None,
            external_number: None,
        }
    }

    fn agent(id: &str) -> AgentInfo {
        AgentInfo {
            id: id.into(),
            session_id: "s".into(),
            name: "debugger-01".into(),
            cwd: "/w".into(),
            status: AgentStatus::Idle,
            detail: None,
            pid: None,
            created_at: 1_000,
            last_event_at: 1_000,
            profile_id: "p".into(),
            profile_name: "P".into(),
            roles: Vec::new(),
            specialist: false,
            model: None,
            effort: None,
            model_observed: false,
            open_reviews: 0,
            seat_kind: SeatKind::Work,
            current_ticket_id: None,
            queue_length: 0,
            project: Some("web".into()),
        }
    }

    fn request(id: &str, created_at: u64) -> PermissionRequestInfo {
        PermissionRequestInfo {
            request_id: id.into(),
            agent_id: "a1".into(),
            agent_name: "coder-01".into(),
            tool_name: "Bash".into(),
            summary: "rm -rf hemmeligt".into(),
            tool_input: json!({"command": "rm -rf hemmeligt"}),
            created_at,
            deadline_at: created_at + 600_000,
        }
    }

    fn notice(kind: NoticeKind, n: u64) -> (NoticeKey, Notice) {
        (
            NoticeKey::new(kind, "x", n),
            Notice::new(kind, format!("n{n}"), n),
        )
    }

    fn kinds(v: &[(NoticeKey, Notice)]) -> Vec<NoticeKind> {
        v.iter().map(|(k, _)| k.kind).collect()
    }

    #[test]
    fn kinds_have_wire_names_labels_and_titles() {
        for k in NoticeKind::ALL {
            assert_eq!(NoticeKind::parse(k.as_str()), Some(k));
            assert_eq!(serde_json::to_value(k).unwrap(), json!(k.as_str()));
            let back: NoticeKind = serde_json::from_value(json!(k.as_str())).unwrap();
            assert_eq!(back, k);
        }
        assert_eq!(NoticeKind::parse("Escalated"), None);
        assert_eq!(NoticeKind::parse("nyType"), None);
        let labels: Vec<&str> = NoticeKind::ALL.iter().map(|k| k.label_da()).collect();
        assert_eq!(
            labels,
            [
                "eskaleret",
                "forløb til godkendelse",
                "tilladelse venter",
                "agent venter i terminalen",
                "tilbagemelding fejlede",
                "budget nået",
                "vagt stoppet",
                "agent afsluttet",
            ]
        );
        let titles: Vec<&str> = NoticeKind::ALL.iter().map(|k| k.title_da()).collect();
        assert_eq!(
            titles,
            [
                "Ticket eskaleret",
                "Forløb klar til din godkendelse",
                "Tilladelse venter",
                "Agent venter i terminalen",
                "Tilbagemelding fejlede",
                "Vagtens budget er nået",
                "Vagten er stoppet",
                "Agent afsluttede med ticket i gang",
            ]
        );
    }

    #[test]
    fn derive_escalated_once_per_round() {
        let id = "ab12cd34-0000-0000-0000-000000000000";
        let before = snap(id);
        let mut after = before.clone();
        after.state = TicketState::Review;
        after.escalated = true;
        after.review_round = 3;
        let v = derive_ticket_notices(
            std::slice::from_ref(&before),
            std::slice::from_ref(&after),
            50,
        );
        assert_eq!(v.len(), 1);
        let (key, n) = &v[0];
        assert_eq!(*key, NoticeKey::new(NoticeKind::Escalated, id, 3));
        assert_eq!(n.text, "ab12cd34: «Fejl i login» efter 3 runder");
        assert_eq!(n.title, "Ticket eskaleret");
        assert_eq!(
            (n.ticket_id.as_deref(), n.project.as_deref(), n.at, n.seen),
            (Some(id), Some("web"), 50, false)
        );
        // Still escalated: nothing new; the same snapshot twice: nothing.
        assert!(derive_ticket_notices(
            std::slice::from_ref(&after),
            std::slice::from_ref(&after),
            51
        )
        .is_empty());
        assert!(derive_ticket_notices(
            std::slice::from_ref(&before),
            std::slice::from_ref(&before),
            51
        )
        .is_empty());
        // Reviewer picked (not escalated) and escalated again in the same round: the same key.
        let mut picked = after.clone();
        picked.escalated = false;
        let again = derive_ticket_notices(&[picked], std::slice::from_ref(&after), 52);
        assert_eq!(again[0].0, NoticeKey::new(NoticeKind::Escalated, id, 3));
        // A later round: a new generation.
        let mut later = after.clone();
        later.escalated = false;
        let mut next = after;
        next.review_round = 4;
        assert_eq!(
            derive_ticket_notices(&[later], &[next], 53)[0].0.generation,
            4
        );
    }

    #[test]
    fn derive_flow_review_once_per_entry_and_not_for_assigned_parent() {
        let id = "ff00ff00-1111";
        let mut before = snap(id);
        before.state = TicketState::Backlog;
        before.assignee = None;
        before.playbook_started_at = Some(5);
        let mut after = before.clone();
        after.state = TicketState::Review;
        after.updated_at = 40;
        let v = derive_ticket_notices(
            std::slice::from_ref(&before),
            std::slice::from_ref(&after),
            41,
        );
        assert_eq!(kinds(&v), [NoticeKind::FlowReview]);
        assert_eq!(v[0].0, NoticeKey::new(NoticeKind::FlowReview, id, 40));
        assert_eq!(v[0].1.text, "ff00ff00: «Fejl i login»");
        assert_eq!(v[0].1.title, "Forløb klar til din godkendelse");
        // Staying in review: nothing.
        let mut later = after.clone();
        later.updated_at = 45;
        assert!(derive_ticket_notices(std::slice::from_ref(&after), &[later], 46).is_empty());
        // An assigned ticket in review (a child, or a parent with an owner): nothing.
        let mut owned = after.clone();
        owned.assignee = Some("a1".into());
        assert!(derive_ticket_notices(std::slice::from_ref(&before), &[owned], 47).is_empty());
        // No playbook: an ordinary unassigned ticket in review is not a flow parent.
        let mut plain_b = before.clone();
        plain_b.playbook_started_at = None;
        let mut plain_a = after.clone();
        plain_a.playbook_started_at = None;
        assert!(derive_ticket_notices(&[plain_b], &[plain_a], 48).is_empty());
        // Rejected and back in review later: a new entry, a new generation.
        let mut again = after;
        again.updated_at = 90;
        let v = derive_ticket_notices(&[before], &[again], 91);
        assert_eq!(v[0].0.generation, 90);
    }

    #[test]
    fn derive_write_back_failed_per_attempt() {
        let id = "0badc0de-2";
        let before = snap(id);
        let mut failed = before.clone();
        failed.wb_failed = Some(1);
        failed.wb_error = Some("ingen forbindelse til GitHub".into());
        let v = derive_ticket_notices(
            std::slice::from_ref(&before),
            std::slice::from_ref(&failed),
            10,
        );
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].0, NoticeKey::new(NoticeKind::WriteBackFailed, id, 1));
        assert_eq!(v[0].1.text, "0badc0de: ingen forbindelse til GitHub");
        // Still failed (another mutation): nothing.
        assert!(derive_ticket_notices(
            std::slice::from_ref(&failed),
            std::slice::from_ref(&failed),
            11
        )
        .is_empty());
        // Retry in flight (not failed), failed again with attempt 2: one more.
        let mut inflight = failed.clone();
        inflight.wb_failed = None;
        let mut second = failed.clone();
        second.wb_failed = Some(2);
        let v = derive_ticket_notices(&[inflight], std::slice::from_ref(&second), 12);
        assert_eq!(v[0].0.generation, 2);
        // A failed → failed with more attempts directly also counts; fewer does not.
        let mut third = second.clone();
        third.wb_failed = Some(3);
        assert_eq!(
            derive_ticket_notices(
                std::slice::from_ref(&second),
                std::slice::from_ref(&third),
                13
            )
            .len(),
            1
        );
        assert!(derive_ticket_notices(&[third], &[second], 14).is_empty());
    }

    #[test]
    fn derive_new_ticket_counts_from_nothing() {
        let mut a = snap("cafe0000-9");
        a.wb_failed = Some(1);
        assert_eq!(
            kinds(&derive_ticket_notices(&[], &[a], 5)),
            [NoticeKind::WriteBackFailed]
        );
        // A deleted ticket gives nothing.
        assert!(derive_ticket_notices(&[snap("x")], &[], 5).is_empty());
    }

    #[test]
    fn waiting_notices_only_after_60s() {
        let mut a = agent("a1");
        a.status = AgentStatus::Starting;
        a.detail = Some(STARTING_HINT_TEXT.into());
        a.last_event_at = 100_000;
        let reqs = [request("r1", 100_000)];
        let agents = [a.clone()];
        assert!(derive_waiting_notices(&reqs, &agents, 159_999).is_empty());
        let v = derive_waiting_notices(&reqs, &agents, 160_000);
        assert_eq!(
            kinds(&v),
            [NoticeKind::PermissionWaiting, NoticeKind::TrustWaiting]
        );
        assert_eq!(
            v[0].0,
            NoticeKey::new(NoticeKind::PermissionWaiting, "r1", 0)
        );
        assert_eq!(v[0].1.text, "coder-01: Bash har ventet over 1 minut");
        assert!(!v[0].1.text.contains("hemmeligt"), "never the tool input");
        assert_eq!(
            (v[0].1.agent_id.as_deref(), v[0].1.ticket_id.as_deref()),
            (Some("a1"), None)
        );
        assert_eq!(
            v[1].0,
            NoticeKey::new(NoticeKind::TrustWaiting, "a1", 100_000)
        );
        assert_eq!(
            v[1].1.text,
            "debugger-01 venter på svar (fx godkendelse af mappen)"
        );
        assert_eq!(v[1].1.project.as_deref(), Some("web"));
        // A queue dedups the second tick: one each, not two.
        let ctx = NoticesCtx::new(Arc::new(|_, _| {}));
        assert_eq!(ctx.push_all(v, 160_000), 2);
        assert_eq!(
            ctx.push_all(derive_waiting_notices(&reqs, &agents, 220_000), 220_000),
            0
        );
        // Not Starting, or another detail: no trust notice.
        let mut idle = a.clone();
        idle.status = AgentStatus::Idle;
        let mut other = a.clone();
        other.detail = Some("Genstarter …".into());
        assert!(derive_waiting_notices(&[], &[idle, other], 500_000).is_empty());
        // A restart (new start time) is a new generation.
        let mut restarted = a;
        restarted.last_event_at = 400_000;
        let v = derive_waiting_notices(&[], &[restarted], 460_000);
        assert_eq!(v[0].0.generation, 400_000);
    }

    #[test]
    fn exit_notice_only_with_released_tickets() {
        let mut a = agent("a9");
        a.status = AgentStatus::Exited { code: Some(1) };
        a.last_event_at = 7_000;
        assert_eq!(notice_for_exit(&a, 0, 7_001), None);
        let (key, n) = notice_for_exit(&a, 2, 7_001).unwrap();
        assert_eq!(key, NoticeKey::new(NoticeKind::AgentExited, "a9", 7_000));
        assert_eq!(n.text, "debugger-01: 2 ticket(s) tilbage i backlog");
        assert_eq!(n.title, "Agent afsluttede med ticket i gang");
        assert_eq!(
            (n.agent_id.as_deref(), n.project.as_deref()),
            (Some("a9"), Some("web"))
        );
    }

    #[test]
    fn budget_and_tripped_notices() {
        let now = 10 * HOUR_MS + 5;
        let (k, n) = budget_notice("web", WaitWhy::Hour, "14:05", now).unwrap();
        assert_eq!(k, NoticeKey::new(NoticeKind::BudgetReached, "web", 10));
        assert_eq!(n.text, "web: timeloft nået — næste: 14:05");
        assert_eq!(n.project.as_deref(), Some("web"));
        assert_eq!(
            budget_notice("web", WaitWhy::Day, "07:00", now)
                .unwrap()
                .1
                .text,
            "web: dagsloft nået — næste: 07:00"
        );
        for why in [WaitWhy::GlobalDay, WaitWhy::GlobalHour] {
            assert_eq!(
                budget_notice("web", why, "14:05", now).unwrap().1.text,
                "web: workspace-loft nået — næste: 14:05"
            );
        }
        assert!(budget_notice("web", WaitWhy::Quiet, "07:00", now).is_none());
        assert!(budget_notice("web", WaitWhy::Off, "07:00", now).is_none());
        // Two refusals within the same hour (also for another cap): one notice.
        let ctx = NoticesCtx::new(Arc::new(|_, _| {}));
        let first = budget_notice("web", WaitWhy::Hour, "14:05", now).unwrap();
        let second = budget_notice("web", WaitWhy::Day, "07:00", now + 1_000).unwrap();
        assert_eq!(ctx.push_all(vec![first, second], now + 1_000), 1);
        let next_hour = budget_notice("web", WaitWhy::Hour, "15:05", now + HOUR_MS).unwrap();
        assert_eq!(ctx.push_all(vec![next_hour], now + HOUR_MS), 1);

        let (k, n) = tripped_notice("web", 99, 100);
        assert_eq!(k, NoticeKey::new(NoticeKind::WatchTripped, "web", 99));
        assert_eq!(n.text, "web: 3 fejl i træk — se Diagnostik → Projekter");
        assert_eq!(n.title, "Vagten er stoppet");
    }

    #[test]
    fn queue_dedups_on_key_and_caps_at_100() {
        let mut q = NoticeQueue::new();
        let (k, n) = notice(NoticeKind::Escalated, 1);
        assert!(q.push(k.clone(), n.clone(), 1));
        assert!(!q.push(
            k.clone(),
            Notice::new(NoticeKind::Escalated, "igen".into(), 2),
            2
        ));
        assert_eq!(q.list().len(), 1);
        for i in 2..=150 {
            let (k, n) = notice(NoticeKind::AgentExited, i);
            assert!(q.push(k, n, i));
        }
        let list = q.list();
        assert_eq!(list.len(), NOTICES_MAX);
        assert_eq!(list[0].text, "n150", "newest first");
        assert_eq!(list[99].text, "n51", "the oldest went");
        // The first key is still known after its notice left the queue.
        assert!(!q.push(k, n, 200));
        assert_eq!(q.unread(), 100);
    }

    #[test]
    fn queue_skips_off_kinds_entirely() {
        let mut q = NoticeQueue::new();
        let (k1, n1) = notice(NoticeKind::BudgetReached, 1);
        assert!(q.push(k1, n1, 1));
        let (k2, n2) = notice(NoticeKind::Escalated, 2);
        assert!(q.push(k2, n2, 2));
        // Unknown names are ignored; the queue loses the notices of the type now off.
        assert!(q.set_off(&["budgetReached".into(), "nyType".into()]));
        assert_eq!(
            q.off().iter().copied().collect::<Vec<_>>(),
            [NoticeKind::BudgetReached]
        );
        assert_eq!(kinds_of(&q), [NoticeKind::Escalated]);
        let (k3, n3) = notice(NoticeKind::BudgetReached, 3);
        assert!(!q.push(k3.clone(), n3.clone(), 3), "not created");
        assert_eq!(q.payload().unread, 1);
        // On again: a later one comes in (it was never remembered).
        assert!(!q.set_off(&[]));
        assert!(q.push(k3, n3, 4));
    }

    fn kinds_of(q: &NoticeQueue) -> Vec<NoticeKind> {
        q.list().iter().map(|n| n.kind).collect()
    }

    #[test]
    fn mark_seen_some_and_all() {
        let mut q = NoticeQueue::new();
        for i in 0..3 {
            let (k, n) = notice(NoticeKind::Escalated, i);
            q.push(k, n, i);
        }
        let ids: Vec<String> = q.list().iter().map(|n| n.id.clone()).collect();
        assert_eq!(q.mark_seen(Some(&[ids[1].clone(), "ukendt".into()])), 1);
        assert_eq!(q.unread(), 2);
        assert!(q.list()[1].seen);
        assert_eq!(q.mark_seen(Some(&[ids[1].clone()])), 0, "already seen");
        assert_eq!(q.mark_seen(None), 2);
        assert_eq!(q.unread(), 0);
        assert_eq!(q.list().len(), 3, "seen notices stay");
    }

    #[test]
    fn prune_forgets_keys_after_24h_but_keeps_items() {
        let mut q = NoticeQueue::new();
        let (k, n) = notice(NoticeKind::TrustWaiting, 1);
        q.push(k.clone(), n.clone(), 1_000);
        q.prune(1_000 + NOTICE_SEEN_TTL_MS - 1);
        assert!(!q.push(k.clone(), n.clone(), 2_000), "still remembered");
        q.prune(1_000 + NOTICE_SEEN_TTL_MS);
        assert_eq!(q.list().len(), 1, "the notice stays");
        assert!(q.push(k, n, 1_000 + NOTICE_SEEN_TTL_MS), "forgotten");
        assert_eq!(q.list().len(), 2);
    }

    #[test]
    fn payload_is_camel_case() {
        let mut n = Notice::new(
            NoticeKind::Escalated,
            "ab12cd34: «x» efter 3 runder".into(),
            7,
        );
        n.id = "u-1".into();
        n.ticket_id = Some("t-1".into());
        n.project = Some("web".into());
        let p = NoticesPayload {
            unread: 1,
            items: vec![n],
        };
        let v = serde_json::to_value(&p).unwrap();
        assert_eq!(
            v,
            json!({
                "unread": 1,
                "items": [{
                    "id": "u-1",
                    "kind": "escalated",
                    "at": 7,
                    "title": "Ticket eskaleret",
                    "text": "ab12cd34: «x» efter 3 runder",
                    "ticketId": "t-1",
                    "agentId": null,
                    "project": "web",
                    "seen": false
                }]
            })
        );
        // Key order as C6d.2.
        let s = serde_json::to_string(&p.items[0]).unwrap();
        let order: Vec<usize> = [
            "\"id\"",
            "\"kind\"",
            "\"at\"",
            "\"title\"",
            "\"text\"",
            "\"ticketId\"",
            "\"agentId\"",
            "\"project\"",
            "\"seen\"",
        ]
        .iter()
        .map(|k| s.find(k).unwrap())
        .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{s}");
    }

    #[test]
    fn ctx_emits_once_per_change_and_never_under_the_lock() {
        let events: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
        let sink = Arc::clone(&events);
        let me: Arc<std::sync::OnceLock<std::sync::Weak<NoticesCtx>>> = Arc::default();
        let probe = Arc::clone(&me);
        let ctx = Arc::new(NoticesCtx::new(Arc::new(move |name: &str, v: Value| {
            // The queue lock is free during the emit.
            let me = probe.get().and_then(std::sync::Weak::upgrade).unwrap();
            assert!(me.queue.try_lock().is_ok());
            sink.lock().unwrap().push((name.to_string(), v));
        })));
        me.set(Arc::downgrade(&ctx)).unwrap();
        let count = || events.lock().unwrap().len();
        assert_eq!(ctx.push_all(Vec::new(), 1), 0);
        assert_eq!(count(), 0);
        let v = vec![
            notice(NoticeKind::Escalated, 1),
            notice(NoticeKind::FlowReview, 2),
        ];
        assert_eq!(ctx.push_all(v.clone(), 3), 2);
        assert_eq!(count(), 1, "one emit for both");
        assert_eq!(ctx.push_all(v, 4), 0, "known keys");
        assert_eq!(count(), 1);
        {
            let e = events.lock().unwrap();
            assert_eq!(e[0].0, NOTICES_CHANGED);
            assert_eq!(e[0].1["unread"], 2);
        }
        let p = ctx.mark_seen(None);
        assert_eq!((p.unread, count()), (0, 2));
        ctx.mark_seen(None);
        assert_eq!(count(), 2, "nothing changed");
        let p = ctx.set_off(&["flowReview".into()]);
        assert_eq!((p.items.len(), count()), (1, 3));
        ctx.set_off(&["flowReview".into()]);
        assert_eq!(count(), 3);
        assert_eq!(ctx.payload().items[0].kind, NoticeKind::Escalated);
    }

    #[test]
    fn titles_are_one_line_and_clipped() {
        assert_eq!(clip_line("  a\r\nb\tc\u{7}  ", 80), "a  b c");
        let long = "x".repeat(200);
        let c = clip_line(&long, NOTICE_TITLE_MAX_CHARS);
        assert_eq!(c.chars().count(), NOTICE_TITLE_MAX_CHARS);
        assert!(c.ends_with('…'));
        assert_eq!(clip_line(&"æ".repeat(80), 80), "æ".repeat(80));
    }
}
