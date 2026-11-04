//! 整机配额域。
//!
//! # 为什么要显式注入而不是自己找
//!
//! 配额是**整机**的：用户只有一个 provider 账号，两个实例各自看到"还有
//! 余量"就会合计超限。所以路径必须由宿主注入，且与配置共用同一个域。
//!
//! 这里**绝不**在凭据缺失时回退到 instance_home 下自建一个池：那会让新
//! 实例悄悄得到一份完整预算，而用户的账单不会因此变少。
//!
//! # 修订
//!
//! 共享配置是多个实例共同读写的文件。任何一方要覆盖它之前必须确认修订
//! 没有前进（见 G2 §6.4）：事务开始时的修订与当前不同，说明别人改过，
//! 回退会把他的新设置抹掉。
//!
//! # `sessions.running` 不是整机值
//!
//! 宿主上报的 `sessions.running` 是**本进程**的活动会话数。把两个实例的
//! 上报值直接相加会在两者都接近上限时突破总配额；把它当整机值则会让每个
//! 实例都以为池子还空着。正确口径是：已登记活 owner 的租约合计，加上
//! 本进程自己的上报，且**同一租约不双扣**。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::PluginError;
use crate::ownership::ProjectOwnership;

/// 宿主注入的配额上下文。**独立于 instance_home**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaContext {
    /// 共享配置路径（并发窗口、峰值）。
    pub config_path: PathBuf,
    /// 共享租约槽位目录。
    pub leases_path: PathBuf,
    /// 读取时观察到的配置修订。
    pub revision: u64,
}

impl QuotaContext {
    /// 从一对路径构造，并要求它们属于**同一个**配额域。
    ///
    /// 配置与租约分属两个域是真实的错误：一个实例读 A 的配置、抢 B 的
    /// 槽位，两个实例都"遵守"了自己的上限，合计仍然超。
    pub fn new(
        config_path: PathBuf,
        leases_path: PathBuf,
        revision: u64,
    ) -> Result<Self, PluginError> {
        let config_domain = domain_of(&config_path)?;
        let lease_domain = domain_of(&leases_path)?;
        if config_domain != lease_domain {
            return Err(PluginError::Conflict(format!(
                "quota config lives under `{}` but leases live under `{}`; one quota domain \
                 needs both, or two instances each honour their own limit and together overrun it",
                config_domain.display(),
                lease_domain.display()
            )));
        }
        Ok(QuotaContext {
            config_path,
            leases_path,
            revision,
        })
    }

    pub fn domain(&self) -> Result<PathBuf, PluginError> {
        domain_of(&self.config_path)
    }
}

/// 配额域的根：`.yi-agent/` 下**任何一个**具名子目录，例如
/// `<home>/.yi-agent/<plugin-name>`。
///
/// 判据是**结构**（`.yi-agent/<name>` 这个两段形状），不是某个插件的名字：
/// 宿主不认识任何具体插件，第二个插件要复用整机配额域时不需要改这里。
/// 取 `.yi-agent` 的直接子目录而不是"上两级"，是因为 `leases`、`quota.json`、
/// `activity` 在域内的相对位置可以不同，但它们必须在同一个具名目录下。
fn domain_of(path: &Path) -> Result<PathBuf, PluginError> {
    for candidate in path.ancestors() {
        let Some(name) = candidate.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        // `.yi-agent/<name>`：candidate 是 `<name>`，它的父目录必须叫 `.yi-agent`。
        let parent_is_agent_dir = candidate
            .parent()
            .and_then(|parent| parent.file_name())
            .and_then(|parent| parent.to_str())
            == Some(".yi-agent");
        if parent_is_agent_dir && !name.is_empty() {
            return Ok(candidate.to_path_buf());
        }
    }
    Err(PluginError::Conflict(format!(
        "{} is not inside a `.yi-agent/<name>` quota domain; refusing to guess one, because \
         an invented domain hands this instance its own full budget",
        path.display()
    )))
}

/// 共享配置的内容。字段名与 G3 的看板设置一致。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SharedQuotaConfig {
    pub revision: u64,
    /// 整机同时运行上限。
    pub max_concurrent: u32,
    /// 整机每小时启动峰值。
    pub max_starts_per_hour: u32,
}

/// 共享配额配置的读写句柄。
#[derive(Debug, Clone)]
pub struct SharedQuota {
    context: QuotaContext,
}

impl SharedQuota {
    pub fn new(context: QuotaContext) -> SharedQuota {
        SharedQuota { context }
    }

    pub fn context(&self) -> &QuotaContext {
        &self.context
    }

