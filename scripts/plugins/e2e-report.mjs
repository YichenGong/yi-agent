// 独立发布验收报告校验器（计划 T27 Step 1）。
//
// 这件事的本质是「不许含糊地宣布通过」。验收报告是 JSON，本条脚本只回答一个
// 问题：这份报告能不能算 A01–A20 全部通过？为此把四类常见的自我欺骗挡在
// 门外：
//
//   1. 少写几条 —— 报告里缺 A07，仍然是「20 项里 19 项通过」，看着很像通过。
//   2. 一个 ID 写两遍 —— 第一次失败、第二次通过，最后一条覆盖了前者。
//   3. 拿别的状态冒充通过 —— skipped / unsupported / timeout / error 都不是通过，
//      但都长得像「跑过了」。
//   4. 拿不该有的证据冒充证据 —— probe 构建不能证明正式签名包；loopback 不能
//      证明跨机器行为。
//
// 第 4 条是计划里最容易被跳过的一条，所以它在这里是硬规则而不是注释：
//   - 报告 `buildKind` 不是 `release` 时，任何 `requires` 含 `official` 的用例
//     都不允许是 `pass`，最多 `unsupported`；
//   - `requires` 含 `remote` 的用例（A16/A17）必须给出 `remoteIdentity`，且其
//     `hostId` 不能是 loopback / localhost，否则不算远端证据。
//
// 用法：
//   node scripts/plugins/e2e-report.mjs <report.json>
// 退出码 0 表示「A01–A20 全部通过且证据形态合规」，其它情况非 0。

import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
export const CASES_PATH = path.join(HERE, "acceptance-cases.json");

// 唯一算通过的最终结果。其它任何字符串都按「未通过」处理，包括拼错的值。
export const PASSING_VERDICT = "pass";

// 明确列出非通过状态，只为在报告里说清它为什么不算通过；未列出的未知字符串
// 同样不通过，所以漏写一个状态不会导致误放行。
const KNOWN_NON_PASSING = new Set([
  "fail",
  "failed",
  "skipped",
  "unsupported",
  "timeout",
  "timed_out",
  "error",
  "aborted",
]);

// loopback 与其别名。远端证据若只来自这些主机，证明不了跨机器行为。
const LOOPBACK_HOSTS = new Set([
  "localhost",
  "127.0.0.1",
  "::1",
  "0.0.0.0",
  "host.docker.internal",
]);

function isNonEmptyString(value) {
  return typeof value === "string" && value.trim() !== "";
}

/// 归一化主机标识：小写、去端口、去方括号。`127.0.0.1:8080` 与 `[::1]:9000`
/// 都必须被认成 loopback。
function normaliseHostId(raw) {
  let host = String(raw).trim().toLowerCase();
  if (host.startsWith("[")) {
    const end = host.indexOf("]");
    if (end !== -1) host = host.slice(1, end);
  } else if (host.split(":").length === 2) {
    host = host.split(":")[0];
  }
  return host;
}

function isLoopback(hostId) {
  const host = normaliseHostId(hostId);
  if (LOOPBACK_HOSTS.has(host)) return true;
  // 整个 127.0.0.0/8 都是 loopback。
  return /^127\.\d{1,3}\.\d{1,3}\.\d{1,3}$/.test(host);
}

