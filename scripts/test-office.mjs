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

// clampFloorHeight(wanted, available, minFloor?, minTerm?) — defaults FLOOR_MIN 312, TERM_MIN 276
for (const [w, a, want] of [
  [360, 668, 360],
  [100, 668, 312],
  [900, 668, 392],
  [360, 300, 180], // too low for both: one seat row, the floor scrolls
  [360, Number.NaN, 360],
  [Number.NaN, 668, 360],
  [360, 0, 180],
  [380.4, 668, 380],
  [380.6, 668, 381],
]) {
  check(o.clampFloorHeight(w, a), want, `clampFloorHeight(${w}, ${a})`);
}
for (const [w, a, minF, minT, want] of [
  [100, 668, 348, 276, 348], // "more": the wall row too
  [900, 668, 348, 300, 368], // measured queue raises the terminal minimum
  [900, 560, 348, 276, 284], // both cannot fit: the terminal wins, the floor scrolls
  [100, 560, 348, 276, 284], // ... whatever was wanted (min = max: splitter hidden)
  [900, 300, 312, 276, 180], // hard bottom: one seat row (discreet)
  [900, 300, 348, 276, 180], // ... also with "more" (the wall scrolls away)
  [100, 588, 312, 276, 312], // exactly enough for both: two rows, terminal at its minimum
  [900, 588, 312, 276, 312],
  [900, 587, 312, 276, 311], // 1 px short: cramped
  [Number.MAX_SAFE_INTEGER, 668, 312, 276, 392], // splitter's aria-valuemax / End
]) {
  check(o.clampFloorHeight(w, a, minF, minT), want, `clampFloorHeight(${w}, ${a}, ${minF}, ${minT})`);
}
check(o.clampFloorHeight(900, 300, 312, 276, 200), 200, "clampFloorHeight with an explicit hardMin");
// clampFloorHeight as Workplace calls it: floorMin(detail), termMinFor(measured header/queue)
for (const [w, a, d, chrome, want] of [
  [360, 668, "discreet", 150, 360], // 1100x720, a running + a queued ticket: unchanged
  [360, 668, "more", 150, 360],
  [360, 668, "more", 250, 293], // 1100x720 + 100 px queue/reviews: terminal keeps XTERM_MIN
  [360, 668, "more", 290, 253], // + 140 px (starting hint too)
  [360, 668, "discreet", 290, 253],
  [360, 488, "more", 168, 195], // 820x540 "Lidt mere": 488 - 293 = 195, terminal keeps its minimum
  [360, 450, "more", 168, 180], // lower than the window minimum: hard bottom
  [360, 488, "discreet", 168, 195], // 820x540 "Diskret": terminal minimum met exactly
  [360, Number.NaN, "more", 168, 360], // not measured yet
]) {
  const minT = o.termMinFor(chrome);
  check(
    o.clampFloorHeight(w, a, o.floorMin(d), minT),
    want,
    `clampFloorHeight(${w}, ${a}, floorMin(${d}), termMinFor(${chrome}))`,
  );
}

// floorMin(detail): both seat rows unclipped at DESK_H_MIN
check(o.floorMin("discreet"), 2 * 132 + 48, "floorMin(discreet)");
check(o.floorMin("more"), 2 * 132 + 48 + 36, "floorMin(more)");
check(o.FLOOR_MIN, o.floorMin("discreet"), "FLOOR_MIN = floorMin(discreet)");
check(o.floorMin("discreet", 1), 132 + 48, "floorMin(discreet, 1)");
check(o.floorMin("more", 1), 132 + 48 + 36, "floorMin(more, 1)");
check(o.FLOOR_HARD_MIN, o.floorMin("discreet", 1), "FLOOR_HARD_MIN = floorMin(discreet, 1)");
for (const d of ["discreet", "more"]) {
  check(o.deskLayout(o.floorMin(d), d).deskH, o.DESK_H_MIN, `deskLayout(floorMin(${d})) = DESK_H_MIN`);
}

// termMinFor(chromeH): header/queue + border 1 + XTERM_MIN, never below TERM_MIN
for (const [c, want] of [
  [0, 276],
  [Number.NaN, 276],
  [119, 276],
  [151, 276],
  [152, 277],
  [200.2, 326],
]) {
  check(o.termMinFor(c), want, `termMinFor(${c})`);
}

// deskLayout(floorHeight, detail, rows?)
for (const [h, d, rows, want] of [
  [360, "discreet", undefined, { deskH: 156, fig: 87 }],
  [360, "more", undefined, { deskH: 138, fig: 77 }],
  [384, "discreet", undefined, { deskH: 168, fig: 94 }],
  [384, "more", undefined, { deskH: 150, fig: 84 }],
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
for (const [s, want] of [["420", 420], ["420.6", 421], ["abc", 360], [null, 360], ["-5", 360], ["0", 360], ["1e9", 1e9]]) {
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
check({ ...o.COMPACT }, { deskH: 48, fig: 28 }, "COMPACT");
check(o.TERM_LINE_H, 34, "TERM_LINE_H");
check(o.PLACEHOLDER_MIN, 56, "PLACEHOLDER_MIN");
// No agent selected at the minimum window (available 488): the office keeps its default height.
check(o.clampFloorHeight(360, 488, o.floorMin("discreet"), o.PLACEHOLDER_MIN), 360, "placeholder keeps floor at 820x540");
check(o.clampFloorHeight(360, 488, o.floorMin("more"), o.PLACEHOLDER_MIN), 360, "placeholder keeps floor at 820x540 (more)");
check(o.SPLITTER_STEP, 16, "SPLITTER_STEP");
check(
  [o.FLOOR_DEFAULT, o.FLOOR_MIN, o.TERM_MIN, o.XTERM_MIN, o.SPLITTER_H, o.FLOOR_CHROME, o.WALL_H],
  [360, 312, 276, 124, 8, 48, 36],
  "floor constants",
);

console.log(`office.ts: ${n} cases ok`);
