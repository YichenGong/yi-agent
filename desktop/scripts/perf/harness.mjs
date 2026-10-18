/**
 * Shared harness for the long-session performance baselines.
 *
 * See `docs/superpowers/specs/2026-10-18-long-session-perf-design.md` §1.1.
 * Everything here bundles the repo's **real** `session.ts` with esbuild and
 * runs it under node — no copies, so a change in the source is reflected here.
 */

import { execFileSync } from "node:child_process";
import { mkdirSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
/** `desktop/` — the scripts live at `desktop/scripts/perf`. */
export const DESKTOP_ROOT = join(here, "..", "..");

/** Where the esbuild output lands. Temp only; never inside the repo. */
export const BUILD_DIR = join(tmpdir(), "yi-agent-perf");

function esbuildBin() {
  const bin = join(DESKTOP_ROOT, "node_modules", ".bin", "esbuild");
  if (!existsSync(bin)) {
    throw new Error(
      `esbuild not found at ${bin}. Run \`cd desktop && npm install\` first.`,
    );
  }
  return bin;
}

/**
 * Bundle a TypeScript module from this repo to ESM and return its file URL.
 *
 * `entry` is resolved against `DESKTOP_ROOT` (e.g. `src/lib/session.ts`), so
 * the scripts always exercise the working tree rather than a stale copy.
 */
export function bundle(entry) {
  mkdirSync(BUILD_DIR, { recursive: true });
  const out = join(BUILD_DIR, entry.replace(/[\\/]/g, "_").replace(/\.ts$/, ".mjs"));
  execFileSync(esbuildBin(), [
    join(DESKTOP_ROOT, entry),
    "--bundle",
    "--format=esm",
    "--platform=node",
    `--outfile=${out}`,
    "--log-level=error",
  ]);
  return pathToFileURL(out).href;
}

/** A node module in `desktop/node_modules`, as an ESM import URL. */
export function vendor(pkg) {
  return pathToFileURL(join(DESKTOP_ROOT, "node_modules", pkg)).href;
}

/** Print a labelled curve row. */
export function row(label, value) {
  console.log(`${label}  ${value}`);
}

/**
 * `import` a package's *entry* from node_modules (not a file inside it), which
 * is what `vendor` cannot do — it takes the package name.
 */
export async function importVendor(name) {
  const url = pathToFileURL(join(DESKTOP_ROOT, "node_modules", name, "index.js")).href;
  return await import(url);
}