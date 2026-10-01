// Pure ticket helpers for the UI: labels, grouping, queue order, the "may I …" rules and the
// drag-and-drop id format. No React, no IPC: every function here is a plain function of its
// arguments (compiled and run in node for the truth tables below).
import type {
  AgentInfo,
  SeatKind,
  TicketActor,
  TicketIssue,
  TicketState,
  TicketSummary,
  WorkplaceTab,
} from "./types";

/** Done tickets shown before "og n flere". */
export const DONE_VISIBLE = 20;
/** Pointer travel before a drag starts, so clicks on seats and note buttons stay clicks. */
export const DRAG_DISTANCE_PX = 6;
/** Mirrors `TICKET_TITLE_MAX_CHARS` / `TICKET_BODY_MAX_CHARS` in Rust; the backend validates. */
export const TITLE_MAX = 200;
export const BODY_MAX = 20000;
/** Mirrors `DELIVERY_FAILED_TEXT` / `TURN_FAILED_TEXT` in Rust (set as the agent's `detail`). */
export const DELIVERY_FAILED_TEXT = "Kunne ikke aflevere ticket, se terminalen";
export const TURN_FAILED_TEXT = "Turn fejlede, prøv igen eller skriv i terminalen";

export const STATE_LABEL: Record<TicketState, string> = {
  backlog: "Backlog",
  assigned: "I kø",
  inProgress: "I gang",
  review: "Review",
  done: "Done",
  rejected: "Afvist",
};

/** Full class strings per state (Tailwind scans the source, so no string building). */
export const STATE_BADGE_CLASS: Record<TicketState, string> = {
  backlog: "bg-neutral-500/15 text-[var(--muted)]",
  assigned: "bg-sky-500/15 text-sky-700 dark:text-sky-300",
  inProgress: "bg-violet-500/15 text-violet-700 dark:text-violet-300",
  review: "bg-amber-400/25 text-amber-800 dark:text-amber-200",
  done: "bg-emerald-500/15 text-emerald-700 dark:text-emerald-300",
  rejected: "bg-rose-500/15 text-rose-700 dark:text-rose-300",
};

export const ISSUE_LABEL: Record<TicketIssue, string> = {
  deliveryFailed: "Levering fejlede",
  turnFailed: "Turn fejlede",
};

/** What the user can do about an issue (shown next to `ISSUE_LABEL`). */
export const ISSUE_HINT: Record<TicketIssue, string> = {
  deliveryFailed: "se terminalen",
  turnFailed: "prøv Send igen",
};

export const ACTOR_LABEL: Record<TicketActor, string> = {
  user: "dig",
  system: "systemet",
  agent: "agenten",
};

/**
 * First 8 characters of the uuid without dashes, lower case (mirrors `short_id` in Rust; the
 * backend also sends `shortId`, this is for ids alone).
 *
 * | id                                     | shortId    |
 * |----------------------------------------|------------|
 * | "3F2A9C10-77aa-4b1e-9d2e-000000000000" | "3f2a9c10" |
 * | "ab-cd-ef"                             | "abcdef"   |
 * | ""                                     | ""         |
 */
export function shortId(id: string): string {
  return id.replace(/-/g, "").slice(0, 8).toLowerCase();
}

function isLive(agent: AgentInfo | null | undefined): agent is AgentInfo {
  return agent !== null && agent !== undefined && agent.status.kind !== "exited";
}

/**
 * Tickets per state, each list in input order (`list_tickets` sorts by `createdAt`).
 *
 * | input states                    | backlog | assigned | review | done |
 * |---------------------------------|---------|----------|--------|------|
 * | []                              | []      | []       | []     | []   |
 * | [backlog a, review b, backlog c]| [a, c]  | []       | [b]    | []   |
 */
export function ticketsByState(
  tickets: readonly TicketSummary[],
): Record<TicketState, TicketSummary[]> {
  const out: Record<TicketState, TicketSummary[]> = {
    backlog: [],
    assigned: [],
    inProgress: [],
    review: [],
    done: [],
    rejected: [],
  };
  for (const t of tickets) out[t.state].push(t);
  return out;
}

/**
 * The agent's queue: its `assigned` tickets ordered by `queuePosition` (null last, then
 * `updatedAt`), i.e. the order the dispatcher sends them in.
 *
 * | tickets (state, assignee, pos)                    | queueFor("A") |
 * |---------------------------------------------------|---------------|
 * | (assigned A 1) x, (assigned A 0) y                | [y, x]        |
 * | (assigned B 0) z, (inProgress A –) w              | []            |
 * | (assigned A null) n, (assigned A 0) y             | [y, n]        |
 */
