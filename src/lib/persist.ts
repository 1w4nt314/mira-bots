// The only localStorage access in src/: both reads and writes may throw (private mode, blocked
// storage, quota), and the UI must work without it.

// TODO(windows-verify): D.67 — the settings survive closing and restarting the installed app
// (localStorage in WebView2's user data folder) and are shared by both windows.

/** The stored string, or null when missing or storage is unavailable. */
export function readLocal(key: string): string | null {
  try {
    return window.localStorage.getItem(key);
  } catch {
    return null;
  }
}

/** Stores a string; failures are ignored. */
export function writeLocal(key: string, value: string): void {
  try {
    window.localStorage.setItem(key, value);
  } catch {
    // Not persisted; the value only lives for this session.
  }
}
