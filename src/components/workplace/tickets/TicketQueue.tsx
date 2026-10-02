import { useMemo } from "react";
import {
  redispatchTicket,
  reorderQueue,
  requestSubmission,
  setTicketState,
  unassignTicket,
} from "../../../lib/ipc";
import { isExited } from "../../../lib/status";
import {
  BLOCKED_HINT,
  blockersOf,
  canRedispatch,
  canReopen,
  canRequestSubmission,
  DELIVERY_FAILED_TEXT,
  hasRedispatchIssue,
  isCoordinationTask,
  ISSUE_HINT,
  ISSUE_LABEL,
  moveUp,
  NOT_SUBMITTED_TEXT,
  progressOf,
  queueFor,
  STATE_BADGE_CLASS,
  STATE_LABEL,
  SUBMIT_PARENT_HINT,
  TURN_FAILED_TEXT,
  WAITING_TITLE,
  WAKE_UNCONFIRMED_TEXT,
} from "../../../lib/tickets";
import type { AgentInfo, TicketSummary } from "../../../lib/types";
import { useStore } from "../../../state/store";
import { smallBtn, useRun } from "./actions";

function Badge({ t }: { t: TicketSummary }) {
  return (
    <span className={`shrink-0 rounded px-1.5 text-[10px] font-medium leading-4 ${STATE_BADGE_CLASS[t.state]}`}>
      {STATE_LABEL[t.state]}
    </span>
  );
}

function IssueBadge({ t }: { t: TicketSummary }) {
  if (t.issue === null) return null;
  return (
    <span
      className="shrink-0 rounded bg-amber-400/30 px-1.5 text-[10px] leading-4 text-amber-900 dark:text-amber-100"
      title={`${ISSUE_LABEL[t.issue]} — ${ISSUE_HINT[t.issue]}`}
    >
      ⚠ {ISSUE_LABEL[t.issue]}
    </span>
  );
}

function BlockedBadge({ t, all }: { t: TicketSummary; all: readonly TicketSummary[] }) {
  const blockers = blockersOf(t, all);
  if (blockers.length === 0) return null;
  return (
    <span
      className="shrink-0 rounded bg-amber-400/30 px-1.5 text-[10px] leading-4 text-amber-900 dark:text-amber-100"
      title={BLOCKED_HINT}
    >
      Venter på {blockers.map((b) => b.shortId).join(", ")}
    </span>
  );
}

function Title({ t }: { t: TicketSummary }) {
  return (
    <span className="min-w-0 flex-1 truncate" title={`${t.title} (${t.shortId})`}>
      {t.title} <span className="font-mono text-[10px] text-[var(--muted)]">({t.shortId})</span>
    </span>
  );
}

/**
 * The selected agent's tickets above its terminal: the ticket in progress (with "Send igen" after
 * a failed turn or delivery, "Send til review" / "Bed om aflevering" after a turn that ended
 * without `mira_submit_for_review`, and manual moves since Esc gives no Stop), the queue with "Flyt op"
 * and "Fjern fra kø", and its tickets waiting in review ("Ikke færdig").
 */
