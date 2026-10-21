//! 会话存活观测：只读旁路，**永不**写回卡片状态。
//!
//! 为什么另立一个文件而不是往 `board.json` 加字段：`board.json` 是控制面渲染的
//! 事实源，而「会话此刻还在不在」是宿主内存里的一时之见。把它写进卡片就等于让
//! 观测反向污染状态——这正是本次改造要根除的误报来源。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

use superpowers_kanban_core::card::{Card, CardState};

/// 一个会话的最近一次观测。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub alive: bool,
    #[serde(default)]
    pub last_seen_alive_at: Option<DateTime<Local>>,
}

/// 整份观测文件。
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ObservationFile {
    #[serde(default)]
    pub threads: BTreeMap<String, Observation>,
}

impl ObservationFile {
    /// 记一次观测。只有「看到活着」才刷新时间戳：会话没了之后，人要看的是
    /// 「它最后一次还活着是什么时候」。
    pub fn observe(&mut self, thread_id: &str, alive: bool, now: DateTime<Local>) {
        let entry = self
            .threads
            .entry(thread_id.to_string())
            .or_insert(Observation {
                alive,
                last_seen_alive_at: None,
            });
        entry.alive = alive;
        if alive {
            entry.last_seen_alive_at = Some(now);
        }
    }

    /// 这个会话现在是否活着。没观测过一律按「不活」算——观测文件是唯一判据，
    /// 缺记录不是「活着」的证据。
    pub fn is_alive(&self, thread_id: &str) -> bool {
        self.threads
            .get(thread_id)
            .is_some_and(|observation| observation.alive)
    }
}

pub fn path(state_dir: &Path) -> PathBuf {
    state_dir.join("observations.json")
}

pub fn load(state_dir: &Path) -> ObservationFile {
    std::fs::read_to_string(path(state_dir))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn save(state_dir: &Path, file: &ObservationFile) -> std::io::Result<()> {
    std::fs::create_dir_all(state_dir)?;
    let text = serde_json::to_string_pretty(file).map_err(std::io::Error::other)?;
    let target = path(state_dir);
    let tmp = target.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, target)
}

/// 一张卡是否「写着 running 但会话已经没了」。只有 `running` 卡会 stale：
/// 其余状态没有活会话是正常的，标 stale 只会制造噪声。
pub fn stale<'a>(card: &Card, file: &'a ObservationFile) -> Option<&'a Observation> {
    if card.state != CardState::Running {
        return None;
    }
    let thread_id = card.thread_id.as_deref().filter(|id| !id.is_empty())?;
    let observation = file.threads.get(thread_id)?;
    (!observation.alive).then_some(observation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use superpowers_kanban_core::board::Board;
    use superpowers_kanban_core::card::CardId;

    fn at(day: u32, hour: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 10, day, hour, 0, 0)
            .single()
            .expect("valid local time")
    }

    #[test]
    fn a_thread_seen_alive_gets_its_timestamp_refreshed() {
        let mut file = ObservationFile::default();
        file.observe("thread-1", true, at(1, 0));
        assert_eq!(file.threads["thread-1"].last_seen_alive_at, Some(at(1, 0)));
        file.observe("thread-1", false, at(1, 1));
        assert!(!file.threads["thread-1"].alive);
        assert_eq!(
            file.threads["thread-1"].last_seen_alive_at,
            Some(at(1, 0)),
            "a dead sighting must not overwrite the last time it was alive"
        );
    }

    #[test]
    fn a_running_card_whose_session_is_dead_is_stale() {
        let mut board = Board::new();
        board.enqueue(
            CardId::new("a"),
            "a.spec.md".into(),
            "a.plan.md".into(),
            at(1, 0),
        );
        board
            .transition(&CardId::new("a"), CardState::Running)
            .unwrap();
        board
            .set_thread_id(&CardId::new("a"), "thread-1".into())
            .unwrap();
        let mut file = ObservationFile::default();
        file.observe("thread-1", false, at(1, 1));
        let card = board.get(&CardId::new("a")).unwrap();
        assert!(stale(card, &file).is_some());
        // 非 running 卡永不 stale：done 的卡没有会话是正常的。
        board
            .transition(&CardId::new("a"), CardState::Done)
            .unwrap();
        let card = board.get(&CardId::new("a")).unwrap();
        assert!(stale(card, &file).is_none());
    }

    #[test]
    fn a_running_card_without_an_observation_is_not_stale() {
        // 还没观测过 ≠ 已经死了。缺记录不构成判据（宁可不报警）。
        let mut board = Board::new();
        board.enqueue(
            CardId::new("a"),
            "a.spec.md".into(),
            "a.plan.md".into(),
            at(1, 0),
        );
        board
            .transition(&CardId::new("a"), CardState::Running)
            .unwrap();
        board
            .set_thread_id(&CardId::new("a"), "thread-1".into())
            .unwrap();
        let card = board.get(&CardId::new("a")).unwrap();
        assert!(stale(card, &ObservationFile::default()).is_none());
    }

    #[test]
    fn a_missing_or_corrupt_file_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).threads.is_empty());
        std::fs::write(path(dir.path()), "{not json").unwrap();
        assert!(load(dir.path()).threads.is_empty());
    }

    #[test]
    fn a_saved_file_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut file = ObservationFile::default();
        file.observe("thread-1", true, at(1, 0));
        save(dir.path(), &file).unwrap();
        let back = load(dir.path());
        assert!(back.is_alive("thread-1"));
        assert_eq!(back.threads["thread-1"].last_seen_alive_at, Some(at(1, 0)));
        assert!(!path(dir.path()).with_extension("json.tmp").exists());
    }
}
