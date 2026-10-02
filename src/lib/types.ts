// Mirrors the IPC contract (commands C.1 + C2.1 + C3.2 + C4.10 + C5.4, events C.2 + C2.2 + C3.4 +
// C5.4, types C.3 + C2.3 + C3.1 + C4.1 + C5.1).
// All fields camelCase.

export type AgentStatus =
  | { kind: "starting" }
  | { kind: "idle" }
  | { kind: "thinking" }
  | { kind: "reading" }
  | { kind: "editing" }
  | { kind: "running" }
  | { kind: "waitingPermission" }
  | { kind: "exited"; code: number | null };

export type AgentStatusKind = AgentStatus["kind"];

/** An agent role (0..6 per agent; wire names). The figure of `coordinator` is called `koord`. */
export type Role = "coder" | "researcher" | "reviewer" | "coordinator" | "planner" | "debugger";

/** Reasoning effort (`--effort`); `max` only as a flag, never in a settings file. */
export type Effort = "low" | "medium" | "high" | "xhigh" | "max";

export type ProfileKind = "builtin" | "custom";

/** Row of seats an agent occupies; each has its own limit (5 work, 3 staff). */
export type SeatKind = "work" | "staff";

/** Figure state derived from the agent status in the frontend. */
export type BotState = "idle" | "work" | "wait" | "done";

/** An agent profile (file `<projectsRoot>/.mira-bots/profiles/<id>.json`). */
export interface AgentProfile {
  /** Empty in `saveProfile` for a new profile (the backend creates `custom-<8 hex>`). */
  id: string;
  name: string;
  /** Derived from the id by the backend. */
  kind: ProfileKind;
  roles: Role[];
  /** null: derived (`roles.length !== 1`). */
  specialist: boolean | null;
  promptAppend: string;
  /** Alias or full model id; null = Claude Code's default. */
  model: string | null;
  effort: Effort | null;
  /** The app's own tool names (without prefix) denied on top of the role matrix. */
  toolDeny: string[];
  defaultSeat: SeatKind;
  /** Raw permission rules added to `permissions.allow` / `permissions.deny`. */
  extraAllow: string[];
  extraDeny: string[];
  /** Milliseconds since the Unix epoch. */
  updatedAt: number;
}

/** Per-spawn overrides of the profile's model/effort. */
export interface SpawnOverrides {
  model?: string | null;
  effort?: Effort | null;
}

export interface AgentInfo {
  id: string;
  sessionId: string;
  name: string;
  cwd: string;
  status: AgentStatus;
  detail: string | null;
  pid: number | null;
  /** Milliseconds since the Unix epoch. */
  createdAt: number;
  /** Milliseconds since the Unix epoch. */
  lastEventAt: number;
  /** Profile snapshot taken at spawn; roles never change during a session. */
  profileId: string;
  profileName: string;
  roles: Role[];
  specialist: boolean;
  /** Requested model, overwritten by the observed one (see `modelObserved`); null = default. */
  model: string | null;
  /** Requested effort, overwritten by the observed `effort.level`; null = default. */
  effort: string | null;
  modelObserved: boolean;
  /** Open review assignments of this agent. */
  openReviews: number;
  seatKind: SeatKind;
  /** The agent's ticket in progress (set by the backend's ticket links only). */
  currentTicketId: string | null;
  /** Number of queued (`assigned`) tickets. */
  queueLength: number;
  /** The agent's project (work seat); null on a staff seat (it runs in the projects root). */
  project: string | null;
}

/** A ticket's project: an existing project id, or a project to create (`{ new: name }`). */
export type ProjectRef = string | { new: string };

/** A project folder under the projects root (`list_projects`). */
export interface Project {
  id: string;
  path: string;
}

export interface PermissionRequestInfo {
  requestId: string;
  agentId: string;
  agentName: string;
  toolName: string;
  summary: string;
  toolInput: unknown;
  /** Milliseconds since the Unix epoch. */
  createdAt: number;
  /** Milliseconds since the Unix epoch; after this the app answers "none". */
  deadlineAt: number;
}

