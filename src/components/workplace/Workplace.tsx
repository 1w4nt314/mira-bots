import {
  closestCenter,
  DndContext,
  DragOverlay,
  KeyboardSensor,
  PointerSensor,
  pointerWithin,
  useSensor,
  useSensors,
  type Announcements,
  type CollisionDetection,
  type DragEndEvent,
  type DragStartEvent,
} from "@dnd-kit/core";
import { useCallback, useEffect, useMemo, useState } from "react";
import { useBotStates, useTheme } from "../../lib/bots";
import {
  assignTicket,
  errorMessage,
  onWorkplaceSelect,
  takeWorkplaceSelection,
} from "../../lib/ipc";
import { assignSeats, STAFF_SEATS, WORK_SEATS } from "../../lib/seats";
import { isExited } from "../../lib/status";
import {
  canDrag,
  draggedTicketId,
  dropTarget,
  DRAG_DISTANCE_PX,
  parseWorkplaceTab,
} from "../../lib/tickets";
import type {
  SeatKind,
  TicketSummary,
  WorkplaceSelection,
  WorkplaceTab,
} from "../../lib/types";
import { useStore } from "../../state/store";
import SeatGrid from "./SeatGrid";
import Sidebar from "./Sidebar";
import SpawnDialog from "./SpawnDialog";
import TerminalPanel from "./TerminalPanel";
import { TicketActionsContext, type TicketActions } from "./tickets/actions";
import StickyNote from "./tickets/StickyNote";

/** The pointer decides the target; keyboard drags have no pointer, so the nearest seat wins. */
const collision: CollisionDetection = (args) =>
  args.pointerCoordinates !== null ? pointerWithin(args) : closestCenter(args);

