//! 扫码配对与设备 token。
//!
//! 一次性配对码(默认 5 分钟)换一枚**明文只出现一次**的设备 token;服务端只
//! 存哈希。撤销设备即删记录,该 token 立刻失效。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::device_store::{Device, DeviceStore};
use crate::protocol::Scope;

/// 配对码有效期。
pub const PAIR_CODE_TTL: Duration = Duration::from_secs(300);

#[derive(Debug, Clone)]
pub struct PairCode {
    pub code: String,
    pub expires_in: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PairError {
    /// 码不存在、已用过或已过期。
    InvalidCode,
}

struct PendingCode {
    expires_at: Instant,
}

pub struct PairingState {
    store: DeviceStore,
    codes: Mutex<HashMap<String, PendingCode>>,
}

fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// token 的存储形态:哈希。用 SHA-256 的十六进制表示。
fn hash_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

impl PairingState {
    pub fn new(store: DeviceStore) -> Self {
        Self {
            store,
            codes: Mutex::new(HashMap::new()),
        }
    }

    pub fn store(&self) -> &DeviceStore {
        &self.store
    }

    /// 铸一枚一次性配对码。桌面端拿到后渲染二维码。
    pub fn create_code(&self) -> PairCode {
        let code = short_code();
        self.codes.lock().unwrap_or_else(|p| p.into_inner()).insert(
            code.clone(),
            PendingCode {
                expires_at: Instant::now() + PAIR_CODE_TTL,
            },
        );
        PairCode {
            code,
            expires_in: PAIR_CODE_TTL.as_secs(),
        }
    }

    /// 用配对码换设备 token。返回设备与**明文 token**;码用后即焚。
    pub fn redeem(&self, code: &str, device_name: &str) -> Result<(Device, String), PairError> {
        let mut guard = self.codes.lock().unwrap_or_else(|p| p.into_inner());
        let Some(pending) = guard.remove(code) else {
            return Err(PairError::InvalidCode);
        };
        if Instant::now() > pending.expires_at {
            return Err(PairError::InvalidCode);
        }
        drop(guard);

        let token = format!("yia_{}", short_code_secret());
        let now = now_epoch_secs();
        let device = Device {
            id: format!("dev-{}", uuid::Uuid::new_v4()),
            name: device_name.to_string(),
            // 新配对设备默认 Control(决策 A)。
            scope: Scope::Control,
            token_hash: hash_token(&token),
            created_at: now,
            last_seen_at: now,
        };
        self.store
            .add(device.clone())
            .map_err(|_| PairError::InvalidCode)?;
        Ok((device, token))
    }

    /// 校验一枚 token。命中即刷新 `last_seen_at`。
    pub fn authenticate(&self, token: &str) -> Option<Device> {
        let hash = hash_token(token);
        let found = self
            .store
            .list()
            .into_iter()
            .find(|d| d.token_hash == hash)?;
        let mut touched = found.clone();
        touched.last_seen_at = now_epoch_secs();
        let _ = self.store.add(touched);
        Some(found)
    }

    pub fn revoke(&self, device_id: &str) -> Result<bool, PairError> {
        self.store
            .revoke(device_id)
            .map_err(|_| PairError::InvalidCode)
    }
}

/// 人类可读的一次性配对码:`XXXX-XXXX`(易输入,去掉易混字符)。
fn short_code() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut out = String::new();
    for _ in 0..8 {
        let byte = uuid::Uuid::new_v4().as_bytes()[0] as usize;
        out.push(ALPHABET[byte % ALPHABET.len()] as char);
    }
    format!("{}-{}", &out[0..4], &out[4..8])
}

/// token 的随机段。
fn short_code_secret() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_redeems_once_and_yields_a_usable_token() {
        let dir = tempfile::TempDir::new().unwrap();
        let pairing = PairingState::new(DeviceStore::new(dir.path().join("devices.json")));
        let code = pairing.create_code();
        let (device, token) = pairing.redeem(&code.code, "iPhone 15").unwrap();
        assert_eq!(device.scope, Scope::Control, "新设备默认 control");
        assert!(pairing.authenticate(&token).is_some());

        // 同一个码不能再用。
        assert!(pairing.redeem(&code.code, "again").is_err());
    }

    #[test]
    fn an_unknown_token_does_not_authenticate() {
        let dir = tempfile::TempDir::new().unwrap();
        let pairing = PairingState::new(DeviceStore::new(dir.path().join("devices.json")));
        assert!(pairing.authenticate("nope").is_none());
    }

    #[test]
    fn revoking_a_device_invalidates_its_token() {
        let dir = tempfile::TempDir::new().unwrap();
        let pairing = PairingState::new(DeviceStore::new(dir.path().join("devices.json")));
        let code = pairing.create_code();
        let (device, token) = pairing.redeem(&code.code, "iPhone").unwrap();
        pairing.revoke(&device.id).unwrap();
        assert!(pairing.authenticate(&token).is_none());
    }
}
