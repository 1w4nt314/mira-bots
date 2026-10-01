import { useEffect, useRef, useState } from "react";
import type { Theme } from "../../lib/bots";
import { errorMessage, openAgentFolder, removeAgent, stopAgent } from "../../lib/ipc";
import { effortLabel, modelLabel } from "../../lib/models";
import type { TermMode } from "../../lib/office";
import { rolesText } from "../../lib/roles";
import { isExited, isStartingHint, statusLabel } from "../../lib/status";
import type { AgentInfo, BotState } from "../../lib/types";
import { useStore } from "../../state/store";
import AgentTerminal from "../AgentTerminal";
import BotFigure from "../BotFigure";
import AgentReviews from "./AgentReviews";
import AgentSwitch from "./AgentSwitch";
import TicketQueue from "./tickets/TicketQueue";

interface Props {
  agent: AgentInfo;
  botState: BotState;
  theme: Theme;
  /** normal = split with the floor, min = one 34 px line (no xterm mounted), max = full height. */
  mode: TermMode;
  onMode: (mode: TermMode) => void;
  /** Called after the agent was removed, so the selection is cleared. */
  onRemoved: () => void;
  /** Height of everything above the xterm (header, queue, reviews, hint) in normal/max mode. */
  onChromeHeight?: (h: number) => void;
}

