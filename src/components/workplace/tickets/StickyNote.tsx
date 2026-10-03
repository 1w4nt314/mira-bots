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
import {
  canRetryWriteBack,
  inboxLabel,
  opensIssue,
  showsWriteBack,
  writeBackBadge,
  writeBackFailureText,
} from "../../../lib/inbox";
import {
  deleteTicket,
  errorMessage,
  getTicket,
  openInboxUrl,
  retryWriteBack,
  unassignTicket,
} from "../../../lib/ipc";
import { isExited } from "../../../lib/status";
import {
  ACTOR_LABEL,
  canDelete,
  canStartPlaybook,
  canReturnWaiting,
  checksBadge,
  gitBadge,
  kindLabel,
  canDrag,
  canHandOver,
  BLOCKED_HINT,
  blockersOf,
  formatAt,
  isCoordinationTask,
  ISSUE_HINT,
  ISSUE_LABEL,
  parentOf,
  progressOf,
  shortId,
  STATE_BADGE_CLASS,
  STATE_LABEL,
  ticketDragId,
  WAITING_HINT,
} from "../../../lib/tickets";
import type { AgentInfo, TicketHistoryEntry, TicketSummary } from "../../../lib/types";
import { useStore } from "../../../state/store";
import BotFigure from "../../BotFigure";
import Markdown from "../../Markdown";
import { smallBtn, useRun, useStartPlaybook } from "./actions";
import AssignMenu from "./AssignMenu";
import ReportsSection from "./ReportsSection";
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
  const { state } = useStore();
  const showActions = interactive && !compact;
  const run = useRun();
  // Relations are looked up in the whole list (not the filtered one), see tickets.ts.
  const all = state.tickets;
  const parent = t.parentId === null ? null : parentOf(t, all);
  const progress = progressOf(t.id, all);
  const blockers = t.state === "done" ? [] : blockersOf(t, all);
  const checks = checksBadge(t);
  const branch = gitBadge(t);
  const childrenDone =
    t.state === "backlog" && t.assigneeAgentId === null && progress.total > 0 && progress.done === progress.total;
  // Step 6c: the source of a ticket started from the inbox ("GitHub #n" opens the issue).
  const source = inboxLabel(t.external);

  return (
    <div
      ref={rootRef}
      {...rootProps}
      className={`office-note rounded-lg border border-[var(--note-border)] bg-[var(--note-bg)] p-2 text-xs text-[var(--note-fg)] shadow-sm outline-none focus-visible:ring-2 focus-visible:ring-[var(--accent)] ${
        grab ? "cursor-grab touch-none active:cursor-grabbing" : ""
      } ${dimmed ? "opacity-40" : ""} ${compact && !interactive ? "w-[260px] shadow-lg" : ""}`}
    >
      <div className="flex items-center gap-1.5">
        <span className={`rounded px-1.5 text-[10px] font-medium leading-4 ${STATE_BADGE_CLASS[t.state]}`}>
          {STATE_LABEL[t.state]}
        </span>
        <span className="font-mono text-[10px] opacity-70">{t.shortId}</span>
        {typeof t.project === "string" ? (
          <span className="max-w-[40%] truncate rounded bg-neutral-500/15 px-1 text-[10px]" title={`Projekt: ${t.project}`}>
            {t.project}
          </span>
        ) : t.project !== null ? (
          <span
            className="max-w-[40%] truncate rounded bg-neutral-500/15 px-1 text-[10px]"
            title={`Nyt projekt «${t.project.new}» oprettes ved tildeling`}
          >
            +{t.project.new}
          </span>
        ) : (
          !compact &&
          (t.state === "backlog" || t.state === "rejected") && (
            <span className="text-[10px] opacity-60" title="Vælg projekt ved tildeling">
              uden projekt
            </span>
          )
        )}
        {t.kind !== null && (
          <span
            className="rounded bg-indigo-500/15 px-1 text-[10px] text-indigo-800 dark:text-indigo-200"
            title={`Type: ${kindLabel(t.kind)}`}
          >
            {kindLabel(t.kind)}
          </span>
        )}
        {t.playbookStartedAt !== null && (
          <span
            className="rounded bg-indigo-500/15 px-1 text-[10px] text-indigo-800 dark:text-indigo-200"
            title="Forløbet er startet"
          >
            forløb
          </span>
        )}
        {checks !== null && (
          <span className={`max-w-[45%] truncate rounded px-1 text-[10px] ${checks.cls}`} title={checks.title}>
            {checks.text}
          </span>
        )}
        {branch !== null && (
          <span
            className="max-w-[45%] truncate rounded bg-neutral-500/15 px-1 font-mono text-[10px]"
            title={branch.title}
          >
            {branch.text}
          </span>
        )}
        {t.parentId !== null && (
          <span
            className="rounded bg-teal-500/15 px-1 text-[10px] text-teal-800 dark:text-teal-200"
            title={parent !== null ? `Del-ticket af ${parent.title}` : "Del-ticket"}
          >
            del af {parent !== null ? parent.shortId : shortId(t.parentId)}
          </span>
        )}
        {progress.total > 0 && (
          <span className="rounded bg-teal-500/15 px-1 text-[10px] text-teal-800 dark:text-teal-200" title="Del-tickets færdige">
            {progress.done}/{progress.total} del-tickets
          </span>
        )}
        {childrenDone && (
          <span
            className="rounded bg-amber-500/15 px-1 text-[10px] text-amber-800 dark:text-amber-200"
            title="Alle del-tickets er godkendt; ticketen har ingen ejer"
          >
            del-tickets færdige
          </span>
        )}
        {blockers.length > 0 && (
          <span
            className="rounded bg-amber-500/15 px-1 text-[10px] text-amber-800 dark:text-amber-200"
            title={BLOCKED_HINT}
          >
            Venter på {blockers.map((b) => b.shortId).join(", ")}
          </span>
        )}
        {t.skipReview && (
          <span className="text-[10px] opacity-70" title="Går direkte til Done uden review">
            uden review
          </span>
        )}
        {source !== null &&
          (opensIssue(t.external) && showActions ? (
            <button
              type="button"
              onClick={() => void run(() => openInboxUrl(t.id))}
              onPointerDown={stop}
              onKeyDown={stop}
              title="Startet fra indbakken — åbn issuen i browseren"
              className="rounded bg-sky-500/15 px-1 text-[10px] text-sky-800 underline decoration-dotted hover:text-[var(--accent)] dark:text-sky-200"
            >
              {source}
            </button>
          ) : (
            <span
              className="rounded bg-sky-500/15 px-1 text-[10px] text-sky-800 dark:text-sky-200"
              title={
                t.external?.kind === "folder"
                  ? `Startet fra indbakke-mappen${t.external.path !== null ? `: ${t.external.path}` : ""}`
                  : "Startet fra indbakken"
              }
            >
              {source}
            </span>
          ))}
        {t.source === "agent" && (
          <span
            className="rounded bg-sky-500/15 px-1 text-[10px] text-sky-800 dark:text-sky-200"
            title="Oprettet af en agent via mira_create_ticket"
          >
            fra agent
          </span>
        )}
        {t.state !== "done" && isCoordinationTask(agent) && (
          <span
            className="rounded bg-amber-500/15 px-1 text-[10px] text-amber-800 dark:text-amber-200"
            title="Ticketen ligger hos en agent på en stabsplads (fx en koordinator)"
          >
            koordineringsopgave
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
      {showActions && (
        <div onPointerDown={stop} onKeyDown={stop} className="cursor-auto">
          <BodyFold ticket={t} />
        </div>
      )}

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
                    roles={agent.roles}
                    specialist={agent.specialist}
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
          {t.state === "waiting" && <div className="mt-1 text-[11px] opacity-80">{WAITING_HINT}</div>}
          {showsWriteBack(t) && <WriteBackRow ticket={t} interactive={interactive} />}
        </>
      )}

      {showActions && (
        <div onPointerDown={stop} onKeyDown={stop} className="cursor-auto">
          <NoteActions ticket={t} agent={agent} onNotice={props.onNotice} />
          {/* The review card shows the reports unfolded itself. */}
          {t.state !== "review" && <ReportsSection ticket={t} />}
          <HistoryFold ticket={t} />
        </div>
      )}
    </div>
  );
}

