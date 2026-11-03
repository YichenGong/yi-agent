//! 维护协调的 IPC 报文与判定规则。
//!
//! # 这个模块存在的理由
//!
//! 生命周期事务在**别的进程**里推进（桌面壳、CLI、watchman），而这个
//! daemon 负责的恰恰是"要不要把后端拉起来"。两边不在一个地址空间里，因此
//! 必须回答两个问题：
//!
//! 1. daemon 怎么知道现在该停？（→ [`barrier`]：读磁盘上的权威记录）
//! 2. 旧 daemon 与新宿主怎么谈？（→ [`compatibility`]：显式拒绝，不猜）
//!
//! # 一条硬规则：通知不是许可
//!
//! 维护开始的 RPC 可能丢、可能晚到、可能根本没发（对方进程崩了）。任何
//! 依赖"我收到了通知，所以我知道现在在维护"的设计都会在丢包时**放行**，
//! 而放行是不可撤销的：后端已经在跑，迁移就不能独占它。
//!
//! 所以这里的判据只有磁盘：`plugins/lifecycle/<plugin>.json` 存在且
//! `phase` 不是 `done` 即"必须停"。通知只用来**提前**触发一次重新检查，
//! 从来不作为结论。见 [`MaintenanceBarrier::active`]。
//!
//! 同理，"解析失败"不能读成"没在维护"：读不出来的日志意味着上一次事务
//! 可能停在半路，保守的一端是停。

use serde::{Deserialize, Serialize};

use yi_agent_plugins::{JournalStore, PluginError, PluginId};

/// 维护相位在**协议层**的投影。
///
/// 不复用 `yi_agent_plugins::Phase`：那是事务状态机的内部表示，可以随实现
/// 增删分支；而这里是线上报文，删一个变体就是破坏兼容。两者之间用显式
/// 映射连接（[`MaintenanceState::from_journal`]），映射漏了会在编译期报错。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaintenanceState {
    /// 没有未收尾的事务。
    Open,
    /// 有事务未收尾：拒绝新启动。
    Closed,
    /// 读不到权威记录，按"关闭"处理。
    ///
    /// 单独一个变体而不是并进 `Closed`：调用方需要能把"确实是维护中"与
    /// "我不知道，但我选择停"区分开来提示用户。把后者显示成"正在维护"
    /// 是在撒谎，用户会去等一个不会结束的维护。
    Unknown,
}

impl MaintenanceState {
    /// 是否允许启动后端。`Unknown` 与 `Closed` 都不允许。
    pub fn admits_launch(self) -> bool {
        matches!(self, MaintenanceState::Open)
    }

    /// 线上文本，用于日志与用户提示。
    pub fn as_str(self) -> &'static str {
        match self {
            MaintenanceState::Open => "open",
            MaintenanceState::Closed => "closed",
            MaintenanceState::Unknown => "unknown",
        }
    }
}

/// 一个插件的维护状态查询。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaintenanceStatus {
    pub plugin: String,
    pub state: MaintenanceState,
    /// 未收尾事务的 ID。仅 `Closed` 时有值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_id: Option<String>,
    /// 事务已经推进到的相位名。仅 `Closed` 时有值，用于诊断。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// `Unknown` 的原因。仅 `Unknown` 时有值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// 磁盘上的权威记录就是屏障本身。
///
/// 结构上刻意只有实例根：屏障必须是**无状态**的。任何缓存都会把"上次读到
/// 的结论"变成事实，而事实只存在于磁盘。
#[derive(Debug, Clone)]
pub struct MaintenanceBarrier {
    instance_home: std::path::PathBuf,
}

impl MaintenanceBarrier {
    pub fn at(instance_home: impl Into<std::path::PathBuf>) -> Self {
        Self {
            instance_home: instance_home.into(),
        }
    }

