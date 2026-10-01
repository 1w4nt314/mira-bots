import { useEffect, useRef, useState } from "react";
import { useTheme } from "../../../lib/bots";
import { approveTicket, assignReviewer, rejectTicket } from "../../../lib/ipc";
import { rolesText } from "../../../lib/roles";
import { isExited } from "../../../lib/status";
import { reviewerCandidates, reviewRoundText } from "../../../lib/tickets";
import type { AgentInfo, TicketSummary } from "../../../lib/types";
import { useStore } from "../../../state/store";
import BotFigure from "../../BotFigure";
import { smallBtn, useRun, useTicketActions } from "./actions";
import ReportsSection from "./ReportsSection";

interface Props {
  ticket: TicketSummary;
  agent: AgentInfo | null;
  onNotice?: (text: string) => void;
}

/** Above this the summary starts clamped with a "vis mere" toggle (about six lines). */
const SUMMARY_CLAMP_CHARS = 300;

/** The agent's `summary` from `mira_submit_for_review`, or a note that it was moved by hand. */
function AgentSummary({ summary }: { summary: string | null }) {
  const [expanded, setExpanded] = useState(false);
  if (summary === null) {
    return <p className="text-[11px] italic opacity-70">(ingen opsummering — flyttet manuelt)</p>;
  }
  const long = summary.length > SUMMARY_CLAMP_CHARS || summary.split("\n").length > 6;
  return (
    <div>
      <div className="text-[11px] font-medium opacity-80">Agentens opsummering:</div>
      <p
        className={`mt-0.5 whitespace-pre-wrap break-words text-[11px] ${
          long && !expanded ? "line-clamp-6" : ""
        }`}
      >
        {summary}
      </p>
      {long && (
        <button
          type="button"
          onClick={() => setExpanded((e) => !e)}
          aria-expanded={expanded}
          className="mt-0.5 text-[11px] underline opacity-70 hover:opacity-100"
        >
          {expanded ? "vis mindre" : "vis mere"}
        </button>
      )}
    </div>
  );
}

