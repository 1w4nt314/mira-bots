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
  /** The folder has `.git` (step 6b); false for a project that was just created. */
  isGitRepo: boolean;
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
  /** Playbook names, sorted (built-in `bug`/`feature` plus the workspace file's; step 6b). */
  playbookKinds: string[];
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
  // step 6b
  /** How a work ticket gets its own branch. */
  git: GitMode;
  /** A failed project check rejects the ticket before review. */
  checksGate: boolean;
  autoSpawnForPlaybook: boolean;
  freshSessionPerTicket: boolean;
  cleanupWorktreesOnDone: boolean;
}

/** Workspace rule `git` (step 6b). */
export type GitMode = "off" | "branch" | "worktree";

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
  /** Unix: why the socket path cannot work (too long); null otherwise and on Windows. */
  pipeNote: string | null;
  framesReceived: number;
  framesUnknownSession: number;
  lastHookEvent: LastHookEvent | null;
  logPath: string | null;
  appVersion: string;
  /** "windows" | "macos" | "linux" (Rust `std::env::consts::OS`). */
  platform: string;
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
  /** Tickets escalated after the workspace's `maxReviewRounds` rejections. */
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
  /** Set when `inbox.json` could not be read (renamed as broken; the inbox starts empty). */
  inboxWarning: string | null;
  /** The found `gh` executable; null = not found (step 6c). */
  ghPath: string | null;
  /** `gh --version`, e.g. "2.102.0". */
  ghVersion: string | null;
  /** "ikke fundet" | "kører stadig" | "ældre end 2.40.0 — ikke afprøvet" | a probe error. */
  ghVersionNote: string | null;
  /** `<app data>/inbox.json`. */
  inboxPath: string;
  /** Items in state `new`. */
  inboxNew: number;
  /** One row per source and project. */
  inboxSources: InboxSourceDiag[];
}

/** One inbox source of one project in Diagnostik (`Diagnostics.inboxSources`). */
export interface InboxSourceDiag {
  /** null = the projects root's `inbox/`. */
  project: string | null;
  kind: ExternalKind;
  label: string;
  /** Milliseconds since the Unix epoch of the last successful fetch. */
  lastFetchAt: number | null;
  /** The last fetch's error, or why `project.json`'s `github` is ignored. */
  error: string | null;
  items: number;
}

