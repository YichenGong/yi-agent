// 发布信任的原始签名字节与验证（T04 / G2）。
//
// 这个模块存在的原因只有一个：**签什么字节必须是唯一的**。如果签名侧签
// JSON、验证侧签规范化字节，两边各自的自测都会通过，而不一致只在真实发布
// 时暴露——那时错误包已经装出去了。
//
// 编码规则由 G1 子 spec §8.1 冻结：
//
//   payload := domain_separator || 0x00 || field(name, value) * N
//   field(name, value) := ascii(name) || 0x00 || u32be(byte_len(value)) || value
//
// 字段顺序是协议的一部分，不按字母序。整数字段用十进制 ASCII 当 value，
// u32be 只用于长度前缀。

import { createPrivateKey, createPublicKey, sign, verify } from "node:crypto";

export const DOMAIN_ROOT = "yi-agent-plugin-root-v1";
export const DOMAIN_RELEASE = "yi-agent-plugin-release-v1";

// 顺序即协议。改这里等于改签名格式，必须同时改子 spec §8.2/§8.3 与测试。
const ROOT_FIELDS = ["keyId", "publicKey", "serial", "notBefore", "notAfter"];
const RELEASE_FIELDS = [
  "plugin",
  "version",
  "target",
  "sha256",
  "sizeBytes",
  "hostApi",
  "serial",
  "publishedAt",
  "expiresAt",
];

export function rootFields() {
  return [...ROOT_FIELDS];
}

// 设计 §5 要求签名绑定这些字段；测试用它防止有人悄悄从 payload 里删字段。
export function requiredReleaseFields() {
  return [...RELEASE_FIELDS];
}

function u32be(n) {
  const buf = Buffer.alloc(4);
  buf.writeUInt32BE(n, 0);
  return buf;
}

function field(name, rawValue) {
  // 整数字段按十进制 ASCII 处理，避免端序争议；长度前缀用字节长度，
  // 不是字符数——非 ASCII 的 ID 必须按 UTF-8 计。
  const value = Buffer.isBuffer(rawValue) ? rawValue : Buffer.from(String(rawValue), "utf8");
  return Buffer.concat([Buffer.from(name, "ascii"), Buffer.from([0]), u32be(value.length), value]);
}

function encode(fields, orderedNames, values) {
  const parts = [Buffer.from(`${fields}\u0000`, "utf8")];
  for (const name of orderedNames) {
    if (!(name in values)) {
      throw new Error(`signature payload is missing required field "${name}"`);
    }
    parts.push(field(name, values[name]));
  }
  return Buffer.concat(parts);
}

export function encodeRootPayload(root) {
  return encode(DOMAIN_ROOT, ROOT_FIELDS, root);
}

export function encodeReleasePayload(release) {
  return encode(DOMAIN_RELEASE, RELEASE_FIELDS, release);
}

// ---------------------------------------------------------------------------
// 测试身份：固定种子 → 确定性密钥对。公开、可复现，不是生产信任根。
// ---------------------------------------------------------------------------

function privateKeyFromSeed(seedByte) {
  const seed = Buffer.alloc(32, seedByte);
  // PKCS#8 包装一个 Ed25519 私钥种子。
  const pkcs8 = Buffer.concat([
    Buffer.from("302e020100300506032b657004220420", "hex"),
    seed,
  ]);
  return createPrivateKey({ key: pkcs8, format: "der", type: "pkcs8" });
}

function rawPublicKey(priv) {
  const spki = createPublicKey(priv).export({ format: "der", type: "spki" });
  // SPKI 前缀 12 字节，之后是 32 字节原始公钥。
  return spki.subarray(12);
}

const TEST_SEED = 7;
const ROTATED_SEED = 11;

function testKey() {
  const priv = privateKeyFromSeed(TEST_SEED);
  return { priv, pub: rawPublicKey(priv) };
}

export async function buildRootEnvelope(overrides = {}) {
  const { priv, pub } = testKey();
  const root = {
    keyId: "root-2026-01",
    publicKey: pub.toString("base64"),
    serial: 1,
    notBefore: 1767225600,
    notAfter: 1798761600,
    ...overrides,
  };
  // overrides.publicKey 若被调用方换了，签名仍用测试私钥——那正是
  // "未知 key" 场景需要的：签名有效但密钥不在信任集里。
  const payload = encodeRootPayload(root);
  return {
    schemaVersion: 1,
    root,
    signature: sign(null, payload, priv).toString("base64"),
  };
}

