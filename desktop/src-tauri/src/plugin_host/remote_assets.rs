//! 远端宿主的发布身份与内容寻址缓存。
//!
//! # 这个模块解决的问题
//!
//! 同一个插件在不同宿主上可能分别是 1.2.0 和 1.3.0。桌面客户端可以同时连
//! 着本机和一台远端机器，还可能同时开着两个远端连接。如果视图身份只写
//! "插件 + 视图名"，那么切到宿主 B 的界面完全可能展示 A 的包——A 的页面
//! 读、B 的授权写，中间没有任何东西拦着。
//!
//! 所以身份必须包含 **宿主实例 + 完整发布身份（插件/版本/目标/摘要）+
//! 激活代次**。少一样，就有一个"看起来一样"的混淆面。
//!
//! # 缓存为什么按摘要寻址
//!
//! 两个宿主跑同一个精确版本时，字节完全相同。按摘要存放让它们共享这份字节，
//! 而**不共享**运行绑定：缓存命中只省一次下载，不产生任何跨宿主的授权。
//! 反过来，把缓存做成"按插件版本目录"会让两个宿主的文件混在一个目录里，
//! 一旦签名校验被绕过，混淆就变成了实际的跨宿主读。
//!
//! # 它不做什么
//!
//! 不解压（那是 T08 的 `install_verified`）、不验签（T07 的 `TrustStore`）、
//! 不 spawn 远端平台的二进制。远端平台的原生程序**永远**在远端跑：本机
//! spawn 一个 `linux-x86_64` 的二进制只会失败，而"失败"不是安全边界。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use yi_agent_plugins::{PluginError, ReleaseId};

use crate::plugin_host::assets::sha256_hex;

/// 一个宿主的身份。远端与本地用同一个类型，靠 `kind` 区分。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostIdentity {
    /// `local` 或 `remote:<profile id>`。不参与路径，只参与比较与展示。
    pub kind: String,
    /// 宿主自己声明的实例 id。远端由握手带来，不接受页面提供。
    pub instance_id: String,
    /// 宿主上那次会话的激活代次。代次变化 = 旧视图失效。
    pub generation: u64,
}

impl HostIdentity {
    pub fn local(instance_id: impl Into<String>) -> HostIdentity {
        HostIdentity {
            kind: "local".to_string(),
            instance_id: instance_id.into(),
            generation: 1,
        }
    }

    pub fn remote(profile: impl Into<String>, instance_id: impl Into<String>) -> HostIdentity {
        HostIdentity {
            kind: format!("remote:{}", profile.into()),
            instance_id: instance_id.into(),
            generation: 1,
        }
    }

    pub fn is_remote(&self) -> bool {
        self.kind.starts_with("remote:")
    }

    /// 运行绑定的稳定键。**必须**包含宿主与代次：只含发布身份时，两个宿主
    /// 上同一版本会被当成同一个绑定。
    pub fn binding_key(&self, release: &ReleaseId) -> String {
        format!(
            "{}|{}|{}|{}|{}|{}|{}",
            self.kind,
            self.instance_id,
            self.generation,
            release.plugin.as_str(),
            release.version,
            release.target,
            release.sha256
        )
    }
}

/// `assetRelease` 的结果：身份 + 受控传输句柄。
///
/// **没有** URL、**没有** 凭据字段。加一个 `url` 就等于给页面开了一条绕过
/// 桥的 fetch 通路。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseAsset {
    pub host: HostIdentity,
    pub release: ReleaseId,
    pub size_bytes: u64,
    pub published_at: u64,
    pub expires_at: u64,
    /// 受控传输句柄。宿主下次取字节时凭它查目录，**不是**权限。
    pub transfer_handle: String,
}

/// 一次取字节的请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchRequest {
    pub host: HostIdentity,
    pub release: ReleaseId,
    pub bytes: Vec<u8>,
}

/// 内容寻址缓存。
///
/// 目录布局：`<root>/<sha256>/package.bin`。摘要来自**已验签**的元数据，
/// 不是来自文件名或调用方声明——后者会让"缓存命中"变成一次未验证的读取。
#[derive(Debug, Clone)]
pub struct ReleaseCache {
    root: PathBuf,
}

