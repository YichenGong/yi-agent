// No browser automation dependency: use Node's WebSocket and Chrome DevTools Protocol.
// Run: cd desktop && npm run test:browser:session-view
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { setTimeout as delay } from "node:timers/promises";
const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const chrome = process.env.CHROME_BIN || (process.platform === "darwin"
  ? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" : "/usr/bin/google-chrome");
const port = Number(process.env.SESSION_VIEW_PORT || 1438);
const debugPort = Number(process.env.SESSION_VIEW_DEBUG_PORT || 9348);
const profile = await mkdtemp(resolve(tmpdir(), "yi-session-view-browser-"));
const children = [];
let ws;
async function ready(url) {
  for (let n = 0; n < 100; n++) {
    try { const response = await fetch(url); if (response.ok) return response; } catch { /* starting */ }
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
  start(process.execPath, [resolve(root, "node_modules/vite/bin/vite.js"), "--port", String(port), "--host", "127.0.0.1"]);
  await ready(`http://127.0.0.1:${port}/scripts/browser/session-view.html`);
  start(chrome, ["--headless=new", "--no-first-run", "--no-default-browser-check",
    `--remote-debugging-port=${debugPort}`, `--user-data-dir=${profile}`, `http://127.0.0.1:${port}/scripts/browser/session-view.html`]);
  let page;
  for (let n = 0; n < 100 && !page; n++) {
    const pages = await (await ready(`http://127.0.0.1:${debugPort}/json/list`)).json();
    page = pages.find(target => target.type === "page" && target.url === `http://127.0.0.1:${port}/scripts/browser/session-view.html`);
    if (!page) await delay(100);
  }
  assert.ok(page, "Chrome must open the fixture page, not an extension background target");
  ws = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => {
    ws.addEventListener("open", resolve, { once: true });
    ws.addEventListener("error", reject, { once: true });
  });
  let seq = 0;
  const pending = new Map();
  ws.addEventListener("message", event => {
    const message = JSON.parse(event.data);
    if (message.method === 'Runtime.exceptionThrown') console.error('PAGE ERROR',JSON.stringify(message.params));
    if (message.id) { pending.get(message.id)?.(message); pending.delete(message.id); }
  });
  const call = (method, params = {}) => new Promise((resolve, reject) => {
    const id = ++seq;
    const timeout = setTimeout(() => { pending.delete(id); reject(new Error(`CDP timeout: ${method}`)); }, 10000);
    pending.set(id, result => { clearTimeout(timeout); result.error ? reject(new Error(JSON.stringify(result.error))) : resolve(result); });
    ws.send(JSON.stringify({ id, method, params }));
  });
  const evaluate = async expression => {
    const result = await call("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
    if (result.result.exceptionDetails) throw new Error(JSON.stringify(result.result.exceptionDetails));
    return result.result.result.value;
  };
  await call('Runtime.enable');
  await call('Page.enable');
  let fixtureLoaded = false;
  for (let n = 0; n < 100; n++) {
    if (await evaluate("Boolean(window.probe && document.querySelector('[data-item-id]'))")) { fixtureLoaded = true; break; }
    await delay(100);
  }
  assert.ok(fixtureLoaded, "Fixture did not mount within 10 seconds");
  await delay(200);
  const measure = () => evaluate(`(() => {
    const s = document.querySelector('[data-thread-id]');
    return { top: s.scrollTop, max: s.scrollHeight-s.clientHeight, anchors: [...probe.anchors] };
  })()`);
  const near = (actual, expected, name) => { assert.ok(Math.abs(actual - expected) <= 1, `${name}: expected ${expected}, got ${actual}`); console.log(`PASS ${name}: ${actual}`); };
  const select = async id => { await evaluate(`probe.select(${JSON.stringify(id)})`); await delay(150); };
  const scroll = async top => { await evaluate(`document.querySelector('[data-thread-id]').scrollTop=${top}`); await delay(100); };
  await scroll(1800);
  const savedA = (await measure()).anchors.find(([id]) => id === "A")[1];
  assert.notEqual(savedA.itemId, "A-39", "top-of-viewport anchor must not be the final item");
  await select("B");
  const bottomB = await measure();
  await select("A"); near((await measure()).top, 1800, "A→B→A");
  assert.equal((await measure()).anchors.find(([id]) => id === "B")[1], null, "A's DOM must not overwrite B's anchor");
  await select("B"); near((await measure()).top, bottomB.max, "B retains bottom");
  await select("A"); near((await measure()).top, 1800, "second return to A");
  await evaluate("probe.remount()"); await delay(150);
  near((await measure()).top, 1800, "remount with saved anchor");
  await select("B");
  await evaluate("probe.cold('A')"); await delay(100);
  assert.deepEqual((await measure()).anchors.find(([id]) => id === "A")[1], savedA, "empty replay must not erase A's anchor");
  for (const count of [4, 12, 40]) { await evaluate(`probe.replay(${count})`); await delay(150); }
  near((await measure()).top, 1800, "frame-by-frame cold replay");
  await scroll(7400); await select("B");
  await evaluate("probe.cold('A')"); await delay(100);
  await evaluate("probe.replay(37)"); await delay(150);
  await evaluate("probe.replay(40)"); await delay(150);
  near((await measure()).top, 7400, "cold replay waits until saved offset fits the content");
  await scroll(1800);
  await select("B"); await scroll(3100); await select("A");
  await evaluate("probe.cold('B')"); await delay(100);
  await select("A"); near((await measure()).top, 1800, "A→pending cold B→A");
  await select("B");
  await evaluate("probe.error('B','Historical error')"); await delay(100);
  await scroll(3100); await select("A"); await select("B");
  near((await measure()).top, 3100, "historical error does not override restoration");
  await select("A");
  await evaluate("const s=document.querySelector('[data-thread-id]');s.scrollTop=2200;s.dispatchEvent(new Event('scroll'));probe.selectSync('B')");
  await delay(100); await select("A");
  near((await measure()).top, 2200, "capture before synchronous switch");
  await select("B"); await scroll(3100); await select("A");
  near((await measure()).top, 2200, "A and B independently saved");
  await evaluate("document.querySelector('[data-item-id]').firstElementChild.style.paddingBottom='248px'"); await delay(250);
  near((await measure()).top, 2440, "content above anchor grows 240px");
  await evaluate("document.querySelector('[data-item-id]').firstElementChild.style.paddingBottom='488px'"); await delay(250);
  near((await measure()).top, 2680, "second async growth still keeps anchor");
  await scroll(2000);
  await evaluate("document.querySelector('[data-item-id]').firstElementChild.style.paddingBottom='728px'"); await delay(250);
  near((await measure()).top, 2000, "user scroll takes control from drift correction");
  await select("B"); near((await measure()).top, 3100, "B returns to its own reading position");
} finally {
  ws?.close();
  for (const child of children) child.kill("SIGTERM");
  await Promise.all(children.map(child => child.exitCode !== null || child.signalCode !== null
    ? Promise.resolve() : new Promise(resolve => child.once("exit", resolve))));
  await rm(profile, { recursive: true, force: true, maxRetries: 3, retryDelay: 200 });
}
