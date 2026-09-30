// Presentation of AgentStatus: Danish labels, dot colours and the "worst status" ranking.
import type { AgentInfo, AgentStatus, AgentStatusKind } from "./types";

/** Full class strings (Tailwind scans the source, so no string building here). */
export const DOT_CLASS: Record<AgentStatusKind, string> = {
  starting: "bg-neutral-400 animate-pulse",
  idle: "bg-emerald-400",
  thinking: "bg-sky-400 animate-pulse",
  reading: "bg-sky-400",
  editing: "bg-amber-400",
  running: "bg-violet-400",
  waitingPermission: "bg-rose-400 animate-pulse",
  exited: "bg-neutral-600",
};

export function statusLabel(s: AgentStatus): string {
  switch (s.kind) {
    case "starting":
      return "Starter";
    case "idle":
      return "Klar";
    case "thinking":
      return "Tænker";
    case "reading":
      return "Læser";
    case "editing":
      return "Redigerer";
    case "running":
      return "Kører";
    case "waitingPermission":
      return "Afventer tilladelse";
    case "exited":
      return s.code === null ? "Afsluttet" : `Afsluttet (kode ${s.code})`;
  }
}

/** Higher = more attention needed. */
const RANK: Record<AgentStatusKind, number> = {
  waitingPermission: 5,
  running: 4,
  editing: 4,
  reading: 3,
  thinking: 3,
  starting: 2,
  idle: 2,
  exited: 1,
};

/** The status that the collapsed strip should show, or null without agents. */
export function worstStatus(agents: AgentInfo[]): AgentStatusKind | null {
  let worst: AgentStatusKind | null = null;
  for (const a of agents) {
    const k = a.status.kind;
    if (worst === null || RANK[k] > RANK[worst]) worst = k;
  }
  return worst;
}

export function isExited(a: AgentInfo): boolean {
  return a.status.kind === "exited";
}

/** Starting for a while without hook events: the backend set a hint in `detail` (plan item 7). */
export function isStartingHint(a: AgentInfo): boolean {
  return a.status.kind === "starting" && a.detail !== null;
}