/// 校验一份证据条目。返回错误文案数组（空数组表示这条证据可用）。
///
/// 证据必须是 `{kind, detail}` 形状：只有一个标题不算证据，因为标题无法复核。
function checkEvidenceEntry(entry, where) {
  const errors = [];
  if (typeof entry === "string") {
    errors.push(
      `${where}: evidence entries must be {kind, detail} objects, not bare strings ` +
        `(a bare string cannot be re-run or inspected)`,
    );
    return errors;
  }
  if (entry === null || typeof entry !== "object") {
    errors.push(`${where}: evidence entry must be an object`);
    return errors;
  }
  if (!isNonEmptyString(entry.kind)) {
    errors.push(`${where}: evidence entry needs a non-empty "kind"`);
  }
  if (!isNonEmptyString(entry.detail)) {
    errors.push(`${where}: evidence entry needs a non-empty "detail"`);
  }
  if (entry.kind === "remote") {
    const identity = entry.remoteIdentity;
    if (identity === null || typeof identity !== "object") {
      errors.push(
        `${where}: a "remote" evidence entry needs remoteIdentity ` +
          `{hostId, os, arch, version}; without a remote host identity this only ` +
          `proves loopback behaviour`,
      );
    } else {
      for (const field of ["hostId", "os", "arch", "version"]) {
        if (!isNonEmptyString(identity[field])) {
          errors.push(`${where}: remoteIdentity.${field} must be a non-empty string`);
        }
      }
      if (isNonEmptyString(identity.hostId) && isLoopback(identity.hostId)) {
        errors.push(
          `${where}: remoteIdentity.hostId "${identity.hostId}" is loopback; ` +
            `a loopback peer cannot prove cross-machine behaviour`,
        );
      }
    }
  }
  if (entry.kind === "official") {
    if (!isNonEmptyString(entry.signingIdentity)) {
      errors.push(`${where}: an "official" evidence entry needs signingIdentity`);
    }
    if (entry.notarized !== true) {
      errors.push(
        `${where}: an "official" evidence entry needs notarized: true ` +
          `(an un-notarised artifact does not prove the user's download path)`,
      );
    }
  }
  return errors;
}

