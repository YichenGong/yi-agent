// `e2e-report.mjs` 的自测（计划 T27 Step 1）。
//
// 校验器的价值全在「它会不会拦住不该通过的报告」。所以这里绝大部分用例是
// 反例：每种自我欺骗手法各一条。只有一条正例，且它必须真的 20/20。
//
// 计划点名的两处变异必须在测试里各有一条：缺少 A07 整条、A10 通过但无证据。

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  loadCatalogue,
  validateReport,
  PASSING_VERDICT,
} from "./e2e-report.mjs";

const catalogue = loadCatalogue();

/// 构造一条合规的通过记录。默认带一条 command 证据；需要 official/remote 的
/// 用例由调用方补充。
function caseEntry(id, extra = {}) {
  return {
    id,
    verdict: PASSING_VERDICT,
    evidence: [{ kind: "command", detail: `just acceptance ${id}` }],
    ...extra,
  };
}

/// 完整通过的 release 报告。这是唯一应当 ok 的报告，其余用例都从它出发做单点
/// 破坏——这样失败原因一定是那条破坏，而不是别的缺项。
function passingReport() {
  const remote = {
    hostId: "runner-b.internal.example",
    os: "macOS 15.2",
    arch: "aarch64",
    version: "1.9.0",
  };
  const official = {
    kind: "official",
    detail: "downloaded signed dmg, opened with Gatekeeper on",
    signingIdentity: "Developer ID Application: Example Corp (ABCDE12345)",
    notarized: true,
  };
  const cases = catalogue.map((spec) => {
    const evidence = [{ kind: "command", detail: `just acceptance ${spec.id}` }];
    if (spec.requires.includes("artifact")) {
      evidence.push({ kind: "artifact", detail: "sha256 manifest compared" });
    }
    if (spec.requires.includes("remote")) {
      evidence.push({ kind: "remote", detail: "two-host run", remoteIdentity: remote });
    }
    if (spec.requires.includes("official")) {
      evidence.push(official);
    }
    return { id: spec.id, verdict: PASSING_VERDICT, evidence };
  });
  return { buildKind: "release", cases };
}

test("a complete release report passes", () => {
  const result = validateReport(passingReport(), catalogue);
  assert.deepEqual(result.errors, []);
  assert.equal(result.ok, true);
  assert.equal(result.passed, catalogue.length);
  assert.equal(catalogue.length, 20, "A01–A20 must be exactly 20 cases");
});

test("the catalogue covers A01..A20 exactly once, in order", () => {
  const ids = catalogue.map((entry) => entry.id);
  const expected = Array.from({ length: 20 }, (_, i) => `A${String(i + 1).padStart(2, "0")}`);
  assert.deepEqual(ids, expected);
});

// —— 计划点名的两处变异 ——

test("mutation: deleting the whole A07 case blocks the report", () => {
  const report = passingReport();
  report.cases = report.cases.filter((entry) => entry.id !== "A07");
  const result = validateReport(report, catalogue);
  assert.equal(result.ok, false);
  assert.equal(result.passed, 19);
  assert.ok(
    result.errors.some((error) => error.startsWith("A07:") && /missing/.test(error)),
    `expected a missing-A07 error, got ${JSON.stringify(result.errors)}`,
  );
});

test("mutation: A10 passing without any evidence blocks the report", () => {
  const report = passingReport();
  const target = report.cases.find((entry) => entry.id === "A10");
  target.evidence = [];
  const result = validateReport(report, catalogue);
  assert.equal(result.ok, false);
  assert.equal(result.passed, 19);
  assert.ok(
    result.errors.some((error) => error.startsWith("A10:") && /evidence/.test(error)),
    `expected an A10 evidence error, got ${JSON.stringify(result.errors)}`,
  );
});

// —— 少写 / 重写 / 含糊 ——

test("a missing case is reported by id", () => {
  const report = passingReport();
  report.cases = report.cases.filter((entry) => entry.id !== "A16");
  const result = validateReport(report, catalogue);
  assert.equal(result.passed, 19);
  assert.ok(result.errors.some((error) => error.startsWith("A16:")));
});

