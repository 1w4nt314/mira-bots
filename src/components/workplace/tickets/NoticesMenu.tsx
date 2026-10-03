import { useCallback, useEffect, useRef, useState } from "react";
import { markNoticesSeen } from "../../../lib/ipc";
import { formatAt, relativeText } from "../../../lib/tickets";
import type { Notice } from "../../../lib/types";
import { NOTICE_KIND_LABEL, visibleNotices } from "../../../lib/watch";
import { useStore } from "../../../state/store";
import { useRun, useTicketActions } from "./actions";

/**
 * Opening the list marks the unread notices it shows read after this pause, so the badge is seen
 * going away. Only those ids (review6d N15): notices hidden by `visibleNotices` or arriving while
 * the list is open stay unread.
 */
const MARK_ALL_AFTER_MS = 1000;

/**
 * "Beskeder (n)" in the workplace header (step 6d): the notice queue, newest first, without the
 * ones whose cause is gone (`visibleNotices`). A click marks the notice read and shows its ticket
 * (Tickets tab) or its agent (terminal). The badge counts unread visible notices; it is reset when
 * this list opens, not when the workplace opens.
 */
export default function NoticesMenu() {
  const { state, dispatch } = useStore();
  const { selectTicket, selectAgent } = useTicketActions();
  const run = useRun();
  const [open, setOpen] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);
  const items = visibleNotices(state.notices?.items ?? [], state.tickets, state.agents);
  const unreadIds = items.filter((n) => !n.seen).map((n) => n.id);
  const unread = unreadIds.length;
  const unreadIdsRef = useRef(unreadIds);
  unreadIdsRef.current = unreadIds;
  const now = Date.now();

  /** Marks exactly `ids` read (never `null` = every notice, review6d N15). */
  const markSeen = useCallback(
    (ids: string[]) =>
      run(async () => {
        if (ids.length === 0) return;
        dispatch({ type: "notices/set", notices: await markNoticesSeen(ids) });
      }),
    [run, dispatch],
  );

  useEffect(() => {
    if (!open) return;
    // The unread notices shown when the list opened.
    const shown = unreadIdsRef.current;
    if (shown.length === 0) return;
    const t = setTimeout(() => void markSeen(shown), MARK_ALL_AFTER_MS);
    return () => clearTimeout(t);
  }, [open, markSeen]);

  // A click outside or Escape closes the list.
  useEffect(() => {
    if (!open) return;
    const onDown = (e: PointerEvent) => {
      const root = rootRef.current;
      if (root !== null && e.target instanceof Node && !root.contains(e.target)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("pointerdown", onDown);
    window.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("pointerdown", onDown);
      window.removeEventListener("keydown", onKey);
    };
  }, [open]);

  const pick = (n: Notice) => {
    setOpen(false);
    void run(async () => {
      dispatch({ type: "notices/set", notices: await markNoticesSeen([n.id]) });
    });
    if (n.ticketId !== null) selectTicket(n.ticketId);
    else if (n.agentId !== null) selectAgent(n.agentId);
  };

  const targetTitle = (n: Notice) =>
    n.ticketId !== null ? "Vis ticketen" : n.agentId !== null ? "Vis agentens terminal" : "Markér som læst";

  return (
    <div ref={rootRef} className="relative shrink-0">
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        aria-expanded={open}
        aria-haspopup="dialog"
        title={
          unread > 0
            ? `${unread} ${unread === 1 ? "ulæst besked" : "ulæste beskeder"}`
            : "Beskeder fra appen: eskaleringer, forløb til godkendelse, ventende agenter og vagten"
        }
        className={`rounded-md border px-2 py-0.5 text-[11px] hover:border-[var(--accent)] ${
          unread > 0
            ? "border-amber-400/60 bg-amber-300/20 font-medium text-amber-800 dark:text-amber-200"
            : "border-[var(--border)] text-[var(--muted)]"
        }`}
      >
        Beskeder ({unread})
      </button>
      {open && (
        <div
          role="dialog"
          aria-label="Beskeder"
          className="absolute right-0 top-full z-20 mt-1 max-h-[60vh] w-[360px] overflow-y-auto rounded-lg border border-[var(--border)] bg-[var(--panel)] p-2 text-xs shadow-lg"
        >
          <div className="mb-1 flex items-center gap-2">
            <span className="font-semibold">Beskeder</span>
            <button
              type="button"
              onClick={() => void markSeen(unreadIds)}
              disabled={unread === 0}
              className="ml-auto rounded-md border border-[var(--border)] px-2 py-0.5 text-[11px] hover:border-[var(--accent)] disabled:cursor-not-allowed disabled:opacity-50"
            >
              Markér alle som læst
            </button>
          </div>
          {items.length === 0 ? (
            <p className="py-2 text-[var(--muted)]">Ingen beskeder</p>
          ) : (
            <ul className="space-y-1">
              {items.map((n) => (
                <li key={n.id}>
                  <button
                    type="button"
                    onClick={() => pick(n)}
                    title={targetTitle(n)}
                    className={`w-full rounded-md border px-2 py-1 text-left hover:border-[var(--accent)] ${
                      n.seen ? "border-transparent" : "border-amber-400/40 bg-amber-300/10"
                    }`}
                  >
                    <div className="flex items-baseline gap-2">
                      <span className="shrink-0 rounded bg-neutral-500/15 px-1 text-[10px]">
                        {NOTICE_KIND_LABEL[n.kind] ?? n.kind}
                      </span>
                      <span className={`min-w-0 truncate ${n.seen ? "" : "font-semibold"}`}>{n.title}</span>
                      <span className="ml-auto shrink-0 text-[10px] text-[var(--muted)]" title={formatAt(n.at)}>
                        {relativeText(n.at, now)}
                      </span>
                    </div>
                    <div className="mt-0.5 break-words text-[var(--muted)]">{n.text}</div>
                  </button>
                </li>
              ))}
            </ul>
          )}
        </div>
      )}
    </div>
  );
}
