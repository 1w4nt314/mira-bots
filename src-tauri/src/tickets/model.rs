//! Ticket data model (plan C3.1). Wire format: camelCase; enum values camelCase strings.

use serde::{Deserialize, Deserializer, Serialize};

use crate::config::{
    AGENTS_MAY_CREATE_PROJECTS, AUTO_REVIEW_ON_STOP, AUTO_SPAWN_FOR_PLAYBOOK, CHECKS_GATE,
    CLEANUP_WORKTREES_ON_DONE, CREATE_TICKET_RATE_LIMIT, FRESH_SESSION_PER_TICKET, GIT_DEFAULT,
    MAX_AGENTS_PER_PROJECT, MAX_REVIEW_ROUNDS, MAX_STAFF_AGENTS, MAX_WORK_AGENTS,
    REPORTS_PER_TICKET_MAX, REPORT_BODY_MAX_CHARS, REVIEW_BY_DEFAULT, TICKETS_SCHEMA_VERSION,
    TICKET_BODY_MAX_CHARS, TICKET_SHORT_ID_LEN, USER_INPUT_GRACE_MS,
};
use crate::projects::ProjectRef;

/// Ticket id (uuid v4 string).
pub type TicketId = String;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum TicketState {
    Backlog,
    Assigned,
    InProgress,
    Review,
    Done,
    Rejected,
    /// A parent whose assignee submitted it while children were still open (step 6a): it keeps
    /// its assignee but is not "current"; it is woken when a child is done.
    Waiting,
}

impl TicketState {
    pub const ALL: [TicketState; 7] = [
        TicketState::Backlog,
        TicketState::Assigned,
        TicketState::InProgress,
        TicketState::Review,
        TicketState::Done,
        TicketState::Rejected,
        TicketState::Waiting,
    ];

    /// Danish UI label (same as `STATE_LABEL` in the frontend).
    pub fn label_da(self) -> &'static str {
        match self {
            TicketState::Backlog => "Backlog",
            TicketState::Assigned => "I kø",
            TicketState::InProgress => "I gang",
            TicketState::Review => "Review",
            TicketState::Done => "Done",
            TicketState::Rejected => "Afvist",
            TicketState::Waiting => "Venter",
        }
    }

    /// The wire string (`"inProgress"` etc.).
    pub fn as_str(self) -> &'static str {
        match self {
            TicketState::Backlog => "backlog",
            TicketState::Assigned => "assigned",
            TicketState::InProgress => "inProgress",
            TicketState::Review => "review",
            TicketState::Done => "done",
            TicketState::Rejected => "rejected",
            TicketState::Waiting => "waiting",
        }
    }
}

/// Who caused a history entry.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum TicketActor {
    User,
    System,
    Agent,
}

/// Where a ticket came from: the user (UI) or an agent (`mira_create_ticket`).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum TicketSource {
    User,
    Agent,
}

/// A problem the UI should show on the ticket.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum TicketIssue {
    /// The typed line was not confirmed (no `UserPromptSubmit` / busy status).
    DeliveryFailed,
    /// The turn ended with `StopFailure`.
    TurnFailed,
    /// The turn ended (Stop) without `mira_submit_for_review`; the ticket stays in progress.
    NotSubmitted,
}

/// How the app gives a work ticket its own git branch (step 6b, workspace rule `git`).
/// Wire: `"off"|"branch"|"worktree"`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GitMode {
    #[default]
    Off,
    /// `git switch ticket/<short>` in the project folder.
    Branch,
    /// A worktree `<project>/.mira-bots/wt/<short>` on branch `ticket/<short>`.
    Worktree,
}

impl GitMode {
    /// Wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            GitMode::Off => "off",
            GitMode::Branch => "branch",
            GitMode::Worktree => "worktree",
        }
    }

    /// Wire name → mode, ASCII-case-insensitive and trimmed (the workspace file).
    pub fn parse(s: &str) -> Option<GitMode> {
        let s = s.trim();
        [GitMode::Off, GitMode::Branch, GitMode::Worktree]
            .into_iter()
            .find(|m| m.as_str().eq_ignore_ascii_case(s))
    }
}

/// State of a ticket's project checks (step 6b).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ChecksState {
    Pending,
    Passed,
    Failed,
    /// No project file, no checks or an unreadable file: nothing ran.
    Skipped,
}

/// The project checks of the ticket's current review entry (step 6b); reset on Submit.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TicketChecks {
    pub state: ChecksState,
    /// Name of the first failed check.
    pub failed: Option<String>,
    /// The ticket's `review_round` when the checks started.
    pub round: u32,
    /// Unix ms.
    pub started_at: u64,
}

/// The ticket's git branch, prepared by the app at delivery (step 6b). Kept on the ticket so the
/// review file and the checks do not depend on the sender's cwd.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TicketGit {
    pub mode: GitMode,
    /// `ticket/<short id>`.
    pub branch: String,
    pub base: String,
    /// The project's repository folder.
    pub repo: String,
    /// The worktree folder (`worktree` mode only).
    pub worktree: Option<String>,
}

/// Where an external ticket came from (step 6c). Wire: `"folder"|"github"`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum ExternalKind {
    Folder,
    Github,
}

impl ExternalKind {
    /// Wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            ExternalKind::Folder => "folder",
            ExternalKind::Github => "github",
        }
    }
}

/// State of one write-back step (step 6c, plan A.7). Wire: `"none"|"inflight"|"done"|"failed"`.
/// `inflight` is saved before the call; at startup it becomes `failed` (the call may or may not
/// have reached the source, so only a click tries again, with the marker check first).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum WriteBackState {
    #[default]
    None,
    Inflight,
    Done,
    Failed,
}

/// The report back to the source when the ticket is Done (step 6c, C6c.2). Every field has a
/// default, so an older or partial object still reads.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct WriteBack {
    /// The comment (GitHub) or the `.result.md` (folder).
    pub comment: WriteBackState,
    /// Closing the issue (GitHub only, after a `done` comment).
    pub close: WriteBackState,
    pub comment_url: Option<String>,
    /// Unix ms.
    pub commented_at: Option<u64>,
    /// Unix ms.
    pub closed_at: Option<u64>,
    /// Write attempts so far (> 0: look for the marker before posting again).
    pub attempts: u32,
    pub last_error: Option<String>,
    /// The text of the last attempt (kept, not shown in 6c).
    pub last_body: Option<String>,
}