test("the same id twice is rejected even when both pass", () => {
  const report = passingReport();
  report.cases.push(caseEntry("A01"));
  const result = validateReport(report, catalogue);
  assert.equal(result.ok, false);
  assert.ok(/A01: appears 2 times/.test(result.errors.join("\n")));
});

test("an unknown id is rejected instead of being counted", () => {
  const report = passingReport();
  report.cases.push(caseEntry("A21"));
  const result = validateReport(report, catalogue);
  assert.equal(result.ok, false);
  assert.ok(/A21: not a known acceptance id/.test(result.errors.join("\n")));
});

test("every non-pass verdict is refused, including skipped and timeout", () => {
  for (const verdict of ["fail", "skipped", "unsupported", "timeout", "error", "aborted"]) {
    const report = passingReport();
    report.cases.find((entry) => entry.id === "A07").verdict = verdict;
    const result = validateReport(report, catalogue);
    assert.equal(result.ok, false, `${verdict} must not pass`);
    assert.ok(
      result.errors.some((error) => error.startsWith("A07:")),
      `${verdict} should be reported against A07`,
    );
  }
});

test("a misspelt verdict is refused rather than silently treated as unknown", () => {
  const report = passingReport();
  report.cases.find((entry) => entry.id === "A07").verdict = "Passed";
  const result = validateReport(report, catalogue);
  assert.equal(result.ok, false);
  assert.ok(/not the only passing value/.test(result.errors.join("\n")));
});

// —— 证据形态：不能只有一个标题 ——

test("bare string evidence is refused", () => {
  const report = passingReport();
  report.cases.find((entry) => entry.id === "A07").evidence = ["we tested it"];
  const result = validateReport(report, catalogue);
  assert.equal(result.ok, false);
  assert.ok(/not bare strings/.test(result.errors.join("\n")));
});

test("evidence without a detail is refused", () => {
  const report = passingReport();
  report.cases.find((entry) => entry.id === "A07").evidence = [{ kind: "command" }];
  const result = validateReport(report, catalogue);
  assert.equal(result.ok, false);
  assert.ok(/non-empty "detail"/.test(result.errors.join("\n")));
});

test("a case that needs artifact evidence is refused when it only has a command", () => {
  const report = passingReport();
  report.cases.find((entry) => entry.id === "A03").evidence = [
    { kind: "command", detail: "just acceptance A03" },
  ];
  const result = validateReport(report, catalogue);
  assert.equal(result.ok, false);
  assert.ok(/A03: requires "artifact" evidence/.test(result.errors.join("\n")));
});

// —— probe 构建不得冒充正式包 ——

test("a probe build cannot pass a case that needs an official artifact", () => {
  const report = passingReport();
  report.buildKind = "probe";
  const result = validateReport(report, catalogue);
  assert.equal(result.ok, false);
  assert.ok(
    result.errors.some(
      (error) => error.startsWith("A20:") && /official signed artifact/.test(error),
    ),
    `expected A20 to refuse a probe build, got ${JSON.stringify(result.errors)}`,
  );
});

test("a probe build still reports honestly on the other cases", () => {
  // probe 构建可以把 A01..A19 记成 pass —— 只是 A20 必须 unsupported。确认
  // 校验器不是「一见 probe 就全否」，那会让本机报告失去价值。
  const report = passingReport();
  report.buildKind = "probe";
  report.cases.find((entry) => entry.id === "A20").verdict = "unsupported";
  const result = validateReport(report, catalogue);
  // A20 未通过，所以整体不 ok；但通过的应是 19 条。
  assert.equal(result.ok, false);
  assert.equal(result.passed, 19);
  assert.deepEqual(
    result.errors.filter((error) => !error.startsWith("A20:")),
    [],
    "no case other than A20 should be blamed",
  );
});