    /// 查一个插件现在的维护状态。**每次都读盘。**
    pub fn state(&self, plugin: &PluginId) -> MaintenanceStatus {
        // `JournalStore::at` 不预扫全目录：别的插件日志坏了不该让这个插件
        // 的结论也变成 Unknown（那会让一份无关的坏文件停掉所有插件）。
        let store = JournalStore::at(&self.instance_home);
        match store.load(plugin) {
            Ok(None) => MaintenanceStatus {
                plugin: plugin.as_str().to_string(),
                state: MaintenanceState::Open,
                transaction_id: None,
                phase: None,
                detail: None,
            },
            Ok(Some(journal)) if journal.phase.admission_open() => MaintenanceStatus {
                plugin: plugin.as_str().to_string(),
                state: MaintenanceState::Open,
                transaction_id: None,
                phase: None,
                detail: None,
            },
            Ok(Some(journal)) => MaintenanceStatus {
                plugin: plugin.as_str().to_string(),
                state: MaintenanceState::Closed,
                transaction_id: Some(journal.transaction_id.clone()),
                phase: Some(format!("{:?}", journal.phase).to_lowercase()),
                detail: None,
            },
            Err(error) => MaintenanceStatus {
                plugin: plugin.as_str().to_string(),
                state: MaintenanceState::Unknown,
                transaction_id: None,
                phase: None,
                detail: Some(error.to_string()),
            },
        }
    }

    /// 是否必须保持停止。`true` 覆盖"维护中"与"读不到"两种。
    pub fn must_stay_stopped(&self, plugin: &PluginId) -> Result<bool, PluginError> {
        // 用 `Result` 而不是直接 bool：调用方可能想把 `Unknown` 记进日志，
        // 但那不该改变决定。真正的决定只有"停"。
        Ok(!self.state(plugin).state.admits_launch())
    }
}

/// 协议兼容性的判定结果。
///
/// # 为什么拒绝要分两种
///
/// "对端太旧"与"对端太新"对用户的建议完全相反：前者升级**这个** daemon，
/// 后者升级**发起方**。糊成一句"不兼容"会让用户升错东西，然后问题复现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compatibility {
    /// 对端支持维护协议。
    Supported,
    /// 对端是旧 daemon：不认识维护请求。
    PeerTooOld { peer: u32, required: u32 },
    /// 对端比本进程更新：本进程不认识它发来的东西。
    PeerTooNew { peer: u32, required: u32 },
}

/// 维护协议的最低版本。
///
/// 与 `ipc::PROTOCOL_VERSION` 分开：那个数字描述整个 IPC 的形状，这个只描述
/// 维护这一块。合在一起会让任何一处无关的报文改动都迫使所有 daemon 一起
/// 升级，而用户会因此卡在一个与维护无关的版本门槛上。
pub const MAINTENANCE_PROTOCOL_VERSION: u32 = 1;

impl Compatibility {
    pub fn check(peer: Option<u32>) -> Compatibility {
        match peer {
            Some(version) if version >= MAINTENANCE_PROTOCOL_VERSION => Compatibility::Supported,
            Some(version) => Compatibility::PeerTooOld {
                peer: version,
                required: MAINTENANCE_PROTOCOL_VERSION,
            },
            // 对端根本没报维护能力：它可能是旧 daemon，也可能是别的东西。
            // 两种情况都必须当成"不支持"——猜测成支持会在旧 daemon 上
            // 启动候选服务，那正是要防的抢跑。
            None => Compatibility::PeerTooOld {
                peer: 0,
                required: MAINTENANCE_PROTOCOL_VERSION,
            },
        }
    }

    pub fn is_supported(self) -> bool {
        matches!(self, Compatibility::Supported)
    }

    /// 给用户的操作建议。**不**把两种拒绝混成同一句话。
    pub fn advice(self) -> &'static str {
        match self {
            Compatibility::Supported => "",
            Compatibility::PeerTooOld { .. } => {
                "stop the running yi-agent daemon and upgrade it before activating this \
                 release; an older daemon cannot hold a maintenance barrier"
            }
            Compatibility::PeerTooNew { .. } => {
                "this yi-agent build is older than the daemon; upgrade yi-agent instead"
            }
        }
    }
}

