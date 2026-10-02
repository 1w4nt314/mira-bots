// Pure helpers for projects (plan4b C4b.8): name rules (the same as `projects.rs`), the
// assignment rule of a ticket to an agent, the "n agenter, ingen koordinator" hint, counts and
// the ticket list's project filter. No DOM; only type imports, so it transpiles on its own
// (scripts/test-projects.mjs).
import type { AgentInfo, ProjectRef, Role, TicketSummary } from "./types";

export const PROJECT_NAME_MAX = 64;
/** localStorage key (via persist.ts) of the ticket list's project filter. */
export const PROJECT_FILTER_KEY = "mira-bots.tickets.projectFilter";

export type ProjectFilter = "all" | "none" | { id: string };

const INVALID_CHARS = ["<", ">", ":", '"', "/", "\\", "|", "?", "*"];
/** Device names Windows reserves in every folder, also with an extension (`NUL.txt`). */
const RESERVED_STEMS = new Set([
  "con", "prn", "aux", "nul",
  "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9",
  "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
  "com¹", "com²", "com³", "lpt¹", "lpt²", "lpt³",
]);

/**
 * null when `name` is a valid project folder name, otherwise the Danish error (the same text and
 * the same order of checks as `validate_project_id` in Rust; the Windows rules on every host).
 */
export function validateProjectName(name: string): string | null {
  const bad = (reason: string) => `Projektnavnet «${name}» er ugyldigt: ${reason}`;
  const chars = Array.from(name);
  if (chars.length === 0) return bad("tomt");
  if (chars.length > PROJECT_NAME_MAX) return bad("må højst være 64 tegn");
  if (chars.some((c) => INVALID_CHARS.includes(c))) {
    return bad('indeholder et ugyldigt tegn (< > : " / \\ | ? *)');
  }
  if (chars.some((c) => c.charCodeAt(0) < 0x20 || c.charCodeAt(0) === 0x7f)) {
    return bad("indeholder et kontroltegn");
  }
  if (chars.every((c) => c === ".")) return bad("må ikke kun bestå af punktummer");
  if (name.startsWith(".")) return bad("må ikke begynde med punktum");
  if (name.startsWith(" ") || name.endsWith(" ")) {
    return bad("må ikke begynde eller slutte med mellemrum");
  }
  if (name.endsWith(".")) return bad("må ikke slutte med punktum");
  const stem = name.split(".")[0].replace(/ +$/, "").toLowerCase();
  if (RESERVED_STEMS.has(stem)) return bad("er et reserveret navn i Windows");
  return null;
}

/** The id of an existing project; null for none or a project still to be created. */
export function projectIdOf(ref: ProjectRef | null): string | null {
  return typeof ref === "string" ? ref : null;
}

/** The project's name: the id, or the name of a project still to be created. */
export function projectName(ref: ProjectRef | null): string | null {
  if (ref === null) return null;
  return typeof ref === "string" ? ref : ref.new;
}

/** Short label: "a", "+navn" (created at assignment) or "uden projekt". */
export function projectLabel(ref: ProjectRef | null): string {
  if (ref === null) return "uden projekt";
  return typeof ref === "string" ? ref : `+${ref.new}`;
}

/**
 * Unicode lower-casing without locale, like Rust's `str::to_lowercase` in `projects::same_id`
 * (NTFS folds "Økonomi" and "økonomi" too).
 */
function foldCase(s: string): string {
  return s.toLowerCase();
}

/** Project ids compare case-insensitively (Unicode); null never equals anything. */
export function sameProjectId(a: string | null, b: string | null): boolean {
  return a !== null && b !== null && foldCase(a) === foldCase(b);
}

export type AssignmentIssue =
  | { kind: "needsProject" }
  | { kind: "wrongProject"; agentProject: string; ticketProject: string };

/**
 * Why `agent` cannot take ticket `t` as it is (mirrors `assignment_target` in Rust): a staff
 * seat takes any ticket; a work agent only one of its own project (`needsProject` when the
 * ticket has none yet: "Hvilket projekt?"), a `{ new }` with the agent's project name included.
 */
export function assignmentIssue(
  t: Pick<TicketSummary, "project">,
  agent: Pick<AgentInfo, "seatKind" | "project">,
): AssignmentIssue | null {
  if (agent.seatKind === "staff") return null;
  const ticketProject = projectName(t.project);
  if (ticketProject === null) return { kind: "needsProject" };
  if (sameProjectId(ticketProject, agent.project)) return null;
  return { kind: "wrongProject", agentProject: agent.project ?? "", ticketProject };
}

/** The explanation shown for a `wrongProject` issue (AssignMenu title, drag error). */
export function wrongProjectText(agentProject: string, ticketProject: string): string {
  return `Agenten står i projekt «${agentProject}»; ticketen hører til «${ticketProject}». Flyt agenten fra dens terminalpanel («Flyt til projekt…»), eller vælg en anden agent.`;
}

const isLive = (a: Pick<AgentInfo, "status">) => a.status.kind !== "exited";

/** The running work agents in project `projectId`. */
export function liveWorkAgentsIn(agents: readonly AgentInfo[], projectId: string): AgentInfo[] {
  return agents.filter(
    (a) => isLive(a) && a.seatKind === "work" && sameProjectId(a.project, projectId),
  );
}

const COORDINATOR: Role = "coordinator";

/** Whether a running agent (any seat) has the coordinator role. */
export function hasLiveCoordinator(agents: readonly AgentInfo[]): boolean {
  return agents.some((a) => isLive(a) && a.roles.includes(COORDINATOR));
}

