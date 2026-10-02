import { useEffect, useState, type ReactNode } from "react";
import { errorMessage } from "../../lib/ipc";
import { projectName, sameProjectId } from "../../lib/projects";
import { switchBlocked } from "../../lib/tickets";
import type { AgentInfo, ProjectRef, TicketSummary } from "../../lib/types";
import ProjectPicker from "./ProjectPicker";

const primaryBtn =
  "rounded-md bg-[var(--accent)] px-3 py-1.5 text-sm font-medium text-white hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-50";
const plainBtn = "rounded-md px-3 py-1.5 text-sm hover:bg-neutral-500/15 disabled:opacity-50";

/** The small modal frame of the project dialogs (like SpawnDialog's, 420 px); Esc closes. */
export function DialogFrame(props: {
  titleId: string;
  title: string;
  busy: boolean;
  onClose: () => void;
  children: ReactNode;
}) {
  const { titleId, title, busy, onClose, children } = props;
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !busy) {
        e.preventDefault();
        onClose();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [busy, onClose]);
  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget && !busy) onClose();
      }}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby={titleId}
        className="w-[420px] max-w-[calc(100vw-32px)] rounded-2xl border border-[var(--border)] bg-[var(--panel)] p-5 text-sm shadow-xl"
      >
        <h2 id={titleId} className="text-base font-semibold">
          {title}
        </h2>
        {children}
      </div>
    </div>
  );
}

/** "Flyt agenten til «p»" (disabled with the reason while the agent cannot be moved). */
function MoveButton(props: { agent: AgentInfo; project: string; onMove: () => void; busy: boolean }) {
  const blocked = switchBlocked(props.agent);
  return (
    <button
      type="button"
      onClick={props.onMove}
      disabled={props.busy || blocked !== null}
      title={blocked ?? `Genstart ${props.agent.name} i projektet «${props.project}» og tildel ticketen bagefter`}
      className={primaryBtn}
    >
      Flyt agenten til «{props.project}»
    </button>
  );
}

interface PromptProps {
  ticket: TicketSummary;
  /** The work agent the ticket goes to (its project is preselected). */
  agent: AgentInfo | null;
  /** Assigns with the picked project; a rejection is shown in the dialog. */
  onPick: (p: ProjectRef) => Promise<void>;
  /** The picked project is not the agent's: move the agent there first (MoveAgentDialog). */
  onMove: (p: ProjectRef) => void;
  onClose: () => void;
}

/**
 * "Hvilket projekt?" (plan4b C4b.9): a ticket without a project going to a work agent. The
 * agent's project is preselected; another project can only be reached by moving the agent.
 */
export default function ProjectPrompt({ ticket, agent, onPick, onMove, onClose }: PromptProps) {
  const [value, setValue] = useState<ProjectRef | null>(agent?.project ?? null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const picked = projectName(value);
  const otherProject =
    agent !== null && agent.seatKind === "work" && picked !== null && !sameProjectId(picked, agent.project);

  const pick = async () => {
    if (value === null) return;
    setBusy(true);
    setError(null);
    try {
      await onPick(value);
    } catch (e) {
      setError(errorMessage(e));
      setBusy(false);
    }
  };

  return (
    <DialogFrame titleId="project-prompt-title" title="Hvilket projekt?" busy={busy} onClose={onClose}>
      <p className="mt-1 text-xs text-[var(--muted)]">
        Ticket {ticket.shortId} har intet projekt. Vælg det projekt, agenten skal arbejde i.
      </p>
      <div className="mt-3">
        <ProjectPicker value={value} onChange={setValue} allowLater={false} allowNew autoFocus />
      </div>
      {otherProject && agent !== null && picked !== null && (
        <p className="mt-2 rounded-lg border border-amber-400/50 bg-amber-300/20 px-2 py-1 text-xs text-amber-800 dark:text-amber-200">
          {agent.name} står i projekt «{agent.project}». En arbejdsagent får kun tickets fra sit eget
          projekt: flyt agenten til «{picked}», eller vælg «{agent.project}».
        </p>
      )}
      {error !== null && (
        <p className="mt-2 text-xs text-rose-500" role="alert">
          {error}
        </p>
      )}
      <div className="mt-4 flex justify-end gap-2">
        <button type="button" onClick={onClose} disabled={busy} className={plainBtn}>
          Annuller
        </button>
        {otherProject && agent !== null && value !== null && picked !== null ? (
          <MoveButton agent={agent} project={picked} busy={busy} onMove={() => onMove(value)} />
        ) : (
          <button
            type="button"
            onClick={() => void pick()}
            disabled={busy || value === null}
            title="Sæt projektet på ticketen og tildel den"
            className={primaryBtn}
          >
            {busy ? "Tildeler…" : "Tildel"}
          </button>
        )}
      </div>
    </DialogFrame>
  );
}

interface WrongProps {
  ticket: TicketSummary;
  agent: AgentInfo;
  /** The ticket's project (id, or the name of a project still to be created). */
  ticketProject: string;
  onMove: () => void;
  onClose: () => void;
}

/** A ticket of another project dropped on (or picked for) a work agent: explain, offer the move. */
export function WrongProjectDialog({ ticket, agent, ticketProject, onMove, onClose }: WrongProps) {
  return (
    <DialogFrame titleId="wrong-project-title" title="Andet projekt" busy={false} onClose={onClose}>
      <p className="mt-1 text-xs">
        {agent.name} står i projekt «{agent.project}»; ticket {ticket.shortId} hører til «
        {ticketProject}». En arbejdsagent får kun tickets fra sit eget projekt.
      </p>
      <p className="mt-1 text-xs text-[var(--muted)]">
        Flyt agenten (den genstarter i projektets mappe med sin samtale), eller vælg en anden agent.
      </p>
      <div className="mt-4 flex justify-end gap-2">
        <button type="button" onClick={onClose} className={plainBtn} autoFocus>
          Annuller
        </button>
        <MoveButton agent={agent} project={ticketProject} busy={false} onMove={onMove} />
      </div>
    </DialogFrame>
  );
}