export interface AppInfo {
  claudePath: string | null;
  hookExe: string | null;
  /** The app's settings.json (hooks + permissions), passed with `--settings`. */
  settingsJson: string;
  pipeName: string;
  maxAgents: number;
  version: string;
  /** False while the hook pipe is not listening; `spawn_agent` then refuses to start agents. */
  pipeReady: boolean;
  maxStaffAgents: number;
  /** The projects root (`<home>/mira-bots/projects` or the app setting). */
  projectsRoot: string;
  /** The effective workspace rules (`mira-bots.workspace.json` over the defaults). */
  rules: WorkspaceRules;
}

/** The rules of the workspace (defaults overridden by `mira-bots.workspace.json`). */
export interface WorkspaceRules {
  maxWorkAgents: number;
  maxStaffAgents: number;
  maxReviewRounds: number;
  autoReviewOnStop: boolean;
  createTicketRateLimit: number;
  ticketBodyMaxChars: number;
  reportBodyMaxChars: number;
  reportsPerTicketMax: number;
  reviewByDefault: boolean;
  userInputGraceMs: number;
  agentsMayCreateProjects: boolean;
  maxAgentsPerProject: number;
}

/** The last tool call from an agent's MCP server (mira-mcp); arguments are never included. */
export interface LastToolCall {
  tool: string;
  agentId: string | null;
  ok: boolean;
  /** Milliseconds since the Unix epoch. */
  at: number;
}

export interface LastHookEvent {
  name: string;
  sessionId: string;
  /** Frame-level agent id (`MIRA_AGENT_ID`) as sent by the hook, if any. */
  agentId: string | null;
  /** Milliseconds since the Unix epoch. */
  at: number;
}

/** Result of `get_diagnostics`. */
export interface Diagnostics {
  claudePath: string | null;
  claudeVersion: string | null;
  /** E.g. "kører stadig", "ikke fundet" or the probe's error text. */
  claudeVersionNote: string | null;
  /** Whether settings.json's exec-form `args` is supported (>= 2.1.139); null when unknown. */
  claudeCodeArgsSupported: boolean | null;
  /** Claude Code >= 2.1.274 (the agent tools); null when the version is unknown. */
  claudeCodeMcpSupported: boolean | null;
  hookExe: string | null;
  /** `<app_data_dir>/settings.json` (hooks + permissions). */
  settingsPath: string;
  settingsExists: boolean;
  /** The mira-mcp exe; null: not found, agents get no tools. */
  mcpExe: string | null;
  mcpConfigPath: string;
  mcpConfigExists: boolean;
  systemPromptPath: string;
  toolCalls: number;
  toolErrors: number;
  /** The latest MCP tool call from any agent since the app started; null before the first. */
  lastToolCall: LastToolCall | null;
  /** Whether a Stop moves the in-progress ticket to review (workspace rule autoReviewOnStop). */
  autoReviewOnStop: boolean;
  pipeName: string;
  pipeReady: boolean;
  framesReceived: number;
  framesUnknownSession: number;
  lastHookEvent: LastHookEvent | null;
  logPath: string | null;
  appVersion: string;
  /** The projects root in use (a changed setting applies after a restart). */
  projectsRoot: string;
  runningAgents: number;
  /** `<app_data_dir>/tickets.json`. */
  ticketsPath: string;
  /** Set when tickets.json could not be read at startup (renamed to `.broken-<ts>`, or unreadable). */
  ticketsWarning: string | null;
  /** tickets.json could not be read at startup: ticket changes are disabled until restart. */
  ticketsReadOnly: boolean;
  ticketsTotal: number;
  /** `<projectsRoot>/.mira-bots/profiles`. */
  profilesPath: string;
  profilesLoaded: number;
  /** Set when profile files were broken (renamed to `.broken-<ts>`) or the folder was unusable. */
  profilesWarning: string | null;
  /** Open review assignments. */
  reviewAssignmentsOpen: number;
  /** Tickets escalated after `MAX_REVIEW_ROUNDS` rejections. */
  ticketsEscalated: number;
  reportsTotal: number;
  /** `<projectsRoot>/mira-bots.workspace.json`. */
  workspaceFilePath: string;
  workspaceFileExists: boolean;
  /** Set when the workspace file could not be read (the defaults apply). */
  workspaceWarning: string | null;
  /** Project folders under the projects root. */
  projectsTotal: number;
  /** Profiles copied from the old agents folder at this start. */
  profilesMigrated: number;
}

export interface AgentOutputPayload {
  agentId: string;
  /** Ring buffer byte counter after this chunk. */
  seq: number;
  dataBase64: string;
}

