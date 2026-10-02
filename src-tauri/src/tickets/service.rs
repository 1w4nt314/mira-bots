//! [`TicketService`]: owns the ticket document in memory, applies every mutation through the state
//! machine, keeps the queue invariants and saves synchronously after each mutation.
//!
//! Invariants (checked by `normalize_queues` / the mutations):
//! - a ticket is in at most one queue (`assignee_agent_id` while `assigned`);
//! - per agent the `assigned` tickets have positions `0..n` without gaps; nothing else has one;
//! - at most one `inProgress` ticket per agent;
//! - a `waiting` ticket (step 6a) has an assignee and no queue position.
//!
//! The service is meant to sit behind a `std::sync::Mutex`; it never blocks on anything but the
//! store's small synchronous write.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io;

use super::model::{
    short_id, ReviewAssignment, Ticket, TicketActor, TicketDoc, TicketError, TicketHistoryEntry,
    TicketId, TicketIssue, TicketPatch, TicketReport, TicketSource, TicketState, TicketSummary,
};
use super::prompt::{one_line, ChildLine, ChildReview};
use super::state::{transition_noted, TicketEvent, REOPENED_NOTE};
use super::store::TicketStore;
use super::NOT_SUBMITTED_NOTE;
use crate::config::{
    BLOCKED_BY_MAX, CHILDREN_DONE_NOTE, MAX_REVIEW_ROUNDS, MOVED_NOTE, PARENT_DELETED_NOTE,
    REPORTS_PER_TICKET_MAX, RESTART_NOTE, REVIEW_DELIVERY_MAX_ATTEMPTS, TICKET_BODY_MAX_CHARS,
    TICKET_SUMMARY_MAX_CHARS, TICKET_TITLE_MAX_CHARS, WAITING_NOTE,
};
use crate::projects::{same_id, validate_project_id, ProjectId, ProjectRef};

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

/// Where a rejected ticket goes (plan5 C.9, W1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectReturn {
    /// First in the sender's queue: the sender is live and may still take the ticket.
    Sender,
    /// The backlog (the sender is gone): a fresh start, review round 0.
    Backlog,
    /// The backlog with [`MOVED_NOTE`]: the sender is live but now stands in another project.
    /// The rejection note and the review round stay.
    Moved,
}

impl From<bool> for RejectReturn {
    /// `sender_live` without a project check.
    fn from(sender_live: bool) -> Self {
        if sender_live {
            Self::Sender
        } else {
            Self::Backlog
        }
    }
}

/// Per reviewer agent: its open review assignments (agents without any are absent).
pub type ReviewCounts = HashMap<String, usize>;

/// Per agent: the `inProgress` ticket (if any) and the number of queued (`assigned`) tickets.
/// Agents without any ticket are absent.
pub type TicketLinks = HashMap<String, (Option<TicketId>, usize)>;

/// How deep the relation walks go (parents, blockers); deeper chains are cut off (step 6a).
const RELATION_DEPTH_MAX: usize = 64;

/// A waiting parent whose assignee is due a wake line (step 6a, plan A.2): children became Done
/// since the parent was last in progress, or no child is open any more.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DueWake {
    pub parent: Ticket,
    /// Children that became Done after the parent was last in progress, in Done order.
    pub newly_done: Vec<Ticket>,
    /// Children still open (not Done).
    pub open_left: usize,
}

/// The relation-relevant part of one ticket (step 6a): compared before/after a mutation to find
/// whom to wake or unblock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelSnap {
    pub id: TicketId,
    pub state: TicketState,
    pub parent_id: Option<TicketId>,
    pub blocked_by: Vec<TicketId>,
    pub assignee: Option<String>,
}

/// What a mutation changed in the relations (step 6a, plan A.5): computed by
/// [`relation_effects`] from two [`RelSnap`] lists, acted on by `TicketsCtx::mutate_if` after the
/// lock is released.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RelationEffects {
    /// Assignees of waiting parents that lost an open child (it became Done or was deleted).
    pub wake: BTreeSet<String>,
    /// Assignees of queued tickets that were blocked before and are not any more.
    pub unblocked: BTreeSet<String>,
    /// Backlog parents without an assignee whose last open child went away.
    pub children_done: Vec<TicketId>,
    /// `(parent, assignee)` behind [`Self::wake`] (for the log; Batch 2 addition to C6.1).
    pub woken: Vec<(TicketId, String)>,
    /// `(ticket, assignee)` behind [`Self::unblocked`] (for the log; Batch 2 addition to C6.1).
    pub freed: Vec<(TicketId, String)>,
}

impl RelationEffects {
    /// Nothing to do.
    pub fn is_empty(&self) -> bool {
        self.wake.is_empty() && self.unblocked.is_empty() && self.children_done.is_empty()
    }
}

/// The relation effects of a mutation (plan A.5; pure). `before`/`after` are
/// [`TicketService::relations_snapshot`]s taken around it:
/// - `wake`: the assignee of a ticket waiting after the mutation, when one of its children was
///   open before and is Done or gone after (or its open-children count went from > 0 to 0);
/// - `unblocked`: the assignee of a queued (`assigned`) ticket that was blocked before and is
///   not after (its blocker became Done or was deleted);
/// - `children_done`: a backlog ticket without an assignee whose open-children count went from
///   > 0 to 0.
///
/// A child that is rejected, put back in the backlog or reopened stays open: no effect.
pub fn relation_effects(before: &[RelSnap], after: &[RelSnap]) -> RelationEffects {
    let state_b: HashMap<&str, TicketState> =
        before.iter().map(|s| (s.id.as_str(), s.state)).collect();
    let state_a: HashMap<&str, TicketState> =
        after.iter().map(|s| (s.id.as_str(), s.state)).collect();
    let open_of = |snaps: &[RelSnap], id: &str| {
        snaps
            .iter()
            .filter(|s| s.parent_id.as_deref() == Some(id) && s.state != TicketState::Done)
            .count()
    };
    let blocked = |states: &HashMap<&str, TicketState>, s: &RelSnap| {
        s.blocked_by.iter().any(|b| {
            states
                .get(b.as_str())
                .is_some_and(|st| *st != TicketState::Done)
        })
    };
    let mut fx = RelationEffects::default();
    for a in after {
        match (a.state, a.assignee.as_deref()) {
            (TicketState::Waiting, Some(agent)) => {
                let lost_child = before.iter().any(|c| {
                    c.parent_id.as_deref() == Some(a.id.as_str())
                        && c.state != TicketState::Done
                        && after.iter().find(|x| x.id == c.id).is_none_or(|x| {
                            x.state == TicketState::Done
                                || x.parent_id.as_deref() != Some(a.id.as_str())
                        })
                });
                let emptied = open_of(before, &a.id) > 0 && open_of(after, &a.id) == 0;
                if lost_child || emptied {
                    fx.wake.insert(agent.to_string());
                    fx.woken.push((a.id.clone(), agent.to_string()));
                }
            }
            (TicketState::Assigned, Some(agent)) => {
                let was_blocked = before
                    .iter()
                    .find(|b| b.id == a.id)
                    .is_some_and(|b| blocked(&state_b, b));
                if was_blocked && !blocked(&state_a, a) {
                    fx.unblocked.insert(agent.to_string());
                    fx.freed.push((a.id.clone(), agent.to_string()));
                }
            }
            (TicketState::Backlog, None) => {
                if open_of(before, &a.id) > 0 && open_of(after, &a.id) == 0 {
                    fx.children_done.push(a.id.clone());
                }
            }
            _ => {}
        }
    }
    fx
}

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

fn find<'a>(doc: &'a TicketDoc, id: &str) -> Option<&'a Ticket> {
    doc.tickets.iter().find(|t| t.id == id)
}

// ---- relations (step 6a): pure helpers over the document ----

/// The children of `id` (tickets whose `parent_id` is `id`), oldest first.
pub(crate) fn children_of<'a>(doc: &'a TicketDoc, id: &str) -> Vec<&'a Ticket> {
    let mut v: Vec<&Ticket> = doc
        .tickets
        .iter()
        .filter(|t| t.parent_id.as_deref() == Some(id))
        .collect();
    v.sort_by_key(|t| t.created_at);
    v
}

/// The children of `id` that are not Done (a deleted child no longer exists and is not open).
pub(crate) fn open_children<'a>(doc: &'a TicketDoc, id: &str) -> Vec<&'a Ticket> {
    children_of(doc, id)
        .into_iter()
        .filter(|t| t.state != TicketState::Done)
        .collect()
}

/// Whether `t` waits for a blocker: an id in `blocked_by` exists and is not Done (a missing id
/// is a deleted ticket and does not block).
pub(crate) fn is_blocked(doc: &TicketDoc, t: &Ticket) -> bool {
    t.blocked_by
        .iter()
        .any(|b| find(doc, b).is_some_and(|x| x.state != TicketState::Done))
}

/// The short ids of `t`'s open blockers, in `blocked_by` order.
pub(crate) fn open_blockers(doc: &TicketDoc, t: &Ticket) -> Vec<String> {
    t.blocked_by
        .iter()
        .filter_map(|b| find(doc, b))
        .filter(|x| x.state != TicketState::Done)
        .map(Ticket::short_id)
        .collect()
}

/// The parent, grandparent, … of `id` (existing tickets only), nearest first; at most
/// [`RELATION_DEPTH_MAX`] and stopping at a repeat (a hand-edited cycle).
pub(crate) fn ancestors(doc: &TicketDoc, id: &str) -> Vec<TicketId> {
    let mut out: Vec<TicketId> = Vec::new();
    let mut cur = find(doc, id).and_then(|t| t.parent_id.clone());
    while let Some(p) = cur {
        if out.len() >= RELATION_DEPTH_MAX || p == id || out.contains(&p) {
            break;
        }
        let Some(pt) = find(doc, &p) else {
            break;
        };
        cur = pt.parent_id.clone();
        out.push(p);
    }
    out
}

/// Whether making `parent` the parent of `child` would form a cycle.
pub(crate) fn would_cycle(doc: &TicketDoc, child: &str, parent: &str) -> bool {
    parent == child || ancestors(doc, parent).iter().any(|a| a == child)
}

/// Whether `child` blocked by `blockers` would form a cycle: `child` is reachable from a blocker
/// over `blocked_by` (depth-first, at most [`RELATION_DEPTH_MAX`] deep).
pub(crate) fn would_block_cycle(doc: &TicketDoc, child: &str, blockers: &[TicketId]) -> bool {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut stack: Vec<(&str, usize)> = blockers.iter().map(|b| (b.as_str(), 0)).collect();
    while let Some((id, depth)) = stack.pop() {
        if id == child {
            return true;
        }
        if depth >= RELATION_DEPTH_MAX || !seen.insert(id) {
            continue;
        }
        if let Some(t) = find(doc, id) {
            stack.extend(t.blocked_by.iter().map(|b| (b.as_str(), depth + 1)));
        }
    }
    false
}

