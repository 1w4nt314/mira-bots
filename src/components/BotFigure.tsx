import { memo } from "react";
import { botSrc, type Theme } from "../lib/bots";
import type { BotState, Role } from "../lib/types";

interface Props {
  /** The agent's or profile's roles (figure: one role → static SVG; specialist → generated). */
  roles: readonly Role[];
  specialist: boolean;
  state: BotState;
  theme: Theme;
  exited?: boolean;
  /** Height in px; the width keeps the 240x250 aspect ratio. */
  size: number;
  /** Show the "afsluttet" badge under a dimmed exited figure (off for tiny icons). */
  badge?: boolean;
}

function BotFigure({ roles, specialist, state, theme, exited = false, size, badge = true }: Props) {
  const width = Math.round((size * 240) / 250);
  return (
    <div className="flex shrink-0 flex-col items-center">
      <img
        src={botSrc(theme, roles, specialist, state)}
        alt=""
        draggable={false}
        width={width}
        height={size}
        style={{ opacity: exited ? 0.5 : 1, width, height: size }}
        className="block max-w-full select-none object-contain"
      />
      {exited && badge && (
        <span className="mt-0.5 rounded bg-neutral-500/20 px-1.5 text-[10px] leading-4 text-[var(--muted)]">
          afsluttet
        </span>
      )}
    </div>
  );
}

export default memo(BotFigure);
