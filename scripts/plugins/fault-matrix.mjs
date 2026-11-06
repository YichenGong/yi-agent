// 故障注入矩阵执行器（计划 T27 Step 2）。
//
// 矩阵本身是 `fault-matrix.json`：把「断线提交 / 旧 frame 重放 / 两实例预算 /
// 活跃审批 / 更新收尾 / 候选不可见 / 共享配置修订冲突 / 旧安装接管 / 待清理
// 卸载 / 每阶段崩溃」这些场景绑到仓库里已有的测试上。
//
// 这个脚本只做一件事：真的跑它们，把每条命令和它的实际输出写进报告。它不
// 允许「没跑就写通过」：
//
//   - 矩阵里列出的每个测试都必须被 nextest 报告为 passed；
//   - 矩阵里出现一个 nextest 里不存在的测试名，直接失败（防止改名后静默失效）；
//   - 任何一项的测试有缺失或失败，该项 verdict 为 fail。
//
// 用法（工作目录必须是 yi-agent-rs，nextest 从这里发现 crate）：
//   node scripts/plugins/fault-matrix.mjs [--output .artifacts/plugins/fault-matrix.json] [--acceptance A12]

import { execFileSync } from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
export const MATRIX_PATH = path.join(HERE, "fault-matrix.json");

export function loadMatrix(file = MATRIX_PATH) {
  const parsed = JSON.parse(readFileSync(file, "utf8"));
  if (!Array.isArray(parsed.items) || parsed.items.length === 0) {
    throw new Error(`${file}: "items" must be a non-empty array`);
  }
  const ids = parsed.items.map((item) => item.id);
  if (new Set(ids).size !== ids.length) {
    throw new Error(`${file}: duplicate item ids`);
  }
  for (const item of parsed.items) {
    if (!Array.isArray(item.tests) || item.tests.length === 0) {
      // 没有测试的矩阵项不能存在：它只能是「我记得验证过」。
      throw new Error(`${item.id}: must list at least one test`);
    }
  }
  return parsed.items;
}

/// 把两种写法归一到同一个键。
///
/// 矩阵里人手写的是 `pkg::module::path test name`（空格分隔模块与测试名）；
/// nextest 实际打印的是 `pkg::module$test::path`，而集成测试还会多插一段
/// `pkg::binary$...`。两边都归一到 `pkg::module::path::test`：
///
///   "yi-agent-plugins::trust a_serial"        -> "yi-agent-plugins::trust::a_serial"
///   "yi-agent-plugins::trust$a_serial"        -> 同上
///   "yi-agent-sup::yi_agent_sup$uninstall::x" -> "yi-agent-sup::uninstall::x"
export function canonicalTestName(raw) {
  const text = String(raw).trim();
  // 形如 pkg::bin$rest
  if (text.includes("$")) {
    const [head, ...restParts] = text.split("$");
    const rest = restParts.join("$");
    const headParts = head.split("::");
    const pkg = headParts[0];
    let modpath = headParts.slice(1);
    // 集成测试的二进制名是包名把 `-` 换成 `_`；它不是模块路径，要丢掉。
    if (modpath.length === 1 && modpath[0] === pkg.replace(/-/g, "_")) {
      modpath = [];
    }
    return [pkg, ...modpath, ...[rest]].filter((part) => part !== "").join("::");
  }
  // 形如 pkg::mod::path test name
  const lastSpace = text.lastIndexOf(" ");
  if (lastSpace === -1) {
    throw new Error(`cannot parse test name: ${raw}`);
  }
  const modulePath = text.slice(0, lastSpace).trim();
  const testName = text.slice(lastSpace + 1).trim();
  return `${modulePath}::${testName}`;
}

export function packageOf(raw) {
  return String(raw).trim().split("::")[0];
}

