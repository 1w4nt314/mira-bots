// Truth table for src/lib/platform.ts: compiles it with the repo's TypeScript, runs it in node.
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import ts from "typescript";

const src = readFileSync(new URL("../src/lib/platform.ts", import.meta.url), "utf8");
const out = ts.transpileModule(src, { compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 } });
const file = join(mkdtempSync(join(tmpdir(), "platform-")), "platform.mjs");
writeFileSync(file, out.outputText);
const p = await import(pathToFileURL(file).href);

let n = 0;
const check = (actual, want, msg) => {
  assert.deepEqual(actual, want, msg);
  n++;
};

// detectPlatform(hint, userAgent)
const MAC_UA = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko)";
const WIN_UA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Edg/130.0";
const LIN_UA = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/605.1.15";
check(p.detectPlatform("macOS", ""), "macos", "userAgentData macOS");
check(p.detectPlatform("MacIntel", WIN_UA), "macos", "navigator.platform MacIntel wins over the UA");
check(p.detectPlatform(undefined, MAC_UA), "macos", "UA fallback Macintosh");
check(p.detectPlatform("", MAC_UA), "macos", "empty hint falls back to the UA");
check(p.detectPlatform("Windows", ""), "windows", "userAgentData Windows");
check(p.detectPlatform("Win32", MAC_UA), "windows", "navigator.platform Win32");
check(p.detectPlatform(undefined, WIN_UA), "windows", "UA fallback Windows");
check(p.detectPlatform("Linux x86_64", LIN_UA), "other", "Linux");
check(p.detectPlatform(undefined, ""), "other", "nothing known");
check(p.detectPlatform("iPhone", "iPhone"), "other", "iPhone is not macOS");

// the module-level PLATFORM in node (no navigator in node 22? navigator exists but is not a browser)
assert.ok(["windows", "macos", "other"].includes(p.PLATFORM));
n++;
check(p.isMac(), p.PLATFORM === "macos", "isMac matches PLATFORM");
check(p.isWindows(), p.PLATFORM === "windows", "isWindows matches PLATFORM");

// fileManagerName / openFolderTitle
check(p.fileManagerName("macos"), "Finder", "fileManagerName macos");
check(p.fileManagerName("windows"), "Stifinder", "fileManagerName windows");
check(p.fileManagerName("other"), "filhåndteringen", "fileManagerName other");
check(p.openFolderTitle("x", "windows"), "Åbn x i Stifinder", "openFolderTitle windows");
check(p.openFolderTitle("x", "macos"), "Åbn x i Finder", "openFolderTitle macos");
check(p.openFolderTitle("agentens mappe", "other"), "Åbn agentens mappe i filhåndteringen", "openFolderTitle other");
check(p.openFolderTitle("C:\\a b"), `Åbn C:\\a b i ${p.fileManagerName()}`, "openFolderTitle default platform");

// isMacShortcut: Cmd+<key> on macOS only
const e = (o) => ({ metaKey: false, ctrlKey: false, altKey: false, shiftKey: false, key: "q", ...o });
check(p.isMacShortcut(e({ metaKey: true, key: "q" }), "q", "macos"), true, "Cmd+q");
check(p.isMacShortcut(e({ metaKey: true, key: "Q" }), "q", "macos"), true, "Cmd+Q (Caps Lock)");
check(p.isMacShortcut(e({ metaKey: true, key: "w" }), "w", "macos"), true, "Cmd+w");
check(p.isMacShortcut(e({ metaKey: true, key: "w" }), "q", "macos"), false, "Cmd+w is not Cmd+q");
check(p.isMacShortcut(e({ metaKey: true, ctrlKey: true }), "q", "macos"), false, "Cmd+Ctrl+q");
check(p.isMacShortcut(e({ metaKey: true, altKey: true }), "q", "macos"), false, "Cmd+Alt+q");
check(p.isMacShortcut(e({ metaKey: true, shiftKey: true }), "q", "macos"), false, "Cmd+Shift+q");
check(p.isMacShortcut(e({ key: "q" }), "q", "macos"), false, "plain q");
check(p.isMacShortcut(e({ ctrlKey: true, key: "q" }), "q", "macos"), false, "Ctrl+q");
check(p.isMacShortcut(e({ metaKey: true }), "q", "windows"), false, "Win+q on Windows");
check(p.isMacShortcut(e({ metaKey: true }), "q", "other"), false, "Meta+q elsewhere");

console.log(`platform.ts: ${n} cases ok`);
