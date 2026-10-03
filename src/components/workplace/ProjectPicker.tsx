import { useEffect, useId, useState } from "react";
import { PROJECT_NAME_MAX, sameProjectId, validateProjectName } from "../../lib/projects";
import type { ProjectRef } from "../../lib/types";
import { useRefreshProjects, useStore } from "../../state/store";

interface Props {
  value: ProjectRef | null;
  /** `null` while nothing (or an invalid new name) is chosen. */
  onChange: (v: ProjectRef | null) => void;
  /** Offers "Vælg senere" (null) instead of "— Vælg projekt —". */
  allowLater: boolean;
  /** Offers "Nyt projekt…" with a name field (`{ new: name }`). */
  allowNew: boolean;
  /** A project left out of the list (e.g. the agent's current one). */
  exclude?: string | null;
  disabled?: boolean;
  autoFocus?: boolean;
  /** Accessible name of the select (default "Projekt"). */
  label?: string;
  /** True while "Nyt projekt…" is chosen without a valid name (the value is then null). */
  onIncomplete?: (incomplete: boolean) => void;
}

const NEW = "\u0000new";

/**
 * Project choice (plan4b C4b.9): an existing project, "Nyt projekt…" (a folder name checked live
 * with the Windows rules, created when the ticket is assigned or the agent starts) or, with
 * `allowLater`, "Vælg senere".
 */
export default function ProjectPicker(props: Props) {
  const { value, onChange, allowLater, allowNew, exclude = null, disabled, autoFocus } = props;
  const { state } = useStore();
  const refresh = useRefreshProjects();
  const nameId = useId();
  const errorId = useId();
  // "Nyt projekt…" stays chosen while the name is still empty or invalid (value is then null).
  const [newMode, setNewMode] = useState(() => value !== null && typeof value !== "string");
  const [newName, setNewName] = useState(() =>
    value !== null && typeof value !== "string" ? value.new : "",
  );
  // The name field takes focus when the user picks "Nyt projekt…" (not on mount).
  const [focusName, setFocusName] = useState(false);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // A value set from outside (another project, or cleared) leaves the new-name mode.
  useEffect(() => {
    if (typeof value === "string") setNewMode(false);
    else if (value !== null) {
      setNewMode(true);
      setNewName(value.new);
    }
  }, [value]);

  const projects = state.projects.filter((p) => !sameProjectId(p.id, exclude));
  const current = typeof value === "string" ? value : null;
  // A chosen id that is not (or no longer) in the list still shows as chosen.
  const missing = current !== null && !projects.some((p) => p.id === current);
  const selectValue = newMode ? NEW : (current ?? "");
  const nameError = newName === "" ? null : validateProjectName(newName);
  const incomplete = newMode && (newName === "" || nameError !== null);
  const { onIncomplete } = props;
  useEffect(() => onIncomplete?.(incomplete), [incomplete, onIncomplete]);
  const existsAlready =
    newName !== "" && nameError === null && state.projects.some((p) => sameProjectId(p.id, newName));

  const pickNewName = (name: string) => {
    setNewName(name);
    onChange(name !== "" && validateProjectName(name) === null ? { new: name } : null);
  };

  if (projects.length === 0 && !allowNew && !missing) {
    return (
      <p className="text-[11px] text-[var(--muted)]">
        Ingen projekter endnu — opret et under Diagnostik eller med «Nyt projekt…»
      </p>
    );
  }

  const field =
    "block w-full rounded-md border border-[var(--border)] bg-[var(--bg)] p-1.5 text-xs outline-none focus:border-[var(--accent)] disabled:opacity-50";

  return (
    <div className="space-y-1">
      <select
        value={selectValue}
        disabled={disabled}
        autoFocus={autoFocus && !newMode}
        aria-label={props.label ?? "Projekt"}
        onChange={(e) => {
          const v = e.target.value;
          if (v === NEW) {
            setNewMode(true);
            setFocusName(true);
            onChange(newName !== "" && validateProjectName(newName) === null ? { new: newName } : null);
          } else {
            setNewMode(false);
            onChange(v === "" ? null : v);
          }
        }}
        className={field}
      >
        <option value="">{allowLater ? "Vælg senere" : "— Vælg projekt —"}</option>
        {missing && <option value={current}>{current}</option>}
        {projects.map((p) => (
          <option key={p.id} value={p.id} title={p.path}>
            {p.isGitRepo ? `${p.id} (git)` : p.id}
          </option>
        ))}
        {allowNew && (
          <option value={NEW}>
            {newMode && newName !== "" && nameError === null ? `Nyt projekt: ${newName}` : "Nyt projekt…"}
          </option>
        )}
      </select>
      {newMode && (
        <div>
          <input
            id={nameId}
            value={newName}
            disabled={disabled}
            autoFocus={focusName || autoFocus}
            onChange={(e) => pickNewName(e.target.value)}
            onKeyDown={(e) => {
              // Never submits a surrounding form by accident (like the ticket title).
              if (e.key === "Enter" && !e.ctrlKey && !e.metaKey) e.preventDefault();
            }}
            maxLength={PROJECT_NAME_MAX}
            placeholder="mappenavn, fx min-app"
            aria-label="Navn på det nye projekt"
            aria-invalid={nameError !== null}
            aria-describedby={nameError !== null || existsAlready ? errorId : undefined}
            className={field}
          />
          {nameError !== null && (
            <p id={errorId} className="mt-0.5 text-[11px] text-rose-500" role="alert">
              {nameError}
            </p>
          )}
          {existsAlready && (
            <p id={errorId} className="mt-0.5 text-[11px] text-[var(--muted)]">
              Projektet findes allerede og bruges som det er.
            </p>
          )}
        </div>
      )}
    </div>
  );
}
