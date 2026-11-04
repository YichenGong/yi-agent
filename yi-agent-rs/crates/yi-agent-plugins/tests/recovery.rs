//! 崩溃恢复的表驱动测试。
//!
//! # 这些测试在防什么
//!
//! 每个持久化 phase 都代表"崩溃可能落在这里"。恢复必须从磁盘上的 phase
//! 得到**唯一**的下一步，而且**在恢复完成前不能打开准入**。
//!
//! 每个用例维持同一个形状：
//!
//! 1. 用一个真实的 `Lifecycle` 走完前 N 步，得到磁盘上的中间状态；
//! 2. 直接丢掉这个句柄（模拟进程死掉，内存状态全没了）；
//! 3. 重新 `Lifecycle::open` 同一个实例根，`recover`；
//! 4. 断言：恢复后准入关闭、旧版本仍然可用、没有提交半截的激活。
//!
//! 只测"顺利路径能恢复"是不够的：一个"重启就重跑整条流水线"的实现也能
//! 通过那些用例，而它会把已完成的步骤再做一遍。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use yi_agent_plugins::journal::LifecyclePhase;
use yi_agent_plugins::{
    Admission, Backup, Binding, Effect, JournalStore, Lifecycle, LifecycleRequest, PluginError,
    PluginId, ProjectId, Registry, ReleaseId, RuntimeControl,
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

fn binding(activation: u64) -> Binding {
    Binding {
        host_id: "host-1".to_string(),
        release: release("1.4.0"),
        activation,
        project: ProjectId::parse("/p/a").unwrap(),
        grant: 1,
    }
}

fn install(home: &std::path::Path, version: &str) {
    Registry::open(home)
        .unwrap()
        .record_install(
            &plugin(),
            version,
            &"a".repeat(64),
            &format!("plugins/install/{version}"),
            1,
        )
        .unwrap();
}

fn request_at(activation: u64, version: &str) -> LifecycleRequest {
    LifecycleRequest::activate(plugin(), release(version), activation)
        .with_projects([ProjectId::parse("/p/a").unwrap()])
}

/// 记录动作、可注入失败的运行期。
#[derive(Debug, Default)]
struct RecordingRuntime {
    actions: Mutex<Vec<String>>,
    health_failure: Mutex<Option<String>>,
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
    fn start_candidate(&self, _plugin: &PluginId, release: &ReleaseId) -> Result<(), PluginError> {
        self.record(format!("start-candidate:{}", release.version));
        Ok(())
    }
    fn stop_candidate(&self, plugin: &PluginId) -> Result<(), PluginError> {
        self.record(format!("stop-candidate:{}", plugin.as_str()));
        Ok(())
    }
    fn health_check(&self, _plugin: &PluginId, project: &str) -> Result<(), PluginError> {
        self.record(format!("health:{project}"));
        if self.health_failure.lock().unwrap().as_deref() == Some(project) {
            return Err(PluginError::Dependency("backend down".to_string()));
        }
        Ok(())
    }
    fn migrate_project(
        &self,
        _plugin: &PluginId,
        project: &str,
        from: Option<&ReleaseId>,
        to: &ReleaseId,
    ) -> Result<(), PluginError> {
        self.record(format!(
            "migrate:{project}:{}->{}",
            from.map(|r| r.version.as_str()).unwrap_or("none"),
            to.version
        ));
        Ok(())
    }
    fn repair_projection(
        &self,
        _plugin: &PluginId,
        release: &ReleaseId,
    ) -> Result<(), PluginError> {
        self.record(format!("project:{}", release.version));
        Ok(())
    }
}

/// 建立"已提交 1.3.2 / 代次 6"的基线。
fn baseline(home: &std::path::Path, runtime: &RecordingRuntime) {
    install(home, "1.3.2");
    let lifecycle = Lifecycle::open(home).unwrap();
    let id = lifecycle
        .start(request_at(6, "1.3.2"))
        .expect("baseline transaction must start");
    for _ in 0..8 {
        let progress = lifecycle.advance(&id, runtime).expect("baseline advance");
        if progress.phase == LifecyclePhase::Done {
            return;
        }
    }
    panic!("baseline transaction never reached done");
}

/// 把事务推进到指定 phase 为止，然后丢掉句柄（模拟进程死掉）。
fn drive_to(home: &std::path::Path, runtime: &RecordingRuntime, target: LifecyclePhase) {
    install(home, "1.4.0");
    let lifecycle = Lifecycle::open(home).unwrap();
    let id = lifecycle
        .start(request_at(7, "1.4.0"))
        .expect("transaction must start");
    for _ in 0..8 {
        let progress = match lifecycle.advance(&id, runtime) {
            Ok(progress) => progress,
            Err(_) => return,
        };
        if progress.phase >= target || progress.phase == LifecyclePhase::Done {
            return;
        }
    }
}

#[test]
fn every_persisted_phase_declares_its_recovery_action() {
    // 表驱动：逐个 phase 断言恢复动作与准入状态。新增 phase 却忘了写
    // 恢复动作时，恢复会"跳过"它继续往下，而下一步依赖的前提根本没建立。
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
            "{phase:?} must declare what recovery does next"
        );
        assert_eq!(
            phase.admission_open(),
            phase == LifecyclePhase::Done,
            "only done opens admission; {phase:?} disagrees"
        );
    }
}

