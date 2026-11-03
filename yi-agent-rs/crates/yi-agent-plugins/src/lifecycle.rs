//! 生命周期事务：激活、回滚、停用、卸载。
//!
//! # 状态机而不是一串步骤
//!
//! 一次版本切换跨越多步外部动作，崩溃可能落在任意两步之间。恢复时必须
//! 能从磁盘上的 phase 得到**唯一**的下一步（见 `journal.rs` 的
//! `recovery_action`），而不是重跑整条流水线——重跑会把已经完成、且不可
//! 重复的动作再做一遍（例如"停旧服务"之后旧服务已经不在，再停一次会报错
//! 并让恢复卡住）。
//!
//! # 锁
//!
//! 生命周期锁是**长锁**：事务一开始就取，收尾或取消时放。registry 短锁
//! 只在提交那一步取一次，且不跨 IO 等待——这是 G2 §5.1 冻结的顺序：
//! 长锁 → 短锁，禁止反向。
//!
//! # 候选版本不是可用版本
//!
//! `health_checking` 期间候选服务已经起来了，但它是**候选**：只有维护
//! 凭据可用，普通 binding 不得使用。[`Lifecycle::check_binding`] 是这条
//! 规则的入口，运行期与恢复都走它。
//!
//! # 未完成的事务会挡住新事务
//!
//! 同一实例同一插件同时只能有一个生命周期写事务（G2 §5.2）。两个并发
//! 激活会各自提交代次，最后谁赢取决于时序——那不是用户能理解的结果。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::admission::Admission;
use crate::error::PluginError;
use crate::journal::{
    Blocking, Journal, JournalError, JournalStore, JournalTarget, LifecycleAction, LifecyclePhase,
    PreviousActivation, write_atomic,
};
use crate::locks;
use crate::migration::Backup;
use crate::registry::Registry;
use crate::types::{Binding, Effect, PluginId, ProjectId, ReleaseId};

/// 生命周期锁的最长等待。
///
/// 一个事务可能包含备份与健康检查，所以比短锁宽；但不能无限等——
/// 用户点"更新"后界面必须有回音。
const LIFECYCLE_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// 读取"当前共享配额配置修订"的回调。宿主注入，因为共享域不属于实例。
type SharedRevisionSource = Box<dyn Fn(&PluginId) -> Option<u64> + Send + Sync + 'static>;

/// 事务请求。只有四种明确动作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleRequest {
    pub plugin: PluginId,
    pub action: LifecycleAction,
    /// 目标发布与要分配的新激活代次。
    pub target: JournalTarget,
    /// 受影响的项目。多项目时任一失败即全部不恢复新准入。
    pub projects: Vec<ProjectId>,
    /// 事务开始时观察到的共享配置修订。回退前要重新核对。
    pub shared_config_revision: Option<u64>,
    /// 需要备份的 `(相对路径, 源文件)`。
    pub backup_sources: Vec<(String, PathBuf)>,
}

impl LifecycleRequest {
    pub fn activate(plugin: PluginId, release: ReleaseId, activation: u64) -> LifecycleRequest {
        LifecycleRequest {
            plugin,
            action: LifecycleAction::Activate,
            target: JournalTarget {
                release,
                activation,
            },
            projects: Vec::new(),
            shared_config_revision: None,
            backup_sources: Vec::new(),
        }
    }

    pub fn with_projects(mut self, projects: impl IntoIterator<Item = ProjectId>) -> Self {
        self.projects = projects.into_iter().collect();
        self
    }

    pub fn with_backup_sources(
        mut self,
        sources: impl IntoIterator<Item = (String, PathBuf)>,
    ) -> Self {
        self.backup_sources = sources.into_iter().collect();
        self
    }

    pub fn with_shared_config_revision(mut self, revision: Option<u64>) -> Self {
        self.shared_config_revision = revision;
        self
    }
}