/// The external source of a ticket started from the inbox (step 6c, C6c.2). Set by the app,
/// never from the external text; `TicketSource` stays `user` (the user clicked Start).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExternalRef {
    pub kind: ExternalKind,
    /// `github:owner/name#123` or `folder:<project|_rod>:<relative path>`.
    pub external_id: String,
    /// `owner/name` (GitHub).
    #[serde(default)]
    pub repo: Option<String>,
    /// Issue number (GitHub).
    #[serde(default)]
    pub number: Option<u64>,
    /// Path relative to the inbox folder (folder).
    #[serde(default)]
    pub path: Option<String>,
    /// Issue URL (GitHub).
    #[serde(default)]
    pub url: Option<String>,
    /// The cleaned external title (only in the ticket file, never in a typed line).
    pub title: String,
    /// Cleaned labels (B1 addition to C6c.2: the ticket file's "labels" line).
    #[serde(default)]
    pub labels: Vec<String>,
    /// Cleaned author login (B1 addition to C6c.2: the ticket file's "oprindelig forfatter").
    #[serde(default)]
    pub author: Option<String>,
    /// Sanitising notes ("2 HTML-kommentar(er) fjernet", …), shown in the ticket file.
    #[serde(default)]
    pub notes: Vec<String>,
    /// The inbox item this ticket was started from.
    pub inbox_item_id: String,
    /// Unix ms.
    pub imported_at: u64,
    #[serde(default)]
    pub write_back: WriteBack,
}

impl ExternalRef {
    /// The `{kilde}` of C6c.4/C6c.5: `GitHub issue #{n} i {repo}` or `filen {path} i indbakken`
    /// (repo and path sanitised like a title: one line, no invisible chars).
    pub fn source_label(&self) -> String {
        use super::prompt::sanitize_title;
        match self.kind {
            ExternalKind::Github => match (self.number, self.repo.as_deref()) {
                (Some(n), Some(r)) => format!("GitHub issue #{n} i {}", sanitize_title(r)),
                _ => "GitHub issue".to_string(),
            },
            ExternalKind::Folder => match self.path.as_deref() {
                Some(p) => format!("filen {} i indbakken", sanitize_title(p)),
                None => "en fil i indbakken".to_string(),
            },
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TicketHistoryEntry {
    /// Unix ms.
    pub at: u64,
    /// `None` only for the creation entry.
    pub from: Option<TicketState>,
    pub to: TicketState,
    pub by: TicketActor,
    pub note: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Ticket {
    pub id: TicketId,
    pub title: String,
    pub body: String,
    pub state: TicketState,
    pub assignee_agent_id: Option<String>,
    /// 0-based position in the assignee's queue; only set while `assigned`.
    pub queue_position: Option<usize>,
    pub skip_review: bool,
    pub source: TicketSource,
    pub issue: Option<TicketIssue>,
    pub rejection_note: Option<String>,
    /// The agent's summary from its last `mira_submit_for_review`. Absent in step-3 files.
    #[serde(default)]
    pub summary: Option<String>,
    /// Unix ms.
    pub created_at: u64,
    /// Unix ms.
    pub updated_at: u64,
    pub history: Vec<TicketHistoryEntry>,
    /// Review rejections so far (plan5 A.6); reset when the ticket goes back to the backlog.
    #[serde(default)]
    pub review_round: u32,
    /// Reached the workspace's `maxReviewRounds` on entering review: no automatic routing, the
    /// user decides.
    #[serde(default)]
    pub escalated: bool,
    /// The reviewer agent while in review (kept after approval for display).
    #[serde(default)]
    pub reviewer_agent_id: Option<String>,
    /// Report metadata; the texts are files under `<app_data>/tickets/<id>/reports/`.
    #[serde(default)]
    pub reports: Vec<TicketReport>,
    /// The project the ticket belongs to (plan4b A.2); absent in step 1–5 files.
    #[serde(default)]
    pub project: Option<ProjectRef>,
    /// The parent ticket (step 6a): this ticket is one of its children. Absent before 6a.
    #[serde(default)]
    pub parent_id: Option<TicketId>,
    /// Tickets that must be Done before this one is delivered (step 6a). A missing id (deleted
    /// ticket) does not block. Absent before 6a.
    #[serde(default)]
    pub blocked_by: Vec<TicketId>,
    /// Ticket type (step 6b): `None` = a plain task, otherwise a playbook name (`feature`, `bug`
    /// or one from the workspace file). Absent before 6b.
    #[serde(default)]
    pub kind: Option<String>,
    /// Unix ms when the playbook was rolled out on this ticket (the "forløb" marker). Absent
    /// before 6b.
    #[serde(default)]
    pub playbook_started_at: Option<u64>,
    /// Project checks of the current review entry (step 6b). Absent before 6b.
    #[serde(default)]
    pub checks: Option<TicketChecks>,
    /// The ticket's git branch/worktree (step 6b). Absent before 6b.
    #[serde(default)]
    pub git: Option<TicketGit>,
    /// The external source (step 6c: started from the inbox). Absent before 6c.
    #[serde(default)]
    pub external: Option<ExternalRef>,
}

impl Ticket {
    pub fn short_id(&self) -> String {
        short_id(&self.id)
    }
}

/// A ticket without its history and without its body (payload of `tickets-changed` /
/// `list_tickets`). The body can be up to `TICKET_BODY_MAX_CHARS` and is not needed for the
/// lists; `get_ticket` returns it (review3 F3).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TicketSummary {
    pub id: TicketId,
    pub short_id: String,
    pub title: String,
    pub state: TicketState,
    pub assignee_agent_id: Option<String>,
    pub queue_position: Option<usize>,
    pub skip_review: bool,
    pub source: TicketSource,
    pub issue: Option<TicketIssue>,
    pub rejection_note: Option<String>,
    /// Copy of `Ticket.summary` (the review card shows it without `get_ticket`).
    pub summary: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    pub history_len: usize,
    pub review_round: u32,
    pub escalated: bool,
    pub reviewer_agent_id: Option<String>,
    pub report_count: usize,
    /// Copy of `Ticket.project` (badge and filter without `get_ticket`).
    pub project: Option<ProjectRef>,
    /// Copy of `Ticket.parent_id` (step 6a; child counts are derived in the UI).
    pub parent_id: Option<TicketId>,
    /// Copy of `Ticket.blocked_by` (step 6a).
    pub blocked_by: Vec<TicketId>,
    /// Copies of the step 6b fields.
    pub kind: Option<String>,
    pub playbook_started_at: Option<u64>,
    pub checks: Option<TicketChecks>,
    pub git: Option<TicketGit>,
    /// Copy of `Ticket.external` (step 6c; badge and write-back state without `get_ticket`).
    pub external: Option<ExternalRef>,
}

impl From<&Ticket> for TicketSummary {
    fn from(t: &Ticket) -> Self {
        TicketSummary {
            id: t.id.clone(),
            short_id: t.short_id(),
            title: t.title.clone(),
            state: t.state,
            assignee_agent_id: t.assignee_agent_id.clone(),
            queue_position: t.queue_position,
            skip_review: t.skip_review,
            source: t.source,
            issue: t.issue,
            rejection_note: t.rejection_note.clone(),
            summary: t.summary.clone(),
            created_at: t.created_at,
            updated_at: t.updated_at,
            history_len: t.history.len(),
            review_round: t.review_round,
            escalated: t.escalated,
            reviewer_agent_id: t.reviewer_agent_id.clone(),
            report_count: t.reports.len(),
            project: t.project.clone(),
            parent_id: t.parent_id.clone(),
            blocked_by: t.blocked_by.clone(),
            kind: t.kind.clone(),
            playbook_started_at: t.playbook_started_at,
            checks: t.checks.clone(),
            git: t.git.clone(),
            external: t.external.clone(),
        }
    }
}

/// Partial update from `update_ticket`; absent fields stay unchanged.
#[derive(Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TicketPatch {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub skip_review: Option<bool>,
    /// Absent: unchanged; `null`: remove the project; a [`ProjectRef`]: set it.
    #[serde(default, deserialize_with = "double_option")]
    pub project: Option<Option<ProjectRef>>,
}

/// A present field (even `null`) becomes `Some(..)`; an absent one stays `None` (serde default).
fn double_option<'de, D>(d: D) -> Result<Option<Option<ProjectRef>>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<ProjectRef>::deserialize(d).map(Some)
}

/// Who wrote a report: an agent (`agentId`), the user or the app itself (step 6b: the
/// «Tjek»/«Ændringer» reports; shown as "appen"). An older build does not know `system` and
/// would quarantine a `tickets.json` containing it (same class as `waiting` in 6a).
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ReportAuthorKind {
    Agent,
    User,
    System,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReportAuthor {
    pub kind: ReportAuthorKind,
    pub agent_id: Option<String>,
}

impl ReportAuthor {
    pub fn user() -> Self {
        ReportAuthor {
            kind: ReportAuthorKind::User,
            agent_id: None,
        }
    }

    pub fn agent(agent_id: &str) -> Self {
        ReportAuthor {
            kind: ReportAuthorKind::Agent,
            agent_id: Some(agent_id.to_string()),
        }
    }

    /// The app itself (step 6b).
    pub fn system() -> Self {
        ReportAuthor {
            kind: ReportAuthorKind::System,
            agent_id: None,
        }
    }
}

/// A report on a ticket (plan5 C5.1). The text is the file `<app_data>/tickets/<ticketId>/<path>`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TicketReport {
    /// Sequence number, two digits (`"01"`).
    pub id: String,
    pub title: String,
    pub author: ReportAuthor,
    /// Unix ms.
    pub created_at: u64,
    /// Relative to the ticket's folder, `/`-separated: `reports/01-slug.md`.
    pub path: String,
    /// Bytes.
    pub size: u64,
}

/// An open review of a ticket by a reviewer agent (plan5 A.6). Removed when the ticket leaves
/// review or the reviewer goes away.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReviewAssignment {
    pub ticket_id: TicketId,
    pub reviewer_agent_id: String,
    /// The ticket's `review_round` when it was assigned.
    pub round: u32,
    /// Unix ms.
    pub assigned_at: u64,
    /// Unix ms; `None` until the review line was confirmed in the reviewer's terminal.
    pub delivered_at: Option<u64>,
    /// Failed deliveries; at [`crate::config::REVIEW_DELIVERY_MAX_ATTEMPTS`] no more automatic
    /// tries until "Send igen".
    #[serde(default)]
    pub attempts: u32,
}

