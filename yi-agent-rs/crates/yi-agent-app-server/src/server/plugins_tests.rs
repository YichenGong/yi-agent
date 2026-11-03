use super::{list_installed_plugins, plugin_scope};
use serde_json::json;
use std::path::{Path, PathBuf};

#[test]
fn an_absent_supervisors_directory_lists_no_plugins() {
    let dir = tempfile::tempdir().unwrap();
    assert!(list_installed_plugins(dir.path(), None).is_empty());
}

/// 设置页「技能」面板的两个 RPC：三层分组、正文读取、越界拒绝，且都必须落在
/// `Observe` 档（iOS 设备默认 Control，加闸即手机端不可用）。
mod skill_rpc_tests {
    // 本模块嵌在 `plugins_tests` 里，故 harness 要用 crate 全路径（`super::tests`
    // 会解析到 `plugins_tests::tests`，那里不存在）。
    use crate::protocol::Scope;
    use crate::server::tests::{Harness, initialize};

    /// 在隔离 home 里造一个用户级 skill，返回它的 SKILL.md 路径。
    fn seed_user_skill(h: &Harness, name: &str) -> std::path::PathBuf {
        let dir = h.skills_home().join(".yi-agent/skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("SKILL.md");
        std::fs::write(
            &path,
            format!("---\nname: {name}\ndescription: 测试用。\n---\n# {name}\n\n正文"),
        )
        .unwrap();
        path
    }

    /// 握手后再发请求并取回该 id 的响应——未 initialize 的客户端除
    /// `initialize` 外一律被拒，少了这一步所有用例都会红在 `-32010`。
    async fn call(h: &mut Harness, id: i64, method: &str, params: &str) -> serde_json::Value {
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"{method}","params":{params}}}"#
        ))
        .await;
        // 按 id 收敛:进程守望者可能把通知插进同一路流。
        for _ in 0..16 {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(id)) {
                return v;
            }
        }
        panic!("no response for id {id}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mcp_list_is_available_at_observe_scope() {
        let mut h = (Harness::with_scope(Scope::Observe)).await;
        initialize(&mut h).await;
        let resp = call(&mut h, 1, "mcp/list", "{}").await;
        assert!(
            resp.get("error").is_none(),
            "observe must list mcp servers: {resp}"
        );
        assert!(resp["result"]["servers"].is_array());
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn skill_list_is_available_at_observe_scope() {
        let mut h = (Harness::with_scope(Scope::Observe)).await;
        initialize(&mut h).await;
        let resp = call(&mut h, 1, "skill/list", "{}").await;
        assert!(
            resp.get("error").is_none(),
            "observe must list skills: {resp}"
        );
        assert!(resp["result"]["groups"].is_array());
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn skill_read_is_available_at_observe_scope() {
        let mut h = (Harness::with_scope(Scope::Observe)).await;
        let path = seed_user_skill(&h, "user-skill");
        initialize(&mut h).await;
        let resp = call(
            &mut h,
            2,
            "skill/read",
            &format!(r#"{{"path":"{}"}}"#, path.to_string_lossy()),
        )
        .await;
        assert!(
            resp.get("error").is_none(),
            "observe must read skills: {resp}"
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn skill_list_groups_the_installed_skills() {
        let mut h = Harness::new();
        seed_user_skill(&h, "user-skill");
        initialize(&mut h).await;
        let resp = call(&mut h, 3, "skill/list", "{}").await;
        let groups = resp["result"]["groups"].as_array().unwrap();
        let user = groups
            .iter()
            .find(|g| g["scope"] == "user")
            .expect("user group present");
        assert!(
            user["skills"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["name"] == "user-skill")
        );
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn skill_read_returns_the_body() {
        let mut h = Harness::new();
        let path = seed_user_skill(&h, "user-skill");
        initialize(&mut h).await;
        let resp = call(
            &mut h,
            4,
            "skill/read",
            &format!(r#"{{"path":"{}"}}"#, path.to_string_lossy()),
        )
        .await;
        assert!(resp.get("error").is_none(), "read failed: {resp}");
        assert_eq!(resp["result"]["body"], "# user-skill\n\n正文");
        h.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn skill_read_refuses_a_path_outside_the_roots() {
        let mut h = Harness::new();
        let outside = h.skills_home().join("outside/SKILL.md");
        std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
        std::fs::write(&outside, "---\nname: x\ndescription: y\n---\nsecret").unwrap();
        initialize(&mut h).await;
        let resp = call(
            &mut h,
            5,
            "skill/read",
            &format!(r#"{{"path":"{}"}}"#, outside.to_string_lossy()),
        )
        .await;
        assert_eq!(resp["error"]["code"], -32602, "{resp}");
        h.shutdown().await;
    }
}

/// 通用插件通道的作用域：给了非空 `project` 就用它，否则回落宿主 workdir。
/// 看板按项目安装，桌面侧车 workdir 是 home，这一条是「看板跑着、设置页却
/// 空着」能修好的前提。
#[test]
fn the_plugin_scope_follows_an_explicit_project_and_defaults_to_the_workdir() {
    let workdir = Path::new("/tmp/yi-agent-host-workdir");
    assert_eq!(
        plugin_scope(workdir, &serde_json::json!({})),
        workdir.to_path_buf()
    );
    assert_eq!(
        plugin_scope(workdir, &serde_json::json!({ "project": "/p/proj" })),
        PathBuf::from("/p/proj")
    );
    // 空/全空白都不算「指定了项目」，回落到 workdir。
    assert_eq!(
        plugin_scope(workdir, &serde_json::json!({ "project": "" })),
        workdir.to_path_buf()
    );
    assert_eq!(
        plugin_scope(workdir, &serde_json::json!({ "project": "   " })),
        workdir.to_path_buf()
    );
}

/// 端到端复现并锁死根因：宿主 workdir（= 桌面侧车的 home）下没有清单，所以
/// 不带 `project` 的 `plugins/list` 必须为空；带上项目后才能看到该项目里装的
/// 看板。修复前第二段断言为空——正是用户看到的现象。
#[tokio::test(flavor = "multi_thread")]
async fn a_project_scoped_plugins_list_finds_a_project_installed_plugin() {
    use super::tests::{Harness, initialize, test_config};

    let home = tempfile::tempdir().unwrap();
    let mut cfg = test_config();
    cfg.workdir = home.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    initialize(&mut h).await;

    let project = tempfile::tempdir().unwrap();
    let sup = project.path().join(".yi-agent/supervisors");
    std::fs::create_dir_all(&sup).unwrap();
    std::fs::write(
        sup.join("superpowers-kanban.json"),
        r#"{"name":"superpowers-kanban","command":"x","switch_key":"superpowers_kanban","query_socket":"{state_dir}/superpowers-kanban.sock"}"#,
    )
    .unwrap();

    // 不带 project：读的是宿主 workdir，这里没有清单。
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"plugins/list","params":{}}"#)
        .await;
    let bare = read_response(&mut h, 2).await;
    assert_eq!(
        bare["result"]["plugins"].as_array().unwrap().len(),
        0,
        "宿主 workdir 下不该凭空冒出插件：{bare}"
    );

    // 带 project：枚举该项目自己的清单，看到看板。
    h.send(
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "plugins/list",
            "params": { "project": project.path().to_string_lossy() },
        })
        .to_string(),
    )
    .await;
    let scoped = read_response(&mut h, 3).await;
    let plugins = scoped["result"]["plugins"].as_array().unwrap();
    assert_eq!(plugins.len(), 1, "按项目作用域应看到看板：{scoped}");
    assert_eq!(plugins[0]["name"], "superpowers-kanban");
    assert_eq!(plugins[0]["queryable"], true);
    h.shutdown().await;
}

/// 与 `plugins/list` 同源的根因回归：`plugin/settings/read` 也必须按项目
/// 作用域。不带 `project` 时，宿主在 home 下找不到清单，读成
/// `plugin_not_installed`（正是用户看到「设置页里没有看板」的那一步）；带上
/// 项目后才会拨该项目 daemon 的 socket 并拿回设置。
#[tokio::test(flavor = "multi_thread")]
async fn project_scoped_settings_read_reaches_the_project_daemon() {
    use super::plugin_query_tests::fake_daemon;
    use super::tests::{Harness, initialize, test_config};

    let home = tempfile::tempdir().unwrap();
    let mut cfg = test_config();
    cfg.workdir = home.path().to_path_buf();
    let mut h = Harness::with_cfg(cfg).await;
    initialize(&mut h).await;

    let project = tempfile::tempdir().unwrap();
    let sup = project.path().join(".yi-agent/supervisors");
    std::fs::create_dir_all(&sup).unwrap();
    std::fs::write(
        sup.join("superpowers-kanban.json"),
        r#"{"name":"superpowers-kanban","command":"x","switch_key":"superpowers_kanban","query_socket":"{state_dir}/superpowers-kanban.sock"}"#,
    )
    .unwrap();
    let (_socket, _seen, handle) = fake_daemon(
        project.path(),
        json!({
            "type": "PluginResult",
            "value": { "settings": { "default_max_tasks": 3, "interval_secs": 10, "windows": [] } },
        }),
    );

    // 不带 project：宿主 workdir（= home）下没有清单 → 读成没装。
    h.send(
        r#"{"jsonrpc":"2.0","id":2,"method":"plugin/settings/read","params":{"plugin":"superpowers-kanban"}}"#,
    )
    .await;
    let bare = read_response(&mut h, 2).await;
    assert_eq!(
        bare["error"]["data"]["code"], "plugin_not_installed",
        "home 作用域下不该声称看板已装：{bare}"
    );

    // 带 project：拨该项目的 daemon，拿回它自己的设置。
    h.send(
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "plugin/settings/read",
            "params": {
                "plugin": "superpowers-kanban",
                "project": project.path().to_string_lossy(),
            },
        })
        .to_string(),
    )
    .await;
    let scoped = read_response(&mut h, 3).await;
    assert_eq!(
        scoped["result"]["settings"]["interval_secs"], 10,
        "按项目作用域应读到设置：{scoped}"
    );
    handle.join().unwrap();
    h.shutdown().await;
}

/// 读到 id 匹配的响应，跳过中间的通知帧。
async fn read_response(h: &mut super::tests::Harness, id: i64) -> serde_json::Value {
    loop {
        let value = h.read_value().await;
        if value.get("id") == Some(&serde_json::json!(id)) {
            return value;
        }
    }
}

#[test]
fn a_manifest_with_a_query_socket_is_queryable() {
    let dir = tempfile::tempdir().unwrap();
    let sup = dir.path().join(".yi-agent/supervisors");
    std::fs::create_dir_all(&sup).unwrap();
    std::fs::write(
        sup.join("superpowers-kanban.json"),
        r#"{"name":"superpowers-kanban","command":"x","switch_key":"superpowers_kanban","query_socket":"{state_dir}/superpowers-kanban.sock"}"#,
    )
    .unwrap();
    std::fs::write(
        sup.join("plain.json"),
        r#"{"name":"plain","command":"y","switch_key":"plain_on"}"#,
    )
    .unwrap();
    let mut plugins = list_installed_plugins(dir.path(), None);
    plugins.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(plugins.len(), 2);
    assert_eq!(plugins[0].name, "plain");
    assert!(!plugins[0].queryable);
    assert_eq!(plugins[1].name, "superpowers-kanban");
    assert!(plugins[1].queryable);
    assert_eq!(plugins[1].switch_key, "superpowers_kanban");
}

/// 开关关着时仍要列出这个插件——用户得能在设置页里看见它、把它打开——
/// 但必须标成停用，否则设置页会显示一个其实没在跑的插件是"正常"的。
#[test]
fn a_switched_off_plugin_is_still_listed_but_marked_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let sup = dir.path().join(".yi-agent/supervisors");
    std::fs::create_dir_all(&sup).unwrap();
    std::fs::write(
        sup.join("superpowers-kanban.json"),
        r#"{"name":"superpowers-kanban","command":"x","switch_key":"superpowers_kanban"}"#,
    )
    .unwrap();

    let flags = dir.path().join("flags.json");
    std::fs::write(&flags, r#"{"version":1,"flags":{}}"#).unwrap();
    let gate = crate::plugin_gate::Gate {
        build_kind: "dev",
        flags_path: flags.clone(),
    };

    let plugins = list_installed_plugins(dir.path(), Some(&gate));
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0].name, "superpowers-kanban");
    assert!(!plugins[0].enabled, "off must read as disabled");

    std::fs::write(
        &flags,
        r#"{"version":1,"flags":{"superpowers_kanban":true}}"#,
    )
    .unwrap();
    let plugins = list_installed_plugins(dir.path(), Some(&gate));
    assert!(plugins[0].enabled, "on must read as enabled");
}

/// 没有 `Gate` 时不判定：显示"关着"会让设置页凭空报一片停用。
#[test]
fn without_a_gate_a_plugin_is_not_reported_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let sup = dir.path().join(".yi-agent/supervisors");
    std::fs::create_dir_all(&sup).unwrap();
    std::fs::write(
        sup.join("superpowers-kanban.json"),
        r#"{"name":"superpowers-kanban","command":"x","switch_key":"superpowers_kanban"}"#,
    )
    .unwrap();
    assert!(list_installed_plugins(dir.path(), None)[0].enabled);
}

#[test]
fn a_plugin_not_in_the_manifests_is_not_installed() {
    let dir = tempfile::tempdir().unwrap();
    let error =
        super::plugin_query_self(dir.path(), "ghost", "settings.read", serde_json::json!({}))
            .unwrap_err();
    assert_eq!(error.code, "plugin_not_installed", "{error}");
}

#[test]
fn an_installed_plugin_without_a_query_socket_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let sup = dir.path().join(".yi-agent/supervisors");
    std::fs::create_dir_all(&sup).unwrap();
    std::fs::write(
        sup.join("quiet.json"),
        r#"{"name":"quiet","command":"y","switch_key":"quiet_on"}"#,
    )
    .unwrap();
    let error =
        super::plugin_query_self(dir.path(), "quiet", "settings.read", serde_json::json!({}))
            .unwrap_err();
    assert_eq!(error.code, "plugin_unavailable", "{error}");
}
