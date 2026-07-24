//! yi-agent WebUI：通过 Web 页面管理环境变量配置。

pub mod api;
pub mod config_meta;
pub mod env_file;

use std::path::PathBuf;

use anyhow::Result;
use axum::routing::get;

/// 启动 Web 配置服务器。
pub async fn serve(host: &str, port: u16, env_path: PathBuf) -> Result<()> {
    let state = api::AppState { env_path };
    let app = axum::Router::new()
        .route("/", get(api::index_html))
        .route("/api/config", get(api::get_config).put(api::put_config))
        .with_state(state);

    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
