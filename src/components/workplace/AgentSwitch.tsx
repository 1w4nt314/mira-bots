import { useEffect, useRef, useState } from "react";
import { setAgentEffort, setAgentModel } from "../../lib/ipc";
import { isEffort } from "../../lib/models";
import { switchBlocked } from "../../lib/tickets";
import type { AgentInfo, Effort } from "../../lib/types";
import ModelPicker, { EffortSelect, modelChoiceValid } from "./agents/ModelPicker";
import { useRun } from "./tickets/actions";

type Kind = "model" | "effort";

const btn =
  "shrink-0 rounded-md border border-[var(--border)] px-2.5 py-1 text-xs hover:border-[var(--accent)] disabled:cursor-not-allowed disabled:opacity-50";

/**
 * "Skift model" / "Skift effort": a popover with the picker and a confirmation. The backend
 * restarts the agent with `--resume <session-id>` and the new flag (never `/model` or `/effort`
 * in the terminal); only possible while the agent is idle without a ticket in progress.
 */
// TODO(windows-verify): "Skift model" restarts with `--resume <session-id> --model <alias>`, the
// transcript stays visible, the agent goes back to Klar and no old process is left (plan D.53).
export default function AgentSwitch({ agent }: { agent: AgentInfo }) {
  const run = useRun();
  const [open, setOpen] = useState<Kind | null>(null);
  const [model, setModel] = useState<string | null>(null);
  const [effort, setEffort] = useState<Effort | null>(null);
  const [busy, setBusy] = useState(false);
  const box = useRef<HTMLDivElement>(null);
  const blocked = switchBlocked(agent);

  // Another agent, or the agent started working: close the popover.
  useEffect(() => setOpen(null), [agent.id]);
  useEffect(() => {
    if (blocked !== null) setOpen(null);
  }, [blocked]);

  useEffect(() => {
    if (open === null) return;
    const onDown = (e: MouseEvent) => {
      if (box.current !== null && !box.current.contains(e.target as Node)) setOpen(null);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(null);
    };
    window.addEventListener("mousedown", onDown);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("mousedown", onDown);
      window.removeEventListener("keydown", onKey);
    };
  }, [open]);

  const show = (k: Kind) => {
    if (open === k) {
      setOpen(null);
      return;
    }
    setModel(agent.model);
    setEffort(agent.effort !== null && isEffort(agent.effort) ? agent.effort : null);
    setOpen(k);
  };

  const apply = async () => {
    setBusy(true);
    const ok =
      open === "model"
        ? await run(() => setAgentModel(agent.id, model))
        : effort !== null && (await run(() => setAgentEffort(agent.id, effort)));
    setBusy(false);
    if (ok) setOpen(null);
  };

  const canApply = !busy && (open === "model" ? modelChoiceValid(model) : effort !== null);

  return (
    <div ref={box} className="relative flex shrink-0 gap-2">
      <button
        type="button"
        onClick={() => show("model")}
        disabled={blocked !== null}
        title={blocked ?? "Genstart agenten med en anden model (samtalen bevares)"}
        aria-expanded={open === "model"}
        className={btn}
      >
        Skift model
      </button>
      <button
        type="button"
        onClick={() => show("effort")}
        disabled={blocked !== null}
        title={blocked ?? "Genstart agenten med et andet effort-niveau (samtalen bevares)"}
        aria-expanded={open === "effort"}
        className={btn}
      >
        Skift effort
      </button>
      {open !== null && (
        <div
          role="dialog"
          aria-label={open === "model" ? "Skift model" : "Skift effort"}
          className="absolute right-0 top-full z-40 mt-1 w-[320px] space-y-2 rounded-xl border border-[var(--border)] bg-[var(--panel)] p-3 text-xs shadow-xl"
        >
          {open === "model" ? (
            <ModelPicker key={agent.id} value={model} onChange={setModel} label="Ny model" />
          ) : (
            <EffortSelect value={effort} onChange={setEffort} standardLabel={null} label="Nyt effort-niveau" />
          )}
          <p className="text-[11px] text-[var(--muted)]">
            Sessionen genstartes med <span className="font-mono">--resume</span>: samtalen bevares,
            og agenten fortsætter med {open === "model" ? "den nye model" : "det nye niveau"}.
            {open === "model" && " Standard betyder Claude Codes egen standardmodel."}
          </p>
          <div className="flex justify-end gap-2">
            <button type="button" onClick={() => setOpen(null)} disabled={busy} className={btn}>
              Annuller
            </button>
            <button
              type="button"
              onClick={() => void apply()}
              disabled={!canApply}
              className="rounded-md bg-[var(--accent)] px-2.5 py-1 text-xs font-medium text-white hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-50"
            >
              {busy ? "Genstarter…" : "Genstart"}
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
