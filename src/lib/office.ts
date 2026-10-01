// Office look for the workplace: pure helpers and constants (no DOM, no React).

export type OfficeDetail = "discreet" | "more";
export type TermMode = "normal" | "min" | "max";
export type OfficeItem = "mug" | "pens" | "papers" | "plant" | "lamp";

/** localStorage keys (read and written only through `persist.ts`). */
export const STORAGE_KEYS = {
  termMode: "mira-bots.workplace.termMode",
  floorHeight: "mira-bots.workplace.floorHeight",
  detail: "mira-bots.workplace.detail",
} as const;

// Measured at the default window (1100x720, Chromium): with FLOOR_DEFAULT 360 the xterm gets
// 11 rows with only the panel header above it (~119 px) and 9 with a running + a queued ticket
// (~150 px); 384 gave 7. Lower would push the desks towards DESK_H_MIN (360 gives 156, or 138
// with the wall), so 360 is the compromise. Heights stored by earlier versions are kept.
export const FLOOR_DEFAULT = 360; // px, floor height in normal mode
export const WALL_H = 36; // px, wall strip ("more")
// seats padding 10+8 + row gap 10 + staff padding 12+6 + staff border 1+1
export const FLOOR_CHROME = 48; // px
// Smallest xterm box (wrapper incl. its 2x8 px padding): 6 rows at ~17-18 px. AgentTerminal's
// wrapper has this as min-height, so a crowded panel overflows instead of squeezing the PTY.
export const XTERM_MIN = 124; // px
// Static lower bound for the terminal panel in normal mode: border 1 + header ~119 + a running
// and a queued ticket ~31 + XTERM_MIN = 275. Workplace raises it with the measured header/queue
// height (`termMinFor`), so a longer queue or a wrapped header still leaves XTERM_MIN.
export const TERM_MIN = 276; // px
export const SPLITTER_H = 8; // px
export const TERM_LINE_H = 34; // px, minimised terminal line
export const SEAT_ROWS = 2;
export const DESK_H_MIN = 132;
export const DESK_H_MAX = 240;
export const FIG_RATIO = 0.56;
export const COMPACT = { deskH: 48, fig: 28 } as const; // max mode
export const SPLITTER_STEP = 16; // px per arrow key

/** Smallest floor that shows both seat rows unclipped: rows * DESK_H_MIN + FLOOR_CHROME (+ WALL_H with "more"). */
export function floorMin(detail: OfficeDetail): number {
  return SEAT_ROWS * DESK_H_MIN + FLOOR_CHROME + (detail === "more" ? WALL_H : 0);
}
/** discreet */
export const FLOOR_MIN = SEAT_ROWS * DESK_H_MIN + FLOOR_CHROME;

/** Terminal panel minimum in normal mode for a measured header/queue height (0 = not measured). */
export function termMinFor(chromeH: number): number {
  const h = Number.isFinite(chromeH) && chromeH > 0 ? chromeH : 0;
  // + 1: the panel's border-top sits outside the measured header/queue block
  return Math.max(TERM_MIN, Math.ceil(h) + 1 + XTERM_MIN);
}

export const ITEM_SETS: readonly (readonly OfficeItem[])[] = [
  ["mug", "pens"],
  ["papers", "plant"],
  ["lamp", "pens"],
  ["plant", "papers"],
  ["lamp", "papers"],
  ["pens", "mug"],
  ["mug", "plant"],
  ["lamp", "mug"],
];

/** FNV-1a 32-bit, unsigned. */
export function hashKey(key: string): number {
  let h = 0x811c9dc5;
  for (let i = 0; i < key.length; i++) {
    h ^= key.charCodeAt(i);
    h = Math.imul(h, 0x01000193) >>> 0;
  }
  return h >>> 0;
}

/** Deterministic desk items for an agent id (or seat key). */
export function itemsFor(key: string): OfficeItem[] {
  return [...ITEM_SETS[hashKey(key) % ITEM_SETS.length]];
}

/**
 * max(minFloor, min(wanted, available - minTerm)), rounded. The floor minimum wins when both
 * cannot be met (then the xterm box keeps XTERM_MIN and the panel overflows instead).
 * Non-finite wanted gives FLOOR_DEFAULT; non-finite available skips the upper bound.
 */
export function clampFloorHeight(
  wanted: number,
  available: number,
  minFloor: number = FLOOR_MIN,
  minTerm: number = TERM_MIN,
): number {
  const w = Number.isFinite(wanted) ? wanted : FLOOR_DEFAULT;
  const upper = Number.isFinite(available) ? available - minTerm : Number.POSITIVE_INFINITY;
  return Math.max(minFloor, Math.round(Math.min(w, upper)));
}

/** inner = floorHeight - (more ? WALL_H : 0) - FLOOR_CHROME; deskH = clamp(round(inner / rows), 132, 240); fig = round(deskH * FIG_RATIO). */
export function deskLayout(
  floorHeight: number,
  detail: OfficeDetail,
  rows = SEAT_ROWS,
): { deskH: number; fig: number } {
  const inner = floorHeight - (detail === "more" ? WALL_H : 0) - FLOOR_CHROME;
  const raw = Math.round(inner / Math.max(1, rows));
  const deskH = Number.isFinite(raw) ? Math.min(DESK_H_MAX, Math.max(DESK_H_MIN, raw)) : DESK_H_MIN;
  return { deskH, fig: Math.round(deskH * FIG_RATIO) };
}

export function parseTermMode(s: string | null): TermMode {
  return s === "min" || s === "max" || s === "normal" ? s : "normal";
}

export function parseDetail(s: string | null): OfficeDetail {
  return s === "more" ? "more" : "discreet";
}

export function parseFloorHeight(s: string | null): number {
  if (s === null) return FLOOR_DEFAULT;
  const n = Number(s);
  return Number.isFinite(n) && n > 0 ? Math.round(n) : FLOOR_DEFAULT;
}
