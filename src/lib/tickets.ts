// Pure ticket helpers for the UI: labels, grouping, queue order, the "may I …" rules and the
// drag-and-drop id format. No React, no IPC: every function here is a plain function of its
// arguments (compiled and run in node for the truth tables below).
import type {
  AgentInfo,
  SeatKind,
  Ticket,
  TicketActor,
  TicketGit,
  TicketHistoryEntry,
  TicketIssue,
  TicketReport,
  TicketSource,
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
/** Mirrors `TICKET_SUMMARY_MAX_CHARS` in Rust (an agent's summary; display only). */
export const SUMMARY_MAX = 2000;
/**
 * Mirrors `DELIVERY_FAILED_TEXT` / `TURN_FAILED_TEXT` / `NOT_SUBMITTED_TEXT` /
 * `WAKE_UNCONFIRMED_TEXT` in Rust (set as the agent's `detail`).
 */
export const DELIVERY_FAILED_TEXT = "Kunne ikke aflevere ticket, se terminalen";
export const TURN_FAILED_TEXT = "Turn fejlede, prøv igen eller skriv i terminalen";
export const NOT_SUBMITTED_TEXT = "Turn afsluttet uden aflevering";
/** The wake line of a waiting parent went unconfirmed twice (review 6a W1). */
export const WAKE_UNCONFIRMED_TEXT = "Vækning ikke bekræftet, se terminalen";

export const STATE_LABEL: Record<TicketState, string> = {
  backlog: "Backlog",
  assigned: "I kø",
  inProgress: "I gang",
  waiting: "Venter",
  review: "Review",
  done: "Done",
  rejected: "Afvist",
};

/** Full class strings per state (Tailwind scans the source, so no string building). */
export const STATE_BADGE_CLASS: Record<TicketState, string> = {
  backlog: "bg-neutral-500/15 text-[var(--muted)]",
  assigned: "bg-sky-500/15 text-sky-700 dark:text-sky-300",
  inProgress: "bg-violet-500/15 text-violet-700 dark:text-violet-300",
  waiting: "bg-teal-500/15 text-teal-700 dark:text-teal-300",
  review: "bg-amber-400/25 text-amber-800 dark:text-amber-200",
  done: "bg-emerald-500/15 text-emerald-700 dark:text-emerald-300",
  rejected: "bg-rose-500/15 text-rose-700 dark:text-rose-300",
};

/** Heading of the "waiting" block under a terminal (a parent whose children are still open). */
export const WAITING_TITLE = "Venter på del-tickets";
/** Shown on a waiting ticket: the app wakes the agent ("Læg tilbage" is the only button). */
export const WAITING_HINT = "Venter på del-tickets — vækkes automatisk når de er godkendt";
/** Title (tooltip) on the "Venter på …" badge of a blocked ticket. */
export const BLOCKED_HINT = "Leveres når blokeringerne er Done";
/** Tooltip on "Send til review" for a parent with open children. */
export const SUBMIT_PARENT_HINT = "Sender til review selv om del-tickets er åbne";

export const ISSUE_LABEL: Record<TicketIssue, string> = {
  deliveryFailed: "Levering fejlede",
  turnFailed: "Turn fejlede",
  notSubmitted: "Ikke afleveret",
};

/** What the user can do about an issue (shown next to `ISSUE_LABEL`). */
export const ISSUE_HINT: Record<TicketIssue, string> = {
  deliveryFailed: "se terminalen",
  turnFailed: "prøv Send igen",
  notSubmitted: "bed om aflevering eller send selv til review",
};

export const SOURCE_LABEL: Record<TicketSource, string> = {
  user: "dig",
  agent: "agent",
};

export const ACTOR_LABEL: Record<TicketActor, string> = {
  user: "dig",
  system: "systemet",
  agent: "agenten",
};

/** Short local date and time ("01.10. 14.05") for history lines and reports. */
export function formatAt(ms: number): string {
  return new Date(ms).toLocaleString("da-DK", {
    day: "2-digit",
    month: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

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
    waiting: [],
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
  /** Parents waiting for their children (step 6a), oldest `updatedAt` first. */
  waiting: TicketSummary[];
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
      s = { current: null, queue: [], waiting: [] };
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
      case "waiting":
        if (t.assigneeAgentId !== null) slot(t.assigneeAgentId).waiting.push(t);
        break;
      case "review":
        review.push(t);
        break;
      case "done":
        done.push(t);
        break;
    }
  }
  for (const [id, s] of byAgent) {
    s.queue = queueFor(s.queue, id);
    s.waiting.sort((a, b) => a.updatedAt - b.updatedAt);
  }
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
 * | waiting    | A        | false   |
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
 * Whether "Tildel…" may hand a ticket in progress over to another agent (step 5c: the backend's
 * `assign_ticket` moves it from its agent to the end of the new agent's queue). Dragging stays
 * as before (`canDrag`). The menu leaves out the current assignee.
 *
 * | state      | assignee | canHandOver |
 * |------------|----------|-------------|
 * | inProgress | A        | true        |
 * | inProgress | –        | false       |
 * | assigned   | A        | false       |
 * | review     | A        | false       |
 * | done       | any      | false       |
 * | backlog    | –        | false       |
 */
export function canHandOver(t: TicketSummary): boolean {
  return t.state === "inProgress" && t.assigneeAgentId !== null;
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
 * Whether the issue is one "Send igen" can repair. `notSubmitted` is not: the ticket was already
 * delivered and the agent worked on it, so it gets "Send til review" / "Bed om aflevering".
 *
 * | issue          | redispatchable |
 * |----------------|----------------|
 * | deliveryFailed | true           |
 * | turnFailed     | true           |
 * | notSubmitted   | false          |
 * | null           | false          |
 */
export function hasRedispatchIssue(t: TicketSummary): boolean {
  return t.issue === "deliveryFailed" || t.issue === "turnFailed";
}

/**
 * Whether "Send igen" makes sense: the ticket has a redispatchable issue and its agent is running
 * and idle (the dispatcher ignores a redispatch otherwise).
 *
 * | issue          | agent status | canRedispatch |
 * |----------------|--------------|---------------|
 * | turnFailed     | idle         | true          |
 * | deliveryFailed | idle         | true          |
 * | turnFailed     | running      | false         |
 * | turnFailed     | exited       | false         |
 * | turnFailed     | (no agent)   | false         |
 * | notSubmitted   | idle         | false         |
 * | null           | idle         | false         |
 */
export function canRedispatch(t: TicketSummary, agent: AgentInfo | null): boolean {
  return hasRedispatchIssue(t) && agent !== null && agent.status.kind === "idle";
}

/**
 * Whether "Bed om aflevering" can work: the ticket is in progress (or a waiting parent, whose
 * wake line the dispatcher then types again; review 6a W1) and its agent is idle (the dispatcher
 * only types into an idle agent; `request_submission` also needs it running).
 *
 * | state      | agent status | canRequestSubmission |
 * |------------|--------------|----------------------|
 * | inProgress | idle         | true                 |
 * | waiting    | idle         | true                 |
 * | inProgress | running      | false                |
 * | waiting    | running      | false                |
 * | inProgress | exited       | false                |
 * | inProgress | (no agent)   | false                |
 * | review     | idle         | false                |
 * | assigned   | idle         | false                |
 */
export function canRequestSubmission(t: TicketSummary, agent: AgentInfo | null): boolean {
  return (
    (t.state === "inProgress" || t.state === "waiting") && isLive(agent) && agent.status.kind === "idle"
  );
}

/**
 * Whether "Læg tilbage" may put a waiting parent back in the backlog (review 6a N3: the user's
 * `unassign_ticket`; the parent loses its agent, its children are untouched). Assigning or
 * handing over a waiting ticket stays refused.
 *
 * | state      | assignee | canReturnWaiting |
 * |------------|----------|------------------|
 * | waiting    | A        | true             |
 * | waiting    | –        | false            |
 * | inProgress | A        | false            |
 * | assigned   | A        | false            |
 * | backlog    | –        | false            |
 */
export function canReturnWaiting(t: TicketSummary): boolean {
  return t.state === "waiting" && t.assigneeAgentId !== null;
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

// --- parents, children and blockers (step 6a) ---------------------------------------------------
// Derived from the full ticket list: the payload only carries `parentId` and `blockedBy` (full ids).
// The backend counts everything but Done as an open child (a deleted child is simply gone).

/**
 * The parent of `t` in `all`, or null (no parent, or the parent is not in the list).
 *
 * | t.parentId | parent in all | parentOf |
 * |------------|---------------|----------|
 * | null       | -             | null     |
 * | "p"        | yes           | p        |
 * | "p"        | no            | null     |
 */
export function parentOf(t: TicketSummary, all: readonly TicketSummary[]): TicketSummary | null {
  if (t.parentId === null) return null;
  return all.find((x) => x.id === t.parentId) ?? null;
}

/**
 * The children of the ticket with `id`, oldest `createdAt` first (ties keep input order).
 *
 * | all (id: parentId)       | childrenOf("p") |
 * |--------------------------|-----------------|
 * | a: p, b: q, c: p         | [a, c]          |
 * | a: null                  | []              |
 */
export function childrenOf(id: string, all: readonly TicketSummary[]): TicketSummary[] {
  return all.filter((t) => t.parentId === id).sort((a, b) => a.createdAt - b.createdAt);
}

/**
 * Finished children out of all children. Anything but `done` counts as open (like the backend).
 *
 * | children of p              | progressOf("p")         |
 * |----------------------------|-------------------------|
 * | none                       | { done: 0, total: 0 }   |
 * | done, review, backlog      | { done: 1, total: 3 }   |
 * | done (a deleted one is gone)| { done: 1, total: 1 }  |
 */
export function progressOf(
  id: string,
  all: readonly TicketSummary[],
): { done: number; total: number } {
  let done = 0;
  let total = 0;
  for (const t of all) {
    if (t.parentId !== id) continue;
    total++;
    if (t.state === "done") done++;
  }
  return { done, total };
}

/**
 * The tickets in `t.blockedBy` that exist in `all` and are not done, in `blockedBy` order. An
 * unknown id is a deleted ticket and does not block.
 *
 * | blockedBy                  | blockersOf        |
 * |----------------------------|-------------------|
 * | []                         | []                |
 * | [x (done)]                 | []                |
 * | [x (review), gone]         | [x]               |
 */
export function blockersOf(t: TicketSummary, all: readonly TicketSummary[]): TicketSummary[] {
  const out: TicketSummary[] = [];
  for (const id of t.blockedBy) {
    const b = all.find((x) => x.id === id);
    if (b !== undefined && b.state !== "done") out.push(b);
  }
  return out;
}

/** Whether a blocker is still open (see `blockersOf`). */
export function isBlocked(t: TicketSummary, all: readonly TicketSummary[]): boolean {
  return blockersOf(t, all).length > 0;
}

/**
 * Whether the backlog hint may offer the ticket: it can be dragged/assigned and nothing blocks it.
 *
 * | canDrag | blocked | isDeliverable |
 * |---------|---------|---------------|
 * | true    | no      | true          |
 * | true    | yes     | false         |
 * | false   | no      | false         |
 */
export function isDeliverable(t: TicketSummary, all: readonly TicketSummary[]): boolean {
  return canDrag(t) && !isBlocked(t, all);
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
 * | "agents"       | "agents"          |
 * | "Tickets"      | null              |
 * | null           | null              |
 */
export function parseWorkplaceTab(tab: string | null): WorkplaceTab | null {
  return tab === "tickets" || tab === "permissions" || tab === "diagnostics" || tab === "agents"
    ? tab
    : null;
}

// --- review between agents, coordination (plan5 A.6/A.7) --------------------------------------

/**
 * The review round shown on the card (`reviewRound` counts rejections so far; the review file
 * says "Runde r+1 af max"), capped at the maximum once escalated. `max` is the workspace's
 * `maxReviewRounds` (`appInfo.rules.maxReviewRounds`; 3 is Rust's default before appInfo loads).
 *
 * | reviewRound | max | reviewRoundText   |
 * |-------------|-----|-------------------|
 * | 0           | 3   | "Runde 1 af 3"    |
 * | 2           | 3   | "Runde 3 af 3"    |
 * | 7           | 3   | "Runde 3 af 3"    |
 * | 1           | 2   | "Runde 2 af 2"    |
 */
export function reviewRoundText(t: Pick<TicketSummary, "reviewRound">, max = 3): string {
  return `Runde ${Math.min(t.reviewRound + 1, max)} af ${max}`;
}

/**
 * Agents the user may pick as reviewer of `t` (`assign_reviewer`): running, with the reviewer
 * role, not the sender, not already its reviewer; fewest open reviews first, then oldest.
 *
 * | agent                                   | candidate |
 * |-----------------------------------------|-----------|
 * | reviewer role, idle, not the sender     | yes       |
 * | reviewer role, exited                   | no        |
 * | reviewer role, the sender (assignee)    | no        |
 * | reviewer role, already `reviewerAgentId`| no        |
 * | coder only                              | no        |
 */
export function reviewerCandidates(
  agents: readonly AgentInfo[],
  t: Pick<TicketSummary, "assigneeAgentId" | "reviewerAgentId">,
): AgentInfo[] {
  return agents
    .filter(
      (a) =>
        isLive(a) &&
        a.roles.includes("reviewer") &&
        a.id !== t.assigneeAgentId &&
        a.id !== t.reviewerAgentId,
    )
    .sort((a, b) => a.openReviews - b.openReviews || a.createdAt - b.createdAt);
}

/**
 * Tickets the agent reviews right now: in review with `reviewerAgentId === agentId`, oldest
 * first (input order).
 */
export function reviewsFor(tickets: readonly TicketSummary[], agentId: string): TicketSummary[] {
  return tickets.filter((t) => t.state === "review" && t.reviewerAgentId === agentId);
}

/**
 * A ticket on a staff agent, or on an agent without a work role, is a "koordineringsopgave"
 * (plan5 A.7; review 5c W1, mirrors `TicketDelivery::for_agent` in Rust).
 *
 * | agent seat | work role (coder/researcher/debugger) | isCoordinationTask |
 * |------------|---------------------------------------|--------------------|
 * | staff      | any                                   | true               |
 * | work       | yes                                   | false              |
 * | work       | no                                    | true               |
 * | no agent   | -                                     | false              |
 */
export function isCoordinationTask(agent: Pick<AgentInfo, "seatKind" | "roles"> | null): boolean {
  if (agent === null) return false;
  // Same set as `WORK_ROLES` in roles.ts; kept local so this module transpiles on its own.
  const hasWorkRole = agent.roles.some((r) => r === "coder" || r === "researcher" || r === "debugger");
  return agent.seatKind === "staff" || !hasWorkRole;
}

/** Tooltip on "Skift model"/"Skift effort" while switching is not possible. */
export const AGENT_BUSY_TEXT = "Agenten arbejder — vent til den er idle uden ticket i gang";

/**
 * Why "Skift model"/"Skift effort" is disabled, or null when the agent can be restarted (mirrors
 * the backend's `AgentWorking` rule: idle and no ticket in progress).
 *
 * | status   | currentTicketId | switchBlocked       |
 * |----------|-----------------|---------------------|
 * | idle     | null            | null                |
 * | idle     | "t1"            | AGENT_BUSY_TEXT     |
 * | thinking | null            | AGENT_BUSY_TEXT     |
 * | starting | null            | AGENT_BUSY_TEXT     |
 * | exited   | null            | "Agenten kører ikke"|
 */
export function switchBlocked(agent: Pick<AgentInfo, "status" | "currentTicketId">): string | null {
  if (agent.status.kind === "exited") return "Agenten kører ikke";
  if (agent.status.kind !== "idle" || agent.currentTicketId !== null) return AGENT_BUSY_TEXT;
  return null;
}

/** The agent's waiting parents (step 6a; not part of `queueLength`, review 6a W2). */
export function waitingCount(tickets: readonly TicketSummary[], agentId: string): number {
  return tickets.filter((t) => t.state === "waiting" && t.assigneeAgentId === agentId).length;
}

/**
 * What "Flyt til projekt…" puts back in the backlog after the confirmation (review 6a W2): the
 * agent's queued tickets and its waiting parents; null when there are none (no confirmation).
 *
 * | queue | waiting | project | movePendingText                                          |
 * |-------|---------|---------|----------------------------------------------------------|
 * | 0     | 0       | "p"     | null                                                     |
 * | 1     | 0       | "p"     | "1 ticket i kø til «p» lægges tilbage i Backlog"          |
 * | 2     | 0       | "p"     | "2 tickets i kø til «p» lægges tilbage i Backlog"         |
 * | 0     | 1       | "p"     | "1 ventende ticket til «p» lægges tilbage i Backlog"      |
 * | 2     | 1       | "p"     | "2 tickets i kø og 1 ventende til «p» lægges tilbage i Backlog" |
 * | 1     | 2       | null    | "1 ticket i kø og 2 ventende lægges tilbage i Backlog"    |
 */
export function movePendingText(queue: number, waiting: number, project: string | null): string | null {
  if (queue === 0 && waiting === 0) return null;
  const tickets = (n: number) => `${n} ${n === 1 ? "ticket" : "tickets"}`;
  const what =
    queue > 0 && waiting > 0
      ? `${tickets(queue)} i kø og ${waiting} ventende`
      : queue > 0
        ? `${tickets(queue)} i kø`
        : `${waiting} ventende ${waiting === 1 ? "ticket" : "tickets"}`;
  const where = project === null ? "" : ` til «${project}»`;
  return `${what}${where} lægges tilbage i Backlog`;
}

// --- ticket types, playbooks, checks and git (step 6b) -----------------------------------------
// The payload carries `kind`, `playbookStartedAt`, `checks` and `git` (see `TicketSummary`); the
// playbook names come from `appInfo.playbookKinds`. Nothing here starts anything by itself: "Start
// forløb" is always a click.

/**
 * The ticket type as shown in badges and the dropdown.
 *
 * | kind      | kindLabel   |
 * |-----------|-------------|
 * | null      | "Opgave"    |
 * | "feature" | "Feature"   |
 * | "bug"     | "Bug"       |
 * | "docs"    | "docs"      |
 */
export function kindLabel(kind: string | null): string {
  if (kind === null) return "Opgave";
  if (kind === "feature") return "Feature";
  if (kind === "bug") return "Bug";
  return kind;
}

export interface KindOption {
  /** `null` = plain task (the backend stores no kind). */
  value: string | null;
  label: string;
}

/**
 * The "Type" dropdown: Opgave, Feature, Bug, then the other playbook names sorted. `feature` and
 * `bug` are always offered (they are built in); `task` is never a playbook name.
 *
 * | playbookKinds          | values                                  |
 * |------------------------|-----------------------------------------|
 * | []                     | [null, "feature", "bug"]                |
 * | ["bug", "feature"]     | [null, "feature", "bug"]                |
 * | ["feature", "docs", "api", "docs"] | [null, "feature", "bug", "api", "docs"] |
 * | ["task"]               | [null, "feature", "bug"]                |
 */
export function KIND_OPTIONS(playbookKinds: readonly string[]): KindOption[] {
  const extra = [...new Set(playbookKinds)]
    .filter((k) => k !== "" && k !== "task" && k !== "feature" && k !== "bug")
    .sort();
  return [null, "feature", "bug", ...extra].map((value) => ({ value, label: kindLabel(value) }));
}

/**
 * Whether "Start forløb" may be offered (mirrors `create_playbook_children`/`start_playbook`):
 * a Backlog ticket whose kind has a playbook, not started before and without children.
 *
 * | state   | kind      | in playbookKinds | started | children | canStartPlaybook |
 * |---------|-----------|------------------|---------|----------|------------------|
 * | backlog | "feature" | yes              | no      | none     | true             |
 * | backlog | null      | -                | no      | none     | false            |
 * | backlog | "docs"    | no               | no      | none     | false            |
 * | backlog | "feature" | yes              | yes     | none     | false            |
 * | backlog | "feature" | yes              | no      | some     | false            |
 * | review  | "feature" | yes              | no      | none     | false            |
 * | rejected| "feature" | yes              | no      | none     | false            |
 */
export function canStartPlaybook(
  t: TicketSummary,
  all: readonly TicketSummary[],
  playbookKinds: readonly string[],
): boolean {
  return (
    t.state === "backlog" &&
    t.kind !== null &&
    playbookKinds.includes(t.kind) &&
    t.playbookStartedAt === null &&
    !all.some((x) => x.parentId === t.id)
  );
}

/**
 * Whether the ticket is a flow parent: "Start forløb" ran and the ticket has no owner (the user
 * started it). Its review has no sender and no reviewer agent; only the user decides.
 *
 * | playbookStartedAt | assigneeAgentId | isFlowParent |
 * |-------------------|-----------------|--------------|
 * | 1700000000000     | null            | true         |
 * | 1700000000000     | "A"             | false        |
 * | null              | null            | false        |
 */
export function isFlowParent(t: Pick<TicketSummary, "playbookStartedAt" | "assigneeAgentId">): boolean {
  return t.playbookStartedAt !== null && t.assigneeAgentId === null;
}

export interface ChecksBadge {
  text: string;
  /** Full Tailwind class string. */
  cls: string;
  title: string;
}

/**
 * The "Tjek" badge. `skipped` (nothing ran) and no checks at all show nothing.
 *
 * | checks.state | failed  | text                  | title                |
 * |--------------|---------|-----------------------|----------------------|
 * | null         | -       | null                  |                      |
 * | skipped      | -       | null                  |                      |
 * | pending      | -       | "Tjek: kører"         | "Projekt-tjek"       |
 * | passed       | -       | "Tjek: OK"            | "Projekt-tjek"       |
 * | failed       | "tests" | "Tjek: FEJL (tests)"  | "tests"              |
 * | failed       | null    | "Tjek: FEJL"          | "Projekt-tjek"       |
 */
export function checksBadge(t: Pick<TicketSummary, "checks">): ChecksBadge | null {
  const c = t.checks;
  if (c === null) return null;
  switch (c.state) {
    case "pending":
      return {
        text: "Tjek: kører",
        cls: "bg-sky-500/15 text-sky-800 dark:text-sky-200",
        title: "Projekt-tjek",
      };
    case "passed":
      return {
        text: "Tjek: OK",
        cls: "bg-emerald-500/15 text-emerald-800 dark:text-emerald-200",
        title: "Projekt-tjek",
      };
    case "failed":
      return {
        text: c.failed === null ? "Tjek: FEJL" : `Tjek: FEJL (${c.failed})`,
        cls: "bg-rose-500/15 text-rose-700 dark:text-rose-300",
        title: c.failed ?? "Projekt-tjek",
      };
    case "skipped":
      return null;
  }
}

/**
 * The review card's check line (same states as `checksBadge`; "se rapport" points at the «Tjek»
 * report below).
 *
 * | checks.state | failed  | checksLineText                     |
 * |--------------|---------|------------------------------------|
 * | null/skipped | -       | null                               |
 * | pending      | -       | "Tjek: kører…"                     |
 * | passed       | -       | "Tjek: OK"                         |
 * | failed       | "tests" | "Tjek: FEJL (tests) · se rapport"  |
 * | failed       | null    | "Tjek: FEJL · se rapport"          |
 */
export function checksLineText(t: Pick<TicketSummary, "checks">): string | null {
  const c = t.checks;
  if (c === null) return null;
  switch (c.state) {
    case "pending":
      return "Tjek: kører…";
    case "passed":
      return "Tjek: OK";
    case "failed":
      return `${c.failed === null ? "Tjek: FEJL" : `Tjek: FEJL (${c.failed})`} · se rapport`;
    case "skipped":
      return null;
  }
}

/** Why "Vælg reviewer…" is off while the checks run (same text as the backend's refusal). */
export const CHECKS_RUNNING_REVIEWER = "Tjek kører; vælg reviewer når det er færdigt";

/**
 * Review6b W7: a manual reviewer choice waits while the project checks run with the gate on (a
 * failing check would move the ticket away from that reviewer); the backend refuses it too.
 *
 * | checks.state | checksGate | reviewerChoiceBlocked     |
 * |--------------|------------|---------------------------|
 * | pending      | true       | CHECKS_RUNNING_REVIEWER   |
 * | pending      | false      | null                      |
 * | other/null   | any        | null                      |
 */
export function reviewerChoiceBlocked(t: Pick<TicketSummary, "checks">, checksGate: boolean): string | null {
  return checksGate && t.checks?.state === "pending" ? CHECKS_RUNNING_REVIEWER : null;
}

/**
 * The note's branch badge: "⎇ branch", title = worktree folder, else the repository.
 *
 * | git                                   | gitBadge                                      |
 * |---------------------------------------|-----------------------------------------------|
 * | null                                  | null                                          |
 * | branch ticket/ab12cd34, worktree "W"  | { text: "⎇ ticket/ab12cd34", title: "W" }     |
 * | branch ticket/ab12cd34, worktree null | { text: "⎇ ticket/ab12cd34", title: <repo> }  |
 */
export function gitBadge(t: Pick<TicketSummary, "git">): { text: string; title: string } | null {
  const g = t.git;
  if (g === null) return null;
  return { text: `⎇ ${g.branch}`, title: g.worktree ?? g.repo };
}

/** The review card's git line: "Branch {branch} fra {base}". */
export function gitLineText(g: Pick<TicketGit, "branch" | "base">): string {
  return `Branch ${g.branch} fra ${g.base}`;
}

/** The confirmation after "Start forløb": "Forløb startet: n del-tickets", then the backend's notes. */
export function playbookStartedText(children: number, notes: readonly string[]): string {
  const head = `Forløb startet: ${children} ${children === 1 ? "del-ticket" : "del-tickets"}`;
  return notes.length === 0 ? head : `${head} · ${notes.join(" · ")}`;
}

/**
 * An agent's folder, shortened when it is a ticket worktree (`<projekt>/.mira-bots/wt/<kort>`,
 * any slash style): "…/.mira-bots/wt/<kort>" (a subfolder is kept). Other paths are unchanged.
 *
 * | cwd                                              | shortCwd                          |
 * |--------------------------------------------------|-----------------------------------|
 * | "C:\\p\\app\\.mira-bots\\wt\\ab12cd34"        | "…/.mira-bots/wt/ab12cd34"        |
 * | "/home/u/p/app/.mira-bots/wt/ab12cd34/"          | "…/.mira-bots/wt/ab12cd34"        |
 * | "/home/u/p/app/.mira-bots/wt/ab12cd34/src/x"     | "…/.mira-bots/wt/ab12cd34/src/x"  |
 * | "/home/u/p/app"                                  | "/home/u/p/app"                   |
 * | "/home/u/.mira-bots/wt/not-a-short-id"           | unchanged                         |
 */
export function shortCwd(cwd: string): string {
  const m = /[\\/]\.mira-bots[\\/]wt[\\/]([0-9a-f]{8})(?:[\\/](.*?))?[\\/]*$/.exec(cwd);
  if (m === null) return cwd;
  const rest = m[2] === undefined || m[2] === "" ? "" : `/${m[2].replace(/\\/g, "/")}`;
  return `…/.mira-bots/wt/${m[1]}${rest}`;
}

// --- tidslinje (step 6d, plan A.10 / C6d.6) --------------------------------------------------

/** Kilden til en linje i tidslinjen; `note` er en historiknote som ingen anden kind genkender. */
export type TimelineKind =
  | "created"
  | "state"
  | "note"
  | "report"
  | "playbook"
  | "checks"
  | "git"
  | "writeBack"
  | "watch"
  | "session";

export interface TimelineEntry {
  /** Milliseconds since the Unix epoch. */
  at: number;
  kind: TimelineKind;
  text: string;
  /** Hvem: "dig", "agenten", "systemet" (historik) eller "appen"/"agenten"/"dig" (rapporter). */
  by: string;
}

/** Rapportforfatter i tidslinjen (som `ReportsSection`): appen, agenten eller dig. */
const REPORT_AUTHOR_LABEL: Record<TicketReport["author"]["kind"], string> = {
  system: "appen",
  agent: "agenten",
  user: "dig",
};

/** Titelpræfiks på appens tjek-rapport (`CHECKS_REPORT_TITLE` i Rust). */
const CHECKS_REPORT_TITLE = "Tjek";
/** Titel på appens rapport med ticketens git-ændringer (`CHANGES_REPORT_TITLE` i Rust). */
const CHANGES_REPORT_TITLE = "Ændringer";
/** Appens afvisningspræfiks når et tjek fejlede (`CHECKS_REJECT_PREFIX` i Rust). */
const CHECKS_REJECT_PREFIX = "afvist af appen";

/**
 * Genkendelse af System-noterne (ordret fra `config.rs`, `dispatcher.rs`, `write_back.rs`):
 * første match vinder; ukendte noter er `note`.
 *
 * | note begynder med                                                        | kind        |
 * |--------------------------------------------------------------------------|-------------|
 * | "startet af vagten", "vagt:"                                             | "watch"     |
 * | "worktree oprettet"                                                      | "git"       |
 * | "ny session", "session fortsat"                                          | "session"   |
 * | "meldt tilbage", "issue #", "kunne ikke melde tilbage", "resultat skrevet", "tilbagemelding" | "writeBack" |
 * | "tjek" (uanset store/små), "afvist af appen"                             | "checks"    |
 * | andet                                                                    | "note"      |
 */
const NOTE_KINDS: ReadonlyArray<readonly [TimelineKind, readonly string[]]> = [
  ["watch", ["startet af vagten", "vagt:"]],
  ["git", ["worktree oprettet"]],
  ["session", ["ny session", "session fortsat"]],
  ["writeBack", ["meldt tilbage", "issue #", "kunne ikke melde tilbage", "resultat skrevet", "tilbagemelding"]],
  ["checks", ["tjek", CHECKS_REJECT_PREFIX]],
];

function noteKind(note: string): TimelineKind {
  const lower = note.toLowerCase();
  for (const [kind, prefixes] of NOTE_KINDS) {
    if (prefixes.some((p) => lower.startsWith(p))) return kind;
  }
  return "note";
}

/** Historikpost → linje: oprettelse, tilstandsskift (med note bagved) eller note (`from == to`). */
function historyEntry(h: TicketHistoryEntry): TimelineEntry {
  const by = ACTOR_LABEL[h.by];
  const note = h.note === null ? "" : h.note.trim();
  if (h.from === null) {
    const text = `oprettet i ${STATE_LABEL[h.to]}`;
    return { at: h.at, kind: "created", text: note === "" ? text : `${text} — ${note}`, by };
  }
  if (h.from === h.to) {
    return { at: h.at, kind: note === "" ? "note" : noteKind(note), text: note === "" ? "(note)" : note, by };
  }
  const move = `${STATE_LABEL[h.from]} → ${STATE_LABEL[h.to]}`;
  // Appens afvisning efter et fejlet tjek hører til tjekkene, ikke til de almindelige skift.
  const kind: TimelineKind = note.toLowerCase().startsWith(CHECKS_REJECT_PREFIX) ? "checks" : "state";
  return { at: h.at, kind, text: note === "" ? move : `${move} — ${note}`, by };
}

function isChecksReport(r: TicketReport): boolean {
  return r.author.kind === "system" && r.title.startsWith(CHECKS_REPORT_TITLE);
}

/** Rapport → linje "rapport {id}: {titel}"; appens «Tjek …» er `checks`, «Ændringer» er `git`. */
function reportEntry(r: TicketReport): TimelineEntry {
  const kind: TimelineKind = isChecksReport(r)
    ? "checks"
    : r.author.kind === "system" && r.title === CHANGES_REPORT_TITLE
      ? "git"
      : "report";
  return { at: r.createdAt, kind, text: `rapport ${r.id}: ${r.title}`, by: REPORT_AUTHOR_LABEL[r.author.kind] };
}

/**
 * Ticketens tidslinje, ældst først, sammensat af det `get_ticket` allerede har: historik (oprettelse,
 * tilstandsskift, noter — System-noterne genkendes på teksten, se `noteKind`), rapporter (titel og
 * forfatter, aldrig brødtekst), `playbookStartedAt` ("forløb startet ({type}), n del-tickets" når
 * `children` er givet, ellers uden antal) og `checks.startedAt` ("tjek kører" mens tjekkene er
 * `pending` og ingen «Tjek»-rapport er kommet efter starten). `git` uden "worktree oprettet"-note
 * udelades: der er ingen tid at opfinde. Stabil sortering på `at`, dernæst kilde (historik,
 * rapporter, afledte); en afledt linje med samme `at` og tekst som en historiklinje fjernes.
 *
 * | ticket                                             | buildTimeline                                   |
 * |----------------------------------------------------|-------------------------------------------------|
 * | kun oprettet                                       | [created "oprettet i Backlog" (dig)]            |
 * | playbookStartedAt, children 2                      | … playbook "forløb startet (Feature), 2 del-tickets" (systemet) |
 * | note "startet af vagten (forløb «bug»)"            | … watch (systemet)                              |
 * | rapport "Tjek: 1 fejlede" af system                | … checks "rapport 02: Tjek: 1 fejlede" (appen)  |
 */
export function buildTimeline(t: Ticket, opts: { children?: readonly TicketSummary[] } = {}): TimelineEntry[] {
  const fromHistory = t.history.map(historyEntry);
  const fromReports = t.reports.map(reportEntry);
  const derived: TimelineEntry[] = [];
  if (t.playbookStartedAt !== null) {
    const head = `forløb startet (${kindLabel(t.kind)})`;
    const children = opts.children;
    const text =
      children === undefined
        ? head
        : `${head}, ${children.length} ${children.length === 1 ? "del-ticket" : "del-tickets"}`;
    derived.push({ at: t.playbookStartedAt, kind: "playbook", text, by: ACTOR_LABEL.system });
  }
  const checks = t.checks;
  if (checks !== null && checks.state === "pending") {
    const reported = t.reports.some((r) => isChecksReport(r) && r.createdAt >= checks.startedAt);
    if (!reported) derived.push({ at: checks.startedAt, kind: "checks", text: "tjek kører", by: ACTOR_LABEL.system });
  }
  const kept = derived.filter((d) => !fromHistory.some((h) => h.at === d.at && h.text === d.text));
  const all = [...fromHistory, ...fromReports, ...kept].map((entry, index) => ({ entry, index }));
  all.sort((a, b) => a.entry.at - b.entry.at || a.index - b.index);
  return all.map((x) => x.entry);
}

/**
 * Kopiértekst: overskrift "{kort-id} {titel}", så en linje pr. post "{formatAt} · {text} ({by})".
 *
 * | entries           | timelineText                                              |
 * |-------------------|-----------------------------------------------------------|
 * | []                | "ab12cd34 Titel"                                          |
 * | [created, state]  | "ab12cd34 Titel\n01.10. 14.05 · oprettet i Backlog (dig)\n…" |
 */
export function timelineText(entries: readonly TimelineEntry[], t: Pick<Ticket, "id" | "title">): string {
  const lines = entries.map((e) => `${formatAt(e.at)} · ${e.text} (${e.by})`);
  return [`${shortId(t.id)} ${t.title}`, ...lines].join("\n");
}

/**
 * Antallet i foldens overskrift før `get_ticket` er hentet (tidslinjen selv kan afvige lidt):
 * historik + rapporter + 1 for et startet forløb.
 *
 * | historyLen | reportCount | playbookStartedAt | timelineCount |
 * |------------|-------------|-------------------|---------------|
 * | 1          | 0           | null              | 1             |
 * | 4          | 2           | 1700000000000     | 7             |
 */
export function timelineCount(t: Pick<TicketSummary, "historyLen" | "reportCount" | "playbookStartedAt">): number {
  return t.historyLen + t.reportCount + (t.playbookStartedAt === null ? 0 : 1);
}

/** Lokal kalenderdag som tal (år·10000 + måned·100 + dag) til sammenligning. */
function localDay(ms: number): number {
  const d = new Date(ms);
  return d.getFullYear() * 10_000 + (d.getMonth() + 1) * 100 + d.getDate();
}

/** Lokalt klokkeslæt "14.05" (da-DK). */
function clockText(ms: number): string {
  return new Date(ms).toLocaleTimeString("da-DK", { hour: "2-digit", minute: "2-digit" });
}

/**
 * Relativ tid på dansk ud fra `now` (uret må gerne være bagud: fremtid = "lige nu").
 *
 * | now − ms                        | relativeText        |
 * |---------------------------------|---------------------|
 * | < 60 s (eller negativ)          | "lige nu"           |
 * | 3 min                           | "for 3 min siden"   |
 * | 2 t, samme lokale dag           | "for 2 t siden"     |
 * | i går (lokal kalenderdag)       | "i går 14.05"       |
 * | ældre                           | formatAt(ms)        |
 */
export function relativeText(ms: number, now: number): string {
  const diff = Math.max(0, now - ms);
  if (diff < 60_000) return "lige nu";
  if (diff < 3_600_000) return `for ${Math.floor(diff / 60_000)} min siden`;
  const day = localDay(ms);
  if (day === localDay(now)) return `for ${Math.floor(diff / 3_600_000)} t siden`;
  const n = new Date(now);
  // Kalenderdagen før `now` via datodelene (ikke −24 t: sommertidsskift giver 23/25-timers dage).
  const yesterday = localDay(new Date(n.getFullYear(), n.getMonth(), n.getDate() - 1).getTime());
  if (day === yesterday) return `i går ${clockText(ms)}`;
  return formatAt(ms);
}
