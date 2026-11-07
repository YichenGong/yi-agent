use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn app(settings_path: std::path::PathBuf) -> axum::Router {
    yi_agent_web::router(settings_path)
}

#[tokio::test]
async fn get_config_reports_null_for_unset_and_default_source() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, r#"{"agent":{"max_turns":500}}"#).unwrap();

    let response = app(path)
        .oneshot(Request::get("/api/config").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;

    let all: Vec<serde_json::Value> = value["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["vars"].as_array().unwrap().clone())
        .collect();
    let max_turns = all.iter().find(|v| v["key"] == "agent.max_turns").unwrap();
    assert_eq!(max_turns["value"], 500);
    assert_eq!(max_turns["source"], "local");

    let ratio = all.iter().find(|v| v["key"] == "compact.ratio").unwrap();
    assert!(ratio["value"].is_null());
    assert_eq!(ratio["source"], "default");
    assert_eq!(ratio["default"], "80");
}

/// 一号缺陷的回归网：保存一次不能删掉不认识的键。
#[tokio::test]
async fn put_config_preserves_unknown_keys() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, r#"{"my_note":"keep me"}"#).unwrap();

    let response = app(path.clone())
        .oneshot(
            Request::put("/api/config")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"updates":[{"key":"agent.max_turns","value":"500"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(value["my_note"], "keep me");
    assert_eq!(value["agent"]["max_turns"], 500);
}

/// 未设置的字段不得被写成空值行。
#[tokio::test]
async fn put_config_does_not_materialize_unset_fields() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("settings.json");

    app(path.clone())
        .oneshot(
            Request::put("/api/config")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"updates":[{"key":"agent.max_turns","value":"500"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let agent = value["agent"].as_object().unwrap();
    assert_eq!(agent.len(), 1);
    assert!(value.get("compact").is_none());
}

#[tokio::test]
async fn put_config_rejects_an_invalid_value_with_400() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("settings.json");

    let response = app(path.clone())
        .oneshot(
            Request::put("/api/config")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"updates":[{"key":"compact.ratio","value":"0"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(!path.exists(), "a rejected write must not create the file");
}

/// 「真实 LLM 测试」面板已删除：路由必须是 404。
#[tokio::test]
async fn the_real_llm_test_routes_are_gone() {
    let dir = tempfile::TempDir::new().unwrap();
    for (method, url) in [
        ("GET", "/api/real-llm-test-config"),
        ("PUT", "/api/real-llm-test-config"),
        ("POST", "/api/real-llm-test-config/validate"),
        ("DELETE", "/api/real-llm-test-config/api-key"),
    ] {
        let request = match method {
            "GET" => Request::get(url),
            "PUT" => Request::put(url),
            "POST" => Request::post(url),
            _ => Request::delete(url),
        }
        .body(Body::empty())
        .unwrap();
        let response = app(dir.path().join("settings.json"))
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{method} {url} must be gone"
        );
    }
}

/// 每个字段都必须带 `label`，且与 runtime 的 FIELDS 同源。
/// 漏一个键会让前端回退成显示机器键名（`agent.max_turns`），
/// 故按 FIELDS 遍历断言，而不是抽查几个。
#[tokio::test]
async fn get_config_reports_a_label_for_every_field() {
    use yi_agent_runtime::settings;

    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("settings.json");

    let response = app(path)
        .oneshot(Request::get("/api/config").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let value = body_json(response).await;

    let vars: Vec<serde_json::Value> = value["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["vars"].as_array().unwrap().clone())
        .collect();

    assert_eq!(vars.len(), settings::FIELDS.len(), "每个字段都要出现");

    for spec in settings::FIELDS {
        let var = vars
            .iter()
            .find(|v| v["key"] == spec.key)
            .unwrap_or_else(|| panic!("missing field {}", spec.key));
        assert_eq!(
            var["label"], spec.label,
            "{} 的 label 必须来自 FIELDS，不得另写一份",
            spec.key
        );
    }
}

/// `FIELDS` 里的每个键都必须能从 `/api/config` 读回非 null 的值。
///
/// 与 `settings_rpc::tests::every_field_in_fields_can_be_read_back` 同构。
/// 这条守卫曾经缺失：`get_config_reports_a_label_for_every_field` 也遍历
/// `FIELDS`，但它只断言 `label`，而 `label` 直接来自 `spec.label`、与
/// `field_value` 无关。于是 `tui.terminal_title` 漏了 value 分支之后，
/// 接口照旧 200、label 照旧正确，只是 `value` 恒为 null——页面上那个开关
/// 永远显示「未设置」，且没有任何测试会红。
///
/// 所以这里断言 `value`，而不是 `label`。给每个字段都写一个合法值，
/// 读回后不允许有 null：漏一个分支就从这个键开始报。
#[tokio::test]
async fn get_config_reports_a_value_for_every_field() {
    use yi_agent_runtime::settings::{self, FieldType};

    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("settings.json");

    // 先按 FIELDS 把每个键都写成一个合法值，确保「读回 null」只可能是因为
    // `field_value` 漏了分支，而不是因为文件里本来就没设。
    let mut patch = serde_json::Map::new();
    for spec in settings::FIELDS {
        let value = match spec.field_type {
            FieldType::Number => serde_json::json!(spec.min.unwrap_or(1)),
            FieldType::Text => serde_json::json!("set"),
            FieldType::Select => serde_json::json!(spec.options[0]),
        };
        patch.insert(spec.key.to_string(), value);
    }
    settings::write_to(&path, &serde_json::Value::Object(patch)).unwrap();

    let response = app(path)
        .oneshot(Request::get("/api/config").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value = body_json(response).await;

    let vars: Vec<serde_json::Value> = value["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["vars"].as_array().unwrap().clone())
        .collect();

    for spec in settings::FIELDS {
        let var = vars
            .iter()
            .find(|v| v["key"] == spec.key)
            .unwrap_or_else(|| panic!("missing field {}", spec.key));
        assert!(
            !var["value"].is_null(),
            "{} is in FIELDS but field_value returned null for it \
             (the match arm in yi-agent-web/src/api.rs is missing)",
            spec.key
        );
    }
}

/// 显式 `value: null` 必须表示「删掉这个键」，而不是报错或写成 null 字面量。
#[tokio::test]
async fn put_config_with_null_value_deletes_the_key() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(
        &path,
        r#"{"agent":{"max_turns":500,"system_prompt":"keep"},"my_note":"keep me"}"#,
    )
    .unwrap();

    let response = app(path.clone())
        .oneshot(
            Request::put("/api/config")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"updates":[{"key":"agent.max_turns","value":null}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let text = std::fs::read_to_string(&path).unwrap();
    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert!(
        value["agent"].get("max_turns").is_none(),
        "null 必须删键，实际内容: {text}"
    );
    assert_eq!(
        value["agent"]["system_prompt"], "keep",
        "同组其它键必须保留"
    );
    assert_eq!(value["my_note"], "keep me", "未知顶层键必须保留");
}

/// 省略 `value` 字段等价于发 null（serde 对 Option 缺字段即 None）。
/// 这条固定该行为，避免日后被误当 bug 改来改去。
#[tokio::test]
async fn put_config_treats_a_missing_value_as_null() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, r#"{"agent":{"max_turns":500}}"#).unwrap();

    let response = app(path.clone())
        .oneshot(
            Request::put("/api/config")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"updates":[{"key":"agent.max_turns"}]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(value.get("agent").is_none(), "空壳分组应被清掉: {value}");
}

/// `groups` 必须覆盖 FIELDS 里出现的每个 group，且不重复、不遗漏。
/// 前端改为从这份数据派生侧栏，任何一侧漏组都会让整组字段消失。
#[tokio::test]
async fn get_config_groups_match_every_group_in_fields() {
    use std::collections::BTreeSet;
    use yi_agent_runtime::settings;

    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("settings.json");

    let response = app(path)
        .oneshot(Request::get("/api/config").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let value = body_json(response).await;

    let returned: Vec<&str> = value["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["name"].as_str().unwrap())
        .collect();

    let expected: BTreeSet<&str> = settings::FIELDS.iter().map(|f| f.group).collect();

    assert_eq!(
        returned.len(),
        expected.len(),
        "分组数不符；返回 {returned:?}，期望 {expected:?}"
    );
    assert_eq!(
        returned.iter().copied().collect::<BTreeSet<_>>(),
        expected,
        "分组集合不符"
    );
    assert!(
        !returned.contains(&"Tools"),
        "Tools 分组已不存在，出现它说明硬编码残留"
    );
}

/// HTML 契约：挡住「又加一行硬编码分组」这类回归。
///
/// 注意这是**弱保证**——纯文本断言，不执行 JS，无法证明渲染正确。
/// 真正的渲染正确性靠人工浏览器验收（见 plan 的人工验收清单）。
#[tokio::test]
async fn index_html_derives_groups_instead_of_hardcoding_them() {
    let dir = tempfile::TempDir::new().unwrap();
    let response = app(dir.path().join("settings.json"))
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let html = body_text(response).await;

    assert!(
        !html.contains("const tabs = ["),
        "禁止再把分组写死；应从 data.groups 派生"
    );
    assert!(html.contains("data.groups"), "分组必须来自 API 数据");
    assert!(
        html.contains("GROUP_LABELS"),
        "显示名需要映射表，否则侧栏显示英文名"
    );
    assert!(
        html.contains("v.min") && html.contains("v.max"),
        "数字输入必须带上后端给的上下限"
    );
    assert!(html.contains("reset-btn"), "必须有「恢复默认」按钮");
    assert!(
        html.contains("currentValues[v.key] = null"),
        "恢复默认的核心是把当前值置 null（save 靠差分检测收进 updates）"
    );
}

/// 把响应体当纯文本读出来（HTML 不走 JSON）。
async fn body_text(response: axum::response::Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// 手机端保存必须与桌面端走**同一个提交点**：经共享实例提交后，桌面那一侧的
/// 读者（watch 订阅者）立刻看到新值。直接用 `settings::write_to` 也能写对文件，
/// 但不会唤醒已登记的实例，热更新就会"文件变了、运行中的会话不知道"。
#[tokio::test]
async fn put_config_publishes_to_the_shared_live_instance() {
    use yi_agent_runtime::live_settings::LiveSettings;

    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("settings.json");
    // 先登记实例，模拟桌面/app-server 已经持有同一个文件。
    let live = LiveSettings::shared(path.clone());
    let mut changes = live.subscribe();

    let response = app(path.clone())
        .oneshot(
            Request::put("/api/config")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"updates":[{"key":"agent.max_turns","value":"321"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // 旧成功形状不变（只有 ok:true），前端无需改动。
    let body = body_json(response).await;
    assert_eq!(body, serde_json::json!({ "ok": true }));

    tokio::time::timeout(std::time::Duration::from_secs(5), changes.changed())
        .await
        .expect("the shared instance must be woken by a web save")
        .expect("the watch channel is alive");
    assert_eq!(live.refresh().settings.agent.max_turns, Some(321));
}

/// 旧 GET 形状必须逐字保留：`groups[].vars[].source` 是手机端"默认/已设置"
/// 标记的唯一来源，加字段可以，删字段会让老页面静默把所有行显示成默认。
#[tokio::test]
async fn get_config_keeps_the_legacy_group_and_var_shape() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(&path, r#"{"agent":{"max_turns":500}}"#).unwrap();

    let value = body_json(
        app(path)
            .oneshot(Request::get("/api/config").body(Body::empty()).unwrap())
            .await
            .unwrap(),
    )
    .await;

    assert!(value["path"].is_string());
    let groups = value["groups"].as_array().expect("groups array");
    let agent = groups.iter().find(|g| g["name"] == "agent").unwrap();
    let var = agent["vars"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["key"] == "agent.max_turns")
        .unwrap();
    for field in [
        "key", "label", "value", "default", "type", "group", "source",
    ] {
        assert!(var.get(field).is_some(), "missing {field}: {var}");
    }
}

/// GET 报告健康状态；诊断只给类别与键名，不回显用户内容。
#[tokio::test]
async fn get_config_reports_degraded_health_without_echoing_values() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("settings.json");
    std::fs::write(
        &path,
        r#"{"agent":{"max_turns":"WEB-SECRET","system_prompt":"WEB-SECRET"}}"#,
    )
    .unwrap();

    let value = body_json(
        app(path)
            .oneshot(Request::get("/api/config").body(Body::empty()).unwrap())
            .await
            .unwrap(),
    )
    .await;

    assert_eq!(value["live_apply_version"], 1);
    assert_eq!(value["health"]["state"], "degraded");
    let health = value["health"].to_string();
    assert!(!health.contains("WEB-SECRET"), "health leaked: {health}");
    // 旧形状仍在：降级不能让页面变成空白。
    assert!(value["groups"].as_array().is_some_and(|g| !g.is_empty()));
}
