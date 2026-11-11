//! stdin 行读取与 stdout 泵。

use crate::transport::{MessageReader, MessageWriter};
use std::sync::Arc;
use tokio::sync::mpsc;

/// 从一条 `AsyncRead` 逐行读取并送入主循环。
///
/// `MessageReader::next_line` 基于 `read_line`,不是 cancel-safe,因此由本任务
/// 独占 reader,主循环只 select cancel-safe 的 channel `recv`。
pub(crate) async fn read_lines<R>(
    reader: R,
    client: crate::broadcast::ClientId,
    tx: mpsc::Sender<(crate::broadcast::ClientId, anyhow::Result<String>)>,
    // 合并模式下,stdio 读端「结束」要在主循环里被 `select!` 看到。纯 stdio 传
    // `None`:此时 EOF 直接体现为入站 channel 关闭(`tx` 随本任务结束而 drop),
    // 与改造前一致。
    eof_tx: Option<tokio::sync::oneshot::Sender<()>>,
    // 本会话的出站管理通道(若有)。授权应答在**这里**截获,不在主循环:
    // 发起授权的会话正是在主循环任务上阻塞等待,由主循环去读自己的应答必然
    // 自己等自己。本任务独占 reader,是唯一能同时看到应答的地方。
    management: Option<std::sync::Arc<super::super::local_management::ManagementHandle>>,
    hub: Arc<crate::broadcast::Broadcaster>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut reader = MessageReader::new(reader);
    loop {
        let next = tokio::select! {
            biased;
            // Cancellation is safe only here: the reader is discarded on exit,
            // never reused after a partially consumed line. Once serve drops its
            // receiver, private frames must not start work for a closed session.
            _ = tx.closed() => break,
            next = reader.next_line() => next,
        };
        match next {
            Ok(Some(line)) => {
                if let Some(handle) = management.as_ref()
                    && let Some(outbound) = handle.outbound()
                    && deliver_management_response(&outbound, &line)
                {
                    continue;
                }
                // A resume can wait for ClientLeased. The shell cannot register
                // that lease until prepare replies, so management REQUESTS must
                // bypass the business loop just like authorization responses do.
                if client == crate::broadcast::ClientId::local()
                    && let Some(handle) = management.as_ref()
                    && let Ok(value) = serde_json::from_str::<serde_json::Value>(&line)
                {
                    match super::handle_local_management(&value, handle, &hub, &client).await {
                        Ok(true) => continue,
                        Ok(false) => {}
                        Err(error) => {
                            tracing::error!(%error, "local management frame failed");
                            break;
                        }
                    }
                    // Check at ingress too: a later bootstrap must not admit a
                    // business frame that arrived before authentication.
                    if handle.session().lock().await.admit_business()
                        != super::super::local_management::Admission::Accepted
                    {
                        let refused = super::super::local_management::refusal_for(
                            value.get("id"),
                            "the local session has not completed its bootstrap",
                        );
                        if let Ok(frame) = serde_json::to_value(refused) {
                            hub.reply(&client, frame).await.ok();
                        }
                        continue;
                    }
                }
                if tx.send((client.clone(), Ok(line))).await.is_err() {
                    break;
                }
            }
            Ok(None) => break, // EOF → 丢弃 tx → 主循环优雅退出
            Err(e) => {
                tracing::error!("app-server read error: {e}");
                let _ = tx.send((client.clone(), Err(e))).await;
                break;
            }
        }
    }
    // 通知主循环「stdio 读端结束了」。纯 stdio 下 `eof_tx` 为 `None`,no-op。
    if let Some(eof_tx) = eof_tx {
        let _ = eof_tx.send(());
    }
}
/// Consumes one line if it is a management response. Returns whether it was.
///
/// A frame that parses but is not a management response is left for the business loop.
/// A frame that *is* one is never forwarded, even when nobody is waiting for it: it
/// carries a capability, and the business path would hand it to the WebView.
fn deliver_management_response(
    management: &super::super::management_client::ManagementClient,
    line: &str,
) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    management.accept_response(&value)
}

/// 把该客户端的出站帧写到一条 `AsyncWrite`(stdio 场景即 stdout)。
///
/// 写失败(对端已消失)时把该客户端从 hub 摘除:主循环之后经 `hub.reply` 定向写
/// 响应会拿到 `Closed`,按**改造前**的语义以错误退出——`write_response(..)?` 曾把
/// 写失败直接变成会话错误,这条路径必须保留。
///
/// **不持有入站 channel 的 sender**:否则读端 EOF 后 channel 仍有一只存活 sender,
/// 主循环的 `recv` 永不返回 `None`,优雅退出被拖住。入站的存活只由 `read_lines`
/// 决定(它 EOF 即丢弃自己的 sender)。
pub(crate) async fn pump_stdout<W>(
    mut outbound: mpsc::Receiver<serde_json::Value>,
    writer: W,
    hub: Arc<crate::broadcast::Broadcaster>,
    client: crate::broadcast::ClientId,
) where
    W: tokio::io::AsyncWrite + Unpin,
{
    let writer = MessageWriter::new(writer);
    while let Some(frame) = outbound.recv().await {
        if let Err(e) = writer.write_value(&frame).await {
            tracing::error!("app-server stdout write failed: {e}");
            // 出口泵写失败 = 对端真的没了(stdout 管道断裂)。这是 stdio 会话的
            // **唯一**断连判据:可靠登记不会被背压摘除,故此处必须显式摘除,
            // 让主循环随后的 `reply`/`write_response` 拿到 `Closed` 并按既有
            // 致命语义结束会话(`driver_reports_finished_when_writer_fails`)。
            hub.unregister(&client);
            break;
        }
    }
}
