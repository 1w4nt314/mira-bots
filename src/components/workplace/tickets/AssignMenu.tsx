import { useEffect, useId, useRef, useState, type KeyboardEvent } from "react";
import { assignTicket } from "../../../lib/ipc";
import { isExited } from "../../../lib/status";
import { isCoordinationTask } from "../../../lib/tickets";
import type { TicketSummary } from "../../../lib/types";
import { useStore } from "../../../state/store";
import { smallBtn, useRun, useTicketActions } from "./actions";

interface Item {
  key: string;
  label: string;
  title: string;
  disabled: boolean;
  act: () => void;
}

/**
 * Keyboard (and mouse) alternative to dragging: "Tildel…" opens a menu with the running agents
 * and "Ny agent på arbejdsplads/stabsplads". Arrow keys move, Enter picks, Esc closes; focus goes
 * back to the button. For a ticket in progress (step 5c, `canHandOver`) it hands the ticket over:
 * only the other running agents are offered, no new agent.
 */
export default function AssignMenu({ ticket }: { ticket: TicketSummary }) {
  const { state } = useStore();
  const { spawnBlocked, spawnWithTicket } = useTicketActions();
  const run = useRun();
  const [open, setOpen] = useState(false);
  const btnRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const menuId = useId();

  const handOver = ticket.state === "inProgress";
  const live = state.agents.filter(
    (a) => !isExited(a) && !(handOver && a.id === ticket.assigneeAgentId),
  );
  const spawnItems: Item[] = handOver
    ? []
    : [
        {
          key: "new-work",
          label: "Ny agent på arbejdsplads",
          title: spawnBlocked.work ?? "Start en ny agent på en arbejdsplads med denne ticket",
          disabled: spawnBlocked.work !== null,
          act: () => spawnWithTicket("work", ticket),
        },
        {
          key: "new-staff",
          label: "Ny agent på stabsplads",
          title: spawnBlocked.staff ?? "Start en ny agent på en stabsplads med denne ticket",
          disabled: spawnBlocked.staff !== null,
          act: () => spawnWithTicket("staff", ticket),
        },
      ];
  const items: Item[] = [
    ...live.map((a) => ({
      key: a.id,
      // Review 5c N4: a staff agent gets the ticket as a coordination task, not as work.
      label: isCoordinationTask(a)
        ? `${a.name} (koordineringsopgave) · ${a.queueLength} i kø`
        : `${a.name} · arbejdsplads · ${a.queueLength} i kø`,
      title:
        (handOver
          ? `Giv ticketen videre: den forlader sin agent og sættes bagerst i køen hos ${a.name}`
          : `Sæt ticketen bagerst i køen hos ${a.name}`) +
        (isCoordinationTask(a) ? " som koordineringsopgave (den fordeles, ikke udføres)" : ""),
      disabled: false,
      act: () => void run(() => assignTicket(ticket.id, a.id)),
    })),
    ...spawnItems,
  ];

  const enabledButtons = () =>
    Array.from(
      menuRef.current?.querySelectorAll<HTMLButtonElement>('[role="menuitem"]:not(:disabled)') ?? [],
    );

  // Focus the first usable entry when the menu opens.
  useEffect(() => {
    if (open) enabledButtons()[0]?.focus();
  }, [open]);

  // Close on a click outside.
  useEffect(() => {
    if (!open) return;
    const onDown = (e: PointerEvent) => {
      const target = e.target as Node;
      if (menuRef.current?.contains(target) || btnRef.current?.contains(target)) return;
      setOpen(false);
    };
    document.addEventListener("pointerdown", onDown);
    return () => document.removeEventListener("pointerdown", onDown);
  }, [open]);

  const close = (refocus: boolean) => {
    setOpen(false);
    if (refocus) btnRef.current?.focus();
  };

  const onMenuKey = (e: KeyboardEvent<HTMLDivElement>) => {
    const list = enabledButtons();
    const at = list.indexOf(document.activeElement as HTMLButtonElement);
    switch (e.key) {
      case "Escape":
        e.preventDefault();
        close(true);
        break;
      case "ArrowDown":
        e.preventDefault();
        list[(at + 1) % list.length]?.focus();
        break;
      case "ArrowUp":
        e.preventDefault();
        list[(at - 1 + list.length) % list.length]?.focus();
        break;
      case "Home":
        e.preventDefault();
        list[0]?.focus();
        break;
      case "End":
        e.preventDefault();
        list[list.length - 1]?.focus();
        break;
      case "Tab":
        close(false);
        break;
    }
  };

  return (
    <div className="relative">
      <button
        ref={btnRef}
        type="button"
        aria-haspopup="menu"
        aria-expanded={open}
        aria-controls={open ? menuId : undefined}
        onClick={() => setOpen((o) => !o)}
        onKeyDown={(e) => {
          if (e.key === "ArrowDown" && !open) {
            e.preventDefault();
            setOpen(true);
          }
        }}
        title={handOver ? "Giv ticketen videre til en anden agent" : "Tildel ticketen til en agent"}
        className={smallBtn}
      >
        Tildel…
      </button>
      {open && (
        <div
          ref={menuRef}
          id={menuId}
          role="menu"
          aria-label={`Tildel ticket ${ticket.shortId}`}
          onKeyDown={onMenuKey}
          className="absolute left-0 z-20 mt-1 w-64 rounded-lg border border-[var(--border)] bg-[var(--panel)] py-1 text-xs text-[var(--fg)] shadow-lg"
        >
          {live.length === 0 && (
            <p className="px-3 py-1 text-[var(--muted)]">
              {handOver ? "Ingen andre kørende agenter" : "Ingen kørende agenter"}
            </p>
          )}
          {items.map((it, i) => (
            <button
              key={it.key}
              type="button"
              role="menuitem"
              tabIndex={-1}
              disabled={it.disabled}
              title={it.title}
              onClick={() => {
                close(true);
                it.act();
              }}
              className={`block w-full truncate px-3 py-1 text-left hover:bg-[var(--accent)]/15 focus:bg-[var(--accent)]/15 focus:outline-none disabled:cursor-not-allowed disabled:opacity-50 ${
                i === live.length ? "mt-1 border-t border-[var(--border)] pt-1.5" : ""
              }`}
            >
              {it.label}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
