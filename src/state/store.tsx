import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useReducer,
  type Dispatch,
  type ReactNode,
} from "react";
import type { UnlistenFn } from "@tauri-apps/api/event";
import {
  errorMessage,
  getAppInfo,
  getInbox,
  listAgents,
  listPendingPermissions,
  listProfiles,
  listProjects,
  listTickets,
  onAgentsChanged,
  onInboxChanged,
  onPermissionRequest,
  onPermissionResolved,
  onProfilesChanged,
  onTicketsChanged,
  uiReady,
} from "../lib/ipc";
import type {
  AgentInfo,
  AgentProfile,
  AppInfo,
  InboxPayload,
  PermissionRequestInfo,
  Project,
  TicketSummary,
} from "../lib/types";

export interface State {
  agents: AgentInfo[];
  pending: PermissionRequestInfo[];
  /** Expanded by hovering the island. */
  expanded: boolean;
  error: string | null;
  appInfo: AppInfo | null;
  /** All tickets without history (`tickets-changed` replaces the whole list). */
  tickets: TicketSummary[];
  /** All agent profiles, built-in first (`profiles-changed` replaces the whole list). */
  profiles: AgentProfile[];
  /** The project folders under the projects root (`listProjects`; refreshed on demand). */
  projects: Project[];
  /**
   * The inbox (step 6c): items without body and the status per source. null until the first
   * answer. Both windows load it; only the workplace polls (`useInboxPolling`).
   */
  inbox: InboxPayload | null;
}

export type Action =
  | { type: "agents/set"; agents: AgentInfo[] }
  | { type: "permission/add"; request: PermissionRequestInfo }
  | { type: "permission/remove"; requestId: string }
  | { type: "ui/expand" }
  | { type: "ui/collapse" }
  | { type: "error/set"; error: string | null }
  | { type: "appInfo/set"; appInfo: AppInfo }
  | { type: "tickets/set"; tickets: TicketSummary[] }
  | { type: "profiles/set"; profiles: AgentProfile[] }
  | { type: "projects/set"; projects: Project[] }
  | { type: "inbox/set"; inbox: InboxPayload };

export const initialState: State = {
  agents: [],
  pending: [],
  expanded: false,
  error: null,
  appInfo: null,
  tickets: [],
  profiles: [],
  projects: [],
  inbox: null,
};

export function reducer(state: State, action: Action): State {
  switch (action.type) {
    case "agents/set":
      return { ...state, agents: action.agents };
    case "permission/add":
      if (state.pending.some((p) => p.requestId === action.request.requestId)) return state;
      return { ...state, pending: [...state.pending, action.request] };
    case "permission/remove":
      return { ...state, pending: state.pending.filter((p) => p.requestId !== action.requestId) };
    case "ui/expand":
      return state.expanded ? state : { ...state, expanded: true };
    case "ui/collapse":
      return state.expanded ? { ...state, expanded: false } : state;
    case "error/set":
      return { ...state, error: action.error };
    case "appInfo/set":
      return { ...state, appInfo: action.appInfo };
    case "tickets/set":
      return { ...state, tickets: action.tickets };
    case "profiles/set":
      return { ...state, profiles: action.profiles };
    case "projects/set":
      return { ...state, projects: action.projects };
    case "inbox/set":
      return { ...state, inbox: action.inbox };
  }
}

interface Store {
  state: State;
  dispatch: Dispatch<Action>;
}

const StoreContext = createContext<Store | null>(null);

const ERROR_VISIBLE_MS = 5000;
/** Pause before the trailing app-info refresh when changes came in during a call (W4). */
const APP_INFO_REFRESH_GAP_MS = 500;

