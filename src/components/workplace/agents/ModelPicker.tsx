import { useId, useState } from "react";
import { EFFORT_LEVELS, isEffort, isValidModel, MODEL_ALIASES, MODEL_INVALID_TEXT } from "../../../lib/models";
import type { Effort } from "../../../lib/types";

const OTHER = "__other__";

const field =
  "rounded-md border border-[var(--border)] bg-[var(--bg)] px-2 py-1 text-xs outline-none focus:border-[var(--accent)]";

interface ModelPickerProps {
  /** Alias or full id; null = the "Standard" entry; "" while a free id is being typed. */
  value: string | null;
  onChange: (value: string | null) => void;
  /** Label of the null entry (e.g. "Standard" or "Fra profilen (opus)"); omit to hide it. */
  standardLabel?: string | null;
  label?: string;
}

/** Whether a `ModelPicker` value can be saved: null (standard) or a valid alias/id. */
export function modelChoiceValid(value: string | null): boolean {
  return value === null || isValidModel(value);
}

/**
 * Model dropdown: Standard, the aliases, and "Andet id…" with a text field that is checked with
 * `isValidModel` (the same rules as the backend). Used by the profile editor, the spawn dialog
 * and "Skift model".
 */
export default function ModelPicker({ value, onChange, standardLabel = "Standard", label = "Model" }: ModelPickerProps) {
  const id = useId();
  const [other, setOther] = useState(value !== null && !MODEL_ALIASES.includes(value));
  const selected = other ? OTHER : (value ?? "");
  const invalid = other && value !== null && value !== "" && !isValidModel(value);

  return (
    <div className="space-y-1">
      <label htmlFor={id} className="block text-xs font-medium text-[var(--muted)]">
        {label}
      </label>
      <div className="flex flex-wrap items-center gap-2">
        <select
          id={id}
          value={selected}
          onChange={(e) => {
            const v = e.target.value;
            if (v === OTHER) {
              setOther(true);
              onChange("");
            } else {
              setOther(false);
              onChange(v === "" ? null : v);
            }
          }}
          className={field}
        >
          {standardLabel !== null && <option value="">{standardLabel}</option>}
          {MODEL_ALIASES.map((a) => (
            <option key={a} value={a}>
              {a}
            </option>
          ))}
          <option value={OTHER}>Andet id…</option>
        </select>
        {other && (
          <input
            type="text"
            value={value ?? ""}
            onChange={(e) => onChange(e.target.value.trim())}
            placeholder="claude-sonnet-5-5"
            aria-label="Fuldt model-id"
            aria-invalid={invalid}
            spellCheck={false}
            autoFocus
            className={`${field} min-w-0 flex-1 font-mono ${invalid ? "border-rose-500" : ""}`}
          />
        )}
      </div>
      {invalid && (
        <p className="text-[11px] text-rose-500" role="alert">
          {MODEL_INVALID_TEXT}
        </p>
      )}
      {other && value === "" && <p className="text-[11px] text-[var(--muted)]">Skriv et fuldt model-id</p>}
    </div>
  );
}

interface EffortSelectProps {
  value: Effort | null;
  onChange: (value: Effort | null) => void;
  /** Label of the null entry; null hides it (a restart needs a concrete level). */
  standardLabel?: string | null;
  label?: string;
}

/** Effort dropdown: optional "Standard" + low … max. */
export function EffortSelect({ value, onChange, standardLabel = "Standard", label = "Effort" }: EffortSelectProps) {
  const id = useId();
  return (
    <div className="space-y-1">
      <label htmlFor={id} className="block text-xs font-medium text-[var(--muted)]">
        {label}
      </label>
      <select
        id={id}
        value={value ?? ""}
        onChange={(e) => onChange(isEffort(e.target.value) ? e.target.value : null)}
        className={field}
      >
        {standardLabel !== null && <option value="">{standardLabel}</option>}
        {standardLabel === null && value === null && (
          <option value="" disabled>
            Vælg niveau…
          </option>
        )}
        {EFFORT_LEVELS.map((e) => (
          <option key={e} value={e}>
            {e}
          </option>
        ))}
      </select>
    </div>
  );
}
