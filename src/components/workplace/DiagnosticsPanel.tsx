import { useCallback, useEffect, useRef, useState } from "react";
import {
  createProject,
  errorMessage,
  getDiagnostics,
  listProjects,
  onHookEvent,
  openLogDir,
  openProjectFolder,
  pickFolder,
  setProjectsRoot,
} from "../../lib/ipc";
import {
  coordinatorHint,
  countsByProject,
  countsFor,
  PROJECT_NAME_MAX,
  validateProjectName,
} from "../../lib/projects";
import { openFolderTitle } from "../../lib/platform";
import type { Diagnostics, LastHookEvent, LastToolCall } from "../../lib/types";
import { useStore } from "../../state/store";

export const DIAG_REFRESH_MS = 2000;

/** Danish labels in display order; keys are the raw field names (used in the copied text). */
const FIELDS: { key: keyof Diagnostics; label: string }[] = [
  { key: "appVersion", label: "App-version" },
  { key: "platform", label: "Platform" },
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
  { key: "pipeName", label: "Pipe/socket" },
  { key: "pipeReady", label: "Pipe lytter" },
  { key: "pipeNote", label: "Pipe-note" },
  { key: "framesReceived", label: "Hook-events modtaget" },
  { key: "framesUnknownSession", label: "Hook-events uden kendt agent" },
  { key: "lastHookEvent", label: "Sidste hook-event" },
  { key: "toolCalls", label: "Værktøjskald" },
  { key: "toolErrors", label: "Værktøjskald med fejl" },
  { key: "lastToolCall", label: "Sidste værktøjskald" },
  { key: "runningAgents", label: "Kørende agenter" },
  { key: "projectsRoot", label: "Projektrod" },
  { key: "projectsTotal", label: "Projekter" },
  { key: "workspaceFilePath", label: "Workspace-fil" },
  { key: "workspaceFileExists", label: "Workspace-fil findes" },
  { key: "workspaceWarning", label: "Workspace-advarsel" },
  { key: "ticketsPath", label: "Tickets-fil" },
  { key: "ticketsTotal", label: "Tickets" },
  { key: "ticketsWarning", label: "Tickets-advarsel" },
  { key: "ticketsReadOnly", label: "Tickets skrivebeskyttet" },
  { key: "profilesPath", label: "Profiler" },
  { key: "profilesLoaded", label: "Profiler indlæst" },
  { key: "profilesWarning", label: "Profil-advarsel" },
  { key: "profilesMigrated", label: "Profiler kopieret ved start" },
  { key: "reviewAssignmentsOpen", label: "Åbne reviews (agenter)" },
  { key: "ticketsEscalated", label: "Eskalerede tickets" },
  { key: "reportsTotal", label: "Rapporter" },
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
  if (d.pipeNote) out.push(d.pipeNote);
  if (!d.settingsExists) out.push("settings.json mangler");
  if (d.mcpExe === null) {
    out.push("Agentværktøjer utilgængelige: mira-mcp mangler (sæt MIRA_MCP_EXE eller byg den med npm run build:hook)");
  } else if (!d.mcpConfigExists) {
    out.push("mcp.json mangler");
  }
  if (d.ticketsWarning !== null) out.push(d.ticketsWarning);
  if (d.workspaceWarning !== null) out.push(d.workspaceWarning);
  if (d.profilesWarning !== null) out.push(d.profilesWarning);
  if (d.ticketsEscalated > 0) {
    out.push(
      `${d.ticketsEscalated} ${d.ticketsEscalated === 1 ? "ticket er eskaleret" : "tickets er eskaleret"} efter 3 afvisninger — afgør dem under Tickets`,
    );
  }
  if (d.framesReceived === 0 && d.runningAgents > 0) {
    out.push(
      "Ingen hook-events modtaget endnu — hvis en agent står på 'Starter', så svar på trust-spørgsmålet i dens terminal",
    );
  }
  return out;
}