/// The time `t` last entered `state` (its last history entry with `to == state`).
fn entered_at(t: &Ticket, state: TicketState) -> Option<u64> {
    t.history.iter().rev().find(|h| h.to == state).map(|h| h.at)
}

/// Normalised title for the duplicate check (plan A.8): one line, whitespace runs collapsed,
/// lowercase (Unicode).
pub(crate) fn norm_title(s: &str) -> String {
    one_line(s)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The submit decision (plan A.1): a ticket with open children goes to `waiting` (also with
/// `skip_review`), else it is submitted (review, or done with `skip_review`). A ticket already
/// waiting that still has open children stays waiting (one history entry).
pub(crate) fn wait_or_submit(
    doc: &mut TicketDoc,
    id: &str,
    by: TicketActor,
    summary_note: Option<String>,
    now: u64,
) -> Result<Ticket, TicketError> {
    let n = open_children(doc, id).len();
    if n == 0 {
        return apply(doc, id, &TicketEvent::Submit, by, summary_note, now);
    }
    let note = format!("{WAITING_NOTE} ({n})");
    let t = find_mut(doc, id)?;
    if t.state == TicketState::Waiting {
        note_entry(t, by, note, now);
        return Ok(t.clone());
    }
    apply(doc, id, &TicketEvent::Wait, by, Some(note), now)
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

/// The project name of `r` checked against the folder-name rules (plan4b C4b.1).
fn validate_project_ref(r: Option<ProjectRef>) -> Result<Option<ProjectRef>, TicketError> {
    if let Some(r) = &r {
        validate_project_id(r.name())?;
    }
    Ok(r)
}

/// History note of a project change.
fn project_note(project: Option<&ProjectRef>) -> String {
    match project {
        Some(ProjectRef::Existing(id)) => format!("projekt: «{id}»"),
        Some(ProjectRef::New { new }) => format!("projekt: «{new}» (oprettes ved tildeling)"),
        None => "projekt fjernet".to_string(),
    }
}

/// Sets the ticket's project with a history note; no-op when it is unchanged.
fn put_project(t: &mut Ticket, project: Option<ProjectRef>, by: TicketActor, now: u64) {
    if t.project == project {
        return;
    }
    note_entry(t, by, project_note(project.as_ref()), now);
    t.project = project;
}

/// The project may only change while the ticket waits in the backlog (or was rejected and has
/// no agent).
fn project_changeable(t: &Ticket) -> bool {
    match t.state {
        TicketState::Backlog => true,
        TicketState::Rejected => t.assignee_agent_id.is_none(),
        _ => false,
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

    /// Loads the store and moves `assigned`/`inProgress`/`waiting` tickets (whose agents no
    /// longer exist after a restart) to the backlog with [`RESTART_NOTE`]. Saves only if something changed.
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
            .filter(|t| {
                matches!(
                    t.state,
                    TicketState::Assigned | TicketState::InProgress | TicketState::Waiting
                )
            })
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

    /// The agent's own tickets ("mine"): the one in progress first, then its waiting parents
    /// (step 6a, oldest change first), then its queue in order.
    pub fn list_for_agent(&self, agent_id: &str) -> Vec<TicketSummary> {
        let mut v: Vec<&Ticket> = self
            .doc
            .tickets
            .iter()
            .filter(|t| {
                t.assignee_agent_id.as_deref() == Some(agent_id)
                    && matches!(
                        t.state,
                        TicketState::Assigned | TicketState::InProgress | TicketState::Waiting
                    )
            })
            .collect();
        v.sort_by_key(|t| {
            let rank = match t.state {
                TicketState::InProgress => 0,
                TicketState::Waiting => 1,
                _ => 2,
            };
            (rank, t.queue_position, t.updated_at)
        });
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

    /// The ticket to deliver next: `None` while the agent has one in progress, else the first
    /// queued ticket that is not blocked (step 6a; a blocked ticket keeps its position).
    pub fn next_for_agent(&self, agent_id: &str) -> Option<Ticket> {
        if in_progress_of(&self.doc, agent_id).is_some() {
            return None;
        }
        self.doc
            .tickets
            .iter()
            .filter(|t| {
                t.state == TicketState::Assigned
                    && t.assignee_agent_id.as_deref() == Some(agent_id)
                    && !is_blocked(&self.doc, t)
            })
            .min_by_key(|t| t.queue_position)
            .cloned()
    }

    // ---- relations (step 6a) ----

    /// Children of `id` that are not Done.
    pub fn open_children_count(&self, id: &str) -> usize {
        open_children(&self.doc, id).len()
    }

    /// The open blockers of ticket `id` as short ids (`"a1b2c3d4, e5f6a7b8"`); `None` when it is
    /// not blocked or does not exist.
    pub fn blocked_text(&self, id: &str) -> Option<String> {
        let t = find(&self.doc, id)?;
        let b = open_blockers(&self.doc, t);
        (!b.is_empty()).then(|| b.join(", "))
    }

    /// The children of `id`, oldest first (step 6a).
    pub fn children(&self, id: &str) -> Vec<Ticket> {
        children_of(&self.doc, id).into_iter().cloned().collect()
    }

    /// The children of `id` for a ticket file's `## Del-tickets` (step 6a, C6.3): short id, raw
    /// title, state and open blockers (short ids). Oldest first.
    pub fn child_lines(&self, id: &str) -> Vec<ChildLine> {
        children_of(&self.doc, id)
            .into_iter()
            .map(|c| ChildLine {
                short: c.short_id(),
                title: c.title.clone(),
                state: c.state,
                open_blockers: open_blockers(&self.doc, c),
            })
            .collect()
    }

    /// The children of `id` for a parent's review file (step 6a, C6.3): short id, raw title,
    /// state and summary. Oldest first.
    pub fn child_reviews(&self, id: &str) -> Vec<ChildReview> {
        children_of(&self.doc, id)
            .into_iter()
            .map(|c| ChildReview {
                short: c.short_id(),
                title: c.title.clone(),
                state: c.state,
                summary: c.summary.clone(),
            })
            .collect()
    }

    /// The relation-relevant part of every ticket (for the `mutate_if` hook, plan A.5).
    pub fn relations_snapshot(&self) -> Vec<RelSnap> {
        self.doc
            .tickets
            .iter()
            .map(|t| RelSnap {
                id: t.id.clone(),
                state: t.state,
                parent_id: t.parent_id.clone(),
                blocked_by: t.blocked_by.clone(),
                assignee: t.assignee_agent_id.clone(),
            })
            .collect()
    }

    /// Open (not Done) tickets whose [`norm_title`] is `norm`, in the same project as `project`
    /// (both without one, or the same name per `projects::same_id`); oldest first.
    pub fn find_open_by_title(&self, norm: &str, project: Option<&ProjectRef>) -> Vec<Ticket> {
        let mut v: Vec<Ticket> = self
            .doc
            .tickets
            .iter()
            .filter(|t| t.state != TicketState::Done && norm_title(&t.title) == norm)
            .filter(|t| match (t.project.as_ref(), project) {
                (None, None) => true,
                (Some(a), Some(b)) => same_id(a.name(), b.name()),
                _ => false,
            })
            .cloned()
            .collect();
        v.sort_by_key(|t| t.created_at);
        v
    }

    /// The waiting parent of `agent_id` that is due a wake line (plan A.2): a child became Done
    /// after the parent last entered `inProgress` (fallback: its creation; strictly later), or
    /// it has no open child left. Of several, the one changed longest ago.
    pub fn due_wake_for(&self, agent_id: &str) -> Option<DueWake> {
        let mut parents: Vec<&Ticket> = self
            .doc
            .tickets
            .iter()
            .filter(|t| {
                t.state == TicketState::Waiting && t.assignee_agent_id.as_deref() == Some(agent_id)
            })
            .collect();
        parents.sort_by_key(|t| (t.updated_at, t.created_at));
        parents.into_iter().find_map(|p| {
            let since = entered_at(p, TicketState::InProgress).unwrap_or(p.created_at);
            let children = children_of(&self.doc, &p.id);
            let open_left = children
                .iter()
                .filter(|c| c.state != TicketState::Done)
                .count();
            let mut newly_done: Vec<(u64, &Ticket)> = children
                .iter()
                .filter(|c| c.state == TicketState::Done)
                .map(|c| (entered_at(c, TicketState::Done).unwrap_or(c.updated_at), *c))
                .filter(|(at, _)| *at > since)
                .collect();
            if open_left > 0 && newly_done.is_empty() {
                return None;
            }
            newly_done.sort_by_key(|(at, c)| (*at, c.created_at));
            Some(DueWake {
                parent: p.clone(),
                newly_done: newly_done.into_iter().map(|(_, c)| c.clone()).collect(),
                open_left,
            })
        })
    }

    // ---- user mutations ----

    pub fn create(
        &mut self,
        title: &str,
        body: &str,
        skip_review: bool,
        now: u64,
    ) -> Result<Ticket, TicketError> {
        self.create_in(title, body, skip_review, None, now)
    }

    /// [`Self::create`] with a project (validated against the folder-name rules; a `New` name is
    /// only created when the ticket is assigned or spawned with, plan4b A.2).
    pub fn create_in(
        &mut self,
        title: &str,
        body: &str,
        skip_review: bool,
        project: Option<ProjectRef>,
        now: u64,
    ) -> Result<Ticket, TicketError> {
        self.create_with_id_source(
            &mut || uuid::Uuid::new_v4().to_string(),
            title,
            body,
            skip_review,
            project,
            (TicketSource::User, TicketActor::User),
            now,
        )
    }

    /// `create` with injectable ids (tests force a short-id collision) and origin (`source`, and
    /// the creation entry's `by`). Ids whose short id is already taken are skipped.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_with_id_source(
        &mut self,
        next_id: &mut dyn FnMut() -> String,
        title: &str,
        body: &str,
        skip_review: bool,
        project: Option<ProjectRef>,
        origin: (TicketSource, TicketActor),
        now: u64,
    ) -> Result<Ticket, TicketError> {
        let t = self.new_ticket(next_id, title, body, skip_review, project, origin, now)?;
        let id = t.id.clone();
        self.commit(|doc| {
            doc.tickets.push(t);
            Ok(())
        })?;
        self.fetch(&id)
    }

    /// A validated backlog ticket with a fresh id (not yet in the document).
    #[allow(clippy::too_many_arguments)]
    fn new_ticket(
        &self,
        next_id: &mut dyn FnMut() -> String,
        title: &str,
        body: &str,
        skip_review: bool,
        project: Option<ProjectRef>,
        (source, by): (TicketSource, TicketActor),
        now: u64,
    ) -> Result<Ticket, TicketError> {
        let title = validate_title(title)?;
        validate_body(body)?;
        let project = validate_project_ref(project)?;
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
            project,
            parent_id: None,
            blocked_by: Vec::new(),
        };
        Ok(t)
    }

    /// Title/body/skipReview in any state (same validation as `create`); the project only as
    /// [`Self::set_project`] allows (an unchanged project is accepted in any state).
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
        let project = patch.project.map(validate_project_ref).transpose()?;
        self.commit(|doc| {
            let t = find_mut(doc, id)?;
            if let Some(p) = project {
                if t.project != p {
                    if !project_changeable(t) {
                        return Err(TicketError::ProjectChangeNotAllowed);
                    }
                    put_project(t, p, TicketActor::User, now);
                }
            }
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

    /// Sets or removes the ticket's project (the user; plan4b punkt 8). Only in the backlog, or
    /// rejected without an agent ([`TicketError::ProjectChangeNotAllowed`]).
    pub fn set_project(
        &mut self,
        id: &str,
        project: Option<ProjectRef>,
        now: u64,
    ) -> Result<Ticket, TicketError> {
        let project = validate_project_ref(project)?;
        self.commit(|doc| {
            let t = find_mut(doc, id)?;
            if t.project == project {
                return Ok(());
            }
            if !project_changeable(t) {
                return Err(TicketError::ProjectChangeNotAllowed);
            }
            put_project(t, project, TicketActor::User, now);
            Ok(())
        })?;
        self.fetch(id)
    }

    /// Only backlog and done tickets, and rejected ones without an agent, can be deleted.
    /// [`Self::delete_at`] with the current time.
    pub fn delete(&mut self, id: &str) -> Result<(), TicketError> {
        self.delete_at(id, crate::agent::now_ms())
    }

    /// [`Self::delete`] at `now`. In the same save (step 6a) its children lose their parent
    /// (history note [`PARENT_DELETED_NOTE`]) and it leaves every `blocked_by` (no note).
    pub fn delete_at(&mut self, id: &str, now: u64) -> Result<(), TicketError> {
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
            for t in doc.tickets.iter_mut() {
                if t.parent_id.as_deref() == Some(id) {
                    t.parent_id = None;
                    note_entry(t, TicketActor::System, PARENT_DELETED_NOTE.to_string(), now);
                }
                t.blocked_by.retain(|b| b != id);
            }
            Ok(())
        })
    }

    /// backlog/rejected → assigned, at the end of the agent's queue. The caller checks that the
    /// agent is alive.
    pub fn assign(&mut self, id: &str, agent_id: &str, now: u64) -> Result<Ticket, TicketError> {
        self.assign_in(id, agent_id, None, now)
    }

    /// [`Self::assign`]; `Some(project)` makes the ticket's project `Existing(project)` in the
    /// same save, before the assignment (a realised "new project", or one picked at assignment;
    /// the caller checked it against the agent, plan4b A.2).
    pub fn assign_in(
        &mut self,
        id: &str,
        agent_id: &str,
        project: Option<ProjectId>,
        now: u64,
    ) -> Result<Ticket, TicketError> {
        self.commit(|doc| {
            let t = find_mut(doc, id)?;
            if let Some(p) = project {
                put_project(t, Some(ProjectRef::Existing(p)), TicketActor::User, now);
            }
            let state = t.state;
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
                let to = RejectReturn::from(agent_live);
                return self.reject(id, note.as_deref().unwrap_or_default(), to, now);
            }
            (S::Rejected, from) if from != S::Rejected => return Err(TicketError::UseReject),
            (S::Assigned, from) if from != S::Assigned => return Err(TicketError::UseAssign),
            (S::Backlog, S::Assigned) => TicketEvent::Unassign,
            (S::Backlog, S::Done) => TicketEvent::ToBacklog {
                note: Some(note.unwrap_or_else(|| REOPENED_NOTE.into())),
            },
            (S::Backlog, S::InProgress | S::Review | S::Rejected | S::Waiting) => {
                TicketEvent::ToBacklog { note }
            }
            (S::InProgress, S::Assigned | S::Review | S::Waiting) => {
                let agent = t.assignee_agent_id.as_deref().unwrap_or_default();
                if !agent_live {
                    return Err(TicketError::AgentNotLive);
                }
                if in_progress_of(&self.doc, agent).is_some() {
                    return Err(TicketError::AgentBusy);
                }
                match t.state {
                    S::Assigned => TicketEvent::Dispatched,
                    S::Waiting => TicketEvent::Resume,
                    _ => TicketEvent::Reopen,
                }
            }
            // The user's move is an override: no waiting for open children (plan A.1).
            (S::Review, S::InProgress | S::Waiting) => TicketEvent::Submit,
            (S::Done, S::InProgress | S::Waiting) if t.skip_review => TicketEvent::Submit,
            (S::Done, S::InProgress | S::Waiting) => return Err(TicketError::DoneNeedsReview),
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

    /// review → rejected → first in the same agent's queue or the backlog (see [`RejectReturn`]).
    /// One save.
    pub fn reject(
        &mut self,
        id: &str,
        note: &str,
        to: RejectReturn,
        now: u64,
    ) -> Result<Ticket, TicketError> {
        self.reject_as(id, note, to, TicketActor::User, None, now)
    }

    /// [`Self::reject`] by `by`, with `prefix` before the note in the history.
    fn reject_as(
        &mut self,
        id: &str,
        note: &str,
        to: RejectReturn,
        by: TicketActor,
        prefix: Option<String>,
        now: u64,
    ) -> Result<Ticket, TicketError> {
        let ev = TicketEvent::Reject {
            note: note.to_string(),
        };
        self.commit(|doc| {
            let t = apply(doc, id, &ev, by, prefix, now)?;
            match (to, t.assignee_agent_id) {
                (RejectReturn::Moved, Some(_)) => {
                    // W1: the sender moved to another project; the ticket must not follow it
                    // there. It keeps its rejection note and review round for the next agent.
                    let round = t.review_round;
                    apply(
                        doc,
                        id,
                        &TicketEvent::ToBacklog {
                            note: Some(MOVED_NOTE.to_string()),
                        },
                        TicketActor::System,
                        None,
                        now,
                    )?;
                    find_mut(doc, id)?.review_round = round;
                }
                (RejectReturn::Sender, Some(agent)) => {
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

    /// The agent's turn ended normally: its inProgress ticket → review (done with skipReview),
    /// or waiting when it has open children (step 6a). `Ok(None)` when the agent had no ticket
    /// in progress (nothing saved).
    pub fn complete_turn(
        &mut self,
        agent_id: &str,
        now: u64,
    ) -> Result<Option<Ticket>, TicketError> {
        let Some(t) = self.current_for_agent(agent_id) else {
            return Ok(None);
        };
        let note = Some(TURN_ENDED_NOTE.to_string());
        self.commit(|doc| wait_or_submit(doc, &t.id, TicketActor::System, note, now))?;
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
    /// state and gets `issue: notSubmitted` with [`NOT_SUBMITTED_NOTE`] (by the system). A
    /// ticket with open children goes to waiting instead (step 6a; returned with
    /// `state == Waiting`, no issue). `Ok(None)` when the agent has no ticket in progress
    /// (nothing saved).
    pub fn mark_not_submitted(
        &mut self,
        agent_id: &str,
        now: u64,
    ) -> Result<Option<Ticket>, TicketError> {
        let Some(t) = self.current_for_agent(agent_id) else {
            return Ok(None);
        };
        if self.open_children_count(&t.id) > 0 {
            self.commit(|doc| wait_or_submit(doc, &t.id, TicketActor::System, None, now))?;
            return self.fetch(&t.id).map(Some);
        }
        self.set_issue(
            &t.id,
            Some(TicketIssue::NotSubmitted),
            Some(NOT_SUBMITTED_NOTE.to_string()),
            now,
        )
        .map(Some)
    }

    /// The wake line for a waiting parent was typed (plan A.2): waiting → inProgress by the
    /// system with `note`. `Ok(None)` (nothing saved) when the ticket is no longer waiting with
    /// `agent_id`, or the agent has another ticket in progress.
    pub fn resume_after_wake(
        &mut self,
        parent_id: &str,
        agent_id: &str,
        note: &str,
        now: u64,
    ) -> Result<Option<Ticket>, TicketError> {
        let t = self.get(parent_id).ok_or(TicketError::NotFound)?;
        if t.state != TicketState::Waiting
            || t.assignee_agent_id.as_deref() != Some(agent_id)
            || in_progress_of(&self.doc, agent_id).is_some()
        {
            return Ok(None);
        }
        let note = Some(note.to_string());
        self.commit(|doc| {
            apply(
                doc,
                &t.id,
                &TicketEvent::Resume,
                TicketActor::System,
                note,
                now,
            )
        })?;
        self.fetch(&t.id).map(Some)
    }

    /// A parent without an assignee in the backlog lost its last open child (plan A.2): a
    /// history entry [`CHILDREN_DONE_NOTE`] by the system. `Ok(None)` (nothing saved) when it is
    /// not such a parent any more, still has open children, or the note is already its last
    /// history entry.
    pub fn note_children_done(
        &mut self,
        id: &str,
        now: u64,
    ) -> Result<Option<Ticket>, TicketError> {
        let t = self.get(id).ok_or(TicketError::NotFound)?;
        let noted = t
            .history
            .last()
            .is_some_and(|h| h.note.as_deref() == Some(CHILDREN_DONE_NOTE));
        if t.state != TicketState::Backlog
            || t.assignee_agent_id.is_some()
            || noted
            || self.open_children_count(id) > 0
        {
            return Ok(None);
        }
        self.commit(|doc| {
            let t = find_mut(doc, id)?;
            note_entry(t, TicketActor::System, CHILDREN_DONE_NOTE.to_string(), now);
            Ok(())
        })?;
        self.fetch(id).map(Some)
    }

    /// Any non-backlog state → backlog by the system (delivery failure etc.).
    pub fn to_backlog(&mut self, id: &str, note: &str, now: u64) -> Result<Ticket, TicketError> {
        let ev = TicketEvent::ToBacklog {
            note: Some(note.to_string()),
        };
        self.commit(|doc| apply(doc, id, &ev, TicketActor::System, None, now))?;
        self.fetch(id)
    }

    /// The agent stopped/exited/was removed: all its assigned/inProgress/rejected/waiting
    /// tickets go to the backlog with `note`. One save (none when nothing changed).
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
                        TicketState::Assigned
                            | TicketState::InProgress
                            | TicketState::Rejected
                            | TicketState::Waiting
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
        project: Option<ProjectRef>,
        now: u64,
    ) -> Result<Ticket, TicketError> {
        self.create_by_agent_related(
            title,
            body,
            skip_review,
            assign_to,
            project,
            None,
            Vec::new(),
            now,
        )
    }

    /// [`Self::create_by_agent`] with relations (step 6a, plan A.3). `parent_id` and the
    /// `blocked_by` entries are full or short ids (stored as full ids). Checked in this order:
    /// the parent exists ([`TicketError::ParentNotFound`]) and is not Done
    /// ([`TicketError::ParentDone`]); without a project the child inherits the parent's, an
    /// explicit one must match it ([`TicketError::ParentProjectMismatch`]; a parent without a
    /// project accepts any); the blockers (duplicates dropped) are at most [`BLOCKED_BY_MAX`]
    /// ([`TicketError::TooManyBlockers`]), exist ([`TicketError::BlockerNotFound`]), are not the
    /// parent or an ancestor ([`TicketError::BlockedByAncestor`]) and form no cycle
    /// ([`TicketError::Cycle`]).
    #[allow(clippy::too_many_arguments)]
    pub fn create_by_agent_related(
        &mut self,
        title: &str,
        body: &str,
        skip_review: bool,
        assign_to: Option<(&str, &str)>,
        project: Option<ProjectRef>,
        parent_id: Option<TicketId>,
        blocked_by: Vec<TicketId>,
        now: u64,
    ) -> Result<Ticket, TicketError> {
        let parent = match parent_id.as_deref() {
            Some(p) => {
                let p = self.get_by_any_id(p).ok_or(TicketError::ParentNotFound)?;
                if p.state == TicketState::Done {
                    return Err(TicketError::ParentDone);
                }
                Some(p)
            }
            None => None,
        };
        let project = match (project, parent.as_ref().and_then(|p| p.project.clone())) {
            (None, inherited) => inherited,
            (Some(child), Some(pp)) if !same_id(child.name(), pp.name()) => {
                return Err(TicketError::ParentProjectMismatch {
                    parent: pp.name().to_string(),
                    child: child.name().to_string(),
                });
            }
            (Some(child), _) => Some(child),
        };
        let mut blockers: Vec<TicketId> = Vec::new();
        for b in &blocked_by {
            let full = match self.get_by_any_id(b) {
                Some(t) => t.id,
                None => b.trim().to_string(),
            };
            if !blockers.contains(&full) {
                blockers.push(full);
            }
        }
        if blockers.len() > BLOCKED_BY_MAX {
            return Err(TicketError::TooManyBlockers);
        }
        if let Some(missing) = blockers.iter().find(|b| self.get(b).is_none()) {
            return Err(TicketError::BlockerNotFound(missing.clone()));
        }
        if let Some(p) = &parent {
            let mut line = ancestors(&self.doc, &p.id);
            line.push(p.id.clone());
            if blockers.iter().any(|b| line.contains(b)) {
                return Err(TicketError::BlockedByAncestor);
            }
        }
        let mut t = self.new_ticket(
            &mut || uuid::Uuid::new_v4().to_string(),
            title,
            body,
            skip_review,
            project,
            (TicketSource::Agent, TicketActor::Agent),
            now,
        )?;
        // A fresh id cannot be part of a cycle yet; checked for the rule's sake (plan A.3).
        if parent
            .as_ref()
            .is_some_and(|p| would_cycle(&self.doc, &t.id, &p.id))
            || would_block_cycle(&self.doc, &t.id, &blockers)
        {
            return Err(TicketError::Cycle);
        }
        t.parent_id = parent.map(|p| p.id);
        t.blocked_by = blockers;
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
    /// history by the agent. Clears `issue` (the Submit transition does). Step 6a: a ticket with
    /// open children goes to waiting instead ([`wait_or_submit`]); a waiting ticket of the
    /// agent's own may be submitted again (by `ticket_id`).
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
                if !matches!(t.state, TicketState::InProgress | TicketState::Waiting) {
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
            wait_or_submit(doc, &t.id, TicketActor::Agent, Some(summary), now)
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
        to: RejectReturn,
        now: u64,
    ) -> Result<Ticket, TicketError> {
        let t = self.review_ticket_for(agent_id, ticket_id)?;
        if note.trim().is_empty() {
            return Err(TicketError::NeedsNote);
        }
        self.reject_as(
            &t.id,
            note.trim(),
            to,
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
        project: Option<ProjectId>,
        now: u64,
    ) -> Result<Ticket, TicketError> {
        let t = self.get_by_any_id(ticket_id).ok_or(TicketError::NotFound)?;
        let note = Some(format!("tildelt af koordinator {by_name}"));
        self.commit(|doc| {
            if let Some(p) = project {
                let tk = find_mut(doc, &t.id)?;
                put_project(tk, Some(ProjectRef::Existing(p)), TicketActor::Agent, now);
            }
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
    /// (any ticket in progress); `Some(agent)` = only the agent's own ticket in progress. Also
    /// read before a handoff creates a project folder (N3).
    pub fn handoff_source(
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
        names: (&str, &str),
        now: u64,
    ) -> Result<Ticket, TicketError> {
        self.handoff_in(ticket_id, to_agent, by_agent, names, None, now)
    }

    /// [`Self::handoff`]; `Some(project)` makes the ticket's project `Existing(project)` in the
    /// same save (a `{"new": …}` matching the new agent's project, plan4b A.2).
    pub fn handoff_in(
        &mut self,
        ticket_id: &str,
        to_agent: &str,
        by_agent: Option<&str>,
        (from_name, to_name): (&str, &str),
        project: Option<ProjectId>,
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
        self.commit(|doc| {
            if let Some(p) = project {
                let tk = find_mut(doc, &t.id)?;
                put_project(tk, Some(ProjectRef::Existing(p)), by, now);
            }
            apply(doc, &t.id, &ev, by, note, now)
        })?;
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
            // Step 6a: a waiting parent keeps its assignee and has no queue position.
            if t.state == S::Waiting {
                assert!(
                    t.assignee_agent_id.is_some(),
                    "waiting without agent: {t:?}"
                );
                assert_eq!(t.queue_position, None, "{t:?}");
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
                None,
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
                None,
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
        assert_eq!(
            s.reject(&a.id, "  ", RejectReturn::Sender, 7),
            Err(TicketError::NeedsNote)
        );
        let saves = m.saves();
        let r = s
            .reject(&a.id, "mangler test", RejectReturn::Sender, 7)
            .unwrap();
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
        let r = s.reject(&a.id, "nej", RejectReturn::Backlog, 10).unwrap();
        assert_eq!((r.state, r.assignee_agent_id.clone()), (S::Backlog, None));
        assert_invariants(&s);

        // Sender moved to another project (W1): backlog with the note; the rejection note and
        // the review round stay, the queue is untouched.
        s.assign(&a.id, "a1", 11).unwrap();
        s.mark_dispatched(&a.id, "demo", 12).unwrap();
        s.complete_turn("a1", 13).unwrap();
        let round = s.get(&a.id).unwrap().review_round;
        let r = s.reject(&a.id, "igen", RejectReturn::Moved, 14).unwrap();
        assert_eq!((r.state, r.assignee_agent_id.clone()), (S::Backlog, None));
        assert_eq!(r.rejection_note.as_deref(), Some("igen"));
        assert_eq!(r.review_round, round + 1);
        assert_eq!(r.history.last().unwrap().note.as_deref(), Some(MOVED_NOTE));
        assert_eq!(
            positions(&s, "a1"),
            vec![("b".into(), Some(0)), ("c".into(), Some(1))]
        );
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
            .create_by_agent("Følg op", "detaljer", true, None, None, 9)
            .unwrap();
        assert_eq!(t.source, TicketSource::Agent);
        assert_eq!((t.state, t.skip_review), (S::Backlog, true));
        assert_eq!(t.assignee_agent_id, None);
        assert_eq!(t.history.len(), 1);
        assert_eq!(t.history[0].by, TicketActor::Agent);
        assert_eq!(
            s.create_by_agent(" ", "", false, None, None, 9)
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
            s.reject_by_agent("rev", "r", &t.id, "  ", RejectReturn::Sender, 6),
            Err(TicketError::NeedsNote)
        );
        let r = s
            .reject_by_agent(
                "rev",
                "bot-rev",
                &t.id,
                "mangler test",
                RejectReturn::Sender,
                7,
            )
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
            .reject_by_agent("rev", "r", &t.id, "nej", RejectReturn::Backlog, 11)
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
        let t = s
            .assign_by_agent(&b.short_id(), "a1", "koord", None, 2)
            .unwrap();
        assert_eq!(
            (t.state, t.assignee_agent_id.as_deref()),
            (S::Assigned, Some("a1"))
        );
        let last = t.history.last().unwrap();
        assert_eq!(
            (last.by, last.note.as_deref()),
            (TicketActor::Agent, Some("tildelt af koordinator koord"))
        );
        let err = s.assign_by_agent(&b.id, "a2", "k", None, 3).unwrap_err();
        assert!(
            matches!(err, TicketError::IllegalTransition { .. }),
            "{err:?}"
        );
        let u = s.unassign_by_agent(&b.id, "k", 4).unwrap();
        assert_eq!(u.state, S::Backlog);
        let p = in_review(&mut s, "a1", "p");
        assert!(s.assign_by_agent(&p.id, "a2", "k", None, 5).is_err());
        assert_eq!(
            s.assign_by_agent("nope", "a2", "k", None, 5),
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
            .create_by_agent("Ny", "", false, Some(("a1", "koord")), None, 3)
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

    // ---- step 4b: projects on tickets ----

    #[test]
    fn create_in_validates_the_project_name() {
        let (mut s, _) = svc();
        let err = s
            .create_in(
                "t",
                "",
                false,
                Some(ProjectRef::New { new: "CON".into() }),
                1,
            )
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Projektnavnet «CON» er ugyldigt: er et reserveret navn i Windows"
        );
        assert!(s.is_empty());
        let t = s
            .create_in("t", "", false, Some(ProjectRef::Existing("p".into())), 2)
            .unwrap();
        assert_eq!(t.project, Some(ProjectRef::Existing("p".into())));
        assert_eq!(s.create("u", "", false, 3).unwrap().project, None);
    }

    #[test]
    fn set_project_only_in_backlog_or_rejected_without_agent() {
        let (mut s, _) = svc();
        let t = mk(&mut s, "a", 1);
        let p = Some(ProjectRef::Existing("p".into()));
        let u = s.set_project(&t.id, p.clone(), 2).unwrap();
        assert_eq!(u.project, p);
        assert_eq!(
            u.history.last().unwrap().note.as_deref(),
            Some("projekt: «p»")
        );
        let n = u.history.len();
        // Unchanged: no new entry.
        assert_eq!(s.set_project(&t.id, p.clone(), 3).unwrap().history.len(), n);
        let r = s.set_project(&t.id, None, 4).unwrap();
        assert_eq!(r.project, None);
        assert_eq!(
            r.history.last().unwrap().note.as_deref(),
            Some("projekt fjernet")
        );
        s.assign(&t.id, "a1", 5).unwrap();
        assert_eq!(
            s.set_project(&t.id, p.clone(), 6),
            Err(TicketError::ProjectChangeNotAllowed)
        );
        // Same project in another state is no change: accepted.
        assert!(s.set_project(&t.id, None, 7).is_ok());
    }

    #[test]
    fn assign_in_sets_the_project_and_assigns_in_one_save() {
        let (mut s, store) = svc();
        let t = s
            .create_in("t", "", false, Some(ProjectRef::New { new: "P".into() }), 1)
            .unwrap();
        let before = store.saves();
        let a = s.assign_in(&t.id, "a1", Some("p".into()), 2).unwrap();
        assert_eq!(store.saves(), before + 1);
        assert_eq!(a.project, Some(ProjectRef::Existing("p".into())));
        assert_eq!(a.state, S::Assigned);
        assert_eq!(a.assignee_agent_id.as_deref(), Some("a1"));
        // By an agent: the same.
        let t2 = mk(&mut s, "b", 3);
        let b = s
            .assign_by_agent(&t2.id, "a1", "koord", Some("q".into()), 4)
            .unwrap();
        assert_eq!(b.project, Some(ProjectRef::Existing("q".into())));
        assert_invariants(&s);
    }

    #[test]
    fn create_by_agent_and_update_handle_the_project() {
        let (mut s, _) = svc();
        let t = s
            .create_by_agent(
                "x",
                "",
                false,
                None,
                Some(ProjectRef::Existing("p".into())),
                1,
            )
            .unwrap();
        assert_eq!(t.project, Some(ProjectRef::Existing("p".into())));
        let patch: TicketPatch = serde_json::from_str(r#"{"project": null}"#).unwrap();
        assert_eq!(s.update(&t.id, patch, 2).unwrap().project, None);
        let patch: TicketPatch = serde_json::from_str(r#"{"project": {"new": "a:b"}}"#).unwrap();
        assert!(matches!(
            s.update(&t.id, patch, 3),
            Err(TicketError::Validation(_))
        ));
        let patch: TicketPatch = serde_json::from_str(r#"{"project": "q"}"#).unwrap();
        assert_eq!(
            s.update(&t.id, patch, 4).unwrap().project,
            Some(ProjectRef::Existing("q".into()))
        );
        s.assign(&t.id, "a1", 5).unwrap();
        let patch: TicketPatch = serde_json::from_str(r#"{"project": "r"}"#).unwrap();
        assert_eq!(
            s.update(&t.id, patch, 6),
            Err(TicketError::ProjectChangeNotAllowed)
        );
        // Title only: fine in any state.
        let patch: TicketPatch = serde_json::from_str(r#"{"title": "y"}"#).unwrap();
        assert!(s.update(&t.id, patch, 7).is_ok());
    }

    // ---- step 6a: relations, waiting, blocking ----

    use crate::config::{BLOCKED_BY_MAX, CHILDREN_DONE_NOTE, WOKEN_NOTE};

    /// A child of `parent` created by an agent (no project, no blockers).
    fn child_of(s: &mut TicketService, parent: &Ticket, title: &str, now: u64) -> Ticket {
        s.create_by_agent_related(
            title,
            "b",
            false,
            None,
            None,
            Some(parent.id.clone()),
            Vec::new(),
            now,
        )
        .unwrap()
    }

    /// Takes backlog ticket `id` through `agent` to Done (assign, dispatch, review, approve).
    fn finish(s: &mut TicketService, id: &str, agent: &str, now: u64) -> Ticket {
        s.assign(id, agent, now).unwrap();
        s.mark_dispatched(id, agent, now + 1).unwrap();
        assert_eq!(
            s.complete_turn(agent, now + 2).unwrap().unwrap().state,
            S::Review
        );
        s.approve(id, now + 3).unwrap()
    }

    fn related(
        s: &mut TicketService,
        title: &str,
        project: Option<ProjectRef>,
        parent: Option<&str>,
        blocked_by: &[&str],
    ) -> Result<Ticket, TicketError> {
        s.create_by_agent_related(
            title,
            "",
            false,
            None,
            project,
            parent.map(str::to_string),
            blocked_by.iter().map(|b| b.to_string()).collect(),
            50,
        )
    }

    #[test]
    fn create_related_inherits_parent_project_and_rejects_mismatch() {
        let (mut s, m) = svc();
        let p = s
            .create_in(
                "forælder",
                "",
                false,
                Some(ProjectRef::Existing("p".into())),
                1,
            )
            .unwrap();
        // By short id; the child inherits the parent's project and stores the full id.
        let c = related(
            &mut s,
            "barn",
            None,
            Some(&p.short_id().to_uppercase()),
            &[],
        )
        .unwrap();
        assert_eq!(c.parent_id.as_deref(), Some(p.id.as_str()));
        assert_eq!(c.project, Some(ProjectRef::Existing("p".into())));
        assert_eq!((c.source, c.state), (TicketSource::Agent, S::Backlog));
        // The same project in other letters is the same project.
        let c2 = related(
            &mut s,
            "barn 2",
            Some(ProjectRef::Existing("P".into())),
            Some(&p.id),
            &[],
        )
        .unwrap();
        assert_eq!(c2.project, Some(ProjectRef::Existing("P".into())));
        let saves = m.saves();
        assert_eq!(
            related(
                &mut s,
                "barn 3",
                Some(ProjectRef::New { new: "q".into() }),
                Some(&p.id),
                &[]
            ),
            Err(TicketError::ParentProjectMismatch {
                parent: "p".into(),
                child: "q".into()
            })
        );
        assert_eq!(m.saves(), saves);
        // A parent without a project accepts a child with one.
        let free = mk(&mut s, "uden projekt", 2);
        let c4 = related(
            &mut s,
            "barn 4",
            Some(ProjectRef::Existing("q".into())),
            Some(&free.id),
            &[],
        )
        .unwrap();
        assert_eq!(c4.project, Some(ProjectRef::Existing("q".into())));
        // With assign_to the child is queued in the same save.
        let saves = m.saves();
        let c5 = s
            .create_by_agent_related(
                "barn 5",
                "",
                false,
                Some(("a1", "koord-01")),
                None,
                Some(p.id.clone()),
                vec![c.id.clone()],
                60,
            )
            .unwrap();
        assert_eq!(m.saves(), saves + 1);
        assert_eq!((c5.state, c5.queue_position), (S::Assigned, Some(0)));
        assert_eq!(c5.blocked_by, vec![c.id.clone()]);
        assert_eq!(s.open_children_count(&p.id), 3);
        // Plain create_by_agent keeps working without relations.
        let plain = s.create_by_agent("x", "", false, None, None, 70).unwrap();
        assert_eq!((plain.parent_id, plain.blocked_by.len()), (None, 0));
        assert_invariants(&s);
    }

    #[test]
    fn create_related_rejects_unknown_parent_done_parent_and_unknown_blocker() {
        let (mut s, m) = svc();
        assert_eq!(
            related(&mut s, "a", None, Some("ffffffff"), &[]),
            Err(TicketError::ParentNotFound)
        );
        let done = mk(&mut s, "færdig", 1);
        finish(&mut s, &done.id, "a1", 2);
        assert_eq!(
            related(&mut s, "a", None, Some(&done.id), &[]),
            Err(TicketError::ParentDone)
        );
        let b = mk(&mut s, "blokering", 3);
        assert_eq!(
            related(&mut s, "a", None, None, &[&b.id, "ffffffff"]),
            Err(TicketError::BlockerNotFound("ffffffff".into()))
        );
        // Too many blockers (after dropping duplicates).
        let many: Vec<String> = (0..=BLOCKED_BY_MAX)
            .map(|i| mk(&mut s, &format!("b{i}"), 10 + i as u64).id)
            .collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let saves = m.saves();
        assert_eq!(
            related(&mut s, "a", None, None, &refs),
            Err(TicketError::TooManyBlockers)
        );
        assert_eq!(m.saves(), saves);
        let mut ten: Vec<&str> = refs[..BLOCKED_BY_MAX].to_vec();
        ten.push(refs[0]);
        let ok = related(&mut s, "a", None, None, &ten).unwrap();
        assert_eq!(ok.blocked_by.len(), BLOCKED_BY_MAX);
        // A done blocker is accepted (it simply does not block).
        let d = related(&mut s, "efter færdig", None, None, &[&done.short_id()]).unwrap();
        assert_eq!(d.blocked_by, vec![done.id.clone()]);
        assert_eq!(s.blocked_text(&d.id), None);
    }

    #[test]
    fn create_related_rejects_blocker_that_is_ancestor_and_cycles() {
        let (mut s, _) = svc();
        let p = mk(&mut s, "p", 1);
        let c = child_of(&mut s, &p, "c", 2);
        for blocker in [&p.id, &c.id] {
            assert_eq!(
                related(&mut s, "g", None, Some(&c.id), &[blocker]),
                Err(TicketError::BlockedByAncestor),
                "{blocker}"
            );
        }
        assert_eq!(
            related(&mut s, "g", None, Some(&p.id), &[&p.short_id()]),
            Err(TicketError::BlockedByAncestor)
        );
        // A sibling may block.
        let g = related(&mut s, "g", None, Some(&p.id), &[&c.id]).unwrap();
        assert_eq!(g.blocked_by, vec![c.id.clone()]);
        assert_eq!(ancestors(&s.doc, &g.id), vec![p.id.clone()]);

        // The pure checks, on a document edited by hand.
        assert!(would_cycle(&s.doc, &p.id, &p.id));
        assert!(would_cycle(&s.doc, &p.id, &c.id), "p is c's parent");
        assert!(!would_cycle(&s.doc, &c.id, &p.id));
        assert!(would_block_cycle(
            &s.doc,
            &c.id,
            std::slice::from_ref(&g.id)
        ));
        assert!(would_block_cycle(
            &s.doc,
            &c.id,
            std::slice::from_ref(&c.id)
        ));
        assert!(!would_block_cycle(
            &s.doc,
            &g.id,
            std::slice::from_ref(&c.id)
        ));
        // A hand-made parent cycle p ↔ c ends the walk.
        find_mut(&mut s.doc, &p.id).unwrap().parent_id = Some(c.id.clone());
        assert_eq!(ancestors(&s.doc, &c.id), vec![p.id.clone()]);
        assert!(would_cycle(&s.doc, &c.id, &p.id));
        // A hand-made blocker cycle b1 → b2 → b1.
        let b1 = mk(&mut s, "b1", 3);
        let b2 = mk(&mut s, "b2", 4);
        find_mut(&mut s.doc, &b1.id).unwrap().blocked_by = vec![b2.id.clone()];
        find_mut(&mut s.doc, &b2.id).unwrap().blocked_by = vec![b1.id.clone()];
        assert!(would_block_cycle(
            &s.doc,
            &b1.id,
            std::slice::from_ref(&b2.id)
        ));
        assert!(!would_block_cycle(
            &s.doc,
            "other",
            std::slice::from_ref(&b1.id)
        ));
        // Depth limit: a parent chain longer than 64 is cut off.
        let mut prev = mk(&mut s, "root", 5);
        for i in 0..70 {
            prev = child_of(&mut s, &prev, &format!("l{i}"), 6 + i);
        }
        assert_eq!(ancestors(&s.doc, &prev.id).len(), RELATION_DEPTH_MAX);
    }

    /// Parent `p` in progress with agent `k` and two open children.
    fn parent_with_children(s: &mut TicketService, skip: bool) -> (Ticket, Ticket, Ticket) {
        let p = in_progress(s, "k", "forælder", skip);
        let c1 = child_of(s, &p, "plan", 10);
        let c2 = child_of(s, &p, "byg", 11);
        (p, c1, c2)
    }

    #[test]
    fn submit_with_open_children_waits_and_keeps_summary() {
        for skip in [false, true] {
            let (mut s, m) = svc();
            let (p, _c1, _c2) = parent_with_children(&mut s, skip);
            let saves = m.saves();
            let w = s.submit_by_agent("k", None, "fordelt", 20).unwrap();
            assert_eq!(m.saves(), saves + 1);
            assert_eq!(w.state, S::Waiting, "skip {skip}");
            assert_eq!(w.summary.as_deref(), Some("fordelt"));
            assert_eq!(w.assignee_agent_id.as_deref(), Some("k"));
            let h = w.history.last().unwrap();
            assert_eq!(
                (h.from, h.to, h.by, h.note.as_deref()),
                (
                    Some(S::InProgress),
                    S::Waiting,
                    TicketActor::Agent,
                    Some("venter på del-tickets (2)")
                )
            );
            // The agent is free: no current ticket, nothing queued.
            assert_eq!(s.current_for_agent("k"), None);
            assert_eq!(s.links().get("k"), None);
            // Submitting again while children are open: still waiting, new summary, one entry.
            let len = w.history.len();
            let again = s
                .submit_by_agent("k", Some(&p.short_id()), "ny plan", 21)
                .unwrap();
            assert_eq!(again.state, S::Waiting);
            assert_eq!(again.summary.as_deref(), Some("ny plan"));
            assert_eq!(again.history.len(), len + 1);
            // Without a ticket id there is nothing in progress.
            assert_eq!(
                s.submit_by_agent("k", None, "x", 22),
                Err(TicketError::NoTicketInProgress)
            );
            // Someone else's waiting ticket is not theirs.
            assert_eq!(
                s.submit_by_agent("z", Some(&p.id), "x", 22),
                Err(TicketError::NotYours)
            );
            assert_invariants(&s);
        }
    }

    #[test]
    fn submit_without_open_children_goes_to_review_or_done() {
        for (skip, want) in [(false, S::Review), (true, S::Done)] {
            let (mut s, _) = svc();
            let p = in_progress(&mut s, "k", "forælder", skip);
            let c = child_of(&mut s, &p, "c", 10);
            finish(&mut s, &c.id, "c1", 11);
            let r = s.submit_by_agent("k", None, "samlet", 20).unwrap();
            assert_eq!(r.state, want);
            assert_eq!(r.history.last().unwrap().note.as_deref(), Some("samlet"));
            // A ticket without any children behaves as before.
            let t = in_progress(&mut s, "k2", "enkel", skip);
            assert_eq!(s.submit_by_agent("k2", None, "ok", 21).unwrap().state, want);
            assert_eq!(s.open_children_count(&t.id), 0);
        }
    }

    #[test]
    fn submit_waiting_parent_when_all_done_submits() {
        let (mut s, _) = svc();
        let (p, c1, c2) = parent_with_children(&mut s, false);
        s.submit_by_agent("k", None, "fordelt", 20).unwrap();
        finish(&mut s, &c1.id, "c", 30);
        // One child still open: stays waiting.
        let w = s.submit_by_agent("k", Some(&p.id), "delvis", 40).unwrap();
        assert_eq!(w.state, S::Waiting);
        assert_eq!(
            w.history.last().unwrap().note.as_deref(),
            Some("venter på del-tickets (1)")
        );
        // A deleted child no longer counts.
        s.delete_at(&c2.id, 42).unwrap();
        let r = s
            .submit_by_agent("k", Some(&p.short_id()), "alt klar", 50)
            .unwrap();
        assert_eq!(r.state, S::Review);
        assert_eq!(r.summary.as_deref(), Some("alt klar"));
        assert_eq!(r.assignee_agent_id.as_deref(), Some("k"));
        assert_invariants(&s);
    }

    #[test]
    fn mark_not_submitted_waits_instead_of_issue() {
        let (mut s, _) = svc();
        let (p, c1, c2) = parent_with_children(&mut s, false);
        let w = s.mark_not_submitted("k", 20).unwrap().unwrap();
        assert_eq!((w.state, w.issue), (S::Waiting, None));
        let h = w.history.last().unwrap();
        assert_eq!(
            (h.by, h.note.as_deref()),
            (TicketActor::System, Some("venter på del-tickets (2)"))
        );
        // Nothing in progress any more: the next Stop saves nothing.
        assert_eq!(s.mark_not_submitted("k", 21).unwrap(), None);
        // All children done: the ordinary "not submitted".
        finish(&mut s, &c1.id, "c", 30);
        finish(&mut s, &c2.id, "c", 40);
        s.resume_after_wake(&p.id, "k", WOKEN_NOTE, 50)
            .unwrap()
            .unwrap();
        let n = s.mark_not_submitted("k", 60).unwrap().unwrap();
        assert_eq!(
            (n.state, n.issue),
            (S::InProgress, Some(TicketIssue::NotSubmitted))
        );
        assert_invariants(&s);
    }

    #[test]
    fn complete_turn_waits() {
        for skip in [false, true] {
            let (mut s, _) = svc();
            let (_p, _c1, _c2) = parent_with_children(&mut s, skip);
            let w = s.complete_turn("k", 20).unwrap().unwrap();
            assert_eq!(w.state, S::Waiting, "skip {skip}");
            assert_eq!(w.history.last().unwrap().by, TicketActor::System);
            assert_eq!(s.complete_turn("k", 21).unwrap(), None);
            assert_invariants(&s);
        }
    }

    #[test]
    fn next_for_agent_skips_blocked_and_keeps_positions() {
        let (mut s, m) = svc();
        let blocker = mk(&mut s, "plan", 1);
        let blocked = s
            .create_by_agent_related(
                "byg",
                "",
                false,
                Some(("c", "koord")),
                None,
                None,
                vec![blocker.short_id()],
                2,
            )
            .unwrap();
        let free = s
            .create_by_agent("test", "", false, Some(("c", "koord")), None, 3)
            .unwrap();
        assert_eq!(blocked.queue_position, Some(0));
        assert_eq!(s.blocked_text(&blocked.id), Some(blocker.short_id()));
        assert_eq!(s.blocked_text(&free.id), None);
        assert_eq!(s.blocked_text("missing"), None);
        // The blocked head is skipped; its position stays.
        assert_eq!(s.next_for_agent("c").unwrap().id, free.id);
        assert_eq!(
            positions(&s, "c"),
            vec![("byg".to_string(), Some(0)), ("test".to_string(), Some(1))]
        );
        // The queue still counts it (the agent is not idle).
        assert_eq!(s.links().get("c"), Some(&(None, 2)));
        s.mark_dispatched(&free.id, "c", 4).unwrap();
        s.complete_turn("c", 5).unwrap();
        assert_eq!(s.next_for_agent("c"), None, "only a blocked ticket left");
        let saves = m.saves();
        finish(&mut s, &blocker.id, "p", 10);
        assert!(m.saves() > saves);
        let next = s.next_for_agent("c").unwrap();
        assert_eq!(
            (next.id, next.queue_position),
            (blocked.id.clone(), Some(0))
        );
        assert_eq!(s.blocked_text(&blocked.id), None);
        assert_invariants(&s);
    }

    #[test]
    fn delete_clears_parent_and_blockers_in_one_save() {
        let (mut s, m) = svc();
        let p = mk(&mut s, "p", 1);
        let b = mk(&mut s, "b", 2);
        let c = related(&mut s, "c", None, Some(&p.id), &[&b.id]).unwrap();
        let d = related(&mut s, "d", None, None, &[&b.id]).unwrap();
        let other = mk(&mut s, "andet", 3);
        let other_len = other.history.len();
        let c_len = s.get(&c.id).unwrap().history.len();

        let saves = m.saves();
        s.delete_at(&p.id, 100).unwrap();
        assert_eq!(m.saves(), saves + 1);
        let c1 = s.get(&c.id).unwrap();
        assert_eq!(c1.parent_id, None);
        assert_eq!(c1.history.len(), c_len + 1);
        let h = c1.history.last().unwrap();
        assert_eq!(
            (h.at, h.from, h.to, h.by, h.note.as_deref()),
            (
                100,
                Some(S::Backlog),
                S::Backlog,
                TicketActor::System,
                Some(PARENT_DELETED_NOTE)
            )
        );
        assert_eq!(c1.blocked_by, vec![b.id.clone()]);

        s.delete_at(&b.id, 101).unwrap();
        assert_eq!(m.saves(), saves + 2);
        let c2 = s.get(&c.id).unwrap();
        assert!(c2.blocked_by.is_empty());
        assert_eq!(c2.history.len(), c_len + 1, "no note for a removed blocker");
        assert!(s.get(&d.id).unwrap().blocked_by.is_empty());
        assert_eq!(s.get(&other.id).unwrap().history.len(), other_len);
        let saved = m.doc().unwrap();
        assert!(saved
            .tickets
            .iter()
            .all(|t| t.parent_id.is_none() && t.blocked_by.is_empty()));
        // A waiting parent cannot be deleted.
        let (_, _, _) = {
            let (pp, c1, c2) = parent_with_children(&mut s, false);
            s.submit_by_agent("k", None, "x", 110).unwrap();
            assert_eq!(s.delete_at(&pp.id, 111), Err(TicketError::NotDeletable));
            (pp, c1, c2)
        };
        assert_invariants(&s);
    }

    #[test]
    fn release_agent_and_recover_move_waiting_to_backlog() {
        let (mut s, m) = svc();
        let (p, c1, c2) = parent_with_children(&mut s, false);
        s.submit_by_agent("k", None, "fordelt", 20).unwrap();
        let doc = s.doc.clone();

        let saves = m.saves();
        let released = s.release_agent("k", "agent stoppet", 30).unwrap();
        assert_eq!(m.saves(), saves + 1);
        assert_eq!(released.len(), 1);
        let b = s.get(&p.id).unwrap();
        assert_eq!((b.state, b.assignee_agent_id.clone()), (S::Backlog, None));
        assert_eq!(
            b.history.last().unwrap().note.as_deref(),
            Some("agent stoppet")
        );
        // The children are untouched and keep their parent.
        for c in [&c1, &c2] {
            let t = s.get(&c.id).unwrap();
            assert_eq!(
                (t.state, t.parent_id.as_deref()),
                (S::Backlog, Some(p.id.as_str()))
            );
        }
        assert_invariants(&s);

        // A restart with a waiting parent.
        let store = MemoryStore::with_doc(doc);
        let (r, warning) = TicketService::load_and_recover(Box::new(store.clone()), 100);
        assert_eq!(warning, None);
        assert_eq!(store.saves(), 1);
        let t = r.get(&p.id).unwrap();
        assert_eq!((t.state, t.assignee_agent_id.clone()), (S::Backlog, None));
        assert_eq!(
            t.history.last().unwrap().note.as_deref(),
            Some(RESTART_NOTE)
        );
        assert_eq!(
            r.get(&c1.id).unwrap().parent_id.as_deref(),
            Some(p.id.as_str())
        );
        assert_invariants(&r);
    }

    #[test]
    fn due_wake_for_table() {
        let (mut s, _) = svc();
        // No waiting parent.
        assert_eq!(s.due_wake_for("k"), None);
        let (p, c1, c2) = parent_with_children(&mut s, false);
        assert_eq!(s.due_wake_for("k"), None, "in progress, not waiting");
        s.submit_by_agent("k", None, "fordelt", 20).unwrap();
        // Waiting, nothing done yet.
        assert_eq!(s.due_wake_for("k"), None);
        // One child done.
        finish(&mut s, &c1.id, "c", 30);
        let w = s.due_wake_for("k").unwrap();
        assert_eq!(w.parent.id, p.id);
        assert_eq!(
            w.newly_done
                .iter()
                .map(|t| t.id.clone())
                .collect::<Vec<_>>(),
            vec![c1.id.clone()]
        );
        assert_eq!(w.open_left, 1);
        assert_eq!(s.due_wake_for("other"), None);
        // Woken (the line was typed) and waiting again: c1 no longer counts.
        s.resume_after_wake(&p.id, "k", WOKEN_NOTE, 40)
            .unwrap()
            .unwrap();
        assert_eq!(s.due_wake_for("k"), None, "in progress again");
        s.submit_by_agent("k", None, "næste", 41).unwrap();
        assert_eq!(s.due_wake_for("k"), None, "c1 was done before the resume");
        // The last child done.
        finish(&mut s, &c2.id, "c", 50);
        let w = s.due_wake_for("k").unwrap();
        assert_eq!(
            (w.newly_done.len(), w.newly_done[0].id.clone(), w.open_left),
            (1, c2.id.clone(), 0)
        );

        // Time reference: a child done at exactly the resume time is not new (strictly later).
        let (mut s, _) = svc();
        let (p, c1, c2) = parent_with_children(&mut s, false);
        let _c3 = child_of(&mut s, &p, "test", 12);
        s.submit_by_agent("k", None, "fordelt", 20).unwrap();
        finish(&mut s, &c1.id, "c", 30);
        s.resume_after_wake(&p.id, "k", WOKEN_NOTE, 40)
            .unwrap()
            .unwrap();
        s.submit_by_agent("k", None, "næste", 41).unwrap();
        finish(&mut s, &c2.id, "c", 37);
        let t = find_mut(&mut s.doc, &c2.id).unwrap();
        t.history.last_mut().unwrap().at = 40;
        assert_eq!(s.due_wake_for("k"), None, "done at 40 is not after 40");
        find_mut(&mut s.doc, &c2.id)
            .unwrap()
            .history
            .last_mut()
            .unwrap()
            .at = 41;
        assert_eq!(s.due_wake_for("k").unwrap().newly_done.len(), 1);

        // The last open child deleted: due without a newly done child.
        let (mut s, _) = svc();
        let p = in_progress(&mut s, "k", "forælder", false);
        let c = child_of(&mut s, &p, "c", 10);
        s.submit_by_agent("k", None, "fordelt", 20).unwrap();
        assert_eq!(s.due_wake_for("k"), None);
        s.delete_at(&c.id, 30).unwrap();
        let w = s.due_wake_for("k").unwrap();
        assert_eq!((w.parent.id, w.newly_done.len(), w.open_left), (p.id, 0, 0));

        // Two due parents: the one changed longest ago; newly done in Done order.
        let (mut s, _) = svc();
        let p1 = in_progress(&mut s, "k", "p1", false);
        let a = child_of(&mut s, &p1, "a", 10);
        let b = child_of(&mut s, &p1, "b", 11);
        let z = child_of(&mut s, &p1, "z", 12);
        s.submit_by_agent("k", None, "x", 20).unwrap();
        let p2 = s.create("p2", "", false, 21).unwrap();
        s.assign(&p2.id, "k", 22).unwrap();
        s.mark_dispatched(&p2.id, "k", 23).unwrap();
        let y = child_of(&mut s, &p2, "y", 24);
        s.submit_by_agent("k", None, "y", 25).unwrap();
        finish(&mut s, &y.id, "c", 30);
        finish(&mut s, &b.id, "c", 40);
        finish(&mut s, &a.id, "c", 50);
        let w = s.due_wake_for("k").unwrap();
        assert_eq!(w.parent.id, p1.id);
        assert_eq!(
            w.newly_done
                .iter()
                .map(|t| t.title.as_str())
                .collect::<Vec<_>>(),
            vec!["b", "a"]
        );
        assert_eq!(w.open_left, 1);
        assert_eq!(s.get(&z.id).unwrap().state, S::Backlog);
        assert_invariants(&s);
    }

    #[test]
    fn resume_after_wake_requires_waiting_and_free_agent() {
        let (mut s, m) = svc();
        let (p, c1, _c2) = parent_with_children(&mut s, false);
        // Not waiting.
        assert_eq!(s.resume_after_wake(&p.id, "k", WOKEN_NOTE, 15), Ok(None));
        assert_eq!(
            s.resume_after_wake("missing", "k", WOKEN_NOTE, 15),
            Err(TicketError::NotFound)
        );
        s.submit_by_agent("k", None, "fordelt", 20).unwrap();
        finish(&mut s, &c1.id, "c", 30);
        // Another agent's parent.
        assert_eq!(s.resume_after_wake(&p.id, "z", WOKEN_NOTE, 35), Ok(None));
        // The agent took another ticket meanwhile.
        let other = in_progress(&mut s, "k", "andet", false);
        let saves = m.saves();
        assert_eq!(s.resume_after_wake(&p.id, "k", WOKEN_NOTE, 36), Ok(None));
        assert_eq!(m.saves(), saves);
        s.submit_by_agent("k", Some(&other.id), "ok", 37).unwrap();
        // Free: resumed, by the system with the note.
        let r = s
            .resume_after_wake(&p.id, "k", WOKEN_NOTE, 40)
            .unwrap()
            .unwrap();
        assert_eq!(r.state, S::InProgress);
        let h = r.history.last().unwrap();
        assert_eq!(
            (h.at, h.from, h.by, h.note.as_deref()),
            (40, Some(S::Waiting), TicketActor::System, Some(WOKEN_NOTE))
        );
        assert_eq!(s.current_for_agent("k").unwrap().id, p.id);
        assert_eq!(m.saves(), saves + 2);
        assert_invariants(&s);
    }

    #[test]
    fn note_children_done_only_for_backlog_parent_without_open_children() {
        let (mut s, m) = svc();
        let p = mk(&mut s, "p", 1);
        let c = child_of(&mut s, &p, "c", 2);
        assert_eq!(s.note_children_done(&p.id, 3), Ok(None), "open child");
        finish(&mut s, &c.id, "a", 10);
        let saves = m.saves();
        let n = s.note_children_done(&p.id, 20).unwrap().unwrap();
        assert_eq!(m.saves(), saves + 1);
        let h = n.history.last().unwrap();
        assert_eq!(
            (h.from, h.to, h.by, h.note.as_deref()),
            (
                Some(S::Backlog),
                S::Backlog,
                TicketActor::System,
                Some(CHILDREN_DONE_NOTE)
            )
        );
        // Not twice in a row.
        assert_eq!(s.note_children_done(&p.id, 21), Ok(None));
        assert_eq!(m.saves(), saves + 1);
        // Not for a parent with an assignee.
        s.assign(&p.id, "a", 22).unwrap();
        assert_eq!(s.note_children_done(&p.id, 23), Ok(None));
    }

    #[test]
    fn set_state_from_waiting() {
        let waiting = |skip: bool| {
            let (mut s, _) = svc();
            let (p, _, _) = parent_with_children(&mut s, skip);
            s.submit_by_agent("k", None, "fordelt", 20).unwrap();
            (s, p.id)
        };
        // → Backlog with the note.
        let (mut s, id) = waiting(false);
        let b = s
            .set_state(&id, S::Backlog, Some("selv".into()), true, 30)
            .unwrap();
        assert_eq!((b.state, b.assignee_agent_id.clone()), (S::Backlog, None));
        assert_eq!(b.history.last().unwrap().note.as_deref(), Some("selv"));
        // → In progress: Resume, only with a live and free agent.
        let (mut s, id) = waiting(false);
        assert_eq!(
            s.set_state(&id, S::InProgress, None, false, 30),
            Err(TicketError::AgentNotLive)
        );
        let other = in_progress(&mut s, "k", "andet", false);
        assert_eq!(
            s.set_state(&id, S::InProgress, None, true, 30),
            Err(TicketError::AgentBusy)
        );
        s.submit_by_agent("k", Some(&other.id), "ok", 31).unwrap();
        let r = s.set_state(&id, S::InProgress, None, true, 32).unwrap();
        assert_eq!(r.state, S::InProgress);
        assert_eq!(r.history.last().unwrap().from, Some(S::Waiting));
        // → Review: the user's override, open children notwithstanding.
        let (mut s, id) = waiting(false);
        assert_eq!(
            s.set_state(&id, S::Review, None, true, 30).unwrap().state,
            S::Review
        );
        // → Done only with skipReview.
        let (mut s, id) = waiting(false);
        assert_eq!(
            s.set_state(&id, S::Done, None, true, 30),
            Err(TicketError::DoneNeedsReview)
        );
        let (mut s, id) = waiting(true);
        assert_eq!(
            s.set_state(&id, S::Done, None, true, 30).unwrap().state,
            S::Done
        );
        // → Assigned / Rejected as for other states; nobody moves a ticket into waiting.
        let (mut s, id) = waiting(false);
        assert_eq!(
            s.set_state(&id, S::Assigned, None, true, 30),
            Err(TicketError::UseAssign)
        );
        assert_eq!(
            s.set_state(&id, S::Rejected, None, true, 30),
            Err(TicketError::UseReject)
        );
        let t = in_progress(&mut s, "k2", "i gang", false);
        assert_eq!(
            s.set_state(&t.id, S::Waiting, None, true, 30),
            Err(TicketError::IllegalTransition {
                from: S::InProgress,
                to: S::Waiting
            })
        );
        assert_invariants(&s);
    }

    #[test]
    fn list_for_agent_includes_waiting() {
        let (mut s, _) = svc();
        let (p, _, _) = parent_with_children(&mut s, false);
        s.submit_by_agent("k", None, "fordelt", 20).unwrap();
        let cur = in_progress(&mut s, "k", "i gang", false);
        let q = mk(&mut s, "i kø", 21);
        s.assign(&q.id, "k", 22).unwrap();
        let ids: Vec<String> = s.list_for_agent("k").into_iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![cur.id, p.id.clone(), q.id]);
        let w = s.list_for_agent("k").remove(1);
        assert_eq!(w.state, S::Waiting);
        assert_eq!((w.parent_id, w.blocked_by.len()), (None, 0));
        // The children carry their parent in the summary.
        let kids: Vec<_> = s
            .list()
            .into_iter()
            .filter(|t| t.parent_id.as_deref() == Some(p.id.as_str()))
            .collect();
        assert_eq!(kids.len(), 2);
    }

    #[test]
    fn find_open_by_title_normalises() {
        let (mut s, _) = svc();
        assert_eq!(norm_title("  Byg \t SPIL\n "), "byg spil");
        assert_eq!(norm_title("ÆBLE  Øl"), "æble øl");
        let p = Some(ProjectRef::Existing("Snake".into()));
        let a = s
            .create_in("  Byg  SPIL ", "", false, p.clone(), 1)
            .unwrap();
        let b = s
            .create_in(
                "byg spil",
                "",
                false,
                Some(ProjectRef::New {
                    new: "snake".into(),
                }),
                2,
            )
            .unwrap();
        let other = s
            .create_in(
                "byg spil",
                "",
                false,
                Some(ProjectRef::Existing("andet".into())),
                3,
            )
            .unwrap();
        let none = s.create("Byg spil", "", false, 4).unwrap();
        let done = s.create_in("byg spil", "", false, p.clone(), 5).unwrap();
        finish(&mut s, &done.id, "a", 6);
        let norm = norm_title("BYG   spil");
        let ids = |v: Vec<Ticket>| v.into_iter().map(|t| t.id).collect::<Vec<_>>();
        assert_eq!(
            ids(s.find_open_by_title(&norm, Some(&ProjectRef::Existing("snake".into())))),
            vec![a.id.clone(), b.id.clone()]
        );
        assert_eq!(
            ids(s.find_open_by_title(&norm, Some(&ProjectRef::Existing("andet".into())))),
            vec![other.id]
        );
        assert_eq!(ids(s.find_open_by_title(&norm, None)), vec![none.id]);
        assert!(s.find_open_by_title("byg", p.as_ref()).is_empty());
    }

    // ---- step 6a, Batch 2: relation_effects (plan A.5) ----

    fn rs(
        id: &str,
        state: S,
        parent: Option<&str>,
        blocked: &[&str],
        who: Option<&str>,
    ) -> RelSnap {
        RelSnap {
            id: id.into(),
            state,
            parent_id: parent.map(str::to_string),
            blocked_by: blocked.iter().map(|b| b.to_string()).collect(),
            assignee: who.map(str::to_string),
        }
    }

    #[test]
    fn relation_effects_table() {
        let set = |v: &[&str]| {
            v.iter()
                .map(|x| x.to_string())
                .collect::<BTreeSet<String>>()
        };
        let parent = rs("p", S::Waiting, None, &[], Some("k"));
        let open = rs("c1", S::Review, Some("p"), &[], Some("a"));
        let done = rs("c1", S::Done, Some("p"), &[], Some("a"));
        let other_open = rs("c2", S::Assigned, Some("p"), &[], Some("b"));

        // A child Done under a waiting parent → wake its assignee (also when others are open).
        let fx = relation_effects(
            &[parent.clone(), open.clone(), other_open.clone()],
            &[parent.clone(), done.clone(), other_open.clone()],
        );
        assert_eq!(fx.wake, set(&["k"]));
        assert_eq!(fx.woken, vec![("p".to_string(), "k".to_string())]);
        assert!(fx.unblocked.is_empty() && fx.children_done.is_empty());
        // The last open child deleted → wake.
        let fx = relation_effects(
            &[parent.clone(), open.clone()],
            std::slice::from_ref(&parent),
        );
        assert_eq!(fx.wake, set(&["k"]));
        // A child rejected / back in the backlog / reopened stays open → nothing.
        for after in [
            rs("c1", S::Assigned, Some("p"), &[], Some("a")),
            rs("c1", S::Backlog, Some("p"), &[], None),
            rs("c1", S::Rejected, Some("p"), &[], Some("a")),
        ] {
            let fx = relation_effects(&[parent.clone(), open.clone()], &[parent.clone(), after]);
            assert!(fx.is_empty(), "{fx:?}");
            assert_eq!(fx, RelationEffects::default());
        }
        let fx = relation_effects(
            &[parent.clone(), done.clone()],
            &[parent.clone(), rs("c1", S::Backlog, Some("p"), &[], None)],
        );
        assert!(fx.is_empty(), "reopened child");
        // The parent is not waiting (in progress) → no wake.
        let busy = rs("p", S::InProgress, None, &[], Some("k"));
        let fx = relation_effects(&[busy.clone(), open.clone()], &[busy, done.clone()]);
        assert!(fx.is_empty());
        // Unchanged → nothing.
        let all = [parent.clone(), open.clone(), other_open.clone()];
        assert!(relation_effects(&all, &all).is_empty());

        // A blocker Done → the queued ticket's agent; a blocker deleted → the same.
        let blocker = rs("b1", S::Review, None, &[], Some("x"));
        let blocker_done = rs("b1", S::Done, None, &[], Some("x"));
        let queued = rs("q", S::Assigned, None, &["b1"], Some("a"));
        let fx = relation_effects(
            &[blocker.clone(), queued.clone()],
            &[blocker_done.clone(), queued.clone()],
        );
        assert_eq!(fx.unblocked, set(&["a"]));
        assert_eq!(fx.freed, vec![("q".to_string(), "a".to_string())]);
        assert!(fx.wake.is_empty());
        let fx = relation_effects(
            &[blocker.clone(), queued.clone()],
            std::slice::from_ref(&queued),
        );
        assert_eq!(fx.unblocked, set(&["a"]));
        // Still blocked by another open ticket → nothing.
        let two = rs("q", S::Assigned, None, &["b1", "b2"], Some("a"));
        let b2 = rs("b2", S::InProgress, None, &[], Some("y"));
        let fx = relation_effects(
            &[blocker.clone(), b2.clone(), two.clone()],
            &[blocker_done.clone(), b2, two],
        );
        assert!(fx.is_empty());
        // A blocked backlog ticket (no agent) → nothing to notify.
        let loose = rs("q", S::Backlog, None, &["b1"], None);
        let fx = relation_effects(&[blocker, loose.clone()], &[blocker_done, loose]);
        assert!(fx.is_empty());

        // A backlog parent without an assignee loses its last open child → children_done.
        let lonely = rs("p", S::Backlog, None, &[], None);
        let fx = relation_effects(&[lonely.clone(), open.clone()], &[lonely.clone(), done]);
        assert_eq!(fx.children_done, vec!["p".to_string()]);
        assert!(fx.wake.is_empty() && fx.unblocked.is_empty());
        let fx = relation_effects(&[lonely.clone(), open], &[lonely]);
        assert_eq!(fx.children_done, vec!["p".to_string()]);
    }

    #[test]
    fn child_lines_and_reviews_list_children_oldest_first() {
        let (mut s, _) = svc();
        let p = s.create("Forælder", "", false, 1).unwrap();
        let b = s.create("Blokering", "", false, 2).unwrap();
        let c1 = s
            .create_by_agent_related("Plan", "", false, None, None, Some(p.id.clone()), vec![], 3)
            .unwrap();
        let c2 = s
            .create_by_agent_related(
                "Byg",
                "",
                false,
                None,
                None,
                Some(p.short_id()),
                vec![b.short_id()],
                4,
            )
            .unwrap();
        let lines = s.child_lines(&p.id);
        assert_eq!(lines.len(), 2);
        assert_eq!(
            (lines[0].short.as_str(), lines[0].title.as_str()),
            (c1.short_id().as_str(), "Plan")
        );
        assert!(lines[0].open_blockers.is_empty());
        assert_eq!(lines[1].short, c2.short_id());
        assert_eq!(lines[1].open_blockers, vec![b.short_id()]);
        assert_eq!(lines[1].state, S::Backlog);
        let reviews = s.child_reviews(&p.id);
        assert_eq!(reviews.len(), 2);
        assert_eq!(reviews[1].summary, None);
        assert_eq!(s.children(&p.id).len(), 2);
        assert!(s.child_lines(&c1.id).is_empty());
    }
}
