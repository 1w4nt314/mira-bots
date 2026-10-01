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
  Ticket,
  TicketPatch,
  TicketState,
  TicketSummary,
  WorkplaceSelection,
  WorkplaceTab,
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
  listTickets: "list_tickets",
  getTicket: "get_ticket",
  createTicket: "create_ticket",
  updateTicket: "update_ticket",
  deleteTicket: "delete_ticket",
  assignTicket: "assign_ticket",
  unassignTicket: "unassign_ticket",
  reorderQueue: "reorder_queue",
  setTicketState: "set_ticket_state",
  approveTicket: "approve_ticket",
  rejectTicket: "reject_ticket",
  redispatchTicket: "redispatch_ticket",
  spawnAgentWithTicket: "spawn_agent_with_ticket",
  requestSubmission: "request_submission",
} as const;

export const EVENTS = {
  agentsChanged: "agents-changed",
  agentOutput: "agent-output",
  permissionRequest: "permission-request",
  permissionResolved: "permission-resolved",
  hookEvent: "hook-event",
  workplaceSelect: "workplace-select",
  ticketsChanged: "tickets-changed",
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
/** `userInitiated` false for the terminal's automatic replies: they do not count as user typing. */
export const writeAgentInput = (agentId: string, data: string, userInitiated: boolean) =>
  invoke<void>(COMMANDS.writeAgentInput, { agentId, data, userInitiated });
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
/** Opens or focuses the workplace window and selects `agentId` and/or the sidebar `tab` there. */
export const openWorkplace = (agentId: string | null, tab: WorkplaceTab | null = null) =>
  invoke<void>(COMMANDS.openWorkplace, { agentId, tab });
/** Takes the selection stored by `openWorkplace` for a newly created window (once). */
export const takeWorkplaceSelection = () =>
  invoke<WorkplaceSelection | null>(COMMANDS.takeWorkplaceSelection);
export const openAgentFolder = (agentId: string) =>
  invoke<void>(COMMANDS.openAgentFolder, { agentId });
export const openLogDir = () => invoke<void>(COMMANDS.openLogDir);

// tickets (C3.2)
/** All tickets without history, ordered by `createdAt`. */
export const listTickets = () => invoke<TicketSummary[]>(COMMANDS.listTickets);
/** One ticket with its history. */
export const getTicket = (id: string) => invoke<Ticket>(COMMANDS.getTicket, { id });
export const createTicket = (title: string, body: string, skipReview: boolean) =>
  invoke<TicketSummary>(COMMANDS.createTicket, { title, body, skipReview });
export const updateTicket = (id: string, patch: TicketPatch) =>
  invoke<TicketSummary>(COMMANDS.updateTicket, { id, patch });
/** Only backlog/done tickets and rejected ones without an agent. */
export const deleteTicket = (id: string) => invoke<void>(COMMANDS.deleteTicket, { id });
/** backlog/rejected → end of the agent's queue; the agent must be running. */
export const assignTicket = (id: string, agentId: string) =>
  invoke<TicketSummary>(COMMANDS.assignTicket, { id, agentId });
export const unassignTicket = (id: string) =>
  invoke<TicketSummary>(COMMANDS.unassignTicket, { id });
/** `ticketIds` must be exactly the agent's queued tickets; returns the new queue. */
export const reorderQueue = (agentId: string, ticketIds: string[]) =>
  invoke<TicketSummary[]>(COMMANDS.reorderQueue, { agentId, ticketIds });
/** Manual move (rules in plan C3.3). */
export const setTicketState = (id: string, state: TicketState, note: string | null) =>
  invoke<TicketSummary>(COMMANDS.setTicketState, { id, state, note });
export const approveTicket = (id: string) =>
  invoke<TicketSummary>(COMMANDS.approveTicket, { id });
/** `note` must not be blank; the ticket goes first in the agent's queue (or the backlog). */
export const rejectTicket = (id: string, note: string) =>
  invoke<TicketSummary>(COMMANDS.rejectTicket, { id, note });
/** "Bed om aflevering": the ticket must be in progress with a running agent; the line is typed
 *  once the agent is idle. */
export const requestSubmission = (ticketId: string) =>
  invoke<void>(COMMANDS.requestSubmission, { ticketId });
/** "Send igen": the dispatcher decides whether the ticket can be delivered now. */
export const redispatchTicket = (id: string) => invoke<void>(COMMANDS.redispatchTicket, { id });
/** Like `spawnAgent`, with the ticket line as the first prompt; the ticket heads the new queue. */
export const spawnAgentWithTicket = (
  ticketId: string,
  cwd: string | null,
  role: AgentRole | null,
  seatKind: SeatKind | null,
) => invoke<AgentInfo>(COMMANDS.spawnAgentWithTicket, { ticketId, cwd, role, seatKind });

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
export const onWorkplaceSelect = (cb: (s: WorkplaceSelection) => void): Promise<UnlistenFn> =>
  listen<WorkplaceSelection>(EVENTS.workplaceSelect, (e) => cb(e.payload));
/** Full ticket list (without history) after any ticket change. */
export const onTicketsChanged = (cb: (tickets: TicketSummary[]) => void): Promise<UnlistenFn> =>
  listen<TicketSummary[]>(EVENTS.ticketsChanged, (e) => cb(e.payload));

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