/** Mounted only while its tab is visible, so the refresh timer stops with the tab. */
export default function DiagnosticsPanel() {
  const { state, dispatch } = useStore();
  const [diag, setDiag] = useState<Diagnostics | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const [fallback, setFallback] = useState<string | null>(null);
  const fallbackRef = useRef<HTMLTextAreaElement>(null);
  const alive = useRef(true);

  const refresh = useCallback(async () => {
    // The project list (a cheap readdir) refreshes with the diagnostics, into the store.
    listProjects()
      .then((projects) => {
        if (alive.current) dispatch({ type: "projects/set", projects });
      })
      .catch(() => {});
    try {
      const d = await getDiagnostics();
      if (!alive.current) return;
      setDiag(d);
      setLoadError(null);
    } catch (e) {
      if (alive.current) setLoadError(errorMessage(e));
    }
  }, [dispatch]);

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
  const counts = countsByProject(state.agents, state.tickets);

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
          title={openFolderTitle("mappen med logfilen")}
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

      <ProjectsSection
        counts={counts}
        hintOf={(id) => coordinatorHint(state.agents, id)}
        btn={btn}
        onChanged={() => void refresh()}
      />

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

// TODO(windows-verify): "Vælg projektrod…" opens the folder picker in front of the workplace,
// stores the path in %APPDATA%\dk.mira.bots\app-settings.json, and only after a restart do
// profiles, the workspace file and new agents live under the new root (plan4b D.86).
/** "Projekter": the list with counts and buttons, "Nyt projekt…", the projects root. */
function ProjectsSection(props: {
  counts: ReturnType<typeof countsByProject>;
  hintOf: (id: string) => string | null;
  btn: string;
  onChanged: () => void;
}) {
  const { counts, hintOf, btn, onChanged } = props;
  const { state } = useStore();
  const [creating, setCreating] = useState(false);
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const nameError = name === "" ? null : validateProjectName(name);

  const run = async (action: () => Promise<void>) => {
    setBusy(true);
    setError(null);
    try {
      await action();
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  const create = () =>
    run(async () => {
      await createProject(name);
      setName("");
      setCreating(false);
      onChanged();
    });

  const chooseRoot = () =>
    run(async () => {
      const path = await pickFolder("Vælg projektrod");
      if (path === null) return;
      const stored = await setProjectsRoot(path);
      setNotice(`Projektroden er gemt: ${stored}. Gælder efter genstart af mira-bots.`);
    });

  return (
    <section className="space-y-2" aria-labelledby="diag-projects">
      <h3 id="diag-projects" className="text-[11px] font-semibold uppercase tracking-wide text-[var(--muted)]">
        Projekter
      </h3>
      <div className="flex flex-wrap gap-2">
        <button
          type="button"
          onClick={() => setCreating((c) => !c)}
          aria-expanded={creating}
          title="Opret en projektmappe under projektroden"
          className={btn}
        >
          Nyt projekt…
        </button>
        <button
          type="button"
          onClick={() => void run(() => openProjectFolder(null))}
          title={openFolderTitle("projektroden")}
          className={btn}
        >
          Åbn projektroden
        </button>
        <button
          type="button"
          onClick={() => void chooseRoot()}
          disabled={busy}
          title="Vælg en anden mappe som projektrod (gælder efter genstart)"
          className={btn}
        >
          Vælg projektrod…
        </button>
      </div>
      {creating && (
        <form
          className="flex items-start gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            if (name !== "" && nameError === null && !busy) void create();
          }}
        >
          <div className="min-w-0 flex-1">
            <input
              value={name}
              onChange={(e) => setName(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Escape") {
                  e.preventDefault();
                  setCreating(false);
                }
              }}
              maxLength={PROJECT_NAME_MAX}
              placeholder="mappenavn, fx min-app"
              aria-label="Navn på det nye projekt"
              aria-invalid={nameError !== null}
              autoFocus
              className="block w-full rounded-md border border-[var(--border)] bg-[var(--bg)] p-1.5 text-xs outline-none focus:border-[var(--accent)]"
            />
            {nameError !== null && <p className="mt-0.5 text-[11px] text-rose-500">{nameError}</p>}
          </div>
          <button type="submit" disabled={busy || name === "" || nameError !== null} className={btn}>
            Opret
          </button>
        </form>
      )}
      {error !== null && (
        <p className="text-rose-500" role="alert">
          {error}
        </p>
      )}
      {notice !== null && (
        <p
          className="rounded-lg border border-emerald-500/40 bg-emerald-400/15 px-2 py-1 text-emerald-800 dark:text-emerald-200"
          role="status"
        >
          {notice}
        </p>
      )}
      {state.projects.length === 0 ? (
        <p className="text-[var(--muted)]">Ingen projekter endnu</p>
      ) : (
        <ul className="space-y-1">
          {state.projects.map((p) => {
            const c = countsFor(counts, p.id);
            const hint = hintOf(p.id);
            return (
              <li
                key={p.id}
                className="flex items-center gap-2 rounded-lg border border-[var(--border)] px-2 py-1"
              >
                <span className="min-w-0 flex-1">
                  <span className="block truncate" title={p.path}>
                    <span className="font-medium">{p.id}</span>
                    <span className="text-[var(--muted)]">
                      {" "}
                      · {c.agents} {c.agents === 1 ? "agent" : "agenter"} · {c.tickets}{" "}
                      {c.tickets === 1 ? "ticket" : "tickets"}
                    </span>
                  </span>
                  {hint !== null && (
                    <span className="block text-[11px] text-amber-700 dark:text-amber-300">⚠ {hint}</span>
                  )}
                </span>
                <button
                  type="button"
                  onClick={() => void run(() => openProjectFolder(p.id))}
                  title={openFolderTitle(p.path)}
                  aria-label={`Åbn mappen for projektet ${p.id}`}
                  className={btn}
                >
                  Åbn mappe
                </button>
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}
