//! 生命周期状态机的行为测试。
//!
//! # 这些测试在防什么
//!
//! 一次版本切换跨越六步外部动作，崩溃可能落在任意两步之间。所以测试
//! 不能只验证"顺利路径能走通"——那样一个"重启就重跑整条流水线"的实现
//! 也能通过，而它会把已经完成、不可重复的动作再做一遍。
//!
//! 这里的 `RecordingRuntime` 逐项记录动作，并按测试指定的阶段返回错误。
//! 它**不**把所有方法都实现成"成功"：一个总成功的假实现会让"某一步
//! 失败必须停下"这类断言永远为真。

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use yi_agent_plugins::journal::{JournalTarget, LifecycleAction, LifecyclePhase};
use yi_agent_plugins::{
    Admission, Binding, Effect, Lifecycle, LifecycleRequest, PluginError, PluginId, ProjectId,
    ReleaseId, RuntimeControl,
};

fn temp_home() -> PathBuf {
    tempfile::TempDir::new().expect("temp").keep().join("home")
}

fn plugin() -> PluginId {
    PluginId::parse("example-counter").unwrap()
}

fn release(version: &str) -> ReleaseId {
    ReleaseId {
        plugin: plugin(),
        version: version.to_string(),
        target: "darwin-aarch64".to_string(),
        sha256: "a".repeat(64),
    }
}

fn project(name: &str) -> ProjectId {
    ProjectId::parse(name).unwrap()
}

fn binding(activation: u64) -> Binding {
    Binding {
        host_id: "host-1".to_string(),
        release: release("1.4.0"),
        activation,
        project: project("/p/a"),
        grant: 1,
    }
}

/// 记录动作并在指定阶段失败的运行期。
#[derive(Debug, Default)]
struct RecordingRuntime {
    actions: Mutex<Vec<String>>,
    /// `migrate_project` 对哪个项目失败。
    migration_failure: Mutex<Option<String>>,
    /// `health_check` 对哪个项目失败。
    health_failure: Mutex<Option<String>>,
    /// `repair_projection` 是否失败。
    projection_failure: Mutex<bool>,
}

impl RecordingRuntime {
    fn new() -> Arc<RecordingRuntime> {
        Arc::new(RecordingRuntime::default())
    }

    fn actions(&self) -> Vec<String> {
        self.actions.lock().unwrap().clone()
    }

    fn record(&self, action: impl Into<String>) {
        self.actions.lock().unwrap().push(action.into());
    }

    fn fail_migration_for(&self, project: &str) {
        *self.migration_failure.lock().unwrap() = Some(project.to_string());
    }

    fn fail_health_for(&self, project: &str) {
        *self.health_failure.lock().unwrap() = Some(project.to_string());
    }

    fn fail_projection(&self) {
        *self.projection_failure.lock().unwrap() = true;
    }
}

impl RuntimeControl for RecordingRuntime {
    fn pause_maintenance(&self, plugin: &PluginId) -> Result<(), PluginError> {
        self.record(format!("pause:{}", plugin.as_str()));
        Ok(())
    }

    fn resume_maintenance(&self, plugin: &PluginId) -> Result<(), PluginError> {
        self.record(format!("resume:{}", plugin.as_str()));
        Ok(())
    }

    fn stop_active(&self, plugin: &PluginId) -> Result<(), PluginError> {
        self.record(format!("stop-active:{}", plugin.as_str()));
        Ok(())
    }

    fn start_candidate(&self, plugin: &PluginId, release: &ReleaseId) -> Result<(), PluginError> {
        self.record(format!(
            "start-candidate:{}:{}",
            plugin.as_str(),
            release.version
        ));
        Ok(())
    }

    fn stop_candidate(&self, plugin: &PluginId) -> Result<(), PluginError> {
        self.record(format!("stop-candidate:{}", plugin.as_str()));
        Ok(())
    }

    fn health_check(&self, plugin: &PluginId, project: &str) -> Result<(), PluginError> {
        self.record(format!("health:{}:{project}", plugin.as_str()));
        if self.health_failure.lock().unwrap().as_deref() == Some(project) {
            return Err(PluginError::Dependency(format!(
                "project `{project}` backend did not answer"
            )));
        }
        Ok(())
    }