/// The whole store document (`tickets.json`):
/// `{"schemaVersion":1,"tickets":[…],"reviewAssignments":[…]}`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TicketDoc {
    pub schema_version: u32,
    pub tickets: Vec<Ticket>,
    #[serde(default)]
    pub review_assignments: Vec<ReviewAssignment>,
}

impl Default for TicketDoc {
    fn default() -> Self {
        TicketDoc {
            schema_version: TICKETS_SCHEMA_VERSION,
            tickets: Vec::new(),
            review_assignments: Vec::new(),
        }
    }
}

/// First [`TICKET_SHORT_ID_LEN`] chars of the id without dashes, lowercase.
pub fn short_id(id: &str) -> String {
    id.chars()
        .filter(|c| *c != '-')
        .take(TICKET_SHORT_ID_LEN)
        .flat_map(char::to_lowercase)
        .collect()
}

/// Ticket errors. The messages are user-facing (Danish) because commands pass them to the UI.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TicketError {
    #[error("Ticketen findes ikke")]
    NotFound,
    #[error("Kan ikke flytte en ticket fra {} til {}", .from.label_da(), .to.label_da())]
    IllegalTransition { from: TicketState, to: TicketState },
    #[error("Afvisning kræver en note")]
    NeedsNote,
    #[error("Agenten har allerede en ticket i gang")]
    AgentBusy,
    /// Step 5c handoff to the agent that already has the ticket.
    #[error("Ticketen kan ikke gives videre til den agent, der allerede har den")]
    HandoffToSelf,
    #[error("Kun tickets i backlog, done eller afvist uden agent kan slettes")]
    NotDeletable,
    #[error("{0}")]
    Validation(String),
    #[error("Brug Tildel for at sætte en ticket i kø")]
    UseAssign,
    #[error("Brug Afvis med note")]
    UseReject,
    #[error("Done kræver review (eller skipReview på ticketen)")]
    DoneNeedsReview,
    #[error("Agenten kører ikke")]
    AgentNotLive,
    #[error("Kunne ikke gemme tickets: {0}")]
    Io(String),
    /// The service started read-only because `tickets.json` could not be read (see
    /// `TicketService::load_and_recover`).
    #[error("Tickets-filen kunne ikke læses ved opstart; ændringer er slået fra. Genstart appen.")]
    ReadOnly,
    /// Agent tools: the ticket belongs to (or is waiting for) someone else.
    #[error("Ticketen er tildelt en anden agent")]
    NotYours,
    #[error("Du har ingen ticket i gang")]
    NoTicketInProgress,
    #[error("Ticketen er ikke i gang")]
    NotInProgress,
    #[error("For mange tickets oprettet den seneste time (maks 20)")]
    RateLimited,
    // ---- step 5 (C5.6) ----
    #[error("Ticketen er ikke i review")]
    NotInReview,
    #[error("Du er ikke reviewer på denne ticket")]
    NotYourReview,
    #[error("Du kan ikke reviewe din egen aflevering")]
    OwnSubmission,
    #[error("Ticketen har allerede 20 rapporter")]
    TooManyReports,
    #[error("Rapporten findes ikke")]
    ReportNotFound,
    #[error("Agenten er ikke reviewer")]
    NotAReviewer,
    #[error("Agenten arbejder")]
    AgentWorking,
    /// `assign_reviewer` with the ticket's own sender (C5.4).
    #[error("Afsenderen kan ikke reviewe sin egen ticket")]
    SenderCannotReview,
    // ---- step 4b (C4b.1) ----
    #[error("Ticketen mangler et projekt — vælg et, før den tildeles")]
    ProjectRequired,
    #[error(
        "Agenten {agent} står i projekt «{agent_project}»; ticketen hører til «{ticket_project}»"
    )]
    WrongProject {
        agent: String,
        agent_project: String,
        ticket_project: String,
    },
    #[error("Projektet kan kun ændres, mens ticketen ligger i Backlog eller er afvist uden agent")]
    ProjectChangeNotAllowed,
    // ---- step 6a (forløb) ----
    #[error("Forælderen findes ikke")]
    ParentNotFound,
    #[error("Forælderen er allerede færdig (Done)")]
    ParentDone,
    #[error("Del-ticketen hører til «{child}», men forælderen til «{parent}»")]
    ParentProjectMismatch { parent: String, child: String },
    #[error("Blokeringen {0} findes ikke")]
    BlockerNotFound(String),
    #[error("En ticket kan ikke blokeres af sin egen forælder")]
    BlockedByAncestor,
    #[error("Relationen ville danne en cyklus")]
    Cycle,
    /// The ticket has open blockers (short ids, comma-separated).
    #[error("Ticketen venter på {0}")]
    Blocked(String),
    #[error("Højst 10 blokeringer pr. ticket")]
    TooManyBlockers,
    // ---- step 6b (playbooks) ----
    #[error("Ingen playbook for «{0}»")]
    NoPlaybook(String),
    #[error("Forløbet er allerede startet (ticketen har del-tickets)")]
    PlaybookAlreadyStarted,
    #[error("kind skal være task, feature, bug eller et playbook-navn fra workspace-filen")]
    InvalidKind,
    #[error("Playbooken kan ikke udrulles: {0}")]
    PlaybookStepsInvalid(String),
    // ---- step 6c (inbox) ----
    /// Start of an inbox item that already has a ticket (short id).
    #[error("Issue/filen er allerede startet som ticket {0}")]
    ExternalAlreadyStarted(String),
}

