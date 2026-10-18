/**
 * Runs every long-session performance baseline and prints spec §1.1's curves.
 *
 * Usage: `cd desktop && node scripts/perf/run.mjs`
 *
 * Each script is standalone too (see README.md), which is handy when you only
 * care about one of the four costs.
 */

import { spawnSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));

const SCRIPTS = [
  ["session.mjs", "§1.1(a)  per-delta id lookup (target at the tail)"],
  ["index-lookup.mjs", "§3.1     the three lookup strategies (key evidence)"],
  ["render.mjs", "§1.1(b)  per-delta re-render + the three render shapes"],
  ["markdown.mjs", "§1.1(c)  streaming markdown, quadratic over a turn"],
  ["wire.mjs", "§8.1     frame bytes per delta (funnel item background)"],
  ["checkpoint.mjs", "§3.4     why the turn checkpoint must not be thinned"],
];

for (const [script, title] of SCRIPTS) {
  console.log(`\n${"=".repeat(72)}\n${title}\n${"=".repeat(72)}`);
  const r = spawnSync(process.execPath, [join(here, script)], { stdio: "inherit" });
  if (r.status !== 0) {
    console.error(`\n${script} exited with ${r.status}`);
    process.exit(r.status ?? 1);
  }
}

console.log(
  "\nNote: these run under node + jsdom, not WebKit. Trust the *trend*\n" +
    "(O(N) / O(N^2)), not the absolute milliseconds. See README.md.",
);