export default function TicketQueue({ agent }: { agent: AgentInfo }) {
  const { state } = useStore();
  const run = useRun();
  const exited = isExited(agent);

  const { current, queue, review, waiting } = useMemo(() => {
    const mine = state.tickets.filter((t) => t.assigneeAgentId === agent.id);
    return {
      current: mine.find((t) => t.state === "inProgress") ?? null,
      queue: queueFor(mine, agent.id),
      review: mine.filter((t) => t.state === "review"),
      // Parents waiting for their children, oldest first (the app wakes the agent; a button only
      // after an unconfirmed wake, review 6a W1).
      waiting: mine.filter((t) => t.state === "waiting").sort((a, b) => a.updatedAt - b.updatedAt),
    };
  }, [state.tickets, agent.id]);

  // The ticket "Send igen" applies to: the running one after a failed turn, or the queue head
  // after a failed delivery. `notSubmitted` has its own buttons (below) and is never redispatched.
  const issueTicket =
    current !== null && hasRedispatchIssue(current)
      ? current
      : (queue.find(hasRedispatchIssue) ?? null);
  const notSubmitted = current !== null && current.issue === "notSubmitted" ? current : null;
  const hintText =
    agent.detail === DELIVERY_FAILED_TEXT ||
    agent.detail === TURN_FAILED_TEXT ||
    agent.detail === NOT_SUBMITTED_TEXT ||
    agent.detail === WAKE_UNCONFIRMED_TEXT
      ? agent.detail
      : null;
  // The hint row carries the notSubmitted buttons when the agent's detail shows the hint; the
  // ticket's own row carries them otherwise (detail cleared or replaced by a status line).
  const submitButtonsInHint = notSubmitted !== null && hintText === NOT_SUBMITTED_TEXT;

  const submitButtons = (t: TicketSummary) => (
    <>
      <button
        type="button"
        onClick={() => void run(() => setTicketState(t.id, "review", null))}
        title={
          t.skipReview
            ? "Flyt ticketen til Done uden agentens opsummering"
            : "Flyt ticketen til Review uden agentens opsummering"
        }
        className={smallBtn}
      >
        {t.skipReview ? "Færdig" : "Send til review"}
      </button>
      <button
        type="button"
        onClick={() => void run(() => requestSubmission(t.id))}
        disabled={!canRequestSubmission(t, agent)}
        title={
          canRequestSubmission(t, agent)
            ? "Taster en kort besked i terminalen, der beder agenten kalde mira_submit_for_review"
            : "Virker når agenten er Klar"
        }
        className={smallBtn}
      >
        Bed om aflevering
      </button>
    </>
  );

  const resend = (t: TicketSummary) => (
    <button
      type="button"
      onClick={() => void run(() => redispatchTicket(t.id))}
      disabled={!canRedispatch(t, agent)}
      title={
        canRedispatch(t, agent)
          ? "Skriv ticket-linjen i terminalen igen"
          : "Virker når agenten er Klar"
      }
      className={smallBtn}
    >
      Send igen
    </button>
  );

  const total = (current === null ? 0 : 1) + queue.length + review.length + waiting.length;
  // Tickets on a staff agent (coordinator, reviewer) are coordination tasks (plan5 A.7).
  const heading = isCoordinationTask(agent) ? "Koordineringsopgaver" : "Tickets";
  const summary =
    total === 0
      ? `${heading}: ingen`
      : `${heading}: ${current === null ? "ingen i gang" : "1 i gang"}${
          waiting.length > 0 ? ` · ${waiting.length} venter` : ""
        } · ${queue.length} i kø${review.length > 0 ? ` · ${review.length} i review` : ""}`;

  return (
    <details open className="mx-3 mb-2 shrink-0 rounded-lg border border-[var(--border)] text-xs">
      <summary className="cursor-pointer select-none px-2 py-1 text-[var(--muted)] hover:text-[var(--fg)]">
        {summary}
      </summary>
      <div className="max-h-[140px] space-y-1.5 overflow-y-auto px-2 pb-2">
        {hintText !== null && (
          <div className="flex items-center gap-2 rounded-lg border border-amber-400/50 bg-amber-300/20 px-2 py-1 text-amber-800 dark:text-amber-200">
            <span className="min-w-0 flex-1">{hintText}</span>
            {issueTicket !== null && resend(issueTicket)}
            {submitButtonsInHint && notSubmitted !== null && submitButtons(notSubmitted)}
          </div>
        )}

        {current === null ? (
          <p className="text-[var(--muted)]">Ingen ticket i gang</p>
        ) : (
          <div className="flex items-center gap-1.5">
            <span className="shrink-0 text-[var(--muted)]">Aktuel:</span>
            <Title t={current} />
            <Badge t={current} />
            <IssueBadge t={current} />
            {hasRedispatchIssue(current) && hintText === null && resend(current)}
            {notSubmitted !== null && !submitButtonsInHint && submitButtons(notSubmitted)}
            {notSubmitted === null && (
              <button
                type="button"
                onClick={() => void run(() => setTicketState(current.id, "review", null))}
                title={
                  progressOf(current.id, state.tickets).done < progressOf(current.id, state.tickets).total
                    ? SUBMIT_PARENT_HINT
                    : current.skipReview
                      ? "Marker som færdig (ticketen springer review over og går til Done)"
                      : "Agenten er færdig (fx efter Esc): flyt ticketen til Review"
                }
                className={smallBtn}
              >
                {current.skipReview ? "Færdig" : "Til review"}
              </button>
            )}
            <button
              type="button"
              onClick={() => void run(() => setTicketState(current.id, "backlog", "flyttet tilbage af brugeren"))}
              title="Tag ticketen fra agenten og læg den i Backlog"
              className={smallBtn}
            >
              Til backlog
            </button>
          </div>
        )}

        {waiting.map((t) => {
          const p = progressOf(t.id, state.tickets);
          return (
            <div key={t.id} className="flex items-center gap-1.5">
              <span className="shrink-0 text-[var(--muted)]">{WAITING_TITLE}:</span>
              <Title t={t} />
              <Badge t={t} />
              {p.total > 0 && (
                <span className="shrink-0 text-[10px] text-[var(--muted)]" title="Del-tickets færdige">
                  {p.done}/{p.total}
                </span>
              )}
              {/* Review 6a W1: the wake line went unconfirmed; the user asks for it again. */}
              {hintText === WAKE_UNCONFIRMED_TEXT && (
                <button
                  type="button"
                  onClick={() => void run(() => requestSubmission(t.id))}
                  disabled={!canRequestSubmission(t, agent)}
                  title={
                    canRequestSubmission(t, agent)
                      ? "Taster beskeden om de godkendte del-tickets i terminalen igen"
                      : "Virker når agenten er Klar"
                  }
                  className={smallBtn}
                >
                  Bed om aflevering
                </button>
              )}
            </div>
          );
        })}

        {queue.length > 0 && (
          <ol className="space-y-1">
            {queue.map((t, i) => (
              <li key={t.id} className="flex items-center gap-1.5">
                <span className="w-5 shrink-0 text-right font-mono text-[var(--muted)]">{i + 1}.</span>
                <Title t={t} />
                <IssueBadge t={t} />
                <BlockedBadge t={t} all={state.tickets} />
                {i === 0 && hasRedispatchIssue(t) && hintText === null && resend(t)}
                <button
                  type="button"
                  onClick={() => {
                    const ids = moveUp(
                      queue.map((q) => q.id),
                      i,
                    );
                    if (ids !== null) void run(() => reorderQueue(agent.id, ids));
                  }}
                  disabled={i === 0}
                  title="Flyt én plads frem i køen"
                  aria-label={`Flyt ${t.title} op`}
                  className={smallBtn}
                >
                  Flyt op
                </button>
                <button
                  type="button"
                  onClick={() => void run(() => unassignTicket(t.id))}
                  title="Tag ticketen ud af køen og læg den i Backlog"
                  aria-label={`Fjern ${t.title} fra kø`}
                  className={smallBtn}
                >
                  Fjern fra kø
                </button>
              </li>
            ))}
          </ol>
        )}

        {review.map((t) => (
          <div key={t.id} className="flex items-center gap-1.5">
            <Title t={t} />
            <Badge t={t} />
            <button
              type="button"
              onClick={() => void run(() => setTicketState(t.id, "inProgress", "ikke færdig"))}
              disabled={!canReopen(t, agent)}
              title={
                exited
                  ? "Agenten kører ikke"
                  : "Flyt ticketen tilbage til I gang; skriv selv til agenten hvad der mangler"
              }
              className={smallBtn}
            >
              Ikke færdig
            </button>
          </div>
        ))}
      </div>
    </details>
  );
}
