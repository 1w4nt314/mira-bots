//! The pure ticket state machine (plan B.3). `transition` never touches the store, the queue
//! order (it only clears `queue_position`) or anything outside the one ticket.
//!
//! Events: Assign, Unassign, Dispatched, Submit, Approve, Reject, Requeue, ToBacklog, Reopen,
//! Handoff, and (step 6a) Wait / Resume. Whether a submit becomes `Wait` (open children) or
//! `Submit` is decided by the service, which knows the other tickets.

use super::model::{Ticket, TicketActor, TicketError, TicketHistoryEntry, TicketState};

/// History note when a done ticket is moved back to the backlog without a note.
pub const REOPENED_NOTE: &str = "genåbnet";
/// History note when a ticket in progress is put back in the backlog (`Unassign`) without a note.
pub const RETURNED_NOTE: &str = "lagt tilbage";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TicketEvent {
    /// backlog → assigned (the service places it in the queue).
    Assign { agent_id: String },
    /// assigned → backlog; also inProgress | waiting → backlog ("lagt tilbage", step 5c), by the
    /// user or an agent, never by the system (the service checks that an agent is the assignee).
    Unassign,
    /// assigned → inProgress (delivered to the agent).
    Dispatched,
    /// inProgress | waiting → review, or done when `skip_review`.
    Submit,
    /// review → done.
    Approve,
    /// review → rejected; the note must not be blank.
    Reject { note: String },
    /// rejected → assigned (same agent).
    Requeue,
    /// assigned | inProgress | review | rejected | done | waiting → backlog.
    ToBacklog { note: Option<String> },
    /// review → inProgress (same agent continues).
    Reopen,
    /// inProgress → assigned with another agent (step 5c handoff): by the user or an agent,
    /// never by the system; the service checks that an agent is the current assignee and that
    /// the target is live. The rejection note and the review round stay; `issue` is cleared.
    Handoff { to_agent_id: String },
    /// inProgress → waiting (step 6a): the assignee submitted a parent with open children. The
    /// service decides from the children; the assignee, summary, review round and rejection note
    /// stay, `issue` is cleared.
    Wait,
    /// waiting → inProgress (step 6a): the assignee was woken; the service checks that it has no
    /// other ticket in progress. `issue` is cleared.
    Resume,
}

impl TicketEvent {
    /// The state this event normally leads to (also used in `IllegalTransition`).
    pub fn nominal_target(&self) -> TicketState {
        match self {
            TicketEvent::Assign { .. } | TicketEvent::Requeue | TicketEvent::Handoff { .. } => {
                TicketState::Assigned
            }
            TicketEvent::Unassign | TicketEvent::ToBacklog { .. } => TicketState::Backlog,
            TicketEvent::Dispatched | TicketEvent::Reopen | TicketEvent::Resume => {
                TicketState::InProgress
            }
            TicketEvent::Wait => TicketState::Waiting,
            TicketEvent::Submit => TicketState::Review,
            TicketEvent::Approve => TicketState::Done,
            TicketEvent::Reject { .. } => TicketState::Rejected,
        }
    }
}

/// Applies `ev` to `t`. Returns the new ticket (with `updated_at = now` and one new history entry)
/// or `IllegalTransition` / `NeedsNote`.
pub fn transition(
    t: &Ticket,
    ev: &TicketEvent,
    by: TicketActor,
    now: u64,
) -> Result<Ticket, TicketError> {
    transition_noted(t, ev, by, None, now)
}