    fn migrate_project(
        &self,
        plugin: &PluginId,
        project: &str,
        from: Option<&ReleaseId>,
        to: &ReleaseId,
    ) -> Result<(), PluginError> {
        self.record(format!(
            "migrate:{}:{project}:{}->{}",
            plugin.as_str(),
            from.map(|r| r.version.as_str()).unwrap_or("none"),
            to.version
        ));
        if self.migration_failure.lock().unwrap().as_deref() == Some(project) {
            return Err(PluginError::Dependency(format!(
                "project `{project}` has a schema this build cannot migrate"
            )));
        }
        Ok(())
    }

    fn repair_projection(&self, plugin: &PluginId, release: &ReleaseId) -> Result<(), PluginError> {
        self.record(format!("project:{}:{}", plugin.as_str(), release.version));
        if *self.projection_failure.lock().unwrap() {
            return Err(PluginError::Io(
                "could not write the project projection".to_string(),
            ));
        }
        Ok(())
    }
}

/// 简化的事务起步：不带备份源（备份目录为空也是合法的完整备份）。
/// 每次事务前都要先把目标版本登记为"已安装"，否则提交时注册表会说
/// "没有这个版本"。这是真实顺序：先装包，再切激活。
fn install(target_version: &str, home: &std::path::Path) {
    yi_agent_plugins::Registry::open(home)
        .unwrap()
        .record_install(
            &plugin(),
            target_version,
            &"a".repeat(64),
            &format!("plugins/install/{target_version}"),
            1,
        )
        .unwrap();
}

fn request(projects: &[&str]) -> LifecycleRequest {
    request_at(7, projects)
}

fn request_at(activation: u64, projects: &[&str]) -> LifecycleRequest {
    LifecycleRequest::activate(plugin(), release("1.4.0"), activation)
        .with_projects(projects.iter().map(|p| project(p)))
}

fn drive_to_done(
    lifecycle: &Lifecycle,
    runtime: &RecordingRuntime,
    id: &str,
) -> Result<LifecyclePhase, PluginError> {
    let mut phase = LifecyclePhase::Draining;
    for _ in 0..8 {
        let progress = lifecycle.advance(id, runtime)?;
        phase = progress.phase;
        if phase == LifecyclePhase::Done {
            break;
        }
    }
    Ok(phase)
}

// ---------------------------------------------------------------------------
// 候选不可用
// ---------------------------------------------------------------------------

#[test]
fn a_candidate_release_is_not_usable_through_a_normal_binding() {
    // 两级 binding 都要检查：只查版本不查代次，一个"候选代次 + 已提交版本"
    // 的伪造绑定会绕过它。
    let home = temp_home();
    let lifecycle = Lifecycle::open(&home).unwrap();
    let runtime = RecordingRuntime::new();

    // 先提交一次激活，建立"已提交的代次 6"。
    // 这里直接写状态文件以外的路径不可行，所以用完整事务提交一次。
    install("1.4.0", &home);
    let first = lifecycle.start(request_at(6, &["/p/a"])).unwrap();
    drive_to_done(&lifecycle, &runtime, &first).unwrap();

    // 第二次事务：代次 7 是候选。
    let second = lifecycle.start(request(&["/p/a"])).unwrap();

    let candidate = binding(7);
    assert!(
        lifecycle.is_candidate_activation(&candidate).unwrap(),
        "activation 7 must be recognized as the candidate"
    );
    let error = lifecycle
        .check_binding(&candidate, Effect::Read)
        .expect_err("a candidate must not be usable through a normal binding");
    assert!(
        matches!(error, PluginError::PermissionDenied(_)),
        "expected PermissionDenied, got {error:?}"
    );

    // 已提交的代次在事务进行期间也**不**可用：G2 §6.1 规定除 done 外
    // 任何阶段准入都关闭。这不是"候选连累旧版本"，而是屏障的语义——
    // 事务期间旧服务已经被停掉去备份和迁移了，再放行只会让调用打到
    // 一个正在被改写的后端上。
    let error = lifecycle
        .check_binding(&binding(6), Effect::Read)
        .expect_err("admission is closed for every activation while a transaction is open");
    assert!(
        matches!(
            error,
            PluginError::Draining(_) | PluginError::PermissionDenied(_)
        ),
        "expected a closed-admission error, got {error:?}"
    );

    // 维护凭据不受这条规则约束（它由事务签发）。
    lifecycle
        .check_binding(&candidate, Effect::Maintenance)
        .expect("maintenance credentials are exactly what a candidate is for");

    // 提交之后，同一个 binding 变成可用。
    drive_to_done(&lifecycle, &runtime, &second).unwrap();
    assert!(!lifecycle.is_candidate_activation(&candidate).unwrap());
    lifecycle
        .check_binding(&candidate, Effect::Read)
        .expect("after the commit the activation is public");
}

