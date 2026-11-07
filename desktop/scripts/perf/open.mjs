/**
 * §7: the **client half** of "opening a long session".
 *
 * The server half lives in `scripts/bench_resume.py` (real app-server, real
 * session logs). This file measures what the desktop transcript costs after the
 * items have arrived — which the design shows is the dominant half:
 *
 *     3251 items, full render, one shot       773 ms
 *     same, real chunked replay (26 frames)   822 ms
 *     only the most recent 60                  21 ms
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

// Reuse the production window rule. Writing a copy of the paging logic here
// would measure an algorithm that is not the one that ships, and any number it
// produced would be meaningless.
const { nextWindow, DEFAULT_LIMITS } = await import("../../src/lib/transcriptWindow.ts");

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
 * Mirrors `ChatView`: a history range plus a separately mounted live tail.
 *
 * Two ways to select the range, because the windowed case is not always
 * anchored to the tail:
 * - `limit`: mount the last N items (the cold-open case).
 * - `win`: mount `[win.start, win.end)`, which is what paging up produces —
 *   once the user leaves the tail, `end` stops tracking `itemCount` and the
 *   range is no longer "the last N". Modelling it as a limit would silently
 *   over-mount and inflate both the node count and the time.
 */
function View({ items, limit, win }) {
  const start = win ? win.start : limit == null ? 0 : Math.max(0, items.length - limit);
  const end = win ? win.end : items.length;
  // The final item is always rendered separately as the live tail.
  const count = items.length - 1;
  const rows = [];
  for (let i = start; i < Math.min(count, end); i += 1) {
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
 * Mirrors the server's `chunk_items_for_replay`
 * (yi-agent-rs/crates/yi-agent-app-server/src/server/turn/replay.rs):
 * a frame closes when adding the next item would exceed 256 KiB **or** the frame
 * already holds 200 items, whichever comes first. Chunking by count alone is
 * wrong here — tool-call JSON is large, so real logs get ~112 items/frame.
 */
const REPLAY_CHUNK_BYTES = 256 * 1024;
const REPLAY_CHUNK_MAX_ITEMS = 200;

function chunkItemsForReplay(items) {
  const chunks = [];
  let cur = [];
  let curBytes = 0;
  for (const item of items) {
    const approx = Buffer.byteLength(JSON.stringify(item));
    if (cur.length > 0 && curBytes + approx > REPLAY_CHUNK_BYTES) {
      chunks.push(cur);
      cur = [];
      curBytes = 0;
    }
    cur.push(item);
    curBytes += approx;
    if (cur.length >= REPLAY_CHUNK_MAX_ITEMS) {
      chunks.push(cur);
      cur = [];
      curBytes = 0;
    }
  }
  if (cur.length > 0) chunks.push(cur);
  return chunks;
}

/**
 * Paged replay: the *actual* cold-open path today. The server sends history as
 * `items/completed` frames (see `chunkItemsForReplay`), and each frame
 * re-reconciles a longer list.
 */
async function renderPaged(items, chunks, label) {
  const el = document.createElement("div");
  document.body.appendChild(el);
  const root = createRoot(el);
  const t0 = process.hrtime.bigint();
  let frames = 0;
  let n = 0;
  for (const chunk of chunks) {
    n += chunk.length;
    root.render(h(View, { items: items.slice(0, n), limit: null }));
    await new Promise((r) => setTimeout(r, 0));
    frames += 1;
  }
  const ms = Number(process.hrtime.bigint() - t0) / 1e6;
  console.log(
    `${label.padEnd(46)} ${ms.toFixed(0).padStart(6)} ms   ` +
      `frames=${String(frames).padStart(4)}  nodes=${String(el.querySelectorAll("*").length).padStart(6)}`,
  );
  root.unmount();
  el.remove();
  return ms;
}

/**
 * Windowed open, then page up the way a user would: start at the tail (60
 * items), and each time the viewport sits at the top of the mounted range,
 * ask the production `nextWindow` for the next page, until the mounted count
 * hits `maxMounted`. This measures the real cold-open-then-scroll-up path
 * rather than an idealized constant window.
 */
async function renderWindowed(items, label) {
  const el = document.createElement("div");
  document.body.appendChild(el);
  const root = createRoot(el);
  const itemCount = items.length;
  const t0 = process.hrtime.bigint();

  let win = nextWindow({ current: null, itemCount, limits: DEFAULT_LIMITS, visible: null });
  // Render the real range, not "the last N": after paging, `end` no longer
  // tracks `itemCount`, and collapsing that to a trailing limit would mount
  // more rows than the algorithm actually asks for.
  const draw = () => {
    root.render(h(View, { items, win }));
  };
  draw();
  await new Promise((r) => setTimeout(r, 0));

  let frames = 1;
  let maxMountedSeen = win.end - win.start;
  // Sit at the top edge: gapTopPx 0 means the first mounted row is flush with
  // the viewport top, and the bottom edge is far below (negative gap).
  while (win.start > 0) {
    const next = nextWindow({
      current: win,
      itemCount,
      limits: DEFAULT_LIMITS,
      visible: { first: win.start, last: win.start + 10, gapTopPx: 0, gapBottomPx: -5000 },
    });
    if (next.start === win.start && next.end === win.end) break; // no progress
    win = next;
    draw();
    await new Promise((r) => setTimeout(r, 0));
    frames += 1;
    maxMountedSeen = Math.max(maxMountedSeen, win.end - win.start);
    if (win.end - win.start >= DEFAULT_LIMITS.maxMounted) break;
  }

  const ms = Number(process.hrtime.bigint() - t0) / 1e6;
  console.log(
    `${label.padEnd(46)} ${ms.toFixed(0).padStart(6)} ms   ` +
      `frames=${String(frames).padStart(4)}  mounted=${String(
        el.querySelectorAll("[data-item-id]").length,
      ).padStart(5)}  cap=${maxMountedSeen}`,
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
await renderPaged(items, chunkItemsForReplay(items), "today: real paged replay (byte-or-count chunks)");
console.log("");
await renderOnce(items, 60, "proposed: window = last 60");
await renderOnce(items, 400, "proposed: window = last 400 (unload cap)");
await renderOnce(items, 30, "reference: window = last 30");
await renderWindowed(items, "proposed: windowed open + page up till cap");
console.log("");
console.log("Reminder: jsdom, not WKWebView. Ratios transfer, absolute ms do not.");