/**
 * Write-back status of a Done ticket from the inbox (step 6c): "meldt tilbage ✓" (title = the
 * comment's address), "melder tilbage…", "ikke meldt tilbage" with the error as a line below
 * and "Prøv igen" (`retry_write_back`; the backend's text, e.g. "Allerede meldt tilbage", goes to
 * the error line). The history notes are shown by the history fold as usual.
 */
function WriteBackRow({ ticket: t, interactive }: { ticket: TicketSummary; interactive: boolean }) {
  const run = useRun();
  const [busy, setBusy] = useState(false);
  if (t.external === null) return null;
  const badge = writeBackBadge(t.external);
  if (badge === null) return null;
  const failure = writeBackFailureText(t.external);
  const retry = async () => {
    setBusy(true);
    try {
      await run(() => retryWriteBack(t.id));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div onPointerDown={stop} onKeyDown={stop} className="mt-1 cursor-auto text-[11px]">
      <div className="flex flex-wrap items-center gap-1.5">
        <span className={`rounded px-1 text-[10px] ${badge.cls}`} title={badge.title}>
          {badge.text}
        </span>
        {interactive && canRetryWriteBack(t.external) && (
          <button
            type="button"
            onClick={() => void retry()}
            disabled={busy || t.external.writeBack.comment === "inflight"}
            title="Prøv tilbagemeldingen igen (der postes aldrig to gange)"
            className={smallBtn}
          >
            Prøv igen
          </button>
        )}
      </div>
      {failure !== null && (
        <p className="mt-0.5 break-words text-rose-700 dark:text-rose-300" role="status">
          {failure}
        </p>
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
  const startPlaybook = useStartPlaybook();
  const { state } = useStore();
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [starting, setStarting] = useState(false);
  const canStart = canStartPlaybook(t, state.tickets, state.appInfo?.playbookKinds ?? []);

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

  const start = async () => {
    setStarting(true);
    try {
      await startPlaybook(t, onNotice);
    } finally {
      setStarting(false);
    }
  };

  const buttons: ReactNode[] = [];
  if (canStart) {
    buttons.push(
      <button
        key="start"
        type="button"
        onClick={() => void start()}
        disabled={starting}
        title={`Opretter del-ticketsene for ${kindLabel(t.kind)} og giver dem til agenter med den rette rolle`}
        className={`${smallBtn} font-medium`}
      >
        {starting ? "Starter…" : "Start forløb"}
      </button>,
    );
  }
  if (canDrag(t) || canHandOver(t)) buttons.push(<AssignMenu key="assign" ticket={t} onNotice={onNotice} />);
  if (canReturnWaiting(t)) {
    // Review 6a N3: the user frees a waiting parent (its children stay where they are).
    buttons.push(
      <button
        key="return"
        type="button"
        onClick={() => void run(() => unassignTicket(t.id))}
        title="Tag forælderen fra agenten og læg den i Backlog; del-tickets bliver, hvor de er"
        className={smallBtn}
      >
        Læg tilbage
      </button>,
    );
  }
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

function historyLine(h: TicketHistoryEntry): string {
  const move = h.from === null ? `oprettet i ${STATE_LABEL[h.to]}` : `${STATE_LABEL[h.from]} → ${STATE_LABEL[h.to]}`;
  return `${move} (${ACTOR_LABEL[h.by]})${h.note ? ` — ${h.note}` : ""}`;
}

/** The body is not part of `tickets-changed` either: fetched with `getTicket` while unfolded and
 * shown as markdown (same `<details>` pattern as `HistoryFold`). */
function BodyFold({ ticket: t }: { ticket: TicketSummary }) {
  const [open, setOpen] = useState(false);
  const [body, setBody] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!open) return;
    let alive = true;
    getTicket(t.id)
      .then((full) => {
        if (!alive) return;
        setBody(full.body);
        setError(null);
      })
      .catch((e: unknown) => {
        if (alive) setError(errorMessage(e));
      });
    return () => {
      alive = false;
    };
    // Refetch when the ticket changes while unfolded.
  }, [open, t.id, t.updatedAt]);

  return (
    <details className="mt-1" onToggle={(e) => setOpen(e.currentTarget.open)}>
      <summary className="cursor-pointer select-none text-[11px] opacity-70 hover:opacity-100">
        Beskrivelse
      </summary>
      {error !== null && (
        <p className="mt-1 text-[11px] text-rose-600 dark:text-rose-300" role="alert">
          {error}
        </p>
      )}
      {body === null
        ? error === null && <p className="mt-1 text-[11px] opacity-70">Henter…</p>
        : (
            <div className="mt-1 max-h-[240px] overflow-y-auto pr-1">
              {body.trim() === "" ? (
                <p className="text-[11px] opacity-70">(ingen beskrivelse)</p>
              ) : (
                <Markdown text={body} />
              )}
            </div>
          )}
    </details>
  );
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
