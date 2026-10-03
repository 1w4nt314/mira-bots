// Pure helpers for the inbox (step 6c): grouping, filtering, sorting, labels, the write-back
// badge and the polling rules. No React, no IPC, no DOM; only type imports, so it transpiles on
// its own (scripts/test-inbox.mjs).
import type {
  ExternalRef,
  InboxItemSummary,
  InboxRefreshReason,
  InboxStatus,
  SeatKind,
} from "./types";
import type { ProjectFilter } from "./projects";

/** The polling timer (only while the workplace window is visible). */
export const POLL_INTERVAL_MS = 60_000;
/** Timer ticks are not exact: a fetch this much short of the interval still counts as due. */
export const POLL_TIMER_SLACK_MS = 1_000;
/** A focus (or "visible again") trigger fetches at most this often. */
export const FOCUS_FLOOR_MS = 15_000;
/** The "Opdatér" button fetches at most this often. */
export const MANUAL_FLOOR_MS = 5_000;
/** Labels shown on a card before "+k". */
export const LABELS_VISIBLE = 5;
/** The limit of `gh issue list` (mirrors `INBOX_GITHUB_LIMIT` in Rust; for the hint). */
export const GITHUB_LIMIT = 100;
/** Drag-and-drop id of an inbox card (step 6c B5); never collides with `ticket:`/`agent:`. */
export const INBOX_DRAG_PREFIX = "inbox:";

// --- grouping, filter, sort -------------------------------------------------------------------

export interface InboxGroups {
  new: InboxItemSummary[];
  started: InboxItemSummary[];
  dismissed: InboxItemSummary[];
}

/** The time an item is ordered by: GitHub's `updatedAt`, else when the app first saw it. */
function sortTime(i: InboxItemSummary): number {
  if (i.updatedAt !== null) {
    const t = Date.parse(i.updatedAt);
    if (!Number.isNaN(t)) return t;
  }
  return i.seenAt;
}

/** Newest first; ties by title and id so the order never jumps between refreshes. */
export function sortInbox(items: readonly InboxItemSummary[]): InboxItemSummary[] {
  return [...items].sort((a, b) => {
    const d = sortTime(b) - sortTime(a);
    if (d !== 0) return d;
    const t = a.title.toLowerCase().localeCompare(b.title.toLowerCase());
    return t !== 0 ? t : a.id < b.id ? -1 : a.id > b.id ? 1 : 0;
  });
}

/** Splits by state (each group sorted); `gone` items are never sent by the backend. */
export function groupInbox(items: readonly InboxItemSummary[]): InboxGroups {
  const groups: InboxGroups = { new: [], started: [], dismissed: [] };
  for (const i of sortInbox(items)) groups[i.state].push(i);
  return groups;
}

/** Project ids compare case-insensitively (same fold as `sameProjectId` in projects.ts). */
const fold = (s: string) => s.toLowerCase();

/**
 * Whether an item is shown under the ticket list's project filter (the same filter values as
 * `matchesFilter`). An item without a project that lists candidates shows under each of them;
 * under "Uden projekt" it shows when it has no project.
 */
export function matchesInboxFilter(
  item: Pick<InboxItemSummary, "project"> & { candidates?: readonly string[] },
  f: ProjectFilter,
): boolean {
  if (f === "all") return true;
  if (f === "none") return item.project === null;
  const id = fold(f.id);
  if (item.project !== null) return fold(item.project) === id;
  return (item.candidates ?? []).some((c) => fold(c) === id);
}

// --- labels and texts -------------------------------------------------------------------------

/** The badge of a ticket's source: "GitHub #123", "GitHub" without a number, "indbakke" for a file. */
export function inboxLabel(e: ExternalRef | null): string | null {
  if (e === null) return null;
  if (e.kind === "github") return e.number === null ? "GitHub" : `GitHub #${e.number}`;
  return "indbakke";
}

/** Short badge of an item: "GitHub" or "fil" (the card also shows "#n" as a button). */
export function sourceBadge(kind: InboxItemSummary["kind"]): string {
  return kind === "github" ? "GitHub" : "fil";
}