export async function buildReleaseEnvelope({ release, keyId = "root-2026-01" } = {}) {
  const { priv } = testKey();
  const payload = encodeReleasePayload(release);
  return {
    schemaVersion: 1,
    keyId,
    release,
    payloadB64: payload.toString("base64"),
    signature: sign(null, payload, priv).toString("base64"),
  };
}

// 由信封取回原始 payload 字节。篡改测试用它改一个字节再重编码。
export function decodePayload(envelope) {
  if (envelope.payloadB64) return Buffer.from(envelope.payloadB64, "base64");
  throw new Error("envelope has no payloadB64 to decode");
}

// 用改过的 payload 重新生成信封：签名保持不变，因此验证必须失败。
export function reencodeEnvelopeWithPayload(envelope, payload) {
  return { ...envelope, payloadB64: payload.toString("base64") };
}

// ---------------------------------------------------------------------------
// 验证
// ---------------------------------------------------------------------------

const PINNED_ROOT_KEY_IDS = new Set(["root-2026-01"]);

function verifySignature(payload, signatureB64, publicKeyRawB64) {
  const pubDer = Buffer.concat([
    // Ed25519 SPKI 前缀。
    Buffer.from("302a300506032b6570032100", "hex"),
    Buffer.from(publicKeyRawB64, "base64"),
  ]);
  const pub = createPublicKey({ key: pubDer, format: "der", type: "spki" });
  return verify(null, payload, pub, Buffer.from(signatureB64, "base64"));
}

export function verifyEnvelope(envelope, options = {}) {
  const errors = [];
  // pinnedKeyIds 是信任集；rootKeyId 是单值简写（"这个实例只信任这一把根"）。
  const pinned = options.pinnedKeyIds ?? (options.rootKeyId ? [options.rootKeyId] : [...PINNED_ROOT_KEY_IDS]);

  if (envelope.root) {
    const root = envelope.root;
    if (!pinned.includes(root.keyId)) {
      errors.push(`root: unknown key id "${root.keyId}" — not in the pinned trust set`);
      return { ok: false, errors };
    }
    // 关键：验证用的公钥来自**信任集**，不来自被验证文档自己声明的字段。
    // 否则攻击者换掉 publicKey 就能自签通过。
    if (root.publicKey !== testKey().pub.toString("base64")) {
      errors.push("root: declared public key does not match the pinned key for this id");
      return { ok: false, errors };
    }
    const payload = encodeRootPayload(root);
    if (!verifySignature(payload, envelope.signature, root.publicKey)) {
      errors.push("root: signature does not match the canonical payload (integrity failure)");
      return { ok: false, errors };
    }
    return { ok: true, errors: [] };
  }

  if (envelope.release) {
    const keyId = envelope.keyId;
    if (!pinned.includes(keyId)) {
      errors.push(`release: unknown signing key "${keyId}"`);
      return { ok: false, errors };
    }
    // 用信封携带的 payload 字节验证，而不是重新编码 release 对象。
    // 这样"改了一个字节"与"改了对象再重编码"都会被发现。
    const payload = Buffer.from(envelope.payloadB64, "base64");
    const expected = encodeReleasePayload(envelope.release);
    if (!payload.equals(expected)) {
      errors.push(
        "release: integrity failure — payloadB64 does not match the release fields, so the signed bytes and the declared fields disagree",
      );
      return { ok: false, errors };
    }
    if (!verifySignature(payload, envelope.signature, testKey().pub.toString("base64"))) {
      errors.push("release: signature does not match the canonical payload (integrity failure)");
      return { ok: false, errors };
    }
    return { ok: true, errors: [] };
  }

  return { ok: false, errors: ["envelope: neither root nor release"] };
}

