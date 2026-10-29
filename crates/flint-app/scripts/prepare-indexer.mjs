// Builds flint-indexer (release) and copies it to installer/bin, where the
// installer hooks (installer/hooks.nsh) pick it up. Tauri runs this as the
// `beforeBundleCommand`, so `tauri build` makes a complete installer.
// The program's path comes from cargo's own build report, so a custom
// CARGO_TARGET_DIR works too.
import { spawnSync } from "node:child_process";
import { copyFileSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const app = join(dirname(fileURLToPath(import.meta.url)), "..");
const r = spawnSync(
  "cargo",
  ["build", "-p", "flint-indexer", "--release", "--bin", "flint-indexer", "--message-format=json-render-diagnostics"],
  { cwd: app, encoding: "utf8", stdio: ["ignore", "pipe", "inherit"], maxBuffer: 64 * 1024 * 1024 },
);
if (r.status !== 0) {
  console.error("prepare-indexer: cargo build failed");
  process.exit(1);
}
const exe = r.stdout
  .split("\n")
  .filter((l) => l.startsWith("{"))
  .map((l) => JSON.parse(l))
  .find((m) => m.reason === "compiler-artifact" && m.target?.name === "flint-indexer" && m.executable)?.executable;
if (!exe) {
  console.error("prepare-indexer: cargo reported no flint-indexer program");
  process.exit(1);
}
const out = join(app, "installer", "bin");
mkdirSync(out, { recursive: true });
copyFileSync(exe, join(out, "flint-indexer.exe"));
console.log(`prepare-indexer: ${exe} -> installer/bin/flint-indexer.exe`);
