// `e2e.mjs` 纯决策层的自测（计划 T27 缺失执行器的第一半）。
//
// 这里全部是「会不会把不该通过的东西算成通过」的反例为主：一个执行器的价值
// 不在于它能跑命令，而在于它不会在命令没跑的时候假装跑过。

import assert from "node:assert/strict";
import { test } from "node:test";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

import {
  loadCommandMap,
  planSuites,
  buildEvidenceEntry,
  verdictFor,
  assembleReport,
  requiresPassExitCode,
  unsupportedReason,
  parseCommand,
  runCommand,
  runCase,
  runSelected,
  detectCapabilities,
  parseArgs,
} from "./e2e.mjs";
import { loadCatalogue, validateReport } from "./e2e-report.mjs";

const catalogue = loadCatalogue();
const map = loadCommandMap();

/// 写一份临时映射文件，供「该被拒绝」的用例使用。
function writeMap(contents) {
  const dir = mkdtempSync(path.join(tmpdir(), "e2e-map-"));
  const file = path.join(dir, "acceptance-commands.json");
  writeFileSync(file, JSON.stringify(contents));
  return file;
}

/// 一份合法映射：每条命令都指向一个不存在的动作，只用于结构测试。
function mapWith(cases) {
  return writeMap({ schemaVersion: 1, note: "test", cases });
}

test("the command map covers exactly the catalogue ids", () => {
  assert.deepEqual(
    map.cases.map((entry) => entry.id),
    catalogue.map((entry) => entry.id),
    "the command map must bind every acceptance id, in catalogue order",
  );
});

test("every bound command names an evidence kind the validator understands", () => {
  for (const entry of map.cases) {
    for (const command of entry.commands) {
      assert.ok(
        ["command", "artifact", "remote", "official"].includes(command.kind),
        `${entry.id} uses an evidence kind the validator would not understand`,
      );
      assert.ok(command.run.trim() !== "", `${entry.id} has a blank command`);
    }
  }
});

test("a duplicate id in the map is rejected", () => {
  const file = mapWith([
    { id: "A01", commands: [{ kind: "command", run: "true" }], unsupportedWithout: null },
    { id: "A01", commands: [{ kind: "command", run: "true" }], unsupportedWithout: null },
  ]);
  assert.throws(() => loadCommandMap(file), /appears more than once/);
});

test("a case with no commands is rejected", () => {
  const file = mapWith([
    { id: "A01", commands: [], unsupportedWithout: null },
  ]);
  assert.throws(() => loadCommandMap(file), /must list at least one command/);
});

test("a map that omits an acceptance id is rejected", () => {
  // 只写 A01，其余 19 条都不存在。
  const file = mapWith([
    { id: "A01", commands: [{ kind: "command", run: "true" }], unsupportedWithout: null },
    { id: "A02", commands: [{ kind: "command", run: "true" }], unsupportedWithout: null },
  ]);
  assert.throws(() => loadCommandMap(file), /no command is bound for/);
});

test("a map naming an unknown evidence kind is rejected", () => {
  const file = writeMap({
    schemaVersion: 1,
    note: "test",
    cases: [
      { id: "A01", commands: [{ kind: "vibes", run: "true" }], unsupportedWithout: null },
    ],
  });
  assert.throws(() => loadCommandMap(file), /unknown evidence kind/);
});

test("planSuites selects all cases in catalogue order", () => {
  const selected = planSuites(map, "all");
  assert.equal(selected.length, 20);
  assert.deepEqual(
    selected.map((entry) => entry.id),
    catalogue.map((entry) => entry.id),
  );
});

test("planSuites narrows to one id and rejects an unknown one", () => {
  assert.deepEqual(
    planSuites(map, "A16").map((entry) => entry.id),
    ["A16"],
  );
  assert.deepEqual(
    planSuites(map, "A19,A02").map((entry) => entry.id),
    ["A02", "A19"],
    "selection order follows the catalogue, not the argument order",
  );
  assert.throws(() => planSuites(map, "A99"), /unknown acceptance id/);
});

test("planSuites rejects an empty suite instead of selecting everything", () => {
  // 空 suite 静默等于 "all" 会让一次漏参数变成一次完整跑，代价很大。
  assert.throws(() => planSuites(map, ""), /needs a value/);
});

test("a missing outcome is a failure, never a skip", () => {
  const spec = { id: "A01", unsupportedWithout: null };
  assert.equal(verdictFor({ caseSpec: spec, outcome: "missing" }), "fail");
  assert.equal(verdictFor({ caseSpec: spec, outcome: "fail" }), "fail");
  assert.equal(verdictFor({ caseSpec: spec, outcome: "pass" }), "pass");
});

