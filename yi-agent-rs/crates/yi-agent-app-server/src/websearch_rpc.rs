//! `websearch/*` RPC：桌面设置页「网页搜索」Tab 的读写与连通性测试。
//!
//! 三类方法对应两个权限档位：`get` 只读盘且只回掩码（`Observe`）；
//! `set` 与 `test` 触及密钥且会落盘/发网络请求（`Control`），档位判在
//! `rpc/websearch.rs`。
//!
//! 与 `model/*` 同构：密钥从不下发明文，`set` 收到掩码值时跳过该字段。
//!
//! 入口是 async 的：`test` 要真的发一次 HTTPS 请求。曾打算用
//! `block_in_place` 把同步入口包成同步，但 app-server 的测试里既有多线程
//! runtime 也有 current_thread runtime，后者会 panic，故统一走 await。

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use yi_agent_runtime::models::mask_key;
use yi_agent_tools::{
    BochaSearchProvider, EngineConfig, Freshness, WebSearchConfig, WebSearchProvider,
    load_websearch_config_from,
};

use crate::protocol::RpcError;

pub async fn handle_websearch_request(
    path: &Path,
    method: &str,
    params: &Value,
) -> Result<Value, RpcError> {
    match method {
        "websearch/get" => get(path),
        "websearch/set" => set(path, params),
        "websearch/test" => test(path, params).await,
        _ => Err(RpcError::method_not_found(method)),
    }
}

fn get(path: &Path) -> Result<Value, RpcError> {
    let cfg = load_websearch_config_from(path);
    let engines: serde_json::Map<String, Value> = cfg
        .engines
        .iter()
        .map(|(id, e)| (id.clone(), engine_view(e)))
        .collect();
    Ok(json!({
        "enabled": cfg.enabled,
        "active_engine": cfg.active_engine,
        "engines": Value::Object(engines),
        "path": path.to_string_lossy(),
    }))
}

fn engine_view(e: &EngineConfig) -> Value {
    json!({
        "api_key_masked": mask_key(&e.api_key),
        // 只看文件：面板据此决定要不要提示「未配置」。
        "has_key": e.resolved_api_key().is_some(),
        "base_url": e.resolved_base_url(),
        "count": e.clamped_count(),
        "freshness": e.freshness.as_bocha_str(),
    })
}

fn set(path: &Path, params: &Value) -> Result<Value, RpcError> {
    let mut cfg = load_websearch_config_from(path);

    if let Some(enabled) = params.get("enabled").and_then(Value::as_bool) {
        cfg.enabled = enabled;
    }
    if let Some(id) = params.get("active_engine").and_then(Value::as_str) {
        if cfg.engines.contains_key(id) {
            cfg.active_engine = id.to_string();
        }
    }
    if let Some(engines) = params.get("engines").and_then(Value::as_object) {
        for (id, patch) in engines {
            let entry = cfg.engines.entry(id.clone()).or_default();
            apply_engine_patch(entry, patch);
        }
    }

    save_config(path, &cfg)
        .map_err(|e| RpcError::internal(format!("failed to write config: {e}")))?;
    Ok(json!({ "ok": true }))
}

fn apply_engine_patch(entry: &mut EngineConfig, patch: &Value) {
    if let Some(key) = patch.get("api_key").and_then(Value::as_str) {
        // 掩码值不得回写：它代表「用户没改」，不是新 key。
        let is_mask = key.contains('•');
        if !is_mask && !key.is_empty() {
            entry.api_key = key.to_string();
        }
    }
    if let Some(url) = patch.get("base_url").and_then(Value::as_str) {
        entry.base_url = url.trim().to_string();
    }
    if let Some(count) = patch.get("count").and_then(Value::as_u64) {
        entry.count = (count as usize).clamp(1, 50);
    }
    if let Some(f) = patch.get("freshness").and_then(Value::as_str) {
        entry.freshness = Freshness::parse(f);
    }
}

