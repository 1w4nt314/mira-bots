// Copies the release builds of mira-hook and mira-mcp into src-tauri/resources/ so the bundler
// picks them up (`resources/*` in tauri.conf.json). Plain Node, no dependencies.
import { copyFileSync, existsSync, mkdirSync } from "node:fs";
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
  copyFileSync(join(root, "target", "release", name), join(destDir, name));
  console.log(`copy-hook: copied ${name} to ${destDir}`);
}
