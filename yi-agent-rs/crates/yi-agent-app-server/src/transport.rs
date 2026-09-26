//! stdio JSONL 传输:一行一个 JSON 对象。

use anyhow::{Result, anyhow};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use crate::protocol::MAX_FRAME_BYTES;

/// 按行读取 JSONL 消息,一帧 = 一行。
pub struct MessageReader<R> {
    inner: BufReader<R>,
}

impl<R> MessageReader<R>
where
    R: tokio::io::AsyncRead + Unpin,
{
    pub fn new(r: R) -> Self {
        Self {
            inner: BufReader::new(r),
        }
    }

    /// 读取下一行;返回 `Ok(None)` 表示 EOF。
    ///
    /// 超出 [`MAX_FRAME_BYTES`] 的帧返回错误,避免被超大消息打爆内存。
    pub async fn next_line(&mut self) -> Result<Option<String>> {
        let mut buf = String::new();
        let n = self.inner.read_line(&mut buf).await?;
        if n == 0 {
            return Ok(None);
        }
        if buf.len() > MAX_FRAME_BYTES {
            return Err(anyhow!("frame exceeds max size of {MAX_FRAME_BYTES} bytes"));
        }
        let line = buf.strip_suffix('\n').unwrap_or(&buf);
        let line = line.strip_suffix('\r').unwrap_or(line);
        Ok(Some(line.to_string()))
    }
}

/// 按行写出 JSONL 消息;内部加锁,可通过 `&self` 共享。
pub struct MessageWriter<W> {
    inner: Mutex<W>,
}

impl<W> MessageWriter<W>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    pub fn new(w: W) -> Self {
        Self {
            inner: Mutex::new(w),
        }
    }

    /// 序列化并写出一个 JSON 值 + 换行。
    ///
    /// 序列化失败只记日志、不 panic,以免一条坏消息拖垮整个服务进程。
    pub async fn write_value(&self, v: &impl serde::Serialize) -> Result<()> {
        let line = match serde_json::to_string(v) {
            Ok(line) => line,
            Err(e) => {
                tracing::error!("failed to serialize message: {e}");
                return Ok(());
            }
        };
        let mut guard = self.inner.lock().await;
        guard.write_all(line.as_bytes()).await?;
        guard.write_all(b"\n").await?;
        guard.flush().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_one_message_per_line() {
        let input = b"{\"a\":1}\n{\"b\":2}\n";
        let mut reader = MessageReader::new(&input[..]);
        assert_eq!(
            reader.next_line().await.unwrap().as_deref(),
            Some("{\"a\":1}")
        );
        assert_eq!(
            reader.next_line().await.unwrap().as_deref(),
            Some("{\"b\":2}")
        );
        assert_eq!(reader.next_line().await.unwrap(), None);
    }

    #[tokio::test]
    async fn strips_crlf_line_endings() {
        let input = b"{\"a\":1}\r\n";
        let mut reader = MessageReader::new(&input[..]);
        assert_eq!(
            reader.next_line().await.unwrap().as_deref(),
            Some("{\"a\":1}")
        );
    }

    #[tokio::test]
    async fn eof_without_trailing_newline_yields_last_line() {
        let input = b"{\"a\":1}";
        let mut reader = MessageReader::new(&input[..]);
        assert_eq!(
            reader.next_line().await.unwrap().as_deref(),
            Some("{\"a\":1}")
        );
        assert_eq!(reader.next_line().await.unwrap(), None);
    }

    #[tokio::test]
    async fn rejects_oversized_frame() {
        let big = vec![b'x'; MAX_FRAME_BYTES + 10];
        let mut reader = MessageReader::new(&big[..]);
        assert!(reader.next_line().await.is_err());
    }

    #[tokio::test]
    async fn writer_emits_one_json_line_per_value() {
        let mut buf = Vec::new();
        {
            let writer = MessageWriter::new(&mut buf);
            writer
                .write_value(&serde_json::json!({"a": 1}))
                .await
                .unwrap();
            writer
                .write_value(&serde_json::json!({"b": 2}))
                .await
                .unwrap();
        }
        assert_eq!(String::from_utf8(buf).unwrap(), "{\"a\":1}\n{\"b\":2}\n");
    }
}
