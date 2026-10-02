//! 扫码配对与设备 token。
//!
//! 一次性配对码(默认 5 分钟)换一枚**明文只出现一次**的设备 token;服务端只
//! 存哈希。撤销设备即删记录,该 token 立刻失效。

use std::io;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

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

/// 落盘的配对码文件形态。
#[derive(Debug, Default, Serialize, Deserialize)]
struct CodesFile {
    #[serde(default)]
    codes: Vec<StoredCode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredCode {
    code: String,
    expires_at: i64,
}

pub struct PairingState {
    store: DeviceStore,
    /// 待兑现的码落在这个文件里,使**同一台机器**上的任意 app-server 进程都能
    /// 兑换(桌面 stdio 进程铸码、`--relay`/`ws://` 进程兑换)——否则各自进程内的
    /// `HashMap` 互不相见,跨进程兑换恒 4401。
    codes_path: PathBuf,
    /// 进程内串行化 `codes_path` 的读-改-写。跨进程并发不在这把锁的范围内。
    codes_lock: Mutex<()>,
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
        // 配对码文件与设备表同目录:`<devices.json 所在目录>/pairing.json`。
        let codes_path = store.path().with_file_name("pairing.json");
        Self::with_codes_path(store, codes_path)
    }

    /// 显式注入配对码文件路径(测试用;也让调用方在需要时换目录)。
    pub fn with_codes_path(store: DeviceStore, codes_path: PathBuf) -> Self {
        Self {
            store,
            codes_path,
            codes_lock: Mutex::new(()),
        }
    }

    pub fn store(&self) -> &DeviceStore {
        &self.store
    }

