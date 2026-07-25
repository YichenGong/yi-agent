//! Running task registry: tracks bash tool calls for status bar + popup.

use std::time::Instant;
use yi_agent_core::OutputStream;

/// Per-stream cap for retained output (last 64KB).
const MAX_STREAM_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Running,
    Done,
    Failed,
    Timeout,
}

#[derive(Debug, Clone)]
pub struct TaskState {
    pub id: String,
    pub tool_name: String,
    pub command: String,
    pub start_time: Instant,
    pub end_time: Option<Instant>,
    /// None = still running; Some(Some(code)) = exited; Some(None) = killed.
    pub exit_code: Option<Option<i32>>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: TaskStatus,
    pub expected_timeout_sec: u32,
}

impl TaskState {
    pub fn elapsed(&self) -> std::time::Duration {
        match self.end_time {
            Some(end) => end.duration_since(self.start_time),
            None => self.start_time.elapsed(),
        }
    }

    /// True if the task is running and has exceeded its expected timeout.
    pub fn exceeds_expected(&self) -> bool {
        self.status == TaskStatus::Running
            && self.elapsed().as_secs() > self.expected_timeout_sec as u64
    }
}

pub struct RunningTaskRegistry {
    tasks: std::collections::HashMap<String, TaskState>,
    order: Vec<String>,
}

impl RunningTaskRegistry {
    pub fn new() -> Self {
        Self {
            tasks: Default::default(),
            order: Vec::new(),
        }
    }

    pub fn on_tool_call(
        &mut self,
        id: &str,
        tool_name: &str,
        command: &str,
        expected_timeout_sec: u32,
    ) {
        let state = TaskState {
            id: id.into(),
            tool_name: tool_name.into(),
            command: command.into(),
            start_time: Instant::now(),
            end_time: None,
            exit_code: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            status: TaskStatus::Running,
            expected_timeout_sec,
        };
        if self.tasks.insert(id.into(), state).is_none() {
            self.order.push(id.into());
        }
    }

    pub fn on_output_delta(&mut self, id: &str, stream: OutputStream, text: &str) {
        if let Some(t) = self.tasks.get_mut(id) {
            let buf = match stream {
                OutputStream::Stdout => &mut t.stdout,
                OutputStream::Stderr => &mut t.stderr,
            };
            buf.extend_from_slice(text.as_bytes());
            if buf.len() > MAX_STREAM_BYTES {
                let cut = buf.len() - MAX_STREAM_BYTES;
                buf.drain(..cut);
            }
        }
    }

    pub fn on_exit(&mut self, id: &str, code: Option<i32>) {
        if let Some(t) = self.tasks.get_mut(id) {
            t.end_time = Some(Instant::now());
            t.exit_code = Some(code);
            t.status = match code {
                Some(0) => TaskStatus::Done,
                Some(_) => TaskStatus::Failed,
                None => TaskStatus::Timeout,
            };
        }
    }

    pub fn on_timeout(&mut self, id: &str) {
        if let Some(t) = self.tasks.get_mut(id) {
            t.end_time = Some(Instant::now());
            t.exit_code = None;
            t.status = TaskStatus::Timeout;
        }
    }

    #[allow(dead_code)]
    pub fn running_count(&self) -> usize {
        self.tasks
            .values()
            .filter(|t| t.status == TaskStatus::Running)
            .count()
    }

    pub fn get(&self, id: &str) -> Option<&TaskState> {
        self.tasks.get(id)
    }

    /// List all tasks, newest first (insertion order reversed).
    pub fn list(&self) -> Vec<&TaskState> {
        self.order
            .iter()
            .rev()
            .filter_map(|id| self.tasks.get(id))
            .collect()
    }
}

impl Default for RunningTaskRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_lifecycle() {
        let mut r = RunningTaskRegistry::new();
        r.on_tool_call("t1", "bash", "ls -la", 120);
        assert_eq!(r.running_count(), 1);
        r.on_output_delta("t1", OutputStream::Stdout, "hello\n");
        r.on_output_delta("t1", OutputStream::Stderr, "warn\n");
        let state = r.get("t1").unwrap();
        assert!(state.stdout.windows(5).any(|w| w == b"hello"));
        assert!(state.stderr.windows(4).any(|w| w == b"warn"));
        r.on_exit("t1", Some(0));
        assert_eq!(r.running_count(), 0);
        assert_eq!(r.get("t1").unwrap().status, TaskStatus::Done);
    }

    #[test]
    fn test_registry_truncation() {
        let mut r = RunningTaskRegistry::new();
        r.on_tool_call("t1", "bash", "cat big", 120);
        let big = "x".repeat(100 * 1024);
        r.on_output_delta("t1", OutputStream::Stdout, &big);
        let state = r.get("t1").unwrap();
        assert!(state.stdout.len() <= MAX_STREAM_BYTES + 1024);
    }

    #[test]
    fn test_registry_listing_order() {
        let mut r = RunningTaskRegistry::new();
        r.on_tool_call("a", "bash", "cmd_a", 120);
        std::thread::sleep(std::time::Duration::from_millis(10));
        r.on_tool_call("b", "bash", "cmd_b", 120);
        let list = r.list();
        assert_eq!(list[0].id, "b"); // newest first
        assert_eq!(list[1].id, "a");
    }

    #[test]
    fn test_registry_failed_and_timeout_status() {
        let mut r = RunningTaskRegistry::new();
        r.on_tool_call("f", "bash", "exit 1", 120);
        r.on_exit("f", Some(1));
        assert_eq!(r.get("f").unwrap().status, TaskStatus::Failed);

        r.on_tool_call("t", "bash", "sleep 10", 1);
        r.on_timeout("t");
        assert_eq!(r.get("t").unwrap().status, TaskStatus::Timeout);
        assert_eq!(r.get("t").unwrap().exit_code, None);
    }

    #[test]
    fn test_registry_exceeds_expected() {
        let mut r = RunningTaskRegistry::new();
        r.on_tool_call("t", "bash", "sleep 5", 0);
        // expected=0, any elapsed > 0s counts as exceeded
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert!(r.get("t").unwrap().exceeds_expected());
        r.on_exit("t", Some(0));
        // Once done, exceeds_expected is false (status != Running)
        assert!(!r.get("t").unwrap().exceeds_expected());
    }

    #[test]
    fn test_registry_duplicate_id_does_not_duplicate_order() {
        let mut r = RunningTaskRegistry::new();
        r.on_tool_call("t1", "bash", "a", 120);
        r.on_tool_call("t1", "bash", "b", 120); // same id, second call overwrites
        let list = r.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].command, "b");
    }
}
