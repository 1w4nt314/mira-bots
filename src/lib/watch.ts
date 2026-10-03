// Pure helpers for the watch and the notices (step 6d): badge texts for parked inbox items, the
// island chip, the Diagnostics lines and the filter that hides notices whose cause is gone.
// Only type imports: scripts/test-watch.mjs transpiles this file and runs it in node. Relative
// times live in tickets.ts (`relativeText`); this file takes `now` and returns texts.
import type {
  AgentInfo,
  Notice,
  NoticeKind,
  TicketSummary,
  WaitingInfo,
  WatchProjectView,
  WatchView,
} from "./types";

// --- time -------------------------------------------------------------------------------------

function pad2(n: number): string {
  return n < 10 ? `0${n}` : String(n);
}

/** `HH:MM` in local time (a colon, like the backend's `hhmm`). */
export function hhmm(ms: number): string {
  const d = new Date(ms);
  return `${pad2(d.getHours())}:${pad2(d.getMinutes())}`;
}

/** Local calendar day as `yyyymmdd`. */
function localDay(ms: number): number {
  const d = new Date(ms);
  return d.getFullYear() * 10_000 + (d.getMonth() + 1) * 100 + d.getDate();
}

/** The calendar day after the day of `ms`, via the date parts (DST days are 23/25 hours). */
function dayAfter(ms: number): number {
  const d = new Date(ms);
  return localDay(new Date(d.getFullYear(), d.getMonth(), d.getDate() + 1).getTime());
}

/**
 * When the budget is free again, relative to `now`.
 *
 * | nextMs                       | text                   |
 * |------------------------------|------------------------|
 * | null                         | ""                     |
 * | same local day (or earlier)  | `næste: 14:05`         |
 * | the next local day           | `i morgen 07:00`       |
 * | later                        | `næste: 05.10 07:00`   |
 */
export function nextFreeText(nextMs: number | null, now: number): string {
  if (nextMs === null) return "";
  const day = localDay(nextMs);
  if (day <= localDay(now)) return `næste: ${hhmm(nextMs)}`;
  if (day === dayAfter(now)) return `i morgen ${hhmm(nextMs)}`;
  const d = new Date(nextMs);
  return `næste: ${pad2(d.getDate())}.${pad2(d.getMonth() + 1)} ${hhmm(nextMs)}`;
}

// --- parked inbox items -----------------------------------------------------------------------

/** Full class strings (Tailwind scans the source, so no string building here). */
export const BADGE_WAIT = "bg-amber-400/20 text-amber-800 dark:text-amber-200";
export const BADGE_FAILED = "bg-rose-500/15 text-rose-700 dark:text-rose-300";

/**
 * The badge of an inbox item the watch parked (C6d.5 texts). The budget text is rebuilt from
 * `nextAt` so "i morgen" shows up; every other text is the backend's. A failed start shows the
 * short text and keeps the error in the title.
 */
export function waitingBadge(w: WaitingInfo, now: number): { text: string; title: string; cls: string } {
  switch (w.reason) {
    case "budget": {
      const next = nextFreeText(w.nextAt, now);
      return {
        text: next === "" ? w.text : `venter på budget (${next})`,
        title: "Vagtens budget er brugt; emnet startes når der igen er plads (du kan altid starte det selv)",
        cls: BADGE_WAIT,
      };
    }
    case "seat":
      return {
        text: w.text,
        title: "Ingen fri arbejdsplads til vagten (rækken, projektloftet eller watch.maxAgents er fuldt)",
        cls: BADGE_WAIT,
      };
    case "planner":
      return {
        text: w.text,
        title: "Forløbet kræver en planlægger, og vagten starter ingen stabsagenter — start en selv",
        cls: BADGE_WAIT,
      };
    case "duplicate":
      return {
        text: w.text,
        title: "Ligner en åben ticket i projektet; vagten rører ikke emnet",
        cls: BADGE_WAIT,
      };
    case "playbook":
      return {
        text: w.text,
        title: "Sæt watch.playbook i project.json (et navn, eller byLabel/default)",
        cls: BADGE_WAIT,
      };
    case "failed":
      return { text: "vagt: start fejlede", title: w.text, cls: BADGE_FAILED };
  }
}

