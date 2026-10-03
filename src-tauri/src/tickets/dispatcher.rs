//! The dispatcher: delivers the next queued ticket to an idle agent by writing the ticket file and
//! typing one line plus a separate Enter into its terminal, then waits for a confirmation
//! (plan A.4).
//!
//! `handle` is synchronous and processes one message at a time; every delay is a message the
//! dispatcher schedules to itself through [`Timers`], so tests run without a runtime
//! ([`FakeTimers`]). Each delivery sequence carries a token; timers with an outdated token are
//! ignored, so a cancelled sequence is never revived.
//!
//! Ports the app glue implements (plan B.8): [`AgentPort`] over the agent manager and
//! [`TicketsHost`] over the shared ticket service (`TicketsCtx::mutate`, which saves, syncs the
//! agents' ticket links and emits).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use super::model::{
    ReportAuthor, ReportAuthorKind, Ticket, TicketError, TicketGit, TicketIssue, TicketState,
    WorkspaceRules,
};
use super::prompt::{self, ReviewSender, TicketDelivery, WakeInfo};
use super::service::{DueWake, TicketService};
use crate::agent::roles::has_work_role;
use crate::agent::{now_ms, Role, SeatKind};
use crate::config::{
    CHILDREN_DONE_NOTE, CONFIRM_TIMEOUT_MS, DELIVERY_FAILED_TEXT, DISPATCH_DELAY_MS,
    ENTER_DELAY_MS, NOT_SUBMITTED_TEXT, RESTART_TIMEOUT_MS, RETRY_TIMEOUT_MS,
    SPAWN_CONFIRM_TIMEOUT_MS, TURN_FAILED_TEXT, WAKE_MAX_ATTEMPTS, WAKE_UNCONFIRMED_TEXT,
    WOKEN_NOTE,
};
use crate::events::StatusEvent;
use crate::hooks::status::AgentStatus;

/// History note when the delivery was not confirmed after the retry.
pub const DELIVERY_UNCONFIRMED_NOTE: &str = "levering ikke bekræftet";
/// History note on `StopFailure` while a ticket was in progress.
pub const TURN_FAILED_NOTE: &str = "StopFailure";
/// History note when the terminal could not be written.
pub const TERMINAL_GONE_NOTE: &str = "Terminalen er væk";
/// History note when a different prompt was submitted while the ticket line was being delivered.
pub const USER_TYPED_NOTE: &str = "brugeren skrev selv i terminalen";
/// History note after the "Bed om aflevering" line was typed (the ticket is otherwise unchanged).
pub const SUBMISSION_REQUESTED_NOTE: &str = "bedt om aflevering";
/// History note (by the app) when the restart before a delivery did not bring the agent back
/// within [`RESTART_TIMEOUT_MS`] (step 6b, plan A.7): the ticket goes to the agent's session as
/// it is once it is idle.
pub const RESTART_TIMEOUT_NOTE: &str =
    "ny session startede ikke inden for 60 s; ticketen leveres i agentens nuværende session";

/// History note (by the app) when the restart before a delivery was refused (step 6b): the
/// ticket is typed into the agent's current session.
pub fn restart_failed_note(err: &str) -> String {
    format!("ny session kunne ikke startes ({err}); ticketen leveres i agentens nuværende session")
}

/// No ticket is typed into a terminal the user typed into less than this long ago (their
/// half-written prompt would be merged with the ticket line); the dispatch waits instead. The
/// effective value is the workspace rule `userInputGraceMs` ([`TicketsHost::rules`]).
pub use crate::config::USER_INPUT_GRACE_MS;

/// What the dispatcher needs to know about an agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSnapshot {
    pub name: String,
    pub cwd: PathBuf,
    pub status: AgentStatus,
    /// Current detail text (to clear [`DELIVERY_FAILED_TEXT`]/[`TURN_FAILED_TEXT`]/
    /// [`NOT_SUBMITTED_TEXT`] after a successful delivery).
    pub detail: Option<String>,
    /// When the user last typed into the terminal ([`Timers::now_ms`] clock); see
    /// [`USER_INPUT_GRACE_MS`].
    pub last_user_input_at: Option<u64>,
    /// The agent's seat and roles: a ticket for an agent on a staff seat is delivered as a
    /// coordination task (5c C.1, [`TicketDelivery::for_agent`]).
    pub seat_kind: SeatKind,
    pub roles: Vec<Role>,
    /// The agent's project (plan4b A.1); `None` on a staff seat.
    pub project: Option<String>,
    /// The current session has had a turn (`AgentManager::has_conversation`): a fresh session
    /// per ticket restarts only such a session (step 6b, plan A.7); a new session has none.
    pub has_conversation: bool,
}

/// The restart the dispatcher asks for before a work delivery (step 6b, plan A.7): `cwd` is the
/// ticket's worktree when the agent must move there, or its project folder when it sits in
/// another ticket's worktree and this ticket has none (`None` keeps the cwd); `force_fresh`
/// starts a new session (never `--resume`); otherwise the session continues in the new cwd
/// (`--resume`, like "Flyt til projekt"). The agent keeps its project, model and effort.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestartForTicket {
    pub agent_id: String,
    pub cwd: Option<PathBuf>,
    pub force_fresh: bool,
    pub ticket_short: String,
}

/// The app's restart path for [`AgentPort::restart_fresh`] (`commands::restart_for_ticket`,
/// installed in `lib.rs` like the tools' `SpawnPort`).
pub type RestartPort = Arc<dyn Fn(RestartForTicket) -> Result<(), String> + Send + Sync>;

/// What [`fresh_decision`] says about a work delivery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FreshDecision {
    /// Type the ticket into the agent's current session.
    None,
    /// Restart first ([`RestartForTicket`]), then deliver at the next Idle.
    Restart {
        cwd: Option<PathBuf>,
        force_fresh: bool,
    },
}

/// Step 6b (plan A.7): whether a work delivery of `ticket` to the agent `snap` restarts it first.
/// Never for a staff seat or an agent without a work role (a coordination task). Otherwise:
/// - `needs_cwd`: the ticket's worktree (`git.worktree`, an existing folder) is not the agent's
///   cwd ([`crate::git::same_path`]) → restart in it; without one, an agent that sits in an app
///   worktree (`<project>/.mira-bots/wt/<short>`, [`crate::git::worktree_project`]) goes back
///   to the project folder (it must not work on another ticket's branch);
/// - `needs_fresh`: rule `freshSessionPerTicket`, the session has had a turn
///   (`has_conversation`; the first ticket after a spawn never restarts) and the ticket is not a
///   rejected one coming back (`review_round > 0` or a rejection note: the context is kept).
///
/// `Restart { cwd, force_fresh: needs_fresh }` when either holds; a rejected ticket that must
/// change folders continues its session there (`--resume`).
// TODO(windows-verify): a rejected ticket coming back to the same agent does not restart the
// session (the conversation continues, round 1) (plan6b D.102).
pub fn fresh_decision(
    rules: &WorkspaceRules,
    snap: &AgentSnapshot,
    ticket: &Ticket,
    git: Option<&TicketGit>,
) -> FreshDecision {
    if snap.seat_kind != SeatKind::Work || !has_work_role(&snap.roles) {
        return FreshDecision::None;
    }
    let cwd = git
        .and_then(|g| g.worktree.as_deref())
        .map(PathBuf::from)
        .filter(|wt| wt.is_dir())
        .or_else(|| crate::git::worktree_project(&snap.cwd).filter(|p| p.is_dir()))
        .filter(|want| {
            !crate::git::same_path(&want.to_string_lossy(), &snap.cwd.to_string_lossy())
        });
    let rejected_back = ticket.review_round > 0 || ticket.rejection_note.is_some();
    let force_fresh = rules.fresh_session_per_ticket && snap.has_conversation && !rejected_back;
    if cwd.is_none() && !force_fresh {
        return FreshDecision::None;
    }
    FreshDecision::Restart { cwd, force_fresh }
}

/// Access to the agents (implemented by `ManagerPort` over `Arc<Mutex<AgentManager>>`; each call
/// takes the manager lock briefly).
pub trait AgentPort: Send {
    /// `None` when the agent does not exist.
    fn snapshot(&self, id: &str) -> Option<AgentSnapshot>;
    fn write_input(&self, id: &str, bytes: &[u8]) -> Result<(), String>;
    /// Sets the agent's detail text; `false` when unknown or exited.
    fn set_detail(&self, id: &str, detail: Option<String>) -> bool;
    /// Names of the other live work agents in `id`'s project (plan4b A.5). Empty by default
    /// (test ports without projects).
    fn peers_in_project(&self, _id: &str) -> Vec<String> {
        Vec::new()
    }
    /// Whether a live agent has the coordinator role (step 6a: the planner's text). `false` by
    /// default (test ports).
    fn has_live_coordinator(&self) -> bool {
        false
    }
    /// Step 6b (plan A.7): restarts the idle agent before a work delivery (a fresh session and/or
    /// the ticket's worktree as cwd). Synchronous; the dispatcher holds no lock while calling
    /// it. On `Ok` the app sends [`DispatchMsg::AgentRestarting`] and, after the new session's
    /// SessionStart, [`DispatchMsg::AgentIdle`]. Refused by default (test ports, no app).
    fn restart_fresh(&self, _req: RestartForTicket) -> Result<(), String> {
        Err("ingen genstart i denne port".into())
    }
}

/// Access to the shared ticket service (implemented for the app's `TicketsCtx`).
pub trait TicketsHost: Send {
    /// Runs `f` under the service lock; on success the implementation syncs the agents' ticket
    /// links and emits `tickets-changed` (+ `agents-changed`), without holding any lock.
    fn mutate<T>(
        &self,
        f: impl FnOnce(&mut TicketService) -> Result<T, TicketError>,
    ) -> Result<T, String>;
    /// Read-only access under the service lock; no emits.
    fn read<T>(&self, f: impl FnOnce(&TicketService) -> T) -> T;
    /// Routes tickets in review without a reviewer (plan5 A.6; `TicketsCtx::route_reviews`).
    /// Called when an agent becomes idle and after an automatic move to review. No-op by
    /// default (tests that do not route).
    fn reroute_reviews(&self) {}
    /// The effective workspace rules (plan4b A.4; `TicketsCtx` reads the workspace file).
    /// The defaults from `config.rs` unless overridden.
    fn rules(&self) -> WorkspaceRules {
        WorkspaceRules::defaults()
    }
    /// The project ids under the projects root (plan4b A.6). Empty by default.
    fn project_ids(&self) -> Vec<String> {
        Vec::new()
    }
    /// Step 6b (plan A.4): the ticket's git branch for a real work delivery to an agent in
    /// `cwd` (`TicketsCtx::prepare_git`: an existing `ticket.git` is kept; with `git: worktree`
    /// the worktree is prepared and saved; any failure is a history note and `None`). Blocks
    /// for the git commands. `None` by default (git off).
    fn prepare_git(&self, _ticket: &Ticket, _cwd: &Path) -> Option<TicketGit> {
        None
    }
    /// Step 6b (plan punkt 16): the project's checks as "name: `run`" lines for the review file
    /// (`TicketsCtx` reads `<project>/.mira-bots/project.json`). Empty by default.
    fn project_checks(&self, _project: Option<&str>) -> Vec<String> {
        Vec::new()
    }
}

/// Schedules `msg` to be fed back into the dispatcher after `delay_ms`, and tells the time the
/// delays are measured on (wall clock in the app, the fake clock in tests).
pub trait Timers: Send {
    fn schedule(&mut self, delay_ms: u64, msg: DispatchMsg);
    /// Milliseconds since the epoch (compared with [`AgentSnapshot::last_user_input_at`]).
    fn now_ms(&self) -> u64;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimerKind {
    DispatchDelay,
    SendEnter,
    Confirm,
    /// "Bed om aflevering": waiting for the user-input grace before typing the line.
    NudgeDelay,
    /// "Bed om aflevering": line typed, Enter pending.
    NudgeEnter,
    /// Step 6b: the restart before a delivery did not bring the agent back in time
    /// ([`RESTART_TIMEOUT_MS`]).
    RestartTimeout,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchMsg {
    /// The agent is idle (SessionStart, Stop, StopFailure, idle notification).
    AgentIdle { agent_id: String },
    /// A busy status (thinking/reading/editing/running/waiting for permission).
    AgentBusy { agent_id: String },
    /// `UserPromptSubmit`; `prompt` is never logged.
    PromptSubmitted {
        agent_id: String,
        prompt: Option<String>,
    },
    /// Stop (`failed: false`) or StopFailure (`failed: true`).
    TurnEnded { agent_id: String, failed: bool },
    /// The agent's queue changed (assign, reorder, reject, …).
    QueueChanged { agent_id: String },
    /// A review was assigned to this reviewer (plan5 A.6): deliver it when it is idle.
    ReviewAssigned { reviewer_agent_id: String },
    /// The agent stopped/exited/was removed (its tickets are already released).
    AgentGone { agent_id: String },
    /// The agent is restarted with `--resume` (model/effort change, plan5 A.5): any delivery
    /// state is dropped like for `AgentGone`; its queue continues at the next Idle. A
    /// [`Delivery::AwaitingRestart`] (the dispatcher's own restart, step 6b) is kept.
    AgentRestarting { agent_id: String },
    /// `spawn_agent_with_ticket`: the line went in as the positional prompt.
    SpawnedWithTicket { agent_id: String, ticket_id: String },
    /// "Send igen" from the UI.
    Redispatch { ticket_id: String },
    /// "Bed om aflevering" from the UI: type the nudge line into the in-progress ticket's agent.
    RequestSubmission { ticket_id: String },
    /// Review 5c W4: someone else (the user) took `agent_id`'s ticket in progress from it, handed
    /// to `to_name` (`None`: back to the backlog). When the agent is idle it gets
    /// [`prompt::handed_over_line`] before its next delivery. Never sent when the agent handed
    /// the ticket on itself.
    HandedOver {
        agent_id: String,
        ticket_id: String,
        to_name: Option<String>,
    },
    Timer {
        agent_id: String,
        token: u64,
        kind: TimerKind,
    },
}

/// Maps a status notification from the pipe handler to dispatcher messages.
pub fn messages_for(ev: &StatusEvent) -> Vec<DispatchMsg> {
    let agent_id = ev.agent_id.clone();
    match ev.hook_event_name.as_str() {
        "UserPromptSubmit" => vec![DispatchMsg::PromptSubmitted {
            agent_id,
            prompt: ev.prompt.clone(),
        }],
        "Stop" | "StopFailure" => vec![
            DispatchMsg::TurnEnded {
                agent_id: agent_id.clone(),
                failed: ev.hook_event_name == "StopFailure",
            },
            DispatchMsg::AgentIdle { agent_id },
        ],
        _ => match &ev.status {
            Some(AgentStatus::Idle) => vec![DispatchMsg::AgentIdle { agent_id }],
            Some(
                AgentStatus::Thinking
                | AgentStatus::Reading
                | AgentStatus::Editing
                | AgentStatus::Running
                | AgentStatus::WaitingPermission,
            ) => vec![DispatchMsg::AgentBusy { agent_id }],
            _ => Vec::new(),
        },
    }
}

/// What a delivery types: a work ticket ("Ticket …" line), a review ("Review af ticket …"
/// line, plan5 C5.12) or the wake line of a waiting parent ("Du har fået besked: …", review 6a
/// W1; the parent resumes only once the line is confirmed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryKind {
    Work,
    Review,
    Wake,
}

impl DeliveryKind {
    /// Whether the submitted `prompt` is this delivery's line: a ticket goes in as
    /// "Ticket <short>" or, as a coordination task, as "Koordiner ticket <short>" (5c C.1; the
    /// first word matched tolerantly, review 5c N1); a review as "Review af ticket <short>"; a
    /// wake as "Du har fået besked: … (ticketId <parent short>)" ([`prompt::is_wake_line_for`]).
    /// The prefixes exclude each other, so no kind ever confirms another kind's line.
    fn confirms(self, ticket_id: &str, prompt: &str) -> bool {
        let short = super::model::short_id(ticket_id);
        let p = prompt.trim_start();
        match self {
            DeliveryKind::Work => {
                p.starts_with(&format!("Ticket {short}"))
                    || prompt::is_coordination_line_for(p, &short)
            }
            DeliveryKind::Review => p.starts_with(&format!("Review af ticket {short}")),
            DeliveryKind::Wake => prompt::is_wake_line_for(p, &short),
        }
    }
}

/// What an idle agent gets next: its oldest undelivered review first, else its queue head.
#[derive(Clone, Debug)]
enum Item {
    Work(Ticket),
    Review(Ticket),
}

/// Per-agent delivery state (plan A.4).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Delivery {
    Free,
    /// Waiting `DISPATCH_DELAY_MS` before typing the queue head.
    Delaying {
        token: u64,
    },
    /// Line typed, Enter pending.
    Typed {
        ticket_id: String,
        token: u64,
        kind: DeliveryKind,
    },
    /// Enter sent, waiting for `UserPromptSubmit` or a busy status (a wake: only for the
    /// `UserPromptSubmit` with its line, review 6a W1).
    Waiting {
        ticket_id: String,
        token: u64,
        retried: bool,
        /// Came from `AwaitingSession`: a timeout falls back to an ordinary PTY dispatch.
        from_spawn: bool,
        kind: DeliveryKind,
    },
    /// Spawned with the ticket line as positional prompt; waiting for the session to start.
    AwaitingSession {
        ticket_id: String,
        token: u64,
    },
    /// Step 6b (plan A.7): the agent was restarted before `ticket_id` is delivered (a fresh
    /// session and/or the ticket's worktree); survives `AgentRestarting`, the next Idle (or
    /// `RestartTimeout`) frees the agent and the queue is considered again.
    AwaitingRestart {
        ticket_id: String,
        token: u64,
    },
    /// Typing a "Du …" line for `ticket_id`: "Bed om aflevering" (C4.7, the ticket in progress),
    /// "stop, it was handed on" (review 5c W4) or the wake line of a waiting parent (step 6a):
    /// waiting for the grace period (`NudgeDelay`), then for the Enter (`NudgeEnter`). Nothing is
    /// confirmed.
    Nudging {
        ticket_id: String,
        token: u64,
        nudge: Nudge,
    },
}

/// Which "Du …" line a [`Delivery::Nudging`] types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Nudge {
    /// [`prompt::request_submission_line`]: the ticket is still the agent's, in progress.
    Submit,
    /// [`prompt::handed_over_line`]: the ticket left the agent (review 5c W4).
    Stop,
    /// [`prompt::wake_line`]: `ticket_id` is the agent's waiting parent and a child is done
    /// (step 6a, plan A.2). After the Enter the dispatcher waits for the confirmation like a
    /// delivery ([`DeliveryKind::Wake`], review 6a W1); only then the parent goes back in progress.
    Wake,
}

/// The wake a [`WakeMiss`] counts: the same parent with the same children done and open.
#[derive(Clone, Debug, PartialEq, Eq)]
struct WakeKey {
    parent_id: String,
    newly_done: Vec<String>,
    open_left: usize,
}

impl WakeKey {
    fn of(w: &DueWake) -> Self {
        WakeKey {
            parent_id: w.parent.id.clone(),
            newly_done: w.newly_done.iter().map(|c| c.id.clone()).collect(),
            open_left: w.open_left,
        }
    }
}

/// Unconfirmed attempts at an agent's wake line (review 6a W1). At [`WAKE_MAX_ATTEMPTS`] the
/// wake is not typed again until it changes (another child done), the user asks ("Bed om
/// aflevering" on the waiting parent) or the agent restarts.
#[derive(Clone, Debug, PartialEq, Eq)]
struct WakeMiss {
    key: WakeKey,
    misses: u8,
}

/// A pending [`prompt::handed_over_line`] for an agent (review 5c W4).
#[derive(Clone, Debug, PartialEq, Eq)]
struct StopNotice {
    ticket_id: String,
    to_name: Option<String>,
}

pub struct Dispatcher<H, P, T> {
    host: H,
    port: P,
    timers: T,
    deliveries: HashMap<String, Delivery>,
    /// Agents to tell that their ticket in progress was handed on (review 5c W4), typed when
    /// they are idle, before their next delivery.
    stop_notices: HashMap<String, StopNotice>,
    /// Per agent: unconfirmed wake attempts (review 6a W1).
    wake_misses: HashMap<String, WakeMiss>,
    /// Per agent: the ticket it was last restarted for (step 6b). That ticket is never restarted
    /// for again by the same agent (no loop when the new cwd does not match, a timeout delivers
    /// as it is); cleared when a ticket is typed or the agent is gone.
    restarted_for: HashMap<String, String>,
    next_token: u64,
    /// Overrides the workspace rule `autoReviewOnStop` (tests); `None` = [`TicketsHost::rules`].
    auto_review_on_stop: Option<bool>,
}

impl<H: TicketsHost, P: AgentPort, T: Timers> Dispatcher<H, P, T> {
    pub fn new(host: H, port: P, timers: T) -> Self {
        Dispatcher {
            host,
            port,
            timers,
            deliveries: HashMap::new(),
            stop_notices: HashMap::new(),
            wake_misses: HashMap::new(),
            restarted_for: HashMap::new(),
            next_token: 0,
            auto_review_on_stop: None,
        }
    }

    /// Overrides the workspace rule `autoReviewOnStop` (default
    /// [`crate::config::AUTO_REVIEW_ON_STOP`]; tests).
    pub fn with_auto_review(mut self, on: bool) -> Self {
        self.auto_review_on_stop = Some(on);
        self
    }

    /// Whether a Stop moves the in-progress ticket to review: the override, else the workspace
    /// rule (read when needed, plan4b A.4).
    fn auto_review(&self) -> bool {
        self.auto_review_on_stop
            .unwrap_or_else(|| self.host.rules().auto_review_on_stop)
    }

    fn token(&mut self) -> u64 {
        self.next_token += 1;
        self.next_token
    }

    fn state(&self, agent_id: &str) -> &Delivery {
        self.deliveries.get(agent_id).unwrap_or(&Delivery::Free)
    }

    fn set(&mut self, agent_id: &str, d: Delivery) {
        if d == Delivery::Free {
            self.deliveries.remove(agent_id);
        } else {
            self.deliveries.insert(agent_id.to_string(), d);
        }
    }

    fn schedule(&mut self, agent_id: &str, token: u64, kind: TimerKind, delay_ms: u64) {
        self.timers.schedule(
            delay_ms,
            DispatchMsg::Timer {
                agent_id: agent_id.to_string(),
                token,
                kind,
            },
        );
    }

    pub fn handle(&mut self, msg: DispatchMsg) {
        match msg {
            DispatchMsg::AgentIdle { agent_id } => self.on_idle(&agent_id),
            DispatchMsg::AgentBusy { agent_id } => self.on_busy(&agent_id),
            DispatchMsg::PromptSubmitted { agent_id, prompt } => {
                self.on_prompt(&agent_id, prompt.as_deref())
            }
            DispatchMsg::TurnEnded { agent_id, failed } => self.on_turn_ended(&agent_id, failed),
            DispatchMsg::QueueChanged { agent_id }
            | DispatchMsg::ReviewAssigned {
                reviewer_agent_id: agent_id,
            } => self.consider(&agent_id),
            DispatchMsg::AgentGone { agent_id } => {
                self.deliveries.remove(&agent_id);
                self.stop_notices.remove(&agent_id);
                self.wake_misses.remove(&agent_id);
                self.restarted_for.remove(&agent_id);
            }
            DispatchMsg::AgentRestarting { agent_id } => {
                // Our own restart before a delivery announces itself too: keep waiting for it.
                if !matches!(self.state(&agent_id), Delivery::AwaitingRestart { .. }) {
                    self.deliveries.remove(&agent_id);
                }
                self.wake_misses.remove(&agent_id);
            }
            DispatchMsg::SpawnedWithTicket {
                agent_id,
                ticket_id,
            } => {
                let token = self.token();
                self.set(&agent_id, Delivery::AwaitingSession { ticket_id, token });
            }
            DispatchMsg::Redispatch { ticket_id } => self.redispatch(&ticket_id),
            DispatchMsg::RequestSubmission { ticket_id } => self.on_request_submission(&ticket_id),
            DispatchMsg::HandedOver {
                agent_id,
                ticket_id,
                to_name,
            } => {
                self.stop_notices
                    .insert(agent_id.clone(), StopNotice { ticket_id, to_name });
                self.consider(&agent_id);
            }
            DispatchMsg::Timer {
                agent_id,
                token,
                kind,
            } => self.on_timer(&agent_id, token, kind),
        }
    }

