// Bot figures: SVG lookup per theme/role/state, the status -> figure-state mapping, the short
// "done" phase after a finished turn, and the colour-scheme hook.
import { useEffect, useMemo, useState, useSyncExternalStore } from "react";
import type { AgentInfo, AgentRole, AgentStatus, AgentStatusKind, BotState } from "./types";

export type BotRole = AgentRole;
export type Theme = "dark" | "light";

/** How long the "done" figure shows after an agent went back to idle from a working state. */
export const DONE_STATE_MS = 8000;

// All 40 figures (2 themes x 5 roles x 4 states), bundled as separate asset URLs.
const BOT_SVGS = import.meta.glob<string>("../assets/bots/*/bot-*.svg", {
  eager: true,
  query: "?url",
  import: "default",
});

const FALLBACK_KEY = "../assets/bots/dark/bot-none-idle.svg";

/** URL of the figure; falls back to the neutral idle bot so `src` is never undefined. */
export function botSrc(theme: Theme, role: BotRole, state: BotState): string {
  return (
    BOT_SVGS[`../assets/bots/${theme}/bot-${role}-${state}.svg`] ??
    BOT_SVGS[`../assets/bots/${theme}/bot-none-idle.svg`] ??
    BOT_SVGS[FALLBACK_KEY] ??
    ""
  );
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
