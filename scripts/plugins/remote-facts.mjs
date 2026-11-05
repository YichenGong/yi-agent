// 远端事实采集（计划 Task 1）。
//
// 这一层只回答一个问题：**远端到底是什么**。它不做判定、不碰 ssh 本身
// （命令执行器由调用方注入），因为判定逻辑必须能在没有远端机器时被测试。
//
// 设计上刻意「不猜」：
//
//   - 未知架构返回 null，不默认成 aarch64。默认的后果是「架构匹配」这条判据
//     在一台 i686 机器上通过，而二进制装错的问题发生在远端、用户看不到。
//   - 任何一条探测失败就抛错，不返回半份事实。缺字段的证据比没有证据更糟：
//     读的人无法判断缺的是哪一环。

/// 远程 SSH 能装的 target。与 `desktop/src-tauri/src/remote/rpc.rs` 的
/// `SUPPORTED_ARCHES` 保持一致——两处不一致时，桌面会拒绝一个这里说能装的架构。
export const SUPPORTED_ARCHES = Object.freeze([
  "aarch64-apple-darwin",
  "x86_64-apple-darwin",
  "x86_64-unknown-linux-gnu",
]);

/// 这个主机名是否指向本机。
///
/// 整个 `127.0.0.0/8` 都是 loopback，不只是 `127.0.0.1`：只认最后一个等于给
/// 「127.0.0.2 冒充远端」留了门。`0.0.0.0` 同样不算远端（它没有指代任何一台）。
export function isLoopbackHost(hostId) {
  if (typeof hostId !== "string") return false;
  const host = hostId.trim().toLowerCase().replace(/^\[|\]$/g, "");
  if (host === "") return false;
  if (host === "localhost" || host === "localhost.localdomain") return true;
  if (host === "::1" || host === "0.0.0.0") return true;
  if (/^127\.\d{1,3}\.\d{1,3}\.\d{1,3}$/.test(host)) return true;
  return false;
}

/// 从 `--version` 的输出里取版本号。
///
/// 输出形如 `yi-agent 0.1.13-dev+32365036`。取不到就返回 null——
/// 猜一个版本会让「两端版本相等」这条判据失去意义。
export function parseVersion(text) {
  if (typeof text !== "string") return null;
  const match = /^\s*yi-agent\s+(\S+)\s*$/.exec(text.split("\n")[0] ?? "");
  return match ? match[1] : null;
}

/// 把 `uname -m` + 远端 OS 归一成 target triple。
///
/// 返回 null 表示这台机器没有预编译产物。不返回「最接近的那个」：装错架构的
/// 失败发生在远端，而执行器只会在本地报告里写一个错的「匹配」。
export function normalizeArch(machine, osName) {
  const m = String(machine ?? "").trim();
  const os = String(osName ?? "").trim().toLowerCase();
  if (os === "darwin") {
    if (m === "arm64") return "aarch64-apple-darwin";
    if (m === "x86_64") return "x86_64-apple-darwin";
    return null;
  }
  if (os === "linux") {
    if (m === "x86_64") return "x86_64-unknown-linux-gnu";
    return null;
  }
  return null;
}

/// 采集四项事实：hostId、os、arch、version。
///
/// `run(args)` 返回该命令的 stdout；失败时抛错。每项单独探测，因为合成一条
/// 命令会让某一项失败时无法定位。
export function collectRemoteFacts({ run }) {
  if (typeof run !== "function") {
    throw new Error("collectRemoteFacts needs a `run` function");
  }
  const hostId = run(["hostname"]).trim();
  if (hostId === "") {
    throw new Error("remote hostname probe returned nothing");
  }

  const osName = run(["uname", "-s"]).trim();
  const osVersion = run(["sw_vers", "-productVersion"]).trim();
  const os = osName.toLowerCase() === "darwin" ? `macOS ${osVersion}` : `${osName} ${osVersion}`;

  const machine = run(["uname", "-m"]).trim();
  const arch = normalizeArch(machine, osName);

  const version = parseVersion(run(["--version"]));
  if (version === null) {
    throw new Error("remote `--version` output was not recognised");
  }

  return { hostId, os, arch, version };
}