    /// Starts a delivery sequence if the agent is free, idle and has a review or a queued ticket.
    // TODO(windows-verify): 750 ms after Stop the input field is ready, and the extra Enter on
    // retry neither sends an empty prompt nor closes a dialog (plan D.29).
    /// A pending stop notice (review 5c W4) goes first: no delivery until it is typed. Then a
    /// due wake line for a waiting parent (step 6a), then reviews and the queue.
    fn consider(&mut self, agent_id: &str) {
        if *self.state(agent_id) != Delivery::Free {
            return;
        }
        if self.stop_notices.contains_key(agent_id) {
            self.start_stop_notice(agent_id);
            return;
        }
        if let Some((snap, _)) = self.due_wake(agent_id) {
            self.start_wake(agent_id, &snap);
            return;
        }
        let Some((snap, _)) = self.ready_item(agent_id) else {
            return;
        };
        // The user typed recently: wait until the grace period is over (at least the usual delay).
        let delay = self
            .user_grace_left(&snap)
            .map_or(DISPATCH_DELAY_MS, |left| left.max(DISPATCH_DELAY_MS));
        let token = self.token();
        self.schedule(agent_id, token, TimerKind::DispatchDelay, delay);
        self.set(agent_id, Delivery::Delaying { token });
    }

    /// Milliseconds left of the grace period (workspace rule `userInputGraceMs`, default
    /// [`USER_INPUT_GRACE_MS`]) since the user last typed, if any.
    fn user_grace_left(&self, snap: &AgentSnapshot) -> Option<u64> {
        let at = snap.last_user_input_at?;
        let grace = self.host.rules().user_input_grace_ms;
        let elapsed = self.timers.now_ms().saturating_sub(at);
        (elapsed < grace).then(|| grace - elapsed)
    }

    /// The agent's due wake (step 6a, plan A.2): only while it is idle without a ticket in
    /// progress, and a waiting parent of its own is due ([`TicketService::due_wake_for`]). Not
    /// once the same wake went unconfirmed [`WAKE_MAX_ATTEMPTS`] times (review 6a W1).
    fn due_wake(&self, agent_id: &str) -> Option<(AgentSnapshot, DueWake)> {
        let snap = self.port.snapshot(agent_id)?;
        if snap.status != AgentStatus::Idle {
            return None;
        }
        if self.host.read(|s| s.current_for_agent(agent_id)).is_some() {
            return None;
        }
        let w = self.host.read(|s| s.due_wake_for(agent_id))?;
        let given_up = self
            .wake_misses
            .get(agent_id)
            .is_some_and(|m| m.misses >= WAKE_MAX_ATTEMPTS && m.key == WakeKey::of(&w));
        if given_up {
            return None;
        }
        Some((snap, w))
    }

    /// Schedules the wake line after the usual delay and the user-input grace (like a stop
    /// notice). What is typed is decided again when the timer comes due ([`Self::wake_type`]).
    fn start_wake(&mut self, agent_id: &str, snap: &AgentSnapshot) {
        let Some(w) = self.host.read(|s| s.due_wake_for(agent_id)) else {
            return;
        };
        let delay = self
            .user_grace_left(snap)
            .map_or(DISPATCH_DELAY_MS, |left| left.max(DISPATCH_DELAY_MS));
        let token = self.token();
        self.schedule(agent_id, token, TimerKind::NudgeDelay, delay);
        self.set(
            agent_id,
            Delivery::Nudging {
                ticket_id: w.parent.id,
                token,
                nudge: Nudge::Wake,
            },
        );
    }

    /// `NudgeDelay` of a wake came due: type [`prompt::wake_line`] if the agent is still idle
    /// without a ticket in progress, the user is not typing and the parent is still due; else
    /// free the agent (and consider it again when the wake is no longer due).
    // TODO(windows-verify): "Du har fået besked: …" is typed about 1 s after the coordinator is
    // idle and the parent shows "I gang" with the note "vækket …" (plan6a D.91/92).
    fn wake_type(&mut self, agent_id: &str, parent_id: &str, token: u64) {
        let Some(snap) = self.port.snapshot(agent_id) else {
            self.set(agent_id, Delivery::Free);
            return;
        };
        if let Some(left) = self.user_grace_left(&snap) {
            log::info!("dispatch {agent_id}: user typed recently; wake line in {left} ms");
            self.schedule(agent_id, token, TimerKind::NudgeDelay, left);
            return;
        }
        if snap.status != AgentStatus::Idle
            || self.host.read(|s| s.current_for_agent(agent_id)).is_some()
        {
            // Still due: the next Stop/Idle considers it again.
            log::info!("dispatch {agent_id}: busy again; no wake line now");
            self.set(agent_id, Delivery::Free);
            return;
        }
        let due = self
            .host
            .read(|s| s.due_wake_for(agent_id))
            .filter(|w| w.parent.id == parent_id);
        let Some(w) = due else {
            self.set(agent_id, Delivery::Free);
            self.consider(agent_id);
            return;
        };
        let info = WakeInfo {
            parent_short: w.parent.short_id(),
            newly_done: w
                .newly_done
                .iter()
                .map(|c| (c.short_id(), c.title.clone()))
                .collect(),
            open_left: w.open_left,
        };
        let line = prompt::wake_line(&info);
        if let Err(e) = self.port.write_input(agent_id, line.as_bytes()) {
            log::warn!("dispatch {agent_id}: typing the wake line failed: {e}");
            self.set(agent_id, Delivery::Free);
            return;
        }
        log::info!(
            "dispatch {agent_id}: woke for parent {} ({} children done, {} open)",
            info.parent_short,
            w.newly_done.len(),
            w.open_left
        );
        self.schedule(agent_id, token, TimerKind::NudgeEnter, ENTER_DELAY_MS);
    }

    /// The agent (idle, not exited) and what it gets next, if a delivery may start now: its
    /// oldest undelivered review assignment (plan5 A.6: reviews go before work), else its queue
    /// head. Never while the agent has a ticket in progress: the next one waits until that one is
    /// submitted or moved (step 4; also covers Esc mid-turn, which gives Idle without Stop).
    fn ready_item(&self, agent_id: &str) -> Option<(AgentSnapshot, Item)> {
        let snap = self.port.snapshot(agent_id)?;
        if snap.status != AgentStatus::Idle {
            return None;
        }
        if self.host.read(|s| s.current_for_agent(agent_id)).is_some() {
            return None;
        }
        if let Some((_, t)) = self.host.read(|s| s.next_review_for(agent_id)) {
            return Some((snap, Item::Review(t)));
        }
        let ticket = self.host.read(|s| s.next_for_agent(agent_id))?;
        Some((snap, Item::Work(ticket)))
    }

    fn on_idle(&mut self, agent_id: &str) {
        match self.state(agent_id).clone() {
            Delivery::Free => {
                // An idle agent may be a reviewer that came up after reviews were waiting.
                self.host.reroute_reviews();
                self.consider(agent_id);
            }
            Delivery::AwaitingRestart { ticket_id, .. } => {
                // The new session is up (step 6b): deliver as usual; it has no conversation and
                // `restarted_for` holds the ticket, so it is not restarted again.
                log::info!(
                    "dispatch {agent_id}: restarted for ticket {}; delivering",
                    super::model::short_id(&ticket_id)
                );
                self.set(agent_id, Delivery::Free);
                self.consider(agent_id);
            }
            Delivery::AwaitingSession { ticket_id, token } => {
                // The session is up; the positional prompt should be submitted soon.
                self.schedule(
                    agent_id,
                    token,
                    TimerKind::Confirm,
                    SPAWN_CONFIRM_TIMEOUT_MS,
                );
                self.set(
                    agent_id,
                    Delivery::Waiting {
                        ticket_id,
                        token,
                        retried: true,
                        from_spawn: true,
                        kind: DeliveryKind::Work,
                    },
                );
            }
            // Mid-sequence: no double dispatch.
            _ => {}
        }
    }

    fn on_busy(&mut self, agent_id: &str) {
        match self.state(agent_id).clone() {
            // A wake is confirmed by its line only (review 6a W1): a busy status may belong to
            // something else, and a wrongly resumed parent is never woken again.
            Delivery::Waiting {
                kind: DeliveryKind::Wake,
                ..
            } => {}
            Delivery::Waiting {
                ticket_id, kind, ..
            } => self.confirm(agent_id, &ticket_id, kind),
            Delivery::AwaitingSession { ticket_id, .. } => {
                self.confirm(agent_id, &ticket_id, DeliveryKind::Work)
            }
            _ => {}
        }
    }

    fn on_prompt(&mut self, agent_id: &str, prompt: Option<&str>) {
        let (ticket_id, typed_by_us, kind) = match self.state(agent_id) {
            Delivery::Waiting {
                ticket_id, kind, ..
            }
            | Delivery::Typed {
                ticket_id, kind, ..
            } => (ticket_id.clone(), true, *kind),
            Delivery::AwaitingSession { ticket_id, .. } => {
                (ticket_id.clone(), false, DeliveryKind::Work)
            }
            Delivery::Free
            | Delivery::Nudging {
                nudge: Nudge::Wake, ..
            } => {
                // A wake line submitted outside its confirm window (late, or Enter pressed before
                // ours) still counts (review 6a W1).
                if let Some(p) = prompt {
                    self.late_wake(agent_id, p);
                }
                return;
            }
            _ => return,
        };
        match prompt {
            Some(p) if kind.confirms(&ticket_id, p) => self.confirm(agent_id, &ticket_id, kind),
            None if kind == DeliveryKind::Wake => {
                log::info!(
                    "dispatch {agent_id}: UserPromptSubmit without prompt; wake not confirmed yet"
                );
            }
            Some(_) if typed_by_us => {
                // Another prompt went in while our line was in the terminal (typically the user's
                // own text, possibly merged with the line). A busy status that follows belongs
                // to that prompt, so it must not confirm the ticket: give up this delivery.
                log::info!("dispatch {agent_id}: a different prompt was submitted; aborting");
                self.delivery_failed(agent_id, &ticket_id, USER_TYPED_NOTE, kind);
            }
            Some(_) => log::debug!("dispatch {agent_id}: submitted prompt is not the ticket line"),
            None => {
                log::warn!("dispatch {agent_id}: UserPromptSubmit without prompt; counting it as delivered");
                self.confirm(agent_id, &ticket_id, kind);
            }
        }
    }

    /// Delivery confirmed: the ticket goes in progress (or gets a "sent again" entry); a review
    /// counts as delivered ("review sendt til <name>").
    fn confirm(&mut self, agent_id: &str, ticket_id: &str, kind: DeliveryKind) {
        self.set(agent_id, Delivery::Free);
        if kind == DeliveryKind::Wake {
            self.wake_confirmed(agent_id, ticket_id);
            return;
        }
        let snap = self.port.snapshot(agent_id);
        let name = snap
            .as_ref()
            .map_or_else(|| agent_id.to_string(), |s| s.name.clone());
        let now = now_ms();
        // The ticket changed hands while its line was on the way (moved back, handed over to
        // another agent, step 5c): it must not go in progress with the new assignee.
        let moved = kind == DeliveryKind::Work
            && self
                .host
                .read(|s| s.get(ticket_id))
                .is_some_and(|t| t.assignee_agent_id.as_deref() != Some(agent_id));
        if moved {
            log::info!(
                "dispatch {agent_id}: ticket {ticket_id} changed hands; not marked dispatched"
            );
        }
        let r = match kind {
            DeliveryKind::Work if moved => Ok(()),
            DeliveryKind::Work => self
                .host
                .mutate(|s| s.mark_dispatched(ticket_id, &name, now))
                .map(|_| ()),
            DeliveryKind::Review => self
                .host
                .mutate(|s| s.mark_review_delivered(ticket_id, &name, now))
                .map(|_| ()),
            DeliveryKind::Wake => Ok(()),
        };
        if let Err(e) = r {
            log::warn!("dispatch {agent_id}: confirming {kind:?} {ticket_id} failed: {e}");
        }
        if snap.is_some_and(|s| {
            matches!(
                s.detail.as_deref(),
                Some(DELIVERY_FAILED_TEXT | TURN_FAILED_TEXT | NOT_SUBMITTED_TEXT)
            ) || s
                .detail
                .as_deref()
                .is_some_and(prompt::is_handed_over_detail)
        }) {
            self.port.set_detail(agent_id, None);
        }
    }

    /// Stop: with `autoReviewOnStop` ([`Self::auto_review`]) the in-progress ticket goes to
    /// review (step 3).
    /// Otherwise (step 4) a ticket still in progress was not submitted with
    /// `mira_submit_for_review`: it keeps its state with `issue: notSubmitted` and the agent gets
    /// [`NOT_SUBMITTED_TEXT`]; no next ticket is considered. Without a ticket in progress (e.g.
    /// submitted by the tool in this turn) the queue moves on. Step 6a: a parent with open
    /// children goes to waiting instead (no hint) and the queue moves on.
    // TODO(windows-verify): Esc mid-turn gives no Stop (the ticket stays in progress); StopFailure
    // shows the "Turn fejlede" hint and "Send igen" works (plan D.33). Stop without
    // `mira_submit_for_review` shows "Ikke afleveret"; with it, the ticket is in review before or
    // right after the Stop (plan4 D.42).
    fn on_turn_ended(&mut self, agent_id: &str, failed: bool) {
        let now = now_ms();
        if failed {
            let Some(t) = self.host.read(|s| s.current_for_agent(agent_id)) else {
                return;
            };
            let r = self.host.mutate(|s| {
                s.set_issue(
                    &t.id,
                    Some(TicketIssue::TurnFailed),
                    Some(TURN_FAILED_NOTE.into()),
                    now,
                )
            });
            if let Err(e) = r {
                log::warn!("dispatch {agent_id}: marking the turn failure failed: {e}");
            }
            self.port
                .set_detail(agent_id, Some(TURN_FAILED_TEXT.to_string()));
        } else if self.auto_review() {
            match self.host.mutate(|s| s.complete_turn(agent_id, now)) {
                Ok(Some(t)) if t.state == TicketState::Waiting => log::info!(
                    "dispatch {agent_id}: turn ended; parent {} waits for its children",
                    t.short_id()
                ),
                Ok(Some(t)) if t.state == TicketState::Review => self.host.reroute_reviews(),
                Ok(_) => {}
                Err(e) => log::warn!("dispatch {agent_id}: completing the turn failed: {e}"),
            }
            self.consider(agent_id);
        } else {
            match self.host.mutate(|s| s.mark_not_submitted(agent_id, now)) {
                // Step 6a: a parent with open children waits instead ("Ikke afleveret" would be
                // wrong); the agent is free for its queue.
                Ok(Some(t)) if t.state == TicketState::Waiting => {
                    log::info!(
                        "dispatch {agent_id}: turn ended; parent {} waits for its children",
                        t.short_id()
                    );
                    self.consider(agent_id);
                }
                Ok(Some(t)) => {
                    log::info!(
                        "dispatch {agent_id}: turn ended without submitting ticket {}",
                        t.short_id()
                    );
                    self.port
                        .set_detail(agent_id, Some(NOT_SUBMITTED_TEXT.to_string()));
                }
                Ok(None) => self.consider(agent_id),
                Err(e) => log::warn!("dispatch {agent_id}: marking not submitted failed: {e}"),
            }
        }
    }

    /// "Bed om aflevering": the ticket must be in progress with an agent that is idle and has no
    /// delivery running; otherwise this only logs. The line goes in after the user-input grace.
    fn on_request_submission(&mut self, ticket_id: &str) {
        let Some(t) = self.host.read(|s| s.get(ticket_id)) else {
            log::info!("request submission: ticket {ticket_id} not found");
            return;
        };
        let agent_id = match (&t.state, &t.assignee_agent_id) {
            (TicketState::InProgress, Some(a)) => a.clone(),
            (TicketState::Waiting, Some(a)) => {
                // Review 6a W1: on a waiting parent the request types its wake line again, also
                // after the dispatcher gave up on an unconfirmed one.
                let a = a.clone();
                self.wake_misses.remove(&a);
                log::info!(
                    "request submission: parent {} waits; its wake is tried again",
                    t.short_id()
                );
                self.consider(&a);
                return;
            }
            _ => {
                log::info!(
                    "request submission: ticket {} is not in progress",
                    t.short_id()
                );
                return;
            }
        };
        let Some(snap) = self.port.snapshot(&agent_id) else {
            log::info!("request submission: agent {agent_id} is gone");
            return;
        };
        if snap.status != AgentStatus::Idle || *self.state(&agent_id) != Delivery::Free {
            log::info!(
                "request submission: agent {agent_id} is busy; ticket {} not nudged",
                t.short_id()
            );
            return;
        }
        let delay = self.user_grace_left(&snap).unwrap_or(0);
        let token = self.token();
        self.schedule(&agent_id, token, TimerKind::NudgeDelay, delay);
        self.set(
            &agent_id,
            Delivery::Nudging {
                ticket_id: t.id,
                token,
                nudge: Nudge::Submit,
            },
        );
    }

    /// The agent's pending stop notice (review 5c W4): typed after the usual delay (and the
    /// user-input grace) once the agent is idle; while it is busy the notice waits for the next
    /// [`Self::consider`] (its Stop).
    fn start_stop_notice(&mut self, agent_id: &str) {
        let Some(notice) = self.stop_notices.get(agent_id).cloned() else {
            return;
        };
        let Some(snap) = self.port.snapshot(agent_id) else {
            self.stop_notices.remove(agent_id);
            return;
        };
        if snap.status != AgentStatus::Idle {
            return;
        }
        let delay = self
            .user_grace_left(&snap)
            .map_or(DISPATCH_DELAY_MS, |left| left.max(DISPATCH_DELAY_MS));
        let token = self.token();
        self.schedule(agent_id, token, TimerKind::NudgeDelay, delay);
        self.set(
            agent_id,
            Delivery::Nudging {
                ticket_id: notice.ticket_id,
                token,
                nudge: Nudge::Stop,
            },
        );
    }

    /// `NudgeDelay` of a stop notice came due: type [`prompt::handed_over_line`] if the agent is
    /// still idle and the ticket has not come back to it. Typed (or failed) = no longer pending.
    fn stop_notice_type(&mut self, agent_id: &str, ticket_id: &str, token: u64) {
        let notice = self
            .stop_notices
            .get(agent_id)
            .filter(|n| n.ticket_id == ticket_id)
            .cloned();
        let (Some(notice), Some(snap)) = (notice, self.port.snapshot(agent_id)) else {
            self.set(agent_id, Delivery::Free);
            self.consider(agent_id);
            return;
        };
        if let Some(left) = self.user_grace_left(&snap) {
            log::info!("dispatch {agent_id}: user typed recently; stop notice in {left} ms");
            self.schedule(agent_id, token, TimerKind::NudgeDelay, left);
            return;
        }
        if snap.status != AgentStatus::Idle {
            // Still pending: the next Stop considers it again.
            self.set(agent_id, Delivery::Free);
            return;
        }
        self.stop_notices.remove(agent_id);
        let back = self
            .host
            .read(|s| s.get(ticket_id))
            .is_some_and(|t| t.assignee_agent_id.as_deref() == Some(agent_id));
        if back {
            log::info!("dispatch {agent_id}: ticket {ticket_id} came back; no stop notice");
            self.set(agent_id, Delivery::Free);
            self.consider(agent_id);
            return;
        }
        let short = super::model::short_id(ticket_id);
        let line = prompt::handed_over_line(&short, notice.to_name.as_deref());
        if let Err(e) = self.port.write_input(agent_id, line.as_bytes()) {
            log::warn!("dispatch {agent_id}: typing the stop notice failed: {e}");
            self.set(agent_id, Delivery::Free);
            return;
        }
        log::info!("dispatch {agent_id}: told to stop working on ticket {short}");
        self.schedule(agent_id, token, TimerKind::NudgeEnter, ENTER_DELAY_MS);
    }

    /// `NudgeDelay` came due: type the line if the agent is still idle, the user is not typing
    /// and the ticket is still its in-progress ticket.
    fn nudge_type(&mut self, agent_id: &str, ticket_id: &str, token: u64) {
        let Some(snap) = self.port.snapshot(agent_id) else {
            self.set(agent_id, Delivery::Free);
            return;
        };
        if let Some(left) = self.user_grace_left(&snap) {
            log::info!("dispatch {agent_id}: user typed recently; nudging in {left} ms");
            self.schedule(agent_id, token, TimerKind::NudgeDelay, left);
            return;
        }
        let current = self.host.read(|s| s.current_for_agent(agent_id));
        let Some(t) = current.filter(|t| t.id == ticket_id) else {
            log::info!("dispatch {agent_id}: ticket no longer in progress; no nudge");
            self.set(agent_id, Delivery::Free);
            return;
        };
        if snap.status != AgentStatus::Idle {
            log::info!("dispatch {agent_id}: agent busy again; no nudge");
            self.set(agent_id, Delivery::Free);
            return;
        }
        let line = prompt::request_submission_line(&t.short_id());
        if let Err(e) = self.port.write_input(agent_id, line.as_bytes()) {
            log::warn!("dispatch {agent_id}: typing the submission request failed: {e}");
            self.set(agent_id, Delivery::Free);
            return;
        }
        log::info!(
            "dispatch {agent_id}: asked to submit ticket {}",
            t.short_id()
        );
        self.schedule(agent_id, token, TimerKind::NudgeEnter, ENTER_DELAY_MS);
    }

    /// `NudgeEnter` came due: Enter, then (for "Bed om aflevering") the history note. The
    /// ticket's state and issue stay. The note only while the ticket is still the agent's in
    /// progress: handed on in the meantime, it must not land on the new owner's ticket (review
    /// 5c N3, the same guard as [`Self::confirm`]).
    fn nudge_enter(&mut self, agent_id: &str, ticket_id: &str, nudge: Nudge) {
        self.set(agent_id, Delivery::Free);
        if let Err(e) = self.port.write_input(agent_id, b"\r") {
            log::warn!("dispatch {agent_id}: sending Enter after the {nudge:?} line failed: {e}");
            return;
        }
        match nudge {
            Nudge::Stop => return,
            Nudge::Wake => {
                // Review 6a W1: the parent stays waiting until the line is confirmed.
                log::info!(
                    "dispatch {agent_id}: wake line for parent {} entered; waiting for confirmation",
                    super::model::short_id(ticket_id)
                );
                let token = self.token();
                self.schedule(agent_id, token, TimerKind::Confirm, CONFIRM_TIMEOUT_MS);
                self.set(
                    agent_id,
                    Delivery::Waiting {
                        ticket_id: ticket_id.to_string(),
                        token,
                        retried: true,
                        from_spawn: false,
                        kind: DeliveryKind::Wake,
                    },
                );
                return;
            }
            Nudge::Submit => {}
        }
        let still_ours = self
            .host
            .read(|s| s.current_for_agent(agent_id))
            .is_some_and(|t| t.id == ticket_id);
        if !still_ours {
            log::info!("dispatch {agent_id}: ticket {ticket_id} changed hands; no request note");
            return;
        }
        let now = now_ms();
        let r = self.host.mutate(|s| {
            let issue = s.get(ticket_id).ok_or(TicketError::NotFound)?.issue;
            s.set_issue(
                ticket_id,
                issue,
                Some(SUBMISSION_REQUESTED_NOTE.into()),
                now,
            )
        });
        if let Err(e) = r {
            log::warn!("dispatch {agent_id}: noting the submission request failed: {e}");
        }
    }

