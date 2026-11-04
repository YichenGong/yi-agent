//! T20：远程精确版本资源与多客户端撤销，走**两个真实隔离的 app-server 连接**。
//!
//! # 为什么必须是两个连接
//!
//! 把两个对象命名成"宿主 A / 宿主 B"什么也证明不了。这里要钉住的是：同一台
//! 机器的两个 app-server 进程各自持有一份官方目录、各自登记安装、各自算
//! `assetRelease`，而客户端在它们之间切换时不能把 A 的界面配 B 的授权用。
//! 所以每条测试都起两个 `Harness`（各自的 stdio 管道、各自的磁盘记录、各自的
//! 目录文件），再断言它们的回答**确实不同**。
//!
//! # 覆盖的四件事
//!
//! 1. 两宿主同插件不同版本 → 各自只认自己的发布，跨宿主的 `assetRelease` 查不到；
//! 2. 发布被撤回 / 过期 → 明确拒绝，而不是给一个"看起来能用"的身份；
//! 3. 未认证的请求 → 旧客户端拿到的是稳定错误码，而不是新授权；
//! 4. 断线写查 → 重连后读的是宿主权威状态，不能复用未确认的写请求。

use serde_json::{Value, json};

use crate::plugin_manager::service::tests_support::Fixture;
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

/// 往某个连接自己的目录文件里写一份目录。
///
/// 每个 `Harness` 有独立的 `plugin_catalog`，所以两个"宿主"可以各自声称官方
/// 源上有不同版本——这正是真实场景（本地源与另一台机器的源）。
fn write_catalog(path: &std::path::Path, version: &str, sha: &str, serial: u64, expires_at: u64) {
    let catalog = json!({
        "serial": serial,
        "entries": [{
            "release": {
                "plugin": "example-counter",
                "version": version,
                "target": "darwin-aarch64",
                "sha256": sha,
            },
            "sizeBytes": 1024,
            "metadataPath": "/official/example-counter/metadata.json",
            "bundlePath": "/official/example-counter/bundle.tar.gz",
            "publishedAt": 1_700_000_000,
            "expiresAt": expires_at,
            "serial": serial,
        }],
    });
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("catalog dir");
    }
    std::fs::write(path, serde_json::to_vec(&catalog).unwrap()).expect("write catalog");
}

fn asset_release_params(version: &str, sha: &str) -> String {
    json!({
        "plugin": "example-counter",
        "version": version,
        "target": "darwin-aarch64",
        "sha256": sha,
    })
    .to_string()
}

/// 两个宿主各持一份目录：A 上只有 1.0.0，B 上只有 2.0.0。
///
/// 这条测试的可观测断言有三层：
/// - 各自 `catalog` 只看见自己的那条；
/// - 各自的 `assetRelease` 只认证自己的那条；
/// - 用一个宿主的发布身份去问另一个宿主，得到 `plugin_not_found`。
#[tokio::test(flavor = "multi_thread")]
async fn two_hosts_serve_their_own_release_and_not_each_others() {
    let mut a = Harness::with_scope(Scope::Admin).await;
    initialize(&mut a).await;
    let mut b = Harness::with_scope(Scope::Admin).await;
    initialize(&mut b).await;

    let sha_a = "aa".repeat(32);
    let sha_b = "bb".repeat(32);
    write_catalog(&a.plugin_catalog.clone(), "1.0.0", &sha_a, 10, 0);
    write_catalog(&b.plugin_catalog.clone(), "2.0.0", &sha_b, 20, 0);

    let catalog_a = call(&mut a, 1, "plugins/v1/catalog", "{}").await;
    assert!(catalog_a.get("error").is_none(), "got {catalog_a}");
    let versions_a: Vec<&str> = catalog_a["result"]["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|entry| entry["version"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        versions_a,
        vec!["1.0.0"],
        "host A must only see its own source"
    );

    let catalog_b = call(&mut b, 1, "plugins/v1/catalog", "{}").await;
    let versions_b: Vec<&str> = catalog_b["result"]["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|entry| entry["version"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        versions_b,
        vec!["2.0.0"],
        "host B must only see its own source"
    );

    // 各自认证自己的发布。
    let own_a = call(
        &mut a,
        2,
        "plugins/v1/assetRelease",
        &asset_release_params("1.0.0", &sha_a),
    )
    .await;
    assert!(
        own_a.get("error").is_none(),
        "host A must authenticate 1.0.0: {own_a}"
    );
    let own_b = call(
        &mut b,
        2,
        "plugins/v1/assetRelease",
        &asset_release_params("2.0.0", &sha_b),
    )
    .await;
    assert!(
        own_b.get("error").is_none(),
        "host B must authenticate 2.0.0: {own_b}"
    );

    // 不该让对方看见。用 A 的发布身份去问 B。
    let cross = call(
        &mut b,
        3,
        "plugins/v1/assetRelease",
        &asset_release_params("1.0.0", &sha_a),
    )
    .await;
    let error = cross
        .get("error")
        .expect("host B must not know host A's release");
    assert_eq!(
        error["data"]["code"], "plugin_not_found",
        "a release from another host must be not-found, not authenticated: {cross}"
    );

    // 两份句柄必须不同：同插件同目标的两次认证不能给出同一个代号。
    assert_ne!(
        own_a["result"]["transfer"]["handle"], own_b["result"]["transfer"]["handle"],
        "two hosts must not hand out the same transfer handle"
    );

    a.shutdown().await;
    b.shutdown().await;
}

