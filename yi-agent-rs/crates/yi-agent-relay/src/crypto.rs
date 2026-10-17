//! 进程级 rustls crypto provider 的显式安装。
//!
//! 为什么必须显式:`rustls` 0.23 只在「**恰好一个** provider feature 被启用」时才肯
//! 从 crate features 推断进程级默认 provider([`from_crate_features`])。桌面
//! app-server 进程里两套 provider 并存:
//!
//! - `ring` — 自有 `reqwest 0.12`(`yi-agent-llm`/`yi-agent-tools` 的 `rustls-tls`);
//! - `aws_lc_rs` — `rmcp 3.5` 经 `reqwest 0.13`(`transport-streamable-http-client-reqwest`)。
//!
//! 于是 `ClientConfig::builder()`(tokio-tungstenite 建 `wss://` 时唯一隐式取 provider
//! 的地方)会走进 `get_default_or_install_from_crate_features()` 的 `expect` 分支并 panic。
//! 该 panic 发生在被 `spawn` 的桥接任务里,被 tokio 运行时吞成任务结束:表现为「手机连
//! 不上中继,而电脑 app 一切正常」。
//!
//! [`install_default`] 是 rustls 官方给出的消歧手段(其 panic 文案亦如此建议)。它在
//! **本进程内**一次性把选定 provider 写进 rustls 的全局 `OnceLock`,此后所有 rustls
//! 使用者(tokio-tungstenite、两个版本的 reqwest)都取到同一份,歧义不复存在。
//!
//! [`from_crate_features`]: rustls::crypto::CryptoProvider
//! [`install_default`]: rustls::crypto::CryptoProvider::install_default

use std::sync::Once;

static INSTALL: Once = Once::new();

/// 确保本进程已装好一个 rustls crypto provider;可重复调用,只生效一次。
///
/// 不覆盖「别人已装的 provider」:`install_default` 在已有默认值时返回 `Err`,这里
/// 有意忽略——先装者胜,与 rustls 的「首个 install 生效」语义一致。
pub fn ensure_default_provider() {
    INSTALL.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}
