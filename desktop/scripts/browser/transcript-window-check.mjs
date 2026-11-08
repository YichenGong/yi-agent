// Real-browser regression for long-session transcript rendering.
//
// Run: cd desktop && npm run test:browser:transcript-window
//
// Why this cannot be a jsdom test: the defect is purely geometric. jsdom has no
// layout engine, so `getBoundingClientRect().height`, `offsetHeight` and
// `scrollHeight` are all 0 and the assertion "the spacer actually occupies space"
// is vacuous there. Both checks below were confirmed to fail on the pre-fix code
// and pass after it.
//
// No LLM, no sidecar, no production session data.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { setTimeout as delay } from "node:timers/promises";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const chrome =
  process.env.CHROME_BIN ||
  (process.platform === "darwin"
    ? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
    : "/usr/bin/google-chrome");
const port = Number(process.env.TRANSCRIPT_WINDOW_PORT || 1441);
const debugPort = Number(process.env.TRANSCRIPT_WINDOW_DEBUG_PORT || 9351);
const profile = await mkdtemp(resolve(tmpdir(), "yi-transcript-window-"));
const children = [];
let ws;

async function ready(url) {
  for (let n = 0; n < 100; n += 1) {
    try {
      const response = await fetch(url);
      if (response.ok) return response;
    } catch {
      /* still starting */
    }
    await delay(100);
  }
  throw new Error(`Timed out waiting for ${url}`);
}

function start(command, args) {
  const child = spawn(command, args, { cwd: root, stdio: ["ignore", "ignore", "pipe"] });
  children.push(child);
  child.on("error", error => console.error(error));
  child.stderr.on("data", () => {});
  return child;
}

