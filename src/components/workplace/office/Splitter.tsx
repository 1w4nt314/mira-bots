import { useRef, useState, type KeyboardEvent, type PointerEvent } from "react";
import { SPLITTER_STEP } from "../../../lib/office";

interface Props {
  /** Current floor height in px (the clamped height actually shown). */
  value: number;
  /** Smallest floor height (Home): both seat rows, or less when the window is too low (the floor scrolls). */
  min: number;
  /** Largest floor height (End), or undefined before the column has been measured. */
  max: number | undefined;
  /** True when the terminal is minimised or maximised, or min = max: hidden, no dragging. */
  disabled: boolean;
  /** Wanted floor height in px; the caller clamps it. */
  onChange: (next: number) => void;
  /** Back to the default floor height (double-click). */
  onReset: () => void;
}

// Horizontal splitter between the office floor and the terminal. Not a dnd-kit draggable, so the
// PointerSensor never sees it; pointer capture keeps the drag alive outside the grip.
// TODO(windows-verify): D.64
export default function Splitter({ value, min, max, disabled, onChange, onReset }: Props) {
  const drag = useRef<{ startY: number; startValue: number } | null>(null);
  const [active, setActive] = useState(false);

  const end = () => {
    if (drag.current === null) return;
    drag.current = null;
    setActive(false);
  };

  const onPointerDown = (e: PointerEvent<HTMLDivElement>) => {
    if (disabled || e.button !== 0) return;
    e.preventDefault();
    e.currentTarget.setPointerCapture(e.pointerId);
    drag.current = { startY: e.clientY, startValue: value };
    setActive(true);
  };

  const onPointerMove = (e: PointerEvent<HTMLDivElement>) => {
    const d = drag.current;
    if (d === null || disabled) return;
    onChange(d.startValue + (e.clientY - d.startY));
  };

  const onKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (disabled) return;
    let next: number;
    if (e.key === "ArrowUp") next = value - SPLITTER_STEP;
    else if (e.key === "ArrowDown") next = value + SPLITTER_STEP;
    else if (e.key === "Home") next = min;
    else if (e.key === "End") next = max ?? Number.MAX_SAFE_INTEGER;
    else return;
    e.preventDefault();
    onChange(next);
  };

  return (
    <div
      role="separator"
      aria-orientation="horizontal"
      aria-label="Træk for at ændre terminalens højde"
      aria-valuenow={Math.round(value)}
      aria-valuemin={min}
      aria-valuemax={max}
      aria-disabled={disabled}
      tabIndex={disabled ? -1 : 0}
      title={
        disabled
          ? undefined
          : "Træk for at ændre terminalens højde (piletaster flytter, dobbeltklik nulstiller)"
      }
      data-active={active}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={end}
      onPointerCancel={end}
      onLostPointerCapture={end}
      onKeyDown={onKeyDown}
      onDoubleClick={() => {
        if (!disabled) onReset();
      }}
      // Disabled (min/max, or nothing to move): no band, no second border line, out of the a11y tree.
      className={`office-splitter h-2 shrink-0 touch-none border-t border-[var(--border)] bg-[var(--bg)] ${
        disabled ? "hidden" : "cursor-row-resize"
      }`}
    />
  );
}
