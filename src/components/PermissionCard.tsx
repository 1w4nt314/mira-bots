import { useEffect, useState } from "react";
import { errorMessage, respondPermission } from "../lib/ipc";
import type { PermissionRequestInfo } from "../lib/types";
import { useStore } from "../state/store";

/** Re-renders every `ms` and returns the current time in ms. */
function useNow(ms: number): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), ms);
    return () => clearInterval(t);
  }, [ms]);
  return now;
}

const EXPIRED = "Anmodningen er udløbet";

export default function PermissionCard({ request }: { request: PermissionRequestInfo }) {
  const { dispatch } = useStore();
  const [busy, setBusy] = useState(false);
  const now = useNow(1000);
  const secondsLeft = Math.max(0, Math.ceil((request.deadlineAt - now) / 1000));

  const answer = async (allow: boolean, always: boolean) => {
    setBusy(true);
    try {
      await respondPermission(request.requestId, allow, always);
      dispatch({ type: "permission/remove", requestId: request.requestId });
    } catch (e) {
      const msg = errorMessage(e);
      dispatch({ type: "error/set", error: msg });
      // An expired request can never be answered: drop the card.
      if (msg === EXPIRED) dispatch({ type: "permission/remove", requestId: request.requestId });
      else setBusy(false);
    }
  };

  const btn = "rounded-md px-2.5 py-1 text-[11px] font-medium disabled:opacity-50";
  return (
    <div className="mx-3 mb-2 rounded-lg border border-rose-400/40 bg-rose-400/10 p-2">
      <div className="flex items-baseline justify-between gap-2">
        <div className="min-w-0 truncate">
          <span className="font-medium">{request.agentName}</span> vil køre{" "}
          <span className="font-medium">{request.toolName}</span>
        </div>
        <div className="shrink-0 text-[10px] text-neutral-400">Udløber om {secondsLeft} s</div>
      </div>
      {request.summary && (
        <div
          className="mt-1 line-clamp-2 break-all rounded bg-black/40 px-1.5 py-1 font-mono text-[11px] text-neutral-200"
          title={request.summary}
        >
          {request.summary}
        </div>
      )}
      <div className="mt-2 flex gap-1.5">
        <button
          type="button"
          disabled={busy}
          onClick={() => answer(true, false)}
          className={`${btn} bg-emerald-500 text-neutral-950 hover:bg-emerald-400`}
        >
          Tillad
        </button>
        <button
          type="button"
          disabled={busy}
          onClick={() => answer(false, false)}
          className={`${btn} bg-white/10 hover:bg-white/20`}
        >
          Afvis
        </button>
        <button
          type="button"
          disabled={busy}
          onClick={() => answer(true, true)}
          title={`Tillader alle fremtidige kald af ${request.toolName} for denne agent (hele værktøjet, fx alle Bash-kommandoer)`}
          className={`${btn} bg-white/10 hover:bg-white/20`}
        >
          Altid for denne agent
        </button>
      </div>
    </div>
  );
}
