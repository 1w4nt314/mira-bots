// Truth tables for src/lib/tickets.ts (pure helpers): compiles it with the repo's TypeScript and
// checks the tables from its doc comments in node, with the plan5 review/coordination helpers.
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import ts from "typescript";

const src = readFileSync(new URL("../src/lib/tickets.ts", import.meta.url), "utf8");
const out = ts.transpileModule(src, {
  compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 },
});
const file = join(mkdtempSync(join(tmpdir(), "tickets-")), "tickets.mjs");
writeFileSync(file, out.outputText);
const k = await import(pathToFileURL(file).href);

let n = 0;
const eq = (got, want, msg) => {
  assert.deepEqual(got, want, msg);
  n++;
};

const agent = (id, over = {}) => ({
  id,
  name: id,
  roles: ["reviewer"],
  status: { kind: "idle" },
  seatKind: "staff",
  openReviews: 0,
  createdAt: 0,
  currentTicketId: null,
  ...over,
});
const ticket = (over = {}) => ({ id: "t", state: "review", assigneeAgentId: "S", reviewerAgentId: null, reviewRound: 0, ...over });

// shortId, moveUp, dropTarget, parseWorkplaceTab
eq(k.shortId("3F2A9C10-77aa-4b1e-9d2e-000000000000"), "3f2a9c10", "shortId");
eq(k.moveUp(["a", "b", "c"], 2), ["a", "c", "b"], "moveUp 2");
eq(k.moveUp(["a", "b", "c"], 0), null, "moveUp 0");
eq(k.dropTarget("empty:staff:0"), { kind: "empty", seatKind: "staff", index: 0 }, "dropTarget");
eq(k.dropTarget("agent:"), null, "dropTarget empty agent");
// Step 6c B5: inbox drag ids never parse as tickets or seats (and the other way round, test-inbox).
eq(k.draggedTicketId("inbox:x"), null, "draggedTicketId inbox");
eq(k.draggedTicketId("ticket:x"), "x", "draggedTicketId ticket");
eq(k.dropTarget("inbox:x"), null, "dropTarget inbox");
for (const [tab, want] of [
  ["tickets", "tickets"],
  ["permissions", "permissions"],
  ["diagnostics", "diagnostics"],
  ["agents", "agents"],
  ["Tickets", null],
  [null, null],
])
  eq(k.parseWorkplaceTab(tab), want, `parseWorkplaceTab ${tab}`);

// reviewRoundText
for (const [r, want] of [
  [0, "Runde 1 af 3"],
  [2, "Runde 3 af 3"],
  [3, "Runde 3 af 3"],
  [7, "Runde 3 af 3"],
])
  eq(k.reviewRoundText({ reviewRound: r }), want, `reviewRoundText ${r}`);
// Step 6b: the workspace's maxReviewRounds (appInfo.rules.maxReviewRounds).
for (const [r, max, want] of [
  [0, 2, "Runde 1 af 2"],
  [1, 2, "Runde 2 af 2"],
  [5, 2, "Runde 2 af 2"],
  [0, 1, "Runde 1 af 1"],
  [3, 10, "Runde 4 af 10"],
])
  eq(k.reviewRoundText({ reviewRound: r }, max), want, `reviewRoundText ${r} af ${max}`);
eq(k.MAX_REVIEW_ROUNDS, undefined, "MAX_REVIEW_ROUNDS is gone (the workspace decides)");

// reviewerCandidates
const agents = [
  agent("R1", { openReviews: 2, createdAt: 1 }),
  agent("R2", { openReviews: 0, createdAt: 5 }),
  agent("R3", { openReviews: 0, createdAt: 2 }),
  agent("RX", { status: { kind: "exited", code: 0 } }),
  agent("S"),
  agent("CUR"),
  agent("C", { roles: ["coder"] }),
];
eq(
  k.reviewerCandidates(agents, ticket({ reviewerAgentId: "CUR" })).map((a) => a.id),
  ["R3", "R2", "R1"],
  "reviewerCandidates order and filters",
);
eq(k.reviewerCandidates([], ticket()), [], "reviewerCandidates empty");

// reviewsFor
const tickets = [
  ticket({ id: "a", reviewerAgentId: "R1" }),
  ticket({ id: "b", reviewerAgentId: "R2" }),
  ticket({ id: "c", reviewerAgentId: "R1", state: "done" }),
  ticket({ id: "d", reviewerAgentId: "R1" }),
];
eq(k.reviewsFor(tickets, "R1").map((t) => t.id), ["a", "d"], "reviewsFor");

// isCoordinationTask
eq(k.isCoordinationTask({ seatKind: "staff", roles: ["coordinator"] }), true, "staff");
eq(k.isCoordinationTask({ seatKind: "staff", roles: ["reviewer", "coder"] }), true, "staff + work role");
eq(k.isCoordinationTask({ seatKind: "work", roles: ["coder"] }), false, "work");
eq(k.isCoordinationTask({ seatKind: "work", roles: ["reviewer", "debugger"] }), false, "work + work role");
eq(k.isCoordinationTask({ seatKind: "work", roles: ["reviewer"] }), true, "work, no work role");
eq(k.isCoordinationTask({ seatKind: "work", roles: [] }), true, "work, no roles");
eq(k.isCoordinationTask(null), false, "no agent");

