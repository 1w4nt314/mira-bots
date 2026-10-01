import { useDraggable } from "@dnd-kit/core";
import {
  useCallback,
  useEffect,
  useState,
  type HTMLAttributes,
  type ReactNode,
  type Ref,
  type SyntheticEvent,
} from "react";
import { useTheme } from "../../../lib/bots";
import { deleteTicket, errorMessage, getTicket } from "../../../lib/ipc";
import { isExited } from "../../../lib/status";
import {
  ACTOR_LABEL,
  canDelete,
  canDrag,
  ISSUE_HINT,
  ISSUE_LABEL,
  STATE_BADGE_CLASS,
  STATE_LABEL,
  ticketDragId,
} from "../../../lib/tickets";
import type { AgentInfo, TicketHistoryEntry, TicketSummary } from "../../../lib/types";
import BotFigure from "../../BotFigure";
import { smallBtn, useRun } from "./actions";
import AssignMenu from "./AssignMenu";
import ReviewActions from "./ReviewActions";

interface Props {
  ticket: TicketSummary;
  /** The assignee, if it still exists. */
  agent: AgentInfo | null;
  /** May be dragged onto a seat (only used when `interactive`). */
  draggable: boolean;
  /** Title and badges only (drag overlay, spawn dialog). */
  compact?: boolean;
  /** False: a static picture (no drag hook, no buttons) for DragOverlay and SpawnDialog. */
  interactive?: boolean;
  /** Short confirmation shown by the panel (e.g. after a rejection). */
  onNotice?: (text: string) => void;
}

/** Stops the note's drag sensors for events inside its buttons, menus and text fields. */
const stop = (e: SyntheticEvent) => e.stopPropagation();

/**
 * A ticket as a yellow note. Interactive notes register with dnd-kit (`ticket:<id>`); the
 * sensors' listeners sit on the note's root, and every control inside stops pointer/key events
 * so a click on "Tildel…" never starts a drag. Never render inside a `data-tauri-drag-region`.
 */
export default function StickyNote(props: Props) {
  if (props.interactive === false) return <NoteFrame {...props} />;
  return <DraggableNote {...props} />;
}

function DraggableNote(props: Props) {
  const { ticket, draggable } = props;
  const enabled = draggable && canDrag(ticket);
  const { attributes, listeners, setNodeRef, setActivatorNodeRef, isDragging } = useDraggable({
    id: ticketDragId(ticket.id),
    data: { ticket },
    disabled: !enabled,
    attributes: { roleDescription: "ticket" },
  });
  // The root is both the node and the activator: keyboard drags then only start when the note
  // itself has focus, not when Enter/Space is pressed on a button inside it.
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
        "aria-label": `Ticket ${ticket.shortId}: ${ticket.title}. Træk til en plads, eller brug Tildel.`,
      }
    : {};
  return <NoteFrame {...props} rootRef={ref} rootProps={rootProps} dimmed={isDragging} grab={enabled} />;
}

interface FrameProps extends Props {
  rootRef?: Ref<HTMLDivElement>;
  rootProps?: HTMLAttributes<HTMLDivElement>;
  dimmed?: boolean;
  grab?: boolean;
}

function NoteFrame(props: FrameProps) {
  const { ticket: t, agent, compact = false, interactive = true, rootRef, rootProps, dimmed, grab } =
    props;
  const theme = useTheme();
  const showActions = interactive && !compact;

  return (
    <div
      ref={rootRef}
      {...rootProps}
      className={`rounded-lg border border-[var(--note-border)] bg-[var(--note-bg)] p-2 text-xs text-[var(--note-fg)] shadow-sm outline-none focus-visible:ring-2 focus-visible:ring-[var(--accent)] ${
        grab ? "cursor-grab touch-none active:cursor-grabbing" : ""
      } ${dimmed ? "opacity-40" : ""} ${compact && !interactive ? "w-[260px] shadow-lg" : ""}`}
    >
      <div className="flex items-center gap-1.5">
        <span className={`rounded px-1.5 text-[10px] font-medium leading-4 ${STATE_BADGE_CLASS[t.state]}`}>
          {STATE_LABEL[t.state]}
        </span>
        <span className="font-mono text-[10px] opacity-70">{t.shortId}</span>
        {t.skipReview && (
          <span className="text-[10px] opacity-70" title="Går direkte til Done uden review">
            uden review
          </span>
        )}
        {t.source === "agent" && (
          <span
            className="rounded bg-sky-500/15 px-1 text-[10px] text-sky-800 dark:text-sky-200"
            title="Oprettet af en agent via mira_create_ticket"
          >
            fra agent
          </span>
        )}
        {t.rejectionNote !== null && (
          <span
            className="ml-auto rounded bg-rose-500/15 px-1 text-[10px] text-rose-700 dark:text-rose-300"
            title={`Afvist med noten: ${t.rejectionNote}`}
          >
            afvist før
          </span>
        )}
      </div>
      <div className="mt-1 line-clamp-2 break-words font-medium" title={t.title}>
        {t.title}
      </div>

      {!compact && (
        <>
          {t.issue !== null && (
            <div
              className="mt-1 rounded border border-amber-500/50 bg-amber-300/30 px-1.5 py-0.5 text-[11px] text-amber-900 dark:text-amber-100"
              role="status"
            >
              ⚠ {ISSUE_LABEL[t.issue]} — {ISSUE_HINT[t.issue]}
            </div>
          )}
          {t.assigneeAgentId !== null && (
            <div className="mt-1 flex items-center gap-1.5 text-[11px]">
              {agent !== null ? (
                <>
                  <BotFigure
                    role={agent.role}
                    state="idle"
                    theme={theme}
                    exited={isExited(agent)}
                    size={18}
                    badge={false}
                  />
                  <span className="truncate">{agent.name}</span>
                </>
              ) : (
                <span className="opacity-70">agenten findes ikke længere</span>
              )}
              {t.state === "assigned" && t.queuePosition !== null && (
                <span className="ml-auto opacity-70">#{t.queuePosition + 1} i kø</span>
              )}
            </div>
          )}
        </>
      )}

      {showActions && (
        <div onPointerDown={stop} onKeyDown={stop} className="cursor-auto">
          <NoteActions ticket={t} agent={agent} onNotice={props.onNotice} />
          <HistoryFold ticket={t} />
        </div>
      )}
    </div>
  );
}

