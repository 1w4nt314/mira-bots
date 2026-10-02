// Copies the release builds of mira-hook and mira-mcp into src-tauri/resources/ so the bundler
// picks them up (`resources/*` in tauri.conf.json). Plain Node, no dependencies.
// Unix: the copies are made executable (0755). macOS: they are also ad-hoc signed
// (`codesign --force -s -`), because Apple Silicon only runs signed Mach-O binaries and the
// bundler only seals them as resources; skip with MIRA_SKIP_CODESIGN=1. Signing happens here and
// not in CI because `tauri build` runs this script again (beforeBuildCommand) and would overwrite
// a file signed earlier.
// TODO(macos-verify): mira-hook and mira-mcp in mira-bots.app/Contents/Resources/resources/ are
// executable and ad-hoc signed (`codesign -dv`); /mcp shows mira-bots connected (plan7 M.10).
import { chmodSync, copyFileSync, existsSync, mkdirSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const suffix = process.platform === "win32" ? ".exe" : "";
const destDir = join(root, "src-tauri", "resources");
const binaries = ["mira-hook", "mira-mcp"];

const missing = binaries
  .map((bin) => join(root, "target", "release", `${bin}${suffix}`))
  .filter((src) => !existsSync(src));
if (missing.length > 0) {
  for (const src of missing) console.error(`copy-hook: ${src} not found.`);
  console.error('copy-hook: run "npm run build:hook" first (it builds mira-hook and mira-mcp).');
  process.exit(1);
}
mkdirSync(destDir, { recursive: true });
for (const bin of binaries) {
  const name = `${bin}${suffix}`;
  const dest = join(destDir, name);
  copyFileSync(join(root, "target", "release", name), dest);
  console.log(`copy-hook: copied ${name} to ${destDir}`);
  if (suffix === "") chmodSync(dest, 0o755);
  if (process.platform === "darwin" && process.env.MIRA_SKIP_CODESIGN !== "1") {
    execFileSync("codesign", ["--force", "-s", "-", dest], { stdio: "inherit" });
    console.log(`copy-hook: ad-hoc signed ${name}`);
  }
}
