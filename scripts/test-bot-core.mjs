// Snapshot test for src/lib/botCore.ts: compiles it with the repo's TypeScript and compares
// `renderBot` string for string with the oracle src/assets/bots/bot-core.reference.js
// (CommonJS, loaded with createRequire) over the full matrix: 4 states x 10 role sets x
// dark/light x specialist true/false = 160 cases.
import assert from "node:assert/strict";
import { copyFileSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import ts from "typescript";

const src = readFileSync(new URL("../src/lib/botCore.ts", import.meta.url), "utf8");
const out = ts.transpileModule(src, {
  compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 },
});
const file = join(mkdtempSync(join(tmpdir(), "bot-core-")), "botCore.mjs");
writeFileSync(file, out.outputText);
const { renderBot } = await import(pathToFileURL(file).href);

// The package is "type": "module", so a .js file would load as ESM: require a .cjs copy instead.
const oracle = join(mkdtempSync(join(tmpdir(), "bot-core-ref-")), "bot-core.reference.cjs");
copyFileSync(fileURLToPath(new URL("../src/assets/bots/bot-core.reference.js", import.meta.url)), oracle);
const { bot } = createRequire(import.meta.url)(oracle);

const STATES = ["idle", "work", "wait", "done"];
const ROLE_SETS = [
  ["coder"],
  ["researcher"],
  ["reviewer"],
  ["koord"],
  ["planner"],
  ["debugger"],
  ["coder", "reviewer"],
  ["planner", "researcher", "debugger"],
  ["coder", "researcher", "reviewer", "koord", "planner", "debugger"],
  [],
];

let n = 0;
for (const st of STATES)
  for (const roles of ROLE_SETS)
    for (const dark of [true, false])
      for (const spec of [true, false]) {
        const id = `c${n}`;
        const want = bot(st, roles, dark, spec, id);
        const got = renderBot({ state: st, roles, dark, specialist: spec, id });
        assert.equal(got, want, `${st} [${roles.join(",")}] dark=${dark} spec=${spec}`);
        n++;
      }
assert.equal(n, 160);
console.log(`renderBot: ${n} cases ok`);