test("a remote case is unsupported only when no remote capability was provided", () => {
  const spec = { id: "A16", unsupportedWithout: "remote" };
  assert.equal(
    verdictFor({ caseSpec: spec, outcome: "missing", capabilities: { remote: false } }),
    "unsupported",
  );
  assert.equal(
    verdictFor({ caseSpec: spec, outcome: "missing", capabilities: {} }),
    "unsupported",
    "an absent capability counts as not provided",
  );
  assert.equal(
    verdictFor({ caseSpec: spec, outcome: "missing", capabilities: { remote: true } }),
    "fail",
    "with a remote host available, not running is a failure",
  );
  assert.equal(
    verdictFor({ caseSpec: spec, outcome: "fail", capabilities: { remote: true } }),
    "fail",
    "a remote test that ran and failed is never softened into unsupported",
  );
});

test("a case without unsupportedWithout can never become unsupported", () => {
  const spec = { id: "A05", unsupportedWithout: null };
  assert.equal(
    verdictFor({ caseSpec: spec, outcome: "missing", capabilities: { remote: false } }),
    "fail",
  );
});

test("an official case degrades to unsupported without an official build only", () => {
  const spec = { id: "A20", unsupportedWithout: "official" };
  assert.equal(
    verdictFor({ caseSpec: spec, outcome: "missing", capabilities: {} }),
    "unsupported",
  );
  assert.equal(
    verdictFor({ caseSpec: spec, outcome: "missing", capabilities: { official: true } }),
    "fail",
  );
});

test("unsupported reasons name what is missing", () => {
  assert.match(unsupportedReason({ unsupportedWithout: "remote" }), /remote host/);
  assert.match(unsupportedReason({ unsupportedWithout: "official" }), /official/);
});

test("buildEvidenceEntry omits absent optional fields and rejects a blank detail", () => {
  const entry = buildEvidenceEntry({ kind: "command", detail: "true" });
  assert.deepEqual(entry, { kind: "command", detail: "true" });
  assert.ok(!("remoteIdentity" in entry), "absent remoteIdentity must not be written as null");

  assert.throws(() => buildEvidenceEntry({ kind: "command", detail: "" }), /non-empty detail/);
  assert.throws(() => buildEvidenceEntry({ kind: "guess", detail: "x" }), /evidence kind/);
});

test("an assembled report has the shape the validator expects", () => {
  const results = catalogue.map((spec) => ({
    caseSpec: spec,
    outcome: "missing",
    evidence: [],
  }));
  const report = assembleReport({
    buildKind: "probe",
    suite: "all",
    results,
    capabilities: {},
    now: "2026-11-05T00:00:00Z",
  });

  assert.equal(report.buildKind, "probe");
  assert.equal(report.cases.length, 20);

  // 校验器会因为这些判词不是 pass 而报错，这是预期的。这里要断言的是**形状**
  // 没问题：不缺 id、不重复、不出现未知 id、证据结构合规。
  const outcome = validateReport(report, catalogue);
  const shapeErrors = outcome.errors.filter((error) =>
    /missing from the report|appears \d+ times|not a known acceptance id|evidence entry|every report case/.test(
      error,
    ),
  );
  assert.deepEqual(shapeErrors, [], `shape problems: ${shapeErrors.join("; ")}`);
});

test("an assembled report never contains the word skipped", () => {
  const results = catalogue.map((spec) => ({ caseSpec: spec, outcome: "missing", evidence: [] }));
  const report = assembleReport({
    buildKind: "probe",
    results,
    capabilities: {},
    now: "2026-11-05T00:00:00Z",
  });
  for (const entry of report.cases) {
    assert.ok(
      ["pass", "fail", "unsupported"].includes(entry.verdict),
      `${entry.id} used a verdict outside the vocabulary: ${entry.verdict}`,
    );
  }
});

test("requiresPassExitCode is 0 only when everything passed", () => {
  const all = { cases: [{ id: "A01", verdict: "pass" }] };
  assert.equal(requiresPassExitCode(all), 0);
  assert.notEqual(
    requiresPassExitCode({ cases: [{ id: "A01", verdict: "pass" }, { id: "A02", verdict: "fail" }] }),
    0,
  );
  assert.notEqual(
    requiresPassExitCode({ cases: [{ id: "A16", verdict: "unsupported" }] }),
    0,
    "unsupported is not a pass",
  );
});

// —— 任务 2：执行层 ——
//
// 执行层的风险与决策层相反：决策层怕「把没跑算成跑了」，执行层怕「把环境问题
// 说成实现失败」和「把跑失败说成没跑」。两种误判都会让人去修错的地方。

test("parseCommand splits an executable, its args, and leading env assignments", () => {
  assert.deepEqual(parseCommand("python3 scripts/plugins/check-host-boundary.py"), {
    command: "python3",
    args: ["scripts/plugins/check-host-boundary.py"],
    env: {},
  });
  assert.deepEqual(parseCommand('FOO=1 BAR="a b" cargo nextest run'), {
    command: "cargo",
    args: ["nextest", "run"],
    env: { FOO: "1", BAR: "a b" },
  });
  assert.throws(() => parseCommand(""), /non-empty/);
});