/// 宿主对运行期的控制接口。
///
/// 刻意只做这几件事：维护暂停/恢复、停旧/起停候选服务、健康检查、
/// 迁移与投影。**不**接受任意脚本——那会让"管理插件"变成"执行任意代码"。
pub trait RuntimeControl {
    /// 进入维护：暂停自动任务，但保留所有权。
    fn pause_maintenance(&self, plugin: &PluginId) -> Result<(), PluginError>;
    /// 退出维护。
    fn resume_maintenance(&self, plugin: &PluginId) -> Result<(), PluginError>;
    /// 停掉当前激活的服务。旧版本仍完整，只是不再服务。
    fn stop_active(&self, plugin: &PluginId) -> Result<(), PluginError>;
    /// 启动候选版本。
    fn start_candidate(&self, plugin: &PluginId, release: &ReleaseId) -> Result<(), PluginError>;
    /// 停掉候选版本。
    fn stop_candidate(&self, plugin: &PluginId) -> Result<(), PluginError>;
    /// 对一个项目做健康检查。
    fn health_check(&self, plugin: &PluginId, project: &str) -> Result<(), PluginError>;
    /// 迁移一个项目的数据。必须幂等：崩溃后重跑同一步得到相同结果。
    fn migrate_project(
        &self,
        plugin: &PluginId,
        project: &str,
        from: Option<&ReleaseId>,
        to: &ReleaseId,
    ) -> Result<(), PluginError>;
    /// 从已提交的激活修复项目投影。
    fn repair_projection(&self, plugin: &PluginId, release: &ReleaseId) -> Result<(), PluginError>;
}

// `Arc<T>` 也要能当 `&dyn RuntimeControl` 用：宿主常常共享一个运行期
// 控制器给多个事务，而测试需要同时持有记录器与把它传进去。
impl<T: RuntimeControl + ?Sized> RuntimeControl for std::sync::Arc<T> {
    fn pause_maintenance(&self, plugin: &PluginId) -> Result<(), PluginError> {
        (**self).pause_maintenance(plugin)
    }

    fn resume_maintenance(&self, plugin: &PluginId) -> Result<(), PluginError> {
        (**self).resume_maintenance(plugin)
    }

    fn stop_active(&self, plugin: &PluginId) -> Result<(), PluginError> {
        (**self).stop_active(plugin)
    }

    fn start_candidate(&self, plugin: &PluginId, release: &ReleaseId) -> Result<(), PluginError> {
        (**self).start_candidate(plugin, release)
    }

    fn stop_candidate(&self, plugin: &PluginId) -> Result<(), PluginError> {
        (**self).stop_candidate(plugin)
    }

    fn health_check(&self, plugin: &PluginId, project: &str) -> Result<(), PluginError> {
        (**self).health_check(plugin, project)
    }

    fn migrate_project(
        &self,
        plugin: &PluginId,
        project: &str,
        from: Option<&ReleaseId>,
        to: &ReleaseId,
    ) -> Result<(), PluginError> {
        (**self).migrate_project(plugin, project, from, to)
    }

    fn repair_projection(&self, plugin: &PluginId, release: &ReleaseId) -> Result<(), PluginError> {
        (**self).repair_projection(plugin, release)
    }
}

/// 一次推进的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub transaction_id: String,
    pub phase: LifecyclePhase,
    pub blocking: Blocking,
    pub error: Option<JournalError>,
    /// 还能不能继续推进。false = 需要人工介入。
    pub recoverable: bool,
    pub admission_open: bool,
}

impl Progress {
    fn from_journal(journal: &Journal) -> Progress {
        Progress {
            transaction_id: journal.transaction_id.clone(),
            phase: journal.phase,
            blocking: journal.blocking.clone(),
            error: journal.error.clone(),
            recoverable: journal.phase.rollbackable() || journal.phase.admission_open(),
            admission_open: journal.phase.admission_open(),
        }
    }
}

/// 已提交的激活。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommittedActivation {
    pub release: ReleaseId,
    pub activation: u64,
}

/// 一份待备份的源。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackupSource {
    pub relative: String,
    pub path: PathBuf,
}

