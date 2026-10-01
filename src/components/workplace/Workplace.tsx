import { useEffect, useMemo, useState } from "react";
import { useBotStates, useTheme } from "../../lib/bots";
import { errorMessage, onWorkplaceSelect, takeWorkplaceSelection } from "../../lib/ipc";
import { assignSeats, STAFF_SEATS, WORK_SEATS } from "../../lib/seats";
import { isExited } from "../../lib/status";
import type { SeatKind } from "../../lib/types";
import { useStore } from "../../state/store";
import SeatGrid from "./SeatGrid";
import Sidebar from "./Sidebar";
import SpawnDialog from "./SpawnDialog";
import TerminalPanel from "./TerminalPanel";

export default function Workplace() {
  const { state, dispatch } = useStore();
  const { agents, appInfo, error } = state;
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [spawnFor, setSpawnFor] = useState<SeatKind | null>(null);
  const theme = useTheme();
  const botStates = useBotStates(agents);
  const seats = useMemo(() => assignSeats(agents), [agents]);

  // Selection handed over by the island: stored for a new window, pushed to an existing one.
  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | null = null;
    takeWorkplaceSelection()
      .then((sel) => {
        // Deliberately NOT guarded by `cancelled`: the backend slot is take-once, and StrictMode
        // (dev) runs mount -> cleanup -> mount, so the first call takes the id and the second gets
        // null. Dropping the id when the first effect is already cleaned up would lose it. The
        // component instance survives that simulated remount, and a real unmount makes the
        // setState a harmless no-op.
        // `sel.tab` (sidebar tab) is not used here yet.
        if (sel?.agentId) setSelectedId(sel.agentId);
      })
      .catch((e: unknown) => {
        if (!cancelled) dispatch({ type: "error/set", error: errorMessage(e) });
      });
    onWorkplaceSelect((sel) => {
      if (sel.agentId !== null) setSelectedId(sel.agentId);
    })
      .then((u) => {
        if (cancelled) u();
        else unlisten = u;
      })
      .catch((e: unknown) => {
        if (!cancelled) dispatch({ type: "error/set", error: errorMessage(e) });
      });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [dispatch]);

  // A selected agent that disappears (removed) simply shows no panel; ids are never reused.
  const selected = agents.find((a) => a.id === selectedId) ?? null;

  const maxWork = appInfo?.maxAgents ?? WORK_SEATS;
  const maxStaff = appInfo?.maxStaffAgents ?? STAFF_SEATS;
  const liveWork = agents.filter((a) => a.seatKind === "work" && !isExited(a)).length;
  const liveStaff = agents.filter((a) => a.seatKind === "staff" && !isExited(a)).length;

  let spawnDisabled: string | null = null;
  if (appInfo !== null && appInfo.hookExe === null) {
    spawnDisabled = "Fandt ikke mira-hook — sæt MIRA_HOOK_EXE";
  } else if (appInfo !== null && !appInfo.pipeReady) {
    spawnDisabled = "Hook-forbindelsen er ikke klar — genstart mira-bots";
  }

  return (
    <div className="flex h-full flex-col bg-[var(--bg)] text-sm text-[var(--fg)]">
      <header className="flex h-11 shrink-0 items-center gap-4 border-b border-[var(--border)] px-4">
        <h1 className="font-semibold">mira-bots · Workplace</h1>
        <span className="text-xs text-[var(--muted)]">
          {liveWork}/{maxWork} arbejdspladser · {liveStaff}/{maxStaff} stab
        </span>
        {error !== null && (
          <span className="ml-auto truncate text-xs text-rose-500" role="alert" title={error}>
            {error}
          </span>
        )}
      </header>
      <main className="grid min-h-0 flex-1 grid-cols-[1fr_340px]">
        <section className="flex min-h-0 min-w-0 flex-col">
          <SeatGrid
            seats={seats}
            botStates={botStates}
            theme={theme}
            selectedId={selected?.id ?? null}
            spawnDisabled={spawnDisabled}
            limits={{ work: liveWork >= maxWork, staff: liveStaff >= maxStaff }}
            onSelect={setSelectedId}
            onSpawn={setSpawnFor}
          />
          {selected === null ? (
            <div className="flex min-h-0 flex-1 items-center justify-center border-t border-[var(--border)] text-[var(--muted)]">
              Vælg en plads for at se terminalen
            </div>
          ) : (
            <TerminalPanel
              agent={selected}
              botState={botStates.get(selected.id) ?? "idle"}
              theme={theme}
              onRemoved={() => setSelectedId(null)}
            />
          )}
        </section>
        <Sidebar />
      </main>
      {spawnFor !== null && (
        <SpawnDialog
          seatKind={spawnFor}
          theme={theme}
          agentsRoot={appInfo?.agentsRoot ?? null}
          onClose={() => setSpawnFor(null)}
          onSpawned={(id) => {
            setSpawnFor(null);
            setSelectedId(id);
          }}
        />
      )}
    </div>
  );
}
