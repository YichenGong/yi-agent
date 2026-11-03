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

/// 为一个**新**会话算出的插件计划：绑哪个发布、看哪些 skill。
///
/// 这两件事由同一个函数给出，而不是让调用方各算一遍：绑定与 skill 来源必须
/// 指向**同一个**发布。分开算的典型错误是先取"最新版本"给 skill（于是新会话
/// 看到 1.5.0 的 skill），再取"已提交激活"写绑定（仍停在 1.4.0），会话于是
/// 用着它没绑定的那份 skill。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPlan {
    /// 要写进 `ThreadMeta::plugin_bindings` 的那一条。
    pub binding: Binding,
    /// 该发布声明的 skill 目录，作为**受管来源**交给 skill 发现。
    ///
    /// `None` = 该插件没有已安装的发布可供解析（调用方应视为"没 skill"）。
    pub managed_skill_roots: Option<Vec<yi_agent_skills::ManagedRoot>>,
}

/// 算出一个新会话应该绑什么。
///
/// 顺序是**先授权后解析**，与 [`SessionBindings::check`] 同源：
/// 1. 项目有没有把这个插件授权给 `Launch`（没有 = 不该起插件会话）
/// 2. 已提交的发布是什么（没有 = 没装/没激活）
///
/// 第 1 步先做的理由：一个未授权的项目去解析发布，只会得到"版本号"这种
/// 与用户当前问题无关的信息；而用户真正要知道的是"我没授权这个插件"。
pub fn plan_session(
    instance_home: &Path,
    project: &yi_agent_plugins::ProjectId,
    plugin: &str,
) -> Result<SessionPlan, PluginError> {
    let table = yi_agent_supervisors::managed::grants::load(instance_home)?;
    let Some(grant) = table.get(plugin, project) else {
        return Err(PluginError::PermissionDenied(format!(
            "plugin `{plugin}` is not authorized for project {}; enable the plugin for that project \
             before starting a session",
            project.as_str()
        )));
    };
    if !grant.granted.contains(&Effect::Launch) {
        return Err(PluginError::PermissionDenied(format!(
            "plugin `{plugin}` may not start sessions in {}: it was never granted Launch",
            project.as_str()
        )));
    }
    if grant.pending.contains(&Effect::Launch) {
        return Err(PluginError::PermissionDenied(format!(
            "plugin `{plugin}` needs reconfirmation for Launch in {} after a capability change",
            project.as_str()
        )));
    }

    let plugin_id = yi_agent_plugins::PluginId::parse(plugin)?;
    let install = managed::resolve(instance_home, &plugin_id)?;

    let binding = Binding {
        host_id: grant.host_id.clone(),
        release: install.release.clone(),
        activation: install.activation,
        project: project.clone(),
        grant: grant.generation,
    };

    // skill 目录只取清单声明的那几个，并**必须**落在本次安装目录之内——清单里
    // 的路径已由 `Manifest::parse` 校验过不是绝对路径/无 `..`，这里再兜一次：
    // 一个被人手工改过的安装目录不该让 skill 发现去读包外文件。
    let mut roots = Vec::new();
    for entry in &install.manifest.skills {
        let path = install.directory.join(&entry.path);
        if !path.starts_with(&install.directory) {
            return Err(PluginError::Integrity(format!(
                "skill `{}` resolves outside the installed release directory",
                entry.name
            )));
        }
        roots.push(yi_agent_skills::ManagedRoot {
            path,
            plugin: plugin.to_string(),
            version: install.release.version.clone(),
        });
    }

    Ok(SessionPlan {
        binding,
        managed_skill_roots: Some(roots),
    })
}

#[cfg(test)]
mod tests;

/// 驱动动作的准入闸门：按项目的**授权表**判一个插件能否新建会话。
///
/// # 为什么只查授权表，不查这个会话的绑定
///
/// 驱动动作要**新建**一个会话，此时还没有任何会话绑定可查。宿主在这里能回答的
/// 是"这个项目把这个插件的 Launch 授权给了吗"。绑定是会话级的概念，在新会话
/// 建成之后才写进 meta——把两件事混在一步里会让"起会话"和"用会话"的判据同源，
/// 于是任何一处放宽都会同时放宽两边。
///
/// # 为什么拒绝要说清缺少什么
///
/// 一次驱动动作被拒后，插件只会把 `{ok:false, error}` 显示给用户。如果这里只写
/// "denied"，用户既不知道是哪个项目、也不知道该去补哪个权限。
pub struct GrantGate {
    instance_home: std::path::PathBuf,
}

impl GrantGate {
    pub fn new(instance_home: impl Into<std::path::PathBuf>) -> Self {
        Self {
            instance_home: instance_home.into(),
        }
    }

    /// 生产装配：按宿主实例根。取不到实例根时返回 `None`——此时驱动路径应当
    /// **不加闸也不放行**，调用方必须决定怎么办；这里选择让调用方看到 `None`,
    /// 而不是悄悄用一个猜出来的目录去读授权表。
    pub fn from_process_environment() -> Option<Self> {
        yi_agent_store::paths::instance_home().map(Self::new)
    }
}