/// 恢复后的共同断言：准入关闭、旧版本仍可用、没有半截提交。
fn assert_recovered_safely(home: &std::path::Path, runtime: &RecordingRuntime) {
    let lifecycle = Lifecycle::open(home).unwrap();
    lifecycle
        .recover(runtime)
        .expect("recovery must not leave a half-applied transaction");

    // 恢复完成后必须没有任何未完成日志——否则准入永远开不了。
    let unfinished = JournalStore::open(home).unwrap().unfinished().unwrap();
    assert!(
        unfinished.is_empty(),
        "recovery must settle every journal, still open: {:?}",
        unfinished
            .iter()
            .map(|j| (j.transaction_id.clone(), j.phase))
            .collect::<Vec<_>>()
    );

    // 基线代次仍可用：恢复不能把用户当前在用的版本弄丢。
    lifecycle
        .check_binding(&binding(6), Effect::Read)
        .expect("recovery must leave the previous activation usable");

    // 候选代次不可用。
    let error = lifecycle
        .check_binding(&binding(7), Effect::Read)
        .expect_err("an interrupted activation must not become usable");
    assert!(
        matches!(error, PluginError::StaleBinding(_)),
        "expected StaleBinding for the abandoned target, got {error:?}"
    );

    let committed = lifecycle.committed(&plugin()).unwrap().unwrap();
    assert_eq!(
        committed.activation, 6,
        "recovery must keep the previous activation, not commit the interrupted target"
    );
}

#[test]
fn recovery_from_draining_keeps_the_old_version() {
    let home = temp_home();
    let runtime = RecordingRuntime::new();
    baseline(&home, &runtime);
    drive_to(&home, &runtime, LifecyclePhase::Draining);

    assert_recovered_safely(&home, &runtime);
}

#[test]
fn recovery_from_drained_keeps_the_old_version() {
    let home = temp_home();
    let runtime = RecordingRuntime::new();
    baseline(&home, &runtime);
    drive_to(&home, &runtime, LifecyclePhase::Drained);

    assert_recovered_safely(&home, &runtime);
}

#[test]
fn recovery_from_backing_up_keeps_the_old_version() {
    let home = temp_home();
    let runtime = RecordingRuntime::new();
    baseline(&home, &runtime);
    drive_to(&home, &runtime, LifecyclePhase::BackingUp);

    assert_recovered_safely(&home, &runtime);
}

#[test]
fn recovery_from_migrating_keeps_the_old_version() {
    let home = temp_home();
    let runtime = RecordingRuntime::new();
    baseline(&home, &runtime);
    drive_to(&home, &runtime, LifecyclePhase::Migrating);

    assert_recovered_safely(&home, &runtime);
}

#[test]
fn recovery_from_health_checking_stops_the_candidate() {
    let home = temp_home();
    let runtime = RecordingRuntime::new();
    baseline(&home, &runtime);
    drive_to(&home, &runtime, LifecyclePhase::HealthChecking);

    let before = runtime.actions().len();
    assert_recovered_safely(&home, &runtime);

    let after = runtime.actions();
    assert!(
        after[before..]
            .iter()
            .any(|action| action.starts_with("stop-candidate:")),
        "recovery from health_checking must stop the candidate it may have started, got {:?}",
        &after[before..]
    );
}

#[test]
fn recovery_from_committing_keeps_the_old_version() {
    // `committing` 崩溃时提交可能做了一半：注册表已指向新版本，但状态文件
    // 还是旧的（或反过来）。恢复必须回到一致状态，不能停在"两个都自称真话"。
    let home = temp_home();
    let runtime = RecordingRuntime::new();
    baseline(&home, &runtime);
    drive_to(&home, &runtime, LifecyclePhase::Committing);

    assert_recovered_safely(&home, &runtime);
}

