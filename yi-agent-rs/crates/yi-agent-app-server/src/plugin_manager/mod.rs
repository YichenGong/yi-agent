//! 会话与插件发布之间的绑定：一个**具体的**版本，不是一个插件名。
//!
//! # 为什么需要这一层
//!
//! 会话是长命的，插件发布是短命的。用户在 1.4.0 上开了十几个会话，然后插件
//! 升到 1.5.0。此时有三条路，只有一条是对的：
//!
//! 1. **旧会话继续调 1.5.0** —— 错。旧会话的上下文、权限授予、历史产物都是在
//!    1.4.0 下产生的；把它们接到一个没在这里验证过的版本上，等于让新版本无审
//!    计地继承旧的信任。
//! 2. **旧会话静默失效** —— 错。用户看到的是一句"插件坏了"，而真实原因只是
//!    插件升级过，需要重建会话。
//! 3. **旧会话保留 1.4.0 的绑定，调用被明确判为陈旧** —— 对。会话知道自己
//!    绑的是哪个发布，宿主能说清"你绑的是 1.4.0，现在是 1.5.0，请重建会话"。
//!
//! 所以 `ThreadMeta::plugin_bindings` 存的是完整的 [`Binding`]（含 `activation`
//! 代次与 `release` 摘要），而本模块只做一件事：把「会话以为的」与「磁盘上
//! 已提交的」比出来，给出一个**能讲清楚原因**的结论。
//!
//! # 为什么判定要读磁盘
//!
//! 陈旧与否只由 `Lifecycle` 里已提交的激活决定。若缓存一个"当前版本"，进程
//! 重启、或另一个实例完成了一次激活/回滚，缓存就和事实脱节了。这里每次都读。

use std::path::Path;

use yi_agent_plugins::{Binding, Effect, Lifecycle, PluginError};
use yi_agent_supervisors::managed::{self, ManagedInstall};

/// 一个会话绑定的插件，现在还能不能用来干活。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingState {
    /// 绑定指向的正是已提交的激活，可以用。
    Current,
    /// 绑定陈旧：插件还在，但已提交的激活换了。
    ///
    /// 带上是哪个版本、现在是什么版本，因为用户下一步要做的动作（重建会话）
    /// 只有在知道"我绑的是旧的"时才说得通。
    Stale {
        bound_activation: u64,
        bound_version: String,
        committed_activation: u64,
        committed_version: String,
    },
    /// 这个插件已经没有任何已提交的激活（被回滚掉、或卸载了）。
    ///
    /// 与 [`BindingState::Stale`] 分开：陈旧是"换个版本还能用"，这里是
    /// "已经没有能用的版本"，用户该做的是安装/激活，而不是重建会话。
    Gone,
}

/// 判定一个绑定现在的状态。
///
/// `instance_home` 是宿主控制的实例根；`binding` 是会话里存下来的那一条。
///
/// # 为什么把 `also_require_install` 单独拎出来说
///
/// 一个绑定的代次对得上，但它指向的安装已经被 GC 掉时，`Lifecycle` 仍会
/// 说"当前"。此时说 `Current` 会让调用方以为可以启动，实际一启动就
/// `NotFound`。所以这里额外核一次 [`managed::resolve`]：**能真正解析出可启动
/// 目标**才算 `Current`。
pub fn binding_state(instance_home: &Path, binding: &Binding) -> Result<BindingState, PluginError> {
    let lifecycle = Lifecycle::open(instance_home)?;
    let committed = lifecycle.committed(&binding.release.plugin)?;

    let Some(committed) = committed else {
        return Ok(BindingState::Gone);
    };

    if committed.activation != binding.activation {
        return Ok(BindingState::Stale {
            bound_activation: binding.activation,
            bound_version: binding.release.version.clone(),
            committed_activation: committed.activation,
            committed_version: committed.release.version.clone(),
        });
    }

    // 代次对得上，但版本/摘要必须也一致：同一代次在合法路径上不会换版本，
    // 若换过（被人为改写、或磁盘损坏），按陈旧处理比按"当前"处理安全。
    if committed.release.version != binding.release.version
        || committed.release.sha256 != binding.release.sha256
    {
        return Ok(BindingState::Stale {
            bound_activation: binding.activation,
            bound_version: binding.release.version.clone(),
            committed_activation: committed.activation,
            committed_version: committed.release.version.clone(),
        });
    }

    // 最后确认安装仍能解析成可启动目标。解析失败分两种：
    // - `NotFound`：安装被清掉了 -> `Gone`（用户要重装，不是重建会话）
    // - 其它（摘要不符、清单不符）：完整性坏了 -> 让错误冒上去，不要伪装成状态
    match managed::resolve(instance_home, &binding.release.plugin) {
        Ok(ManagedInstall { .. }) => Ok(BindingState::Current),
        Err(PluginError::NotFound(_)) => Ok(BindingState::Gone),
        Err(other) => Err(other),
    }
}

