// Truth table for src/lib/terminalInput.ts: compiles it with the repo's TypeScript, runs it in node.
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import ts from "typescript";

const src = readFileSync(new URL("../src/lib/terminalInput.ts", import.meta.url), "utf8");
const out = ts.transpileModule(src, { compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 } });
const file = join(mkdtempSync(join(tmpdir(), "terminal-input-")), "terminalInput.mjs");
writeFileSync(file, out.outputText);
const { classifyInput } = await import(pathToFileURL(file).href);

const table = [
  ["a", Infinity, true],
  ["\r", Infinity, true],
  ["\x1b[A", 3, true],
  ["\x1b", 10, true],
  ["\x1b[?1;2c", Infinity, false],
  ["\x1b[12;40R", 500, false],
  ["\x1b[I", Infinity, false],
  ["\x1b[O", Infinity, false],
  ["\x1b[200~hej\x1b[201~", Infinity, true],
  ["hello\nworld", Infinity, true],
  ["æ", 1000, true],
];
for (const [data, ms, want] of table) {
  assert.equal(classifyInput(data, ms), want, `${JSON.stringify(data)} @ ${ms} ms`);
}
console.log(`classifyInput: ${table.length} cases ok`);