try {
  start(process.execPath, [
    resolve(root, "node_modules/vite/bin/vite.js"),
    "--port",
    String(port),
    "--host",
    "127.0.0.1",
  ]);
  const pageUrl = `http://127.0.0.1:${port}/scripts/browser/transcript-window.html`;
  await ready(pageUrl);

  start(chrome, [
    "--headless=new",
    "--no-first-run",
    "--no-default-browser-check",
    `--remote-debugging-port=${debugPort}`,
    `--user-data-dir=${profile}`,
    pageUrl,
  ]);

  let page;
  for (let n = 0; n < 100 && !page; n += 1) {
    const pages = await (await ready(`http://127.0.0.1:${debugPort}/json/list`)).json();
    page = pages.find(target => target.type === "page" && target.url === pageUrl);
    if (!page) await delay(100);
  }
  assert.ok(page, "Chrome must open the fixture page");

  ws = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((resolveOpen, rejectOpen) => {
    ws.addEventListener("open", resolveOpen, { once: true });
    ws.addEventListener("error", rejectOpen, { once: true });
  });

  let seq = 0;
  const pending = new Map();
  ws.addEventListener("message", event => {
    const message = JSON.parse(event.data);
    if (message.method === "Runtime.exceptionThrown") {
      console.error("PAGE ERROR", JSON.stringify(message.params));
    }
    if (message.id) {
      pending.get(message.id)?.(message);
      pending.delete(message.id);
    }
  });

  const call = (method, params = {}) =>
    new Promise((resolveCall, rejectCall) => {
      const id = ++seq;
      const timeout = setTimeout(() => {
        pending.delete(id);
        rejectCall(new Error(`CDP timeout: ${method}`));
      }, 15000);
      pending.set(id, result => {
        clearTimeout(timeout);
        if (result.error) rejectCall(new Error(JSON.stringify(result.error)));
        else resolveCall(result);
      });
      ws.send(JSON.stringify({ id, method, params }));
    });

  const evaluate = async expression => {
    const result = await call("Runtime.evaluate", {
      expression,
      awaitPromise: true,
      returnByValue: true,
    });
    if (result.result.exceptionDetails) {
      throw new Error(JSON.stringify(result.result.exceptionDetails));
    }
    return result.result.result.value;
  };

  await call("Runtime.enable");

  let mounted = false;
  for (let n = 0; n < 100; n += 1) {
    if (await evaluate("Boolean(window.probe && document.querySelector('[data-item-id]'))")) {
      mounted = true;
      break;
    }
    await delay(100);
  }
  assert.ok(mounted, "Fixture did not mount within 10 seconds");
  await delay(300);

  const snapshot = () =>
    evaluate(`(() => {
      const scroller = document.querySelector('[data-thread-id]');
      const rows = [...document.querySelectorAll('[data-item-id]')];
      const top = document.querySelector("[data-spacer='top']");
      const bottom = document.querySelector("[data-spacer='bottom']");
      return {
        threadId: scroller.dataset.threadId,
        scrollTop: scroller.scrollTop,
        scrollHeight: scroller.scrollHeight,
        clientHeight: scroller.clientHeight,
        mounted: rows.length,
        firstId: rows[0]?.dataset.itemId ?? null,
        lastId: rows.at(-1)?.dataset.itemId ?? null,
        spacerTop: {
          styled: parseFloat(top.style.height) || 0,
          rendered: top.getBoundingClientRect().height,
          flexShrink: getComputedStyle(top).flexShrink,
        },
        spacerBottom: {
          styled: parseFloat(bottom.style.height) || 0,
          rendered: bottom.getBoundingClientRect().height,
          flexShrink: getComputedStyle(bottom).flexShrink,
        },
      };
    })()`);

  const pass = message => console.log(`PASS ${message}`);

  // ---------------------------------------------------------------- cold open
  // Following the tail: 60 of 600 mounted, so 540 items must be reserved above.
  const cold = await snapshot();
  assert.equal(cold.threadId, "A", `expected session A, got ${cold.threadId}`);
  assert.equal(cold.mounted, 60, `cold open must mount 60 items, got ${cold.mounted}`);
  assert.equal(cold.lastId, `A-${600 - 1}`, `cold open must show the newest item, got ${cold.lastId}`);
  assert.ok(
    cold.spacerTop.styled > 0,
    `cold open must reserve space above (styled height ${cold.spacerTop.styled})`,
  );

  assert.equal(cold.spacerTop.flexShrink, "0", "top spacer must not be flex-shrinkable");
  assert.equal(cold.spacerBottom.flexShrink, "0", "bottom spacer must not be flex-shrinkable");

  // THE core assertion. Before the fix, `style.height` was 53984px while the
  // rendered box was 0px, because the flex container shrank the child away.
  assert.ok(
    Math.abs(cold.spacerTop.rendered - cold.spacerTop.styled) <= 1,
    `top spacer must actually occupy its styled height: styled ${cold.spacerTop.styled}px, ` +
      `rendered ${cold.spacerTop.rendered}px`,
  );
  pass(`top spacer occupies ${cold.spacerTop.rendered}px (was 0px before the fix)`);

  // The scroller must be tall enough to hold the reserved range, not just the
  // mounted 60. Otherwise the scrollbar thumb is meaningless and the reserved area
  // has nowhere to live.
  const mountedHeight = cold.scrollHeight - cold.spacerTop.rendered - cold.spacerBottom.rendered;
  assert.ok(
    cold.scrollHeight >= cold.spacerTop.rendered + mountedHeight,
    "scrollHeight must include the reserved range",
  );
  assert.ok(
    cold.scrollHeight > 4 * cold.clientHeight,
    `scrollHeight must reflect the whole session: got ${cold.scrollHeight}px for ` +
      `${cold.clientHeight}px viewport`,
  );
  pass(`scrollHeight ${cold.scrollHeight}px covers the reserved history`);

  // --------------------------------------------------- paging through history
  // Jump into the middle of the reserved range. The window must page in more
  // items; landing inside the spacer with nothing mounted is the "blank screen"
  // symptom.
  await evaluate("const s=document.querySelector('[data-thread-id]');s.scrollTop=Math.round((s.scrollHeight-s.clientHeight)*0.5)");
  await delay(500);
  const mid = await snapshot();
  assert.ok(
    mid.mounted > 60,
    `scrolling into the reserved range must page in history, still ${mid.mounted} mounted`,
  );
  const rowsInViewport = await evaluate(`(() => {
    const scroller = document.querySelector('[data-thread-id]');
    const rect = scroller.getBoundingClientRect();
    return [...document.querySelectorAll('[data-item-id]')].filter(row => {
      const box = row.getBoundingClientRect();
      return box.bottom > rect.top && box.top < rect.bottom;
    }).length;
  })()`);
  assert.ok(
    rowsInViewport > 0,
    "the viewport must contain mounted items after paging (blank screen otherwise)",
  );
  pass(`paged to ${mid.mounted} items, ${rowsInViewport} rows visible in the viewport`);

  // -------------------------------------------------- no cross-session leakage
  // Scroll A away from the tail, then switch to the equal-length B. B must open on
  // its own tail with a fresh first screen, not inherit A's paged window.
  assert.equal((await snapshot()).threadId, "A", "still on A before the switch");
  await evaluate("probe.select('B')");
  await delay(700);
  const switched = await snapshot();
  assert.equal(switched.threadId, "B", `expected session B, got ${switched.threadId}`);
  assert.equal(
    switched.mounted,
    60,
    `a session switch must restart from the first screen, got ${switched.mounted} mounted ` +
      `(inherited window ${mid.mounted})`,
  );
  assert.equal(
    switched.lastId,
    `B-${600 - 1}`,
    `session B must open on its newest item, got ${switched.lastId}`,
  );
  assert.ok(
    Math.abs(switched.scrollTop - (switched.scrollHeight - switched.clientHeight)) <= 4,
    `session B must open scrolled to the bottom: scrollTop ${switched.scrollTop}, ` +
      `max ${switched.scrollHeight - switched.clientHeight}`,
  );
  pass("switching sessions restarts from the tail of the new session");
} finally {
  ws?.close();
  for (const child of children) child.kill("SIGTERM");
  await Promise.all(
    children.map(child =>
      child.exitCode !== null || child.signalCode !== null
        ? Promise.resolve()
        : new Promise(resolveExit => child.once("exit", resolveExit)),
    ),
  );
  await rm(profile, { recursive: true, force: true, maxRetries: 3, retryDelay: 200 });
}
