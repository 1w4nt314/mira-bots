import { useEffect, useRef } from "react";
import { Terminal, type IDisposable, type ITheme } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import {
  errorMessage,
  getAgentOutput,
  onAgentOutput,
  resizeAgentPty,
  writeAgentInput,
} from "../lib/ipc";
import { classifyInput } from "../lib/terminalInput";
import { decodeBase64, planChunk } from "../lib/terminalSeq";
import type { AgentOutputPayload } from "../lib/types";
import { useStore } from "../state/store";

const isWindows = typeof navigator !== "undefined" && navigator.userAgent.includes("Windows");

/** xterm colours from the `--term-*` CSS tokens (styles.css), so both themes stay in one place. */
function readTermTheme(): ITheme {
  const css = getComputedStyle(document.documentElement);
  const v = (name: string) => css.getPropertyValue(`--term-${name}`).trim();
  return {
    background: v("bg"),
    foreground: v("fg"),
    cursor: v("cursor"),
    cursorAccent: v("bg"),
    selectionBackground: v("selection"),
    black: v("black"),
    red: v("red"),
    green: v("green"),
    yellow: v("yellow"),
    blue: v("blue"),
    magenta: v("magenta"),
    cyan: v("cyan"),
    white: v("white"),
    brightBlack: v("bright-black"),
    brightRed: v("bright-red"),
    brightGreen: v("bright-green"),
    brightYellow: v("bright-yellow"),
    brightBlue: v("bright-blue"),
    brightMagenta: v("bright-magenta"),
    brightCyan: v("bright-cyan"),
    brightWhite: v("bright-white"),
  };
}

type Phase = "loading" | "live" | "resync" | "dead";

/**
 * Live terminal of one agent. Mount with `key={agentId}`: switching agents is a full remount.
 *
 * Order: listen to `agent-output` first (chunks are queued), then take the snapshot, write it,
 * and replay the queue through `planChunk` so duplicates/overlaps/gaps are handled.
 */
