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
eq(k.MAX_REVIEW_ROUNDS, 3, "MAX_REVIEW_ROUNDS");

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

console.log(`tickets.ts: ${n} cases ok`);