// --- the island chip and the master switch ----------------------------------------------------

/**
 * | view                                  | chip                 |
 * |---------------------------------------|----------------------|
 * | null                                  | null                 |
 * | active ≥ 1                            | `Vagt: 1 projekt`    |
 * | paused and a project has `enabled`    | `Vagt: pause`        |
 * | otherwise                             | null                 |
 */
export function watchChipText(v: WatchView | null): string | null {
  if (v === null) return null;
  if (v.active > 0) return `Vagt: ${v.active} ${v.active === 1 ? "projekt" : "projekter"}`;
  if (v.paused && v.projects.some((p) => p.enabled)) return "Vagt: pause";
  return null;
}

/** "Stop vagten" is offered while something is actually watching. */
export function showStopWatch(v: WatchView | null): boolean {
  return v !== null && v.active > 0 && !v.paused;
}

/** "Genoptag"/"Start vagten igen" is offered while the master pause hides enabled projects. */
export function showResumeWatch(v: WatchView | null): boolean {
  return v !== null && v.paused && v.projects.some((p) => p.enabled);
}

/** `{n} besked(er)` for the badge; "" at 0. */
export function unreadText(n: number): string {
  if (n <= 0) return "";
  return `${n} ${n === 1 ? "besked" : "beskeder"}`;
}

// --- Diagnostics → Projekter ------------------------------------------------------------------

/** `3/3 i timen · 7/10 i dag · 1/2 agenter`. */
export function budgetText(p: WatchProjectView): string {
  return `${p.usedHour}/${p.capHour} i timen · ${p.usedDay}/${p.capDay} i dag · ${p.agents}/${p.maxAgents} agenter`;
}

/** The end of `quietHours` ("23-07" → "07:00"); null when the text has another shape. */
export function quietEndText(quiet: string | null): string | null {
  if (quiet === null) return null;
  const m = /^\s*(\d{1,2})\s*-\s*(\d{1,2})\s*$/.exec(quiet);
  if (m === null) return null;
  const end = Number(m[2]);
  if (!Number.isInteger(end) || end < 0 || end > 24) return null;
  return `${pad2(end % 24)}:00`;
}

/**
 * The status after "Vagt:" in Diagnostics. An inactive project shows the backend's reason (the
 * first failing condition, C6d.5); the short texts below are fallbacks for a view without one.
 */
export function watchStatusText(p: WatchProjectView): string {
  if (p.active) {
    if (!p.inQuiet) return "aktiv";
    const end = quietEndText(p.quiet);
    return end === null ? "stille timer" : `stille timer til ${end}`;
  }
  if (p.reason !== null) return p.reason;
  if (p.tripped) return "stoppet efter 3 fejl";
  if (p.paused) return "på pause";
  if (!p.enabled) return "watch.enabled mangler i project.json";
  return "inaktiv";
}

/** "Hold vagt" can only be switched when the file allows the watch. */
export function canHoldWatch(p: WatchProjectView): boolean {
  return p.enabled;
}

/** The project's row in the view (ids compare case-insensitively, like the backend's `same_id`). */
export function projectWatch(v: WatchView | null, id: string): WatchProjectView | null {
  if (v === null) return null;
  const key = id.toLowerCase();
  return v.projects.find((p) => p.id.toLowerCase() === key) ?? null;
}

/** The whole watch line of a project for Diagnostics (status, budget, next free time, playbook). */
export function watchLineText(p: WatchProjectView, now: number): string {
  const parts = [`Vagt: ${watchStatusText(p)}`, budgetText(p)];
  const next = nextFreeText(p.nextFreeAt, now);
  if (next !== "") parts.push(next);
  parts.push(p.playbook === null ? "ingen playbook" : `playbook: ${p.playbook}`);
  return parts.join(" · ");
}

/** Lines for the copied diagnostics text: `watch: til|på pause`, then one `watch.<id>: …` per project. */
export function watchCopyLines(v: WatchView | null, now: number): string[] {
  if (v === null) return [];
  const out = [`watch: ${v.paused ? "på pause" : "til"} · ${v.active} aktive · ${v.global.usedHour}/${v.global.capHour} i timen · ${v.global.usedDay}/${v.global.capDay} i dag`];
  for (const p of v.projects) out.push(`watch.${p.id}: ${watchLineText(p, now)}`);
  return out;
}