// TODO(windows-verify): xterm renders the TUI correctly under ConPTY (`windowsPty: conpty`):
// colours, cursor, reflow on resize, no doubled line feeds (CRLF untouched); Enter, arrows,
// Ctrl+C, Tab, Esc and æøå reach the agent as UTF-8 (plan D.20).
// TODO(windows-verify): D.38 xterm's automatic replies (Device Attributes, cursor-position and
// focus reports, bracketed-paste markers) are classified as not user-initiated under ConPTY, so
// they do not postpone ticket delivery; real typing, arrows and paste still do.
// TODO(windows-verify): 5 agents with heavy output and one mounted xterm cause no noticeable UI
// lag (plan D.27).
export default function AgentTerminal({ agentId, exited }: { agentId: string; exited: boolean }) {
  const { dispatch } = useStore();
  const containerRef = useRef<HTMLDivElement>(null);
  const exitedRef = useRef(exited);

  useEffect(() => {
    exitedRef.current = exited;
  }, [exited]);

  useEffect(() => {
    const el = containerRef.current;
    if (el === null) return;
    let cancelled = false;

    const term = new Terminal({
      cursorBlink: true,
      scrollback: 5000,
      fontSize: 13,
      fontFamily: '"Cascadia Mono", Consolas, "DejaVu Sans Mono", monospace',
      theme: readTermTheme(),
      windowsPty: isWindows ? { backend: "conpty" } : undefined,
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(el);

    // --- size ------------------------------------------------------------------------------
    let sentCols = 0;
    let sentRows = 0;
    const sendSize = (cols: number, rows: number) => {
      if (exitedRef.current || (cols === sentCols && rows === sentRows)) return;
      sentCols = cols;
      sentRows = rows;
      // A failed resize is not actionable for the user (e.g. the agent just exited).
      resizeAgentPty(agentId, cols, rows).catch(() => {
        sentCols = 0;
        sentRows = 0;
      });
    };
    const disposables: IDisposable[] = [];
    disposables.push(term.onResize(({ cols, rows }) => sendSize(cols, rows)));
    // A 0 px box would fit to rows=1 and send that to the PTY: skip, the observer refits later.
    const fitNow = () => {
      if (el.clientWidth === 0 || el.clientHeight === 0) return;
      fit.fit();
    };
    fitNow();
    // onResize only fires on a change, so send the first size explicitly.
    sendSize(term.cols, term.rows);

    let raf: number | null = null;
    const ro = new ResizeObserver(() => {
      if (raf !== null) return;
      raf = requestAnimationFrame(() => {
        raf = null;
        if (!cancelled) fitNow();
      });
    });
    ro.observe(el);

    // --- theme -----------------------------------------------------------------------------
    const mql = window.matchMedia("(prefers-color-scheme: dark)");
    const onScheme = () => {
      term.options.theme = readTermTheme();
    };
    mql.addEventListener("change", onScheme);

    // --- input: serial queue keeps keystrokes in order ------------------------------------
    // `onData` also carries the terminal's own replies (device attributes, cursor/focus reports).
    // Those are ESC-prefixed and come without a keypress; arrows/Esc are ESC-prefixed too but
    // follow a keydown (`onKey`); pasted text has no keydown but is not ESC-prefixed. Only
    // user-initiated data may postpone ticket delivery (see `classifyInput`).
    let lastKeyAt = Number.NEGATIVE_INFINITY;
    disposables.push(
      term.onKey(() => {
        lastKeyAt = performance.now();
      }),
    );
    let inputQueue: Promise<void> = Promise.resolve();
    disposables.push(
      term.onData((data) => {
        if (exitedRef.current) return;
        const userInitiated = classifyInput(data, performance.now() - lastKeyAt);
        inputQueue = inputQueue
          .then(() => writeAgentInput(agentId, data, userInitiated))
          .catch((e: unknown) => {
            if (!cancelled) dispatch({ type: "error/set", error: errorMessage(e) });
          });
      }),
    );

    // --- output ----------------------------------------------------------------------------
    let phase: Phase = "loading";
    let lastSeq = -1;
    let queue: AgentOutputPayload[] = [];
    let unlisten: (() => void) | null = null;

    const handle = (p: AgentOutputPayload) => {
      if (phase === "dead") return;
      if (phase !== "live") {
        queue.push(p);
        return;
      }
      const bytes = decodeBase64(p.dataBase64);
      const plan = planChunk(lastSeq, p.seq, bytes.length);
      if (plan.kind === "skip") return;
      if (plan.kind === "write") {
        term.write(plan.from === 0 ? bytes : bytes.subarray(plan.from));
        lastSeq = p.seq;
        return;
      }
      phase = "resync";
      void loadSnapshot(true);
    };

    const loadSnapshot = async (reset: boolean) => {
      let snap: AgentOutputPayload;
      try {
        snap = await getAgentOutput(agentId);
      } catch (e) {
        if (cancelled) return;
        phase = "dead";
        queue = [];
        term.write(`\x1b[90m${errorMessage(e)}\x1b[0m\r\n`);
        return;
      }
      if (cancelled) return;
      if (reset) term.reset();
      term.write(decodeBase64(snap.dataBase64));
      lastSeq = snap.seq;
      phase = "live";
      const pending = queue;
      queue = [];
      for (const p of pending) handle(p);
    };

    void (async () => {
      try {
        const u = await onAgentOutput((p) => {
          if (p.agentId === agentId) handle(p);
        });
        if (cancelled) {
          u();
          return;
        }
        unlisten = u;
      } catch (e) {
        if (!cancelled) dispatch({ type: "error/set", error: errorMessage(e) });
        return;
      }
      await loadSnapshot(false);
    })();

    if (!exitedRef.current) term.focus();

    return () => {
      cancelled = true;
      if (raf !== null) cancelAnimationFrame(raf);
      ro.disconnect();
      mql.removeEventListener("change", onScheme);
      unlisten?.();
      for (const d of disposables) d.dispose();
      term.dispose();
      el.replaceChildren();
    };
  }, [agentId, dispatch]);

  // FitAddon measures this element's parent box: padding lives on the wrapper, not on .xterm.
  return (
    <div className="flex min-h-0 flex-1 flex-col bg-[var(--term-bg)] p-2">
      <div ref={containerRef} className="min-h-0 w-full flex-1 overflow-hidden" />
    </div>
  );
}
