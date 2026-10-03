import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { useBotStates, useTheme } from "../lib/bots";
import { errorMessage, markNoticesSeen, openWorkplace, quitApp, resizeIsland, setWatch } from "../lib/ipc";
import { DOT_CLASS, worstStatus } from "../lib/status";
import { reviewCount } from "../lib/tickets";
import type { WorkplaceTab } from "../lib/types";
import {
  newestWithTicket,
  showResumeWatch,
  showStopWatch,
  unreadText,
  visibleNotices,
  watchChipText,
} from "../lib/watch";
import { useStore } from "../state/store";
import AgentChip from "./AgentChip";
import NewAgentButton from "./NewAgentButton";
import PermissionCard from "./PermissionCard";

/** Must match `island::COLLAPSED` in Rust. */
const COLLAPSED_SIZE = { width: 240, height: 8 } as const;
const MAX_WIDTH = 960;
const COLLAPSE_DELAY_MS = 400;
const MAX_VISIBLE_CARDS = 2;
/** Extra width while the "n i review" chip is shown. */
const REVIEW_CHIP_WIDTH = 90;
/** Extra width for the step 6d chips: "Vagt: n projekter" / "Vagt sat på pause", "n beskeder"
 *  and "Stop vagten" / "Genoptag". */
const WATCH_CHIP_WIDTH = 110;
const NOTICE_CHIP_WIDTH = 100;
const STOP_WIDTH = 90;

