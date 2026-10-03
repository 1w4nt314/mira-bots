// Truth table for src/lib/projects.ts: compiles it with the repo's TypeScript, runs it in node.
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import ts from "typescript";

const src = readFileSync(new URL("../src/lib/projects.ts", import.meta.url), "utf8");
const out = ts.transpileModule(src, { compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 } });
const file = join(mkdtempSync(join(tmpdir(), "projects-")), "projects.mjs");
writeFileSync(file, out.outputText);
const p = await import(pathToFileURL(file).href);

let n = 0;
const check = (actual, want, msg) => {
  assert.deepEqual(actual, want, msg);
  n++;
};

// validateProjectName: the same cases as projects.rs (null = ok, else the Danish reason)
const reason = (name, r) => `Projektnavnet «${name}» er ugyldigt: ${r}`;
for (const [name, want] of [
  ["a", null],
  ["mira-bots", null],
  ["æøå ok", null],
  ["a.b", null],
  ["x".repeat(64), null],
  ["", reason("", "tomt")],
  ["x".repeat(65), reason("x".repeat(65), "må højst være 64 tegn")],
  ["con", reason("con", "er et reserveret navn i Windows")],
  ["CON", reason("CON", "er et reserveret navn i Windows")],
  ["Nul.txt", reason("Nul.txt", "er et reserveret navn i Windows")],
  ["aux .txt", reason("aux .txt", "er et reserveret navn i Windows")],
  ["COM¹", reason("COM¹", "er et reserveret navn i Windows")],
  ["com10", null],
  ["x.", reason("x.", "må ikke slutte med punktum")],
  [" x", reason(" x", "må ikke begynde eller slutte med mellemrum")],
  ["x ", reason("x ", "må ikke begynde eller slutte med mellemrum")],
  ["a:b", reason("a:b", 'indeholder et ugyldigt tegn (< > : " / \\ | ? *)')],
  ["a/b", reason("a/b", 'indeholder et ugyldigt tegn (< > : " / \\ | ? *)')],
  ["a\tb", reason("a\tb", "indeholder et kontroltegn")],
  [".git", reason(".git", "må ikke begynde med punktum")],
  ["...", reason("...", "må ikke kun bestå af punktummer")],
]) {
  check(p.validateProjectName(name), want, `validateProjectName(${JSON.stringify(name)})`);
}

// projectIdOf / projectName / projectLabel / sameProjectId
check(p.projectIdOf("a"), "a", "projectIdOf(a)");
check(p.projectIdOf({ new: "b" }), null, "projectIdOf({new})");
check(p.projectIdOf(null), null, "projectIdOf(null)");
check(p.projectName({ new: "b" }), "b", "projectName({new})");
check(p.projectLabel("a"), "a", "projectLabel(a)");
check(p.projectLabel({ new: "b" }), "+b", "projectLabel({new})");
check(p.projectLabel(null), "uden projekt", "projectLabel(null)");
check(p.sameProjectId("A", "a"), true, "sameProjectId(A, a)");
check(p.sameProjectId("Æ", "æ"), true, "sameProjectId folds non-ASCII, like Rust's to_lowercase");
check(p.sameProjectId("Økonomi", "økonomi"), true, "sameProjectId(Økonomi, økonomi)");
check(p.sameProjectId("Økonomi", "Okonomi"), false, "Ø is not O");
check(p.sameProjectId(null, null), false, "null equals nothing");
check(p.sameProjectId("a", null), false, "a vs null");

