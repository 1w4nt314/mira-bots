import type { Theme } from "../lib/bots";
import { errorMessage, openWorkplace, removeAgent, stopAgent } from "../lib/ipc";
import { isExited, isStartingHint, statusLabel } from "../lib/status";
import type { AgentInfo, BotState } from "../lib/types";
import { useStore } from "../state/store";
import BotFigure from "./BotFigure";

interface Props {
  agent: AgentInfo;
  theme: Theme;
  /** From `useBotStates` in Island (one timer for all chips). */
  botState: BotState;
}

export default function AgentChip({ agent, theme, botState }: Props) {
  const { dispatch } = useStore();
  const exited = isExited(agent);
  const hint = isStartingHint(agent);

  const run = async (action: () => Promise<void>) => {
    try {
      await action();
    } catch (e) {
      dispatch({ type: "error/set", error: errorMessage(e) });
    }
  };

  const label = statusLabel(agent.status);
  const tooltip = `kører Claude Code i ${agent.cwd}\n${label}${agent.detail ? `: ${agent.detail}` : ""}`;

  return (
    <div
      className="group flex min-w-0 max-w-[220px] flex-1 items-center gap-2 rounded-lg bg-white/5 px-2 py-1"
      title={tooltip}
    >
      <BotFigure
        role={agent.role}
        state={botState}
        theme={theme}
        exited={exited}
        size={22}
        badge={false}
      />
      <div className="min-w-0 flex-1 leading-tight">
        <div className="truncate font-medium">{agent.name}</div>
        <div className="truncate text-[10px] text-neutral-400">
          {label}
          {agent.detail ? ` · ${agent.detail}` : ""}
        </div>
      </div>
      {hint && (
        <button
          type="button"
          onClick={() => void run(() => openWorkplace(agent.id))}
          title="Åbn agentens terminal i Workplace og svar der"
          aria-label={`Åbn terminal for ${agent.name}`}
          className="shrink-0 rounded bg-amber-400/20 px-1.5 py-0.5 text-[10px] text-amber-200 hover:bg-amber-400/30"
        >
          Åbn terminal
        </button>
      )}
      <button
        type="button"
        onClick={() => void run(() => (exited ? removeAgent(agent.id) : stopAgent(agent.id)))}
        title={exited ? "Fjern agenten fra listen" : "Stop agenten"}
        aria-label={exited ? `Fjern ${agent.name}` : `Stop ${agent.name}`}
        className="shrink-0 rounded px-1 text-[10px] text-neutral-300 opacity-0 hover:bg-white/10 hover:text-white group-hover:opacity-100"
      >
        {exited ? "Fjern" : "×"}
      </button>
    </div>
  );
}
