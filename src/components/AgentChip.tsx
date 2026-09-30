import { errorMessage, removeAgent, stopAgent } from "../lib/ipc";
import { DOT_CLASS, isExited, statusLabel } from "../lib/status";
import type { AgentInfo } from "../lib/types";
import { useStore } from "../state/store";

export default function AgentChip({ agent }: { agent: AgentInfo }) {
  const { dispatch } = useStore();
  const exited = isExited(agent);

  const act = async () => {
    try {
      await (exited ? removeAgent(agent.id) : stopAgent(agent.id));
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
      <span
        className={`h-2.5 w-2.5 shrink-0 rounded-full ${DOT_CLASS[agent.status.kind]}`}
        aria-hidden="true"
      />
      <div className="min-w-0 flex-1 leading-tight">
        <div className="truncate font-medium">{agent.name}</div>
        <div className="truncate text-[10px] text-neutral-400">
          {label}
          {agent.detail ? ` · ${agent.detail}` : ""}
        </div>
      </div>
      <button
        type="button"
        onClick={act}
        title={exited ? "Fjern agenten fra listen" : "Stop agenten"}
        aria-label={exited ? `Fjern ${agent.name}` : `Stop ${agent.name}`}
        className="shrink-0 rounded px-1 text-[10px] text-neutral-300 opacity-0 hover:bg-white/10 hover:text-white group-hover:opacity-100"
      >
        {exited ? "Fjern" : "×"}
      </button>
    </div>
  );
}
