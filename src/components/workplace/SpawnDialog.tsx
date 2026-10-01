import { useEffect, useState } from "react";
import type { Theme } from "../../lib/bots";
import { errorMessage, pickFolder, spawnAgent, spawnAgentWithTicket } from "../../lib/ipc";
import type { AgentRole, SeatKind, TicketSummary } from "../../lib/types";
import BotFigure from "../BotFigure";
import { ROLE_LABEL } from "./Seat";
import StickyNote from "./tickets/StickyNote";

const ROLES: AgentRole[] = ["none", "coder", "researcher", "reviewer", "koord"];

/** Folder name prefix of the default folder (mirrors `AgentRole::prefix` in Rust). */
function rolePrefix(role: AgentRole): string {
  return role === "none" ? "bot" : role;
}

interface Props {
  seatKind: SeatKind;
  theme: Theme;
  agentsRoot: string | null;
  /** Start the agent with this ticket (its line becomes the first prompt) instead of a prompt. */
  ticket?: TicketSummary | null;
  onClose: () => void;
  onSpawned: (agentId: string) => void;
}

export default function SpawnDialog(props: Props) {
  const { seatKind, theme, agentsRoot, ticket = null, onClose, onSpawned } = props;
  const [role, setRole] = useState<AgentRole>("none");
  const [folderMode, setFolderMode] = useState<"default" | "custom">("default");
  const [folder, setFolder] = useState<string | null>(null);
  const [prompt, setPrompt] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !busy) onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [busy, onClose]);

  const sep = agentsRoot !== null && agentsRoot.includes("\\") ? "\\" : "/";
  const defaultPath = `${agentsRoot ?? "…"}${sep}${rolePrefix(role)}-nn`;

  const choose = async () => {
    try {
      const picked = await pickFolder();
      if (picked !== null) {
        setFolder(picked);
        setFolderMode("custom");
      }
    } catch (e) {
      setError(errorMessage(e));
    }
  };

  const start = async () => {
    setBusy(true);
    setError(null);
    try {
      const cwd = folderMode === "custom" ? folder : null;
      const text = prompt.trim();
      const agent =
        ticket !== null
          ? await spawnAgentWithTicket(ticket.id, cwd, role, seatKind)
          : await spawnAgent(cwd, text === "" ? null : text, role, seatKind);
      onSpawned(agent.id);
    } catch (e) {
      setError(errorMessage(e));
      setBusy(false);
    }
  };

  const canStart = !busy && (folderMode === "default" || folder !== null);

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget && !busy) onClose();
      }}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby="spawn-title"
        className="w-[560px] max-w-[calc(100vw-32px)] rounded-2xl border border-[var(--border)] bg-[var(--panel)] p-5 text-sm shadow-xl"
      >
        <h2 id="spawn-title" className="text-base font-semibold">
          {ticket !== null
            ? `Ny agent til ticket ${ticket.shortId}`
            : `Ny agent på ${seatKind === "work" ? "en arbejdsplads" : "en stabsplads"}`}
        </h2>
        {ticket !== null && (
          <p className="mt-0.5 text-xs text-[var(--muted)]">
            {seatKind === "work" ? "Arbejdsplads" : "Stabsplads"} · agenten får ticketen som sin
            første opgave
          </p>
        )}

        <fieldset className="mt-4">
          <legend className="mb-2 text-xs font-medium text-[var(--muted)]">Rolle</legend>
          <div className="grid grid-cols-5 gap-2">
            {ROLES.map((r) => (
              <button
                key={r}
                type="button"
                onClick={() => setRole(r)}
                aria-pressed={role === r}
                title={ROLE_LABEL[r]}
                className={`flex flex-col items-center gap-1 rounded-xl border p-2 ${
                  role === r
                    ? "border-[var(--accent)] ring-2 ring-[var(--accent)]/40"
                    : "border-[var(--border)] hover:border-[var(--accent)]"
                }`}
              >
                <BotFigure role={r} state="idle" theme={theme} size={64} />
                <span className="text-[11px]">{ROLE_LABEL[r]}</span>
              </button>
            ))}
          </div>
          <p className="mt-1 text-[11px] text-[var(--muted)]">
            Rollen er kun visuel i denne version (figur og mappenavn).
          </p>
        </fieldset>

        <fieldset className="mt-4 space-y-1.5">
          <legend className="mb-1 text-xs font-medium text-[var(--muted)]">Mappe</legend>
          <label className="flex items-center gap-2">
            <input
              type="radio"
              name="folder"
              checked={folderMode === "default"}
              onChange={() => setFolderMode("default")}
            />
            <span>
              Standardmappe <span className="font-mono text-xs text-[var(--muted)]">{defaultPath}</span>
            </span>
          </label>
          <label className="flex items-center gap-2">
            <input
              type="radio"
              name="folder"
              checked={folderMode === "custom"}
              onChange={() => (folder === null ? void choose() : setFolderMode("custom"))}
            />
            <span className="flex min-w-0 items-center gap-2">
              <button
                type="button"
                onClick={() => void choose()}
                title="Vælg en eksisterende mappe"
                className="shrink-0 rounded-md border border-[var(--border)] px-2 py-0.5 text-xs hover:border-[var(--accent)]"
              >
                Vælg mappe…
              </button>
              {folder !== null && (
                <span className="truncate font-mono text-xs text-[var(--muted)]" title={folder}>
                  {folder}
                </span>
              )}
            </span>
          </label>
        </fieldset>

        {ticket !== null ? (
          <div className="mt-4">
            <span className="text-xs font-medium text-[var(--muted)]">Ticket</span>
            <div className="mt-1">
              <StickyNote ticket={ticket} agent={null} draggable={false} compact interactive={false} />
            </div>
          </div>
        ) : (
          <label className="mt-4 block">
            <span className="text-xs font-medium text-[var(--muted)]">Første prompt (valgfri)</span>
            <textarea
              value={prompt}
              onChange={(e) => setPrompt(e.target.value)}
              rows={3}
              placeholder="Hvad skal agenten starte med? (må ikke begynde med '-')"
              className="mt-1 block w-full resize-y rounded-lg border border-[var(--border)] bg-[var(--bg)] p-2 text-sm outline-none focus:border-[var(--accent)]"
            />
          </label>
        )}

        {error !== null && (
          <p className="mt-3 text-xs text-rose-500" role="alert">
            {error}
          </p>
        )}

        <div className="mt-5 flex justify-end gap-2">
          <button
            type="button"
            onClick={onClose}
            disabled={busy}
            title="Luk uden at starte en agent"
            className="rounded-md px-3 py-1.5 text-sm hover:bg-neutral-500/15 disabled:opacity-50"
          >
            Annuller
          </button>
          <button
            type="button"
            onClick={() => void start()}
            disabled={!canStart}
            title="Start agenten"
            className="rounded-md bg-[var(--accent)] px-3 py-1.5 text-sm font-medium text-white hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-50"
          >
            {busy ? "Starter…" : ticket !== null ? "Start med ticket" : "Start"}
          </button>
        </div>
      </div>
    </div>
  );
}
