// Markdown mini-parser for reports (plan C5.14): text → a small AST that `components/Markdown`
// turns into React elements. Pure, no dependencies, no HTML: tags such as `<script>` are plain
// text in the AST and React escapes them when rendering. `dangerouslySetInnerHTML` is never used.
//
// Truth table (input → blocks; `inl` shortened to its text, "\n" = line break):
//
// | input                                  | result                                                   |
// |----------------------------------------|----------------------------------------------------------|
// | ""                                     | []                                                       |
// | "# Titel"                              | heading 1 "Titel"                                        |
// | "###### x"                             | heading 6 "x"; "####### x" is a paragraph                |
// | "#Titel"                               | paragraph "#Titel" (a space is required)                 |
// | "a\nb\n\nc"                            | para "a\nb", para "c" (blank lines separate blocks)      |
// | "- a\n- b"  / "* a"                    | list (unordered) [a, b] / [a]                            |
// | "1. a\n2. b"                           | list (ordered) [a, b]                                    |
// | "- a\n1. b"                            | list unordered [a], list ordered [b] (type change)       |
// | "- a\n  fortsat"                       | list [a fortsat] (indented line continues the item)      |
// | "```ts\nx\n```"                        | code lang "ts" text "x"                                  |
// | "```\nx"                               | code lang null text "x" (unclosed: runs to the end)      |
// | "> a\n> b"                             | quote "a\nb"                                             |
// | "tekst\n# H"                           | para "tekst", heading 1 "H" (block type change)          |
// | "`a **b**`"                            | code span "a **b**" (no nesting inside code)             |
// | "**fed** og *kursiv*"                  | strong [fed], text " og ", em [kursiv]                   |
// | "**fed `kode`**"                       | strong [text "fed ", code "kode"]                        |
// | "**uafsluttet" / "2 * 3 * 4"           | text as typed                                            |
// | "<script>alert(1)</script>"            | para with that exact text (rendered as text)             |
// | > 20 000 characters                    | cut to 20 000 characters + "…"                           |

export type Inline =
  | { t: "text"; v: string }
  | { t: "code"; v: string }
  | { t: "strong"; v: Inline[] }
  | { t: "em"; v: Inline[] };

export type Block =
  | { t: "heading"; level: 1 | 2 | 3 | 4 | 5 | 6; inl: Inline[] }
  | { t: "para"; inl: Inline[] }
  | { t: "list"; ordered: boolean; items: Inline[][] }
  | { t: "code"; lang: string | null; text: string }
  | { t: "quote"; inl: Inline[] };

/** Mirrors `REPORT_BODY_MAX_CHARS`: longer input is cut and marked with "…". */
export const MARKDOWN_MAX_CHARS = 20000;

