import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { errorMessage, quitApp, resizeIsland } from "../lib/ipc";
import { DOT_CLASS, worstStatus } from "../lib/status";
import { useStore } from "../state/store";
import AgentChip from "./AgentChip";
import NewAgentButton from "./NewAgentButton";
import PermissionCard from "./PermissionCard";

/** Must match `island::COLLAPSED` in Rust. */
const COLLAPSED_SIZE = { width: 240, height: 8 } as const;
const MAX_WIDTH = 960;
const COLLAPSE_DELAY_MS = 400;
const MAX_VISIBLE_CARDS = 2;

export default function Island() {
  const { state, dispatch } = useStore();
  const { agents, pending, expanded, pinned, error } = state;

  // Open by hover, by an open permission request or by a pin (folder dialog).
  const open = expanded || pinned || pending.length > 0;
  const width = Math.min(MAX_WIDTH, 320 + 160 * agents.length);

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
  // Pinned or pending requests keep the island open (`open`) regardless of `expanded`.
  const onLeave = () => {
    if (leaveTimer.current !== null) clearTimeout(leaveTimer.current);
    leaveTimer.current = setTimeout(() => dispatch({ type: "ui/collapse" }), COLLAPSE_DELAY_MS);
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
              agents.map((a) => <AgentChip key={a.id} agent={a} />)
            )}
          </div>
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
