import { useEffect, useId, useState } from "react";
import type { Theme } from "../../../lib/bots";
import { errorMessage, saveProfile } from "../../../lib/ipc";
import { PROFILE_NAME_MAX, PROMPT_APPEND_MAX } from "../../../lib/models";
import { isSpecialist, ROLE_LABEL, ROLE_ORDER, sortRoles } from "../../../lib/roles";
import type { AgentProfile, Effort, Role, SeatKind } from "../../../lib/types";
import BotFigure from "../../BotFigure";
import ModelPicker, { EffortSelect, modelChoiceValid } from "./ModelPicker";

interface Props {
  /** null: a new custom profile. */
  profile: AgentProfile | null;
  theme: Theme;
  onClose: () => void;
  onSaved: (profile: AgentProfile) => void;
}

const field =
  "block w-full rounded-lg border border-[var(--border)] bg-[var(--bg)] p-2 text-sm outline-none focus:border-[var(--accent)]";

/** Client-side checks that mirror the backend (C5.6); the backend's own text wins on save. */
function validate(name: string, promptAppend: string, model: string | null): string | null {
  const n = [...name.trim()].length;
  if (n === 0 || n > PROFILE_NAME_MAX) return "Navn skal være 1–60 tegn";
  if (!modelChoiceValid(model)) {
    return model === "" ? "Skriv et model-id, eller vælg Standard" : "Ukendt model";
  }
  if ([...promptAppend].length > PROMPT_APPEND_MAX) {
    return "Prompt-tillægget er for langt (maks 4000 tegn)";
  }
  return null;
}

/**
 * Modal editor for one profile: name, roles, specialist figure, prompt addition, model, effort
 * and default seat, with a live figure preview (idle + work). Saving goes through `saveProfile`;
 * the backend validates again and its Danish error is shown as is.
 */
