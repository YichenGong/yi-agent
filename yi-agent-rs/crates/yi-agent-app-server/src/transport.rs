//! stdio JSONL 传输:一行一个 JSON 对象。

use anyhow::{Result, anyhow};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
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
    /// 帧大小语义:一帧的**内容**(不含行终止符 `\n` / `\r\n`)必须
    /// ≤ [`MAX_FRAME_BYTES`],超限返回错误。读取时最多缓冲
    /// `MAX_FRAME_BYTES + 1` 字节,因此超大行不会打爆内存。
    pub async fn next_line(&mut self) -> Result<Option<String>> {
        let mut buf = String::new();
        let limit = (MAX_FRAME_BYTES + 1) as u64;
        let n = {
            let mut limited = (&mut self.inner).take(limit);
            limited.read_line(&mut buf).await?
        };
        if n == 0 {
            return Ok(None);
        }
        let content = buf.strip_suffix('\n').unwrap_or(&buf);
        let content = content.strip_suffix('\r').unwrap_or(content);
        if content.len() > MAX_FRAME_BYTES {
            return Err(anyhow!("frame exceeds max size of {MAX_FRAME_BYTES} bytes"));
        }
        Ok(Some(content.to_string()))
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
    /// 序列化失败返回错误(不 panic、不静默丢弃),调用方可据此判断该帧
    /// 是否已发出。
    pub async fn write_value(&self, v: &impl serde::Serialize) -> Result<()> {
        let line =
            serde_json::to_string(v).map_err(|e| anyhow!("failed to serialize message: {e}"))?;
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
    async fn frame_size_bound_excludes_line_terminator() {
        // 恰好 MAX_FRAME_BYTES 字节的内容(不含 `\n`)应被接受。
        let mut exact = vec![b'x'; MAX_FRAME_BYTES];
        exact.push(b'\n');
        let mut reader = MessageReader::new(&exact[..]);
        let line = reader.next_line().await.unwrap().unwrap();
        assert_eq!(line.len(), MAX_FRAME_BYTES);

        // 内容为 MAX_FRAME_BYTES + 1 字节应被拒绝。
        let mut over = vec![b'x'; MAX_FRAME_BYTES + 1];
        over.push(b'\n');
        let mut reader = MessageReader::new(&over[..]);
        assert!(reader.next_line().await.is_err());
    }

    struct Fails;

    impl serde::Serialize for Fails {
        fn serialize<S: serde::Serializer>(&self, _s: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("boom"))
        }
    }

    #[tokio::test]
    async fn write_value_returns_error_on_serialization_failure() {
        let mut buf = Vec::new();
        let writer = MessageWriter::new(&mut buf);
        assert!(writer.write_value(&Fails).await.is_err());
        assert!(buf.is_empty(), "no partial frame should be written");
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