// assignmentIssue (C4b.8)
const work = (project) => ({ seatKind: "work", project });
const staff = { seatKind: "staff", project: null };
for (const [ticket, agent, want, msg] of [
  [null, staff, null, "staff, no project"],
  ["a", staff, null, "staff, existing"],
  [{ new: "x" }, staff, null, "staff, new"],
  [null, work("a"), { kind: "needsProject" }, "work, no project"],
  ["a", work("a"), null, "work, same"],
  ["A", work("a"), null, "work, same (case)"],
  ["a", work("b"), { kind: "wrongProject", agentProject: "b", ticketProject: "a" }, "work, other"],
  [{ new: "a" }, work("a"), null, "work, new = agent's"],
  [{ new: "A" }, work("a"), null, "work, new = agent's (case)"],
  [{ new: "c" }, work("a"), { kind: "wrongProject", agentProject: "a", ticketProject: "c" }, "work, new other"],
]) {
  check(p.assignmentIssue({ project: ticket }, agent), want, `assignmentIssue: ${msg}`);
}
check(
  p.wrongProjectText("B", "A"),
  "Agenten står i projekt «B»; ticketen hører til «A». Flyt agenten fra dens terminalpanel («Flyt til projekt…»), eller vælg en anden agent.",
  "wrongProjectText",
);

// coordinatorHint / liveWorkAgentsIn / hasLiveCoordinator
const agent = (id, seatKind, project, roles = ["coder"], status = "idle") => ({
  id,
  seatKind,
  project,
  roles,
  status: status === "exited" ? { kind: "exited", code: 0 } : { kind: status },
});
const two = [agent("1", "work", "p"), agent("2", "work", "P")];
check(p.liveWorkAgentsIn(two, "p").length, 2, "liveWorkAgentsIn counts case-insensitively");
check(p.coordinatorHint(two, "p"), "2 agenter, ingen koordinator", "two agents, no coordinator");
check(
  p.coordinatorHint([...two, agent("k", "staff", null, ["coordinator"])], "p"),
  null,
  "with a live coordinator",
);
check(
  p.coordinatorHint([...two, agent("k", "staff", null, ["coordinator"], "exited")], "p"),
  "2 agenter, ingen koordinator",
  "an exited coordinator does not count",
);
check(p.coordinatorHint([agent("1", "work", "p")], "p"), null, "one agent");
check(
  p.coordinatorHint([agent("1", "work", "p"), agent("2", "work", "p", ["coder"], "exited")], "p"),
  null,
  "an exited agent does not count",
);
check(p.coordinatorHint(two, "q"), null, "another project");
check(p.hasLiveCoordinator([agent("w", "work", "p", ["coder", "coordinator"])]), true, "coordinator on a work seat");

// backlogHint (step 6a)
const wa = (id, project, over = {}) => ({ ...agent(id, "work", project), name: id, currentTicketId: null, queueLength: 0, ...over });
const bt = (id, over = {}) => ({
  id,
  state: "backlog",
  assigneeAgentId: null,
  project: "p",
  createdAt: 0,
  blockedBy: [],
  ...over,
});
{
  const h = p.backlogHint([wa("coder-01", "p")], [bt("b", { createdAt: 5 }), bt("a", { createdAt: 1 })], "p");
  check(h.text, "coder-01 er ledig: 2 tickets uden ejer", "backlogHint text (plural)");
  check(h.next.id, "a", "backlogHint next is the oldest");
  check(h.waiting.map((t) => t.id), ["a", "b"], "backlogHint waiting oldest first");
  check(h.agent.id, "coder-01", "backlogHint agent");
}
check(p.backlogHint([wa("c", "p")], [bt("a")], "p").text, "c er ledig: 1 ticket uden ejer", "backlogHint singular");
check(p.backlogHint([wa("c", "P")], [bt("a", { project: "p" })], "p")?.next.id, "a", "backlogHint folds project case");
check(p.backlogHint([wa("c", "p")], [bt("a", { project: { new: "p" } })], "p")?.next.id, "a", "backlogHint {new} = agent's project");
check(p.backlogHint([wa("c", "p")], [bt("a", { state: "rejected" })], "p")?.next.id, "a", "backlogHint rejected without assignee");
{
  const blocked = bt("x", { blockedBy: ["open"], createdAt: 0 });
  const open = bt("open", { state: "review", assigneeAgentId: "z", createdAt: 1 });
  const h = p.backlogHint([wa("c", "p")], [blocked, open, bt("y", { createdAt: 2 })], "p");
  check(h.waiting.map((t) => t.id), ["y"], "backlogHint skips a blocked ticket");
  check(h.next.id, "y", "backlogHint next skips a blocked ticket");
  check(p.backlogHint([wa("c", "p")], [blocked, open], "p"), null, "only a blocked ticket -> null");
}
check(
  p.backlogHint([wa("c", "p")], [bt("x", { blockedBy: ["d", "gone"] }), bt("d", { state: "done" })], "p")?.next.id,
  "x",
  "a done or deleted blocker does not block",
);
check(p.backlogHint([wa("c", "p", { queueLength: 1 })], [bt("a")], "p"), null, "agent with a queue -> null");
check(p.backlogHint([wa("c", "p", { currentTicketId: "t" })], [bt("a")], "p"), null, "agent with a current ticket -> null");
check(p.backlogHint([wa("c", "p", { status: { kind: "thinking" } })], [bt("a")], "p"), null, "busy agent -> null");
check(p.backlogHint([wa("c", "p", { status: { kind: "exited", code: 0 } })], [bt("a")], "p"), null, "exited agent -> null");
check(p.backlogHint([agent("s", "staff", "p")], [bt("a")], "p"), null, "staff agent does not count");
check(p.backlogHint([wa("c", "p")], [bt("a", { project: null })], "p"), null, "ticket without a project does not count");
check(p.backlogHint([wa("c", "p")], [bt("a", { project: "q" })], "p"), null, "another project -> null");
check(p.backlogHint([wa("c", "q")], [bt("a")], "p"), null, "agent in another project -> null");
check(p.backlogHint([wa("c", "p")], [bt("a", { state: "done" }), bt("b", { state: "review", assigneeAgentId: "z" })], "p"), null, "no backlog tickets -> null");
check(p.backlogHint([wa("c", "p")], [bt("a", { assigneeAgentId: "z", state: "assigned" })], "p"), null, "owned ticket does not count");
check(p.backlogHint([], [bt("a")], "p"), null, "no agents -> null");
check(
  p.backlogHint([wa("busy", "p", { queueLength: 2 }), wa("free", "p")], [bt("a")], "p")?.agent.id,
  "free",
  "picks the first free agent",
);

