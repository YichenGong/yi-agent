// Live-streaming probe: does a streamed reply become visible while it arrives?
// Same CDP harness as `session-view-check.mjs` (real Chrome, real layout, real
// scroll events), but the state is driven exactly like a live turn.
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
const port = Number(process.env.STREAM_LIVE_PORT || 1439);
const debugPort = Number(process.env.STREAM_LIVE_DEBUG_PORT || 9349);
const profile = await mkdtemp(resolve(tmpdir(), "yi-stream-live-"));
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
  child.on("error", (error) => console.error(error));
  child.stderr.on("data", () => {});
  return child;
}

try {
  start(process.execPath, [resolve(root, "node_modules/vite/bin/vite.js"), "--port", String(port), "--host", "127.0.0.1"]);
  const pageUrl = `http://127.0.0.1:${port}/scripts/browser/stream-live.html`;
  await ready(pageUrl);
  start(chrome, ["--headless=new", "--no-first-run", "--no-default-browser-check",
    `--remote-debugging-port=${debugPort}`, `--user-data-dir=${profile}`, pageUrl]);
  let page;
  for (let n = 0; n < 100 && !page; n++) {
    const pages = await (await ready(`http://127.0.0.1:${debugPort}/json/list`)).json();
    page = pages.find((target) => target.type === "page" && target.url === pageUrl);
    if (!page) await delay(100);
  }
  assert.ok(page, "Chrome must open the fixture page");
  ws = new WebSocket(page.webSocketDebuggerUrl);
  await new Promise((resolveOpen, reject) => {
    ws.addEventListener("open", resolveOpen, { once: true });
    ws.addEventListener("error", reject, { once: true });
  });
  let seq = 0;
  const pending = new Map();
  ws.addEventListener("message", (event) => {
    const message = JSON.parse(event.data);
    if (message.method === "Runtime.exceptionThrown") console.error("PAGE ERROR", JSON.stringify(message.params));
    if (message.id) { pending.get(message.id)?.(message); pending.delete(message.id); }
  });
  const call = (method, params = {}) => new Promise((resolveCall, reject) => {
    const id = ++seq;
    const timeout = setTimeout(() => { pending.delete(id); reject(new Error(`CDP timeout: ${method}`)); }, 10000);
    pending.set(id, (result) => {
      clearTimeout(timeout);
      result.error ? reject(new Error(JSON.stringify(result.error))) : resolveCall(result);
    });
    ws.send(JSON.stringify({ id, method, params }));
  });
  const evaluate = async (expression) => {
    const result = await call("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
    if (result.result.exceptionDetails) throw new Error(JSON.stringify(result.result.exceptionDetails));
    return result.result.result.value;
  };
  await call("Runtime.enable");
  await call("Page.enable");
  for (let n = 0; n < 100; n++) {
    if (await evaluate("Boolean(window.probe)")) break;
    await delay(100);
  }

  // Warm-up turn so the transcript is scrollable (starts pinned to the bottom,
  // exactly like opening a session with history).
  await evaluate("probe.startTurn('warmup')");
  for (let i = 0; i < 80; i += 1) {
    await evaluate(`probe.delta('a-warmup', ${JSON.stringify(`warm line ${i}\n`)})`);
    await delay(5);
  }
  await delay(200);
  console.log("after warm-up:", JSON.stringify(await evaluate(`({
    top: probe.scrollTop(), max: probe.maxScroll(), items: probe.count(), text: probe.textOf('a-question-1').length
  })`)));

  // The real thing: a fresh turn streamed in small deltas, 30ms apart, the way
  // `item/delta` arrives from the server.
  await evaluate("probe.startTurn('question-2')");
  const FLUSH = 40;
  let streamed = "";
  for (let i = 0; i < FLUSH; i += 1) {
    const chunk = `streamed line ${i}\n`;
    streamed += chunk;
    await evaluate(`probe.delta('a-question-2', ${JSON.stringify(chunk)})`);
    await delay(30);
  }
  // Let throttled markdown / follow effects settle.
  await delay(400);

  const state = await evaluate(`({
    top: probe.scrollTop(),
    max: probe.maxScroll(),
    textLen: probe.textOf('a-question-2').length,
    expectedLen: ${JSON.stringify(streamed.length)},
    tailVisible: probe.visible('streamed line 39'),
    firstVisible: probe.visible('streamed line 0'),
    atBottom: probe.maxScroll() - probe.scrollTop() <= 4
  })`);
  console.log("tail-item streaming:", JSON.stringify(state));
  assert.equal(state.textLen, state.expectedLen, "the model text must have landed in the session");
  assert.ok(state.atBottom, `viewport must stay pinned to the bottom (gap ${state.max - state.top}px)`);
  assert.ok(state.tailVisible, "the newest streamed line must be inside the viewport");
  console.log("PASS the trailing reply stays visible while it streams");

  // Case 2: the streaming message is NOT the transcript tail. The server can push
  // a CoT block or a tool card after it while the answer keeps growing.
  await evaluate("probe.startTurn('question-3')");
  let late = "alpha ";
  await evaluate("probe.delta('a-question-3', 'alpha ')");
  await evaluate("probe.reasoning('thinking...')");
  late += "beta ";
  await evaluate("probe.delta('a-question-3', 'beta ')");
  await evaluate("probe.toolStart('tool-1', 'bash')");
  for (let i = 0; i < 30; i += 1) {
    const chunk = `late line ${i}\n`;
    late += chunk;
    await evaluate(`probe.delta('a-question-3', ${JSON.stringify(chunk)})`);
    await delay(30);
  }
  await delay(400);
  const case2 = await evaluate(`({
    domHasTail: probe.domText().includes('late line 29'),
    countInDom: (probe.domText().match(/late line/g) || []).length,
    textLen: probe.textOf('a-question-3').length,
    expectedLen: ${JSON.stringify(late.length)},
    stillTail: probe.isTail('a-question-3'),
    atBottom: probe.maxScroll() - probe.scrollTop() <= 4,
    tailVisible: probe.visible('late line 29')
  })`);
  console.log("displaced-reply streaming:", JSON.stringify(case2));
  assert.equal(case2.textLen, case2.expectedLen, "the displaced reply must keep its full text");
  assert.equal(case2.stillTail, false, "the fixture must really displace the reply from the tail");
  assert.ok(case2.atBottom, "viewport must follow a message that is no longer the tail");
  assert.ok(case2.domHasTail, "a non-tail streamed message must still reach the DOM");
  assert.ok(case2.tailVisible, "the newest streamed line must be inside the viewport");
  console.log("PASS the reply keeps following after a CoT block displaced it");
} finally {
  ws?.close();
  for (const child of children) child.kill("SIGTERM");
  await Promise.all(children.map((child) => child.exitCode !== null || child.signalCode !== null
    ? Promise.resolve() : new Promise((resolveExit) => child.once("exit", resolveExit))));
  await rm(profile, { recursive: true, force: true, maxRetries: 3, retryDelay: 200 });
}