/// The rules of this workspace (plan5 C5.1): the "Regler" section of every profile's system
/// prompt and the result of `mira_get_workspace_rules`. Defaults from `config.rs`; the effective
/// values come from `mira-bots.workspace.json` (plan4b A.4, `crate::workspace`).
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRules {
    pub max_work_agents: usize,
    pub max_staff_agents: usize,
    pub max_review_rounds: u32,
    pub auto_review_on_stop: bool,
    pub create_ticket_rate_limit: usize,
    pub ticket_body_max_chars: usize,
    pub report_body_max_chars: usize,
    pub reports_per_ticket_max: usize,
    // step 4b
    pub review_by_default: bool,
    pub user_input_grace_ms: u64,
    pub agents_may_create_projects: bool,
    /// 0 = unlimited.
    pub max_agents_per_project: usize,
    // step 6b (the playbooks map and `gitBase` are in `workspace::WorkspaceConfig`, so the rules
    // stay `Copy`)
    pub git: GitMode,
    pub checks_gate: bool,
    pub auto_spawn_for_playbook: bool,
    pub fresh_session_per_ticket: bool,
    pub cleanup_worktrees_on_done: bool,
}

impl WorkspaceRules {
    /// The values from `config.rs`.
    pub fn defaults() -> Self {
        Self {
            max_work_agents: MAX_WORK_AGENTS,
            max_staff_agents: MAX_STAFF_AGENTS,
            max_review_rounds: MAX_REVIEW_ROUNDS,
            auto_review_on_stop: AUTO_REVIEW_ON_STOP,
            create_ticket_rate_limit: CREATE_TICKET_RATE_LIMIT,
            ticket_body_max_chars: TICKET_BODY_MAX_CHARS,
            report_body_max_chars: REPORT_BODY_MAX_CHARS,
            reports_per_ticket_max: REPORTS_PER_TICKET_MAX,
            review_by_default: REVIEW_BY_DEFAULT,
            user_input_grace_ms: USER_INPUT_GRACE_MS,
            agents_may_create_projects: AGENTS_MAY_CREATE_PROJECTS,
            max_agents_per_project: MAX_AGENTS_PER_PROJECT,
            git: GIT_DEFAULT,
            checks_gate: CHECKS_GATE,
            auto_spawn_for_playbook: AUTO_SPAWN_FOR_PLAYBOOK,
            fresh_session_per_ticket: FRESH_SESSION_PER_TICKET,
            cleanup_worktrees_on_done: CLEANUP_WORKTREES_ON_DONE,
        }
    }
}

