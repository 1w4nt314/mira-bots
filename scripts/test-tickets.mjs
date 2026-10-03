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

console.log(`tickets.ts: ${n} cases ok`);