/** "GitHub #123 i owner/name" / "fil fejl-1.md" (the dialog's source line). */
export function sourceLine(
  i: Pick<InboxItemSummary, "kind" | "number" | "repo" | "path">,
): string {
  if (i.kind === "github") {
    const n = i.number === null ? "" : ` #${i.number}`;
    return `GitHub${n}${i.repo === null ? "" : ` i ${i.repo}`}`;
  }
  return i.path === null ? "fil" : `fil ${i.path}`;
}

/**
 * The source id with its labels as the dialog's subtitle: `github:o/r[bug,ui]` → "GitHub o/r
 * [bug, ui]", `github:o/r` → "GitHub o/r", `folder:web` → "indbakke i web", `folder:_rod` →
 * "indbakke". The repo's own spelling (`repo`) wins over the lower-cased id.
 */
export function sourceIdText(i: Pick<InboxItemSummary, "sourceId" | "repo">): string {
  const id = i.sourceId;
  if (id.startsWith("github:")) {
    const rest = id.slice("github:".length);
    const open = rest.indexOf("[");
    const name = open === -1 ? rest : rest.slice(0, open);
    const labels =
      open === -1 || !rest.endsWith("]")
        ? []
        : rest
            .slice(open + 1, -1)
            .split(",")
            .filter((l) => l !== "");
    const shown = i.repo !== null && fold(i.repo) === fold(name) ? i.repo : name;
    return labels.length === 0 ? `GitHub ${shown}` : `GitHub ${shown} [${labels.join(", ")}]`;
  }
  if (id.startsWith("folder:")) {
    const key = id.slice("folder:".length);
    return key === "_rod" || key === "" ? "indbakke" : `indbakke i ${key}`;
  }
  return id;
}

/** At most `max` labels plus how many were left out ("+k"). */
export function visibleLabels(
  labels: readonly string[],
  max: number = LABELS_VISIBLE,
): { shown: string[]; extra: number } {
  return { shown: labels.slice(0, max), extra: Math.max(0, labels.length - max) };
}

/** "Ligner ticket {short}: «{titel}»". */
export function duplicateText(d: NonNullable<InboxItemSummary["duplicateOf"]>): string {
  return `Ligner ticket ${d.shortId}: «${d.title}»`;
}

/** The empty list's text; `filter` "all" has no " i dette projekt". */
export function inboxEmptyText(filter: ProjectFilter): string {
  return `Ingen nye emner i indbakken${filter === "all" ? "" : " i dette projekt"}`;
}

/** What the Start dialog's confirm button says. */
export type StartTarget =
  | { kind: "agent"; agentName: string }
  | { kind: "empty"; seatKind: SeatKind };

export function startButtonText(target: StartTarget | null, busy: boolean, github: boolean): string {
  if (busy) return github ? "Henter fra GitHub…" : "Starter…";
  if (target === null) return "Start";
  return target.kind === "agent" ? `Start og tildel til ${target.agentName}` : "Start og start agent";
}

// --- write-back badge -------------------------------------------------------------------------

const BADGE_OK = "bg-emerald-500/15 text-emerald-800 dark:text-emerald-200";
const BADGE_BUSY = "bg-sky-500/15 text-sky-800 dark:text-sky-200";
const BADGE_FAILED = "bg-rose-500/15 text-rose-700 dark:text-rose-300";

/**
 * The write-back badge of a Done ticket from the inbox (`writeBack.comment`): none → null,
 * inflight "melder tilbage…", done "meldt tilbage ✓" (title = the comment's url), failed
 * "ikke meldt tilbage" (title = the error, shown as the backend wrote it).
 */
export function writeBackBadge(e: ExternalRef): { text: string; cls: string; title: string } | null {
  const w = e.writeBack;
  switch (w.comment) {
    case "none":
      return null;
    case "inflight":
      return { text: "melder tilbage…", cls: BADGE_BUSY, title: "Tilbagemeldingen er i gang" };
    case "done":
      return { text: "meldt tilbage ✓", cls: BADGE_OK, title: w.commentUrl ?? "Meldt tilbage" };
    case "failed":
      return {
        text: "ikke meldt tilbage",
        cls: BADGE_FAILED,
        title: w.lastError ?? "Tilbagemeldingen mislykkedes",
      };
  }
}

