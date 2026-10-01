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
import { useCallback, useEffect, useMemo, useRef, useState, type CSSProperties } from "react";
import { useBotStates, useTheme } from "../../lib/bots";
import {
  assignTicket,
  errorMessage,
  onWorkplaceSelect,
  takeWorkplaceSelection,
} from "../../lib/ipc";
import {
  clampFloorHeight,
  COMPACT,
  deskLayout,
  FLOOR_DEFAULT,
  floorMin,
  parseDetail,
  parseFloorHeight,
  parseTermMode,
  SPLITTER_H,
  STORAGE_KEYS,
  termMinFor,
  type OfficeDetail,
  type TermMode,
} from "../../lib/office";
import { readLocal, writeLocal } from "../../lib/persist";
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
import OfficeDefs from "./office/OfficeDefs";
import Splitter from "./office/Splitter";
import SeatGrid, { SeatOverflow } from "./SeatGrid";
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
  // Office look: detail level, terminal mode and floor height are remembered (persist.ts).
  const [detail, setDetail] = useState<OfficeDetail>(() => parseDetail(readLocal(STORAGE_KEYS.detail)));
  const [termMode, setTermMode] = useState<TermMode>(() =>
    parseTermMode(readLocal(STORAGE_KEYS.termMode)),
  );
  const [floorHeight, setFloorHeight] = useState(() =>
    parseFloorHeight(readLocal(STORAGE_KEYS.floorHeight)),
  );
  // Measured heights: the left column, the overflow list below the floor (both make up
  // `available`) and the floor itself (`floorMeasured`).
  const [sectionH, setSectionH] = useState(0);
  const [overflowH, setOverflowH] = useState(0);
  const [floorMeasured, setFloorMeasured] = useState(FLOOR_DEFAULT);
  // Header/queue block above the xterm, reported by TerminalPanel (normal/max mode).
  const [chromeH, setChromeH] = useState(0);
  const sectionRef = useRef<HTMLElement>(null);
  const floorRef = useRef<HTMLDivElement>(null);
  const overflowRef = useRef<HTMLDivElement>(null);
  const theme = useTheme();
  const botStates = useBotStates(agents);
  const seats = useMemo(() => assignSeats(agents), [agents]);
  const ticketsById = useMemo(
    () => new Map<string, TicketSummary>(tickets.map((t) => [t.id, t])),
    [tickets],
  );

  useEffect(() => writeLocal(STORAGE_KEYS.detail, detail), [detail]);
  useEffect(() => writeLocal(STORAGE_KEYS.termMode, termMode), [termMode]);
  // Dragging changes the height every frame: store it once the splitter has rested for a moment.
  // A value still waiting when the window goes away (unmount, reload, close) is written at once.
  const pendingFloor = useRef<number | null>(null);
  const flushFloor = useCallback(() => {
    if (pendingFloor.current === null) return;
    writeLocal(STORAGE_KEYS.floorHeight, String(Math.round(pendingFloor.current)));
    pendingFloor.current = null;
  }, []);
  useEffect(() => {
    pendingFloor.current = floorHeight;
    const t = setTimeout(flushFloor, 150);
    return () => clearTimeout(t);
  }, [floorHeight, flushFloor]);
  useEffect(() => {
    window.addEventListener("pagehide", flushFloor);
    return () => {
      window.removeEventListener("pagehide", flushFloor);
      flushFloor();
    };
  }, [flushFloor]);

  // One observer for the column, the floor and the overflow list. The desk size only changes the
  // floor's children, never its own height (that comes from `floorHeight` / flex), so there is no
  // feedback loop. The overflow wrapper is always mounted (empty = 0 px) so it is observed too.
  useEffect(() => {
    const section = sectionRef.current;
    const floor = floorRef.current;
    const overflow = overflowRef.current;
    if (section === null || floor === null || overflow === null) return;
    const ro = new ResizeObserver((entries) => {
      for (const entry of entries) {
        const h = Math.round(entry.contentRect.height);
        if (entry.target === section) setSectionH(h);
        else if (entry.target === floor) setFloorMeasured(h);
        else if (entry.target === overflow) setOverflowH(h);
      }
    });
    ro.observe(section);
    ro.observe(floor);
    ro.observe(overflow);
    return () => ro.disconnect();
  }, []);

  // Choosing an agent while the terminal is minimised brings the terminal back (stored height).
  const selectAgent = useCallback((id: string) => {
    setSelectedId(id);
    setTermMode((m) => (m === "min" ? "normal" : m));
  }, []);

  // Selection handed over by the island: stored for a new window, pushed to an existing one.
  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | null = null;
    const apply = (sel: WorkplaceSelection) => {
      if (sel.agentId !== null) selectAgent(sel.agentId);
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
  }, [dispatch, selectAgent]);

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
      selectAgent,
      spawnWithTicket: (seatKind, ticket) => setSpawnFor({ seatKind, ticket }),
      spawnBlocked: { work: spawnBlockedWork, staff: spawnBlockedStaff },
    }),
    [selectAgent, spawnBlockedWork, spawnBlockedStaff],
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

  // Height for the floor and the terminal in normal mode: the column minus the splitter and the
  // overflow list. Before the first measurement it is NaN: clampFloorHeight then skips the upper
  // bound instead of jumping to the floor minimum for one frame.
  const available = sectionH > 0 ? Math.max(0, sectionH - SPLITTER_H - overflowH) : Number.NaN;
  // Without a selected agent there is no terminal: lay out as normal, but keep `termMode`.
  const layoutMode: TermMode = selected === null ? "normal" : termMode;
  // Both seat rows fit (W1) and the terminal keeps its header/queue plus XTERM_MIN (C1). When the
  // column is too low for both, the terminal wins: the floor shrinks towards one seat row
  // (FLOOR_HARD_MIN) and scrolls (`cramped`), so the xterm's input line stays on screen (W5).
  const minFloor = floorMin(detail);
  const minTerm = selected === null ? termMinFor(0) : termMinFor(chromeH);
  const clampFloor = (h: number) => clampFloorHeight(h, available, minFloor, minTerm);
  const floorStyleHeight = clampFloor(floorHeight);
  const measured = Number.isFinite(available);
  // Effective splitter range (Home/End, aria-valuemin/max); equal when the floor is cramped.
  const lowFloor = measured ? clampFloor(0) : minFloor;
  const maxFloor = measured ? clampFloor(Number.MAX_SAFE_INTEGER) : undefined;
  const cramped = layoutMode === "normal" && floorStyleHeight < minFloor;
  const { deskH, fig } = layoutMode === "max" ? COMPACT : deskLayout(floorMeasured, detail);
  const floorSize: CSSProperties =
    layoutMode === "normal"
      ? { height: floorStyleHeight, flex: "none" }
      : layoutMode === "min"
        ? { flex: "1 1 auto", minHeight: 0 }
        : { flex: "none" };
  const floorStyle = {
    ...floorSize,
    "--desk-h": `${deskH}px`,
    "--fig": `${fig}px`,
  } as CSSProperties;
  // Splitter values are clamped against the measured column; nothing to clamp against before the
  // first measurement (the splitter cannot be used before the first layout anyway).
  const changeFloorHeight = (next: number) => {
    if (measured) setFloorHeight(clampFloor(next));
  };

  return (
    <TicketActionsContext.Provider value={ticketActions}>
      <div
        data-detail={detail}
        data-term={layoutMode}
        className="flex h-full flex-col bg-[var(--bg)] text-sm text-[var(--fg)]"
      >
        <header className="flex h-11 shrink-0 items-center gap-4 border-b border-[var(--border)] px-4">
          <h1 className="font-semibold">mira-bots · Workplace</h1>
          <span className="text-xs text-[var(--muted)]">
            {liveWork}/{maxWork} arbejdspladser · {liveStaff}/{maxStaff} stab
          </span>
          <button
            type="button"
            onClick={() => setDetail((d) => (d === "more" ? "discreet" : "more"))}
            aria-pressed={detail === "more"}
            aria-label="Kontor-detaljer"
            title="Kontor-detaljer: Diskret eller Lidt mere (huskes)"
            className="rounded-md border border-[var(--border)] px-2 py-0.5 text-[11px] text-[var(--muted)] hover:border-[var(--accent)]"
          >
            Kontor: {detail === "more" ? "Lidt mere" : "Diskret"}
          </button>
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
            <section ref={sectionRef} className="relative flex min-h-0 min-w-0 flex-col">
              <OfficeDefs />
              <div
                ref={floorRef}
                className="office-floor"
                data-term={layoutMode}
                data-cramped={cramped ? "true" : undefined}
                style={floorStyle}
              >
                <SeatGrid
                  seats={seats}
                  botStates={botStates}
                  theme={theme}
                  selectedId={selected?.id ?? null}
                  spawnDisabled={spawnDisabled}
                  limits={{ work: liveWork >= maxWork, staff: liveStaff >= maxStaff }}
                  tickets={ticketsById}
                  dragging={activeTicket !== null}
                  onSelect={selectAgent}
                  onSpawn={(seatKind) => setSpawnFor({ seatKind, ticket: null })}
                  detail={detail}
                  mode={layoutMode}
                  fig={fig}
                />
              </div>
              {/* Outside the floor: its fixed height and overflow:hidden would clip the list. */}
              <div ref={overflowRef} className="shrink-0">
                <SeatOverflow overflow={seats.overflow} />
              </div>
              <Splitter
                value={floorStyleHeight}
                min={lowFloor}
                max={maxFloor}
                // Nothing to move when the floor is cramped (min = max): hidden like in min/max.
                disabled={layoutMode !== "normal" || (maxFloor !== undefined && maxFloor <= lowFloor)}
                onChange={changeFloorHeight}
                onReset={() => setFloorHeight(FLOOR_DEFAULT)}
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
                  mode={termMode}
                  onMode={setTermMode}
                  onRemoved={() => setSelectedId(null)}
                  onChromeHeight={setChromeH}
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
              selectAgent(id);
            }}
          />
        )}
      </div>
    </TicketActionsContext.Provider>
  );
}
