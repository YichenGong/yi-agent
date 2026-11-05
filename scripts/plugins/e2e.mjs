// 独立发布验收执行器（计划 T27 缺失的那一半）。
//
// `e2e-report.mjs` 只回答「这份报告能不能算全部通过」，却从来没有东西去**产出**
// 报告。本文件补上产出方，因此它的全部风险都在一处：会不会把「没跑」写成
// 「通过」。为此这里刻意把决定判词的地方收成唯一一个函数
// [`verdictFor`]，且它对三种输入有明确态度：
//
//   - `pass`   —— 命令真的跑过且成功；
//   - `fail`   —— 跑过但失败，**或者** 根本没跑（`missing`）；
//   - `unsupported` —— 只有该用例声明了 `unsupportedWithout`、而对应的能力
//     （远端主机 / 官方签名包）确实缺席时才允许。
//
// 最后一条是这套东西最容易滑坡的地方：把「没有远端机器」写成 `skipped` 看起来
// 很诚实，实际上报告会少一条通过、却也没失败，读的人容易当成「无关紧要」。
// 所以本文件**不产生 `skipped`**，词汇表只有 pass / fail / unsupported。

import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { CASES_PATH as CATALOGUE_PATH } from "./e2e-report.mjs";

const HERE = path.dirname(fileURLToPath(import.meta.url));

/// 命令映射的路径。它是「哪条命令证明哪条验收」的唯一真相。
export const COMMANDS_PATH = path.join(HERE, "acceptance-commands.json");

/// 证据种类。与 `e2e-report.mjs` 认识的词汇表保持一致，多一个都会让校验器
/// 认不出、少一个会让某类验收无法举证。
export const EVIDENCE_KINDS = Object.freeze(["command", "artifact", "remote", "official"]);

/// `unsupportedWithout` 允许的能力名。
export const CAPABILITIES = Object.freeze(["remote", "official"]);

const VERDICTS = Object.freeze(["pass", "fail", "unsupported"]);

function fail(message) {
  throw new Error(message);
}

function isNonEmptyString(value) {
  return typeof value === "string" && value.trim() !== "";
}

/// 读取并校验命令映射。
///
/// 校验刻意比「字段存在」更严：它必须与 `acceptance-cases.json` 的 id 集合**完全
/// 相等**。少一条意味着有条验收永远不会被证据覆盖；多一条意味着有人写了个校验器
/// 不认识的 id，两条都不会让报告变成「全部通过」，但都会让失败原因变得难查，
/// 所以在加载时就拒绝。
export function loadCommandMap(file = COMMANDS_PATH) {
  const parsed = JSON.parse(readFileSync(file, "utf8"));
  if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
    fail(`${file}: the command map must be a JSON object`);
  }
  if (!Array.isArray(parsed.cases) || parsed.cases.length === 0) {
    fail(`${file}: "cases" must be a non-empty array`);
  }

  const seen = new Set();
  for (const entry of parsed.cases) {
    if (entry === null || typeof entry !== "object") {
      fail(`${file}: every case entry must be an object`);
    }
    const id = entry.id;
    if (!isNonEmptyString(id)) {
      fail(`${file}: every case entry needs a non-empty id`);
    }
    if (seen.has(id)) {
      fail(`${file}: ${id} appears more than once; each id needs exactly one entry`);
    }
    seen.add(id);

    if (!Array.isArray(entry.commands) || entry.commands.length === 0) {
      // 没有命令的用例就是「我记得验证过」。这种用例存在一天，报告就能
      // 在多一天里声称自己覆盖了它。
      fail(`${file}: ${id} must list at least one command`);
    }
    for (const command of entry.commands) {
      if (command === null || typeof command !== "object") {
        fail(`${file}: ${id} has a command entry that is not an object`);
      }
      if (!EVIDENCE_KINDS.includes(command.kind)) {
        fail(
          `${file}: ${id} uses unknown evidence kind ${JSON.stringify(command.kind)} ` +
            `(allowed: ${EVIDENCE_KINDS.join(", ")})`,
        );
      }
      if (!isNonEmptyString(command.run)) {
        fail(`${file}: ${id} has a command entry with no "run"`);
      }
      if (command.cwd !== undefined && !isNonEmptyString(command.cwd)) {
        fail(`${file}: ${id} has a command entry whose "cwd" is not a path`);
      }
      if (command.requires !== undefined) {
        if (!CAPABILITIES.includes(command.requires)) {
          fail(
            `${file}: ${id} requires unknown capability ${JSON.stringify(command.requires)} ` +
              `(allowed: ${CAPABILITIES.join(", ")})`,
          );
        }
      }
      if (command.needs !== undefined) {
        if (!Array.isArray(command.needs) || command.needs.some((p) => !isNonEmptyString(p))) {
          fail(`${file}: ${id} has a command whose "needs" is not a list of paths`);
        }
      }
    }

    const without = entry.unsupportedWithout;
    if (without !== null && without !== undefined && !CAPABILITIES.includes(without)) {
      fail(
        `${file}: ${id} names unknown capability ${JSON.stringify(without)} ` +
          `(allowed: ${CAPABILITIES.join(", ")}, or null)`,
      );
    }
  }

  // 与验收目录逐条对齐。目录是 A01–A20 的权威定义，映射不能自己另立一套。
  const catalogue = JSON.parse(readFileSync(CATALOGUE_PATH, "utf8"));
  const catalogueIds = catalogue.cases.map((entry) => entry.id);
  const missing = catalogueIds.filter((id) => !seen.has(id));
  const extra = [...seen].filter((id) => !catalogueIds.includes(id));
  if (missing.length > 0) {
    fail(`${file}: no command is bound for ${missing.join(", ")}`);
  }
  if (extra.length > 0) {
    fail(`${file}: ${extra.join(", ")} is not an acceptance id in the catalogue`);
  }

  return parsed;
}

