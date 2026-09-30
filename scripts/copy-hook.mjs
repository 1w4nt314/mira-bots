// Copies the release build of mira-hook into src-tauri/resources/ so the bundler picks it up.
// Plain Node, no dependencies.
import { copyFileSync, existsSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const suffix = process.platform === "win32" ? ".exe" : "";
const name = `mira-hook${suffix}`;
const src = join(root, "target", "release", name);
const destDir = join(root, "src-tauri", "resources");

if (!existsSync(src)) {
  console.error(`copy-hook: ${src} not found. Run "npm run build:hook" first.`);
  process.exit(1);
}
mkdirSync(destDir, { recursive: true });
copyFileSync(src, join(destDir, name));
console.log(`copy-hook: copied ${name} to ${destDir}`);
