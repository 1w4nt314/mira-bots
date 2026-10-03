import { useCallback, useEffect, useRef } from "react";
import { pollReason, POLL_INTERVAL_MS, type PollTrigger } from "../../../lib/inbox";
import { errorMessage, refreshInbox } from "../../../lib/ipc";
import { useStore } from "../../../state/store";

/** A failing automatic fetch reaches the error line at most this often (no noise). */
const ERROR_EVERY_MS = 10 * 60_000;

/**
 * The inbox polling (plan6c A.5). Called by Workplace only: the island and a closed workplace
 * never poll, because the timer and the listeners live and die with this hook.
 * - once at mount ("startup");
 * - every 60 s while `document.visibilityState === "visible"`;
 * - when the window becomes visible again (after 60 s) or gets focus (15 s floor);
 * - `refreshNow()` for "Opdatér" (5 s floor between clicks).
 * The backend enforces its own minimum interval per source and the back-off; `manual` is the
 * only reason that overrides them. Returns `refreshNow`, whose failure is always shown.
 */
export function useInboxPolling(): { refreshNow: () => void } {
  const { dispatch } = useStore();
  const lastAsk = useRef<number | null>(null);
  const lastManual = useRef<number | null>(null);
  const lastError = useRef<number | null>(null);

  const ask = useCallback(
    (trigger: PollTrigger): void => {
      const now = Date.now();
      const visible = document.visibilityState === "visible";
      const floorFrom = trigger === "manual" ? lastManual.current : lastAsk.current;
      const reason = pollReason(trigger, visible, floorFrom, now);
      if (reason === null) return;
      lastAsk.current = now;
      if (trigger === "manual") lastManual.current = now;
      refreshInbox(reason).catch((e: unknown) => {
        if (trigger !== "manual") {
          const at = Date.now();
          if (lastError.current !== null && at - lastError.current < ERROR_EVERY_MS) return;
          lastError.current = at;
        }
        dispatch({ type: "error/set", error: errorMessage(e) });
      });
    },
    [dispatch],
  );

  useEffect(() => {
    ask("mount");
    const timer = setInterval(() => ask("timer"), POLL_INTERVAL_MS);
    const onVisibility = () => ask("visible");
    const onFocus = () => ask("focus");
    document.addEventListener("visibilitychange", onVisibility);
    window.addEventListener("focus", onFocus);
    return () => {
      clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisibility);
      window.removeEventListener("focus", onFocus);
    };
  }, [ask]);

  const refreshNow = useCallback(() => ask("manual"), [ask]);
  return { refreshNow };
}
