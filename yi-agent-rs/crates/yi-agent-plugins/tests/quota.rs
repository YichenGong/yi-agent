//! 整机配额域的行为测试。
//!
//! # 这些测试在防什么
//!
//! 配额错了不会报错，只会让账单变大。所以每个"该拒绝"的场景都必须有
//! **只有该机制才能拦** 的样本：
//!
//! - 两个实例读同一份配置：只测一个实例看不出"各自拿全额"。
//! - 修订冲突：只测成功写入看不出"覆盖了别人的设置"。
//! - `sessions.running` 口径：只测一个实例看不出"直接相加会超"。
//!
//! 另外，无法定位共享域时必须**阻止**新自动任务，不能静默建一个独立池——
//! 后者让测试全绿而用户账单翻倍。

use std::path::{Path, PathBuf};

use yi_agent_plugins::{
    ActivityObservation, ObservationScope, PluginError, QuotaContext, SharedQuota,
    SharedQuotaConfig,
};

fn temp_dir() -> PathBuf {
    tempfile::TempDir::new().expect("temp").keep()
}

/// 建一个配额域。域是 `.yi-agent/<name>` 这个结构，名字本身不承载语义——
/// 用它是为了让测试同时覆盖"第二个插件也能有自己的域"。
fn domain(root: &Path) -> PathBuf {
    let domain = root.join(".yi-agent").join("example-plugin");
    std::fs::create_dir_all(domain.join("leases")).unwrap();
    domain
}

fn context(root: &Path) -> QuotaContext {
    let domain = domain(root);
    QuotaContext::new(domain.join("quota.json"), domain.join("leases"), 0).expect("context")
}

fn config(revision: u64, max_concurrent: u32, max_starts_per_hour: u32) -> SharedQuotaConfig {
    SharedQuotaConfig {
        revision,
        max_concurrent,
        max_starts_per_hour,
    }
}

// ---------------------------------------------------------------------------
// 域解析
// ---------------------------------------------------------------------------

#[test]
fn a_config_and_lease_directory_in_different_domains_is_refused() {
    // 读 A 的配置、抢 B 的槽位：两个实例都"遵守"自己的上限，合计仍然超。
    let root = temp_dir();
    let a = domain(&root).join("quota.json");
    let other = root.join("other").join("example-plugin");
    std::fs::create_dir_all(other.join("leases")).unwrap();

    let error = QuotaContext::new(a, other.join("leases"), 0)
        .expect_err("config and leases must share one domain");
    assert!(
        matches!(error, PluginError::Conflict(_)),
        "expected Conflict, got {error:?}"
    );
}

#[test]
fn a_path_outside_any_quota_domain_is_refused_rather_than_guessed() {
    // 猜一个域 = 给这个实例一份完整预算。必须拒绝。
    let root = temp_dir();
    let error = QuotaContext::new(
        root.join("nowhere").join("quota.json"),
        root.join("nowhere").join("leases"),
        0,
    )
    .expect_err("an invented domain hands this instance its own budget");
    assert!(
        matches!(error, PluginError::Conflict(_)),
        "expected Conflict, got {error:?}"
    );
}

#[test]
fn two_instances_share_one_budget_instead_of_each_getting_a_full_one() {
    // 两个实例读同一份配置。若各自新建一份配置，两边都会看到"上限 4"，
    // 合计 8 —— 用户的 provider 只认 4。
    let root = temp_dir();
    let quota = SharedQuota::new(context(&root));
    quota.write(0, &config(1, 4, 40)).expect("first write");

    let instance_a = SharedQuota::new(context(&root));
    let instance_b = SharedQuota::new(context(&root));

    let mut observed = ActivityObservation::new(ObservationScope::Machine);
    observed.record("instance-a", 3);
    observed.record("instance-b", 1);

    // 两个实例看到的是同一个"已用 4"，因此都没有余量。
    assert_eq!(instance_a.remaining(&observed, 0).unwrap(), 0);
    assert_eq!(instance_b.remaining(&observed, 0).unwrap(), 0);

    // 只观测到一个 owner 时，两个实例都看到同样的余量——不是各自再加满。
    let mut one = ActivityObservation::new(ObservationScope::Machine);
    one.record("instance-a", 1);
    assert_eq!(instance_a.remaining(&one, 0).unwrap(), 3);
    assert_eq!(instance_b.remaining(&one, 0).unwrap(), 3);
}

#[test]
fn a_missing_config_allows_no_new_automatic_work() {
    // 没有配置 = 没有人授权过整机配额。默认拒绝，而不是"没有上限"。
    let root = temp_dir();
    let quota = SharedQuota::new(context(&root));
    assert!(quota.read().unwrap().is_none());

    let observed = ActivityObservation::new(ObservationScope::Machine);
    assert_eq!(
        quota.remaining(&observed, 0).unwrap(),
        0,
        "an absent limit is not an unlimited one"
    );
}

// ---------------------------------------------------------------------------
// 修订冲突
// ---------------------------------------------------------------------------

