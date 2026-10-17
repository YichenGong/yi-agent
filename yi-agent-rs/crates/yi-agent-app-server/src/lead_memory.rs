//! 组长的私有决策账本 `memory.md` 的读写与检索。
//!
//! 只装**不可再生**的信息（决策与理由、长期偏好、跨 thread 约定）——可再生的
//! 一律现算，见 spec §7.3。行格式是有意为之的「一行一条 JSON」：条目里含换行
//! （模型很爱写多行）也不会破坏结构，且追加是纯 `O_APPEND`，无读-改-写竞态。

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 一条账本条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEntry {
    /// Unix 毫秒。写入时由 `lead_remember` 铸造，不由模型提供。
    pub timestamp_ms: i64,
    /// 来源（相关 thread id / turn id），使「这条是谁、何时、因为什么写的」可审计。
    pub source: String,
    pub text: String,
}

/// `<project_root>/.yi-agent/lead_memory/memory.md`。
pub fn memory_path(project_root: &Path) -> PathBuf {
    project_root
        .join(".yi-agent")
        .join("lead_memory")
        .join("memory.md")
}

/// 追加一条。目录不存在则创建。只 `append`，不做读-改-写。
pub fn append_entry(project_root: &Path, entry: &MemoryEntry) -> std::io::Result<()> {
    let path = memory_path(project_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut line = serde_json::to_string(entry)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    line.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    file.write_all(line.as_bytes())
}

/// 读出全部条目。文件不存在视为空；无法解析的行跳过（账本损坏不应让 lead 崩）。
pub fn read_entries(project_root: &Path) -> std::io::Result<Vec<MemoryEntry>> {
    let path = memory_path(project_root);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    Ok(text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<MemoryEntry>(l).ok())
        .collect())
}

/// 朴素子串检索（ASCII 不区分大小写），按时间升序返回至多 `limit` 条。
///
/// 不做索引、不做分词：`memory.md` 本期无界增长是已知限制（spec §7.6），
/// 现在给它加索引是过早优化。
pub fn search(project_root: &Path, query: &str, limit: usize) -> std::io::Result<Vec<MemoryEntry>> {
    let needle = query.to_lowercase();
    if needle.is_empty() {
        return Ok(Vec::new());
    }
    let mut hits: Vec<MemoryEntry> = read_entries(project_root)?
        .into_iter()
        .filter(|e| e.text.to_lowercase().contains(&needle))
        .collect();
    hits.sort_by_key(|e| e.timestamp_ms);
    hits.truncate(limit);
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_project() -> tempfile::TempDir {
        tempfile::TempDir::new().expect("tempdir")
    }

    #[test]
    fn memory_path_is_under_the_project_state_dir() {
        let dir = temp_project();
        let path = memory_path(dir.path());
        assert_eq!(
            path,
            dir.path()
                .join(".yi-agent")
                .join("lead_memory")
                .join("memory.md")
        );
    }

    #[test]
    fn appending_then_reading_round_trips_entries_in_order() {
        let dir = temp_project();
        let first = MemoryEntry {
            timestamp_ms: 1_000,
            source: "thread-a".into(),
            text: "为什么打回 thread A：偏离了验收标准".into(),
        };
        let second = MemoryEntry {
            timestamp_ms: 2_000,
            source: "thread-b".into(),
            text: "用户长期偏好：提交前必须跑 fmt".into(),
        };
        append_entry(dir.path(), &first).expect("append first");
        append_entry(dir.path(), &second).expect("append second");

        let loaded = read_entries(dir.path()).expect("read");
        assert_eq!(loaded, vec![first, second]);
    }

    #[test]
    fn reading_a_project_without_memory_yields_no_entries() {
        let dir = temp_project();
        assert!(read_entries(dir.path()).expect("read").is_empty());
    }

    #[test]
    fn search_matches_substrings_and_honours_the_limit() {
        let dir = temp_project();
        for (ms, text) in [(1, "打回 thread A"), (2, "打回 thread B"), (3, "无关内容")] {
            append_entry(
                dir.path(),
                &MemoryEntry {
                    timestamp_ms: ms,
                    source: "s".into(),
                    text: text.into(),
                },
            )
            .expect("append");
        }
        let hits = search(dir.path(), "打回", 10).expect("search");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].timestamp_ms, 1);
        assert_eq!(hits[1].timestamp_ms, 2);

        let capped = search(dir.path(), "打回", 1).expect("search");
        assert_eq!(capped.len(), 1);
    }

    #[test]
    fn search_is_case_insensitive_for_ascii() {
        let dir = temp_project();
        append_entry(
            dir.path(),
            &MemoryEntry {
                timestamp_ms: 1,
                source: "s".into(),
                text: "Prefer FMT".into(),
            },
        )
        .expect("append");
        assert_eq!(search(dir.path(), "fmt", 10).expect("search").len(), 1);
    }
}
