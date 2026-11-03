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

// ---------------------------------------------------------------------------
// T04 / G2：发布信任与生命周期
//
// 这一组的判据比报文结构更强：它要证明「改一个字节就验不过」。因此不能只
// 断言 schema 拒绝，必须真的用 Ed25519 验签——schema 校验通过但签名不符，
// 才是这里最危险的漏洞（一个只会结构校验的检查器会把篡改过的包判为合格）。
// ---------------------------------------------------------------------------

test("a signature over the canonical payload verifies", async () => {
  const { buildRootEnvelope, buildReleaseEnvelope, verifyEnvelope } = await import("./sign-contracts.mjs");
  const root = await buildRootEnvelope();
  const release = await buildReleaseEnvelope({ release: fixture("release-v1.json").release });
  assert.equal(verifyEnvelope(root).ok, true, JSON.stringify(verifyEnvelope(root).errors));
  assert.equal(verifyEnvelope(release).ok, true, JSON.stringify(verifyEnvelope(release).errors));
});

test("flipping one payload byte breaks the signature", async () => {
  const { buildReleaseEnvelope, verifyEnvelope, decodePayload } = await import("./sign-contracts.mjs");
  const release = await buildReleaseEnvelope({ release: fixture("release-v1.json").release });
  const payload = decodePayload(release);
  payload[Math.floor(payload.length / 2)] ^= 1;
  const { reencodeEnvelopeWithPayload } = await import("./sign-contracts.mjs");
  const tampered = reencodeEnvelopeWithPayload(release, payload);
  const result = verifyEnvelope(tampered);
  assert.equal(result.ok, false);
  assert.ok(result.errors.some((e) => /signature|integrity/i.test(e)), JSON.stringify(result.errors));
});

test("the signature covers every field the design requires", async () => {
  // 设计 §5：签名绑定 ID、版本、平台、包摘要、大小、兼容范围及发布序号。
  // 少绑一个字段就意味着那个字段可以被单独篡改而不破坏签名。
  const { requiredReleaseFields } = await import("./sign-contracts.mjs");
  const fields = requiredReleaseFields();
  for (const needed of [
    "plugin", "version", "target", "sha256", "sizeBytes",
    "hostApi", "serial", "publishedAt", "expiresAt",
  ]) {
    assert.ok(fields.includes(needed), `signature payload is missing ${needed}: ${fields.join(",")}`);
  }
});

test("an expired release is rejected even with a valid signature", async () => {
  const { buildReleaseEnvelope, verifyWithClock } = await import("./sign-contracts.mjs");
  const release = await buildReleaseEnvelope({ release: fixture("release-v1.json").release });
  const result = verifyWithClock(release, { now: 2_000_000_000 });
  assert.equal(result.ok, false);
  assert.ok(result.errors.some((e) => /expired/i.test(e)), JSON.stringify(result.errors));
});

test("a release published in the future is rejected", async () => {
  // 时钟回退会让「尚未发布」的包看起来有效；这类拒绝必须显式，不能靠签名。
  const { buildReleaseEnvelope, verifyWithClock } = await import("./sign-contracts.mjs");
  const release = await buildReleaseEnvelope({ release: fixture("release-v1.json").release });
  const result = verifyWithClock(release, { now: 1_600_000_000 });
  assert.equal(result.ok, false);
  assert.ok(result.errors.some((e) => /not.?before|future/i.test(e)), JSON.stringify(result.errors));
});

test("a serial lower than the accepted one is rejected as a replay", async () => {
  const { buildReleaseEnvelope, verifyWithClock } = await import("./sign-contracts.mjs");
  const release = await buildReleaseEnvelope({ release: fixture("release-v1.json").release });
  const result = verifyWithClock(release, { now: 1_760_000_000, acceptedSerial: 99 });
  assert.equal(result.ok, false);
  assert.ok(result.errors.some((e) => /serial|replay|rollback/i.test(e)), JSON.stringify(result.errors));
});

