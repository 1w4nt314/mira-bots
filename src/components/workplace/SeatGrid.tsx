import type { Theme } from "../../lib/bots";
import { errorMessage, removeAgent } from "../../lib/ipc";
import type { SeatAssignment } from "../../lib/seats";
import type { BotState, SeatKind, TicketSummary } from "../../lib/types";
import { useStore } from "../../state/store";
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
  onSelect: (agentId: string) => void;
  onSpawn: (seatKind: SeatKind) => void;
}

export default function SeatGrid(props: Props) {
  const { seats, botStates, theme, selectedId, spawnDisabled, limits, onSelect, onSpawn } = props;
  const { tickets, dragging } = props;
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
    <div className="shrink-0 p-3">
      <div className="flex gap-3">
        <div className="grid min-w-0 flex-[5] grid-cols-5 gap-2" aria-label="Arbejdspladser">
          {row("work", seats.work)}
        </div>
        <div
          className="relative min-w-0 flex-[2] rounded-xl border border-dashed border-[var(--border)] p-2 pt-4"
          aria-label="Stabspladser"
        >
          <span className="absolute -top-2 left-3 bg-[var(--bg)] px-1 text-[10px] uppercase tracking-wide text-[var(--muted)]">
            Stab
          </span>
          <div className="grid grid-cols-2 gap-2">{row("staff", seats.staff)}</div>
        </div>
      </div>
      {seats.overflow.length > 0 && (
        <div className="mt-2 flex flex-wrap items-center gap-2 text-xs text-[var(--muted)]">
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
    </div>
  );
}
