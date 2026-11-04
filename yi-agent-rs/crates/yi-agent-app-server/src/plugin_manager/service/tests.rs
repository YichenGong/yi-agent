//! 管理服务的测试：只证明"这一层自己的决定"，不重复底层原语的用例。
//!
//! 底层（验签、解包、注册表、授权表、事务日志）各自已有测试。这里关心的是
//! 这一层独有的规则：维护能力不得由调用方申请、激活只建立事务不推进、
//! 变更授权要求原授权已存在。

use super::*;

/// `Maintenance` 只能由生命周期事务签发。管理入口明确拒绝，而不是过滤掉它
/// 然后在别处悄悄生效。
#[test]
fn a_caller_may_not_request_maintenance() {
    let error = parse_capability("maintenance").expect_err("maintenance must be refused");
    assert_eq!(error.kind(), "PermissionDenied");
}

#[test]
fn every_user_facing_capability_parses() {
    for (raw, expected) in [
        ("read", Effect::Read),
        ("write", Effect::Write),
        ("launch", Effect::Launch),
        ("quota-write", Effect::QuotaWrite),
        ("resident", Effect::Resident),
    ] {
        assert_eq!(parse_capability(raw).expect(raw), expected);
    }
}

#[test]
fn an_unknown_capability_is_rejected_rather_than_ignored() {
    let error = parse_capability("everything").expect_err("must refuse");
    assert_eq!(error.kind(), "InvalidManifest");
}

/// 安装位置只由清单内容决定。若允许调用方指定，包就能解到受管根之外。
#[test]
fn the_release_directory_comes_from_the_manifest() {
    // 用仓库里的 golden 清单，而不是手写一份：手写的 schema 会与真实格式
    // 分叉，而分叉的清单正是"宿主按 A 读、安装按 B 写"的形状。
    let bytes = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../plugins/contracts/fixtures/manifest-v1.json"),
    )
    .expect("golden manifest");
    let manifest = Manifest::parse(&bytes).expect("valid manifest");
    assert_eq!(
        release_directory(&manifest),
        "plugins/installed/example-counter/1.4.0/darwin-aarch64"
    );
}
