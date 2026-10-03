import { useEffect, useMemo, useState } from "react";
import { assignReviewer, errorMessage, listReviewAssignments } from "../../lib/ipc";
import { reviewRoundText, reviewsFor } from "../../lib/tickets";
import type { AgentInfo, ReviewAssignment } from "../../lib/types";
import { useStore } from "../../state/store";
import { smallBtn, useRun } from "./tickets/actions";

/**
 * "Reviews til denne agent (n)": tickets in review with this agent as reviewer, with the round
 * and whether the review line has been typed yet (`listReviewAssignments`, refetched when the
 * tickets or the agent's open reviews change). Shown for reviewer agents and whenever n > 0.
 */
export default function AgentReviews({ agent }: { agent: AgentInfo }) {
  const { state } = useStore();
  const run = useRun();
  const reviews = useMemo(() => reviewsFor(state.tickets, agent.id), [state.tickets, agent.id]);
  const [assignments, setAssignments] = useState<ReviewAssignment[]>([]);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    listReviewAssignments()
      .then((list) => {
        if (!alive) return;
        setAssignments(list.filter((a) => a.reviewerAgentId === agent.id));
        setError(null);
      })
      .catch((e: unknown) => {
        if (alive) setError(errorMessage(e));
      });
    return () => {
      alive = false;
    };
  }, [agent.id, agent.openReviews, state.tickets]);

  if (reviews.length === 0 && !agent.roles.includes("reviewer")) return null;

  return (
    <details className="mx-3 mb-2 shrink-0 rounded-lg border border-[var(--border)] text-xs">
      <summary className="cursor-pointer select-none px-2 py-1 text-[var(--muted)] hover:text-[var(--fg)]">
        Reviews til denne agent ({reviews.length})
      </summary>
      <div className="max-h-[120px] space-y-1 overflow-y-auto px-2 pb-2">
        {error !== null && (
          <p className="text-rose-500" role="alert">
            {error}
          </p>
        )}
        {reviews.length === 0 ? (
          <p className="text-[var(--muted)]">Ingen reviews lige nu</p>
        ) : (
          reviews.map((t) => {
            const a = assignments.find((x) => x.ticketId === t.id);
            return (
              <div key={t.id} className="flex items-center gap-1.5">
                <span className="min-w-0 flex-1 truncate" title={`${t.title} (${t.shortId})`}>
                  {t.title} <span className="font-mono text-[10px] text-[var(--muted)]">({t.shortId})</span>
                </span>
                <span className="shrink-0 text-[10px] text-[var(--muted)]">{reviewRoundText(t, state.appInfo?.rules.maxReviewRounds ?? 3)}</span>
                <span className="shrink-0 text-[10px] text-[var(--muted)]">
                  {a === undefined ? "" : a.deliveredAt !== null ? "sendt" : "venter på levering"}
                </span>
                <button
                  type="button"
                  onClick={() => void run(() => assignReviewer(t.id, null))}
                  title="Tag reviewet fra agenten; appen finder en anden reviewer, hvis der er en"
                  className={smallBtn}
                >
                  Fjern reviewer
                </button>
              </div>
            );
          })
        )}
      </div>
    </details>
  );
}
