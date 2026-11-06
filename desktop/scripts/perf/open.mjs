/**
 * §7: the **client half** of "opening a long session".
 *
 * The server half lives in `scripts/bench_resume.py` (real app-server, real
 * session logs). This file measures what the desktop transcript costs after the
 * items have arrived — which the design shows is the dominant half:
 *
 *     1361 markdown items, full render        768 ms
 *     same, paged 200/frame (real replay)    1506 ms
 *     only the most recent 60                   54 ms
 *
 * It consumes the **real** production rendering stack (`react-markdown` +
 * `remark-gfm` + `rehype-highlight` from `desktop/node_modules`) and a **real**
 * session log, so the numbers are not a toy approximation. It writes nothing.
 *
 * Usage:
 *     cd desktop
 *     node scripts/perf/open.mjs                       # auto-pick the longest thread log
 *     node scripts/perf/open.mjs path/to/thread.jsonl  # explicit log
 *     ITEMS_LIMIT=400 node scripts/perf/open.mjs       # cap items for a quicker run
 *
 * Where the log comes from: `<repo>/.yi-agent/threads/thread-*.jsonl`. If none is
 * found the script reports that honestly and exits 0 rather than inventing data
 * (same rule as `checkpoint.mjs`).
 *
 * Honest boundary: this runs under jsdom, not WKWebView. The absolute numbers are
 * not device numbers; the **ratios** and the growth shape are what transfer.
 */

import { readFileSync, readdirSync, statSync, existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { JSDOM } from "jsdom";

// ---------------------------------------------------------------------------
// DOM harness (same shape as markdown.mjs, so results are comparable)
// ---------------------------------------------------------------------------

const dom = new JSDOM("<!doctype html><html><body><div id=root></div></body></html>", {
  pretendToBeVisual: true,
});
globalThis.window = dom.window;
globalThis.document = dom.window.document;
Object.defineProperty(globalThis, "navigator", {
  value: dom.window.navigator,
  configurable: true,
});
globalThis.HTMLElement = dom.window.HTMLElement;
globalThis.Element = dom.window.Element;
globalThis.Node = dom.window.Node;
globalThis.MutationObserver = dom.window.MutationObserver;
globalThis.requestAnimationFrame = (cb) => setTimeout(() => cb(Date.now()), 16);
globalThis.cancelAnimationFrame = (id) => clearTimeout(id);

const React = (await import("react")).default;
const { createRoot } = await import("react-dom/client");
const ReactMarkdown = (await import("react-markdown")).default;
const remarkGfm = (await import("remark-gfm")).default;
const rehypeHighlight = (await import("rehype-highlight")).default;
const h = React.createElement;

const HERE = dirname(fileURLToPath(import.meta.url));
const DESKTOP = resolve(HERE, "../..");

// ---------------------------------------------------------------------------
// Locate a real transcript
// ---------------------------------------------------------------------------

/**
 * Every plausible `<repo>/.yi-agent/threads`, deduplicated.
 *
 * A git worktree has no `.yi-agent/` of its own, so walking up from `desktop/`
 * is not enough: the interesting logs live in the **main** checkout. For a
 * worktree, `.git` is a *file* pointing at the real git dir, whose parent's
 * parent is the main repository root.
 */
function candidateThreadDirs() {
  const seen = new Set();
  const out = [];
  const push = (p) => {
    const abs = resolve(p);
    if (seen.has(abs)) return;
    seen.add(abs);
    if (existsSync(abs)) out.push(abs);
  };

  // 1. Walk ancestors of this worktree/checkout.
  let dir = DESKTOP;
  for (;;) {
    push(join(dir, ".yi-agent/threads"));
    const parent = dirname(dir);
    if (parent === dir) break;
    dir = parent;
  }

  // 2. If this is a git worktree, follow `.git` back to the main checkout.
  try {
    const dotGit = join(DESKTOP, "../.git");
    if (existsSync(dotGit) && statSync(dotGit).isFile()) {
      const text = readFileSync(dotGit, "utf8").trim();
      const m = /^gitdir:\s*(.+)$/m.exec(text);
      if (m) {
        // <main>/.git/worktrees/<name> -> <main>
        const gitDir = resolve(DESKTOP, "../", m[1]);
        push(join(gitDir, "../..", ".yi-agent/threads"));
      }
    }
  } catch {
    // A missing/unparsable `.git` only costs us candidates, never correctness.
  }

  // 3. The user's global logs are the last resort.
  if (process.env.HOME) push(join(process.env.HOME, ".yi-agent/threads"));
  return out;
}

function pickLog(explicit) {
  if (explicit) {
    if (!existsSync(explicit)) {
      console.log(`no such log: ${explicit}`);
      return null;
    }
    return explicit;
  }
  const files = [];
  for (const dir of candidateThreadDirs()) {
    for (const name of readdirSync(dir)) {
      if (!name.endsWith(".jsonl")) continue;
      const path = join(dir, name);
      files.push({ path, size: statSync(path).size });
    }
  }
  if (files.length === 0) return null;
  files.sort((a, b) => b.size - a.size);
  return files[0].path;
}

/**
 * Flatten a thread log into its items.
 *
 * Each jsonl line is one turn carrying `items` (and a full `messages` snapshot,
 * which is *not* rendered — the transcript shows items).
 */
function loadItems(path) {
  const items = [];
  for (const line of readFileSync(path, "utf8").split("\n")) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    let turn;
    try {
      turn = JSON.parse(trimmed);
    } catch {
      continue; // corrupt line: the effects are the same as the server skipping it
    }
    if (Array.isArray(turn.items)) items.push(...turn.items);
  }
  const limit = Number(process.env.ITEMS_LIMIT ?? 0);
  return limit > 0 ? items.slice(-limit) : items;
}

