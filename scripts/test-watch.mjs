// Truth tables for src/lib/watch.ts (step 6d): compiles it with the repo's TypeScript, runs it in
// node. Times are built with local Date parts, so the tables hold in any time zone.
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import ts from "typescript";

const src = readFileSync(new URL("../src/lib/watch.ts", import.meta.url), "utf8");
// Only type imports are allowed (the file must run on its own): a runtime import fails here.
assert.ok(!/^import\s+(?!type\b)/m.test(src), "watch.ts must only have type imports");
const out = ts.transpileModule(src, {
  compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 },
});
const file = join(mkdtempSync(join(tmpdir(), "watch-")), "watch.mjs");
writeFileSync(file, out.outputText);
const w = await import(pathToFileURL(file).href);

let n = 0;
const eq = (got, want, msg) => {
  assert.deepEqual(got, want, msg);
  n++;
};

const at = (y, mo, d, h, mi) => new Date(y, mo - 1, d, h, mi).getTime();
const NOW = at(2026, 10, 3, 9, 5);

const project = (o = {}) => ({
  id: "web", enabled: true, paused: false, active: true, reason: null, tripped: false, trippedAt: null,
  trippedReason: null, usedHour: 1, capHour: 3, usedDay: 2, capDay: 10, agents: 1, maxAgents: 2,
  nextFreeAt: null, quiet: "23-07", inQuiet: false, playbook: "bug", notes: [], ...o,
});
const view = (o = {}) => ({
  paused: false, active: 1, lastTickAt: NOW, global: { usedHour: 1, capHour: 6, usedDay: 2, capDay: 20 },
  projects: [project()], waiting: {}, ...o,
});
const waiting = (o = {}) => ({ reason: "budget", text: "venter på budget (næste: 14:05)", nextAt: at(2026, 10, 3, 14, 5), project: "web", ...o });
const notice = (o = {}) => ({
  id: "n1", kind: "escalated", at: NOW - 120_000, title: "Ticket eskaleret", text: "ab12cd34: «x» efter 3 runder",
  ticketId: "t1", agentId: null, project: "web", seen: false, ...o,
});
const ticket = (o = {}) => ({
  id: "t1", shortId: "ab12cd34", title: "x", state: "review", assigneeAgentId: null, queuePosition: null,
  skipReview: false, source: "user", issue: null, rejectionNote: null, summary: null, createdAt: 0, updatedAt: 0,
  historyLen: 1, reviewRound: 3, escalated: true, reviewerAgentId: null, reportCount: 0, project: "web",
  parentId: null, blockedBy: [], kind: null, playbookStartedAt: null, checks: null, git: null, external: null, ...o,
});
const wb = (o = {}) => ({ comment: "none", close: "none", commentUrl: null, commentedAt: null, closedAt: null, attempts: 0, lastError: null, lastBody: null, ...o });
const ext = (o = {}) => ({
  kind: "github", externalId: "github:o/r#7", repo: "o/r", number: 7, path: null, url: null, title: "t", labels: [],
  author: null, notes: [], inboxItemId: "i1", importedAt: 0, writeBack: wb(), inherited: false, ...o,
});
const agent = (o = {}) => ({
  id: "a1", sessionId: "s", name: "coder-01", cwd: "/p", status: { kind: "waitingPermission" }, detail: null, pid: 1,
  createdAt: 0, lastEventAt: 0, profileId: "coder", profileName: "Coder", roles: ["coder"], specialist: true,
  model: null, effort: null, modelObserved: false, openReviews: 0, seatKind: "work", currentTicketId: null,
  queueLength: 0, project: "web", ...o,
});