const FENCE = /^\s{0,3}```(.*)$/;
const HEADING = /^\s{0,3}(#{1,6})(?:[ \t]+(.*?))?[ \t]*$/;
const BULLET = /^\s{0,3}[-*][ \t]+(.*)$/;
const ORDERED = /^\s{0,3}\d{1,9}\.[ \t]+(.*)$/;
const QUOTE = /^\s{0,3}>[ \t]?(.*)$/;

type Open =
  | { t: "para"; lines: string[] }
  | { t: "list"; ordered: boolean; items: string[] }
  | { t: "quote"; lines: string[] }
  | null;

export function parseMarkdown(input: string): Block[] {
  const chars = Array.from(input);
  const text =
    chars.length > MARKDOWN_MAX_CHARS ? chars.slice(0, MARKDOWN_MAX_CHARS).join("") + "…" : input;
  const lines = text.replace(/\r\n?/g, "\n").split("\n");
  const out: Block[] = [];
  let open: Open = null;

  const close = () => {
    if (open === null) return;
    if (open.t === "para") out.push({ t: "para", inl: parseInline(open.lines.join("\n")) });
    else if (open.t === "quote") out.push({ t: "quote", inl: parseInline(open.lines.join("\n")) });
    else out.push({ t: "list", ordered: open.ordered, items: open.items.map((i) => parseInline(i)) });
    open = null;
  };

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const fence = FENCE.exec(line);
    if (fence !== null) {
      close();
      const lang = fence[1].trim();
      const body: string[] = [];
      i++;
      while (i < lines.length && !FENCE.test(lines[i])) body.push(lines[i++]);
      out.push({ t: "code", lang: lang === "" ? null : lang, text: body.join("\n") });
      continue;
    }
    if (line.trim() === "") {
      close();
      continue;
    }
    const heading = HEADING.exec(line);
    if (heading !== null) {
      close();
      const level = heading[1].length as 1 | 2 | 3 | 4 | 5 | 6;
      out.push({ t: "heading", level, inl: parseInline(heading[2] ?? "") });
      continue;
    }
    const bullet = BULLET.exec(line);
    const ordered = bullet === null ? ORDERED.exec(line) : null;
    if (bullet !== null || ordered !== null) {
      const isOrdered = ordered !== null;
      const item = (bullet ?? ordered)![1];
      if (open !== null && open.t === "list" && open.ordered === isOrdered) open.items.push(item);
      else {
        close();
        open = { t: "list", ordered: isOrdered, items: [item] };
      }
      continue;
    }
    const quote = QUOTE.exec(line);
    if (quote !== null) {
      if (open !== null && open.t === "quote") open.lines.push(quote[1]);
      else {
        close();
        open = { t: "quote", lines: [quote[1]] };
      }
      continue;
    }
    // An indented line continues the last list item.
    if (open !== null && open.t === "list" && /^\s/.test(line)) {
      open.items[open.items.length - 1] += " " + line.trim();
      continue;
    }
    if (open !== null && open.t === "para") open.lines.push(line);
    else {
      close();
      open = { t: "para", lines: [line] };
    }
  }
  close();
  return out;
}

/** Same as `parseMarkdown` (name used in the batch brief: "renderMarkdown(text) → AST"). */
export const renderMarkdown = parseMarkdown;

const isSpace = (c: string | undefined) => c === undefined || /\s/.test(c);

/**
 * Index of the closing `delim` at or after `from`, skipping code spans; -1 if there is none.
 * A closer must not follow whitespace; a single `*` must not be part of `**`.
 */
function findClose(s: string, from: number, delim: "**" | "*"): number {
  for (let j = from; j < s.length; j++) {
    if (s[j] === "`") {
      const end = s.indexOf("`", j + 1);
      if (end !== -1) {
        j = end;
        continue;
      }
    }
    if (!s.startsWith(delim, j) || isSpace(s[j - 1])) continue;
    if (delim === "*" && (s[j + 1] === "*" || s[j - 1] === "*")) continue;
    return j;
  }
  return -1;
}

/** Inline spans: `code` > **strong** > *em*; anything unmatched is text. */
export function parseInline(s: string): Inline[] {
  const out: Inline[] = [];
  const text = (v: string) => {
    const last = out[out.length - 1];
    if (last !== undefined && last.t === "text") last.v += v;
    else out.push({ t: "text", v });
  };
  let i = 0;
  while (i < s.length) {
    const c = s[i];
    if (c === "`") {
      const end = s.indexOf("`", i + 1);
      if (end > i + 1) {
        out.push({ t: "code", v: s.slice(i + 1, end) });
        i = end + 1;
        continue;
      }
    } else if (s.startsWith("**", i) && !isSpace(s[i + 2])) {
      const end = findClose(s, i + 3, "**");
      if (end !== -1) {
        out.push({ t: "strong", v: parseInline(s.slice(i + 2, end)) });
        i = end + 2;
        continue;
      }
    } else if (c === "*" && !isSpace(s[i + 1]) && s[i + 1] !== "*") {
      const end = findClose(s, i + 2, "*");
      if (end !== -1) {
        out.push({ t: "em", v: parseInline(s.slice(i + 1, end)) });
        i = end + 1;
        continue;
      }
    }
    // Unmatched: the delimiter itself is text ("**" as one piece so it never opens an em).
    const step = s.startsWith("**", i) ? 2 : 1;
    text(s.slice(i, i + step));
    i += step;
  }
  return out;
}
