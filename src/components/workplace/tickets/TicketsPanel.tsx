import { useEffect, useMemo, useState, type ReactNode } from "react";
import { getDiagnostics } from "../../../lib/ipc";
import { readLocal, writeLocal } from "../../../lib/persist";
import {
  backlogHint,
  matchesFilter,
  parseProjectFilter,
  PROJECT_FILTER_KEY,
  projectFilterKey,
  projectIdOf,
  projectName,
  type BacklogHint,
  type ProjectFilter,
} from "../../../lib/projects";
import { DONE_VISIBLE, groupTickets } from "../../../lib/tickets";
import type { AgentInfo, TicketSummary } from "../../../lib/types";
import { useStore } from "../../../state/store";
import NewTicketForm from "./NewTicketForm";
import { useTicketActions } from "./actions";
import InboxSection, { DismissedFold } from "./InboxSection";
import StickyNote from "./StickyNote";

const NOTICE_VISIBLE_MS = 5000;
/** How long a note selected from a notice keeps its ring (step 6d). */
const HIGHLIGHT_MS = 2000;
// The nonce of the last `selectTicket` this window acted on. Module-level on purpose: the panel
// unmounts with its tab, and a remount must not scroll to an old selection again.
let handledNonce = 0;

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
 * The sidebar's Tickets tab: Indbakke (new items from files and GitHub, step 6c), Review (only when something waits), Backlog with "Ny ticket", and
 * a folded Done list. Queued and running tickets are shown at the seats and under the terminal.
 * Backlog notes are draggable onto seats (the DndContext lives in Workplace).
 */