export function queueFor(tickets: readonly TicketSummary[], agentId: string): TicketSummary[] {
  return tickets
    .filter((t) => t.state === "assigned" && t.assigneeAgentId === agentId)
    .sort(
      (a, b) =>
        (a.queuePosition ?? Number.MAX_SAFE_INTEGER) - (b.queuePosition ?? Number.MAX_SAFE_INTEGER) ||
        a.updatedAt - b.updatedAt,
    );
}

/**
 * The agent's ticket in progress (at most one; the backend guarantees it), or null.
 *
 * | tickets                                  | currentFor("A") |
 * |------------------------------------------|-----------------|
 * | (inProgress A) w                         | w               |
 * | (assigned A) x, (inProgress B) v         | null            |
 */
export function currentFor(
  tickets: readonly TicketSummary[],
  agentId: string,
): TicketSummary | null {
  return tickets.find((t) => t.state === "inProgress" && t.assigneeAgentId === agentId) ?? null;
}

/**
 * Number of tickets waiting for the user's review (island chip, sidebar tab).
 *
 * | states                         | reviewCount |
 * |--------------------------------|-------------|
 * | []                             | 0           |
 * | [review, done, review, backlog]| 2           |
 */
export function reviewCount(tickets: readonly TicketSummary[]): number {
  let n = 0;
  for (const t of tickets) if (t.state === "review") n++;
  return n;
}

export interface AgentTickets {
  current: TicketSummary | null;
  /** Ordered like `queueFor`. */
  queue: TicketSummary[];
}

export interface TicketGroups {
  /** backlog plus rejected without an agent (both can be assigned again), oldest first. */
  backlog: TicketSummary[];
  /** Oldest waiting first. */
  review: TicketSummary[];
  /** Newest first. */
  done: TicketSummary[];
  byAgent: Map<string, AgentTickets>;
}

/**
 * Sidebar sections and the per-agent view.
 *
 * | ticket (state, assignee)  | lands in                         |
 * |---------------------------|----------------------------------|
 * | backlog, –                | backlog                          |
 * | rejected, –               | backlog                          |
 * | rejected, A (transient)   | byAgent A (neither list)         |
 * | assigned, A               | byAgent A .queue                 |
 * | inProgress, A             | byAgent A .current               |
 * | review, A                 | review                           |
 * | done, any                 | done (sorted by updatedAt desc)  |
 */
export function groupTickets(tickets: readonly TicketSummary[]): TicketGroups {
  const backlog: TicketSummary[] = [];
  const review: TicketSummary[] = [];
  const done: TicketSummary[] = [];
  const byAgent = new Map<string, AgentTickets>();
  const slot = (agentId: string): AgentTickets => {
    let s = byAgent.get(agentId);
    if (s === undefined) {
      s = { current: null, queue: [] };
      byAgent.set(agentId, s);
    }
    return s;
  };
  for (const t of tickets) {
    switch (t.state) {
      case "backlog":
        backlog.push(t);
        break;
      case "rejected":
        if (t.assigneeAgentId === null) backlog.push(t);
        else slot(t.assigneeAgentId);
        break;
      case "assigned":
        if (t.assigneeAgentId !== null) slot(t.assigneeAgentId).queue.push(t);
        break;
      case "inProgress":
        if (t.assigneeAgentId !== null) slot(t.assigneeAgentId).current = t;
        break;
      case "review":
        review.push(t);
        break;
      case "done":
        done.push(t);
        break;
    }
  }
  for (const [id, s] of byAgent) s.queue = queueFor(s.queue, id);
  review.sort((a, b) => a.updatedAt - b.updatedAt);
  done.sort((a, b) => b.updatedAt - a.updatedAt);
  return { backlog, review, done, byAgent };
}

/**
 * Whether the note may be dragged onto a seat (same set `assign_ticket` accepts).
 *
 * | state      | assignee | canDrag |
 * |------------|----------|---------|
 * | backlog    | –        | true    |
 * | rejected   | –        | true    |
 * | rejected   | A        | false   |
 * | assigned   | A        | false   |
 * | inProgress | A        | false   |
 * | review     | A        | false   |
 * | done       | any      | false   |
 */
export function canDrag(t: TicketSummary): boolean {
  return (t.state === "backlog" || t.state === "rejected") && t.assigneeAgentId === null;
}

/**
 * Whether `assign_ticket(t, agent)` can succeed: a draggable ticket and a running agent.
 *
 * | canDrag(t) | agent          | canAssign |
 * |------------|----------------|-----------|
 * | true       | idle/running   | true      |
 * | true       | exited         | false     |
 * | true       | null           | false     |
 * | false      | idle           | false     |
 */
export function canAssign(t: TicketSummary, agent: AgentInfo | null): boolean {
  return canDrag(t) && isLive(agent);
}

/**
 * Mirrors the backend rule for `delete_ticket`.
 *
 * | state      | assignee | canDelete |
 * |------------|----------|-----------|
 * | backlog    | any      | true      |
 * | done       | any      | true      |
 * | rejected   | –        | true      |
 * | rejected   | A        | false     |
 * | assigned   | A        | false     |
 * | inProgress | A        | false     |
 * | review     | A        | false     |
 */
