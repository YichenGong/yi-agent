// D-group (display convergence) checks in a real browser with real CSS/layout.
//
// Run: cd desktop && npm run test:browser:session-completeness
//
// The defect family these cover is geometric: a window that stops paging because
// the always-mounted LiveTail polluted the "first/last mounted row" measurement,
// and a reader anchor that is moved by height changes above/below it. jsdom has
// no layout engine, so all of it is vacuous there.
//
// Environment: PORT / CDP_PORT, or SESSION_COMPLETENESS_PORT / _DEBUG_PORT.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
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
const port = Number(process.env.SESSION_COMPLETENESS_PORT || 1452);
const debugPort = Number(process.env.SESSION_COMPLETENESS_DEBUG_PORT || 9362);
const profile = await mkdtemp(resolve(tmpdir(), "yi-session-completeness-"));
const children = [];
let ws;
const results = [];

/**
 * 端口必须**空闲**才能启动。
 *
 * 真实事故：上一次运行的 Vite / Chrome 没被回收，新脚本复用了同一端口，于是
 * 连上的是旧模块（`estimateFocusIndex is not defined` 这类已删代码才有的错误），
 * 结果是「明明改好了却一直红」。宁可立刻失败，也不要拿到不可信的结论。
 */
async function assertPortFree(port, label) {
  try {
    await fetch(`http://127.0.0.1:${port}/`, { signal: AbortSignal.timeout(500) });
  } catch {
    return; // 连不上 = 空闲
  }
  throw new Error(
    `${label} port ${port} is already in use; a previous run left a process behind. ` +
      `Kill it (lsof -nP -iTCP:${port} -sTCP:LISTEN) and retry.`,
  );
}

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
  await assertPortFree(port, "Vite");
  await assertPortFree(debugPort, "CDP");
  start(process.execPath, [
    resolve(root, "node_modules/vite/bin/vite.js"),
    "--port",
    String(port),
    "--host",
    "127.0.0.1",
  ]);
  const pageUrl = `http://127.0.0.1:${port}/scripts/browser/session-completeness.html`;
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
  let pageError = null;
  ws.addEventListener("message", event => {
    const message = JSON.parse(event.data);
    if (message.method === "Runtime.exceptionThrown") {
      pageError = JSON.stringify(message.params.exceptionDetails?.text ?? message.params);
      console.error("PAGE ERROR", pageError);
    }
    if (message.method === "Runtime.exceptionThrown") {
      console.log("THROWN", message.params.exceptionDetails?.exception?.description?.slice(0, 3000));
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
    if (result.result.exceptionDetails) throw new Error(JSON.stringify(result.result.exceptionDetails));
    return result.result.result.value;
  };

  const waitUntil = async (expression, timeoutMs = 5000) => {
    const started = Date.now();
    for (;;) {
      if (await evaluate(expression)) return true;
      if (Date.now() - started > timeoutMs) return false;
      await delay(50);
    }
  };

  const snapshot = () => evaluate("displayProbe.snapshot()");
  /**
   * Geometry dump used only in failure messages. A "blank viewport" can come from
   * four very different causes (window not paging, spacer collapsed, scrollTop
   * clamped, rows mounted but zero height), and the snapshot alone cannot tell
   * them apart.
   */
  const diagnose = () =>
    evaluate(`(() => {
      const scroller = document.querySelector('[data-thread-id]');
      if (!scroller) return { error: 'no scroller' };
      const box = scroller.getBoundingClientRect();
      const rows = [...document.querySelectorAll('[data-item-id]')].map(row => {
        const r = row.getBoundingClientRect();
        return {
          id: row.dataset.itemId,
          index: Number(row.dataset.itemIndex),
          region: row.dataset.transcriptRegion,
          top: Math.round(r.top - box.top),
          bottom: Math.round(r.bottom - box.top),
          height: Math.round(r.height),
        };
      });
      const spacer = side => {
        const el = document.querySelector("[data-spacer='" + side + "']");
        return el ? {
          styled: parseFloat(el.style.height) || 0,
          rendered: Math.round(el.getBoundingClientRect().height),
        } : null;
      };
      return {
        top: scroller.scrollTop,
        scrollHeight: scroller.scrollHeight,
        clientHeight: scroller.clientHeight,
        viewportTop: Math.round(box.top),
        mounted: rows.length,
        historyRows: rows.filter(r => r.region === 'history').length,
        tailRows: rows.filter(r => r.region === 'tail').length,
        firstIndex: rows[0]?.index ?? null,
        lastHistoryIndex: [...rows].reverse().find(r => r.region === 'history')?.index ?? null,
        inViewport: rows.filter(r => r.bottom > 0 && r.top < scroller.clientHeight).length,
        spacerTop: spacer('top'),
        spacerBottom: spacer('bottom'),
        rows: rows.slice(0, 4).concat(rows.slice(-2)),
      };
    })()`);
  const scrollTo = top =>
    evaluate(`(() => { const s=document.querySelector('[data-thread-id]'); s.scrollTop=${top}; return s.scrollTop; })()`);

  /** Wheel the real scroller, so this is a user gesture, not a programmatic write. */
  const wheel = async (deltaY, times = 1) => {
    let last = null;
    for (let i = 0; i < times; i += 1) {
      last = await call("Input.dispatchMouseEvent", {
        type: "mouseWheel",
        x: 450,
        y: 400,
        deltaX: 0,
        deltaY,
      });
      await delay(60);
    }
    return last;
  };

  const record = (id, passed, detail) => {
    results.push({ id, passed, detail });
    if (!passed) console.error(`FAIL ${id}: ${detail}`);
    else console.log(`PASS ${id}`);
  };

  const check = (id, fn) =>
    (async () => {
      try {
        const detail = await fn();
        record(id, true, detail ?? "");
      } catch (error) {
        record(id, false, String(error?.message ?? error));
      }
    })();

  await call("Runtime.enable");
  let mounted = false;
  for (let n = 0; n < 100; n += 1) {
    if (await evaluate("Boolean(window.displayProbe && document.querySelector('[data-item-id]'))")) {
      mounted = true;
      break;
    }
    await delay(100);
  }
  assert.ok(mounted, "fixture did not mount within 10 seconds");
  await delay(300);
  assert.equal(pageError, null, `the fixture must not throw: ${pageError}`);

  // --------------------------------------------------------------------- D01
  // A→B→A, then one programmatic jump into the middle. Without any further user
  // input the window must cover the viewport. This is the reported symptom:
  // content stopped at a fixed place and only appeared after leaving and coming
  // back (which remounts and forces the tail).
  await check("D01", async () => {
    await evaluate("displayProbe.select('B'); displayProbe.select('A')");
    await delay(200);
    const before = await snapshot();
    assert.equal(before.count, 600);
    await scrollTo(60802);
    const started = Date.now();
    let s = await snapshot();
    while (s.visibleIds.length === 0 && Date.now() - started < 3000) {
      await delay(50);
      s = await snapshot();
    }
    assert.ok(s.visibleIds.length > 0, `viewport stayed blank after the jump (top=${s.top})`);
    assert.ok(s.mounted <= 400, `mounted ${s.mounted} must stay within the DOM budget`);
    return `mounted=${s.mounted} visible=${s.visibleIds.length}`;
  });

  // --------------------------------------------------------------------- D02
  await check("D02", async () => {
    await evaluate("displayProbe.setCount(3000)");
    await delay(200);
    await scrollTo(0);
    await wheel(-600, 3);
    await delay(400);
    const top = await snapshot();
    if (top.visibleIds.length === 0) {
      const diag = await diagnose();
      assert.fail(
        "no mounted row in the viewport at the top of history\n" +
          JSON.stringify(diag, null, 2),
      );
    }
    assert.ok(top.mounted <= 400, `mounted ${top.mounted} exceeds the budget`);
    return `mounted=${top.mounted}`;
  });

  // --------------------------------------------------------------------- D05
  // Reader turns upward while the height median is still converging, then an
  // off-screen row grows. The reader's own row must stay put.
  await check("D05", async () => {
    await evaluate("displayProbe.select('A')");
    await delay(200);
    await scrollTo(20000);
    await delay(200);
    await wheel(-400, 2);
    await delay(300);
    const before = await snapshot();
    const anchorId = before.visibleIds[0];
    assert.ok(anchorId, "need a visible anchor row before growing content");
    const anchorTop = await evaluate(`(() => {
      const row = [...document.querySelectorAll('[data-item-id]')].find(n => n.dataset.itemId === ${JSON.stringify(anchorId)});
      return row ? row.getBoundingClientRect().top : null;
    })()`);
    // Grow a row far below the reader (a late image / expanding code block).
    await evaluate("displayProbe.grow('A-599', 40)");
    await delay(400);
    const after = await snapshot();
    const newTop = await evaluate(`(() => {
      const row = [...document.querySelectorAll('[data-item-id]')].find(n => n.dataset.itemId === ${JSON.stringify(anchorId)});
      return row ? row.getBoundingClientRect().top : null;
    })()`);
    assert.ok(newTop !== null, "the reader's anchor row must stay mounted");
    assert.ok(
      Math.abs(newTop - anchorTop) <= 2,
      `reader anchor moved ${Math.abs(newTop - anchorTop)}px when content below grew`,
    );
    return `anchor=${anchorId} drift=${Math.abs(newTop - anchorTop)}px top=${after.top}`;
  });

  // --------------------------------------------------------------------- D09
  await check("D09", async () => {
    const before = await evaluate("displayProbe.snapshot()");
    await delay(600);
    const after = await evaluate("displayProbe.snapshot()");
    assert.equal(after.top, before.top, "an idle viewport must not keep scrolling itself");
    return `top stable at ${after.top}`;
  });

  const reportDir = resolve(root, "../.artifacts/session-completeness");
  await mkdir(reportDir, { recursive: true });
  await writeFile(
    resolve(reportDir, "display.json"),
    `${JSON.stringify({ commit: process.env.GIT_COMMIT ?? null, results }, null, 2)}\n`,
  );
  console.log(`\n${results.filter(r => r.passed).length}/${results.length} display checks passed`);
  console.log("D02/D03/D04/D06/D07/D08 are still to be added (A2–A4 implement their rules first).");
  if (results.some(r => !r.passed)) process.exitCode = 1;
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