    /// 读码文件,顺带剪掉已过期项(返回**未过期**的码)。
    fn read_codes(&self) -> CodesFile {
        let file: CodesFile = std::fs::read_to_string(&self.codes_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        let now = now_epoch_secs();
        CodesFile {
            codes: file
                .codes
                .into_iter()
                .filter(|c| c.expires_at > now)
                .collect(),
        }
    }

    fn write_codes(&self, file: &CodesFile) -> io::Result<()> {
        if let Some(parent) = self.codes_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_string_pretty(file).map_err(io::Error::other)?;
        let tmp = self.codes_path.with_extension("json.tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &self.codes_path)
    }

    /// 铸一枚一次性配对码。桌面端拿到后渲染二维码/明文。
    ///
    /// 落盘(不缓存进程内状态),这样**另一个进程**在 `?pair=` 兑换时能读到。
    pub fn create_code(&self) -> PairCode {
        let code = short_code();
        let expires_at = now_epoch_secs() + PAIR_CODE_TTL.as_secs() as i64;
        let _guard = self.codes_lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut file = self.read_codes();
        file.codes.push(StoredCode {
            code: code.clone(),
            expires_at,
        });
        if let Err(e) = self.write_codes(&file) {
            tracing::warn!(error = %e, "failed to persist pairing code; it will not be redeemable");
        }
        PairCode {
            code,
            expires_in: PAIR_CODE_TTL.as_secs(),
        }
    }

    /// 用配对码换设备 token。返回设备与**明文 token**;码用后即焚(从文件移除)。
    pub fn redeem(&self, code: &str, device_name: &str) -> Result<(Device, String), PairError> {
        {
            let _guard = self.codes_lock.lock().unwrap_or_else(|p| p.into_inner());
            // `read_codes` 已剪掉过期项:命中即未过期。
            let mut file = self.read_codes();
            let before = file.codes.len();
            file.codes.retain(|c| c.code != code);
            if file.codes.len() == before {
                return Err(PairError::InvalidCode);
            }
            // 码用后即焚:写回(已不含该码)。写失败则保守拒绝,绝不放行。
            self.write_codes(&file)
                .map_err(|_| PairError::InvalidCode)?;
        }

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

    /// 铸一台设备并落表,返回设备与**明文 token**。
    ///
    /// 生产的正常入口是 `create_code` + `redeem`;此私有用例服务于两类**本机**
    /// 客户端:测试里要造非默认 scope 的设备,以及 `--relay` 模式下本机中继桥
    /// 需要的一枚本地凭据。
    fn mint(&self, name: &str, scope: Scope) -> (Device, String) {
        let token = format!("yia_{}", short_code_secret());
        let now = now_epoch_secs();
        let device = Device {
            id: format!("dev-{}", uuid::Uuid::new_v4()),
            name: name.to_string(),
            scope,
            token_hash: hash_token(&token),
            created_at: now,
            last_seen_at: now,
        };
        self.store
            .add(device.clone())
            .expect("mint device must persist");
        (device, token)
    }

    /// 铸一枚**本机**设备凭据(scope = `Control`),返回明文 token。
    ///
    /// `--relay` 模式下,本机中继桥要作为 ws 客户端连**本机**的环回 app-server,
    /// 而该 server 仍是「无 token 即 4401」。用本方法在启动时取一枚本地 token,
    /// 好过把环回 ws 改成免认证(那会削弱「准入即认证」的网络路径不变量)。
    /// 与 spec §5.4「新配对设备默认 control」一致。
    pub fn seed_local_device(&self, name: &str) -> String {
        self.mint(name, Scope::Control).1
    }

    /// 仅供测试:直接铸一台设备并返回其 id 与明文 token。
    ///
    /// 生产只会经 `create_code` + `redeem` 铸设备;但测试要覆盖「已配对的
    /// Admin 设备」这类**非默认** scope(新配对设备恒为 Control)时,用配对码
    /// 换不出 Admin,故给一个直接落表的入口。
    #[cfg(test)]
    pub(crate) fn seed_device(&self, name: &str, scope: Scope) -> (Device, String) {
        self.mint(name, scope)
    }

    /// 仅供测试:塞入一枚**已过期**的配对码,以覆盖 `redeem` 的过期分支。
    /// 生产构建不含此方法。因码现在落盘,这里直接写文件。
    #[cfg(test)]
    fn insert_expired_code(&self, code: &str) {
        let _guard = self.codes_lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut file = self.read_codes();
        file.codes.push(StoredCode {
            code: code.to_string(),
            expires_at: now_epoch_secs() - 1,
        });
        self.write_codes(&file).expect("write expired code");
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
    fn an_expired_code_is_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        let pairing = PairingState::new(DeviceStore::new(dir.path().join("devices.json")));
        pairing.insert_expired_code("EXPD-1234");

        assert_eq!(
            pairing.redeem("EXPD-1234", "iPhone").unwrap_err(),
            PairError::InvalidCode
        );
        assert!(pairing.store().list().is_empty(), "过期码不得配出设备");
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

    /// 本期关键证明:桌面 stdio 进程铸码、`--relay`/`ws` 进程兑换,是两个进程、
    /// 两个 `PairingState` 实例,只共享 `devices.json` 与 `pairing.json`。用两个
    /// 实例模拟这条拓扑:实例 A 铸码,实例 B 兑换,再由 A 认证 token。
    #[test]
    fn a_code_survives_across_pairing_instances() {
        let dir = tempfile::TempDir::new().unwrap();
        let devices = dir.path().join("devices.json");

        // 进程 A:桌面 stdio,铸码。
        let desktop = PairingState::new(DeviceStore::new(devices.clone()));
        let code = desktop.create_code();

        // 进程 B:中继/ws,兑换(自己的实例,但同一份文件)。
        let relay = PairingState::new(DeviceStore::new(devices.clone()));
        let (device, token) = relay
            .redeem(&code.code, "iPhone 15")
            .expect("a code minted by another process must redeem");

        // token 落在共享设备表上,两个实例都能认证。
        assert_eq!(device.scope, Scope::Control);
        assert!(relay.authenticate(&token).is_some());
        assert!(desktop.authenticate(&token).is_some());
    }

    #[test]
    fn an_expired_code_is_rejected_after_persistence() {
        let dir = tempfile::TempDir::new().unwrap();
        let pairing = PairingState::new(DeviceStore::new(dir.path().join("devices.json")));
        // 用文件里的过期码(而非进程内状态)。
        pairing.insert_expired_code("EXPD-1234");

        assert_eq!(
            pairing.redeem("EXPD-1234", "iPhone").unwrap_err(),
            PairError::InvalidCode
        );
        assert!(pairing.store().list().is_empty(), "过期码不得配出设备");
    }

    #[test]
    fn redeem_removes_the_code_from_disk() {
        let dir = tempfile::TempDir::new().unwrap();
        let devices = dir.path().join("devices.json");
        let pairing = PairingState::new(DeviceStore::new(devices.clone()));
        let code = pairing.create_code();
        assert!(device_file_contains_code(&pairing.codes_path, &code.code));

        pairing.redeem(&code.code, "iPhone").unwrap();

        assert!(
            !device_file_contains_code(&pairing.codes_path, &code.code),
            "码用后即焚:文件里不得再有该码"
        );
        // 另一个进程也无法再用同一个码。
        let other = PairingState::new(DeviceStore::new(devices));
        assert!(other.redeem(&code.code, "again").is_err());
    }

    #[test]
    fn create_code_omits_expired_codes_when_writing() {
        let dir = tempfile::TempDir::new().unwrap();
        let pairing = PairingState::new(DeviceStore::new(dir.path().join("devices.json")));
        pairing.insert_expired_code("OLD1-0001");
        let fresh = pairing.create_code();
        // 过期码被剪掉,新码保留。
        assert!(!device_file_contains_code(&pairing.codes_path, "OLD1-0001"));
        assert!(device_file_contains_code(&pairing.codes_path, &fresh.code));
    }

    fn device_file_contains_code(path: &std::path::Path, code: &str) -> bool {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str::<CodesFile>(&s).ok())
            .map(|f| f.codes.iter().any(|c| c.code == code))
            .unwrap_or(false)
    }
}