export default function ProfileEditor({ profile, theme, onClose, onSaved }: Props) {
  const nameId = useId();
  const promptId = useId();
  const [name, setName] = useState(profile?.name ?? "");
  const [roles, setRoles] = useState<Role[]>(profile?.roles ?? ["coder"]);
  const [specialistChoice, setSpecialistChoice] = useState(profile !== null ? isSpecialist(profile) : false);
  const [promptAppend, setPromptAppend] = useState(profile?.promptAppend ?? "");
  const [model, setModel] = useState<string | null>(profile?.model ?? null);
  const [effort, setEffort] = useState<Effort | null>(profile?.effort ?? null);
  const [seat, setSeat] = useState<SeatKind>(profile?.defaultSeat ?? "work");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !busy) onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [busy, onClose]);

  // More than one role is always a specialist (the figure combines the roles).
  const locked = roles.length > 1;
  const specialist = locked || specialistChoice;
  const invalid = validate(name, promptAppend, model);
  const promptLen = [...promptAppend].length;

  const toggleRole = (r: Role) =>
    setRoles((cur) => sortRoles(cur.includes(r) ? cur.filter((x) => x !== r) : [...cur, r]));

  const save = async () => {
    if (invalid !== null) {
      setError(invalid);
      return;
    }
    setBusy(true);
    setError(null);
    // Explicit only when it differs from the derived default (`roles.length !== 1`).
    const derived = roles.length !== 1;
    const next: AgentProfile = {
      id: profile?.id ?? "",
      kind: profile?.kind ?? "custom",
      name: name.trim(),
      roles: sortRoles(roles),
      specialist: specialist === derived ? null : specialist,
      promptAppend,
      model,
      effort,
      defaultSeat: seat,
      toolDeny: profile?.toolDeny ?? [],
      extraAllow: profile?.extraAllow ?? [],
      extraDeny: profile?.extraDeny ?? [],
      updatedAt: profile?.updatedAt ?? 0,
    };
    try {
      onSaved(await saveProfile(next));
    } catch (e) {
      setError(errorMessage(e));
      setBusy(false);
    }
  };

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
        aria-labelledby="profile-title"
        className="max-h-[calc(100vh-32px)] w-[620px] max-w-[calc(100vw-32px)] overflow-y-auto rounded-2xl border border-[var(--border)] bg-[var(--panel)] p-5 text-sm shadow-xl"
      >
        <h2 id="profile-title" className="text-base font-semibold">
          {profile === null ? "Ny profil" : `Rediger profil: ${profile.name}`}
        </h2>
        {profile?.kind === "builtin" && (
          <p className="mt-0.5 text-xs text-[var(--muted)]">
            Indbygget profil — "Nulstil" i listen gendanner standarden.
          </p>
        )}

        <div className="mt-4 flex gap-4">
          <div className="min-w-0 flex-1 space-y-3">
            <label htmlFor={nameId} className="block">
              <span className="text-xs font-medium text-[var(--muted)]">Navn</span>
              <input
                id={nameId}
                type="text"
                value={name}
                onChange={(e) => setName(e.target.value)}
                maxLength={PROFILE_NAME_MAX + 10}
                autoFocus
                className={`mt-1 ${field}`}
              />
            </label>

            <fieldset>
              <legend className="mb-1 text-xs font-medium text-[var(--muted)]">Roller</legend>
              <div className="grid grid-cols-3 gap-x-3 gap-y-1">
                {ROLE_ORDER.map((r) => (
                  <label key={r} className="flex items-center gap-1.5 text-xs">
                    <input type="checkbox" checked={roles.includes(r)} onChange={() => toggleRole(r)} />
                    {ROLE_LABEL[r]}
                  </label>
                ))}
              </div>
              <label
                className="mt-1.5 flex items-center gap-1.5 text-xs"
                title={locked ? "Flere roller giver altid en specialist-figur" : undefined}
              >
                <input
                  type="checkbox"
                  checked={specialist}
                  disabled={locked}
                  onChange={(e) => setSpecialistChoice(e.target.checked)}
                />
                Specialist (dynamisk figur)
              </label>
            </fieldset>
          </div>

          <div className="flex shrink-0 flex-col items-center gap-1" aria-label="Forhåndsvisning af figuren">
            <div className="flex gap-1">
              <BotFigure roles={roles} specialist={specialist} state="idle" theme={theme} size={96} />
              <BotFigure roles={roles} specialist={specialist} state="work" theme={theme} size={96} />
            </div>
            <span className="text-[10px] text-[var(--muted)]">Klar · Arbejder</span>
          </div>
        </div>

        <label htmlFor={promptId} className="mt-3 block">
          <span className="flex text-xs font-medium text-[var(--muted)]">
            <span className="flex-1">Prompt-tillæg (valgfrit)</span>
            <span className={promptLen > PROMPT_APPEND_MAX ? "text-rose-500" : ""}>
              {promptLen}/{PROMPT_APPEND_MAX}
            </span>
          </span>
          <textarea
            id={promptId}
            value={promptAppend}
            onChange={(e) => setPromptAppend(e.target.value)}
            rows={4}
            placeholder="Ekstra instruktioner til agenter fra denne profil (lægges efter rolleteksterne)"
            className={`mt-1 resize-y ${field}`}
          />
        </label>

        <div className="mt-3 grid grid-cols-2 gap-3">
          <ModelPicker value={model} onChange={setModel} />
          <EffortSelect value={effort} onChange={setEffort} />
        </div>

        <fieldset className="mt-3">
          <legend className="mb-1 text-xs font-medium text-[var(--muted)]">Standardplads</legend>
          <div className="flex gap-4 text-xs">
            {(["work", "staff"] as const).map((k) => (
              <label key={k} className="flex items-center gap-1.5">
                <input type="radio" name="default-seat" checked={seat === k} onChange={() => setSeat(k)} />
                {k === "work" ? "Arbejdsplads" : "Stabsplads"}
              </label>
            ))}
          </div>
        </fieldset>

        <p className="mt-3 text-[11px] text-[var(--muted)]">
          Ændringer gælder nye agenter; kørende agenter beholder deres roller.
        </p>

        {error !== null ? (
          <p className="mt-2 text-xs text-rose-500" role="alert">
            {error}
          </p>
        ) : (
          invalid !== null && <p className="mt-2 text-xs text-amber-600 dark:text-amber-300">{invalid}</p>
        )}

        <div className="mt-4 flex justify-end gap-2">
          <button
            type="button"
            onClick={onClose}
            disabled={busy}
            className="rounded-md px-3 py-1.5 text-sm hover:bg-neutral-500/15 disabled:opacity-50"
          >
            Annuller
          </button>
          <button
            type="button"
            onClick={() => void save()}
            disabled={busy || invalid !== null}
            title={invalid ?? "Gem profilen"}
            className="rounded-md bg-[var(--accent)] px-3 py-1.5 text-sm font-medium text-white hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-50"
          >
            {busy ? "Gemmer…" : "Gem"}
          </button>
        </div>
      </div>
    </div>
  );
}