impl From<TicketError> for String {
    fn from(e: TicketError) -> Self {
        e.to_string()
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// A backlog ticket with a fixed id and creation entry (for state/service/prompt tests).
    pub fn ticket(id: &str, state: TicketState) -> Ticket {
        Ticket {
            id: id.to_string(),
            title: "Fix the bug".into(),
            body: "Details".into(),
            state,
            assignee_agent_id: None,
            queue_position: None,
            skip_review: false,
            source: TicketSource::User,
            issue: None,
            rejection_note: None,
            summary: None,
            created_at: 1_000,
            updated_at: 1_000,
            history: vec![TicketHistoryEntry {
                at: 1_000,
                from: None,
                to: TicketState::Backlog,
                by: TicketActor::User,
                note: None,
            }],
            review_round: 0,
            escalated: false,
            reviewer_agent_id: None,
            reports: Vec::new(),
            project: None,
            parent_id: None,
            blocked_by: Vec::new(),
            kind: None,
            playbook_started_at: None,
            checks: None,
            git: None,
            external: None,
        }
    }

    /// A GitHub [`ExternalRef`] for issue `#n` in `o/r` (step 6c tests).
    pub fn github_ref(n: u64) -> ExternalRef {
        ExternalRef {
            kind: ExternalKind::Github,
            external_id: format!("github:o/r#{n}"),
            repo: Some("o/r".into()),
            number: Some(n),
            path: None,
            url: Some(format!("https://github.com/o/r/issues/{n}")),
            title: "Crash ved start".into(),
            labels: vec!["bug".into()],
            author: Some("alice".into()),
            notes: Vec::new(),
            inbox_item_id: "item-1".into(),
            imported_at: 5,
            write_back: WriteBack::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::ticket;
    use super::*;
    use serde_json::json;

    #[test]
    fn enums_serialize_as_camel_case_strings() {
        let states: Vec<_> = TicketState::ALL
            .iter()
            .map(|s| serde_json::to_value(s).unwrap())
            .collect();
        assert_eq!(
            states,
            vec![
                json!("backlog"),
                json!("assigned"),
                json!("inProgress"),
                json!("review"),
                json!("done"),
                json!("rejected"),
                json!("waiting")
            ]
        );
        assert_eq!(TicketState::ALL.len(), 7);
        assert_eq!(TicketState::Waiting.label_da(), "Venter");
        assert_eq!(
            serde_json::from_value::<TicketState>(json!("waiting")).unwrap(),
            TicketState::Waiting
        );
        for s in TicketState::ALL {
            assert_eq!(serde_json::to_value(s).unwrap(), json!(s.as_str()));
        }
        assert_eq!(
            serde_json::to_value(TicketActor::User).unwrap(),
            json!("user")
        );
        assert_eq!(
            serde_json::to_value(TicketActor::System).unwrap(),
            json!("system")
        );
        assert_eq!(
            serde_json::to_value(TicketActor::Agent).unwrap(),
            json!("agent")
        );
        assert_eq!(
            serde_json::to_value(TicketSource::User).unwrap(),
            json!("user")
        );
        assert_eq!(
            serde_json::to_value(TicketIssue::DeliveryFailed).unwrap(),
            json!("deliveryFailed")
        );
        assert_eq!(
            serde_json::to_value(TicketIssue::TurnFailed).unwrap(),
            json!("turnFailed")
        );
        assert_eq!(
            serde_json::to_value(TicketIssue::NotSubmitted).unwrap(),
            json!("notSubmitted")
        );
        assert_eq!(
            serde_json::to_value(TicketSource::Agent).unwrap(),
            json!("agent")
        );
        assert_eq!(
            serde_json::from_value::<TicketIssue>(json!("notSubmitted")).unwrap(),
            TicketIssue::NotSubmitted
        );
        assert_eq!(
            serde_json::to_value(None::<TicketIssue>).unwrap(),
            json!(null)
        );
        // step 6b
        for (m, wire) in [
            (GitMode::Off, "off"),
            (GitMode::Branch, "branch"),
            (GitMode::Worktree, "worktree"),
        ] {
            assert_eq!(serde_json::to_value(m).unwrap(), json!(wire));
            assert_eq!(serde_json::from_value::<GitMode>(json!(wire)).unwrap(), m);
            assert_eq!(m.as_str(), wire);
            assert_eq!(GitMode::parse(&wire.to_uppercase()), Some(m));
        }
        assert_eq!(GitMode::default(), GitMode::Off);
        assert_eq!(GitMode::parse("foo"), None);
        for (c, wire) in [
            (ChecksState::Pending, "pending"),
            (ChecksState::Passed, "passed"),
            (ChecksState::Failed, "failed"),
            (ChecksState::Skipped, "skipped"),
        ] {
            assert_eq!(serde_json::to_value(c).unwrap(), json!(wire));
        }
        assert_eq!(
            serde_json::to_value(ReportAuthor::system()).unwrap(),
            json!({"kind":"system","agentId":null})
        );
    }

    #[test]
    fn ticket_and_summary_are_camel_case() {
        let mut t = ticket(
            "0A1B2C3D-4e5f-6789-abcd-ef0123456789",
            TicketState::Assigned,
        );
        t.assignee_agent_id = Some("agent-1".into());
        t.queue_position = Some(2);
        t.issue = Some(TicketIssue::DeliveryFailed);
        let v = serde_json::to_value(&t).unwrap();
        for key in [
            "id",
            "title",
            "body",
            "state",
            "assigneeAgentId",
            "queuePosition",
            "skipReview",
            "source",
            "issue",
            "rejectionNote",
            "summary",
            "createdAt",
            "updatedAt",
            "history",
            "parentId",
            "blockedBy",
            "kind",
            "playbookStartedAt",
            "checks",
            "git",
            "external",
        ] {
            assert!(v.get(key).is_some(), "missing {key}");
        }
        assert_eq!(v["state"], json!("assigned"));
        assert_eq!(v["issue"], json!("deliveryFailed"));
        assert_eq!(
            v["history"][0],
            json!({"at":1000,"from":null,"to":"backlog","by":"user","note":null})
        );

        let s = serde_json::to_value(TicketSummary::from(&t)).unwrap();
        assert_eq!(s["shortId"], json!("0a1b2c3d"));
        assert_eq!(s["historyLen"], json!(1));
        assert!(s.get("history").is_none());
        assert!(s.get("body").is_none(), "the body is only in get_ticket");
        assert_eq!(s["queuePosition"], json!(2));
        assert_eq!(s["title"], json!(t.title));
        assert_eq!(s["summary"], json!(null));
        assert_eq!(v["summary"], json!(null));

        t.summary = Some("Rettet og testet".into());
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(v["summary"], json!("Rettet og testet"));
        let s = serde_json::to_value(TicketSummary::from(&t)).unwrap();
        assert_eq!(s["summary"], json!("Rettet og testet"));
        assert_eq!(serde_json::from_value::<Ticket>(v).unwrap(), t);

        // step 4b: project in both, round-trip of both wire forms.
        assert_eq!(s["project"], json!(null));
        for (p, wire) in [
            (ProjectRef::Existing("mira".into()), json!("mira")),
            (ProjectRef::New { new: "x".into() }, json!({"new": "x"})),
        ] {
            t.project = Some(p);
            let v = serde_json::to_value(&t).unwrap();
            assert_eq!(v["project"], wire);
            let s = serde_json::to_value(TicketSummary::from(&t)).unwrap();
            assert_eq!(s["project"], wire);
            assert_eq!(serde_json::from_value::<Ticket>(v).unwrap(), t);
            assert_eq!(
                serde_json::from_value::<TicketSummary>(s).unwrap().project,
                t.project
            );
        }

        // step 6a: relations in both.
        let v = serde_json::to_value(&t).unwrap();
        let s = serde_json::to_value(TicketSummary::from(&t)).unwrap();
        assert_eq!(v["parentId"], json!(null));
        assert_eq!(v["blockedBy"], json!([]));
        assert_eq!(s["parentId"], json!(null));
        assert_eq!(s["blockedBy"], json!([]));
        t.parent_id = Some("p1".into());
        t.blocked_by = vec!["b1".into(), "b2".into()];
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(v["parentId"], json!("p1"));
        assert_eq!(v["blockedBy"], json!(["b1", "b2"]));
        let s = serde_json::to_value(TicketSummary::from(&t)).unwrap();
        assert_eq!(s["parentId"], json!("p1"));
        assert_eq!(s["blockedBy"], json!(["b1", "b2"]));
        assert_eq!(serde_json::from_value::<Ticket>(v).unwrap(), t);

        // step 6b: kind, playbook marker, checks and git in both (C6b.2).
        let v = serde_json::to_value(&t).unwrap();
        let s = serde_json::to_value(TicketSummary::from(&t)).unwrap();
        for k in ["kind", "playbookStartedAt", "checks", "git"] {
            assert_eq!(v[k], json!(null), "{k}");
            assert_eq!(s[k], json!(null), "{k}");
        }
        t.kind = Some("feature".into());
        t.playbook_started_at = Some(1_700_000_000_000);
        t.checks = Some(TicketChecks {
            state: ChecksState::Pending,
            failed: None,
            round: 0,
            started_at: 5,
        });
        t.git = Some(TicketGit {
            mode: GitMode::Worktree,
            branch: "ticket/ab12cd34".into(),
            base: "main".into(),
            repo: "/p".into(),
            worktree: Some("/p/.mira-bots/wt/ab12cd34".into()),
        });
        let v = serde_json::to_value(&t).unwrap();
        let s = serde_json::to_value(TicketSummary::from(&t)).unwrap();
        for x in [&v, &s] {
            assert_eq!(x["kind"], json!("feature"));
            assert_eq!(x["playbookStartedAt"], json!(1_700_000_000_000u64));
            assert_eq!(
                x["checks"],
                json!({"state":"pending","failed":null,"round":0,"startedAt":5})
            );
            assert_eq!(
                x["git"],
                json!({"mode":"worktree","branch":"ticket/ab12cd34","base":"main","repo":"/p",
                       "worktree":"/p/.mira-bots/wt/ab12cd34"})
            );
        }
        assert_eq!(serde_json::from_value::<Ticket>(v).unwrap(), t);
        let back = serde_json::from_value::<TicketSummary>(s).unwrap();
        assert_eq!((back.kind, back.git), (t.kind.clone(), t.git.clone()));

        // step 6c: external in both (C6c.2).
        let v = serde_json::to_value(&t).unwrap();
        let s = serde_json::to_value(TicketSummary::from(&t)).unwrap();
        assert_eq!(v["external"], json!(null));
        assert_eq!(s["external"], json!(null));
        t.external = Some(test_support::github_ref(123));
        let v = serde_json::to_value(&t).unwrap();
        let s = serde_json::to_value(TicketSummary::from(&t)).unwrap();
        let want = json!({"kind":"github","externalId":"github:o/r#123","repo":"o/r","number":123,
            "path":null,"url":"https://github.com/o/r/issues/123","title":"Crash ved start",
            "labels":["bug"],"author":"alice","notes":[],"inboxItemId":"item-1","importedAt":5,
            "writeBack":{"comment":"none","close":"none","commentUrl":null,"commentedAt":null,
                "closedAt":null,"attempts":0,"lastError":null,"lastBody":null}});
        assert_eq!(v["external"], want);
        assert_eq!(s["external"], want);
        assert_eq!(serde_json::from_value::<Ticket>(v).unwrap(), t);
        assert_eq!(
            serde_json::from_value::<TicketSummary>(s).unwrap().external,
            t.external
        );
    }

    #[test]
    fn external_ref_round_trips_and_defaults() {
        for (st, wire) in [
            (WriteBackState::None, "none"),
            (WriteBackState::Inflight, "inflight"),
            (WriteBackState::Done, "done"),
            (WriteBackState::Failed, "failed"),
        ] {
            assert_eq!(serde_json::to_value(st).unwrap(), json!(wire));
        }
        assert_eq!(
            serde_json::to_value(ExternalKind::Folder).unwrap(),
            json!("folder")
        );
        assert_eq!(ExternalKind::Github.as_str(), "github");
        let mut e = test_support::github_ref(7);
        e.write_back = WriteBack {
            comment: WriteBackState::Done,
            close: WriteBackState::Failed,
            comment_url: Some("https://github.com/o/r/issues/7#issuecomment-1".into()),
            commented_at: Some(9),
            closed_at: None,
            attempts: 2,
            last_error: Some("x".into()),
            last_body: Some("y".into()),
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(serde_json::from_value::<ExternalRef>(v).unwrap(), e);
        // Only the required fields: the rest defaults (a folder ref without GitHub data).
        let min = json!({"kind":"folder","externalId":"folder:web:fejl-1.md","title":"T",
            "inboxItemId":"i","importedAt":1});
        let e: ExternalRef = serde_json::from_value(min).unwrap();
        assert_eq!(e.kind, ExternalKind::Folder);
        assert_eq!((e.repo, e.number, e.path, e.url), (None, None, None, None));
        assert!(e.labels.is_empty() && e.notes.is_empty() && e.author.is_none());
        assert_eq!(e.write_back, WriteBack::default());
        // A partial write-back object reads with defaults.
        let wb: WriteBack = serde_json::from_value(json!({"comment":"inflight"})).unwrap();
        assert_eq!(wb.comment, WriteBackState::Inflight);
        assert_eq!((wb.close, wb.attempts), (WriteBackState::None, 0));
    }

    #[test]
    fn old_ticket_without_external_loads() {
        let mut v = serde_json::to_value(ticket("t1", TicketState::Done)).unwrap();
        assert!(v.as_object_mut().unwrap().remove("external").is_some());
        let t: Ticket = serde_json::from_value(v).unwrap();
        assert_eq!(t.external, None);
        assert_eq!(t, ticket("t1", TicketState::Done));
    }

    #[test]
    fn file_without_6b_fields_loads_with_defaults() {
        let mut v = serde_json::to_value(ticket("t1", TicketState::Review)).unwrap();
        let o = v.as_object_mut().unwrap();
        for k in ["kind", "playbookStartedAt", "checks", "git"] {
            assert!(o.remove(k).is_some(), "{k}");
        }
        let t: Ticket = serde_json::from_value(v).unwrap();
        assert_eq!((t.kind.as_deref(), t.playbook_started_at), (None, None));
        assert_eq!((t.checks.as_ref(), t.git.as_ref()), (None, None));
        assert_eq!(t, ticket("t1", TicketState::Review));
    }

    #[test]
    fn file_without_relations_loads_with_defaults() {
        let mut v = serde_json::to_value(ticket("t1", TicketState::Assigned)).unwrap();
        let o = v.as_object_mut().unwrap();
        assert!(o.remove("parentId").is_some());
        assert!(o.remove("blockedBy").is_some());
        let t: Ticket = serde_json::from_value(v).unwrap();
        assert_eq!(t.parent_id, None);
        assert!(t.blocked_by.is_empty());
        assert_eq!(t, ticket("t1", TicketState::Assigned));
    }

    #[test]
    fn step3_ticket_without_summary_still_reads() {
        let mut v = serde_json::to_value(ticket("t1", TicketState::Review)).unwrap();
        v.as_object_mut().unwrap().remove("summary");
        let t: Ticket = serde_json::from_value(v).unwrap();
        assert_eq!(t.summary, None);
        assert_eq!(t.source, TicketSource::User);
    }

    #[test]
    fn doc_and_patch_wire_format() {
        let doc = TicketDoc::default();
        assert_eq!(
            serde_json::to_value(&doc).unwrap(),
            json!({"schemaVersion":1,"tickets":[],"reviewAssignments":[]})
        );
        let p: TicketPatch = serde_json::from_value(json!({"skipReview": true})).unwrap();
        assert_eq!(
            p,
            TicketPatch {
                title: None,
                body: None,
                skip_review: Some(true),
                project: None,
            }
        );
        let empty: TicketPatch = serde_json::from_value(json!({})).unwrap();
        assert_eq!(empty, TicketPatch::default());
        assert_eq!(empty.project, None);
        let p: TicketPatch = serde_json::from_value(json!({"project": null})).unwrap();
        assert_eq!(p.project, Some(None));
        let p: TicketPatch = serde_json::from_value(json!({"project": "a"})).unwrap();
        assert_eq!(p.project, Some(Some(ProjectRef::Existing("a".into()))));
        let p: TicketPatch = serde_json::from_value(json!({"project": {"new": "b"}})).unwrap();
        assert_eq!(p.project, Some(Some(ProjectRef::New { new: "b".into() })));
        assert!(serde_json::from_value::<TicketPatch>(json!({"project": 3})).is_err());
    }

    #[test]
    fn workspace_rules_defaults_wire_format() {
        assert_eq!(
            serde_json::to_string(&WorkspaceRules::defaults()).unwrap(),
            r#"{"maxWorkAgents":5,"maxStaffAgents":3,"maxReviewRounds":3,"autoReviewOnStop":false,"createTicketRateLimit":20,"ticketBodyMaxChars":20000,"reportBodyMaxChars":20000,"reportsPerTicketMax":20,"reviewByDefault":true,"userInputGraceMs":5000,"agentsMayCreateProjects":false,"maxAgentsPerProject":0,"git":"off","checksGate":true,"autoSpawnForPlaybook":false,"freshSessionPerTicket":true,"cleanupWorktreesOnDone":false}"#
        );
    }

    #[test]
    fn short_id_is_eight_lowercase_chars_without_dashes() {
        assert_eq!(short_id("ABCD-EF01-2345-6789"), "abcdef01");
        let id = uuid::Uuid::new_v4().to_string();
        let s = short_id(&id);
        assert_eq!(s.chars().count(), TICKET_SHORT_ID_LEN);
        assert!(s
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_eq!(s, id.replace('-', "")[..8].to_lowercase());
    }

    #[test]
    fn errors_are_danish() {
        assert_eq!(String::from(TicketError::NotFound), "Ticketen findes ikke");
        assert_eq!(
            TicketError::IllegalTransition {
                from: TicketState::Done,
                to: TicketState::InProgress
            }
            .to_string(),
            "Kan ikke flytte en ticket fra Done til I gang"
        );
        assert_eq!(
            TicketError::IllegalTransition {
                from: TicketState::Waiting,
                to: TicketState::Review
            }
            .to_string(),
            "Kan ikke flytte en ticket fra Venter til Review"
        );
        assert_eq!(
            TicketError::Validation("Titel må ikke være tom".into()).to_string(),
            "Titel må ikke være tom"
        );
        assert_eq!(
            TicketError::DoneNeedsReview.to_string(),
            "Done kræver review (eller skipReview på ticketen)"
        );
        assert_eq!(
            TicketError::ReadOnly.to_string(),
            "Tickets-filen kunne ikke læses ved opstart; ændringer er slået fra. Genstart appen."
        );
        assert_eq!(
            TicketError::NotYours.to_string(),
            "Ticketen er tildelt en anden agent"
        );
        assert_eq!(
            TicketError::NoTicketInProgress.to_string(),
            "Du har ingen ticket i gang"
        );
        assert_eq!(
            TicketError::NotInProgress.to_string(),
            "Ticketen er ikke i gang"
        );
        assert_eq!(
            TicketError::RateLimited.to_string(),
            "For mange tickets oprettet den seneste time (maks 20)"
        );
        assert_eq!(
            TicketError::ProjectRequired.to_string(),
            "Ticketen mangler et projekt — vælg et, før den tildeles"
        );
        assert_eq!(
            TicketError::WrongProject {
                agent: "coder-01".into(),
                agent_project: "b".into(),
                ticket_project: "a".into()
            }
            .to_string(),
            "Agenten coder-01 står i projekt «b»; ticketen hører til «a»"
        );
        assert_eq!(
            TicketError::ProjectChangeNotAllowed.to_string(),
            "Projektet kan kun ændres, mens ticketen ligger i Backlog eller er afvist uden agent"
        );
        // step 6a
        let table = [
            (TicketError::ParentNotFound, "Forælderen findes ikke"),
            (
                TicketError::ParentDone,
                "Forælderen er allerede færdig (Done)",
            ),
            (
                TicketError::ParentProjectMismatch {
                    parent: "a".into(),
                    child: "b".into(),
                },
                "Del-ticketen hører til «b», men forælderen til «a»",
            ),
            (
                TicketError::BlockerNotFound("abc".into()),
                "Blokeringen abc findes ikke",
            ),
            (
                TicketError::BlockedByAncestor,
                "En ticket kan ikke blokeres af sin egen forælder",
            ),
            (TicketError::Cycle, "Relationen ville danne en cyklus"),
            (
                TicketError::Blocked("a1b2c3d4, e5f6a7b8".into()),
                "Ticketen venter på a1b2c3d4, e5f6a7b8",
            ),
            (
                TicketError::TooManyBlockers,
                "Højst 10 blokeringer pr. ticket",
            ),
            // step 6b
            (
                TicketError::NoPlaybook("docs".into()),
                "Ingen playbook for «docs»",
            ),
            (
                TicketError::PlaybookAlreadyStarted,
                "Forløbet er allerede startet (ticketen har del-tickets)",
            ),
            (
                TicketError::InvalidKind,
                "kind skal være task, feature, bug eller et playbook-navn fra workspace-filen",
            ),
            (
                TicketError::PlaybookStepsInvalid("trin 1 mangler titel".into()),
                "Playbooken kan ikke udrulles: trin 1 mangler titel",
            ),
            (
                TicketError::ExternalAlreadyStarted("ab12cd34".into()),
                "Issue/filen er allerede startet som ticket ab12cd34",
            ),
        ];
        for (e, text) in table {
            assert_eq!(e.to_string(), text);
        }
    }

    #[test]
    fn step3_file_without_new_fields_loads() {
        let mut v = serde_json::to_value(ticket("t1", TicketState::Review)).unwrap();
        let o = v.as_object_mut().unwrap();
        for k in [
            "reviewRound",
            "escalated",
            "reviewerAgentId",
            "reports",
            "project",
        ] {
            assert!(o.remove(k).is_some(), "{k}");
        }
        let t: Ticket = serde_json::from_value(v).unwrap();
        assert_eq!(
            (
                t.review_round,
                t.escalated,
                t.reviewer_agent_id,
                t.reports.len(),
                t.project
            ),
            (0, false, None, 0, None)
        );
        let doc: TicketDoc =
            serde_json::from_value(json!({"schemaVersion":1,"tickets":[]})).unwrap();
        assert!(doc.review_assignments.is_empty());
    }

    #[test]
    fn summary_carries_review_fields_and_report_count() {
        let mut t = ticket("t1", TicketState::Review);
        t.review_round = 2;
        t.escalated = true;
        t.reviewer_agent_id = Some("rev".into());
        t.reports.push(TicketReport {
            id: "01".into(),
            title: "R".into(),
            author: ReportAuthor::user(),
            created_at: 5,
            path: "reports/01-r.md".into(),
            size: 3,
        });
        let s = serde_json::to_value(TicketSummary::from(&t)).unwrap();
        assert_eq!(s["reviewRound"], 2);
        assert_eq!(s["escalated"], true);
        assert_eq!(s["reviewerAgentId"], "rev");
        assert_eq!(s["reportCount"], 1);
        assert!(s.get("reports").is_none());
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(v["reports"][0]["path"], "reports/01-r.md");
    }

    #[test]
    fn review_assignment_and_report_are_camel_case() {
        let a = ReviewAssignment {
            ticket_id: "t".into(),
            reviewer_agent_id: "r".into(),
            round: 1,
            assigned_at: 2,
            delivered_at: None,
            attempts: 0,
        };
        assert_eq!(
            serde_json::to_value(&a).unwrap(),
            json!({"ticketId":"t","reviewerAgentId":"r","round":1,"assignedAt":2,"deliveredAt":null,"attempts":0})
        );
        let old: ReviewAssignment = serde_json::from_value(
            json!({"ticketId":"t","reviewerAgentId":"r","round":1,"assignedAt":2,"deliveredAt":7}),
        )
        .unwrap();
        assert_eq!((old.attempts, old.delivered_at), (0, Some(7)));
        let r = TicketReport {
            id: "02".into(),
            title: "T".into(),
            author: ReportAuthor::agent("a1"),
            created_at: 9,
            path: "reports/02-t.md".into(),
            size: 10,
        };
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({"id":"02","title":"T","author":{"kind":"agent","agentId":"a1"},"createdAt":9,"path":"reports/02-t.md","size":10})
        );
        assert_eq!(
            serde_json::to_value(ReportAuthor::user()).unwrap(),
            json!({"kind":"user","agentId":null})
        );
    }

    #[test]
    fn step5_errors_are_danish() {
        let table = [
            (TicketError::NotInReview, "Ticketen er ikke i review"),
            (
                TicketError::NotYourReview,
                "Du er ikke reviewer på denne ticket",
            ),
            (
                TicketError::OwnSubmission,
                "Du kan ikke reviewe din egen aflevering",
            ),
            (
                TicketError::TooManyReports,
                "Ticketen har allerede 20 rapporter",
            ),
            (TicketError::ReportNotFound, "Rapporten findes ikke"),
            (TicketError::NotAReviewer, "Agenten er ikke reviewer"),
            (
                TicketError::HandoffToSelf,
                "Ticketen kan ikke gives videre til den agent, der allerede har den",
            ),
            (TicketError::AgentWorking, "Agenten arbejder"),
            (
                TicketError::SenderCannotReview,
                "Afsenderen kan ikke reviewe sin egen ticket",
            ),
        ];
        for (e, text) in table {
            assert_eq!(e.to_string(), text);
        }
    }
}