// switchBlocked
for (const [status, cur, want] of [
  ["idle", null, null],
  ["idle", "t1", k.AGENT_BUSY_TEXT],
  ["thinking", null, k.AGENT_BUSY_TEXT],
  ["starting", null, k.AGENT_BUSY_TEXT],
  ["exited", null, "Agenten kører ikke"],
])
  eq(k.switchBlocked({ status: { kind: status }, currentTicketId: cur }), want, `switchBlocked ${status} ${cur}`);

// canHandOver (step 5c): "Tildel…" on a ticket in progress.
for (const [state, assignee, want] of [
  ["inProgress", "A", true],
  ["inProgress", null, false],
  ["assigned", "A", false],
  ["review", "A", false],
  ["done", "A", false],
  ["done", null, false],
  ["backlog", null, false],
  ["rejected", null, false],
])
  eq(k.canHandOver(ticket({ state, assigneeAgentId: assignee })), want, `canHandOver ${state} ${assignee}`);

// --- step 6a: waiting, parents, children, blockers ------------------------------------------------
const tk = (id, over = {}) => ({
  id,
  shortId: id.slice(0, 8),
  title: `Titel ${id}`,
  state: "backlog",
  assigneeAgentId: null,
  queuePosition: null,
  createdAt: 0,
  updatedAt: 0,
  parentId: null,
  blockedBy: [],
  ...over,
});

eq(k.STATE_LABEL.waiting, "Venter", "STATE_LABEL waiting");
eq(k.STATE_BADGE_CLASS.waiting, "bg-teal-500/15 text-teal-700 dark:text-teal-300", "badge waiting");
eq(k.WAITING_HINT, "Venter på del-tickets — vækkes automatisk når de er godkendt", "WAITING_HINT");
eq(k.WAITING_TITLE, "Venter på del-tickets", "WAITING_TITLE");
eq(k.BLOCKED_HINT, "Leveres når blokeringerne er Done", "BLOCKED_HINT");
eq(k.SUBMIT_PARENT_HINT, "Sender til review selv om del-tickets er åbne", "SUBMIT_PARENT_HINT");

// ticketsByState has a waiting list
eq(
  k.ticketsByState([tk("a", { state: "waiting" }), tk("b")]).waiting.map((t) => t.id),
  ["a"],
  "ticketsByState waiting",
);

// groupTickets: waiting lands in byAgent[assignee].waiting, oldest updatedAt first, nowhere else
const g = k.groupTickets([
  tk("w2", { state: "waiting", assigneeAgentId: "A", updatedAt: 20 }),
  tk("w1", { state: "waiting", assigneeAgentId: "A", updatedAt: 10 }),
  tk("cur", { state: "inProgress", assigneeAgentId: "A" }),
  tk("wb", { state: "waiting", assigneeAgentId: "B" }),
  tk("orphan", { state: "waiting", assigneeAgentId: null }),
]);
eq(g.byAgent.get("A").waiting.map((t) => t.id), ["w1", "w2"], "groupTickets waiting order");
eq(g.byAgent.get("A").current.id, "cur", "groupTickets current unaffected by waiting");
eq(g.byAgent.get("B").waiting.map((t) => t.id), ["wb"], "groupTickets waiting per agent");
eq([g.backlog, g.review, g.done].map((l) => l.length), [0, 0, 0], "waiting is in no section list");
eq(g.byAgent.get("A").queue, [], "waiting is not in the queue");

// canDrag/canAssign/canHandOver/canDelete: waiting is untouchable
const wt = tk("w", { state: "waiting", assigneeAgentId: "A" });
eq(k.canDrag(wt), false, "canDrag waiting");
eq(k.canAssign(wt, agent("A")), false, "canAssign waiting");
eq(k.canHandOver(wt), false, "canHandOver waiting");
eq(k.canDelete(wt), false, "canDelete waiting");

// parentOf
const P = tk("p");
const all1 = [P, tk("c1", { parentId: "p", createdAt: 5 }), tk("c0", { parentId: "p", createdAt: 1 }), tk("x", { parentId: "q" })];
eq(k.parentOf(all1[1], all1)?.id, "p", "parentOf found");
eq(k.parentOf(P, all1), null, "parentOf none");
eq(k.parentOf(all1[3], all1), null, "parentOf missing parent");

// childrenOf: createdAt ascending, ignores foreign children
eq(k.childrenOf("p", all1).map((t) => t.id), ["c0", "c1"], "childrenOf order and filter");
eq(k.childrenOf("zzz", all1), [], "childrenOf none");
eq(k.childrenOf("p", []), [], "childrenOf empty list");

// progressOf: anything but done is open; a deleted child is simply not in the list
eq(k.progressOf("p", all1), { done: 0, total: 2 }, "progressOf all open");
eq(
  k.progressOf("p", [P, tk("a", { parentId: "p", state: "done" }), tk("b", { parentId: "p", state: "review" }), tk("c", { parentId: "p", state: "backlog" }), tk("d", { parentId: "q", state: "done" })]),
  { done: 1, total: 3 },
  "progressOf mixed",
);
eq(k.progressOf("p", [P, tk("a", { parentId: "p", state: "done" })]), { done: 1, total: 1 }, "progressOf all done");
eq(k.progressOf("p", [P]), { done: 0, total: 0 }, "progressOf no children");
eq(k.progressOf("p", [P, tk("a", { parentId: null, state: "done" })]), { done: 0, total: 0 }, "progressOf parent deleted -> parentId null");

