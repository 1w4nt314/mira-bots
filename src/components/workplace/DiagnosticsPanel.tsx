import { useCallback, useEffect, useRef, useState } from "react";
import { errorMessage, getDiagnostics, onHookEvent, openLogDir } from "../../lib/ipc";
import type { Diagnostics, LastHookEvent, LastToolCall } from "../../lib/types";
import { useStore } from "../../state/store";

export const DIAG_REFRESH_MS = 2000;

/** Danish labels in display order; keys are the raw field names (used in the copied text). */
const FIELDS: { key: keyof Diagnostics; label: string }[] = [
  { key: "appVersion", label: "App-version" },
  { key: "claudePath", label: "Claude Code-sti" },
  { key: "claudeVersion", label: "Claude Code-version" },
  { key: "claudeVersionNote", label: "Versionsnote" },
  { key: "claudeCodeArgsSupported", label: "Hooks med args understøttet" },
  { key: "claudeCodeMcpSupported", label: "Agentværktøjer understøttet (≥ 2.1.274)" },
  { key: "hookExe", label: "Hook-program" },
  { key: "settingsPath", label: "settings.json" },
  { key: "settingsExists", label: "settings.json findes" },
  { key: "mcpExe", label: "MCP-server (mira-mcp)" },
  { key: "mcpConfigPath", label: "mcp.json" },
  { key: "mcpConfigExists", label: "mcp.json findes" },
  { key: "systemPromptPath", label: "Systemprompt-fil" },
  { key: "autoReviewOnStop", label: "Stop sender til review automatisk" },
  { key: "pipeName", label: "Pipe" },
  { key: "pipeReady", label: "Pipe lytter" },
  { key: "framesReceived", label: "Hook-events modtaget" },
  { key: "framesUnknownSession", label: "Hook-events uden kendt agent" },
  { key: "lastHookEvent", label: "Sidste hook-event" },
  { key: "toolCalls", label: "Værktøjskald" },
  { key: "toolErrors", label: "Værktøjskald med fejl" },
  { key: "lastToolCall", label: "Sidste værktøjskald" },
  { key: "runningAgents", label: "Kørende agenter" },
  { key: "agentsRoot", label: "Agentmappe" },
  { key: "ticketsPath", label: "Tickets-fil" },
  { key: "ticketsTotal", label: "Tickets" },
  { key: "ticketsWarning", label: "Tickets-advarsel" },
  { key: "ticketsReadOnly", label: "Tickets skrivebeskyttet" },
  { key: "logPath", label: "Logfil" },
];

function formatLast(e: LastHookEvent): string {
  return `${e.name} ${e.sessionId} ${e.agentId ?? "-"} ${new Date(e.at).toISOString()}`;
}

function formatLastTool(c: LastToolCall): string {
  return `${c.tool} ${c.agentId ?? "-"} ${c.ok ? "ok" : "fejl"} ${new Date(c.at).toISOString()}`;
}

function formatValue(v: Diagnostics[keyof Diagnostics]): string {
  if (v === null) return "–";
  if (typeof v === "boolean") return v ? "ja" : "nej";
  if (typeof v === "object") return "tool" in v ? formatLastTool(v) : formatLast(v);
  return String(v);
}

function copyText(d: Diagnostics): string {
  const lines = [`mira-bots ${d.appVersion}`];
  for (const f of FIELDS) lines.push(`${f.key}: ${formatValue(d[f.key])}`);
  return lines.join("\n");
}

function warningsFor(d: Diagnostics): string[] {
  const out: string[] = [];
  if (d.claudeCodeArgsSupported === false) {
    out.push(
      `Claude Code ${d.claudeVersion ?? ""} er ældre end 2.1.139: hooks med \`args\` understøttes ikke — opdater Claude Code`,
    );
  }
  if (d.claudeCodeMcpSupported === false && d.claudeCodeArgsSupported !== false) {
    out.push(
      `Claude Code ${d.claudeVersion ?? ""} er ældre end 2.1.274: agentværktøjerne (mira_*) er ikke verificeret — opdater Claude Code`,
    );
  }
  if (!d.pipeReady) out.push("Hook-forbindelsen lytter ikke");
  if (!d.settingsExists) out.push("settings.json mangler");
  if (d.mcpExe === null) {
    out.push("Agentværktøjer utilgængelige: mira-mcp mangler (sæt MIRA_MCP_EXE eller byg den med npm run build:hook)");
  } else if (!d.mcpConfigExists) {
    out.push("mcp.json mangler");
  }
  if (d.ticketsWarning !== null) out.push(d.ticketsWarning);
  if (d.framesReceived === 0 && d.runningAgents > 0) {
    out.push(
      "Ingen hook-events modtaget endnu — hvis en agent står på 'Starter', så svar på trust-spørgsmålet i dens terminal",
    );
  }
  return out;
}

