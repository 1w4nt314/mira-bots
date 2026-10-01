import { useEffect, useState } from "react";
import type { Theme } from "../../lib/bots";
import { errorMessage, openAgentFolder, removeAgent, stopAgent } from "../../lib/ipc";
import { effortLabel, modelLabel } from "../../lib/models";
import { rolesText } from "../../lib/roles";
import { isExited, isStartingHint, statusLabel } from "../../lib/status";
import type { AgentInfo, BotState } from "../../lib/types";
import { useStore } from "../../state/store";
import AgentTerminal from "../AgentTerminal";
import BotFigure from "../BotFigure";
import AgentReviews from "./AgentReviews";
import AgentSwitch from "./AgentSwitch";
import TicketQueue from "./tickets/TicketQueue";

interface Props {
  agent: AgentInfo;
  botState: BotState;
  theme: Theme;
  /** Called after the agent was removed, so the selection is cleared. */
  onRemoved: () => void;
}

export default function TerminalPanel({ agent, botState, theme, onRemoved }: Props) {
  const { dispatch } = useStore();
  const [confirmStop, setConfirmStop] = useState(false);
  const exited = isExited(agent);

  // The stop confirmation falls back after a moment (like the island's quit button).
  useEffect(() => {
    if (!confirmStop) return;
    const t = setTimeout(() => setConfirmStop(false), 3000);
    return () => clearTimeout(t);
  }, [confirmStop]);

  // Another agent: start without a pending confirmation.
  useEffect(() => setConfirmStop(false), [agent.id]);

  const run = async (action: () => Promise<void>) => {
    try {
      await action();
    } catch (e) {
      dispatch({ type: "error/set", error: errorMessage(e) });
    }
  };

  const stop = () => {
    if (!confirmStop) {
      setConfirmStop(true);
      return;
    }
    setConfirmStop(false);
    void run(() => stopAgent(agent.id));
  };

  const remove = () =>
    void run(async () => {
      await removeAgent(agent.id);
      onRemoved();
    });

  const label = statusLabel(agent.status);
  const btn =
    "shrink-0 rounded-md border border-[var(--border)] px-2.5 py-1 text-xs hover:border-[var(--accent)]";

  return (
    <div className="flex min-h-0 flex-1 flex-col border-t border-[var(--border)]">
      <div className="flex shrink-0 items-center gap-3 px-3 py-2">
        <BotFigure
          roles={agent.roles}
          specialist={agent.specialist}
          state={botState}
          theme={theme}
          exited={exited}
          size={28}
          badge={false}
        />
        <div className="min-w-0 flex-1 leading-tight">
          <div className="flex items-baseline gap-2">
            <span className="font-medium">{agent.name}</span>
            <span className="truncate text-xs text-[var(--muted)]">
              {label}
              {agent.detail ? ` · ${agent.detail}` : ""}
            </span>
          </div>
          <div className="truncate text-[11px] text-[var(--muted)]">
            {agent.profileName} · {rolesText(agent.roles)}
            <span title={agent.modelObserved ? "Rapporteret af Claude Code (statuslinje)" : "Som startet"}>
              {" "}
              · Model: {modelLabel(agent.model)} · Effort: {effortLabel(agent.effort)}
              {agent.modelObserved && " (observeret)"}
            </span>
          </div>
          <div className="truncate font-mono text-[11px] text-[var(--muted)]" title={agent.cwd}>
            {agent.cwd}
          </div>
        </div>
        {!exited && <AgentSwitch agent={agent} />}
        {!exited && (
          <button
            type="button"
            onClick={stop}
            title="Stop agenten (klik igen for at bekræfte)"
            aria-label={`Stop ${agent.name}`}
            className={`${btn} ${confirmStop ? "border-rose-500 text-rose-500" : ""}`}
          >
            {confirmStop ? "Sikker?" : "Stop"}
          </button>
        )}
        {exited && (
          <button
            type="button"
            onClick={remove}
            title="Fjern agenten fra listen"
            aria-label={`Fjern ${agent.name}`}
            className={btn}
          >
            Fjern
          </button>
        )}
        <button
          type="button"
          onClick={() => void run(() => openAgentFolder(agent.id))}
          title="Åbn agentens mappe i Stifinder"
          aria-label={`Åbn mappen for ${agent.name}`}
          className={btn}
        >
          Åbn mappe
        </button>
      </div>
      <TicketQueue agent={agent} />
      <AgentReviews agent={agent} />
      {isStartingHint(agent) && (
        <div className="mx-3 mb-2 shrink-0 rounded-lg border border-amber-400/50 bg-amber-300/20 px-3 py-1.5 text-xs text-amber-800 dark:text-amber-200">
          Agenten venter på et svar i terminalen — fx 'Do you trust the files in this folder?'.
          Svar her.
        </div>
      )}
      <AgentTerminal key={agent.id} agentId={agent.id} exited={exited} />
    </div>
  );
}
