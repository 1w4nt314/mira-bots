import type { Theme } from "../../lib/bots";
import { errorMessage, removeAgent } from "../../lib/ipc";
import type { OfficeDetail, TermMode } from "../../lib/office";
import type { SeatAssignment } from "../../lib/seats";
import type { AgentInfo, BotState, SeatKind, TicketSummary } from "../../lib/types";
import { useStore } from "../../state/store";
import RoomDecor from "./office/RoomDecor";
import WallDecor from "./office/WallDecor";
import Seat from "./Seat";

interface Props {
  seats: SeatAssignment;
  botStates: Map<string, BotState>;
  theme: Theme;
  selectedId: string | null;
  /** Reason why no agent can be started at all (hook exe/pipe), or null. */
  spawnDisabled: string | null;
  /** True when the live-agent limit of that row is reached. */
  limits: Record<SeatKind, boolean>;
  /** All tickets by id (for each agent's `currentTicketId`). */
  tickets: Map<string, TicketSummary>;
  /** A ticket note is being dragged. */
  dragging: boolean;
  /** The dragged ticket (step 4b: seats of another project say so). */
  draggedTicket: TicketSummary | null;
  /** "n agenter, ingen koordinator" for a work agent's project, or null. */
  hintFor: (agent: AgentInfo) => string | null;
  /** "more" adds the wall strip, room furniture and desk items. */
  detail: OfficeDetail;
  /** "max" shows the narrow strip (compact seats, no wall or furniture). */
  mode: TermMode;
  /** Figure height in px (from `deskLayout`). */
  fig: number;
  onSelect: (agentId: string) => void;
  onSpawn: (seatKind: SeatKind) => void;
}

export default function SeatGrid(props: Props) {
  const { seats, botStates, theme, selectedId, spawnDisabled, limits, onSelect, onSpawn } = props;
  const { tickets, dragging, draggedTicket, hintFor, detail, mode, fig } = props;
  // The staff sign names the projects where work agents share a folder without a coordinator.
  const hints = new Map<string, string>();
  for (const a of seats.work) {
    const hint = a === null ? null : hintFor(a);
    if (a !== null && hint !== null && a.project !== null) hints.set(a.project.toLowerCase(), `${a.project}: ${hint}`);
  }
  const staffHint = hints.size === 0 ? null : [...hints.values()].join("\n");
  const roomy = mode !== "max";

  const row = (kind: SeatKind, list: SeatAssignment["work"]) =>
    list.map((agent, i) => (
      <Seat
        key={agent?.id ?? `${kind}-${i}`}
        agent={agent}
        seatKind={kind}
        index={i}
        botState={agent === null ? "idle" : (botStates.get(agent.id) ?? "idle")}
        theme={theme}
        selected={agent !== null && agent.id === selectedId}
        spawnDisabled={
          spawnDisabled ?? (limits[kind] ? "Loftet for denne række er nået" : null)
        }
        currentTicket={
          agent?.currentTicketId != null ? (tickets.get(agent.currentTicketId) ?? null) : null
        }
        dragging={dragging}
        draggedTicket={draggedTicket}
        projectHint={agent === null ? null : hintFor(agent)}
        detail={detail}
        compact={mode === "max"}
        fig={fig}
        onSelect={onSelect}
        onSpawn={onSpawn}
      />
    ));

  return (
    <>
      {detail === "more" && roomy && <WallDecor />}
      <div className="office-seats">
        <div className="office-staff" aria-label="Stabspladser">
          <span
            className="office-sign"
            title={staffHint === null ? undefined : `Projekter uden koordinator:\n${staffHint}`}
          >
            Stab{staffHint !== null && " ⚠"}
          </span>
          {detail === "more" && roomy && <RoomDecor side="left" />}
          <div className="office-row3">{row("staff", seats.staff)}</div>
          {detail === "more" && roomy && <RoomDecor side="right" />}
        </div>
        <div className="office-row5" aria-label="Arbejdspladser">
          {row("work", seats.work)}
        </div>
      </div>
    </>
  );
}

/**
 * Agents without a seat (more live agents than seats). Rendered by Workplace outside the floor, so
 * the floor's fixed height and `overflow: hidden` never clip it.
 */
export function SeatOverflow({ overflow }: { overflow: SeatAssignment["overflow"] }) {
  const { dispatch } = useStore();

  const remove = async (id: string) => {
    try {
      await removeAgent(id);
    } catch (e) {
      dispatch({ type: "error/set", error: errorMessage(e) });
    }
  };

  if (overflow.length === 0) return null;
  return (
    <div className="mx-3 my-1.5 flex flex-wrap items-center gap-2 text-xs text-[var(--muted)]">
      <span>Uden plads:</span>
      {overflow.map((a) => (
        <span
          key={a.id}
          className="flex items-center gap-1 rounded border border-[var(--border)] px-1.5 py-0.5"
        >
          {a.name}
          <button
            type="button"
            onClick={() => void remove(a.id)}
            title={`Fjern ${a.name} fra listen`}
            aria-label={`Fjern ${a.name}`}
            className="rounded px-1 hover:bg-neutral-500/20"
          >
            Fjern
          </button>
        </span>
      ))}
    </div>
  );
}