test("an unknown signing key is rejected", async () => {
  const { buildReleaseEnvelope, verifyEnvelope } = await import("./sign-contracts.mjs");
  const release = await buildReleaseEnvelope({ release: fixture("release-v1.json").release });
  const result = verifyEnvelope(release, { rootKeyId: "root-unknown" });
  assert.equal(result.ok, false);
  assert.ok(result.errors.some((e) => /unknown|key/i.test(e)), JSON.stringify(result.errors));
});

test("a root envelope cannot bootstrap itself", async () => {
  // bootstrap 只接受宿主内置根；首次网络返回的 root 自签不能成为新锚，
  // 否则信任根可以被任意替换。
  const { buildRootEnvelope, bootstrapTrust } = await import("./sign-contracts.mjs");
  const selfSigned = await buildRootEnvelope({ keyId: "root-intruder" });
  const result = bootstrapTrust(selfSigned, { pinnedKeyIds: ["root-2026-01"] });
  assert.equal(result.ok, false);
  assert.ok(result.errors.some((e) => /untrusted|pinned|root/i.test(result.errors.join(" "))), JSON.stringify(result.errors));
});

test("a root rotation must be authorized by the existing root", async () => {
  const { buildRootEnvelope, rotateRoot } = await import("./sign-contracts.mjs");
  const current = await buildRootEnvelope();
  const next = await buildRootEnvelope({ keyId: "root-2027-01", serial: 2 });
  // 换根本身必须带旧根签名；没有旧根授权的换根是攻击。
  const unauthorized = { ...next, authorizedBy: undefined };
  const result = rotateRoot(current, unauthorized);
  assert.equal(result.ok, false);
  assert.ok(result.errors.some((e) => /authoriz|previous|rotat/i.test(result.errors.join(" "))), JSON.stringify(result.errors));
});

// ---------------------------------------------------------------------------
// 生命周期：候选版本不可被普通 binding 使用，迁移失败不得部分恢复
// ---------------------------------------------------------------------------

test("a candidate release is not usable through a normal binding", () => {
  const trace = {
    binding: { plugin: "example-counter", project: "project-1", grant: 1, activation: 1 },
    requests: [{ requestId: "r-1", method: "counters/increment", params: { by: 1 } }],
  };
  const result = checkTrace(contracts, trace, { candidateActivation: 1 });
  assert.equal(result.ok, false);
  assert.ok(result.errors.some((e) => /candidate/i.test(e)), JSON.stringify(result.errors));
});

test("a failed migration does not restore admission for any project", async () => {
  const { evaluateMigration } = await import("./sign-contracts.mjs");
  const result = evaluateMigration([
    { project: "project-1", ok: true },
    { project: "project-2", ok: false, reason: "format" },
  ]);
  assert.equal(result.admittedProjects.length, 0);
  assert.equal(result.outcome, "recovery_required");
});

test("every persisted phase declares its recovery action", async () => {
  const { recoveryTable } = await import("./sign-contracts.mjs");
  const table = recoveryTable();
  const phases = [
    "draining", "drained", "backing_up", "migrating",
    "health_checking", "committing", "projecting", "done",
  ];
  for (const phase of phases) {
    assert.ok(table[phase], `no recovery action for phase ${phase}`);
    assert.ok(
      typeof table[phase].nextAction === "string" && table[phase].nextAction.length > 0,
      `phase ${phase} has no nextAction`,
    );
    assert.ok(
      typeof table[phase].rollback === "boolean",
      `phase ${phase} does not state whether it can roll back`,
    );
    assert.ok(
      typeof table[phase].admissionOpen === "boolean",
      `phase ${phase} does not state whether admission may be open`,
    );
  }
  // 恢复先于准入：除 done 外，任何阶段的准入都必须是关闭的。
  for (const phase of phases.filter((p) => p !== "done")) {
    assert.equal(table[phase].admissionOpen, false, `phase ${phase} must keep admission closed`);
  }
});