/** Warnings for Diagnostics: an active watch project without its own worktree shares the folder. */
export function watchWarnings(v: WatchView | null, git: string | undefined): string[] {
  if (v === null || git !== "off") return [];
  return v.projects
    .filter((p) => p.active)
    .map((p) => `Vagt-projektet «${p.id}» kører uden git: worktree — agenten og du deler samme mappe`);
}

// --- notices ----------------------------------------------------------------------------------

/** The kinds in the backend's order (`NoticeKind::ALL`). */
export const NOTICE_KINDS: readonly NoticeKind[] = [
  "escalated",
  "flowReview",
  "permissionWaiting",
  "trustWaiting",
  "writeBackFailed",
  "budgetReached",
  "watchTripped",
  "agentExited",
];

/** Danish labels (C6d.5; the same as the backend's `label_da`). */
export const NOTICE_KIND_LABEL: Record<NoticeKind, string> = {
  escalated: "eskaleret",
  flowReview: "forløb til godkendelse",
  permissionWaiting: "tilladelse venter",
  trustWaiting: "agent venter i terminalen",
  writeBackFailed: "tilbagemelding fejlede",
  budgetReached: "budget nået",
  watchTripped: "vagt stoppet",
  agentExited: "agent afsluttet",
};

/** Guard for `notifyOff` strings from a settings file or a newer build. */
export function isNoticeKind(s: string): s is NoticeKind {
  return (NOTICE_KINDS as readonly string[]).includes(s);
}

/**
 * Hides notices whose cause is gone, judged on the current lists (the queue itself only grows):
 *
 * | kind                             | kept while                                             |
 * |----------------------------------|--------------------------------------------------------|
 * | escalated                        | the ticket exists, is escalated and in review          |
 * | flowReview                       | the ticket exists and is in review                     |
 * | writeBackFailed                  | the ticket's comment or close write-back is `failed`   |
 * | permissionWaiting                | the agent exists and waits for a permission            |
 * | trustWaiting                     | the agent exists and is still starting                 |
 * | budgetReached, watchTripped, agentExited | always                                         |
 *
 * A ticket/agent notice without an id is kept (nothing to judge). Order is kept (newest first).
 */
export function visibleNotices(
  notices: readonly Notice[],
  tickets: readonly TicketSummary[],
  agents: readonly AgentInfo[],
): Notice[] {
  const ticketById = new Map(tickets.map((t) => [t.id, t]));
  const agentById = new Map(agents.map((a) => [a.id, a]));
  return notices.filter((n) => {
    switch (n.kind) {
      case "escalated": {
        if (n.ticketId === null) return true;
        const t = ticketById.get(n.ticketId);
        return t !== undefined && t.escalated && t.state === "review";
      }
      case "flowReview": {
        if (n.ticketId === null) return true;
        const t = ticketById.get(n.ticketId);
        return t !== undefined && t.state === "review";
      }
      case "writeBackFailed": {
        if (n.ticketId === null) return true;
        const t = ticketById.get(n.ticketId);
        if (t === undefined || t.external === null) return false;
        const w = t.external.writeBack;
        return w.comment === "failed" || w.close === "failed";
      }
      case "permissionWaiting": {
        if (n.agentId === null) return true;
        const a = agentById.get(n.agentId);
        return a !== undefined && a.status.kind === "waitingPermission";
      }
      case "trustWaiting": {
        if (n.agentId === null) return true;
        const a = agentById.get(n.agentId);
        return a !== undefined && a.status.kind === "starting";
      }
      case "budgetReached":
      case "watchTripped":
      case "agentExited":
        return true;
    }
  });
}

/** Unread notices among the visible ones (the badge on the island and in the header). */
export function unreadVisible(
  notices: readonly Notice[],
  tickets: readonly TicketSummary[],
  agents: readonly AgentInfo[],
): number {
  return visibleNotices(notices, tickets, agents).filter((n) => !n.seen).length;
}

/** The newest unread notice with a ticket (the island's badge opens that ticket); null if none. */
export function newestWithTicket(notices: readonly Notice[]): Notice | null {
  return notices.find((n) => !n.seen && n.ticketId !== null) ?? null;
}