/** "Vælg reviewer…": a small menu of the running reviewer agents that may review the ticket. */
function ReviewerMenu({ ticket }: { ticket: TicketSummary }) {
  const { state } = useStore();
  const run = useRun();
  const theme = useTheme();
  const [open, setOpen] = useState(false);
  const box = useRef<HTMLDivElement>(null);
  const candidates = reviewerCandidates(state.agents, ticket);

  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (box.current !== null && !box.current.contains(e.target as Node)) setOpen(false);
    };
    window.addEventListener("mousedown", onDown);
    return () => window.removeEventListener("mousedown", onDown);
  }, [open]);

  return (
    <div ref={box} className="relative">
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        disabled={candidates.length === 0}
        aria-expanded={open}
        title={
          candidates.length === 0
            ? "Ingen anden kørende agent med reviewer-rollen"
            : "Vælg hvilken reviewer-agent der skal reviewe ticketen"
        }
        className={smallBtn}
      >
        Vælg reviewer…
      </button>
      {open && (
        <ul
          role="menu"
          className="absolute left-0 top-full z-30 mt-1 w-[220px] space-y-0.5 rounded-lg border border-[var(--border)] bg-[var(--panel)] p-1 text-[11px] text-[var(--fg)] shadow-lg"
        >
          {candidates.map((a) => (
            <li key={a.id}>
              <button
                type="button"
                role="menuitem"
                onClick={() => {
                  setOpen(false);
                  void run(() => assignReviewer(ticket.id, a.id));
                }}
                title={`${a.profileName} · ${rolesText(a.roles)}`}
                className="flex w-full items-center gap-1.5 rounded px-1.5 py-1 text-left hover:bg-[var(--accent)]/15"
              >
                <BotFigure roles={a.roles} specialist={a.specialist} state="idle" theme={theme} size={18} badge={false} />
                <span className="min-w-0 flex-1 truncate">{a.name}</span>
                <span className="shrink-0 opacity-60">{a.openReviews} åbne</span>
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

/** Who reviews the ticket: the reviewer agent's figure and name, or "venter på dig". */
function ReviewerLine({ ticket }: { ticket: TicketSummary }) {
  const { state } = useStore();
  const theme = useTheme();
  const reviewer =
    ticket.reviewerAgentId === null ? null : state.agents.find((a) => a.id === ticket.reviewerAgentId);
  return (
    <div className="flex flex-wrap items-center gap-1.5 text-[11px]">
      <span className="opacity-80">Reviewer:</span>
      {ticket.reviewerAgentId === null ? (
        <span className="font-medium">venter på dig</span>
      ) : reviewer === undefined || reviewer === null ? (
        <span className="opacity-70">reviewer findes ikke længere</span>
      ) : (
        <span className="inline-flex min-w-0 items-center gap-1" title={`${reviewer.profileName} · ${rolesText(reviewer.roles)}`}>
          <BotFigure
            roles={reviewer.roles}
            specialist={reviewer.specialist}
            state="idle"
            theme={theme}
            exited={isExited(reviewer)}
            size={18}
            badge={false}
          />
          <span className="truncate font-medium">{reviewer.name}</span>
        </span>
      )}
      <span className="opacity-70">· {reviewRoundText(ticket)}</span>
      {ticket.escalated && (
        <span
          className="rounded bg-rose-500/20 px-1 text-[10px] font-medium text-rose-700 dark:text-rose-300"
          title="3 afvisninger — afgør selv"
        >
          Eskaleret til dig
        </span>
      )}
    </div>
  );
}

/**
 * Review of a finished ticket: the reviewer (agent or "venter på dig"), the round of 3 and the
 * "Eskaleret til dig" badge, "Vælg reviewer…" / "Fjern reviewer", the agent's summary above the
 * buttons, "Godkend" (→ Done),
 * "Afvis…" with a required note (the ticket goes back first in the agent's queue, or to the
 * backlog when the agent no longer runs; it leaves the Review list either way), and "Åbn
 * terminal" to look at the agent's work.
 */
export default function ReviewActions({ ticket, agent, onNotice }: Props) {
  const run = useRun();
  const { selectAgent } = useTicketActions();
  const [rejecting, setRejecting] = useState(false);
  const [note, setNote] = useState("");
  const [busy, setBusy] = useState(false);

  const approve = async () => {
    setBusy(true);
    if (await run(() => approveTicket(ticket.id))) onNotice?.(`Ticket ${ticket.shortId} er godkendt og ligger i Done`);
    setBusy(false);
  };

  const reject = async () => {
    const text = note.trim();
    if (text === "") return;
    setBusy(true);
    const box: { after: TicketSummary | null } = { after: null };
    const ok = await run(async () => {
      box.after = await rejectTicket(ticket.id, text);
    });
    setBusy(false);
    const after = box.after;
    if (!ok || after === null) return;
    setRejecting(false);
    setNote("");
    onNotice?.(
      after.state === "assigned"
        ? `Ticket ${ticket.shortId} er afvist og sendt tilbage forrest i køen hos ${agent?.name ?? "agenten"}`
        : `Ticket ${ticket.shortId} er afvist; agenten kører ikke, så den ligger nu i Backlog`,
    );
  };

  return (
    <div className="mt-1.5 space-y-1.5">
      <ReviewerLine ticket={ticket} />
      <AgentSummary summary={ticket.summary} />
      <div className="flex flex-wrap gap-1.5">
        <button
          type="button"
          onClick={() => void approve()}
          disabled={busy}
          title="Godkend arbejdet og flyt ticketen til Done"
          className={`${smallBtn} font-medium`}
        >
          Godkend
        </button>
        <button
          type="button"
          onClick={() => setRejecting((r) => !r)}
          disabled={busy}
          aria-expanded={rejecting}
          title="Send ticketen tilbage til agenten med en note"
          className={smallBtn}
        >
          Afvis…
        </button>
        <button
          type="button"
          onClick={() => ticket.assigneeAgentId !== null && selectAgent(ticket.assigneeAgentId)}
          disabled={agent === null}
          title={agent === null ? "Agenten findes ikke længere" : `Vis terminalen for ${agent.name}`}
          className={smallBtn}
        >
          Åbn terminal
        </button>
        <ReviewerMenu ticket={ticket} />
        {ticket.reviewerAgentId !== null && (
          <button
            type="button"
            onClick={() => void run(() => assignReviewer(ticket.id, null))}
            disabled={busy}
            title="Fjern revieweren; appen finder en anden reviewer, hvis der er en, ellers venter ticketen på dig"
            className={smallBtn}
          >
            Fjern reviewer
          </button>
        )}
      </div>
      {rejecting && (
        <div className="space-y-1">
          <label className="block">
            <span className="text-[11px] opacity-80">Hvad mangler? (påkrævet)</span>
            <textarea
              value={note}
              onChange={(e) => setNote(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Escape") {
                  e.preventDefault();
                  setRejecting(false);
                } else if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
                  e.preventDefault();
                  void reject();
                }
              }}
              rows={2}
              autoFocus
              className="mt-0.5 block w-full resize-y rounded border border-[var(--note-border)] bg-[var(--bg)] p-1 text-[11px] text-[var(--fg)] outline-none focus:border-[var(--accent)]"
            />
          </label>
          <div className="flex gap-1.5">
            <button
              type="button"
              onClick={() => void reject()}
              disabled={busy || note.trim() === ""}
              title="Afvis med noten (Ctrl+Enter)"
              className={smallBtn}
            >
              Send tilbage
            </button>
            <button
              type="button"
              onClick={() => setRejecting(false)}
              disabled={busy}
              title="Fortryd afvisningen"
              className={smallBtn}
            >
              Annuller
            </button>
          </div>
        </div>
      )}
      <ReportsSection ticket={ticket} defaultOpen />
    </div>
  );
}
