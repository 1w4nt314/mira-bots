import { useEffect, useState } from "react";
import { errorMessage, getAppInfo, pickFolder, spawnAgent } from "../lib/ipc";
import { isExited } from "../lib/status";
import { useStore } from "../state/store";

export default function NewAgentButton() {
  const { state, dispatch } = useStore();
  const [busy, setBusy] = useState(false);
  // Fallback when the native dialog fails: type the folder path.
  // TODO(windows-verify): the island is not focusable, so this field may not take keyboard
  // input; it is only a fallback for a failing folder dialog (plan D.11).
  const [manual, setManual] = useState<string | null>(null);

  // Keep the island open while the folder dialog or the fallback field is in use.
  const pinned = busy || manual !== null;
  useEffect(() => {
    dispatch({ type: "ui/pin", pinned });
    return () => dispatch({ type: "ui/pin", pinned: false });
  }, [pinned, dispatch]);

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
  const running = state.agents.filter((a) => !isExited(a)).length;
  // A missing claude does not disable the button: the backend looks it up again on spawn and
  // its (Danish) error is shown if it is still missing.
  let disabledReason: string | null = null;
  if (info !== null && info.hookExe === null) {
    disabledReason = "Fandt ikke mira-hook — sæt MIRA_HOOK_EXE";
  } else if (info !== null && !info.pipeReady) {
    disabledReason = "Hook-forbindelsen er ikke klar — genstart mira-bots";
  } else if (info !== null && running >= info.maxAgents) {
    disabledReason = `Loft på ${info.maxAgents} agenter nået`;
  }
  const pipeDown = info !== null && info.hookExe !== null && !info.pipeReady;
  const claudeHint =
    info !== null && info.claudePath === null
      ? "Fandt ikke claude endnu — installer Claude Code eller sæt MIRA_CLAUDE_PATH"
      : null;

  const start = async (path: string) => {
    try {
      await spawnAgent(path, null);
    } catch (e) {
      dispatch({ type: "error/set", error: errorMessage(e) });
    }
  };

  const choose = async () => {
    setBusy(true);
    try {
      let path: string | null = null;
      try {
        path = await pickFolder();
      } catch (e) {
        dispatch({ type: "error/set", error: `Mappevælgeren fejlede: ${errorMessage(e)}` });
        setManual("");
        return;
      }
      if (path) await start(path);
    } finally {
      setBusy(false);
    }
  };

  const submitManual = async () => {
    const path = (manual ?? "").trim();
    setManual(null);
    if (path) {
      setBusy(true);
      await start(path);
      setBusy(false);
    }
  };

  if (manual !== null) {
    return (
      <form
        className="flex items-center gap-1"
        onSubmit={(e) => {
          e.preventDefault();
          void submitManual();
        }}
      >
        <input
          autoFocus
          value={manual}
          onChange={(e) => setManual(e.target.value)}
          placeholder="Sti til mappe"
          className="w-40 rounded bg-black/40 px-1.5 py-1 text-[11px] outline-none"
        />
        <button type="submit" className="rounded bg-white/10 px-2 py-1 text-[11px]">
          Start
        </button>
        <button
          type="button"
          onClick={() => setManual(null)}
          className="rounded px-1 text-[11px] text-neutral-400 hover:text-white"
          aria-label="Annuller"
        >
          {"×"}
        </button>
      </form>
    );
  }

  return (
    <>
      {pipeDown && (
        <span className="shrink-0 text-[11px] text-rose-300" role="status">
          Hooks ikke klar
        </span>
      )}
      <button
        type="button"
        onClick={() => void choose()}
        disabled={busy || disabledReason !== null}
        title={
          disabledReason ??
          claudeHint ??
          "Start en ny agent (kører Claude Code i den valgte mappe)"
        }
        className="shrink-0 rounded-md bg-white/10 px-2.5 py-1 text-[11px] font-medium hover:bg-white/20 disabled:cursor-not-allowed disabled:opacity-40"
      >
        + Ny agent
      </button>
    </>
  );
}