/** Result of `check_gh_auth` (never contains a token). */
export interface GhAuthResult {
  ok: boolean;
  text: string;
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

export type TicketState =
  | "backlog"
  | "assigned"
  | "inProgress"
  | "waiting"
  | "review"
  | "done"
  | "rejected";
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

/** State of a ticket's project checks (step 6b). `skipped`: nothing ran (no file or no checks). */
export type ChecksState = "pending" | "passed" | "failed" | "skipped";

/** The project checks of the ticket's current review entry (step 6b); reset on submit. */
export interface TicketChecks {
  state: ChecksState;
  /** Name of the first failed check. */
  failed: string | null;
  /** The ticket's `reviewRound` when the checks started. */
  round: number;
  /** Milliseconds since the Unix epoch. */
  startedAt: number;
}

/** The ticket's git branch, prepared by the app at delivery (step 6b). */
export interface TicketGit {
  mode: "branch" | "worktree";
  /** `ticket/<shortId>`. */
  branch: string;
  base: string;
  /** The project's repository folder. */
  repo: string;
  /** The worktree folder (`worktree` mode only). */
  worktree: string | null;
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
  /** The parent ticket's full id (step 6a); null when none or when the parent was deleted. */
  parentId: string | null;
  /** Full ids of the tickets that must be done before this one is delivered (step 6a). */
  blockedBy: string[];
  /** The ticket type (step 6b): `feature`, `bug`, a playbook name; null = plain task ("Opgave"). */
  kind: string | null;
  /** Milliseconds since the Unix epoch; set when "Start forløb" created the children. */
  playbookStartedAt: number | null;
  /** Project checks of the current review entry; null before/without a review. */
  checks: TicketChecks | null;
  /** The ticket's branch/worktree; null when `git` is off or not prepared yet. */
  git: TicketGit | null;
  /** Where the ticket came from (step 6c: an inbox item); null for tickets made in the app. */
  external: ExternalRef | null;
}

/** Where an inbox item came from: a file in an `inbox/` folder or a GitHub issue. */
export type ExternalKind = "folder" | "github";
/** State of one write-back step (comment / close). */
export type WriteBackState = "none" | "inflight" | "done" | "failed";

/** The write-back to the source when the ticket reaches Done (step 6c). */
export interface WriteBack {
  comment: WriteBackState;
  close: WriteBackState;
  commentUrl: string | null;
  commentedAt: number | null;
  closedAt: number | null;
  attempts: number;
  lastError: string | null;
  lastBody: string | null;
}

/** The inbox item a ticket was started from; every text is cleaned by the backend. */
export interface ExternalRef {
  kind: ExternalKind;
  externalId: string;
  repo: string | null;
  number: number | null;
  path: string | null;
  url: string | null;
  title: string;
  labels: string[];
  author: string | null;
  notes: string[];
  inboxItemId: string;
  /** Milliseconds since the Unix epoch. */
  importedAt: number;
  writeBack: WriteBack;
  /** A playbook child of an external ticket: shows the source, never writes back. */
  inherited: boolean;
}

export type InboxState = "new" | "started" | "dismissed";

/** An inbox item as listed (`get_inbox`, `inbox-changed`): without its body. */
export interface InboxItemSummary {
  id: string;
  /** The source type. */
  kind: ExternalKind;
  externalId: string;
  sourceId: string;
  title: string;
  hasBody: boolean;
  labels: string[];
  url: string | null;
  number: number | null;
  repo: string | null;
  path: string | null;
  author: string | null;
  /** The project the item belongs to; null = unknown (see `candidates`). */
  project: string | null;
  /** Projects sharing the item's repo and labels when `project` is null. */
  candidates: string[];
  updatedAt: string | null;
  /** Milliseconds since the Unix epoch. */
  seenAt: number;
  state: InboxState;
  ticketId: string | null;
  notes: string[];
  /** The ticket type from a file's `kind:` (`feature`, `bug`, a playbook); null = plain task. */
  ticketKind: string | null;
  /** An open ticket with the same title in the item's project. */
  duplicateOf: { shortId: string; title: string } | null;
}

/** `get_inbox_item`: the summary fields plus the body (null for GitHub items before Start). */
export interface InboxItem extends Omit<InboxItemSummary, "hasBody" | "duplicateOf"> {
  body: string | null;
  fingerprint: string | null;
  gone: boolean;
  /** Folder items: whether the file was moved to `started/` (null = not started). */
  moved: boolean | null;
}

export type InboxErrorKind =
  | "folder"
  | "ghMissing"
  | "notLoggedIn"
  | "badCredentials"
  | "repoNotFound"
  | "rateLimited"
  | "issuesDisabled"
  | "network"
  | "timeout"
  | "tooLarge"
  | "badJson"
  | "other"
  | "internal";

export interface InboxSourceStatus {
  /** `folder:…`, `github:owner/name` or `github:owner/name[a,b]` (with labels, sorted). */
  id: string;
  kind: ExternalKind;
  label: string;
  project: string | null;
  /** Milliseconds since the Unix epoch of the last successful fetch. */
  lastFetchAt: number | null;
  ok: boolean;
  /** The backend's Danish text (with the retry time for a rate limit); shown as it is. */
  error: string | null;
  errorKind: InboxErrorKind | null;
  nextRetryAt: number | null;
  items: number;
  /** More issues exist than were fetched (the limit is 100 per repo). */
  capped: boolean;
  /** Notes of the latest fetch (files skipped, …). */
  notes: string[];
}

export interface InboxStatus {
  refreshing: boolean;
  /** Milliseconds since the Unix epoch. */
  lastRefreshAt: number | null;
  sources: InboxSourceStatus[];
}

/** `get_inbox` and the `inbox-changed` event. */
export interface InboxPayload {
  items: InboxItemSummary[];
  status: InboxStatus;
}

export type InboxRefreshReason = "startup" | "timer" | "focus" | "manual";

/** The argument of `start_inbox_item`. */
export interface StartInboxRequest {
  itemId: string;
  /** The ticket type; null = plain task. */
  kind: string | null;
  /** null = the item's own project (the backend asks for one when it has none). */
  project: ProjectRef | null;
  skipReview: boolean;
}

/** Who wrote a report: an agent (`agentId`), the user or the app itself (`system`, step 6b). */
export interface ReportAuthor {
  kind: "agent" | "user" | "system";
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

/** One child created by "Start forløb" and whom it went to. */
export interface StartedChild {
  ticket: TicketSummary;
  role: Role;
  /** The agent it was assigned to; null = it waits in the backlog. */
  assignee: string | null;
}

/** `ticket_start_playbook` result (step 6b). */
export interface PlaybookStarted {
  parent: TicketSummary;
  children: StartedChild[];
  /** Ids of agents started for the playbook (`autoSpawnForPlaybook`). */
  spawned: string[];
  /** What could not be done (no agent, refused assignment, failed spawn); Danish. */
  notes: string[];
}

/** Sidebar tabs `openWorkplace` may select. */
export type WorkplaceTab = "permissions" | "diagnostics" | "tickets" | "agents";

/** Payload of `workplace-select` and result of `take_workplace_selection`. */
export interface WorkplaceSelection {
  agentId: string | null;
  tab: string | null;
  /** "work" | "staff": open the "Ny agent" dialog for that seat kind. */
  spawn: string | null;
}
