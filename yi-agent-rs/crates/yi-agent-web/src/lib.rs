//! yi-agent WebUI：通过 Web 页面管理环境变量配置。

use std::path::PathBuf;

use anyhow::Result;

/// 启动 Web 配置服务器。
pub async fn serve(host: &str, port: u16, _env_path: PathBuf) -> Result<()> {
    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let app = axum::Router::new();
    axum::serve(listener, app).await?;
    Ok(())
}