/// 校验一份验收报告。
///
/// 返回 `{ ok, errors, passed, total }`。`ok` 为真当且仅当 A01–A20 各有且仅有
/// 一个最终结果、全部为 `pass`，且每条证据形态合规。
export function validateReport(report, catalogue) {
  const errors = [];
  const catalogueIds = catalogue.map((entry) => entry.id);
  const catalogueById = new Map(catalogue.map((entry) => [entry.id, entry]));

  if (report === null || typeof report !== "object" || Array.isArray(report)) {
    return { ok: false, errors: ["report must be a JSON object"], passed: 0, total: 0 };
  }
  if (report.buildKind !== "probe" && report.buildKind !== "release") {
    errors.push(
      `report.buildKind must be "probe" or "release", got ${JSON.stringify(report.buildKind)}`,
    );
  }
  if (!Array.isArray(report.cases)) {
    errors.push('report.cases must be an array');
    return { ok: false, errors, passed: 0, total: 0 };
  }

  // 一个 ID 是否只出现一次。重复即失败，哪怕是两个 pass——重复本身就说明
  // 报告不是「一项一个结果」。
  const seen = new Map();
  for (const entry of report.cases) {
    if (entry === null || typeof entry !== "object") {
      errors.push("every report case must be an object");
      continue;
    }
    if (!isNonEmptyString(entry.id)) {
      errors.push("every report case needs a non-empty id");
      continue;
    }
    seen.set(entry.id, (seen.get(entry.id) ?? 0) + 1);
  }

  const duplicated = [...seen.entries()].filter(([, count]) => count > 1);
  for (const [id, count] of duplicated) {
    errors.push(`${id}: appears ${count} times; each acceptance id needs exactly one result`);
  }

  const unknown = [...seen.keys()].filter((id) => !catalogueById.has(id));
  for (const id of unknown) {
    errors.push(`${id}: not a known acceptance id (catalogue has ${catalogueIds.length} cases)`);
  }

  const missing = catalogueIds.filter((id) => !seen.has(id));
  for (const id of missing) {
    errors.push(
      `${id}: missing from the report; a case that was never reported is not a pass`,
    );
  }

  let passed = 0;
  for (const entry of report.cases) {
    if (entry === null || typeof entry !== "object" || !isNonEmptyString(entry.id)) continue;
    const spec = catalogueById.get(entry.id);
    if (!spec) continue; // 已在上面的 unknown 里报过，不重复刷屏

    const where = entry.id;

    if (entry.verdict !== PASSING_VERDICT) {
      const why = KNOWN_NON_PASSING.has(entry.verdict)
        ? `"${entry.verdict}" is a recorded non-pass`
        : `verdict ${JSON.stringify(entry.verdict)} is not the only passing value`;
      errors.push(`${where}: ${why}; only "${PASSING_VERDICT}" counts`);
      continue;
    }

    if (!Array.isArray(entry.evidence) || entry.evidence.length === 0) {
      errors.push(`${where}: passing requires at least one evidence entry`);
      continue;
    }

    const evidenceErrors = [];
    for (const item of entry.evidence) {
      evidenceErrors.push(...checkEvidenceEntry(item, where));
    }

    // `requires` 是该用例必须有哪类证据。缺这类证据就说不上验证过它。
    const kinds = new Set(
      entry.evidence
        .filter((item) => item !== null && typeof item === "object")
        .map((item) => item.kind),
    );
    const remoteEvidence = entry.evidence.filter(
      (item) => item !== null && typeof item === "object" && item.kind === "remote",
    );
    // 远端证据「给了但全是 loopback」时，kind 仍在集合里；这里按可用证据计。
    const usableRemote = remoteEvidence.some(
      (item) =>
        item.remoteIdentity &&
        isNonEmptyString(item.remoteIdentity.hostId) &&
        !isLoopback(item.remoteIdentity.hostId),
    );
    for (const requirement of spec.requires ?? []) {
      if (requirement === "remote") {
        if (!usableRemote) {
          evidenceErrors.push(
            `${where}: requires remote evidence, but none names a non-loopback host`,
          );
        }
      } else if (!kinds.has(requirement)) {
        evidenceErrors.push(
          `${where}: requires "${requirement}" evidence, none was provided`,
        );
      }
    }

    // probe 构建不得冒充正式包。这一条比「有没有 official 证据」更强：即使
    // 报告里贴了签字信息，probe 构建也不是被下载执行的那个字节。
    if ((spec.requires ?? []).includes("official") && report.buildKind !== "release") {
      evidenceErrors.push(
        `${where}: requires an official signed artifact, but this report is a ` +
          `"${report.buildKind}" build; mark it "unsupported" until a signed ` +
          `release candidate is authorised`,
      );
    }

    if (evidenceErrors.length > 0) {
      errors.push(...evidenceErrors);
      continue;
    }
    passed += 1;
  }

  return {
    ok: errors.length === 0,
    errors,
    passed,
    total: catalogueIds.length,
  };
}

export function loadCatalogue(file = CASES_PATH) {
  const parsed = JSON.parse(readFileSync(file, "utf8"));
  if (!Array.isArray(parsed.cases) || parsed.cases.length === 0) {
    throw new Error(`${file}: "cases" must be a non-empty array`);
  }
  // 目录自身也要自洽：ID 唯一、顺序为 A01..A20。
  const ids = parsed.cases.map((entry) => entry.id);
  const unique = new Set(ids);
  if (unique.size !== ids.length) {
    throw new Error(`${file}: duplicate acceptance ids`);
  }
  return parsed.cases;
}

function main(argv) {
  const target = argv[2];
  if (!target) {
    console.error("usage: node scripts/plugins/e2e-report.mjs <report.json>");
    return 2;
  }
  const catalogue = loadCatalogue();
  let report;
  try {
    report = JSON.parse(readFileSync(target, "utf8"));
  } catch (err) {
    console.error(`${target}: cannot read report: ${err.message}`);
    return 2;
  }
  const result = validateReport(report, catalogue);
  if (result.ok) {
    console.log(`acceptance report passed: ${result.passed}/${result.total} cases`);
    return 0;
  }
  console.error(`acceptance report FAILED (${result.passed}/${result.total} passing)`);
  for (const error of result.errors) console.error(`  - ${error}`);
  return 1;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  process.exit(main(process.argv));
}
