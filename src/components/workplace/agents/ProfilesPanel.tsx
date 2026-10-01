import { useEffect, useState } from "react";
import { useTheme } from "../../../lib/bots";
import { deleteProfile, resetBuiltinProfile } from "../../../lib/ipc";
import { effortLabel, modelLabel } from "../../../lib/models";
import { isSpecialist, ROLE_LABEL, sortRoles } from "../../../lib/roles";
import type { AgentProfile } from "../../../lib/types";
import { useStore } from "../../../state/store";
import BotFigure from "../../BotFigure";
import { useRun } from "../tickets/actions";
import ProfileEditor from "./ProfileEditor";

const btn =
  "rounded-md border border-[var(--border)] px-2 py-0.5 text-[11px] hover:border-[var(--accent)] disabled:cursor-not-allowed disabled:opacity-50";

/** A button that asks "Sikker?" on the first click and acts on the second (within 3 s). */
function ConfirmButton({ label, title, onConfirm }: { label: string; title: string; onConfirm: () => void }) {
  const [armed, setArmed] = useState(false);
  useEffect(() => {
    if (!armed) return;
    const t = setTimeout(() => setArmed(false), 3000);
    return () => clearTimeout(t);
  }, [armed]);
  return (
    <button
      type="button"
      onClick={() => {
        if (!armed) {
          setArmed(true);
          return;
        }
        setArmed(false);
        onConfirm();
      }}
      title={`${title} (klik igen for at bekræfte)`}
      className={`${btn} ${armed ? "border-rose-500 text-rose-600 dark:text-rose-300" : ""}`}
    >
      {armed ? "Sikker?" : label}
    </button>
  );
}

function ProfileRow({ p, onEdit }: { p: AgentProfile; onEdit: () => void }) {
  const theme = useTheme();
  const run = useRun();
  const roles = sortRoles(p.roles);
  return (
    <li className="flex gap-2 rounded-lg border border-[var(--border)] p-2">
      <BotFigure roles={roles} specialist={isSpecialist(p)} state="idle" theme={theme} size={48} />
      <div className="min-w-0 flex-1 space-y-1">
        <div className="flex items-center gap-1.5">
          <span className="truncate font-medium" title={p.name}>
            {p.name}
          </span>
          <span className="shrink-0 rounded bg-neutral-500/15 px-1 text-[10px] text-[var(--muted)]">
            {p.kind === "builtin" ? "indbygget" : "egen"}
          </span>
        </div>
        <div className="flex flex-wrap gap-1">
          {roles.length === 0 ? (
            <span className="text-[10px] text-[var(--muted)]">Ingen roller</span>
          ) : (
            roles.map((r) => (
              <span key={r} className="rounded bg-[var(--accent)]/15 px-1.5 text-[10px] text-[var(--accent)]">
                {ROLE_LABEL[r]}
              </span>
            ))
          )}
        </div>
        <div className="text-[11px] text-[var(--muted)]">
          Model: {modelLabel(p.model)} · Effort: {effortLabel(p.effort)} ·{" "}
          {p.defaultSeat === "work" ? "Arbejdsplads" : "Stabsplads"}
        </div>
        <div className="flex flex-wrap gap-1.5 pt-0.5">
          <button type="button" onClick={onEdit} title="Rediger profilen" className={btn}>
            Rediger
          </button>
          {p.kind === "builtin" ? (
            <ConfirmButton
              label="Nulstil"
              title="Gendan den indbyggede standard for profilen"
              onConfirm={() => void run(() => resetBuiltinProfile(p.id))}
            />
          ) : (
            <ConfirmButton
              label="Slet"
              title="Slet profilen (kørende agenter påvirkes ikke)"
              onConfirm={() => void run(() => deleteProfile(p.id))}
            />
          )}
        </div>
      </div>
    </li>
  );
}

/**
 * Sidebar tab "Agenter": all profiles (built-in first) with figure, roles, model/effort and
 * seat; "Ny profil" / "Rediger" open the editor, "Nulstil" (built-in) and "Slet" (custom) ask
 * for confirmation. The list follows `profiles-changed` through the store.
 */
// TODO(windows-verify): on first start after the upgrade the seven built-in profiles show here
// with figures; Nulstil/Slet work and `profiles-changed` refreshes the list (plan D.49).
export default function ProfilesPanel() {
  const { state } = useStore();
  const theme = useTheme();
  // undefined: closed; null: new profile.
  const [editing, setEditing] = useState<AgentProfile | null | undefined>(undefined);

  return (
    <div className="space-y-2 p-3 text-xs">
      <div className="flex items-center gap-2">
        <span className="flex-1 text-[var(--muted)]">Profiler ({state.profiles.length})</span>
        <button type="button" onClick={() => setEditing(null)} title="Opret en ny profil" className={btn}>
          Ny profil
        </button>
      </div>
      {state.profiles.length === 0 ? (
        <p className="text-[var(--muted)]">Henter profiler…</p>
      ) : (
        <ul className="space-y-1.5">
          {state.profiles.map((p) => (
            <ProfileRow key={p.id} p={p} onEdit={() => setEditing(p)} />
          ))}
        </ul>
      )}
      <p className="text-[11px] text-[var(--muted)]">
        Ændringer gælder nye agenter; kørende agenter beholder deres roller.
      </p>
      {editing !== undefined && (
        <ProfileEditor
          key={editing?.id ?? "new"}
          profile={editing}
          theme={theme}
          onClose={() => setEditing(undefined)}
          onSaved={() => setEditing(undefined)}
        />
      )}
    </div>
  );
}