#[test]
fn recovery_never_starts_anything_before_it_settles() {
    // "恢复先于准入"的另一面：恢复期间不能启动候选。启动候选是"新工作"，
    // 而屏障还没拆。
    let home = temp_home();
    let runtime = RecordingRuntime::new();
    baseline(&home, &runtime);
    drive_to(&home, &runtime, LifecyclePhase::Migrating);

    let before = runtime.actions().len();
    let lifecycle = Lifecycle::open(&home).unwrap();
    lifecycle.recover(&runtime).unwrap();

    let after = runtime.actions();
    assert!(
        !after[before..]
            .iter()
            .any(|action| action.starts_with("start-candidate:")),
        "recovery must never launch the candidate, got {:?}",
        &after[before..]
    );
}

#[test]
fn recovery_does_not_restore_from_an_incomplete_backup() {
    // 备份写到一半（`complete: false`）时不能用它恢复：那会把项目数据
    // 截断到某个中间状态，而"看起来完整"的清单会让这一步悄无声息地成功。
    let home = temp_home();
    let runtime = RecordingRuntime::new();
    baseline(&home, &runtime);
    drive_to(&home, &runtime, LifecyclePhase::Migrating);

    // 找一个正在进行的备份目录，把清单改成 complete: false。
    let backups = home.join("plugins").join("backups");
    let mut mutated = false;
    if let Ok(entries) = std::fs::read_dir(&backups) {
        for entry in entries.flatten() {
            let path = entry.path().join("manifest.json");
            if let Ok(bytes) = std::fs::read(&path) {
                let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                value["complete"] = serde_json::Value::Bool(false);
                std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
                mutated = true;
            }
        }
    }
    assert!(
        mutated,
        "the transaction must have taken a backup to mutate"
    );

    let transaction_id = JournalStore::open(&home)
        .unwrap()
        .unfinished()
        .unwrap()
        .first()
        .map(|journal| journal.transaction_id.clone())
        .expect("a transaction is open");

    let backup = Backup::new(&home, &transaction_id);
    let error = backup
        .verify()
        .expect_err("an incomplete backup must not verify");
    assert!(
        matches!(error, PluginError::RecoveryRequired(_)),
        "expected RecoveryRequired, got {error:?}"
    );
}

#[test]
fn a_backup_never_contains_sockets_pids_or_live_locks() {
    // 备份里带活锁会让恢复后出现"幽灵持有者"：锁文件指向早已不存在的
    // 进程，而存活判定只认 flock，于是它永远显示"仍被持有"。
    let home = temp_home();
    let sources_dir = home.join("sources");
    std::fs::create_dir_all(&sources_dir).unwrap();
    std::fs::write(sources_dir.join("data.json"), b"{}").unwrap();
    std::fs::write(sources_dir.join("runtime.sock"), b"x").unwrap();
    std::fs::write(sources_dir.join("daemon.pid"), b"1").unwrap();
    std::fs::write(sources_dir.join("lifecycle.lock"), b"").unwrap();

    let transaction_id = "tx-activate-00000001";
    let backup = Backup::new(&home, transaction_id);
    let sources: Vec<(String, PathBuf)> =
        ["data.json", "runtime.sock", "daemon.pid", "lifecycle.lock"]
            .iter()
            .map(|name| (name.to_string(), sources_dir.join(name)))
            .collect();

    let manifest = backup.take(transaction_id, None, &sources).unwrap();
    let paths: Vec<&str> = manifest
        .entries
        .iter()
        .map(|entry| entry.path.as_str())
        .collect();
    assert_eq!(
        paths,
        vec!["data.json"],
        "transient resources must never be backed up, got {paths:?}"
    );
}

#[test]
fn a_backup_is_rejected_if_a_file_no_longer_matches_its_digest() {
    let home = temp_home();
    let sources_dir = home.join("sources");
    std::fs::create_dir_all(&sources_dir).unwrap();
    std::fs::write(sources_dir.join("data.json"), b"{\"v\":1}").unwrap();

    let transaction_id = "tx-activate-00000002";
    let backup = Backup::new(&home, transaction_id);
    let sources = vec![("data.json".to_string(), sources_dir.join("data.json"))];
    backup.take(transaction_id, None, &sources).unwrap();

    // 备份完成后文件被换掉：恢复它会把坏数据写回去。
    std::fs::write(backup.root().join("data.json"), b"{\"v\":999}").unwrap();

    let error = backup
        .verify()
        .expect_err("a tampered backup must not verify");
    assert!(
        matches!(error, PluginError::RecoveryRequired(_)),
        "expected RecoveryRequired, got {error:?}"
    );
}

