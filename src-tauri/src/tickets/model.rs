//! Ticket data model (plan C3.1). Wire format: camelCase; enum values camelCase strings.

use serde::{Deserialize, Serialize};

use crate::config::{
    AUTO_REVIEW_ON_STOP, CREATE_TICKET_RATE_LIMIT, MAX_REVIEW_ROUNDS, MAX_STAFF_AGENTS,
    MAX_WORK_AGENTS, REPORTS_PER_TICKET_MAX, REPORT_BODY_MAX_CHARS, TICKETS_SCHEMA_VERSION,
    TICKET_BODY_MAX_CHARS, TICKET_SHORT_ID_LEN,
};

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
}

impl TicketState {
    pub const ALL: [TicketState; 6] = [
        TicketState::Backlog,
        TicketState::Assigned,
        TicketState::InProgress,
        TicketState::Review,
        TicketState::Done,
        TicketState::Rejected,
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
    /// Reached [`MAX_REVIEW_ROUNDS`] on entering review: no automatic routing, the user decides.
    #[serde(default)]
    pub escalated: bool,
    /// The reviewer agent while in review (kept after approval for display).
    #[serde(default)]
    pub reviewer_agent_id: Option<String>,
    /// Report metadata; the texts are files under `<app_data>/tickets/<id>/reports/`.
    #[serde(default)]
    pub reports: Vec<TicketReport>,
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
}

/// Who wrote a report: an agent (`agentId`) or the user.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ReportAuthorKind {
    Agent,
    User,
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
}

/// The rules of this workspace (plan5 C5.1): the "Regler" section of every profile's system
/// prompt and, from batch 2, the result of `mira_get_workspace_rules`. Compile-time values.
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
}

impl WorkspaceRules {
    /// The values from `config.rs`.
    pub fn current() -> Self {
        Self {
            max_work_agents: MAX_WORK_AGENTS,
            max_staff_agents: MAX_STAFF_AGENTS,
            max_review_rounds: MAX_REVIEW_ROUNDS,
            auto_review_on_stop: AUTO_REVIEW_ON_STOP,
            create_ticket_rate_limit: CREATE_TICKET_RATE_LIMIT,
            ticket_body_max_chars: TICKET_BODY_MAX_CHARS,
            report_body_max_chars: REPORT_BODY_MAX_CHARS,
            reports_per_ticket_max: REPORTS_PER_TICKET_MAX,
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
                json!("rejected")
            ]
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
                skip_review: Some(true)
            }
        );
        let empty: TicketPatch = serde_json::from_value(json!({})).unwrap();
        assert_eq!(empty, TicketPatch::default());
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
    }

    #[test]
    fn step3_file_without_new_fields_loads() {
        let mut v = serde_json::to_value(ticket("t1", TicketState::Review)).unwrap();
        let o = v.as_object_mut().unwrap();
        for k in ["reviewRound", "escalated", "reviewerAgentId", "reports"] {
            assert!(o.remove(k).is_some(), "{k}");
        }
        let t: Ticket = serde_json::from_value(v).unwrap();
        assert_eq!(
            (
                t.review_round,
                t.escalated,
                t.reviewer_agent_id,
                t.reports.len()
            ),
            (0, false, None, 0)
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
