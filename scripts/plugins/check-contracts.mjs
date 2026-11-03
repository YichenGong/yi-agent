// 插件线协议契约检查器。
//
// 这一层是 G1 的可执行部分：schema 冻结「报文长什么样」，trace validator 冻结
// 「一串请求放在一起必须满足什么」。计划 T03 Step 2 明确要求后者不能假装由
// 单个 JSON schema 证明——schema 看不见跨请求的重复 ID 与 binding 冲突，
// 所以这里单独实现。
//
// 依赖解析必须指向 plugins/contracts/package.json：该目录有自己的
// package-lock，根 scripts/ 没有 node_modules，靠 Node 的目录上溯找不到。
// 也不能临时下载未知版本，否则「契约冻结」这句话就没有意义。

import { createRequire } from "node:module";
import { readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO_ROOT = path.resolve(HERE, "..", "..");
const CONTRACTS_DIR = path.join(REPO_ROOT, "plugins", "contracts");

// ajv 只从 plugins/contracts 解析，且必须用它自己的 package.json 作为锚点。
const contractsRequire = createRequire(path.join(CONTRACTS_DIR, "package.json"));

// ajv 默认实例只认 draft-07；三个 schema 都声明 2020-12，必须用 2020 构建。
// 用 require 而不是 import 是为了让上面 createRequire 的锚点真正生效。
//
// 依赖缺失时给出可执行的下一步，而不是一个裸 MODULE_NOT_FOUND——那会让人
// 以为是代码坏了，而实际只是没装插件契约目录的依赖。
function requireContractsDependency(spec) {
  try {
    return contractsRequire(spec);
  } catch (err) {
    const hint =
      `

Cannot resolve "${spec}" from ${path.join(CONTRACTS_DIR, "package.json")}.
` +
      `Contract checks need the pinned dependencies of plugins/contracts.
` +
      `Run: npm ci --prefix plugins/contracts`;
    err.message = `${err.message}${hint}`;
    throw err;
  }
}

const Ajv2020 = requireContractsDependency("ajv/dist/2020.js");
const addFormats = requireContractsDependency("ajv-formats");

const SCHEMA_FILES = {
  manifest: "manifest-v1.schema.json",
  host: "host-v1.schema.json",
  bridge: "bridge-v1.schema.json",
  root: "root-v1.schema.json",
  release: "release-v1.schema.json",
  lifecycle: "lifecycle-v1.schema.json",
};

// 破坏性动词。声明为 Read 的 method / verb 若名字里带这些词，说明声明与语义
// 矛盾；计划要求「未知方法不允许由调用方自报为 Read」，而这里是同一问题的
// 可判定版本：名字自己就说它要写。
//
// 这条规则要写进子 spec（docs/superpowers/specs/2026-11-02-plugin-host-protocol.md），
// 不能只留在这里当魔法字符串。
const DESTRUCTIVE_VERBS = [
  "delete",
  "remove",
  "write",
  "set",
  "update",
  "create",
  "merge",
  "install",
  "uninstall",
  "drain",
  "kill",
  "restart",
  "reset",
  "clear",
  "purge",
  "drop",
  "truncate",
  "replace",
  "move",
  "rename",
  "enable",
  "disable",
  "activate",
  "commit",
  "rollback",
];

function readJSON(file) {
  return JSON.parse(readFileSync(file, "utf8"));
}

export function loadContracts() {
  const ajv = new Ajv2020({ allErrors: true, strict: true, allowUnionTypes: false });
  addFormats(ajv);

  const validators = {};
  for (const [kind, file] of Object.entries(SCHEMA_FILES)) {
    const schema = readJSON(path.join(CONTRACTS_DIR, file));
    validators[kind] = ajv.compile(schema);
  }
  return { validators, ajv };
}

function formatAjvError(err) {
  const where = err.instancePath === "" ? "(root)" : err.instancePath;
  return `${where}: ${err.message}`;
}

function effectOfReadDeclaration(decl, label) {
  const errors = [];
  if (!decl || decl.effect !== "Read") return errors;
  const name = String(decl.name ?? decl.verb ?? "").toLowerCase();
  const hit = DESTRUCTIVE_VERBS.find((verb) => name.includes(verb));
  if (hit) {
    errors.push(
      `${label}: declares effect Read but its name contains the destructive verb "${hit}" ` +
        `— a method/verb may not self-declare Read while naming a write`,
    );
  }
  return errors;
}

function checkManifestSemantics(doc) {
  const errors = [];

  if (Array.isArray(doc.methods)) {
    doc.methods.forEach((m, i) => errors.push(...effectOfReadDeclaration(m, `/methods/${i}`)));
  }
  if (Array.isArray(doc.cli)) {
    doc.cli.forEach((c, i) => errors.push(...effectOfReadDeclaration(c, `/cli/${i}`)));
  }

  // 主视图必须恰好一个：设计 §3.1 要求导航由清单声明，宿主保留通用外层。
  // 零个主视图 → 插件没有可用界面；多个 → 宿主必须猜哪一个是入口。
  const views = Array.isArray(doc.views) ? doc.views : [];
  const mains = views.filter((v) => v?.kind === "main");
  if (views.length > 0 && mains.length !== 1) {
    errors.push(`/views: expected exactly one view of kind "main", found ${mains.length}`);
  }
  const viewIds = new Set();
  views.forEach((v, i) => {
    if (v?.id && viewIds.has(v.id)) {
      errors.push(`/views/${i}: duplicate view id "${v.id}" — view ids must be unambiguous`);
    }
    if (v?.id) viewIds.add(v.id);
  });

  // Read/Write 是"经桥访问业务后端"的能力，必须有方法兑现：申请一个
  // 没有任何方法能提供的读写能力，启用时的授权清单就是空的。
  //
  // Launch / QuotaWrite / Resident 不同：它们是宿主签发的能力，不对应某个
  // 业务方法（设计 §6 把"发起自动任务""修改整机配额""后台常驻"列为独立
  // 确认项）。因此不能要求它们有 backing method，否则合法清单会被误拒。
  const declaredEffects = new Set(
    Array.isArray(doc.methods) ? doc.methods.map((m) => m?.effect) : [],
  );
  const METHOD_BACKED_EFFECTS = ["Read", "Write"];
  const caps = doc.capabilities ?? {};
  const required = Array.isArray(caps.required) ? caps.required : [];
  const optional = Array.isArray(caps.optional) ? caps.optional : [];
  for (const [label, list] of [["required", required], ["optional", optional]]) {
    for (const effect of list) {
      if (METHOD_BACKED_EFFECTS.includes(effect) && !declaredEffects.has(effect)) {
        errors.push(
          `/capabilities/${label}: requests effect "${effect}" but no declared method has that effect`,
        );
      }
    }
  }
  for (const effect of required) {
    if (optional.includes(effect)) {
      errors.push(`/capabilities: effect "${effect}" appears in both required and optional`);
    }
  }

  // 迁移必须从低到高且相邻：跳过中间格式意味着某个已存在的数据版本
  // 没有可用的升级路径，而设计 §9 不允许"原地猜迁移"。
  const migrations = Array.isArray(doc.data?.migrations) ? doc.data.migrations : [];
  migrations.forEach((m, i) => {
    if (!(typeof m?.from === "number" && typeof m?.to === "number")) return;
    if (m.to !== m.from + 1) {
      errors.push(`/data/migrations/${i}: migration must step exactly one format version (from ${m.from} to ${m.from + 1}, got ${m.to})`);
    }
  });

  const skillNames = new Set();
  for (const [i, skill] of (Array.isArray(doc.skills) ? doc.skills : []).entries()) {
    if (skill?.name && skillNames.has(skill.name)) {
      errors.push(`/skills/${i}: duplicate skill name "${skill.name}"`);
    }
    if (skill?.name) skillNames.add(skill.name);
  }

  return errors;
}

// 生命周期阶段顺序。journal 里的 phase 必须在这条链上；跳过任何一段都意味着
// 某个持久化阶段没有对应的恢复动作。
const LIFECYCLE_PHASES = [
  "draining",
  "drained",
  "backing_up",
  "migrating",
  "health_checking",
  "committing",
  "projecting",
  "done",
];

function checkLifecycleSemantics(doc) {
  const errors = [];

  if (!LIFECYCLE_PHASES.includes(doc.phase)) {
    errors.push(`/phase: "${doc.phase}" is not a known lifecycle phase`);
    return errors;
  }

  // target.release.plugin 必须与顶层 plugin 一致：否则事务日志会指向
  // 另一个插件，恢复时会把 A 的进度套到 B 上。
  if (doc.target?.release?.plugin && doc.target.release.plugin !== doc.plugin) {
    errors.push(
      `/target/release/plugin: "${doc.target.release.plugin}" does not match the transaction plugin "${doc.plugin}"`,
    );
  }

  // done 之外的阶段一律不允许保留 error=null 却声称成功；done 阶段若有
  // error 则是自相矛盾。
  if (doc.phase === "done" && doc.error) {
    errors.push("/error: a completed transaction must not carry an error");
  }

  // 回滚必须能说明上一代次；没有 previous 的 Rollback 无从回退。
  if (doc.action === "Rollback" && !doc.previous) {
    errors.push("/previous: a Rollback transaction must record the previous activation");
  }
  if (doc.previous && doc.previous.activation >= doc.target.activation) {
    errors.push(
      `/previous/activation: ${doc.previous.activation} must be lower than the target activation ${doc.target.activation}`,
    );
  }

  return errors;
}

export function checkDocument(contracts, kind, doc) {
  const validate = contracts.validators[kind];
  if (!validate) {
    return { ok: false, errors: [`unknown contract kind "${kind}"`] };
  }

  const ok = validate(doc);
  const errors = [];
  if (!ok) {
    for (const err of validate.errors ?? []) errors.push(formatAjvError(err));
  }
  if (kind === "manifest") errors.push(...checkManifestSemantics(doc));
  if (kind === "lifecycle") errors.push(...checkLifecycleSemantics(doc));

  return { ok: errors.length === 0, errors };
}

// ---------------------------------------------------------------------------
// Trace validator：跨请求行为，schema 看不到。
// ---------------------------------------------------------------------------

function bindingKey(b) {
  return [b?.plugin, b?.project, String(b?.grant), String(b?.activation)].join("\u0000");
}

function stableStringify(value) {
  // 参数比较必须与键序无关：{a:1,b:2} 与 {b:2,a:1} 是同一个请求。
  if (value === null || typeof value !== "object") return JSON.stringify(value) ?? "null";
  if (Array.isArray(value)) return `[${value.map(stableStringify).join(",")}]`;
  const keys = Object.keys(value).sort();
  return `{${keys.map((k) => `${JSON.stringify(k)}:${stableStringify(value[k])}`).join(",")}}`;
}

function checkBindingShape(b, label, errors) {
  if (!b || typeof b !== "object") {
    errors.push(`${label}: binding is missing — a request without a binding has no grant behind it`);
    return false;
  }
  let good = true;
  for (const field of ["plugin", "project"]) {
    if (typeof b[field] !== "string" || b[field].length === 0) {
      errors.push(`${label}: binding.${field} must be a non-empty string`);
      good = false;
    }
  }
  for (const field of ["grant", "activation"]) {
    if (!Number.isInteger(b[field])) {
      errors.push(`${label}: binding.${field} must be an integer generation`);
      good = false;
    }
  }
  return good;
}

export function checkTrace(_contracts, trace, options = {}) {
  const errors = [];

  if (!trace || typeof trace !== "object") {
    return { ok: false, errors: ["trace: not an object"] };
  }

  const topOk = checkBindingShape(trace.binding, "trace", errors);
  if (!topOk && !trace.binding) {
    errors.push("trace: binding is required — an empty trace proves nothing about the contract");
  }

  if (!Array.isArray(trace.requests) || trace.requests.length === 0) {
    errors.push("trace: requests must be a non-empty array — an empty trace proves nothing");
    return { ok: false, errors };
  }

  const topKey = trace.binding ? bindingKey(trace.binding) : null;
  const seen = new Map(); // requestId -> stable params key

  trace.requests.forEach((req, i) => {
    const label = `requests/${i}`;
    if (!req || typeof req !== "object") {
      errors.push(`${label}: not an object`);
      return;
    }
    if (typeof req.requestId !== "string" || req.requestId.length === 0) {
      errors.push(`${label}: requestId must be a non-empty string`);
      return;
    }
    if (typeof req.method !== "string" || req.method.length === 0) {
      errors.push(`${label}: method must be a non-empty string`);
      return;
    }
    if (!("params" in req)) {
      errors.push(`${label}: params is required (use {} for a parameterless call)`);
      return;
    }

    if (req.binding !== undefined && req.binding !== null) {
      if (!checkBindingShape(req.binding, label, errors)) return;
      if (topKey !== null && bindingKey(req.binding) !== topKey) {
        errors.push(
          `${label}: binding project "${req.binding.project}" does not match the trace binding project ` +
            `"${trace.binding.project}" — one request id may not span two grants`,
        );
      }
    }

    // 候选版本不是公共激活版本：客户端和普通 CLI 不可使用（设计 §9.2 第 6 步）。
    // 请求可以继承 trace 级 binding，因此两级都要看，否则继承路径就是绕过口。
    if (options.candidateActivation !== undefined) {
      const effective = req.binding ?? trace.binding;
      if (effective && Number.isInteger(effective.activation) && effective.activation === options.candidateActivation) {
        errors.push(
          `${label}: binding uses activation ${effective.activation}, which is a candidate release ` +
            `under maintenance — candidate releases are not usable through a normal binding`,
        );
      }
    }

    const paramsKey = `${req.method}\u0001${stableStringify(req.params)}`;
    const prior = seen.get(req.requestId);
    if (prior === undefined) {
      seen.set(req.requestId, paramsKey);
    } else if (prior !== paramsKey) {
      errors.push(
        `${label}: requestId "${req.requestId}" is reused with different params ` +
          `— an idempotent retry must repeat method and params exactly`,
      );
    }
  });

  return { ok: errors.length === 0, errors };
}

// ---------------------------------------------------------------------------
// CLI：跑黄金样本 + 负例，打印实际计数。
// ---------------------------------------------------------------------------

function tamper(base, mutate) {
  const copy = structuredClone(base);
  mutate(copy);
  return copy;
}

function negativeSamples(fixtures) {
  return [
    {
      name: "manifest: unknown required field",
      kind: "manifest",
      doc: tamper(fixtures.manifest, (d) => d.methods.push({ name: "cards/readAll", effect: "Read", unknownButRequired: true })),
    },
    {
      name: "manifest: entry escapes package root",
      kind: "manifest",
      doc: tamper(fixtures.manifest, (d) => { d.views[0].entry = "../../../etc/passwd"; }),
    },
    {
      name: "manifest: no single main view",
      kind: "manifest",
      doc: tamper(fixtures.manifest, (d) => { d.views = d.views.map((v) => ({ ...v, kind: "settings" })); }),
    },
    {
      name: "manifest: migration skips a format version",
      kind: "manifest",
      doc: tamper(fixtures.manifest, (d) => { d.data.migrations = [{ from: 1, to: 3, entry: "migrations/1-to-3.sh" }]; }),
    },
    {
      name: "manifest: write method self-declared Read",
      kind: "manifest",
      doc: tamper(fixtures.manifest, (d) => d.methods.push({ name: "cards.deleteEverything", effect: "Read" })),
    },
    {
      name: "manifest: Maintenance effect declared by plugin",
      kind: "manifest",
      doc: tamper(fixtures.manifest, (d) => { d.methods[0].effect = "Maintenance"; }),
    },
    {
      name: "manifest: views removed",
      kind: "manifest",
      doc: tamper(fixtures.manifest, (d) => { delete d.views; }),
    },
    {
      name: "manifest: unsupported schema version",
      kind: "manifest",
      doc: tamper(fixtures.manifest, (d) => { d.schemaVersion = 99; }),
    },
    {
      name: "host: unknown api version",
      kind: "host",
      doc: tamper(fixtures.host, (d) => { d.api = "v99"; }),
    },
    {
      name: "host: no reserved environment declared",
      kind: "host",
      doc: tamper(fixtures.host, (d) => { d.reservedEnv = []; }),
    },
    {
      name: "host: package limit raised above the ceiling",
      kind: "host",
      doc: tamper(fixtures.host, (d) => { d.limits.packageBytes = 536870912; }),
    },
    {
      name: "bridge: arbitrary eval method",
      kind: "bridge",
      doc: tamper(fixtures.bridge, (d) => d.methods.push({ name: "plugins/v1/evalArbitrary", effect: "Read" })),
    },
    {
      name: "bridge: generic rpc passthrough",
      kind: "bridge",
      doc: tamper(fixtures.bridge, (d) => d.methods.push({ name: "plugins/v1/rpc", effect: "Write" })),
    },
    {
      name: "bridge: result union drops unknown",
      kind: "bridge",
      doc: tamper(fixtures.bridge, (d) => { d.responseVariants = ["pending", "committed", "rejected"]; }),
    },
    {
      name: "lifecycle: journal names another plugin",
      kind: "lifecycle",
      doc: tamper(fixtures.lifecycle, (d) => { d.target.release.plugin = "other-plugin"; }),
    },
    {
      name: "lifecycle: rollback without a previous activation",
      kind: "lifecycle",
      doc: tamper(fixtures.lifecycle, (d) => { d.action = "Rollback"; d.previous = null; }),
    },
    {
      name: "lifecycle: unknown phase",
      kind: "lifecycle",
      doc: tamper(fixtures.lifecycle, (d) => { d.phase = "teleporting"; }),
    },
    {
      name: "lifecycle: completed transaction carrying an error",
      kind: "lifecycle",
      doc: tamper(fixtures.lifecycle, (d) => {
        d.phase = "done";
        d.error = { kind: "Dependency", message: "unresolved" };
      }),
    },
    {
      name: "root: public key is not 32 bytes",
      kind: "root",
      doc: tamper(fixtures.root, (d) => { d.root.publicKey = Buffer.alloc(16).toString("base64"); }),
    },
    {
      name: "release: signature covers a field removed from the release",
      kind: "release",
      doc: tamper(fixtures.release, (d) => { delete d.release.sizeBytes; }),
    },
    {
      name: "release: unknown extra envelope field",
      kind: "release",
      doc: tamper(fixtures.release, (d) => { d.trustLevel = "official"; }),
    },
  ];
}

function negativeTraces() {
  const binding = { plugin: "example-counter", project: "project-1", grant: 1, activation: 1 };
  return [
    {
      name: "trace: same id, different params",
      trace: {
        binding,
        requests: [
          { requestId: "r-1", method: "counters/increment", params: { by: 1 } },
          { requestId: "r-1", method: "counters/increment", params: { by: 2 } },
        ],
      },
    },
    {
      name: "trace: request binding disagrees with trace binding",
      trace: {
        binding,
        requests: [
          {
            requestId: "r-1",
            method: "counters/increment",
            params: { by: 1 },
            binding: { ...binding, project: "project-2" },
          },
        ],
      },
    },
    { name: "trace: empty", trace: { binding: null, requests: [] } },
  ];
}

function positiveTrace() {
  const binding = { plugin: "example-counter", project: "project-1", grant: 1, activation: 1 };
  return {
    binding,
    requests: [
      { requestId: "r-1", method: "counters/increment", params: { by: 1 } },
      { requestId: "r-1", method: "counters/increment", params: { by: 1 } }, // 幂等重试
      { requestId: "r-2", method: "counters/read", params: {} },
    ],
  };
}

async function main() {
  const contracts = loadContracts();
  const fixtures = {
    manifest: readJSON(path.join(CONTRACTS_DIR, "fixtures", "manifest-v1.json")),
    host: readJSON(path.join(CONTRACTS_DIR, "fixtures", "host-v1.json")),
    bridge: readJSON(path.join(CONTRACTS_DIR, "fixtures", "bridge-v1.json")),
    lifecycle: readJSON(path.join(CONTRACTS_DIR, "fixtures", "lifecycle-v1.json")),
    root: readJSON(path.join(CONTRACTS_DIR, "fixtures", "root-v1.json")),
    release: readJSON(path.join(CONTRACTS_DIR, "fixtures", "release-v1.json")),
  };

  let failures = 0;
  let positive = 0;
  let negative = 0;

  console.log("contract checker: positive samples");

  for (const [kind, doc] of Object.entries(fixtures)) {
    const result = checkDocument(contracts, kind, doc);
    positive += 1;
    if (result.ok) {
      console.log(`  ok    ${kind} fixture`);
    } else {
      failures += 1;
      console.log(`  FAIL  ${kind} fixture`);
      for (const line of result.errors) console.log(`          ${line}`);
    }
  }

  // 签名信封是派生样本：payload 与 signature 由测试密钥现算，不能手写。
  // 手写的"有效签名"必须有人验过才算有效，所以这里现算再验。
  const { buildRootEnvelope, buildReleaseEnvelope, verifyEnvelope } = await import("./sign-contracts.mjs");
  const signedSamples = [
    ["root envelope (sig)", await buildRootEnvelope()],
    [
      "release envelope (sig)",
      await buildReleaseEnvelope({
        release: readJSON(path.join(CONTRACTS_DIR, "fixtures", "release-v1.json")).release,
      }),
    ],
  ];
  for (const [name, envelope] of signedSamples) {
    positive += 1;
    const verified = verifyEnvelope(envelope);
    if (verified.ok) {
      console.log(`  ok    ${name}`);
    } else {
      failures += 1;
      console.log(`  FAIL  ${name}`);
      for (const line of verified.errors) console.log(`          ${line}`);
    }
  }

  const traceResult = checkTrace(contracts, positiveTrace());
  positive += 1;
  if (traceResult.ok) {
    console.log("  ok    idempotent retry trace");
  } else {
    failures += 1;
    console.log("  FAIL  idempotent retry trace");
    for (const line of traceResult.errors) console.log(`          ${line}`);
  }

  console.log("contract checker: negative samples (each must be rejected)");

  for (const sample of negativeSamples(fixtures)) {
    negative += 1;
    const result = checkDocument(contracts, sample.kind, sample.doc);
    if (result.ok) {
      failures += 1;
      console.log(`  FAIL  ${sample.name} was accepted but must be rejected`);
    } else {
      console.log(`  ok    ${sample.name}`);
    }
  }

  for (const sample of negativeTraces()) {
    negative += 1;
    const result = checkTrace(contracts, sample.trace);
    if (result.ok) {
      failures += 1;
      console.log(`  FAIL  ${sample.name} was accepted but must be rejected`);
    } else {
      console.log(`  ok    ${sample.name}`);
    }
  }

  console.log(`\npositive: ${positive}, negative: ${negative}, failures: ${failures}`);

  if (failures > 0 || positive === 0 || negative === 0) {
    // 零样本也算失败：一个什么都没验的 checker 不能报绿。
    console.error("contract check FAILED");
    process.exit(1);
  }
  console.log("contract check passed");
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch((err) => {
    console.error(err);
    process.exit(1);
  });
}
