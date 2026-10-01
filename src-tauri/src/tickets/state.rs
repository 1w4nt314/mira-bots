//! The pure ticket state machine (plan B.3). `transition` never touches the store, the queue
//! order (it only clears `queue_position`) or anything outside the one ticket.

use super::model::{Ticket, TicketActor, TicketError, TicketHistoryEntry, TicketState};

/// History note when a done ticket is moved back to the backlog without a note.
pub const REOPENED_NOTE: &str = "genåbnet";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TicketEvent {
    /// backlog → assigned (the service places it in the queue).
    Assign { agent_id: String },
    /// assigned → backlog.
    Unassign,
    /// assigned → inProgress (delivered to the agent).
    Dispatched,
    /// inProgress → review, or done when `skip_review`.
    Submit,
    /// review → done.
    Approve,
    /// review → rejected; the note must not be blank.
    Reject { note: String },
    /// rejected → assigned (same agent).
    Requeue,
    /// assigned | inProgress | review | rejected | done → backlog.
    ToBacklog { note: Option<String> },
    /// review → inProgress (same agent continues).
    Reopen,
}

impl TicketEvent {
    /// The state this event normally leads to (also used in `IllegalTransition`).
    pub fn nominal_target(&self) -> TicketState {
        match self {
            TicketEvent::Assign { .. } | TicketEvent::Requeue => TicketState::Assigned,
            TicketEvent::Unassign | TicketEvent::ToBacklog { .. } => TicketState::Backlog,
            TicketEvent::Dispatched | TicketEvent::Reopen => TicketState::InProgress,
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
/// (`Reject` and `ToBacklog` notes take precedence).
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
        (S::Assigned, E::Dispatched) => {
            n.queue_position = None;
            n.issue = None;
            S::InProgress
        }
        (S::InProgress, E::Submit) => {
            // A finished turn supersedes an earlier turn failure.
            n.issue = None;
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
            S::Done
        }
        (S::Review, E::Reject { note: r }) => {
            let r = r.trim();
            if r.is_empty() {
                return Err(TicketError::NeedsNote);
            }
            n.rejection_note = Some(r.to_string());
            note = Some(r.to_string());
            S::Rejected
        }
        (S::Rejected, E::Requeue) => {
            n.queue_position = None;
            S::Assigned
        }
        (
            S::Assigned | S::InProgress | S::Review | S::Rejected | S::Done,
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
        (S::Review, E::Reopen) => S::InProgress,
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
        ]
    }

    /// A ticket in `state` that looks like it got there legally (assignee where one belongs).
    fn in_state(state: TicketState) -> Ticket {
        let mut t = ticket("00000000-0000-4000-8000-000000000001", state);
        if matches!(state, S::Assigned | S::InProgress | S::Review | S::Rejected) {
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
        let table: [(S, [Option<S>; 9]); 6] = [
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
                ],
            ),
            (
                S::InProgress,
                [
                    None,
                    None,
                    None,
                    Some(S::Review),
                    None,
                    None,
                    None,
                    Some(S::Backlog),
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
        assert_eq!(cases, 54);
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
}
