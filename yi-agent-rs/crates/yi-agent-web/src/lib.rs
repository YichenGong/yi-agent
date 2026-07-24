//! yi-agent WebUI：通过 Web 页面管理环境变量配置。

pub mod api;
pub mod config_meta;
pub mod env_file;

use std::path::{Path, PathBuf};

use anyhow::Result;
use axum::routing::get;

/// 启动 Web 配置服务器。
pub async fn serve(host: &str, port: u16, env_path: PathBuf) -> Result<()> {
    // 从 env_path 向上查找 .env.example
    let env_example_path = find_env_example(&env_path);
    let state = api::AppState {
        env_path,
        env_example_path,
    };
    let app = axum::Router::new()
        .route("/", get(api::index_html))
        .route("/api/config", get(api::get_config).put(api::put_config))
        .with_state(state);

    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// 从 .env 路径向上查找 .env.example（最多 5 层）。
fn find_env_example(env_path: &Path) -> Option<PathBuf> {
    let dir = env_path.parent()?;
    let mut current = dir;
    for _ in 0..5 {
        let candidate = current.join(".env.example");
        if candidate.exists() {
            return Some(candidate);
        }
        current = current.parent()?;
    }
    None
}
