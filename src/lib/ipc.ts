// The only place with command and event name strings. Components import the wrappers below.
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import type {
  AgentInfo,
  AgentOutputPayload,
  AgentOutputSnapshot,
  AgentProfile,
  AppInfo,
  Diagnostics,
  Effort,
  HookEventPayload,
  PermissionRequestInfo,
  PermissionResolvedPayload,
  ReportContent,
  ReviewAssignment,
  SeatKind,
  SpawnOverrides,
  Ticket,
  TicketPatch,
  TicketReport,
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
  listProfiles: "list_profiles",
  getProfile: "get_profile",
  saveProfile: "save_profile",
  deleteProfile: "delete_profile",
  resetBuiltinProfile: "reset_builtin_profile",
  setAgentModel: "set_agent_model",
  setAgentEffort: "set_agent_effort",
  addReport: "add_report",
  getReport: "get_report",
  openReportDir: "open_report_dir",
  assignReviewer: "assign_reviewer",
  listReviewAssignments: "list_review_assignments",
} as const;

export const EVENTS = {
  agentsChanged: "agents-changed",
  agentOutput: "agent-output",
  permissionRequest: "permission-request",
  permissionResolved: "permission-resolved",
  hookEvent: "hook-event",
  workplaceSelect: "workplace-select",
  ticketsChanged: "tickets-changed",
  profilesChanged: "profiles-changed",
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
/**
 * `profileId` null → "coder"; `overrides` replace the profile's model/effort; `cwd` null/blank →
 * default folder `<agentsRoot>/<prefix>-nn`; `seatKind` null → the profile's `defaultSeat`.
 */
export const spawnAgent = (
  profileId: string | null,
  overrides: SpawnOverrides | null,
  cwd: string | null,
  prompt: string | null,
  seatKind: SeatKind | null,
) => invoke<AgentInfo>(COMMANDS.spawnAgent, { profileId, overrides, cwd, prompt, seatKind });
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
  profileId: string | null,
  overrides: SpawnOverrides | null,
  cwd: string | null,
  seatKind: SeatKind | null,
) =>
  invoke<AgentInfo>(COMMANDS.spawnAgentWithTicket, {
    ticketId,
    profileId,
    overrides,
    cwd,
    seatKind,
  });

// profiles and model/effort (C5.4)
/** Built-in profiles first, then the custom ones by name. */
export const listProfiles = () => invoke<AgentProfile[]>(COMMANDS.listProfiles);
export const getProfile = (id: string) => invoke<AgentProfile>(COMMANDS.getProfile, { id });
/** `profile.id` empty → a new custom profile; returns the stored (validated) profile. */
export const saveProfile = (profile: AgentProfile) =>
  invoke<AgentProfile>(COMMANDS.saveProfile, { profile });
/** Only custom profiles. */
export const deleteProfile = (id: string) => invoke<void>(COMMANDS.deleteProfile, { id });
/** Only built-in profiles: back to the default. */
export const resetBuiltinProfile = (id: string) =>
  invoke<AgentProfile>(COMMANDS.resetBuiltinProfile, { id });
/** Restarts the agent with `--resume` and the new model (null → default); only when idle
 *  without a ticket in progress. */
export const setAgentModel = (agentId: string, model: string | null) =>
  invoke<AgentInfo>(COMMANDS.setAgentModel, { agentId, model });
/** Like `setAgentModel`, for the effort level. */
export const setAgentEffort = (agentId: string, effort: Effort) =>
  invoke<AgentInfo>(COMMANDS.setAgentEffort, { agentId, effort });
/** The user adds a report to a ticket (author "user"). */
export const addReport = (ticketId: string, title: string, body: string) =>
  invoke<TicketReport>(COMMANDS.addReport, { ticketId, title, body });
export const getReport = (ticketId: string, reportId: string) =>
  invoke<ReportContent>(COMMANDS.getReport, { ticketId, reportId });
/** Opens the ticket's report folder in Explorer (created first if needed). */
export const openReportDir = (ticketId: string) =>
  invoke<void>(COMMANDS.openReportDir, { ticketId });
/** Picks the reviewer of a ticket in review; `null` removes it and routes the ticket again. */
export const assignReviewer = (ticketId: string, agentId: string | null) =>
  invoke<TicketSummary>(COMMANDS.assignReviewer, { ticketId, agentId });
export const listReviewAssignments = () =>
  invoke<ReviewAssignment[]>(COMMANDS.listReviewAssignments);

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
/** Full profile list after a profile was saved, deleted or reset. */
export const onProfilesChanged = (cb: (profiles: AgentProfile[]) => void): Promise<UnlistenFn> =>
  listen<AgentProfile[]>(EVENTS.profilesChanged, (e) => cb(e.payload));

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