/// 会话与插件的绑定集合，以及基于它的一次性判定结果。
///
/// 不持有 `Lifecycle` 句柄：每次判定重新打开，避免长命进程读到旧快照。
#[derive(Debug, Clone, Default)]
pub struct SessionBindings {
    bindings: Vec<Binding>,
}

impl SessionBindings {
    pub fn new(bindings: Vec<Binding>) -> Self {
        Self { bindings }
    }

    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Binding> {
        self.bindings.iter()
    }

    /// 找一个能用于 `plugin` 的绑定。
    ///
    /// 一个会话理论上只绑一个插件的一个发布，但 `Vec` 允许将来并行绑多个。
    /// 找不到返回 `None`（这个会话根本没绑这个插件）。
    pub fn find(&self, plugin: &str) -> Option<&Binding> {
        self.bindings
            .iter()
            .find(|binding| binding.release.plugin.as_str() == plugin)
    }

    /// 判定 `plugin` 是否可用于一次 `effect` 调用。
    ///
    /// 依次做三件事，顺序不能换：
    /// 1. 会话有没有这个插件的绑定（没有 = 不是插件会话，拒绝）
    /// 2. 绑定是否仍是当前已提交的激活（不是 = `StaleBinding`，说清版本）
    /// 3. 能力是否覆盖 `effect`（不覆盖 = `PermissionDenied`）
    ///
    /// 第 2 步必须早于第 3 步：一个陈旧绑定上"有没有权限"是无意义的问题，
    /// 先答它会让用户以为问题出在权限上。
    pub fn check(
        &self,
        instance_home: &Path,
        plugin: &str,
        effect: Effect,
    ) -> Result<&Binding, PluginError> {
        let binding = self.find(plugin).ok_or_else(|| {
            PluginError::StaleBinding(format!(
                "this session is not bound to plugin `{plugin}`; start a plugin session to use it"
            ))
        })?;

        match binding_state(instance_home, binding)? {
            BindingState::Current => {}
            BindingState::Stale {
                bound_activation,
                bound_version,
                committed_activation,
                committed_version,
            } => {
                return Err(PluginError::StaleBinding(format!(
                    "this session is bound to {}/{} (activation {bound_activation}), but plugin \
                     `{plugin}` has {}/{} (activation {committed_activation}) committed; \
                     start a new session to pick up the new release",
                    bound_version, bound_activation, committed_version, committed_activation
                )));
            }
            BindingState::Gone => {
                return Err(PluginError::StaleBinding(format!(
                    "this session is bound to {}/{}, which is no longer committed for plugin \
                     `{plugin}`; install and activate a release, then start a new session",
                    binding.release.version, binding.activation
                )));
            }
        }

        // 两条**不同**的检查，缺一不可，且顺序是先授权后生命周期：
        //
        // - `grants.check`：这个项目**有没有被授予**该能力，以及授权代次是否
        //   还对得上。它读 `grants.json`。
        // - `Lifecycle::check_binding`：这个激活是否已提交、是否还在候选、
        //   有没有未收尾事务。它读生命周期状态与日志。
        //
        // 二者读的文件不同，任何一条都不能代替另一条。实测反例：只跑
        // `check_binding` 时，一个只授予 `Read` 的项目上 `Write` 会被放行
        // （`check_binding` 不认识授权表）。反过来只跑 `grants.check` 时，
        // 一个候选代次会被当成可用（授权表不知道代次是否已提交）。
        let table = yi_agent_supervisors::managed::grants::load(instance_home)?;
        table.check(
            plugin,
            &binding.project,
            effect,
            &binding.host_id,
            binding.grant,
        )?;

        Lifecycle::open(instance_home)?.check_binding(binding, effect)?;
        Ok(binding)
    }
}

#[cfg(test)]
mod tests;