/// 原子落盘：临时文件 + rename，临时名含 pid 与递增序号，避免并发写者互相截断
/// （与 `settings_store::temp_path_for` 同一约定）。
fn save_config(path: &Path, cfg: &WebSearchConfig) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // 读-改-写：以既有对象为底，只覆盖本模块拥有的键，其余顶层键原样保留。
    let mut object = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    let ours = serde_json::to_value(cfg).map_err(|e| std::io::Error::other(e.to_string()))?;
    if let Some(map) = ours.as_object() {
        for (k, v) in map {
            object.insert(k.clone(), v.clone());
        }
    }

    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("json.{}.{}.tmp", std::process::id(), seq));
    std::fs::write(&tmp, serde_json::to_vec_pretty(&Value::Object(object))?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

async fn test(path: &Path, params: &Value) -> Result<Value, RpcError> {
    let cfg = load_websearch_config_from(path);
    let engine = cfg.active().cloned().unwrap_or_default();

    // 参数里的明文 key 优先（用户刚填还没保存）；否则按磁盘上的值 + 环境变量兜底。
    let explicit = params
        .get("api_key")
        .and_then(Value::as_str)
        .filter(|k| !k.is_empty() && !k.contains('•'))
        .map(str::to_string);
    let api_key = match explicit.or_else(|| engine.resolved_api_key()) {
        Some(key) => key,
        None => {
            return Err(RpcError::invalid_params(
                "no api key: enter one before testing the connection",
            ));
        }
    };
    let base_url = params
        .get("base_url")
        .and_then(Value::as_str)
        .filter(|u| !u.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| engine.resolved_base_url());
    let freshness = params
        .get("freshness")
        .and_then(Value::as_str)
        .map(Freshness::parse)
        .unwrap_or(engine.freshness);

    let provider = BochaSearchProvider::with_options(api_key, base_url, freshness);
    let started = Instant::now();

    // 一次只需一条结果就能判断连通性；超时与 provider 的 30s 客户端超时对齐。
    let result =
        tokio::time::timeout(Duration::from_secs(30), provider.search("yi-agent", 1)).await;

    let elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(match result {
        Ok(Ok(results)) => json!({
            "ok": true,
            "message": format!("连接正常，返回 {} 条结果", results.len()),
            "elapsed_ms": elapsed_ms,
        }),
        Ok(Err(e)) => json!({
            "ok": false,
            "message": describe_error(&e.to_string()),
            "elapsed_ms": elapsed_ms,
        }),
        Err(_) => json!({
            "ok": false,
            "message": "请求超时（30 秒）",
            "elapsed_ms": elapsed_ms,
        }),
    })
}

/// 把 provider 的英文错误翻成用户能动的提示。
fn describe_error(raw: &str) -> String {
    if raw.contains("invalid API key") {
        "API Key 无效，请检查后重试".to_string()
    } else if raw.contains("insufficient balance") {
        "账户余额不足".to_string()
    } else if raw.contains("rate limited") {
        "请求太频繁，已被限流".to_string()
    } else {
        format!("连接失败：{raw}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp_cfg(json_text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("websearch.json");
        if !json_text.is_empty() {
            std::fs::write(&path, json_text).unwrap();
        }
        (dir, path)
    }

    #[tokio::test]
    async fn get_never_returns_the_plaintext_key() {
        let (_d, p) = tmp_cfg(r#"{"engines":{"bocha":{"api_key":"sk-secret-1234"}}}"#);
        let out = handle_websearch_request(&p, "websearch/get", &json!({}))
            .await
            .unwrap();
        let bocha = &out["engines"]["bocha"];
        assert_eq!(bocha["api_key_masked"], "••••1234");
        assert_eq!(bocha["has_key"], true);
        // 明文绝不出现在整个响应里。
        assert!(!out.to_string().contains("sk-secret-1234"));
    }

    #[tokio::test]
    async fn get_defaults_when_no_file_exists() {
        let (_d, p) = tmp_cfg("");
        let out = handle_websearch_request(&p, "websearch/get", &json!({}))
            .await
            .unwrap();
        assert_eq!(out["enabled"], true);
        assert_eq!(out["active_engine"], "bocha");
        assert_eq!(out["engines"]["bocha"]["count"], 25);
        assert_eq!(out["engines"]["bocha"]["freshness"], "noLimit");
    }

    #[tokio::test]
    async fn set_writes_the_fields_it_was_given() {
        let (_d, p) = tmp_cfg("");
        handle_websearch_request(
            &p,
            "websearch/set",
            &json!({"enabled": false, "engines": {"bocha": {"count": 9, "freshness": "oneDay"}}}),
        )
        .await
        .unwrap();
        let out = handle_websearch_request(&p, "websearch/get", &json!({}))
            .await
            .unwrap();
        assert_eq!(out["enabled"], false);
        assert_eq!(out["engines"]["bocha"]["count"], 9);
        assert_eq!(out["engines"]["bocha"]["freshness"], "oneDay");
    }

    #[tokio::test]
    async fn set_ignores_a_masked_key_instead_of_storing_it() {
        let (_d, p) = tmp_cfg(r#"{"engines":{"bocha":{"api_key":"sk-real-9999"}}}"#);
        handle_websearch_request(
            &p,
            "websearch/set",
            &json!({"engines": {"bocha": {"api_key": "••••9999"}}}),
        )
        .await
        .unwrap();
        let out = handle_websearch_request(&p, "websearch/get", &json!({}))
            .await
            .unwrap();
        assert_eq!(out["engines"]["bocha"]["api_key_masked"], "••••9999");
        // 磁盘上必须还是原来那把真 key，而不是被掩码覆盖。
        let saved = std::fs::read_to_string(&p).unwrap();
        assert!(
            saved.contains("sk-real-9999"),
            "masked value overwrote the key: {saved}"
        );
    }

    #[tokio::test]
    async fn set_preserves_keys_it_does_not_own() {
        let (_d, p) = tmp_cfg(r#"{"future_key":"keep-me","engines":{"bocha":{}}}"#);
        handle_websearch_request(&p, "websearch/set", &json!({"enabled": false}))
            .await
            .unwrap();
        let saved = std::fs::read_to_string(&p).unwrap();
        assert!(saved.contains("keep-me"), "unrelated key dropped: {saved}");
    }

    /// 测试连接走真实网络，这里只验「无 key 时立刻拒绝，不发注定 401 的请求」
    /// （与 `model/test` 的 `resolve_test_settings` 同语义）。
    #[tokio::test]
    async fn test_refuses_when_no_key_is_available() {
        let (_d, p) = tmp_cfg(r#"{"engines":{"bocha":{"api_key":""}}}"#);
        // edition 2024 起 `remove_var` 是 unsafe；本模块只有这一个测试碰它。
        unsafe { std::env::remove_var("BOCHA_API_KEY") };
        let err = handle_websearch_request(&p, "websearch/test", &json!({}))
            .await
            .unwrap_err();
        assert!(err.message.contains("no api key"), "got: {}", err.message);
    }

    #[tokio::test]
    async fn unknown_methods_are_method_not_found() {
        let (_d, p) = tmp_cfg("");
        let err = handle_websearch_request(&p, "websearch/nope", &json!({}))
            .await
            .unwrap_err();
        assert_eq!(err.code, -32601);
    }
}