// The xterm is unmounted in min mode (never mounted at a few px: fit would send rows=1 to the
// PTY); restoring remounts it and replays the backend's output ring buffer.
// TODO(windows-verify): D.65, D.66
export default function TerminalPanel(props: Props) {
  const { agent, botState, theme, mode, onMode, onRemoved, onChromeHeight } = props;
  const { dispatch } = useStore();
  const [confirmStop, setConfirmStop] = useState(false);
  const exited = isExited(agent);
  const chromeRef = useRef<HTMLDivElement>(null);
  const isMin = mode === "min";
  // Keyboard focus across min <-> normal/max: the clicked button is unmounted, so focus moves to
  // its counterpart instead of falling back to <body>. Only for changes made with these buttons.
  const restoreRef = useRef<HTMLButtonElement>(null);
  const minRef = useRef<HTMLButtonElement>(null);
  const maxRef = useRef<HTMLButtonElement>(null);
  const focusNext = useRef<"restore" | "min" | "max" | null>(null);
  const setMode = (next: TermMode, focus: "restore" | "min" | "max") => {
    focusNext.current = focus;
    onMode(next);
  };

  // The block above the xterm varies (queue, reviews, starting hint): report its height so the
  // splitter clamp keeps XTERM_MIN for the xterm itself (C1: never a 1-row PTY).
  useEffect(() => {
    const el = chromeRef.current;
    if (isMin || el === null || onChromeHeight === undefined) return;
    const ro = new ResizeObserver(() => onChromeHeight(Math.ceil(el.getBoundingClientRect().height)));
    ro.observe(el);
    return () => {
      ro.disconnect();
      // Unmounted or minimised: no stale height for the next panel's first layout.
      onChromeHeight(0);
    };
  }, [isMin, onChromeHeight]);

  // Runs after AgentTerminal's mount effect, which focuses the xterm on the way back: only when
  // nothing has focus (exited agent) does the minimise/maximise button get it.
  useEffect(() => {
    const target = focusNext.current;
    focusNext.current = null;
    if (target === null) return;
    const active = document.activeElement;
    if (target === "restore") restoreRef.current?.focus();
    else if (active === null || active === document.body) {
      (target === "min" ? minRef : maxRef).current?.focus();
    }
  }, [isMin]);

  // The stop confirmation falls back after a moment (like the island's quit button).
  useEffect(() => {
    if (!confirmStop) return;
    const t = setTimeout(() => setConfirmStop(false), 3000);
    return () => clearTimeout(t);
  }, [confirmStop]);

  // Another agent: start without a pending confirmation.
  useEffect(() => setConfirmStop(false), [agent.id]);

  const run = async (action: () => Promise<void>) => {
    try {
      await action();
    } catch (e) {
      dispatch({ type: "error/set", error: errorMessage(e) });
    }
  };

  const stop = () => {
    if (!confirmStop) {
      setConfirmStop(true);
      return;
    }
    setConfirmStop(false);
    void run(() => stopAgent(agent.id));
  };

  const remove = () =>
    void run(async () => {
      await removeAgent(agent.id);
      onRemoved();
    });

  const label = statusLabel(agent.status);
  const btn =
    "shrink-0 rounded-md border border-[var(--border)] px-2.5 py-1 text-xs hover:border-[var(--accent)]";
  // Same look as `btn`, square: no px-2.5 to fight with px-0.
  const ibtn =
    "w-7 shrink-0 rounded-md border border-[var(--border)] px-0 py-1 text-center text-xs hover:border-[var(--accent)]";

  if (isMin) {
    return (
      <div className="flex h-[34px] shrink-0 items-center gap-2 border-t border-[var(--border)] px-3 text-xs">
        <BotFigure
          roles={agent.roles}
          specialist={agent.specialist}
          state={botState}
          theme={theme}
          exited={exited}
          size={24}
          badge={false}
        />
        <span className="shrink-0 font-medium">{agent.name}</span>
        <span className="min-w-0 truncate text-[var(--muted)]">
          {label}
          {agent.detail ? ` · ${agent.detail}` : ""}
        </span>
        <span className="ml-auto inline-flex shrink-0 gap-1">
          <button
            ref={restoreRef}
            type="button"
            onClick={() => setMode("normal", "min")}
            title="Gendan terminalen (gemt højde)"
            aria-label="Gendan terminalen"
            className={btn}
          >
            Gendan
          </button>
          <button
            type="button"
            onClick={() => setMode("max", "max")}
            title="Maksimér terminalen"
            aria-label="Maksimér terminalen"
            className={ibtn}
          >
            ⤢
          </button>
        </span>
      </div>
    );
  }

  return (
    <div className="flex min-h-0 flex-1 flex-col border-t border-[var(--border)]">
      <div ref={chromeRef} className="flex shrink-0 flex-col">
        <div className="flex shrink-0 items-center gap-3 px-3 py-2">
          <BotFigure
            roles={agent.roles}
            specialist={agent.specialist}
            state={botState}
            theme={theme}
            exited={exited}
            size={28}
            badge={false}
          />
          <div className="min-w-0 flex-1 leading-tight">
            <div className="flex items-baseline gap-2">
              <span className="font-medium">{agent.name}</span>
              <span className="truncate text-xs text-[var(--muted)]">
                {label}
                {agent.detail ? ` · ${agent.detail}` : ""}
              </span>
            </div>
            <div className="truncate text-[11px] text-[var(--muted)]">
              {agent.profileName} · {rolesText(agent.roles)}
              <span title={agent.modelObserved ? "Rapporteret af Claude Code (statuslinje)" : "Som startet"}>
                {" "}
                · Model: {modelLabel(agent.model)} · Effort: {effortLabel(agent.effort)}
                {agent.modelObserved && " (observeret)"}
              </span>
            </div>
            <div className="truncate font-mono text-[11px] text-[var(--muted)]" title={agent.cwd}>
              {agent.cwd}
            </div>
          </div>
          {!exited && <AgentSwitch agent={agent} />}
          {!exited && (
            <button
              type="button"
              onClick={stop}
              title="Stop agenten (klik igen for at bekræfte)"
              aria-label={`Stop ${agent.name}`}
              className={`${btn} ${confirmStop ? "border-rose-500 text-rose-500" : ""}`}
            >
              {confirmStop ? "Sikker?" : "Stop"}
            </button>
          )}
          {exited && (
            <button
              type="button"
              onClick={remove}
              title="Fjern agenten fra listen"
              aria-label={`Fjern ${agent.name}`}
              className={btn}
            >
              Fjern
            </button>
          )}
          <button
            type="button"
            onClick={() => void run(() => openAgentFolder(agent.id))}
            title="Åbn agentens mappe i Stifinder"
            aria-label={`Åbn mappen for ${agent.name}`}
            className={btn}
          >
            Åbn mappe
          </button>
          <span
            role="group"
            aria-label="Terminalvindue"
            className="ml-1 inline-flex shrink-0 gap-1 border-l border-[var(--border)] pl-2.5"
          >
            <button
              ref={minRef}
              type="button"
              onClick={() => setMode("min", "restore")}
              title="Minimér terminalen"
              aria-label="Minimér terminalen"
              className={ibtn}
            >
              ▁
            </button>
            <button
              ref={maxRef}
              type="button"
              onClick={() => onMode(mode === "max" ? "normal" : "max")}
              title={mode === "max" ? "Gendan delt visning" : "Maksimér terminalen"}
              aria-label={mode === "max" ? "Gendan delt visning" : "Maksimér terminalen"}
              aria-pressed={mode === "max"}
              className={`${ibtn} ${mode === "max" ? "office-ibtn-on" : ""}`}
            >
              {mode === "max" ? "⤡" : "⤢"}
            </button>
          </span>
        </div>
        <TicketQueue agent={agent} />
        <AgentReviews agent={agent} />
        {isStartingHint(agent) && (
          <div className="mx-3 mb-2 shrink-0 rounded-lg border border-amber-400/50 bg-amber-300/20 px-3 py-1.5 text-xs text-amber-800 dark:text-amber-200">
            Agenten venter på et svar i terminalen — fx 'Do you trust the files in this folder?'.
            Svar her.
          </div>
        )}
      </div>
      {/* Only in normal/max: switching between them keeps it mounted and the RO refits it. */}
      <AgentTerminal key={agent.id} agentId={agent.id} exited={exited} />
    </div>
  );
}
