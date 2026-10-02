// Platform detection and the few places where the UI differs between Windows and macOS
// (plan7 C7.5). Pure apart from reading `navigator` once at module load.

export type Platform = "windows" | "macos" | "other";

/**
 * `hint` = `navigator.userAgentData?.platform ?? navigator.platform`. /mac/i -> macos,
 * /win/i -> windows; the user agent ("Macintosh"/"Windows") is the fallback when the hint is
 * empty or unrecognised.
 */
export function detectPlatform(hint: string | undefined, userAgent: string): Platform {
  const h = hint ?? "";
  if (/mac/i.test(h)) return "macos";
  if (/win/i.test(h)) return "windows";
  if (/macintosh|mac os x/i.test(userAgent)) return "macos";
  if (/windows/i.test(userAgent)) return "windows";
  return "other";
}

function detectHere(): Platform {
  if (typeof navigator === "undefined") return "other";
  const nav = navigator as Navigator & { userAgentData?: { platform?: string } };
  return detectPlatform(nav.userAgentData?.platform ?? nav.platform, nav.userAgent ?? "");
}

export const PLATFORM: Platform = detectHere();

export const isMac = (): boolean => PLATFORM === "macos";
export const isWindows = (): boolean => PLATFORM === "windows";

/** The name of the system file manager, as shown in Danish UI texts. */
export function fileManagerName(p: Platform = PLATFORM): string {
  if (p === "macos") return "Finder";
  if (p === "windows") return "Stifinder";
  return "filhåndteringen";
}

/** Title/aria text for the "open folder" buttons: `Åbn <what> i Finder/Stifinder/filhåndteringen`. */
export function openFolderTitle(what: string, p: Platform = PLATFORM): string {
  return `Åbn ${what} i ${fileManagerName(p)}`;
}

/**
 * Cmd+<key> on macOS only: meta without ctrl/alt/shift; `key` is lower case and compared
 * case-insensitively with `e.key` (Caps Lock reports "Q").
 */
export function isMacShortcut(
  e: { metaKey: boolean; ctrlKey: boolean; altKey: boolean; shiftKey: boolean; key: string },
  key: string,
  p: Platform = PLATFORM,
): boolean {
  return (
    p === "macos" &&
    e.metaKey &&
    !e.ctrlKey &&
    !e.altKey &&
    !e.shiftKey &&
    e.key.toLowerCase() === key
  );
}
