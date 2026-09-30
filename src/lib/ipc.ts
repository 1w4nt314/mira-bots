// The only place with command and event name strings. Components import the wrappers below.
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import type {
  AgentInfo,
  AgentOutputPayload,
  AgentOutputSnapshot,
  AppInfo,
  HookEventPayload,
  PermissionRequestInfo,
  PermissionResolvedPayload,
} from "./types";

export const COMMANDS = {
  uiReady: "ui_ready",
  getAppInfo: "get_app_info",
  listAgents: "list_agents",
  spawnAgent: "spawn_agent",
  stopAgent: "stop_agent",
  removeAgent: "remove_agent",
  writeAgentInput: "write_agent_input",
  resizeAgentPty: "resize_agent_pty",
  getAgentOutput: "get_agent_output",
  listPendingPermissions: "list_pending_permissions",
  respondPermission: "respond_permission",
  resizeIsland: "resize_island",
  quitApp: "quit_app",
} as const;

export const EVENTS = {
  agentsChanged: "agents-changed",
  agentOutput: "agent-output",
  permissionRequest: "permission-request",
  permissionResolved: "permission-resolved",
  hookEvent: "hook-event",
} as const;

/** Commands reject with the Rust error string (Danish, user-facing). */
export function errorMessage(e: unknown): string {
  if (typeof e === "string") return e;
  if (e instanceof Error) return e.message;
  return String(e);
}

// --- commands ---------------------------------------------------------------------------------

export const uiReady = () => invoke<void>(COMMANDS.uiReady);
export const getAppInfo = () => invoke<AppInfo>(COMMANDS.getAppInfo);
export const listAgents = () => invoke<AgentInfo[]>(COMMANDS.listAgents);
export const spawnAgent = (cwd: string, prompt: string | null) =>
  invoke<AgentInfo>(COMMANDS.spawnAgent, { cwd, prompt });
export const stopAgent = (agentId: string) => invoke<void>(COMMANDS.stopAgent, { agentId });
export const removeAgent = (agentId: string) => invoke<void>(COMMANDS.removeAgent, { agentId });
export const writeAgentInput = (agentId: string, data: string) =>
  invoke<void>(COMMANDS.writeAgentInput, { agentId, data });
export const resizeAgentPty = (agentId: string, cols: number, rows: number) =>
  invoke<void>(COMMANDS.resizeAgentPty, { agentId, cols, rows });
export const getAgentOutput = (agentId: string) =>
  invoke<AgentOutputSnapshot>(COMMANDS.getAgentOutput, { agentId });
export const listPendingPermissions = () =>
  invoke<PermissionRequestInfo[]>(COMMANDS.listPendingPermissions);
export const respondPermission = (requestId: string, allow: boolean, always: boolean) =>
  invoke<void>(COMMANDS.respondPermission, { requestId, allow, always });
export const resizeIsland = (width: number, height: number) =>
  invoke<void>(COMMANDS.resizeIsland, { width: Math.round(width), height: Math.round(height) });
export const quitApp = () => invoke<void>(COMMANDS.quitApp);

// --- events (each returns the unlisten function) ----------------------------------------------

export const onAgentsChanged = (cb: (agents: AgentInfo[]) => void): Promise<UnlistenFn> =>
  listen<AgentInfo[]>(EVENTS.agentsChanged, (e) => cb(e.payload));
export const onAgentOutput = (cb: (p: AgentOutputPayload) => void): Promise<UnlistenFn> =>
  listen<AgentOutputPayload>(EVENTS.agentOutput, (e) => cb(e.payload));
export const onPermissionRequest = (
  cb: (r: PermissionRequestInfo) => void,
): Promise<UnlistenFn> =>
  listen<PermissionRequestInfo>(EVENTS.permissionRequest, (e) => cb(e.payload));
export const onPermissionResolved = (
  cb: (p: PermissionResolvedPayload) => void,
): Promise<UnlistenFn> =>
  listen<PermissionResolvedPayload>(EVENTS.permissionResolved, (e) => cb(e.payload));
export const onHookEvent = (cb: (p: HookEventPayload) => void): Promise<UnlistenFn> =>
  listen<HookEventPayload>(EVENTS.hookEvent, (e) => cb(e.payload));

// --- dialog -----------------------------------------------------------------------------------

/** Native folder picker. Resolves to the chosen path, or null if the user cancelled. */
// TODO(windows-verify): the folder picker opens in front of the island and returns a
// backslash path that spawn_agent accepts (plan D.11).
export async function pickFolder(): Promise<string | null> {
  const picked = await open({
    directory: true,
    multiple: false,
    title: "Vælg mappe til agenten",
  });
  return typeof picked === "string" ? picked : null;
}
