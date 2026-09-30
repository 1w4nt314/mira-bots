// Mirrors the IPC contract (commands C.1 + C2.1, events C.2 + C2.2, types C.3 + C2.3).
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
  hooksJson: string;
  pipeName: string;
  maxAgents: number;
  version: string;
  /** False while the hook pipe is not listening; `spawn_agent` then refuses to start agents. */
  pipeReady: boolean;
  maxStaffAgents: number;
  /** Parent of the default agent folders (`<home>/mira-bots/agents`). */
  agentsRoot: string;
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
  hooksJsonPath: string;
  hooksJsonExists: boolean;
  pipeName: string;
  pipeReady: boolean;
  framesReceived: number;
  framesUnknownSession: number;
  lastHookEvent: LastHookEvent | null;
  logPath: string | null;
  appVersion: string;
  agentsRoot: string;
  runningAgents: number;
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