impl GrantGate {
    /// 受管 skill 与该项目的普通 skill 有同名冲突时拒绝自动任务。
    ///
    /// 只扫**这个项目**的普通 skill 根：用户全局目录里的同名条目会在新会话
    /// 的 catalog 里一起出现，但那不影响"这个项目该不该自动跑"——用户已经
    /// 在这个项目里显式授权了插件，项目级冲突才是他要就地解决的东西。
    fn check_skill_conflicts(&self, project: &Path, plugin: &str) -> Result<(), String> {
        let plugin_id = yi_agent_plugins::PluginId::parse(plugin)
            .map_err(|error| format!("plugin id `{plugin}` is invalid: {error}"))?;
        // 解析不出发布时不算冲突（未安装/未激活由上面的授权与维护检查负责，
        // 这里只回答"有了 skill 之后有没有撞名"）。
        let Ok(install) = managed::resolve(&self.instance_home, &plugin_id) else {
            return Ok(());
        };
        let managed: Vec<yi_agent_skills::ManagedRoot> = install
            .manifest
            .skills
            .iter()
            .map(|entry| yi_agent_skills::ManagedRoot {
                path: install.directory.join(&entry.path),
                plugin: plugin.to_string(),
                version: install.release.version.clone(),
            })
            .collect();
        if managed.is_empty() {
            return Ok(());
        }

        let project_id = yi_agent_plugins::ProjectId::parse(&project.to_string_lossy())
            .map_err(|error| format!("project key is invalid: {error}"))?;
        let _ = project_id;

        let roots = vec![(
            project.join(".yi-agent/skills"),
            yi_agent_skills::SkillScope::Project,
        )];
        let service = yi_agent_skills::SkillsService::with_managed(roots, managed);
        let conflicts = service
            .conflicts()
            .map_err(|error| format!("could not scan skills for `{plugin}`: {error}"))?;
        if let Some(conflict) = conflicts.first() {
            let listed: Vec<String> = conflict
                .entries
                .iter()
                .map(|entry| format!("{} ({})", entry.path.display(), entry.source.label()))
                .collect();
            return Err(format!(
                "skill `{}` exists in more than one place, so plugin `{plugin}` will not start \
                 automatic tasks until you resolve it: {}",
                conflict.name,
                listed.join(", ")
            ));
        }
        Ok(())
    }
}

impl crate::session_driver::DriverGate for GrantGate {
    fn may_launch(&self, _project: &Path, plugin: &str) -> Result<(), String> {
        // 项目由驱动插件给出的是 workdir；授权表按 canonical 项目键索引，
        // 故先规范化。路径不存在时不猜：那说明插件的动作指向一个不存在的
        // 目录，放行会让会话建在一个没有项目状态的地方。
        let project = std::fs::canonicalize(_project).map_err(|error| {
            format!(
                "plugin `{plugin}` asked to start a session in {}, which cannot be resolved:                  {error}",
                _project.display()
            )
        })?;
        let table = yi_agent_supervisors::managed::grants::load(&self.instance_home)
            .map_err(|error| format!("could not read the grant table: {error}"))?;
        let host = yi_agent_supervisors::managed::host_id(&self.instance_home)
            .map_err(|error| format!("could not determine the host id: {error}"))?;
        let project_id = yi_agent_plugins::ProjectId::parse(&project.to_string_lossy())
            .map_err(|error| format!("project key is invalid: {error}"))?;

        let Some(grant) = table.get(plugin, &project_id) else {
            return Err(format!(
                "plugin `{plugin}` is not authorized for project {}; enable the plugin for that                  project before it can start sessions",
                project.display()
            ));
        };
        if grant.host_id != host {
            return Err(format!(
                "the grant for plugin `{plugin}` in {} belongs to host `{}`, not `{host}`",
                project.display(),
                grant.host_id
            ));
        }
        if !grant.granted.contains(&Effect::Launch) {
            return Err(format!(
                "plugin `{plugin}` may not start sessions in {}: it was never granted Launch",
                project.display()
            ));
        }
        // 待确认的能力即使出现在 `granted` 里也不放行——升级不该顺手扩权。
        if grant.pending.contains(&Effect::Launch) {
            return Err(format!(
                "plugin `{plugin}` needs reconfirmation for Launch in {} after a capability change",
                project.display()
            ));
        }
        // 维护屏障在授权之后检查：先让用户看到"你没授权"，再看"这个插件正在
        // 维护"。反过来会把一个未授权插件报告的拒绝原因说成维护中，用户会去
        // 等一个永远不结束的维护。
        //
        // `Unknown`（读不到权威记录）也**不放行**：宁可少起一个会话，也不要在
        // 一次未收尾的升级旁边再起一个后端——那正是屏障要拦的那种事故。
        let plugin_id = yi_agent_plugins::PluginId::parse(plugin)
            .map_err(|error| format!("plugin id `{plugin}` is invalid: {error}"))?;
        let maintenance =
            yi_agent_store::ipc::MaintenanceBarrier::at(&self.instance_home).state(&plugin_id);
        if !maintenance.state.admits_launch() {
            let detail = maintenance
                .detail
                .as_deref()
                .map(|d| format!(": {d}"))
                .unwrap_or_default();
            return Err(format!(
                "plugin `{plugin}` is not accepting new sessions in {} ({:?}{detail})",
                project.display(),
                maintenance.state
            ));
        }
        // skill 同名冲突也必须挡住**自动任务**——这是设计 §7.2 的硬要求：
        // 冲突时宿主不许静默挑一个副本去执行，否则用户可能在自己手写的 skill
        // 与发布包里的同名 skill 之间被随机选一个跑。普通聊天与只读查看不受
        // 影响（它们不经过这个闸门）。
        //
        // 放在最后一步：一个未授权/维护中的插件先说"技能冲突"会把用户引到
        // 一个不相干的问题上。
        self.check_skill_conflicts(&project, plugin)?;
        Ok(())
    }
}