/// Like [`transition`], with an extra history note for events that carry none themselves
/// (a `ToBacklog` note takes precedence; for `Reject` the extra note prefixes the rejection
/// text: `"<extra>: <note>"`).
pub fn transition_noted(
    t: &Ticket,
    ev: &TicketEvent,
    by: TicketActor,
    extra_note: Option<String>,
    now: u64,
) -> Result<Ticket, TicketError> {
    use TicketEvent as E;
    use TicketState as S;

    let from = t.state;
    let illegal = || TicketError::IllegalTransition {
        from,
        to: ev.nominal_target(),
    };
    let mut n = t.clone();
    let mut note = extra_note;

    let to = match (from, ev) {
        (S::Backlog, E::Assign { agent_id }) => {
            n.assignee_agent_id = Some(agent_id.clone());
            n.queue_position = None;
            S::Assigned
        }
        (S::Assigned, E::Unassign) => {
            clear_assignment(&mut n);
            S::Backlog
        }
        // Step 5c: the assignee (or the user) gives a ticket in progress back or away. The
        // system never does this; it uses `ToBacklog` with a reason.
        (S::InProgress | S::Waiting, E::Unassign) if by != TicketActor::System => {
            clear_assignment(&mut n);
            note = note.or_else(|| Some(RETURNED_NOTE.to_string()));
            S::Backlog
        }
        (S::InProgress, E::Handoff { to_agent_id }) if by != TicketActor::System => {
            if t.assignee_agent_id.as_deref() == Some(to_agent_id.as_str()) {
                return Err(TicketError::HandoffToSelf);
            }
            n.assignee_agent_id = Some(to_agent_id.clone());
            // Placed by the service (`normalize_queues`: last, like an ordinary assignment).
            n.queue_position = None;
            n.issue = None;
            S::Assigned
        }
        (S::Assigned, E::Dispatched) => {
            n.queue_position = None;
            n.issue = None;
            S::InProgress
        }
        // Step 6a: a waiting parent is submitted by the user (override) or by its assignee once
        // all children are done; same rules as from in progress.
        (S::InProgress | S::Waiting, E::Submit) => {
            // A finished turn supersedes an earlier turn failure.
            n.issue = None;
            // A new review starts without a reviewer; routing decides (plan5 A.6).
            n.reviewer_agent_id = None;
            n.escalated = false;
            if t.skip_review {
                n.rejection_note = None;
                S::Done
            } else {
                S::Review
            }
        }
        (S::Review, E::Approve) => {
            n.rejection_note = None;
            n.issue = None;
            // The reviewer stays for display; an escalation is settled.
            n.escalated = false;
            S::Done
        }
        (S::Review, E::Reject { note: r }) => {
            let r = r.trim();
            if r.is_empty() {
                return Err(TicketError::NeedsNote);
            }
            n.rejection_note = Some(r.to_string());
            // An extra note (e.g. "afvist af <agent>") prefixes the rejection text.
            note = Some(match note {
                Some(prefix) => format!("{prefix}: {r}"),
                None => r.to_string(),
            });
            n.review_round = t.review_round.saturating_add(1);
            n.reviewer_agent_id = None;
            n.escalated = false;
            S::Rejected
        }
        (S::Rejected, E::Requeue) => {
            n.queue_position = None;
            S::Assigned
        }
        (
            S::Assigned | S::InProgress | S::Review | S::Rejected | S::Done | S::Waiting,
            E::ToBacklog { note: b },
        ) => {
            clear_assignment(&mut n);
            note = match (b, from) {
                (Some(b), _) => Some(b.clone()),
                (None, S::Done) => Some(REOPENED_NOTE.to_string()),
                (None, _) => note,
            };
            S::Backlog
        }
        (S::Review, E::Reopen) => {
            n.reviewer_agent_id = None;
            n.escalated = false;
            S::InProgress
        }
        // Step 6a: assignee, summary, review round and rejection note stay.
        (S::InProgress, E::Wait) => {
            n.issue = None;
            S::Waiting
        }
        (S::Waiting, E::Resume) => {
            n.issue = None;
            S::InProgress
        }
        _ => return Err(illegal()),
    };

    n.state = to;
    if to != S::Assigned {
        n.queue_position = None;
    }
    n.updated_at = now;
    n.history.push(TicketHistoryEntry {
        at: now,
        from: Some(from),
        to,
        by,
        note,
    });
    Ok(n)
}

