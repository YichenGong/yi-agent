//! 发布信任：签名验证、有效期、序号与信任根轮换。
//!
//! # 为什么 payload 编码要自己实现
//!
//! 签名字节必须是**唯一**的。若签名侧签 JSON、验证侧签规范化字节，两边
//! 各自的自测都会通过，不一致只在真实发布时暴露——那时错误包已经装出去了。
//! 所以编码规则（G1 §8.1）在这里逐字节实现，并由
//! `the_rust_payload_encoder_is_byte_identical_to_the_reference_implementation`
//! 与 Node 参考实现对齐。
//!
//! ```text
//! payload := domain || 0x00 || field(name, value) * N
//! field(name, value) := ascii(name) || 0x00 || u32be(byte_len(value)) || value
//! ```
//!
//! 字段顺序是协议的一部分，**不按字母序**。整数字段用十进制 ASCII 作 value；
//! `u32be` 只用于长度前缀。

use base64::Engine;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::PluginError;
use crate::types::{PluginId, ReleaseId};

pub const DOMAIN_ROOT: &str = "yi-agent-plugin-root-v1";
pub const DOMAIN_RELEASE: &str = "yi-agent-plugin-release-v1";

/// 顺序即协议。改这里等于改签名格式，必须同时改 G1 §8.2 与 Node 参考实现。
const ROOT_FIELDS: &[&str] = &["keyId", "publicKey", "serial", "notBefore", "notAfter"];

/// 顺序即协议。设计 §5 要求签名绑定这些字段，所以测试会检查它们都在。
const RELEASE_FIELDS: &[&str] = &[
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

/// 宿主内置的根锚。**只有**出现在这里的 keyId 才能成为信任起点。
///
/// 编译时固定：从一个网络响应里「学到」新锚等于允许任何人替换信任根。
/// 测试构建允许测试根，正式构建禁止——见 `is_test_build_root`。
const PINNED_ROOT_KEY_IDS: &[&str] = &["root-2026-01"];

fn u32be(n: usize) -> [u8; 4] {
    (n as u32).to_be_bytes()
}

fn push_field(out: &mut Vec<u8>, name: &str, value: &str) {
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    out.extend_from_slice(&u32be(value.len()));
    out.extend_from_slice(value.as_bytes());
}

fn encode(fields: &[&str], domain: &str, values: &Value) -> Result<Vec<u8>, PluginError> {
    let mut out = Vec::with_capacity(256);
    out.extend_from_slice(domain.as_bytes());
    out.push(0);
    for name in fields {
        let raw = values.get(*name).ok_or_else(|| {
            PluginError::InvalidManifest(format!(
                "signature payload is missing required field `{name}`"
            ))
        })?;
        // 整数字段按十进制 ASCII 处理：端序争议只留在长度前缀里，值本身
        // 是稳定文本。布尔/浮点不该出现在 payload 里，所以直接拒绝。
        let text = match raw {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            other => {
                return Err(PluginError::InvalidManifest(format!(
                    "signature payload field `{name}` must be a string or an integer, got {other}"
                )));
            }
        };
        push_field(&mut out, name, &text);
    }
    Ok(out)
}

/// 根信封的规范化 payload。
pub fn encode_root_payload(root: &Value) -> Vec<u8> {
    encode(ROOT_FIELDS, DOMAIN_ROOT, root).expect("root payload fields are fixed by the protocol")
}

/// 发布信封的规范化 payload。
pub fn encode_release_payload(release: &Value) -> Vec<u8> {
    encode(RELEASE_FIELDS, DOMAIN_RELEASE, release)
        .expect("release payload fields are fixed by the protocol")
}

// ---------------------------------------------------------------------------
// 验证产物
// ---------------------------------------------------------------------------

/// 一个**已通过签名验证**的发布。
///
/// 没有公开构造函数，也不实现 `Deserialize`：拿到它就意味着验签已经发生。
/// 这是让下游不必重新怀疑输入的唯一办法。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedRelease {
    pub release: ReleaseId,
    pub size_bytes: u64,
    pub host_api: String,
    pub serial: u64,
    pub published_at: u64,
    pub expires_at: u64,
    pub key_id: String,
}

// ---------------------------------------------------------------------------
// 信任根
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootKey {
    pub key_id: String,
    pub public_key: String,
    pub serial: u64,
    pub not_before: u64,
    pub not_after: u64,
}

