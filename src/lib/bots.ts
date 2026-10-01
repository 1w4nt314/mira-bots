// Bot figures: static SVG lookup or the generated specialist figure per theme/roles/state, the status -> figure-state mapping, the short
// "done" phase after a finished turn, and the colour-scheme hook.
import { useEffect, useMemo, useState, useSyncExternalStore } from "react";
import { renderBot } from "./botCore";
import { figureNameFor, sortRoles } from "./roles";
import type { AgentInfo, AgentStatus, AgentStatusKind, BotState, Role } from "./types";

/** File name part of a static figure (`bot-<name>-<state>.svg`). */
export type BotRole =
  | "none"
  | "coder"
  | "researcher"
  | "reviewer"
  | "koord"
  | "planner"
  | "debugger";

/** How a role set is drawn: a bundled static SVG, or the generated (specialist) figure. */
export type Figure = { kind: "static"; name: BotRole } | { kind: "dynamic" };

/**
 * Figure for a role set (mirrors `prefix_for` in Rust, except that every specialist is drawn by
 * `renderBot`):
 *
 * | roles             | specialist | figure                    |
 * |-------------------|------------|---------------------------|
 * | [coder]           | false      | static "coder"            |
 * | [coordinator]     | false      | static "koord"            |
 * | [planner]         | false      | static "planner"          |
 * | []                | false      | static "none"             |
 * | [coder]           | true       | dynamic                   |
 * | [coder, reviewer] | any        | dynamic                   |
 * | []                | true       | dynamic                   |
 */
export function figureFor(roles: readonly Role[], specialist: boolean): Figure {
  if (roles.length === 1 && !specialist) return { kind: "static", name: figureNameFor(roles[0]) };
  if (roles.length === 0 && !specialist) return { kind: "static", name: "none" };
  return { kind: "dynamic" };
}

export type Theme = "dark" | "light";

/** How long the "done" figure shows after an agent went back to idle from a working state. */
export const DONE_STATE_MS = 8000;

// All static figures (2 themes x 10 names x 4 states), bundled as separate asset URLs. The
// static specialist files are no longer used (specialists are generated), but stay in assets.
const BOT_SVGS = import.meta.glob<string>("../assets/bots/*/bot-*.svg", {
  eager: true,
  query: "?url",
  import: "default",
});

const FALLBACK_KEY = "../assets/bots/dark/bot-none-idle.svg";

function staticSrc(theme: Theme, name: BotRole, state: BotState): string {
  return (
    BOT_SVGS[`../assets/bots/${theme}/bot-${name}-${state}.svg`] ??
    BOT_SVGS[`../assets/bots/${theme}/bot-none-idle.svg`] ??
    BOT_SVGS[FALLBACK_KEY] ??
    ""
  );
}

// Generated figures by `theme|state|roles|spec` (at most 2 x 4 x 64 x 2 entries).
const DYNAMIC_CACHE = new Map<string, string>();

/**
 * `src` of a figure: a static asset URL, or for a specialist a `data:image/svg+xml` URL of
 * `renderBot` (memoised). Only known role names reach the SVG, never user text.
 */
// TODO(windows-verify): the generated specialist figure (data URL with CSS animations in <img>)
// renders in WebView2 on seats, chips and in the profile editor preview, in both themes; the
// static planner/debugger figures show (plan D.59).
export function botSrc(
  theme: Theme,
  roles: readonly Role[],
  specialist: boolean,
  state: BotState,
): string {
  const fig = figureFor(roles, specialist);
  if (fig.kind === "static") return staticSrc(theme, fig.name, state);
  const names = sortRoles(roles).map(figureNameFor);
  const key = `${theme}|${state}|${names.join(",")}|${specialist ? 1 : 0}`;
  let url = DYNAMIC_CACHE.get(key);
  if (url === undefined) {
    const svg = renderBot({ state, roles: names, dark: theme === "dark", specialist, id: "screen" });
    url = `data:image/svg+xml;utf8,${encodeURIComponent(svg)}`;
    DYNAMIC_CACHE.set(key, url);
  }
  return url;
}

