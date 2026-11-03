//! `plugins/v1/*` 的端到端测试：走真实的 RPC 循环、真实的磁盘记录。
//!
//! # 为什么必须走 RPC 而不是直接调服务
//!
//! 服务层自己的规则各有单测。这里要证明的是**接线**：方法名确实被路由到新
//! 域、错误确实按稳定 code 返回、旧协议确实没有变成新授权、安装确实没有
//! 顺带启用或拉起任何东西。直接调服务会把这一层全部跳过。

use serde_json::{Value, json};

use crate::protocol::Scope;
use crate::server::tests::{Harness, initialize};

/// 握手后发一个请求并取回该 id 的响应。
async fn call(h: &mut Harness, id: i64, method: &str, params: &str) -> Value {
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#
    ))
    .await;
    for _ in 0..16 {
        let value = h.read_value().await;
        if value.get("id") == Some(&json!(id)) {
            return value;
        }
    }
    panic!("no response for id {id}");
}

/// `list` 在空实例上返回空列表，而不是报错。
#[tokio::test(flavor = "multi_thread")]
async fn list_on_an_empty_instance_returns_no_plugins() {
    let mut h = Harness::with_scope(Scope::Admin).await;
    initialize(&mut h).await;
    let resp = call(&mut h, 1, "plugins/v1/list", "{}").await;
    assert!(resp.get("error").is_none(), "list must succeed: {resp}");
    assert_eq!(resp["result"]["plugins"], json!([]));
    h.shutdown().await;
}

/// 未安装的插件在 list 里不出现；安装之后出现，且**没有**项目授权。
///
/// 这钉住了一条关键区分：安装 ≠ 启用。若安装顺手写了授权，下面
/// `projects` 就不会是空的。
#[tokio::test(flavor = "multi_thread")]
async fn installing_a_local_bundle_does_not_enable_any_project() {
    let mut h = Harness::with_scope(Scope::Admin).await;
    initialize(&mut h).await;

    let fixture = crate::plugin_manager::service::tests_support::Fixture::build();
    let params = json!({
        "bundlePath": fixture.bundle().to_string_lossy(),
        "metadataPath": fixture.metadata().to_string_lossy(),
    })
    .to_string();

    let resp = call(&mut h, 1, "plugins/v1/install", &params).await;
    assert!(
        resp.get("error").is_none(),
        "local install must succeed: {resp}"
    );
    assert_eq!(resp["result"]["installed"], json!(true));

    let resp = call(&mut h, 2, "plugins/v1/list", "{}").await;
    let plugins = resp["result"]["plugins"].as_array().expect("plugins array");
    assert_eq!(plugins.len(), 1, "{resp}");
    assert_eq!(plugins[0]["plugin"], "example-counter");
    assert_eq!(
        plugins[0]["projects"],
        json!([]),
        "installing must not grant any project"
    );
    // 安装也不该选中/激活任何东西之外的东西：selected 由注册表决定，
    // 而激活是另一个事务。
    let resp = call(
        &mut h,
        3,
        "plugins/v1/transaction",
        &json!({ "plugin": "example-counter" }).to_string(),
    )
    .await;
    assert!(
        resp["result"]["transaction"].is_null(),
        "install must not open an activation transaction: {resp}"
    );

    h.shutdown().await;
}

/// 旧的 `plugin/install` 不会让插件在新协议里变成"已安装/已授权"。
///
/// 两条协议各自维护自己的记录。若旧方法在新协议里留下痕迹，用户就会看到
/// 一个他没通过新流程批准过的授权。
#[tokio::test(flavor = "multi_thread")]
async fn the_legacy_install_path_does_not_create_managed_state() {
    let mut h = Harness::with_scope(Scope::Admin).await;
    initialize(&mut h).await;

    // 旧方法按名字解析可执行文件；这里给一个不存在的名字，它会明确失败——
    // 无论成败，新协议里都不该出现记录。
    let resp = call(
        &mut h,
        1,
        "plugin/install",
        &json!({ "plugin": "definitely-not-installed" }).to_string(),
    )
    .await;
    assert!(
        resp.get("error").is_some(),
        "the legacy path must refuse an unknown plugin: {resp}"
    );

    let resp = call(&mut h, 2, "plugins/v1/list", "{}").await;
    assert_eq!(
        resp["result"]["plugins"],
        json!([]),
        "the legacy path must not create managed state: {resp}"
    );
    h.shutdown().await;
}

/// 一个下游任务的方法返回稳定的 NotSupported，而不是假装成功。
#[tokio::test(flavor = "multi_thread")]
async fn unimplemented_management_methods_report_not_supported() {
    let mut h = Harness::with_scope(Scope::Admin).await;
    initialize(&mut h).await;

    // `assetRelease` 已在 T20 实现（目录查得到才返回身份），不再列在这里：
    // 一条会随任务推进而消失的断言若留着，只会变成"测试阻止功能变好"。
    for (index, method) in [
        "plugins/v1/inspectLegacy",
        "plugins/v1/adoptLegacy",
        "plugins/v1/operationResult",
    ]
    .iter()
    .enumerate()
    {
        let resp = call(&mut h, index as i64 + 1, method, "{}").await;
        let error = resp
            .get("error")
            .unwrap_or_else(|| panic!("{method} must fail: {resp}"));
        assert_eq!(
            error["data"]["code"], "plugin_not_supported",
            "{method} must be classified as NotSupported: {resp}"
        );
    }
    h.shutdown().await;
}

/// 缺参数是明确的 invalid 错误，不是"成功但什么都没做"。
#[tokio::test(flavor = "multi_thread")]
async fn a_missing_plugin_parameter_is_an_error() {
    let mut h = Harness::with_scope(Scope::Admin).await;
    initialize(&mut h).await;
    let resp = call(&mut h, 1, "plugins/v1/transaction", "{}").await;
    let error = resp.get("error").expect("must fail");
    assert!(
        error["message"].as_str().unwrap_or("").contains("plugin"),
        "the error must name the missing parameter: {resp}"
    );
    h.shutdown().await;
}

/// 清除数据要二次确认：没有 `confirm` 就什么都不删。
#[tokio::test(flavor = "multi_thread")]
async fn clear_data_requires_confirmation() {
    let mut h = Harness::with_scope(Scope::Admin).await;
    initialize(&mut h).await;
    let resp = call(
        &mut h,
        1,
        "plugins/v1/clearData",
        &json!({ "plugin": "example-counter", "projects": ["/p"] }).to_string(),
    )
    .await;
    let error = resp.get("error").expect("must refuse");
    assert_eq!(error["data"]["code"], "plugin_permission_denied", "{resp}");
    h.shutdown().await;
}

/// 空实例上的事务查询是"没有事务"，不是一个错误。
#[tokio::test(flavor = "multi_thread")]
async fn transaction_on_an_uninstalled_plugin_is_null_not_an_error() {
    let mut h = Harness::with_scope(Scope::Admin).await;
    initialize(&mut h).await;
    let resp = call(
        &mut h,
        1,
        "plugins/v1/transaction",
        &json!({ "plugin": "example-counter" }).to_string(),
    )
    .await;
    assert!(resp.get("error").is_none(), "must not error: {resp}");
    assert!(resp["result"]["transaction"].is_null(), "{resp}");
    h.shutdown().await;
}
