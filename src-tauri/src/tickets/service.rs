//! [`TicketService`]: owns the ticket document in memory, applies every mutation through the state
//! machine, keeps the queue invariants and saves synchronously after each mutation.
//!
//! Invariants (checked by `normalize_queues` / the mutations):
//! - a ticket is in at most one queue (`assignee_agent_id` while `assigned`);
//! - per agent the `assigned` tickets have positions `0..n` without gaps; nothing else has one;
//! - at most one `inProgress` ticket per agent.
//!
//! The service is meant to sit behind a `std::sync::Mutex`; it never blocks on anything but the
//! store's small synchronous write.

use std::collections::{HashMap, HashSet};
use std::io;

use super::model::{
    short_id, ReviewAssignment, Ticket, TicketActor, TicketDoc, TicketError, TicketHistoryEntry,
    TicketId, TicketIssue, TicketPatch, TicketReport, TicketSource, TicketState, TicketSummary,
};
use super::state::{transition_noted, TicketEvent, REOPENED_NOTE};
use super::store::TicketStore;
use super::NOT_SUBMITTED_NOTE;
use crate::config::{
    MAX_REVIEW_ROUNDS, REPORTS_PER_TICKET_MAX, RESTART_NOTE, REVIEW_DELIVERY_MAX_ATTEMPTS,
    TICKET_BODY_MAX_CHARS, TICKET_SUMMARY_MAX_CHARS, TICKET_TITLE_MAX_CHARS,
};

/// History note when a turn ended normally (Stop hook).
pub const TURN_ENDED_NOTE: &str = "auto: turn afsluttet";
/// History note when a ticket in progress was delivered again.
pub const RESENT_NOTE: &str = "sendt igen";
/// History note after the review line could not be delivered [`REVIEW_DELIVERY_MAX_ATTEMPTS`]
/// times (plan5 C5.8).
pub const REVIEW_UNDELIVERED_NOTE: &str = "review kunne ikke leveres";
/// History note when the user removed the reviewer.
pub const REVIEWER_REMOVED_NOTE: &str = "reviewer fjernet";

/// `"eskaleret efter 3 runder"`.
pub fn escalated_note() -> String {
    format!("eskaleret efter {MAX_REVIEW_ROUNDS} runder")
}

/// Per reviewer agent: its open review assignments (agents without any are absent).
pub type ReviewCounts = HashMap<String, usize>;

/// Per agent: the `inProgress` ticket (if any) and the number of queued (`assigned`) tickets.
/// Agents without any ticket are absent.
pub type TicketLinks = HashMap<String, (Option<TicketId>, usize)>;

pub struct TicketService {
    store: Box<dyn TicketStore>,
    doc: TicketDoc,
    /// Set when the store could not be read at startup (I/O error, not a missing or corrupt
    /// file). Every mutation then fails with [`TicketError::ReadOnly`] and nothing is saved, so
    /// the unread file is never replaced. Lasts until the app restarts.
    read_only: bool,
}

fn validate_title(title: &str) -> Result<String, TicketError> {
    let t = title.trim();
    if t.is_empty() {
        return Err(TicketError::Validation("Titel må ikke være tom".into()));
    }
    if t.chars().count() > TICKET_TITLE_MAX_CHARS {
        return Err(TicketError::Validation(format!(
            "Titlen er for lang (maks {TICKET_TITLE_MAX_CHARS} tegn)"
        )));
    }
    Ok(t.to_string())
}

fn validate_body(body: &str) -> Result<(), TicketError> {
    if body.chars().count() > TICKET_BODY_MAX_CHARS {
        return Err(TicketError::Validation(format!(
            "Teksten er for lang (maks {TICKET_BODY_MAX_CHARS} tegn)"
        )));
    }
    Ok(())
}

/// Trimmed summary of 1–[`TICKET_SUMMARY_MAX_CHARS`] chars.
fn validate_summary(summary: &str) -> Result<String, TicketError> {
    let s = summary.trim();
    let n = s.chars().count();
    if n == 0 || n > TICKET_SUMMARY_MAX_CHARS {
        return Err(TicketError::Validation(format!(
            "summary skal være en tekst på 1–{TICKET_SUMMARY_MAX_CHARS} tegn"
        )));
    }
    Ok(s.to_string())
}

fn find_mut<'a>(doc: &'a mut TicketDoc, id: &str) -> Result<&'a mut Ticket, TicketError> {
    doc.tickets
        .iter_mut()
        .find(|t| t.id == id)
        .ok_or(TicketError::NotFound)
}

/// Applies `ev` to ticket `id` in place.
fn apply(
    doc: &mut TicketDoc,
    id: &str,
    ev: &TicketEvent,
    by: TicketActor,
    note: Option<String>,
    now: u64,
) -> Result<Ticket, TicketError> {
    let t = find_mut(doc, id)?;
    *t = transition_noted(t, ev, by, note, now)?;
    Ok(t.clone())
}

fn in_progress_of<'a>(doc: &'a TicketDoc, agent_id: &str) -> Option<&'a Ticket> {
    doc.tickets.iter().find(|t| {
        t.state == TicketState::InProgress && t.assignee_agent_id.as_deref() == Some(agent_id)
    })
}

/// Keeps the review assignments consistent with the tickets: an assignment stays only while its
/// ticket is in review with that reviewer (one per ticket); a ticket in review whose reviewer has
/// no assignment loses the reviewer (it is routed again).
fn normalize_reviews(doc: &mut TicketDoc) {
    let mut seen: HashSet<TicketId> = HashSet::new();
    let tickets = &doc.tickets;
    doc.review_assignments.retain(|a| {
        let ok = tickets.iter().any(|t| {
            t.id == a.ticket_id
                && t.state == TicketState::Review
                && t.reviewer_agent_id.as_deref() == Some(a.reviewer_agent_id.as_str())
        });
        ok && seen.insert(a.ticket_id.clone())
    });
    for t in doc.tickets.iter_mut() {
        if t.state == TicketState::Review && t.reviewer_agent_id.is_some() && !seen.contains(&t.id)
        {
            t.reviewer_agent_id = None;
        }
    }
}

/// A history entry that keeps the state (`from == to`).
fn note_entry(t: &mut Ticket, by: TicketActor, note: String, now: u64) {
    t.updated_at = now;
    t.history.push(TicketHistoryEntry {
        at: now,
        from: Some(t.state),
        to: t.state,
        by,
        note: Some(note),
    });
}

/// Rewrites queue positions: per agent the `assigned` tickets ordered by
/// `(queue_position or MAX, updated_at)` get `0..n`; every other ticket gets `None`.
fn normalize_queues(doc: &mut TicketDoc) {
    let mut queues: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, t) in doc.tickets.iter_mut().enumerate() {
        match (&t.state, &t.assignee_agent_id) {
            (TicketState::Assigned, Some(a)) => queues.entry(a.clone()).or_default().push(i),
            _ => t.queue_position = None,
        }
    }
    for idx in queues.values_mut() {
        idx.sort_by_key(|&i| {
            let t = &doc.tickets[i];
            (t.queue_position.unwrap_or(usize::MAX), t.updated_at)
        });
        for (pos, &i) in idx.iter().enumerate() {
            doc.tickets[i].queue_position = Some(pos);
        }
    }
}

impl TicketService {
    pub fn new(store: Box<dyn TicketStore>, doc: TicketDoc) -> Self {
        let mut s = TicketService {
            store,
            doc,
            read_only: false,
        };
        normalize_queues(&mut s.doc);
        s
    }

    /// Loads the store and moves `assigned`/`inProgress` tickets (whose agents no longer exist
    /// after a restart) to the backlog with [`RESTART_NOTE`]. Saves only if something changed.
    /// Returns the load warning (corrupt file etc.) for Diagnostics.
    ///
    /// A missing file is an empty list; a corrupt or unknown-version file has already been moved
    /// aside by the store (warning, empty list). Any other read error (locked file, no
    /// permission, …) starts the service read-only with an empty list: the file may hold tickets
    /// we could not see, so nothing may be saved over it.
    pub fn load_and_recover(store: Box<dyn TicketStore>, now: u64) -> (Self, Option<String>) {
        let (doc, mut warning) = match store.load() {
            Ok(r) => (r.doc, r.warning),
            Err(e) => {
                log::error!("tickets: load failed, starting read-only: {e}");
                let mut svc = TicketService::new(store, TicketDoc::default());
                svc.read_only = true;
                let warning = format!(
                    "tickets.json kunne ikke læses ved opstart ({e}); ændringer er slået fra. \
                     Genstart appen."
                );
                return (svc, Some(warning));
            }
        };
        let mut svc = TicketService::new(store, doc);
        // Reviewers did not survive the restart either: tickets in review wait for routing again.
        let stale_reviews = !svc.doc.review_assignments.is_empty()
            || svc
                .doc
                .tickets
                .iter()
                .any(|t| t.state == TicketState::Review && t.reviewer_agent_id.is_some());
        let stale: Vec<TicketId> = svc
            .doc
            .tickets
            .iter()
            .filter(|t| matches!(t.state, TicketState::Assigned | TicketState::InProgress))
            .map(|t| t.id.clone())
            .collect();
        if !stale.is_empty() || stale_reviews {
            let r = svc.commit(|doc| {
                doc.review_assignments.clear();
                for t in doc.tickets.iter_mut() {
                    if t.state == TicketState::Review {
                        t.reviewer_agent_id = None;
                    }
                }
                for id in &stale {
                    apply(
                        doc,
                        id,
                        &TicketEvent::ToBacklog {
                            note: Some(RESTART_NOTE.into()),
                        },
                        TicketActor::System,
                        None,
                        now,
                    )?;
                }
                Ok(())
            });
            if let Err(e) = r {
                log::error!("tickets: saving the restart recovery failed: {e}");
                warning.get_or_insert_with(|| e.to_string());
            }
        }
        (svc, warning)
    }

    /// Runs `f` on the document, normalises the queues and saves. Any error (from `f` or the
    /// store) restores the document as it was before.
    fn commit<T>(
        &mut self,
        f: impl FnOnce(&mut TicketDoc) -> Result<T, TicketError>,
    ) -> Result<T, TicketError> {
        if self.read_only {
            return Err(TicketError::ReadOnly);
        }
        let before = self.doc.clone();
        let result = f(&mut self.doc).and_then(|v| {
            normalize_queues(&mut self.doc);
            normalize_reviews(&mut self.doc);
            self.persist().map(|()| v)
        });
        if result.is_err() {
            self.doc = before;
        }
        result
    }

    fn persist(&self) -> Result<(), TicketError> {
        self.store.save(&self.doc).map_err(|e: io::Error| {
            log::error!("tickets: save failed: {e}");
            TicketError::Io(e.to_string())
        })
    }

    /// Re-reads a ticket after a commit (positions may have changed during normalisation).
    fn fetch(&self, id: &str) -> Result<Ticket, TicketError> {
        self.get(id).ok_or(TicketError::NotFound)
    }

    // ---- reading ----

    /// All tickets without history, ordered by `created_at` (stable).
    pub fn list(&self) -> Vec<TicketSummary> {
        let mut v: Vec<TicketSummary> = self.doc.tickets.iter().map(TicketSummary::from).collect();
        v.sort_by_key(|t| t.created_at);
        v
    }

