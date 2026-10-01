// Seat assignment for the workplace: 5 work seats and 3 staff seats.
import type { AgentInfo } from "./types";

export const WORK_SEATS = 5;
export const STAFF_SEATS = 3;

export interface SeatAssignment {
  /** Always `WORK_SEATS` long; null = empty seat. */
  work: (AgentInfo | null)[];
  /** Always `STAFF_SEATS` long; null = empty seat. */
  staff: (AgentInfo | null)[];
  /** Agents that did not fit (only exited agents that were not removed yet). */
  overflow: AgentInfo[];
}

function fillRow(agents: AgentInfo[], size: number, overflow: AgentInfo[]): (AgentInfo | null)[] {
  const row: (AgentInfo | null)[] = [];
  let liveLeft = agents.filter((a) => a.status.kind !== "exited").length;
  for (const a of agents) {
    const live = a.status.kind !== "exited";
    if (live) liveLeft--;
    // An exited agent keeps its seat only while every later live agent still gets one.
    if (row.length < size && (live || size - row.length - 1 >= liveLeft)) row.push(a);
    else overflow.push(a);
  }
  while (row.length < size) row.push(null);
  return row;
}

/**
 * Places agents by `seatKind`, oldest (`createdAt`) first, left to right. Live agents always get
 * a seat (the backend limits them to the seat count); exited agents that are not removed yet
 * only keep a seat while there is room, the rest go to `overflow`.
 *
 * Example: work agents A(idle), B(exited), C(running), D, E, F all created in that order with
 * F live and B exited → work = [A, C, D, E, F], overflow = [B]. Without F: work = [A, B, C, D, E].
 */
export function assignSeats(agents: readonly AgentInfo[]): SeatAssignment {
  const sorted = [...agents].sort((a, b) => a.createdAt - b.createdAt);
  const overflow: AgentInfo[] = [];
  const work = fillRow(
    sorted.filter((a) => a.seatKind === "work"),
    WORK_SEATS,
    overflow,
  );
  const staff = fillRow(
    sorted.filter((a) => a.seatKind === "staff"),
    STAFF_SEATS,
    overflow,
  );
  return { work, staff, overflow };
}
