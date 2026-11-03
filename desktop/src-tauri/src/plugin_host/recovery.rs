//! 原生恢复：容器死掉之后怎么办。
//!
//! # 三种"死掉"，只有两种能自动恢复
//!
//! 1. **页面被脚本/资源问题搞坏**（假死、白屏、渲染进程崩溃）——可以
//!    `close_and_rebuild`：销毁容器、重建窗口、重新加载入口。G0 已确认接受
//!    "丢弃页面未持久化的状态"。
//! 2. **安装本身坏了**（文件被替换、入口不见了）——重建窗口无济于事，因为
//!    读的还是同一个坏目录。必须报 [`RecoveryDecision::NeedsReinstall`]。
//! 3. **句柄陈旧**（升级/回滚之后旧代次的视图）——重建到新代次是宿主主动行为，
//!    不属于"恢复"；这里只报告"需要重新打开"。
//!
//! # 为什么恢复要有上限
//!
//! 一个每次加载都崩溃的包会变成无限重建循环，把机器拖垮。所以每次恢复都记账；
//! 连续失败超过阈值就停下并报告，让人来看。

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use yi_agent_plugins::{CommittedActivation, PluginError};

use crate::plugin_host::assets::AssetCache;
use crate::plugin_host::container::{ViewInstance, ViewRejection};

/// 一次恢复尝试的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryDecision {
    /// 销毁并重建容器，重新加载同一个入口。
    CloseAndRebuild,
    /// 安装本身不可用，重建也没用。
    NeedsReinstall(String),
    /// 句柄陈旧：需要宿主按当前激活重新打开。
    Reopen,
    /// 连续失败太多，停止自动恢复。
    GiveUp { attempts: u32 },
}

/// 恢复记账。按实例 id 记，实例关闭后移除。
#[derive(Debug, Default)]
pub struct RecoveryBook {
    attempts: BTreeMap<String, Attempts>,
    /// 连续失败多少次后放弃。
    limit: u32,
}

#[derive(Debug, Clone)]
struct Attempts {
    since: Instant,
    count: u32,
}

impl RecoveryBook {
    pub fn new(limit: u32) -> RecoveryBook {
        RecoveryBook {
            attempts: BTreeMap::new(),
            limit: limit.max(1),
        }
    }

    /// 记一次失败并给出下一步。
    ///
    /// 传 `now` 而不是读系统时钟：测试要能在不 sleep 的情况下推进时间。
    pub fn record_failure(&mut self, instance: &str, now: Instant) -> RecoveryDecision {
        let entry = self
            .attempts
            .entry(instance.to_string())
            .or_insert(Attempts {
                since: now,
                count: 0,
            });
        // 计数窗口：一分钟内连续失败才算"同一轮"。
        if now.duration_since(entry.since) > Duration::from_secs(60) {
            entry.since = now;
            entry.count = 0;
        }
        entry.count += 1;
        if entry.count > self.limit {
            RecoveryDecision::GiveUp {
                attempts: entry.count,
            }
        } else {
            RecoveryDecision::CloseAndRebuild
        }
    }

    /// 一次成功加载清零计数。
    pub fn record_success(&mut self, instance: &str) {
        self.attempts.remove(instance);
    }

    pub fn forget(&mut self, instance: &str) {
        self.attempts.remove(instance);
    }
}

/// 判断一个视图实例现在能不能加载，不能的话下一步做什么。
///
/// 顺序很重要：先判句柄是否陈旧（那是宿主状态问题），再判安装是否完整
/// （那是磁盘问题）。反过来会把"升级了"报成"包坏了"，用户会去重装一个
/// 本来好好的插件。
pub fn diagnose(
    instance: &ViewInstance,
    committed: Option<&CommittedActivation>,
    cache: &AssetCache,
) -> Result<(), (ViewRejection, RecoveryDecision)> {
    if !instance.is_current(committed) {
        let current = committed.map(|active| active.activation);
        return Err((
            ViewRejection::StaleGeneration {
                bound: instance.activation,
                current,
            },
            RecoveryDecision::Reopen,
        ));
    }
    if cache.resolve(&instance.entry).is_none() {
        return Err((
            ViewRejection::BrokenInstall(format!(
                "the entry `{}` is not present in the installation",
                instance.entry
            )),
            RecoveryDecision::NeedsReinstall(
                "the view entry disappeared after installation".to_string(),
            ),
        ));
    }
    Ok(())
}

/// 把安装期的完整性错误翻译成恢复结论。
///
/// `verify_installed` 的失败意味着**磁盘上的字节已经不是被验证过的那些**。
/// 这种状态下重建容器只会再读一遍坏字节。
pub fn decision_for_integrity(error: &PluginError) -> RecoveryDecision {
    match error {
        PluginError::Integrity(detail) => RecoveryDecision::NeedsReinstall(detail.clone()),
        other => RecoveryDecision::NeedsReinstall(other.to_string()),
    }
}
