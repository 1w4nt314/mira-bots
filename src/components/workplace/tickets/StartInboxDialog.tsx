import { useMemo, useState } from "react";
import {
  duplicateText,
  sourceIdText,
  sourceLine,
  startButtonText,
  type StartTarget,
} from "../../../lib/inbox";
import { errorMessage, startInboxItem } from "../../../lib/ipc";
import { KIND_OPTIONS } from "../../../lib/tickets";
import type { AgentInfo, InboxItemSummary, ProjectRef, SeatKind, TicketSummary } from "../../../lib/types";
import { useRefreshProjects, useStore } from "../../../state/store";
import ProjectPicker from "../ProjectPicker";
import { DialogFrame } from "../ProjectPrompt";
import { useTicketActions } from "./actions";

export type StartInboxTarget =
  | { kind: "agent"; agent: AgentInfo }
  | { kind: "empty"; seatKind: SeatKind };

interface Props {
  item: InboxItemSummary;
  /** Dropped on an agent / an empty seat (step 6c B5): "Start og tildel til …" / "Start og start agent". */
  target?: StartInboxTarget | null;
  onClose: () => void;
  onStarted: (ticket: TicketSummary) => void;
}

/**
 * "Start…" on an inbox item: the type (preselected from the file's `kind:`), the project
 * (preselected from the source; locked to the agent's project on a drop onto an agent), "Spring
 * review over", the source (id with labels) and the backend's duplicate warning. Confirming
 * calls `start_inbox_item`; the item becomes a Backlog ticket and, with a target, is assigned
 * to the agent or starts a new one. Nothing happens before the click.
 */