export default function TicketsPanel() {
  const { state } = useStore();
  const { assignTo, selectAgent, selectedTicket } = useTicketActions();
  // Step 4b: the project filter (remembered) applies to Review, Backlog and Done.
  const [filter, setFilter] = useState<ProjectFilter>(() =>
    parseProjectFilter(readLocal(PROJECT_FILTER_KEY)),
  );
  useEffect(() => writeLocal(PROJECT_FILTER_KEY, projectFilterKey(filter)), [filter]);
  const [doneOpen, setDoneOpen] = useState(false);
  const [highlight, setHighlight] = useState<{ id: string; nonce: number } | null>(null);

  // Step 6d: a ticket chosen from a notice or the island's badge (`selectTicket`). The filter is
  // lifted when it hides the ticket, the Done fold opens for a finished one, a ticket that sits at
  // an agent also selects that agent (its note is under the terminal), and the note gets a ring.
  useEffect(() => {
    if (selectedTicket === null || selectedTicket.nonce === handledNonce) return;
    const t = state.tickets.find((x) => x.id === selectedTicket.id);
    if (t === undefined) return; // the list may still be loading; tried again when it changes
    handledNonce = selectedTicket.nonce;
    if (!matchesFilter(t, filter)) setFilter("all");
    if (t.state === "done") setDoneOpen(true);
    const atAgent = t.state === "assigned" || t.state === "inProgress" || t.state === "rejected";
    if (atAgent && t.assigneeAgentId !== null) selectAgent(t.assigneeAgentId);
    setHighlight({ id: t.id, nonce: selectedTicket.nonce });
  }, [selectedTicket, state.tickets, filter, selectAgent]);

  useEffect(() => {
    if (highlight === null) return;
    // Next frame: the lifted filter / opened fold has rendered the note by then.
    const raf = requestAnimationFrame(() => {
      document.getElementById(`ticket-${highlight.id}`)?.scrollIntoView({ block: "center" });
    });
    const t = setTimeout(() => setHighlight(null), HIGHLIGHT_MS);
    return () => {
      cancelAnimationFrame(raf);
      clearTimeout(t);
    };
  }, [highlight]);
  const groups = useMemo(() => groupTickets(state.tickets), [state.tickets]);
  const shown = useMemo(
    () => ({
      review: groups.review.filter((t) => matchesFilter(t, filter)),
      // Parents waiting for their children (step 6a), oldest first across all agents.
      waiting: [...groups.byAgent.values()]
        .flatMap((s) => s.waiting)
        .filter((t) => matchesFilter(t, filter))
        .sort((a, b) => a.updatedAt - b.updatedAt),
      backlog: groups.backlog.filter((t) => matchesFilter(t, filter)),
      done: groups.done.filter((t) => matchesFilter(t, filter)),
    }),
    [groups, filter],
  );
  // Banner at the top of Backlog: an idle work agent and unowned tickets in the same project.
  // One project filtered: that project; "all": the first project with a hint; "none": no banner.
  const hint = useMemo((): BacklogHint | null => {
    if (filter === "none") return null;
    if (filter !== "all") return backlogHint(state.agents, state.tickets, filter.id);
    const seen = new Set<string>();
    for (const t of groups.backlog) {
      const id = projectName(t.project);
      if (id === null || seen.has(id.toLowerCase())) continue;
      seen.add(id.toLowerCase());
      const h = backlogHint(state.agents, state.tickets, id);
      if (h !== null) return h;
    }
    return null;
  }, [filter, state.agents, state.tickets, groups.backlog]);
  // The projects on disk plus those named by tickets (deleted folders, other spellings).
  const filterIds = useMemo(() => {
    const ids = new Map<string, string>();
    for (const p of state.projects) ids.set(p.id.toLowerCase(), p.id);
    for (const t of state.tickets) {
      const id = projectIdOf(t.project);
      if (id !== null && !ids.has(id.toLowerCase())) ids.set(id.toLowerCase(), id);
    }
    if (typeof filter !== "string" && !ids.has(filter.id.toLowerCase())) {
      ids.set(filter.id.toLowerCase(), filter.id);
    }
    return [...ids.values()].sort((a, b) => a.toLowerCase().localeCompare(b.toLowerCase()));
  }, [state.projects, state.tickets, filter]);
  const inProject = filter === "all" ? "" : " i dette projekt";
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

  const doneShown = shown.done.slice(0, DONE_VISIBLE);
  const doneHidden = shown.done.length - doneShown.length;
  const lit = (t: TicketSummary) => highlight !== null && highlight.id === t.id;

  return (
    <div className="office-cork space-y-4 p-3">
      <label className="flex items-center gap-2 text-xs">
        <span className="text-[var(--muted)]">Projekt</span>
        <select
          value={projectFilterKey(filter)}
          onChange={(e) => setFilter(parseProjectFilter(e.target.value))}
          className="min-w-0 flex-1 rounded-md border border-[var(--border)] bg-[var(--bg)] p-1 text-xs outline-none focus:border-[var(--accent)]"
        >
          <option value="all">Alle projekter</option>
          <option value="none">Uden projekt</option>
          {filterIds.map((id) => (
            <option key={id} value={projectFilterKey({ id })}>
              {id}
            </option>
          ))}
        </select>
      </label>
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

      <InboxSection filter={filter} onNotice={setNotice} />

      {shown.review.length > 0 && (
        <Section title="Review" count={shown.review.length}>
          {shown.review.map((t) => (
            <StickyNote key={t.id} ticket={t} agent={agentOf(t)} draggable={false} onNotice={setNotice} highlight={lit(t)} />
          ))}
        </Section>
      )}

      {shown.waiting.length > 0 && (
        <Section title="Venter" count={shown.waiting.length}>
          {shown.waiting.map((t) => (
            <StickyNote key={t.id} ticket={t} agent={agentOf(t)} draggable={false} onNotice={setNotice} highlight={lit(t)} />
          ))}
        </Section>
      )}

      <Section title="Backlog" count={shown.backlog.length}>
        {hint !== null && (
          <div
            className="flex items-center gap-2 rounded-lg border border-amber-400/50 bg-amber-300/20 px-2 py-1 text-xs text-amber-800 dark:text-amber-200"
            role="status"
          >
            <span className="min-w-0 flex-1">{hint.text}</span>
            <button
              type="button"
              onClick={() => assignTo(hint.next, hint.agent)}
              title={`Tildeler den ældste leverbare ticket (${hint.next.shortId}) til ${hint.agent.name}`}
              className="shrink-0 rounded-md border border-amber-500/60 px-2 py-0.5 text-[11px] hover:border-[var(--accent)]"
            >
              Tildel til {hint.agent.name}
            </button>
          </div>
        )}
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
        {shown.backlog.length === 0 ? (
          <Empty text={`Ingen tickets i Backlog${inProject}`} />
        ) : (
          <>
            <p className="text-[11px] text-[var(--muted)]">
              Træk en note til en plads, eller brug Tildel…
            </p>
            {shown.backlog.map((t) => (
              <StickyNote key={t.id} ticket={t} agent={agentOf(t)} draggable onNotice={setNotice} highlight={lit(t)} />
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

      <details className="group" open={doneOpen} onToggle={(e) => setDoneOpen(e.currentTarget.open)}>
        <summary className="cursor-pointer select-none text-[11px] font-semibold uppercase tracking-wide text-[var(--muted)] hover:text-[var(--fg)]">
          Done ({shown.done.length})
        </summary>
        <div className="mt-2 space-y-2">
          {shown.done.length === 0 ? (
            <Empty text={filter === "all" ? "Ingen færdige tickets endnu" : `Ingen færdige tickets${inProject}`} />
          ) : (
            <>
              {doneShown.map((t) => (
                <StickyNote key={t.id} ticket={t} agent={agentOf(t)} draggable={false} highlight={lit(t)} />
              ))}
              {doneHidden > 0 && (
                <p className="text-[11px] text-[var(--muted)]">og {doneHidden} flere</p>
              )}
            </>
          )}
        </div>
      </details>

      <DismissedFold filter={filter} />
    </div>
  );
}