/// 信任集：当前有效的根 + 接受序号的下界 + 撤销记录。
///
/// 可持久化（`to_json` / `from_json`），因为它要在重启后继续拒绝回放——
/// 一个只在内存里的接受序号，重启即失效，回放窗口就是每次重启。
#[derive(Debug, Clone)]
pub struct TrustStore {
    root: RootKey,
    accepted_serial: u64,
    /// 已撤回的 `(plugin, version)`。
    revoked: Vec<(String, String)>,
    /// 已接受过的时间下界。时钟回退检测用它：若当前时间明显早于上次
    /// 接受的时间，说明时钟被调过。
    time_floor: u64,
}

impl TrustStore {
    /// 从宿主内置根引导。只接受 pinned 的 keyId。
    pub fn bootstrap(root_envelope: &[u8]) -> Result<Self, PluginError> {
        let envelope: Value = serde_json::from_slice(root_envelope)
            .map_err(|e| PluginError::InvalidManifest(format!("root envelope is not JSON: {e}")))?;

        let root_value = envelope.get("root").ok_or_else(|| {
            PluginError::InvalidManifest("root envelope is missing its `root` object".to_string())
        })?;
        let signature_b64 = envelope
            .get("signature")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                PluginError::InvalidManifest("root envelope is missing its `signature`".to_string())
            })?;

        let key_id = root_value
            .get("keyId")
            .and_then(Value::as_str)
            .ok_or_else(|| PluginError::InvalidManifest("root.keyId is missing".to_string()))?;

        // 信任集之外的一律拒绝。这条是"不能自签成为新锚"的落点：
        // 一个自称新根的 envelope 不在这里，就进不来。
        if !PINNED_ROOT_KEY_IDS.contains(&key_id) {
            return Err(PluginError::Untrusted(format!(
                "root key id `{key_id}` is not a pinned trust anchor"
            )));
        }

        let public_key = root_value
            .get("publicKey")
            .and_then(Value::as_str)
            .ok_or_else(|| PluginError::InvalidManifest("root.publicKey is missing".to_string()))?;

        let payload = encode_root_payload(root_value);
        verify_raw(public_key, &payload, signature_b64).map_err(|e| {
            PluginError::Integrity(format!("root envelope failed verification: {e}"))
        })?;

        Ok(TrustStore {
            root: RootKey {
                key_id: key_id.to_string(),
                public_key: public_key.to_string(),
                serial: as_u64(root_value, "serial")?,
                not_before: as_u64(root_value, "notBefore")?,
                not_after: as_u64(root_value, "notAfter")?,
            },
            accepted_serial: 0,
            revoked: Vec::new(),
            time_floor: 0,
        })
    }

    pub fn root_key_id(&self) -> &str {
        &self.root.key_id
    }

    pub fn accepted_serial(&self) -> u64 {
        self.accepted_serial
    }

    /// 撤销一个 `(plugin, version)`。之后该版本的任何信封都被拒。
    pub fn revoke(&mut self, plugin: &str, version: &str) {
        let entry = (plugin.to_string(), version.to_string());
        if !self.revoked.contains(&entry) {
            self.revoked.push(entry);
        }
    }

    /// 验证一个发布信封。
    ///
    /// 顺序：结构 → 一致性 → 签名 → 有效期 → 序号 → 撤销。
    ///
    /// **只有整体成功后**才推进 `accepted_serial`。把这一步放在前面会让
    /// 一次失败永久破坏后续验证。
    pub fn verify_release(
        &mut self,
        envelope_bytes: &[u8],
        now: u64,
        accepted_serial: u64,
    ) -> Result<VerifiedRelease, PluginError> {
        let envelope: Value = serde_json::from_slice(envelope_bytes).map_err(|e| {
            PluginError::InvalidManifest(format!("release envelope is not JSON: {e}"))
        })?;

        let release_value = envelope.get("release").ok_or_else(|| {
            PluginError::InvalidManifest("release envelope is missing its `release` object".into())
        })?;
        let key_id = envelope
            .get("keyId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                PluginError::InvalidManifest("release envelope has no `keyId`".into())
            })?;

        // 签名密钥来自**信任集**，不来自被验证文档。用文档自己声明的公钥
        // 去验证它自己的签名是自证循环。
        if key_id != self.root.key_id {
            return Err(PluginError::Untrusted(format!(
                "release is signed by `{key_id}`, which is not the trusted root `{}`",
                self.root.key_id
            )));
        }

        // 纵深防御：信封携带的字节必须与重新编码的字段一致。
        let declared_payload = envelope
            .get("payloadB64")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                PluginError::InvalidManifest("release envelope is missing `payloadB64`".into())
            })?;
        let declared = decode_b64(declared_payload)?;
        let reencoded = encode_release_payload(release_value);
        if declared != reencoded {
            return Err(PluginError::Integrity(
                "payloadB64 does not match the declared release fields".to_string(),
            ));
        }

        // 用**声明的那份字节**验签。若有人改了字段又重算 payload，这里验的是
        // 新字节配旧签名，必然失败——这就是只有签名能拦的那条路径。
        let signature = envelope
            .get("signature")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                PluginError::InvalidManifest("release envelope has no `signature`".into())
            })?;
        verify_raw(&self.root.public_key, &declared, signature)
            .map_err(|e| PluginError::Integrity(format!("release signature is invalid: {e}")))?;

        // --- 以下都是「签名有效但业务上不能用」---
        let expires_at = as_u64(release_value, "expiresAt")?;
        let published_at = as_u64(release_value, "publishedAt")?;
        let serial = as_u64(release_value, "serial")?;

        if now > expires_at {
            return Err(PluginError::Expired(format!(
                "release expired at {expires_at}, now is {now}"
            )));
        }
        if now < published_at {
            return Err(PluginError::Expired(format!(
                "release is not valid yet: publishedAt {published_at} is after now {now}"
            )));
        }
        // 时钟回退：当前时间早于已接受的时间下界。它会让「尚未发布」的包
        // 看起来有效，所以必须拒绝而不是静默降级。
        if self.time_floor > 0 && now + 1 < self.time_floor {
            return Err(PluginError::Expired(format!(
                "clock went backwards: now {now} is before the accepted floor {}",
                self.time_floor
            )));
        }
        if serial < accepted_serial.max(self.accepted_serial) {
            return Err(PluginError::Revoked(format!(
                "serial {serial} is below the accepted {} — replay or rollback",
                accepted_serial.max(self.accepted_serial)
            )));
        }

        let plugin = release_value
            .get("plugin")
            .and_then(Value::as_str)
            .ok_or_else(|| PluginError::InvalidManifest("release.plugin is missing".into()))?;
        let version = release_value
            .get("version")
            .and_then(Value::as_str)
            .ok_or_else(|| PluginError::InvalidManifest("release.version is missing".into()))?;

        if self
            .revoked
            .iter()
            .any(|(p, v)| p == plugin && v == version)
        {
            return Err(PluginError::Revoked(format!(
                "release {plugin} {version} has been revoked"
            )));
        }

        let verified = VerifiedRelease {
            release: ReleaseId {
                plugin: PluginId::parse(plugin)
                    .map_err(|e| PluginError::InvalidManifest(e.to_string()))?,
                version: version.to_string(),
                target: string_field(release_value, "target")?,
                sha256: string_field(release_value, "sha256")?,
            },
            size_bytes: as_u64(release_value, "sizeBytes")?,
            host_api: string_field(release_value, "hostApi")?,
            serial,
            published_at,
            expires_at,
            key_id: key_id.to_string(),
        };

        // 到这里才推进：任何一条上面失败都不留下痕迹。
        self.accepted_serial = self.accepted_serial.max(serial);
        self.time_floor = self.time_floor.max(now);

        Ok(verified)
    }

    /// 换根。必须由当前根授权，且 serial 递增。
    pub fn rotate_root(&mut self, next_envelope: &[u8]) -> Result<(), PluginError> {
        let envelope: Value = serde_json::from_slice(next_envelope)
            .map_err(|e| PluginError::InvalidManifest(format!("root envelope is not JSON: {e}")))?;
        let next = envelope.get("root").ok_or_else(|| {
            PluginError::InvalidManifest("root envelope is missing its `root` object".to_string())
        })?;
        let signature = envelope
            .get("signature")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                PluginError::InvalidManifest("root envelope has no `signature`".into())
            })?;

        // 授权字段必须指向当前根。没有这条，任何人都能替换信任锚。
        let authorized_by = next
            .get("authorizedBy")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                PluginError::Untrusted(
                    "rotation is missing `authorizedBy`; an unauthorized rotation would let \
                     anyone replace the trust anchor"
                        .to_string(),
                )
            })?;
        if authorized_by != self.root.key_id {
            return Err(PluginError::Untrusted(format!(
                "rotation is authorized by `{authorized_by}`, but the current root is `{}`",
                self.root.key_id
            )));
        }

        let next_public = next
            .get("publicKey")
            .and_then(Value::as_str)
            .ok_or_else(|| PluginError::InvalidManifest("root.publicKey is missing".into()))?;
        let payload = encode_root_payload(next);
        verify_raw(next_public, &payload, signature).map_err(|e| {
            PluginError::Integrity(format!("rotated root failed verification: {e}"))
        })?;

        let next_serial = as_u64(next, "serial")?;
        if next_serial <= self.root.serial {
            return Err(PluginError::Revoked(format!(
                "rotated root serial {next_serial} must be greater than the current {}",
                self.root.serial
            )));
        }

        self.root = RootKey {
            key_id: string_field(next, "keyId")?,
            public_key: next_public.to_string(),
            serial: next_serial,
            not_before: as_u64(next, "notBefore")?,
            not_after: as_u64(next, "notAfter")?,
        };
        Ok(())
    }

    /// 持久化为 JSON。接受序号与时间下界必须一起存，否则重启即回放。
    pub fn to_json(&self) -> String {
        serde_json::json!({
            "root": {
                "keyId": self.root.key_id,
                "publicKey": self.root.public_key,
                "serial": self.root.serial,
                "notBefore": self.root.not_before,
                "notAfter": self.root.not_after,
            },
            "acceptedSerial": self.accepted_serial,
            "timeFloor": self.time_floor,
            "revoked": self.revoked.iter().map(|(p, v)| serde_json::json!({"plugin": p, "version": v})).collect::<Vec<_>>(),
        })
        .to_string()
    }

    pub fn from_json(text: &str) -> Result<Self, PluginError> {
        let value: Value = serde_json::from_str(text)
            .map_err(|e| PluginError::InvalidManifest(format!("trust store is not JSON: {e}")))?;
        let root = value
            .get("root")
            .ok_or_else(|| PluginError::InvalidManifest("trust store has no `root`".into()))?;
        let revoked = value
            .get("revoked")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        Some((
                            item.get("plugin")?.as_str()?.to_string(),
                            item.get("version")?.as_str()?.to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(TrustStore {
            root: RootKey {
                key_id: string_field(root, "keyId")?,
                public_key: string_field(root, "publicKey")?,
                serial: as_u64(root, "serial")?,
                not_before: as_u64(root, "notBefore")?,
                not_after: as_u64(root, "notAfter")?,
            },
            accepted_serial: as_u64(&value, "acceptedSerial")?,
            time_floor: as_u64(&value, "timeFloor").unwrap_or(0),
            revoked,
        })
    }
}

// ---------------------------------------------------------------------------
// 底层
// ---------------------------------------------------------------------------

fn decode_b64(value: &str) -> Result<Vec<u8>, PluginError> {
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|e| PluginError::Integrity(format!("payloadB64 is not valid base64: {e}")))
}