/// 选出本次要跑的用例。
///
/// `suite` 是 `"all"`、单个 id，或逗号分隔的 id 列表。返回顺序**永远按目录顺序**，
/// 这样报告的可读性与 diff 稳定性不依赖调用方怎么写参数。
export function planSuites(commandMap, suite) {
  const catalogue = JSON.parse(readFileSync(CATALOGUE_PATH, "utf8"));
  const order = catalogue.cases.map((entry) => entry.id);
  const byId = new Map(commandMap.cases.map((entry) => [entry.id, entry]));

  const raw = isNonEmptyString(suite) ? suite.trim() : "";
  if (raw === "" ) fail("--suite needs a value (\"all\" or acceptance ids)");

  let wanted;
  if (raw === "all") {
    wanted = new Set(order);
  } else {
    const ids = raw
      .split(",")
      .map((part) => part.trim())
      .filter((part) => part !== "");
    if (ids.length === 0) fail(`--suite ${JSON.stringify(raw)} selected nothing`);
    for (const id of ids) {
      if (!byId.has(id)) fail(`unknown acceptance id in --suite: ${id}`);
    }
    wanted = new Set(ids);
  }

  return order.filter((id) => wanted.has(id)).map((id) => byId.get(id));
}

/// 组装一条证据。
///
/// `detail` 是能让人**复核**的东西（命令、摘要、报告路径），不是一个标题——
/// 标题无法重跑。可选字段只在真的提供了才写入，避免报告里出现一堆 `null` 让人
/// 误以为有证据。
export function buildEvidenceEntry({
  kind,
  detail,
  remoteIdentity,
  signingIdentity,
  notarized,
} = {}) {
  if (!EVIDENCE_KINDS.includes(kind)) {
    fail(`evidence kind must be one of ${EVIDENCE_KINDS.join(", ")}, got ${JSON.stringify(kind)}`);
  }
  if (!isNonEmptyString(detail)) {
    fail("evidence entry needs a non-empty detail (a title cannot be re-inspected)");
  }
  const entry = { kind, detail };
  if (remoteIdentity !== undefined && remoteIdentity !== null) {
    entry.remoteIdentity = remoteIdentity;
  }
  if (signingIdentity !== undefined && signingIdentity !== null) {
    entry.signingIdentity = signingIdentity;
  }
  if (notarized !== undefined && notarized !== null) {
    entry.notarized = notarized;
  }
  return entry;
}