export default function Island() {
  const { state, dispatch } = useStore();
  const { agents, pending, expanded, error, tickets, watch, notices } = state;
  const reviews = reviewCount(tickets);
  // Step 6d: the watch chip, the unread notices and the master switch. Everything comes from the
  // store's events (`watch-changed`, `notices-changed`); the island polls nothing.
  const watchChip = watchChipText(watch);
  const unseen = visibleNotices(notices?.items ?? [], tickets, agents).filter((n) => !n.seen);
  const noticeChip = unreadText(unseen.length);
  const stopWatch = showStopWatch(watch);
  const resumeWatch = showResumeWatch(watch);

  // Open by hover or by an open permission request (reviews do not hold it open).
  const open = expanded || pending.length > 0;
  const width = Math.min(
    MAX_WIDTH,
    400 +
      160 * agents.length +
      (reviews > 0 ? REVIEW_CHIP_WIDTH : 0) +
      (watchChip !== null || resumeWatch ? WATCH_CHIP_WIDTH : 0) +
      (noticeChip !== "" ? NOTICE_CHIP_WIDTH : 0) +
      (stopWatch || resumeWatch ? STOP_WIDTH : 0),
  );
  const theme = useTheme();
  const botStates = useBotStates(agents);

  const contentRef = useRef<HTMLDivElement>(null);
  const lastSent = useRef("");
  const leaveTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const [confirmQuit, setConfirmQuit] = useState(false);

  const send = useCallback(
    (w: number, h: number) => {
      const key = `${w}x${h}`;
      if (key === lastSent.current) return;
      lastSent.current = key;
      resizeIsland(w, h).catch((e: unknown) => {
        lastSent.current = "";
        dispatch({ type: "error/set", error: errorMessage(e) });
      });
    },
    [dispatch],
  );

  // Keep the native window as large as the content (collapsed: the 240x8 strip).
  useLayoutEffect(() => {
    if (!open) {
      send(COLLAPSED_SIZE.width, COLLAPSED_SIZE.height);
      return;
    }
    const el = contentRef.current;
    if (el === null) return;
    const apply = () => send(width, Math.ceil(el.getBoundingClientRect().height));
    apply();
    const ro = new ResizeObserver(apply);
    ro.observe(el);
    return () => ro.disconnect();
  }, [open, width, send]);

  useEffect(
    () => () => {
      if (leaveTimer.current !== null) clearTimeout(leaveTimer.current);
    },
    [],
  );

  // The quit confirmation falls back after a moment.
  useEffect(() => {
    if (!confirmQuit) return;
    const t = setTimeout(() => setConfirmQuit(false), 3000);
    return () => clearTimeout(t);
  }, [confirmQuit]);

  const onEnter = () => {
    if (leaveTimer.current !== null) clearTimeout(leaveTimer.current);
    leaveTimer.current = null;
    dispatch({ type: "ui/expand" });
  };
  // Pending requests keep the island open (`open`) regardless of `expanded`.
  const onLeave = () => {
    if (leaveTimer.current !== null) clearTimeout(leaveTimer.current);
    leaveTimer.current = setTimeout(() => dispatch({ type: "ui/collapse" }), COLLAPSE_DELAY_MS);
  };

  const workplace = async (tab: WorkplaceTab | null = null, ticketId: string | null = null) => {
    try {
      await openWorkplace(null, tab, null, ticketId);
    } catch (e) {
      dispatch({ type: "error/set", error: errorMessage(e) });
    }
  };

  // The notice chip: Workplace on the newest unread notice's ticket. Was the ticket deleted
  // between render and click (`open_workplace` refuses an unknown ticket, review6d N16), the
  // notice is marked read and Workplace opens on the Tickets tab without a selection.
  const openNotice = async () => {
    const n = newestWithTicket(unseen);
    if (n === null || n.ticketId === null) {
      await workplace("tickets");
      return;
    }
    try {
      await openWorkplace(null, "tickets", null, n.ticketId);
      return;
    } catch {
      // Fall through: the ticket is gone.
    }
    try {
      dispatch({ type: "notices/set", notices: await markNoticesSeen([n.id]) });
    } catch {
      // Marking read is a courtesy; opening Workplace matters more.
    }
    await workplace("tickets");
  };

  // "Stop vagten" / "Genoptag": the master pause in the app's own settings (never project.json).
  const toggleWatch = async (on: boolean) => {
    try {
      dispatch({ type: "watch/set", watch: await setWatch(null, on) });
    } catch (e) {
      dispatch({ type: "error/set", error: errorMessage(e) });
    }
  };

  const quit = async () => {
    if (!confirmQuit) {
      setConfirmQuit(true);
      return;
    }
    try {
      await quitApp();
    } catch (e) {
      dispatch({ type: "error/set", error: errorMessage(e) });
    }
  };

  if (!open) {
    const worst = worstStatus(agents);
    return (
      <div className="h-screen w-screen" onMouseEnter={onEnter}>
        {worst !== null && (
          <div className={`h-1 w-full rounded-b-sm ${DOT_CLASS[worst]}`} aria-hidden="true" />
        )}
      </div>
    );
  }

  const shown = pending.slice(0, MAX_VISIBLE_CARDS);
  const hidden = pending.length - shown.length;

  return (
    <div className="h-screen w-screen" onMouseEnter={onEnter} onMouseLeave={onLeave}>
      <div
        ref={contentRef}
        style={{ width }}
        className="rounded-b-2xl bg-neutral-900/90 text-xs text-neutral-100 shadow-lg"
      >
        <div className="flex h-14 items-center gap-2 px-3">
          <div className="flex min-w-0 flex-1 items-center gap-2">
            {agents.length === 0 ? (
              <span className="text-neutral-400">Ingen agenter endnu</span>
            ) : (
              agents.map((a) => (
                <AgentChip
                  key={a.id}
                  agent={a}
                  theme={theme}
                  botState={botStates.get(a.id) ?? "idle"}
                />
              ))
            )}
          </div>
          {reviews > 0 && (
            <button
              type="button"
              onClick={() => void workplace("tickets")}
              title="Åbn Tickets i Workplace og se arbejdet der venter på dig"
              className="shrink-0 rounded-full bg-amber-400/25 px-2.5 py-1 text-[11px] font-medium text-amber-200 hover:bg-amber-400/35"
            >
              {reviews} i review
            </button>
          )}
          {resumeWatch ? (
            <>
              <button
                type="button"
                onClick={() => void workplace("diagnostics")}
                title="Vagten er sat på pause for alle projekter — se Diagnostik → Projekter"
                className="shrink-0 rounded-full bg-white/10 px-2.5 py-1 text-[11px] font-medium text-neutral-300 hover:bg-white/20"
              >
                Vagt sat på pause
              </button>
              <button
                type="button"
                onClick={() => void toggleWatch(true)}
                title="Start vagten igen for de projekter der tillader den"
                className="shrink-0 rounded-md border border-emerald-400/40 px-2 py-1 text-[11px] text-emerald-200 hover:bg-emerald-400/20"
              >
                Genoptag
              </button>
            </>
          ) : (
            watchChip !== null && (
              <button
                type="button"
                onClick={() => void workplace("diagnostics")}
                title="Vagten holder øje med indbakken — se projekter, budget og status under Diagnostik"
                className="shrink-0 rounded-full bg-sky-400/20 px-2.5 py-1 text-[11px] font-medium text-sky-200 hover:bg-sky-400/30"
              >
                {watchChip}
              </button>
            )
          )}
          {noticeChip !== "" && (
            <button
              type="button"
              onClick={() => void openNotice()}
              title="Åbn beskederne i Workplace (eskaleringer, forløb til godkendelse, vagten)"
              className="shrink-0 rounded-full bg-amber-400/25 px-2.5 py-1 text-[11px] font-medium text-amber-200 hover:bg-amber-400/35"
            >
              {noticeChip}
            </button>
          )}
          {stopWatch && (
            <button
              type="button"
              onClick={() => void toggleWatch(false)}
              title="Sæt vagten på pause for alle projekter (gemmes i appens indstillinger, ikke i project.json)"
              className="shrink-0 rounded-md border border-rose-400/40 px-2 py-1 text-[11px] text-rose-200 hover:bg-rose-400/20"
            >
              Stop vagten
            </button>
          )}
          <button
            type="button"
            onClick={() => void workplace()}
            title="Åbn Workplace (pladser, terminaler, tilladelser og diagnostik)"
            className="shrink-0 rounded-md bg-white/10 px-2.5 py-1 text-[11px] font-medium hover:bg-white/20"
          >
            Workplace
          </button>
          <NewAgentButton />
          <button
            type="button"
            onClick={() => void quit()}
            title="Afslut mira-bots (stopper alle agenter)"
            className="shrink-0 rounded-md px-2 py-1 text-[11px] text-neutral-400 hover:bg-white/10 hover:text-white"
          >
            {confirmQuit ? "Sikker?" : "Afslut"}
          </button>
        </div>
        {shown.map((r) => (
          <PermissionCard key={r.requestId} request={r} />
        ))}
        {hidden > 0 && (
          <div className="px-3 pb-2 text-[11px] text-neutral-400">
            + {hidden} {hidden === 1 ? "anmodning" : "anmodninger"} venter
          </div>
        )}
        {error !== null && <div className="px-3 pb-2 text-[11px] text-rose-400">{error}</div>}
      </div>
    </div>
  );
}
