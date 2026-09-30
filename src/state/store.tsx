import {
  createContext,
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
  listAgents,
  listPendingPermissions,
  onAgentsChanged,
  onPermissionRequest,
  onPermissionResolved,
  uiReady,
} from "../lib/ipc";
import type { AgentInfo, AppInfo, PermissionRequestInfo } from "../lib/types";

export interface State {
  agents: AgentInfo[];
  pending: PermissionRequestInfo[];
  /** Expanded by hovering the island. */
  expanded: boolean;
  error: string | null;
  appInfo: AppInfo | null;
}

export type Action =
  | { type: "agents/set"; agents: AgentInfo[] }
  | { type: "permission/add"; request: PermissionRequestInfo }
  | { type: "permission/remove"; requestId: string }
  | { type: "ui/expand" }
  | { type: "ui/collapse" }
  | { type: "error/set"; error: string | null }
  | { type: "appInfo/set"; appInfo: AppInfo };

export const initialState: State = {
  agents: [],
  pending: [],
  expanded: false,
  error: null,
  appInfo: null,
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
  }
}

interface Store {
  state: State;
  dispatch: Dispatch<Action>;
}

const StoreContext = createContext<Store | null>(null);

const ERROR_VISIBLE_MS = 5000;

export function StoreProvider({ children }: { children: ReactNode }) {
  const [state, dispatch] = useReducer(reducer, initialState);

  useEffect(() => {
    let cancelled = false;
    const unlisteners: UnlistenFn[] = [];

    (async () => {
      try {
        // `agent-output` is consumed by AgentTerminal itself (workplace only).
        const subs = await Promise.all([
          onAgentsChanged((agents) => dispatch({ type: "agents/set", agents })),
          onPermissionRequest((request) => dispatch({ type: "permission/add", request })),
          onPermissionResolved((p) =>
            dispatch({ type: "permission/remove", requestId: p.requestId }),
          ),
        ]);
        if (cancelled) {
          for (const u of subs) u();
          return;
        }
        unlisteners.push(...subs);
        // Only after the listeners exist: from now on permission requests go to this UI.
        await uiReady();
        const [agents, pending, appInfo] = await Promise.all([
          listAgents(),
          listPendingPermissions(),
          getAppInfo(),
        ]);
        if (cancelled) return;
        dispatch({ type: "agents/set", agents });
        for (const request of pending) dispatch({ type: "permission/add", request });
        dispatch({ type: "appInfo/set", appInfo });
      } catch (e) {
        if (!cancelled) dispatch({ type: "error/set", error: errorMessage(e) });
      }
    })();

    return () => {
      cancelled = true;
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