// ---------------------------------------------------------------------------
// The rendering stack, mirroring `ChatView`'s structure
// ---------------------------------------------------------------------------

/** Mirrors `MarkdownText` (memo keyed on text) with the production plugins. */
const Md = React.memo(
  function Md({ text }) {
    return h(
      ReactMarkdown,
      { remarkPlugins: [remarkGfm], rehypePlugins: [rehypeHighlight] },
      text,
    );
  },
  (prev, next) => prev.text === next.text,
);

/**
 * Mirrors `ChatItem`: memoized on the item object, with `text` as its own
 * primitive prop. Tool calls render JSON, which is what makes them expensive.
 */
const ChatItem = React.memo(
  function ChatItem({ item }) {
    const body =
      item.type === "toolCall"
        ? h("pre", null, JSON.stringify(item.input ?? {}, null, 2))
        : h(Md, { text: item.text ?? "" });
    return h("div", { "data-item-id": item.id }, body);
  },
  (prev, next) => prev.item === next.item,
);

/**
 * Mirrors `ChatView`: a history prefix of `count` items plus a separately
 * mounted live tail. `limit` is the *windowed* variant — when set, only the
 * last `limit` items are mounted, which is what the design proposes.
 */
function View({ items, limit }) {
  const start = limit == null ? 0 : Math.max(0, items.length - limit);
  const count = items.length - 1;
  const rows = [];
  for (let i = start; i < count; i += 1) {
    rows.push(h(ChatItem, { key: items[i].id, item: items[i] }));
  }
  const tail = items[items.length - 1];
  return h("div", null, rows, tail ? h(ChatItem, { key: tail.id, item: tail }) : null);
}

// ---------------------------------------------------------------------------
// Measurements
// ---------------------------------------------------------------------------

async function renderOnce(items, limit, label) {
  const el = document.createElement("div");
  document.body.appendChild(el);
  const root = createRoot(el);
  const t0 = process.hrtime.bigint();
  root.render(h(View, { items, limit }));
  await new Promise((r) => setTimeout(r, 0));
  const ms = Number(process.hrtime.bigint() - t0) / 1e6;
  const nodes = el.querySelectorAll("*").length;
  const mounted = el.querySelectorAll("[data-item-id]").length;
  console.log(
    `${label.padEnd(46)} ${ms.toFixed(0).padStart(6)} ms   ` +
      `mounted=${String(mounted).padStart(5)}  nodes=${String(nodes).padStart(6)}`,
  );
  root.unmount();
  el.remove();
  return ms;
}

/**
 * Paged replay: the *actual* cold-open path today. The server sends history as
 * `items/completed` frames of 200 items, and each frame re-reconciles a longer
 * list.
 */
async function renderPaged(items, chunk, label) {
  const el = document.createElement("div");
  document.body.appendChild(el);
  const root = createRoot(el);
  const t0 = process.hrtime.bigint();
  let frames = 0;
  for (let n = chunk; n < items.length; n += chunk) {
    root.render(h(View, { items: items.slice(0, n), limit: null }));
    await new Promise((r) => setTimeout(r, 0));
    frames += 1;
  }
  root.render(h(View, { items, limit: null }));
  await new Promise((r) => setTimeout(r, 0));
  frames += 1;
  const ms = Number(process.hrtime.bigint() - t0) / 1e6;
  console.log(
    `${label.padEnd(46)} ${ms.toFixed(0).padStart(6)} ms   ` +
      `frames=${String(frames).padStart(4)}  nodes=${String(el.querySelectorAll("*").length).padStart(6)}`,
  );
  root.unmount();
  el.remove();
  return ms;
}

// ---------------------------------------------------------------------------

const logPath = pickLog(process.argv[2]);
if (!logPath) {
  console.log("no thread log found — nothing to measure (this is not a failure).");
  console.log("pass one explicitly, or set ITEMS_LIMIT on a smaller log:");
  for (const dir of candidateThreadDirs()) console.log(`  ${dir}`);
  process.exit(0);
}

const items = loadItems(logPath);
const markdownItems = items.filter(
  (i) => i.type === "agentMessage" || i.type === "userMessage",
).length;
const toolItems = items.filter((i) => i.type === "toolCall").length;

console.log(`log     : ${logPath}`);
console.log(
  `items   : ${items.length}  (markdown=${markdownItems}, toolCall=${toolItems})`,
);
console.log("");

await renderOnce(items, null, "today: full render, one shot");
await renderPaged(items, 200, "today: paged replay 200 items/frame");
console.log("");
await renderOnce(items, 60, "proposed: window = last 60");
await renderOnce(items, 400, "proposed: window = last 400 (unload cap)");
await renderOnce(items, 30, "reference: window = last 30");
console.log("");
console.log("Reminder: jsdom, not WKWebView. Ratios transfer, absolute ms do not.");
