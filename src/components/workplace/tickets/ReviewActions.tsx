import { useState } from "react";
import { approveTicket, rejectTicket } from "../../../lib/ipc";
import type { AgentInfo, TicketSummary } from "../../../lib/types";
import { smallBtn, useRun, useTicketActions } from "./actions";

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

/**
 * Review of a finished ticket: the agent's summary above the buttons, "Godkend" (→ Done),
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
    </div>
  );
}