#[test]
fn restoring_a_backup_twice_is_idempotent() {
    // 迁移失败后"从备份重做"依赖这条：崩溃后重跑整步不该先判断它做到了哪。
    let home = temp_home();
    let sources_dir = home.join("sources");
    std::fs::create_dir_all(&sources_dir).unwrap();
    std::fs::write(sources_dir.join("data.json"), b"{\"v\":1}").unwrap();

    let transaction_id = "tx-activate-00000003";
    let backup = Backup::new(&home, transaction_id);
    let sources = vec![("data.json".to_string(), sources_dir.join("data.json"))];
    backup.take(transaction_id, None, &sources).unwrap();

    let destination = home.join("restored");
    let first = backup.restore(&destination).unwrap();
    let second = backup
        .restore(&destination)
        .expect("restoring twice must be safe");
    assert_eq!(first, second);
    assert_eq!(
        std::fs::read(destination.join("data.json")).unwrap(),
        b"{\"v\":1}"
    );
}

#[test]
fn recovery_does_not_overwrite_a_shared_config_revision_that_moved() {
    // 事务开始后别的实例改了共享配额配置。用旧备份覆盖会把别人的新设置
    // 抹掉——设计 §9.3 明写这不许发生。
    let home = temp_home();
    let runtime = RecordingRuntime::new();
    baseline(&home, &runtime);
    install(&home, "1.4.0");

    let lifecycle = Lifecycle::open(&home)
        .unwrap()
        .with_shared_revision_source({
            let home = home.clone();
            move |_plugin: &PluginId| {
                // 从磁盘读：测试用它模拟"别的实例在事务中途改了配置"。
                let path = home.join("shared-revision");
                std::fs::read_to_string(path)
                    .ok()
                    .and_then(|raw| raw.trim().parse::<u64>().ok())
            }
        });
    std::fs::write(home.join("shared-revision"), b"12").unwrap();

    let id = lifecycle
        .start(request_at(7, "1.4.0").with_shared_config_revision(Some(12)))
        .unwrap();
    lifecycle.advance(&id, &runtime).unwrap(); // drained

    // 别的实例把修订推进到 13。
    std::fs::write(home.join("shared-revision"), b"13").unwrap();

    let error = lifecycle
        .cancel(&id, &runtime)
        .expect_err("rolling back would overwrite the other instance's newer settings");
    assert!(
        matches!(error, PluginError::RecoveryRequired(_)),
        "expected RecoveryRequired, got {error:?}"
    );

    // 事务保留在原地：日志还在，准入仍关闭。
    assert!(
        JournalStore::open(&home)
            .unwrap()
            .load(&plugin())
            .unwrap()
            .is_some(),
        "the journal must be kept so a human can decide what to do"
    );
}

#[test]
fn admit_is_closed_for_the_duration_of_recovery() {
    let home = temp_home();
    let runtime = RecordingRuntime::new();
    baseline(&home, &runtime);
    drive_to(&home, &runtime, LifecyclePhase::Migrating);

    // 恢复前：日志存在 → 准入关闭（判据来自磁盘，内存状态重启就已丢失）。
    let lifecycle = Lifecycle::open(&home).unwrap();
    let error = lifecycle
        .check_binding(&binding(6), Effect::Read)
        .expect_err("admission must be closed while a journal is open");
    assert!(
        matches!(error, PluginError::Draining(_)),
        "expected Draining, got {error:?}"
    );

    lifecycle.recover(&runtime).unwrap();

    // 恢复后：日志清掉了，旧版本恢复可用。
    lifecycle
        .check_binding(&binding(6), Effect::Read)
        .expect("after recovery the previous activation must be usable again");
}

#[test]
fn the_admission_barrier_counts_permits_from_another_handle() {
    // 屏障的读者通常是另一个 `Admission` 句柄（生命周期事务）甚至是另一个
    // 进程。读进程内登记表会把它们算成零，于是屏障立刻"通过"。
    let home = temp_home();
    let holder = Admission::new(&home);
    let permit = holder.admit_turn("turn-a", binding(6)).unwrap();

    let observer = Admission::new(&home);
    assert_eq!(
        observer.live_before_barrier_permits(),
        1,
        "the barrier must see a permit created through another handle"
    );

    drop(permit);
    assert_eq!(observer.live_before_barrier_permits(), 0);
}