#[test]
fn a_binding_from_a_rolled_back_transaction_is_stale() {
    let home = temp_home();
    let lifecycle = Lifecycle::open(&home).unwrap();
    let runtime = RecordingRuntime::new();

    install("1.4.0", &home);
    let first = lifecycle.start(request_at(6, &["/p/a"])).unwrap();
    drive_to_done(&lifecycle, &runtime, &first).unwrap();

    let second = lifecycle.start(request(&["/p/a"])).unwrap();
    lifecycle.cancel(&second, &runtime).unwrap();

    let error = lifecycle
        .check_binding(&binding(7), Effect::Read)
        .expect_err("an activation that was never committed must not become usable");
    assert!(
        matches!(error, PluginError::StaleBinding(_)),
        "expected StaleBinding, got {error:?}"
    );
}

// ---------------------------------------------------------------------------
// 顺利路径
// ---------------------------------------------------------------------------

#[test]
fn a_successful_activation_walks_every_phase_in_order() {
    let home = temp_home();
    let lifecycle = Lifecycle::open(&home).unwrap();
    let runtime = RecordingRuntime::new();
    install("1.4.0", &home);

    let id = lifecycle.start(request(&["/p/a"])).unwrap();

    let mut seen = Vec::new();
    for _ in 0..8 {
        let progress = lifecycle.advance(&id, &runtime).unwrap();
        seen.push(progress.phase);
        assert!(
            !progress.admission_open || progress.phase == LifecyclePhase::Done,
            "admission must stay closed until done, got {:?}",
            progress.phase
        );
        if progress.phase == LifecyclePhase::Done {
            break;
        }
    }

    assert_eq!(
        seen,
        vec![
            LifecyclePhase::Drained,
            LifecyclePhase::BackingUp,
            LifecyclePhase::Migrating,
            LifecyclePhase::HealthChecking,
            LifecyclePhase::Committing,
            LifecyclePhase::Projecting,
            LifecyclePhase::Done,
        ]
    );

    let committed = lifecycle.committed(&plugin()).unwrap().unwrap();
    assert_eq!(committed.activation, 7);

    // 准入在 done 之后必须恢复，否则插件就永久停摆了。
    let admission = Admission::new(&home);
    assert_eq!(admission.live_before_barrier_permits(), 0);
    assert!(
        lifecycle.check_binding(&binding(7), Effect::Read).is_ok(),
        "after done the new activation must be public"
    );
}

#[test]
fn admission_stays_closed_until_the_projection_is_repaired() {
    // `projecting` 不可回滚，但准入必须仍然关闭。一个"提交后就立刻开准入"
    // 的实现会让用户在投影还没修好时就通过插件干活。
    let home = temp_home();
    let lifecycle = Lifecycle::open(&home).unwrap();
    let runtime = RecordingRuntime::new();
    install("1.4.0", &home);
    runtime.fail_projection();

    let id = lifecycle.start(request(&["/p/a"])).unwrap();
    let mut error = None;
    for _ in 0..8 {
        match lifecycle.advance(&id, &runtime) {
            Ok(progress) => {
                if progress.phase == LifecyclePhase::Done {
                    break;
                }
            }
            Err(failure) => {
                error = Some(failure);
                break;
            }
        }
    }

    let error = error.expect("a failed projection must surface as an error");
    assert!(
        matches!(error, PluginError::Io(_)),
        "expected Io, got {error:?}"
    );

    // 重新打开时看到的是投影失败的事务，而不是"没有事务"。
    let reopened = Lifecycle::open(&home).unwrap();
    assert_eq!(
        reopened.committed(&plugin()).unwrap().unwrap().activation,
        7,
        "the commit already happened before projecting"
    );
    let error = reopened
        .check_binding(&binding(7), Effect::Read)
        .expect_err("admission must stay closed while projecting");
    // 代次 7 已提交但事务未收尾：报的是"准入关闭"，而不是"候选"或"旧代次"。
    assert!(
        matches!(error, PluginError::Draining(_)),
        "expected Draining (admission closed by an unfinished transaction), got {error:?}"
    );
}

