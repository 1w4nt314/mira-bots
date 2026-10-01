import { useDroppable } from "@dnd-kit/core";
import type { CSSProperties } from "react";
import type { Theme } from "../../lib/bots";
import { COMPACT, itemsFor, type OfficeDetail } from "../../lib/office";
import { rolesText } from "../../lib/roles";
import { isExited, statusLabel } from "../../lib/status";
import { agentDropId, emptyDropId } from "../../lib/tickets";
import type { AgentInfo, BotState, SeatKind, TicketSummary } from "../../lib/types";
import BotFigure from "../BotFigure";
import DeskArt from "./office/DeskArt";

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
  /** "more" adds desk items (deterministic per agent). */
  detail: OfficeDetail;
  /** Narrow one-row variant for the maximised terminal (no desk art). */
  compact: boolean;
  /** Figure height in px (normal variant). */
  fig: number;
  onSelect: (agentId: string) => void;
  onSpawn: (seatKind: SeatKind) => void;
}

// A seat is a desk: the figure sits behind it, the laptop screen glows in the agent's state
// colour (`--scr`, set inline from `botState`), a post-it shows an active ticket.
// TODO(windows-verify): dropping a dragged note on a seat works in WebView2 with mouse and
// touchpad, a click (< 6 px) still selects/spawns, and the drop highlight follows the pointer
// (plan D.31; slip på tom plads → SpawnDialog med ticket: D.32).
export default function Seat(props: Props) {
  const { agent, seatKind, index, botState, theme, selected, spawnDisabled } = props;
  const { currentTicket, dragging, detail, compact, fig, onSelect, onSpawn } = props;
  const exited = agent !== null && isExited(agent);
  // Exited agents cannot take tickets; empty seats only while an agent may be started there.
  const dropDisabled = agent !== null ? exited : spawnDisabled !== null;
  const { setNodeRef, isOver } = useDroppable({
    id: agent !== null ? agentDropId(agent.id) : emptyDropId(seatKind, index),
    disabled: dropDisabled,
  });

  const state: BotState = agent === null ? "idle" : botState;
  const stateStyle = { "--scr": `var(--scr-${state})` } as CSSProperties;
  // Full class strings for the drag feedback (valid target / pointer over it / invalid target).
  const dropClass = !dragging
    ? ""
    : dropDisabled
      ? "cursor-not-allowed opacity-40"
      : isOver
        ? "ring-4 ring-[var(--accent)] bg-[var(--accent)]/10"
        : "outline-2 outline-dashed outline-offset-2 outline-[var(--accent)]/60";
  // Compact (max strip): small figure above the name, so the name gets the seat's full width
  // (~75 px at 1100 px); the status is in the title.
  const base = compact
    ? "office-seat relative flex min-w-0 flex-col items-center justify-center gap-0 rounded-xl border px-1 py-0.5 text-center"
    : "office-seat relative flex min-w-0 flex-col items-center justify-end rounded-xl border p-1 pb-0.5";

  if (agent === null) {
    const label =
      seatKind === "work" ? "Start en agent på denne arbejdsplads" : "Start en agent på denne stabsplads";
    const kindText = seatKind === "work" ? "Arbejdsplads" : "Stabsplads";
    const dropHint = dragging && !dropDisabled;
    return (
      <button
        ref={setNodeRef}
        type="button"
        onClick={() => onSpawn(seatKind)}
        disabled={spawnDisabled !== null}
        title={spawnDisabled ?? label}
        aria-label={label}
        data-state="idle"
        style={stateStyle}
        className={`${base} border-transparent text-[var(--muted)] hover:border-[var(--accent)] hover:text-[var(--accent)] disabled:cursor-not-allowed disabled:opacity-40 disabled:hover:border-[var(--border)] disabled:hover:text-[var(--muted)] ${dropClass}`}
      >
        {compact ? (
          <>
            <span className="shrink-0 text-lg leading-none">+</span>
            <span className="max-w-full truncate text-[11px] font-medium leading-3">Ledig</span>
          </>
        ) : (
          <>
            {dropHint ? (
              <span className="absolute left-1/2 top-[12%] z-[3] w-[90%] -translate-x-1/2 px-1 text-center text-[11px] text-[var(--accent)]">
                Slip for at starte en agent med ticketen
              </span>
            ) : (
              <span className="office-plus">+</span>
            )}
            <span className="office-desk office-desk-empty">
              <DeskArt laptop={false} postit={false} chair items={[]} />
            </span>
            <span className="office-below">
              <span className="office-ticketline">{" "}</span>
              <span className="office-status">{kindText}</span>
            </span>
          </>
        )}
      </button>
    );
  }

  const label = statusLabel(agent.status);
  const statusText = `${label}${agent.detail ? ` · ${agent.detail}` : ""}`;
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
      data-state={botState}
      style={stateStyle}
      className={`${base} ${
        selected
          ? "border-[var(--accent)] ring-2 ring-[var(--accent)]/40"
          : "border-transparent hover:border-[var(--accent)]"
      } ${dropClass}`}
    >
      {compact ? (
        <>
          <BotFigure
            roles={agent.roles}
            specialist={agent.specialist}
            state={botState}
            theme={theme}
            exited={exited}
            size={COMPACT.fig}
            badge={false}
          />
          <span className="max-w-full truncate text-[11px] font-medium leading-3">{agent.name}</span>
        </>
      ) : (
        <>
          {agent.queueLength > 0 && (
            <span
              className="absolute right-1.5 top-1.5 z-[3] rounded-full bg-sky-500/20 px-1.5 text-[10px] leading-4 text-sky-700 dark:text-sky-300"
              title={`${agent.queueLength} ${agent.queueLength === 1 ? "ticket" : "tickets"} i kø`}
            >
              {agent.queueLength} i kø
            </span>
          )}
          <span className="office-fig">
            <BotFigure
              roles={agent.roles}
              specialist={agent.specialist}
              state={botState}
              theme={theme}
              exited={exited}
              size={fig}
              badge={false}
            />
          </span>
          <span className="office-desk">
            <DeskArt
              laptop
              postit={currentTicket !== null}
              chair={false}
              items={detail === "more" ? itemsFor(agent.id) : []}
            />
            <span className="office-plate-wrap">
              <span className="office-plate">{agent.name}</span>
              <span className="office-status">{statusText}</span>
            </span>
          </span>
          <span className="office-below">
            <span className="office-ticketline" title={currentTicket?.title}>
              {currentTicket !== null ? `▸ ${currentTicket.title}` : " "}
            </span>
            <span
              className="max-w-full truncate rounded bg-[var(--accent)]/15 px-1.5 text-[10px] leading-3 text-[var(--accent)]"
              title={`Profil: ${agent.profileName}\nRoller: ${rolesText(agent.roles)}`}
            >
              {agent.profileName}
            </span>
          </span>
        </>
      )}
    </button>
  );
}
