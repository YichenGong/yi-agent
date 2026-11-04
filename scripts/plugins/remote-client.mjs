// ssh stdio JSON-RPC 客户端（计划 Task 2）。
//
// 与产品同形：把 `ssh` 当字节管道，远端跑 `yi-agent app-server --listen stdio://`。
// 不在验收脚本里自实现 SSH——那会引入一条产品根本不用的通路，测出来的东西
// 证明不了产品行为。
//
// 这里的重点是**失败时不许装作没事**：
//
//   - 握手超时抛错，不返回 null 让调用方「自己看着办」；
//   - 子进程提前退出立刻失败，不挂到超时才报；
//   - 响应按 id 配对，不把「第一行到达的」当成答案（握手期间通知与旧响应都可能先到）。

/// 远端路径进的是远端 shell 的命令行，所以要做单引号引用。
///
/// 不引用时 `/Users/a b/yi-agent` 会在远端被拆成两个词，报出来的错误与真实
/// 原因无关，排查会绕很久。
export function shellQuote(value) {
  const text = String(value);
  if (text === "") return "''";
  return `'${text.replaceAll("'", `'\\''`)}'`;
}

/// 环境变量赋值右侧的引用：只在含特殊字符时加单引号。
///
/// 与 [`shellQuote`] 分开是因为 `NAME='v'` 与 `NAME=v` 对调用方看到的字符串
/// 不同，而测试与运维都会去读实际下发的命令行——同一条命令在两种写法下都
/// 应该一眼能看懂。
function simpleQuote(value) {
  const text = String(value);
  if (/^[A-Za-z0-9_./:@%+-]+$/.test(text)) return text;
  return shellQuote(text);
}

/// 拼 ssh 的 argv。
///
/// `YI_AGENT_HOME` 走远端命令行赋值而不是本地环境：本地设它只会影响本机的
/// ssh 客户端，远端 app-server 读的是远端的那个值。
export function buildSshArgs({ host, remotePath, remoteHome }) {
  if (!host) throw new Error("buildSshArgs needs a host");
  if (!remotePath) throw new Error("buildSshArgs needs a remote path");
  const parts = [];
  // 赋值号左边不能带引号（那是 shell 语法错误），右边带引号才安全。
  if (remoteHome) parts.push(`YI_AGENT_HOME=${simpleQuote(remoteHome)}`);
  parts.push(shellQuote(remotePath));
  parts.push("app-server --listen stdio://");
  return [
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=10",
    host,
    parts.join(" "),
  ];
}

const DEFAULT_TIMEOUT_MS = 20_000;

/// 一条活着的 ssh 通道。
export class RemoteSession {
  /// 起进程并返回会话。**不在这里握手**：分开是为了让调用方能决定何时握手、
  /// 以及断开后是否要再探一次远端残留。
  static async connect({ host, remotePath, remoteHome, spawn, timeoutMs = DEFAULT_TIMEOUT_MS }) {
    if (typeof spawn !== "function") throw new Error("RemoteSession.connect needs a `spawn`");
    const args = buildSshArgs({ host, remotePath, remoteHome });
    const child = spawn("ssh", args, { stdio: ["pipe", "pipe", "pipe"] });
    const session = new RemoteSession(child, timeoutMs);
    session.#attach();
    return session;
  }

  #child;
  #timeoutMs;
  #buffer = "";
  #pending = new Map();
  #nextId = 1;
  #exited = false;
  #exitError = null;
  #closeListeners = [];

  constructor(child, timeoutMs) {
    this.#child = child;
    this.#timeoutMs = timeoutMs;
  }

  #attach() {
    this.#child.stdout.setEncoding?.("utf8");
    this.#child.stdout.on("data", (chunk) => this.#onData(chunk));
    this.#child.on("exit", (code, signal) => {
      this.#exited = true;
      // 提前退出时立刻让所有等待者失败。等到超时才报会把「连不上」和
      // 「跑起来了但不说话」混成同一个现象。
      this.#exitError = new Error(`remote app-server exited (code=${code}, signal=${signal})`);
      for (const [, entry] of this.#pending) {
        entry.reject(this.#exitError);
      }
      this.#pending.clear();
      for (const listener of this.#closeListeners) listener();
      this.#closeListeners = [];
    });
    this.#child.on("error", (error) => {
      this.#exited = true;
      this.#exitError = error;
      for (const [, entry] of this.#pending) entry.reject(error);
      this.#pending.clear();
    });
  }

  #onData(chunk) {
    this.#buffer += typeof chunk === "string" ? chunk : chunk.toString("utf8");
    let index;
    while ((index = this.#buffer.indexOf("\n")) >= 0) {
      const line = this.#buffer.slice(0, index).trim();
      this.#buffer = this.#buffer.slice(index + 1);
      if (line === "") continue;
      let message;
      try {
        message = JSON.parse(line);
      } catch {
        // 非 JSON 行（远端 shell 的报错等）直接忽略：它们不是协议的一部分，
        // 让它们把配对逻辑搞乱反而会掩盖真正的问题。
        continue;
      }
      if (message.id === undefined || message.id === null) continue;
      const entry = this.#pending.get(message.id);
      // 按 id 配对：握手期间通知与旧响应都可能先到，拿「第一行」当答案会读到
      // undefined 的 sideHostInstanceId。
      if (!entry) continue;
      this.#pending.delete(message.id);
      clearTimeout(entry.timer);
      if (message.error) {
        entry.reject(new Error(`rpc error: ${JSON.stringify(message.error)}`));
      } else {
        entry.resolve(message.result);
      }
    }
  }

  #request(method, params) {
    if (this.#exited) {
      return Promise.reject(this.#exitError ?? new Error("remote session is closed"));
    }
    const id = this.#nextId++;
    const payload = { jsonrpc: "2.0", id, method, params };
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.#pending.delete(id);
        reject(new Error(`${method} timed out after ${this.#timeoutMs}ms`));
      }, this.#timeoutMs);
      this.#pending.set(id, { resolve, reject, timer });
      try {
        this.#child.stdin.write(JSON.stringify(payload) + "\n");
      } catch (error) {
        clearTimeout(timer);
        this.#pending.delete(id);
        reject(error);
      }
    });
  }

  /// 握手并返回结果。缺 `sideHostInstanceId` 直接报错：让它一路变成证据里的
  /// 空 hostId 比在这里失败更难查。
  async initialize() {
    const result = await this.#request("initialize", {});
    const instanceId = result?.sideHostInstanceId;
    if (typeof instanceId !== "string" || instanceId.trim() === "") {
      throw new Error("initialize result has no sideHostInstanceId");
    }
    return result;
  }

  /// 终止与会话。幂等：断开是在收尾路径上调用的，抛错会让真正的失败被掩盖。
  close() {
    if (this.#exited) return;
    this.#exited = true;
    for (const [, entry] of this.#pending) {
      clearTimeout(entry.timer);
      entry.reject(new Error("remote session closed"));
    }
    this.#pending.clear();
    try {
      this.#child.stdin.end();
    } catch {
      // 已经关掉的管道不是错误。
    }
    try {
      this.#child.kill();
    } catch {
      // 同上。
    }
  }
}