#[test]
fn writing_with_a_stale_revision_is_refused_and_keeps_the_newer_value() {
    let root = temp_dir();
    let quota = SharedQuota::new(context(&root));
    quota.write(0, &config(1, 4, 40)).unwrap();

    // 另一个实例推进到修订 2。
    let other = SharedQuota::new(context(&root));
    other.write(1, &config(2, 8, 80)).unwrap();

    // 第一个实例还拿着修订 1 想写：必须拒绝，且不能覆盖。
    let error = quota
        .write(1, &config(2, 2, 20))
        .expect_err("a stale revision must not overwrite a newer limit");
    assert!(
        matches!(error, PluginError::Conflict(_)),
        "expected Conflict, got {error:?}"
    );

    let current = quota.read().unwrap().unwrap();
    assert_eq!(
        current.max_concurrent, 8,
        "the newer instance's limit must survive"
    );
    assert_eq!(current.revision, 2);
}

#[test]
fn a_write_must_advance_the_revision_by_exactly_one() {
    let root = temp_dir();
    let quota = SharedQuota::new(context(&root));
    quota.write(0, &config(1, 4, 40)).unwrap();

    let error = quota
        .write(1, &config(5, 4, 40))
        .expect_err("jumping revisions makes conflict detection meaningless");
    assert!(matches!(error, PluginError::Conflict(_)));
}

#[test]
fn a_corrupt_shared_config_is_reported_rather_than_treated_as_absent() {
    // 当"不存在"会得到 remaining() == 0（安全），但当空表后写一份默认值
    // 会覆盖另一个实例刚写的限额。所以必须显式报错。
    let root = temp_dir();
    let ctx = context(&root);
    std::fs::write(&ctx.config_path, b"{ not json").unwrap();
    let quota = SharedQuota::new(ctx);

    let error = quota
        .read()
        .expect_err("a corrupt config must not be guessed");
    assert!(
        matches!(error, PluginError::RecoveryRequired(_)),
        "expected RecoveryRequired, got {error:?}"
    );
}

// ---------------------------------------------------------------------------
// 观测口径
// ---------------------------------------------------------------------------

#[test]
fn the_same_owner_recorded_twice_is_not_double_counted() {
    // 本进程的会话同时也是某个 owner 的租约。累加会把它算成两倍，
    // 于是"还有余量"变成"没有余量"，用户看到任务被莫名拒绝。
    let root = temp_dir();
    let quota = SharedQuota::new(context(&root));
    quota.write(0, &config(1, 4, 40)).unwrap();

    let mut observed = ActivityObservation::new(ObservationScope::Machine);
    observed.record("instance-a", 2);
    observed.record("instance-a", 2);
    assert_eq!(
        observed.total(),
        2,
        "the same owner must not be summed twice"
    );
}

#[test]
fn own_sessions_are_not_added_on_top_of_the_observation() {
    // `own` 是本进程在读的会话数，它已经在 `observed` 里了。相加会双扣。
    let root = temp_dir();
    let quota = SharedQuota::new(context(&root));
    quota.write(0, &config(1, 4, 40)).unwrap();

    let mut observed = ActivityObservation::new(ObservationScope::Machine);
    observed.record("instance-a", 3);

    assert_eq!(
        quota.remaining(&observed, 3).unwrap(),
        1,
        "own sessions already appear in the observation; adding them again undercounts capacity"
    );
    // 观测不到自己（老宿主）时，本进程的数字才是唯一来源。
    let empty = ActivityObservation::new(ObservationScope::InstanceOnly);
    assert_eq!(quota.remaining(&empty, 3).unwrap(), 1);
}

#[test]
fn an_instance_only_observation_is_not_advertised_as_a_machine_total() {
    // 老宿主不上报。此时统计只覆盖本实例，UI 必须能看出来，
    // 否则"整机还有余量"是一句假话。
    let mut observed = ActivityObservation::new(ObservationScope::InstanceOnly);
    observed.record("instance-a", 1);
    assert!(!observed.is_machine_wide());

    let mut machine = ActivityObservation::new(ObservationScope::Machine);
    machine.record("instance-a", 1);
    assert!(machine.is_machine_wide());
}

#[test]
fn a_machine_wide_observation_counts_every_owner() {
    let root = temp_dir();
    let quota = SharedQuota::new(context(&root));
    quota.write(0, &config(1, 6, 40)).unwrap();

    let mut observed = ActivityObservation::new(ObservationScope::Machine);
    observed.record("instance-a", 2);
    observed.record("instance-b", 3);
    assert_eq!(observed.total(), 5);
    assert_eq!(quota.remaining(&observed, 0).unwrap(), 1);
}

#[test]
fn a_limit_smaller_than_usage_saturates_at_zero() {
    // 两个实例先后降限额时，已用可能高于新上限。不能让饱和减法 panic，
    // 也不能回绕成一个巨大的"余量"。
    let root = temp_dir();
    let quota = SharedQuota::new(context(&root));
    quota.write(0, &config(1, 2, 40)).unwrap();

    let mut observed = ActivityObservation::new(ObservationScope::Machine);
    observed.record("instance-a", 5);
    assert_eq!(quota.remaining(&observed, 0).unwrap(), 0);
}