export function StoreProvider({ children }: { children: ReactNode }) {
  const [state, dispatch] = useReducer(reducer, initialState);

  useEffect(() => {
    let cancelled = false;
    const unlisteners: UnlistenFn[] = [];
    // A `tickets-changed` that arrives before the initial `listTickets()` answer is newer.
    let ticketsFromEvent = false;
    let profilesFromEvent = false;
    let inboxFromEvent = false;

    // W4: the limits and reviewByDefault come from the workspace file, which may change while
    // the app runs (no file watcher). Fetch the app info again after agent/ticket changes and
    // when the window gets focus. Coalesced: one call in flight, at most one more after it.
    let infoBusy = false;
    let infoAgain = false;
    const refreshAppInfo = () => {
      if (cancelled) return;
      if (infoBusy) {
        infoAgain = true;
        return;
      }
      infoBusy = true;
      getAppInfo()
        .then((appInfo) => {
          if (!cancelled) dispatch({ type: "appInfo/set", appInfo });
        })
        // Keeps the last known info; the backend checks the limits on every spawn anyway.
        .catch(() => {})
        .finally(() => {
          infoBusy = false;
          if (infoAgain) {
            infoAgain = false;
            setTimeout(refreshAppInfo, APP_INFO_REFRESH_GAP_MS);
          }
        });
    };
    window.addEventListener("focus", refreshAppInfo);

    (async () => {
      try {
        // `agent-output` is consumed by AgentTerminal itself (workplace only).
        const subs = await Promise.all([
          onAgentsChanged((agents) => {
            dispatch({ type: "agents/set", agents });
            refreshAppInfo();
          }),
          onPermissionRequest((request) => dispatch({ type: "permission/add", request })),
          onPermissionResolved((p) =>
            dispatch({ type: "permission/remove", requestId: p.requestId }),
          ),
          onTicketsChanged((tickets) => {
            ticketsFromEvent = true;
            dispatch({ type: "tickets/set", tickets });
            refreshAppInfo();
          }),
          onProfilesChanged((profiles) => {
            profilesFromEvent = true;
            dispatch({ type: "profiles/set", profiles });
          }),
          onInboxChanged((inbox) => {
            inboxFromEvent = true;
            dispatch({ type: "inbox/set", inbox });
          }),
        ]);
        if (cancelled) {
          for (const u of subs) u();
          return;
        }
        unlisteners.push(...subs);
        // Only after the listeners exist: from now on permission requests go to this UI.
        await uiReady();
        // Both windows load the tickets; the island only uses the review count.
        // Profiles feed the spawn dialog and the "Agenter" tab (both windows keep them current).
        // Projects are optional for the start: a failure gives an empty list, no error.
        // The inbox is optional too: a failure leaves it null (the next `inbox-changed` fills it).
        const [agents, pending, appInfo, tickets, profiles, projects, inbox] = await Promise.all([
          listAgents(),
          listPendingPermissions(),
          getAppInfo(),
          listTickets(),
          listProfiles(),
          listProjects().catch(() => [] as Project[]),
          getInbox().catch(() => null),
        ]);
        if (cancelled) return;
        dispatch({ type: "agents/set", agents });
        if (!ticketsFromEvent) dispatch({ type: "tickets/set", tickets });
        if (!profilesFromEvent) dispatch({ type: "profiles/set", profiles });
        for (const request of pending) dispatch({ type: "permission/add", request });
        dispatch({ type: "appInfo/set", appInfo });
        dispatch({ type: "projects/set", projects });
        if (!inboxFromEvent && inbox !== null) dispatch({ type: "inbox/set", inbox });
      } catch (e) {
        if (!cancelled) dispatch({ type: "error/set", error: errorMessage(e) });
      }
    })();

    return () => {
      cancelled = true;
      window.removeEventListener("focus", refreshAppInfo);
      for (const u of unlisteners) u();
    };
  }, []);

  // Errors disappear after a few seconds.
  useEffect(() => {
    if (state.error === null) return;
    const t = setTimeout(() => dispatch({ type: "error/set", error: null }), ERROR_VISIBLE_MS);
    return () => clearTimeout(t);
  }, [state.error]);

  return <StoreContext.Provider value={{ state, dispatch }}>{children}</StoreContext.Provider>;
}

export function useStore(): Store {
  const store = useContext(StoreContext);
  if (store === null) throw new Error("useStore must be used inside <StoreProvider>");
  return store;
}

/**
 * Reloads the project list into the store (a failure keeps the old list). Called by the project
 * picker when it mounts and after anything that may create a project (spawn, assignment,
 * "Nyt projekt…", moving an agent).
 */
export function useRefreshProjects(): () => Promise<void> {
  const { dispatch } = useStore();
  return useCallback(
    () =>
      listProjects()
        .then((projects) => dispatch({ type: "projects/set", projects }))
        .catch(() => {}),
    [dispatch],
  );
}
