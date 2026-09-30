import type { Theme } from "../../lib/bots";
import { isExited, statusLabel } from "../../lib/status";
import type { AgentInfo, AgentRole, BotState, SeatKind } from "../../lib/types";
import BotFigure from "../BotFigure";

export const ROLE_LABEL: Record<AgentRole, string> = {
  none: "Ingen rolle",
  coder: "Koder",
  researcher: "Researcher",
  reviewer: "Reviewer",
  koord: "Koordinator",
};

interface Props {
  agent: AgentInfo | null;
  seatKind: SeatKind;
  botState: BotState;
  theme: Theme;
  selected: boolean;
  spawnDisabled: string | null;
  onSelect: (agentId: string) => void;
  onSpawn: (seatKind: SeatKind) => void;
}

export default function Seat(props: Props) {
  const { agent, seatKind, botState, theme, selected, spawnDisabled, onSelect, onSpawn } = props;
  const base = "flex h-[168px] min-w-0 flex-col items-center justify-center rounded-xl border p-2";

  if (agent === null) {
    const label = seatKind === "work" ? "Start en agent på denne arbejdsplads" : "Start en agent på denne stabsplads";
    return (
      <button
        type="button"
        onClick={() => onSpawn(seatKind)}
        disabled={spawnDisabled !== null}
        title={spawnDisabled ?? label}
        aria-label={label}
        className={`${base} border-dashed border-[var(--border)] text-3xl text-[var(--muted)] hover:border-[var(--accent)] hover:text-[var(--accent)] disabled:cursor-not-allowed disabled:opacity-40 disabled:hover:border-[var(--border)] disabled:hover:text-[var(--muted)]`}
      >
        +
      </button>
    );
  }

  const label = statusLabel(agent.status);
  const exited = isExited(agent);
  return (
    <button
      type="button"
      onClick={() => onSelect(agent.id)}
      title={`${agent.name} — ${label}${agent.detail ? `: ${agent.detail}` : ""}\n${agent.cwd}`}
      aria-label={`Vis terminal for ${agent.name}`}
      aria-pressed={selected}
      className={`${base} gap-0.5 bg-[var(--panel)] ${
        selected
          ? "border-[var(--accent)] ring-2 ring-[var(--accent)]/40"
          : "border-[var(--border)] hover:border-[var(--accent)]"
      }`}
    >
      <BotFigure role={agent.role} state={botState} theme={theme} exited={exited} size={exited ? 78 : 96} />
      <span className="w-full truncate text-center text-xs font-medium">{agent.name}</span>
      <span className="w-full truncate text-center text-[10px] text-[var(--muted)]">
        {label}
        {agent.detail ? ` · ${agent.detail}` : ""}
      </span>
      {agent.role !== "none" && (
        <span className="rounded bg-[var(--accent)]/15 px-1.5 text-[10px] text-[var(--accent)]">
          {ROLE_LABEL[agent.role]}
        </span>
      )}
    </button>
  );
}