/**
 * Figure state for an agent status.
 *
 * | status                                  | doneUntil > now | figure |
 * |-----------------------------------------|-----------------|--------|
 * | waitingPermission                       | –               | wait   |
 * | thinking, reading, editing, running     | –               | work   |
 * | idle                                    | yes             | done   |
 * | idle                                    | no / undefined  | idle   |
 * | starting                                | –               | idle   |
 * | exited                                  | –               | idle (the figure is dimmed and badged "afsluttet" by BotFigure) |
 *
 * `doneUntil` is set by `computeDoneMarks` when an agent goes from a working state (or waiting
 * for permission) to idle, i.e. after a Stop event; it lasts `DONE_STATE_MS`.
 */
export function botStateFor(
  status: AgentStatus,
  doneUntil: number | undefined,
  now: number,
): BotState {
  switch (status.kind) {
    case "waitingPermission":
      return "wait";
    case "thinking":
    case "reading":
    case "editing":
    case "running":
      return "work";
    case "idle":
      return doneUntil !== undefined && now < doneUntil ? "done" : "idle";
    case "starting":
    case "exited":
      return "idle";
  }
}

const ACTIVE_KINDS: ReadonlySet<AgentStatusKind> = new Set<AgentStatusKind>([
  "thinking",
  "reading",
  "editing",
  "running",
  "waitingPermission",
]);

/**
 * New "done" marks (agentId -> until ms). An agent whose previous status was active and whose
 * current status is idle gets `now + DONE_STATE_MS`; existing marks survive only while their
 * agent is still present and idle. Pure: `marks` is not mutated.
 */
export function computeDoneMarks(
  prev: ReadonlyMap<string, AgentStatusKind>,
  agents: readonly AgentInfo[],
  marks: ReadonlyMap<string, number>,
  now: number,
): Map<string, number> {
  const next = new Map<string, number>();
  for (const a of agents) {
    if (a.status.kind !== "idle") continue;
    const before = prev.get(a.id);
    if (before !== undefined && ACTIVE_KINDS.has(before)) next.set(a.id, now + DONE_STATE_MS);
    else {
      const old = marks.get(a.id);
      if (old !== undefined) next.set(a.id, old);
    }
  }
  return next;
}

function kindsOf(agents: readonly AgentInfo[]): Map<string, AgentStatusKind> {
  return new Map(agents.map((a) => [a.id, a.status.kind]));
}

interface DoneTracking {
  agents: readonly AgentInfo[];
  prev: Map<string, AgentStatusKind>;
  marks: Map<string, number>;
}

/** Figure state per agent id. Re-renders once when the earliest "done" mark runs out. */
export function useBotStates(agents: readonly AgentInfo[]): Map<string, BotState> {
  const [track, setTrack] = useState<DoneTracking>(() => ({
    agents,
    prev: kindsOf(agents),
    marks: new Map(),
  }));
  const [now, setNow] = useState(() => Date.now());

  // Derive from the previous agents list during render (React's "previous props" pattern).
  let current = track;
  if (track.agents !== agents) {
    const t = Date.now();
    current = { agents, prev: kindsOf(agents), marks: computeDoneMarks(track.prev, agents, track.marks, t) };
    setTrack(current);
    if (t > now) setNow(t);
  }
  const marks = current.marks;

  // Timer only while a mark is still running: wakes up when the earliest one expires.
  useEffect(() => {
    let next = Infinity;
    for (const until of marks.values()) if (until > now && until < next) next = until;
    if (next === Infinity) return;
    const t = setTimeout(() => setNow(Date.now()), next - now + 50);
    return () => clearTimeout(t);
  }, [marks, now]);

  return useMemo(() => {
    const out = new Map<string, BotState>();
    for (const a of agents) out.set(a.id, botStateFor(a.status, marks.get(a.id), now));
    return out;
  }, [agents, marks, now]);
}

const DARK_QUERY = "(prefers-color-scheme: dark)";

function subscribeTheme(cb: () => void): () => void {
  const mql = window.matchMedia(DARK_QUERY);
  mql.addEventListener("change", cb);
  return () => mql.removeEventListener("change", cb);
}

function themeSnapshot(): Theme {
  return window.matchMedia(DARK_QUERY).matches ? "dark" : "light";
}

/** Current colour scheme; follows the OS/WebView setting live. */
// TODO(windows-verify): prefers-color-scheme follows the Windows app theme in both windows and
// a theme switch swaps bot SVGs and the xterm theme live (plan D.23).
export function useTheme(): Theme {
  return useSyncExternalStore(subscribeTheme, themeSnapshot, () => "dark");
}