test("rewriting a field and recomputing the payload still fails the signature", async () => {
  // 这条是签名验证的**唯一**独立证据。上一条（flipping one byte）会被
  // "payload 与字段一致性"检查拦下，因此即使签名验证被整个删掉它也照样绿。
  //
  // 真正的攻击形态是：改掉 release 字段，同时重算 payload 让两者一致，
  // 但签名仍是旧的。此时只有 Ed25519 验证能发现。没有这条测试，
  // "verify() 永远返回 true" 这种致命改动不会被任何断言发现。
  const { buildReleaseEnvelope, verifyEnvelope, encodeReleasePayload } = await import("./sign-contracts.mjs");
  const release = fixture("release-v1.json").release;
  const envelope = await buildReleaseEnvelope({ release });

  const forged = { ...envelope, release: { ...release, sizeBytes: release.sizeBytes + 1 } };
  forged.payloadB64 = encodeReleasePayload(forged.release).toString("base64");

  // 一致性检查必须通过（否则测不到签名）。
  assert.notEqual(forged.payloadB64, envelope.payloadB64);
  const result = verifyEnvelope(forged);
  assert.equal(result.ok, false);
  assert.ok(
    result.errors.some((e) => /signature/i.test(e)),
    `the signature must be what rejects this, got: ${JSON.stringify(result.errors)}`,
  );
});

test("a valid signature does not authorize an unknown root key", async () => {
  // 签名有效但签发者不在信任集里，也必须拒绝——这是"下载端不能选择公钥"。
  const { buildReleaseEnvelope, verifyEnvelope } = await import("./sign-contracts.mjs");
  const envelope = await buildReleaseEnvelope({ release: fixture("release-v1.json").release });
  assert.equal(verifyEnvelope(envelope).ok, true);
  const strict = verifyEnvelope(envelope, { pinnedKeyIds: ["root-2027-01"] });
  assert.equal(strict.ok, false);
});

test("the root, release and lifecycle fixtures satisfy their schemas", () => {
  for (const [kind, name] of [
    ["root", "root-v1.json"],
    ["release", "release-v1.json"],
    ["lifecycle", "lifecycle-v1.json"],
  ]) {
    const result = checkDocument(contracts, kind, fixture(name));
    assert.equal(result.ok, true, `${name}: ${JSON.stringify(result.errors)}`);
  }
});

test("a lifecycle journal entry naming another plugin is rejected", () => {
  // 恢复时会把 A 的进度套到 B 上，这是最危险的一类记账错误。
  const doc = fixture("lifecycle-v1.json");
  doc.target.release.plugin = "some-other-plugin";
  rejects("lifecycle", doc, /plugin/i);
});

test("a rollback without a previous activation is rejected", () => {
  const doc = fixture("lifecycle-v1.json");
  doc.action = "Rollback";
  doc.previous = null;
  rejects("lifecycle", doc, /previous|rollback/i);
});

test("a rollback whose previous activation is not lower is rejected", () => {
  const doc = fixture("lifecycle-v1.json");
  doc.action = "Rollback";
  doc.previous = { activation: doc.target.activation, version: "1.4.0" };
  rejects("lifecycle", doc, /previous|activation/i);
});

test("a completed transaction carrying an error is rejected", () => {
  const doc = fixture("lifecycle-v1.json");
  doc.phase = "done";
  doc.error = { kind: "Dependency", message: "should have been resolved" };
  rejects("lifecycle", doc, /error|completed/i);
});

test("an unknown lifecycle phase is rejected", () => {
  const doc = fixture("lifecycle-v1.json");
  doc.phase = "teleporting";
  rejects("lifecycle", doc, /phase|enum/i);
});

test("a root envelope with a short public key is rejected", () => {
  const doc = fixture("root-v1.json");
  doc.root.publicKey = Buffer.alloc(16).toString("base64");
  rejects("root", doc, /publicKey|pattern/i);
});
