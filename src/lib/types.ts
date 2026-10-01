// Mirrors the IPC contract (commands C.1 + C2.1 + C3.2, events C.2 + C2.2 + C3.4, types C.3 +
// C2.3 + C3.1).
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

/** Visual role of an agent (figure and default folder name). */
export type AgentRole = "none" | "coder" | "researcher" | "reviewer" | "koord";

/** Row of seats an agent occupies; each has its own limit (5 work, 2 staff). */
export type SeatKind = "work" | "staff";

/** Figure state derived from the agent status in the frontend. */
export type BotState = "idle" | "work" | "wait" | "done";

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
  role: AgentRole;
  seatKind: SeatKind;
  /** The agent's ticket in progress (set by the backend's ticket links only). */
  currentTicketId: string | null;
  /** Number of queued (`assigned`) tickets. */
  queueLength: number;
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
  /** Parent of the default agent folders (`<home>/mira-bots/agents`). */
  agentsRoot: string;
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
  /** Whether hooks.json's exec-form `args` is supported (>= 2.1.139); null when unknown. */
  claudeCodeArgsSupported: boolean | null;
  hookExe: string | null;
  /**
   * @deprecated No longer sent by the backend (renamed to `settingsPath`/`settingsExists` in
   * step 4); still read by DiagnosticsPanel until the step 4 frontend batch replaces them.
   */
  hooksJsonPath?: string;
  /** @deprecated See `hooksJsonPath`. */
  hooksJsonExists?: boolean;
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
  // TODO(step 4 frontend): `lastToolCall: LastToolCall | null` is sent too; add it together with
  // DiagnosticsPanel's formatValue, which only knows LastHookEvent objects.
  /** Whether a Stop moves the in-progress ticket to review (AUTO_REVIEW_ON_STOP). */
  autoReviewOnStop: boolean;
  pipeName: string;
  pipeReady: boolean;
  framesReceived: number;
  framesUnknownSession: number;
  lastHookEvent: LastHookEvent | null;
  logPath: string | null;
  appVersion: string;
  agentsRoot: string;
  runningAgents: number;
  /** `<app_data_dir>/tickets.json`. */
  ticketsPath: string;
  /** Set when tickets.json could not be read at startup (renamed to `.broken-<ts>`, or unreadable). */
  ticketsWarning: string | null;
  /** tickets.json could not be read at startup: ticket changes are disabled until restart. */
  ticketsReadOnly: boolean;
  ticketsTotal: number;
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
export type TicketSource = "user";
export type TicketIssue = "deliveryFailed" | "turnFailed";

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
  /** Milliseconds since the Unix epoch. */
  createdAt: number;
  /** Milliseconds since the Unix epoch. */
  updatedAt: number;
  historyLen: number;
}

/**
 * `get_ticket` result: with history. The Rust `Ticket` has no `shortId` (only the summary does),
 * so it is omitted here; use `shortId(id)` from `lib/tickets` if it is needed.
 */
export interface Ticket extends Omit<TicketSummary, "historyLen" | "shortId"> {
  body: string;
  history: TicketHistoryEntry[];
}

/** `update_ticket` patch; absent fields stay unchanged. */
export interface TicketPatch {
  title?: string;
  body?: string;
  skipReview?: boolean;
}

/** Sidebar tabs `openWorkplace` may select. */
export type WorkplaceTab = "permissions" | "diagnostics" | "tickets";

/** Payload of `workplace-select` and result of `take_workplace_selection`. */
export interface WorkplaceSelection {
  agentId: string | null;
  tab: string | null;
}