export default function Workplace() {
  const { state, dispatch } = useStore();
  const { agents, appInfo, error, tickets } = state;
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [spawnFor, setSpawnFor] = useState<{
    seatKind: SeatKind;
    ticket: TicketSummary | null;
  } | null>(null);
  const [requestedTab, setRequestedTab] = useState<{ tab: WorkplaceTab; nonce: number } | null>(
    null,
  );
  const [activeTicketId, setActiveTicketId] = useState<string | null>(null);
  const theme = useTheme();
  const botStates = useBotStates(agents);
  const seats = useMemo(() => assignSeats(agents), [agents]);
  const ticketsById = useMemo(
    () => new Map<string, TicketSummary>(tickets.map((t) => [t.id, t])),
    [tickets],
  );

  // Selection handed over by the island: stored for a new window, pushed to an existing one.
  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | null = null;
    const apply = (sel: WorkplaceSelection) => {
      if (sel.agentId !== null) setSelectedId(sel.agentId);
      const tab = parseWorkplaceTab(sel.tab);
      if (tab !== null) setRequestedTab((prev) => ({ tab, nonce: (prev?.nonce ?? 0) + 1 }));
    };
    takeWorkplaceSelection()
      .then((sel) => {
        // Deliberately NOT guarded by `cancelled`: the backend slot is take-once, and StrictMode
        // (dev) runs mount -> cleanup -> mount, so the first call takes the selection and the
        // second gets null. Dropping it when the first effect is already cleaned up would lose
        // it. The component instance survives that simulated remount, and a real unmount makes
        // the setState a harmless no-op.
        if (sel !== null) apply(sel);
      })
      .catch((e: unknown) => {
        if (!cancelled) dispatch({ type: "error/set", error: errorMessage(e) });
      });
    onWorkplaceSelect(apply)
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
  const limitReached = "Loftet for denne række er nået";
  const spawnBlockedWork = spawnDisabled ?? (liveWork >= maxWork ? limitReached : null);
  const spawnBlockedStaff = spawnDisabled ?? (liveStaff >= maxStaff ? limitReached : null);

  const ticketActions = useMemo<TicketActions>(
    () => ({
      selectAgent: setSelectedId,
      spawnWithTicket: (seatKind, ticket) => setSpawnFor({ seatKind, ticket }),
      spawnBlocked: { work: spawnBlockedWork, staff: spawnBlockedStaff },
    }),
    [spawnBlockedWork, spawnBlockedStaff],
  );

  // --- drag-and-drop: backlog notes (sidebar) onto seats ---------------------------------------
  // TODO(windows-verify): PointerSensor in WebView2 — dragging a note from the sidebar to a seat
  // works with mouse and touchpad, a click (< 6 px) on a seat still selects the terminal, the
  // DragOverlay follows the pointer, and no `data-tauri-drag-region` exists in this window
  // (plan D.31). Drags never cross windows (one document per webview).
  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: DRAG_DISTANCE_PX } }),
    useSensor(KeyboardSensor),
  );

  const titleOf = useCallback(
    (id: string | number) => {
      const tid = draggedTicketId(String(id));
      const t = tid === null ? undefined : ticketsById.get(tid);
      return t === undefined ? "ticketen" : `ticket ${t.shortId}`;
    },
    [ticketsById],
  );
  const seatOf = useCallback(
    (id: string | number) => {
      const target = dropTarget(String(id));
      if (target === null) return "et ugyldigt sted";
      if (target.kind === "empty") {
        return target.seatKind === "work" ? "en tom arbejdsplads" : "en tom stabsplads";
      }
      return agents.find((a) => a.id === target.agentId)?.name ?? "en agent";
    },
    [agents],
  );
  const announcements = useMemo<Announcements>(
    () => ({
      onDragStart: ({ active }) => `Tog ${titleOf(active.id)}.`,
      onDragOver: ({ active, over }) =>
        over ? `${titleOf(active.id)} er over ${seatOf(over.id)}.` : `${titleOf(active.id)} er ikke over en plads.`,
      onDragEnd: ({ active, over }) =>
        over ? `${titleOf(active.id)} sluppet på ${seatOf(over.id)}.` : `${titleOf(active.id)} sluppet uden mål.`,
      onDragCancel: ({ active }) => `Træk af ${titleOf(active.id)} annulleret.`,
    }),
    [titleOf, seatOf],
  );
  const screenReaderInstructions = {
    draggable:
      "Tryk mellemrum eller Enter for at tage ticketen. Flyt med piletasterne, slip med mellemrum eller Enter, annuller med Escape. Eller brug knappen Tildel.",
  };

  const onDragStart = (e: DragStartEvent) => setActiveTicketId(draggedTicketId(String(e.active.id)));

  const onDragEnd = (e: DragEndEvent) => {
    setActiveTicketId(null);
    const ticketId = draggedTicketId(String(e.active.id));
    const target = e.over ? dropTarget(String(e.over.id)) : null;
    if (ticketId === null || target === null) return;
    const ticket = ticketsById.get(ticketId);
    if (ticket === undefined || !canDrag(ticket)) return;
    if (target.kind === "agent") {
      assignTicket(ticketId, target.agentId).catch((err: unknown) =>
        dispatch({ type: "error/set", error: errorMessage(err) }),
      );
    } else {
      setSpawnFor({ seatKind: target.seatKind, ticket });
    }
  };

  const activeTicket = activeTicketId === null ? null : (ticketsById.get(activeTicketId) ?? null);

  return (
    <TicketActionsContext.Provider value={ticketActions}>
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
        <DndContext
          sensors={sensors}
          collisionDetection={collision}
          accessibility={{ announcements, screenReaderInstructions }}
          onDragStart={onDragStart}
          onDragEnd={onDragEnd}
          onDragCancel={() => setActiveTicketId(null)}
        >
          <main className="grid min-h-0 flex-1 grid-cols-[1fr_340px]">
            <section className="flex min-h-0 min-w-0 flex-col">
              <SeatGrid
                seats={seats}
                botStates={botStates}
                theme={theme}
                selectedId={selected?.id ?? null}
                spawnDisabled={spawnDisabled}
                limits={{ work: liveWork >= maxWork, staff: liveStaff >= maxStaff }}
                tickets={ticketsById}
                dragging={activeTicket !== null}
                onSelect={setSelectedId}
                onSpawn={(seatKind) => setSpawnFor({ seatKind, ticket: null })}
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
            <Sidebar requestedTab={requestedTab} />
          </main>
          <DragOverlay dropAnimation={null}>
            {activeTicket !== null && (
              <StickyNote ticket={activeTicket} agent={null} draggable={false} compact interactive={false} />
            )}
          </DragOverlay>
        </DndContext>
        {spawnFor !== null && (
          <SpawnDialog
            seatKind={spawnFor.seatKind}
            ticket={spawnFor.ticket}
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
    </TicketActionsContext.Provider>
  );
}
