#!/usr/bin/env node
// 远端验收执行器 A16/A17（计划 Task 3）。
//
// 设计见 docs/superpowers/specs/2026-11-05-remote-acceptance-executor-design.md。
//
// 这个脚本回答的是「在真实远端上，哪些判据真的成立」。它不是用来把 A16/A17
// 刷成 pass 的，所以判定层刻意做了几件不讨喜的事：
//
//   - 版本必须**相等**，不是兼容；
//   - hostId 是 loopback 就失败（声明两台机器却给一台，是伪造）；
//   - A17 只证明了一部分，因此整体判 unsupported，理由点名还没实现的东西；
//     证据仍然完整写出，不因为判词不是 pass 就丢掉。
//
// 判定全部是纯函数，执行（ssh）由调用方注入，这样没有远端机器的门禁也能测。

import { execFileSync, spawn as nodeSpawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";

import { collectRemoteFacts, isLoopbackHost, SUPPORTED_ARCHES } from "./remote-facts.mjs";
import { RemoteSession, shellQuote } from "./remote-client.mjs";

/// 两端都认识且本执行器验证过的协议版本。
export const KNOWN_PROTOCOL_VERSIONS = Object.freeze([1]);

/// A17 里本执行器**无法**证明的部分。
///
/// 写成一个常量而不是散在文案里：报告与测试都引用同一份文本，改的时候不会
/// 只改到一处。内容对应 `plugin_host/remote_assets.rs` 的实际情况——
/// `FetchRequest` 只有定义、零使用点，app-server 也没有对外提供取包字节的 RPC。
export const UNPROVEN_A17 =
  "offline cache and cross-host asset fetch are not implemented " +
  "(plugin_host/remote_assets.rs FetchRequest has no caller), " +
  "so only reconnect-not-reusing-old-binding is proven here";

/// 解析命令行。远端路径必须是绝对路径：相对路径会在**远端**被解析，
/// 落到的目录与本机看到的完全不是一回事。
export function parseArgs(argv, { env = process.env } = {}) {
  const args = {
    case: null,
    host: null,
    remotePath: null,
    remoteHome: null,
    localVersion: null,
    json: false,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
    const value = () => {
      const next = argv[index + 1];
      if (next === undefined || next.startsWith("--")) {
        throw new Error(`${flag} needs a value`);
      }
      index += 1;
      return next;
    };
    if (flag === "--case") args.case = value();
    else if (flag === "--host") args.host = value();
    else if (flag === "--remote-path") args.remotePath = value();
    else if (flag === "--remote-home") args.remoteHome = value();
    else if (flag === "--local-version") args.localVersion = value();
    else if (flag === "--json") args.json = true;
    else throw new Error(`unknown argument: ${flag}`);
  }

  // 环境变量兜底：`e2e.mjs` 的 parseCommand 不过 shell，命令里写 `$VAR`
  // 只会得到字面量。显式参数优先，其次环境，最后才是「没有」。
  const fromEnv = (name) => {
    const value = env?.[name];
    return typeof value === "string" && value.trim() !== "" ? value.trim() : null;
  };
  args.host ??= fromEnv("YI_AGENT_ACCEPTANCE_REMOTE_HOST");
  args.remotePath ??= fromEnv("YI_AGENT_ACCEPTANCE_REMOTE_PATH");
  args.remoteHome ??= fromEnv("YI_AGENT_ACCEPTANCE_REMOTE_HOME");
  args.localVersion ??= fromEnv("YI_AGENT_ACCEPTANCE_LOCAL_VERSION");

  if (!["A16", "A17"].includes(args.case)) {
    throw new Error(`--case must be A16 or A17, got ${JSON.stringify(args.case)}`);
  }
  if (!args.host) throw new Error("--host is required (no remote host, nothing to prove)");
  if (!args.remotePath) throw new Error("--remote-path is required");
  // 路径校验先行：它是「参数本身错了」，比「少了某个用例特有的参数」更根本。
  // 反过来会让一个明显写错的相对路径报出与本机版本有关的错误。
  if (!path.posix.isAbsolute(args.remotePath)) {
    throw new Error(
      `--remote-path must be absolute, got ${JSON.stringify(args.remotePath)}: ` +
        "a relative path resolves on the remote, not here",
    );
  }
  if (args.case === "A16" && !args.localVersion) {
    // A16 的判据是「两端版本相等」。缺本机版本就无法比较，不能默认成相等。
    throw new Error("A16 needs --local-version (the version to compare against)");
  }
  return args;
}

/// 复刻 `HostBindings::is_current` 的语义：**完整身份**比较。
///
/// 实测过：远端重连后 generation 仍是 1（`bind_plugin_host_to` 只在同实例同
/// 类别时递增，而实例 id 每次都变，所以每次都从 1 开始）。只比代次会把重连
/// 当成同一台机器——那正是 A17 要证明不会发生的事。
export function isBindingCurrent(binding, host) {
  if (!binding || !host) return false;
  return (
    binding.kind === host.kind &&
    binding.instanceId === host.instanceId &&
    binding.generation === host.generation
  );
}

/// A16 的判定。
///
/// 六条判据全部成立才 pass；任何一条不成立即 fail，并在 `checks` 里指出是哪条。
/// 只有「没有远端主机」是 unsupported，其余都是产品/配置缺陷。
export function judgeA16({ facts, local, handshake, localProcesses, remotePath }) {
  const checks = [];
  const add = (name, ok, detail) => checks.push({ name, ok: Boolean(ok), detail });

  add(
    "version-exact",
    facts.version === local.version,
    `local ${local.version} vs remote ${facts.version}`,
  );
  add(
    "protocol-version",
    KNOWN_PROTOCOL_VERSIONS.includes(handshake?.protocolVersion),
    `protocolVersion ${JSON.stringify(handshake?.protocolVersion)}`,
  );
  add(
    "host-instance-id",
    typeof handshake?.sideHostInstanceId === "string" && handshake.sideHostInstanceId !== "",
    `sideHostInstanceId ${JSON.stringify(handshake?.sideHostInstanceId)}`,
  );
  add(
    "host-not-loopback",
    !isLoopbackHost(facts.hostId),
    `hostId ${JSON.stringify(facts.hostId)}`,
  );
  add(
    "arch-supported",
    SUPPORTED_ARCHES.includes(facts.arch),
    `arch ${JSON.stringify(facts.arch)}`,
  );

  // 「本地不运行远端程序」：本地进程表里出现从该路径启动的进程，就说明它被
  // 拉到本机来跑了，而这正是判据禁止的。
  const offenders = (localProcesses ?? []).filter((line) => line.includes(remotePath));
  add(
    "no-local-remote-program",
    offenders.length === 0,
    offenders.length === 0 ? "no local process runs the remote binary" : offenders.join(" | "),
  );

  return {
    verdict: checks.every((check) => check.ok) ? "pass" : "fail",
    checks,
    remoteIdentity: {
      hostId: facts.hostId,
      os: facts.os,
      arch: facts.arch,
      version: facts.version,
    },
  };
}

/// A17 的判定。
///
/// 整体**永远**是 unsupported（spec §4.2 选择 (b)）：A17 是一条判据，不是四条。
/// 只证明「重连不复用旧绑定」就给 pass，等于宣称离线缓存、headless 更新、
/// 过期客户端也都成立。但证据要完整写出——读的人应该能看见已证明的部分。
export function judgeA17({ first, second, orphanCount, profileKind, facts }) {
  const checks = [];
  const add = (name, ok, detail) => checks.push({ name, ok: Boolean(ok), detail });

  const firstHost = {
    kind: profileKind,
    instanceId: first?.sideHostInstanceId ?? "",
    generation: first?.generation ?? 1,
  };
  const secondHost = {
    kind: profileKind,
    instanceId: second?.sideHostInstanceId ?? "",
    generation: second?.generation ?? 1,
  };

  add(
    "no-orphan",
    orphanCount === 0,
    `${orphanCount} remote app-server process(es) survived the disconnect`,
  );
  add(
    "reconnect-new-identity",
    firstHost.instanceId !== "" && secondHost.instanceId !== "" && firstHost.instanceId !== secondHost.instanceId,
    `${JSON.stringify(firstHost.instanceId)} -> ${JSON.stringify(secondHost.instanceId)}`,
  );
  // 绑在第一次身份上的视图，在第二次身份下必须不成立。这是判据的实质内容。
  add(
    "old-binding-invalid",
    !isBindingCurrent(
      { kind: firstHost.kind, instanceId: firstHost.instanceId, generation: firstHost.generation },
      secondHost,
    ),
    "the binding made on the first connection does not hold for the second",
  );
  // 显式记录代次相同：这正是「只比 generation 会误判」的证据，值得留在报告里。
  add(
    "generation-reused",
    firstHost.generation === secondHost.generation,
    `both connections report generation ${firstHost.generation}; identity, not generation, invalidates the binding`,
  );

  return {
    verdict: "unsupported",
    reason: UNPROVEN_A17,
    checks,
    // 判词是 unsupported，但证据不能因此变空：报告读者要能看见这是哪台机器上
    // 验过的。缺身份的 remote 证据会被校验器直接拒绝。
    remoteIdentity: {
      hostId: facts?.hostId ?? "",
      os: facts?.os ?? "",
      arch: facts?.arch ?? "",
      version: facts?.version ?? "",
    },
  };
}

/// 在本地找是否存在**真的执行**了远端二进制的进程。
///
/// 子串匹配会匹配到调用链上的每一个 shell：本进程和它的父 `sh -c` 命令行里
/// 都含 `remotePath`（它是参数）。那样这条判据每次都红，然后被所有人忽略——
/// 一个永远失败的检查等于没有检查。
///
/// 所以两个判据都不看「命令行里出现过这个字符串」：
///
///   1. `comm`（内核记录的可执行文件）就是那个路径 —— 直接执行；
///   2. 命令行去掉解释器前缀后，**第一个词**是那个路径 —— `sh -c '<path> ...'`。
export function findLocalProcesses(remotePath, { psOutput, selfPid } = {}) {
  if (typeof remotePath !== "string" || remotePath === "") return [];
  let output = psOutput;
  if (output === undefined) {
    try {
      output = execFileSync("ps", ["axo", "pid=,comm="], { encoding: "utf8" });
    } catch {
      // 查不到进程表时返回空：这不是「本地在跑远端程序」的证据，不该因此判失败。
      return [];
    }
  }
  const ownPid = String(selfPid ?? process.pid);
  const offenders = [];
  for (const line of output.split("\n")) {
    const match = /^\s*(\d+)\s+(.*)$/.exec(line);
    if (!match) continue;
    const [, pid, command] = match;
    if (pid === ownPid) continue;
    // `comm` 是内核记录的可执行文件，任何 shell 包装都改不了它。
    const executable = command.trim();
    if (executable === remotePath) {
      offenders.push(executable);
      continue;
    }
    // `sh -c` 与 `bash -lc` 之类：真正的目标在解释器之后。只认**第一个**非
    // 解释器词，避免把参数里出现的路径算成执行。
    const words = executable.split(/\s+/);
    const firstReal = words.find(
      (word) => !["sh", "bash", "zsh", "dash", "-c", "-lc", "-l"].includes(path.basename(word)) &&
        !word.startsWith("-"),
    );
    if (firstReal && firstReal.replace(/^['"]|['"]$/g, "") === remotePath) {
      offenders.push(executable);
    }
  }
  return offenders;
}

/// 在远端跑一条**普通**命令（`hostname`、`uname` 等），返回 stdout。
///
/// 不能把 `remotePath` 拼在前面：`yi-agent` 只认自己的子命令，`yi-agent
/// hostname` 会被当成一个不存在的子命令。只有 `--version` 需要那个前缀。
function remoteFileCommand({ host, args }) {
  return execFileSync(
    "ssh",
    ["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", host, args.join(" ")],
    { encoding: "utf8" },
  );
}

/// 远端二进制的 `--version`，供与本机比较。
function remoteVersionCommand({ host, remotePath }) {
  return execFileSync(
    "ssh",
    ["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", host, `${shellQuote(remotePath)} --version`],
    { encoding: "utf8" },
  );
}

/// 真跑一次 A16。
export function runA16({ host, remotePath, remoteHome, localVersion, session: injected }) {
  const facts = collectRemoteFacts({
    run: (args) => {
      // `--version` 要带二进制路径，其余（hostname/uname/sw_vers）是远端自己的
      // 命令，加前缀会被当成 yi-agent 的子命令而失败。
      if (args[0] === "--version") return remoteVersionCommand({ host, remotePath });
      return remoteFileCommand({ host, args });
    },
  });

  const startSession =
    injected ??
    (() =>
      RemoteSession.connect({
        host,
        remotePath,
        remoteHome,
        spawn: nodeSpawn,
      }));

  return (async () => {
    const session = await startSession();
    try {
      const handshake = await session.initialize();
      return judgeA16({
        facts,
        local: { version: localVersion },
        handshake,
        localProcesses: findLocalProcesses(remotePath),
        remotePath,
      });
    } finally {
      session.close();
    }
  })();
}

/// 真跑一次 A17：连两次，中间断开并查远端残留。
export function runA17({ host, remotePath, remoteHome, profileKind = "remote:probe", session: injected }) {
  const startSession =
    injected ??
    (() =>
      RemoteSession.connect({
        host,
        remotePath,
        remoteHome,
        spawn: nodeSpawn,
      }));

  const facts = collectRemoteFacts({
    run: (args) => {
      if (args[0] === "--version") return remoteVersionCommand({ host, remotePath });
      return remoteFileCommand({ host, args });
    },
  });

  const orphanCount = () => {
    const output = execFileSync(
      "ssh",
      ["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", host, `pgrep -f ${JSON.stringify(remotePath)} | wc -l`],
      { encoding: "utf8" },
    );
    return Number.parseInt(output.trim(), 10) || 0;
  };

  return (async () => {
    const first = await startSession();
    let firstHello;
    try {
      firstHello = await first.initialize();
    } finally {
      first.close();
    }

    const leftover = orphanCount();

    const second = await startSession();
    try {
      const secondHello = await second.initialize();
      return judgeA17({
        first: firstHello,
        second: secondHello,
        orphanCount: leftover,
        profileKind,
        facts,
      });
    } finally {
      second.close();
    }
  })();
}

async function main(argv) {
  let args;
  try {
    args = parseArgs(argv);
  } catch (error) {
    console.error(`e2e-remote: ${error.message}`);
    return 2;
  }

  try {
    let result;
    if (args.case === "A16") {
      result = await runA16({
        host: args.host,
        remotePath: args.remotePath,
        remoteHome: args.remoteHome,
        localVersion: args.localVersion,
      });
    } else {
      result = await runA17({
        host: args.host,
        remotePath: args.remotePath,
        remoteHome: args.remoteHome,
      });
    }
    result.case = args.case;
    console.log(args.json ? JSON.stringify(result) : formatHuman(result));
    return result.verdict === "fail" ? 1 : 0;
  } catch (error) {
    // 连接失败、握手超时、远端二进制缺失：全部是 fail，不是 unsupported。
    // 把「连不上」说成「不需要」是这份报告最不该出现的粉饰。
    const result = {
      case: args.case,
      verdict: "fail",
      reason: error.message,
      checks: [],
    };
    console.log(args.json ? JSON.stringify(result) : formatHuman(result));
    return 1;
  }
}

function formatHuman(result) {
  const lines = [`${result.case}: ${result.verdict}`];
  if (result.reason) lines.push(`  reason: ${result.reason}`);
  for (const check of result.checks ?? []) {
    lines.push(`  ${check.ok ? "ok  " : "FAIL"} ${check.name}: ${check.detail}`);
  }
  return lines.join("\n");
}

if (process.argv[1] && path.resolve(process.argv[1]) === path.resolve(fileURLToPath(import.meta.url))) {
  main(process.argv.slice(2)).then((code) => process.exit(code));
}
