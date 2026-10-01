import type { Theme } from "../../lib/bots";
import { errorMessage, removeAgent } from "../../lib/ipc";
import type { OfficeDetail, TermMode } from "../../lib/office";
import type { SeatAssignment } from "../../lib/seats";
import type { BotState, SeatKind, TicketSummary } from "../../lib/types";
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
  const { tickets, dragging, detail, mode, fig } = props;
  const roomy = mode !== "max";
  const { dispatch } = useStore();

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
        detail={detail}
        compact={mode === "max"}
        fig={fig}
        onSelect={onSelect}
        onSpawn={onSpawn}
      />
    ));

  const remove = async (id: string) => {
    try {
      await removeAgent(id);
    } catch (e) {
      dispatch({ type: "error/set", error: errorMessage(e) });
    }
  };

  return (
    <>
      {detail === "more" && roomy && <WallDecor />}
      <div className="office-seats">
        <div className="office-staff" aria-label="Stabspladser">
          <span className="office-sign">Stab</span>
          {detail === "more" && roomy && <RoomDecor side="left" />}
          <div className="office-row3">{row("staff", seats.staff)}</div>
          {detail === "more" && roomy && <RoomDecor side="right" />}
        </div>
        <div className="office-row5" aria-label="Arbejdspladser">
          {row("work", seats.work)}
        </div>
      </div>
      {seats.overflow.length > 0 && (
        <div className="mx-3 mb-2 flex flex-wrap items-center gap-2 text-xs text-[var(--muted)]">
          <span>Uden plads:</span>
          {seats.overflow.map((a) => (
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
      )}
    </>
  );
}
