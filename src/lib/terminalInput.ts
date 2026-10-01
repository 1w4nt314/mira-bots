// Pure helper: is a chunk from xterm's `onData` the user's own input, or an automatic reply?

/** A reply-bearing keydown is followed by its `onData` within this many ms (same task in practice). */
export const KEY_WINDOW_MS = 50;

/**
 * Whether `data` (from `term.onData`) counts as user input (it feeds `last_user_input_at`, which
 * makes the ticket dispatcher hold back delivery). xterm sends the terminal's automatic replies
 * (Device Attributes `ESC[?..c`, cursor-position reports `ESC[..R`, focus reports `ESC[I`/`ESC[O`,
 * bracketed-paste markers) through `onData` too, and those must not count.
 *
 * Replies are always ESC-prefixed and come without a keypress. Arrow keys and Esc are ESC-prefixed
 * but follow a `keydown` (`term.onKey`). Pasted text has no keydown but does not start with ESC.
 * `msSinceKey` is the time since the last `onKey` (`Infinity` if there was none).
 *
 * | data                 | msSinceKey | user input | why                                  |
 * |----------------------|------------|------------|--------------------------------------|
 * | `a`                  | any        | true       | not ESC-prefixed                     |
 * | `\r`                 | any        | true       | not ESC-prefixed                     |
 * | `hello\nworld`       | Infinity   | true       | paste: no keydown, not ESC-prefixed  |
 * | `\x1b[A`             | 3          | true       | arrow key right after a keydown      |
 * | `\x1b`               | 10         | true       | Esc right after a keydown            |
 * | `\x1b[?1;2c`         | Infinity   | false      | Device Attributes reply              |
 * | `\x1b[12;40R`        | 500        | false      | cursor-position report, stale key    |
 * | `\x1b[I` / `\x1b[O`  | Infinity   | false      | focus reports                        |
 */
export function classifyInput(data: string, msSinceKey: number): boolean {
  return !data.startsWith("\x1b") || msSinceKey < KEY_WINDOW_MS;
}