function NoteActions({
  ticket: t,
  agent,
  onNotice,
}: {
  ticket: TicketSummary;
  agent: AgentInfo | null;
  onNotice?: (text: string) => void;
}) {
  const run = useRun();
  const [confirmDelete, setConfirmDelete] = useState(false);

  useEffect(() => {
    if (!confirmDelete) return;
    const timer = setTimeout(() => setConfirmDelete(false), 3000);
    return () => clearTimeout(timer);
  }, [confirmDelete]);

  const remove = () => {
    if (!confirmDelete) {
      setConfirmDelete(true);
      return;
    }
    setConfirmDelete(false);
    void run(() => deleteTicket(t.id));
  };

  const buttons: ReactNode[] = [];
  if (canDrag(t)) buttons.push(<AssignMenu key="assign" ticket={t} />);
  if (canDelete(t)) {
    buttons.push(
      <button
        key="delete"
        type="button"
        onClick={remove}
        title="Slet ticketen (klik igen for at bekræfte)"
        className={`${smallBtn} ${confirmDelete ? "border-rose-500 text-rose-600 dark:text-rose-300" : ""}`}
      >
        {confirmDelete ? "Sikker?" : "Slet"}
      </button>,
    );
  }

  return (
    <>
      {t.state === "review" && <ReviewActions ticket={t} agent={agent} onNotice={onNotice} />}
      {buttons.length > 0 && <div className="mt-1.5 flex flex-wrap items-center gap-1.5">{buttons}</div>}
    </>
  );
}

function formatAt(ms: number): string {
  return new Date(ms).toLocaleString("da-DK", {
    day: "2-digit",
    month: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

function historyLine(h: TicketHistoryEntry): string {
  const move = h.from === null ? `oprettet i ${STATE_LABEL[h.to]}` : `${STATE_LABEL[h.from]} → ${STATE_LABEL[h.to]}`;
  return `${move} (${ACTOR_LABEL[h.by]})${h.note ? ` — ${h.note}` : ""}`;
}

/** History is not part of `tickets-changed`; it is fetched with `getTicket` while unfolded. */
function HistoryFold({ ticket: t }: { ticket: TicketSummary }) {
  const [open, setOpen] = useState(false);
  const [history, setHistory] = useState<TicketHistoryEntry[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    let alive = true;
    getTicket(t.id)
      .then((full) => {
        if (!alive) return;
        setHistory(full.history);
        setError(null);
      })
      .catch((e: unknown) => {
        if (alive) setError(errorMessage(e));
      });
    return () => {
      alive = false;
    };
    // Refetch when the ticket changes while unfolded.
  }, [open, t.id, t.historyLen, t.updatedAt]);

  return (
    <details className="mt-1.5" onToggle={(e) => setOpen(e.currentTarget.open)}>
      <summary className="cursor-pointer select-none text-[11px] opacity-70 hover:opacity-100">
        Historik ({t.historyLen})
      </summary>
      {error !== null && (
        <p className="mt-1 text-[11px] text-rose-600 dark:text-rose-300" role="alert">
          {error}
        </p>
      )}
      {history === null
        ? error === null && <p className="mt-1 text-[11px] opacity-70">Henter…</p>
        : (
            <ol className="mt-1 space-y-0.5 text-[11px]">
              {[...history].reverse().map((h, i) => (
                <li key={`${h.at}-${i}`} className="break-words">
                  <span className="font-mono opacity-70">{formatAt(h.at)}</span> {historyLine(h)}
                </li>
              ))}
            </ol>
          )}
    </details>
  );
}
