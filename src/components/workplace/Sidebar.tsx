import { useEffect, useRef, useState } from "react";
import { useStore } from "../../state/store";
import DiagnosticsPanel from "./DiagnosticsPanel";
import PermissionsPanel from "./PermissionsPanel";

type Tab = "permissions" | "diagnostics" | "tickets";

export default function Sidebar() {
  const { state } = useStore();
  const count = state.pending.length;
  const [tab, setTab] = useState<Tab>("permissions");

  // Jump to the permissions tab when requests start waiting (once per 0 -> n change).
  const prevCount = useRef(count);
  useEffect(() => {
    if (prevCount.current === 0 && count > 0) setTab("permissions");
    prevCount.current = count;
  }, [count]);

  const tabs: { id: Tab; label: string }[] = [
    { id: "permissions", label: `Tilladelser (${count})` },
    { id: "diagnostics", label: "Diagnostik" },
    { id: "tickets", label: "Tickets (trin 3)" },
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
            title={t.label}
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
        {tab === "tickets" && <p className="p-4 text-xs text-[var(--muted)]">Kommer i trin 3.</p>}
      </div>
    </aside>
  );
}