/** Mounted only while its tab is visible, so the refresh timer stops with the tab. */
export default function DiagnosticsPanel() {
  const { dispatch } = useStore();
  const [diag, setDiag] = useState<Diagnostics | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const [fallback, setFallback] = useState<string | null>(null);
  const fallbackRef = useRef<HTMLTextAreaElement>(null);
  const alive = useRef(true);

  const refresh = useCallback(async () => {
    try {
      const d = await getDiagnostics();
      if (!alive.current) return;
      setDiag(d);
      setLoadError(null);
    } catch (e) {
      if (alive.current) setLoadError(errorMessage(e));
    }
  }, []);

  useEffect(() => {
    alive.current = true;
    let unlisten: (() => void) | null = null;
    void refresh();
    const t = setInterval(() => void refresh(), DIAG_REFRESH_MS);
    onHookEvent(() => void refresh())
      .then((u) => {
        if (alive.current) unlisten = u;
        else u();
      })
      .catch(() => {});
    return () => {
      alive.current = false;
      clearInterval(t);
      unlisten?.();
    };
  }, [refresh]);

  useEffect(() => {
    if (!copied) return;
    const t = setTimeout(() => setCopied(false), 2000);
    return () => clearTimeout(t);
  }, [copied]);

  useEffect(() => {
    if (fallback !== null) fallbackRef.current?.select();
  }, [fallback]);

  // TODO(windows-verify): navigator.clipboard works in WebView2 (secure context); otherwise the
  // selected fallback text field is shown (plan D.26).
  const copy = async () => {
    if (diag === null) return;
    const text = copyText(diag);
    try {
      await navigator.clipboard.writeText(text);
      setFallback(null);
      setCopied(true);
    } catch {
      setFallback(text);
    }
  };

  const openLogs = async () => {
    try {
      await openLogDir();
    } catch (e) {
      dispatch({ type: "error/set", error: errorMessage(e) });
    }
  };

  const btn =
    "rounded-md border border-[var(--border)] px-2.5 py-1 text-xs hover:border-[var(--accent)] disabled:opacity-50";
  const warnings = diag === null ? [] : warningsFor(diag);

  return (
    <div className="space-y-3 p-3 text-xs">
      <div className="flex flex-wrap gap-2">
        <button type="button" onClick={() => void refresh()} title="Hent diagnostik igen" className={btn}>
          Opdatér
        </button>
        <button
          type="button"
          onClick={() => void copy()}
          disabled={diag === null}
          title="Kopiér diagnostikken som tekst (til en fejlrapport)"
          className={btn}
        >
          {copied ? "Kopieret" : "Kopiér"}
        </button>
        <button
          type="button"
          onClick={() => void openLogs()}
          title="Åbn mappen med logfilen i Stifinder"
          className={btn}
        >
          Åbn logmappe
        </button>
      </div>

      {fallback !== null && (
        <div>
          <p className="mb-1 text-[var(--muted)]">Kunne ikke kopiere automatisk — tryk Ctrl+C:</p>
          <textarea
            ref={fallbackRef}
            readOnly
            value={fallback}
            rows={8}
            className="block w-full resize-y rounded-lg border border-[var(--border)] bg-[var(--bg)] p-2 font-mono text-[11px]"
          />
        </div>
      )}

      {loadError !== null && <p className="text-rose-500" role="alert">{loadError}</p>}

      {warnings.length > 0 && (
        <ul className="space-y-1">
          {warnings.map((w) => (
            <li
              key={w}
              className="rounded-lg border border-amber-400/50 bg-amber-300/20 px-2 py-1 text-amber-800 dark:text-amber-200"
            >
              {w}
            </li>
          ))}
        </ul>
      )}

      {diag === null ? (
        loadError === null && <p className="text-[var(--muted)]">Henter diagnostik…</p>
      ) : (
        <dl className="space-y-1.5">
          {FIELDS.map((f) => {
            const value = formatValue(diag[f.key]);
            return (
              <div key={f.key}>
                <dt className="text-[var(--muted)]">{f.label}</dt>
                <dd className="break-all font-mono text-[11px] select-text" title={value}>
                  {value}
                </dd>
              </div>
            );
          })}
        </dl>
      )}
    </div>
  );
}