/// 进行中事务的完整上下文。
///
/// journal 是**契约**（`lifecycle-v1.schema.json`，`additionalProperties: false`），
/// 装不下项目清单与备份源。这些是宿主私有状态，放这里；两者的一致性由
/// "同一把长锁"保证。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PendingTransaction {
    pub transaction_id: String,
    pub action: LifecycleAction,
    /// 受影响的项目。多项目迁移任一失败即全部不恢复新准入。
    pub projects: Vec<String>,
    pub backup_sources: Vec<BackupSource>,
    /// 事务开始前的完整发布身份，用于回滚时恢复状态。
    pub previous_release: Option<ReleaseId>,
    pub previous_activation: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LifecycleState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active: Option<CommittedActivation>,
    /// 候选激活代次。非空且不等于 active 时，该代次是候选。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    candidate: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending: Option<PendingTransaction>,
}

/// 生命周期句柄。同一实例共享 registry / admission / journal 路径。
pub struct Lifecycle {
    home: PathBuf,
    journals: JournalStore,
    /// 本进程持有的长锁，按事务 id 索引。
    held: Mutex<BTreeMap<String, std::sync::Arc<locks::ExclusiveLock>>>,
    /// 宿主注入的共享配置修订读取器。`None` 表示未观察。
    shared_revision_source: Option<SharedRevisionSource>,
}

impl std::fmt::Debug for Lifecycle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lifecycle")
            .field("home", &self.home)
            .finish()
    }
}

impl Lifecycle {
    pub fn open(instance_home: impl Into<PathBuf>) -> Result<Lifecycle, PluginError> {
        let home = instance_home.into();
        let journals = JournalStore::open(&home)?;
        Ok(Lifecycle {
            home,
            journals,
            held: Mutex::new(BTreeMap::new()),
            shared_revision_source: None,
        })
    }

    pub fn home(&self) -> &std::path::Path {
        &self.home
    }

    pub fn admission(&self) -> Admission {
        Admission::new(&self.home)
    }

    fn registry(&self) -> Result<Registry, PluginError> {
        Registry::open(&self.home)
    }

    /// 状态文件**必须**与日志分开目录。
    ///
    /// 放在 `plugins/lifecycle/` 下时，`JournalStore::load_all` 会把状态文件
    /// 当成日志读，得到 "unknown field `candidate`" 这种完全误导的报错。
    /// 目录分离让"哪类文件该按哪种格式解析"由路径决定，而不是靠字段嗅探。
    fn state_path(&self, plugin: &PluginId) -> PathBuf {
        self.home
            .join("plugins")
            .join("lifecycle-state")
            .join(format!("{}.json", plugin.as_str()))
    }