// blockersOf / isBlocked
const openB = tk("b1", { state: "review" });
const doneB = tk("b2", { state: "done" });
const bt = tk("t", { blockedBy: ["b1", "b2", "gone"] });
const allB = [openB, doneB, bt];
eq(k.blockersOf(bt, allB).map((t) => t.id), ["b1"], "blockersOf skips done and unknown");
eq(k.isBlocked(bt, allB), true, "isBlocked open blocker");
eq(k.isBlocked(tk("t2", { blockedBy: ["b2"] }), allB), false, "isBlocked done blocker");
eq(k.isBlocked(tk("t3", { blockedBy: ["gone"] }), allB), false, "isBlocked unknown id");
eq(k.isBlocked(tk("t4"), allB), false, "isBlocked no blockers");
eq(k.blockersOf(tk("t5", { blockedBy: ["b2", "b1"] }), allB).map((t) => t.shortId), ["b1"], "blockersOf short ids");
eq(k.blockersOf(tk("t6", { blockedBy: ["b1", "b3"] }), [openB, tk("b3", { state: "backlog" })]).map((t) => t.id), ["b1", "b3"], "blockersOf keeps blockedBy order");

// isDeliverable = canDrag && !isBlocked
eq(k.isDeliverable(tk("d1"), allB), true, "isDeliverable free backlog");
eq(k.isDeliverable(bt, allB), false, "isDeliverable blocked");
eq(k.isDeliverable(tk("d2", { state: "assigned", assigneeAgentId: "A" }), allB), false, "isDeliverable assigned");
eq(k.isDeliverable(tk("d3", { state: "rejected" }), allB), true, "isDeliverable rejected without assignee");

// Review 6a W1/W2/N3: canRequestSubmission (waiting parent), canReturnWaiting, waitingCount,
// movePendingText
const live = (kind) => agent("A", { status: { kind } });
for (const [state, a, want] of [
  ["inProgress", live("idle"), true],
  ["waiting", live("idle"), true],
  ["inProgress", live("thinking"), false],
  ["waiting", live("thinking"), false],
  ["inProgress", live("exited"), false],
  ["inProgress", null, false],
  ["review", live("idle"), false],
  ["assigned", live("idle"), false],
])
  eq(k.canRequestSubmission(ticket({ state }), a), want, `canRequestSubmission ${state} ${a?.status.kind}`);
for (const [state, assignee, want] of [
  ["waiting", "A", true],
  ["waiting", null, false],
  ["inProgress", "A", false],
  ["assigned", "A", false],
  ["backlog", null, false],
])
  eq(k.canReturnWaiting(ticket({ state, assigneeAgentId: assignee })), want, `canReturnWaiting ${state} ${assignee}`);
eq(
  k.waitingCount(
    [
      ticket({ id: "w1", state: "waiting", assigneeAgentId: "A" }),
      ticket({ id: "w2", state: "waiting", assigneeAgentId: "B" }),
      ticket({ id: "q1", state: "assigned", assigneeAgentId: "A" }),
      ticket({ id: "w3", state: "waiting", assigneeAgentId: "A" }),
    ],
    "A",
  ),
  2,
  "waitingCount counts only the agent's waiting tickets",
);
for (const [q, w, p, want] of [
  [0, 0, "p", null],
  [1, 0, "p", "1 ticket i kø til «p» lægges tilbage i Backlog"],
  [2, 0, "p", "2 tickets i kø til «p» lægges tilbage i Backlog"],
  [0, 1, "p", "1 ventende ticket til «p» lægges tilbage i Backlog"],
  [0, 2, "p", "2 ventende tickets til «p» lægges tilbage i Backlog"],
  [2, 1, "p", "2 tickets i kø og 1 ventende til «p» lægges tilbage i Backlog"],
  [1, 2, null, "1 ticket i kø og 2 ventende lægges tilbage i Backlog"],
])
  eq(k.movePendingText(q, w, p), want, `movePendingText ${q} ${w} ${p}`);
eq(k.WAKE_UNCONFIRMED_TEXT, "Vækning ikke bekræftet, se terminalen", "WAKE_UNCONFIRMED_TEXT mirrors Rust");

// --- step 6b: kinds, playbooks, checks, git, worktree paths -------------------------------------
const t6 = (over = {}) => ({
  id: "t",
  state: "backlog",
  assigneeAgentId: null,
  parentId: null,
  kind: null,
  playbookStartedAt: null,
  checks: null,
  git: null,
  ...over,
});

// kindLabel
for (const [kind, want] of [
  [null, "Opgave"],
  ["feature", "Feature"],
  ["bug", "Bug"],
  ["docs", "docs"],
  ["Docs", "Docs"],
])
  eq(k.kindLabel(kind), want, `kindLabel ${kind}`);

