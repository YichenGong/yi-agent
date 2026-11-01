use super::*;
// 与 `board_rpc_tests` 共用同一份泵（原先两边各抄了一份）。
use crate::test_support::rpc_over_stdio as rpc;

/// `resident/list` 走完整 dispatch（域路由 + handler + 线格式），插进
/// **已有**的登记目录，故 harness 的隔离 tempdir 就是数据源。
#[tokio::test(flavor = "multi_thread")]
async fn resident_list_answers_over_the_wire_from_the_injected_registry() {
    let mut h = tests::Harness::new();
    tests::initialize(&mut h).await;
    yi_agent_store::resident::require(
        h.resident_dir.path(),
        Path::new("/registered/project"),
        "some-plugin",
    )
    .unwrap();

    let response = rpc(&mut h, 2, "resident/list", json!({})).await;

    assert_eq!(
        response["result"]["projects"][0]["project"], "/registered/project",
        "{response}"
    );
    assert_eq!(
        response["result"]["projects"][0]["required_by"][0], "some-plugin",
        "{response}"
    );
    h.shutdown().await;
}

/// 没有登记表时报空表（不是错误）：侧栏据此不画条目，与「登记表还没有」
/// 是同一件事。
#[tokio::test(flavor = "multi_thread")]
async fn resident_list_is_empty_and_not_an_error_before_anything_registers() {
    let mut h = tests::Harness::new();
    tests::initialize(&mut h).await;

    let response = rpc(&mut h, 2, "resident/list", json!({})).await;

    assert_eq!(response["result"], json!({ "projects": [] }), "{response}");
    assert!(response.get("error").is_none(), "{response}");
    h.shutdown().await;
}

/// 未登记的项目照样能被转发：通用通道不看登记表（门控已摘）。
#[tokio::test(flavor = "multi_thread")]
async fn plugin_query_answers_for_a_project_that_never_registered() {
    let mut h = tests::Harness::new();
    tests::initialize(&mut h).await;
    // 未登记的目录也得能在磁盘上存在；用隔离 tempdir 造一个。
    let project_dir = tempfile::TempDir::new().unwrap();
    let project = project_dir.path().join("never-registered");
    std::fs::create_dir_all(&project).unwrap();

    let response = rpc(
        &mut h,
        2,
        "plugin/query",
        json!({
            "project": project.to_string_lossy(),
            "plugin": "superpowers-kanban",
            "method": "list",
            "params": {},
        }),
    )
    .await;

    // 没有 daemon 在答，于是失败——但失败的理由是「拨不到」，不是「没有看板」。
    assert_eq!(
        response["error"]["data"]["code"], "daemon_unavailable",
        "{response}"
    );
    h.shutdown().await;
}