    fn load_state(&self, plugin: &PluginId) -> Result<LifecycleState, PluginError> {
        let path = self.state_path(plugin);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                PluginError::RecoveryRequired(format!(
                    "lifecycle state at {} is unreadable ({error}); a bad state file means we \
                     cannot tell which activation is committed",
                    path.display()
                ))
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(LifecycleState::default())
            }
            Err(error) => Err(PluginError::Io(format!("{}: {error}", path.display()))),
        }
    }

    fn write_state(&self, plugin: &PluginId, state: &LifecycleState) -> Result<(), PluginError> {
        let path = self.state_path(plugin);
        let parent = path.parent().expect("state path has a parent");
        std::fs::create_dir_all(parent)?;
        let bytes = serde_json::to_vec_pretty(state)
            .map_err(|e| PluginError::Io(format!("could not serialize lifecycle state: {e}")))?;
        write_atomic(parent, &path, &bytes)
    }

    /// 当前已提交的激活。
    pub fn committed(&self, plugin: &PluginId) -> Result<Option<CommittedActivation>, PluginError> {
        Ok(self.load_state(plugin)?.active)
    }

    /// 这个 binding 指向的激活是不是一个**尚未提交的候选**。
    ///
    /// 候选在 `health_checking` 期间真实存在且能响应，但它还没有被授予
    /// 公共能力。若不做这个判断，"健康检查通过"就会被当作"已可用"，
    /// 用户可能在提交前就通过插件干活——那些副作用在回滚后没有任何记录。
    pub fn is_candidate_activation(&self, binding: &Binding) -> Result<bool, PluginError> {
        let state = self.load_state(&binding.release.plugin)?;
        Ok(state.candidate == Some(binding.activation)
            && state
                .active
                .as_ref()
                .map(|active| active.activation != binding.activation)
                .unwrap_or(true))
    }

    /// 用普通 binding 检查一次调用。
    ///
    /// 与 `GrantTable::check` 分开：能力检查不知道"候选"这回事，而这两条
    /// 规则都必须在**任何**能力检查之前生效。
    ///
    /// 1. 只有**已提交**的激活可以被普通 binding 使用。候选、已回滚的目标、
    ///    尚未存在的代次一律 `StaleBinding`。
    /// 2. `Maintenance` 能力由生命周期事务签发，不走这条路径。
    pub fn check_binding(&self, binding: &Binding, effect: Effect) -> Result<(), PluginError> {
        if effect == Effect::Maintenance {
            return Ok(());
        }

        let state = self.load_state(&binding.release.plugin)?;

        if state.candidate == Some(binding.activation)
            && state
                .active
                .as_ref()
                .map(|active| active.activation != binding.activation)
                .unwrap_or(true)
        {
            return Err(PluginError::PermissionDenied(format!(
                "activation {} is a candidate under health checking; it is reachable only \
                 with maintenance credentials until the transaction commits",
                binding.activation
            )));
        }

        match &state.active {
            Some(active) if active.activation == binding.activation => {}
            Some(active) => {
                return Err(PluginError::StaleBinding(format!(
                    "binding carries activation {}, but plugin `{}` has activation {} committed",
                    binding.activation,
                    binding.release.plugin.as_str(),
                    active.activation
                )));
            }
            None => {
                return Err(PluginError::StaleBinding(format!(
                    "plugin `{}` has no committed activation; binding carries activation {}",
                    binding.release.plugin.as_str(),
                    binding.activation
                )));
            }
        }

        // 最后一条：准入关闭的判据是**磁盘上的日志**，不是内存里的 `Phase`。
        //
        // 内存判据在进程重启后就没了：用户重开面板，一个"停在 projecting"
        // 的事务会立刻变成"准入开放"，而投影根本没修好。日志只有事务
        // 真正收尾时才删除，因此"日志存在且 phase 不是 done"就是"准入必须关闭"。
        //
        // 放在候选/代次检查之后，是为了让更具体的诊断先说话：用户看到
        // "这个代次还是候选"比"有个事务没结束"更容易定位。
        if let Some(open) = self.journals.load(&binding.release.plugin)? {
            if !open.phase.admission_open() {
                return Err(PluginError::Draining(format!(
                    "plugin `{}` has an unfinished lifecycle transaction `{}` at phase {}; \
                     admission reopens only after it reaches done",
                    binding.release.plugin.as_str(),
                    open.transaction_id,
                    open.phase.as_str()
                )));
            }
        }
        Ok(())
    }

    /// 开始一个事务。
    pub fn start(&self, request: LifecycleRequest) -> Result<String, PluginError> {
        if let Some(existing) = self.journals.load(&request.plugin)? {
            return Err(PluginError::Busy(format!(
                "plugin `{}` already has an open transaction `{}` at phase {}; \
                 finish or cancel it before starting another",
                request.plugin.as_str(),
                existing.transaction_id,
                existing.phase.as_str()
            )));
        }

        let transaction_id = new_transaction_id(&request.plugin, request.action);
        self.hold_lock(&transaction_id)?;

        let current = self.load_state(&request.plugin)?;
        let previous = current.active.as_ref().map(|active| PreviousActivation {
            activation: active.activation,
            version: Some(active.release.version.clone()),
        });

        let journal = Journal::start(
            &transaction_id,
            request.plugin.clone(),
            request.action,
            request.target.clone(),
            previous,
            request.shared_config_revision,
        )?;

        // 先落盘 phase 再建屏障：反过来的话，崩溃会留下"屏障已建立但没有
        // 日志"的状态，恢复无从得知该拆屏障还是继续。
        self.journals.write_locked(&journal)?;

        self.write_state(
            &request.plugin,
            &LifecycleState {
                active: current.active.clone(),
                // 候选标记从这一刻起就生效：`health_checking` 之前候选还没
                // 起来，但这个代次已经"不是公共版本"了。
                candidate: Some(request.target.activation),
                pending: Some(PendingTransaction {
                    transaction_id: transaction_id.clone(),
                    action: request.action,
                    projects: request
                        .projects
                        .iter()
                        .map(|project| project.as_str().to_string())
                        .collect(),
                    backup_sources: request
                        .backup_sources
                        .iter()
                        .map(|(relative, path)| BackupSource {
                            relative: relative.clone(),
                            path: path.clone(),
                        })
                        .collect(),
                    previous_release: current.active.as_ref().map(|a| a.release.clone()),
                    previous_activation: current.active.as_ref().map(|a| a.activation),
                }),
            },
        )?;

        self.admission().begin_drain()?;
        Ok(transaction_id)
    }

    /// 推进一个事务一步。
    ///
    /// 每步只做"当前 phase 对应的那件事"，然后把下一个 phase 落盘。
    /// 调用方反复调用直到 `phase == done` 或出错。
    pub fn advance(&self, id: &str, runtime: &dyn RuntimeControl) -> Result<Progress, PluginError> {
        let journal = self.load_own_journal(id)?;
        self.hold_lock(id)?;

        let mut journal = journal;
        let result = match journal.phase {
            LifecyclePhase::Draining => self.step_draining(&mut journal),
            LifecyclePhase::Drained => self.step_drained(&mut journal, runtime),
            LifecyclePhase::BackingUp => self.step_backing_up(&mut journal),
            LifecyclePhase::Migrating => self.step_migrating(&mut journal, runtime),
            LifecyclePhase::HealthChecking => self.step_health_checking(&mut journal, runtime),
            LifecyclePhase::Committing => self.step_committing(&mut journal),
            LifecyclePhase::Projecting => self.step_projecting(&mut journal, runtime),
            LifecyclePhase::Done => Ok(Progress::from_journal(&journal)),
        };

        match result {
            Ok(progress) => Ok(progress),
            Err(error) => {
                // 把错误落进日志：用户下次打开面板时要看到"上次为什么停下"，
                // 而不是只看到"停在 health_checking"。
                journal.error = Some(JournalError::from(&error));
                let _ = self.journals.write_locked(&journal);
                Err(error)
            }
        }
    }

    /// 取消一个事务。
    ///
    /// 只在可安全退回的阶段允许。`projecting` 之后提交已经发生，
    /// 回退就是第二次提交。
    pub fn cancel(&self, id: &str, runtime: &dyn RuntimeControl) -> Result<Progress, PluginError> {
        let mut journal = self.load_own_journal(id)?;
        self.hold_lock(id)?;

        if !journal.phase.rollbackable() {
            return Err(PluginError::Conflict(format!(
                "transaction `{id}` is at phase {}; the commit already happened, so undoing it \
                 would be a second commit",
                journal.phase.as_str()
            )));
        }

        self.rollback(&mut journal, runtime)?;
        journal.phase = LifecyclePhase::Draining;
        Ok(Progress::from_journal(&journal))
    }

    /// 恢复所有未完成的事务。
    ///
    /// **恢复先于准入**：返回前不会打开任何准入。每份日志的 phase 决定
    /// 唯一动作（`journal.rs` 的 `recovery_action` 表）。
    pub fn recover(&self, runtime: &dyn RuntimeControl) -> Result<Vec<Progress>, PluginError> {
        let mut results = Vec::new();

        for journal in self.journals.load_all()? {
            self.hold_lock(&journal.transaction_id)?;
            let mut journal = journal;

            if journal.phase.admission_open() {
                // 崩在"写 done"与"删日志"之间：唯一合法的下一步是收尾。
                self.finish_done(&mut journal, runtime)?;
                results.push(Progress::from_journal(&journal));
                continue;
            }

            // 候选可能已经起来了：先按 phase 停它，再回滚。`draining` 之前
            // 候选根本不存在，去停只会让录制的动作与真实发生的对不上。
            if journal.phase.candidate_may_be_running() {
                // 停候选失败不阻断恢复：它可能本来就没起来。
                let _ = runtime.stop_candidate(&journal.plugin);
            }

            if journal.phase.rollbackable() {
                self.rollback(&mut journal, runtime)?;
            } else {
                // `projecting`：提交已发生，只能往前修投影。
                self.step_projecting(&mut journal, runtime)?;
            }
            results.push(Progress::from_journal(&journal));
        }
        Ok(results)
    }

    // -----------------------------------------------------------------
    // 各阶段
    // -----------------------------------------------------------------

    fn step_draining(&self, journal: &mut Journal) -> Result<Progress, PluginError> {
        // `begin_drain` 幂等：崩在"建屏障"与"落盘 drained"之间时重跑无害。
        self.admission().begin_drain()?;
        journal.phase = LifecyclePhase::Drained;
        self.journals.write_locked(journal)?;
        Ok(Progress::from_journal(journal))
    }

    fn step_drained(
        &self,
        journal: &mut Journal,
        runtime: &dyn RuntimeControl,
    ) -> Result<Progress, PluginError> {
        let admission = self.admission();
        let live = admission.live_before_barrier_permits();
        if live > 0 {
            // 屏障还没等到零。列出挡路的回合，让界面能显示"谁在阻塞"，
            // 而不是一个没有解释的等待。
            journal.blocking.turns = (1..=live).map(|i| format!("turn-{i}")).collect();
            self.journals.write_locked(journal)?;
            return Ok(Progress::from_journal(journal));
        }
        journal.blocking.turns.clear();

        // 屏障已过：进入维护、停旧服务。所有权保留（不释放租约），
        // 否则维护期间后端重启会让 owner 丢失。
        runtime.pause_maintenance(&journal.plugin)?;
        runtime.stop_active(&journal.plugin)?;
        journal.phase = LifecyclePhase::BackingUp;
        self.journals.write_locked(journal)?;
        Ok(Progress::from_journal(journal))
    }

    fn step_backing_up(&self, journal: &mut Journal) -> Result<Progress, PluginError> {
        let pending = self.pending(journal)?;
        let sources: Vec<(String, PathBuf)> = pending
            .backup_sources
            .iter()
            .map(|source| (source.relative.clone(), source.path.clone()))
            .collect();

        // `take` 自己会清掉上一次的目录，所以"丢弃残缺备份并重做"是幂等的。
        let backup = Backup::new(&self.home, &journal.transaction_id);
        let manifest = backup.take(
            &journal.transaction_id,
            journal.shared_config_revision,
            &sources,
        )?;
        if !manifest.complete {
            return Err(PluginError::Integrity(
                "backup finished but is still marked incomplete".to_string(),
            ));
        }
        // 回读校验：备份写完就坏的话，现在发现比到恢复时才发现便宜。
        backup.verify()?;

        journal.phase = LifecyclePhase::Migrating;
        self.journals.write_locked(journal)?;
        Ok(Progress::from_journal(journal))
    }

    fn step_migrating(
        &self,
        journal: &mut Journal,
        runtime: &dyn RuntimeControl,
    ) -> Result<Progress, PluginError> {
        let pending = self.pending(journal)?;
        let mut failed = Vec::new();

        for project in &pending.projects {
            if let Err(error) = runtime.migrate_project(
                &journal.plugin,
                project,
                pending.previous_release.as_ref(),
                &journal.target.release,
            ) {
                failed.push((project.clone(), error));
            }
        }

        if !failed.is_empty() {
            // 任一项目失败 → 全部项目都不恢复新准入。只回滚失败的那个会让
            // 一半项目在新格式、一半在旧格式，而共享数据只可能对应其中一种。
            journal.blocking.projects = failed.iter().map(|(p, _)| p.clone()).collect();
            let first = failed[0].1.to_string();
            self.journals.write_locked(journal)?;

            // 数据回到一致状态，但**不**打开准入、**不**结束事务：
            // phase 停在 `migrating`，恢复可以幂等重做，用户也可以取消。
            let backup = Backup::new(&self.home, &journal.transaction_id);
            if backup.manifest_path().exists() {
                backup.restore(&self.home)?;
            }
            let _ = runtime.stop_candidate(&journal.plugin);

            return Err(PluginError::RecoveryRequired(format!(
                "migration failed for {} of {} project(s) ({}); every project was rolled back \
                 and admission stays closed until the transaction is retried or cancelled: {first}",
                failed.len(),
                pending.projects.len(),
                journal.blocking.projects.join(", ")
            )));
        }

        journal.blocking.projects.clear();
        journal.phase = LifecyclePhase::HealthChecking;
        self.journals.write_locked(journal)?;
        Ok(Progress::from_journal(journal))
    }

    fn step_health_checking(
        &self,
        journal: &mut Journal,
        runtime: &dyn RuntimeControl,
    ) -> Result<Progress, PluginError> {
        // 崩溃后会重跑这一步：`start_candidate` 必须对"已经起来了"幂等。
        runtime.start_candidate(&journal.plugin, &journal.target.release)?;

        let pending = self.pending(journal)?;
        for project in &pending.projects {
            if let Err(error) = runtime.health_check(&journal.plugin, project) {
                // 任一项不健康 → 停候选。此时仍可回滚，但**不**自动回滚：
                // 让用户看到具体哪个项目不健康，由他决定重试还是取消。
                let _ = runtime.stop_candidate(&journal.plugin);
                journal.error = Some(JournalError::from(&error));
                self.journals.write_locked(journal)?;
                return Err(PluginError::RecoveryRequired(format!(
                    "health check failed for project `{project}`: {error}; the candidate was \
                     stopped, the previous activation is still committed, and admission stays \
                     closed until the transaction is retried or cancelled"
                )));
            }
        }

        journal.phase = LifecyclePhase::Committing;
        self.journals.write_locked(journal)?;
        Ok(Progress::from_journal(journal))
    }

    fn step_committing(&self, journal: &mut Journal) -> Result<Progress, PluginError> {
        // 原子提交：先写生命周期状态（谁是激活），再改注册表的 selected。
        // 顺序不能反——注册表指向新版本但激活记录还是旧的，会让"已安装"
        // 与"已激活"永久不一致，而两者都自称是真话。
        let state = self.load_state(&journal.plugin)?;
        self.write_state(
            &journal.plugin,
            &LifecycleState {
                active: Some(CommittedActivation {
                    release: journal.target.release.clone(),
                    activation: journal.target.activation,
                }),
                candidate: Some(journal.target.activation),
                pending: state.pending,
            },
        )?;

        // registry 短锁只包这一次写，不跨 IO。
        self.registry()?
            .select_version(&journal.plugin, &journal.target.release.version)
            .map_err(|error| {
                PluginError::RecoveryRequired(format!(
                    "could not point the registry at version `{}`: {error}; the commit is \
                     partially applied and must be redone or rolled back",
                    journal.target.release.version
                ))
            })?;

        journal.phase = LifecyclePhase::Projecting;
        self.journals.write_locked(journal)?;
        Ok(Progress::from_journal(journal))
    }

    fn step_projecting(
        &self,
        journal: &mut Journal,
        runtime: &dyn RuntimeControl,
    ) -> Result<Progress, PluginError> {
        // 提交已经发生，这里只能往前修。修失败就停在 `projecting`：
        // 准入继续关闭，但**不**回滚（回滚等于第二次提交）。
        runtime.repair_projection(&journal.plugin, &journal.target.release)?;

        journal.phase = LifecyclePhase::Done;
        self.journals.write_locked(journal)?;
        self.finish_done(journal, runtime)?;
        Ok(Progress::from_journal(journal))
    }

    /// `done` 的收尾：清候选、开准入、删日志、放长锁。
    ///
    /// 幂等：崩在中间时恢复会重跑，所有步骤都必须能重做。
    fn finish_done(
        &self,
        journal: &mut Journal,
        runtime: &dyn RuntimeControl,
    ) -> Result<(), PluginError> {
        let state = self.load_state(&journal.plugin)?;
        self.write_state(
            &journal.plugin,
            &LifecycleState {
                active: Some(CommittedActivation {
                    release: journal.target.release.clone(),
                    activation: journal.target.activation,
                }),
                candidate: None,
                pending: None,
            },
        )?;

        let _ = state.pending;
        let _ = runtime.resume_maintenance(&journal.plugin);
        self.admission().cancel_drain()?;
        self.journals.remove_locked(&journal.plugin)?;
        self.release_lock(&journal.transaction_id);
        Ok(())
    }

    // -----------------------------------------------------------------
    // 回滚
    // -----------------------------------------------------------------

    fn rollback(
        &self,
        journal: &mut Journal,
        runtime: &dyn RuntimeControl,
    ) -> Result<(), PluginError> {
        // 回滚要尊重共享配置修订：事务开始后别的实例可能改了共享配置，
        // 用旧备份覆盖会把别人的新设置抹掉。
        if let Some(observed) = journal.shared_config_revision {
            if let Some(current) = self.current_shared_revision(journal) {
                if current != observed {
                    let message = format!(
                        "shared config revision advanced from {observed} to {current} while this \
                         transaction was open; refusing to roll back because it would overwrite \
                         another instance's newer settings"
                    );
                    journal.error = Some(JournalError {
                        kind: crate::journal::JournalErrorKind::RecoveryRequired,
                        message: message.clone(),
                    });
                    self.journals.write_locked(journal)?;
                    return Err(PluginError::RecoveryRequired(message));
                }
            }
        }

        // 从备份恢复项目数据。备份可能不存在（`backing_up` 中途崩溃），
        // 那就不恢复——项目数据还没被迁移改写过。
        let backup = Backup::new(&self.home, &journal.transaction_id);
        if backup.manifest_path().exists() {
            backup.restore(&self.home)?;
        }

        let _ = runtime.stop_candidate(&journal.plugin);
        self.restore_previous_state(journal)?;
        let _ = runtime.resume_maintenance(&journal.plugin);
        self.admission().cancel_drain()?;
        self.journals.remove_locked(&journal.plugin)?;
        self.release_lock(&journal.transaction_id);
        Ok(())
    }

    fn restore_previous_state(&self, journal: &mut Journal) -> Result<(), PluginError> {
        let pending = self.load_state(&journal.plugin)?.pending;
        let active = pending.and_then(|pending| {
            pending
                .previous_release
                .map(|release| CommittedActivation {
                    release,
                    activation: pending.previous_activation.unwrap_or(0),
                })
                .filter(|committed| committed.activation > 0)
        });
        self.write_state(
            &journal.plugin,
            &LifecycleState {
                active,
                candidate: None,
                pending: None,
            },
        )
    }

    // -----------------------------------------------------------------
    // 辅助
    // -----------------------------------------------------------------

    fn pending(&self, journal: &Journal) -> Result<PendingTransaction, PluginError> {
        self.load_state(&journal.plugin)?.pending.ok_or_else(|| {
            PluginError::RecoveryRequired(format!(
                "transaction `{}` has a journal but no pending state; we cannot tell which \
                     projects or backups it was working with",
                journal.transaction_id
            ))
        })
    }

    fn current_shared_revision(&self, journal: &Journal) -> Option<u64> {
        // 共享配置由宿主提供；未提供时视为"无人观察"，不阻断回滚。
        self.shared_revision_source
            .as_ref()
            .and_then(|source| source(&journal.plugin))
    }

    fn load_own_journal(&self, id: &str) -> Result<Journal, PluginError> {
        let journals = self.journals.load_all()?;
        journals
            .into_iter()
            .find(|journal| journal.transaction_id == id)
            .ok_or_else(|| {
                PluginError::NotFound(format!("no lifecycle transaction `{id}` on disk"))
            })
    }

    fn hold_lock(&self, id: &str) -> Result<(), PluginError> {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        if held.contains_key(id) {
            return Ok(());
        }
        let lock = locks::acquire(&self.journals.lock_path(), LIFECYCLE_LOCK_TIMEOUT)?;
        held.insert(id.to_string(), lock);
        Ok(())
    }

    fn release_lock(&self, id: &str) {
        self.held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
    }

    /// 注入共享配置修订的读取器。宿主用它观察"别的实例改没改"。
    pub fn with_shared_revision_source(
        mut self,
        source: impl Fn(&PluginId) -> Option<u64> + Send + Sync + 'static,
    ) -> Self {
        self.shared_revision_source = Some(Box::new(source));
        self
    }
}

fn new_transaction_id(plugin: &PluginId, action: LifecycleAction) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let raw = format!("tx-{}-{}-{nanos}", action.as_str(), plugin.as_str());
    raw.chars()
        .map(|c| {
            if c.is_ascii_uppercase() {
                c.to_ascii_lowercase()
            } else {
                c
            }
        })
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
        .collect()
}
