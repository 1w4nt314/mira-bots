import { useEffect, useMemo, useState, type ReactNode } from "react";
import { getDiagnostics } from "../../../lib/ipc";
import { DONE_VISIBLE, groupTickets } from "../../../lib/tickets";
import type { AgentInfo, TicketSummary } from "../../../lib/types";
import { useStore } from "../../../state/store";
import NewTicketForm from "./NewTicketForm";
import StickyNote from "./StickyNote";

const NOTICE_VISIBLE_MS = 5000;

function Section({ title, count, children }: { title: string; count?: number; children: ReactNode }) {
  return (
    <section className="space-y-2">
      <h3 className="text-[11px] font-semibold uppercase tracking-wide text-[var(--muted)]">
        {title}
        {count !== undefined && ` (${count})`}
      </h3>
      {children}
    </section>
  );
}

function Empty({ text }: { text: string }) {
  return <p className="text-xs text-[var(--muted)]">{text}</p>;
}

/**
 * The sidebar's Tickets tab: Review (only when something waits), Backlog with "Ny ticket", and
 * a folded Done list. Queued and running tickets are shown at the seats and under the terminal.
 * Backlog notes are draggable onto seats (the DndContext lives in Workplace).
 */
export default function TicketsPanel() {
  const { state } = useStore();
  const groups = useMemo(() => groupTickets(state.tickets), [state.tickets]);
  const agentsById = useMemo(
    () => new Map<string, AgentInfo>(state.agents.map((a) => [a.id, a])),
    [state.agents],
  );
  const [showForm, setShowForm] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  // The startup warning for tickets.json (unreadable → read-only, or renamed as broken). It
  // cannot change while the app runs, so one read on mount is enough.
  const [fileWarning, setFileWarning] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    getDiagnostics()
      .then((d) => {
        if (alive && d.ticketsWarning !== null) setFileWarning(d.ticketsWarning);
      })
      .catch(() => {
        // Diagnostics shows the same warning; nothing more to do here.
      });
    return () => {
      alive = false;
    };
  }, []);

  useEffect(() => {
    if (notice === null) return;
    const t = setTimeout(() => setNotice(null), NOTICE_VISIBLE_MS);
    return () => clearTimeout(t);
  }, [notice]);

  const agentOf = (t: TicketSummary) =>
    t.assigneeAgentId === null ? null : (agentsById.get(t.assigneeAgentId) ?? null);

  let atAgents = 0;
  for (const s of groups.byAgent.values()) atAgents += s.queue.length + (s.current === null ? 0 : 1);

  const doneShown = groups.done.slice(0, DONE_VISIBLE);
  const doneHidden = groups.done.length - doneShown.length;

  return (
    <div className="office-cork space-y-4 p-3">
      {fileWarning !== null && (
        <p
          className="rounded-lg border border-amber-400/50 bg-amber-300/20 px-2 py-1 text-xs text-amber-800 dark:text-amber-200"
          role="alert"
        >
          {fileWarning}
        </p>
      )}
      {notice !== null && (
        <p
          className="rounded-lg border border-emerald-500/40 bg-emerald-400/15 px-2 py-1 text-xs text-emerald-800 dark:text-emerald-200"
          role="status"
        >
          {notice}
        </p>
      )}

      {groups.review.length > 0 && (
        <Section title="Review" count={groups.review.length}>
          {groups.review.map((t) => (
            <StickyNote key={t.id} ticket={t} agent={agentOf(t)} draggable={false} onNotice={setNotice} />
          ))}
        </Section>
      )}

      <Section title="Backlog" count={groups.backlog.length}>
        {showForm ? (
          <NewTicketForm onClose={() => setShowForm(false)} />
        ) : (
          <button
            type="button"
            onClick={() => setShowForm(true)}
            title="Opret en ny ticket i Backlog"
            className="w-full rounded-lg border border-dashed border-[var(--border)] px-2 py-1.5 text-xs text-[var(--muted)] hover:border-[var(--accent)] hover:text-[var(--accent)]"
          >
            + Ny ticket
          </button>
        )}
        {groups.backlog.length === 0 ? (
          <Empty text="Ingen tickets i Backlog" />
        ) : (
          <>
            <p className="text-[11px] text-[var(--muted)]">
              Træk en note til en plads, eller brug Tildel…
            </p>
            {groups.backlog.map((t) => (
              <StickyNote key={t.id} ticket={t} agent={agentOf(t)} draggable onNotice={setNotice} />
            ))}
          </>
        )}
        {atAgents > 0 && (
          <p className="text-[11px] text-[var(--muted)]">
            {atAgents} {atAgents === 1 ? "ticket ligger" : "tickets ligger"} hos agenterne — se køen
            under hver terminal.
          </p>
        )}
      </Section>

      <details className="group">
        <summary className="cursor-pointer select-none text-[11px] font-semibold uppercase tracking-wide text-[var(--muted)] hover:text-[var(--fg)]">
          Done ({groups.done.length})
        </summary>
        <div className="mt-2 space-y-2">
          {groups.done.length === 0 ? (
            <Empty text="Ingen færdige tickets endnu" />
          ) : (
            <>
              {doneShown.map((t) => (
                <StickyNote key={t.id} ticket={t} agent={agentOf(t)} draggable={false} />
              ))}
              {doneHidden > 0 && (
                <p className="text-[11px] text-[var(--muted)]">og {doneHidden} flere</p>
              )}
            </>
          )}
        </div>
      </details>
    </div>
  );
}
