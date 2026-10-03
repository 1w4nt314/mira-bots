import { useDraggable } from "@dnd-kit/core";
import { useCallback, type HTMLAttributes, type Ref, type SyntheticEvent } from "react";
import { openInboxUrl } from "../../../lib/ipc";
import {
  canDragInbox,
  duplicateText,
  inboxDragId,
  sourceBadge,
  visibleLabels,
} from "../../../lib/inbox";
import type { InboxItemSummary } from "../../../lib/types";
import { smallBtn, useRun } from "./actions";

interface Props {
  item: InboxItemSummary;
  /** Compact look (no buttons), e.g. as a drag overlay (step 6c B5). */
  compact?: boolean;
  /** "Start…": opens the Start dialog. Only from a click. */
  onStart?: (item: InboxItemSummary) => void;
  /** "Afvis". */
  onDismiss?: (item: InboxItemSummary) => void;
  /** "Fortryd" on a dismissed item (the "Afviste" fold); replaces "Start…" and "Afvis". */
  onUndismiss?: (item: InboxItemSummary) => void;
  busy?: boolean;
  /** May be dragged onto a seat (`inbox:<id>`, only new items); the drop opens the Start dialog. */
  draggable?: boolean;
}

/** Stops the card's drag sensors for events inside its buttons. */
const stop = (e: SyntheticEvent) => e.stopPropagation();

/**
 * A new item may be dragged onto a seat (step 6c B5): the drop never starts anything itself, it
 * opens the Start dialog with the seat as target (Workplace's `onDragEnd`).
 */
export default function InboxCard(props: Props) {
  if (props.draggable !== true || props.compact === true) return <CardFrame {...props} />;
  return <DraggableCard {...props} />;
}

function DraggableCard(props: Props) {
  const { item, busy = false } = props;
  const enabled = canDragInbox(item) && !busy;
  const { attributes, listeners, setNodeRef, setActivatorNodeRef, isDragging } = useDraggable({
    id: inboxDragId(item.id),
    data: { item },
    disabled: !enabled,
    attributes: { roleDescription: "emne fra indbakken" },
  });
  // Root = node and activator, like StickyNote: a key press on a button never starts a drag.
  const ref = useCallback(
    (el: HTMLDivElement | null) => {
      setNodeRef(el);
      setActivatorNodeRef(el);
    },
    [setNodeRef, setActivatorNodeRef],
  );
  const rootProps: HTMLAttributes<HTMLDivElement> = enabled
    ? {
        ...attributes,
        ...listeners,
        "aria-label": `Emne fra indbakken: ${item.title}. Træk til en plads, eller brug Start.`,
      }
    : {};
  return <CardFrame {...props} rootRef={ref} rootProps={rootProps} dimmed={isDragging} grab={enabled} />;
}

interface FrameProps extends Props {
  rootRef?: Ref<HTMLDivElement>;
  rootProps?: HTMLAttributes<HTMLDivElement>;
  dimmed?: boolean;
  grab?: boolean;
}

/**
 * One inbox item: a source badge ("GitHub"/"fil"), the title, labels (at most 5 and "+k"),
 * "#n" as a button that opens the issue in the browser, the project (or "vælg projekt"), the
 * backend's notes and the buttons "Start…" and "Afvis". Nothing here starts a ticket by itself.
 */
