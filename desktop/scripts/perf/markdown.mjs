/**
 * §1.1(c): streaming markdown is quadratic over a turn.
 *
 * `MarkdownText` is memoized on `text`, but the message that is *growing*
 * changes on every delta — so it re-parses the whole accumulated text each
 * time. Per parse that is linear in the current length; summed over a turn it
 * is quadratic.
 *
 * Both measurements below use the real `react-markdown` under jsdom.
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

/** Mirrors `MarkdownText`: memo keyed on `text`. */
const Md = React.memo(
  function Md({ text }) {
    return h(ReactMarkdown, null, text);
  },
  (p, n) => p.text === n.text,
);

const root = createRoot(document.getElementById("root"));
const CHUNK = "lorem ipsum **bold** and `code` sentence here. ";

function render(text) {
  return new Promise((resolve) => {
    root.render(h(Md, { text }));
    setTimeout(resolve, 0);
  });
}

console.log("--- one parse, by current text length (lines of the quadratic) ---");
for (const len of [500, 2000, 8000, 20000, 40000]) {
  const text = CHUNK.repeat(Math.ceil(len / CHUNK.length));
  // The memo is keyed on `text`, so a second render of the *same* string is a
  // no-op and would measure nothing. Render a fresh string each time (one extra
  // character) so every measurement is a real re-parse.
  await render(text + " ");
  const t0 = process.hrtime.bigint();
  await render(text);
  const t1 = process.hrtime.bigint();
  console.log(
    `chars=${String(text.length).padStart(6)}  render=${(Number(t1 - t0) / 1e6).toFixed(1)} ms`,
  );
}

console.log("\n--- a whole streamed turn, 30 chars per delta ---");
for (const total of [2000, 5000, 10000, 20000, 40000]) {
  let text = "";
  let frames = 0;
  const t0 = process.hrtime.bigint();
  while (text.length < total) {
    text += CHUNK.slice(0, 30);
    root.render(h(Md, { text }));
    frames += 1;
    await new Promise((r) => setTimeout(r, 0));
  }
  const t1 = process.hrtime.bigint();
  console.log(
    `final=${String(total).padStart(6)} chars  frames=${String(frames).padStart(5)}  wall=${(Number(t1 - t0) / 1e6).toFixed(0)} ms`,
  );
}