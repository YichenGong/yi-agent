// Real-browser acceptance for the subagent trace panel.
//
// Same harness as `stream-live-check.mjs` (Node's WebSocket + Chrome DevTools
// Protocol, a throwaway profile, no automation dependency), but here the subject
// is geometry and colour: panel width, viewport height, horizontal overflow,
// reading anchors and WCAG contrast. jsdom has no layout engine, so none of this
// can be proven there.
//
// Matrix: 2 themes × 3 panel widths × 3 font scales = 18 combinations, all at
// 900x600 — the smallest window the panel is designed for.
//
// Everything that depends on *user intent* (pausing, resuming, keyboard) is
// driven through CDP's real input pipeline. Assigning `scrollTop` in page script
// would not pause the panel: it only trusts wheel/pointer/key events as a gesture.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { setTimeout as delay } from "node:timers/promises";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const repoRoot = resolve(root, "..");
const chrome = process.env.CHROME_BIN || (process.platform === "darwin"
  ? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" : "/usr/bin/google-chrome");
const port = Number(process.env.SUBAGENT_TRACE_PORT || 1449);
const debugPort = Number(process.env.SUBAGENT_TRACE_DEBUG_PORT || 9359);
const reportPath = process.env.SUBAGENT_TRACE_REPORT
  || resolve(repoRoot, ".artifacts/desktop-subagent-trace/report.json");
const profile = await mkdtemp(resolve(tmpdir(), "yi-subagent-trace-"));
const children = [];
let ws;
const report = {
  startedAt: new Date().toISOString(),
  versions: { node: process.version, chrome: null, platform: process.platform },
  viewport: { width: 900, height: 600 },
  matrix: [],
  checks: [],
  failures: [],
};

async function ready(url) {
  for (let n = 0; n < 150; n += 1) {
    try {
      const response = await fetch(url);
      if (response.ok) return response;
    } catch { /* still starting */ }
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

/** One check: record it, and fail loudly instead of quietly limping on. */
function check(name, fn) {
  try {
    fn();
    report.checks.push({ name, ok: true });
    console.log(`PASS ${name}`);
  } catch (error) {
    const failure = { name, error: String(error?.message ?? error) };
    report.checks.push({ ...failure, ok: false });
    report.failures.push(failure);
    throw error;
  }
}

try {
  start(process.execPath, [resolve(root, "node_modules/vite/bin/vite.js"), "--port", String(port), "--host", "127.0.0.1"]);
  const pageUrl = `http://127.0.0.1:${port}/scripts/browser/subagent-trace.html`;
  await ready(pageUrl);
  const chromeProcess = start(chrome, ["--headless=new", "--no-first-run", "--no-default-browser-check",
    "--hide-scrollbars=false", "--force-device-scale-factor=1",
    `--remote-debugging-port=${debugPort}`, `--user-data-dir=${profile}`, pageUrl]);
  void chromeProcess;
  let page;
  for (let n = 0; n < 150 && !page; n += 1) {
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
  const pageErrors = [];
  ws.addEventListener("message", (event) => {
    const message = JSON.parse(event.data);
    if (message.method === "Runtime.exceptionThrown") {
      pageErrors.push(message.params);
      console.error("PAGE ERROR", JSON.stringify(message.params));
    }
    if (message.id) {
      pending.get(message.id)?.(message);
      pending.delete(message.id);
    }
  });
  const call = (method, params = {}) => new Promise((resolveCall, reject) => {
    const id = ++seq;
    const timeout = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`CDP timeout: ${method}`));
    }, 20000);
    pending.set(id, (result) => {
      clearTimeout(timeout);
      if (result.error) reject(new Error(JSON.stringify(result.error)));
      else resolveCall(result);
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
  const version = await call("Browser.getVersion");
  report.versions.chrome = version.result.product;
  await call("Emulation.setDeviceMetricsOverride", {
    width: 900, height: 600, deviceScaleFactor: 1, mobile: false,
  });

  let mounted = false;
  for (let n = 0; n < 150; n += 1) {
    mounted = await evaluate("Boolean(window.traceProbe && document.querySelector('[data-trace-viewport]'))");
    if (mounted) break;
    await delay(100);
  }
  check("fixture mounts the real trace panel", () => assert.equal(mounted, true));

  const metrics = () => evaluate("traceProbe.metrics()");
  const wait = (ms = 250) => delay(ms);
  const wheel = async (deltaY) => {
    const box = await evaluate("traceProbe.viewportBox()");
    assert.ok(box, "the trace viewport must be measurable to aim real wheel input");
    await call("Input.dispatchMouseEvent", {
      type: "mouseWheel", x: box.x, y: box.y, deltaX: 0, deltaY, modifiers: 0,
    });
    await wait(320);
  };

  // ---- 18-combination matrix -----------------------------------------------
  //
  // Every cell re-loads the long scenario from scratch, so a stale scroll
  // position cannot make a later cell look better than it is.
  const themes = ["dark", "light"];
  const widths = [260, 360, 720];
  const scales = ["small", "standard", "large"];
  for (const theme of themes) {
    for (const width of widths) {
      for (const scale of scales) {
        await evaluate(`traceProbe.setTheme(${JSON.stringify(theme)})`);
        await evaluate(`traceProbe.setScale(${JSON.stringify(scale)})`);
        await evaluate(`traceProbe.setPanelWidth(${width})`);
        await evaluate("traceProbe.loadScenario('long')");
        await wait(380);
        const m = await metrics();
        const cell = {
          theme, width, scale,
          panelWidth: await evaluate("traceProbe.panelWidth()"),
          panelOverflow: m.panelOverflow,
          bodyHeight: m.bodyHeight,
          bottomGap: Math.round(m.bottomGap),
          messageReachable: m.messageReachable,
          documentOverflow: m.documentOverflow,
        };
        report.matrix.push(cell);
        check(`${theme}/${width}/${scale}: no horizontal overflow`, () => {
          assert.ok(m.panelOverflow <= 1, `panel overflow ${m.panelOverflow}px`);
          assert.ok(m.documentOverflow <= 1, `document overflow ${m.documentOverflow}px`);
        });
        check(`${theme}/${width}/${scale}: timeline keeps >=120px at 900x600`, () => {
          assert.ok(m.bodyHeight >= 120, `body height ${m.bodyHeight}px`);
        });
        check(`${theme}/${width}/${scale}: message box reachable`, () => {
          assert.equal(m.messageReachable, true);
        });
      }
    }
  }

  // ---- long content: tables, code, error, children --------------------------
  await evaluate("traceProbe.setTheme('dark')");
  await evaluate("traceProbe.setScale('standard')");
  await evaluate("traceProbe.setPanelWidth(260)");
  await evaluate("traceProbe.loadScenario('long')");
  await wait(350);
  const narrow = await metrics();
  check("tables and code scroll inside themselves at 260px", () => {
    assert.ok(narrow.panelOverflow <= 1, `panel overflow ${narrow.panelOverflow}px`);
  });
  const text = await evaluate("traceProbe.domText()");
  check("long prose, tokens and paths all reach the DOM", () => {
    assert.ok(text.includes("会话恢复"), "assistant text missing");
    assert.ok(text.includes("example.com"), "long URL missing");
    assert.ok(text.includes("very_long_module_name.rs"), "no-space path missing");
    assert.ok(text.includes("结果摘要"), "tool result label missing");
    assert.ok(text.includes("展开结果摘要"), "long summary is not folded");
  });
  // Folding is only honest if the expansion says it is a server summary, not the
  // full output — that sentence must appear once the user opens it.
  await evaluate("traceProbe.expandFirstSummary()");
  await wait(250);
  const expandedSummary = await evaluate("traceProbe.domText()");
  check("an expanded summary admits it is not the full output", () => {
    assert.ok(expandedSummary.includes("这是服务端提供的摘要"), "disclosure sentence missing");
    assert.ok(expandedSummary.includes("收起结果摘要"), "expansion did not open");
  });
  await evaluate("traceProbe.collapseFirstSummary()");
  await wait(200);

  // ---- empty metadata: the real "server sent nothing" shape ----------------
  await evaluate("traceProbe.loadScenario('emptyMetadata')");
  await wait(300);
  const emptyText = await evaluate("traceProbe.domText()");
  check("missing objective/lastStep say so instead of inventing a task id", () => {
    assert.ok(emptyText.includes("服务端未提供"), "the 'not provided' fallback is missing");
  });
  check("undecodable rows keep a raw-payload entry", () => {
    assert.ok(emptyText.includes("无法解析的事件"), "raw fallback label missing");
  });
  await evaluate("traceProbe.expandFirstRaw()");
  await wait(200);
  check("raw payload expands to the original text", () => {
    return evaluate("traceProbe.domText()").then((expanded) => {
      assert.ok(expanded.includes("not json") || expanded.includes("thinking_block"), "raw payload text missing");
    });
  });

  // ---- progressive window ---------------------------------------------------
  //
  // The window is established by the *first successful snapshot*. Appends push the
  // tail and never re-slice: taking content away from a reader who already has it
  // is worse than a larger DOM. So the 1000 -> 200 claim is about opening a long
  // history, which is what `loadHistory` builds.
  await evaluate("traceProbe.loadHistory(1000)");
  await wait(600);
  const initialWindow = await evaluate("traceProbe.mountedCount()");
  report.window = { initial: initialWindow };
  check("opening a 1000-block history mounts only the newest 200", () => {
    assert.equal(initialWindow, 200, `mounted ${initialWindow}`);
  });
  const hasLoadEarlier = await evaluate("traceProbe.hasLoadEarlier()");
  check("a long timeline offers 'load earlier' rather than mounting everything", () => {
    assert.equal(hasLoadEarlier, true);
  });
  const beforeLoad = await evaluate("traceProbe.mountedCount()");
  await evaluate("traceProbe.clickLoadEarlier()");
  await wait(450);
  const afterLoad = await evaluate("traceProbe.mountedCount()");
  report.window.afterLoadEarlier = afterLoad;
  check("load earlier mounts one more 200-block batch", () => {
    assert.equal(afterLoad, beforeLoad + 200, `mounted ${beforeLoad} -> ${afterLoad}`);
  });
  // Appending after the window exists must push the tail, never shrink the window.
  await evaluate("traceProbe.appendEvents(40)");
  await wait(450);
  const afterAppend = await evaluate("traceProbe.mountedCount()");
  report.window.afterAppend = afterAppend;
  const stableAfterAppend = await evaluate("traceProbe.mountedCount()");
  check("appending pushes the tail without shrinking the visible window", () => {
    assert.ok(afterAppend > afterLoad, `mounted ${afterLoad} -> ${afterAppend}`);
    assert.equal(stableAfterAppend, afterAppend);
  });
  // Keep loading earlier until the whole history is reachable, then confirm no
  // block was lost in the process.
  let clicks = 0;
  while ((await evaluate("traceProbe.hasLoadEarlier()")) && clicks < 10) {
    await evaluate("traceProbe.clickLoadEarlier()");
    await wait(280);
    clicks += 1;
  }
  const fullWindow = await evaluate("traceProbe.mountedCount()");
  report.window.afterAllEarlier = fullWindow;
  const stillHasEarlier = await evaluate("traceProbe.hasLoadEarlier()");
  check("every block is reachable by loading earlier in batches", () => {
    assert.ok(fullWindow > afterAppend, `mounted ${afterAppend} -> ${fullWindow}`);
    assert.equal(stillHasEarlier, false, "'load earlier' still offered at the start");
  });

  // ---- reading anchor round trip -------------------------------------------
  await evaluate("traceProbe.setPanelWidth(360)");
  await evaluate("traceProbe.loadScenario('long')");
  await wait(350);
  await wheel(-900);
  const paused = await metrics();
  check("a real wheel gesture pauses following", () => {
    assert.equal(paused.following, false, "wheel did not pause following");
    assert.notEqual(paused.anchorKey, null);
  });
  await evaluate("traceProbe.appendText('追加的同块文本，用来验证暂停期间视口不被带走。')");
  await wait(350);
  const grown = await metrics();
  check("same-block growth does not move a paused anchor", () => {
    assert.ok(Math.abs(grown.anchorOffset - paused.anchorOffset) <= 2,
      `anchor moved ${grown.anchorOffset - paused.anchorOffset}px`);
    assert.equal(grown.anchorKey, paused.anchorKey);
  });
  await evaluate("traceProbe.expandFirstSummary()");
  await wait(350);
  const expanded = await metrics();
  check("expanding a summary keeps the reading anchor", () => {
    assert.ok(Math.abs(expanded.anchorOffset - grown.anchorOffset) <= 2,
      `anchor moved ${expanded.anchorOffset - grown.anchorOffset}px`);
  });
  await evaluate("traceProbe.collapseFirstSummary()");
  await wait(350);
  const collapsed = await metrics();
  check("collapsing again keeps the reading anchor", () => {
    assert.ok(Math.abs(collapsed.anchorOffset - grown.anchorOffset) <= 2,
      `anchor moved ${collapsed.anchorOffset - grown.anchorOffset}px`);
  });
  // A real wheel back to the bottom restores following.
  await wheel(-20000);
  await wheel(20000);
  const resumed = await metrics();
  check("scrolling back to the end resumes following", () => {
    assert.equal(resumed.following, true, "following was not resumed");
    assert.ok(resumed.bottomGap <= 4, `bottom gap ${resumed.bottomGap}px`);
  });

  // ---- images growing after load -------------------------------------------
  await evaluate("traceProbe.loadScenario('images')");
  await wait(300);
  const imagesBefore = await metrics();
  const grew = await evaluate("traceProbe.growFirstImage(420)");
  await wait(400);
  const imagesAfter = await metrics();
  check("an image that grows after load keeps the reading position", () => {
    assert.equal(grew, true, "scenario has no image");
    if (imagesBefore.following) {
      assert.ok(imagesAfter.bottomGap <= 4, `following lost the bottom: gap ${imagesAfter.bottomGap}px`);
    } else {
      assert.ok(Math.abs(imagesAfter.anchorOffset - imagesBefore.anchorOffset) <= 2,
        `anchor moved ${imagesAfter.anchorOffset - imagesBefore.anchorOffset}px`);
    }
  });

  // ---- states --------------------------------------------------------------
  await evaluate("traceProbe.beginLoading()");
  await wait(250);
  const loadingText = await evaluate("traceProbe.domText()");
  check("loading names the operation instead of showing an empty list", () => {
    assert.ok(loadingText.includes("正在加载轨迹"), "loading copy missing");
  });
  await evaluate("traceProbe.loadScenario('long')");
  await wait(250);
  await evaluate("traceProbe.setPhase('error', '读取失败：连接超时')");
  await wait(200);
  const errorText = await evaluate("traceProbe.domText()");
  check("an error keeps the history it already has", () => {
    assert.ok(errorText.includes("读取失败"), "error copy missing");
    assert.ok(errorText.includes("会话恢复"), "history was wiped by the error");
    assert.ok(errorText.includes("重新订阅"), "retry entry missing");
  });
  await evaluate("traceProbe.setDisconnected(true)");
  await evaluate("traceProbe.setPhase('ready')");
  await wait(200);
  const disconnected = await evaluate("traceProbe.domText()");
  check("a dropped connection says so and offers a retry", () => {
    assert.ok(disconnected.includes("连接已断开"), "disconnected copy missing");
  });

  // ---- confirmation flow ---------------------------------------------------
  await evaluate("traceProbe.setDisconnected(false)");
  await evaluate("traceProbe.loadScenario('long')");
  await wait(250);
  // The real path: the user reaches the confirmation by pressing "停止任务", so
  // focus is already inside the composer when the confirm region replaces the row.
  await evaluate("traceProbe.focusStop()");
  await wait(120);
  await evaluate(`traceProbe.setActions({ confirmation: { token: "t", taskIds: ["task-a"], deadline: Date.now() + 60000 } })`);
  await wait(250);
  const confirmText = await evaluate("traceProbe.domText()");
  const textareaStillThere = await evaluate("Boolean(document.querySelector('#subagent-message'))");
  check("the confirmation names the real scope and keeps the textarea", () => {
    assert.ok(confirmText.includes("停止该任务？"), "scope copy missing");
    assert.equal(textareaStillThere, true, "textarea was unmounted");
  });
  // Keyboard: Tab must reach the harmless choice first, and Enter must activate it.
  const focusAfterOpen = await evaluate("traceProbe.activeElement()");
  check("the confirmation opens with focus on the harmless choice", () => {
    assert.equal(focusAfterOpen?.label, "放弃", `focus landed on ${JSON.stringify(focusAfterOpen)}`);
  });
  await call("Input.dispatchKeyEvent", { type: "keyDown", key: "Tab", code: "Tab", windowsVirtualKeyCode: 9 });
  await call("Input.dispatchKeyEvent", { type: "keyUp", key: "Tab", code: "Tab", windowsVirtualKeyCode: 9 });
  await wait(120);
  const afterTab = await evaluate("traceProbe.activeElement()");
  check("Tab moves to a named control without leaving the panel", () => {
    assert.ok(afterTab !== null, "focus left the document");
  });
  await evaluate("traceProbe.clickText('放弃')");
  await wait(200);
  const dismissed = await evaluate("traceProbe.domText()");
  check("dismissing closes the confirmation and restores the action row", () => {
    assert.ok(!dismissed.includes("停止该任务？"), "confirmation stayed open");
    assert.ok(dismissed.includes("停止任务"), "stop control did not come back");
  });

  // ---- reduced motion ------------------------------------------------------
  await call("Emulation.setEmulatedMedia", { features: [{ name: "prefers-reduced-motion", value: "reduce" }] });
  await evaluate("traceProbe.loadScenario('long')");
  await wait(250);
  const reduced = await evaluate("traceProbe.reducedMotion()");
  const reducedMetrics = await metrics();
  check("reduced motion is honoured without breaking the panel", () => {
    assert.equal(reduced, true, "reduced motion was not applied");
    assert.ok(reducedMetrics.bodyHeight >= 120);
  });
  await call("Emulation.setEmulatedMedia", { features: [] });

  // ---- accessibility contract ---------------------------------------------
  const a11y = await evaluate("traceProbe.accessibleSnapshot()");
  check("the timeline is a named list, not a live region", () => {
    assert.equal(a11y.headingTag, "H2", "task heading is not a heading");
    assert.equal(a11y.logName, "子 agent 轨迹", "scrolling region is not named");
    assert.ok(a11y.listItems > 0, "timeline is not an ol/li list");
    assert.equal(a11y.logIsLive, false, "streaming text must not be announced");
  });
  check("the message box has an independent label", () => {
    assert.equal(a11y.textareaLabel, "向这个 agent 发送消息");
  });
  check("status and alerts use one polite region each", () => {
    assert.equal(a11y.statusRegions, 1, `status regions: ${a11y.statusRegions}`);
    assert.ok(a11y.alertRegions <= 1);
  });
  const disclosures = await evaluate("traceProbe.disclosureContract()");
  check("every expandable control exposes aria-expanded and an existing target", () => {
    assert.ok(disclosures.length > 0, "no disclosure controls found");
    for (const item of disclosures) {
      assert.ok(item.expanded === "true" || item.expanded === "false", `${item.text}: bad aria-expanded`);
      assert.ok(item.targetExists, `${item.text}: aria-controls target missing`);
    }
  });

  // ---- contrast ------------------------------------------------------------
  //
  // Only the trace panel is measured: the tokens are shared, but the panel is
  // where the trace-specific emphasis colours are used.
  await evaluate("traceProbe.setTheme('dark')");
  await evaluate("traceProbe.loadScenario('long')");
  await wait(300);
  const darkContrast = await evaluate("traceProbe.contrastReport()");
  await evaluate("traceProbe.setTheme('light')");
  await wait(250);
  const lightContrast = await evaluate("traceProbe.contrastReport()");
  for (const [theme, samples] of [["dark", darkContrast], ["light", lightContrast]]) {
    // Small text needs 4.5:1; large text (>=18.66px bold or >=24px) only 3:1.
    // The panel has no large text, but the threshold is computed rather than assumed.
    const low = samples.filter((sample) => {
      const px = parseFloat(sample.size);
      return sample.ratio < (px >= 24 ? 3 : 4.5);
    });
    report.contrast = { ...(report.contrast ?? {}), [theme]: { samples: samples.length, low } };
    check(`${theme} theme text meets its contrast floor`, () => {
      assert.equal(low.length, 0,
        `below floor: ${low.map((s) => `${s.label}=${s.ratio} (${s.size})`).join(", ")}`);
    });
  }

  check("no page exception was raised", () => assert.equal(pageErrors.length, 0));
} catch (error) {
  console.error("FAILED:", error.message);
  process.exitCode = 1;
} finally {
  ws?.close();
  for (const child of children) child.kill("SIGTERM");
  await Promise.all(children.map((child) => (child.exitCode !== null || child.signalCode !== null
    ? Promise.resolve()
    : new Promise((resolveExit) => {
      const timer = setTimeout(() => child.kill("SIGKILL"), 5000);
      child.once("exit", () => { clearTimeout(timer); resolveExit(); });
    }))));
  await rm(profile, { recursive: true, force: true, maxRetries: 3, retryDelay: 200 });
  report.finishedAt = new Date().toISOString();
  report.passed = report.failures.length === 0;
  await mkdir(dirname(reportPath), { recursive: true });
  await writeFile(reportPath, `${JSON.stringify(report, null, 2)}\n`);
  console.log(`report: ${reportPath}`);
  console.log(report.passed
    ? `PASS subagent trace browser checks (${report.checks.length} checks, ${report.matrix.length} matrix cells)`
    : `FAIL ${report.failures.length} check(s)`);
}
