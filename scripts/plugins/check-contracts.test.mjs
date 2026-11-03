// Contract checker 的行为测试。
//
// 这一层刻意不用「schema 能过」当唯一判据：计划 T03 Step 2 明确要求
// **跨请求的行为**（重复 request ID、生命周期顺序）不能声称由单个 JSON
// schema 证明，必须由 checker 里的显式 trace validator 验证。
//
// 所以这里的测试分两类：
//   1. 结构断言（schema 该拒的拒、该过的过）；
//   2. 行为断言（黄金样本通过；篡改任一处必须红）。
//
// 每个「必须拒绝」的样本都写成**从黄金样本派生一个最小改动**，而不是手写
// 一份独立坏 JSON。这样测试同时保证了「黄金样本本身是合法的」——否则一个
// 全拒绝的 checker 也能让所有负例变绿。

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { loadContracts, checkDocument, checkTrace } from "./check-contracts.mjs";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(HERE, "..", "..");
const FIXTURES = path.join(REPO_ROOT, "plugins", "contracts", "fixtures");

function fixture(name) {
  return JSON.parse(readFileSync(path.join(FIXTURES, name), "utf8"));
}

const contracts = loadContracts();

function rejects(kind, doc, pattern) {
  const result = checkDocument(contracts, kind, doc);
  assert.equal(result.ok, false, `expected ${kind} to be rejected, but it passed`);
  assert.ok(result.errors.length > 0, "a rejection must carry at least one reason");
  if (pattern) {
    assert.ok(
      result.errors.some((line) => pattern.test(line)),
      `rejection reasons did not mention ${pattern}: ${JSON.stringify(result.errors)}`,
    );
  }
}

// ---------------------------------------------------------------------------
// 黄金样本：三个 schema 的合法实例必须全部通过。
// ---------------------------------------------------------------------------

test("the golden fixtures all pass", () => {
  for (const [kind, name] of [
    ["manifest", "manifest-v1.json"],
    ["host", "host-v1.json"],
    ["bridge", "bridge-v1.json"],
  ]) {
    const result = checkDocument(contracts, kind, fixture(name));
    assert.equal(
      result.ok,
      true,
      `${name} should be valid but failed: ${JSON.stringify(result.errors)}`,
    );
  }
});

// ---------------------------------------------------------------------------
// 发布清单
// ---------------------------------------------------------------------------

test("an unknown required field is rejected instead of ignored", () => {
  // 发布清单是签名包绑定的权威声明。多出一个宿主不认识的字段，说明
  // 插件比宿主新——必须拒绝，不能"忽略未知字段然后当作兼容"。
  const doc = fixture("manifest-v1.json");
  doc.methods.push({ name: "cards/readAll", effect: "Read", unknownButRequired: true });
  rejects("manifest", doc, /unknown|additional/i);
});

test("an entry that escapes the package root is rejected", () => {
  const doc = fixture("manifest-v1.json");
  doc.views[0].entry = "../../../etc/passwd";
  rejects("manifest", doc, /entry|pattern|path/i);
});

test("a business method cannot self-declare Read", () => {
  // 计划 §3：未知方法不允许由调用方自报为 Read。声明本身必须是白名单，
  // 且 Maintenance 只能由生命周期事务签发，不能作为插件可申请的能力。
  const doc = fixture("manifest-v1.json");
  doc.methods.push({ name: "cards.deleteEverything", effect: "Read" });
  rejects("manifest", doc, /effect|method|unknown/i);
});

test("Maintenance is not an effect a plugin may declare", () => {
  const doc = fixture("manifest-v1.json");
  doc.methods[0].effect = "Maintenance";
  rejects("manifest", doc, /effect|Maintenance/i);
});

test("a manifest without exactly one main view is rejected", () => {
  // 设计 §3.1：导航由插件声明，宿主保留通用外层。零个或两个主视图都会
  // 逼宿主去猜哪一个是入口。
  const none = fixture("manifest-v1.json");
  none.views = none.views.map((v) => ({ ...v, kind: "settings" }));
  const r1 = checkDocument(contracts, "manifest", none);
  assert.equal(r1.ok, false);
  assert.ok(r1.errors.some((l) => /main/i.test(l)), JSON.stringify(r1.errors));

  const two = fixture("manifest-v1.json");
  two.views.push({ ...two.views[0], id: "main2" });
  const r2 = checkDocument(contracts, "manifest", two);
  assert.equal(r2.ok, false);
  assert.ok(r2.errors.some((l) => /main/i.test(l)), JSON.stringify(r2.errors));
});

test("a read/write capability with no backing method is rejected", () => {
  // 申请经桥访问业务后端的读写能力，却没有一个方法能兑现它，启用时给用户
  // 看的授权清单就是空的。Launch/QuotaWrite/Resident 不适用这条：它们是
  // 宿主签发的能力，本来就不对应某个业务方法。
  const doc = fixture("manifest-v1.json");
  doc.methods = doc.methods.map((m) => ({ ...m, effect: "Read" }));
  rejects("manifest", doc, /capabilities|effect/i);

  // 反证：合法的宿主签发能力不该被误拒。
  const hostIssued = fixture("manifest-v1.json");
  hostIssued.capabilities.optional.push("QuotaWrite");
  const result = checkDocument(contracts, "manifest", hostIssued);
  assert.equal(result.ok, true, JSON.stringify(result.errors));
});

test("the same capability cannot be both required and optional", () => {
  const doc = fixture("manifest-v1.json");
  doc.capabilities.optional.push("Read");
  rejects("manifest", doc, /capabilities|both|required|optional/i);
});