// countsByProject
const tickets = [{ project: "p" }, { project: "P" }, { project: { new: "p" } }, { project: null }, { project: "q" }];
const counts = p.countsByProject([...two, agent("3", "work", "q", ["coder"], "exited"), agent("s", "staff", null)], tickets);
check(p.countsFor(counts, "p"), { agents: 2, tickets: 2 }, "countsByProject p");
check(p.countsFor(counts, "Q"), { agents: 0, tickets: 1 }, "countsByProject q (exited agent ignored)");
check(p.countsFor(counts, "zz"), { agents: 0, tickets: 0 }, "countsFor missing");
check([...counts.keys()].sort(), ["p", "q"], "countsByProject keys");

// parseProjectFilter / projectFilterKey / matchesFilter
for (const [s, want] of [
  [null, "all"],
  ["all", "all"],
  ["", "all"],
  ["none", "none"],
  ["p", { id: "p" }],
  ["project:none", { id: "none" }],
]) {
  check(p.parseProjectFilter(s), want, `parseProjectFilter(${s})`);
}
for (const f of ["all", "none", { id: "p" }, { id: "none" }, { id: "all" }]) {
  check(p.parseProjectFilter(p.projectFilterKey(f)), f, `round trip ${JSON.stringify(f)}`);
}
check(p.matchesFilter({ project: "P" }, { id: "p" }), true, "matches same id");
check(p.matchesFilter({ project: { new: "p" } }, { id: "p" }), true, "matches a new project by name");
check(p.matchesFilter({ project: null }, { id: "p" }), false, "no project under an id");
check(p.matchesFilter({ project: null }, "none"), true, "none");
check(p.matchesFilter({ project: "p" }, "none"), false, "project under none");
check(p.matchesFilter({ project: "p" }, "all"), true, "all");

// projectPath
check(p.projectPath("C:\\Users\\a\\mira-bots\\projects", "x"), "C:\\Users\\a\\mira-bots\\projects\\x", "projectPath windows");
check(p.projectPath("/home/a/projects/", "x"), "/home/a/projects/x", "projectPath unix");

console.log(`projects.ts: ${n} cases ok`);
