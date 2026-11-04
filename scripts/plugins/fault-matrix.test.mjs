// `fault-matrix.mjs` 的自测（计划 T27 Step 2 的校验部分）。
//
// 真正跑测试的部分在 `fault-matrix.mjs` 里，需要 cargo/nextest，不适合放进
// 单元测试。这里测的是最容易出错、也最容易假装正确的两层：
//
//   1. 测试名归一化 —— nextest 打印的是 `pkg::module$test`，矩阵里人手写的是
//      `pkg::module test`。两边对不上时，一切都会「missing」，而 missing 如果
//      被当成通过，整张矩阵就失去意义。
//   2. 判定规则 —— 缺失≠通过，失败≠通过，空项不能 pass。

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  buildReport,
  canonicalTestName,
  loadMatrix,
  packageOf,
  summarise,
} from "./fault-matrix.mjs";

test("hand-written and nextest names normalise to the same key", () => {
  assert.equal(
    canonicalTestName("yi-agent-plugins::trust a_serial_below_the_accepted_one"),
    "yi-agent-plugins::trust::a_serial_below_the_accepted_one",
  );
  assert.equal(
    canonicalTestName("yi-agent-plugins::trust$a_serial_below_the_accepted_one"),
    "yi-agent-plugins::trust::a_serial_below_the_accepted_one",
  );
});

test("an integration-test binary segment is dropped", () => {
  // 集成测试的 nextest 名字里多一段与包同名的二进制名。
  assert.equal(
    canonicalTestName(
      "yi-agent-supervisors::yi_agent_supervisors$uninstall::tests::x",
    ),
    "yi-agent-supervisors::uninstall::tests::x",
  );
  assert.equal(
    canonicalTestName(
      "yi-agent-store::runtime_ipc$subscription_filters_apply_to_replay",
    ),
    "yi-agent-store::runtime_ipc::subscription_filters_apply_to_replay",
  );
});

test("nested module paths survive normalisation", () => {
  assert.equal(
    canonicalTestName("yi-agent-app-server::server::tests::turn approval_timeout"),
    "yi-agent-app-server::server::tests::turn::approval_timeout",
  );
  assert.equal(
    canonicalTestName("yi-agent-app-server::server::tests::turn$approval_timeout"),
    "yi-agent-app-server::server::tests::turn::approval_timeout",
  );
});

test("packageOf reads the crate from either spelling", () => {
  assert.equal(packageOf("yi-agent-plugins::trust a_test"), "yi-agent-plugins");
  assert.equal(packageOf("yi-agent-plugins::trust$a_test"), "yi-agent-plugins");
});

// —— 判定规则 ——

const matrix = [
  { id: "one", title: "one", acceptance: ["A01"], tests: ["pkg::mod a_test"] },
  { id: "two", title: "two", acceptance: ["A02"], tests: ["pkg::mod b_test"] },
];

function resultsOf(entries) {
  return new Map(entries.map(([name, outcome]) => [canonicalTestName(name), outcome]));
}

test("a matrix where every test passed is ok", () => {
  const report = buildReport(
    matrix,
    resultsOf([
      ["pkg::mod a_test", "ok"],
      ["pkg::mod b_test", "ok"],
    ]),
  );
  const summary = summarise(report);
  assert.equal(summary.ok, true);
  assert.equal(summary.passed, 2);
});

test("a missing test is not a pass", () => {
  const report = buildReport(
    matrix,
    resultsOf([
      ["pkg::mod a_test", "ok"],
      // b_test 根本没被跑到
    ]),
  );
  const summary = summarise(report);
  assert.equal(summary.ok, false);
  assert.equal(summary.passed, 1);
  assert.deepEqual(summary.failed, [
    { id: "two", missing: ["pkg::mod::b_test"], failed: [] },
  ]);
});

test("a failed test is not a pass", () => {
  const report = buildReport(
    matrix,
    resultsOf([
      ["pkg::mod a_test", "ok"],
      ["pkg::mod b_test", "failed"],
    ]),
  );
  const summary = summarise(report);
  assert.equal(summary.ok, false);
  assert.deepEqual(summary.failed, [
    { id: "two", missing: [], failed: ["pkg::mod::b_test"] },
  ]);
});

test("an ignored test is not a pass", () => {
  // nextest 对 ignored 报 `ignored`；它没有跑，所以不能算通过。
  const report = buildReport(
    matrix,
    resultsOf([
      ["pkg::mod a_test", "ok"],
      ["pkg::mod b_test", "ignored"],
    ]),
  );
  assert.equal(summarise(report).ok, false);
});

test("the report records the exact command-relevant test names", () => {
  const report = buildReport(matrix, resultsOf([["pkg::mod a_test", "ok"]]));
  assert.equal(report.buildKind, "probe");
  assert.equal(report.items[0].outcomes[0].test, "pkg::mod a_test");
  assert.equal(report.items[0].outcomes[0].key, "pkg::mod::a_test");
});

// —— 矩阵文件自身必须自洽 ——

test("the shipped matrix has unique ids and at least one test per item", () => {
  const items = loadMatrix();
  assert.ok(items.length >= 9, `expected the full matrix, got ${items.length} items`);
  const ids = new Set(items.map((item) => item.id));
  assert.equal(ids.size, items.length);
  for (const item of items) {
    assert.ok(item.tests.length > 0, `${item.id} has no tests`);
    assert.ok(item.acceptance.length > 0, `${item.id} maps to no acceptance id`);
  }
});

test("the matrix covers every scenario the plan names", () => {
  const ids = new Set(loadMatrix().map((item) => item.id));
  for (const required of [
    "disconnect-commit",
    "stale-frame-replay",
    "two-instance-budget",
    "active-approval",
    "update-wrap-up",
    "candidate-invisible",
    "shared-config-revision",
    "old-install-takeover",
    "pending-uninstall",
  ]) {
    assert.ok(ids.has(required), `matrix is missing ${required}`);
  }
});