fn clear_assignment(t: &mut Ticket) {
    t.assignee_agent_id = None;
    t.queue_position = None;
    t.issue = None;
    // Back to the backlog = a fresh start for review too (plan5 C.9).
    t.review_round = 0;
    t.escalated = false;
    t.reviewer_agent_id = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tickets::model::test_support::ticket;
    use crate::tickets::model::TicketIssue;
    use TicketState as S;

    fn events() -> Vec<TicketEvent> {
        vec![
            TicketEvent::Assign {
                agent_id: "a1".into(),
            },
            TicketEvent::Unassign,
            TicketEvent::Dispatched,
            TicketEvent::Submit,
            TicketEvent::Approve,
            TicketEvent::Reject {
                note: "mangler test".into(),
            },
            TicketEvent::Requeue,
            TicketEvent::ToBacklog { note: None },
            TicketEvent::Reopen,
            TicketEvent::Handoff {
                to_agent_id: "a2".into(),
            },
            TicketEvent::Wait,
            TicketEvent::Resume,
        ]
    }

    /// A ticket in `state` that looks like it got there legally (assignee where one belongs).
    fn in_state(state: TicketState) -> Ticket {
        let mut t = ticket("00000000-0000-4000-8000-000000000001", state);
        if matches!(
            state,
            S::Assigned | S::InProgress | S::Review | S::Rejected | S::Waiting
        ) {
            t.assignee_agent_id = Some("a1".into());
        }
        if state == S::Assigned {
            t.queue_position = Some(0);
        }
        t
    }

    #[test]
    fn full_transition_table() {
        // Rows: from-state in TicketState::ALL order; columns: events() order.
        // Some(target) = legal, None = IllegalTransition.
        // Columns: Assign, Unassign, Dispatched, Submit, Approve, Reject, Requeue, ToBacklog,
        // Reopen, Handoff, Wait, Resume.
        let table: [(S, [Option<S>; 12]); 7] = [
            (
                S::Backlog,
                [
                    Some(S::Assigned),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                ],
            ),
            (
                S::Assigned,
                [
                    None,
                    Some(S::Backlog),
                    Some(S::InProgress),
                    None,
                    None,
                    None,
                    None,
                    Some(S::Backlog),
                    None,
                    None,
                    None,
                    None,
                ],
            ),
            (
                S::InProgress,
                [
                    None,
                    Some(S::Backlog),
                    None,
                    Some(S::Review),
                    None,
                    None,
                    None,
                    Some(S::Backlog),
                    None,
                    Some(S::Assigned),
                    Some(S::Waiting),
                    None,
                ],
            ),
            (
                S::Review,
                [
                    None,
                    None,
                    None,
                    None,
                    Some(S::Done),
                    Some(S::Rejected),
                    None,
                    Some(S::Backlog),
                    Some(S::InProgress),
                    None,
                    None,
                    None,
                ],
            ),
            (
                S::Done,
                [
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some(S::Backlog),
                    None,
                    None,
                    None,
                    None,
                ],
            ),
            (
                S::Rejected,
                [
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some(S::Assigned),
                    Some(S::Backlog),
                    None,
                    None,
                    None,
                    None,
                ],
            ),
            (
                S::Waiting,
                [
                    None,
                    Some(S::Backlog),
                    None,
                    Some(S::Review),
                    None,
                    None,
                    None,
                    Some(S::Backlog),
                    None,
                    None,
                    None,
                    Some(S::InProgress),
                ],
            ),
        ];
        let evs = events();
        let mut cases = 0;
        for (from, row) in table {
            for (ev, want) in evs.iter().zip(row) {
                cases += 1;
                let t = in_state(from);
                let got = transition(&t, ev, TicketActor::User, 5_000);
                match want {
                    Some(to) => {
                        let n = got.unwrap_or_else(|e| panic!("{from:?} {ev:?}: {e}"));
                        assert_eq!(n.state, to, "{from:?} {ev:?}");
                        assert_eq!(n.updated_at, 5_000);
                        assert_eq!(n.created_at, t.created_at);
                        assert_eq!(n.history.len(), t.history.len() + 1);
                        let h = n.history.last().unwrap();
                        assert_eq!((h.from, h.to, h.at), (Some(from), to, 5_000));
                    }
                    None => assert_eq!(
                        got,
                        Err(TicketError::IllegalTransition {
                            from,
                            to: ev.nominal_target()
                        }),
                        "{from:?} {ev:?}"
                    ),
                }
            }
        }
        assert_eq!(cases, 84);
    }

    #[test]
    fn submit_follows_skip_review() {
        let mut t = in_state(S::InProgress);
        t.issue = Some(TicketIssue::TurnFailed);
        t.rejection_note = Some("old".into());
        let r = transition(&t, &TicketEvent::Submit, TicketActor::System, 2).unwrap();
        assert_eq!(r.state, S::Review);
        assert_eq!(r.issue, None);
        assert_eq!(r.rejection_note.as_deref(), Some("old"));
        assert_eq!(r.assignee_agent_id.as_deref(), Some("a1"));

        t.skip_review = true;
        let d = transition(&t, &TicketEvent::Submit, TicketActor::System, 2).unwrap();
        assert_eq!(d.state, S::Done);
        assert_eq!(d.rejection_note, None);
        assert_eq!(d.issue, None);
    }

    #[test]
    fn reject_requires_a_note() {
        let t = in_state(S::Review);
        for blank in ["", "   ", "\n"] {
            assert_eq!(
                transition(
                    &t,
                    &TicketEvent::Reject { note: blank.into() },
                    TicketActor::User,
                    2
                ),
                Err(TicketError::NeedsNote)
            );
        }
        let r = transition(
            &t,
            &TicketEvent::Reject {
                note: "  mangler test ".into(),
            },
            TicketActor::User,
            2,
        )
        .unwrap();
        assert_eq!(r.rejection_note.as_deref(), Some("mangler test"));
        assert_eq!(
            r.history.last().unwrap().note.as_deref(),
            Some("mangler test")
        );
    }

    #[test]
    fn history_records_from_to_by_and_note() {
        let t = in_state(S::InProgress);
        let n = transition(
            &t,
            &TicketEvent::ToBacklog {
                note: Some("agent stoppet".into()),
            },
            TicketActor::System,
            9,
        )
        .unwrap();
        assert_eq!(
            n.history.last().unwrap(),
            &TicketHistoryEntry {
                at: 9,
                from: Some(S::InProgress),
                to: S::Backlog,
                by: TicketActor::System,
                note: Some("agent stoppet".into()),
            }
        );
        let d = transition_noted(
            &in_state(S::Assigned),
            &TicketEvent::Dispatched,
            TicketActor::System,
            Some("sendt til demo".into()),
            10,
        )
        .unwrap();
        assert_eq!(
            d.history.last().unwrap().note.as_deref(),
            Some("sendt til demo")
        );
        assert_eq!(d.history.last().unwrap().by, TicketActor::System);
    }

    #[test]
    fn rejection_note_survives_requeue_and_clears_on_approve() {
        let t = in_state(S::Review);
        let rej = transition(
            &t,
            &TicketEvent::Reject { note: "nej".into() },
            TicketActor::User,
            2,
        )
        .unwrap();
        let req = transition(&rej, &TicketEvent::Requeue, TicketActor::System, 3).unwrap();
        assert_eq!(req.state, S::Assigned);
        assert_eq!(req.rejection_note.as_deref(), Some("nej"));
        assert_eq!(req.assignee_agent_id.as_deref(), Some("a1"));
        let dis = transition(&req, &TicketEvent::Dispatched, TicketActor::System, 4).unwrap();
        let sub = transition(&dis, &TicketEvent::Submit, TicketActor::System, 5).unwrap();
        assert_eq!(sub.rejection_note.as_deref(), Some("nej"));
        let ok = transition(&sub, &TicketEvent::Approve, TicketActor::User, 6).unwrap();
        assert_eq!(ok.rejection_note, None);
        assert_eq!(ok.created_at, t.created_at);
        assert_eq!(ok.history.len(), t.history.len() + 5);
    }

    #[test]
    fn assignment_fields_follow_the_state() {
        let a = transition(
            &in_state(S::Backlog),
            &TicketEvent::Assign {
                agent_id: "a9".into(),
            },
            TicketActor::User,
            2,
        )
        .unwrap();
        assert_eq!(a.assignee_agent_id.as_deref(), Some("a9"));
        assert_eq!(a.queue_position, None);

        let mut q = in_state(S::Assigned);
        q.issue = Some(TicketIssue::DeliveryFailed);
        let d = transition(&q, &TicketEvent::Dispatched, TicketActor::System, 3).unwrap();
        assert_eq!((d.queue_position, d.issue), (None, None));
        assert_eq!(d.assignee_agent_id.as_deref(), Some("a1"));

        let u = transition(&q, &TicketEvent::Unassign, TicketActor::User, 3).unwrap();
        assert_eq!(
            (u.assignee_agent_id, u.queue_position, u.issue),
            (None, None, None)
        );

        let mut done = in_state(S::Done);
        done.assignee_agent_id = Some("a1".into());
        let b = transition(
            &done,
            &TicketEvent::ToBacklog { note: None },
            TicketActor::User,
            4,
        )
        .unwrap();
        assert_eq!(b.assignee_agent_id, None);
        assert_eq!(
            b.history.last().unwrap().note.as_deref(),
            Some(REOPENED_NOTE)
        );

        let r = transition(
            &in_state(S::Review),
            &TicketEvent::Reopen,
            TicketActor::User,
            5,
        )
        .unwrap();
        assert_eq!(r.assignee_agent_id.as_deref(), Some("a1"));
    }

    #[test]
    fn reject_counts_rounds_and_backlog_resets() {
        let mut t = in_state(S::Review);
        t.reviewer_agent_id = Some("rev".into());
        t.escalated = true;
        let r = transition_noted(
            &t,
            &TicketEvent::Reject {
                note: " mangler test ".into(),
            },
            TicketActor::Agent,
            Some("afvist af bot".into()),
            2,
        )
        .unwrap();
        assert_eq!(
            (r.review_round, r.escalated, r.reviewer_agent_id.clone()),
            (1, false, None)
        );
        assert_eq!(r.rejection_note.as_deref(), Some("mangler test"));
        assert_eq!(
            r.history.last().unwrap().note.as_deref(),
            Some("afvist af bot: mangler test")
        );
        let r2 = transition(&r, &TicketEvent::Requeue, TicketActor::System, 3).unwrap();
        assert_eq!(r2.review_round, 1, "requeue keeps the round");
        let b = transition(
            &r2,
            &TicketEvent::ToBacklog { note: None },
            TicketActor::User,
            4,
        )
        .unwrap();
        assert_eq!(
            (b.review_round, b.escalated, b.reviewer_agent_id),
            (0, false, None)
        );

        // Approve keeps the reviewer (display) and settles an escalation.
        let mut t = in_state(S::Review);
        t.reviewer_agent_id = Some("rev".into());
        t.escalated = true;
        t.review_round = 3;
        let a = transition(&t, &TicketEvent::Approve, TicketActor::User, 5).unwrap();
        assert_eq!(
            (a.review_round, a.escalated, a.reviewer_agent_id.as_deref()),
            (3, false, Some("rev"))
        );
        // Entering review again starts without a reviewer.
        let mut p = in_state(S::InProgress);
        p.reviewer_agent_id = Some("old".into());
        p.review_round = 2;
        let s = transition(&p, &TicketEvent::Submit, TicketActor::Agent, 6).unwrap();
        assert_eq!(
            (s.state, s.reviewer_agent_id, s.review_round),
            (S::Review, None, 2)
        );
    }

    #[test]
    fn handoff_from_in_progress_per_actor_and_state() {
        let ev = TicketEvent::Handoff {
            to_agent_id: "a2".into(),
        };
        // User and agent may; the system never hands off.
        for by in [TicketActor::User, TicketActor::Agent] {
            let mut t = in_state(S::InProgress);
            t.issue = Some(TicketIssue::NotSubmitted);
            t.rejection_note = Some("mangler test".into());
            t.review_round = 2;
            let h = transition_noted(&t, &ev, by, Some("overdraget fra A til B".into()), 7)
                .unwrap_or_else(|e| panic!("{by:?}: {e}"));
            assert_eq!(h.state, S::Assigned);
            assert_eq!(h.assignee_agent_id.as_deref(), Some("a2"));
            assert_eq!((h.queue_position, h.issue), (None, None));
            assert_eq!(h.rejection_note.as_deref(), Some("mangler test"));
            assert_eq!(h.review_round, 2);
            let last = h.history.last().unwrap();
            assert_eq!(
                (last.from, last.to, last.by, last.note.as_deref()),
                (
                    Some(S::InProgress),
                    S::Assigned,
                    by,
                    Some("overdraget fra A til B")
                )
            );
        }
        assert_eq!(
            transition(&in_state(S::InProgress), &ev, TicketActor::System, 7),
            Err(TicketError::IllegalTransition {
                from: S::InProgress,
                to: S::Assigned
            })
        );
        // Never to the agent that already has it.
        let same = TicketEvent::Handoff {
            to_agent_id: "a1".into(),
        };
        assert_eq!(
            transition(&in_state(S::InProgress), &same, TicketActor::Agent, 7),
            Err(TicketError::HandoffToSelf)
        );
        // Only from in progress: review, done, assigned, rejected and backlog refuse it.
        for from in [S::Backlog, S::Assigned, S::Review, S::Done, S::Rejected] {
            for by in [TicketActor::User, TicketActor::Agent] {
                assert_eq!(
                    transition(&in_state(from), &ev, by, 7),
                    Err(TicketError::IllegalTransition {
                        from,
                        to: S::Assigned
                    }),
                    "{from:?} {by:?}"
                );
            }
        }
    }

    #[test]
    fn unassign_from_in_progress_puts_it_back() {
        for by in [TicketActor::User, TicketActor::Agent] {
            let mut t = in_state(S::InProgress);
            t.review_round = 1;
            let b = transition(&t, &TicketEvent::Unassign, by, 8).unwrap();
            assert_eq!(b.state, S::Backlog);
            assert_eq!(b.assignee_agent_id, None);
            assert_eq!(b.review_round, 0);
            assert_eq!(
                b.history.last().unwrap().note.as_deref(),
                Some(RETURNED_NOTE)
            );
            // A given note wins.
            let n = transition_noted(&t, &TicketEvent::Unassign, by, Some("forkert".into()), 8)
                .unwrap();
            assert_eq!(n.history.last().unwrap().note.as_deref(), Some("forkert"));
        }
        assert_eq!(
            transition(
                &in_state(S::InProgress),
                &TicketEvent::Unassign,
                TicketActor::System,
                8
            ),
            Err(TicketError::IllegalTransition {
                from: S::InProgress,
                to: S::Backlog
            })
        );
        // An ordinary unassign from the queue keeps having no note.
        let q = transition(
            &in_state(S::Assigned),
            &TicketEvent::Unassign,
            TicketActor::Agent,
            8,
        )
        .unwrap();
        assert_eq!(q.history.last().unwrap().note, None);
    }

    #[test]
    fn wait_keeps_assignee_summary_and_round_and_clears_issue() {
        let mut t = in_state(S::InProgress);
        t.summary = Some("plan fordelt".into());
        t.review_round = 2;
        t.rejection_note = Some("mangler test".into());
        t.reviewer_agent_id = Some("rev".into());
        t.issue = Some(TicketIssue::NotSubmitted);
        let w = transition_noted(
            &t,
            &TicketEvent::Wait,
            TicketActor::Agent,
            Some("venter på del-tickets (2)".into()),
            7,
        )
        .unwrap();
        assert_eq!(w.state, S::Waiting);
        assert_eq!(w.assignee_agent_id.as_deref(), Some("a1"));
        assert_eq!(w.summary.as_deref(), Some("plan fordelt"));
        assert_eq!(w.review_round, 2);
        assert_eq!(w.rejection_note.as_deref(), Some("mangler test"));
        assert_eq!(w.reviewer_agent_id.as_deref(), Some("rev"));
        assert_eq!((w.issue, w.queue_position), (None, None));
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
        // Only from in progress.
        for from in [
            S::Backlog,
            S::Assigned,
            S::Review,
            S::Done,
            S::Rejected,
            S::Waiting,
        ] {
            assert_eq!(
                transition(&in_state(from), &TicketEvent::Wait, TicketActor::System, 7),
                Err(TicketError::IllegalTransition {
                    from,
                    to: S::Waiting
                }),
                "{from:?}"
            );
        }
    }

    #[test]
    fn resume_clears_issue() {
        let mut t = in_state(S::Waiting);
        t.issue = Some(TicketIssue::TurnFailed);
        t.summary = Some("s".into());
        let r = transition_noted(
            &t,
            &TicketEvent::Resume,
            TicketActor::System,
            Some("vækket: del-ticket godkendt".into()),
            8,
        )
        .unwrap();
        assert_eq!(r.state, S::InProgress);
        assert_eq!(r.issue, None);
        assert_eq!(r.assignee_agent_id.as_deref(), Some("a1"));
        assert_eq!(r.summary.as_deref(), Some("s"));
        assert_eq!(
            r.history.last().unwrap().note.as_deref(),
            Some("vækket: del-ticket godkendt")
        );
        for from in [
            S::Backlog,
            S::Assigned,
            S::InProgress,
            S::Review,
            S::Done,
            S::Rejected,
        ] {
            assert_eq!(
                transition(
                    &in_state(from),
                    &TicketEvent::Resume,
                    TicketActor::System,
                    8
                ),
                Err(TicketError::IllegalTransition {
                    from,
                    to: S::InProgress
                }),
                "{from:?}"
            );
        }
    }

    #[test]
    fn system_cannot_unassign_waiting() {
        assert_eq!(
            transition(
                &in_state(S::Waiting),
                &TicketEvent::Unassign,
                TicketActor::System,
                9
            ),
            Err(TicketError::IllegalTransition {
                from: S::Waiting,
                to: S::Backlog
            })
        );
        for by in [TicketActor::User, TicketActor::Agent] {
            let b = transition(&in_state(S::Waiting), &TicketEvent::Unassign, by, 9).unwrap();
            assert_eq!(b.state, S::Backlog);
            assert_eq!(b.assignee_agent_id, None);
            assert_eq!(
                b.history.last().unwrap().note.as_deref(),
                Some(RETURNED_NOTE)
            );
        }
        // The system moves a waiting parent with ToBacklog and a reason.
        let s = transition(
            &in_state(S::Waiting),
            &TicketEvent::ToBacklog {
                note: Some("agent stoppet".into()),
            },
            TicketActor::System,
            9,
        )
        .unwrap();
        assert_eq!((s.state, s.assignee_agent_id), (S::Backlog, None));
    }

    #[test]
    fn waiting_submit_respects_skip_review() {
        let mut t = in_state(S::Waiting);
        t.reviewer_agent_id = Some("old".into());
        t.escalated = true;
        t.rejection_note = Some("old".into());
        let r = transition(&t, &TicketEvent::Submit, TicketActor::Agent, 3).unwrap();
        assert_eq!(r.state, S::Review);
        assert_eq!((r.reviewer_agent_id, r.escalated), (None, false));
        assert_eq!(r.rejection_note.as_deref(), Some("old"));
        assert_eq!(r.assignee_agent_id.as_deref(), Some("a1"));

        t.skip_review = true;
        let d = transition(&t, &TicketEvent::Submit, TicketActor::User, 3).unwrap();
        assert_eq!(d.state, S::Done);
        assert_eq!(d.rejection_note, None);
    }
}