    /// Whether mutations are disabled because the file could not be read at startup.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn get(&self, id: &str) -> Option<Ticket> {
        self.doc.tickets.iter().find(|t| t.id == id).cloned()
    }

    pub fn len(&self) -> usize {
        self.doc.tickets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.doc.tickets.is_empty()
    }

    /// The agent's queue (`assigned`), in order.
    pub fn queue(&self, agent_id: &str) -> Vec<TicketSummary> {
        let mut v: Vec<&Ticket> = self
            .doc
            .tickets
            .iter()
            .filter(|t| {
                t.state == TicketState::Assigned && t.assignee_agent_id.as_deref() == Some(agent_id)
            })
            .collect();
        v.sort_by_key(|t| t.queue_position);
        v.into_iter().map(TicketSummary::from).collect()
    }

    /// The agent's `inProgress` ticket.
    pub fn current_for_agent(&self, agent_id: &str) -> Option<Ticket> {
        in_progress_of(&self.doc, agent_id).cloned()
    }

    /// Data for `AgentInfo.currentTicketId` / `queueLength` (agents without tickets are absent;
    /// the caller sets them to `None`/0).
    pub fn links(&self) -> TicketLinks {
        let mut m: TicketLinks = HashMap::new();
        for t in &self.doc.tickets {
            let Some(a) = &t.assignee_agent_id else {
                continue;
            };
            match t.state {
                TicketState::InProgress => m.entry(a.clone()).or_default().0 = Some(t.id.clone()),
                TicketState::Assigned => m.entry(a.clone()).or_default().1 += 1,
                _ => {}
            }
        }
        m
    }

    /// A ticket by its full id or its short id (case-insensitive, surrounding spaces ignored).
    pub fn get_by_any_id(&self, id: &str) -> Option<Ticket> {
        let q = id.trim().to_lowercase();
        if q.is_empty() {
            return None;
        }
        self.doc
            .tickets
            .iter()
            .find(|t| t.id.to_lowercase() == q)
            .or_else(|| self.doc.tickets.iter().find(|t| t.short_id() == q))
            .cloned()
    }

    /// The agent's own tickets ("mine"): the one in progress first, then its queue in order.
    pub fn list_for_agent(&self, agent_id: &str) -> Vec<TicketSummary> {
        let mut v: Vec<&Ticket> = self
            .doc
            .tickets
            .iter()
            .filter(|t| {
                t.assignee_agent_id.as_deref() == Some(agent_id)
                    && matches!(t.state, TicketState::Assigned | TicketState::InProgress)
            })
            .collect();
        v.sort_by_key(|t| (t.state != TicketState::InProgress, t.queue_position));
        v.into_iter().map(TicketSummary::from).collect()
    }

    /// Unassigned tickets, oldest first.
    pub fn backlog(&self) -> Vec<TicketSummary> {
        let mut v: Vec<TicketSummary> = self
            .doc
            .tickets
            .iter()
            .filter(|t| t.state == TicketState::Backlog)
            .map(TicketSummary::from)
            .collect();
        v.sort_by_key(|t| t.created_at);
        v
    }

    /// The ticket to deliver next: `None` while the agent has one in progress, else the queue
    /// head.
    pub fn next_for_agent(&self, agent_id: &str) -> Option<Ticket> {
        if in_progress_of(&self.doc, agent_id).is_some() {
            return None;
        }
        self.doc
            .tickets
            .iter()
            .filter(|t| {
                t.state == TicketState::Assigned && t.assignee_agent_id.as_deref() == Some(agent_id)
            })
            .min_by_key(|t| t.queue_position)
            .cloned()
    }

    // ---- user mutations ----

    pub fn create(
        &mut self,
        title: &str,
        body: &str,
        skip_review: bool,
        now: u64,
    ) -> Result<Ticket, TicketError> {
        self.create_with_id_source(
            &mut || uuid::Uuid::new_v4().to_string(),
            title,
            body,
            skip_review,
            (TicketSource::User, TicketActor::User),
            now,
        )
    }

    /// `create` with injectable ids (tests force a short-id collision) and origin (`source`, and
    /// the creation entry's `by`). Ids whose short id is already taken are skipped.
    pub(crate) fn create_with_id_source(
        &mut self,
        next_id: &mut dyn FnMut() -> String,
        title: &str,
        body: &str,
        skip_review: bool,
        origin: (TicketSource, TicketActor),
        now: u64,
    ) -> Result<Ticket, TicketError> {
        let t = self.new_ticket(next_id, title, body, skip_review, origin, now)?;
        let id = t.id.clone();
        self.commit(|doc| {
            doc.tickets.push(t);
            Ok(())
        })?;
        self.fetch(&id)
    }

    /// A validated backlog ticket with a fresh id (not yet in the document).
    fn new_ticket(
        &self,
        next_id: &mut dyn FnMut() -> String,
        title: &str,
        body: &str,
        skip_review: bool,
        (source, by): (TicketSource, TicketActor),
        now: u64,
    ) -> Result<Ticket, TicketError> {
        let title = validate_title(title)?;
        validate_body(body)?;
        let taken: HashSet<String> = self.doc.tickets.iter().map(Ticket::short_id).collect();
        let id = loop {
            let id = next_id();
            if !taken.contains(&short_id(&id)) {
                break id;
            }
        };
        let t = Ticket {
            id,
            title,
            body: body.to_string(),
            state: TicketState::Backlog,
            assignee_agent_id: None,
            queue_position: None,
            skip_review,
            source,
            issue: None,
            rejection_note: None,
            summary: None,
            created_at: now,
            updated_at: now,
            history: vec![TicketHistoryEntry {
                at: now,
                from: None,
                to: TicketState::Backlog,
                by,
                note: None,
            }],
            review_round: 0,
            escalated: false,
            reviewer_agent_id: None,
            reports: Vec::new(),
            project: None,
        };
        Ok(t)
    }

    /// Title/body/skipReview in any state (same validation as `create`).
    pub fn update(
        &mut self,
        id: &str,
        patch: TicketPatch,
        now: u64,
    ) -> Result<Ticket, TicketError> {
        let title = patch.title.as_deref().map(validate_title).transpose()?;
        if let Some(b) = &patch.body {
            validate_body(b)?;
        }
        self.commit(|doc| {
            let t = find_mut(doc, id)?;
            if let Some(title) = title {
                t.title = title;
            }
            if let Some(b) = patch.body {
                t.body = b;
            }
            if let Some(s) = patch.skip_review {
                t.skip_review = s;
            }
            t.updated_at = now;
            Ok(())
        })?;
        self.fetch(id)
    }

    /// Only backlog and done tickets, and rejected ones without an agent, can be deleted.
    pub fn delete(&mut self, id: &str) -> Result<(), TicketError> {
        let t = self.get(id).ok_or(TicketError::NotFound)?;
        let deletable = match t.state {
            TicketState::Backlog | TicketState::Done => true,
            TicketState::Rejected => t.assignee_agent_id.is_none(),
            _ => false,
        };
        if !deletable {
            return Err(TicketError::NotDeletable);
        }
        self.commit(|doc| {
            doc.tickets.retain(|t| t.id != id);
            Ok(())
        })
    }

    /// backlog/rejected → assigned, at the end of the agent's queue. The caller checks that the
    /// agent is alive.
    pub fn assign(&mut self, id: &str, agent_id: &str, now: u64) -> Result<Ticket, TicketError> {
        self.commit(|doc| {
            let state = find_mut(doc, id)?.state;
            if state == TicketState::Rejected {
                apply(
                    doc,
                    id,
                    &TicketEvent::ToBacklog { note: None },
                    TicketActor::User,
                    None,
                    now,
                )?;
            }
            let ev = TicketEvent::Assign {
                agent_id: agent_id.to_string(),
            };
            apply(doc, id, &ev, TicketActor::User, None, now)?;
            Ok(())
        })?;
        self.fetch(id)
    }

    /// assigned → backlog.
    pub fn unassign(&mut self, id: &str, now: u64) -> Result<Ticket, TicketError> {
        self.commit(|doc| {
            apply(
                doc,
                id,
                &TicketEvent::Unassign,
                TicketActor::User,
                None,
                now,
            )
        })?;
        self.fetch(id)
    }

    /// Sets the agent's queue order. `ids` must be exactly the agent's queued tickets.
    pub fn reorder(
        &mut self,
        agent_id: &str,
        ids: &[String],
    ) -> Result<Vec<TicketSummary>, TicketError> {
        let current: HashSet<TicketId> = self.queue(agent_id).into_iter().map(|t| t.id).collect();
        let wanted: HashSet<TicketId> = ids.iter().cloned().collect();
        if wanted.len() != ids.len() || wanted != current {
            return Err(TicketError::Validation("Køen passer ikke".into()));
        }
        self.commit(|doc| {
            for (pos, id) in ids.iter().enumerate() {
                find_mut(doc, id)?.queue_position = Some(pos);
            }
            Ok(())
        })?;
        Ok(self.queue(agent_id))
    }

    /// Manual move by the user (plan C3.3). `agent_live`: the ticket's assignee exists and has not
    /// exited.
    pub fn set_state(
        &mut self,
        id: &str,
        target: TicketState,
        note: Option<String>,
        agent_live: bool,
        now: u64,
    ) -> Result<Ticket, TicketError> {
        use TicketState as S;
        let t = self.get(id).ok_or(TicketError::NotFound)?;
        let illegal = Err(TicketError::IllegalTransition {
            from: t.state,
            to: target,
        });
        let note = note.filter(|n| !n.trim().is_empty());
        let ev = match (target, t.state) {
            (S::Rejected, S::Review) => {
                return self.reject(id, note.as_deref().unwrap_or_default(), agent_live, now)
            }
            (S::Rejected, from) if from != S::Rejected => return Err(TicketError::UseReject),
            (S::Assigned, from) if from != S::Assigned => return Err(TicketError::UseAssign),
            (S::Backlog, S::Assigned) => TicketEvent::Unassign,
            (S::Backlog, S::Done) => TicketEvent::ToBacklog {
                note: Some(note.unwrap_or_else(|| REOPENED_NOTE.into())),
            },
            (S::Backlog, S::InProgress | S::Review | S::Rejected) => {
                TicketEvent::ToBacklog { note }
            }
            (S::InProgress, S::Assigned | S::Review) => {
                let agent = t.assignee_agent_id.as_deref().unwrap_or_default();
                if !agent_live {
                    return Err(TicketError::AgentNotLive);
                }
                if in_progress_of(&self.doc, agent).is_some() {
                    return Err(TicketError::AgentBusy);
                }
                if t.state == S::Assigned {
                    TicketEvent::Dispatched
                } else {
                    TicketEvent::Reopen
                }
            }
            (S::Review, S::InProgress) => TicketEvent::Submit,
            (S::Done, S::InProgress) if t.skip_review => TicketEvent::Submit,
            (S::Done, S::InProgress) => return Err(TicketError::DoneNeedsReview),
            (S::Done, S::Review) => TicketEvent::Approve,
            _ => return illegal,
        };
        self.commit(|doc| apply(doc, id, &ev, TicketActor::User, None, now))?;
        self.fetch(id)
    }

    /// review → done.
    pub fn approve(&mut self, id: &str, now: u64) -> Result<Ticket, TicketError> {
        self.commit(|doc| apply(doc, id, &TicketEvent::Approve, TicketActor::User, None, now))?;
        self.fetch(id)
    }

    /// review → rejected → first in the same agent's queue (agent alive) or backlog. One save.
    pub fn reject(
        &mut self,
        id: &str,
        note: &str,
        agent_live: bool,
        now: u64,
    ) -> Result<Ticket, TicketError> {
        self.reject_as(id, note, agent_live, TicketActor::User, None, now)
    }

    /// [`Self::reject`] by `by`, with `prefix` before the note in the history.
    fn reject_as(
        &mut self,
        id: &str,
        note: &str,
        agent_live: bool,
        by: TicketActor,
        prefix: Option<String>,
        now: u64,
    ) -> Result<Ticket, TicketError> {
        let ev = TicketEvent::Reject {
            note: note.to_string(),
        };
        self.commit(|doc| {
            let t = apply(doc, id, &ev, by, prefix, now)?;
            match (agent_live, t.assignee_agent_id) {
                (true, Some(agent)) => {
                    // Make room at the front of the queue.
                    for q in doc.tickets.iter_mut().filter(|q| {
                        q.state == TicketState::Assigned
                            && q.assignee_agent_id.as_deref() == Some(agent.as_str())
                    }) {
                        q.queue_position = q.queue_position.map(|p| p + 1);
                    }
                    apply(
                        doc,
                        id,
                        &TicketEvent::Requeue,
                        TicketActor::System,
                        None,
                        now,
                    )?;
                    find_mut(doc, id)?.queue_position = Some(0);
                }
                _ => {
                    apply(
                        doc,
                        id,
                        &TicketEvent::ToBacklog { note: None },
                        TicketActor::System,
                        None,
                        now,
                    )?;
                }
            }
            Ok(())
        })?;
        self.fetch(id)
    }

    // ---- dispatcher API ----

    /// Delivery confirmed: assigned → inProgress (history "sendt til <agent>"), or, for a ticket
    /// already in progress (redispatch), a "sendt igen" entry. Clears `issue`.
    pub fn mark_dispatched(
        &mut self,
        id: &str,
        agent_name: &str,
        now: u64,
    ) -> Result<Ticket, TicketError> {
        let t = self.get(id).ok_or(TicketError::NotFound)?;
        self.commit(|doc| match t.state {
            TicketState::InProgress => {
                let t = find_mut(doc, id)?;
                t.issue = None;
                t.updated_at = now;
                t.history.push(TicketHistoryEntry {
                    at: now,
                    from: Some(TicketState::InProgress),
                    to: TicketState::InProgress,
                    by: TicketActor::System,
                    note: Some(RESENT_NOTE.into()),
                });
                Ok(())
            }
            _ => {
                let agent = t.assignee_agent_id.as_deref().unwrap_or_default();
                if in_progress_of(doc, agent).is_some() {
                    return Err(TicketError::AgentBusy);
                }
                let note = Some(format!("sendt til {agent_name}"));
                apply(
                    doc,
                    id,
                    &TicketEvent::Dispatched,
                    TicketActor::System,
                    note,
                    now,
                )
                .map(|_| ())
            }
        })?;
        self.fetch(id)
    }

    /// The agent's turn ended normally: its inProgress ticket → review (done with skipReview).
    /// `Ok(None)` when the agent had no ticket in progress (nothing saved).
    pub fn complete_turn(
        &mut self,
        agent_id: &str,
        now: u64,
    ) -> Result<Option<Ticket>, TicketError> {
        let Some(t) = self.current_for_agent(agent_id) else {
            return Ok(None);
        };
        let note = Some(TURN_ENDED_NOTE.to_string());
        self.commit(|doc| {
            apply(
                doc,
                &t.id,
                &TicketEvent::Submit,
                TicketActor::System,
                note,
                now,
            )
        })?;
        self.fetch(&t.id).map(Some)
    }

    /// Sets or clears `issue` (state unchanged). A note adds a history entry.
    pub fn set_issue(
        &mut self,
        id: &str,
        issue: Option<TicketIssue>,
        note: Option<String>,
        now: u64,
    ) -> Result<Ticket, TicketError> {
        self.commit(|doc| {
            let t = find_mut(doc, id)?;
            t.issue = issue;
            t.updated_at = now;
            if note.is_some() {
                t.history.push(TicketHistoryEntry {
                    at: now,
                    from: Some(t.state),
                    to: t.state,
                    by: TicketActor::System,
                    note,
                });
            }
            Ok(())
        })?;
        self.fetch(id)
    }

    /// The turn ended without `mira_submit_for_review`: the agent's inProgress ticket keeps its
    /// state and gets `issue: notSubmitted` with [`NOT_SUBMITTED_NOTE`] (by the system).
    /// `Ok(None)` when the agent has no ticket in progress (nothing saved).
    pub fn mark_not_submitted(
        &mut self,
        agent_id: &str,
        now: u64,
    ) -> Result<Option<Ticket>, TicketError> {
        let Some(t) = self.current_for_agent(agent_id) else {
            return Ok(None);
        };
        self.set_issue(
            &t.id,
            Some(TicketIssue::NotSubmitted),
            Some(NOT_SUBMITTED_NOTE.to_string()),
            now,
        )
        .map(Some)
    }

    /// Any non-backlog state → backlog by the system (delivery failure etc.).
    pub fn to_backlog(&mut self, id: &str, note: &str, now: u64) -> Result<Ticket, TicketError> {
        let ev = TicketEvent::ToBacklog {
            note: Some(note.to_string()),
        };
        self.commit(|doc| apply(doc, id, &ev, TicketActor::System, None, now))?;
        self.fetch(id)
    }

    /// The agent stopped/exited/was removed: all its assigned/inProgress/rejected tickets go to
    /// the backlog with `note`. One save (none when nothing changed).
    pub fn release_agent(
        &mut self,
        agent_id: &str,
        note: &str,
        now: u64,
    ) -> Result<Vec<Ticket>, TicketError> {
        let ids: Vec<TicketId> = self
            .doc
            .tickets
            .iter()
            .filter(|t| {
                t.assignee_agent_id.as_deref() == Some(agent_id)
                    && matches!(
                        t.state,
                        TicketState::Assigned | TicketState::InProgress | TicketState::Rejected
                    )
            })
            .map(|t| t.id.clone())
            .collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let ev = TicketEvent::ToBacklog {
            note: Some(note.to_string()),
        };
        self.commit(|doc| {
            ids.iter()
                .map(|id| apply(doc, id, &ev, TicketActor::System, None, now))
                .collect::<Result<Vec<_>, _>>()
        })
    }

    // ---- agent tools API (tickets::tools; the caller has checked that the agent is live) ----

    /// `mira_create_ticket`: like [`Self::create`], but `source: agent` and the creation entry
    /// by the agent. With `assign_to` (agent id, coordinator name; the caller checked the role
    /// and that the agent is live) the ticket goes last in that agent's queue in the same save.
    pub fn create_by_agent(
        &mut self,
        title: &str,
        body: &str,
        skip_review: bool,
        assign_to: Option<(&str, &str)>,
        now: u64,
    ) -> Result<Ticket, TicketError> {
        let t = self.new_ticket(
            &mut || uuid::Uuid::new_v4().to_string(),
            title,
            body,
            skip_review,
            (TicketSource::Agent, TicketActor::Agent),
            now,
        )?;
        let id = t.id.clone();
        self.commit(|doc| {
            doc.tickets.push(t);
            if let Some((agent_id, by_name)) = assign_to {
                let ev = TicketEvent::Assign {
                    agent_id: agent_id.to_string(),
                };
                let note = Some(format!("tildelt af koordinator {by_name}"));
                apply(doc, &id, &ev, TicketActor::Agent, note, now)?;
            }
            Ok(())
        })?;
        self.fetch(&id)
    }

    /// `mira_submit_for_review`: the agent's ticket (`ticket_id`, full or short id, or else its
    /// inProgress ticket) → review (done with skipReview), `summary` stored and noted in the
    /// history by the agent. Clears `issue` (the Submit transition does).
    pub fn submit_by_agent(
        &mut self,
        agent_id: &str,
        ticket_id: Option<&str>,
        summary: &str,
        now: u64,
    ) -> Result<Ticket, TicketError> {
        let summary = validate_summary(summary)?;
        let t = match ticket_id {
            Some(id) => {
                let t = self.get_by_any_id(id).ok_or(TicketError::NotFound)?;
                if t.assignee_agent_id.as_deref() != Some(agent_id) {
                    return Err(TicketError::NotYours);
                }
                if t.state != TicketState::InProgress {
                    return Err(TicketError::NotInProgress);
                }
                t
            }
            None => self
                .current_for_agent(agent_id)
                .ok_or(TicketError::NoTicketInProgress)?,
        };
        self.commit(|doc| {
            find_mut(doc, &t.id)?.summary = Some(summary.clone());
            apply(
                doc,
                &t.id,
                &TicketEvent::Submit,
                TicketActor::Agent,
                Some(summary),
                now,
            )
        })?;
        self.fetch(&t.id)
    }

    /// `mira_update_status`: a history entry (state unchanged, by the agent) with `note` on the
    /// agent's inProgress ticket. `Ok(None)` when it has none (nothing saved).
    pub fn note_by_agent(
        &mut self,
        agent_id: &str,
        note: &str,
        now: u64,
    ) -> Result<Option<Ticket>, TicketError> {
        let Some(t) = self.current_for_agent(agent_id) else {
            return Ok(None);
        };
        self.commit(|doc| {
            let t = find_mut(doc, &t.id)?;
            t.updated_at = now;
            t.history.push(TicketHistoryEntry {
                at: now,
                from: Some(t.state),
                to: t.state,
                by: TicketActor::Agent,
                note: Some(note.to_string()),
            });
            Ok(())
        })?;
        self.fetch(&t.id).map(Some)
    }

    // ---- review routing and agent review/coordination tools (step 5) ----

    /// Open review assignments per reviewer.
    pub fn open_review_counts(&self) -> ReviewCounts {
        let mut m = ReviewCounts::new();
        for a in &self.doc.review_assignments {
            *m.entry(a.reviewer_agent_id.clone()).or_default() += 1;
        }
        m
    }

    /// All review assignments, oldest first.
    pub fn review_assignments(&self) -> Vec<ReviewAssignment> {
        let mut v = self.doc.review_assignments.clone();
        v.sort_by_key(|a| a.assigned_at);
        v
    }

    /// The reviewer's assignments: undelivered first, each group oldest first.
    pub fn assignments_for(&self, reviewer_id: &str) -> Vec<ReviewAssignment> {
        let mut v: Vec<ReviewAssignment> = self
            .doc
            .review_assignments
            .iter()
            .filter(|a| a.reviewer_agent_id == reviewer_id)
            .cloned()
            .collect();
        v.sort_by_key(|a| (a.delivered_at.is_some(), a.assigned_at));
        v
    }

    pub fn assignment_for_ticket(&self, ticket_id: &str) -> Option<ReviewAssignment> {
        self.doc
            .review_assignments
            .iter()
            .find(|a| a.ticket_id == ticket_id)
            .cloned()
    }

    /// The next review to type into `reviewer_id`: its oldest undelivered assignment that has
    /// not used up its delivery attempts, with the ticket.
    pub fn next_review_for(&self, reviewer_id: &str) -> Option<(ReviewAssignment, Ticket)> {
        self.assignments_for(reviewer_id)
            .into_iter()
            .filter(|a| a.delivered_at.is_none() && a.attempts < REVIEW_DELIVERY_MAX_ATTEMPTS)
            .find_map(|a| self.get(&a.ticket_id).map(|t| (a, t)))
    }

    /// Tickets in review without a reviewer and not escalated, oldest first.
    pub fn unrouted_reviews(&self) -> Vec<Ticket> {
        let mut v: Vec<Ticket> = self
            .doc
            .tickets
            .iter()
            .filter(|t| {
                t.state == TicketState::Review && t.reviewer_agent_id.is_none() && !t.escalated
            })
            .cloned()
            .collect();
        v.sort_by_key(|t| (t.updated_at, t.created_at));
        v
    }

    /// Number of escalated tickets (Diagnostics).
    pub fn escalated_count(&self) -> usize {
        self.doc.tickets.iter().filter(|t| t.escalated).count()
    }

    /// Number of reports on all tickets (Diagnostics).
    pub fn report_count(&self) -> usize {
        self.doc.tickets.iter().map(|t| t.reports.len()).sum()
    }

    /// Gives an unrouted review ticket to `reviewer_id`: assignment with `round = review_round`,
    /// `reviewer_agent_id` set, history "review tildelt <name>" by the system. `Ok(None)` when the
    /// ticket no longer needs routing (already routed, escalated, left review): idempotent.
    pub fn route_review(
        &mut self,
        ticket_id: &str,
        reviewer_id: &str,
        reviewer_name: &str,
        now: u64,
    ) -> Result<Option<Ticket>, TicketError> {
        let t = self.get(ticket_id).ok_or(TicketError::NotFound)?;
        if t.state != TicketState::Review || t.reviewer_agent_id.is_some() || t.escalated {
            return Ok(None);
        }
        if t.assignee_agent_id.as_deref() == Some(reviewer_id) {
            return Err(TicketError::SenderCannotReview);
        }
        self.set_reviewer_in(ticket_id, reviewer_id, reviewer_name, now)
            .map(Some)
    }

    /// Manual reviewer choice (`assign_reviewer`): replaces any current reviewer and clears an
    /// escalation. The caller checked the agent (live, reviewer role).
    pub fn set_reviewer(
        &mut self,
        ticket_id: &str,
        reviewer_id: &str,
        reviewer_name: &str,
        now: u64,
    ) -> Result<Ticket, TicketError> {
        let t = self.get(ticket_id).ok_or(TicketError::NotFound)?;
        if t.state != TicketState::Review {
            return Err(TicketError::NotInReview);
        }
        if t.assignee_agent_id.as_deref() == Some(reviewer_id) {
            return Err(TicketError::SenderCannotReview);
        }
        self.set_reviewer_in(ticket_id, reviewer_id, reviewer_name, now)
    }

    fn set_reviewer_in(
        &mut self,
        ticket_id: &str,
        reviewer_id: &str,
        reviewer_name: &str,
        now: u64,
    ) -> Result<Ticket, TicketError> {
        self.commit(|doc| {
            doc.review_assignments.retain(|a| a.ticket_id != ticket_id);
            let t = find_mut(doc, ticket_id)?;
            t.reviewer_agent_id = Some(reviewer_id.to_string());
            t.escalated = false;
            let round = t.review_round;
            note_entry(
                t,
                TicketActor::System,
                format!("review tildelt {reviewer_name}"),
                now,
            );
            doc.review_assignments.push(ReviewAssignment {
                ticket_id: ticket_id.to_string(),
                reviewer_agent_id: reviewer_id.to_string(),
                round,
                assigned_at: now,
                delivered_at: None,
                attempts: 0,
            });
            Ok(())
        })?;
        self.fetch(ticket_id)
    }

    /// The ticket reached [`MAX_REVIEW_ROUNDS`]: `escalated`, note "eskaleret efter 3 runder",
    /// no routing. `Ok(None)` when it is not an unrouted review ticket (idempotent).
    pub fn escalate(&mut self, ticket_id: &str, now: u64) -> Result<Option<Ticket>, TicketError> {
        let t = self.get(ticket_id).ok_or(TicketError::NotFound)?;
        if t.state != TicketState::Review || t.escalated {
            return Ok(None);
        }
        self.commit(|doc| {
            let t = find_mut(doc, ticket_id)?;
            t.escalated = true;
            t.reviewer_agent_id = None;
            note_entry(t, TicketActor::System, escalated_note(), now);
            Ok(())
        })?;
        self.fetch(ticket_id).map(Some)
    }

    /// The review line was confirmed in the reviewer's terminal: `delivered_at`, note "review
    /// sendt til <name>".
    pub fn mark_review_delivered(
        &mut self,
        ticket_id: &str,
        reviewer_name: &str,
        now: u64,
    ) -> Result<Ticket, TicketError> {
        if self.assignment_for_ticket(ticket_id).is_none() {
            return Err(TicketError::NotInReview);
        }
        self.commit(|doc| {
            if let Some(a) = doc
                .review_assignments
                .iter_mut()
                .find(|a| a.ticket_id == ticket_id)
            {
                a.delivered_at = Some(now);
            }
            let t = find_mut(doc, ticket_id)?;
            note_entry(
                t,
                TicketActor::System,
                format!("review sendt til {reviewer_name}"),
                now,
            );
            Ok(())
        })?;
        self.fetch(ticket_id)
    }

    /// A review delivery failed: `attempts += 1`; at [`REVIEW_DELIVERY_MAX_ATTEMPTS`] the note
    /// "review kunne ikke leveres" (no more automatic tries). Returns the new attempt count.
    pub fn review_delivery_failed(
        &mut self,
        ticket_id: &str,
        now: u64,
    ) -> Result<u32, TicketError> {
        let a = self
            .assignment_for_ticket(ticket_id)
            .ok_or(TicketError::NotInReview)?;
        let attempts = a.attempts.saturating_add(1);
        self.commit(|doc| {
            if let Some(a) = doc
                .review_assignments
                .iter_mut()
                .find(|a| a.ticket_id == ticket_id)
            {
                a.attempts = attempts;
            }
            if attempts >= REVIEW_DELIVERY_MAX_ATTEMPTS {
                let t = find_mut(doc, ticket_id)?;
                note_entry(t, TicketActor::System, REVIEW_UNDELIVERED_NOTE.into(), now);
            }
            Ok(())
        })?;
        Ok(attempts)
    }

    /// "Send igen" for a review: the attempts start over and the line counts as undelivered.
    pub fn reset_review_delivery(
        &mut self,
        ticket_id: &str,
    ) -> Result<ReviewAssignment, TicketError> {
        if self.assignment_for_ticket(ticket_id).is_none() {
            return Err(TicketError::NotInReview);
        }
        self.commit(|doc| {
            if let Some(a) = doc
                .review_assignments
                .iter_mut()
                .find(|a| a.ticket_id == ticket_id)
            {
                a.attempts = 0;
                a.delivered_at = None;
            }
            Ok(())
        })?;
        self.assignment_for_ticket(ticket_id)
            .ok_or(TicketError::NotInReview)
    }

    /// Removes the ticket's reviewer and assignment (stays in review; routed again) and clears an
    /// escalation, with `note` by `by`. A ticket that reached [`MAX_REVIEW_ROUNDS`] stays (or
    /// becomes again) escalated without a new escalation note, so routing leaves it to the user
    /// (review5 N5). `Ok(None)` when there is nothing to remove.
    pub fn clear_reviewer(
        &mut self,
        ticket_id: &str,
        note: &str,
        by: TicketActor,
        now: u64,
    ) -> Result<Option<Ticket>, TicketError> {
        let t = self.get(ticket_id).ok_or(TicketError::NotFound)?;
        if t.state != TicketState::Review {
            return Err(TicketError::NotInReview);
        }
        let keep_escalated = t.review_round >= MAX_REVIEW_ROUNDS;
        if t.reviewer_agent_id.is_none() && (!t.escalated || keep_escalated) {
            return Ok(None);
        }
        self.commit(|doc| {
            doc.review_assignments.retain(|a| a.ticket_id != ticket_id);
            let t = find_mut(doc, ticket_id)?;
            t.reviewer_agent_id = None;
            t.escalated = keep_escalated;
            note_entry(t, by, note.to_string(), now);
            Ok(())
        })?;
        self.fetch(ticket_id).map(Some)
    }

    /// The reviewer stopped/exited/was removed: all its assignments go; the tickets stay in
    /// review without a reviewer (note "reviewer <note>"). One save (none when it had none).
    pub fn release_reviewer(
        &mut self,
        agent_id: &str,
        note: &str,
        now: u64,
    ) -> Result<Vec<TicketId>, TicketError> {
        let ids: Vec<TicketId> = self
            .doc
            .tickets
            .iter()
            .filter(|t| {
                t.state == TicketState::Review && t.reviewer_agent_id.as_deref() == Some(agent_id)
            })
            .map(|t| t.id.clone())
            .collect();
        let has_assignments = self
            .doc
            .review_assignments
            .iter()
            .any(|a| a.reviewer_agent_id == agent_id);
        if ids.is_empty() && !has_assignments {
            return Ok(Vec::new());
        }
        self.commit(|doc| {
            doc.review_assignments
                .retain(|a| a.reviewer_agent_id != agent_id);
            for id in &ids {
                let t = find_mut(doc, id)?;
                t.reviewer_agent_id = None;
                note_entry(t, TicketActor::System, format!("reviewer {note}"), now);
            }
            Ok(())
        })?;
        Ok(ids)
    }

    /// The review checks shared by approve/reject (any id): in review, not the agent's own
    /// submission, and the agent is its reviewer.
    fn review_ticket_for(&self, agent_id: &str, ticket_id: &str) -> Result<Ticket, TicketError> {
        let t = self.get_by_any_id(ticket_id).ok_or(TicketError::NotFound)?;
        if t.state != TicketState::Review {
            return Err(TicketError::NotInReview);
        }
        if t.assignee_agent_id.as_deref() == Some(agent_id) {
            return Err(TicketError::OwnSubmission);
        }
        if t.reviewer_agent_id.as_deref() != Some(agent_id) {
            return Err(TicketError::NotYourReview);
        }
        Ok(t)
    }

    /// `mira_approve_ticket`: review → done by the agent, note "godkendt af <name>[: note]";
    /// the assignment goes.
    pub fn approve_by_agent(
        &mut self,
        agent_id: &str,
        agent_name: &str,
        ticket_id: &str,
        note: Option<&str>,
        now: u64,
    ) -> Result<Ticket, TicketError> {
        let t = self.review_ticket_for(agent_id, ticket_id)?;
        let note = match note.map(str::trim).filter(|n| !n.is_empty()) {
            Some(n) => format!("godkendt af {agent_name}: {n}"),
            None => format!("godkendt af {agent_name}"),
        };
        self.commit(|doc| {
            doc.review_assignments.retain(|a| a.ticket_id != t.id);
            apply(
                doc,
                &t.id,
                &TicketEvent::Approve,
                TicketActor::Agent,
                Some(note),
                now,
            )
        })?;
        self.fetch(&t.id)
    }

    /// `mira_reject_ticket`: like the user's reject (round + 1, first in the sender's queue when
    /// it is live, else the backlog), by the agent with "afvist af <name>: <note>".
    pub fn reject_by_agent(
        &mut self,
        agent_id: &str,
        agent_name: &str,
        ticket_id: &str,
        note: &str,
        sender_live: bool,
        now: u64,
    ) -> Result<Ticket, TicketError> {
        let t = self.review_ticket_for(agent_id, ticket_id)?;
        if note.trim().is_empty() {
            return Err(TicketError::NeedsNote);
        }
        self.reject_as(
            &t.id,
            note.trim(),
            sender_live,
            TicketActor::Agent,
            Some(format!("afvist af {agent_name}")),
            now,
        )
    }

    /// `mira_assign_ticket` (coordinator; any id): backlog/rejected → last in `target`'s queue,
    /// by the agent with "tildelt af koordinator <name>". The caller checked that `target` is
    /// live.
    pub fn assign_by_agent(
        &mut self,
        ticket_id: &str,
        target: &str,
        by_name: &str,
        now: u64,
    ) -> Result<Ticket, TicketError> {
        let t = self.get_by_any_id(ticket_id).ok_or(TicketError::NotFound)?;
        let note = Some(format!("tildelt af koordinator {by_name}"));
        self.commit(|doc| {
            if t.state == TicketState::Rejected {
                apply(
                    doc,
                    &t.id,
                    &TicketEvent::ToBacklog { note: None },
                    TicketActor::Agent,
                    None,
                    now,
                )?;
            }
            let ev = TicketEvent::Assign {
                agent_id: target.to_string(),
            };
            apply(doc, &t.id, &ev, TicketActor::Agent, note, now)
        })?;
        self.fetch(&t.id)
    }

    /// `mira_unassign_ticket` (coordinator; any id): assigned → backlog by the agent. A ticket in
    /// progress goes the [`Self::give_back`] way (only the caller's own, step 5c).
    pub fn unassign_by_agent(
        &mut self,
        ticket_id: &str,
        by_agent: &str,
        now: u64,
    ) -> Result<Ticket, TicketError> {
        let t = self.get_by_any_id(ticket_id).ok_or(TicketError::NotFound)?;
        if t.state == TicketState::InProgress {
            return self.give_back(&t.id, Some(by_agent), now);
        }
        self.commit(|doc| {
            apply(
                doc,
                &t.id,
                &TicketEvent::Unassign,
                TicketActor::Agent,
                None,
                now,
            )
        })?;
        self.fetch(&t.id)
    }

    // ---- step 5c: handing a ticket in progress on ----

    /// The ticket in progress (full or short id) that `by_agent` may hand on: `None` = the user
    /// (any ticket in progress); `Some(agent)` = only the agent's own ticket in progress.
    fn handoff_source(
        &self,
        ticket_id: &str,
        by_agent: Option<&str>,
    ) -> Result<Ticket, TicketError> {
        let t = self.get_by_any_id(ticket_id).ok_or(TicketError::NotFound)?;
        if let Some(agent) = by_agent {
            if t.assignee_agent_id.as_deref() != Some(agent) {
                return Err(TicketError::NotYours);
            }
        }
        if t.state != TicketState::InProgress {
            return Err(TicketError::NotInProgress);
        }
        Ok(t)
    }

    /// Handoff (step 5c): the ticket in progress → last in `to_agent`'s queue, with the history
    /// note "overdraget fra <from_name> til <to_name>". `by_agent`: `None` = the user, else the
    /// agent asking, which must be the current assignee ([`TicketError::NotYours`]). The
    /// rejection note and review round stay. The old assignee then has no ticket in progress, so
    /// its next Stop does not mark this one "ikke afleveret" and its queue moves on. The caller
    /// checked that `to_agent` is live, and notifies both agents.
    pub fn handoff(
        &mut self,
        ticket_id: &str,
        to_agent: &str,
        by_agent: Option<&str>,
        (from_name, to_name): (&str, &str),
        now: u64,
    ) -> Result<Ticket, TicketError> {
        let t = self.handoff_source(ticket_id, by_agent)?;
        let by = if by_agent.is_some() {
            TicketActor::Agent
        } else {
            TicketActor::User
        };
        let ev = TicketEvent::Handoff {
            to_agent_id: to_agent.to_string(),
        };
        let note = Some(format!("overdraget fra {from_name} til {to_name}"));
        self.commit(|doc| apply(doc, &t.id, &ev, by, note, now))?;
        self.fetch(&t.id)
    }

    /// The ticket in progress back to the backlog ("lagt tilbage", step 5c); `by_agent` as in
    /// [`Self::handoff`].
    pub fn give_back(
        &mut self,
        ticket_id: &str,
        by_agent: Option<&str>,
        now: u64,
    ) -> Result<Ticket, TicketError> {
        let t = self.handoff_source(ticket_id, by_agent)?;
        let by = if by_agent.is_some() {
            TicketActor::Agent
        } else {
            TicketActor::User
        };
        self.commit(|doc| apply(doc, &t.id, &TicketEvent::Unassign, by, None, now))?;
        self.fetch(&t.id)
    }

    /// The next report number of the ticket (1-based; never reused).
    pub fn next_report_seq(&self, ticket_id: &str) -> Result<u32, TicketError> {
        let t = self.get(ticket_id).ok_or(TicketError::NotFound)?;
        if t.reports.len() >= REPORTS_PER_TICKET_MAX {
            return Err(TicketError::TooManyReports);
        }
        let max = t
            .reports
            .iter()
            .filter_map(|r| r.id.parse::<u32>().ok())
            .max()
            .unwrap_or(0);
        Ok(max + 1)
    }

    /// Adds report metadata (the file is already written). At most [`REPORTS_PER_TICKET_MAX`].
    pub fn add_report_meta(
        &mut self,
        ticket_id: &str,
        report: TicketReport,
        now: u64,
    ) -> Result<Ticket, TicketError> {
        self.commit(|doc| {
            let t = find_mut(doc, ticket_id)?;
            if t.reports.len() >= REPORTS_PER_TICKET_MAX {
                return Err(TicketError::TooManyReports);
            }
            t.reports.push(report);
            t.updated_at = now;
            Ok(())
        })?;
        self.fetch(ticket_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tickets::store::MemoryStore;
    use TicketState as S;

    fn svc() -> (TicketService, MemoryStore) {
        let m = MemoryStore::new();
        (
            TicketService::new(Box::new(m.clone()), TicketDoc::default()),
            m,
        )
    }

    fn mk(s: &mut TicketService, title: &str, now: u64) -> Ticket {
        s.create(title, "body", false, now).unwrap()
    }

    fn positions(s: &TicketService, agent: &str) -> Vec<(String, Option<usize>)> {
        s.queue(agent)
            .into_iter()
            .map(|t| (t.title, t.queue_position))
            .collect()
    }

    /// Checks the service invariants.
    fn assert_invariants(s: &TicketService) {
        let mut queues: HashMap<String, Vec<usize>> = HashMap::new();
        let mut in_progress: HashMap<String, usize> = HashMap::new();
        for t in &s.doc.tickets {
            match t.state {
                S::Assigned => {
                    let a = t.assignee_agent_id.clone().expect("assigned without agent");
                    queues
                        .entry(a)
                        .or_default()
                        .push(t.queue_position.expect("no position"));
                }
                _ => assert_eq!(t.queue_position, None, "{t:?}"),
            }
            if t.state == S::InProgress {
                *in_progress
                    .entry(t.assignee_agent_id.clone().unwrap())
                    .or_default() += 1;
            }
            if t.state == S::Backlog {
                assert_eq!(t.assignee_agent_id, None);
            }
        }
        for (_, mut p) in queues {
            p.sort_unstable();
            assert_eq!(p, (0..p.len()).collect::<Vec<_>>());
        }
        assert!(in_progress.values().all(|n| *n <= 1));
    }

    #[test]
    fn create_validates_and_saves() {
        let (mut s, m) = svc();
        assert_eq!(
            s.create("   ", "", false, 1).unwrap_err().to_string(),
            "Titel må ikke være tom"
        );
        assert_eq!(
            s.create(&"x".repeat(201), "", false, 1)
                .unwrap_err()
                .to_string(),
            "Titlen er for lang (maks 200 tegn)"
        );
        assert_eq!(
            s.create("ok", &"y".repeat(20_001), false, 1)
                .unwrap_err()
                .to_string(),
            "Teksten er for lang (maks 20000 tegn)"
        );
        assert_eq!(m.saves(), 0);
        let t = s.create("  Ret login ", "b", true, 5).unwrap();
        assert_eq!(t.title, "Ret login");
        assert_eq!(
            (t.state, t.skip_review, t.created_at),
            (S::Backlog, true, 5)
        );
        assert_eq!(t.history.len(), 1);
        assert_eq!(t.history[0].from, None);
        assert_eq!(m.saves(), 1);
        assert_eq!(m.doc().unwrap().tickets.len(), 1);
        assert!(s.create(&"æ".repeat(200), "", false, 1).is_ok());
    }

    #[test]
    fn create_regenerates_on_short_id_collision() {
        let (mut s, _) = svc();
        let mut ids = vec!["abcdef01-0000-4000-8000-000000000001".to_string()].into_iter();
        let a = s
            .create_with_id_source(
                &mut || ids.next().unwrap(),
                "a",
                "",
                false,
                (TicketSource::User, TicketActor::User),
                1,
            )
            .unwrap();
        let mut ids = vec![
            "ABCDEF01-9999-4000-8000-000000000002".to_string(),
            "12345678-0000-4000-8000-000000000003".to_string(),
        ]
        .into_iter();
        let b = s
            .create_with_id_source(
                &mut || ids.next().unwrap(),
                "b",
                "",
                false,
                (TicketSource::User, TicketActor::User),
                2,
            )
            .unwrap();
        assert_eq!(a.short_id(), "abcdef01");
        assert_eq!(b.short_id(), "12345678");
    }

    #[test]
    fn update_and_delete_rules() {
        let (mut s, _) = svc();
        let t = mk(&mut s, "a", 1);
        let u = s
            .update(
                &t.id,
                TicketPatch {
                    title: Some(" ny ".into()),
                    body: None,
                    skip_review: Some(true),
                    project: None,
                },
                9,
            )
            .unwrap();
        assert_eq!(
            (
                u.title.as_str(),
                u.body.as_str(),
                u.skip_review,
                u.updated_at
            ),
            ("ny", "body", true, 9)
        );
        assert!(s
            .update(
                &t.id,
                TicketPatch {
                    title: Some("".into()),
                    ..Default::default()
                },
                9
            )
            .is_err());
        assert_eq!(s.get(&t.id).unwrap().title, "ny");

        s.assign(&t.id, "a1", 2).unwrap();
        assert_eq!(s.delete(&t.id), Err(TicketError::NotDeletable));
        s.unassign(&t.id, 3).unwrap();
        s.delete(&t.id).unwrap();
        assert!(s.get(&t.id).is_none());
        assert_eq!(s.delete(&t.id), Err(TicketError::NotFound));
    }

    #[test]
    fn assign_unassign_and_reorder_keep_positions_dense() {
        let (mut s, _) = svc();
        let ids: Vec<String> = (0..4).map(|i| mk(&mut s, &format!("t{i}"), i).id).collect();
        for (i, id) in ids.iter().enumerate() {
            let t = s.assign(id, "a1", 10 + i as u64).unwrap();
            assert_eq!(t.queue_position, Some(i));
        }
        assert_invariants(&s);
        s.unassign(&ids[1], 20).unwrap();
        assert_eq!(
            positions(&s, "a1"),
            vec![
                ("t0".into(), Some(0)),
                ("t2".into(), Some(1)),
                ("t3".into(), Some(2))
            ]
        );
        let q = s
            .reorder("a1", &[ids[3].clone(), ids[0].clone(), ids[2].clone()])
            .unwrap();
        assert_eq!(
            q.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(),
            ["t3", "t0", "t2"]
        );
        assert_eq!(
            s.reorder("a1", &[ids[3].clone(), ids[0].clone()])
                .unwrap_err()
                .to_string(),
            "Køen passer ikke"
        );
        assert!(s
            .reorder("a1", &[ids[3].clone(), ids[3].clone(), ids[0].clone()])
            .is_err());
        // Assigning a queued ticket again is illegal (one queue per ticket).
        assert!(matches!(
            s.assign(&ids[0], "a2", 30),
            Err(TicketError::IllegalTransition { .. })
        ));
        assert_invariants(&s);
    }

    #[test]
    fn reject_requeues_first_or_goes_to_backlog() {
        let (mut s, m) = svc();
        let a = mk(&mut s, "a", 1);
        let b = mk(&mut s, "b", 2);
        let c = mk(&mut s, "c", 3);
        for id in [&a.id, &b.id, &c.id] {
            s.assign(id, "a1", 4).unwrap();
        }
        s.mark_dispatched(&a.id, "demo", 5).unwrap();
        s.complete_turn("a1", 6).unwrap();
        assert_eq!(s.get(&a.id).unwrap().state, S::Review);
        assert_eq!(s.reject(&a.id, "  ", true, 7), Err(TicketError::NeedsNote));
        let saves = m.saves();
        let r = s.reject(&a.id, "mangler test", true, 7).unwrap();
        assert_eq!(m.saves(), saves + 1);
        assert_eq!((r.state, r.queue_position), (S::Assigned, Some(0)));
        assert_eq!(r.rejection_note.as_deref(), Some("mangler test"));
        assert_eq!(
            positions(&s, "a1"),
            vec![
                ("a".into(), Some(0)),
                ("b".into(), Some(1)),
                ("c".into(), Some(2))
            ]
        );
        let tail: Vec<_> = r.history.iter().rev().take(2).map(|h| h.to).collect();
        assert_eq!(tail, vec![S::Assigned, S::Rejected]);

        // Agent gone: rejected → backlog.
        s.mark_dispatched(&a.id, "demo", 8).unwrap();
        s.complete_turn("a1", 9).unwrap();
        let r = s.reject(&a.id, "nej", false, 10).unwrap();
        assert_eq!((r.state, r.assignee_agent_id.clone()), (S::Backlog, None));
        assert_invariants(&s);
    }

    #[test]
    fn release_agent_moves_everything_to_backlog_in_one_save() {
        let (mut s, m) = svc();
        let ids: Vec<String> = (0..3).map(|i| mk(&mut s, &format!("t{i}"), i).id).collect();
        for id in &ids {
            s.assign(id, "a1", 5).unwrap();
        }
        let other = mk(&mut s, "x", 6);
        s.assign(&other.id, "a2", 6).unwrap();
        s.mark_dispatched(&ids[0], "demo", 7).unwrap();
        let saves = m.saves();
        let released = s.release_agent("a1", "agent stoppet", 8).unwrap();
        assert_eq!(released.len(), 3);
        assert_eq!(m.saves(), saves + 1);
        for id in &ids {
            let t = s.get(id).unwrap();
            assert_eq!((t.state, t.assignee_agent_id.clone()), (S::Backlog, None));
            assert_eq!(
                t.history.last().unwrap().note.as_deref(),
                Some("agent stoppet")
            );
        }
        assert_eq!(s.get(&other.id).unwrap().state, S::Assigned);
        assert!(s.release_agent("a1", "igen", 9).unwrap().is_empty());
        assert_eq!(m.saves(), saves + 1);
        assert_invariants(&s);
    }

    #[test]
    fn next_for_agent_and_dispatch_invariant() {
        let (mut s, _) = svc();
        assert!(s.next_for_agent("a1").is_none());
        let a = mk(&mut s, "a", 1);
        let b = mk(&mut s, "b", 2);
        s.assign(&a.id, "a1", 3).unwrap();
        s.assign(&b.id, "a1", 4).unwrap();
        assert_eq!(s.next_for_agent("a1").unwrap().id, a.id);
        let d = s.mark_dispatched(&a.id, "demo", 5).unwrap();
        assert_eq!(d.state, S::InProgress);
        assert_eq!(
            d.history.last().unwrap().note.as_deref(),
            Some("sendt til demo")
        );
        assert!(s.next_for_agent("a1").is_none());
        // A second inProgress for the same agent is refused.
        assert_eq!(
            s.mark_dispatched(&b.id, "demo", 6),
            Err(TicketError::AgentBusy)
        );
        assert_eq!(
            s.set_state(&b.id, S::InProgress, None, true, 6),
            Err(TicketError::AgentBusy)
        );
        // Redispatch of the inProgress ticket: history entry, issue cleared.
        s.set_issue(
            &a.id,
            Some(TicketIssue::TurnFailed),
            Some("StopFailure".into()),
            7,
        )
        .unwrap();
        let again = s.mark_dispatched(&a.id, "demo", 8).unwrap();
        assert_eq!((again.state, again.issue), (S::InProgress, None));
        assert_eq!(
            again.history.last().unwrap().note.as_deref(),
            Some(RESENT_NOTE)
        );
        assert_invariants(&s);
    }

    #[test]
    fn complete_turn_respects_skip_review() {
        let (mut s, m) = svc();
        assert_eq!(s.complete_turn("a1", 1).unwrap(), None);
        assert_eq!(m.saves(), 0);
        let a = s.create("a", "", false, 1).unwrap();
        let b = s.create("b", "", true, 1).unwrap();
        s.assign(&a.id, "a1", 2).unwrap();
        s.assign(&b.id, "a2", 2).unwrap();
        s.mark_dispatched(&a.id, "x", 3).unwrap();
        s.mark_dispatched(&b.id, "y", 3).unwrap();
        let ra = s.complete_turn("a1", 4).unwrap().unwrap();
        assert_eq!(ra.state, S::Review);
        assert_eq!(
            ra.history.last().unwrap().note.as_deref(),
            Some(TURN_ENDED_NOTE)
        );
        assert_eq!(ra.history.last().unwrap().by, TicketActor::System);
        assert_eq!(s.complete_turn("a2", 4).unwrap().unwrap().state, S::Done);
    }

    #[test]
    fn set_state_follows_the_c3_3_table() {
        let (mut s, _) = svc();
        let t = mk(&mut s, "a", 1);
        let id = t.id.clone();
        let err =
            |s: &mut TicketService, to, live| s.set_state(&id, to, None, live, 2).unwrap_err();
        // From backlog.
        assert!(matches!(
            err(&mut s, S::Backlog, true),
            TicketError::IllegalTransition { .. }
        ));
        assert_eq!(err(&mut s, S::Assigned, true), TicketError::UseAssign);
        assert_eq!(err(&mut s, S::Rejected, true), TicketError::UseReject);
        assert!(matches!(
            err(&mut s, S::InProgress, true),
            TicketError::IllegalTransition { .. }
        ));
        assert!(matches!(
            err(&mut s, S::Done, true),
            TicketError::IllegalTransition { .. }
        ));
        // assigned → inProgress needs a live agent; assigned → backlog is Unassign.
        s.assign(&id, "a1", 2).unwrap();
        assert_eq!(err(&mut s, S::InProgress, false), TicketError::AgentNotLive);
        assert_eq!(
            err(&mut s, S::Done, true),
            TicketError::IllegalTransition {
                from: S::Assigned,
                to: S::Done
            }
        );
        assert_eq!(
            s.set_state(&id, S::InProgress, None, true, 3)
                .unwrap()
                .state,
            S::InProgress
        );
        // inProgress → done needs skipReview.
        assert_eq!(err(&mut s, S::Done, true), TicketError::DoneNeedsReview);
        assert_eq!(err(&mut s, S::Rejected, true), TicketError::UseReject);
        assert_eq!(
            s.set_state(&id, S::Review, None, true, 4).unwrap().state,
            S::Review
        );
        // review → inProgress (Reopen) needs a live agent.
        assert_eq!(err(&mut s, S::InProgress, false), TicketError::AgentNotLive);
        assert_eq!(
            s.set_state(&id, S::InProgress, None, true, 5)
                .unwrap()
                .state,
            S::InProgress
        );
        s.set_state(&id, S::Review, None, true, 6).unwrap();
        // review → rejected = reject_ticket (note required).
        assert_eq!(err(&mut s, S::Rejected, true), TicketError::NeedsNote);
        let r = s
            .set_state(&id, S::Rejected, Some("nej".into()), true, 7)
            .unwrap();
        assert_eq!((r.state, r.queue_position), (S::Assigned, Some(0)));
        s.set_state(&id, S::InProgress, None, true, 8).unwrap();
        s.set_state(&id, S::Review, None, true, 9).unwrap();
        assert_eq!(
            s.set_state(&id, S::Done, None, true, 10).unwrap().state,
            S::Done
        );
        assert_eq!(
            err(&mut s, S::Review, true),
            TicketError::IllegalTransition {
                from: S::Done,
                to: S::Review
            }
        );
        // done → backlog reopens.
        let b = s.set_state(&id, S::Backlog, None, true, 11).unwrap();
        assert_eq!(
            b.history.last().unwrap().note.as_deref(),
            Some(REOPENED_NOTE)
        );
        // skipReview: inProgress → done directly.
        s.update(
            &id,
            TicketPatch {
                skip_review: Some(true),
                ..Default::default()
            },
            12,
        )
        .unwrap();
        s.assign(&id, "a1", 13).unwrap();
        s.set_state(&id, S::InProgress, None, true, 14).unwrap();
        assert_eq!(
            s.set_state(&id, S::Done, None, true, 15).unwrap().state,
            S::Done
        );
        // inProgress/review → backlog with note.
        let t2 = mk(&mut s, "b", 16);
        s.assign(&t2.id, "a1", 17).unwrap();
        s.set_state(&t2.id, S::InProgress, None, true, 18).unwrap();
        let back = s
            .set_state(&t2.id, S::Backlog, Some("for stor".into()), true, 19)
            .unwrap();
        assert_eq!(
            back.history.last().unwrap().note.as_deref(),
            Some("for stor")
        );
        assert_invariants(&s);
    }

    #[test]
    fn load_and_recover_moves_queued_work_to_backlog() {
        let (mut s, _) = svc();
        let ids: Vec<String> = ["queued", "busy", "rev", "done", "back"]
            .iter()
            .enumerate()
            .map(|(i, t)| mk(&mut s, t, i as u64).id)
            .collect();
        s.assign(&ids[0], "a1", 10).unwrap();
        s.assign(&ids[1], "a1", 10).unwrap();
        s.assign(&ids[2], "a2", 10).unwrap();
        s.assign(&ids[3], "a2", 10).unwrap();
        s.mark_dispatched(&ids[1], "x", 11).unwrap();
        s.mark_dispatched(&ids[2], "y", 11).unwrap();
        s.complete_turn("a2", 12).unwrap();
        s.mark_dispatched(&ids[3], "y", 13).unwrap();
        s.complete_turn("a2", 14).unwrap();
        s.approve(&ids[3], 15).unwrap();
        s.mark_dispatched(&ids[0], "x", 16).unwrap_err();
        let doc = s.doc.clone();

        let m = MemoryStore::with_doc(doc.clone());
        let (r, warning) = TicketService::load_and_recover(Box::new(m.clone()), 100);
        assert_eq!(warning, None);
        assert_eq!(m.saves(), 1);
        let st = |i: usize| r.get(&ids[i]).unwrap();
        for i in [0, 1] {
            assert_eq!(st(i).state, S::Backlog);
            assert_eq!(st(i).assignee_agent_id, None);
            assert_eq!(
                st(i).history.last().unwrap().note.as_deref(),
                Some(RESTART_NOTE)
            );
        }
        assert_eq!(st(2), doc.tickets[2]);
        assert_eq!(st(3), doc.tickets[3]);
        assert_eq!(st(4), doc.tickets[4]);
        assert_invariants(&r);

        // Nothing to recover → no save.
        let m2 = MemoryStore::with_doc(m.doc().unwrap());
        let (_r2, _) = TicketService::load_and_recover(Box::new(m2.clone()), 200);
        assert_eq!(m2.saves(), 0);
    }

    #[test]
    fn unreadable_store_starts_read_only_and_never_saves() {
        let m = MemoryStore::with_doc(TicketDoc {
            schema_version: 1,
            tickets: vec![crate::tickets::model::test_support::ticket(
                "11111111-0000-4000-8000-000000000000",
                S::Assigned,
            )],
            review_assignments: Vec::new(),
        });
        let on_disk = m.doc();
        m.fail_load();
        let (mut r, warning) = TicketService::load_and_recover(Box::new(m.clone()), 100);
        assert!(r.is_read_only());
        let w = warning.expect("warning");
        assert!(w.contains("ændringer er slået fra"), "{w}");
        assert!(r.is_empty());
        assert_eq!(
            r.create("ny", "", false, 101).unwrap_err().to_string(),
            "Tickets-filen kunne ikke læses ved opstart; ændringer er slået fra. Genstart appen."
        );
        // Nothing to release (empty list) stays a silent no-op, also read-only.
        assert_eq!(r.release_agent("a1", "x", 102), Ok(Vec::new()));
        assert_eq!(m.saves(), 0);
        assert_eq!(m.doc(), on_disk);
        assert!(r.is_empty());

        // A readable store is not read-only.
        let (ok, _) = TicketService::load_and_recover(Box::new(MemoryStore::new()), 1);
        assert!(!ok.is_read_only());
    }

    #[test]
    fn unreadable_file_on_disk_is_left_untouched() {
        let dir = std::env::temp_dir().join(format!("mira-ro-{}", uuid::Uuid::new_v4()));
        let path = dir.join("tickets.json");
        std::fs::create_dir_all(&path).unwrap();
        let (mut r, warning) = TicketService::load_and_recover(
            Box::new(crate::tickets::store::JsonFileStore::new(path.clone())),
            1,
        );
        assert!(r.is_read_only());
        assert!(warning.is_some());
        assert_eq!(r.create("ny", "", false, 2), Err(TicketError::ReadOnly));
        assert!(path.is_dir());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_save_rolls_back() {
        let (mut s, m) = svc();
        let t = mk(&mut s, "a", 1);
        m.fail_next_save();
        assert!(matches!(s.assign(&t.id, "a1", 2), Err(TicketError::Io(_))));
        let now = s.get(&t.id).unwrap();
        assert_eq!(now, t);
        assert_eq!(m.doc().unwrap().tickets[0], t);
        m.fail_next_save();
        assert!(s.create("b", "", false, 3).is_err());
        assert_eq!(s.len(), 1);
        // Next mutation works again.
        assert_eq!(s.assign(&t.id, "a1", 4).unwrap().state, S::Assigned);
    }

    #[test]
    fn links_and_list() {
        let (mut s, _) = svc();
        let a = mk(&mut s, "a", 3);
        let b = mk(&mut s, "b", 1);
        let c = mk(&mut s, "c", 2);
        assert!(s.links().is_empty());
        s.assign(&a.id, "a1", 4).unwrap();
        s.assign(&b.id, "a1", 4).unwrap();
        s.assign(&c.id, "a2", 4).unwrap();
        s.mark_dispatched(&a.id, "x", 5).unwrap();
        let l = s.links();
        assert_eq!(l.get("a1"), Some(&(Some(a.id.clone()), 1)));
        assert_eq!(l.get("a2"), Some(&(None, 1)));
        assert_eq!(l.len(), 2);
        let titles: Vec<_> = s.list().into_iter().map(|t| t.title).collect();
        assert_eq!(titles, ["b", "c", "a"]);
        assert!(!s.is_empty());
    }

    // ---- agent tools API ----

    /// A ticket in progress for `agent` (created by the user, assigned, dispatched).
    fn in_progress(s: &mut TicketService, agent: &str, title: &str, skip: bool) -> Ticket {
        let t = s.create(title, "b", skip, 1).unwrap();
        s.assign(&t.id, agent, 2).unwrap();
        s.mark_dispatched(&t.id, agent, 3).unwrap()
    }

    #[test]
    fn submit_by_agent_needs_a_ticket_of_its_own_in_progress() {
        let (mut s, _) = svc();
        assert_eq!(
            s.submit_by_agent("a1", None, "done", 5).unwrap_err(),
            TicketError::NoTicketInProgress
        );
        let other = in_progress(&mut s, "a2", "other", false);
        assert_eq!(
            s.submit_by_agent("a1", Some(&other.id), "done", 5)
                .unwrap_err(),
            TicketError::NotYours
        );
        // Backlog ticket (no assignee) is not the agent's either.
        let free = mk(&mut s, "free", 4);
        assert_eq!(
            s.submit_by_agent("a1", Some(&free.short_id()), "done", 5)
                .unwrap_err(),
            TicketError::NotYours
        );
        let mine = in_progress(&mut s, "a1", "mine", false);
        let r = s.submit_by_agent("a1", None, "x", 6).unwrap();
        assert_eq!(r.id, mine.id);
        // Now in review: not in progress any more.
        assert_eq!(
            s.submit_by_agent("a1", Some(&mine.id), "again", 7)
                .unwrap_err(),
            TicketError::NotInProgress
        );
        assert_eq!(
            s.submit_by_agent("a1", Some("ffffffff"), "x", 7)
                .unwrap_err(),
            TicketError::NotFound
        );
        for bad in ["", "   ", &"x".repeat(2_001)] {
            assert_eq!(
                s.submit_by_agent("a2", None, bad, 8)
                    .unwrap_err()
                    .to_string(),
                "summary skal være en tekst på 1–2000 tegn"
            );
        }
        assert_invariants(&s);
    }

    #[test]
    fn submit_by_agent_moves_to_review_with_summary_and_clears_the_issue() {
        let (mut s, m) = svc();
        let t = in_progress(&mut s, "a1", "fix", false);
        let marked = s.mark_not_submitted("a1", 4).unwrap().unwrap();
        assert_eq!(marked.state, S::InProgress);
        assert_eq!(marked.issue, Some(TicketIssue::NotSubmitted));
        let last = marked.history.last().unwrap();
        assert_eq!(
            (last.by, last.note.as_deref()),
            (TicketActor::System, Some(NOT_SUBMITTED_NOTE))
        );
        let saves = m.saves();
        let r = s
            .submit_by_agent(
                "a1",
                Some(&t.short_id().to_uppercase()),
                "  Rettet; se login.rs ",
                5,
            )
            .unwrap();
        assert_eq!(m.saves(), saves + 1);
        assert_eq!(r.state, S::Review);
        assert_eq!(r.summary.as_deref(), Some("Rettet; se login.rs"));
        assert_eq!(r.issue, None);
        let last = r.history.last().unwrap();
        assert_eq!(
            (last.from, last.to, last.by, last.note.as_deref()),
            (
                Some(S::InProgress),
                S::Review,
                TicketActor::Agent,
                Some("Rettet; se login.rs")
            )
        );
        assert_eq!(s.current_for_agent("a1"), None);
        assert_eq!(
            TicketSummary::from(&r).summary.as_deref(),
            Some("Rettet; se login.rs")
        );
        // skipReview → done.
        let t2 = in_progress(&mut s, "a1", "quick", true);
        let d = s.submit_by_agent("a1", None, "ok", 6).unwrap();
        assert_eq!((d.id, d.state), (t2.id, S::Done));
        assert_invariants(&s);
    }

    #[test]
    fn mark_not_submitted_without_a_ticket_saves_nothing() {
        let (mut s, m) = svc();
        mk(&mut s, "x", 1);
        let saves = m.saves();
        assert_eq!(s.mark_not_submitted("a1", 2).unwrap(), None);
        assert_eq!(s.note_by_agent("a1", "hej", 2).unwrap(), None);
        assert_eq!(m.saves(), saves);
    }

    #[test]
    fn create_by_agent_is_marked_as_agent_work() {
        let (mut s, _) = svc();
        let t = s
            .create_by_agent("Følg op", "detaljer", true, None, 9)
            .unwrap();
        assert_eq!(t.source, TicketSource::Agent);
        assert_eq!((t.state, t.skip_review), (S::Backlog, true));
        assert_eq!(t.assignee_agent_id, None);
        assert_eq!(t.history.len(), 1);
        assert_eq!(t.history[0].by, TicketActor::Agent);
        assert_eq!(
            s.create_by_agent(" ", "", false, None, 9)
                .unwrap_err()
                .to_string(),
            "Titel må ikke være tom"
        );
        // The user's tickets keep source/by user.
        let u = mk(&mut s, "u", 10);
        assert_eq!(
            (u.source, u.history[0].by),
            (TicketSource::User, TicketActor::User)
        );
    }

    #[test]
    fn note_by_agent_adds_a_history_entry_on_the_current_ticket() {
        let (mut s, _) = svc();
        let t = in_progress(&mut s, "a1", "x", false);
        let n = s.note_by_agent("a1", "Kører tests", 7).unwrap().unwrap();
        assert_eq!(n.id, t.id);
        assert_eq!(n.state, S::InProgress);
        assert_eq!(n.updated_at, 7);
        let last = n.history.last().unwrap();
        assert_eq!(
            (last.from, last.to, last.by, last.note.as_deref()),
            (
                Some(S::InProgress),
                S::InProgress,
                TicketActor::Agent,
                Some("Kører tests")
            )
        );
    }

    #[test]
    fn list_for_agent_backlog_and_any_id_lookup() {
        let (mut s, _) = svc();
        let q1 = mk(&mut s, "q1", 1);
        let q2 = mk(&mut s, "q2", 2);
        let cur = mk(&mut s, "cur", 3);
        let other = mk(&mut s, "other", 4);
        let free = mk(&mut s, "free", 5);
        s.assign(&cur.id, "a1", 6).unwrap();
        s.mark_dispatched(&cur.id, "a1", 7).unwrap();
        s.assign(&q2.id, "a1", 8).unwrap();
        s.assign(&q1.id, "a1", 9).unwrap();
        s.assign(&other.id, "a2", 10).unwrap();
        let mine: Vec<String> = s
            .list_for_agent("a1")
            .into_iter()
            .map(|t| t.title)
            .collect();
        assert_eq!(mine, vec!["cur", "q2", "q1"]);
        let backlog: Vec<String> = s.backlog().into_iter().map(|t| t.title).collect();
        assert_eq!(backlog, vec!["free"]);
        assert!(s.list_for_agent("nobody").is_empty());

        assert_eq!(s.get_by_any_id(&free.id).unwrap().id, free.id);
        assert_eq!(
            s.get_by_any_id(&free.id.to_uppercase()).unwrap().id,
            free.id
        );
        assert_eq!(
            s.get_by_any_id(&format!(" {} ", free.short_id()))
                .unwrap()
                .id,
            free.id
        );
        assert_eq!(
            s.get_by_any_id(&free.short_id().to_uppercase()).unwrap().id,
            free.id
        );
        assert_eq!(s.get_by_any_id("nope"), None);
        assert_eq!(s.get_by_any_id(""), None);
    }

    // ---- step 5: review routing, agent review/coordination, report metadata ----

    /// A ticket submitted by `agent` (in review).
    fn in_review(s: &mut TicketService, agent: &str, title: &str) -> Ticket {
        let t = mk(s, title, 1);
        s.assign(&t.id, agent, 2).unwrap();
        s.mark_dispatched(&t.id, agent, 3).unwrap();
        s.submit_by_agent(agent, None, "klar", 4).unwrap()
    }

    fn report(id: &str) -> TicketReport {
        TicketReport {
            id: id.into(),
            title: "R".into(),
            author: crate::tickets::model::ReportAuthor::user(),
            created_at: 1,
            path: format!("reports/{id}-r.md"),
            size: 1,
        }
    }

    #[test]
    fn route_review_creates_assignment_and_history() {
        let (mut s, _) = svc();
        let t = in_review(&mut s, "a1", "x");
        assert_eq!(s.unrouted_reviews().len(), 1);
        assert_eq!(
            s.route_review(&t.id, "a1", "bot-a1", 5).unwrap_err(),
            TicketError::SenderCannotReview
        );
        let r = s.route_review(&t.id, "rev", "bot-rev", 5).unwrap().unwrap();
        assert_eq!(r.reviewer_agent_id.as_deref(), Some("rev"));
        let last = r.history.last().unwrap();
        assert_eq!(
            (last.from, last.to, last.by, last.note.as_deref()),
            (
                Some(S::Review),
                S::Review,
                TicketActor::System,
                Some("review tildelt bot-rev")
            )
        );
        let a = s.assignment_for_ticket(&t.id).unwrap();
        assert_eq!(
            (
                a.reviewer_agent_id.as_str(),
                a.round,
                a.assigned_at,
                a.delivered_at,
                a.attempts
            ),
            ("rev", 0, 5, None, 0)
        );
        assert_eq!(s.open_review_counts().get("rev"), Some(&1));
        assert!(s.unrouted_reviews().is_empty());
        // Idempotent: a second routing does nothing.
        assert_eq!(s.route_review(&t.id, "rev2", "x", 6).unwrap(), None);
        assert_eq!(s.review_assignments().len(), 1);
        // Delivery bookkeeping.
        assert_eq!(s.next_review_for("rev").unwrap().0.ticket_id, t.id);
        let d = s.mark_review_delivered(&t.id, "bot-rev", 7).unwrap();
        assert_eq!(
            d.history.last().unwrap().note.as_deref(),
            Some("review sendt til bot-rev")
        );
        assert!(s.next_review_for("rev").is_none());
        // Leaving review (user moves it back to in progress) drops the assignment.
        s.set_state(&t.id, S::InProgress, None, true, 8).unwrap();
        assert!(s.review_assignments().is_empty());
        assert_eq!(s.get(&t.id).unwrap().reviewer_agent_id, None);
        assert_invariants(&s);
    }

    #[test]
    fn review_delivery_attempts_end_with_a_note() {
        let (mut s, _) = svc();
        let t = in_review(&mut s, "a1", "x");
        s.route_review(&t.id, "rev", "r", 5).unwrap();
        assert_eq!(s.review_delivery_failed(&t.id, 6), Ok(1));
        assert_eq!(s.review_delivery_failed(&t.id, 7), Ok(2));
        assert!(s.next_review_for("rev").is_some());
        assert_eq!(s.review_delivery_failed(&t.id, 8), Ok(3));
        assert!(
            s.next_review_for("rev").is_none(),
            "no more automatic tries"
        );
        assert_eq!(
            s.get(&t.id)
                .unwrap()
                .history
                .last()
                .unwrap()
                .note
                .as_deref(),
            Some(REVIEW_UNDELIVERED_NOTE)
        );
        let a = s.reset_review_delivery(&t.id).unwrap();
        assert_eq!((a.attempts, a.delivered_at), (0, None));
        assert!(s.next_review_for("rev").is_some());
    }

    #[test]
    fn approve_by_agent_rules() {
        let (mut s, _) = svc();
        let t = in_review(&mut s, "a1", "x");
        let queued = mk(&mut s, "q", 1);
        assert_eq!(
            s.approve_by_agent("rev", "r", &queued.id, None, 5),
            Err(TicketError::NotInReview)
        );
        assert_eq!(
            s.approve_by_agent("rev", "r", "nope", None, 5),
            Err(TicketError::NotFound)
        );
        assert_eq!(
            s.approve_by_agent("a1", "a", &t.id, None, 5),
            Err(TicketError::OwnSubmission)
        );
        // Not routed yet: nobody is its reviewer.
        assert_eq!(
            s.approve_by_agent("rev", "r", &t.id, None, 5),
            Err(TicketError::NotYourReview)
        );
        s.route_review(&t.id, "rev", "r", 5).unwrap();
        assert_eq!(
            s.approve_by_agent("other", "o", &t.id, None, 6),
            Err(TicketError::NotYourReview)
        );
        let d = s
            .approve_by_agent("rev", "bot-rev", &t.short_id(), Some(" tests ok "), 7)
            .unwrap();
        assert_eq!(d.state, S::Done);
        assert_eq!(d.reviewer_agent_id.as_deref(), Some("rev"));
        let last = d.history.last().unwrap();
        assert_eq!(
            (last.by, last.note.as_deref()),
            (TicketActor::Agent, Some("godkendt af bot-rev: tests ok"))
        );
        assert!(s.review_assignments().is_empty());
        assert_invariants(&s);
    }

    #[test]
    fn reject_by_agent_increments_round_and_requeues_front() {
        let (mut s, _) = svc();
        let first = mk(&mut s, "first", 1);
        let t = in_review(&mut s, "a1", "x");
        s.assign(&first.id, "a1", 5).unwrap();
        s.route_review(&t.id, "rev", "r", 5).unwrap();
        assert_eq!(
            s.reject_by_agent("rev", "r", &t.id, "  ", true, 6),
            Err(TicketError::NeedsNote)
        );
        let r = s
            .reject_by_agent("rev", "bot-rev", &t.id, "mangler test", true, 7)
            .unwrap();
        assert_eq!(
            (r.state, r.queue_position, r.review_round),
            (S::Assigned, Some(0), 1)
        );
        assert_eq!(r.rejection_note.as_deref(), Some("mangler test"));
        assert_eq!(r.reviewer_agent_id, None);
        assert!(r.history.iter().any(|h| h.by == TicketActor::Agent
            && h.note.as_deref() == Some("afvist af bot-rev: mangler test")));
        assert_eq!(
            positions(&s, "a1"),
            vec![("x".into(), Some(0)), ("first".into(), Some(1))]
        );
        assert!(s.review_assignments().is_empty());
        // A dead sender: to the backlog (round reset there).
        s.mark_dispatched(&t.id, "a1", 8).unwrap();
        s.submit_by_agent("a1", None, "igen", 9).unwrap();
        assert_eq!(s.get(&t.id).unwrap().review_round, 1);
        s.route_review(&t.id, "rev", "r", 10).unwrap();
        let b = s
            .reject_by_agent("rev", "r", &t.id, "nej", false, 11)
            .unwrap();
        assert_eq!((b.state, b.review_round), (S::Backlog, 0));
        assert_invariants(&s);
    }

    #[test]
    fn escalate_and_clear_reviewer() {
        let (mut s, _) = svc();
        let t = in_review(&mut s, "a1", "x");
        let e = s.escalate(&t.id, 5).unwrap().unwrap();
        assert!(e.escalated);
        assert_eq!(
            e.history.last().unwrap().note.as_deref(),
            Some("eskaleret efter 3 runder")
        );
        assert_eq!(s.escalate(&t.id, 6).unwrap(), None, "idempotent");
        assert!(s.unrouted_reviews().is_empty(), "escalated: no routing");
        assert_eq!(s.escalated_count(), 1);
        // The user picks a reviewer anyway.
        let r = s.set_reviewer(&t.id, "rev", "r", 7).unwrap();
        assert!(!r.escalated);
        assert_eq!(s.review_assignments().len(), 1);
        assert_eq!(
            s.set_reviewer(&t.id, "a1", "a", 7),
            Err(TicketError::SenderCannotReview)
        );
        let c = s
            .clear_reviewer(&t.id, REVIEWER_REMOVED_NOTE, TicketActor::User, 8)
            .unwrap()
            .unwrap();
        assert_eq!((c.reviewer_agent_id, c.escalated), (None, false));
        assert!(s.review_assignments().is_empty());
        assert_eq!(
            s.clear_reviewer(&t.id, REVIEWER_REMOVED_NOTE, TicketActor::User, 9),
            Ok(None)
        );
    }

    /// review5 N5: removing the hand-picked reviewer of a ticket that used up its rounds keeps
    /// it escalated (no routing, no second escalation note); an escalated ticket without a
    /// reviewer has nothing to remove.
    #[test]
    fn clear_reviewer_keeps_escalation_after_max_rounds() {
        let (mut s, _) = svc();
        let t = in_review(&mut s, "a1", "x");
        s.commit(|doc| {
            find_mut(doc, &t.id)?.review_round = MAX_REVIEW_ROUNDS;
            Ok(())
        })
        .unwrap();
        s.escalate(&t.id, 5).unwrap().unwrap();
        assert_eq!(
            s.clear_reviewer(&t.id, REVIEWER_REMOVED_NOTE, TicketActor::User, 6),
            Ok(None)
        );
        let r = s.set_reviewer(&t.id, "rev", "r", 7).unwrap();
        assert_eq!((r.escalated, r.review_round), (false, MAX_REVIEW_ROUNDS));
        let c = s
            .clear_reviewer(&t.id, REVIEWER_REMOVED_NOTE, TicketActor::User, 8)
            .unwrap()
            .unwrap();
        assert_eq!(
            (c.reviewer_agent_id.as_deref(), c.escalated, c.review_round),
            (None, true, MAX_REVIEW_ROUNDS)
        );
        assert!(s.review_assignments().is_empty());
        assert!(s.unrouted_reviews().is_empty(), "escalated: no routing");
        assert_eq!(
            c.history.last().unwrap().note.as_deref(),
            Some(REVIEWER_REMOVED_NOTE)
        );
        assert_eq!(s.escalate(&t.id, 9).unwrap(), None, "no second escalation");
    }

    #[test]
    fn release_reviewer_keeps_tickets_in_review() {
        let (mut s, _) = svc();
        let t1 = in_review(&mut s, "a1", "x");
        let t2 = in_review(&mut s, "a2", "y");
        s.route_review(&t1.id, "rev", "r", 5).unwrap();
        s.route_review(&t2.id, "rev", "r", 5).unwrap();
        let released = s.release_reviewer("rev", "agent stoppet", 6).unwrap();
        assert_eq!(released.len(), 2);
        for id in [&t1.id, &t2.id] {
            let t = s.get(id).unwrap();
            assert_eq!((t.state, t.reviewer_agent_id.clone()), (S::Review, None));
            assert_eq!(
                t.history.last().unwrap().note.as_deref(),
                Some("reviewer agent stoppet")
            );
        }
        assert!(s.review_assignments().is_empty());
        assert_eq!(s.unrouted_reviews().len(), 2);
        assert_eq!(
            s.release_reviewer("rev", "x", 7).unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn assign_by_agent_requires_backlog_or_rejected() {
        let (mut s, _) = svc();
        let b = mk(&mut s, "b", 1);
        let t = s.assign_by_agent(&b.short_id(), "a1", "koord", 2).unwrap();
        assert_eq!(
            (t.state, t.assignee_agent_id.as_deref()),
            (S::Assigned, Some("a1"))
        );
        let last = t.history.last().unwrap();
        assert_eq!(
            (last.by, last.note.as_deref()),
            (TicketActor::Agent, Some("tildelt af koordinator koord"))
        );
        let err = s.assign_by_agent(&b.id, "a2", "k", 3).unwrap_err();
        assert!(
            matches!(err, TicketError::IllegalTransition { .. }),
            "{err:?}"
        );
        let u = s.unassign_by_agent(&b.id, "k", 4).unwrap();
        assert_eq!(u.state, S::Backlog);
        let p = in_review(&mut s, "a1", "p");
        assert!(s.assign_by_agent(&p.id, "a2", "k", 5).is_err());
        assert_eq!(
            s.assign_by_agent("nope", "a2", "k", 5),
            Err(TicketError::NotFound)
        );
        assert_invariants(&s);
    }

    #[test]
    fn create_by_agent_with_assign_to_queues_last() {
        let (mut s, m) = svc();
        let q = mk(&mut s, "q", 1);
        s.assign(&q.id, "a1", 2).unwrap();
        let saves = m.saves();
        let t = s
            .create_by_agent("Ny", "", false, Some(("a1", "koord")), 3)
            .unwrap();
        assert_eq!(m.saves(), saves + 1, "one save");
        assert_eq!((t.state, t.queue_position), (S::Assigned, Some(1)));
        assert_eq!(t.source, TicketSource::Agent);
        assert_eq!(t.history.len(), 2);
        assert_eq!(
            t.history[1].note.as_deref(),
            Some("tildelt af koordinator koord")
        );
        assert_invariants(&s);
    }

    #[test]
    fn add_report_meta_limit_20() {
        let (mut s, _) = svc();
        let t = mk(&mut s, "x", 1);
        assert_eq!(s.next_report_seq(&t.id), Ok(1));
        for i in 1..=REPORTS_PER_TICKET_MAX {
            let id = format!("{i:02}");
            s.add_report_meta(&t.id, report(&id), 2).unwrap();
        }
        assert_eq!(s.next_report_seq(&t.id), Err(TicketError::TooManyReports));
        assert_eq!(
            s.add_report_meta(&t.id, report("21"), 3),
            Err(TicketError::TooManyReports)
        );
        assert_eq!(s.get(&t.id).unwrap().reports.len(), 20);
        assert_eq!(s.report_count(), 20);
        assert_eq!(s.list()[0].report_count, 20);
        assert_eq!(
            s.add_report_meta("nope", report("01"), 3),
            Err(TicketError::NotFound)
        );
    }

    #[test]
    fn delete_ticket_in_review_with_assignment_refused() {
        let (mut s, _) = svc();
        let t = in_review(&mut s, "a1", "x");
        s.route_review(&t.id, "rev", "r", 5).unwrap();
        assert_eq!(s.delete(&t.id), Err(TicketError::NotDeletable));
        assert_eq!(s.review_assignments().len(), 1);
        s.approve(&t.id, 6).unwrap();
        assert!(s.review_assignments().is_empty());
        s.delete(&t.id).unwrap();
        assert!(s.get(&t.id).is_none());
    }

    #[test]
    fn restart_clears_reviewers_and_assignments() {
        let (mut s, m) = svc();
        let t = in_review(&mut s, "a1", "x");
        s.route_review(&t.id, "rev", "r", 5).unwrap();
        let doc = m.doc().unwrap();
        assert_eq!(doc.review_assignments.len(), 1);
        let m2 = MemoryStore::with_doc(doc);
        let (r, _) = TicketService::load_and_recover(Box::new(m2.clone()), 100);
        assert!(r.review_assignments().is_empty());
        let t2 = r.get(&t.id).unwrap();
        assert_eq!((t2.state, t2.reviewer_agent_id), (S::Review, None));
        assert_eq!(r.unrouted_reviews().len(), 1);
        assert_eq!(m2.saves(), 1);
    }

    #[test]
    fn handoff_moves_the_ticket_in_progress_to_the_end_of_the_target_queue() {
        let (mut s, m) = svc();
        let t = in_progress(&mut s, "k", "Lav siden", false);
        // The coordinator also has a queue; the target already has one ticket queued.
        let k2 = mk(&mut s, "k2", 3);
        s.assign(&k2.id, "k", 3).unwrap();
        let w1 = mk(&mut s, "w1", 3);
        s.assign(&w1.id, "w", 3).unwrap();
        s.set_issue(&t.id, Some(TicketIssue::NotSubmitted), None, 4)
            .unwrap();

        let saves = m.saves();
        let h = s
            .handoff(&t.short_id(), "w", Some("k"), ("Koord", "Koder"), 5)
            .unwrap();
        assert_eq!(m.saves(), saves + 1, "one save");
        assert_eq!(h.state, S::Assigned);
        assert_eq!(h.assignee_agent_id.as_deref(), Some("w"));
        assert_eq!(h.queue_position, Some(1), "last in the target's queue");
        assert_eq!(h.issue, None);
        let last = h.history.last().unwrap();
        assert_eq!(
            (last.by, last.note.as_deref()),
            (TicketActor::Agent, Some("overdraget fra Koord til Koder"))
        );
        // The coordinator has nothing in progress: its queue head is next, and a Stop now has
        // nothing to mark "ikke afleveret".
        assert_eq!(s.current_for_agent("k"), None);
        assert_eq!(s.next_for_agent("k").map(|t| t.id), Some(k2.id.clone()));
        assert_eq!(s.mark_not_submitted("k", 6), Ok(None));
        assert_eq!(s.complete_turn("k", 6), Ok(None));
        assert_eq!(s.links().get("k"), Some(&(None, 1)));
        assert_eq!(s.links().get("w"), Some(&(None, 2)));
        // The target gets it after its own queue.
        assert_eq!(s.next_for_agent("w").map(|t| t.id), Some(w1.id.clone()));
        assert_invariants(&s);
    }

    #[test]
    fn handoff_rules_by_actor_and_state() {
        let (mut s, m) = svc();
        let t = in_progress(&mut s, "a1", "x", false);
        let saves = m.saves();
        // Another agent may not hand someone else's ticket on.
        assert_eq!(
            s.handoff(&t.id, "a3", Some("a2"), ("a", "c"), 4),
            Err(TicketError::NotYours)
        );
        assert_eq!(
            s.give_back(&t.id, Some("a2"), 4),
            Err(TicketError::NotYours)
        );
        assert_eq!(
            s.unassign_by_agent(&t.id, "a2", 4),
            Err(TicketError::NotYours)
        );
        // Not to itself.
        assert_eq!(
            s.handoff(&t.id, "a1", Some("a1"), ("a", "a"), 4),
            Err(TicketError::HandoffToSelf)
        );
        assert_eq!(
            s.handoff("nope", "a2", None, ("a", "b"), 4),
            Err(TicketError::NotFound)
        );
        assert_eq!(m.saves(), saves, "refusals save nothing");
        // Review and done cannot be handed on.
        let r = in_review(&mut s, "a3", "r");
        assert_eq!(
            s.handoff(&r.id, "a2", Some("a3"), ("a", "b"), 5),
            Err(TicketError::NotInProgress)
        );
        assert_eq!(
            s.handoff(&r.id, "a2", None, ("a", "b"), 5),
            Err(TicketError::NotInProgress)
        );
        // The user may hand on any ticket in progress.
        let u = s.handoff(&t.id, "a2", None, ("a", "b"), 6).unwrap();
        assert_eq!(
            (u.state, u.assignee_agent_id.as_deref()),
            (S::Assigned, Some("a2"))
        );
        assert_eq!(u.history.last().unwrap().by, TicketActor::User);
        assert_invariants(&s);
    }

    #[test]
    fn give_back_and_unassign_by_agent_of_the_ticket_in_progress() {
        let (mut s, _) = svc();
        let t = in_progress(&mut s, "k", "x", false);
        let b = s.unassign_by_agent(&t.short_id(), "k", 4).unwrap();
        assert_eq!((b.state, b.assignee_agent_id), (S::Backlog, None));
        assert_eq!(
            b.history.last().unwrap().note.as_deref(),
            Some(crate::tickets::state::RETURNED_NOTE)
        );
        assert_eq!(s.current_for_agent("k"), None);
        // The user puts a ticket in progress back (Fjern tildeling).
        let t2 = in_progress(&mut s, "k", "y", false);
        let u = s.unassign(&t2.id, 5).unwrap();
        assert_eq!(u.state, S::Backlog);
        assert_eq!(u.history.last().unwrap().by, TicketActor::User);
        // give_back needs a ticket in progress.
        assert_eq!(
            s.give_back(&t2.id, None, 6),
            Err(TicketError::NotInProgress)
        );
        assert_invariants(&s);
    }
}
