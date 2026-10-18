/**
 * §1.1(b) + §3.2: per-delta re-render cost, and why `memo` alone is not enough.
 *
 * Runs the real `react-dom` under jsdom (no WebKit), so the absolute
 * milliseconds are not the point — the **growth with history depth** is.
 *
 * Also compares the three render shapes the spec discusses, which is what
 * justifies "freeze the prefix" over "just wrap ChatItem in memo".
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
const ReactMarkdown = (await import("react-markdown")).default;
const h = React.createElement;

const Md = React.memo(
  function Md({ text }) {
    return h(ReactMarkdown, null, text);
  },
  (p, n) => p.text === n.text,
);

function ChatItemBase({ item }) {
  if (item.type === "agentMessage") {
    return h("div", { className: "msg" }, h(Md, { text: item.text }));
  }
  return h("div", { className: "msg" }, item.text);
}

/** The spec's proposal: memo keyed on the mutating primitive, not identity. */
const ChatItemMemo = React.memo(
  ChatItemBase,
  (p, n) => p.item === n.item && p.item.text === n.item.text,
);

/** The frozen prefix: props stable during streaming (length + element refs). */
const FrozenPrefix = React.memo(function FrozenPrefix({ items }) {
  return h(
    React.Fragment,
    null,
    items.map((it) => h(ChatItemBase, { key: it.id, item: it })),
  );
});

const root = createRoot(document.getElementById("root"));

function makeItems(n) {
  const items = [];
  for (let i = 0; i < n; i += 1) {
    items.push({ type: "agentMessage", id: `i${i}`, text: `history message ${i}` });
  }
  return items;
}

async function run(label, renderFn, items, deltas = 200) {
  root.render(renderFn(items));
  await new Promise((r) => setTimeout(r, 0));
  let text = "";
  const t0 = process.hrtime.bigint();
  for (let k = 0; k < deltas; k += 1) {
    text += "tok ";
    items[items.length - 1] = {
      type: "agentMessage",
      id: `i${items.length - 1}`,
      text,
    };
    root.render(renderFn(items));
    await new Promise((r) => setTimeout(r, 0));
  }
  const t1 = process.hrtime.bigint();
  console.log(
    `${label}  per-delta=${(Number(t1 - t0) / deltas / 1e6).toFixed(2)} ms`,
  );
}

console.log("--- every delta re-renders the whole list (baseline) ---");
for (const n of [100, 500, 1000, 2000, 4000]) {
  const items = makeItems(n);
  await run(
    `items=${String(n).padStart(5)}`,
    (it) => h("div", null, it.map((x) => h(ChatItemBase, { key: x.id, item: x }))),
    items,
  );
}

console.log("\n--- the three shapes at 4000 history items ---");
{
  const items = makeItems(4000);
  await run(
    "baseline (no memo on ChatItem)",
    (it) => h("div", null, it.map((x) => h(ChatItemBase, { key: x.id, item: x }))),
    items,
  );
}
{
  const items = makeItems(4000);
  await run(
    "memo(ChatItem) by text        ",
    (it) => h("div", null, it.map((x) => h(ChatItemMemo, { key: x.id, item: x }))),
    items,
  );
}
{
  const items = makeItems(4000);
  const frozen = items.slice(0, -1);
  await run(
    "frozen prefix + 1 live tail   ",
    (it) => h("div", null, h(FrozenPrefix, { items: frozen }), h(ChatItemBase, { item: it[it.length - 1] })),
    items,
  );
}