// KIND_OPTIONS
const values = (kinds) => k.KIND_OPTIONS(kinds).map((o) => o.value);
eq(values([]), [null, "feature", "bug"], "KIND_OPTIONS without playbooks keeps the built-ins");
eq(values(["bug", "feature"]), [null, "feature", "bug"], "KIND_OPTIONS built-ins only");
eq(values(["feature", "docs", "api", "docs"]), [null, "feature", "bug", "api", "docs"], "KIND_OPTIONS extras sorted and deduped");
eq(values(["task", ""]), [null, "feature", "bug"], "KIND_OPTIONS ignores task and blank");
eq(
  k.KIND_OPTIONS(["docs"]).map((o) => o.label),
  ["Opgave", "Feature", "Bug", "docs"],
  "KIND_OPTIONS labels",
);

// canStartPlaybook: backlog + kind with a playbook + not started + no children
const kinds = ["bug", "feature"];
const start = (over, all = []) => k.canStartPlaybook(t6(over), all, kinds);
eq(start({ kind: "feature" }), true, "canStartPlaybook feature in backlog");
eq(start({ kind: "bug" }), true, "canStartPlaybook bug in backlog");
eq(start({ kind: null }), false, "canStartPlaybook needs a kind");
eq(start({ kind: "docs" }), false, "canStartPlaybook kind without playbook");
eq(start({ kind: "feature", playbookStartedAt: 1 }), false, "canStartPlaybook already started");
eq(start({ kind: "feature" }, [t6({ id: "c", parentId: "t" })]), false, "canStartPlaybook has children");
eq(start({ kind: "feature" }, [t6({ id: "c", parentId: "other" })]), true, "canStartPlaybook other family's child");
for (const state of ["assigned", "inProgress", "waiting", "review", "done", "rejected"])
  eq(start({ kind: "feature", state }), false, `canStartPlaybook not in ${state}`);
eq(k.canStartPlaybook(t6({ kind: "feature" }), [], []), false, "canStartPlaybook before appInfo (no kinds)");

// isFlowParent
eq(k.isFlowParent(t6({ playbookStartedAt: 1 })), true, "isFlowParent started without owner");
eq(k.isFlowParent(t6({ playbookStartedAt: 1, assigneeAgentId: "A" })), false, "isFlowParent with owner");
eq(k.isFlowParent(t6()), false, "isFlowParent not started");

// checksBadge / checksLineText
const chk = (state, failed = null) => t6({ checks: { state, failed, round: 0, startedAt: 1 } });
eq(k.checksBadge(t6()), null, "checksBadge null");
eq(k.checksBadge(chk("skipped")), null, "checksBadge skipped");
eq(k.checksBadge(chk("pending")).text, "Tjek: kører", "checksBadge pending");
eq(k.checksBadge(chk("passed")).text, "Tjek: OK", "checksBadge passed");
eq(k.checksBadge(chk("failed", "tests")).text, "Tjek: FEJL (tests)", "checksBadge failed with name");
eq(k.checksBadge(chk("failed", "tests")).title, "tests", "checksBadge failed title is the name");
eq(k.checksBadge(chk("failed")).text, "Tjek: FEJL", "checksBadge failed without name");
eq(k.checksBadge(chk("failed")).title, "Projekt-tjek", "checksBadge failed title fallback");
eq(k.checksBadge(chk("passed")).title, "Projekt-tjek", "checksBadge passed title");
eq(
  new Set(["pending", "passed", "failed"].map((s) => k.checksBadge(chk(s)).cls)).size,
  3,
  "checksBadge: one class per state",
);
eq(k.checksLineText(t6()), null, "checksLineText null");
eq(k.checksLineText(chk("skipped")), null, "checksLineText skipped");
eq(k.checksLineText(chk("pending")), "Tjek: kører…", "checksLineText pending");
eq(k.checksLineText(chk("passed")), "Tjek: OK", "checksLineText passed");
eq(k.checksLineText(chk("failed", "tests")), "Tjek: FEJL (tests) · se rapport", "checksLineText failed");
eq(k.checksLineText(chk("failed")), "Tjek: FEJL · se rapport", "checksLineText failed without name");

// reviewerChoiceBlocked (review6b W7)
eq(k.CHECKS_RUNNING_REVIEWER, "Tjek kører; vælg reviewer når det er færdigt", "CHECKS_RUNNING_REVIEWER text");
eq(k.reviewerChoiceBlocked(chk("pending"), true), k.CHECKS_RUNNING_REVIEWER, "reviewerChoiceBlocked pending with gate");
eq(k.reviewerChoiceBlocked(chk("pending"), false), null, "reviewerChoiceBlocked pending without gate");
for (const s of ["passed", "failed", "skipped"])
  eq(k.reviewerChoiceBlocked(chk(s), true), null, `reviewerChoiceBlocked ${s}`);
eq(k.reviewerChoiceBlocked(t6(), true), null, "reviewerChoiceBlocked no checks");

// gitBadge / gitLineText
const git = (worktree) => ({ mode: "worktree", branch: "ticket/ab12cd34", base: "main", repo: "/r", worktree });
eq(k.gitBadge(t6()), null, "gitBadge null");
eq(k.gitBadge(t6({ git: git("/r/.mira-bots/wt/ab12cd34") })), { text: "⎇ ticket/ab12cd34", title: "/r/.mira-bots/wt/ab12cd34" }, "gitBadge worktree title");
eq(k.gitBadge(t6({ git: git(null) })), { text: "⎇ ticket/ab12cd34", title: "/r" }, "gitBadge repo title");
eq(k.gitLineText(git(null)), "Branch ticket/ab12cd34 fra main", "gitLineText");

