/**
 * §3.1: the decisive comparison of the three item-lookup strategies.
 *
 * This is the script that **refuted** the first draft of the spec. The draft
 * said "check the array tail, else fall back to `Array.prototype.findIndex`".
 * That is not enough: a reasoning block is `push`ed to the tail
 * (`session.ts:500`), so when CoT resumes after the text has started it
 * displaces the streaming agent message to `length - 2` — and a *forward*
 * `findIndex` then scans the entire history on every delta.
 *
 * We measure with the real `session.ts`, patched to count id comparisons.
 * The three variants are produced by rewriting the one hot lookup, so the rest
 * of `apply` (insertion, reasoning handling, cursor bookkeeping) is identical
 * and the counts are comparable.
 */

import { execFileSync } from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";
import { BUILD_DIR, DESKTOP_ROOT, row } from "./harness.mjs";

const SESSION_SRC = join(DESKTOP_ROOT, "src", "lib", "session.ts");
const HOT = "        const index = this.items.findIndex((i) => i.id === item_id);";

const COUNTER = `let CMP = 0;
export function cmpCount() { return CMP; }
export function resetCmp() { CMP = 0; }
`;

/** Backwards scan from the tail. Bounded in practice; still O(N) worst case. */
const BACKWARD = `function lookupBackward(items, id) {
  const n = items.length;
  for (let i = n - 1; i >= 0; i -= 1) { CMP += 1; if (items[i].id === id) return i; }
  return -1;
}
`;

/** The draft's proposal: tail check, then a FORWARD findIndex. */
const FORWARD = `function lookupForward(items, id) {
  const n = items.length;
  CMP += 1;
  if (n > 0 && items[n - 1].id === id) return n - 1;
  for (let i = 0; i < n - 1; i += 1) { CMP += 1; if (items[i].id === id) return i; }
  return -1;
}
`;

/** Tail scan + a self-validating cached index. */
const CACHED = `let cachedId = null;
let cachedIndex = -1;
function lookupCached(items, id) {
  if (cachedId === id && items[cachedIndex]?.id === id) { CMP += 1; return cachedIndex; }
  for (let i = items.length - 1; i >= 0; i -= 1) {
    CMP += 1;
    if (items[i].id === id) { cachedId = id; cachedIndex = i; return i; }
  }
  return -1;
}
`;

function variantSource(name) {
  const src = readFileSync(SESSION_SRC, "utf8");
  if (!src.includes(HOT)) {
    throw new Error(
      `the hot lookup moved; update this script. Expected to find:\n${HOT}`,
    );
  }
  const impl =
    name === "forward" ? FORWARD : name === "backward" ? BACKWARD : CACHED;
  const call =
    name === "cached"
      ? "        const index = lookupCached(this.items, item_id);"
      : `        const index = ${
          name === "forward" ? "lookupForward" : "lookupBackward"
        }(this.items, item_id);`;
  return COUNTER + impl + src.replace(HOT, call);
}

async function buildVariant(name) {
  mkdirSync(BUILD_DIR, { recursive: true });
  const srcPath = join(BUILD_DIR, `variant_${name}.ts`);
  const outPath = join(BUILD_DIR, `variant_${name}.mjs`);
  writeFileSync(srcPath, variantSource(name));
  execFileSync(join(DESKTOP_ROOT, "node_modules", ".bin", "esbuild"), [
    srcPath,
    "--bundle",
    "--format=esm",
    "--platform=node",
    `--outfile=${outPath}`,
    "--log-level=error",
  ]);
  return await import(pathToFileURL(outPath).href);
}

/**
 * A realistic turn: history, an opening user item, reasoning, agent text, then
 * reasoning resuming *after* the text (which pushes the reasoning block to the
 * tail and displaces the text target).
 */
function runStream(Session, historyN, { deltas = 120, reasoningRuns = 6 } = {}) {
  const TID = "t";
  const s = new Session();
  for (let i = 0; i < historyN; i += 1) {
    s.apply({
      method: "item/completed",
      params: {
        thread_id: TID,
        item: { type: "agentMessage", id: `h${i}`, text: `history ${i}` },
      },
    });
  }
  s.apply({
    method: "item/started",
    params: { thread_id: TID, item: { type: "userMessage", id: "u1", text: "hi" } },
  });
  s.apply({
    method: "thread/reasoningDelta",
    params: { thread_id: TID, text: "r ", kind: "Content" },
  });
  s.apply({
    method: "item/started",
    params: { thread_id: TID, item: { type: "agentMessage", id: "a1", text: "" } },
  });
  const per = Math.floor(deltas / reasoningRuns);
  for (let r = 0; r < reasoningRuns; r += 1) {
    for (let i = 0; i < per; i += 1) {
      s.apply({
        method: "item/delta",
        params: { thread_id: TID, item_id: "a1", delta: "x" },
      });
    }
    s.apply({
      method: "thread/reasoningDelta",
      params: { thread_id: TID, text: "r ", kind: "Content" },
    });
  }
  return s;
}

console.log(
  "--- id comparisons for a 120-delta turn, 6 reasoning runs interleaved ---",
);
console.log("(the reasoning runs are what displace the streaming target)");

for (const name of ["forward", "backward", "cached"]) {
  const mod = await buildVariant(name);
  for (const n of [1000, 4000]) {
    mod.resetCmp();
    const s = runStream(mod.Session, n);
    row(
      `${name.padEnd(8)} history=${String(n).padStart(4)}`,
      `comparisons=${mod.cmpCount()}  items=${s.items.length}`,
    );
  }
}

console.log(
  "\nforward degrades to O(N) because the target sits just before the tail\n" +
    "whenever a reasoning block resumes after the text; backward and cached stay flat.",
);