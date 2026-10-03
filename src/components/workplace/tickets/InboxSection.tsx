import { useEffect, useMemo, useState } from "react";
import {
  cappedHint,
  dismissedItems,
  fetchStatusText,
  groupInbox,
  inboxEmptyText,
  matchesInboxFilter,
  MANUAL_FLOOR_MS,
  showInboxSection,
  sourceErrors,
  sourceNotes,
} from "../../../lib/inbox";
import { dismissInboxItem, undismissInboxItem } from "../../../lib/ipc";
import type { ProjectFilter } from "../../../lib/projects";
import type { InboxItemSummary, TicketSummary } from "../../../lib/types";
import { useStore } from "../../../state/store";
import { useRun, useTicketActions } from "./actions";
import InboxCard from "./InboxCard";
import StartInboxDialog from "./StartInboxDialog";

interface Props {
  filter: ProjectFilter;
  /** The panel's green notice line ("Startet som ticket …"). */
  onNotice: (text: string) => void;
}

/**
 * "Indbakke (n)": new items from the inbox folders and GitHub, the latest fetch and each
 * source's error as the backend wrote it. Shown when something is new or a source failed.
 * Nothing starts without a click ("Start…" opens the dialog); the polling lives in Workplace.
 */
export default function InboxSection({ filter, onNotice }: Props) {
  const { state } = useStore();
  const { refreshInbox } = useTicketActions();
  const run = useRun();
  const inbox = state.inbox;
  const [startFor, setStartFor] = useState<InboxItemSummary | null>(null);
  const [busyIds, setBusyIds] = useState<ReadonlySet<string>>(new Set());
  // "Opdatér" rests for the floor after a click (the hook enforces it as well).
  const [cooling, setCooling] = useState(false);

  const shown = useMemo(
    () => groupInbox(inbox?.items ?? []).new.filter((i) => matchesInboxFilter(i, filter)),
    [inbox, filter],
  );

  useEffect(() => {
    if (!cooling) return;
    const t = setTimeout(() => setCooling(false), MANUAL_FLOOR_MS);
    return () => clearTimeout(t);
  }, [cooling]);

  const dismiss = async (item: InboxItemSummary) => {
    setBusyIds((s) => new Set(s).add(item.id));
    await run(() => dismissInboxItem(item.id));
    setBusyIds((s) => {
      const next = new Set(s);
      next.delete(item.id);
      return next;
    });
  };

  // The dialog is rendered outside the section: the item leaves the list the moment it is
  // started, and the section may hide with it while the dialog finishes its work.
  const dialog = startFor !== null && (
    <StartInboxDialog
      item={startFor}
      onClose={() => setStartFor(null)}
      onStarted={(t: TicketSummary) => {
        setStartFor(null);
        onNotice(`Startet som ticket ${t.shortId}`);
      }}
    />
  );
  const status = inbox?.status ?? null;
  if (status === null || !showInboxSection(shown.length, status)) return <>{dialog}</>;

  const errors = sourceErrors(status);
  const notes = sourceNotes(status);
  const hint = cappedHint(status);
  const refreshing = status.refreshing;

  return (
    <>
      <section className="space-y-2" aria-label="Indbakke">
        <div className="flex items-center gap-2">
          <h3 className="text-[11px] font-semibold uppercase tracking-wide text-[var(--muted)]">
            Indbakke ({shown.length})
          </h3>
          <button
            type="button"
            onClick={() => {
              setCooling(true);
              refreshInbox();
            }}
            disabled={refreshing || cooling}
            title="Hent nye emner fra indbakke-mapperne og GitHub nu"
            className="ml-auto rounded-md border border-[var(--border)] px-2 py-0.5 text-[11px] hover:border-[var(--accent)] disabled:cursor-not-allowed disabled:opacity-50"
          >
            Opdatér
          </button>
        </div>
        <p className="text-[11px] text-[var(--muted)]" role="status">
          {fetchStatusText(status)}
        </p>
        {errors.map((e) => (
          <p
            key={e.id}
            className="rounded-lg border border-amber-400/50 bg-amber-300/20 px-2 py-1 text-xs text-amber-800 dark:text-amber-200"
            role="status"
          >
            <span className="font-medium">{e.label}:</span> {e.error}
          </p>
        ))}
        {shown.length === 0 ? (
          <p className="text-xs text-[var(--muted)]">{inboxEmptyText(filter)}</p>
        ) : (
          shown.map((item) => (
            <InboxCard
              key={item.id}
              item={item}
              busy={busyIds.has(item.id)}
              draggable
              onStart={setStartFor}
              onDismiss={(i) => void dismiss(i)}
            />
          ))
        )}
        {notes.map((n, i) => (
          <p key={`${i}:${n}`} className="text-[11px] text-[var(--muted)]">
            {n}
          </p>
        ))}
        {hint !== null && <p className="text-[11px] text-[var(--muted)]">{hint}</p>}
      </section>
      {dialog}
    </>
  );
}

/**
 * "Afviste (n)" (step 6c B5): dismissed items under the project filter, folded like Done, each
 * with "Fortryd" (back to the inbox as new). Hidden when nothing is dismissed. The backend keeps
 * dismissed items for 30 days.
 */
export function DismissedFold({ filter }: { filter: ProjectFilter }) {
  const { state } = useStore();
  const run = useRun();
  const [busyIds, setBusyIds] = useState<ReadonlySet<string>>(new Set());
  const items = useMemo(() => dismissedItems(state.inbox?.items ?? [], filter), [state.inbox, filter]);
  if (items.length === 0) return null;

  const undo = async (item: InboxItemSummary) => {
    setBusyIds((s) => new Set(s).add(item.id));
    await run(() => undismissInboxItem(item.id));
    setBusyIds((s) => {
      const next = new Set(s);
      next.delete(item.id);
      return next;
    });
  };

  return (
    <details className="group">
      <summary className="cursor-pointer select-none text-[11px] font-semibold uppercase tracking-wide text-[var(--muted)] hover:text-[var(--fg)]">
        Afviste ({items.length})
      </summary>
      <div className="mt-2 space-y-2">
        {items.map((item) => (
          <InboxCard key={item.id} item={item} busy={busyIds.has(item.id)} onUndismiss={(i) => void undo(i)} />
        ))}
      </div>
    </details>
  );
}
