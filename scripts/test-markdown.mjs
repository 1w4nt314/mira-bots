// Truth table for src/lib/markdown.ts (plan C5.14): compiles it with the repo's TypeScript and
// checks `parseMarkdown` in node. HTML must stay text; nothing is ever parsed as markup.
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import ts from "typescript";

const src = readFileSync(new URL("../src/lib/markdown.ts", import.meta.url), "utf8");
const out = ts.transpileModule(src, {
  compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 },
});
const file = join(mkdtempSync(join(tmpdir(), "markdown-")), "markdown.mjs");
writeFileSync(file, out.outputText);
const { parseMarkdown, renderMarkdown, MARKDOWN_MAX_CHARS } = await import(pathToFileURL(file).href);

const T = (v) => ({ t: "text", v });
const C = (v) => ({ t: "code", v });
const B = (...v) => ({ t: "strong", v });
const E = (...v) => ({ t: "em", v });
const P = (...inl) => ({ t: "para", inl });
const H = (level, ...inl) => ({ t: "heading", level, inl });
const L = (ordered, ...items) => ({ t: "list", ordered, items });

const long = "a".repeat(MARKDOWN_MAX_CHARS + 5);

const table = [
  ["empty", "", []],
  ["heading 1", "# Titel", [H(1, T("Titel"))]],
  ["heading 6 and 7 hashes", "###### x\n\n####### y", [H(6, T("x")), P(T("####### y"))]],
  ["hash without space is text", "#Titel", [P(T("#Titel"))]],
  ["paragraphs split by blank line", "a\nb\n\nc", [P(T("a\nb")), P(T("c"))]],
  ["unordered list with - and *", "- a\n* b", [L(false, [T("a")], [T("b")])]],
  ["ordered list", "1. a\n2. b", [L(true, [T("a")], [T("b")])]],
  ["mixed lists split", "- a\n1. b\n- c", [L(false, [T("a")]), L(true, [T("b")]), L(false, [T("c")])]],
  ["indented continuation", "- a\n  fortsat\n- b", [L(false, [T("a fortsat")], [T("b")])]],
  ["fenced code with lang", "```ts\nconst x = 1;\n\n**y**\n```\nefter", [
    { t: "code", lang: "ts", text: "const x = 1;\n\n**y**" },
    P(T("efter")),
  ]],
  ["unclosed fence runs to end", "før\n```\nx\n# ikke overskrift", [
    P(T("før")),
    { t: "code", lang: null, text: "x\n# ikke overskrift" },
  ]],
  ["quote lines joined", "> a\n> b\n\nc", [{ t: "quote", inl: [T("a\nb")] }, P(T("c"))]],
  ["block type change", "tekst\n# H\n- l", [P(T("tekst")), H(1, T("H")), L(false, [T("l")])]],
  ["inline code beats strong", "`a **b**` c", [P(C("a **b**"), T(" c"))]],
  ["strong and em", "**fed** og *kursiv*", [P(B(T("fed")), T(" og "), E(T("kursiv")))]],
  ["code inside strong", "**fed `k*o*de`**", [P(B(T("fed "), C("k*o*de")))]],
  ["unclosed strong is text", "**uafsluttet og 2 * 3 * 4", [P(T("**uafsluttet og 2 * 3 * 4"))]],
  ["script tag is text", "<script>alert(1)</script>", [P(T("<script>alert(1)</script>"))]],
  ["raw html block is text", "<div onclick=\"x()\">\n<b>fed</b>\n</div>", [
    P(T("<div onclick=\"x()\">\n<b>fed</b>\n</div>")),
  ]],
  ["CRLF and æøå", "# Æble\r\nø **å**", [H(1, T("Æble")), P(T("ø "), B(T("å")))]],
  ["over the limit is cut with …", long, [P(T("a".repeat(MARKDOWN_MAX_CHARS) + "…"))]],
];

for (const [name, input, want] of table) {
  assert.deepEqual(parseMarkdown(input), want, name);
}
assert.equal(renderMarkdown, parseMarkdown, "renderMarkdown is an alias");
console.log(`parseMarkdown: ${table.length} cases ok`);
