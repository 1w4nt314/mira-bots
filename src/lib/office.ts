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

export const FLOOR_DEFAULT = 384; // px, floor height in normal mode
export const FLOOR_MIN = 250; // px
export const TERM_MIN = 140; // px, smallest terminal height in normal mode
export const SPLITTER_H = 8; // px
export const TERM_LINE_H = 34; // px, minimised terminal line
export const WALL_H = 36; // px, wall strip ("more")
export const FLOOR_CHROME = 46; // px = seats padding 18 + row gap 10 + staff padding 18
export const SEAT_ROWS = 2;
export const DESK_H_MIN = 132;
export const DESK_H_MAX = 240;
export const FIG_RATIO = 0.56;
export const COMPACT = { deskH: 48, fig: 40 } as const; // max mode
export const SPLITTER_STEP = 16; // px per arrow key

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

/** max(FLOOR_MIN, min(wanted, available - TERM_MIN)), rounded. Non-finite wanted gives FLOOR_DEFAULT; non-finite available skips the upper bound. */
export function clampFloorHeight(wanted: number, available: number): number {
  const w = Number.isFinite(wanted) ? wanted : FLOOR_DEFAULT;
  const upper = Number.isFinite(available) ? available - TERM_MIN : Number.POSITIVE_INFINITY;
  return Math.max(FLOOR_MIN, Math.round(Math.min(w, upper)));
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