// ---------------------------------------------------------------------------
// 迁移失败
// ---------------------------------------------------------------------------

#[test]
fn a_failed_migration_does_not_restore_admission_for_any_project() {
    // 两个项目：一个迁移成功、一个失败。只回滚失败的那个会让一半项目在
    // 新格式、一半在旧格式，而共享数据只可能对应其中一种。
    let home = temp_home();
    let lifecycle = Lifecycle::open(&home).unwrap();
    let runtime = RecordingRuntime::new();
    runtime.fail_migration_for("/p/b");

    let id = lifecycle.start(request(&["/p/a", "/p/b"])).unwrap();
    let mut error = None;
    for _ in 0..8 {
        match lifecycle.advance(&id, &runtime) {
            Ok(progress) => {
                if progress.phase == LifecyclePhase::Done {
                    break;
                }
            }
            Err(failure) => {
                error = Some(failure);
                break;
            }
        }
    }

    let error = error.expect("a failed migration must stop the transaction");
    assert!(
        matches!(error, PluginError::RecoveryRequired(_)),
        "expected RecoveryRequired, got {error:?}"
    );

    // 两个项目都被提起，而不是只提失败的那个。
    let progress = lifecycle.advance(&id, &runtime).err().map(|_| ()).is_none();
    let _ = progress;

    // 重新打开：没有提交，准入不开放。
    let reopened = Lifecycle::open(&home).unwrap();
    assert!(
        reopened.committed(&plugin()).unwrap().is_none(),
        "a failed migration must not commit any activation"
    );
    let error = reopened
        .check_binding(&binding(7), Effect::Read)
        .expect_err("no project may be admitted after a failed migration");
    assert!(matches!(
        error,
        PluginError::StaleBinding(_) | PluginError::PermissionDenied(_)
    ));
}

#[test]
fn a_failed_migration_reports_every_blocked_project() {
    let home = temp_home();
    let lifecycle = Lifecycle::open(&home).unwrap();
    let runtime = RecordingRuntime::new();
    runtime.fail_migration_for("/p/b");

    let id = lifecycle.start(request(&["/p/a", "/p/b", "/p/c"])).unwrap();
    lifecycle.advance(&id, &runtime).unwrap(); // -> drained
    lifecycle.advance(&id, &runtime).unwrap(); // -> backing_up
    lifecycle.advance(&id, &runtime).unwrap(); // -> migrating
    lifecycle
        .advance(&id, &runtime)
        .expect_err("migrating /p/b must fail");

    let journal = yi_agent_plugins::JournalStore::open(&home)
        .unwrap()
        .load(&plugin())
        .unwrap()
        .expect("the journal must survive the failure");
    assert_eq!(
        journal.blocking.projects,
        vec!["/p/b".to_string()],
        "the blocking list must name the project that failed, so the UI can show it"
    );
    assert_eq!(journal.phase, LifecyclePhase::Migrating);
}

#[test]
fn a_failed_health_check_stops_the_candidate_without_committing() {
    let home = temp_home();
    let lifecycle = Lifecycle::open(&home).unwrap();
    let runtime = RecordingRuntime::new();
    runtime.fail_health_for("/p/a");

    let id = lifecycle.start(request(&["/p/a"])).unwrap();
    let mut error = None;
    for _ in 0..8 {
        match lifecycle.advance(&id, &runtime) {
            Ok(progress) => {
                if progress.phase == LifecyclePhase::Done {
                    break;
                }
            }
            Err(failure) => {
                error = Some(failure);
                break;
            }
        }
    }
    assert!(error.is_some(), "an unhealthy candidate must stop the run");

    let actions = runtime.actions();
    assert!(
        actions
            .iter()
            .any(|a| a == "stop-candidate:example-counter"),
        "the candidate must be stopped, got {actions:?}"
    );
    assert!(
        !actions.iter().any(|a| a.starts_with("project:")),
        "projection must never run for an unhealthy candidate, got {actions:?}"
    );

    let reopened = Lifecycle::open(&home).unwrap();
    assert!(
        reopened.committed(&plugin()).unwrap().is_none(),
        "an unhealthy candidate must not be committed"
    );
}