function CardFrame(props: FrameProps) {
  const { item, compact = false, onStart, onDismiss, onUndismiss, busy = false } = props;
  const { rootRef, rootProps, dimmed, grab } = props;
  const run = useRun();
  const labels = visibleLabels(item.labels);
  const showButtons = !compact;
  return (
    <div
      ref={rootRef}
      {...rootProps}
      className={`office-note rounded-lg border border-[var(--note-border)] bg-[var(--note-bg)] p-2 text-xs text-[var(--note-fg)] shadow-sm outline-none focus-visible:ring-2 focus-visible:ring-[var(--accent)] ${
        compact ? "w-[260px] shadow-lg" : ""
      } ${grab ? "cursor-grab touch-none active:cursor-grabbing" : ""} ${dimmed ? "opacity-40" : ""}`}
      data-inbox-id={item.id}
    >
      <div className="flex items-center gap-1.5">
        <span
          className="rounded bg-neutral-500/15 px-1.5 text-[10px] font-medium leading-4"
          title={item.kind === "github" ? "GitHub-issue" : "Fil i en indbakke-mappe"}
        >
          {sourceBadge(item.kind)}
        </span>
        {item.kind === "github" && item.number !== null && (
          <button
            type="button"
            onClick={() => void run(() => openInboxUrl(item.id))}
            onPointerDown={stop}
            onKeyDown={stop}
            disabled={!showButtons}
            title="Åbn issuen i browseren"
            className="font-mono text-[10px] underline decoration-dotted hover:text-[var(--accent)] disabled:no-underline"
          >
            #{item.number}
          </button>
        )}
        {item.project !== null ? (
          <span className="max-w-[40%] truncate rounded bg-neutral-500/15 px-1 text-[10px]" title={`Projekt: ${item.project}`}>
            {item.project}
          </span>
        ) : (
          <span
            className="text-[10px] opacity-60"
            title={
              item.candidates.length > 0
                ? `Kan høre til: ${item.candidates.join(", ")}`
                : "Vælg projekt ved Start"
            }
          >
            vælg projekt
          </span>
        )}
        {item.ticketKind !== null && (
          <span className="rounded bg-indigo-500/15 px-1 text-[10px] text-indigo-800 dark:text-indigo-200" title="Type fra filen">
            {item.ticketKind}
          </span>
        )}
        {item.notes.length > 0 && (
          <span
            className="ml-auto text-[10px] text-amber-700 dark:text-amber-300"
            title={item.notes.join("\n")}
            aria-label={`${item.notes.length} note(r): ${item.notes.join(". ")}`}
          >
            ⚠
          </span>
        )}
      </div>
      <div className="mt-1 line-clamp-2 break-words font-medium" title={item.title}>
        {item.title}
      </div>
      {item.labels.length > 0 && (
        <div className="mt-1 flex flex-wrap gap-1">
          {labels.shown.map((l) => (
            <span key={l} className="max-w-[45%] truncate rounded bg-neutral-500/15 px-1 text-[10px]" title={l}>
              {l}
            </span>
          ))}
          {labels.extra > 0 && (
            <span className="rounded bg-neutral-500/15 px-1 text-[10px]" title={item.labels.slice(labels.shown.length).join(", ")}>
              +{labels.extra}
            </span>
          )}
        </div>
      )}
      {item.duplicateOf !== null && !compact && (
        <p className="mt-1 text-[10px] text-amber-700 dark:text-amber-300">
          {duplicateText(item.duplicateOf)}
        </p>
      )}
      {showButtons && (
        <div onPointerDown={stop} onKeyDown={stop} className="mt-1.5 flex cursor-auto flex-wrap items-center gap-1.5">
          {onUndismiss !== undefined ? (
            <button
              type="button"
              onClick={() => onUndismiss(item)}
              disabled={busy}
              title="Læg emnet tilbage i indbakken"
              className={smallBtn}
            >
              Fortryd
            </button>
          ) : (
            <>
              <button
                type="button"
                onClick={() => onStart?.(item)}
                disabled={busy || onStart === undefined}
                title="Opret en ticket i Backlog ud fra emnet"
                className={smallBtn}
              >
                Start…
              </button>
              <button
                type="button"
                onClick={() => onDismiss?.(item)}
                disabled={busy || onDismiss === undefined}
                title="Skjul emnet i indbakken (det startes ikke)"
                className={smallBtn}
              >
                Afvis
              </button>
            </>
          )}
        </div>
      )}
    </div>
  );
}
