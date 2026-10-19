/**
 * What do the remaining changes actually buy?
 *
 * This is the "so what" measurement: one realistic turn, four shapes of the
 * code, same work each time. It answers the question the other scripts cannot —
 * the per-delta curves show *growth*, this shows the *turn* the user waits for.
 *
 * Scenario (one long reply inside an already-long session):
 *   - 4000 history items (a session that has been open for a while)
 *   - a ~10k-char reply arriving in 30-char deltas (334 deltas)
 *
 * Four variants:
 *   current      — full `items.map` every delta, memo only on MarkdownText
 *   G2           — frozen history prefix + only the live tail re-renders
 *   G2+G3        — the same, plus the streaming tail's markdown is throttled
 *
 * Two caveats, stated up front because they matter:
 *   1. jsdom, not WebKit — trust the ratios, not the absolute ms.
 *   2. `flushSync` makes each delta's render synchronous so it can be attributed.
 *      The throttle decides on a *modelled* cadence of 6 ms/delta, which is what
 *      the earlier streaming measurement produced (8025 ms / 1334 deltas), and
 *      it uses an injectable clock rather than real timers so the policy is
 *      what gets measured.
 */

import { JSDOM } from "jsdom";

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
// `flushSync` lives on the `react-dom` entry, not `react-dom/client`.
const { flushSync } = await import("react-dom");
const ReactMarkdown = (await import("react-markdown")).default;
const h = React.createElement;

const Md = React.memo(
  function Md({ text }) {
    return h(ReactMarkdown, null, text);
  },
  (p, n) => p.text === n.text,
);

function ChatItem({ item }) {
  if (item.type === "agentMessage") {
    return h("div", { className: "msg" }, h(Md, { text: item.text }));
  }
  return h("div", { className: "msg" }, item.text);
}

/** History whose props stay stable while only the tail streams. */
const FrozenPrefix = React.memo(function FrozenPrefix({ items }) {
  return h(
    React.Fragment,
    null,
    items.map((it) => h(ChatItem, { key: it.id, item: it })),
  );
});

const root = createRoot(document.getElementById("root"));

const HISTORY = 4000;
/** Set by the depth sweep below; `null` means "use HISTORY". */
let HISTORY_OVERRIDE = null;
const DELTAS = 334;
const CHARS_PER_DELTA = 30;
const THROTTLE_MS = 60;
const MODELLED_DELTA_MS = 6; // grounded in the earlier streaming measurement

const CHUNK = "lorem ipsum **bold** and `code` sentence here. ";

function history() {
  const items = [];
  const n = HISTORY_OVERRIDE ?? HISTORY;
  for (let i = 0; i < n; i += 1) {
    items.push({ type: "agentMessage", id: `h${i}`, text: `history message ${i}` });
  }
  return items;
}

function render(variant, tailText, frozen, tailItem) {
  if (variant === "current") {
    const all = frozen.concat([{ type: "agentMessage", id: "tail", text: tailText }]);
    root.render(h("div", null, all.map((x) => h(ChatItem, { key: x.id, item: x }))));
  } else {
    root.render(
      h(
        "div",
        null,
        h(FrozenPrefix, { items: frozen }),
        h(ChatItem, { key: "tail", item: { ...tailItem, text: tailText } }),
      ),
    );
  }
}

/** One turn. Returns the wall time attributable to per-delta work. */
function runTurn(variant) {
  const frozen = history();
  const tailItem = { type: "agentMessage", id: "tail", text: "" };

  flushSync(() => render(variant, "", frozen, tailItem));

  let text = "";
  let lastMdText = "";
  let lastThrottleAt = -Infinity;
  let virtualNow = 0;
  const throttled = variant === "G2+G3";

  const t0 = process.hrtime.bigint();
  for (let d = 0; d < DELTAS; d += 1) {
    virtualNow += MODELLED_DELTA_MS;
    text += CHUNK.slice(0, CHARS_PER_DELTA);

    let shown = text;
    if (throttled) {
      // Trailing-edge throttle: skip until the window elapses, always show the
      // final value (the last iteration is not skipped).
      const isLast = d === DELTAS - 1;
      if (!isLast && virtualNow - lastThrottleAt < THROTTLE_MS) continue;
      lastThrottleAt = virtualNow;
    }
    if (shown === lastMdText && variant === "current") {
      // memo would absorb it; keep the list rebuild honest though
    }
    lastMdText = shown;

    flushSync(() => render(variant, shown, frozen, tailItem));
  }
  const t1 = process.hrtime.bigint();
  return { ms: Number(t1 - t0) / 1e6, chars: text.length };
}

console.log(
  `--- one turn: ${HISTORY} history items, ${DELTAS} deltas (${DELTAS * CHARS_PER_DELTA} chars) ---`,
);
console.log("(G2+G3 assumes 6 ms/delta cadence, 60 ms throttle window)\n");

const results = {};
for (const variant of ["current", "G2", "G2+G3"]) {
  const r = runTurn(variant);
  results[variant] = r;
  console.log(
    `${variant.padEnd(8)}  turn=${String(Math.round(r.ms)).padStart(6)} ms   ` +
      `per-delta=${(r.ms / DELTAS).toFixed(2)} ms`,
  );
}

const base = results["current"].ms;
console.log("\n--- what the changes buy, relative to today ---");
for (const variant of ["G2", "G2+G3"]) {
  const r = results[variant];
  console.log(
    `${variant.padEnd(8)}  ${(base / r.ms).toFixed(1)}x faster on this turn   ` +
      `(saves ${Math.round(base - r.ms)} ms of main-thread work)`,
  );
}

// The user's actual complaint is growth: "the longer the thread, the slower it
// gets". Sweep the history depth and show which shapes grow.
console.log("\n--- does it grow with the thread? (same turn, different history) ---");
const depths = [0, 500, 1000, 2000, 4000];
const table = [];
for (const variant of ["current", "G2"]) {
  const row = [variant];
  for (const depth of depths) {
    HISTORY_OVERRIDE = depth;
    row.push(Math.round(runTurn(variant).ms));
  }
  table.push(row);
}
HISTORY_OVERRIDE = null;
const header = ["variant", ...depths.map((d) => `${d} items`)];
console.log(header.map((c, i) => String(c).padStart(i === 0 ? 8 : 10)).join(""));
for (const row of table) {
  console.log(row.map((c, i) => String(c).padStart(i === 0 ? 8 : 10)).join(""));
}

console.log(
  "\nThe point is not the multiplier — it is that the `current` row climbs with\n" +
    "the history above the reply while G2 stays flat. That climb is what the user\n" +
    "feels as \"the longer the thread, the slower it gets\".",
);