export function canDelete(t: TicketSummary): boolean {
  if (t.state === "backlog" || t.state === "done") return true;
  return t.state === "rejected" && t.assigneeAgentId === null;
}

/**
 * Whether "Send igen" makes sense: the ticket has an issue and its agent is running and idle
 * (the dispatcher ignores a redispatch otherwise).
 *
 * | issue          | agent status | canRedispatch |
 * |----------------|--------------|---------------|
 * | turnFailed     | idle         | true          |
 * | deliveryFailed | idle         | true          |
 * | turnFailed     | running      | false         |
 * | turnFailed     | exited       | false         |
 * | turnFailed     | (no agent)   | false         |
 * | null           | idle         | false         |
 */
export function canRedispatch(t: TicketSummary, agent: AgentInfo | null): boolean {
  return t.issue !== null && agent !== null && agent.status.kind === "idle";
}

/**
 * Whether a review ticket can be moved back to "I gang" ("Ikke færdig"; `set_ticket_state`
 * Reopen needs a running agent).
 *
 * | state  | agent   | canReopen |
 * |--------|---------|-----------|
 * | review | idle    | true      |
 * | review | exited  | false     |
 * | done   | idle    | false     |
 */
export function canReopen(t: TicketSummary, agent: AgentInfo | null): boolean {
  return t.state === "review" && isLive(agent) && t.assigneeAgentId === agent.id;
}

/**
 * The queue ids after moving the ticket at `index` one place up, or null when it cannot move.
 *
 * | ids        | index | moveUp     |
 * |------------|-------|------------|
 * | [a, b, c]  | 2     | [a, c, b]  |
 * | [a, b, c]  | 1     | [b, a, c]  |
 * | [a, b, c]  | 0     | null       |
 * | [a]        | 5     | null       |
 */
export function moveUp(ids: readonly string[], index: number): string[] | null {
  if (index <= 0 || index >= ids.length) return null;
  const out = [...ids];
  const tmp = out[index - 1] as string;
  out[index - 1] = out[index] as string;
  out[index] = tmp;
  return out;
}

// --- drag-and-drop ids --------------------------------------------------------------------------

export const ticketDragId = (ticketId: string) => `ticket:${ticketId}`;
export const agentDropId = (agentId: string) => `agent:${agentId}`;
export const emptyDropId = (seatKind: SeatKind, index: number) => `empty:${seatKind}:${index}`;

export type DropTarget =
  | { kind: "agent"; agentId: string }
  | { kind: "empty"; seatKind: SeatKind; index: number };

/**
 * Parses a droppable id (inverse of `agentDropId` / `emptyDropId`).
 *
 * | id                 | dropTarget                                  |
 * |--------------------|---------------------------------------------|
 * | "agent:abc-1"      | { kind: "agent", agentId: "abc-1" }         |
 * | "empty:work:3"     | { kind: "empty", seatKind: "work", index: 3 }|
 * | "empty:staff:0"    | { kind: "empty", seatKind: "staff", index: 0 }|
 * | "empty:desk:1"     | null                                        |
 * | "empty:work:x"     | null                                        |
 * | "agent:"           | null                                        |
 * | "ticket:abc"       | null                                        |
 */
export function dropTarget(id: string): DropTarget | null {
  if (id.startsWith("agent:")) {
    const agentId = id.slice("agent:".length);
    return agentId === "" ? null : { kind: "agent", agentId };
  }
  const m = /^empty:(work|staff):(\d+)$/.exec(id);
  if (m === null) return null;
  return { kind: "empty", seatKind: m[1] as SeatKind, index: Number(m[2]) };
}

/**
 * Parses a draggable id (inverse of `ticketDragId`).
 *
 * | id            | draggedTicketId |
 * |---------------|-----------------|
 * | "ticket:abc"  | "abc"           |
 * | "ticket:"     | null            |
 * | "agent:abc"   | null            |
 */
export function draggedTicketId(id: string): string | null {
  if (!id.startsWith("ticket:")) return null;
  const rest = id.slice("ticket:".length);
  return rest === "" ? null : rest;
}

/**
 * Sidebar tab from a `WorkplaceSelection.tab` string (the backend passes it through).
 *
 * | tab            | parseWorkplaceTab |
 * |----------------|-------------------|
 * | "tickets"      | "tickets"         |
 * | "permissions"  | "permissions"     |
 * | "diagnostics"  | "diagnostics"     |
 * | "Tickets"      | null              |
 * | null           | null              |
 */
export function parseWorkplaceTab(tab: string | null): WorkplaceTab | null {
  return tab === "tickets" || tab === "permissions" || tab === "diagnostics" ? tab : null;
}