// playbookStartedText
eq(k.playbookStartedText(2, []), "Forløb startet: 2 del-tickets", "playbookStartedText");
eq(k.playbookStartedText(1, []), "Forløb startet: 1 del-ticket", "playbookStartedText singular");
eq(
  k.playbookStartedText(2, ["trin 2: a", "trin 1: b"]),
  "Forløb startet: 2 del-tickets · trin 2: a · trin 1: b",
  "playbookStartedText with notes",
);

// shortCwd
eq(k.shortCwd("C:\\p\\app\\.mira-bots\\wt\\ab12cd34"), "…/.mira-bots/wt/ab12cd34", "shortCwd windows");
eq(k.shortCwd("C:\\p\\app\\.mira-bots\\wt\\ab12cd34\\"), "…/.mira-bots/wt/ab12cd34", "shortCwd windows trailing slash");
eq(k.shortCwd("/home/u/p/app/.mira-bots/wt/ab12cd34"), "…/.mira-bots/wt/ab12cd34", "shortCwd unix");
eq(k.shortCwd("/home/u/p/app/.mira-bots/wt/ab12cd34/"), "…/.mira-bots/wt/ab12cd34", "shortCwd unix trailing slash");
eq(k.shortCwd("/home/u/p/app/.mira-bots/wt/ab12cd34/src/x"), "…/.mira-bots/wt/ab12cd34/src/x", "shortCwd subfolder kept");
eq(k.shortCwd("C:\\p\\.mira-bots\\wt\\ab12cd34\\src\\x"), "…/.mira-bots/wt/ab12cd34/src/x", "shortCwd windows subfolder");
eq(k.shortCwd("/home/u/p/app"), "/home/u/p/app", "shortCwd plain project");
eq(k.shortCwd("/home/u/.mira-bots/wt/not-a-short-id"), "/home/u/.mira-bots/wt/not-a-short-id", "shortCwd bad short id");
eq(k.shortCwd("/home/u/.mira-bots/wt/ab12cd34ef"), "/home/u/.mira-bots/wt/ab12cd34ef", "shortCwd longer id");
eq(k.shortCwd("/home/u/.mira-bots/tickets"), "/home/u/.mira-bots/tickets", "shortCwd other .mira-bots folder");
eq(k.shortCwd(""), "", "shortCwd empty");

// reviewRoundText with the workspace maximum (unchanged from 6a; guards the card text)
eq(k.reviewRoundText({ reviewRound: 0 }, 5), "Runde 1 af 5", "reviewRoundText workspace max");
eq(k.reviewRoundText({ reviewRound: 9 }, 5), "Runde 5 af 5", "reviewRoundText capped");

// --- tidslinje (step 6d B6): buildTimeline, timelineText, timelineCount, relativeText ---------
const T0 = 1_700_000_000_000;
const hist = (at, from, to, by = "user", note = null) => ({ at, from, to, by, note });
const noteAt = (at, note, by = "system", state = "backlog") => hist(at, state, state, by, note);
const report = (id, title, kind, createdAt) => ({
  id,
  title,
  author: { kind, agentId: kind === "agent" ? "A" : null },
  createdAt,
  path: `reports/${id}-x.md`,
  size: 10,
});
const full = (over = {}) => ({
  id: "ab12cd34-0000-4000-8000-000000000000",
  title: "Titel",
  state: "backlog",
  assigneeAgentId: null,
  queuePosition: null,
  skipReview: false,
  source: "user",
  issue: null,
  rejectionNote: null,
  summary: null,
  createdAt: T0,
  updatedAt: T0,
  reviewRound: 0,
  escalated: false,
  reviewerAgentId: null,
  project: null,
  parentId: null,
  blockedBy: [],
  kind: null,
  playbookStartedAt: null,
  checks: null,
  git: null,
  external: null,
  body: "HEMMELIG brødtekst",
  history: [hist(T0, null, "backlog")],
  reports: [],
  ...over,
});
const kindsOf = (entries) => entries.map((e) => e.kind);
const textsOf = (entries) => entries.map((e) => e.text);

// Tom ticket: kun oprettelsen.
eq(k.buildTimeline(full()), [{ at: T0, kind: "created", text: "oprettet i Backlog", by: "dig" }], "timeline empty ticket");
eq(k.buildTimeline(full({ history: [], reports: [] })), [], "timeline no history at all");
// Oprettelse fra indbakken beholder noten.
eq(
  k.buildTimeline(full({ history: [hist(T0, null, "backlog", "user", "startet fra indbakken: GitHub issue #7 i o/r")] }))[0].text,
  "oprettet i Backlog — startet fra indbakken: GitHub issue #7 i o/r",
  "timeline created with note",
);