/// 跑一遍 nextest 并返回「归一化测试名 -> 结果」。只跑矩阵涉及的包。
export function runMatrix(matrix, { cwd = process.cwd(), runner = execFileSync } = {}) {
  const packages = new Set(matrix.flatMap((item) => item.tests.map(packageOf)));
  const args = [
    "nextest",
    "run",
    "--no-fail-fast",
    // nextest 的 JSON 需要实验开关；JSON 里字段是稳定的。
    "--message-format",
    "libtest-json",
    ...Array.from(packages).flatMap((pkg) => ["-p", pkg]),
  ];
  const env = { ...process.env, NEXTEST_EXPERIMENTAL_LIBTEST_JSON: "1" };
  let stdout;
  try {
    stdout = runner("cargo", args, {
      cwd,
      encoding: "utf8",
      env,
      maxBuffer: 256 * 1024 * 1024,
      stdio: ["ignore", "pipe", "pipe"],
    });
  } catch (err) {
    // 有测试失败时 nextest 退出码非 0，但 stdout 仍是完整 JSON —— 必须继续
    // 解析，否则「失败了」会被误判成「跑不起来」。
    stdout = err.stdout ?? "";
    if (!stdout) throw err;
  }

  const results = new Map();
  for (const line of String(stdout).split("\n")) {
    if (!line.startsWith("{")) continue;
    let event;
    try {
      event = JSON.parse(line);
    } catch {
      continue;
    }
    if (event.type === "test" && event.name) {
      results.set(canonicalTestName(event.name), event.event);
    }
  }
  return results;
}

/// 把执行结果折进矩阵，产出可复核的报告。
export function buildReport(matrix, results, { cwd = process.cwd(), acceptance = null } = {}) {
  return {
    buildKind: "probe",
    generatedAt: new Date().toISOString(),
    cwd,
    ...(acceptance ? { acceptance } : {}),
    items: matrix.map((item) => {
      const outcomes = item.tests.map((testName) => {
        const key = canonicalTestName(testName);
        return { test: testName, key, outcome: results.get(key) ?? "missing" };
      });
      const failures = outcomes.filter((o) => o.outcome !== "ok");
      return {
        id: item.id,
        title: item.title,
        acceptance: item.acceptance ?? [],
        verdict: failures.length === 0 ? "pass" : "fail",
        outcomes,
      };
    }),
  };
}

export function summarise(report) {
  const failed = report.items.filter((item) => item.verdict !== "pass");
  return {
    ok: failed.length === 0 && report.items.length > 0,
    passed: report.items.length - failed.length,
    total: report.items.length,
    failed: failed.map((item) => ({
      id: item.id,
      missing: item.outcomes.filter((o) => o.outcome === "missing").map((o) => o.key),
      failed: item.outcomes.filter((o) => o.outcome === "failed").map((o) => o.key),
    })),
  };
}

function main(argv) {
  const outIndex = argv.indexOf("--output");
  const output = outIndex === -1 ? null : argv[outIndex + 1];
  const acceptanceIndex = argv.indexOf("--acceptance");
  const acceptance = acceptanceIndex === -1 ? null : argv[acceptanceIndex + 1];

  const matrix = loadMatrix();
  // 每条验收判据只判**自己归属**的矩阵项。此前 A09-A19 九条用例跑的是同一份
  // 完整矩阵，于是同一个整体结果被记成九条独立证据：既无法把失败定位到具体
  // 判据，也让「9 条通过」看起来比真实证据多 9 倍。矩阵项本身已声明归属
  // （fault-matrix.json 的 `acceptance` 字段），按它筛。
  const scoped = acceptance
    ? matrix.filter((item) => (item.acceptance ?? []).includes(acceptance))
    : matrix;
  if (acceptance && scoped.length === 0) {
    process.stderr.write(
      `fault matrix: no matrix item claims acceptance ${acceptance}\n`,
    );
    return 1;
  }
  const results = runMatrix(scoped);
  const report = buildReport(scoped, results, { acceptance });
  const summary = summarise(report);

  if (output) {
    mkdirSync(path.dirname(path.resolve(output)), { recursive: true });
    writeFileSync(output, `${JSON.stringify(report, null, 2)}\n`);
    console.log(`wrote ${output}`);
  }
  console.log(`fault matrix: ${summary.passed}/${summary.total} items passed`);
  for (const failure of summary.failed) {
    console.error(`  - ${failure.id}`);
    for (const test of failure.missing) console.error(`      missing: ${test}`);
    for (const test of failure.failed) console.error(`      failed:  ${test}`);
  }
  return summary.ok ? 0 : 1;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  process.exit(main(process.argv));
}