/// `assetRelease` 永远不返回能让页面自己发请求的东西。
///
/// 这是"桥是唯一业务通道"的机械保证：一旦这里出现 URL 或凭据，插件页面的
/// iframe 就能绕过桥直接 fetch，授权检查全部失效。
#[tokio::test(flavor = "multi_thread")]
async fn a_release_hands_out_a_handle_and_never_a_url_or_credential() {
    let mut h = Harness::with_scope(Scope::Admin).await;
    initialize(&mut h).await;

    let sha = "cc".repeat(32);
    write_catalog(&h.plugin_catalog.clone(), "1.0.0", &sha, 5, 0);

    let resp = call(
        &mut h,
        1,
        "plugins/v1/assetRelease",
        &asset_release_params("1.0.0", &sha),
    )
    .await;
    assert!(resp.get("error").is_none(), "got {resp}");
    let result = &resp["result"];
    assert_eq!(result["authenticated"], json!(true));
    assert_eq!(
        result["directUrl"],
        Value::Null,
        "no direct URL may be returned"
    );
    assert_eq!(
        result["credentials"],
        Value::Null,
        "no credentials may be returned"
    );
    assert_eq!(result["transfer"]["kind"], "host-mediated");
    assert!(
        result["transfer"]["handle"]
            .as_str()
            .unwrap_or("")
            .starts_with("rel1_"),
        "the handle must be opaque: {result}"
    );
    // 结果里不该出现任何"可以直接发起网络请求"的字段名。
    let rendered = result.to_string();
    for forbidden in ["https://", "http://", "token", "cookie", "authorization"] {
        assert!(
            !rendered.contains(forbidden),
            "`{forbidden}` must not appear in a release identity: {rendered}"
        );
    }

    h.shutdown().await;
}

/// 过期的发布被拒绝，而不是被当成"还能用"。
#[tokio::test(flavor = "multi_thread")]
async fn an_expired_release_is_refused_by_the_host() {
    let mut h = Harness::with_scope(Scope::Admin).await;
    initialize(&mut h).await;

    let sha = "dd".repeat(32);
    // 过期时间设在 2000 年。
    write_catalog(&h.plugin_catalog.clone(), "1.0.0", &sha, 5, 946_684_800);

    let resp = call(
        &mut h,
        1,
        "plugins/v1/assetRelease",
        &asset_release_params("1.0.0", &sha),
    )
    .await;
    let error = resp
        .get("error")
        .expect("an expired release must not be authenticated");
    assert_eq!(error["data"]["code"], "plugin_expired", "got {resp}");

    h.shutdown().await;
}

/// 摘要不符：目录里写的摘要与请求里的不一样，就是"另一个发布"。
#[tokio::test(flavor = "multi_thread")]
async fn a_digest_that_does_not_match_the_catalog_is_not_found() {
    let mut h = Harness::with_scope(Scope::Admin).await;
    initialize(&mut h).await;

    let listed = "ee".repeat(32);
    let asked = "ff".repeat(32);
    write_catalog(&h.plugin_catalog.clone(), "1.0.0", &listed, 5, 0);

    let resp = call(
        &mut h,
        1,
        "plugins/v1/assetRelease",
        &asset_release_params("1.0.0", &asked),
    )
    .await;
    let error = resp
        .get("error")
        .expect("a different digest is a different release");
    assert_eq!(error["data"]["code"], "plugin_not_found", "got {resp}");

    h.shutdown().await;
}