// Hel playbook-forælder med børn: historik, vagt-note, forløb, rapport, review.
const parent = full({
  kind: "feature",
  playbookStartedAt: T0 + 2000,
  history: [
    hist(T0, null, "backlog"),
    noteAt(T0 + 1000, "startet af vagten (forløb «feature»)"),
    hist(T0 + 3000, "backlog", "waiting", "system", "venter på del-tickets (2)"),
    noteAt(T0 + 9000, "vækket: del-ticket godkendt", "system", "waiting"),
    hist(T0 + 10_000, "waiting", "review", "system", "forløb afsluttet: alle del-tickets godkendt"),
  ],
  reports: [report("01", "Plan", "agent", T0 + 5000)],
});
const children = [{ id: "c1", parentId: parent.id }, { id: "c2", parentId: parent.id }];
const pt = k.buildTimeline(parent, { children });
eq(kindsOf(pt), ["created", "watch", "playbook", "state", "report", "note", "state"], "timeline parent kinds");
eq(
  textsOf(pt),
  [
    "oprettet i Backlog",
    "startet af vagten (forløb «feature»)",
    "forløb startet (Feature), 2 del-tickets",
    "Backlog → Venter — venter på del-tickets (2)",
    "rapport 01: Plan",
    "vækket: del-ticket godkendt",
    "Venter → Review — forløb afsluttet: alle del-tickets godkendt",
  ],
  "timeline parent texts",
);
eq(pt.map((e) => e.by), ["dig", "systemet", "systemet", "systemet", "agenten", "systemet", "systemet"], "timeline parent by");
eq(pt.map((e) => e.at), [T0, T0 + 1000, T0 + 2000, T0 + 3000, T0 + 5000, T0 + 9000, T0 + 10_000], "timeline parent sorted");
// Uden children: tekst uden antal; ét barn: ental.
eq(k.buildTimeline(parent)[2].text, "forløb startet (Feature)", "timeline playbook without children");
eq(k.buildTimeline(parent, { children: [children[0]] })[2].text, "forløb startet (Feature), 1 del-ticket", "timeline playbook one child");
eq(k.buildTimeline(full({ kind: "bug", playbookStartedAt: T0 + 1 }), { children: [] })[1].text, "forløb startet (Bug), 0 del-tickets", "timeline playbook bug");
eq(k.buildTimeline(full({ kind: null, playbookStartedAt: T0 + 1 }))[1].text, "forløb startet (Opgave)", "timeline playbook plain kind");

// Afvist → afvist → godkendt: tre state-linjer med by, noterne bagved; eskalering som note.
const rr = full({
  history: [
    hist(T0, null, "backlog"),
    hist(T0 + 1, "inProgress", "review", "agent"),
    hist(T0 + 2, "review", "rejected", "agent", "mangler test"),
    hist(T0 + 3, "rejected", "review", "agent"),
    hist(T0 + 4, "review", "rejected", "user", "stadig ikke"),
    noteAt(T0 + 5, "eskaleret efter 2 runder", "system", "rejected"),
    hist(T0 + 6, "review", "done", "user"),
  ],
});
const rt = k.buildTimeline(rr);
eq(kindsOf(rt), ["created", "state", "state", "state", "state", "note", "state"], "timeline review rounds kinds");
eq(rt[2], { at: T0 + 2, kind: "state", text: "Review → Afvist — mangler test", by: "agenten" }, "timeline rejected by agent");
eq(rt[4], { at: T0 + 4, kind: "state", text: "Review → Afvist — stadig ikke", by: "dig" }, "timeline rejected by user");
eq(rt[6].text, "Review → Done", "timeline approved");

// Tjek fejlet → afvist af appen: Tjek-rapporten og afvisningen er `checks`; "tjek kører" forsvinder.
const checksTicket = full({
  checks: { state: "pending", failed: null, round: 0, startedAt: T0 + 10 },
  history: [hist(T0, null, "backlog"), hist(T0 + 5, "inProgress", "review", "agent")],
});
eq(kindsOf(k.buildTimeline(checksTicket)), ["created", "state", "checks"], "timeline checks running kinds");
eq(k.buildTimeline(checksTicket)[2], { at: T0 + 10, kind: "checks", text: "tjek kører", by: "systemet" }, "timeline checks running");
const checksDone = full({
  checks: { state: "failed", failed: "build", round: 0, startedAt: T0 + 10 },
  history: [
    hist(T0, null, "backlog"),
    hist(T0 + 5, "inProgress", "review", "agent"),
    hist(T0 + 30, "review", "rejected", "system", "afvist af appen: Tjek fejlede: build (exit 1). Se rapport 01."),
  ],
  reports: [report("01", "Tjek: 1 fejlede", "system", T0 + 20)],
});
const ct = k.buildTimeline(checksDone);
eq(kindsOf(ct), ["created", "state", "checks", "checks"], "timeline checks failed kinds");
eq(ct[2], { at: T0 + 20, kind: "checks", text: "rapport 01: Tjek: 1 fejlede", by: "appen" }, "timeline checks report");
eq(ct[3].text, "Review → Afvist — afvist af appen: Tjek fejlede: build (exit 1). Se rapport 01.", "timeline checks rejection text");
// Tjek-rapport efter starten fjerner "tjek kører" selv om tilstanden stadig er pending.
eq(
  kindsOf(k.buildTimeline(full({ ...checksTicket, reports: [report("01", "Tjek: alle bestået", "system", T0 + 20)] }))),
  ["created", "state", "checks"],
  "timeline checks report removes running line",
);
// En ældre Tjek-rapport (før startedAt) fjerner den ikke.
eq(
  kindsOf(k.buildTimeline(full({ ...checksTicket, reports: [report("01", "Tjek: alle bestået", "system", T0 + 1)] }))),
  ["created", "checks", "state", "checks"],
  "timeline older checks report keeps running line",
);
// Skipped checks: ingen "tjek kører".
eq(kindsOf(k.buildTimeline(full({ checks: { state: "skipped", failed: null, round: 0, startedAt: T0 + 10 } }))), ["created"], "timeline checks skipped");