/** Result of `get_agent_output`; same shape as the `agent-output` event payload. */
export type AgentOutputSnapshot = AgentOutputPayload;

export type PermissionDecision = "allow" | "deny" | "none";

export interface PermissionResolvedPayload {
  requestId: string;
  decision: PermissionDecision;
}

export interface HookEventPayload {
  agentId: string | null;
  sessionId: string;
  hookEventName: string;
  toolName: string | null;
  receivedAt: number;
}

// --- tickets (C3.1) ---------------------------------------------------------------------------

export type TicketState = "backlog" | "assigned" | "inProgress" | "review" | "done" | "rejected";
export type TicketActor = "user" | "system" | "agent";
/** Who created the ticket: the user (UI) or an agent (`mira_create_ticket`). */
export type TicketSource = "user" | "agent";
/**
 * `notSubmitted`: the agent's turn ended (Stop) without `mira_submit_for_review`; the ticket stays
 * in progress.
 */
export type TicketIssue = "deliveryFailed" | "turnFailed" | "notSubmitted";

export interface TicketHistoryEntry {
  /** Milliseconds since the Unix epoch. */
  at: number;
  from: TicketState | null;
  to: TicketState;
  by: TicketActor;
  note: string | null;
}

/** A ticket without its history and body (`list_tickets`, `tickets-changed`); `getTicket` has both. */
export interface TicketSummary {
  id: string;
  shortId: string;
  title: string;
  state: TicketState;
  assigneeAgentId: string | null;
  /** 0-based position in the assignee's queue; only while `assigned`. */
  queuePosition: number | null;
  skipReview: boolean;
  source: TicketSource;
  issue: TicketIssue | null;
  rejectionNote: string | null;
  /** The agent's summary from `mira_submit_for_review`; null when moved by hand. */
  summary: string | null;
  /** Milliseconds since the Unix epoch. */
  createdAt: number;
  /** Milliseconds since the Unix epoch. */
  updatedAt: number;
  historyLen: number;
  /** Review rejections so far; reset when the ticket goes back to the backlog. */
  reviewRound: number;
  /** Reached the maximum review rounds: no automatic routing, the user decides. */
  escalated: boolean;
  /** The reviewer agent while in review (kept after approval). */
  reviewerAgentId: string | null;
  reportCount: number;
  /** The ticket's project; null = none yet (it must get one before a work agent takes it). */
  project: ProjectRef | null;
}

/** Who wrote a report: an agent (`agentId`) or the user. */
export interface ReportAuthor {
  kind: "agent" | "user";
  agentId: string | null;
}

/** A report on a ticket; its text comes from `getReport`. */
export interface TicketReport {
  /** Two-digit sequence number ("01"). */
  id: string;
  title: string;
  author: ReportAuthor;
  /** Milliseconds since the Unix epoch. */
  createdAt: number;
  /** Relative to the ticket's folder: `reports/01-slug.md`. */
  path: string;
  /** Bytes. */
  size: number;
}

/** `get_report` result. */
export interface ReportContent {
  report: TicketReport;
  body: string;
}

/** An open review of a ticket by a reviewer agent. */
export interface ReviewAssignment {
  ticketId: string;
  reviewerAgentId: string;
  round: number;
  /** Milliseconds since the Unix epoch. */
  assignedAt: number;
  /** null until the review line was confirmed in the reviewer's terminal. */
  deliveredAt: number | null;
  attempts: number;
}

/**
 * `get_ticket` result: with history. The Rust `Ticket` has no `shortId` (only the summary does),
 * so it is omitted here; use `shortId(id)` from `lib/tickets` if it is needed.
 */
export interface Ticket extends Omit<TicketSummary, "historyLen" | "shortId" | "reportCount"> {
  body: string;
  history: TicketHistoryEntry[];
  reports: TicketReport[];
}

/** `update_ticket` patch; absent fields stay unchanged. */
export interface TicketPatch {
  title?: string;
  body?: string;
  skipReview?: boolean;
  /** `null` removes the project; only while the ticket is in the backlog. */
  project?: ProjectRef | null;
}

/** Sidebar tabs `openWorkplace` may select. */
export type WorkplaceTab = "permissions" | "diagnostics" | "tickets" | "agents";

/** Payload of `workplace-select` and result of `take_workplace_selection`. */
export interface WorkplaceSelection {
  agentId: string | null;
  tab: string | null;
}
