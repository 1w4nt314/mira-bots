import { useMemo, type ReactNode } from "react";
import { parseMarkdown, type Block, type Inline } from "../lib/markdown";

/** Inline spans as React nodes; text stays text (React escapes it), never HTML. */
function inlines(list: Inline[]): ReactNode[] {
  return list.map((n, i) => {
    switch (n.t) {
      case "text":
        return n.v;
      case "code":
        return (
          <code key={i} className="rounded bg-neutral-500/15 px-1 font-mono text-[0.95em]">
            {n.v}
          </code>
        );
      case "strong":
        return <strong key={i}>{inlines(n.v)}</strong>;
      case "em":
        return <em key={i}>{inlines(n.v)}</em>;
    }
  });
}

const HEADING_CLASS = ["text-sm font-semibold", "text-[13px] font-semibold", "font-semibold"];

function block(b: Block, i: number): ReactNode {
  switch (b.t) {
    case "heading": {
      const Tag = `h${b.level}` as "h1";
      return (
        <Tag key={i} className={HEADING_CLASS[Math.min(b.level, 3) - 1]}>
          {inlines(b.inl)}
        </Tag>
      );
    }
    case "para":
      return (
        <p key={i} className="whitespace-pre-wrap break-words">
          {inlines(b.inl)}
        </p>
      );
    case "list": {
      const items = b.items.map((it, j) => (
        <li key={j} className="break-words">
          {inlines(it)}
        </li>
      ));
      return b.ordered ? (
        <ol key={i} className="list-decimal space-y-0.5 pl-5">
          {items}
        </ol>
      ) : (
        <ul key={i} className="list-disc space-y-0.5 pl-5">
          {items}
        </ul>
      );
    }
    case "code":
      return (
        <pre
          key={i}
          className="overflow-x-auto rounded bg-neutral-500/15 p-2 font-mono text-[11px] leading-snug"
          title={b.lang ?? undefined}
        >
          <code>{b.text}</code>
        </pre>
      );
    case "quote":
      return (
        <blockquote
          key={i}
          className="whitespace-pre-wrap break-words border-l-2 border-[var(--accent)]/60 pl-2 opacity-90"
        >
          {inlines(b.inl)}
        </blockquote>
      );
  }
}

/** A report's markdown (mini dialect, `lib/markdown`) as React elements. */
export default function Markdown({ text }: { text: string }) {
  const blocks = useMemo(() => parseMarkdown(text), [text]);
  return <div className="space-y-1.5 select-text text-[11px] leading-relaxed">{blocks.map(block)}</div>;
}
