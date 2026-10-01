import { useDroppable } from "@dnd-kit/core";
import type { Theme } from "../../lib/bots";
import { isExited, statusLabel } from "../../lib/status";
import { agentDropId, emptyDropId } from "../../lib/tickets";
import type { AgentInfo, AgentRole, BotState, SeatKind, TicketSummary } from "../../lib/types";
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
  /** Position in its row (part of an empty seat's drop id). */
  index: number;
  botState: BotState;
  theme: Theme;
  selected: boolean;
  spawnDisabled: string | null;
  /** The agent's ticket in progress (`currentTicketId`), if known. */
  currentTicket: TicketSummary | null;
  /** A ticket note is being dragged: show which seats accept it. */
  dragging: boolean;
  onSelect: (agentId: string) => void;
  onSpawn: (seatKind: SeatKind) => void;
}

// TODO(windows-verify): dropping a dragged note on a seat works in WebView2 with mouse and
// touchpad, a click (< 6 px) still selects/spawns, and the drop highlight follows the pointer
// (plan D.31; slip på tom plads → SpawnDialog med ticket: D.32).
export default function Seat(props: Props) {
  const { agent, seatKind, index, botState, theme, selected, spawnDisabled } = props;
  const { currentTicket, dragging, onSelect, onSpawn } = props;
  const exited = agent !== null && isExited(agent);
  // Exited agents cannot take tickets; empty seats only while an agent may be started there.
  const dropDisabled = agent !== null ? exited : spawnDisabled !== null;
  const { setNodeRef, isOver } = useDroppable({
    id: agent !== null ? agentDropId(agent.id) : emptyDropId(seatKind, index),
    disabled: dropDisabled,
  });

  const base = "relative flex h-[168px] min-w-0 flex-col items-center justify-center rounded-xl border p-2";
  // Full class strings for the drag feedback (valid target / pointer over it / invalid target).
  const dropClass = !dragging
    ? ""
    : dropDisabled
      ? "cursor-not-allowed opacity-40"
      : isOver
        ? "ring-4 ring-[var(--accent)] bg-[var(--accent)]/10"
        : "outline-2 outline-dashed outline-offset-2 outline-[var(--accent)]/60";

  if (agent === null) {
    const label =
      seatKind === "work" ? "Start en agent på denne arbejdsplads" : "Start en agent på denne stabsplads";
    return (
      <button
        ref={setNodeRef}
        type="button"
        onClick={() => onSpawn(seatKind)}
        disabled={spawnDisabled !== null}
        title={spawnDisabled ?? label}
        aria-label={label}
        className={`${base} border-dashed border-[var(--border)] text-3xl text-[var(--muted)] hover:border-[var(--accent)] hover:text-[var(--accent)] disabled:cursor-not-allowed disabled:opacity-40 disabled:hover:border-[var(--border)] disabled:hover:text-[var(--muted)] ${dropClass}`}
      >
        {dragging && !dropDisabled ? (
          <span className="px-1 text-center text-[11px] text-[var(--accent)]">
            Slip for at starte en agent med ticketen
          </span>
        ) : (
          "+"
        )}
      </button>
    );
  }

  const label = statusLabel(agent.status);
  const hasTickets = currentTicket !== null || agent.queueLength > 0;
  const figure = exited ? 78 : hasTickets ? 80 : 96;
  return (
    <button
      ref={setNodeRef}
      type="button"
      onClick={() => onSelect(agent.id)}
      title={`${agent.name} — ${label}${agent.detail ? `: ${agent.detail}` : ""}\n${agent.cwd}${
        currentTicket !== null ? `\nI gang: ${currentTicket.title}` : ""
      }${dragging && exited ? "\nAfsluttet: kan ikke få tickets" : ""}`}
      aria-label={`Vis terminal for ${agent.name}`}
      aria-pressed={selected}
      className={`${base} gap-0.5 bg-[var(--panel)] ${
        selected
          ? "border-[var(--accent)] ring-2 ring-[var(--accent)]/40"
          : "border-[var(--border)] hover:border-[var(--accent)]"
      } ${dropClass}`}
    >
      {agent.queueLength > 0 && (
        <span
          className="absolute right-1.5 top-1.5 rounded-full bg-sky-500/20 px-1.5 text-[10px] leading-4 text-sky-700 dark:text-sky-300"
          title={`${agent.queueLength} ${agent.queueLength === 1 ? "ticket" : "tickets"} i kø`}
        >
          {agent.queueLength} i kø
        </span>
      )}
      <BotFigure role={agent.role} state={botState} theme={theme} exited={exited} size={figure} />
      <span className="w-full truncate text-center text-xs font-medium">{agent.name}</span>
      <span className="w-full truncate text-center text-[10px] text-[var(--muted)]">
        {label}
        {agent.detail ? ` · ${agent.detail}` : ""}
      </span>
      {currentTicket !== null && (
        <span className="w-full truncate text-center text-[10px]" title={currentTicket.title}>
          ▸ {currentTicket.title}
        </span>
      )}
      {agent.role !== "none" && (
        <span className="rounded bg-[var(--accent)]/15 px-1.5 text-[10px] text-[var(--accent)]">
          {ROLE_LABEL[agent.role]}
        </span>
      )}
    </button>
  );
}
