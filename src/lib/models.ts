// Model and effort choices for profiles, spawn overrides and "Skift model/effort". Mirrors
// `MODEL_ALIASES`, `MODEL_ID_MAX_CHARS`, `model_is_valid` and `Effort` in Rust (the backend
// validates again; this is for an early, Danish error in the editor). Pure, no imports at runtime.
import type { Effort } from "./types";

/** Aliases offered in the dropdowns ("default" is the "Standard" entry, i.e. null). */
export const MODEL_ALIASES: readonly string[] = [
  "best",
  "fable",
  "sonnet",
  "opus",
  "haiku",
  "sonnet[1m]",
  "opus[1m]",
  "opusplan",
];

/** All aliases the backend accepts (`MODEL_ALIASES` in config.rs). */
const ACCEPTED_ALIASES: readonly string[] = ["default", ...MODEL_ALIASES];

export const MODEL_ID_MAX_CHARS = 64;

export const EFFORT_LEVELS: readonly Effort[] = ["low", "medium", "high", "xhigh", "max"];

/** Mirrors `REPORT_*_MAX_CHARS`, `PROMPT_APPEND_MAX_CHARS` and `PROFILE_NAME_MAX_CHARS`
 *  (`MAX_REVIEW_ROUNDS` lives in lib/tickets). */
export const REPORT_TITLE_MAX = 120;
export const REPORT_BODY_MAX = 20000;
export const PROMPT_APPEND_MAX = 4000;
export const PROFILE_NAME_MAX = 60;

/** Text of the backend's error for an invalid model (C5.6). */
export const MODEL_INVALID_TEXT =
  "Ukendt model: brug et alias (sonnet, opus, haiku, fable, best, opusplan, sonnet[1m], opus[1m]) eller et fuldt id som claude-sonnet-5-5";

/**
 * Same rules as `model_is_valid` in Rust: an alias, or `claude-` + at least one of [a-z0-9-],
 * optionally followed by `[1m]`, at most 64 characters in total.
 *
 * | input                         | valid |
 * |-------------------------------|-------|
 * | "sonnet", "default", "opus[1m]" | yes |
 * | "claude-sonnet-5-5"           | yes   |
 * | "claude-opus-5-5[1m]"         | yes   |
 * | "claude-", "claude-[1m]"      | no    |
 * | "Claude-x", "claude-Sonnet"   | no    |
 * | " sonnet", "bogus", ""        | no    |
 * | "claude-x[2m]"                | no    |
 * | 65 characters                 | no    |
 */
export function isValidModel(s: string): boolean {
  if (ACCEPTED_ALIASES.includes(s)) return true;
  if ([...s].length > MODEL_ID_MAX_CHARS) return false;
  const base = s.endsWith("[1m]") ? s.slice(0, -4) : s;
  if (!base.startsWith("claude-")) return false;
  const rest = base.slice("claude-".length);
  return rest.length > 0 && /^[a-z0-9-]+$/.test(rest);
}

export function isEffort(s: string): s is Effort {
  return (EFFORT_LEVELS as readonly string[]).includes(s);
}

/** "standard" for null (Claude Code's default), else the value itself. */
export function modelLabel(model: string | null): string {
  return model === null || model === "default" ? "standard" : model;
}

export function effortLabel(effort: string | null): string {
  return effort === null ? "standard" : effort;
}