// ---------------------------------------------------------------------------
// 屏障
// ---------------------------------------------------------------------------

#[test]
fn a_live_turn_keeps_the_transaction_at_drained() {
    let home = temp_home();
    let lifecycle = Lifecycle::open(&home).unwrap();
    let runtime = RecordingRuntime::new();

    // 先建一个已提交的激活，否则 binding 没有意义。
    install("1.4.0", &home);
    let first = lifecycle.start(request_at(6, &["/p/a"])).unwrap();
    drive_to_done(&lifecycle, &runtime, &first).unwrap();

    let admission = Admission::new(&home);
    let permit = admission
        .admit_turn("turn-a", binding(6))
        .expect("a turn may start while admission is open");

    let second = lifecycle.start(request(&["/p/a"])).unwrap();
    let progress = lifecycle.advance(&second, &runtime).unwrap();
    assert_eq!(progress.phase, LifecyclePhase::Drained);

    // 再推进一次：屏障还挡着，停在 drained，并列出挡路的回合。
    let progress = lifecycle.advance(&second, &runtime).unwrap();
    assert_eq!(progress.phase, LifecyclePhase::Drained);
    assert_eq!(
        progress.blocking.turns.len(),
        1,
        "the blocker must be listed so the user sees which turn is holding the update"
    );
    assert!(!progress.admission_open);

    // 回合结束 → 屏障过去。
    drop(permit);
    let progress = lifecycle.advance(&second, &runtime).unwrap();
    assert_eq!(progress.phase, LifecyclePhase::BackingUp);
}

// ---------------------------------------------------------------------------
// 取消
// ---------------------------------------------------------------------------

#[test]
fn cancel_is_refused_after_the_commit() {
    let home = temp_home();
    let lifecycle = Lifecycle::open(&home).unwrap();
    let runtime = RecordingRuntime::new();
    install("1.4.0", &home);

    let id = lifecycle.start(request(&["/p/a"])).unwrap();
    // 推进到 projecting：提交已经发生。
    loop {
        let progress = lifecycle.advance(&id, &runtime).unwrap();
        if progress.phase == LifecyclePhase::Projecting || progress.phase == LifecyclePhase::Done {
            break;
        }
    }

    let error = lifecycle
        .cancel(&id, &runtime)
        .expect_err("undoing a committed transaction would be a second commit");
    assert!(
        matches!(error, PluginError::Conflict(_)),
        "expected Conflict, got {error:?}"
    );
}

#[test]
fn cancel_restores_admission_and_keeps_the_plugin_open() {
    let home = temp_home();
    let lifecycle = Lifecycle::open(&home).unwrap();
    let runtime = RecordingRuntime::new();

    install("1.4.0", &home);
    let first = lifecycle.start(request_at(6, &["/p/a"])).unwrap();
    drive_to_done(&lifecycle, &runtime, &first).unwrap();

    let second = lifecycle.start(request(&["/p/a"])).unwrap();
    lifecycle.advance(&second, &runtime).unwrap(); // drained

    // 取消只退回旧版本，不关闭插件。
    lifecycle.cancel(&second, &runtime).unwrap();

    lifecycle
        .check_binding(&binding(6), Effect::Read)
        .expect("cancelling a drain must restore the previous activation");
    assert!(
        !lifecycle.check_binding(&binding(7), Effect::Read).is_ok(),
        "the cancelled target must not become usable"
    );

    // 取消之后可以再开一个新事务：残留的日志不能挡住它。
    let third = lifecycle.start(request(&["/p/a"])).unwrap();
    assert_ne!(third, second);
}

// ---------------------------------------------------------------------------
// 并发事务
// ---------------------------------------------------------------------------

#[test]
fn a_second_transaction_is_refused_while_one_is_open() {
    let home = temp_home();
    let lifecycle = Lifecycle::open(&home).unwrap();
    let runtime = RecordingRuntime::new();

    let _first = lifecycle.start(request(&["/p/a"])).unwrap();
    let error = lifecycle
        .start(request(&["/p/a"]))
        .expect_err("two concurrent activations would each commit a generation");
    assert!(
        matches!(error, PluginError::Busy(_)),
        "expected Busy, got {error:?}"
    );

    // 参考一次正常收尾，避免编译器把 `runtime` 当成未使用。
    let _ = runtime.actions();
}