    /// The wake line was confirmed (step 6a, plan P2; review 6a W1: its `UserPromptSubmit`):
    /// the parent goes waiting → in progress with [`WOKEN_NOTE`] ([`CHILDREN_DONE_NOTE`] when no
    /// child is open any more). Nothing happens when it is no longer the agent's waiting parent
    /// or the agent has another ticket in progress (`Ok(None)`). The attempts are forgotten and a
    /// [`WAKE_UNCONFIRMED_TEXT`] detail is cleared.
    fn wake_confirmed(&mut self, agent_id: &str, parent_id: &str) {
        self.wake_misses.remove(agent_id);
        let now = now_ms();
        let r = self.host.mutate(|s| {
            let note = if s.open_children_count(parent_id) == 0 {
                CHILDREN_DONE_NOTE
            } else {
                WOKEN_NOTE
            };
            s.resume_after_wake(parent_id, agent_id, note, now)
        });
        match r {
            Ok(Some(t)) => log::info!(
                "dispatch {agent_id}: wake confirmed; parent {} back in progress",
                t.short_id()
            ),
            Ok(None) => log::info!(
                "dispatch {agent_id}: wake confirmed; parent {} not resumed (no longer waiting or agent busy)",
                super::model::short_id(parent_id)
            ),
            Err(e) => log::warn!("dispatch {agent_id}: resuming the parent failed: {e}"),
        }
        let stale = self
            .port
            .snapshot(agent_id)
            .is_some_and(|s| s.detail.as_deref() == Some(WAKE_UNCONFIRMED_TEXT));
        if stale {
            self.port.set_detail(agent_id, None);
        }
    }

    /// A `UserPromptSubmit` outside a wake's confirm window (review 6a W1): when it is the wake
    /// line of one of the agent's waiting parents (submitted after the timeout, or the user
    /// pressed Enter before ours), the parent resumes as if confirmed; a pending Enter for it is
    /// dropped, so the line is never entered twice.
    fn late_wake(&mut self, agent_id: &str, prompt: &str) {
        let parent = self.host.read(|s| {
            s.list_for_agent(agent_id)
                .into_iter()
                .find(|t| {
                    t.state == TicketState::Waiting && DeliveryKind::Wake.confirms(&t.id, prompt)
                })
                .map(|t| t.id)
        });
        let Some(parent_id) = parent else {
            return;
        };
        log::info!(
            "dispatch {agent_id}: wake line for parent {} submitted outside the confirm window",
            super::model::short_id(&parent_id)
        );
        if matches!(self.state(agent_id), Delivery::Nudging { ticket_id, .. } if *ticket_id == parent_id)
        {
            self.set(agent_id, Delivery::Free);
        }
        self.wake_confirmed(agent_id, &parent_id);
    }

    /// The wake line was not confirmed in [`CONFIRM_TIMEOUT_MS`] after its Enter, or another
    /// prompt went in (review 6a W1): the agent is free, the parent stays waiting and
    /// [`TicketService::due_wake_for`] finds it again at the next idle — once; after
    /// [`WAKE_MAX_ATTEMPTS`] unconfirmed attempts of the same wake the agent shows
    /// [`WAKE_UNCONFIRMED_TEXT`] and the user asks again ("Bed om aflevering").
    fn wake_unconfirmed(&mut self, agent_id: &str, parent_id: &str, why: &str) {
        self.set(agent_id, Delivery::Free);
        let short = super::model::short_id(parent_id);
        let due = self
            .host
            .read(|s| s.due_wake_for(agent_id))
            .filter(|w| w.parent.id == parent_id);
        let Some(w) = due else {
            log::info!(
                "dispatch {agent_id}: wake for parent {short} not confirmed ({why}); no longer due"
            );
            self.wake_misses.remove(agent_id);
            return;
        };
        let key = WakeKey::of(&w);
        let misses = match self.wake_misses.get(agent_id) {
            Some(m) if m.key == key => m.misses.saturating_add(1),
            _ => 1,
        };
        self.wake_misses
            .insert(agent_id.to_string(), WakeMiss { key, misses });
        if misses >= WAKE_MAX_ATTEMPTS {
            log::warn!(
                "dispatch {agent_id}: wake for parent {short} not confirmed ({why}) after {misses} attempts; not typed again until the user asks"
            );
            self.port
                .set_detail(agent_id, Some(WAKE_UNCONFIRMED_TEXT.to_string()));
        } else {
            log::info!(
                "dispatch {agent_id}: wake for parent {short} not confirmed ({why}); typed again at the next idle"
            );
        }
    }

    fn on_timer(&mut self, agent_id: &str, token: u64, kind: TimerKind) {
        let state = self.state(agent_id).clone();
        match (state, kind) {
            (Delivery::Delaying { token: t }, TimerKind::DispatchDelay) if t == token => {
                self.set(agent_id, Delivery::Free);
                if self.stop_notices.contains_key(agent_id) || self.due_wake(agent_id).is_some() {
                    // A stop notice (review 5c W4) or a wake (step 6a) arrived during the delay:
                    // it goes first.
                    self.consider(agent_id);
                } else if let Some((snap, item)) = self.ready_item(agent_id) {
                    if let Some(left) = self.user_grace_left(&snap) {
                        // The user typed during the delay: try again when the grace is over.
                        log::info!("dispatch {agent_id}: user typed recently; waiting {left} ms");
                        self.schedule(agent_id, token, TimerKind::DispatchDelay, left);
                        self.set(agent_id, Delivery::Delaying { token });
                    } else {
                        match item {
                            Item::Work(ticket) => {
                                self.deliver_work(agent_id, &snap, &ticket, token)
                            }
                            Item::Review(ticket) => {
                                self.type_review(agent_id, &snap, &ticket, token)
                            }
                        }
                    }
                }
            }
            (
                Delivery::Typed {
                    ticket_id,
                    token: t,
                    kind,
                },
                TimerKind::SendEnter,
            ) if t == token => {
                if self.write_enter(agent_id, &ticket_id, kind) {
                    self.schedule(agent_id, token, TimerKind::Confirm, CONFIRM_TIMEOUT_MS);
                    self.set(
                        agent_id,
                        Delivery::Waiting {
                            ticket_id,
                            token,
                            retried: false,
                            from_spawn: false,
                            kind,
                        },
                    );
                }
            }
            (
                Delivery::Waiting {
                    ticket_id,
                    token: t,
                    kind: DeliveryKind::Wake,
                    ..
                },
                TimerKind::Confirm,
            ) if t == token => self.wake_unconfirmed(agent_id, &ticket_id, "timeout"),
            (
                Delivery::Waiting {
                    ticket_id,
                    token: t,
                    retried,
                    from_spawn,
                    kind,
                },
                TimerKind::Confirm,
            ) if t == token => {
                if !retried {
                    // Enter may have been swallowed (popup, invisible chars): press it once more.
                    if self.write_enter(agent_id, &ticket_id, kind) {
                        self.schedule(agent_id, token, TimerKind::Confirm, RETRY_TIMEOUT_MS);
                        self.set(
                            agent_id,
                            Delivery::Waiting {
                                ticket_id,
                                token,
                                retried: true,
                                from_spawn,
                                kind,
                            },
                        );
                    }
                } else if from_spawn {
                    log::info!(
                        "dispatch {agent_id}: positional prompt not confirmed; typing the ticket instead"
                    );
                    self.set(agent_id, Delivery::Free);
                    self.consider(agent_id);
                } else {
                    self.delivery_failed(agent_id, &ticket_id, DELIVERY_UNCONFIRMED_NOTE, kind);
                }
            }
            (
                Delivery::Nudging {
                    ticket_id,
                    token: t,
                    nudge,
                },
                TimerKind::NudgeDelay,
            ) if t == token => match nudge {
                Nudge::Submit => self.nudge_type(agent_id, &ticket_id, token),
                Nudge::Stop => self.stop_notice_type(agent_id, &ticket_id, token),
                Nudge::Wake => self.wake_type(agent_id, &ticket_id, token),
            },
            (
                Delivery::Nudging {
                    ticket_id,
                    token: t,
                    nudge,
                },
                TimerKind::NudgeEnter,
            ) if t == token => self.nudge_enter(agent_id, &ticket_id, nudge),
            (
                Delivery::AwaitingRestart {
                    ticket_id,
                    token: t,
                },
                TimerKind::RestartTimeout,
            ) if t == token => {
                // Fallback: the ticket goes into whatever session the agent has once it is idle
                // (`restarted_for` prevents a second restart).
                log::warn!(
                    "dispatch {agent_id}: no session {} s after the restart for ticket {}; delivering without it",
                    RESTART_TIMEOUT_MS / 1000,
                    super::model::short_id(&ticket_id)
                );
                let now = now_ms();
                if let Err(e) = self
                    .host
                    .mutate(|s| s.note_by_system(&ticket_id, RESTART_TIMEOUT_NOTE, now))
                {
                    log::warn!("dispatch {agent_id}: noting the restart timeout failed: {e}");
                }
                self.set(agent_id, Delivery::Free);
                self.consider(agent_id);
            }
            _ => log::debug!("dispatch {agent_id}: ignoring stale {kind:?} timer"),
        }
    }

    /// The delivery for the agent (5c C.1) with the step 4b sections: `## Delt projekt` for a
    /// real work delivery when other live work agents share the project (plan4b A.5), the
    /// project list for a coordination task (A.6). Step 6a: `## Del-tickets` for a ticket with
    /// children, and a planning task knows whether a coordinator runs (a live agent with the
    /// role, or the ticket's parent belongs to one).
    fn delivery_for(
        &self,
        agent_id: &str,
        snap: &AgentSnapshot,
        ticket: &Ticket,
    ) -> TicketDelivery {
        let mut delivery = TicketDelivery::for_agent(snap.seat_kind, &snap.roles)
            .with_children(self.host.read(|s| s.child_lines(&ticket.id)));
        if delivery.coordination == Some(prompt::CoordinationKind::Plan) {
            let available = self.port.has_live_coordinator() || self.parent_is_coordinators(ticket);
            delivery = delivery.with_coordinator(available);
        }
        if let (Some(p), SeatKind::Work, true) = (&snap.project, snap.seat_kind, delivery.is_work())
        {
            let others = self.port.peers_in_project(agent_id);
            if !others.is_empty() {
                delivery = delivery.with_shared(p, others);
            }
        }
        if !delivery.is_work() {
            let may_create = self.host.rules().agents_may_create_projects;
            delivery = delivery.with_projects(self.host.project_ids(), may_create);
        }
        delivery
    }

    /// Whether `ticket`'s parent has an assignee with the coordinator role (step 6a).
    fn parent_is_coordinators(&self, ticket: &Ticket) -> bool {
        let Some(parent) = ticket.parent_id.as_deref() else {
            return false;
        };
        self.host
            .read(|s| s.get(parent))
            .and_then(|p| p.assignee_agent_id)
            .and_then(|a| self.port.snapshot(&a))
            .is_some_and(|a| a.roles.contains(&Role::Coordinator))
    }

    /// The queue head's delivery after the dispatch delay (step 6b, plan A.7): the ticket's git
    /// branch first (a real work delivery), then [`fresh_decision`]. A restart that is accepted
    /// leaves the agent in [`Delivery::AwaitingRestart`] (timer [`TimerKind::RestartTimeout`]);
    /// the ticket file is written after it, in the new cwd. A refused restart types the ticket
    /// into the current session with a note, if the agent is still idle with it next.
    fn deliver_work(&mut self, agent_id: &str, snap: &AgentSnapshot, ticket: &Ticket, token: u64) {
        let delivery = self.delivery_for(agent_id, snap, ticket);
        let git = if delivery.is_work() {
            self.host.prepare_git(ticket, &snap.cwd)
        } else {
            None
        };
        let decision = if self.restarted_for.get(agent_id) == Some(&ticket.id) {
            FreshDecision::None
        } else {
            fresh_decision(&self.host.rules(), snap, ticket, git.as_ref())
        };
        let FreshDecision::Restart { cwd, force_fresh } = decision else {
            self.type_prepared(agent_id, snap, ticket, token, delivery.with_git(git));
            return;
        };
        let short = ticket.short_id();
        log::info!(
            "dispatch {agent_id}: fresh session for ticket {short} (cwd {}, {})",
            if cwd.is_some() { "changed" } else { "same" },
            if force_fresh {
                "new session"
            } else {
                "session kept"
            }
        );
        self.restarted_for
            .insert(agent_id.to_string(), ticket.id.clone());
        let req = RestartForTicket {
            agent_id: agent_id.to_string(),
            cwd,
            force_fresh,
            ticket_short: short.clone(),
        };
        match self.port.restart_fresh(req) {
            Ok(()) => {
                self.schedule(
                    agent_id,
                    token,
                    TimerKind::RestartTimeout,
                    RESTART_TIMEOUT_MS,
                );
                self.set(
                    agent_id,
                    Delivery::AwaitingRestart {
                        ticket_id: ticket.id.clone(),
                        token,
                    },
                );
            }
            Err(e) => {
                log::warn!("dispatch {agent_id}: restart for ticket {short} failed: {e}");
                // The agent may have exited (its tickets released) or started working: only a
                // still idle agent with the same queue head gets it now.
                match self.ready_item(agent_id) {
                    Some((snap, Item::Work(t))) if t.id == ticket.id => {
                        let now = now_ms();
                        let note = restart_failed_note(&e);
                        if let Err(e) = self.host.mutate(|s| s.note_by_system(&t.id, &note, now)) {
                            log::warn!(
                                "dispatch {agent_id}: noting the restart failure failed: {e}"
                            );
                        }
                        let t = self.host.read(|s| s.get(&t.id)).unwrap_or(t);
                        let delivery = self.delivery_for(agent_id, &snap, &t).with_git(git);
                        self.type_prepared(agent_id, &snap, &t, token, delivery);
                    }
                    _ => {
                        self.set(agent_id, Delivery::Free);
                        self.consider(agent_id);
                    }
                }
            }
        }
    }

    /// Writes the ticket file and types the line (a coordination task on a staff seat, 5c C.1);
    /// Enter follows after `ENTER_DELAY_MS`. Step 6b: a real work delivery asks the host for the
    /// ticket's git branch first (`## Git` in the file). The file is written in the agent's cwd:
    /// the ticket's worktree after the restart of [`Self::deliver_work`] or for an agent
    /// spawned with the ticket; otherwise the section names the worktree folder.
    fn type_ticket(&mut self, agent_id: &str, snap: &AgentSnapshot, ticket: &Ticket, token: u64) {
        let delivery = self.delivery_for(agent_id, snap, ticket);
        let delivery = if delivery.is_work() {
            let git = self.host.prepare_git(ticket, &snap.cwd);
            delivery.with_git(git)
        } else {
            delivery
        };
        self.type_prepared(agent_id, snap, ticket, token, delivery);
    }

    /// [`Self::type_ticket`] with the delivery (and its git branch) already decided.
    fn type_prepared(
        &mut self,
        agent_id: &str,
        snap: &AgentSnapshot,
        ticket: &Ticket,
        token: u64,
        delivery: TicketDelivery,
    ) {
        let now = now_ms();
        self.restarted_for.remove(agent_id);
        if let Err(e) = prompt::write_ticket_file(&snap.cwd, ticket, now, &delivery) {
            log::warn!("dispatch {agent_id}: writing the ticket file failed: {e}");
            self.send_to_backlog(
                agent_id,
                &ticket.id,
                &format!("Kunne ikke skrive ticket-fil: {e}"),
            );
            return;
        }
        let line = prompt::line_for(ticket, &delivery);
        if let Err(e) = self.port.write_input(agent_id, line.as_bytes()) {
            log::warn!("dispatch {agent_id}: typing the ticket line failed: {e}");
            self.send_to_backlog(agent_id, &ticket.id, TERMINAL_GONE_NOTE);
            return;
        }
        log::info!(
            "dispatch {agent_id}: typed ticket {} (coordination: {:?})",
            ticket.short_id(),
            delivery.coordination
        );
        self.schedule(agent_id, token, TimerKind::SendEnter, ENTER_DELAY_MS);
        self.set(
            agent_id,
            Delivery::Typed {
                ticket_id: ticket.id.clone(),
                token,
                kind: DeliveryKind::Work,
            },
        );
    }

    /// A report author as a display name: "brugeren", "appen" (step 6b), the agent's name, or
    /// its id when gone.
    fn author_name(&self, a: &ReportAuthor) -> String {
        match (a.kind, &a.agent_id) {
            (ReportAuthorKind::System, _) => "appen".to_string(),
            (ReportAuthorKind::User, _) | (_, None) => "brugeren".to_string(),
            (ReportAuthorKind::Agent, Some(id)) => self
                .port
                .snapshot(id)
                .map_or_else(|| id.clone(), |s| s.name),
        }
    }

    /// Writes the review file in the reviewer's folder and types the review line (C5.12);
    /// Enter follows after `ENTER_DELAY_MS`. A failure counts as a failed review delivery (the
    /// ticket stays in review with its reviewer).
    // TODO(windows-verify): the reviewer reads .mira-bots\reviews\<short>.md and runs
    // `git -C "<sender cwd>" diff .` without a prompt (allow rules); `git commit` is refused
    // (plan5 D.54).
    fn type_review(&mut self, agent_id: &str, snap: &AgentSnapshot, ticket: &Ticket, token: u64) {
        let sender = ticket
            .assignee_agent_id
            .as_deref()
            .and_then(|a| self.port.snapshot(a))
            .map(|s| ReviewSender {
                name: s.name,
                // Step 6b: the ticket's worktree (or repository) is where the work is, not the
                // sender's current folder.
                cwd: match &ticket.git {
                    Some(g) => g.worktree.clone().unwrap_or_else(|| g.repo.clone()),
                    None => s.cwd.to_string_lossy().into_owned(),
                },
            });
        let author = |a: &ReportAuthor| self.author_name(a);
        // Step 6a: a parent's review file lists its children (already reviewed).
        let children = self.host.read(|s| s.child_reviews(&ticket.id));
        let max = self.host.rules().max_review_rounds;
        let checks = self
            .host
            .project_checks(ticket.project.as_ref().and_then(|p| p.id()));
        if let Err(e) = prompt::write_review_file(
            &snap.cwd,
            ticket,
            sender.as_ref(),
            &author,
            &children,
            max,
            &checks,
        ) {
            log::warn!("dispatch {agent_id}: writing the review file failed: {e}");
            self.review_failed(agent_id, &ticket.id);
            return;
        }
        let line = prompt::review_line_for(ticket, sender.as_ref().map(|s| s.cwd.as_str()));
        if let Err(e) = self.port.write_input(agent_id, line.as_bytes()) {
            log::warn!("dispatch {agent_id}: typing the review line failed: {e}");
            self.review_failed(agent_id, &ticket.id);
            return;
        }
        log::info!(
            "dispatch {agent_id}: typed review of ticket {}",
            ticket.short_id()
        );
        self.schedule(agent_id, token, TimerKind::SendEnter, ENTER_DELAY_MS);
        self.set(
            agent_id,
            Delivery::Typed {
                ticket_id: ticket.id.clone(),
                token,
                kind: DeliveryKind::Review,
            },
        );
    }

    /// A review delivery failed: one attempt used (after [`REVIEW_DELIVERY_MAX_ATTEMPTS`] the
    /// history says so and no automatic retries follow), the reviewer gets
    /// [`DELIVERY_FAILED_TEXT`]. The assignment stays; the next Idle tries again.
    ///
    /// [`REVIEW_DELIVERY_MAX_ATTEMPTS`]: crate::config::REVIEW_DELIVERY_MAX_ATTEMPTS
    fn review_failed(&mut self, agent_id: &str, ticket_id: &str) {
        self.set(agent_id, Delivery::Free);
        let now = now_ms();
        match self
            .host
            .mutate(|s| s.review_delivery_failed(ticket_id, now))
        {
            Ok(n) => {
                log::warn!("dispatch {agent_id}: review of {ticket_id} not delivered (attempt {n})")
            }
            Err(e) => log::warn!("dispatch {agent_id}: noting the review failure failed: {e}"),
        }
        self.port
            .set_detail(agent_id, Some(DELIVERY_FAILED_TEXT.to_string()));
    }

    /// Sends `\r` on its own. On failure a work ticket goes to the backlog (a review counts as
    /// a failed delivery) and `false` is returned.
    // TODO(windows-verify): a separate `\r` write submits the typed line under ConPTY (plan D.28).
    fn write_enter(&mut self, agent_id: &str, ticket_id: &str, kind: DeliveryKind) -> bool {
        match self.port.write_input(agent_id, b"\r") {
            Ok(()) => true,
            Err(e) => {
                log::warn!("dispatch {agent_id}: sending Enter failed: {e}");
                match kind {
                    DeliveryKind::Work => {
                        self.send_to_backlog(agent_id, ticket_id, TERMINAL_GONE_NOTE)
                    }
                    DeliveryKind::Review => self.review_failed(agent_id, ticket_id),
                    DeliveryKind::Wake => self.set(agent_id, Delivery::Free),
                }
                false
            }
        }
    }

    fn send_to_backlog(&mut self, agent_id: &str, ticket_id: &str, note: &str) {
        self.set(agent_id, Delivery::Free);
        let now = now_ms();
        if let Err(e) = self.host.mutate(|s| s.to_backlog(ticket_id, note, now)) {
            log::warn!("dispatch {agent_id}: moving ticket {ticket_id} to the backlog failed: {e}");
        }
    }

    /// No confirmation after the retry, or another prompt went in: the ticket stays first in
    /// the queue with an issue and `note` in its history (a review: see [`Self::review_failed`]).
    fn delivery_failed(&mut self, agent_id: &str, ticket_id: &str, note: &str, kind: DeliveryKind) {
        if kind == DeliveryKind::Wake {
            self.wake_unconfirmed(agent_id, ticket_id, note);
            return;
        }
        if kind == DeliveryKind::Review {
            log::info!("dispatch {agent_id}: review delivery of {ticket_id} failed ({note})");
            self.review_failed(agent_id, ticket_id);
            return;
        }
        self.set(agent_id, Delivery::Free);
        log::warn!("dispatch {agent_id}: delivery of ticket {ticket_id} failed ({note})");
        let now = now_ms();
        let r = self.host.mutate(|s| {
            s.set_issue(
                ticket_id,
                Some(TicketIssue::DeliveryFailed),
                Some(note.into()),
                now,
            )
        });
        if let Err(e) = r {
            log::warn!("dispatch {agent_id}: marking the delivery failure failed: {e}");
        }
        self.port
            .set_detail(agent_id, Some(DELIVERY_FAILED_TEXT.to_string()));
    }