/// 唯一决定判词的地方。
///
/// `outcome` 来自执行层，取值 `pass` / `fail` / `missing`。注意 `missing` 是
/// **fail** 而不是跳过：一条命令没找到、没运行，恰恰说明这条验收没有被验证。
export function verdictFor({ caseSpec, outcome, capabilities = {}, executorUnsupported = null } = {}) {
  if (caseSpec === null || typeof caseSpec !== "object") {
    fail("verdictFor needs a case specification");
  }
  if (!["pass", "fail", "missing"].includes(outcome)) {
    fail(`unknown outcome ${JSON.stringify(outcome)}`);
  }

  // 执行器自己说 unsupported 时优先信它：它知道哪些判据没被证明，退出码只知道
  // 「跑完了」。这是 A17 那种「证明了一部分」的用例唯一诚实的记法。
  if (executorUnsupported) return "unsupported";
  if (outcome === "pass") return "pass";
  if (outcome === "fail") return "fail";

  // 走到这里说明 outcome 是 missing。两种情况会降级成 unsupported：
  //
  //   1. 该用例声明的能力（远端主机 / 官方签名包）确实缺席；
  //   2. 它需要的**前置产物**不在（没有构建好的 `.app` 等）。
  //
  // 两者都是「这台机器此刻证明不了它」，不是「实现坏了」。除此之外的 missing
  // 一律是 fail：命令明明该能跑却跑不起来，那是真问题。
  const without = caseSpec.unsupportedWithout;
  if (without !== null && without !== undefined && capabilities[without] !== true) {
    return "unsupported";
  }
  if (caseSpec.prerequisiteMissing === true) {
    return "unsupported";
  }
  return "fail";
}

/// 一条用例被判成 unsupported 时给人看的理由。
export function unsupportedReason(caseSpec) {
  const without = caseSpec.unsupportedWithout;
  if (without === "remote") {
    return "no remote host was provided; a loopback peer cannot prove cross-machine behaviour";
  }
  if (without === "official") {
    return "no official signed release candidate was authorised; a local build cannot prove the download path";
  }
  return `no ${without} capability was available`;
}

/// 把执行结果折成报告。
///
/// 形状刻意与 `e2e-report.mjs` 的期望一致：顶层 `buildKind` + `cases`，每条
/// `{id, verdict, evidence}`。`now` 由调用方注入，让同一次执行可重放、测试可断言。
export function assembleReport({ buildKind, suite, results, capabilities = {}, now } = {}) {
  if (buildKind !== "probe" && buildKind !== "release") {
    fail(`buildKind must be "probe" or "release", got ${JSON.stringify(buildKind)}`);
  }
  if (!Array.isArray(results)) fail("results must be an array");
  if (!isNonEmptyString(now)) fail("assembleReport needs a timestamp in `now`");

  const cases = results.map((result) => {
    const verdict = verdictFor({
      caseSpec: result.caseSpec,
      outcome: result.outcome,
      capabilities,
      executorUnsupported: result.executorUnsupported ?? null,
    });
    if (!VERDICTS.includes(verdict)) {
      fail(`internal: verdict ${verdict} is outside the vocabulary`);
    }
    const entry = {
      id: result.caseSpec.id,
      verdict,
      evidence: result.evidence ?? [],
    };
    if (verdict === "unsupported") {
      entry.reason = result.executorUnsupported ?? result.reason ?? unsupportedReason(result.caseSpec);
    }
    // fail 同样要写原因。PASS/UNSUPPORTED 之外留空的报告会把「为什么红」这件事
    // 丢掉，等于让人凭猜去修。
    if (verdict === "fail") {
      entry.reason =
        result.reason ??
        (result.caseSpec?.commands?.length
          ? `no command output was captured for ${result.caseSpec.commands.map((c) => c.run).join("; ")}`
          : "the case has no commands to prove it");
    }
    return entry;
  });

  return {
    buildKind,
    generatedAt: now,
    ...(isNonEmptyString(suite) ? { suite } : {}),
    cases,
  };
}

