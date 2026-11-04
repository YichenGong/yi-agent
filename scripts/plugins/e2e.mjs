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

import { readFileSync } from "node:fs";
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
export function verdictFor({ caseSpec, outcome, capabilities = {} } = {}) {
  if (caseSpec === null || typeof caseSpec !== "object") {
    fail("verdictFor needs a case specification");
  }
  if (!["pass", "fail", "missing"].includes(outcome)) {
    fail(`unknown outcome ${JSON.stringify(outcome)}`);
  }

  if (outcome === "pass") return "pass";
  if (outcome === "fail") return "fail";

  // 走到这里说明 outcome 是 missing。只有声明的能力确实缺席时才降级成
  // unsupported，且必须给出理由——理由会写进报告，读的人才知道缺的是什么。
  const without = caseSpec.unsupportedWithout;
  if (without !== null && without !== undefined && capabilities[without] !== true) {
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
      entry.reason = result.reason ?? unsupportedReason(result.caseSpec);
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

/// `--require-pass` 的退出码：只有每一条都是 `pass` 才是 0。
export function requiresPassExitCode(report) {
  if (!report || !Array.isArray(report.cases)) return 1;
  return report.cases.every((entry) => entry.verdict === "pass") ? 0 : 1;
}
