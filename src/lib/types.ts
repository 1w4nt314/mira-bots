// Mirrors the IPC contract (commands C.1, events C.2, AgentStatus C.3). All fields camelCase.

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