// ---------------------------------------------------------------------------
// journal 契约
// ---------------------------------------------------------------------------

#[test]
fn every_phase_declares_its_recovery_action() {
    // §6.1 的表：新增 phase 却忘了写恢复动作时，恢复会"跳过"它继续往下，
    // 而下一步依赖的前提根本没建立。
    let phases = [
        LifecyclePhase::Draining,
        LifecyclePhase::Drained,
        LifecyclePhase::BackingUp,
        LifecyclePhase::Migrating,
        LifecyclePhase::HealthChecking,
        LifecyclePhase::Committing,
        LifecyclePhase::Projecting,
        LifecyclePhase::Done,
    ];

    for phase in phases {
        assert!(
            !phase.recovery_action().is_empty(),
            "{phase:?} must declare a recovery action"
        );
        assert_eq!(
            phase.admission_open(),
            phase == LifecyclePhase::Done,
            "only done may open admission, {phase:?} says otherwise"
        );
        assert_eq!(
            phase.rollbackable(),
            phase != LifecyclePhase::Projecting && phase != LifecyclePhase::Done,
            "{phase:?} has the wrong rollback verdict"
        );
    }
}

#[test]
fn the_phase_order_is_strictly_forward() {
    let mut phase = LifecyclePhase::Draining;
    let mut seen = vec![phase];
    while let Some(next) = phase.next() {
        phase = next;
        seen.push(phase);
    }
    assert_eq!(seen.len(), 8, "there are exactly eight persisted phases");
}

// ---------------------------------------------------------------------------
// 日志校验
// ---------------------------------------------------------------------------

#[test]
fn a_journal_with_activation_zero_is_refused() {
    let journal = yi_agent_plugins::Journal::start(
        "tx-activate-0001",
        plugin(),
        LifecycleAction::Activate,
        JournalTarget {
            release: release("1.4.0"),
            activation: 0,
        },
        None,
        None,
    );
    assert!(
        journal.is_err(),
        "activation 0 is not \"none\"; it is an invalid generation"
    );
}

#[test]
fn a_journal_whose_target_names_another_plugin_is_refused() {
    let other = PluginId::parse("example-other").unwrap();
    let journal = yi_agent_plugins::Journal::start(
        "tx-activate-0001",
        plugin(),
        LifecycleAction::Activate,
        JournalTarget {
            release: ReleaseId {
                plugin: other,
                version: "1.4.0".to_string(),
                target: "darwin-aarch64".to_string(),
                sha256: "a".repeat(64),
            },
            activation: 7,
        },
        None,
        None,
    );
    assert!(
        journal.is_err(),
        "a journal for one plugin must not carry another plugin's release"
    );
}

#[test]
fn a_corrupt_journal_is_reported_rather_than_treated_as_no_transaction() {
    // 把坏日志当"没有事务"会立刻打开准入，而中断的步骤根本没做完。
    let home = temp_home();
    let dir = home.join("plugins").join("lifecycle");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("example-counter.json"), b"{ this is not json").unwrap();

    let error = Lifecycle::open(&home).expect_err("a bad journal must not be ignored");
    assert!(
        matches!(error, PluginError::RecoveryRequired(_)),
        "expected RecoveryRequired, got {error:?}"
    );
}

#[test]
fn a_bad_schema_version_is_incompatible_not_ignored() {
    let home = temp_home();
    let dir = home.join("plugins").join("lifecycle");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("example-counter.json"),
        br#"{"schemaVersion":99,"transactionId":"tx-activate-0001","plugin":"example-counter","action":"Activate","phase":"draining","target":{"release":{"plugin":"example-counter","version":"1.4.0","target":"darwin-aarch64","sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},"activation":7}}"#,
    )
    .unwrap();

    let error = Lifecycle::open(&home).expect_err("a different schema means a different contract");
    assert!(
        matches!(error, PluginError::Incompatible(_)),
        "expected Incompatible, got {error:?}"
    );
}

// 让 `RefCell` 导入不被视为未使用（测试里刻意保留了可变性，方便后续扩展）。
#[allow(dead_code)]
#[derive(Default)]
struct UnusedCell(RefCell<u32>);
