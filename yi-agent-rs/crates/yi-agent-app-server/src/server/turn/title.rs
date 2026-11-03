//! 标题生成：首轮输入后为新 thread 起标题。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 6）。

use std::sync::Arc;

// 这些项仍由 `server.rs`（或其子模块经其 re-export）提供。
use crate::protocol::Notification;
use crate::server::{TITLE_TIMEOUT, write_notification};

/// 用户首次输入落地时立刻命名的入口。
///
/// 旧的实现把标题生成放在 `persist_and_finish_turn`：用户要等整轮跑完（可能数十
/// 秒）才在侧栏看到有意义的标题。这里把它前移到 turn 开始处，材料也从「提问 +
/// 助手回复」收敛成「只取用户输入」——此刻助手还没有任何输出。
///
/// 触发条件是 `ThreadStore::touch` 返回 `true`（meta 的 title 由 `None` 变成兜底
/// 截断值）。组长会话（`is_lead_meta`）被 `touch` 直接跳过；插件驱动会话创建时
/// 标题已非空，因此天然不触发；手动改名之后 title 也非空，同样不会重新生成——
/// 一旦标题非空，`touch` 永不再返回 `true`，这条路径对同一会话只会有一次。
///
/// 材料只有用户输入（`prompt`，即 `display_text` 原话，不含附件清单）。因此
/// 纯图片 / 纯附件消息（`display_text` 为空）没有可用材料，不会生成标题，
/// 会话保留兜底或空标题。
///
/// **不得阻塞 driver**：调用方负责 `tokio::spawn`，本函数只做生成 → 清洗 → CAS
/// → 广播，任何失败都静默保留兜底标题（标题是锦上添花，绝不能影响 turn）。
pub(crate) async fn generate_title_for_first_input(
    thread_id: String,
    prompt: String,
    fallback: String,
    provider: Arc<dyn yi_agent_core::Provider>,
    config: yi_agent_core::AgentConfig,
    store: Arc<crate::thread_store::ThreadStore>,
    hub: Arc<crate::broadcast::Broadcaster>,
) {
    // 材料只有用户输入：`None` 让 `build_title_material` 省略「助手回复：」段。
    let generated = tokio::time::timeout(
        TITLE_TIMEOUT,
        yi_agent_core::title::generate_title(&provider, &config, &prompt, None),
    )
    .await;
    match generated {
        Ok(Ok(Some(title))) => {
            // 仅在 CAS **确实替换**（`Ok(true)`）后广播：`Ok(false)` 表示标题已被
            // 手动改名抢占或 thread 不存在，磁盘上不是这个值，广播它会让客户端
            // 显示一个不存在的标题。
            match store.set_title_if_unchanged(&thread_id, &fallback, &title) {
                Ok(true) => {
                    if let Err(e) = write_notification(
                        &hub,
                        &Notification::TitleChanged {
                            thread_id: thread_id.clone(),
                            title,
                        },
                    )
                    .await
                    {
                        eprintln!("[app-server] failed to broadcast title change {thread_id}: {e}");
                    }
                }
                Ok(false) => {}
                Err(e) => eprintln!("[app-server] failed to set LLM title {thread_id}: {e}"),
            }
        }
        // 模型产出无法作为标题：保留兜底标题。
        //
        // 这条路径过去完全静默，导致「LLM 标题不常见」查了很久：推理模型把
        // `max_tokens` 烧在思考上、正文留空，就落在这里。留一条 info，
        // 下次同类症状能直接从日志看出来。
        Ok(Ok(None)) => {
            tracing::info!(
                %thread_id,
                "title generation produced no usable title; keeping fallback"
            );
        }
        Ok(Err(e)) => {
            tracing::info!(%e, %thread_id, "title generation failed; keeping fallback");
        }
        Err(_) => {
            tracing::info!(%thread_id, "title generation timed out; keeping fallback");
        }
    }
}