// --- status text ------------------------------------------------------------------------------

const two = (n: number) => String(n).padStart(2, "0");

/** Local "hh:mm" of a Unix-ms time. */
export function clockText(ms: number): string {
  const d = new Date(ms);
  return `${two(d.getHours())}:${two(d.getMinutes())}`;
}

/** "Henter…" while refreshing, else "Seneste hentning kl. hh:mm" (or "Ikke hentet endnu"). */
export function fetchStatusText(s: InboxStatus): string {
  if (s.refreshing) return "Henter…";
  if (s.lastRefreshAt === null) return "Ikke hentet endnu";
  return `Seneste hentning kl. ${clockText(s.lastRefreshAt)}`;
}

/** One line per failing source: its label and the backend's text, unchanged. */
export function sourceErrors(s: InboxStatus): { id: string; label: string; error: string }[] {
  const out: { id: string; label: string; error: string }[] = [];
  for (const src of s.sources) {
    if (src.error !== null) out.push({ id: src.id, label: src.label, error: src.error });
  }
  return out;
}

/** "Højst 100 åbne issues pr. repo vises." when a source hit the limit, else null. */
export function cappedHint(s: InboxStatus): string | null {
  return s.sources.some((src) => src.capped)
    ? `Højst ${GITHUB_LIMIT} åbne issues pr. repo vises.`
    : null;
}

/** Notes of the latest fetches (files skipped, …) with the source they belong to. */
export function sourceNotes(s: InboxStatus): string[] {
  return s.sources.flatMap((src) => src.notes);
}

/** The section shows when something is new, or a source failed (so the error is not hidden). */
export function showInboxSection(newCount: number, s: InboxStatus | null): boolean {
  return newCount > 0 || (s !== null && sourceErrors(s).length > 0);
}

// --- polling rules ----------------------------------------------------------------------------

/** `last` null = never; true once `floorMs` has passed since it. */
export function canRefresh(last: number | null, floorMs: number, now: number): boolean {
  return last === null || now - last >= floorMs;
}

export type PollTrigger = "mount" | "timer" | "visible" | "focus" | "manual";

/**
 * Whether a trigger fetches now, and with which `refresh_inbox` reason (null = not now).
 * - `mount`: always ("startup").
 * - `timer`: a visible window and (almost) 60 s since the last ask.
 * - `visible` (window visible again): visible and 60 s since the last ask.
 * - `focus`: visible and 15 s since the last ask.
 * - `manual` ("Opdatér"): 5 s since the last ask.
 * `lastAt` is when this hook last asked the backend (not the backend's own status).
 */
export function pollReason(
  trigger: PollTrigger,
  visible: boolean,
  lastAt: number | null,
  now: number,
): InboxRefreshReason | null {
  switch (trigger) {
    case "mount":
      return "startup";
    case "manual":
      return canRefresh(lastAt, MANUAL_FLOOR_MS, now) ? "manual" : null;
    case "timer":
      return visible && canRefresh(lastAt, POLL_INTERVAL_MS - POLL_TIMER_SLACK_MS, now)
        ? "timer"
        : null;
    case "visible":
      return visible && canRefresh(lastAt, POLL_INTERVAL_MS, now) ? "focus" : null;
    case "focus":
      return visible && canRefresh(lastAt, FOCUS_FLOOR_MS, now) ? "focus" : null;
  }
}

// --- drag and drop ----------------------------------------------------------------------------

export const inboxDragId = (id: string): string => `${INBOX_DRAG_PREFIX}${id}`;

/** The inbox item id of a drag id, or null for anything else. */
export function draggedInboxId(id: string | number | null | undefined): string | null {
  if (typeof id !== "string" || !id.startsWith(INBOX_DRAG_PREFIX)) return null;
  const rest = id.slice(INBOX_DRAG_PREFIX.length);
  return rest === "" ? null : rest;
}
