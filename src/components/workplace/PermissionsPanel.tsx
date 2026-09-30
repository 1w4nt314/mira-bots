import { useStore } from "../../state/store";
import PermissionCard from "../PermissionCard";

/** Same cards and commands as the island; either window can answer a request. */
export default function PermissionsPanel() {
  const { state } = useStore();
  if (state.pending.length === 0) {
    return <p className="p-4 text-xs text-[var(--muted)]">Ingen anmodninger venter</p>;
  }
  // The cards are styled for the island's dark surface: keep that surface here too, so the
  // contrast is right in both themes.
  return (
    <div className="m-3 rounded-xl bg-neutral-900 pt-2 text-xs text-neutral-100">
      {state.pending.map((r) => (
        <PermissionCard key={r.requestId} request={r} />
      ))}
    </div>
  );
}
