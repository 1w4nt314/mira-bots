import { useEffect, useState } from "react";
import { errorMessage, getAppInfo, spawnAgent } from "../lib/ipc";
import { isExited } from "../lib/status";
import { useStore } from "../state/store";

export default function NewAgentButton() {
  const { state, dispatch } = useStore();
  const [busy, setBusy] = useState(false);

  // This button is only mounted while the island is open: refresh the app info each time it
  // opens, so a claude installed (or a pipe that became ready) while the app runs is picked up.
  useEffect(() => {
    let cancelled = false;
    getAppInfo()
      .then((appInfo) => {
        if (!cancelled) dispatch({ type: "appInfo/set", appInfo });
      })
      .catch((e: unknown) => {
        if (!cancelled) dispatch({ type: "error/set", error: errorMessage(e) });
      });
    return () => {
      cancelled = true;
    };
  }, [dispatch]);

  const info = state.appInfo;
  // The island always spawns into a work seat, so only live work agents count against the limit.
  const running = state.agents.filter((a) => a.seatKind === "work" && !isExited(a)).length;
  // A missing claude does not disable the button: the backend looks it up again on spawn and
  // its (Danish) error is shown if it is still missing.
  let disabledReason: string | null = null;
  if (info !== null && info.hookExe === null) {
    disabledReason = "Fandt ikke mira-hook — sæt MIRA_HOOK_EXE";
  } else if (info !== null && !info.pipeReady) {
    disabledReason = "Hook-forbindelsen er ikke klar — genstart mira-bots";
  } else if (info !== null && running >= info.maxAgents) {
    disabledReason = `Loft på ${info.maxAgents} arbejdspladser nået`;
  }
  const pipeDown = info !== null && info.hookExe !== null && !info.pipeReady;
  const claudeHint =
    info !== null && info.claudePath === null
      ? "Fandt ikke claude endnu — installer Claude Code eller sæt MIRA_CLAUDE_PATH"
      : null;

  // One click: default profile (coder), default folder, work seat (profile/folder are chosen in
  // Workplace).
  const start = async () => {
    setBusy(true);
    try {
      await spawnAgent(null, null, null, null, "work");
    } catch (e) {
      dispatch({ type: "error/set", error: errorMessage(e) });
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      {pipeDown && (
        <span className="shrink-0 text-[11px] text-rose-300" role="status">
          Hooks ikke klar
        </span>
      )}
      <button
        type="button"
        onClick={() => void start()}
        disabled={busy || disabledReason !== null}
        title={
          disabledReason ??
          claudeHint ??
          "Start en ny agent i standardmappen (vælg rolle/mappe i Workplace)"
        }
        className="shrink-0 rounded-md bg-white/10 px-2.5 py-1 text-[11px] font-medium hover:bg-white/20 disabled:cursor-not-allowed disabled:opacity-40"
      >
        + Ny agent
      </button>
    </>
  );
}