// hhmm / nextFreeText
eq(w.hhmm(at(2026, 10, 3, 7, 0)), "07:00");
eq(w.hhmm(at(2026, 10, 3, 14, 5)), "14:05");
eq(w.nextFreeText(null, NOW), "");
eq(w.nextFreeText(at(2026, 10, 3, 14, 5), NOW), "næste: 14:05");
eq(w.nextFreeText(at(2026, 10, 3, 23, 59), NOW), "næste: 23:59");
// earlier the same day (the tick has not cleared it yet): still a time, never "i går"
eq(w.nextFreeText(at(2026, 10, 3, 8, 0), NOW), "næste: 08:00");
eq(w.nextFreeText(at(2026, 10, 4, 0, 0), NOW), "i morgen 00:00");
eq(w.nextFreeText(at(2026, 10, 4, 7, 0), NOW), "i morgen 07:00");
eq(w.nextFreeText(at(2026, 10, 5, 7, 0), NOW), "næste: 05.10 07:00");
// month end: "i morgen" is the next calendar day, not +24 h
eq(w.nextFreeText(at(2026, 11, 1, 7, 0), at(2026, 10, 31, 23, 50)), "i morgen 07:00");
// DST fall-back night (last Sunday of October): the calendar day after is still "i morgen"
eq(w.nextFreeText(at(2026, 10, 26, 7, 0), at(2026, 10, 25, 1, 30)), "i morgen 07:00");

// waitingBadge
{
  const b = w.waitingBadge(waiting(), NOW);
  eq([b.text, b.cls], ["venter på budget (næste: 14:05)", w.BADGE_WAIT]);
  eq(w.waitingBadge(waiting({ nextAt: at(2026, 10, 4, 7, 0), text: "venter på budget (næste: 07:00)" }), NOW).text, "venter på budget (i morgen 07:00)");
  // no next time (cap 0): the backend's text as it is
  eq(w.waitingBadge(waiting({ nextAt: null, text: "venter på budget (næste: --:--)" }), NOW).text, "venter på budget (næste: --:--)");
  eq(w.waitingBadge(waiting({ reason: "seat", text: "venter på plads", nextAt: null }), NOW).text, "venter på plads");
  eq(w.waitingBadge(waiting({ reason: "planner", text: "venter på planlægger (stabsplads)", nextAt: null }), NOW).text, "venter på planlægger (stabsplads)");
  eq(w.waitingBadge(waiting({ reason: "duplicate", text: "mulig dublet, start manuelt", nextAt: null }), NOW).text, "mulig dublet, start manuelt");
  eq(w.waitingBadge(waiting({ reason: "playbook", text: "ingen playbook valgt for vagten", nextAt: null }), NOW).text, "ingen playbook valgt for vagten");
  eq(w.waitingBadge(waiting({ reason: "playbook", text: "playbook «x» findes ikke i workspace", nextAt: null }), NOW).text, "playbook «x» findes ikke i workspace");
  const f = w.waitingBadge(waiting({ reason: "failed", text: "vagt: start fejlede: gh: ingen forbindelse", nextAt: null }), NOW);
  eq([f.text, f.title, f.cls], ["vagt: start fejlede", "vagt: start fejlede: gh: ingen forbindelse", w.BADGE_FAILED]);
  // review6d W3: the source is down — amber, the source's text in the title
  const srcText = "vagt: venter på kilden: gh er ikke logget ind — kør gh auth login i en terminal (prøves igen efter Opdatér i indbakken)";
  const s = w.waitingBadge(waiting({ reason: "failed", text: srcText, nextAt: null }), NOW);
  eq([s.text, s.title, s.cls], ["vagt: venter på kilden", srcText, w.BADGE_WAIT]);
  for (const r of ["budget", "seat", "planner", "duplicate", "playbook"]) {
    eq(w.waitingBadge(waiting({ reason: r }), NOW).cls, w.BADGE_WAIT, r);
    eq(w.waitingBadge(waiting({ reason: r }), NOW).title.length > 0, true, r);
  }
}

// watchChipText / showStopWatch / showResumeWatch / unreadText
eq(w.watchChipText(null), null);
eq(w.watchChipText(view()), "Vagt: 1 projekt");
eq(w.watchChipText(view({ active: 2, projects: [project(), project({ id: "api" })] })), "Vagt: 2 projekter");
eq(w.watchChipText(view({ active: 0, paused: true, projects: [project({ active: false, reason: "vagten er sat på pause" })] })), "Vagt: pause");
// paused but no project allows the watch: nothing to show
eq(w.watchChipText(view({ active: 0, paused: true, projects: [project({ enabled: false, active: false, reason: "watch.enabled mangler i project.json" })] })), null);
eq(w.watchChipText(view({ active: 0, projects: [] })), null);
eq(w.showStopWatch(null), false);
eq(w.showStopWatch(view()), true);
eq(w.showStopWatch(view({ active: 0 })), false);
eq(w.showStopWatch(view({ paused: true, active: 0 })), false);
eq(w.showResumeWatch(view()), false);
eq(w.showResumeWatch(view({ paused: true, active: 0 })), true);
eq(w.showResumeWatch(view({ paused: true, active: 0, projects: [project({ enabled: false })] })), false);
eq(w.unreadText(0), "");
eq(w.unreadText(1), "1 besked");
eq(w.unreadText(2), "2 beskeder");