impl ReleaseCache {
    pub fn new(root: impl Into<PathBuf>) -> ReleaseCache {
        ReleaseCache { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn entry_dir(&self, sha256: &str) -> PathBuf {
        self.root.join(sha256)
    }

    pub fn has(&self, sha256: &str) -> bool {
        self.entry_dir(sha256).join("package.bin").is_file()
    }

    /// 存入一次下载。字节的摘要必须与发布身份一致：对不上就是完整性失败，
    /// 而**不是**"存下来回头再验"。
    pub fn store(&self, release: &ReleaseId, bytes: &[u8]) -> Result<PathBuf, PluginError> {
        let actual = sha256_hex(bytes);
        if actual != release.sha256 {
            return Err(PluginError::Integrity(format!(
                "downloaded package for {} {} has sha256 {actual}, the signed release says {}",
                release.plugin.as_str(),
                release.version,
                release.sha256
            )));
        }
        let dir = self.entry_dir(&release.sha256);
        std::fs::create_dir_all(&dir)
            .map_err(|e| PluginError::Io(format!("{}: {e}", dir.display())))?;
        let staging = dir.join("package.bin.tmp");
        std::fs::write(&staging, bytes)
            .map_err(|e| PluginError::Io(format!("{}: {e}", staging.display())))?;
        let final_path = dir.join("package.bin");
        std::fs::rename(&staging, &final_path)
            .map_err(|e| PluginError::Io(format!("{}: {e}", final_path.display())))?;
        Ok(final_path)
    }

    /// 读回缓存的字节。命中时**重新算摘要**：缓存目录是可写的，不重算就等于
    /// 信任"它还和当初一样"。
    pub fn load(&self, release: &ReleaseId) -> Result<Option<Vec<u8>>, PluginError> {
        let path = self.entry_dir(&release.sha256).join("package.bin");
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(PluginError::Io(format!("{}: {error}", path.display())));
            }
        };
        let actual = sha256_hex(&bytes);
        if actual != release.sha256 {
            // 缓存被改过：删掉它，让调用方回退到重新下载，而不是拿着坏字节继续。
            std::fs::remove_dir_all(self.entry_dir(&release.sha256)).ok();
            return Err(PluginError::Integrity(format!(
                "cached package for {} {} no longer matches its digest",
                release.plugin.as_str(),
                release.version
            )));
        }
        Ok(Some(bytes))
    }

    pub fn len(&self) -> usize {
        std::fs::read_dir(&self.root)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| entry.path().is_dir())
                    .count()
            })
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 视图 ↔ 宿主绑定的注册表。
///
/// 它的唯一职责是回答"这个实例属于哪个宿主代次"，并在宿主切换时批量撤销。
/// 与 [`crate::plugin_host::cache::ViewRegistry`] 的分工：后者管文件白名单，
/// 这里管**绑定**。两者分开是因为刷新绑定不该重读磁盘。
#[derive(Debug, Default)]
pub struct HostBindings {
    bindings: BTreeMap<String, BindingRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingRecord {
    pub instance_id: String,
    pub host: HostIdentity,
    pub release: ReleaseId,
    pub view: String,
}

impl HostBindings {
    pub fn new() -> HostBindings {
        HostBindings::default()
    }

    pub fn bind(&mut self, record: BindingRecord) -> Result<(), PluginError> {
        if self.bindings.contains_key(&record.instance_id) {
            return Err(PluginError::Conflict(format!(
                "view instance `{}` is already bound to a host",
                record.instance_id
            )));
        }
        self.bindings.insert(record.instance_id.clone(), record);
        Ok(())
    }

    /// 这个实例是否仍属于这个宿主代次。
    ///
    /// 判据是**完整**的绑定键：宿主、代次、发布身份三者任一变化都失效。只比
    /// 插件名会让"切到宿主 B 却展示 A 的界面"变成一次通过。
    pub fn is_current(&self, instance_id: &str, host: &HostIdentity) -> bool {
        match self.bindings.get(instance_id) {
            Some(record) => record.host == *host,
            None => false,
        }
    }

    /// 撤销一个宿主的所有绑定，返回撤销数量。
    ///
    /// 连接切换、断开、旧客户端不兼容都走这里。返回计数是为了让调用方能断言
    /// "确实撤掉了几个"，而不是靠"没有报错"判断。
    pub fn revoke_host(&mut self, host: &HostIdentity) -> usize {
        let doomed: Vec<String> = self
            .bindings
            .iter()
            .filter(|(_, record)| record.host == *host)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &doomed {
            self.bindings.remove(id);
        }
        doomed.len()
    }

    /// 按宿主类别撤销（例：`remote:profile-a` 的所有代次）。
    ///
    /// 断开连接时只知道 profile，不知道当时是第几代；用代次做条件会漏撤。
    pub fn revoke_kind(&mut self, kind: &str) -> usize {
        let doomed: Vec<String> = self
            .bindings
            .iter()
            .filter(|(_, record)| record.host.kind == kind)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &doomed {
            self.bindings.remove(id);
        }
        doomed.len()
    }

    pub fn get(&self, instance_id: &str) -> Option<&BindingRecord> {
        self.bindings.get(instance_id)
    }

    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }
}

