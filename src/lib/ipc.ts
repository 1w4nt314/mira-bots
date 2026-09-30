// The only place with command and event name strings. Components import the wrappers below.
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import type {
  AgentInfo,
  AgentOutputPayload,
  AgentOutputSnapshot,
  AgentRole,
  AppInfo,
  Diagnostics,
  HookEventPayload,
  PermissionRequestInfo,
  PermissionResolvedPayload,
  SeatKind,
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
  getDiagnostics: "get_diagnostics",
  openWorkplace: "open_workplace",
  takeWorkplaceSelection: "take_workplace_selection",
  openAgentFolder: "open_agent_folder",
  openLogDir: "open_log_dir",
} as const;

export const EVENTS = {
  agentsChanged: "agents-changed",
  agentOutput: "agent-output",
  permissionRequest: "permission-request",
  permissionResolved: "permission-resolved",
  hookEvent: "hook-event",
  workplaceSelect: "workplace-select",
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
/** `cwd` null/blank → default folder `<agentsRoot>/<role>-nn`; role/seatKind null → "none"/"work". */
export const spawnAgent = (
  cwd: string | null,
  prompt: string | null,
  role: AgentRole | null,
  seatKind: SeatKind | null,
) => invoke<AgentInfo>(COMMANDS.spawnAgent, { cwd, prompt, role, seatKind });
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
export const getDiagnostics = () => invoke<Diagnostics>(COMMANDS.getDiagnostics);
/** Opens or focuses the workplace window and selects `agentId` there (if given). */
export const openWorkplace = (agentId: string | null) =>
  invoke<void>(COMMANDS.openWorkplace, { agentId });
/** Takes the selection stored by `openWorkplace` for a newly created window (once). */
export const takeWorkplaceSelection = () =>
  invoke<string | null>(COMMANDS.takeWorkplaceSelection);
export const openAgentFolder = (agentId: string) =>
  invoke<void>(COMMANDS.openAgentFolder, { agentId });
export const openLogDir = () => invoke<void>(COMMANDS.openLogDir);

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
/** Only delivered to the workplace window. */
export const onWorkplaceSelect = (cb: (agentId: string) => void): Promise<UnlistenFn> =>
  listen<string>(EVENTS.workplaceSelect, (e) => cb(e.payload));

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
