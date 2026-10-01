// Truth table for src/lib/models.ts: the same model rules as `model_is_valid` in Rust
// (profiles/model.rs, test `model_is_valid_table`) plus the effort levels and labels.
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import ts from "typescript";

const src = readFileSync(new URL("../src/lib/models.ts", import.meta.url), "utf8");
const out = ts.transpileModule(src, {
  compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 },
});
const file = join(mkdtempSync(join(tmpdir(), "models-")), "models.mjs");
writeFileSync(file, out.outputText);
const m = await import(pathToFileURL(file).href);

const table = [
  // aliases (all nine the backend accepts)
  ["default", true],
  ["best", true],
  ["fable", true],
  ["sonnet", true],
  ["opus", true],
  ["haiku", true],
  ["sonnet[1m]", true],
  ["opus[1m]", true],
  ["opusplan", true],
  // full ids
  ["claude-sonnet-5-5", true],
  ["claude-opus-5-5[1m]", true],
  ["claude-haiku-4-5-20251001", true],
  ["claude-x", true],
  [`claude-${"a".repeat(57)}`, true],
  // invalid
  ["bogus", false],
  ["Claude-x", false],
  ["claude-", false],
  ["claude-[1m]", false],
  ["claude-sonnet 5", false],
  [" sonnet", false],
  ["claude-Sonnet", false],
  ["claude-x[2m]", false],
  ["", false],
  [`claude-${"a".repeat(58)}`, false],
];
for (const [input, want] of table) assert.equal(m.isValidModel(input), want, JSON.stringify(input));

assert.deepEqual(m.EFFORT_LEVELS, ["low", "medium", "high", "xhigh", "max"]);
assert.ok(!m.MODEL_ALIASES.includes("default"), "default is the Standard entry");
assert.equal(m.MODEL_ALIASES.length, 8);
for (const e of m.EFFORT_LEVELS) assert.ok(m.isEffort(e), e);
assert.ok(!m.isEffort("ultra"));
assert.equal(m.modelLabel(null), "standard");
assert.equal(m.modelLabel("opus"), "opus");
assert.equal(m.effortLabel(null), "standard");
assert.equal(m.effortLabel("xhigh"), "xhigh");
assert.equal(m.REPORT_BODY_MAX, 20000);
assert.equal(m.PROMPT_APPEND_MAX, 4000);
console.log(`isValidModel: ${table.length} cases ok (+ effort/labels)`);

// src/lib/roles.ts: staff roles (mirrors `Role::is_staff` / `has_staff_role` in Rust, test
// `staff_and_work_roles`). The module only has type imports, so it transpiles on its own.
const rolesSrc = readFileSync(new URL("../src/lib/roles.ts", import.meta.url), "utf8");
const rolesOut = ts.transpileModule(rolesSrc, {
  compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 },
});
const rolesFile = join(mkdtempSync(join(tmpdir(), "roles-")), "roles.mjs");
writeFileSync(rolesFile, rolesOut.outputText);
const r = await import(pathToFileURL(rolesFile).href);
assert.deepEqual([...r.STAFF_ROLES], ["reviewer", "coordinator", "planner"]);
const staffTable = [
  [[], false],
  [["coder"], false],
  [["researcher"], false],
  [["debugger"], false],
  [["coder", "researcher", "debugger"], false],
  [["reviewer"], true],
  [["coordinator"], true],
  [["planner"], true],
  [["coder", "reviewer"], true],
  [["debugger", "planner"], true],
  [[...r.ROLE_ORDER], true],
];
for (const [roles, want] of staffTable) assert.equal(r.hasStaffRole(roles), want, JSON.stringify(roles));
console.log(`hasStaffRole: ${staffTable.length} cases ok`);
// staffRank: the coordinator is preferred on a staff seat, then reviewer, then planner.
const rankTable = [
  [["coordinator"], 0],
  [["reviewer"], 1],
  [["planner"], 2],
  [["coder"], 3],
  [[], 3],
  [["planner", "reviewer"], 1],
  [["reviewer", "coordinator"], 0],
  [[...r.ROLE_ORDER], 0],
];
for (const [roles, want] of rankTable) assert.equal(r.staffRank(roles), want, JSON.stringify(roles));
console.log(`staffRank: ${rankTable.length} cases ok`);