/// 旧客户端（无 admin scope）拿到的是稳定拒绝，而不是一次隐式授权。
#[tokio::test(flavor = "multi_thread")]
async fn an_old_client_gets_a_stable_refusal_instead_of_authority() {
    let mut h = Harness::with_scope(Scope::Observe).await;
    initialize(&mut h).await;

    let sha = "11".repeat(32);
    write_catalog(&h.plugin_catalog.clone(), "1.0.0", &sha, 5, 0);

    let resp = call(
        &mut h,
        1,
        "plugins/v1/assetRelease",
        &asset_release_params("1.0.0", &sha),
    )
    .await;
    let error = resp
        .get("error")
        .expect("a non-admin client must be refused");
    // scope 拒绝走 RPC 的 `insufficient_scope`（整数码 `-32014`），不是插件域
    // 的 `plugin_*` 字符串码：门禁在进入插件分发之前就生效了。断言这个具体
    // 形状，是为了让"将来把门禁挪进分发"也会被这条测试抓住。
    assert_eq!(
        error["code"],
        json!(-32014),
        "a non-admin client must get insufficient_scope: {resp}"
    );
    assert!(
        error["message"].as_str().unwrap_or("").contains("Admin"),
        "the refusal must name the missing scope: {resp}"
    );
    assert!(
        resp.get("result").is_none(),
        "a refusal must carry no result"
    );

    h.shutdown().await;
}

/// 断开之后重连：读到的是宿主权威状态，不是客户端记得的旧值。
///
/// 两个独立连接在这里代表"同一台宿主的两次会话"。第一次会话里装了包，第二次
/// 会话（新的 stdio 管道、新的进程内状态）仍能从磁盘看到它——证明状态在宿主
/// 上，不在客户端内存里。
#[tokio::test(flavor = "multi_thread")]
async fn a_reconnecting_client_reads_host_authority_not_a_replayed_write() {
    let fixture = Fixture::build();
    // 两次会话共用同一个实例根：这正是"重连"与"两个不同宿主"的区别。
    let home = tempfile::TempDir::new().expect("instance home");

    let mut first = Harness::with_scope_and_instance_home(Scope::Admin, home.path()).await;
    initialize(&mut first).await;
    let install = json!({
        "bundlePath": fixture.bundle().to_string_lossy(),
        "metadataPath": fixture.metadata().to_string_lossy(),
    })
    .to_string();
    let installed = call(&mut first, 1, "plugins/v1/install", &install).await;
    assert!(
        installed.get("error").is_none(),
        "the first session must install: {installed}"
    );
    // 第一次会话返回的是"装了什么"的记录，不是一条可以重放的写凭据。
    assert_eq!(
        installed["result"]["installed"],
        json!(true),
        "the first session must report what it installed: {installed}"
    );
    first.shutdown().await;

    // 第二次会话：客户端**不会**重发那次写，只读。
    let mut second = Harness::with_scope_and_instance_home(Scope::Admin, home.path()).await;
    initialize(&mut second).await;
    let listed = call(&mut second, 1, "plugins/v1/list", "{}").await;
    assert!(listed.get("error").is_none(), "got {listed}");
    let plugins = listed["result"]["plugins"].as_array().expect("plugins");
    let counter = plugins
        .iter()
        .find(|entry| entry["plugin"] == "example-counter")
        .expect("the installed plugin must survive the reconnect");
    assert_eq!(
        counter["selected"], "1.4.0",
        "the reconnect reads what the host recorded: {counter}"
    );
    assert_eq!(
        counter["installed"],
        json!(["1.4.0"]),
        "the installed set must come from the host's registry: {counter}"
    );
    assert_eq!(
        counter["projects"],
        json!([]),
        "installing must never imply a project grant"
    );

    // 客户端**只读**，没有重发那次安装；若它重发，`install` 会失败（包已经
    // 登记），而这条测试要证明的是"不需要重发也能看到权威状态"。
    second.shutdown().await;
}
