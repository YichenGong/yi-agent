//! 中继服务端入口:在 `--listen`(默认 `127.0.0.1:8080`)上跑中继。
//!
//! - 电脑侧:wss 出站连 `/connect?session=<id>`
//! - 手机侧:wss 出站连 `/ws?session=<id>`
//!
//! 见 [`yi_agent_relay::server`] 了解路由与配对语义。

use std::sync::Arc;

use anyhow::Result;
use yi_agent_relay::Relay;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "yi_agent_relay=info".into()),
        )
        .init();

    let raw = std::env::args()
        .skip_while(|a| a != "--listen")
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:8080".to_string());
    let addr = yi_agent_relay::server::parse_listen(&raw)?;

    let listener = tokio::net::TcpListener::bind(addr).await?;
    yi_agent_relay::server::serve(listener, Arc::new(Relay::new())).await
}