    /// "Send igen": the queue head or the ticket in progress, to an idle agent with no delivery
    /// running. Clears the issue and types the ticket right away. For a ticket in review with a
    /// reviewer: the review's delivery attempts start over and it is offered to the reviewer.
    fn redispatch(&mut self, ticket_id: &str) {
        let Some(t) = self.host.read(|s| s.get(ticket_id)) else {
            log::info!("redispatch: ticket {ticket_id} not found");
            return;
        };
        if t.state == TicketState::Review {
            match self.host.mutate(|s| s.reset_review_delivery(ticket_id)) {
                Ok(a) => self.consider(&a.reviewer_agent_id),
                Err(e) => log::info!(
                    "redispatch: review of {} not deliverable: {e}",
                    t.short_id()
                ),
            }
            return;
        }
        let Some(agent_id) = t.assignee_agent_id.clone() else {
            log::info!("redispatch: ticket {} has no agent", t.short_id());
            return;
        };
        let eligible = match t.state {
            TicketState::InProgress => true,
            TicketState::Assigned => {
                self.host
                    .read(|s| s.next_for_agent(&agent_id))
                    .map(|n| n.id)
                    == Some(t.id.clone())
            }
            _ => false,
        };
        let snap = self.port.snapshot(&agent_id);
        let idle = snap.as_ref().is_some_and(|s| s.status == AgentStatus::Idle);
        if !eligible || !idle || *self.state(&agent_id) != Delivery::Free {
            match self.host.read(|s| s.blocked_text(&t.id)) {
                // Step 6a: a blocked ticket is skipped by `next_for_agent` and never re-sent.
                Some(b) => log::info!("redispatch: ticket {} waits for {b}", t.short_id()),
                None => log::info!("redispatch: ticket {} is not deliverable now", t.short_id()),
            }
            return;
        }
        let now = now_ms();
        let ticket = match self
            .host
            .mutate(|s| s.set_issue(ticket_id, None, None, now))
        {
            Ok(t) => t,
            Err(e) => {
                log::warn!("redispatch: clearing the issue failed: {e}");
                return;
            }
        };
        let token = self.token();
        if let Some(snap) = snap {
            self.type_ticket(&agent_id, &snap, &ticket, token);
        }
    }
}

/// The dispatcher task: one message at a time until all senders are gone.
pub async fn run<H: TicketsHost, P: AgentPort, T: Timers>(
    mut rx: UnboundedReceiver<DispatchMsg>,
    mut d: Dispatcher<H, P, T>,
) {
    while let Some(m) = rx.recv().await {
        d.handle(m);
    }
}

/// Timers on the Tauri async runtime; the message comes back through the dispatcher's channel.
pub struct RealTimers {
    tx: UnboundedSender<DispatchMsg>,
}

impl RealTimers {
    pub fn new(tx: UnboundedSender<DispatchMsg>) -> Self {
        RealTimers { tx }
    }
}

impl Timers for RealTimers {
    fn schedule(&mut self, delay_ms: u64, msg: DispatchMsg) {
        let tx = self.tx.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            // The receiver is gone only at shutdown.
            let _ = tx.send(msg);
        });
    }

    fn now_ms(&self) -> u64 {
        now_ms()
    }
}

/// Manual clock for tests: `advance` returns what came due, in (time, schedule order).
/// Clones share the clock, so a test keeps one while the dispatcher owns another.
#[doc(hidden)]
#[derive(Clone, Default)]
pub struct FakeTimers {
    inner: Arc<Mutex<FakeClock>>,
}

#[derive(Default)]
struct FakeClock {
    now: u64,
    seq: u64,
    due: Vec<(u64, u64, DispatchMsg)>,
}

impl FakeTimers {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, FakeClock> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn now(&self) -> u64 {
        self.lock().now
    }

    pub fn pending(&self) -> usize {
        self.lock().due.len()
    }

    /// Removes and returns the earliest message due at or before `until`, moving the clock to
    /// its due time. Messages scheduled while handling it are seen by the next call.
    pub fn pop_due(&self, until: u64) -> Option<DispatchMsg> {
        let mut c = self.lock();
        let i = c
            .due
            .iter()
            .enumerate()
            .filter(|(_, (at, _, _))| *at <= until)
            .min_by_key(|(_, (at, seq, _))| (*at, *seq))
            .map(|(i, _)| i)?;
        let (at, _, msg) = c.due.remove(i);
        c.now = c.now.max(at);
        Some(msg)
    }

    /// Moves the clock forward by `ms` and returns every message due by then.
    pub fn advance(&self, ms: u64) -> Vec<DispatchMsg> {
        let until = self.now() + ms;
        let mut out = Vec::new();
        while let Some(m) = self.pop_due(until) {
            out.push(m);
        }
        self.lock().now = until;
        out
    }
}

impl Timers for FakeTimers {
    fn schedule(&mut self, delay_ms: u64, msg: DispatchMsg) {
        let mut c = self.lock();
        c.seq += 1;
        let entry = (c.now + delay_ms, c.seq, msg);
        c.due.push(entry);
    }

    fn now_ms(&self) -> u64 {
        self.now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AUTO_REVIEW_ON_STOP;
    use crate::hooks::event::parse;
    use crate::hooks::fixtures as fx;
    use crate::hooks::status::apply;
    use crate::tickets::model::{TicketActor, TicketDoc};
    use crate::tickets::service::RejectReturn;
    use crate::tickets::store::MemoryStore;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use TicketState as S;

    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap()
    }

    /// Host over a shared service; counts successful mutations (stand-in for the emits).
    #[derive(Clone)]
    struct TestHost {
        svc: Arc<Mutex<TicketService>>,
        mutations: Arc<AtomicUsize>,
        rules: WorkspaceRules,
        /// Step 6b: what `prepare_git` prepares for a ticket without git (saved like the app).
        git: Arc<Mutex<Option<TicketGit>>>,
        git_calls: Arc<AtomicUsize>,
    }

    impl TicketsHost for TestHost {
        fn mutate<R>(
            &self,
            f: impl FnOnce(&mut TicketService) -> Result<R, TicketError>,
        ) -> Result<R, String> {
            let r = f(&mut lock(&self.svc)).map_err(String::from);
            if r.is_ok() {
                self.mutations.fetch_add(1, Ordering::SeqCst);
            }
            r
        }

        fn read<R>(&self, f: impl FnOnce(&TicketService) -> R) -> R {
            f(&lock(&self.svc))
        }

        fn rules(&self) -> WorkspaceRules {
            self.rules
        }

        fn prepare_git(&self, ticket: &Ticket, _cwd: &Path) -> Option<TicketGit> {
            self.git_calls.fetch_add(1, Ordering::SeqCst);
            if ticket.git.is_some() {
                return ticket.git.clone();
            }
            let git = lock(&self.git).clone()?;
            lock(&self.svc)
                .set_git(&ticket.id, Some(git.clone()), 5)
                .unwrap();
            Some(git)
        }
    }

    #[derive(Default)]
    struct PortState {
        snapshots: HashMap<String, AgentSnapshot>,
        writes: Vec<(String, Vec<u8>)>,
        fail_writes: bool,
        /// `has_live_coordinator` (step 6a).
        coordinator_live: bool,
        /// Step 6b: every `restart_fresh` request; `Some(err)` refuses them.
        restarts: Vec<RestartForTicket>,
        fail_restart: Option<String>,
        /// The failed restart leaves the agent exited (the new child could not start).
        exit_on_failed_restart: bool,
    }

    #[derive(Clone, Default)]
    struct FakePort(Arc<Mutex<PortState>>);

    impl AgentPort for FakePort {
        fn has_live_coordinator(&self) -> bool {
            lock(&self.0).coordinator_live
        }

        /// Like the app: the agent starts again (no conversation, the new cwd); the test sends
        /// `AgentRestarting` and the Idle of the new session itself.
        fn restart_fresh(&self, req: RestartForTicket) -> Result<(), String> {
            let mut s = lock(&self.0);
            s.restarts.push(req.clone());
            if let Some(e) = s.fail_restart.clone() {
                if s.exit_on_failed_restart {
                    if let Some(snap) = s.snapshots.get_mut(&req.agent_id) {
                        snap.status = AgentStatus::Exited { code: None };
                    }
                }
                return Err(e);
            }
            let snap = s
                .snapshots
                .get_mut(&req.agent_id)
                .ok_or("Agenten kører ikke")?;
            snap.status = AgentStatus::Starting;
            snap.has_conversation = false;
            if let Some(cwd) = req.cwd {
                snap.cwd = cwd;
            }
            Ok(())
        }

        fn snapshot(&self, id: &str) -> Option<AgentSnapshot> {
            lock(&self.0).snapshots.get(id).cloned()
        }

        fn write_input(&self, id: &str, bytes: &[u8]) -> Result<(), String> {
            let mut s = lock(&self.0);
            if s.fail_writes || !s.snapshots.contains_key(id) {
                return Err("Agenten findes ikke".into());
            }
            s.writes.push((id.to_string(), bytes.to_vec()));
            Ok(())
        }

        fn set_detail(&self, id: &str, detail: Option<String>) -> bool {
            match lock(&self.0).snapshots.get_mut(id) {
                Some(s) => {
                    s.detail = detail;
                    true
                }
                None => false,
            }
        }

        fn peers_in_project(&self, id: &str) -> Vec<String> {
            let s = lock(&self.0);
            let Some(p) = s.snapshots.get(id).and_then(|a| a.project.clone()) else {
                return Vec::new();
            };
            let mut names: Vec<String> = s
                .snapshots
                .iter()
                .filter(|(other, a)| {
                    other.as_str() != id
                        && a.seat_kind == SeatKind::Work
                        && !matches!(a.status, AgentStatus::Exited { .. })
                        && a.project
                            .as_deref()
                            .is_some_and(|x| crate::projects::same_id(x, &p))
                })
                .map(|(_, a)| a.name.clone())
                .collect();
            names.sort();
            names
        }
    }

    struct Harness {
        d: Dispatcher<TestHost, FakePort, FakeTimers>,
        svc: Arc<Mutex<TicketService>>,
        /// The host's `prepare_git` answer and call count (step 6b).
        git: Arc<Mutex<Option<TicketGit>>>,
        git_calls: Arc<AtomicUsize>,
        port: FakePort,
        timers: FakeTimers,
        root: PathBuf,
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    impl Harness {
        fn new() -> Self {
            let svc = Arc::new(Mutex::new(TicketService::new(
                Box::new(MemoryStore::new()),
                TicketDoc::default(),
            )));
            let host = TestHost {
                svc: svc.clone(),
                mutations: Arc::default(),
                rules: WorkspaceRules::defaults(),
                git: Arc::default(),
                git_calls: Arc::default(),
            };
            let (git, git_calls) = (host.git.clone(), host.git_calls.clone());
            let port = FakePort::default();
            let timers = FakeTimers::new();
            let root = std::env::temp_dir().join(format!("mira-dispatch-{}", uuid::Uuid::new_v4()));
            Harness {
                d: Dispatcher::new(host, port.clone(), timers.clone()),
                svc,
                git,
                git_calls,
                port,
                timers,
                root,
            }
        }

        /// A live agent with its own cwd (work seat, coder: gets the plain work delivery; review
        /// 5c W1).
        fn agent(&self, id: &str, status: AgentStatus) {
            self.agent_on(id, status, SeatKind::Work, &[Role::Coder]);
        }

        /// A live agent on `seat` with `roles`.
        fn agent_on(&self, id: &str, status: AgentStatus, seat_kind: SeatKind, roles: &[Role]) {
            let cwd = self.root.join(id);
            fs::create_dir_all(&cwd).unwrap();
            lock(&self.port.0).snapshots.insert(
                id.to_string(),
                AgentSnapshot {
                    name: format!("bot-{id}"),
                    cwd,
                    status,
                    detail: None,
                    last_user_input_at: None,
                    seat_kind,
                    roles: roles.to_vec(),
                    project: None,
                    has_conversation: false,
                },
            );
        }

        /// Puts the agent in `project` (plan4b A.1).
        fn in_project(&self, id: &str, project: &str) {
            lock(&self.port.0).snapshots.get_mut(id).unwrap().project = Some(project.to_string());
        }

        /// The user typed into `id`'s terminal now (fake clock).
        fn user_typed(&self, id: &str) {
            let now = self.timers.now();
            lock(&self.port.0)
                .snapshots
                .get_mut(id)
                .unwrap()
                .last_user_input_at = Some(now);
        }

        fn set_status(&self, id: &str, status: AgentStatus) {
            lock(&self.port.0).snapshots.get_mut(id).unwrap().status = status;
        }

        fn remove_agent(&self, id: &str) {
            lock(&self.port.0).snapshots.remove(id);
        }

        fn detail(&self, id: &str) -> Option<String> {
            lock(&self.port.0)
                .snapshots
                .get(id)
                .and_then(|s| s.detail.clone())
        }

        fn svc(&self) -> MutexGuard<'_, TicketService> {
            lock(&self.svc)
        }

        /// Creates a ticket and queues it for `agent`.
        fn queued(&self, agent: &str, title: &str) -> Ticket {
            let mut s = self.svc();
            let t = s.create(title, "Gør det", false, 1).unwrap();
            s.assign(&t.id, agent, 2).unwrap()
        }

        fn ticket(&self, id: &str) -> Ticket {
            self.svc().get(id).unwrap()
        }

        fn send(&mut self, m: DispatchMsg) {
            self.d.handle(m);
        }

        fn idle(&mut self, agent: &str) {
            self.send(DispatchMsg::AgentIdle {
                agent_id: agent.into(),
            });
        }

        fn submitted(&mut self, agent: &str, prompt: &str) {
            self.send(DispatchMsg::PromptSubmitted {
                agent_id: agent.into(),
                prompt: Some(prompt.into()),
            });
        }

        /// Advances the fake clock, handling every timer that comes due (including ones
        /// scheduled on the way).
        fn advance(&mut self, ms: u64) {
            let until = self.timers.now() + ms;
            while let Some(m) = self.timers.pop_due(until) {
                self.d.handle(m);
            }
            assert!(self.timers.advance(0).is_empty());
            let _ = self.timers.advance(until - self.timers.now());
        }

        fn writes(&self) -> Vec<(String, String)> {
            lock(&self.port.0)
                .writes
                .iter()
                .map(|(a, b)| (a.clone(), String::from_utf8(b.clone()).unwrap()))
                .collect()
        }

        fn ticket_file(&self, agent: &str, t: &Ticket) -> String {
            fs::read_to_string(
                prompt::ticket_dir(&self.root.join(agent)).join(format!("{}.md", t.short_id())),
            )
            .unwrap()
        }
    }

    fn line(t: &Ticket) -> String {
        prompt::line_for(t, &TicketDelivery::work())
    }

    fn enter(agent: &str) -> (String, String) {
        (agent.to_string(), "\r".to_string())
    }

    /// Delivers `t` to `agent` up to (but not including) the confirmation.
    fn deliver_until_enter(h: &mut Harness, agent: &str) {
        h.idle(agent);
        h.advance(DISPATCH_DELAY_MS + ENTER_DELAY_MS);
    }

