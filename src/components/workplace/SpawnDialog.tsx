import { useEffect, useState } from "react";
import type { Theme } from "../../lib/bots";
import {
  errorMessage,
  listProfiles,
  pickFolder,
  spawnAgent,
  spawnAgentWithTicket,
} from "../../lib/ipc";
import { effortLabel, modelLabel } from "../../lib/models";
import { folderPrefix, isSpecialist, rolesText } from "../../lib/roles";
import type { AgentProfile, Effort, SeatKind, SpawnOverrides, TicketSummary } from "../../lib/types";
import { useStore } from "../../state/store";
import BotFigure from "../BotFigure";
import ModelPicker, { EffortSelect, modelChoiceValid } from "./agents/ModelPicker";
import StickyNote from "./tickets/StickyNote";

/** Profile used when none is chosen (mirrors `DEFAULT_PROFILE_ID` in Rust). */
const DEFAULT_PROFILE = "coder";

/** Folder name prefix of the default folder (mirrors `roles::prefix_for` in Rust). */
function profilePrefix(p: AgentProfile | undefined): string {
  return p === undefined ? DEFAULT_PROFILE : folderPrefix(p.roles, isSpecialist(p));
}

const SEAT_TEXT: Record<SeatKind, string> = { work: "arbejdsplads", staff: "stabsplads" };

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
  const { state, dispatch } = useStore();
  const profiles = state.profiles;
  const [profileId, setProfileId] = useState<string>(DEFAULT_PROFILE);
  const [overrideModel, setOverrideModel] = useState<string | null>(null);
  const [overrideEffort, setOverrideEffort] = useState<Effort | null>(null);
  const [folderMode, setFolderMode] = useState<"default" | "custom">("default");
  const [folder, setFolder] = useState<string | null>(null);
  const [prompt, setPrompt] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // The store loads the profiles at start; fetch them here only if that has not happened.
  const empty = profiles.length === 0;
  useEffect(() => {
    if (!empty) return;
    listProfiles()
      .then((list) => dispatch({ type: "profiles/set", profiles: list }))
      .catch((e: unknown) => setError(errorMessage(e)));
  }, [empty, dispatch]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !busy) onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [busy, onClose]);

  const sep = agentsRoot !== null && agentsRoot.includes("\\") ? "\\" : "/";
  const profile = profiles.find((p) => p.id === profileId);
  const defaultPath = `${agentsRoot ?? "…"}${sep}${profilePrefix(profile)}-nn`;

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
      const overrides: SpawnOverrides | null =
        overrideModel !== null || overrideEffort !== null
          ? { model: overrideModel, effort: overrideEffort }
          : null;
      const agent =
        ticket !== null
          ? await spawnAgentWithTicket(ticket.id, profileId, overrides, cwd, seatKind)
          : await spawnAgent(profileId, overrides, cwd, text === "" ? null : text, seatKind);
      onSpawned(agent.id);
    } catch (e) {
      setError(errorMessage(e));
      setBusy(false);
    }
  };

  const overridesValid = modelChoiceValid(overrideModel);
  const canStart = !busy && overridesValid && (folderMode === "default" || folder !== null);
  // The seat the user clicked wins over the profile's default seat.
  const seatDiffers = profile !== undefined && profile.defaultSeat !== seatKind;

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
          <legend className="mb-2 text-xs font-medium text-[var(--muted)]">Profil</legend>
          <div className="grid grid-cols-4 gap-2">
            {profiles.map((p) => (
              <button
                key={p.id}
                type="button"
                onClick={() => setProfileId(p.id)}
                aria-pressed={profileId === p.id}
                title={`${rolesText(p.roles)}\nModel: ${modelLabel(p.model)} · Effort: ${effortLabel(p.effort)}`}
                className={`flex flex-col items-center gap-1 rounded-xl border p-2 ${
                  profileId === p.id
                    ? "border-[var(--accent)] ring-2 ring-[var(--accent)]/40"
                    : "border-[var(--border)] hover:border-[var(--accent)]"
                }`}
              >
                <BotFigure
                  roles={p.roles}
                  specialist={isSpecialist(p)}
                  state="idle"
                  theme={theme}
                  size={56}
                />
                <span className="w-full truncate text-center text-[11px]">{p.name}</span>
              </button>
            ))}
          </div>
          {seatDiffers && profile !== undefined && (
            <p className="mt-1.5 text-[11px] text-[var(--muted)]">
              {profile.name} står normalt på en {SEAT_TEXT[profile.defaultSeat]}; agenten starter på
              den {SEAT_TEXT[seatKind]} du valgte.
            </p>
          )}
        </fieldset>

        <details className="mt-3">
          <summary className="cursor-pointer select-none text-xs text-[var(--muted)] hover:text-[var(--fg)]">
            Overskriv model/effort
            {(overrideModel !== null || overrideEffort !== null) && " (ændret)"}
          </summary>
          <div className="mt-2 grid grid-cols-2 gap-3">
            <ModelPicker
              key={profileId}
              value={overrideModel}
              onChange={setOverrideModel}
              standardLabel={`Fra profilen (${modelLabel(profile?.model ?? null)})`}
            />
            <EffortSelect
              value={overrideEffort}
              onChange={setOverrideEffort}
              standardLabel={`Fra profilen (${effortLabel(profile?.effort ?? null)})`}
            />
          </div>
          <p className="mt-1 text-[11px] text-[var(--muted)]">Gælder kun denne agent; profilen ændres ikke.</p>
        </details>

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