test("an official evidence entry without notarization is refused", () => {
  const report = passingReport();
  const target = report.cases.find((entry) => entry.id === "A20");
  target.evidence = [
    { kind: "command", detail: "just acceptance A20" },
    {
      kind: "official",
      detail: "signed",
      signingIdentity: "Developer ID Application: Example Corp (ABCDE12345)",
      notarized: false,
    },
  ];
  const result = validateReport(report, catalogue);
  assert.equal(result.ok, false);
  assert.ok(/needs notarized: true/.test(result.errors.join("\n")));
});

test("official evidence without a signing identity is refused", () => {
  const report = passingReport();
  const target = report.cases.find((entry) => entry.id === "A20");
  target.evidence = [
    { kind: "command", detail: "just acceptance A20" },
    { kind: "official", detail: "signed", notarized: true },
  ];
  const result = validateReport(report, catalogue);
  assert.equal(result.ok, false);
  assert.ok(/needs signingIdentity/.test(result.errors.join("\n")));
});

// —— 远端证据不得用 loopback 冒充 ——

test("A16 with only loopback remote evidence is refused", () => {
  for (const hostId of ["localhost", "127.0.0.1", "127.0.0.5:9000", "[::1]:8080"]) {
    const report = passingReport();
    const target = report.cases.find((entry) => entry.id === "A16");
    target.evidence = [
      { kind: "command", detail: "just acceptance A16" },
      {
        kind: "remote",
        detail: "two processes on this machine",
        remoteIdentity: { hostId, os: "macOS 15.2", arch: "aarch64", version: "1.9.0" },
      },
    ];
    const result = validateReport(report, catalogue);
    assert.equal(result.ok, false, `${hostId} must not prove remote behaviour`);
    assert.ok(
      result.errors.some((error) => error.startsWith("A16:") && /loopback/.test(error)),
      `${hostId}: expected a loopback error, got ${JSON.stringify(result.errors)}`,
    );
  }
});

test("A17 without any remote identity is refused", () => {
  const report = passingReport();
  const target = report.cases.find((entry) => entry.id === "A17");
  target.evidence = [
    { kind: "command", detail: "just acceptance A17" },
    { kind: "remote", detail: "assumed networked" },
  ];
  const result = validateReport(report, catalogue);
  assert.equal(result.ok, false);
  assert.ok(/needs remoteIdentity/.test(result.errors.join("\n")));
});

test("a remote identity missing a field is refused", () => {
  const report = passingReport();
  const target = report.cases.find((entry) => entry.id === "A16");
  target.evidence = [
    { kind: "command", detail: "just acceptance A16" },
    {
      kind: "remote",
      detail: "cross machine",
      remoteIdentity: { hostId: "runner-b.internal.example", os: "macOS 15.2" },
    },
  ];
  const result = validateReport(report, catalogue);
  assert.equal(result.ok, false);
  assert.ok(/remoteIdentity\.arch/.test(result.errors.join("\n")));
});

test("A16 passing with no remote evidence at all is refused", () => {
  const report = passingReport();
  const target = report.cases.find((entry) => entry.id === "A16");
  target.evidence = [{ kind: "command", detail: "just acceptance A16" }];
  const result = validateReport(report, catalogue);
  assert.equal(result.ok, false);
  assert.ok(/requires remote evidence/.test(result.errors.join("\n")));
});

// —— 报告整体形态 ——

test("a non-object report is refused", () => {
  for (const value of [null, [], "report", 42]) {
    const result = validateReport(value, catalogue);
    assert.equal(result.ok, false);
  }
});

test("a report without a buildKind is refused", () => {
  const report = passingReport();
  delete report.buildKind;
  const result = validateReport(report, catalogue);
  assert.equal(result.ok, false);
  assert.ok(/buildKind/.test(result.errors.join("\n")));
});

test("a report with a bogus buildKind is refused", () => {
  const report = passingReport();
  report.buildKind = "whatever-ci-built";
  const result = validateReport(report, catalogue);
  assert.equal(result.ok, false);
  assert.ok(/buildKind/.test(result.errors.join("\n")));
});

test("a report without a cases array is refused", () => {
  const result = validateReport({ buildKind: "release" }, catalogue);
  assert.equal(result.ok, false);
  assert.ok(/cases must be an array/.test(result.errors.join("\n")));
});
