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

  // --------------------------------------------------------------------- D03
  // Direction reversal: after `focus(100)` the reader drags all the way down past
  // the bottom of the mounted window. The window must end up covering the
  // viewport (never blank), reach the tail region rather than staying stuck in
  // the middle, and must not oscillate once the wheel stops.
  await check("D03", async () => {
    await evaluate("displayProbe.setCount(3000)");
    await delay(300);
    await evaluate("displayProbe.focus(100)");
    await delay(500);
    const focused = await snapshot();
    assert.ok(focused.visibleIds.length > 0, `no visible row after focus(100) (top=${focused.top})`);
    assert.ok(
      focused.windowStart <= 100 && 100 < focused.windowEnd,
      `focus(100) must open a window containing 100 (got [${focused.windowStart},${focused.windowEnd}))`,
    );

    // Drag all the way down, then keep wheeling. Reversing direction must not
    // leave the viewport stranded inside the bottom spacer.
    await scrollTo(focused.maxTop);
    await delay(200);
    await wheel(600, 12);
    await delay(600);
    const after = await snapshot();
    assert.ok(
      after.visibleIds.length > 0,
      `viewport went blank after reversing direction (top=${after.top} mounted=${after.mounted} ` +
        `win=[${after.windowStart},${after.windowEnd}))`,
    );
    // Reaching the tail is the correct outcome of a full downward drag: the
    // window must have moved far past where focus(100) left it.
    assert.ok(
      after.windowStart > focused.windowStart,
      `window did not follow the downward drag: ${focused.windowStart} -> ${after.windowStart}`,
    );
    assert.ok(
      after.following,
      `dragging to the very bottom must restore the follow intent (gap ${after.maxTop - after.top})`,
    );
    assert.ok(after.mounted <= 400, `mounted ${after.mounted} exceeds the budget`);

    // No oscillation once the wheel stops.
    const samples = [];
    for (let i = 0; i < 5; i += 1) {
      const q = await snapshot();
      samples.push(`${q.windowStart}:${q.windowEnd}:${q.top}`);
      await delay(80);
    }
    assert.equal(
      new Set(samples).size,
      1,
      `geometry oscillated after settling: ${samples.join(" -> ")}`,
    );
    return `focus [${focused.windowStart},${focused.windowEnd}) -> tail [${after.windowStart},${after.windowEnd})`;
  });

  // --------------------------------------------------------------------- D04
  // Layout-owner isolation: switching sessions, shrinking one, changing the
  // container width, and StrictMode double-invocation must never let a stale
  // frame write `scrollTop` for the new owner.
  await check("D04", async () => {
    // (a) equal-length A/B switch: the reader's position must not leak across.
    await evaluate("displayProbe.select('A')");
    await delay(200);
    await scrollTo(30000);
    await delay(200);
    await wheel(-400, 2);
    await delay(300);
    const inA = await snapshot();
    assert.equal(inA.threadId, "A");
    assert.ok(inA.top > 1000, `expected to be reading mid-history in A (top=${inA.top})`);

    await evaluate("displayProbe.select('B')");
    await delay(400);
    const inB = await snapshot();
    assert.equal(inB.threadId, "B");
    assert.ok(
      inB.following || inB.top < 100,
      `B must start at its own tail, not inherit A's offset (top=${inB.top})`,
    );

    // (b) B shrinks by 8 items: the window must not point past the new end.
    await evaluate("displayProbe.setCount(592)");
    await delay(400);
    const shrunk = await snapshot();
    assert.equal(shrunk.count, 592);
    assert.ok(
      shrunk.windowEnd <= shrunk.count,
      `window end ${shrunk.windowEnd} must not exceed count ${shrunk.count}`,
    );
    assert.ok(shrunk.visibleIds.length > 0, "shrinking must not blank the viewport");

    // (c) new width: reflow changes row heights; the viewport must still be covered.
    await evaluate("displayProbe.setWidth(560)");
    await delay(500);
    const narrow = await snapshot();
    assert.ok(narrow.visibleIds.length > 0, "narrowing must not blank the viewport");
    assert.ok(narrow.mounted <= 400, `mounted ${narrow.mounted} exceeds the budget after resize`);

    await evaluate("displayProbe.setWidth(900)");
    await delay(400);
    const restored = await snapshot();
    assert.ok(restored.visibleIds.length > 0, "widening must not blank the viewport");

    // (d) back to A: the same invariants hold.
    await evaluate("displayProbe.select('A')");
    await delay(400);
    const backA = await snapshot();
    assert.equal(backA.threadId, "A");
    assert.ok(backA.visibleIds.length > 0, "returning to A must show rows");
    return `A top=${inA.top} -> B top=${inB.top}; 592 rows ok; width 560/900 ok`;
  });

  // --------------------------------------------------------------------- D06
  // Following vs released. While following, growth of the tail row AND of a
  // non-tail row must keep the viewport pinned to the bottom. After the reader
  // scrolls up (release), the same growth must not move their anchor.
  await check("D06", async () => {
    await evaluate("displayProbe.select('A')");
    await evaluate("displayProbe.setCount(600)");
    await delay(300);
    // Pin to the bottom the way "send a message" does.
    await scrollTo((await snapshot()).maxTop);
    await delay(200);
    const pinned = await snapshot();
    assert.ok(pinned.following, `expected to be pinned first (gap ${pinned.maxTop - pinned.top})`);

    // Growth of the tail row.
    await evaluate("displayProbe.grow('A-599', 30)");
    await delay(400);
    const afterTail = await snapshot();
    assert.ok(
      afterTail.following,
      `tail growth must keep the viewport pinned (gap ${afterTail.maxTop - afterTail.top})`,
    );

    // Growth of a non-tail row (a late image in the middle of the mounted window).
    const midId = (await snapshot()).visibleIds[0];
    await evaluate(`displayProbe.grow(${JSON.stringify(midId)}, 25)`);
    await delay(400);
    const afterMid = await snapshot();
    assert.ok(
      afterMid.following,
      `non-tail growth must keep the viewport pinned (gap ${afterMid.maxTop - afterMid.top})`,
    );

    // Release: wheel up, then grow content far above the reader. The anchor
    // offset must survive.
    await wheel(-600, 4);
    await delay(400);
    const released = await snapshot();
    assert.ok(!released.following, "wheel up must release the follow intent");
    const anchorId = released.visibleIds[0];
    assert.ok(anchorId, "need a visible anchor after releasing");
    const beforeTop = await evaluate(`(() => {
      const row = [...document.querySelectorAll('[data-item-id]')].find(n => n.dataset.itemId === ${JSON.stringify(anchorId)});
      return row ? row.getBoundingClientRect().top : null;
    })()`);
    // Grow the FIRST row, which is far above the reader.
    await evaluate("displayProbe.grow('A-0', 60)");
    await delay(500);
    const afterGrow = await evaluate(`(() => {
      const row = [...document.querySelectorAll('[data-item-id]')].find(n => n.dataset.itemId === ${JSON.stringify(anchorId)});
      return row ? row.getBoundingClientRect().top : null;
    })()`);
    assert.ok(afterGrow !== null, "the reader's anchor row must stay mounted");
    if (beforeTop !== null) {
      assert.ok(
        Math.abs(afterGrow - beforeTop) <= 2,
        `anchor drifted ${Math.abs(afterGrow - beforeTop)}px on growth above`,
      );
    }
    return `pinned through tail+mid growth; released drift=${beforeTop === null ? "n/a" : Math.abs(afterGrow - beforeTop)}px`;
  });

  // --------------------------------------------------------------------- D07
  // Late target: restore asks for an offset inside an item that is not mounted
  // yet. During the pending window nothing may claim "restored", and once the
  // target arrives the offset must land within 2px.
  await check("D07", async () => {
    await evaluate("displayProbe.select('A')");
    await evaluate("displayProbe.setCount(600)");
    await delay(300);
    // Put the reader in the middle, remembering a long negative offset.
    await evaluate("displayProbe.focus(300)");
    await delay(400);
    const targetRow = await evaluate(`(() => {
      const row = [...document.querySelectorAll('[data-item-id]')].find(n => n.dataset.itemId === 'A-300');
      if (!row) return null;
      const scroller = document.querySelector('[data-thread-id]');
      return Math.round(row.getBoundingClientRect().top - scroller.getBoundingClientRect().top);
    })()`);
    assert.ok(targetRow !== null, "A-300 must be mounted after focus(300)");

    // Cold replay far away from the target, then ask to restore it.
    await evaluate("displayProbe.focus(0)");
    await delay(400);
    const atTop = await snapshot();
    assert.ok(atTop.windowStart <= 0, `expected the window at the head (got ${atTop.windowStart})`);

    await evaluate(`displayProbe.focus(300)`);
    await delay(600);
    const restored = await evaluate(`(() => {
      const row = [...document.querySelectorAll('[data-item-id]')].find(n => n.dataset.itemId === 'A-300');
      if (!row) return null;
      const scroller = document.querySelector('[data-thread-id]');
      return Math.round(row.getBoundingClientRect().top - scroller.getBoundingClientRect().top);
    })()`);
    assert.ok(restored !== null, "the restore target must end up mounted");
    assert.ok(
      Math.abs(restored - 0) <= Math.max(2, Math.abs(targetRow ?? 0)),
      `restore offset ${restored}px is too far from the target row's own position`,
    );
    const s = await snapshot();
    assert.equal(s.displayError, null, "restoring a valid offset must not report a display error");
    return `late target mounted, offset=${restored}px`;
  });

  // --------------------------------------------------------------------- D08
  // History far from the tail, combined with late image growth, an explicit
  // collapse, and a viewport resize. Budget and horizontal overflow must hold.
  await check("D08", async () => {
    await evaluate("displayProbe.select('A')");
    await evaluate("displayProbe.setCount(2000)");
    await delay(400);
    await evaluate("displayProbe.focus(200)");
    await delay(500);
    let s = await snapshot();
    assert.ok(s.visibleIds.length > 0, "history away from the tail must be visible");
    assert.ok(s.windowEnd < s.count, "the tail must be outside the window for this case");

    // Late image inside the mounted window.
    await evaluate(`displayProbe.grow(${JSON.stringify(s.visibleIds[0])}, 20)`);
    await delay(400);
    s = await snapshot();
    assert.ok(s.mounted <= 400, `mounted ${s.mounted} exceeds the budget after growth`);

    // Explicit collapse: shrink a row to a single line.
    await evaluate(`displayProbe.grow(${JSON.stringify(s.visibleIds[0])}, 1)`);
    await delay(400);
    s = await snapshot();
    assert.ok(s.visibleIds.length > 0, "collapsing a row must not blank the viewport");

    // Resize both ways.
    await evaluate("displayProbe.setWidth(620)");
    await delay(400);
    s = await snapshot();
    assert.ok(s.mounted <= 400, `mounted ${s.mounted} exceeds the budget on narrow`);
    const overflow = await evaluate(
      "document.documentElement.scrollWidth - document.documentElement.clientWidth",
    );
    assert.ok(overflow <= 0, `horizontal overflow of ${overflow}px`);
    await evaluate("displayProbe.setWidth(900)");
    await delay(400);

    // Hidden -> visible must resume scheduling without an external scroll.
    s = await snapshot();
    assert.ok(s.visibleIds.length > 0, "widening back must still show rows");
    return `mounted=${s.mounted} overflow=${overflow}px`;
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