// Vækning: note → state; app-genstart og agent-noter er `note`.
const woke = full({
  history: [
    hist(T0, null, "backlog"),
    noteAt(T0 + 1, "vækket: del-ticket godkendt", "system", "waiting"),
    hist(T0 + 2, "waiting", "inProgress", "system"),
    noteAt(T0 + 3, "app genstartet"),
    noteAt(T0 + 4, "agent afsluttet"),
  ],
});
eq(kindsOf(k.buildTimeline(woke)), ["created", "note", "state", "note", "note"], "timeline wake kinds");
eq(k.buildTimeline(woke)[2].text, "Venter → I gang", "timeline wake state");

// Skriv-tilbage fejlet → prøv igen → meldt tilbage, issue lukket, resultat skrevet, afbrudt.
const wb = full({
  history: [
    hist(T0, null, "backlog"),
    noteAt(T0 + 1, "kunne ikke melde tilbage: 502", "system", "done"),
    noteAt(T0 + 2, "meldt tilbage til GitHub #7", "system", "done"),
    noteAt(T0 + 3, "issue #7 lukket på GitHub", "system", "done"),
    noteAt(T0 + 4, "resultat skrevet til inbox/done/x.result.md", "system", "done"),
    noteAt(T0 + 5, "tilbagemelding afbrudt af genstart", "system", "done"),
  ],
});
eq(kindsOf(k.buildTimeline(wb)), ["created", "writeBack", "writeBack", "writeBack", "writeBack", "writeBack"], "timeline write-back kinds");

// Vagt, worktree, ny session, session fortsat, vagt-fejl, ny session fejlet.
const sys = full({
  history: [
    hist(T0, null, "backlog"),
    noteAt(T0 + 1, "startet af vagten (forløb «bug»)"),
    noteAt(T0 + 2, "worktree oprettet: ticket/ab12cd34"),
    noteAt(T0 + 3, "ny session til ticketen"),
    noteAt(T0 + 4, "session fortsat i ny mappe"),
    noteAt(T0 + 5, "vagt: forløbet kunne ikke startes: ingen agent"),
    noteAt(T0 + 6, "ny session kunne ikke startes (x); ticketen leveres i agentens nuværende session"),
    noteAt(T0 + 7, "tjek afbrudt af genstart"),
    noteAt(T0 + 8, "Tjek fejlede: build (exit 1). Se rapport 01."),
  ],
});
eq(
  kindsOf(k.buildTimeline(sys)),
  ["created", "watch", "git", "session", "session", "watch", "session", "checks", "checks"],
  "timeline system note kinds",
);
eq(k.buildTimeline(sys)[2], { at: T0 + 2, kind: "git", text: "worktree oprettet: ticket/ab12cd34", by: "systemet" }, "timeline worktree note");
// Review6d N6: "tjek…"/"afvist af appen" er kun `checks` når systemet skrev dem.
eq(
  kindsOf(
    k.buildTimeline(
      full({
        history: [
          hist(T0, null, "backlog"),
          noteAt(T0 + 1, "Tjek lige om login virker", "agent"),
          noteAt(T0 + 2, "tjek din mail", "user"),
          hist(T0 + 3, "review", "rejected", "user", "afvist af appen: det var mig"),
          hist(T0 + 4, "rejected", "rejected", "system", "afvist af appen: Tjek fejlede"),
          noteAt(T0 + 5, "tjek afbrudt af genstart", "system"),
        ],
      }),
    ),
  ),
  ["created", "note", "note", "state", "checks", "checks"],
  "timeline checks only by the system",
);
// git uden "worktree oprettet"-note udelades (ingen tid at opfinde).
eq(
  kindsOf(k.buildTimeline(full({ git: { mode: "worktree", branch: "ticket/ab12cd34", base: "main", repo: "/r", worktree: "/w" } }))),
  ["created"],
  "timeline git without note is omitted",
);
// writeBack-tilstand uden noter giver ingen linje (inflight er forbigående).
eq(
  kindsOf(
    k.buildTimeline(
      full({
        external: {
          kind: "github", externalId: "7", repo: "o/r", number: 7, path: null, url: null, title: "x", labels: [], author: null, notes: [],
          inboxItemId: "i", importedAt: T0, inherited: false, project: null, skipReview: false,
          writeBack: { comment: "inflight", close: "none", commentUrl: null, commentedAt: null, closedAt: null, attempts: 1, lastError: null, lastBody: "BODY" },
        },
      }),
    ),
  ),
  ["created"],
  "timeline write-back inflight is omitted",
);

// Rapporter: forfatter-etiketter; «Ændringer» fra appen er `git`; en agent-rapport "Tjek…" er en almindelig rapport.
const reps = full({
  reports: [
    report("01", "Plan", "agent", T0 + 1),
    report("02", "Note fra mig", "user", T0 + 2),
    report("03", "Ændringer", "system", T0 + 3),
    report("04", "Tjek af noget", "agent", T0 + 4),
  ],
});
const rp = k.buildTimeline(reps);
eq(kindsOf(rp), ["created", "report", "report", "git", "report"], "timeline report kinds");
eq(rp.slice(1).map((e) => e.by), ["agenten", "dig", "appen", "agenten"], "timeline report by");
eq(rp[3].text, "rapport 03: Ændringer", "timeline changes report text");