test("runCommand reports a non-zero exit as fail, not missing", () => {
  const runner = () => {
    const error = new Error("Command failed");
    error.status = 3;
    error.stdout = "out";
    error.stderr = "err";
    throw error;
  };
  const result = runCommand("false", { runner });
  assert.equal(result.outcome, "fail");
  assert.equal(result.exitCode, 3);
});

test("runCommand reports a missing executable as missing, not fail", () => {
  const runner = () => {
    const error = new Error("spawnSync nonexistent ENOENT");
    error.code = "ENOENT";
    throw error;
  };
  const result = runCommand("nonexistent-binary", { runner });
  assert.equal(result.outcome, "missing");
  assert.match(result.reason, /executable not found/);
});

test("runCommand reports a kill signal as missing", () => {
  const runner = () => {
    const error = new Error("killed");
    error.signal = "SIGKILL";
    throw error;
  };
  assert.equal(runCommand("hang", { runner }).outcome, "missing");
});

test("runCommand passes success through as pass", () => {
  const result = runCommand("true", { runner: () => "ok" });
  assert.equal(result.outcome, "pass");
  assert.equal(result.stdout, "ok");
});

test("detectCapabilities only counts explicit configuration", () => {
  assert.deepEqual(detectCapabilities({ env: {} }), {
    remote: false,
    remoteHost: "",
    official: false,
  });
  const withHost = detectCapabilities({ env: {}, flags: { remoteHost: "build-box.internal" } });
  assert.equal(withHost.remote, true);
  assert.equal(withHost.remoteHost, "build-box.internal");
  assert.equal(detectCapabilities({ env: { YI_AGENT_ACCEPTANCE_OFFICIAL: "1" } }).official, true);
  assert.equal(
    detectCapabilities({ env: { YI_AGENT_ACCEPTANCE_OFFICIAL: "0" } }).official,
    false,
    "any value other than 1 must not claim an official build",
  );
});

test("runCase folds several commands: one failure fails the case", () => {
  const caseSpec = {
    id: "A09",
    unsupportedWithout: null,
    commands: [
      { kind: "command", run: "one" },
      { kind: "command", run: "two" },
    ],
  };
  let call = 0;
  const runner = () => {
    call += 1;
    if (call === 2) {
      const error = new Error("boom");
      error.status = 1;
      throw error;
    }
    return "";
  };
  const result = runCase(caseSpec, { runner });
  assert.equal(result.outcome, "fail");
});

test("runCase collects evidence only from commands that actually passed", () => {
  const caseSpec = {
    id: "A01",
    unsupportedWithout: null,
    commands: [
      { kind: "command", run: "ok" },
      { kind: "artifact", run: "bad" },
    ],
  };
  let call = 0;
  const runner = () => {
    call += 1;
    if (call === 2) {
      const error = new Error("boom");
      error.status = 1;
      throw error;
    }
    return "";
  };
  const result = runCase(caseSpec, { runner });
  assert.equal(result.evidence.length, 1);
  assert.equal(result.evidence[0].detail, "ok");
});

test("an unsupported case runs nothing and records why", () => {
  // 没有远端主机时，A16 的命令必须**一次都不跑**：硬跑一条注定失败的命令会
  // 把「缺一台机器」写成「远端实现失败」，读报告的人会去修错的地方。
  const map = loadCommandMap();
  const a16 = planSuites(map, "A16")[0];
  let ran = 0;
  const results = runSelected([a16], {
    capabilities: { remote: false },
    runner: () => {
      ran += 1;
      return "";
    },
  });
  assert.equal(ran, 0, "a case whose capability is absent must not execute its command");
  assert.equal(results[0].outcome, "missing");
  assert.equal(results[0].skippedBecause, "remote");

  const report = assembleReport({
    buildKind: "probe",
    results,
    capabilities: { remote: false },
    now: "2026-11-05T00:00:00Z",
  });
  assert.equal(report.cases[0].verdict, "unsupported");
  assert.match(report.cases[0].reason, /remote host/);
});

test("a case with its capability present does run and can fail honestly", () => {
  // 反向钉住上一条：能力在的时候必须真跑。否则「不跑」会变成绕过验收的后门。
  const map = loadCommandMap();
  const a16 = planSuites(map, "A16")[0];
  let ran = 0;
  const results = runSelected([a16], {
    capabilities: { remote: true },
    runner: () => {
      ran += 1;
      const error = new Error("remote run failed");
      error.status = 1;
      throw error;
    },
  });
  assert.equal(ran, 1, "with a remote host available the command must actually run");
  const report = assembleReport({
    buildKind: "probe",
    results,
    capabilities: { remote: true },
    now: "2026-11-05T00:00:00Z",
  });
  assert.equal(report.cases[0].verdict, "fail", "a real failure must not be softened to unsupported");
});

test("parseArgs rejects an unknown build kind and unknown flags", () => {
  assert.throws(() => parseArgs(["--build", "signed"]), /must be probe or release/);
  assert.throws(() => parseArgs(["--nope"]), /unknown argument/);
  assert.deepEqual(parseArgs(["--suite", "A01", "--require-pass"]).requirePass, true);
});
