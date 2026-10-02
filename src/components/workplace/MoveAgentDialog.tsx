import { useState } from "react";
import { errorMessage, moveAgentToProject } from "../../lib/ipc";
import { projectName, sameProjectId } from "../../lib/projects";
import { movePendingText, switchBlocked, waitingCount } from "../../lib/tickets";
import type { AgentInfo, ProjectRef } from "../../lib/types";
import { useRefreshProjects, useStore } from "../../state/store";
import { DialogFrame } from "./ProjectPrompt";
import ProjectPicker from "./ProjectPicker";

interface Props {
  agent: AgentInfo;
  /** Preselected target (from "Flyt agenten til «p»"); otherwise nothing is chosen. */
  preselect?: ProjectRef | null;
  onClose: () => void;
  onMoved: (a: AgentInfo) => void;
}

// TODO(windows-verify): the agent restarts with --resume in the new folder, its conversation is
// kept, a git project asks for trust in the terminal, and a queue goes back to the backlog only
// after the confirmation (plan4b D.81).
/**
 * "Flyt til projekt…" (plan4b A.3): restarts a work agent in another project's folder with
 * `--resume`. Only while it is idle without a ticket in progress; queued tickets and waiting
 * parents (review 6a W2: they would otherwise be woken in the new project) go back to the backlog
 * when the user confirms it.
 */
export default function MoveAgentDialog({ agent, preselect = null, onClose, onMoved }: Props) {
  const refreshProjects = useRefreshProjects();
  const { state } = useStore();
  const [value, setValue] = useState<ProjectRef | null>(
    preselect !== null && !sameProjectId(projectName(preselect), agent.project) ? preselect : null,
  );
  const [force, setForce] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const queue = agent.queueLength;
  const waiting = waitingCount(state.tickets, agent.id);
  const pendingText = movePendingText(queue, waiting, agent.project);
  const blocked = switchBlocked(agent);
  const canMove = !busy && blocked === null && value !== null && (pendingText === null || force);

  const move = async () => {
    if (value === null) return;
    setBusy(true);
    setError(null);
    try {
      const moved = await moveAgentToProject(agent.id, value, pendingText !== null && force);
      void refreshProjects();
      onMoved(moved);
    } catch (e) {
      setError(errorMessage(e));
      setBusy(false);
    }
  };

  return (
    <DialogFrame
      titleId="move-agent-title"
      title={`Flyt ${agent.name} til et andet projekt`}
      busy={busy}
      onClose={onClose}
    >
      <p className="mt-1 text-xs text-[var(--muted)]">
        Agenten genstartes i den nye mappe med sin samtale (--resume). Er projektet et git-repo, skal
        du godkende mappen i terminalen første gang.
      </p>
      {agent.project !== null && (
        <p className="mt-1 text-xs">Nu: projekt «{agent.project}»</p>
      )}
      <div className="mt-3">
        <ProjectPicker
          value={value}
          onChange={setValue}
          allowLater={false}
          allowNew
          exclude={agent.project}
          autoFocus
          label="Nyt projekt for agenten"
        />
      </div>
      {pendingText !== null && (
        <div className="mt-2 rounded-lg border border-amber-400/50 bg-amber-300/20 px-2 py-1 text-xs text-amber-800 dark:text-amber-200">
          <p>{pendingText}</p>
          <label className="mt-1 flex items-center gap-2">
            <input type="checkbox" checked={force} onChange={(e) => setForce(e.target.checked)} />
            <span>Ja, læg dem i Backlog</span>
          </label>
        </div>
      )}
      {blocked !== null && (
        <p className="mt-2 text-xs text-[var(--muted)]" role="status">
          {blocked}
        </p>
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
          onClick={() => void move()}
          disabled={!canMove}
          title="Genstart agenten i projektets mappe"
          className="rounded-md bg-[var(--accent)] px-3 py-1.5 text-sm font-medium text-white hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-50"
        >
          {busy ? "Flytter…" : "Flyt"}
        </button>
      </div>
    </DialogFrame>
  );
}
