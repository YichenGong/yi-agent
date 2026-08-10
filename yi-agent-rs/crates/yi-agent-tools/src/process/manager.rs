use serde::{Deserialize, Serialize};

#[allow(dead_code)]
pub const DEFAULT_STREAM_CAP_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnExitPolicy {
    Kill,
    Keep,
}

impl Default for OnExitPolicy {
    fn default() -> Self {
        Self::Kill
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ProcessStatus {
    Starting,
    Running,
    Ready,
    Exited { code: Option<i32> },
    Killed,
    FailedToStart { reason: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct ManagedProcessSnapshot {
    pub process_id: String,
    pub name: Option<String>,
    pub pid: Option<u32>,
    pub command: String,
    pub cwd: String,
    pub status: ProcessStatus,
    pub ready: bool,
    pub on_exit: OnExitPolicy,
    pub exit_code: Option<i32>,
    pub elapsed_sec: f32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessReadResult {
    pub process_id: String,
    pub name: Option<String>,
    pub stdout: String,
    pub stderr: String,
    pub next_cursor: u64,
    pub truncated: bool,
    pub status: ProcessStatus,
    pub ready: bool,
}

#[derive(Debug, Clone)]
struct StreamRingBuffer {
    cap: usize,
    chunks: Vec<(u64, Vec<u8>)>,
    next_cursor: u64,
}

impl StreamRingBuffer {
    fn new(cap: usize) -> Self {
        Self {
            cap,
            chunks: Vec::new(),
            next_cursor: 0,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let start = self.next_cursor;
        self.next_cursor = self.next_cursor.saturating_add(bytes.len() as u64);
        self.chunks.push((start, bytes.to_vec()));
        self.trim_to_cap();
    }

    fn next_cursor(&self) -> u64 {
        self.next_cursor
    }

    fn earliest_cursor(&self) -> u64 {
        self.chunks
            .first()
            .map(|(start, _)| *start)
            .unwrap_or(self.next_cursor)
    }

    fn read(&self, cursor: Option<u64>, max_bytes: usize) -> (String, u64, bool) {
        let earliest = self.earliest_cursor();
        let requested = cursor.unwrap_or(earliest);
        let truncated = requested < earliest;
        let effective_cursor = requested.max(earliest);

        let mut out = Vec::new();
        for (start, bytes) in &self.chunks {
            let end = start.saturating_add(bytes.len() as u64);
            if end <= effective_cursor {
                continue;
            }
            let offset = effective_cursor.saturating_sub(*start) as usize;
            if offset < bytes.len() {
                out.extend_from_slice(&bytes[offset..]);
            }
        }

        if out.len() > max_bytes {
            let keep_from = out.len() - max_bytes;
            out.drain(..keep_from);
        }

        (
            String::from_utf8_lossy(&out).into_owned(),
            self.next_cursor,
            truncated,
        )
    }

    fn trim_to_cap(&mut self) {
        let mut total: usize = self.chunks.iter().map(|(_, bytes)| bytes.len()).sum();
        while total > self.cap {
            let excess = total - self.cap;
            let Some((start, bytes)) = self.chunks.first_mut() else {
                break;
            };
            if excess >= bytes.len() {
                total -= bytes.len();
                self.chunks.remove(0);
            } else {
                bytes.drain(..excess);
                *start = start.saturating_add(excess as u64);
                total -= excess;
            }
        }
    }
}

pub struct ProcessManager;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_ring_buffer_snapshot_returns_recent_output_and_next_cursor() {
        let mut buf = StreamRingBuffer::new(32);
        buf.push(b"hello ");
        buf.push(b"world");

        let (text, next_cursor, truncated) = buf.read(None, 64);

        assert_eq!(text, "hello world");
        assert_eq!(next_cursor, 11);
        assert!(!truncated);
    }

    #[test]
    fn stream_ring_buffer_cursor_returns_only_new_output() {
        let mut buf = StreamRingBuffer::new(64);
        buf.push(b"first\n");
        let cursor = buf.next_cursor();
        buf.push(b"second\n");

        let (text, next_cursor, truncated) = buf.read(Some(cursor), 64);

        assert_eq!(text, "second\n");
        assert_eq!(next_cursor, 13);
        assert!(!truncated);
    }

    #[test]
    fn stream_ring_buffer_old_cursor_reports_truncation() {
        let mut buf = StreamRingBuffer::new(6);
        buf.push(b"abcdef");
        buf.push(b"ghij");

        let (text, next_cursor, truncated) = buf.read(Some(0), 64);

        assert_eq!(text, "efghij");
        assert_eq!(next_cursor, 10);
        assert!(truncated);
    }

    #[test]
    fn stream_ring_buffer_respects_max_bytes() {
        let mut buf = StreamRingBuffer::new(64);
        buf.push(b"abcdefghij");

        let (text, next_cursor, truncated) = buf.read(None, 4);

        assert_eq!(text, "ghij");
        assert_eq!(next_cursor, 10);
        assert!(!truncated);
    }
}