    // (1)
    #[test]
    fn idle_agent_gets_line_then_separate_enter() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        let t = h.queued("a1", "Ret login");
        h.idle("a1");
        h.advance(DISPATCH_DELAY_MS - 1);
        assert!(h.writes().is_empty());
        h.advance(1);
        assert_eq!(h.writes(), vec![("a1".into(), line(&t))]);
        assert!(!h.writes()[0].1.contains('\r') && !h.writes()[0].1.contains('\n'));
        assert!(h.ticket_file("a1", &t).contains("## Opgave\n\nGør det\n"));
        h.advance(ENTER_DELAY_MS);
        assert_eq!(h.writes(), vec![("a1".into(), line(&t)), enter("a1")]);
        assert_eq!(h.ticket(&t.id).state, S::Assigned);
    }

    // 5c C.1: an agent on a staff seat gets the ticket as a coordination task; the
    // "Koordinér ticket" line confirms it like a ticket line.
    #[test]
    fn staff_seat_gets_coordination_task() {
        use prompt::{CoordinationKind, COORDINATION_DISTRIBUTE_TEXT, COORDINATION_PLAN_TEXT};
        let mut h = Harness::new();
        h.agent_on(
            "k1",
            AgentStatus::Idle,
            SeatKind::Staff,
            &[Role::Coordinator],
        );
        h.agent_on("r1", AgentStatus::Idle, SeatKind::Staff, &[Role::Reviewer]);
        // A work seat with a work role: the plain delivery, also with the coordinator role.
        h.agent_on(
            "w1",
            AgentStatus::Idle,
            SeatKind::Work,
            &[Role::Coder, Role::Coordinator],
        );
        let tk = h.queued("k1", "Lav en side");
        let tr = h.queued("r1", "Plan noget");
        let tw = h.queued("w1", "Kod noget");
        for a in ["k1", "r1", "w1"] {
            h.idle(a);
        }
        h.advance(DISPATCH_DELAY_MS);
        let distribute = TicketDelivery {
            coordination: Some(CoordinationKind::Distribute),
            ..TicketDelivery::default()
        };
        let plan = TicketDelivery {
            coordination: Some(CoordinationKind::Plan),
            ..TicketDelivery::default()
        };
        let writes = h.writes();
        let typed = |a: &str| writes.iter().find(|(x, _)| x == a).unwrap().1.clone();
        assert_eq!(typed("k1"), prompt::line_for(&tk, &distribute));
        assert!(typed("k1").starts_with(&format!("Koordiner ticket {}: ", tk.short_id())));
        assert_eq!(typed("r1"), prompt::line_for(&tr, &plan));
        assert_eq!(typed("w1"), line(&tw));
        let fk = h.ticket_file("k1", &tk);
        // Step 4b: the coordination task lists the projects (none in the test host).
        let projects = "Projekter lige nu: ingen. Angiv `project` på hver ticket du opretter; nye projekter skal brugeren oprette (agentsMayCreateProjects er slået fra). Mangler ticketen et projekt, angiv `project` når du giver den videre (mira_assign_ticket/mira_handoff_ticket).";
        assert!(fk.contains(&format!(
            "## Koordineringsopgave\n{COORDINATION_DISTRIBUTE_TEXT}\n{projects}\n\n## Regler\n"
        )));
        let fr = h.ticket_file("r1", &tr);
        assert!(fr.contains(&format!(
            "## Koordineringsopgave\n{COORDINATION_PLAN_TEXT}\n{projects}\n\n## Regler\n"
        )));
        assert!(!h.ticket_file("w1", &tw).contains("Koordineringsopgave"));
        h.advance(ENTER_DELAY_MS);
        // The coordination line confirms the delivery (UserPromptSubmit prefix).
        h.submitted("k1", &typed("k1"));
        assert_eq!(h.ticket(&tk.id).state, S::InProgress);
        h.submitted("w1", &typed("w1"));
        assert_eq!(h.ticket(&tw.id).state, S::InProgress);
        // Another ticket's coordination line does not.
        h.submitted("r1", &format!("Koordinér ticket {}: x", tk.short_id()));
        assert_eq!(h.ticket(&tr.id).state, S::Assigned);
    }

    // (2) + busy status as confirmation
    #[test]
    fn prompt_submit_or_busy_status_confirms_the_delivery() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        let t = h.queued("a1", "Ret login");
        deliver_until_enter(&mut h, "a1");
        h.submitted("a1", &format!("Ticket {}: Ret login. Læs …", t.short_id()));
        let now = h.ticket(&t.id);
        assert_eq!(now.state, S::InProgress);
        assert_eq!(
            now.history.last().unwrap().note.as_deref(),
            Some("sendt til bot-a1")
        );
        assert_eq!(h.svc().links().get("a1"), Some(&(Some(t.id.clone()), 0)));
        assert!(h.d.host.mutations.load(Ordering::SeqCst) >= 1);
        // The pending confirm timer is now stale.
        h.advance(CONFIRM_TIMEOUT_MS + RETRY_TIMEOUT_MS);
        assert_eq!(h.writes().len(), 2);

        // Busy status instead of UserPromptSubmit.
        h.agent("a2", AgentStatus::Idle);
        let t2 = h.queued("a2", "Andet");
        deliver_until_enter(&mut h, "a2");
        h.send(DispatchMsg::AgentBusy {
            agent_id: "a2".into(),
        });
        assert_eq!(h.ticket(&t2.id).state, S::InProgress);
    }

    // review3 F2: the user typed shortly before the agent became idle.
    #[test]
    fn recent_user_input_postpones_the_dispatch() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        let t = h.queued("a1", "Ret login");
        h.advance(1000);
        h.user_typed("a1");
        h.advance(2000);
        h.idle("a1");
        // 3000 ms of the grace are left (more than the usual delay).
        h.advance(USER_INPUT_GRACE_MS - 2000 - 1);
        assert!(h.writes().is_empty());
        h.advance(1);
        assert_eq!(h.writes(), vec![("a1".into(), line(&t))]);
        h.advance(ENTER_DELAY_MS);
        assert_eq!(h.writes().len(), 2);

        // Typing during the usual delay pushes the dispatch to the end of the grace period.
        h.agent("a2", AgentStatus::Idle);
        let t2 = h.queued("a2", "Andet");
        h.idle("a2");
        h.advance(DISPATCH_DELAY_MS - 250);
        h.user_typed("a2");
        h.advance(250);
        let a2_writes = |h: &Harness| h.writes().into_iter().filter(|(a, _)| a == "a2").count();
        assert_eq!(a2_writes(&h), 0);
        h.advance(USER_INPUT_GRACE_MS - 250 - 1);
        assert_eq!(a2_writes(&h), 0);
        h.advance(1);
        assert_eq!(h.writes().last(), Some(&("a2".to_string(), line(&t2))));
        // No double sequence was started on the way.
        h.idle("a2");
        h.advance(ENTER_DELAY_MS);
        assert_eq!(a2_writes(&h), 2);

        // Input older than the grace period does not delay anything.
        h.agent("a3", AgentStatus::Idle);
        h.user_typed("a3");
        h.advance(USER_INPUT_GRACE_MS);
        let t3 = h.queued("a3", "Tredje");
        h.idle("a3");
        h.advance(DISPATCH_DELAY_MS);
        assert_eq!(h.writes().last(), Some(&("a3".to_string(), line(&t3))));
    }

    // review3 F2: a different prompt while our line is in the terminal aborts the delivery, and
    // a busy status after it does not confirm the ticket.
    #[test]
    fn foreign_prompt_while_delivering_aborts() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        let t = h.queued("a1", "Ret login");
        h.queued("a1", "Næste");
        deliver_until_enter(&mut h, "a1");
        h.submitted("a1", "mit eget udkastTicket abc: Ret login");
        h.send(DispatchMsg::AgentBusy {
            agent_id: "a1".into(),
        });
        let now = h.ticket(&t.id);
        assert_eq!((now.state, now.queue_position), (S::Assigned, Some(0)));
        assert_eq!(now.issue, Some(TicketIssue::DeliveryFailed));
        assert_eq!(
            now.history.last().unwrap().note.as_deref(),
            Some(USER_TYPED_NOTE)
        );
        assert_eq!(h.detail("a1").as_deref(), Some(DELIVERY_FAILED_TEXT));
        // No retry Enter: the confirm timer is stale.
        h.advance(CONFIRM_TIMEOUT_MS + RETRY_TIMEOUT_MS);
        assert_eq!(h.writes().len(), 2);

        // Same while the line is typed but Enter not yet sent: the Enter is never sent.
        h.agent("a2", AgentStatus::Idle);
        let t2 = h.queued("a2", "Andet");
        h.idle("a2");
        h.advance(DISPATCH_DELAY_MS);
        assert_eq!(h.writes().last(), Some(&("a2".to_string(), line(&t2))));
        h.submitted("a2", "noget andet");
        h.advance(ENTER_DELAY_MS + CONFIRM_TIMEOUT_MS + RETRY_TIMEOUT_MS);
        assert_eq!(h.writes().last(), Some(&("a2".to_string(), line(&t2))));
        let now = h.ticket(&t2.id);
        assert_eq!(
            (now.state, now.issue),
            (S::Assigned, Some(TicketIssue::DeliveryFailed))
        );

        // A matching prompt still confirms (see also prompt_submit_or_busy_status_confirms_…).
        h.agent("a3", AgentStatus::Idle);
        let t3 = h.queued("a3", "Tredje");
        deliver_until_enter(&mut h, "a3");
        h.submitted("a3", &format!("  Ticket {}: Tredje.", t3.short_id()));
        assert_eq!(h.ticket(&t3.id).state, S::InProgress);
    }

    // (3)
    #[test]
    fn unconfirmed_delivery_retries_enter_once_then_fails() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        let t = h.queued("a1", "Ret login");
        deliver_until_enter(&mut h, "a1");
        h.advance(CONFIRM_TIMEOUT_MS - 1);
        assert_eq!(h.writes().len(), 2);
        h.advance(1);
        assert_eq!(h.writes().last(), Some(&enter("a1")));
        assert_eq!(h.writes().len(), 3);
        h.advance(RETRY_TIMEOUT_MS);
        assert_eq!(h.writes().len(), 3);
        let now = h.ticket(&t.id);
        assert_eq!((now.state, now.queue_position), (S::Assigned, Some(0)));
        assert_eq!(now.issue, Some(TicketIssue::DeliveryFailed));
        assert_eq!(
            now.history.last().unwrap().note.as_deref(),
            Some(DELIVERY_UNCONFIRMED_NOTE)
        );
        assert_eq!(h.detail("a1").as_deref(), Some(DELIVERY_FAILED_TEXT));
        assert_eq!(h.timers.pending(), 0);
        // Next idle tries again; a confirmation clears issue and detail.
        deliver_until_enter(&mut h, "a1");
        assert_eq!(h.writes().len(), 5);
        h.submitted("a1", &line(&t));
        let now = h.ticket(&t.id);
        assert_eq!((now.state, now.issue), (S::InProgress, None));
        assert_eq!(h.detail("a1"), None);
    }

    // (4)
    #[test]
    fn busy_agent_is_not_typed_into_until_idle() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Running);
        let t = h.queued("a1", "Ret login");
        h.send(DispatchMsg::QueueChanged {
            agent_id: "a1".into(),
        });
        h.idle("a1"); // status still Running in the manager
        h.advance(10_000);
        assert!(h.writes().is_empty());
        // Became busy again during the delay: the re-check stops the sequence.
        h.set_status("a1", AgentStatus::Idle);
        h.idle("a1");
        h.set_status("a1", AgentStatus::WaitingPermission);
        h.advance(DISPATCH_DELAY_MS);
        assert!(h.writes().is_empty());
        h.set_status("a1", AgentStatus::Idle);
        h.idle("a1");
        h.advance(DISPATCH_DELAY_MS);
        assert_eq!(h.writes(), vec![("a1".into(), line(&t))]);
    }

    // (5) Step 3 behaviour, kept behind AUTO_REVIEW_ON_STOP = true.
    #[test]
    fn turn_end_moves_the_ticket_to_review_or_done() {
        let mut h = Harness::new();
        h.d.auto_review_on_stop = Some(true);
        h.agent("a1", AgentStatus::Idle);
        let a = h.queued("a1", "A");
        let b = h.queued("a1", "B");
        h.svc()
            .update(
                &b.id,
                crate::tickets::model::TicketPatch {
                    skip_review: Some(true),
                    ..Default::default()
                },
                3,
            )
            .unwrap();
        deliver_until_enter(&mut h, "a1");
        h.submitted("a1", &line(&a));
        h.set_status("a1", AgentStatus::Idle);
        h.send(DispatchMsg::TurnEnded {
            agent_id: "a1".into(),
            failed: false,
        });
        h.idle("a1");
        let ra = h.ticket(&a.id);
        assert_eq!(ra.state, S::Review);
        assert_eq!(ra.history.last().unwrap().by, TicketActor::System);
        // The next ticket follows.
        h.advance(DISPATCH_DELAY_MS + ENTER_DELAY_MS);
        let b = h.ticket(&b.id);
        assert_eq!(h.writes()[2], ("a1".into(), line(&b)));
        h.submitted("a1", &line(&b));
        h.send(DispatchMsg::TurnEnded {
            agent_id: "a1".into(),
            failed: false,
        });
        assert_eq!(h.ticket(&b.id).state, S::Done);
    }

    // (6)
    #[test]
    fn stop_failure_keeps_the_ticket_in_progress_with_an_issue() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        let t = h.queued("a1", "A");
        h.queued("a1", "B");
        deliver_until_enter(&mut h, "a1");
        h.submitted("a1", &line(&t));
        h.send(DispatchMsg::TurnEnded {
            agent_id: "a1".into(),
            failed: true,
        });
        h.idle("a1");
        h.advance(10_000);
        let now = h.ticket(&t.id);
        assert_eq!(
            (now.state, now.issue),
            (S::InProgress, Some(TicketIssue::TurnFailed))
        );
        assert_eq!(
            now.history.last().unwrap().note.as_deref(),
            Some(TURN_FAILED_NOTE)
        );
        assert_eq!(h.detail("a1").as_deref(), Some(TURN_FAILED_TEXT));
        // No second ticket while one is in progress.
        assert_eq!(h.writes().len(), 2);
    }

    // (7)
    #[test]
    fn redispatch_of_a_failed_turn_types_again() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        let t = h.queued("a1", "A");
        deliver_until_enter(&mut h, "a1");
        h.submitted("a1", &line(&t));
        h.send(DispatchMsg::TurnEnded {
            agent_id: "a1".into(),
            failed: true,
        });
        // Not while the agent is busy.
        h.set_status("a1", AgentStatus::Thinking);
        h.send(DispatchMsg::Redispatch {
            ticket_id: t.id.clone(),
        });
        assert_eq!(h.writes().len(), 2);
        h.set_status("a1", AgentStatus::Idle);
        h.send(DispatchMsg::Redispatch {
            ticket_id: t.id.clone(),
        });
        assert_eq!(h.writes().len(), 3);
        assert_eq!(h.writes()[2], ("a1".into(), line(&t)));
        assert_eq!(h.ticket(&t.id).issue, None);
        h.advance(ENTER_DELAY_MS);
        assert_eq!(h.writes()[3], enter("a1"));
        h.submitted("a1", &line(&t));
        let now = h.ticket(&t.id);
        assert_eq!((now.state, now.issue), (S::InProgress, None));
        assert_eq!(
            now.history.last().unwrap().note.as_deref(),
            Some("sendt igen")
        );
        assert_eq!(h.detail("a1"), None);
        // A backlog ticket cannot be redispatched.
        let other = h.svc().create("x", "", false, 1).unwrap();
        h.send(DispatchMsg::Redispatch {
            ticket_id: other.id,
        });
        assert_eq!(h.writes().len(), 4);
    }

    // (8)
    #[test]
    fn rejected_ticket_goes_first_and_its_file_has_the_note() {
        let mut h = Harness::new();
        h.d.auto_review_on_stop = Some(true);
        h.agent("a1", AgentStatus::Idle);
        let a = h.queued("a1", "A");
        let b = h.queued("a1", "B");
        deliver_until_enter(&mut h, "a1");
        h.submitted("a1", &line(&a));
        h.set_status("a1", AgentStatus::Thinking);
        h.send(DispatchMsg::TurnEnded {
            agent_id: "a1".into(),
            failed: false,
        });
        assert_eq!(h.ticket(&a.id).state, S::Review);
        h.svc()
            .reject(&a.id, "Mangler test", RejectReturn::Sender, 10)
            .unwrap();
        assert_eq!(h.ticket(&a.id).queue_position, Some(0));
        assert_eq!(h.ticket(&b.id).queue_position, Some(1));
        h.set_status("a1", AgentStatus::Idle);
        h.send(DispatchMsg::QueueChanged {
            agent_id: "a1".into(),
        });
        h.advance(DISPATCH_DELAY_MS);
        let a_now = h.ticket(&a.id);
        assert_eq!(h.writes().last(), Some(&("a1".into(), line(&a_now))));
        assert!(h
            .ticket_file("a1", &a_now)
            .contains("## Afvist: Mangler test\n"));
    }

    // (9)
    #[test]
    fn agent_gone_mid_delivery_stops_writing() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        let a = h.queued("a1", "A");
        let b = h.queued("a1", "B");
        deliver_until_enter(&mut h, "a1");
        h.svc().release_agent("a1", "agent stoppet", 20).unwrap();
        h.remove_agent("a1");
        h.send(DispatchMsg::AgentGone {
            agent_id: "a1".into(),
        });
        h.advance(CONFIRM_TIMEOUT_MS + RETRY_TIMEOUT_MS + DISPATCH_DELAY_MS);
        assert_eq!(h.writes().len(), 2);
        for id in [&a.id, &b.id] {
            let t = h.ticket(id);
            assert_eq!((t.state, t.issue), (S::Backlog, None));
        }
        assert!(h.d.deliveries.is_empty());
    }

    #[test]
    fn restarting_cancels_pending_delivery() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        let t = h.queued("a1", "A");
        h.send(DispatchMsg::QueueChanged {
            agent_id: "a1".into(),
        });
        assert!(matches!(h.d.state("a1"), Delivery::Delaying { .. }));
        h.send(DispatchMsg::AgentRestarting {
            agent_id: "a1".into(),
        });
        assert!(h.d.deliveries.is_empty());
        // The old timer does nothing; the queue continues at the next Idle (after SessionStart).
        h.advance(DISPATCH_DELAY_MS);
        assert!(h.writes().is_empty());
        assert_eq!(h.ticket(&t.id).state, S::Assigned);
        h.idle("a1");
        h.advance(DISPATCH_DELAY_MS);
        assert_eq!(h.writes(), vec![("a1".into(), line(&t))]);
    }

    // (10)
    #[test]
    fn stale_timer_is_ignored() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        let t = h.queued("a1", "A");
        h.send(DispatchMsg::QueueChanged {
            agent_id: "a1".into(),
        });
        let old = match h.d.state("a1") {
            Delivery::Delaying { token } => *token,
            other => panic!("{other:?}"),
        };
        h.send(DispatchMsg::AgentGone {
            agent_id: "a1".into(),
        });
        // New sequence for the same agent.
        h.idle("a1");
        // The old timer arrives first: ignored.
        h.send(DispatchMsg::Timer {
            agent_id: "a1".into(),
            token: old,
            kind: TimerKind::DispatchDelay,
        });
        assert!(h.writes().is_empty());
        h.advance(DISPATCH_DELAY_MS);
        // Exactly one line: the old timer firing in the fake clock did nothing either.
        assert_eq!(h.writes(), vec![("a1".into(), line(&t))]);
        // Unexpected kinds for the current state are ignored too.
        let cur = match h.d.state("a1") {
            Delivery::Typed { token, .. } => *token,
            other => panic!("{other:?}"),
        };
        h.send(DispatchMsg::Timer {
            agent_id: "a1".into(),
            token: cur,
            kind: TimerKind::Confirm,
        });
        assert_eq!(h.writes().len(), 1);
    }

    // (11)
    #[test]
    fn write_failures_send_the_ticket_to_the_backlog() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        let t = h.queued("a1", "A");
        lock(&h.port.0).fail_writes = true;
        h.idle("a1");
        h.advance(DISPATCH_DELAY_MS);
        let now = h.ticket(&t.id);
        assert_eq!(
            (now.state, now.assignee_agent_id.clone()),
            (S::Backlog, None)
        );
        assert_eq!(
            now.history.last().unwrap().note.as_deref(),
            Some(TERMINAL_GONE_NOTE)
        );

        // The ticket file cannot be written (cwd is a file).
        lock(&h.port.0).fail_writes = false;
        h.agent("a2", AgentStatus::Idle);
        let bad = h.root.join("not-a-dir");
        fs::write(&bad, "x").unwrap();
        lock(&h.port.0).snapshots.get_mut("a2").unwrap().cwd = bad;
        let t2 = h.queued("a2", "B");
        h.idle("a2");
        h.advance(DISPATCH_DELAY_MS);
        let now = h.ticket(&t2.id);
        assert_eq!(now.state, S::Backlog);
        assert!(now
            .history
            .last()
            .unwrap()
            .note
            .as_deref()
            .unwrap()
            .starts_with("Kunne ikke skrive ticket-fil: "));
        assert!(h.writes().is_empty());
    }

    // (12)
    #[test]
    fn spawn_with_ticket_waits_for_the_session_then_falls_back() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Starting);
        let t = h.queued("a1", "A");
        h.send(DispatchMsg::SpawnedWithTicket {
            agent_id: "a1".into(),
            ticket_id: t.id.clone(),
        });
        h.set_status("a1", AgentStatus::Idle);
        h.idle("a1"); // SessionStart
        h.advance(SPAWN_CONFIRM_TIMEOUT_MS - 1);
        assert!(h.writes().is_empty());
        h.send(DispatchMsg::AgentBusy {
            agent_id: "a1".into(),
        });
        assert_eq!(h.ticket(&t.id).state, S::InProgress);
        h.advance(20_000);
        assert!(h.writes().is_empty());

        // No confirmation within the grace period: ordinary PTY dispatch.
        h.agent("a2", AgentStatus::Starting);
        let t2 = h.queued("a2", "B");
        h.send(DispatchMsg::SpawnedWithTicket {
            agent_id: "a2".into(),
            ticket_id: t2.id.clone(),
        });
        h.set_status("a2", AgentStatus::Idle);
        h.idle("a2");
        h.idle("a2"); // a second idle does not restart the grace period
        h.advance(SPAWN_CONFIRM_TIMEOUT_MS);
        assert!(h.writes().is_empty());
        h.advance(DISPATCH_DELAY_MS);
        assert_eq!(h.writes(), vec![("a2".into(), line(&t2))]);
    }

    // (13)
    #[test]
    fn messages_for_every_fixture_and_status() {
        let ev = |json: &str| {
            let e = parse(&serde_json::from_str(json).unwrap()).unwrap();
            StatusEvent {
                agent_id: "a".into(),
                status: apply(&e).status,
                hook_event_name: e.hook_event_name,
                prompt: e.prompt,
            }
        };
        let idle = || DispatchMsg::AgentIdle {
            agent_id: "a".into(),
        };
        let busy = || DispatchMsg::AgentBusy {
            agent_id: "a".into(),
        };
        let ended = |failed| DispatchMsg::TurnEnded {
            agent_id: "a".into(),
            failed,
        };
        let table: Vec<(&str, Vec<DispatchMsg>)> = vec![
            (fx::SESSION_START, vec![idle()]),
            (
                fx::USER_PROMPT_SUBMIT,
                vec![DispatchMsg::PromptSubmitted {
                    agent_id: "a".into(),
                    prompt: Some("fix the bug".into()),
                }],
            ),
            (fx::PRE_TOOL_USE, vec![busy()]),
            (fx::PERMISSION_REQUEST, vec![busy()]),
            (fx::PERMISSION_DENIED, vec![busy()]),
            (fx::POST_TOOL_USE, vec![busy()]),
            (fx::POST_TOOL_USE_FAILURE, vec![busy()]),
            (fx::NOTIFICATION_PERMISSION, vec![busy()]),
            (fx::NOTIFICATION_IDLE, vec![idle()]),
            (fx::NOTIFICATION_OTHER, vec![]),
            (fx::STOP, vec![ended(false), idle()]),
            (fx::STOP_FAILURE, vec![ended(true), idle()]),
            (fx::SESSION_END, vec![]),
            (fx::SUBAGENT_STOP, vec![]),
        ];
        assert_eq!(table.len(), 14);
        for (json, want) in table {
            let e = ev(json);
            assert_eq!(messages_for(&e), want, "{}", e.hook_event_name);
        }
        let status = |s: Option<AgentStatus>| StatusEvent {
            agent_id: "a".into(),
            hook_event_name: "X".into(),
            prompt: None,
            status: s,
        };
        for s in [
            AgentStatus::Thinking,
            AgentStatus::Reading,
            AgentStatus::Editing,
            AgentStatus::Running,
            AgentStatus::WaitingPermission,
        ] {
            assert_eq!(messages_for(&status(Some(s))), vec![busy()]);
        }
        for s in [
            None,
            Some(AgentStatus::Starting),
            Some(AgentStatus::Exited { code: Some(0) }),
        ] {
            assert!(messages_for(&status(s)).is_empty());
        }
        assert_eq!(messages_for(&status(Some(AgentStatus::Idle))), vec![idle()]);
    }

    // (14)
    #[test]
    fn two_agents_do_not_affect_each_other() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        h.agent("a2", AgentStatus::Idle);
        let t1 = h.queued("a1", "A");
        let t2 = h.queued("a2", "B");
        h.idle("a1");
        h.advance(400);
        h.idle("a2");
        h.advance(DISPATCH_DELAY_MS); // a1: line at 750, Enter at 900; a2: line at 1150
        h.submitted("a1", &line(&t1));
        assert_eq!(
            h.writes(),
            vec![
                ("a1".into(), line(&t1)),
                enter("a1"),
                ("a2".into(), line(&t2))
            ]
        );
        h.advance(ENTER_DELAY_MS);
        h.advance(CONFIRM_TIMEOUT_MS + RETRY_TIMEOUT_MS);
        assert_eq!(h.ticket(&t1.id).state, S::InProgress);
        assert_eq!(h.ticket(&t1.id).issue, None);
        assert_eq!(h.ticket(&t2.id).issue, Some(TicketIssue::DeliveryFailed));
        assert_eq!(h.detail("a1"), None);
        assert_eq!(h.detail("a2").as_deref(), Some(DELIVERY_FAILED_TEXT));
    }

    #[test]
    fn queue_change_triggers_dispatch_but_not_mid_sequence() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        // Idle with an empty queue: nothing to do.
        h.idle("a1");
        assert_eq!(h.timers.pending(), 0);
        let a = h.queued("a1", "A");
        h.send(DispatchMsg::QueueChanged {
            agent_id: "a1".into(),
        });
        h.advance(DISPATCH_DELAY_MS);
        h.queued("a1", "B");
        h.send(DispatchMsg::QueueChanged {
            agent_id: "a1".into(),
        });
        h.idle("a1");
        h.advance(ENTER_DELAY_MS);
        h.send(DispatchMsg::QueueChanged {
            agent_id: "a1".into(),
        });
        h.advance(CONFIRM_TIMEOUT_MS - 1);
        assert_eq!(h.writes(), vec![("a1".into(), line(&a)), enter("a1")]);
    }

    // ---- step 4: Stop without submit, the in-progress guard, "Bed om aflevering" ----

    fn stop(h: &mut Harness, agent: &str) {
        h.send(DispatchMsg::TurnEnded {
            agent_id: agent.into(),
            failed: false,
        });
        h.idle(agent);
    }

    /// Queues A and B for a1 and delivers A (confirmed, in progress).
    fn a_in_progress(h: &mut Harness) -> (Ticket, Ticket) {
        h.agent("a1", AgentStatus::Idle);
        let a = h.queued("a1", "A");
        let b = h.queued("a1", "B");
        deliver_until_enter(h, "a1");
        h.submitted("a1", &line(&a));
        assert_eq!(h.ticket(&a.id).state, S::InProgress);
        (a, b)
    }

    fn request(h: &mut Harness, t: &Ticket) {
        h.send(DispatchMsg::RequestSubmission {
            ticket_id: t.id.clone(),
        });
    }

    fn nudge(t: &Ticket) -> (String, String) {
        ("a1".into(), prompt::request_submission_line(&t.short_id()))
    }

    #[test]
    fn auto_review_follows_the_constant() {
        let h = Harness::new();
        assert_eq!(h.d.auto_review(), AUTO_REVIEW_ON_STOP);
        assert!(!h.d.auto_review());
        let d = Dispatcher::new(h.d.host.clone(), h.port.clone(), h.timers.clone());
        assert!(d.with_auto_review(true).auto_review());
        // Without an override the host's (workspace) rules decide (plan4b A.4).
        let mut host = h.d.host.clone();
        host.rules.auto_review_on_stop = true;
        let d = Dispatcher::new(host.clone(), h.port.clone(), h.timers.clone());
        assert!(d.auto_review());
        assert!(!d.with_auto_review(false).auto_review());
    }

    #[test]
    fn user_input_grace_follows_the_rules() {
        let h = Harness::new();
        let now = h.timers.now();
        let snap = AgentSnapshot {
            name: "a".into(),
            cwd: PathBuf::from("/w"),
            status: AgentStatus::Idle,
            detail: None,
            last_user_input_at: Some(now),
            seat_kind: SeatKind::Work,
            roles: vec![],
            project: None,
            has_conversation: false,
        };
        assert_eq!(h.d.user_grace_left(&snap), Some(USER_INPUT_GRACE_MS));
        let mut host = h.d.host.clone();
        host.rules.user_input_grace_ms = 1000;
        let d = Dispatcher::new(host.clone(), h.port.clone(), h.timers.clone());
        assert_eq!(d.user_grace_left(&snap), Some(1000));
        host.rules.user_input_grace_ms = 0;
        let d = Dispatcher::new(host, h.port.clone(), h.timers.clone());
        assert_eq!(d.user_grace_left(&snap), None);
    }

    #[test]
    fn stop_without_submit_keeps_the_ticket_in_progress_and_holds_the_queue() {
        let mut h = Harness::new();
        let (a, b) = a_in_progress(&mut h);
        stop(&mut h, "a1");
        h.advance(10_000);
        let now = h.ticket(&a.id);
        assert_eq!(
            (now.state, now.issue),
            (S::InProgress, Some(TicketIssue::NotSubmitted))
        );
        let last = now.history.last().unwrap();
        assert_eq!(
            last.note.as_deref(),
            Some(crate::tickets::NOT_SUBMITTED_NOTE)
        );
        assert_eq!(last.by, TicketActor::System);
        assert_eq!(h.detail("a1").as_deref(), Some(NOT_SUBMITTED_TEXT));
        // B is not typed although the queue has it.
        assert_eq!(h.writes().len(), 2);
        assert_eq!(h.ticket(&b.id).state, S::Assigned);
        assert_eq!(h.timers.pending(), 0);
    }

    #[test]
    fn stop_with_auto_review_moves_on_to_the_next_ticket() {
        let mut h = Harness::new();
        h.d.auto_review_on_stop = Some(true);
        let (a, b) = a_in_progress(&mut h);
        stop(&mut h, "a1");
        assert_eq!(h.ticket(&a.id).state, S::Review);
        assert_eq!(h.ticket(&a.id).issue, None);
        h.advance(DISPATCH_DELAY_MS);
        assert_eq!(h.writes()[2], ("a1".into(), line(&h.ticket(&b.id))));
    }

    #[test]
    fn stop_without_a_ticket_in_progress_considers_the_queue() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        let a = h.queued("a1", "A");
        // E.g. the user's own prompt ended; nothing in progress.
        h.send(DispatchMsg::TurnEnded {
            agent_id: "a1".into(),
            failed: false,
        });
        assert_eq!(h.ticket(&a.id).issue, None);
        h.advance(DISPATCH_DELAY_MS);
        assert_eq!(h.writes(), vec![("a1".into(), line(&a))]);
        assert_eq!(h.detail("a1"), None);
    }

    #[test]
    fn submit_by_the_tool_before_stop_lets_the_next_ticket_go() {
        let mut h = Harness::new();
        let (a, b) = a_in_progress(&mut h);
        h.svc()
            .submit_by_agent("a1", None, "Rettet login", 50)
            .unwrap();
        stop(&mut h, "a1");
        let a_now = h.ticket(&a.id);
        assert_eq!((a_now.state, a_now.issue), (S::Review, None));
        assert_eq!(a_now.summary.as_deref(), Some("Rettet login"));
        assert_eq!(h.detail("a1"), None);
        h.advance(DISPATCH_DELAY_MS + ENTER_DELAY_MS);
        let b_now = h.ticket(&b.id);
        assert_eq!(
            &h.writes()[2..],
            &[("a1".into(), line(&b_now)), enter("a1")]
        );
    }

    #[test]
    fn submit_after_a_not_submitted_stop_clears_it_and_the_queue_moves() {
        let mut h = Harness::new();
        let (a, b) = a_in_progress(&mut h);
        stop(&mut h, "a1");
        assert_eq!(h.ticket(&a.id).issue, Some(TicketIssue::NotSubmitted));
        h.svc().submit_by_agent("a1", None, "Færdig", 60).unwrap();
        assert_eq!(h.ticket(&a.id).issue, None);
        // tools.rs notifies the dispatcher; the agent is idle.
        h.send(DispatchMsg::QueueChanged {
            agent_id: "a1".into(),
        });
        h.advance(DISPATCH_DELAY_MS);
        assert_eq!(h.writes()[2], ("a1".into(), line(&h.ticket(&b.id))));
    }

    /// Step 5c: a coordinator hands its coordination task to a work agent mid-turn. The ticket
    /// leaves the coordinator, the work agent gets it as an ordinary ticket, the coordinator's
    /// Stop marks nothing "ikke afleveret" and its own queue moves on.
    #[test]
    fn handoff_mid_turn_frees_the_sender_and_delivers_to_the_target() {
        let mut h = Harness::new();
        h.agent_on(
            "k",
            AgentStatus::Idle,
            SeatKind::Staff,
            &[Role::Coordinator],
        );
        h.agent("w", AgentStatus::Thinking);
        let a = h.queued("k", "Lav en HTML-side");
        let b = h.queued("k", "Næste");
        deliver_until_enter(&mut h, "k");
        let coord = TicketDelivery::for_agent(SeatKind::Staff, &[Role::Coordinator]);
        let coord_line = prompt::line_for(&a, &coord);
        assert!(coord_line.starts_with("Koordiner ticket "));
        h.submitted("k", &coord_line);
        assert_eq!(h.ticket(&a.id).state, S::InProgress);
        h.set_status("k", AgentStatus::Thinking);

        // mira_assign_ticket / mira_handoff_ticket during the coordinator's turn.
        h.svc()
            .handoff(&a.id, "w", Some("k"), ("bot-k", "bot-w"), 50)
            .unwrap();
        h.send(DispatchMsg::QueueChanged {
            agent_id: "k".into(),
        });
        h.send(DispatchMsg::QueueChanged {
            agent_id: "w".into(),
        });
        h.advance(10_000);
        assert_eq!(h.writes().len(), 2, "nobody idle yet");

        // The coordinator's turn ends: nothing to mark, its queue head is next.
        h.set_status("k", AgentStatus::Idle);
        stop(&mut h, "k");
        let a_now = h.ticket(&a.id);
        assert_eq!(
            (a_now.state, a_now.issue, a_now.assignee_agent_id.as_deref()),
            (S::Assigned, None, Some("w"))
        );
        assert_eq!(h.detail("k"), None);
        h.advance(DISPATCH_DELAY_MS + ENTER_DELAY_MS);
        let b_now = h.ticket(&b.id);
        assert_eq!(
            &h.writes()[2..],
            &[("k".into(), prompt::line_for(&b_now, &coord)), enter("k")]
        );
        // Review 5c W4: it handed the ticket on itself, so no "Du skal stoppe …" line.
        assert!(h.writes().iter().all(|(_, w)| !w.starts_with("Du ")));

        // The work agent becomes idle and gets the handed-over ticket as an ordinary ticket.
        h.set_status("w", AgentStatus::Idle);
        deliver_until_enter(&mut h, "w");
        assert_eq!(&h.writes()[4..], &[("w".into(), line(&a_now)), enter("w")]);
        assert!(!h.ticket_file("w", &a_now).contains("Koordineringsopgave"));
        h.submitted("w", &line(&a_now));
        let a_done = h.ticket(&a.id);
        assert_eq!(
            (a_done.state, a_done.assignee_agent_id.as_deref()),
            (S::InProgress, Some("w"))
        );
        assert_eq!(
            a_done.history.last().unwrap().note.as_deref(),
            Some("sendt til bot-w")
        );
    }

    /// A ticket that changed hands while its line was typed is not marked dispatched to the new
    /// assignee by the old agent's confirmation.
    #[test]
    fn confirmation_after_the_ticket_changed_hands_is_ignored() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        h.agent("a2", AgentStatus::Thinking);
        let a = h.queued("a1", "A");
        deliver_until_enter(&mut h, "a1");
        {
            let mut s = h.svc();
            s.unassign(&a.id, 10).unwrap();
            s.assign(&a.id, "a2", 11).unwrap();
        }
        h.submitted("a1", &line(&a));
        let now = h.ticket(&a.id);
        assert_eq!(
            (now.state, now.assignee_agent_id.as_deref()),
            (S::Assigned, Some("a2"))
        );
    }

    #[test]
    fn no_ticket_is_typed_while_one_is_in_progress() {
        let mut h = Harness::new();
        let (_, b) = a_in_progress(&mut h);
        // Esc mid-turn: Idle without Stop, then a queue change.
        h.set_status("a1", AgentStatus::Idle);
        h.idle("a1");
        h.send(DispatchMsg::QueueChanged {
            agent_id: "a1".into(),
        });
        h.advance(10_000);
        assert_eq!(h.writes().len(), 2);
        assert_eq!(h.ticket(&b.id).state, S::Assigned);
    }

    #[test]
    fn confirm_clears_the_not_submitted_detail() {
        let mut h = Harness::new();
        let (a, b) = a_in_progress(&mut h);
        stop(&mut h, "a1");
        assert_eq!(h.detail("a1").as_deref(), Some(NOT_SUBMITTED_TEXT));
        // "Send til review" by the user.
        h.svc().set_state(&a.id, S::Review, None, true, 70).unwrap();
        h.send(DispatchMsg::QueueChanged {
            agent_id: "a1".into(),
        });
        h.advance(DISPATCH_DELAY_MS + ENTER_DELAY_MS);
        let b_now = h.ticket(&b.id);
        h.submitted("a1", &line(&b_now));
        assert_eq!(h.ticket(&b.id).state, S::InProgress);
        assert_eq!(h.detail("a1"), None);
    }

    #[test]
    fn request_submission_types_the_line_then_a_separate_enter() {
        let mut h = Harness::new();
        let (a, _) = a_in_progress(&mut h);
        stop(&mut h, "a1");
        let before = h.ticket(&a.id);
        request(&mut h, &a);
        h.advance(0);
        assert_eq!(h.writes()[2], nudge(&a));
        assert!(!h.writes()[2].1.starts_with("Ticket"));
        assert_eq!(h.writes().len(), 3);
        h.advance(ENTER_DELAY_MS - 1);
        assert_eq!(h.writes().len(), 3);
        h.advance(1);
        assert_eq!(h.writes()[3], enter("a1"));
        assert_eq!(*h.d.state("a1"), Delivery::Free);
        // State and issue unchanged; only a history note.
        let after = h.ticket(&a.id);
        assert_eq!((after.state, after.issue), (before.state, before.issue));
        assert_eq!(after.history.len(), before.history.len() + 1);
        assert_eq!(
            after.history.last().unwrap().note.as_deref(),
            Some(SUBMISSION_REQUESTED_NOTE)
        );
        // Nothing else follows (only the stale confirm timer of the delivery is left).
        h.advance(10_000);
        assert_eq!(h.writes().len(), 4);
    }

    #[test]
    fn request_submission_waits_for_the_user_input_grace() {
        let mut h = Harness::new();
        let (a, _) = a_in_progress(&mut h);
        stop(&mut h, "a1");
        h.user_typed("a1");
        request(&mut h, &a);
        h.advance(USER_INPUT_GRACE_MS - 1);
        assert_eq!(h.writes().len(), 2);
        // Typing again during the wait postpones it once more.
        h.user_typed("a1");
        h.advance(1);
        assert_eq!(h.writes().len(), 2);
        h.advance(USER_INPUT_GRACE_MS);
        assert_eq!(h.writes()[2], nudge(&a));
    }

    #[test]
    fn request_submission_does_nothing_for_a_busy_agent_or_a_queued_ticket() {
        let mut h = Harness::new();
        let (a, b) = a_in_progress(&mut h);
        h.set_status("a1", AgentStatus::Thinking);
        request(&mut h, &a);
        h.advance(10_000);
        // A queued (not in progress) ticket is never nudged either.
        h.set_status("a1", AgentStatus::Idle);
        request(&mut h, &b);
        h.advance(10_000);
        assert_eq!(h.writes().len(), 2);
        assert_eq!(h.timers.pending(), 0);
        // Not while a delivery sequence runs for the agent.
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        let c = h.queued("a1", "C");
        h.idle("a1");
        h.svc().mark_dispatched(&c.id, "bot-a1", 5).unwrap();
        request(&mut h, &c);
        assert!(matches!(h.d.state("a1"), Delivery::Delaying { .. }));
    }

    #[test]
    fn foreign_prompt_or_busy_status_during_a_nudge_changes_nothing() {
        let mut h = Harness::new();
        let (a, b) = a_in_progress(&mut h);
        stop(&mut h, "a1");
        request(&mut h, &a);
        h.advance(0);
        h.submitted("a1", "noget helt andet");
        h.send(DispatchMsg::AgentBusy {
            agent_id: "a1".into(),
        });
        h.advance(ENTER_DELAY_MS);
        assert_eq!(h.writes()[3], enter("a1"));
        let now = h.ticket(&a.id);
        assert_eq!(
            (now.state, now.issue),
            (S::InProgress, Some(TicketIssue::NotSubmitted))
        );
        assert_eq!(h.ticket(&b.id).state, S::Assigned);
    }

    #[test]
    fn nudge_write_failure_frees_the_agent_and_leaves_the_ticket() {
        let mut h = Harness::new();
        let (a, _) = a_in_progress(&mut h);
        stop(&mut h, "a1");
        let before = h.ticket(&a.id);
        lock(&h.port.0).fail_writes = true;
        request(&mut h, &a);
        h.advance(ENTER_DELAY_MS * 2);
        assert_eq!(*h.d.state("a1"), Delivery::Free);
        assert_eq!(h.ticket(&a.id), before);
        h.advance(10_000);
        assert_eq!(h.writes().len(), 2);
    }

    #[test]
    fn nudge_is_dropped_when_the_ticket_moved_meanwhile() {
        let mut h = Harness::new();
        let (a, _) = a_in_progress(&mut h);
        stop(&mut h, "a1");
        h.user_typed("a1");
        request(&mut h, &a);
        h.svc().set_state(&a.id, S::Review, None, true, 80).unwrap();
        h.advance(USER_INPUT_GRACE_MS);
        assert_eq!(h.writes().len(), 2);
        assert_eq!(*h.d.state("a1"), Delivery::Free);
    }

    #[test]
    fn fake_timers_deliver_in_time_then_schedule_order() {
        let mut t = FakeTimers::new();
        let m = |id: &str| DispatchMsg::QueueChanged {
            agent_id: id.into(),
        };
        t.schedule(20, m("b"));
        t.schedule(10, m("a"));
        t.schedule(20, m("c"));
        assert_eq!(t.advance(5), vec![]);
        assert_eq!(t.advance(15), vec![m("a"), m("b"), m("c")]);
        assert_eq!(t.now(), 20);
        assert_eq!(t.pending(), 0);
    }

    #[tokio::test]
    async fn real_timers_and_run_loop() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut timers = RealTimers::new(tx.clone());
        timers.schedule(
            1,
            DispatchMsg::QueueChanged {
                agent_id: "x".into(),
            },
        );
        let got = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap();
        assert_eq!(
            got,
            Some(DispatchMsg::QueueChanged {
                agent_id: "x".into()
            })
        );

        let h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        let t = h.queued("a1", "A");
        let (tx2, rx2) = tokio::sync::mpsc::unbounded_channel();
        let host = TestHost {
            svc: h.svc.clone(),
            mutations: Arc::default(),
            rules: WorkspaceRules::defaults(),
            git: Arc::default(),
            git_calls: Arc::default(),
        };
        let d = Dispatcher::new(host, h.port.clone(), h.timers.clone());
        tx2.send(DispatchMsg::QueueChanged {
            agent_id: "a1".into(),
        })
        .unwrap();
        drop(tx2);
        run(rx2, d).await;
        assert_eq!(h.timers.pending(), 1);
        assert_eq!(h.ticket(&t.id).state, S::Assigned);
    }

    // ---- step 6b: git in the delivery ----

    fn worktree_git(short: &str) -> TicketGit {
        TicketGit {
            mode: crate::tickets::model::GitMode::Worktree,
            branch: format!("ticket/{short}"),
            base: "main".into(),
            repo: "/p/proj".into(),
            worktree: Some(format!("/p/proj/.mira-bots/wt/{short}")),
        }
    }

    #[test]
    fn work_delivery_asks_host_for_git_and_writes_git_section() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        let t = h.queued("a1", "Ret login");
        let g = worktree_git(&t.short_id());
        *lock(&h.git) = Some(g.clone());
        deliver_until_enter(&mut h, "a1");
        assert_eq!(h.git_calls.load(Ordering::SeqCst), 1);
        // The line is unchanged; the file has `## Git` before `## Regler`; the ticket keeps it.
        assert_eq!(h.writes(), vec![("a1".into(), line(&t)), enter("a1")]);
        let f = h.ticket_file("a1", &t);
        let section = format!(
            "## Git\n- branch: {} (fra main)\n- mappe: {}\n- Commit dine ændringer",
            g.branch,
            g.worktree.as_deref().unwrap()
        );
        assert!(f.contains(&section), "{f}");
        assert!(f.find("## Git").unwrap() < f.find("## Regler").unwrap());
        assert_eq!(h.ticket(&t.id).git, Some(g));
    }

    #[test]
    fn delivery_without_git_has_no_git_section() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        let t = h.queued("a1", "Ret login");
        deliver_until_enter(&mut h, "a1");
        assert_eq!(h.git_calls.load(Ordering::SeqCst), 1);
        assert!(!h.ticket_file("a1", &t).contains("## Git"));
        assert_eq!(h.ticket(&t.id).git, None);
    }

    #[test]
    fn rejected_ticket_keeps_its_git_info() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        let t = h.queued("a1", "Ret login");
        let first = worktree_git(&t.short_id());
        h.svc().set_git(&t.id, Some(first.clone()), 3).unwrap();
        // The host would prepare something else for a ticket without git.
        *lock(&h.git) = Some(TicketGit {
            branch: "ticket/other".into(),
            ..worktree_git("ffffffff")
        });
        deliver_until_enter(&mut h, "a1");
        let f = h.ticket_file("a1", &t);
        assert!(
            f.contains(&format!("- branch: {} (fra main)", first.branch)),
            "{f}"
        );
        assert!(!f.contains("ticket/other"));
        assert_eq!(h.ticket(&t.id).git, Some(first));
    }

    #[test]
    fn coordination_task_never_asks_for_git() {
        let mut h = Harness::new();
        h.agent_on("p1", AgentStatus::Idle, SeatKind::Staff, &[Role::Planner]);
        let t = h.queued("p1", "Plan noget");
        *lock(&h.git) = Some(worktree_git(&t.short_id()));
        deliver_until_enter(&mut h, "p1");
        assert_eq!(h.git_calls.load(Ordering::SeqCst), 0);
        assert!(!h.ticket_file("p1", &t).contains("## Git"));
        assert_eq!(h.ticket(&t.id).git, None);
    }

    // ---- step 6b: fresh session per ticket (plan A.7, punkt 19) ----

    impl Harness {
        /// Whether `id`'s current session has had a turn.
        fn conversation(&self, id: &str, had: bool) {
            lock(&self.port.0)
                .snapshots
                .get_mut(id)
                .unwrap()
                .has_conversation = had;
        }

        fn restarts(&self) -> Vec<RestartForTicket> {
            lock(&self.port.0).restarts.clone()
        }

        /// A worktree folder for `t` under the harness root (exists) and its git info.
        fn worktree_for(&self, t: &Ticket) -> (PathBuf, TicketGit) {
            let wt = self.root.join("proj/.mira-bots/wt").join(t.short_id());
            fs::create_dir_all(&wt).unwrap();
            let git = TicketGit {
                worktree: Some(wt.to_string_lossy().into_owned()),
                repo: self.root.join("proj").to_string_lossy().into_owned(),
                ..worktree_git(&t.short_id())
            };
            (wt, git)
        }

        /// The new session of a restarted agent comes up (SessionStart → Idle).
        fn session_started(&mut self, agent: &str) {
            self.set_status(agent, AgentStatus::Idle);
            self.idle(agent);
        }
    }

    fn snap_for(seat_kind: SeatKind, roles: &[Role], cwd: &Path, conv: bool) -> AgentSnapshot {
        AgentSnapshot {
            name: "a".into(),
            cwd: cwd.to_path_buf(),
            status: AgentStatus::Idle,
            detail: None,
            last_user_input_at: None,
            seat_kind,
            roles: roles.to_vec(),
            project: Some("proj".into()),
            has_conversation: conv,
        }
    }

    /// One row of [`fresh_decision_table`]: name, rules, agent, ticket, git, expected.
    type Row<'a> = (
        &'a str,
        &'a WorkspaceRules,
        AgentSnapshot,
        &'a Ticket,
        Option<&'a TicketGit>,
        FreshDecision,
    );

    #[test]
    fn fresh_decision_table() {
        let h = Harness::new();
        let t = h.queued("a1", "A");
        let (wt, git) = h.worktree_for(&t);
        let cwd = h.root.join("proj");
        fs::create_dir_all(&cwd).unwrap();
        let on = WorkspaceRules::defaults();
        assert!(on.fresh_session_per_ticket);
        let off = WorkspaceRules {
            fresh_session_per_ticket: false,
            ..on
        };
        let coder = &[Role::Coder][..];
        let work = |conv| snap_for(SeatKind::Work, coder, &cwd, conv);
        let in_wt = |conv| snap_for(SeatKind::Work, coder, &wt, conv);
        let other_wt = cwd.join(".mira-bots/wt/ffffffff");
        fs::create_dir_all(&other_wt).unwrap();
        let in_other = |conv| snap_for(SeatKind::Work, coder, &other_wt, conv);
        let rejected = Ticket {
            review_round: 1,
            ..t.clone()
        };
        let noted = Ticket {
            rejection_note: Some("Mangler test".into()),
            ..t.clone()
        };
        let restart = |cwd: Option<&Path>, force_fresh| FreshDecision::Restart {
            cwd: cwd.map(Path::to_path_buf),
            force_fresh,
        };
        let missing = TicketGit {
            worktree: Some(h.root.join("gone").to_string_lossy().into_owned()),
            ..git.clone()
        };
        let trailing = TicketGit {
            worktree: Some(format!("{}/", wt.to_string_lossy())),
            ..git.clone()
        };
        #[rustfmt::skip]
        let rows: Vec<Row> = vec![
            ("normal: new ticket after a turn", &on, work(true), &t, None, restart(None, true)),
            ("rule off", &off, work(true), &t, None, FreshDecision::None),
            ("staff seat", &on, snap_for(SeatKind::Staff, &[Role::Planner], &cwd, true), &t, Some(&git), FreshDecision::None),
            ("staff seat with a work role", &on, snap_for(SeatKind::Staff, &[Role::Researcher], &cwd, true), &t, Some(&git), FreshDecision::None),
            ("work seat without a work role", &on, snap_for(SeatKind::Work, &[Role::Reviewer], &cwd, true), &t, Some(&git), FreshDecision::None),
            ("first ticket (no turn yet)", &on, work(false), &t, None, FreshDecision::None),
            ("first ticket, spawned in its worktree", &on, in_wt(false), &t, Some(&git), FreshDecision::None),
            ("first ticket, worktree elsewhere", &on, work(false), &t, Some(&git), restart(Some(&wt), false)),
            ("rejected back (round)", &on, work(true), &rejected, None, FreshDecision::None),
            ("rejected back (note)", &on, work(true), &noted, None, FreshDecision::None),
            ("rejected back, already in its worktree", &on, in_wt(true), &rejected, Some(&git), FreshDecision::None),
            ("rejected back, worktree elsewhere", &on, work(true), &rejected, Some(&git), restart(Some(&wt), false)),
            ("rule off, worktree elsewhere", &off, work(true), &t, Some(&git), restart(Some(&wt), false)),
            ("normal, worktree elsewhere", &on, work(true), &t, Some(&git), restart(Some(&wt), true)),
            ("normal, already in the worktree", &on, in_wt(true), &t, Some(&git), restart(None, true)),
            ("same folder written differently", &on, in_wt(false), &t, Some(&trailing), FreshDecision::None),
            ("worktree folder missing", &on, work(false), &t, Some(&missing), FreshDecision::None),
            ("worktree folder missing after a turn", &on, work(true), &t, Some(&missing), restart(None, true)),
            ("no git, in another ticket's worktree", &on, in_other(false), &t, None, restart(Some(&cwd), false)),
            ("no git, in another ticket's worktree after a turn", &on, in_other(true), &t, None, restart(Some(&cwd), true)),
            ("rejected back without git, in another worktree", &on, in_other(true), &rejected, None, restart(Some(&cwd), false)),
            ("worktree missing, in another ticket's worktree", &off, in_other(true), &t, Some(&missing), restart(Some(&cwd), false)),
            ("own worktree, from another ticket's worktree", &on, in_other(false), &t, Some(&git), restart(Some(&wt), false)),
        ];
        for (name, rules, snap, ticket, git, want) in rows {
            assert_eq!(fresh_decision(rules, &snap, ticket, git), want, "{name}");
        }
    }

    #[test]
    fn work_ticket_restarts_agent_before_delivery_and_delivers_after_idle() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        h.conversation("a1", true);
        let t = h.queued("a1", "A");
        h.idle("a1");
        h.advance(DISPATCH_DELAY_MS);
        // Restarted (fresh, same cwd), nothing typed yet.
        assert_eq!(
            h.restarts(),
            vec![RestartForTicket {
                agent_id: "a1".into(),
                cwd: None,
                force_fresh: true,
                ticket_short: t.short_id(),
            }]
        );
        assert!(h.writes().is_empty());
        assert!(matches!(
            h.d.state("a1"),
            Delivery::AwaitingRestart { ticket_id, .. } if *ticket_id == t.id
        ));
        // The restart announces itself; the dispatcher keeps waiting for it.
        h.send(DispatchMsg::AgentRestarting {
            agent_id: "a1".into(),
        });
        assert!(matches!(h.d.state("a1"), Delivery::AwaitingRestart { .. }));
        // An Idle of the old session (queued before the restart) types nothing: still Starting.
        h.idle("a1");
        h.advance(DISPATCH_DELAY_MS);
        assert!(h.writes().is_empty());
        // The new session is up: exactly one delivery, no second restart.
        h.session_started("a1");
        h.advance(DISPATCH_DELAY_MS + ENTER_DELAY_MS);
        assert_eq!(h.writes(), vec![("a1".into(), line(&t)), enter("a1")]);
        assert_eq!(h.restarts().len(), 1);
        h.submitted("a1", &line(&t));
        assert_eq!(h.ticket(&t.id).state, S::InProgress);
        // The stale restart timeout does nothing.
        h.advance(RESTART_TIMEOUT_MS);
        assert_eq!(h.writes().len(), 2);
        assert_eq!(h.restarts().len(), 1);
        assert!(h.d.restarted_for.is_empty());
    }

    #[test]
    fn restart_moves_agent_into_the_worktree_and_writes_the_file_there() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        h.conversation("a1", true);
        let t = h.queued("a1", "Ret login");
        let (wt, git) = h.worktree_for(&t);
        *lock(&h.git) = Some(git.clone());
        h.idle("a1");
        h.advance(DISPATCH_DELAY_MS);
        let r = h.restarts();
        assert_eq!(r.len(), 1);
        assert_eq!(
            (r[0].cwd.as_deref(), r[0].force_fresh),
            (Some(wt.as_path()), true)
        );
        // git was prepared once, before the restart, and saved on the ticket.
        assert_eq!(h.git_calls.load(Ordering::SeqCst), 1);
        assert_eq!(h.ticket(&t.id).git, Some(git.clone()));
        h.send(DispatchMsg::AgentRestarting {
            agent_id: "a1".into(),
        });
        h.session_started("a1");
        h.advance(DISPATCH_DELAY_MS);
        assert_eq!(h.writes(), vec![("a1".into(), line(&t))]);
        assert_eq!(h.restarts().len(), 1);
        // The file lands in the worktree (the agent's new cwd), not in the old folder.
        let file = prompt::ticket_dir(&wt).join(format!("{}.md", t.short_id()));
        let f = fs::read_to_string(&file).unwrap();
        assert!(
            f.contains(&format!("- mappe: {}", wt.to_string_lossy())),
            "{f}"
        );
        assert!(!prompt::ticket_dir(&h.root.join("a1"))
            .join(format!("{}.md", t.short_id()))
            .exists());
    }

    #[test]
    fn restart_failure_falls_back_to_typing() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        h.conversation("a1", true);
        lock(&h.port.0).fail_restart = Some("Agenten arbejder".into());
        let t = h.queued("a1", "A");
        h.idle("a1");
        h.advance(DISPATCH_DELAY_MS);
        assert_eq!(h.restarts().len(), 1);
        // Typed into the current session right away, with a note by the app.
        assert_eq!(h.writes(), vec![("a1".into(), line(&t))]);
        let now = h.ticket(&t.id);
        let last = now.history.last().unwrap();
        assert_eq!(
            last.note.as_deref(),
            Some(restart_failed_note("Agenten arbejder").as_str())
        );
        assert_eq!(last.by, TicketActor::System);
        assert!(matches!(h.d.state("a1"), Delivery::Typed { .. }));
    }

    #[test]
    fn restart_failure_with_the_agent_gone_types_nothing() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        h.conversation("a1", true);
        {
            let mut p = lock(&h.port.0);
            p.fail_restart = Some("kunne ikke starte".into());
            p.exit_on_failed_restart = true;
        }
        let t = h.queued("a1", "A");
        h.idle("a1");
        h.advance(DISPATCH_DELAY_MS);
        // The new child could not start: the agent exited (the app releases its tickets).
        assert_eq!(h.restarts().len(), 1);
        assert!(h.writes().is_empty());
        assert_eq!(h.ticket(&t.id).state, S::Assigned);
        assert!(h.d.deliveries.is_empty());
    }

    #[test]
    fn restart_timeout_frees_and_delivers() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        h.conversation("a1", true);
        let t = h.queued("a1", "A");
        h.idle("a1");
        h.advance(DISPATCH_DELAY_MS);
        h.send(DispatchMsg::AgentRestarting {
            agent_id: "a1".into(),
        });
        // No SessionStart within the timeout (e.g. a dialog in the terminal).
        h.advance(RESTART_TIMEOUT_MS);
        assert!(h.writes().is_empty());
        assert!(h.d.deliveries.is_empty());
        assert_eq!(
            h.ticket(&t.id).history.last().unwrap().note.as_deref(),
            Some(RESTART_TIMEOUT_NOTE)
        );
        // Once idle the ticket is typed as it is: no second restart.
        h.session_started("a1");
        h.advance(DISPATCH_DELAY_MS);
        assert_eq!(h.writes(), vec![("a1".into(), line(&t))]);
        assert_eq!(h.restarts().len(), 1);

        // Already idle at the timeout (the Idle got lost): delivered right after it.
        h.agent("a2", AgentStatus::Idle);
        h.conversation("a2", true);
        let t2 = h.queued("a2", "B");
        h.idle("a2");
        h.advance(DISPATCH_DELAY_MS);
        h.set_status("a2", AgentStatus::Idle);
        h.advance(RESTART_TIMEOUT_MS + DISPATCH_DELAY_MS);
        assert_eq!(h.writes().last(), Some(&("a2".into(), line(&t2))));
        assert_eq!(h.restarts().len(), 2);
    }

    #[test]
    fn rejected_ticket_back_to_same_agent_does_not_restart() {
        let mut h = Harness::new();
        h.d.auto_review_on_stop = Some(true);
        h.agent("a1", AgentStatus::Idle);
        let a = h.queued("a1", "A");
        // First ticket after the spawn: no turn yet, no restart.
        deliver_until_enter(&mut h, "a1");
        assert!(h.restarts().is_empty());
        h.submitted("a1", &line(&a));
        h.conversation("a1", true);
        h.set_status("a1", AgentStatus::Thinking);
        h.send(DispatchMsg::TurnEnded {
            agent_id: "a1".into(),
            failed: false,
        });
        assert_eq!(h.ticket(&a.id).state, S::Review);
        h.svc()
            .reject(&a.id, "Mangler test", RejectReturn::Sender, 10)
            .unwrap();
        h.set_status("a1", AgentStatus::Idle);
        h.send(DispatchMsg::QueueChanged {
            agent_id: "a1".into(),
        });
        h.advance(DISPATCH_DELAY_MS);
        // Back to the same agent: the session (and its context) continues.
        assert!(h.restarts().is_empty());
        let a_now = h.ticket(&a.id);
        assert_eq!(h.writes().last(), Some(&("a1".into(), line(&a_now))));
    }

    #[test]
    fn first_ticket_after_spawn_in_its_worktree_does_not_restart() {
        // B3: an agent spawned with the ticket starts in the ticket's worktree; when the
        // positional prompt is not confirmed, the PTY fallback must not restart it.
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Starting);
        let t = h.queued("a1", "A");
        let (wt, git) = h.worktree_for(&t);
        h.svc().set_git(&t.id, Some(git), 3).unwrap();
        lock(&h.port.0).snapshots.get_mut("a1").unwrap().cwd = wt.clone();
        h.send(DispatchMsg::SpawnedWithTicket {
            agent_id: "a1".into(),
            ticket_id: t.id.clone(),
        });
        h.session_started("a1");
        h.advance(SPAWN_CONFIRM_TIMEOUT_MS + DISPATCH_DELAY_MS);
        assert!(h.restarts().is_empty());
        assert_eq!(h.writes(), vec![("a1".into(), line(&t))]);
        assert!(prompt::ticket_dir(&wt)
            .join(format!("{}.md", t.short_id()))
            .exists());
    }

    #[test]
    fn staff_agent_is_never_restarted() {
        let mut h = Harness::new();
        h.agent_on("p1", AgentStatus::Idle, SeatKind::Staff, &[Role::Planner]);
        h.conversation("p1", true);
        let t = h.queued("p1", "Plan noget");
        deliver_until_enter(&mut h, "p1");
        assert!(h.restarts().is_empty());
        assert_eq!(h.writes().len(), 2);
        let _ = t;
    }

    #[test]
    fn agent_gone_while_awaiting_restart_clears_everything() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        h.conversation("a1", true);
        h.queued("a1", "A");
        h.idle("a1");
        h.advance(DISPATCH_DELAY_MS);
        assert!(matches!(h.d.state("a1"), Delivery::AwaitingRestart { .. }));
        h.remove_agent("a1");
        h.send(DispatchMsg::AgentGone {
            agent_id: "a1".into(),
        });
        assert!(h.d.deliveries.is_empty());
        assert!(h.d.restarted_for.is_empty());
        h.advance(RESTART_TIMEOUT_MS);
        assert!(h.writes().is_empty());
    }

    // ---- review deliveries (plan5 punkt 12) ----

    /// A ticket submitted by `sender` and routed to `reviewer` (no message sent yet).
    fn routed_review(h: &Harness, sender: &str, reviewer: &str, title: &str) -> Ticket {
        let mut s = h.svc();
        let t = s.create(title, "Gør det", false, 1).unwrap();
        s.assign(&t.id, sender, 2).unwrap();
        s.mark_dispatched(&t.id, sender, 3).unwrap();
        s.submit_by_agent(sender, None, "Lavet", 4).unwrap();
        s.route_review(&t.id, reviewer, &format!("bot-{reviewer}"), 5)
            .unwrap()
            .unwrap()
    }

    fn review_line(h: &Harness, t: &Ticket, sender: &str) -> String {
        let cwd = h.root.join(sender).to_string_lossy().into_owned();
        prompt::review_line_for(&h.ticket(&t.id), Some(&cwd))
    }

    fn assigned(h: &mut Harness, reviewer: &str) {
        h.send(DispatchMsg::ReviewAssigned {
            reviewer_agent_id: reviewer.into(),
        });
    }

    #[test]
    fn review_assignment_is_typed_when_reviewer_idle() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        h.agent("rev", AgentStatus::Thinking);
        let t = routed_review(&h, "a1", "rev", "Ret login");
        assigned(&mut h, "rev");
        h.advance(DISPATCH_DELAY_MS + ENTER_DELAY_MS);
        assert!(h.writes().is_empty(), "the reviewer is busy");
        h.set_status("rev", AgentStatus::Idle);
        h.idle("rev");
        h.advance(DISPATCH_DELAY_MS);
        let line = review_line(&h, &t, "a1");
        assert!(line.starts_with(&format!("Review af ticket {}", t.short_id())));
        assert_eq!(h.writes(), vec![("rev".into(), line.clone())]);
        h.advance(ENTER_DELAY_MS);
        assert_eq!(h.writes(), vec![("rev".into(), line), enter("rev")]);
        // Nothing reached the sender; the ticket stays in review.
        assert_eq!(h.ticket(&t.id).state, S::Review);
    }

    #[test]
    fn review_file_written_in_reviewer_cwd() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        h.agent("rev", AgentStatus::Idle);
        let t = routed_review(&h, "a1", "rev", "Ret login");
        assigned(&mut h, "rev");
        h.advance(DISPATCH_DELAY_MS);
        let path = prompt::review_dir(&h.root.join("rev")).join(format!("{}.md", t.short_id()));
        let f = fs::read_to_string(path).unwrap();
        let sender_cwd = h.root.join("a1").to_string_lossy().into_owned();
        assert!(
            f.contains(&format!("Afsender: bot-a1 ({sender_cwd})")),
            "{f}"
        );
        assert!(f.contains("## Opsummering fra afsenderen\nLavet\n"));
        assert!(!prompt::review_dir(&h.root.join("a1")).exists());
    }

    #[test]
    fn review_goes_before_queued_work() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        h.agent("rev", AgentStatus::Idle);
        let work = h.queued("rev", "Eget arbejde");
        let t = routed_review(&h, "a1", "rev", "Ret login");
        h.idle("rev");
        h.advance(DISPATCH_DELAY_MS);
        assert_eq!(h.writes(), vec![("rev".into(), review_line(&h, &t, "a1"))]);
        h.advance(ENTER_DELAY_MS);
        h.submitted("rev", &review_line(&h, &t, "a1"));
        assert_eq!(h.ticket(&work.id).state, S::Assigned);
        // The review turn ends: now the work ticket follows.
        h.send(DispatchMsg::TurnEnded {
            agent_id: "rev".into(),
            failed: false,
        });
        h.idle("rev");
        h.advance(DISPATCH_DELAY_MS);
        assert_eq!(
            h.writes().last().unwrap(),
            &("rev".to_string(), line(&work))
        );
    }

    #[test]
    fn review_line_confirms_on_prompt_prefix() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        h.agent("rev", AgentStatus::Idle);
        let t = routed_review(&h, "a1", "rev", "Ret login");
        assigned(&mut h, "rev");
        h.advance(DISPATCH_DELAY_MS + ENTER_DELAY_MS);
        // A work-style "Ticket <short>" prompt is not the review line.
        h.submitted("rev", &format!("Ticket {}: x", t.short_id()));
        let a = h.svc().assignment_for_ticket(&t.id).unwrap();
        assert_eq!(
            (a.delivered_at, a.attempts),
            (None, 1),
            "aborted as a failure"
        );
        assert_eq!(h.detail("rev").as_deref(), Some(DELIVERY_FAILED_TEXT));

        h.idle("rev");
        h.advance(DISPATCH_DELAY_MS + ENTER_DELAY_MS);
        h.submitted(
            "rev",
            &format!("  Review af ticket {}: Ret login. Læs …", t.short_id()),
        );
        let a = h.svc().assignment_for_ticket(&t.id).unwrap();
        assert!(a.delivered_at.is_some());
        let tk = h.ticket(&t.id);
        assert_eq!(tk.state, S::Review);
        assert_eq!(
            tk.history.last().unwrap().note.as_deref(),
            Some("review sendt til bot-rev")
        );
        assert_eq!(h.detail("rev"), None, "the failure hint is cleared");
        // Delivered: no second typing at the next idle.
        let n = h.writes().len();
        h.idle("rev");
        h.advance(DISPATCH_DELAY_MS + ENTER_DELAY_MS);
        assert_eq!(h.writes().len(), n);

        // A busy status confirms as well.
        h.agent("rev2", AgentStatus::Idle);
        let t2 = routed_review(&h, "a1", "rev2", "Andet");
        assigned(&mut h, "rev2");
        h.advance(DISPATCH_DELAY_MS + ENTER_DELAY_MS);
        h.send(DispatchMsg::AgentBusy {
            agent_id: "rev2".into(),
        });
        assert!(h
            .svc()
            .assignment_for_ticket(&t2.id)
            .unwrap()
            .delivered_at
            .is_some());
    }

    #[test]
    fn review_delivery_failure_retries_next_idle_then_gives_up() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        h.agent("rev", AgentStatus::Idle);
        let t = routed_review(&h, "a1", "rev", "Ret login");
        for attempt in 1..=crate::config::REVIEW_DELIVERY_MAX_ATTEMPTS {
            h.idle("rev");
            h.advance(DISPATCH_DELAY_MS + ENTER_DELAY_MS + CONFIRM_TIMEOUT_MS + RETRY_TIMEOUT_MS);
            let a = h.svc().assignment_for_ticket(&t.id).unwrap();
            assert_eq!((a.attempts, a.delivered_at), (attempt, None));
            assert_eq!(h.detail("rev").as_deref(), Some(DELIVERY_FAILED_TEXT));
        }
        let tk = h.ticket(&t.id);
        assert_eq!(
            (tk.state, tk.reviewer_agent_id.as_deref()),
            (S::Review, Some("rev"))
        );
        assert_eq!(
            tk.history.last().unwrap().note.as_deref(),
            Some(crate::tickets::service::REVIEW_UNDELIVERED_NOTE)
        );
        let n = h.writes().len();
        h.idle("rev");
        h.advance(DISPATCH_DELAY_MS + ENTER_DELAY_MS);
        assert_eq!(h.writes().len(), n, "no more automatic tries");
        // "Send igen" starts over.
        h.send(DispatchMsg::Redispatch {
            ticket_id: t.id.clone(),
        });
        h.advance(DISPATCH_DELAY_MS);
        assert_eq!(h.writes().last().unwrap().1, review_line(&h, &t, "a1"));
    }

    #[test]
    fn restarting_cancels_pending_review_delivery() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        h.agent("rev", AgentStatus::Idle);
        let t = routed_review(&h, "a1", "rev", "Ret login");
        assigned(&mut h, "rev");
        h.advance(DISPATCH_DELAY_MS);
        assert_eq!(h.writes().len(), 1);
        h.send(DispatchMsg::AgentRestarting {
            agent_id: "rev".into(),
        });
        h.advance(ENTER_DELAY_MS + CONFIRM_TIMEOUT_MS + RETRY_TIMEOUT_MS);
        assert_eq!(h.writes().len(), 1, "no Enter after the restart");
        let a = h.svc().assignment_for_ticket(&t.id).unwrap();
        assert_eq!((a.attempts, a.delivered_at), (0, None));
        // The queue continues at the next Idle.
        h.idle("rev");
        h.advance(DISPATCH_DELAY_MS);
        assert_eq!(h.writes().len(), 2);
    }

    #[test]
    fn work_ticket_in_progress_blocks_review_delivery() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        h.agent("rev", AgentStatus::Idle);
        let own = h.queued("rev", "Eget");
        h.svc().mark_dispatched(&own.id, "bot-rev", 3).unwrap();
        routed_review(&h, "a1", "rev", "Ret login");
        assigned(&mut h, "rev");
        h.idle("rev");
        h.advance(DISPATCH_DELAY_MS + ENTER_DELAY_MS);
        assert!(h.writes().is_empty());
        h.svc().submit_by_agent("rev", None, "færdig", 9).unwrap();
        h.idle("rev");
        h.advance(DISPATCH_DELAY_MS);
        assert!(h.writes()[0].1.starts_with("Review af ticket "));
    }

    // ---- review 5c W4/N3: the old agent is told when the user takes its ticket ----

    fn handed_over(h: &mut Harness, agent: &str, t: &Ticket, to_name: Option<&str>) {
        h.send(DispatchMsg::HandedOver {
            agent_id: agent.into(),
            ticket_id: t.id.clone(),
            to_name: to_name.map(str::to_string),
        });
        h.send(DispatchMsg::QueueChanged {
            agent_id: agent.into(),
        });
    }

    /// "Tildel…" mid-turn: nothing is typed while the old agent works; at its Stop it gets the
    /// "Du skal stoppe …" line (before its next ticket), then the queue moves on.
    #[test]
    fn user_handoff_mid_turn_tells_the_old_agent_to_stop_before_its_next_ticket() {
        let mut h = Harness::new();
        let (a, b) = a_in_progress(&mut h);
        h.agent("w", AgentStatus::Thinking);
        h.set_status("a1", AgentStatus::Thinking);
        h.svc()
            .handoff(&a.id, "w", None, ("bot-a1", "bot-w"), 50)
            .unwrap();
        // TicketsCtx::handed_over sets the detail; the dispatcher clears it like its own hints.
        h.port
            .set_detail("a1", Some(prompt::handed_over_detail(&a.short_id(), true)));
        handed_over(&mut h, "a1", &a, Some("bot-w"));
        h.advance(10_000);
        assert_eq!(h.writes().len(), 2, "busy: nothing typed");

        h.set_status("a1", AgentStatus::Idle);
        stop(&mut h, "a1");
        assert_eq!(h.ticket(&a.id).issue, None, "not marked ikke afleveret");
        h.advance(DISPATCH_DELAY_MS - 1);
        assert_eq!(h.writes().len(), 2);
        h.advance(1);
        let stop_line = prompt::handed_over_line(&a.short_id(), Some("bot-w"));
        assert_eq!(
            stop_line,
            format!(
                "Du skal stoppe arbejdet på ticket {}: den er givet videre til bot-w. Afslut dit svar.",
                a.short_id()
            )
        );
        assert_eq!(h.writes()[2], ("a1".into(), stop_line.clone()));
        h.advance(ENTER_DELAY_MS);
        assert_eq!(h.writes()[3], enter("a1"));
        assert_eq!(*h.d.state("a1"), Delivery::Free);
        // The stop line is never taken for a delivery, and B is not typed during its turn.
        h.submitted("a1", &stop_line);
        h.set_status("a1", AgentStatus::Thinking);
        h.advance(10_000);
        assert_eq!(h.writes().len(), 4);
        // Its next Stop: B, whose confirmation clears the "givet videre" detail.
        h.set_status("a1", AgentStatus::Idle);
        stop(&mut h, "a1");
        h.advance(DISPATCH_DELAY_MS + ENTER_DELAY_MS);
        let b_now = h.ticket(&b.id);
        assert_eq!(
            &h.writes()[4..],
            &[("a1".into(), line(&b_now)), enter("a1")]
        );
        h.submitted("a1", &line(&b_now));
        assert_eq!(h.ticket(&b.id).state, S::InProgress);
        assert_eq!(h.detail("a1"), None);
    }

    /// "Fjern tildeling" of an idle agent's ticket in progress: the line comes after the delay.
    #[test]
    fn user_puts_back_an_idle_agents_ticket_and_it_is_told() {
        let mut h = Harness::new();
        let (a, _) = a_in_progress(&mut h);
        stop(&mut h, "a1");
        h.svc().unassign(&a.id, 60).unwrap();
        handed_over(&mut h, "a1", &a, None);
        h.advance(DISPATCH_DELAY_MS + ENTER_DELAY_MS);
        assert_eq!(
            &h.writes()[2..],
            &[
                ("a1".into(), prompt::handed_over_line(&a.short_id(), None)),
                enter("a1")
            ]
        );
        assert!(h.writes()[2]
            .1
            .ends_with("lagt tilbage i backlog. Afslut dit svar."));
    }

    /// No stop line without a HandedOver (the agent handed it on itself), and none when the
    /// ticket came back to the agent before the line was typed.
    #[test]
    fn stop_line_only_for_a_foreign_handoff_and_not_after_it_came_back() {
        let mut h = Harness::new();
        let (a, b) = a_in_progress(&mut h);
        h.agent("w", AgentStatus::Thinking);
        h.svc()
            .handoff(&a.id, "w", Some("a1"), ("bot-a1", "bot-w"), 50)
            .unwrap();
        h.send(DispatchMsg::QueueChanged {
            agent_id: "a1".into(),
        });
        stop(&mut h, "a1");
        h.advance(DISPATCH_DELAY_MS + ENTER_DELAY_MS);
        let b_now = h.ticket(&b.id);
        assert_eq!(
            &h.writes()[2..],
            &[("a1".into(), line(&b_now)), enter("a1")]
        );
        assert!(h.writes().iter().all(|(_, w)| !w.starts_with("Du ")));

        // Came back: the user hands it over, then straight back before a1 is idle.
        let mut h = Harness::new();
        let (a, b) = a_in_progress(&mut h);
        h.agent("w", AgentStatus::Thinking);
        h.set_status("a1", AgentStatus::Thinking);
        h.svc()
            .handoff(&a.id, "w", None, ("bot-a1", "bot-w"), 50)
            .unwrap();
        handed_over(&mut h, "a1", &a, Some("bot-w"));
        h.svc().unassign(&a.id, 51).unwrap();
        h.svc().assign(&a.id, "a1", 52).unwrap();
        h.set_status("a1", AgentStatus::Idle);
        stop(&mut h, "a1");
        h.advance(DISPATCH_DELAY_MS + ENTER_DELAY_MS);
        assert!(h.writes().iter().all(|(_, w)| !w.starts_with("Du ")));
        // Its queue simply moves on after the usual delay (B, then A again at the end).
        h.advance(DISPATCH_DELAY_MS);
        assert_eq!(h.writes()[2], ("a1".into(), line(&h.ticket(&b.id))));
    }

    /// Review 5c N3: handed on between the "Bed om aflevering" line and its Enter, the note
    /// does not land on the ticket (now someone else's).
    #[test]
    fn nudge_enter_skips_the_note_after_a_handoff() {
        let mut h = Harness::new();
        let (a, _) = a_in_progress(&mut h);
        h.agent("w", AgentStatus::Thinking);
        stop(&mut h, "a1");
        request(&mut h, &a);
        h.advance(0);
        assert_eq!(h.writes()[2], nudge(&a));
        h.svc()
            .handoff(&a.id, "w", None, ("bot-a1", "bot-w"), 50)
            .unwrap();
        let before = h.ticket(&a.id).history.len();
        h.advance(ENTER_DELAY_MS);
        assert_eq!(h.writes()[3], enter("a1"));
        let after = h.ticket(&a.id);
        assert_eq!(after.history.len(), before);
        assert_ne!(
            after.history.last().unwrap().note.as_deref(),
            Some(SUBMISSION_REQUESTED_NOTE)
        );
    }

    /// Review 5c N1: the coordination line confirms with or without the accent.
    #[test]
    fn coordination_line_confirms_with_either_spelling() {
        for typed in ["Koordiner", "Koordinér"] {
            let mut h = Harness::new();
            h.agent_on(
                "k",
                AgentStatus::Idle,
                SeatKind::Staff,
                &[Role::Coordinator],
            );
            let t = h.queued("k", "Lav en side");
            deliver_until_enter(&mut h, "k");
            h.submitted(
                "k",
                &format!("{typed} ticket {}: Lav en side.", t.short_id()),
            );
            assert_eq!(h.ticket(&t.id).state, S::InProgress, "{typed}");
            assert_eq!(h.ticket(&t.id).issue, None, "{typed}");
        }
    }

    /// Review 5c W1: a profile without a work role on a work seat gets a coordination task.
    #[test]
    fn work_seat_without_work_role_gets_a_coordination_task() {
        let mut h = Harness::new();
        h.agent_on("r", AgentStatus::Idle, SeatKind::Work, &[Role::Reviewer]);
        let t = h.queued("r", "Lav en side");
        h.idle("r");
        h.advance(DISPATCH_DELAY_MS);
        assert!(h.writes()[0].1.starts_with("Koordiner ticket "));
        assert!(h.ticket_file("r", &t).contains(&format!(
            "## Koordineringsopgave\n{}",
            prompt::COORDINATION_PLAN_TEXT
        )));
    }

    // ---- step 4b: shared project ----

    #[test]
    fn two_agents_in_one_project_get_the_shared_section() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Thinking);
        h.agent("a2", AgentStatus::Idle);
        h.agent("a3", AgentStatus::Idle);
        h.agent_on("r", AgentStatus::Idle, SeatKind::Work, &[Role::Reviewer]);
        h.in_project("a1", "p");
        h.in_project("a2", "P");
        h.in_project("a3", "q");
        h.in_project("r", "p");
        let t2 = h.queued("a2", "To");
        let t3 = h.queued("a3", "Tre");
        let tr = h.queued("r", "Fordel");
        for a in ["a2", "a3", "r"] {
            h.idle(a);
        }
        h.advance(DISPATCH_DELAY_MS);
        let f2 = h.ticket_file("a2", &t2);
        assert!(f2.contains("## Delt projekt\n"), "{f2}");
        // The reviewer on a work seat is a live work agent in p too, but gets a coordination
        // task without the section.
        assert!(f2.contains("(projekt «P»): bot-a1, bot-r."), "{f2}");
        assert!(f2.find("## Delt projekt").unwrap() < f2.find("## Regler").unwrap());
        assert!(!h.ticket_file("a3", &t3).contains("Delt projekt"));
        let fr = h.ticket_file("r", &tr);
        assert!(fr.contains("## Koordineringsopgave"));
        assert!(!fr.contains("Delt projekt"));
        // The line itself is the plain work line.
        let writes = h.writes();
        let typed = writes.iter().find(|(x, _)| x == "a2").unwrap().1.clone();
        assert_eq!(typed, line(&t2));
    }

    // ---- step 6a: wake lines, waiting on Stop, blocked tickets ----

    /// A child of `parent`, assigned to and submitted to review by the portless agent "x".
    fn child_in_review(h: &Harness, parent: &Ticket, title: &str) -> Ticket {
        let mut s = h.svc();
        let c = s
            .create_by_agent_related(
                title,
                "",
                false,
                None,
                None,
                Some(parent.id.clone()),
                vec![],
                None,
                3,
            )
            .unwrap();
        s.assign(&c.id, "x", 4).unwrap();
        s.mark_dispatched(&c.id, "x", 5).unwrap();
        s.submit_by_agent("x", None, "HEMMELIG opsummering", 6)
            .unwrap()
    }

    /// Coordinator k1 (staff, idle) whose parent waits for `n` children in review.
    fn waiting_parent(h: &mut Harness, n: usize) -> (Ticket, Vec<Ticket>) {
        h.agent_on(
            "k1",
            AgentStatus::Idle,
            SeatKind::Staff,
            &[Role::Coordinator],
        );
        let p = h.queued("k1", "Byg spillet");
        h.svc().mark_dispatched(&p.id, "k1", 2).unwrap();
        let kids: Vec<Ticket> = (0..n)
            .map(|i| child_in_review(h, &p, &format!("Del {i}")))
            .collect();
        let p = h.svc().submit_by_agent("k1", None, "fordelt", 7).unwrap();
        assert_eq!(p.state, S::Waiting);
        (p, kids)
    }

    fn queue_changed(h: &mut Harness, agent: &str) {
        h.send(DispatchMsg::QueueChanged {
            agent_id: agent.into(),
        });
    }

    #[test]
    fn child_done_wakes_idle_parent_assignee() {
        let mut h = Harness::new();
        let (p, kids) = waiting_parent(&mut h, 2);
        h.svc().approve(&kids[0].id, 10).unwrap();
        // TicketsCtx's hook sends this for the parent's assignee.
        queue_changed(&mut h, "k1");
        h.advance(DISPATCH_DELAY_MS - 1);
        assert!(h.writes().is_empty(), "the usual delay");
        h.advance(1);
        let w = h.writes();
        assert_eq!(w.len(), 1);
        let l = &w[0].1;
        assert!(l.starts_with("Du har fået besked"), "{l}");
        assert!(
            l.contains(&kids[0].short_id()) && l.contains(&p.short_id()),
            "{l}"
        );
        assert!(l.contains("1 del-ticket(s) mangler stadig"), "{l}");
        assert!(!l.contains("HEMMELIG"), "no child summary in the terminal");
        assert!(!l.contains(['\r', '\n']));
        // P2: the parent waits until the line is entered …
        assert_eq!(h.ticket(&p.id).state, S::Waiting);
        h.advance(ENTER_DELAY_MS);
        assert_eq!(h.writes()[1], enter("k1"));
        // … and (review 6a W1) confirmed by its UserPromptSubmit.
        assert_eq!(h.ticket(&p.id).state, S::Waiting);
        h.submitted("k1", l);
        let pt = h.ticket(&p.id);
        assert_eq!(pt.state, S::InProgress);
        let last = pt.history.last().unwrap();
        assert_eq!(last.note.as_deref(), Some(WOKEN_NOTE));
        assert_eq!(last.by, TicketActor::System);
        // Not due any more: no second line at the next idle.
        h.idle("k1");
        h.advance(10_000);
        assert_eq!(h.writes().len(), 2);
    }

    #[test]
    fn wake_waits_while_agent_busy_or_working() {
        let mut h = Harness::new();
        let (p, kids) = waiting_parent(&mut h, 1);
        // k1 took its next ticket while the parent waits.
        let other = h.queued("k1", "Næste");
        h.svc().mark_dispatched(&other.id, "k1", 8).unwrap();
        h.svc().approve(&kids[0].id, 10).unwrap();
        queue_changed(&mut h, "k1");
        h.idle("k1");
        h.advance(10_000);
        assert!(
            h.writes().is_empty(),
            "never in the middle of another ticket"
        );
        // That ticket is submitted, but the agent is busy: nothing either.
        h.svc().submit_by_agent("k1", None, "ok", 11).unwrap();
        h.set_status("k1", AgentStatus::Thinking);
        queue_changed(&mut h, "k1");
        h.advance(10_000);
        assert!(h.writes().is_empty(), "busy");
        // Idle again, but the user just typed: the grace first.
        h.set_status("k1", AgentStatus::Idle);
        h.user_typed("k1");
        h.idle("k1");
        h.advance(USER_INPUT_GRACE_MS - 1);
        assert!(h.writes().is_empty(), "grace");
        h.advance(1);
        let w = h.writes();
        assert_eq!(w.len(), 1);
        assert!(w[0]
            .1
            .starts_with("Du har fået besked: alle del-tickets til ticket"));
        // The Enter is still pending: the parent waits until then.
        assert_eq!(h.ticket(&p.id).state, S::Waiting);
    }

    #[test]
    fn wake_is_dropped_when_the_agent_gets_busy_before_typing() {
        let mut h = Harness::new();
        let (p, kids) = waiting_parent(&mut h, 1);
        h.svc().approve(&kids[0].id, 10).unwrap();
        queue_changed(&mut h, "k1");
        h.set_status("k1", AgentStatus::Thinking);
        h.advance(DISPATCH_DELAY_MS);
        assert!(h.writes().is_empty());
        // Still due (derived from the data): typed at the next idle.
        h.set_status("k1", AgentStatus::Idle);
        h.idle("k1");
        h.advance(DISPATCH_DELAY_MS + ENTER_DELAY_MS);
        assert_eq!(h.writes().len(), 2);
        let l = h.writes()[0].1.clone();
        h.submitted("k1", &l);
        assert_eq!(h.ticket(&p.id).state, S::InProgress);
    }

    #[test]
    fn last_child_done_line_asks_for_submission() {
        let mut h = Harness::new();
        let (p, kids) = waiting_parent(&mut h, 1);
        h.svc().approve(&kids[0].id, 10).unwrap();
        queue_changed(&mut h, "k1");
        h.advance(DISPATCH_DELAY_MS);
        let l = h.writes()[0].1.clone();
        assert_eq!(
            l,
            prompt::wake_line(&WakeInfo {
                parent_short: p.short_id(),
                newly_done: vec![(kids[0].short_id(), "Del 0".into())],
                open_left: 0,
            })
        );
        assert!(l.contains(&format!(
            "alle del-tickets til ticket {} er afsluttet",
            p.short_id()
        )));
        assert!(l.contains(&format!("(ticketId {})", p.short_id())));
        h.advance(ENTER_DELAY_MS);
        h.submitted("k1", &l);
        let pt = h.ticket(&p.id);
        assert_eq!(pt.state, S::InProgress);
        assert_eq!(
            pt.history.last().unwrap().note.as_deref(),
            Some(CHILDREN_DONE_NOTE)
        );
        // A Stop without submitting now: nothing open → "Ikke afleveret" as before.
        stop(&mut h, "k1");
        assert_eq!(h.ticket(&p.id).state, S::InProgress);
        assert_eq!(h.ticket(&p.id).issue, Some(TicketIssue::NotSubmitted));
        assert_eq!(h.detail("k1").as_deref(), Some(NOT_SUBMITTED_TEXT));
    }

    #[test]
    fn wake_is_confirmed_only_by_its_own_line() {
        let mut h = Harness::new();
        let (p, kids) = waiting_parent(&mut h, 2);
        h.svc().approve(&kids[0].id, 10).unwrap();
        queue_changed(&mut h, "k1");
        h.advance(DISPATCH_DELAY_MS);
        let l = h.writes()[0].1.clone();
        for id in [&p.id, &kids[0].id, &kids[1].id] {
            assert!(!DeliveryKind::Work.confirms(id, &l));
            assert!(!DeliveryKind::Review.confirms(id, &l));
        }
        // Review 6a W1: the wake kind confirms the parent's line only, and never a delivery line.
        assert!(DeliveryKind::Wake.confirms(&p.id, &l));
        assert!(!DeliveryKind::Wake.confirms(&kids[0].id, &l));
        for other in [
            format!("Ticket {}: x", p.short_id()),
            format!("Koordiner ticket {}: x", p.short_id()),
            format!("Review af ticket {}: x", p.short_id()),
        ] {
            assert!(!DeliveryKind::Wake.confirms(&p.id, &other), "{other}");
        }
        // Enter: a busy status and a prompt without text do not resume the parent.
        h.advance(ENTER_DELAY_MS);
        assert_eq!(h.writes()[1], enter("k1"));
        let before = h.ticket(&p.id);
        h.send(DispatchMsg::AgentBusy {
            agent_id: "k1".into(),
        });
        h.send(DispatchMsg::PromptSubmitted {
            agent_id: "k1".into(),
            prompt: None,
        });
        assert_eq!(h.ticket(&p.id), before);
        assert_eq!(h.ticket(&kids[1].id).state, S::Review);
        // The line's UserPromptSubmit does.
        h.submitted("k1", &l);
        let pt = h.ticket(&p.id);
        assert_eq!(pt.state, S::InProgress);
        assert_eq!(pt.history.last().unwrap().note.as_deref(), Some(WOKEN_NOTE));
        // The confirm timer is stale now: nothing more happens.
        h.set_status("k1", AgentStatus::Idle);
        h.idle("k1");
        h.advance(CONFIRM_TIMEOUT_MS + 10_000);
        assert_eq!(h.writes().len(), 2);
        assert_eq!(h.detail("k1"), None);
    }

    /// Review 6a W1: Esc (or an Enter that sends nothing) after the wake line: no confirmation,
    /// so the parent stays waiting, the line is typed once more at the next idle, and after that
    /// the agent shows the hint until the user asks again.
    #[test]
    fn unconfirmed_wake_keeps_parent_waiting_and_retries_once() {
        let mut h = Harness::new();
        let (p, kids) = waiting_parent(&mut h, 2);
        h.svc().approve(&kids[0].id, 10).unwrap();
        queue_changed(&mut h, "k1");
        h.advance(DISPATCH_DELAY_MS + ENTER_DELAY_MS);
        assert_eq!(h.writes().len(), 2);
        let l = h.writes()[0].1.clone();
        // No prompt: still waiting while the confirmation may come …
        h.advance(CONFIRM_TIMEOUT_MS - 1);
        assert_eq!(h.ticket(&p.id).state, S::Waiting);
        // … and after the timeout: no extra Enter, nothing typed until the next idle.
        h.advance(1);
        assert_eq!(h.ticket(&p.id).state, S::Waiting);
        h.advance(10_000);
        assert_eq!(h.writes().len(), 2);
        assert_eq!(h.detail("k1"), None, "the first miss shows nothing");
        // Next idle: the same line once more (still derived from the data).
        h.idle("k1");
        h.advance(DISPATCH_DELAY_MS + ENTER_DELAY_MS);
        let w = h.writes();
        assert_eq!(w.len(), 4);
        assert_eq!(w[2].1, l);
        assert_eq!(w[3], enter("k1"));
        h.advance(CONFIRM_TIMEOUT_MS);
        assert_eq!(h.ticket(&p.id).state, S::Waiting);
        assert_eq!(h.detail("k1").as_deref(), Some(WAKE_UNCONFIRMED_TEXT));
        // Given up: no more lines at idle or queue changes.
        h.idle("k1");
        queue_changed(&mut h, "k1");
        h.advance(10_000);
        assert_eq!(h.writes().len(), 4);
        // "Bed om aflevering" on the waiting parent types it again; confirmed, the hint goes.
        h.send(DispatchMsg::RequestSubmission {
            ticket_id: p.id.clone(),
        });
        h.advance(DISPATCH_DELAY_MS + ENTER_DELAY_MS);
        assert_eq!(h.writes().len(), 6);
        h.submitted("k1", &l);
        assert_eq!(h.ticket(&p.id).state, S::InProgress);
        assert_eq!(h.detail("k1"), None);
    }

    /// Review 6a W1: a changed wake (another child done) is typed again after a give-up, and a
    /// stop/restart forgets the attempts.
    #[test]
    fn new_child_done_after_give_up_wakes_again() {
        let mut h = Harness::new();
        let (p, kids) = waiting_parent(&mut h, 3);
        h.svc().approve(&kids[0].id, 10).unwrap();
        for _ in 0..WAKE_MAX_ATTEMPTS {
            h.idle("k1");
            h.advance(DISPATCH_DELAY_MS + ENTER_DELAY_MS + CONFIRM_TIMEOUT_MS);
        }
        assert_eq!(h.writes().len(), 4);
        assert_eq!(h.detail("k1").as_deref(), Some(WAKE_UNCONFIRMED_TEXT));
        h.svc().approve(&kids[1].id, 20).unwrap();
        queue_changed(&mut h, "k1");
        h.advance(DISPATCH_DELAY_MS + ENTER_DELAY_MS);
        assert_eq!(h.writes().len(), 6);
        let l = h.writes()[4].1.clone();
        assert!(l.contains(&kids[1].short_id()), "{l}");
        h.submitted("k1", &l);
        assert_eq!(h.ticket(&p.id).state, S::InProgress);
    }

    /// Review 6a W1: the wake line is never entered twice: not while its confirmation is
    /// pending, not when it is confirmed late, and not when the user pressed Enter before ours.
    #[test]
    fn wake_line_is_never_typed_twice() {
        let mut h = Harness::new();
        let (p, kids) = waiting_parent(&mut h, 2);
        h.svc().approve(&kids[0].id, 10).unwrap();
        queue_changed(&mut h, "k1");
        h.advance(DISPATCH_DELAY_MS + ENTER_DELAY_MS);
        let l = h.writes()[0].1.clone();
        // Pending confirmation: idle and queue changes start nothing.
        h.idle("k1");
        queue_changed(&mut h, "k1");
        h.advance(CONFIRM_TIMEOUT_MS - 1);
        assert_eq!(h.writes().len(), 2);
        // The confirmation arrives after the timeout: still counts, nothing typed again.
        h.advance(1);
        h.submitted("k1", &l);
        assert_eq!(h.ticket(&p.id).state, S::InProgress);
        h.idle("k1");
        h.advance(10_000);
        assert_eq!(h.writes().len(), 2);

        // The user pressed Enter on the line before the app did: our Enter is dropped.
        let mut h = Harness::new();
        let (p, kids) = waiting_parent(&mut h, 2);
        h.svc().approve(&kids[0].id, 10).unwrap();
        queue_changed(&mut h, "k1");
        h.advance(DISPATCH_DELAY_MS);
        let l = h.writes()[0].1.clone();
        h.submitted("k1", &l);
        assert_eq!(h.ticket(&p.id).state, S::InProgress);
        h.advance(ENTER_DELAY_MS + CONFIRM_TIMEOUT_MS + 10_000);
        assert_eq!(h.writes().len(), 1, "no Enter after the line went in");
    }

    #[test]
    fn stop_without_submit_on_parent_with_open_children_waits_without_hint() {
        let mut h = Harness::new();
        h.agent_on(
            "k1",
            AgentStatus::Idle,
            SeatKind::Staff,
            &[Role::Coordinator],
        );
        let p = h.queued("k1", "Byg spillet");
        h.svc().mark_dispatched(&p.id, "k1", 2).unwrap();
        child_in_review(&h, &p, "Del");
        let next = h.queued("k1", "Næste");
        stop(&mut h, "k1");
        let pt = h.ticket(&p.id);
        assert_eq!(pt.state, S::Waiting);
        assert_eq!(pt.issue, None);
        assert_eq!(h.detail("k1"), None, "no NOT_SUBMITTED_TEXT");
        // The queue moves on.
        h.advance(DISPATCH_DELAY_MS);
        assert!(h.writes()[0]
            .1
            .starts_with(&format!("Koordiner ticket {}: ", next.short_id())));

        // Also with autoReviewOnStop: waiting, not review.
        let mut h = Harness::new();
        h.d.auto_review_on_stop = Some(true);
        h.agent_on(
            "k1",
            AgentStatus::Idle,
            SeatKind::Staff,
            &[Role::Coordinator],
        );
        let p = h.queued("k1", "Byg spillet");
        h.svc().mark_dispatched(&p.id, "k1", 2).unwrap();
        child_in_review(&h, &p, "Del");
        stop(&mut h, "k1");
        assert_eq!(h.ticket(&p.id).state, S::Waiting);
        assert_eq!(h.detail("k1"), None);
    }

    #[test]
    fn blocked_ticket_is_skipped_and_delivered_when_blocker_is_done() {
        let mut h = Harness::new();
        h.agent("a1", AgentStatus::Idle);
        // The blocker: in review with the portless agent "x".
        let blocker = {
            let mut s = h.svc();
            let b = s.create("Plan", "", false, 1).unwrap();
            s.assign(&b.id, "x", 2).unwrap();
            s.mark_dispatched(&b.id, "x", 3).unwrap();
            s.submit_by_agent("x", None, "plan", 4).unwrap()
        };
        let first = {
            let mut s = h.svc();
            let t = s
                .create_by_agent_related(
                    "Byg",
                    "",
                    false,
                    None,
                    None,
                    None,
                    vec![blocker.id.clone()],
                    None,
                    5,
                )
                .unwrap();
            s.assign(&t.id, "a1", 6).unwrap()
        };
        let second = h.queued("a1", "Andet");
        assert_eq!(first.queue_position, Some(0));
        deliver_until_enter(&mut h, "a1");
        assert_eq!(h.writes()[0], ("a1".into(), line(&second)));
        h.submitted("a1", &line(&second));
        assert_eq!(h.ticket(&second.id).state, S::InProgress);
        // The blocked ticket keeps its place.
        assert_eq!(h.ticket(&first.id).queue_position, Some(0));
        assert_eq!(h.ticket(&first.id).state, S::Assigned);
        // "Send igen" of the blocked ticket does nothing.
        h.svc().submit_by_agent("a1", None, "ok", 7).unwrap();
        h.send(DispatchMsg::Redispatch {
            ticket_id: first.id.clone(),
        });
        assert_eq!(h.writes().len(), 2);
        h.idle("a1");
        h.advance(10_000);
        assert_eq!(h.writes().len(), 2, "still blocked");
        // The blocker is approved: the hook's QueueChanged delivers it.
        h.svc().approve(&blocker.id, 20).unwrap();
        queue_changed(&mut h, "a1");
        h.advance(DISPATCH_DELAY_MS);
        assert_eq!(h.writes()[2], ("a1".into(), line(&first)));
    }

    #[test]
    fn plan_delivery_switches_text_when_coordinator_lives() {
        use prompt::{COORDINATION_PLAN_TEXT, COORDINATION_PLAN_WITH_COORDINATOR_TEXT};
        let plan_file = |coordinator_live: bool| {
            let mut h = Harness::new();
            lock(&h.port.0).coordinator_live = coordinator_live;
            h.agent_on("p1", AgentStatus::Idle, SeatKind::Staff, &[Role::Planner]);
            let t = h.queued("p1", "Plan spillet");
            h.idle("p1");
            h.advance(DISPATCH_DELAY_MS);
            assert!(h.writes()[0].1.starts_with("Koordiner ticket "));
            h.ticket_file("p1", &t)
        };
        let f = plan_file(false);
        assert!(f.contains(COORDINATION_PLAN_TEXT));
        assert!(!f.contains("Der er en koordinator i staben"));
        let f = plan_file(true);
        assert!(f.contains(&format!(
            "## Koordineringsopgave\n{COORDINATION_PLAN_WITH_COORDINATOR_TEXT}\n"
        )));
        assert!(!f.contains(COORDINATION_PLAN_TEXT));

        // No live coordinator per the port, but the ticket's parent belongs to one; the parent's
        // own file lists its children.
        let mut h = Harness::new();
        h.agent_on(
            "k1",
            AgentStatus::Idle,
            SeatKind::Staff,
            &[Role::Coordinator],
        );
        h.agent_on("p1", AgentStatus::Idle, SeatKind::Staff, &[Role::Planner]);
        let parent = h.queued("k1", "Mål");
        let child = {
            let mut s = h.svc();
            let c = s
                .create_by_agent_related(
                    "Plan",
                    "",
                    false,
                    None,
                    None,
                    Some(parent.id.clone()),
                    vec![],
                    None,
                    3,
                )
                .unwrap();
            s.assign(&c.id, "p1", 4).unwrap()
        };
        h.idle("p1");
        h.idle("k1");
        h.advance(DISPATCH_DELAY_MS);
        let f = h.ticket_file("p1", &child);
        assert!(f.contains(COORDINATION_PLAN_WITH_COORDINATOR_TEXT), "{f}");
        let f = h.ticket_file("k1", &parent);
        assert!(
            f.contains(&format!(
                "## Del-tickets\n{}\n- {} Plan — I kø\n",
                prompt::CHILDREN_INTRO,
                child.short_id()
            )),
            "{f}"
        );
    }

    #[test]
    fn review_of_parent_lists_children_and_rule() {
        let mut h = Harness::new();
        h.agent_on("rev", AgentStatus::Idle, SeatKind::Staff, &[Role::Reviewer]);
        let (p, kids) = waiting_parent(&mut h, 1);
        h.svc().approve(&kids[0].id, 10).unwrap();
        // The coordinator submits the parent (all children done) → review, routed to "rev".
        {
            let mut s = h.svc();
            s.resume_after_wake(&p.id, "k1", WOKEN_NOTE, 11).unwrap();
            assert_eq!(
                s.submit_by_agent("k1", None, "Samlet", 12).unwrap().state,
                S::Review
            );
            s.route_review(&p.id, "rev", "bot-rev", 13).unwrap();
        }
        h.idle("rev");
        h.advance(DISPATCH_DELAY_MS);
        let f = fs::read_to_string(
            prompt::review_dir(&h.root.join("rev")).join(format!("{}.md", p.short_id())),
        )
        .unwrap();
        assert!(
            f.contains(&format!(
                "## Del-tickets (allerede reviewet)\n- {} Del 0 — Done: HEMMELIG opsummering\n## Opgaven\n",
                kids[0].short_id()
            )),
            "{f}"
        );
        assert!(f.ends_with(&format!("{}\n", prompt::PARENT_REVIEW_RULE)));
    }
}
