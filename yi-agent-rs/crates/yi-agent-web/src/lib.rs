//! yi-agent WebUI：编辑 `<instance_home>/settings.json`。
//!
//! 这是一层 HTTP 外壳，**不含任何配置元数据与校验规则**：字段类型、默认值、
//! 取值范围、可选项都来自 `yi_agent_runtime::settings`，与桌面端同源。过去
//! 这里有一份手抄的 `config_meta::ALL_VARS` 和一套 `env_file` 读写，默认值与
//! `config.rs` 漂移过（`YI_AGENT_MAX_TURNS` 20 vs 200），写盘还会删掉不认识的键。

pub mod api;

use std::path::PathBuf;

use anyhow::Result;
use axum::routing::get;

/// 构造路由。抽出来让测试不必绑定端口。
pub fn router(settings_path: PathBuf) -> axum::Router {
    let state = api::AppState { settings_path };
    axum::Router::new()
        .route("/", get(api::index_html))
        .route("/api/config", get(api::get_config).put(api::put_config))
        .with_state(state)
}

/// 启动 Web 配置服务器。
pub async fn serve(host: &str, port: u16, settings_path: PathBuf) -> Result<()> {
    println!("yi-agent web 配置服务器已启动");
    println!("  地址:     http://{host}:{port}");
    println!("  配置文件: {}", settings_path.display());
    println!("按 Ctrl+C 停止");

    let app = router(settings_path);
    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