// budgetText / quietEndText / watchStatusText / canHoldWatch / projectWatch
eq(w.budgetText(project({ usedHour: 3, capHour: 3, usedDay: 7, capDay: 10, agents: 1, maxAgents: 2 })), "3/3 i timen · 7/10 i dag · 1/2 agenter");
eq(w.quietEndText("23-07"), "07:00");
eq(w.quietEndText("7-7"), "07:00");
eq(w.quietEndText("0-24"), "00:00");
eq(w.quietEndText("x"), null);
eq(w.quietEndText(null), null);
eq(w.watchStatusText(project()), "aktiv");
eq(w.watchStatusText(project({ inQuiet: true })), "stille timer til 07:00");
eq(w.watchStatusText(project({ inQuiet: true, quiet: null })), "stille timer");
// inactive: the backend's reason wins (first failing condition)
eq(w.watchStatusText(project({ active: false, reason: "watch.enabled mangler i project.json", enabled: false })), "watch.enabled mangler i project.json");
eq(w.watchStatusText(project({ active: false, reason: "vagt er sat på pause for projektet", paused: true })), "vagt er sat på pause for projektet");
eq(w.watchStatusText(project({ active: false, reason: "stoppet efter 3 fejl — tryk Genstart vagt", tripped: true })), "stoppet efter 3 fejl — tryk Genstart vagt");
// fallbacks without a reason
eq(w.watchStatusText(project({ active: false, tripped: true })), "stoppet efter 3 fejl");
eq(w.watchStatusText(project({ active: false, paused: true })), "på pause");
eq(w.watchStatusText(project({ active: false, enabled: false })), "watch.enabled mangler i project.json");
eq(w.watchStatusText(project({ active: false })), "inaktiv");
eq(w.canHoldWatch(project()), true);
eq(w.canHoldWatch(project({ enabled: false })), false);
eq(w.projectWatch(null, "web"), null);
eq(w.projectWatch(view(), "WEB")?.id, "web");
eq(w.projectWatch(view(), "api"), null);

// watchLineText / watchCopyLines / watchWarnings
eq(w.watchLineText(project(), NOW), "Vagt: aktiv · 1/3 i timen · 2/10 i dag · 1/2 agenter · playbook: bug");
eq(
  w.watchLineText(project({ nextFreeAt: at(2026, 10, 3, 14, 5), playbook: null }), NOW),
  "Vagt: aktiv · 1/3 i timen · 2/10 i dag · 1/2 agenter · næste: 14:05 · ingen playbook",
);
eq(w.watchCopyLines(null, NOW), []);
eq(w.watchCopyLines(view(), NOW), [
  "watch: til · 1 aktive · 1/6 i timen · 2/20 i dag",
  "watch.web: Vagt: aktiv · 1/3 i timen · 2/10 i dag · 1/2 agenter · playbook: bug",
]);
eq(w.watchCopyLines(view({ paused: true, active: 0, projects: [] }), NOW)[0].startsWith("watch: på pause"), true);
eq(w.watchWarnings(null, "off"), []);
eq(w.watchWarnings(view(), "worktree"), []);
eq(w.watchWarnings(view(), "branch"), []);
eq(w.watchWarnings(view(), undefined), []);
eq(w.watchWarnings(view(), "off"), ["Vagt-projektet «web» kører uden git: worktree — agenten og du deler samme mappe"]);
// only active projects warn
eq(w.watchWarnings(view({ projects: [project({ active: false, reason: "vagten er sat på pause" }), project({ id: "api" })] }), "off"), [
  "Vagt-projektet «api» kører uden git: worktree — agenten og du deler samme mappe",
]);