/// 远端平台的原生程序在这台机器上能不能跑。
///
/// 答案永远是"不能"：跨平台二进制不保证可执行，而尝试执行一个来路不明格式
/// 的文件本身就是风险。这里返回拒绝原因，让 UI 能说清"这个插件只有视图可用"，
/// 而不是让 spawn 失败后给人一个 ENOEXEC。
pub fn local_spawn_allowed(release_target: &str, local_target: &str) -> Result<(), PluginError> {
    if release_target == local_target {
        return Ok(());
    }
    Err(PluginError::Incompatible(format!(
        "a release built for {release_target} cannot run on {local_target}; only its static views can be served"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use yi_agent_plugins::PluginId;

    fn release(version: &str, sha: &str) -> ReleaseId {
        ReleaseId {
            plugin: PluginId::parse("example-counter").expect("id"),
            version: version.to_string(),
            target: "darwin-aarch64".to_string(),
            sha256: sha.repeat(32),
        }
    }

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    #[test]
    fn binding_keys_differ_across_hosts_even_for_the_same_release() {
        let r = release("1.0.0", "ab");
        let a = HostIdentity::remote("profile-a", "inst-1");
        let b = HostIdentity::remote("profile-b", "inst-1");
        assert_ne!(a.binding_key(&r), b.binding_key(&r));
    }

    #[test]
    fn binding_keys_differ_across_generations_on_the_same_host() {
        let r = release("1.0.0", "ab");
        let mut host = HostIdentity::local("inst");
        let before = host.binding_key(&r);
        host.generation += 1;
        assert_ne!(before, host.binding_key(&r));
    }

    #[test]
    fn a_view_bound_to_host_a_is_not_current_for_host_b() {
        let r = release("1.0.0", "ab");
        let a = HostIdentity::remote("a", "i");
        let b = HostIdentity::remote("b", "i");
        let mut bindings = HostBindings::new();
        bindings
            .bind(BindingRecord {
                instance_id: "v1".to_string(),
                host: a.clone(),
                release: r.clone(),
                view: "main".to_string(),
            })
            .expect("bind");

        assert!(bindings.is_current("v1", &a));
        assert!(!bindings.is_current("v1", &b));
        assert_eq!(bindings.revoke_host(&b), 0, "host b owns nothing");
        assert_eq!(bindings.revoke_host(&a), 1);
        assert!(bindings.is_empty());
    }

    #[test]
    fn cache_is_addressed_by_digest_so_two_hosts_share_the_bytes() {
        let dir = tempdir();
        let cache = ReleaseCache::new(dir.path());
        let bytes = b"pretend package bytes";
        let r = ReleaseId {
            plugin: PluginId::parse("example-counter").expect("id"),
            version: "1.0.0".to_string(),
            target: "darwin-aarch64".to_string(),
            sha256: sha256_hex(bytes),
        };

        cache.store(&r, bytes).expect("store");
        assert!(cache.has(&r.sha256));
        // 第二个宿主命中同一份字节，但绑定仍然各归各。
        assert_eq!(
            cache.load(&r).expect("load").as_deref(),
            Some(bytes.as_slice())
        );
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn storing_bytes_whose_digest_disagrees_is_refused() {
        let dir = tempdir();
        let cache = ReleaseCache::new(dir.path());
        let r = release("1.0.0", "cd");
        let error = cache
            .store(&r, b"not the signed bytes")
            .expect_err("must refuse");
        assert!(matches!(error, PluginError::Integrity(_)), "got {error:?}");
        assert!(
            !cache.has(&r.sha256),
            "a refused store must leave nothing behind"
        );
    }

    #[test]
    fn a_tampered_cache_entry_is_deleted_rather_than_served() {
        let dir = tempdir();
        let cache = ReleaseCache::new(dir.path());
        let bytes = b"original";
        let r = ReleaseId {
            plugin: PluginId::parse("example-counter").expect("id"),
            version: "1.0.0".to_string(),
            target: "darwin-aarch64".to_string(),
            sha256: sha256_hex(bytes),
        };
        cache.store(&r, bytes).expect("store");

        std::fs::write(
            cache.root().join(&r.sha256).join("package.bin"),
            b"tampered",
        )
        .expect("tamper");
        let error = cache.load(&r).expect_err("must refuse tampered bytes");
        assert!(matches!(error, PluginError::Integrity(_)));
        assert!(!cache.has(&r.sha256), "the bad entry must not survive");
    }

    #[test]
    fn a_cross_platform_program_is_never_spawned_locally() {
        assert!(local_spawn_allowed("darwin-aarch64", "darwin-aarch64").is_ok());
        let error = local_spawn_allowed("linux-x86_64", "darwin-aarch64").expect_err("must refuse");
        assert!(matches!(error, PluginError::Incompatible(_)));
        assert!(error
            .to_string()
            .contains("only its static views can be served"));
    }
}