/// 一次命令执行的结局。
///
/// `missing` 是刻意与 `fail` 分开的：命令根本跑不起来（可执行文件不存在、
/// 超时被强杀）与「跑了但断言不过」是两种不同的坏消息，报告里要能分辨，
/// 否则读的人会去修错的实现。两者在判词上都算失败，见 [`verdictFor`]。
export const OUTCOMES = Object.freeze(["pass", "fail", "missing"]);

/// 解析一条命令里的可执行文件与环境变量。
///
/// 命令是**字符串**而不是 argv 数组，因为映射表要能被人读懂、能直接复制到终端
/// 里复算。这里自己拆分而不是过 shell，是为了让报告里的命令就是真正跑的那条：
/// 过 shell 会引入引号、变量展开、管道这些无法从字符串反推的东西。
export function parseCommand(raw) {
  if (!isNonEmptyString(raw)) fail("a command must be a non-empty string");
  // 只支持前置 `FOO=bar` 形式的环境变量，够用且能一眼看懂。
  const tokens = raw.match(/(?:[^\s"']+|"[^"]*"|'[^']*')+/g) ?? [];
  const env = {};
  let index = 0;
  while (index < tokens.length && /^[A-Za-z_][A-Za-z0-9_]*=/.test(tokens[index])) {
    const token = tokens[index];
    const split = token.indexOf("=");
    env[token.slice(0, split)] = token.slice(split + 1).replace(/^["']|["']$/g, "");
    index += 1;
  }
  const rest = tokens.slice(index).map((token) => token.replace(/^["']|["']$/g, ""));
  if (rest.length === 0) fail(`command has no executable: ${raw}`);
  return { command: rest[0], args: rest.slice(1), env };
}

/// 缺这个可执行文件，就说明这条命令不可能有意义地跑起来。
///
/// 命令里可能带 `cargo` —— 它不在默认 PATH 上。这里**不**猜路径、也不替用户
/// 装工具链：`ENOENT` 会被如实记成 missing，判词是 fail，读的人自己知道是环境
/// 问题。猜路径的代价是把「环境没配好」伪装成「验收通过」。
function looksLikeEnoent(error) {
  return (
    error &&
    (error.code === "ENOENT" ||
      /ENOENT|command not found|not found/.test(String(error.message ?? "")))
  );
}

/// 跑一条命令并把结局归一成 `pass` / `fail` / `missing`。
///
/// `runner` 可注入，测试因此不需要真的 fork 一个进程；生产路径用
/// `execFileSync`。超时用 SIGKILL 收尾，且**超时算 missing 而不是 fail**：
/// 我们不知道它是卡在实现里还是卡在环境里，把它说成「实现失败」会误导。
export function runCommand(
  raw,
  {
    cwd = process.cwd(),
    timeoutMs = 30 * 60 * 1000,
    runner = execFileSync,
    env = process.env,
    commandCwd = null,
  } = {},
) {
  const parsed = parseCommand(raw);
  // 每条命令可以声明自己的工作目录。cargo 与 fault-matrix 都必须在
  // `yi-agent-rs/` 里跑；从仓库根跑得到的是「找不到 Cargo.toml」，那种错误
  // 会被读成实现失败，而真正的问题是工作目录。相对路径按传入的 `cwd` 解析。
  const effectiveCwd = commandCwd ? path.resolve(cwd, commandCwd) : cwd;
  try {
    const stdout = runner(parsed.command, parsed.args, {
      cwd: effectiveCwd,
      encoding: "utf8",
      env: { ...env, ...parsed.env },
      timeout: timeoutMs,
      killSignal: "SIGKILL",
      maxBuffer: 512 * 1024 * 1024,
      stdio: ["ignore", "pipe", "pipe"],
    });
    return { outcome: "pass", stdout: String(stdout ?? ""), stderr: "", command: raw };
  } catch (error) {
    const stdout = String(error?.stdout ?? "");
    const stderr = String(error?.stderr ?? "");
    // 非零退出码但进程跑起来了：这是货真价实的失败。
    const exitCode = error?.status;
    if (typeof exitCode === "number") {
      return { outcome: "fail", exitCode, stdout, stderr, command: raw };
    }
    // 超时（execFileSync 用 SIGTERM/SIGKILL 且 status 为 null）。
    if (error?.signal) {
      return { outcome: "missing", reason: `killed by ${error.signal}`, stdout, stderr, command: raw };
    }
    if (looksLikeEnoent(error)) {
      return {
        outcome: "missing",
        reason: `executable not found: ${parsed.command}`,
        stdout,
        stderr,
        command: raw,
      };
    }
    return {
      outcome: "missing",
      reason: error?.message ?? "unknown execution error",
      stdout,
      stderr,
      command: raw,
    };
  }
}

/// 探测这台机器能提供哪些能力。
///
/// 只认**显式配置**：远端主机要么来自 `--remote-host`，要么来自
/// `YI_AGENT_ACCEPTANCE_REMOTE_HOST`；官方构建同理。刻意不从任何历史产物里
/// 反推「上次跑过所以这次也算」——那会让一份旧报告悄悄成为新证据。
export function detectCapabilities({ env = process.env, flags = {} } = {}) {
  const remoteHost = (flags.remoteHost ?? env.YI_AGENT_ACCEPTANCE_REMOTE_HOST ?? "").trim();
  const officialBuild = flags.official ?? env.YI_AGENT_ACCEPTANCE_OFFICIAL === "1";
  return {
    remote: remoteHost !== "",
    remoteHost,
    official: Boolean(officialBuild),
  };
}

/// 命令声明的**前置产物**是否就位。
///
/// 这是从一次真实的误判加的：A03/A07/A08 需要先构建好的 `.app`，A20 需要官方
/// 签名包。缺产物时命令跑起来只会报「文件不存在」，判词落成 `fail`，读报告的
/// 人于是去查实现——真正缺的是一个构建步骤。前置不满足属于**环境未就绪**，
/// 与「能力未授信」是同一类：如实记 unsupported 并说明缺什么。
export function missingPrerequisites(command, { root = process.cwd() } = {}) {
  const missing = [];
  for (const relative of command.needs ?? []) {
    if (!existsSync(path.resolve(root, relative))) missing.push(relative);
  }
  return missing;
}

/// `.app` 是不是一个能独立运行的正式构建。
///
/// 来源是一次 240 秒超时：产物目录里躺着一个 dev 构建的 `.app`（`frontendDist`
/// 没打进去，二进制里写死 `http://localhost:1420`）。探针启动它，页面永远加载
/// 不出来，于是干等到超时被杀，报告记成 A07/A08「实现失败」——真正缺的是一次
/// 正式构建。文件在、但内容不是正式产物，光看路径存在判断不出来。
export function isDevBuild(appPath) {
  const binary = path.join(appPath, "Contents", "MacOS", "desktop");
  if (!existsSync(binary)) return false;
  try {
    return readFileSync(binary).includes("localhost:1420");
  } catch {
    // 读不到二进制就不下结论：当成可用，让真正的问题自己暴露。
    return false;
  }
}

/// 前置里那些「存在但是 dev 构建」的 `.app`。
export function devBuildPrerequisites(command, { root = process.cwd() } = {}) {
  return (command.needs ?? [])
    .map((relative) => path.resolve(root, relative))
    .filter((absolute) => absolute.endsWith(".app") && existsSync(absolute) && isDevBuild(absolute));
}

/// 从一次失败里取出最能说明问题的一行。
///
/// 报告曾经在 fail 时只写 `{id, verdict, evidence: []}`——没有原因、没有证据。
/// 那次全量跑出 15 条 fail 全是这个形状，读的人根本不知道要去修什么（真实原因
/// 是 PATH 里没有 cargo）。失败必须自带原因，否则这份报告不如不生成。
export function failureSummary(result) {
  const command = result.command ?? "(unknown command)";
  const exitCode = typeof result.exitCode === "number" ? ` exited ${result.exitCode}` : "";
  // `missing` 的原因不在 stderr 里（进程根本没起来），在 reason 上。
  // 漏掉它会让「找不到 cargo」这种最该直接看懂的失败变成一句空话。
  const declared = String(result.reason ?? "").trim();
  const stderr = String(result.stderr ?? "").trim();
  const lastLine = stderr.split("\n").filter((line) => line.trim() !== "").pop();
  const detail = declared ? `: ${declared}` : lastLine ? `: ${lastLine.trim().slice(0, 300)}` : "";
  return `${command}${exitCode}${detail}`;
}

/// 把一条用例的多条命令折成一条结局。
///
/// 任一条 missing 即 missing（我们没能证明它），任一条 fail 即 fail；只有全部
/// pass 才 pass。
///
/// **能力缺席时一条命令都不跑**。这是从一次真实误判改出来的：A16/A17 要求
/// 非 loopback 的远端主机，没有主机时硬跑 `e2e-remote.mjs` 只会得到一个
/// 「文件/环境不存在」的失败，判词落成 `fail`，读报告的人于是以为远端实现坏
/// 了——真正缺的是那台机器。既然 `unsupportedWithout` 已经声明了缺什么，就
/// 直接按 `missing` 交给 `verdictFor` 降级，不去伪造执行痕迹。
export function runCase(caseSpec, options = {}) {
  const capabilities = options.capabilities ?? {};
  const without = caseSpec.unsupportedWithout;
  if (without && capabilities[without] !== true) {
    return {
      caseSpec,
      outcome: "missing",
      evidence: [],
      outcomes: [],
      skippedBecause: without,
    };
  }

  for (const command of caseSpec.commands) {
    if (command.requires && (options.capabilities ?? {})[command.requires] !== true) {
      return {
        caseSpec: { ...caseSpec, prerequisiteMissing: true },
        outcome: "missing",
        evidence: [],
        outcomes: [],
        skippedBecause: command.requires,
        reason: `missing capability: ${command.requires}`,
      };
    }
    const root = options.cwd ?? process.cwd();
    const missing = missingPrerequisites(command, { root });
    const devApps = devBuildPrerequisites(command, { root });
    if (devApps.length > 0) {
      // 产物在，但是 dev 构建：跑起来只会连不上的开发服务器，等出来的是一个超时。
      // 这是环境未就绪，不是实现失败。
      return {
        caseSpec: { ...caseSpec, prerequisiteMissing: true },
        outcome: "missing",
        evidence: [],
        outcomes: [],
        skippedBecause: "prerequisite",
        reason: `the .app is a dev build (it points at localhost:1420), not a release build: ${devApps
          .map((absolute) => path.relative(root, absolute))
          .join(", ")}`,
      };
    }
    if (missing.length > 0) {
      // 前置产物不在：这条用例无法被证据覆盖，但也不是实现失败。
      return {
        // 不改调用方传进来的对象：映射表被复用于多个用例时，一份副本更安全。
        caseSpec: { ...caseSpec, prerequisiteMissing: true },
        outcome: "missing",
        evidence: [],
        outcomes: [],
        skippedBecause: "prerequisite",
        reason: `missing prerequisite: ${missing.join(", ")}`,
      };
    }
  }

  const memo = options.memo ?? new Map();
  const outcomes = [];
  for (const command of caseSpec.commands) {
    const key = `${command.cwd ?? ""}\u0000${command.run}`;
    let result = memo.get(key);
    if (result === undefined) {
      result = runCommand(command.run, {
        ...options,
        commandCwd: command.cwd ?? null,
      });
      memo.set(key, result);
    }
    outcomes.push({ ...result, kind: command.kind });
  }
  // 声明了 `emitsRemoteIdentity` 的命令必须交出一行合规的身份 JSON。
  // 这里先校验再决定 outcome：跑成功了但没按契约输出，是**失败**而不是跳过。
  let remoteIdentityFailure = null;
  let executorUnsupported = null;
  const identities = new Map();
  for (const [index, command] of caseSpec.commands.entries()) {
    if (command.emitsRemoteIdentity !== true) continue;
    const outcome = outcomes[index];
    const parsed = outcome?.outcome === "pass" ? parseRemoteIdentity(outcome.stdout) : null;
    if (parsed.error) {
      remoteIdentityFailure = `${command.run}: ${parsed.error}`;
      break;
    }
    identities.set(index, parsed.identity);
    // 执行器自己判 unsupported 时，退出码仍是 0（它不是执行错误），所以
    // 只看退出码会把「只证明了一部分」记成通过。这正是要避免的粉饰：
    // A17 只证明了四条判据里的一条，报告里必须是 unsupported。
    if (parsed.verdict === "unsupported") {
      executorUnsupported = parsed.reason ?? "the executor reported unsupported";
    }
  }

  const evidence = outcomes
    .map((result, index) =>
      result.outcome === "pass"
        ? buildEvidenceEntry({
            kind: result.kind,
            detail: result.command,
            remoteIdentity: identities.get(index),
          })
        : null,
    )
    .filter((entry) => entry !== null);

  const anyMissing = outcomes.find((result) => result.outcome === "missing");
  const anyFailed = outcomes.find((result) => result.outcome === "fail");
  const outcome = remoteIdentityFailure
    ? "fail"
    : anyMissing
      ? "missing"
      : anyFailed
        ? "fail"
        : "pass";
  // 取第一个非 pass 的结局作原因。之前只看 fail，于是 `missing`（例如
  // 「找不到 cargo」）走完降级成为 fail 之后没有任何原因，报告里只剩一行
  // 「no command output was captured」。
  const problemOutcome = outcomes.find((result) => result.outcome !== "pass");
  return {
    caseSpec,
    outcome,
    evidence,
    outcomes,
    ...(remoteIdentityFailure
      ? { reason: remoteIdentityFailure }
      : problemOutcome
        ? { reason: failureSummary(problemOutcome), failedOutcome: problemOutcome.command }
        : {}),
    // 由 `verdictFor` 消费：执行器说 unsupported 的用例按 unsupported 记，
    // 不看它的退出码。
    ...(executorUnsupported ? { executorUnsupported } : {}),
  };
}

/// 解析命令 stdout 里的远端身份。
///
/// 契约：stdout 的**首行**是一个 JSON 对象，含
/// `{hostId, os, arch, version}` 四个非空字符串。四个字段缺一不可——缺了
/// 某个字段的身份在报告里看起来像证据，实际无法复核是哪台机器。
export function parseRemoteIdentity(stdout) {
  const firstLine = String(stdout ?? "")
    .split("\n")
    .map((line) => line.trim())
    .find((line) => line !== "");
  if (!firstLine) return { error: "no output to read a remote identity from" };
  let parsed;
  try {
    parsed = JSON.parse(firstLine);
  } catch {
    return { error: "stdout is not the promised JSON remote identity" };
  }
  if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
    return { error: "remote identity must be a JSON object" };
  }
  // 两种形状都收：顶层 `{hostId,...}`，以及远端执行器输出的
  // `{verdict, checks, remoteIdentity:{...}}`。后者的身份是嵌套的，读顶层
  // 会得到「缺 hostId」——一个看起来像脚本坏了的误报。
  const identity = parsed.remoteIdentity ?? parsed;
  if (identity === null || typeof identity !== "object") {
    return { error: "remote identity must be a JSON object" };
  }
  for (const field of ["hostId", "os", "arch", "version"]) {
    if (typeof identity[field] !== "string" || identity[field].trim() === "") {
      return { error: `remote identity is missing ${field}` };
    }
  }
  return {
    identity: {
      hostId: identity.hostId,
      os: identity.os,
      arch: identity.arch,
      version: identity.version,
    },
    ...(typeof parsed.verdict === "string" ? { verdict: parsed.verdict } : {}),
    ...(typeof parsed.reason === "string" ? { reason: parsed.reason } : {}),
  };
}

/// 执行选中的用例，得到 `assembleReport` 需要的结果列表。
///
/// 同一条命令（`run` + `cwd` 相同）在一次运行里只执行一次，结果按命令复用。
/// 这不是优化癖：映射表里 A09–A19 十几条用例都由 `fault-matrix.mjs` 证明，
/// 而它自己要跑十几分钟的 nextest。逐条重跑会让「跑一次全量验收」变成一小时，
/// 实际结果是没人跑它——一份没人跑的报告等于没有报告。
export function runSelected(selected, options = {}) {
  const memo = new Map();
  return selected.map((caseSpec) =>
    runCase(caseSpec, {
      ...options,
      memo,
    }),
  );
}

/// 命令行入口。
///
/// 参数：
///   --suite <all|A01,A02>            选哪些用例（默认 all）
///   --output <path>                  报告写到哪（默认 .artifacts/plugins/acceptance.json）
///   --build <probe|release>          被测构建的类别（默认 probe）
///   --remote-host <id>               提供远端主机身份（含 OS/架构/版本可选）
///   --require-pass                   全绿才退出 0
///   --dry-run                        只打印计划，不跑任何命令
export function parseArgs(argv) {
  const options = {
    suite: "all",
    output: ".artifacts/plugins/acceptance.json",
    buildKind: "probe",
    // null 而不是 ""：空串会作为 `flags.remoteHost` 覆盖掉环境变量，让
    // 「设了 YI_AGENT_ACCEPTANCE_REMOTE_HOST 却仍判 unsupported」变成一个
    // 极难查的谜。
    remoteHost: null,
    requirePass: false,
    dryRun: false,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index];
    switch (arg) {
      case "--suite":
        options.suite = argv[++index] ?? "";
        break;
      case "--output":
        options.output = argv[++index] ?? "";
        break;
      case "--build":
        options.buildKind = argv[++index] ?? "";
        break;
      case "--remote-host":
        options.remoteHost = argv[++index] ?? "";
        break;
      case "--require-pass":
        options.requirePass = true;
        break;
      case "--dry-run":
        options.dryRun = true;
        break;
      default:
        fail(`unknown argument: ${arg}`);
    }
  }
  if (!["probe", "release"].includes(options.buildKind)) {
    fail(`--build must be probe or release, got ${JSON.stringify(options.buildKind)}`);
  }
  if (!isNonEmptyString(options.output)) fail("--output needs a path");
  return options;
}

export function main(argv, { stdout = process.stdout, stderr = process.stderr } = {}) {
  const options = parseArgs(argv);
  const map = loadCommandMap();
  const selected = planSuites(map, options.suite);
  const capabilities = detectCapabilities({
    flags: options.remoteHost === null ? {} : { remoteHost: options.remoteHost },
  });

  if (options.dryRun) {
    stdout.write(`e2e: ${selected.length} case(s), build=${options.buildKind}\n`);
    for (const entry of selected) {
      stdout.write(`  ${entry.id}  ${entry.unsupportedWithout ?? "-"}  ${entry.commands.map((c) => c.run).join(" ; ")}\n`);
    }
    return 0;
  }

  const results = runSelected(selected, { capabilities });
  const report = assembleReport({
    buildKind: options.buildKind,
    suite: options.suite,
    results,
    capabilities,
    now: new Date().toISOString(),
  });

  const outputPath = path.resolve(process.cwd(), options.output);
  mkdirSync(path.dirname(outputPath), { recursive: true });
  writeFileSync(outputPath, `${JSON.stringify(report, null, 2)}\n`);

  const counts = report.cases.reduce((acc, entry) => {
    acc[entry.verdict] = (acc[entry.verdict] ?? 0) + 1;
    return acc;
  }, {});
  stdout.write(
    `e2e: wrote ${outputPath} — pass=${counts.pass ?? 0} fail=${counts.fail ?? 0} ` +
      `unsupported=${counts.unsupported ?? 0}\n`,
  );
  for (const entry of report.cases) {
    if (entry.verdict === "pass") continue;
    stderr.write(`  ${entry.id} ${entry.verdict}${entry.reason ? `: ${entry.reason}` : ""}\n`);
  }

  return options.requirePass ? requiresPassExitCode(report) : 0;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    process.exit(main(process.argv.slice(2)));
  } catch (error) {
    process.stderr.write(`e2e: ${error.message}\n`);
    process.exit(2);
  }
}

/// `--require-pass` 的退出码：只有每一条都是 `pass` 才是 0。
export function requiresPassExitCode(report) {
  if (!report || !Array.isArray(report.cases)) return 1;
  return report.cases.every((entry) => entry.verdict === "pass") ? 0 : 1;
}
