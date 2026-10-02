import { useEffect, useRef, useState, type FormEvent } from "react";
import { createTicket, errorMessage } from "../../../lib/ipc";
import { BODY_MAX, TITLE_MAX } from "../../../lib/tickets";
import type { ProjectRef } from "../../../lib/types";
import { useRefreshProjects, useStore } from "../../../state/store";
import ProjectPicker from "../ProjectPicker";

interface Props {
  onClose: () => void;
}

/**
 * New backlog ticket. Enter in the title moves to the description instead of submitting (so a
 * half-written ticket is never created by accident); Ctrl+Enter or "Opret" submits, Esc closes.
 * After a successful create the title and description are cleared (the project and the review
 * choice stay for the next ticket) and the form stays open with focus in the title.
 */
export default function NewTicketForm({ onClose }: Props) {
  const { state } = useStore();
  const refreshProjects = useRefreshProjects();
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  // The workspace rule `reviewByDefault` presets "Spring review over".
  const reviewByDefault = state.appInfo?.rules.reviewByDefault ?? true;
  const [skipReview, setSkipReview] = useState(!reviewByDefault);
  // Step 4b: an existing project, a new one (created at assignment) or "Vælg senere" (null).
  const [project, setProject] = useState<ProjectRef | null>(null);
  const [projectIncomplete, setProjectIncomplete] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const titleRef = useRef<HTMLInputElement>(null);
  const bodyRef = useRef<HTMLTextAreaElement>(null);

  useEffect(() => titleRef.current?.focus(), []);

  const canSubmit = !busy && title.trim() !== "" && !projectIncomplete;

  const submit = async (e?: FormEvent) => {
    e?.preventDefault();
    if (!canSubmit) return;
    setBusy(true);
    setError(null);
    try {
      await createTicket(title.trim(), body, skipReview, project);
      setTitle("");
      setBody("");
      void refreshProjects();
      titleRef.current?.focus();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  const field =
    "mt-0.5 block w-full rounded-md border border-[var(--border)] bg-[var(--bg)] p-1.5 text-xs outline-none focus:border-[var(--accent)]";

  return (
    <form
      onSubmit={(e) => void submit(e)}
      onKeyDown={(e) => {
        if (e.key === "Escape" && !busy) {
          e.preventDefault();
          onClose();
        } else if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
          e.preventDefault();
          void submit();
        }
      }}
      className="space-y-2 rounded-lg border border-[var(--border)] bg-[var(--bg)]/50 p-2 text-xs"
      aria-label="Ny ticket"
    >
      <label className="block">
        <span className="text-[var(--muted)]">Titel (påkrævet)</span>
        <input
          ref={titleRef}
          value={title}
          onChange={(e) => setTitle(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.ctrlKey && !e.metaKey) {
              e.preventDefault();
              bodyRef.current?.focus();
            }
          }}
          maxLength={TITLE_MAX}
          required
          placeholder="Hvad skal laves?"
          className={field}
        />
      </label>
      <div className="block">
        <span className="text-[var(--muted)]">Projekt</span>
        <div className="mt-0.5">
          <ProjectPicker
            value={project}
            onChange={setProject}
            onIncomplete={setProjectIncomplete}
            allowLater
            allowNew
            disabled={busy}
          />
        </div>
      </div>
      <label className="block">
        <span className="text-[var(--muted)]">Beskrivelse</span>
        <textarea
          ref={bodyRef}
          value={body}
          onChange={(e) => setBody(e.target.value)}
          maxLength={BODY_MAX}
          rows={4}
          placeholder="Detaljer, filer, acceptkriterier … (agenten læser den som fil)"
          className={`${field} resize-y`}
        />
      </label>
      <label className="flex items-center gap-2">
        <input type="checkbox" checked={skipReview} onChange={(e) => setSkipReview(e.target.checked)} />
        <span>Spring review over (går direkte til Done)</span>
      </label>
      {error !== null && (
        <p className="text-rose-500" role="alert">
          {error}
        </p>
      )}
      <div className="flex justify-end gap-2">
        <button
          type="button"
          onClick={onClose}
          disabled={busy}
          title="Luk formularen (Esc)"
          className="rounded-md px-2.5 py-1 hover:bg-neutral-500/15 disabled:opacity-50"
        >
          Luk
        </button>
        <button
          type="submit"
          disabled={!canSubmit}
          title="Opret ticketen i Backlog (Ctrl+Enter)"
          className="rounded-md bg-[var(--accent)] px-2.5 py-1 font-medium text-white hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-50"
        >
          {busy ? "Opretter…" : "Opret"}
        </button>
      </div>
    </form>
  );
}
