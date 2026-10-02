// Workplace-level callbacks the ticket components need (selection and the spawn dialog live in
// Workplace). A context instead of threading them through Sidebar -> TicketsPanel -> StickyNote.
import { createContext, useCallback, useContext } from "react";
import { errorMessage } from "../../../lib/ipc";
import type { AgentInfo, SeatKind, TicketSummary } from "../../../lib/types";
import { useStore } from "../../../state/store";

export interface TicketActions {
  /** Shows the agent's terminal in the workplace. */
  selectAgent: (agentId: string) => void;
  /** Opens the SpawnDialog for a new agent that starts with this ticket. */
  spawnWithTicket: (seatKind: SeatKind, ticket: TicketSummary) => void;
  /** Why no new agent can be started in that row (limit, hook exe, pipe), or null. */
  spawnBlocked: Record<SeatKind, string | null>;
  /**
   * Gives the ticket to a running agent (drag or "Tildel…"), with the project rule of step 4b:
   * no project on a work seat → "Hvilket projekt?"; another project → explanation and
   * "Flyt agenten til «p»".
   */
  assignTo: (ticket: TicketSummary, agent: AgentInfo) => void;
}

const noop = () => {};

/** Small button on a note (and in the terminal's ticket queue). */
export const smallBtn =
  "rounded-md border border-[var(--note-border)] bg-[var(--note-bg)] px-2 py-0.5 text-[11px] text-[var(--note-fg)] hover:border-[var(--accent)] disabled:cursor-not-allowed disabled:opacity-50";

export const TicketActionsContext = createContext<TicketActions>({
  selectAgent: noop,
  spawnWithTicket: noop,
  spawnBlocked: { work: null, staff: null },
  assignTo: noop,
});

export function useTicketActions(): TicketActions {
  return useContext(TicketActionsContext);
}

/**
 * Runs a command and reports a failure through the store's error line (shown in the workplace
 * header). Resolves to true on success.
 */
export function useRun(): (action: () => Promise<unknown>) => Promise<boolean> {
  const { dispatch } = useStore();
  return useCallback(
    async (action: () => Promise<unknown>) => {
      try {
        await action();
        return true;
      } catch (e) {
        dispatch({ type: "error/set", error: errorMessage(e) });
        return false;
      }
    },
    [dispatch],
  );
}
