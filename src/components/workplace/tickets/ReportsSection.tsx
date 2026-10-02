import { useEffect, useState } from "react";
import { useTheme } from "../../../lib/bots";
import { addReport, errorMessage, getReport, getTicket, openReportDir } from "../../../lib/ipc";
import { REPORT_BODY_MAX, REPORT_TITLE_MAX } from "../../../lib/models";
import { openFolderTitle } from "../../../lib/platform";
import { isExited } from "../../../lib/status";
import { formatAt } from "../../../lib/tickets";
import type { TicketReport, TicketSummary } from "../../../lib/types";
import { useStore } from "../../../state/store";
import BotFigure from "../../BotFigure";
import Markdown from "../../Markdown";
import { smallBtn, useRun } from "./actions";

const input =
  "mt-0.5 block w-full rounded border border-[var(--note-border)] bg-[var(--bg)] p-1 text-[11px] text-[var(--fg)] outline-none focus:border-[var(--accent)]";

/** Author of a report: the agent's figure and name, "dig" for the user. */
function Author({ report }: { report: TicketReport }) {
  const { state } = useStore();
  const theme = useTheme();
  if (report.author.kind === "user") return <span className="opacity-80">dig</span>;
  const agent = state.agents.find((a) => a.id === report.author.agentId);
  if (agent === undefined) return <span className="opacity-70">agent (findes ikke længere)</span>;
  return (
    <span className="inline-flex min-w-0 items-center gap-1">
      <BotFigure
        roles={agent.roles}
        specialist={agent.specialist}
        state="idle"
        theme={theme}
        exited={isExited(agent)}
        size={16}
        badge={false}
      />
      <span className="truncate">{agent.name}</span>
    </span>
  );
}

/** One report: a header line; unfolding fetches its text with `getReport` and renders it. */
function ReportItem({ ticketId, report }: { ticketId: string; report: TicketReport }) {
  const [open, setOpen] = useState(false);
  const [body, setBody] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!open || body !== null) return;
    let alive = true;
    getReport(ticketId, report.id)
      .then((c) => {
        if (alive) setBody(c.body);
      })
      .catch((e: unknown) => {
        if (alive) setError(errorMessage(e));
      });
    return () => {
      alive = false;
    };
  }, [open, body, ticketId, report.id]);

  return (
    <li>
      <details onToggle={(e) => setOpen(e.currentTarget.open)}>
        <summary className="flex cursor-pointer select-none items-center gap-1.5 text-[11px]">
          <span className="font-mono opacity-60">{report.id}</span>
          <span className="min-w-0 flex-1 truncate font-medium" title={report.title}>
            {report.title}
          </span>
          <Author report={report} />
          <span className="shrink-0 font-mono opacity-60">{formatAt(report.createdAt)}</span>
        </summary>
        <div className="mt-1 rounded border border-[var(--note-border)] bg-[var(--bg)] p-2 text-[var(--fg)]">
          {error !== null ? (
            <p className="text-[11px] text-rose-500" role="alert">
              {error}
            </p>
          ) : body === null ? (
            <p className="text-[11px] opacity-70">Henter…</p>
          ) : (
            <Markdown text={body} />
          )}
        </div>
      </details>
    </li>
  );
}

/** "Tilføj note": the user adds a report of their own (`addReport`, author "dig"). */
function AddReportForm({ ticketId, onDone }: { ticketId: string; onDone: () => void }) {
  const run = useRun();
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const [busy, setBusy] = useState(false);
  const ok = title.trim() !== "" && body.trim() !== "" && [...body].length <= REPORT_BODY_MAX;

  const save = async () => {
    if (!ok) return;
    setBusy(true);
    const saved = await run(() => addReport(ticketId, title.trim(), body));
    setBusy(false);
    if (saved) onDone();
  };

  return (
    <div className="space-y-1 rounded border border-[var(--note-border)] p-1.5">
      <label className="block">
        <span className="text-[11px] opacity-80">Titel</span>
        <input
          type="text"
          value={title}
          onChange={(e) => setTitle(e.target.value)}
          maxLength={REPORT_TITLE_MAX}
          autoFocus
          className={input}
        />
      </label>
      <label className="block">
        <span className="text-[11px] opacity-80">Tekst (markdown)</span>
        <textarea
          value={body}
          onChange={(e) => setBody(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
              e.preventDefault();
              void save();
            }
          }}
          rows={4}
          className={`${input} resize-y`}
        />
      </label>
      <div className="flex gap-1.5">
        <button type="button" onClick={() => void save()} disabled={busy || !ok} title="Gem noten (Ctrl+Enter)" className={smallBtn}>
          Gem note
        </button>
        <button type="button" onClick={onDone} disabled={busy} className={smallBtn}>
          Annuller
        </button>
      </div>
    </div>
  );
}

interface Props {
  ticket: TicketSummary;
  /** Unfolded from the start (the review card); folded on ordinary notes. */
  defaultOpen?: boolean;
}

/**
 * "Rapporter (n)" on a ticket: the report list comes from `getTicket` while unfolded (refetched
 * when the ticket changes), each report's text from `getReport` when it is opened. "Tilføj note"
 * adds a report as the user; "Åbn mappe" opens the ticket's report folder.
 */
// TODO(windows-verify): "Rapporter (1)" on the note, unfolding renders the markdown with æøå,
// "Åbn mappe" opens the file manager in %APPDATA%\dk.mira.bots\tickets\<id>\reports (plan D.58).
export default function ReportsSection({ ticket: t, defaultOpen = false }: Props) {
  const run = useRun();
  const [open, setOpen] = useState(defaultOpen);
  const [reports, setReports] = useState<TicketReport[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);

  useEffect(() => {
    if (!open) return;
    let alive = true;
    getTicket(t.id)
      .then((full) => {
        if (!alive) return;
        setReports(full.reports);
        setError(null);
      })
      .catch((e: unknown) => {
        if (alive) setError(errorMessage(e));
      });
    return () => {
      alive = false;
    };
  }, [open, t.id, t.reportCount, t.updatedAt]);

  return (
    <details className="mt-1.5" open={defaultOpen} onToggle={(e) => setOpen(e.currentTarget.open)}>
      <summary className="cursor-pointer select-none text-[11px] opacity-70 hover:opacity-100">
        Rapporter ({t.reportCount})
      </summary>
      <div className="mt-1 space-y-1.5">
        {error !== null && (
          <p className="text-[11px] text-rose-600 dark:text-rose-300" role="alert">
            {error}
          </p>
        )}
        {reports === null
          ? error === null && <p className="text-[11px] opacity-70">Henter…</p>
          : reports.length === 0
            ? <p className="text-[11px] opacity-70">Ingen rapporter endnu</p>
            : (
                <ul className="space-y-1">
                  {reports.map((r) => (
                    <ReportItem key={r.id} ticketId={t.id} report={r} />
                  ))}
                </ul>
              )}
        {adding ? (
          <AddReportForm ticketId={t.id} onDone={() => setAdding(false)} />
        ) : (
          <div className="flex flex-wrap gap-1.5">
            <button type="button" onClick={() => setAdding(true)} title="Læg en note (rapport) på ticketen som dig" className={smallBtn}>
              Tilføj note
            </button>
            <button
              type="button"
              onClick={() => void run(() => openReportDir(t.id))}
              title={openFolderTitle("ticketens rapportmappe")}
              className={smallBtn}
            >
              Åbn mappe
            </button>
          </div>
        )}
      </div>
    </details>
  );
}
