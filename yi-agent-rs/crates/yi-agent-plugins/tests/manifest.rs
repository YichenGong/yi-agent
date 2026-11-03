//! `Manifest::parse` 的行为测试。
//!
//! 黄金样本来自 `plugins/contracts/fixtures/manifest-v1.json`（与 JSON Schema 检查器
//! 共用同一份）。每个负例都是**从黄金样本派生一个最小改动**，而不是手写一份
//! 独立坏 JSON——后者不能证明黄金样本本身合法，于是一个「全部拒绝」的实现
//! 也能让所有负例变绿。

mod common;

use common::{fixture_bytes, mutated};
use yi_agent_plugins::Manifest;

#[test]
fn the_golden_manifest_parses() {
    let manifest =
        Manifest::parse(&fixture_bytes("manifest-v1.json")).expect("golden manifest must parse");
    assert_eq!(manifest.plugin.as_str(), "example-counter");
    assert_eq!(manifest.version, "1.4.0");
    assert_eq!(manifest.target, "darwin-aarch64");
}

// ---------------------------------------------------------------------------
// T06 Step 1 指定的那条：绝对路径入口。
// ---------------------------------------------------------------------------

#[test]
fn absolute_ui_entry_is_rejected() {
    let bytes = mutated("manifest-v1.json", |v| {
        v["views"][0]["entry"] = "/etc/passwd".into();
    });
    let error = Manifest::parse(&bytes).expect_err("an absolute entry must be rejected");
    assert_eq!(error.kind(), "InvalidManifest");
    assert!(
        error.to_string().contains("views[0].entry"),
        "the error must point at the offending field, got: {error}"
    );
}

#[test]
fn a_parent_directory_entry_is_rejected() {
    let bytes = mutated("manifest-v1.json", |v| {
        v["views"][0]["entry"] = "../../outside.html".into();
    });
    assert!(Manifest::parse(&bytes).is_err());
}

#[test]
fn a_backslash_entry_is_rejected() {
    // Windows 分隔符在 macOS 上是合法文件名字符，所以不能靠 OS 拦；
    // 必须在清单解析层拒绝，否则同一份包在两个平台上指向不同文件。
    let bytes = mutated("manifest-v1.json", |v| {
        v["views"][0]["entry"] = "ui\\index.html".into();
    });
    assert!(Manifest::parse(&bytes).is_err());
}

// ---------------------------------------------------------------------------
// T06 Step 2：其余四类。
// ---------------------------------------------------------------------------

#[test]
fn an_illegal_plugin_id_is_rejected() {
    for bad in ["Example-Counter", "example_counter", "-leading", ""] {
        let bytes = mutated("manifest-v1.json", |v| {
            v["plugin"] = bad.into();
        });
        assert!(
            Manifest::parse(&bytes).is_err(),
            "plugin id {bad:?} must be rejected"
        );
    }
}

#[test]
fn a_duplicate_view_id_is_rejected() {
    let bytes = mutated("manifest-v1.json", |v| {
        v["views"][1]["id"] = "main".into();
    });
    let error = Manifest::parse(&bytes).expect_err("duplicate view ids must be rejected");
    assert!(error.to_string().contains("main"), "got: {error}");
}

#[test]
fn a_duplicate_skill_name_is_rejected() {
    let bytes = mutated("manifest-v1.json", |v| {
        let first = v["skills"][0].clone();
        v["skills"].as_array_mut().unwrap().push(first);
    });
    assert!(Manifest::parse(&bytes).is_err());
}

#[test]
fn a_duplicate_method_name_is_rejected() {
    let bytes = mutated("manifest-v1.json", |v| {
        let first = v["methods"][0].clone();
        v["methods"].as_array_mut().unwrap().push(first);
    });
    assert!(Manifest::parse(&bytes).is_err());
}

#[test]
fn a_duplicate_cli_verb_is_rejected() {
    let bytes = mutated("manifest-v1.json", |v| {
        let first = v["cli"][0].clone();
        v["cli"].as_array_mut().unwrap().push(first);
    });
    assert!(Manifest::parse(&bytes).is_err());
}