// notice kinds and labels
eq(w.NOTICE_KINDS.length, 8);
eq(Object.keys(w.NOTICE_KIND_LABEL).sort(), [...w.NOTICE_KINDS].sort());
eq(w.NOTICE_KIND_LABEL, {
  escalated: "eskaleret",
  flowReview: "forløb til godkendelse",
  permissionWaiting: "tilladelse venter",
  trustWaiting: "agent venter i terminalen",
  writeBackFailed: "tilbagemelding fejlede",
  budgetReached: "budget nået",
  watchTripped: "vagt stoppet",
  agentExited: "agent afsluttet",
});
eq(w.isNoticeKind("budgetReached"), true);
eq(w.isNoticeKind("toast"), false);

// visibleNotices
{
  const vis = (ns, ts = [ticket()], as = [agent()]) => w.visibleNotices(ns, ts, as).map((x) => x.id);
  // escalated: kept while the ticket is escalated and in review
  eq(vis([notice()]), ["n1"]);
  eq(vis([notice()], [ticket({ state: "done" })]), []);
  eq(vis([notice()], [ticket({ escalated: false })]), []);
  eq(vis([notice()], []), []);
  eq(vis([notice({ ticketId: null })], []), ["n1"]);
  // flowReview: kept while in review (escalated or not)
  eq(vis([notice({ kind: "flowReview" })], [ticket({ escalated: false })]), ["n1"]);
  eq(vis([notice({ kind: "flowReview" })], [ticket({ state: "backlog" })]), []);
  // writeBackFailed: kept while a step is failed
  eq(vis([notice({ kind: "writeBackFailed" })], [ticket({ state: "done", external: ext({ writeBack: wb({ comment: "failed" }) }) })]), ["n1"]);
  eq(vis([notice({ kind: "writeBackFailed" })], [ticket({ state: "done", external: ext({ writeBack: wb({ comment: "done", close: "failed" }) }) })]), ["n1"]);
  eq(vis([notice({ kind: "writeBackFailed" })], [ticket({ state: "done", external: ext({ writeBack: wb({ comment: "done", close: "done" }) }) })]), []);
  eq(vis([notice({ kind: "writeBackFailed" })], [ticket({ state: "done" })]), []);
  // permissionWaiting / trustWaiting follow the agent's status
  eq(vis([notice({ kind: "permissionWaiting", ticketId: null, agentId: "a1" })]), ["n1"]);
  eq(vis([notice({ kind: "permissionWaiting", ticketId: null, agentId: "a1" })], [], [agent({ status: { kind: "idle" } })]), []);
  eq(vis([notice({ kind: "permissionWaiting", ticketId: null, agentId: "a1" })], [], []), []);
  eq(vis([notice({ kind: "trustWaiting", ticketId: null, agentId: "a1" })], [], [agent({ status: { kind: "starting" } })]), ["n1"]);
  eq(vis([notice({ kind: "trustWaiting", ticketId: null, agentId: "a1" })], [], [agent({ status: { kind: "idle" } })]), []);
  // the rest is kept whatever the lists say; order is kept (newest first)
  const rest = [
    notice({ id: "b", kind: "budgetReached", ticketId: null, project: "web" }),
    notice({ id: "t", kind: "watchTripped", ticketId: null }),
    notice({ id: "e", kind: "agentExited", ticketId: null, agentId: "gone" }),
  ];
  eq(vis(rest, [], []), ["b", "t", "e"]);
  // never mutates the input
  const input = [notice({ id: "x", kind: "budgetReached", ticketId: null }), notice({ id: "y" })];
  w.visibleNotices(input, [], []);
  eq(input.map((x) => x.id), ["x", "y"]);
}

// unreadVisible / newestWithTicket
eq(w.unreadVisible([notice(), notice({ id: "n2", seen: true })], [ticket()], []), 1);
eq(w.unreadVisible([notice()], [], []), 0);
eq(w.unreadVisible([notice({ kind: "budgetReached", ticketId: null }), notice({ id: "n2", kind: "watchTripped", ticketId: null })], [], []), 2);
eq(w.newestWithTicket([]), null);
eq(w.newestWithTicket([notice({ id: "a", ticketId: null }), notice({ id: "b", ticketId: "t2" }), notice({ id: "c", ticketId: "t3" })])?.id, "b");
eq(w.newestWithTicket([notice({ id: "a", seen: true })]), null);
eq(w.newestWithTicket([notice({ id: "a", seen: true }), notice({ id: "b", ticketId: "t2" })])?.id, "b");

console.log(`watch.ts: ${n} cases ok`);