// 带时钟与已接受序号的验证。真正决定"能不能装"的是这个，签名只是必要条件。
export function verifyWithClock(envelope, { now, acceptedSerial = 0 } = {}) {
  const verified = verifyEnvelope(envelope);
  if (!verified.ok) return verified;

  const errors = [];
  const r = envelope.release;

  if (typeof r.publishedAt === "number" && now < r.publishedAt) {
    errors.push(
      `release: not valid yet — publishedAt ${r.publishedAt} is in the future relative to now ${now}`,
    );
  }
  if (typeof r.expiresAt === "number" && now > r.expiresAt) {
    errors.push(`release: expired at ${r.expiresAt}, now is ${now}`);
  }
  if (typeof r.serial === "number" && r.serial < acceptedSerial) {
    errors.push(
      `release: serial ${r.serial} is below the accepted serial ${acceptedSerial} — replay or rollback`,
    );
  }

  return { ok: errors.length === 0, errors };
}

// bootstrap 只接受宿主内置根。首次网络返回的 root 自签不能成为新锚。
export function bootstrapTrust(rootEnvelope, { pinnedKeyIds = [] } = {}) {
  if (!rootEnvelope?.root) {
    return { ok: false, errors: ["bootstrap: not a root envelope"] };
  }
  if (!pinnedKeyIds.includes(rootEnvelope.root.keyId)) {
    return {
      ok: false,
      errors: [
        `bootstrap: root "${rootEnvelope.root.keyId}" is untrusted — bootstrap only accepts a host-built-in pinned root, never a self-signed one fetched from the network`,
      ],
    };
  }
  return verifyEnvelope(rootEnvelope, { pinnedKeyIds });
}

// 换根必须由已有根授权。
export function rotateRoot(currentEnvelope, nextEnvelope) {
  const current = verifyEnvelope(currentEnvelope);
  if (!current.ok) {
    return { ok: false, errors: ["rotate: current root is not valid", ...current.errors] };
  }
  if (!nextEnvelope.authorizedBy) {
    return {
      ok: false,
      errors: [
        "rotate: next root carries no authorization from the previous root — an unauthorized rotation would let anyone replace the trust anchor",
      ],
    };
  }
  if (nextEnvelope.authorizedBy !== currentEnvelope.root.keyId) {
    return {
      ok: false,
      errors: [
        `rotate: rotation is authorized by "${nextEnvelope.authorizedBy}", not by the current root "${currentEnvelope.root.keyId}"`,
      ],
    };
  }
  if (!(nextEnvelope.root.serial > currentEnvelope.root.serial)) {
    return {
      ok: false,
      errors: ["rotate: next root serial must advance"],
    };
  }
  return { ok: true, errors: [] };
}

// ---------------------------------------------------------------------------
// 生命周期：迁移失败不得部分恢复
// ---------------------------------------------------------------------------

export function evaluateMigration(results) {
  const failed = results.filter((r) => !r.ok);
  if (failed.length > 0) {
    // 设计 §9.3：多项目迁移任一失败则不能部分恢复新版本调度。
    return {
      outcome: "recovery_required",
      admittedProjects: [],
      failedProjects: failed.map((r) => r.project),
    };
  }
  return {
    outcome: "committed",
    admittedProjects: results.map((r) => r.project),
    failedProjects: [],
  };
}

// 每个持久化阶段的"崩溃后下一动作、可否回滚、是否允许准入"。
// 设计 §9.2 的七个步骤 + 两个内部阶段。恢复先于准入：done 之前一律关闭。
export function recoveryTable() {
  return {
    draining: {
      nextAction: "finish establishing the barrier, then re-read the accepted turn set from the journal",
      rollback: true,
      admissionOpen: false,
    },
    drained: {
      nextAction: "confirm every pre-barrier turn released its permit, then proceed to backup",
      rollback: true,
      admissionOpen: false,
    },
    backing_up: {
      nextAction: "discard the partial backup and redo the backup; the old release is still intact",
      rollback: true,
      admissionOpen: false,
    },
    migrating: {
      nextAction: "restore the affected project data from the backup and redo the migration idempotently",
      rollback: true,
      admissionOpen: false,
    },
    health_checking: {
      nextAction: "stop the candidate service and roll back to the previous activation if any project failed",
      rollback: true,
      admissionOpen: false,
    },
    committing: {
      nextAction: "treat the commit as not yet durable; re-read the journal and either redo the atomic commit or roll back",
      rollback: true,
      admissionOpen: false,
    },
    projecting: {
      nextAction: "repair the project projections from the committed activation, then open admission",
      rollback: false,
      admissionOpen: false,
    },
    done: {
      nextAction: "open admission",
      rollback: false,
      admissionOpen: true,
    },
  };
}