export default function StartInboxDialog({ item, target = null, onClose, onStarted }: Props) {
  const { state } = useStore();
  const refreshProjects = useRefreshProjects();
  const { assignTo, spawnWithTicket } = useTicketActions();
  const github = item.kind === "github";
  const kindOptions = useMemo(() => KIND_OPTIONS(state.appInfo?.playbookKinds ?? []), [state.appInfo]);
  // A file's `kind:` that is not offered (a playbook removed from the workspace file) → "Opgave".
  const [pickedKind, setKind] = useState<string | null>(item.ticketKind);
  const kind = kindOptions.some((o) => o.value === pickedKind) ? pickedKind : null;
  const lockedProject = target?.kind === "agent" ? target.agent.project : null;
  const [project, setProject] = useState<ProjectRef | null>(lockedProject ?? item.project);
  const [projectIncomplete, setProjectIncomplete] = useState(false);
  const reviewByDefault = state.appInfo?.rules.reviewByDefault ?? true;
  const [skipReview, setSkipReview] = useState(!reviewByDefault);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  let startTarget: StartTarget | null = null;
  if (target?.kind === "agent") startTarget = { kind: "agent", agentName: target.agent.name };
  else if (target?.kind === "empty") startTarget = { kind: "empty", seatKind: target.seatKind };
  const canStart = !busy && project !== null && !projectIncomplete;
  // Projects that share the source's repo and labels (the item has none of its own).
  const candidates = item.project === null && lockedProject === null ? item.candidates : [];

  const start = async () => {
    if (!canStart) return;
    setBusy(true);
    setError(null);
    try {
      const ticket = await startInboxItem({ itemId: item.id, kind, project, skipReview });
      void refreshProjects();
      if (target?.kind === "agent") assignTo(ticket, target.agent);
      else if (target?.kind === "empty") spawnWithTicket(target.seatKind, ticket);
      onStarted(ticket);
    } catch (e) {
      // The backend's text as it is (already started, issue closed on GitHub, …).
      setError(errorMessage(e));
      setBusy(false);
    }
  };

  const field =
    "block w-full rounded-md border border-[var(--border)] bg-[var(--bg)] p-1.5 text-xs outline-none focus:border-[var(--accent)] disabled:opacity-50";

  return (
    <DialogFrame titleId="start-inbox-title" title="Start fra indbakken" busy={busy} onClose={onClose}>
      <div className="mt-2 rounded-lg border border-[var(--border)] bg-[var(--bg)]/50 p-2 text-xs">
        <p className="break-words font-medium" title={item.title}>
          {item.title}
        </p>
        <p className="mt-0.5 text-[var(--muted)]">{sourceLine(item)}</p>
        <p className="text-[11px] text-[var(--muted)]">{sourceIdText(item)}</p>
      </div>
      {item.duplicateOf !== null && (
        <p
          className="mt-2 rounded-lg border border-amber-400/50 bg-amber-300/20 px-2 py-1 text-xs text-amber-800 dark:text-amber-200"
          role="status"
        >
          {duplicateText(item.duplicateOf)}
        </p>
      )}
      {item.notes.length > 0 && (
        <ul className="mt-2 space-y-0.5 text-[11px] text-amber-700 dark:text-amber-300">
          {item.notes.map((n, i) => (
            <li key={`${i}:${n}`}>⚠ {n}</li>
          ))}
        </ul>
      )}
      <label className="mt-3 block text-xs">
        <span className="text-[var(--muted)]">Type</span>
        <select
          value={kind ?? ""}
          onChange={(e) => setKind(e.target.value === "" ? null : e.target.value)}
          disabled={busy}
          className={`mt-0.5 ${field}`}
        >
          {kindOptions.map((o) => (
            <option key={o.value ?? ""} value={o.value ?? ""}>
              {o.label}
            </option>
          ))}
        </select>
      </label>
      <div className="mt-2 text-xs">
        <span className="text-[var(--muted)]">Projekt</span>
        {lockedProject !== null ? (
          <p className="mt-0.5" title="Agentens projekt">
            {lockedProject}
          </p>
        ) : (
          <div className="mt-0.5 space-y-1">
            {candidates.length > 0 && (
              <div className="flex flex-wrap items-center gap-1">
                <span className="text-[11px] text-[var(--muted)]">Kan høre til:</span>
                {candidates.map((c) => (
                  <button
                    key={c}
                    type="button"
                    onClick={() => setProject(c)}
                    disabled={busy}
                    aria-pressed={project === c}
                    className={`rounded-md border px-1.5 py-0.5 text-[11px] disabled:opacity-50 ${
                      project === c
                        ? "border-[var(--accent)] text-[var(--accent)]"
                        : "border-[var(--border)] hover:border-[var(--accent)]"
                    }`}
                  >
                    {c}
                  </button>
                ))}
              </div>
            )}
            <ProjectPicker
              value={project}
              onChange={setProject}
              onIncomplete={setProjectIncomplete}
              allowLater={false}
              allowNew
              disabled={busy}
            />
          </div>
        )}
      </div>
      <label className="mt-2 flex items-center gap-2 text-xs">
        <input type="checkbox" checked={skipReview} onChange={(e) => setSkipReview(e.target.checked)} disabled={busy} />
        <span>Spring review over (går direkte til Done)</span>
      </label>
      {project === null && !busy && (
        <p className="mt-2 text-xs text-[var(--muted)]">Vælg et projekt</p>
      )}
      {error !== null && (
        <p className="mt-2 text-xs text-rose-500" role="alert">
          {error}
        </p>
      )}
      <div className="mt-4 flex justify-end gap-2">
        <button
          type="button"
          onClick={onClose}
          disabled={busy}
          className="rounded-md px-3 py-1.5 text-sm hover:bg-neutral-500/15 disabled:opacity-50"
        >
          Annuller
        </button>
        <button
          type="button"
          onClick={() => void start()}
          disabled={!canStart}
          title="Opret ticketen i Backlog ud fra emnet"
          className="rounded-md bg-[var(--accent)] px-3 py-1.5 text-sm font-medium text-white hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-50"
        >
          {startButtonText(startTarget, busy, github)}
        </button>
      </div>
    </DialogFrame>
  );
}
