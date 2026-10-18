/**
 * §3.2: validate the *actual* design against real mutation semantics.
 *
 * The earlier render benchmark replaced the item object on every delta, which is
 * NOT what `session.ts` does: it mutates `item.text += delta` in place, so the
 * item identity never changes. A memo comparator that reads `item.text` would
 * therefore always compare the current value against itself and bail out — the
 * streamed reply would freeze.
 *
 * This script mirrors the real semantics (mutate in place) and checks that
 * passing the mutable text down as its own primitive prop keeps the tail live
 * while the frozen prefix stays untouched.
 *
 * It also asserts the freeze, so the harness proves it can detect the bug it
 * exists to prevent — a test that cannot fail is worthless.
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
const { flushSync } = await import("react-dom");
const h = React.createElement;

const HISTORY = 4000;
const DELTAS = 200;

/** Mirrors the real `AgentMessage`: memo keyed on the text primitive. */
const AgentMessage = React.memo(
  function AgentMessage({ text }) {
    return h("div", null, text);
  },
  (p, n) => p.text === n.text,
);

/** WRONG: compares `item.text`, which in-place mutation makes self-comparing. */
const ChatItemTrap = React.memo(
  function ChatItemTrap({ item }) {
    return item.type === "agentMessage" ? h(AgentMessage, { text: item.text }) : h("div", null, item.text);
  },
  (p, n) => p.item === n.item && p.item.text === n.item.text,
);

/** RIGHT: the mutable text travels as its own immutable string prop. */
const ChatItemOk = React.memo(
  function ChatItemOk({ item, text }) {
    return item.type === "agentMessage" ? h(AgentMessage, { text }) : h("div", null, text);
  },
  (p, n) => p.item === n.item && p.text === n.text,
);

/** Frozen prefix: bails out entirely while `count`/`version` are unchanged. */
const FrozenPrefix = React.memo(function FrozenPrefix({ items, count, version, ChatItem }) {
  void version; // participates only through the memo comparator
  const out = [];
  for (let i = 0; i < count; i += 1) {
    out.push(h(ChatItem, { key: items[i].id, item: items[i], text: textOf(items[i]) }));
  }
  return h(React.Fragment, null, out);
}, (p, n) => p.items === n.items && p.count === n.count && p.version === n.version);

function textOf(item) {
  return typeof item?.text === "string" ? item.text : "";
}

const root = createRoot(document.getElementById("root"));

function makeItems() {
  const items = [];
  for (let i = 0; i < HISTORY; i += 1) {
    items.push({ type: "agentMessage", id: `h${i}`, text: `history ${i}` });
  }
  items.push({ type: "agentMessage", id: "tail", text: "" });
  return items;
}

/** One turn: mutate the tail's text in place, exactly like `Session.apply`. */
function runTurn({ ChatItem, frozen, label }) {
  const items = makeItems();
  const tail = items[items.length - 1];
  let version = 1;

  const paint = () => {
    const renderedTailText = textOf(tail);
    if (frozen) {
      root.render(
        h(
          "div",
          null,
          h(FrozenPrefix, { items, count: items.length - 1, version, ChatItem }),
          h(ChatItem, { key: tail.id, item: tail, text: renderedTailText }),
        ),
      );
    } else {
      root.render(
        h("div", null, items.map((it) => h(ChatItem, { key: it.id, item: it, text: textOf(it) }))),
      );
    }
  };

  flushSync(paint);
  let lastSeen = "";
  const t0 = process.hrtime.bigint();
  for (let d = 0; d < DELTAS; d += 1) {
    tail.text += "tok ";          // in-place, like the real Session
    flushSync(paint);
    lastSeen = textOf(tail);
  }
  const t1 = process.hrtime.bigint();

  const dom = document.getElementById("root").textContent;
  const liveTextVisible = dom.includes(`${lastSeen}`) && lastSeen.length > 0;
  console.log(
    `${label.padEnd(34)} per-delta=${(Number(t1 - t0) / DELTAS / 1e6).toFixed(2)} ms  ` +
      `tail live: ${liveTextVisible ? "yes" : "NO (frozen!)"}`,
  );
  return liveTextVisible;
}

console.log("--- real mutation semantics: `tail.text += delta` in place ---");
const trapLive = runTurn({ ChatItem: ChatItemTrap, frozen: false, label: "trap: compare item.text, no prefix" });
const okNoPrefix = runTurn({ ChatItem: ChatItemOk, frozen: false, label: "ok: text prop, no prefix" });
const okFrozen = runTurn({ ChatItem: ChatItemOk, frozen: true, label: "ok: text prop + frozen prefix" });

console.log("\n--- does the harness detect the trap? ---");
console.log(`trap stays live:        ${trapLive}   (expected false — comparing item.text self-compares)`);
console.log(`text-prop stays live:   ${okNoPrefix}   (expected true)`);
console.log(`frozen stays live:      ${okFrozen}   (expected true)`);
console.log(
  trapLive === false && okNoPrefix === true && okFrozen === true
    ? "\nHarness is sound: it reproduces the freeze AND the fix."
    : "\nHarness is NOT sound — do not trust its timings.",
);