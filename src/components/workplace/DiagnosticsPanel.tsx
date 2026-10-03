import { useCallback, useEffect, useRef, useState } from "react";
import {
  checkGhAuth,
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
import { sourceDiagCopyLines, sourceDiagRows } from "../../lib/inbox";
import { openFolderTitle } from "../../lib/platform";
import type {
  Diagnostics,
  GhAuthResult,
  InboxSourceStatus,
  LastHookEvent,
  LastToolCall,
  WorkspaceRules,
} from "../../lib/types";
import { useStore } from "../../state/store";

export const DIAG_REFRESH_MS = 2000;

/** The fields shown as plain rows; `inboxSources` has its own section ("Kilder"). */
type FieldKey = Exclude<keyof Diagnostics, "inboxSources">;

/** Danish labels in display order; keys are the raw field names (used in the copied text). */
const FIELDS: { key: FieldKey; label: string }[] = [
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
  // Step 6c: the inbox and `gh`.
  { key: "ghPath", label: "gh-sti" },
  { key: "ghVersion", label: "gh-version" },
  { key: "ghVersionNote", label: "gh-note" },
  { key: "inboxPath", label: "Indbakke-fil" },
  { key: "inboxWarning", label: "Indbakke-advarsel" },
  { key: "inboxNew", label: "Nye emner i indbakken" },
  { key: "logPath", label: "Logfil" },
];

function formatLast(e: LastHookEvent): string {
  return `${e.name} ${e.sessionId} ${e.agentId ?? "-"} ${new Date(e.at).toISOString()}`;
}

function formatLastTool(c: LastToolCall): string {
  return `${c.tool} ${c.agentId ?? "-"} ${c.ok ? "ok" : "fejl"} ${new Date(c.at).toISOString()}`;
}

function formatValue(v: Diagnostics[FieldKey]): string {
  if (v === null) return "–";
  if (typeof v === "boolean") return v ? "ja" : "nej";
  if (typeof v === "object") return "tool" in v ? formatLastTool(v) : formatLast(v);
  return String(v);
}

/** The step 6b workspace rules shown after the workspace fields (raw key, label, value). */
function ruleRows(rules: WorkspaceRules | undefined): { key: string; label: string; value: string }[] {
  if (rules === undefined) return [];
  const yesNo = (b: boolean) => (b ? "ja" : "nej");
  return [
    { key: "rules.git", label: "Git pr. ticket", value: rules.git },
    { key: "rules.checksGate", label: "Projekt-tjek afviser ved fejl", value: yesNo(rules.checksGate) },
    {
      key: "rules.freshSessionPerTicket",
      label: "Ny session pr. ticket",
      value: yesNo(rules.freshSessionPerTicket),
    },
  ];
}

function copyText(
  d: Diagnostics,
  rules: WorkspaceRules | undefined,
  sources: InboxSourceStatus[] | null,
): string {
  const lines = [`mira-bots ${d.appVersion}`];
  for (const f of FIELDS) lines.push(`${f.key}: ${formatValue(d[f.key])}`);
  for (const r of ruleRows(rules)) lines.push(`${r.key}: ${r.value}`);
  lines.push(...sourceDiagCopyLines(sourceDiagRows(d.inboxSources, sources)));
  return lines.join("\n");
}

function warningsFor(d: Diagnostics, maxReviewRounds: number): string[] {
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
  if (d.inboxWarning !== null) out.push(d.inboxWarning);
  if (d.ticketsEscalated > 0) {
    out.push(
      `${d.ticketsEscalated} ${d.ticketsEscalated === 1 ? "ticket er eskaleret" : "tickets er eskaleret"} efter ${maxReviewRounds} afvisninger — afgør dem under Tickets`,
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
    const text = copyText(diag, state.appInfo?.rules, state.inbox?.status.sources ?? null);
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
  const warnings = diag === null ? [] : warningsFor(diag, state.appInfo?.rules.maxReviewRounds ?? 3);
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

      {diag !== null && (
        <InboxSourcesSection diag={diag} sources={state.inbox?.status.sources ?? null} btn={btn} />
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
          {ruleRows(state.appInfo?.rules).map((r) => (
            <div key={r.key}>
              <dt className="text-[var(--muted)]">{r.label}</dt>
              <dd className="break-all font-mono text-[11px] select-text" title={r.value}>
                {r.value}
              </dd>
            </div>
          ))}
        </dl>
      )}
    </div>
  );
}

/**
 * "Kilder" (step 6c): one row per inbox source and project (label, project, source id, latest
 * fetch, error, notes of the latest fetch) and "Tjek gh-login", which runs `gh auth status` only
 * on a click (never automatically) and shows its output without token lines.
 */
// TODO(windows-verify): "Tjek gh-login" shows the account without a token, also when gh was
// installed after the app started (plan6c D.108).
function InboxSourcesSection({
  diag,
  sources,
  btn,
}: {
  diag: Diagnostics;
  sources: InboxSourceStatus[] | null;
  btn: string;
}) {
  const rows = sourceDiagRows(diag.inboxSources, sources);
  const [checking, setChecking] = useState(false);
  const [auth, setAuth] = useState<GhAuthResult | null>(null);
  const [authError, setAuthError] = useState<string | null>(null);
  const alive = useRef(true);
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);

  const check = async () => {
    setChecking(true);
    setAuthError(null);
    try {
      const r = await checkGhAuth();
      if (alive.current) setAuth(r);
    } catch (e) {
      if (alive.current) {
        setAuth(null);
        setAuthError(errorMessage(e));
      }
    } finally {
      if (alive.current) setChecking(false);
    }
  };

  return (
    <section className="space-y-2" aria-labelledby="diag-sources">
      <h3 id="diag-sources" className="text-[11px] font-semibold uppercase tracking-wide text-[var(--muted)]">
        Kilder
      </h3>
      {rows.length === 0 ? (
        <p className="text-[var(--muted)]">Ingen indbakke-kilder</p>
      ) : (
        <ul className="space-y-1">
          {rows.map((r) => (
            <li key={r.key} className="rounded-lg border border-[var(--border)] px-2 py-1">
              <div className="flex flex-wrap items-baseline gap-x-2">
                <span className="font-medium">{r.label}</span>
                <span className="text-[var(--muted)]">{r.project}</span>
                <span className="ml-auto text-[11px] text-[var(--muted)]">{r.fetched}</span>
              </div>
              {r.id !== null && (
                <div className="break-all font-mono text-[11px] text-[var(--muted)] select-text">
                  {r.id} · {r.items} {r.items === 1 ? "emne" : "emner"}
                </div>
              )}
              {r.error !== null && (
                <p className="mt-0.5 break-words text-rose-600 dark:text-rose-300">{r.error}</p>
              )}
              {r.notes.map((n, i) => (
                <p key={`${i}:${n}`} className="text-[11px] text-amber-700 dark:text-amber-300">
                  ⚠ {n}
                </p>
              ))}
            </li>
          ))}
        </ul>
      )}
      <button
        type="button"
        onClick={() => void check()}
        disabled={checking}
        title="Kør gh auth status (viser kontoen, aldrig et token)"
        className={btn}
      >
        {checking ? "Tjekker…" : "Tjek gh-login"}
      </button>
      {authError !== null && (
        <p className="text-rose-500" role="alert">
          {authError}
        </p>
      )}
      {auth !== null && (
        <pre
          className={`whitespace-pre-wrap break-all rounded-lg border p-2 font-mono text-[11px] select-text ${
            auth.ok
              ? "border-emerald-500/40 bg-emerald-400/10 text-emerald-800 dark:text-emerald-200"
              : "border-rose-500/40 bg-rose-400/10 text-rose-700 dark:text-rose-300"
          }`}
          role="status"
        >
          {auth.text}
        </pre>
      )}
    </section>
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