    /// 读当前配置。不存在返回 `None`——首次使用时由用户显式创建，
    /// 不是由某个实例替所有实例决定。
    pub fn read(&self) -> Result<Option<SharedQuotaConfig>, PluginError> {
        let bytes = match std::fs::read(&self.context.config_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(PluginError::Io(format!(
                    "{}: {error}",
                    self.context.config_path.display()
                )));
            }
        };
        serde_json::from_slice(&bytes).map(Some).map_err(|error| {
            PluginError::RecoveryRequired(format!(
                "shared quota config at {} is unreadable ({error}); guessing a limit would \
                     let this instance run on a budget nobody chose",
                self.context.config_path.display()
            ))
        })
    }

    /// 当前修订。文件不存在视为 0 —— "从未有人写过"与"写过第 0 版"在
    /// 回退判断里等价。
    pub fn revision(&self) -> Result<u64, PluginError> {
        Ok(self.read()?.map(|config| config.revision).unwrap_or(0))
    }

    /// 覆盖写。`expected_revision` 不符则 `Conflict`，**不覆盖**。
    pub fn write(
        &self,
        expected_revision: u64,
        next: &SharedQuotaConfig,
    ) -> Result<(), PluginError> {
        let current = self.revision()?;
        if current != expected_revision {
            return Err(PluginError::Conflict(format!(
                "shared quota config is at revision {current}, not {expected_revision}; \
                 another instance changed it and overwriting would discard their limit"
            )));
        }
        if next.revision != expected_revision + 1 {
            return Err(PluginError::Conflict(format!(
                "a write must advance the revision to {}, not {}",
                expected_revision + 1,
                next.revision
            )));
        }
        let parent = self
            .context
            .config_path
            .parent()
            .expect("quota config path has a parent");
        std::fs::create_dir_all(parent)?;
        let bytes = serde_json::to_vec_pretty(next)
            .map_err(|e| PluginError::Io(format!("could not serialize quota config: {e}")))?;
        crate::journal::write_atomic(parent, &self.context.config_path, &bytes)
    }

    /// 这个实例现在允许再起几个。
    ///
    /// `observed` 是跨实例的活动观测；`own` 是本进程自己的在读会话。
    /// 两者可能重叠（本进程的会话也是某个 owner 的租约），所以取
    /// `observed` 与 `own` 的较大值而不是相加——相加会双扣。
    pub fn remaining(&self, observed: &ActivityObservation, own: u32) -> Result<u32, PluginError> {
        let Some(config) = self.read()? else {
            // 没有配置 = 没有人授权过整机配额。默认拒绝新自动任务，
            // 而不是假设"没有上限"。
            return Ok(0);
        };
        let in_use = observed.total().max(own);
        Ok(config.max_concurrent.saturating_sub(in_use))
    }
}

/// 跨实例活动观测。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActivityObservation {
    /// 已登记活跃 owner 的租约数，按 owner 去重。
    per_owner: Vec<(String, u32)>,
    /// 观测范围是否只覆盖本实例。UI 必须显示它，否则"整机还有余量"
    /// 会是一句假话。
    pub scope: ObservationScope,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ObservationScope {
    /// 汇总了所有已登记活 owner：这是整机值。
    Machine,
    /// 只有本实例的数据：老宿主没上报，不能当整机值。
    #[default]
    InstanceOnly,
}

impl ActivityObservation {
    pub fn new(scope: ObservationScope) -> ActivityObservation {
        ActivityObservation {
            per_owner: Vec::new(),
            scope,
        }
    }

    /// 登记一个 owner 的活跃租约数。
    ///
    /// 同一 owner 出现两次时**取较大值**，不累加：重复登记通常意味着
    /// 有人在两个地方报了同一批会话，累加会把它算成两倍。
    pub fn record(&mut self, owner: impl Into<String>, active: u32) {
        let owner = owner.into();
        match self.per_owner.iter_mut().find(|(name, _)| name == &owner) {
            Some((_, existing)) => *existing = (*existing).max(active),
            None => self.per_owner.push((owner, active)),
        }
    }

    pub fn total(&self) -> u32 {
        self.per_owner.iter().map(|(_, active)| active).sum()
    }

    /// 这个观测能不能用来判断"整机还有余量"。
    pub fn is_machine_wide(&self) -> bool {
        self.scope == ObservationScope::Machine
    }
}

/// 把一个 owner 的存活锁转换成"活跃"。锁不在 → 0，不是"未知"。
pub fn active_for(
    ownership: &ProjectOwnership,
    owner: impl Into<String>,
) -> Result<u32, PluginError> {
    let owner = owner.into();
    Ok(if ownership.is_alive(&owner)? { 1 } else { 0 })
}

/// 一个空的配额域检查：宿主决定是否允许起新的自动任务。
///
/// 无法定位共享域时返回 `None`，调用方必须**阻止**新自动任务，而不是
/// 退化成按项目限流（设计 §7.1）。`service.rs:490`/`:682` 的现有行为
/// 正是拒绝启动，这里把这个结论固化成一个可测的谓词。
pub fn resolve_domain(config_path: &Path, leases_path: &Path) -> Option<PathBuf> {
    QuotaContext::new(config_path.to_path_buf(), leases_path.to_path_buf(), 0)
        .ok()
        .and_then(|context| context.domain().ok())
}