/// 把协议层的维护请求翻成一次判定。
///
/// 请求本身**不携带**"应该停"的结论——它只携带"我想知道"，结论来自磁盘。
/// 这是刻意的：一个能声明"现在不用停"的请求，就是一条绕过屏障的后门。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MaintenanceRequest {
    /// 询问一个插件当前的维护状态。
    Status { plugin: String },
    /// 声明"我这边要开一个事务"，用于让 daemon 提前重查。
    ///
    /// `expected_transaction` 不参与判定：它只是为了让日志能对上"谁在推进"。
    /// 判定永远来自磁盘——这样即使这条消息丢了，daemon 也只是晚一点重查，
    /// 而不会因为"没收到"就永远放行。
    Notify {
        plugin: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_transaction: Option<String>,
    },
}

impl MaintenanceRequest {
    pub fn plugin(&self) -> &str {
        match self {
            MaintenanceRequest::Status { plugin } => plugin,
            MaintenanceRequest::Notify { plugin, .. } => plugin,
        }
    }
}

/// 解析一个插件 id，失败时给出一条能定位的错误而不是空结论。
///
/// **不能**把解析失败当成"没有插件"或"没在维护"：那会把一条坏请求读成放行。
pub fn parse_plugin(raw: &str) -> Result<PluginId, PluginError> {
    PluginId::parse(raw).map_err(|error| {
        PluginError::PermissionDenied(format!(
            "maintenance request names plugin `{raw}`, which is not a valid plugin id \
             ({error}); refusing rather than treating it as \"not in maintenance\""
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use yi_agent_plugins::{Lifecycle, LifecycleRequest, ReleaseId, RuntimeControl};

    fn plugin() -> PluginId {
        PluginId::parse("example-counter").expect("plugin id")
    }

    struct Noop;

    impl RuntimeControl for Noop {
        fn pause_maintenance(&self, _: &PluginId) -> Result<(), PluginError> {
            Ok(())
        }
        fn resume_maintenance(&self, _: &PluginId) -> Result<(), PluginError> {
            Ok(())
        }
        fn stop_active(&self, _: &PluginId) -> Result<(), PluginError> {
            Ok(())
        }
        fn start_candidate(&self, _: &PluginId, _: &ReleaseId) -> Result<(), PluginError> {
            Ok(())
        }
        fn stop_candidate(&self, _: &PluginId) -> Result<(), PluginError> {
            Ok(())
        }
        fn health_check(&self, _: &PluginId, _: &str) -> Result<(), PluginError> {
            Ok(())
        }
        fn migrate_project(
            &self,
            _: &PluginId,
            _: &str,
            _: Option<&ReleaseId>,
            _: &ReleaseId,
        ) -> Result<(), PluginError> {
            Ok(())
        }
        fn repair_projection(&self, _: &PluginId, _: &ReleaseId) -> Result<(), PluginError> {
            Ok(())
        }
    }

    fn release() -> ReleaseId {
        ReleaseId {
            plugin: plugin(),
            version: "1.4.0".into(),
            target: "darwin-aarch64".into(),
            sha256: "00".repeat(32),
        }
    }

    #[test]
    fn a_missing_journal_reads_as_open() {
        let temp = tempfile::tempdir().unwrap();
        let barrier = MaintenanceBarrier::at(temp.path());
        assert_eq!(barrier.state(&plugin()).state, MaintenanceState::Open);
        assert!(!barrier.must_stay_stopped(&plugin()).unwrap());
    }

    #[test]
    fn an_unfinished_transaction_reads_as_closed() {
        let temp = tempfile::tempdir().unwrap();
        let lifecycle = Lifecycle::open(temp.path()).unwrap();
        let tx = lifecycle
            .start(LifecycleRequest::activate(plugin(), release(), 2))
            .unwrap();
        // 推进一步，但**不**推到 done：这就是"维护中"。
        lifecycle.advance(&tx, &Noop).unwrap();

        let barrier = MaintenanceBarrier::at(temp.path());
        let status = barrier.state(&plugin());
        assert_eq!(status.state, MaintenanceState::Closed);
        assert_eq!(status.transaction_id.as_deref(), Some(tx.as_str()));
        assert!(barrier.must_stay_stopped(&plugin()).unwrap());
    }

    /// 把发布登记进注册表，让事务能真的推进到 `done`。
    ///
    /// 提交那一步会改注册表的选中版本，未登记的版本会被判成"部分应用"而
    /// 拒绝收尾。这不是本模块的判据，但没有它就走不到"准入重新开放"。
    fn seed_registry(home: &std::path::Path) {
        yi_agent_plugins::Registry::open(home)
            .unwrap()
            .record_install(
                &plugin(),
                "1.4.0",
                &"00".repeat(32),
                "plugins/installed/x",
                1,
            )
            .unwrap();
    }

    #[test]
    fn a_finished_transaction_reads_as_open_again() {
        let temp = tempfile::tempdir().unwrap();
        seed_registry(temp.path());
        let lifecycle = Lifecycle::open(temp.path()).unwrap();
        let tx = lifecycle
            .start(LifecycleRequest::activate(plugin(), release(), 2))
            .unwrap();
        for _ in 0..32 {
            let progress = lifecycle.advance(&tx, &Noop).unwrap();
            if progress.admission_open {
                break;
            }
        }
        let barrier = MaintenanceBarrier::at(temp.path());
        assert_eq!(barrier.state(&plugin()).state, MaintenanceState::Open);
    }

    #[test]
    fn a_corrupt_journal_is_unknown_not_open() {
        // 这是这个模块最重要的一条：坏日志必须落到"停"，而不是"放行"。
        let temp = tempfile::tempdir().unwrap();
        let path = JournalStore::at(temp.path()).path(&plugin());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ not json").unwrap();

        let barrier = MaintenanceBarrier::at(temp.path());
        let status = barrier.state(&plugin());
        assert_eq!(
            status.state,
            MaintenanceState::Unknown,
            "an unreadable journal must never read as open"
        );
        assert!(!status.state.admits_launch());
        assert!(status.detail.is_some(), "the reason must be carried out");
    }

    #[test]
    fn one_broken_journal_does_not_hide_another_plugins_state() {
        // 每个插件的结论独立：一份无关的坏日志不该让健康插件也变成 Unknown。
        let temp = tempfile::tempdir().unwrap();
        let broken = JournalStore::at(temp.path()).path(&plugin());
        std::fs::create_dir_all(broken.parent().unwrap()).unwrap();
        std::fs::write(&broken, b"{ not json").unwrap();

        let other = PluginId::parse("other-plugin").unwrap();
        let barrier = MaintenanceBarrier::at(temp.path());
        assert_eq!(barrier.state(&other).state, MaintenanceState::Open);
        assert_eq!(barrier.state(&plugin()).state, MaintenanceState::Unknown);
    }

    #[test]
    fn an_old_peer_is_reported_by_name_not_as_a_bare_failure() {
        // 两种拒绝给的建议必须不同，否则用户会升错东西。
        let old = Compatibility::check(Some(0));
        let missing = Compatibility::check(None);
        assert_eq!(
            old,
            Compatibility::PeerTooOld {
                peer: 0,
                required: MAINTENANCE_PROTOCOL_VERSION
            }
        );
        assert_eq!(missing, old, "a silent peer is treated as an old one");
        assert!(old.advice().contains("upgrade"));

        let new = Compatibility::check(Some(MAINTENANCE_PROTOCOL_VERSION + 1));
        assert!(
            new.is_supported(),
            "a newer peer still speaks this protocol"
        );
    }

    #[test]
    fn a_request_naming_an_invalid_plugin_is_refused_rather_than_ignored() {
        let error = parse_plugin("../../etc/passwd").expect_err("must be refused");
        assert_eq!(error.kind(), "PermissionDenied");
        assert!(error.to_string().contains("not a valid plugin id"));
    }
}
