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

    // `assetRelease`（T20）、`inspectLegacy` 与 `adoptLegacy`（T23）已实现，
    // 不再列在这里：一条会随任务推进而消失的断言若留着，只会变成
    // "测试阻止功能变好"。
    for (index, method) in ["plugins/v1/operationResult"].iter().enumerate() {
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

/// `adoptLegacy` 一次真实接管：备份旧看板、写新授权、返回备份位置。
#[tokio::test(flavor = "multi_thread")]
async fn adopting_a_legacy_install_backs_up_and_grants() {
    let mut h = Harness::with_scope(Scope::Admin).await;
    initialize(&mut h).await;

    let fixture = crate::plugin_manager::service::tests_support::Fixture::build();
    let params = json!({
        "bundlePath": fixture.bundle().to_string_lossy(),
        "metadataPath": fixture.metadata().to_string_lossy(),
    })
    .to_string();
    let resp = call(&mut h, 1, "plugins/v1/install", &params).await;
    assert!(resp.get("error").is_none(), "install must succeed: {resp}");

    // 项目里有一份旧看板，且旧进程已经不在（没有 plugin.lock 被持有）。
    let project = tempfile::tempdir().expect("project");
    let state = project.path().join(".yi-agent/example-counter");
    std::fs::create_dir_all(&state).expect("state dir");
    let board = r#"{"cards":[],"next_order":0}"#;
    std::fs::write(state.join("board.json"), board).expect("board");

    let params = json!({
        "plugin": "example-counter",
        "project": project.path().to_string_lossy(),
        "capabilities": ["read", "write"],
    })
    .to_string();
    let resp = call(&mut h, 2, "plugins/v1/adoptLegacy", &params).await;
    assert!(resp.get("error").is_none(), "adoption must succeed: {resp}");
    let result = &resp["result"];
    assert_eq!(result["activation"], json!(1), "{resp}");
    let backup = result["backup"].as_str().expect("backup path");
    assert!(
        std::path::Path::new(backup).join("board.json").is_file(),
        "the legacy board must be backed up: {backup}"
    );
    // 旧数据没被搬走——接管建立的是"谁有权推进"，不是"把卡搬个地方"。
    assert_eq!(
        std::fs::read_to_string(state.join("board.json")).unwrap(),
        board
    );

    let resp = call(&mut h, 3, "plugins/v1/list", "{}").await;
    assert_eq!(
        resp["result"]["plugins"][0]["projects"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "adoption must grant exactly the project it adopted: {resp}"
    );
    h.shutdown().await;
}

/// 旧开关不是授权：不显式给 `capabilities` 就拒绝，而不是替他勾上。
#[tokio::test(flavor = "multi_thread")]
async fn adopting_without_explicit_capabilities_is_refused() {
    let mut h = Harness::with_scope(Scope::Admin).await;
    initialize(&mut h).await;
    let fixture = crate::plugin_manager::service::tests_support::Fixture::build();
    let params = json!({
        "bundlePath": fixture.bundle().to_string_lossy(),
        "metadataPath": fixture.metadata().to_string_lossy(),
    })
    .to_string();
    let _ = call(&mut h, 1, "plugins/v1/install", &params).await;

    let project = tempfile::tempdir().expect("project");
    let params = json!({
        "plugin": "example-counter",
        "project": project.path().to_string_lossy(),
    })
    .to_string();
    let resp = call(&mut h, 2, "plugins/v1/adoptLegacy", &params).await;
    let error = resp
        .get("error")
        .unwrap_or_else(|| panic!("adoption must be refused: {resp}"));
    assert_eq!(
        error["data"]["code"], "plugin_permission_denied",
        "missing capabilities must be a permission error: {resp}"
    );
    h.shutdown().await;
}

/// 旧进程还持有 `plugin.lock` 时报告阻塞，**不**杀进程。
#[tokio::test(flavor = "multi_thread")]
async fn adopting_while_the_legacy_process_is_alive_reports_busy() {
    let mut h = Harness::with_scope(Scope::Admin).await;
    initialize(&mut h).await;
    let fixture = crate::plugin_manager::service::tests_support::Fixture::build();
    let params = json!({
        "bundlePath": fixture.bundle().to_string_lossy(),
        "metadataPath": fixture.metadata().to_string_lossy(),
    })
    .to_string();
    let _ = call(&mut h, 1, "plugins/v1/install", &params).await;

    let project = tempfile::tempdir().expect("project");
    let state = project.path().join(".yi-agent/example-counter");
    std::fs::create_dir_all(&state).expect("state dir");
    std::fs::write(state.join("board.json"), r#"{"cards":[],"next_order":0}"#).expect("board");
    // 模拟旧进程仍持有锁：用一个独立 fd 起 flock。
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(state.join("plugin.lock"))
        .expect("lock file");
    {
        use std::os::unix::io::AsRawFd;
        let outcome = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(outcome, 0, "test must be able to take the lock");
    }

    let params = json!({
        "plugin": "example-counter",
        "project": project.path().to_string_lossy(),
        "capabilities": ["read", "write"],
    })
    .to_string();
    let resp = call(&mut h, 2, "plugins/v1/adoptLegacy", &params).await;
    let error = resp
        .get("error")
        .unwrap_or_else(|| panic!("adoption must be refused: {resp}"));
    assert_eq!(
        error["data"]["code"], "plugin_busy",
        "a live legacy owner must be reported as busy: {resp}"
    );
    // 没有授权被写进去。
    let resp = call(&mut h, 3, "plugins/v1/list", "{}").await;
    assert_eq!(
        resp["result"]["plugins"][0]["projects"],
        json!([]),
        "{resp}"
    );
    h.shutdown().await;
}

/// 卸载默认保留数据，并且**摘掉宿主入口**：留下的 supervisor 清单会在项目
/// 下次可达时把程序再拉起来。
#[tokio::test(flavor = "multi_thread")]
async fn uninstalling_keeps_data_but_removes_the_host_entry_points() {
    let mut h = Harness::with_scope(Scope::Admin).await;
    initialize(&mut h).await;

    let fixture = crate::plugin_manager::service::tests_support::Fixture::build();
    let params = json!({
        "bundlePath": fixture.bundle().to_string_lossy(),
        "metadataPath": fixture.metadata().to_string_lossy(),
    })
    .to_string();
    let resp = call(&mut h, 1, "plugins/v1/install", &params).await;
    assert!(resp.get("error").is_none(), "install must succeed: {resp}");

    // 项目里既有插件数据，也有一个旧式 supervisor 清单。
    let project = tempfile::tempdir().expect("project");
    let data = project.path().join(".yi-agent/plugins/example-counter");
    std::fs::create_dir_all(&data).expect("data dir");
    std::fs::write(data.join("counter.json"), b"{\"n\":1}").expect("data");
    let supervisors = project.path().join(".yi-agent/supervisors");
    std::fs::create_dir_all(&supervisors).expect("supervisors");
    let manifest = supervisors.join("example-counter.json");
    std::fs::write(&manifest, b"{}").expect("supervisor manifest");

    let params = json!({
        "plugin": "example-counter",
        "project": project.path().to_string_lossy(),
        "capabilities": ["read", "write"],
    })
    .to_string();
    let resp = call(&mut h, 2, "plugins/v1/enableProject", &params).await;
    assert!(resp.get("error").is_none(), "enable must succeed: {resp}");

    let resp = call(
        &mut h,
        3,
        "plugins/v1/uninstall",
        &json!({ "plugin": "example-counter" }).to_string(),
    )
    .await;
    assert!(
        resp.get("error").is_none(),
        "uninstall must succeed: {resp}"
    );
    assert_eq!(resp["result"]["versionsRemoved"], json!(1), "{resp}");
    assert_eq!(resp["result"]["pending"], json!([]), "{resp}");

    assert!(data.exists(), "uninstall must keep user data by default");
    assert!(
        !manifest.exists(),
        "a leftover supervisor manifest restarts the plugin after uninstall"
    );
    h.shutdown().await;
}

/// `clearData` 只删点名的项目：没点名的一个字节都不动。
#[tokio::test(flavor = "multi_thread")]
async fn clearing_data_is_scoped_to_the_named_projects() {
    let mut h = Harness::with_scope(Scope::Admin).await;
    initialize(&mut h).await;

    let fixture = crate::plugin_manager::service::tests_support::Fixture::build();
    let params = json!({
        "bundlePath": fixture.bundle().to_string_lossy(),
        "metadataPath": fixture.metadata().to_string_lossy(),
    })
    .to_string();
    let resp = call(&mut h, 1, "plugins/v1/install", &params).await;
    assert!(resp.get("error").is_none(), "install must succeed: {resp}");

    let first = tempfile::tempdir().expect("first project");
    let second = tempfile::tempdir().expect("second project");
    // 授权表存的是**规范化后的项目句柄**，不是原始路径。要把句柄记下来，
    // 因为 clearData 点名的是它。
    let mut handles = Vec::new();
    for project in [&first, &second] {
        let data = project.path().join(".yi-agent/plugins/example-counter");
        std::fs::create_dir_all(&data).expect("data dir");
        std::fs::write(data.join("counter.json"), b"{\"n\":1}").expect("data");
        let params = json!({
            "plugin": "example-counter",
            "project": project.path().to_string_lossy(),
            "capabilities": ["read", "write"],
        })
        .to_string();
        let id = if project.path() == first.path() { 2 } else { 3 };
        let resp = call(&mut h, id, "plugins/v1/enableProject", &params).await;
        assert!(resp.get("error").is_none(), "enable must succeed: {resp}");
        handles.push(
            resp["result"]["project"]
                .as_str()
                .expect("handle")
                .to_string(),
        );
    }

    let params = json!({
        "plugin": "example-counter",
        "projects": [handles[0].clone()],
        "confirm": true,
    })
    .to_string();
    let resp = call(&mut h, 4, "plugins/v1/clearData", &params).await;
    assert!(
        resp.get("error").is_none(),
        "clearData must succeed: {resp}"
    );

    assert!(
        !first
            .path()
            .join(".yi-agent/plugins/example-counter")
            .exists(),
        "the named project's data must be gone"
    );
    assert!(
        second
            .path()
            .join(".yi-agent/plugins/example-counter")
            .exists(),
        "an unnamed project's data must survive: clearing is scoped, not global"
    );
    h.shutdown().await;
}