test("a migration that skips a format version is rejected", () => {
  // 跳过中间格式意味着某个已存在的数据版本没有升级路径，而设计 §9 不允许
  // 原地猜迁移。
  const doc = fixture("manifest-v1.json");
  doc.data.migrations = [{ from: 1, to: 3, entry: "migrations/1-to-3.sh" }];
  rejects("manifest", doc, /migration|format/i);
});

test("duplicate view ids and duplicate skill names are rejected", () => {
  const dupView = fixture("manifest-v1.json");
  dupView.views[1].id = dupView.views[0].id;
  const r1 = checkDocument(contracts, "manifest", dupView);
  assert.equal(r1.ok, false);
  assert.ok(r1.errors.some((l) => /duplicate view id/i.test(l)), JSON.stringify(r1.errors));

  const dupSkill = fixture("manifest-v1.json");
  dupSkill.skills.push({ ...dupSkill.skills[0] });
  const r2 = checkDocument(contracts, "manifest", dupSkill);
  assert.equal(r2.ok, false);
  assert.ok(r2.errors.some((l) => /duplicate skill/i.test(l)), JSON.stringify(r2.errors));
});

test("a missing required top-level field is rejected", () => {
  const doc = fixture("manifest-v1.json");
  delete doc.views;
  rejects("manifest", doc, /required|views/i);
});

test("an unsupported contract version is rejected", () => {
  const doc = fixture("manifest-v1.json");
  doc.schemaVersion = 99;
  rejects("manifest", doc, /schemaVersion|version|enum/i);
});

// ---------------------------------------------------------------------------
// 宿主描述
// ---------------------------------------------------------------------------

test("an unknown host API version is rejected", () => {
  const doc = fixture("host-v1.json");
  doc.api = "v99";
  rejects("host", doc, /api|enum|version/i);
});

test("a host must declare the reserved environment it owns", () => {
  // 计划要求「声明宿主保留环境变量」。留空等于让插件与宿主抢同一个变量。
  const doc = fixture("host-v1.json");
  doc.reservedEnv = [];
  rejects("host", doc, /reservedEnv|minItems|required/i);
});

// ---------------------------------------------------------------------------
// 桥
// ---------------------------------------------------------------------------

test("a bridge method outside the frozen set is rejected", () => {
  const doc = fixture("bridge-v1.json");
  doc.methods.push({ name: "plugins/v1/evalArbitrary", effect: "Read" });
  rejects("bridge", doc, /method|enum|additional/i);
});

test("the bridge cannot expose a generic rpc passthrough", () => {
  // 设计明确「宿主不提供任意 RPC、任意文件访问或 shell 执行后门」。
  // 这条断言把那个约束钉在协议层，而不是靠 review 记住。
  const doc = fixture("bridge-v1.json");
  doc.methods.push({ name: "plugins/v1/rpc", effect: "Write" });
  rejects("bridge", doc, /method|enum|rpc/i);
});

test("a result union member that forgets its discriminator is rejected", () => {
  const doc = fixture("bridge-v1.json");
  doc.responseVariants = ["pending", "committed", "rejected"];
  rejects("bridge", doc, /responseVariants|unknown|enum/i);
});

// ---------------------------------------------------------------------------
// 行为：跨请求。单个 schema 证明不了这些，必须由 trace validator 判。
// ---------------------------------------------------------------------------

test("a trace that reuses a request id with different params is rejected", () => {
  // 幂等的前提是「同一 request id ⇒ 同一请求」。参数不同却复用 id，
  // 会让重试静默变成另一个操作。
  const trace = {
    binding: { plugin: "example-counter", project: "project-1", grant: 1, activation: 1 },
    requests: [
      { requestId: "r-1", method: "counters/increment", params: { by: 1 } },
      { requestId: "r-1", method: "counters/increment", params: { by: 2 } },
    ],
  };
  const result = checkTrace(contracts, trace);
  assert.equal(result.ok, false);
  assert.ok(result.errors.some((line) => /requestId|params/i.test(line)), JSON.stringify(result.errors));
});

test("a trace that retries an id with identical params is allowed", () => {
  const trace = {
    binding: { plugin: "example-counter", project: "project-1", grant: 1, activation: 1 },
    requests: [
      { requestId: "r-1", method: "counters/increment", params: { by: 1 } },
      { requestId: "r-1", method: "counters/increment", params: { by: 1 } },
    ],
  };
  const result = checkTrace(contracts, trace);
  assert.equal(result.ok, true, JSON.stringify(result.errors));
});

test("a trace that mixes two bindings under one id is rejected", () => {
  // 代次变更后复用旧 request id 会把「旧授权下的结果」当成新授权的结果。
  const trace = {
    binding: { plugin: "example-counter", project: "project-1", grant: 1, activation: 1 },
    requests: [
      {
        requestId: "r-1",
        method: "counters/increment",
        params: { by: 1 },
        binding: { plugin: "example-counter", project: "project-2", grant: 1, activation: 1 },
      },
    ],
  };
  const result = checkTrace(contracts, trace);
  assert.equal(result.ok, false);
  assert.ok(result.errors.some((line) => /binding|project/i.test(line)), JSON.stringify(result.errors));
});

test("the trace validator is not vacuous on an empty trace", () => {
  // 空 trace 不该"通过"——它什么都没验证。这是防止护栏被架空的那一条。
  const result = checkTrace(contracts, { binding: null, requests: [] });
  assert.equal(result.ok, false);
  assert.ok(result.errors.some((line) => /binding|requests/i.test(line)), JSON.stringify(result.errors));
});
