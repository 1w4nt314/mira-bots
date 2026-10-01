import { useEffect, useRef, useState } from "react";
import { reviewCount } from "../../lib/tickets";
import type { WorkplaceTab } from "../../lib/types";
import { useStore } from "../../state/store";
import DiagnosticsPanel from "./DiagnosticsPanel";
import PermissionsPanel from "./PermissionsPanel";
import TicketsPanel from "./tickets/TicketsPanel";

type Tab = WorkplaceTab;

interface Props {
  /** Tab asked for from outside (`openWorkplace(…, tab)`); a new nonce re-applies the same tab. */
  requestedTab?: { tab: Tab; nonce: number } | null;
}

export default function Sidebar({ requestedTab = null }: Props) {
  const { state } = useStore();
  const count = state.pending.length;
  const reviews = reviewCount(state.tickets);
  const [tab, setTab] = useState<Tab>("permissions");

  // Jump to the permissions tab when requests start waiting (once per 0 -> n change).
  const prevCount = useRef(count);
  useEffect(() => {
    if (prevCount.current === 0 && count > 0) setTab("permissions");
    prevCount.current = count;
  }, [count]);

  // A tab requested by the island (e.g. the "n i review" chip).
  useEffect(() => {
    if (requestedTab !== null) setTab(requestedTab.tab);
  }, [requestedTab]);

  const tabs: { id: Tab; label: string; title: string }[] = [
    { id: "permissions", label: `Tilladelser (${count})`, title: `${count} anmodninger venter` },
    { id: "diagnostics", label: "Diagnostik", title: "Diagnostik" },
    {
      id: "tickets",
      label: reviews > 0 ? `Tickets (${reviews})` : "Tickets",
      title: reviews > 0 ? `${reviews} ${reviews === 1 ? "ticket venter" : "tickets venter"} på review` : "Tickets",
    },
  ];

  return (
    <aside className="flex min-h-0 flex-col border-l border-[var(--border)] bg-[var(--panel)]">
      <div className="flex shrink-0 border-b border-[var(--border)]" role="tablist">
        {tabs.map((t) => (
          <button
            key={t.id}
            type="button"
            role="tab"
            aria-selected={tab === t.id}
            onClick={() => setTab(t.id)}
            title={t.title}
            className={`flex-1 truncate px-2 py-2 text-xs ${
              tab === t.id
                ? "border-b-2 border-[var(--accent)] font-medium text-[var(--fg)]"
                : "border-b-2 border-transparent text-[var(--muted)] hover:text-[var(--fg)]"
            }`}
          >
            {t.label}
          </button>
        ))}
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto" role="tabpanel">
        {tab === "permissions" && <PermissionsPanel />}
        {tab === "diagnostics" && <DiagnosticsPanel />}
        {tab === "tickets" && <TicketsPanel />}
      </div>
    </aside>
  );
}