#[test]
fn an_unknown_required_field_is_rejected() {
    // 宿主不认识的必需字段意味着插件比宿主新。猜一个默认值会把
    // 必需降级成可选，所以整份清单必须拒绝。
    let bytes = mutated("manifest-v1.json", |v| {
        v["sandboxProfile"] = "none".into();
    });
    let error = Manifest::parse(&bytes).expect_err("unknown fields must be rejected");
    assert!(
        error.to_string().contains("sandboxProfile"),
        "the error must name the unknown field, got: {error}"
    );
}

#[test]
fn an_unknown_effect_is_rejected() {
    let bytes = mutated("manifest-v1.json", |v| {
        v["methods"][0]["effect"] = "Everything".into();
    });
    let error = Manifest::parse(&bytes).expect_err("unknown effects must be rejected");
    assert_eq!(error.kind(), "InvalidManifest");
    assert!(error.to_string().contains("Everything"), "got: {error}");
}

#[test]
fn maintenance_may_not_be_declared_by_a_plugin() {
    // Maintenance 只由生命周期事务签发。让它出现在清单里，等于允许插件
    // 给自己申请「暂停/迁移/健康检查」的权限。
    let bytes = mutated("manifest-v1.json", |v| {
        v["methods"][0]["effect"] = "Maintenance".into();
    });
    assert!(Manifest::parse(&bytes).is_err());
}

#[test]
fn a_second_main_view_is_rejected() {
    // "恰好一个 main" 是语义约束，schema 表达力够但错误信息很差。
    let bytes = mutated("manifest-v1.json", |v| {
        v["views"][1]["kind"] = "main".into();
    });
    let error = Manifest::parse(&bytes).expect_err("two main views must be rejected");
    assert!(
        error.to_string().to_lowercase().contains("main"),
        "got: {error}"
    );
}

#[test]
fn a_manifest_without_a_main_view_is_rejected() {
    let bytes = mutated("manifest-v1.json", |v| {
        v["views"][0]["kind"] = "settings".into();
    });
    assert!(Manifest::parse(&bytes).is_err());
}

#[test]
fn a_skipped_migration_version_is_rejected() {
    // 迁移必须逐版本相邻（to == from + 1）。跳过中间格式会让恢复
    // 无法判断该从哪一份备份重做。
    let bytes = mutated("manifest-v1.json", |v| {
        v["data"]["format"] = 3.into();
        v["data"]["migrations"] = serde_json::json!([
            { "from": 1, "to": 3, "entry": "migrations/1-3.sh" }
        ]);
    });
    let error = Manifest::parse(&bytes).expect_err("a skipped version must be rejected");
    assert!(
        error.to_string().contains('1') || error.to_string().contains('3'),
        "got: {error}"
    );
}

#[test]
fn an_adjacent_migration_chain_is_accepted() {
    // 正例：证明上一条不是因为「任何 migrations 都被拒」而绿。
    let bytes = mutated("manifest-v1.json", |v| {
        v["data"]["format"] = 3.into();
        v["data"]["migrations"] = serde_json::json!([
            { "from": 1, "to": 2, "entry": "migrations/1-2.sh" },
            { "from": 2, "to": 3, "entry": "migrations/2-3.sh" }
        ]);
    });
    assert!(Manifest::parse(&bytes).is_ok());
}

#[test]
fn a_manifest_declaring_read_with_a_destructive_method_is_rejected() {
    // 计划 T03 的同一条规则，在 Rust 侧也要成立：名字符合正则、effect 合法，
    // 但组合起来是错的，只有显式语义检查能拦。
    let bytes = mutated("manifest-v1.json", |v| {
        v["methods"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "name": "cards/deleteEverything",
                "effect": "Read"
            }));
    });
    let error =
        Manifest::parse(&bytes).expect_err("a destructive name declared Read must be rejected");
    assert!(error.to_string().contains("delete"), "got: {error}");
}

#[test]
fn a_pre_release_version_is_rejected() {
    let bytes = mutated("manifest-v1.json", |v| {
        v["version"] = "1.4.0-beta.1".into();
    });
    assert!(Manifest::parse(&bytes).is_err());
}

#[test]
fn an_unknown_schema_version_is_rejected() {
    let bytes = mutated("manifest-v1.json", |v| {
        v["schemaVersion"] = 2.into();
    });
    assert!(Manifest::parse(&bytes).is_err());
}

#[test]
fn malformed_json_reports_an_invalid_manifest() {
    let error = Manifest::parse(b"{ not json").expect_err("garbage must not panic");
    assert_eq!(error.kind(), "InvalidManifest");
}