/** "n agenter, ingen koordinator" when ≥ 2 work agents share the project and none coordinates. */
export function coordinatorHint(agents: readonly AgentInfo[], projectId: string): string | null {
  const n = liveWorkAgentsIn(agents, projectId).length;
  if (n < 2 || hasLiveCoordinator(agents)) return null;
  return `${n} agenter, ingen koordinator`;
}

/** A work agent that stands idle in a project while Backlog tickets without an owner wait. */
export interface BacklogHint {
  agent: AgentInfo;
  /** The unowned, unblocked Backlog tickets of the project, oldest first. */
  waiting: TicketSummary[];
  /** The oldest deliverable ticket: what "Tildel til <agent>" assigns. */
  next: TicketSummary;
  /** "coder-01 er ledig: 2 tickets uden ejer". */
  text: string;
}

/**
 * Ids in `t.blockedBy` that exist in `tickets` and are not done. A private copy of
 * `blockersOf` in tickets.ts: this module is transpiled alone by scripts/test-projects.mjs, so
 * it cannot import it (both are tested).
 */
function openBlockers(t: Pick<TicketSummary, "blockedBy">, tickets: readonly TicketSummary[]): string[] {
  return t.blockedBy.filter((id) => tickets.some((x) => x.id === id && x.state !== "done"));
}

/**
 * The backlog hint of project `projectId` (step 6a): the first running work agent of the project
 * that is idle with no ticket in progress and an empty queue, and the project's Backlog (or
 * rejected) tickets without an assignee that nothing blocks. Null when either is missing.
 * Tickets without a project never count. Nothing is assigned here; the UI offers a button.
 */
export function backlogHint(
  agents: readonly AgentInfo[],
  tickets: readonly TicketSummary[],
  projectId: string,
): BacklogHint | null {
  const agent = liveWorkAgentsIn(agents, projectId).find(
    (a) => a.status.kind === "idle" && a.currentTicketId === null && a.queueLength === 0,
  );
  if (agent === undefined) return null;
  const waiting = tickets
    .filter(
      (t) =>
        t.assigneeAgentId === null &&
        (t.state === "backlog" || t.state === "rejected") &&
        sameProjectId(projectName(t.project), projectId) &&
        openBlockers(t, tickets).length === 0,
    )
    .sort((a, b) => a.createdAt - b.createdAt);
  const next = waiting[0];
  if (next === undefined) return null;
  const n = waiting.length;
  return { agent, waiting, next, text: `${agent.name} er ledig: ${n} ${n === 1 ? "ticket" : "tickets"} uden ejer` };
}

/**
 * The office hints of one work agent (project badge and staff sign): `coordinator` is the
 * "n agenter, ingen koordinator" text, `idle` the backlog hint text when this agent is the idle
 * one. Both null never happens; callers use null instead of an empty hint.
 */
export interface SeatHint {
  coordinator: string | null;
  idle: string | null;
}

/**
 * Running work agents and tickets (any state, existing project only) per project; the key is
 * the lower-cased id (see `foldCase`).
 */
export function countsByProject(
  agents: readonly AgentInfo[],
  tickets: readonly TicketSummary[],
): Map<string, { agents: number; tickets: number }> {
  const out = new Map<string, { agents: number; tickets: number }>();
  const entry = (id: string) => {
    const key = foldCase(id);
    let e = out.get(key);
    if (e === undefined) {
      e = { agents: 0, tickets: 0 };
      out.set(key, e);
    }
    return e;
  };
  for (const a of agents) {
    if (isLive(a) && a.seatKind === "work" && a.project !== null) entry(a.project).agents++;
  }
  for (const t of tickets) {
    const id = projectIdOf(t.project);
    if (id !== null) entry(id).tickets++;
  }
  return out;
}

/** The counts of `id` from [`countsByProject`] (zeros when absent). */
export function countsFor(
  counts: Map<string, { agents: number; tickets: number }>,
  id: string,
): { agents: number; tickets: number } {
  return counts.get(foldCase(id)) ?? { agents: 0, tickets: 0 };
}

// A project may be called "all" or "none": those ids are stored with a prefix that no project
// name can contain (":" is an invalid character).
const ID_PREFIX = "project:";

/** The stored filter: null/"all" → all, "none" → tickets without a project, otherwise one id. */
export function parseProjectFilter(s: string | null): ProjectFilter {
  if (s === null || s === "" || s === "all") return "all";
  if (s === "none") return "none";
  return { id: s.startsWith(ID_PREFIX) ? s.slice(ID_PREFIX.length) : s };
}

/** The string stored for a filter (and used as the `<select>` value). */
export function projectFilterKey(f: ProjectFilter): string {
  if (f === "all" || f === "none") return f;
  return f.id === "all" || f.id === "none" ? `${ID_PREFIX}${f.id}` : f.id;
}

/** Whether ticket `t` is shown under filter `f` (a `{ new }` counts under its name). */
export function matchesFilter(t: Pick<TicketSummary, "project">, f: ProjectFilter): boolean {
  if (f === "all") return true;
  if (f === "none") return t.project === null;
  return sameProjectId(projectName(t.project), f.id);
}

/** `<root><sep><id>` with the root's own separator (backslash on Windows). */
export function projectPath(root: string, id: string): string {
  const sep = root.includes("\\") ? "\\" : "/";
  return root.endsWith(sep) ? `${root}${id}` : `${root}${sep}${id}`;
}
