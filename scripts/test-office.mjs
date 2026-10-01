// Truth table for src/lib/office.ts: compiles it with the repo's TypeScript, runs it in node.
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import ts from "typescript";

const src = readFileSync(new URL("../src/lib/office.ts", import.meta.url), "utf8");
const out = ts.transpileModule(src, { compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 } });
const file = join(mkdtempSync(join(tmpdir(), "office-")), "office.mjs");
writeFileSync(file, out.outputText);
const o = await import(pathToFileURL(file).href);

let n = 0;
const check = (actual, want, msg) => {
  assert.deepEqual(actual, want, msg);
  n++;
};

// clampFloorHeight(wanted, available)
for (const [w, a, want] of [
  [384, 668, 384],
  [100, 668, 250],
  [900, 668, 528],
  [384, 300, 250],
  [384, Number.NaN, 384],
  [Number.NaN, 668, 384],
  [384, 0, 250],
  [400.4, 668, 400],
  [400.6, 668, 401],
]) {
  check(o.clampFloorHeight(w, a), want, `clampFloorHeight(${w}, ${a})`);
}

// deskLayout(floorHeight, detail, rows?)
for (const [h, d, rows, want] of [
  [384, "discreet", undefined, { deskH: 169, fig: 95 }],
  [384, "more", undefined, { deskH: 151, fig: 85 }],
  [200, "discreet", undefined, { deskH: 132, fig: 74 }],
  [900, "discreet", undefined, { deskH: 240, fig: 134 }],
  [Number.NaN, "discreet", undefined, { deskH: 132, fig: 74 }],
  [0, "more", undefined, { deskH: 132, fig: 74 }],
  [300, "discreet", 1, { deskH: 240, fig: 134 }],
]) {
  check(o.deskLayout(h, d, rows), want, `deskLayout(${h}, ${d}, ${rows})`);
}

// itemsFor
check(o.itemsFor("a"), o.itemsFor("a"), "itemsFor is deterministic");
check(o.itemsFor("agent-7").length, 2, "itemsFor gives two items");
const used = new Set();
for (let i = 1; i <= 40; i++) {
  const items = o.itemsFor(`agent-${i}`);
  assert.ok(o.ITEM_SETS.some((s) => s[0] === items[0] && s[1] === items[1]), `agent-${i} uses an ITEM_SETS entry`);
  used.add(items.join(","));
}
assert.ok(used.size >= 4, `at least 4 distinct sets over 40 keys (got ${used.size})`);
n++;
check(o.hashKey(""), 0x811c9dc5, "FNV-1a offset basis");
check(o.hashKey("a"), 0xe40c292c, "FNV-1a of 'a'");

// parsers
for (const [s, want] of [["min", "min"], ["max", "max"], ["normal", "normal"], [null, "normal"], ["MAX", "normal"], ["", "normal"]]) {
  check(o.parseTermMode(s), want, `parseTermMode(${JSON.stringify(s)})`);
}
for (const [s, want] of [["more", "more"], ["discreet", "discreet"], [null, "discreet"], ["x", "discreet"]]) {
  check(o.parseDetail(s), want, `parseDetail(${JSON.stringify(s)})`);
}
for (const [s, want] of [["420", 420], ["420.6", 421], ["abc", 384], [null, 384], ["-5", 384], ["0", 384], ["1e9", 1e9]]) {
  check(o.parseFloorHeight(s), want, `parseFloorHeight(${JSON.stringify(s)})`);
}

// constants
check(
  o.STORAGE_KEYS,
  {
    termMode: "mira-bots.workplace.termMode",
    floorHeight: "mira-bots.workplace.floorHeight",
    detail: "mira-bots.workplace.detail",
  },
  "STORAGE_KEYS",
);
check({ ...o.COMPACT }, { deskH: 48, fig: 40 }, "COMPACT");
check(o.TERM_LINE_H, 34, "TERM_LINE_H");
check(o.SPLITTER_STEP, 16, "SPLITTER_STEP");
check([o.FLOOR_DEFAULT, o.FLOOR_MIN, o.TERM_MIN, o.SPLITTER_H], [384, 250, 140, 8], "floor constants");

console.log(`office.ts: ${n} cases ok`);
