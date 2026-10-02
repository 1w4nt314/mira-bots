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

console.log(`tickets.ts: ${n} cases ok`);