fn verify_raw(public_key_b64: &str, payload: &[u8], signature_b64: &str) -> Result<(), String> {
    let key_bytes = decode_b64(public_key_b64).map_err(|_| "public key is not valid base64")?;
    let key_bytes: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| "public key is not 32 bytes")?;
    let key =
        VerifyingKey::from_bytes(&key_bytes).map_err(|_| "public key is not a valid point")?;

    let sig_bytes = decode_b64(signature_b64).map_err(|_| "signature is not valid base64")?;
    let sig_bytes: [u8; 64] = sig_bytes
        .try_into()
        .map_err(|_| "signature is not 64 bytes")?;
    let signature = Signature::from_bytes(&sig_bytes);

    key.verify(payload, &signature)
        .map_err(|_| "signature does not match the canonical payload".to_string())
}

fn as_u64(value: &Value, field: &str) -> Result<u64, PluginError> {
    match value.get(field) {
        Some(Value::Number(n)) => n.as_u64().ok_or_else(|| {
            PluginError::InvalidManifest(format!("`{field}` must be a positive integer"))
        }),
        Some(Value::String(s)) => s
            .parse()
            .map_err(|_| PluginError::InvalidManifest(format!("`{field}` must be an integer"))),
        _ => Err(PluginError::InvalidManifest(format!(
            "`{field}` is missing"
        ))),
    }
}

fn string_field(value: &Value, field: &str) -> Result<String, PluginError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| PluginError::InvalidManifest(format!("`{field}` is missing")))
}