// Samme `at`: kilde-rækkefølgen holder (historik, rapporter, afledte), og historikken holder sin rækkefølge.
const same = full({
  playbookStartedAt: T0,
  history: [hist(T0, null, "backlog"), noteAt(T0, "startet af vagten (forløb «bug»)"), hist(T0, "backlog", "assigned", "system")],
  reports: [report("01", "Plan", "agent", T0)],
});
eq(kindsOf(k.buildTimeline(same, { children: [] })), ["created", "watch", "state", "report", "playbook"], "timeline same at keeps source order");
// Dublet: en afledt linje med samme `at` og tekst som en note fjernes.
eq(
  kindsOf(k.buildTimeline(full({ checks: { state: "pending", failed: null, round: 0, startedAt: T0 + 1 }, history: [hist(T0, null, "backlog"), noteAt(T0 + 1, "tjek kører")] }))),
  ["created", "checks"],
  "timeline derived duplicate removed",
);
// Tom note bliver "(note)" i stedet for en tom linje.
eq(k.buildTimeline(full({ history: [noteAt(T0, "")] }))[0], { at: T0, kind: "note", text: "(note)", by: "systemet" }, "timeline empty note");

// Ingen eksterne brødtekster: hverken body, lastBody eller rapporttekster når tidslinjen.
const leaky = full({
  body: "LEAK-BODY",
  external: {
    kind: "github", externalId: "7", repo: "o/r", number: 7, path: null, url: null, title: "LEAK-TITLE", labels: [], author: "LEAK-AUTHOR", notes: ["LEAK-NOTE"],
    inboxItemId: "i", importedAt: T0, inherited: false, project: null, skipReview: false,
    writeBack: { comment: "failed", close: "none", commentUrl: null, commentedAt: null, closedAt: null, attempts: 1, lastError: "LEAK-ERR", lastBody: "LEAK-LASTBODY" },
  },
  reports: [report("01", "Plan", "agent", T0 + 1)],
});
const leakText = k.timelineText(k.buildTimeline(leaky, { children: [] }), leaky);
eq(/LEAK-/.test(leakText), false, "timeline has no external body texts");
eq(/HEMMELIG/.test(k.timelineText(k.buildTimeline(full()), full())), false, "timeline has no ticket body");

// timelineText: overskrift + "{formatAt} · {text} ({by})" pr. linje.
const tl = k.buildTimeline(full({ history: [hist(T0, null, "backlog"), hist(T0 + 60_000, "backlog", "assigned", "user")] }));
eq(
  k.timelineText(tl, full()),
  [`ab12cd34 Titel`, `${k.formatAt(T0)} · oprettet i Backlog (dig)`, `${k.formatAt(T0 + 60_000)} · Backlog → I kø (dig)`].join("\n"),
  "timelineText format",
);
eq(k.timelineText([], full()), "ab12cd34 Titel", "timelineText empty");

// timelineCount
eq(k.timelineCount({ historyLen: 1, reportCount: 0, playbookStartedAt: null }), 1, "timelineCount plain");
eq(k.timelineCount({ historyLen: 4, reportCount: 2, playbookStartedAt: T0 }), 7, "timelineCount playbook");

// relativeText: grænserne (lokal middag, så "i dag"/"i går" ikke afhænger af tidszonen).
const noon = new Date(2026, 9, 3, 12, 0, 0).getTime();
const clock = (ms) => new Date(ms).toLocaleTimeString("da-DK", { hour: "2-digit", minute: "2-digit" });
for (const [ms, want, msg] of [
  [noon, "lige nu", "0"],
  [noon + 5000, "lige nu", "future (clock went back)"],
  [noon - 59_000, "lige nu", "59 s"],
  [noon - 60_000, "for 1 min siden", "60 s"],
  [noon - 3 * 60_000, "for 3 min siden", "3 min"],
  [noon - 59 * 60_000 - 59_000, "for 59 min siden", "59 min 59 s"],
  [noon - 60 * 60_000, "for 1 t siden", "60 min"],
  [noon - 2 * 3_600_000, "for 2 t siden", "2 t"],
  [noon - 11 * 3_600_000, "for 11 t siden", "11 t (same day)"],
  [noon - 13 * 3_600_000, `i går ${clock(noon - 13 * 3_600_000)}`, "13 t (yesterday 23)"],
  [noon - 20 * 3_600_000, `i går ${clock(noon - 20 * 3_600_000)}`, "20 t (yesterday)"],
  [noon - 35 * 3_600_000, `i går ${clock(noon - 35 * 3_600_000)}`, "35 t (yesterday morning)"],
  [noon - 37 * 3_600_000, k.formatAt(noon - 37 * 3_600_000), "37 t (two days ago)"],
  [noon - 3 * 86_400_000, k.formatAt(noon - 3 * 86_400_000), "3 days"],
])
  eq(k.relativeText(ms, noon), want, `relativeText ${msg}`);

console.log(`tickets.ts: ${n} cases ok`);
