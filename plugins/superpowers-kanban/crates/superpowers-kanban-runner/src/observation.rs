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

/// 一张 running 卡的执行会话，**当前宿主是否加载着它**。
///
/// 这三个态刻意不是「活着 / 死了」：`alive` 只说明宿主这一拍还看得见某个会话，
/// 而宿主看得见的只是它自己的热表。`Unknown` 是缺省也是降级态——查询没成功、
/// 旧格式文件、坏字段都落在这里，**绝不**冒充「已经不存在」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionObservation {
    /// 当前宿主热表里有这个会话（无论此刻闲不闲）。
    Loaded,
    /// 当前宿主**明确报告**热表里没有这个会话。历史可能仍然存在。
    NotLoaded,
    /// 没有有效证据：旧格式、查询失败、字段损坏，或这一拍压根没问。
    Unknown,
}

impl SessionObservation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Loaded => "loaded",
            Self::NotLoaded => "not_loaded",
            Self::Unknown => "unknown",
        }
    }
}

/// 把新字段里的字符串读成受控枚举：未知值一律 `None`，由调用方降级成
/// `Unknown`，而不是让整份文件反序列化失败。
fn recognized(value: Option<&str>) -> Option<SessionObservation> {
    match value {
        Some("loaded") => Some(SessionObservation::Loaded),
        Some("not_loaded") => Some(SessionObservation::NotLoaded),
        _ => None,
    }
}

/// 容错字符串反序列化：坏类型（数字、对象、数组）当作缺失。
///
/// 为什么不用 `Option<String>` 直接反序列化：那样一个坏字段就会让**整份**
/// 观测文件 `load` 失败并退回 `default`，把别的会话的有效记录一起抹掉。
fn optional_string<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let value = <serde_json::Value as serde::Deserialize>::deserialize(deserializer)?;
    Ok(value.as_str().map(str::to_owned))
}

/// 一个会话的最近一次观测。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub alive: bool,
    #[serde(default)]
    pub last_seen_alive_at: Option<DateTime<Local>>,
    /// 这一拍的展示态（`loaded` / `not_loaded`）。缺失 = 没有有效证据。
    #[serde(default, deserialize_with = "optional_string")]
    pub session_observation: Option<String>,
    /// 这一拍展示态对应的时间（RFC3339）。**不是**最后活动时间。
    #[serde(default, deserialize_with = "optional_string")]
    pub observed_at: Option<String>,
}

impl Observation {
    /// 这个会话相对**当前宿主**的加载态。
    ///
    /// 只有「合法的 `observed_at` **且**受控枚举值」才算证据：旧文件只有
    /// `alive`，它来自一份不完整的 driver 子集，不能当成加载结论。
    pub fn presentation(&self) -> SessionObservation {
        if self.presentation_at().is_none() {
            return SessionObservation::Unknown;
        }
        recognized(self.session_observation.as_deref()).unwrap_or(SessionObservation::Unknown)
    }

    /// 展示态对应的观测时间。
    ///
    /// 只有「受控枚举值 + 合法 RFC3339」才有时间：没有有效报告的那一拍没有
    /// 「截至」可言，不能借一个孤立的日期编出观测时刻。
    pub fn presentation_at(&self) -> Option<DateTime<Local>> {
        recognized(self.session_observation.as_deref())?;
        self.observed_at
            .as_deref()
            .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
            .map(|at| at.with_timezone(&Local))
    }
}

/// 整份观测文件。
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
                session_observation: None,
                observed_at: None,
            });
        entry.alive = alive;
        // 走到这里的调用方拿到的都是宿主**明确**给出的布尔值，所以它可以同时
        // 写展示态与观测时间（采集侧不经此函数，见 `driver_observation::normalize`）。
        entry.session_observation = Some(
            if alive {
                SessionObservation::Loaded
            } else {
                SessionObservation::NotLoaded
            }
            .as_str()
            .to_string(),
        );
        entry.observed_at = Some(now.to_rfc3339());
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

/// 这张卡的执行会话属于哪个 ID（只认非空 `thread_id`，**不**用来源会话）。
pub fn execution_thread_id(card: &Card) -> Option<&str> {
    card.thread_id.as_deref().filter(|id| !id.trim().is_empty())
}

/// 一张 running 卡的执行会话相对当前宿主是什么加载态。
///
/// 这是 list / CLI / dispatch 的**唯一**投影入口：三处不得各自重新解释 `alive`。
/// 非 running 卡不报告加载问题（没有会话是正常的），缺记录也是 `Unknown`
/// ——「没问过」不是「已经没了」。
pub fn for_card(card: &Card, file: &ObservationFile) -> SessionObservation {
    if card.state != CardState::Running {
        return SessionObservation::Unknown;
    }
    let Some(thread_id) = execution_thread_id(card) else {
        return SessionObservation::Unknown;
    };
    file.threads
        .get(thread_id)
        .map(Observation::presentation)
        .unwrap_or(SessionObservation::Unknown)
}

/// 一张卡是否「写着 running，而当前宿主明确没有加载它的执行会话」。
///
/// 只有 `NotLoaded` 才算 stale。`Unknown`（旧文件、查询失败、缺记录）**不**
/// 声称会话已经不存在——那正是本次要根除的误报。它也只影响展示，永不迁移状态。
pub fn stale<'a>(card: &Card, file: &'a ObservationFile) -> Option<&'a Observation> {
    if for_card(card, file) != SessionObservation::NotLoaded {
        return None;
    }
    let thread_id = execution_thread_id(card)?;
    file.threads.get(thread_id)
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

    /// 旧文件里只有 `alive:false`：那是「这一拍没看见」，不是「当前宿主确认
    /// 没加载」。降级 unknown，且不得拿旧 `last_seen_alive_at` 冒充观测时间。
    #[test]
    fn kanban_observation_legacy_false_is_unknown() {
        let obs: Observation = serde_json::from_value(serde_json::json!({
            "alive":false,"last_seen_alive_at":"2026-11-03T08:35:13+08:00"
        }))
        .unwrap();
        assert_eq!(obs.presentation(), SessionObservation::Unknown);
        assert!(obs.presentation_at().is_none());
        // 旧字段本身仍可读，既有调度口径不变。
        assert!(!obs.alive);
        assert!(obs.last_seen_alive_at.is_some());
    }

    /// 新字段坏类型只降级那一个 ID：不能让整个文件 `load` 失败退回 default，
    /// 也不能连累同一份文件里其他有效条目。
    #[test]
    fn kanban_observation_bad_new_fields_preserve_other_ids() {
        let file: ObservationFile = serde_json::from_value(serde_json::json!({"threads":{
            "bad":{"alive":false,"session_observation":42,"observed_at":{}},
            "good":{"alive":true,"session_observation":"loaded", "observed_at":"2026-11-03T10:00:00+08:00"}
        }}))
        .unwrap();
        assert_eq!(
            file.threads["bad"].presentation(),
            SessionObservation::Unknown
        );
        assert_eq!(
            file.threads["good"].presentation(),
            SessionObservation::Loaded
        );
        assert!(file.threads["good"].alive);
    }

    /// 未知枚举、非法 RFC3339 都是 unknown：不猜、不编造「截至」时间。
    #[test]
    fn kanban_observation_unknown_state_or_bad_date_is_unknown() {
        for value in [
            serde_json::json!({"alive":true,"session_observation":"weird","observed_at":"2026-11-03T10:00:00+08:00"}),
            serde_json::json!({"alive":true,"session_observation":"loaded","observed_at":"not a date"}),
            serde_json::json!({"alive":true,"session_observation":"loaded"}),
            serde_json::json!({"alive":true,"session_observation":"loaded","observed_at":"2026-02-30T10:00:00+08:00"}),
        ] {
            let obs: Observation = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(
                obs.presentation(),
                SessionObservation::Unknown,
                "must not claim an observation state: {value}"
            );
            assert!(obs.presentation_at().is_none(), "{value}");
        }
    }

    /// `presentation_at` 必须是**这份快照**的时间，不是最后活动时间。
    #[test]
    fn kanban_observation_presentation_time_is_the_snapshot_time() {
        let obs: Observation = serde_json::from_value(serde_json::json!({
            "alive":true,
            "last_seen_alive_at":"2026-11-03T08:35:13+08:00",
            "session_observation":"loaded",
            "observed_at":"2026-11-03T10:00:00+08:00"
        }))
        .unwrap();
        assert_eq!(obs.presentation(), SessionObservation::Loaded);
        let at = obs.presentation_at().expect("a valid snapshot time");
        assert_eq!(at.to_rfc3339(), "2026-11-03T10:00:00+08:00");
        assert_ne!(
            at.to_rfc3339(),
            obs.last_seen_alive_at.unwrap().to_rfc3339(),
            "must report the observation time, not the last-alive time"
        );
    }

    /// `for_card` 是 list / CLI 的唯一投影：非 running 不显示加载异常，缺执行
    /// ID 也是 unknown，缺记录同样是 unknown（不推断死亡）。
    #[test]
    fn kanban_observation_for_card_covers_nonrunning_and_missing() {
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
        let mut file = ObservationFile::default();
        let card = board.get(&CardId::new("a")).unwrap();
        assert_eq!(for_card(card, &file), SessionObservation::Unknown);

        board
            .set_thread_id(&CardId::new("a"), "thread-1".into())
            .unwrap();
        let card = board.get(&CardId::new("a")).unwrap();
        assert_eq!(
            for_card(card, &file),
            SessionObservation::Unknown,
            "no record is not proof of death"
        );

        file.threads.insert(
            "thread-1".to_string(),
            Observation {
                alive: false,
                last_seen_alive_at: None,
                session_observation: Some("not_loaded".to_string()),
                observed_at: Some("2026-11-03T10:00:00+08:00".to_string()),
            },
        );
        assert_eq!(for_card(card, &file), SessionObservation::NotLoaded);

        board
            .transition(&CardId::new("a"), CardState::Done)
            .unwrap();
        let card = board.get(&CardId::new("a")).unwrap();
        assert_eq!(
            for_card(card, &file),
            SessionObservation::Unknown,
            "a finished card has no loading problem to report"
        );
    }

    /// `stale` 只在「running + 有非空执行 ID + 明确未加载」时为真。unknown
    /// 不声称已经不存在——这正是本次误报的核心。
    #[test]
    fn kanban_observation_stale_requires_explicit_not_loaded() {
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

        let mut file = ObservationFile::default();
        file.threads.insert(
            "thread-1".to_string(),
            Observation {
                alive: false,
                last_seen_alive_at: None,
                session_observation: None,
                observed_at: None,
            },
        );
        assert!(
            stale(card, &file).is_none(),
            "a legacy false must not render as stale"
        );

        file.threads.insert(
            "thread-1".to_string(),
            Observation {
                alive: false,
                last_seen_alive_at: None,
                session_observation: Some("not_loaded".to_string()),
                observed_at: Some("2026-11-03T10:00:00+08:00".to_string()),
            },
        );
        assert!(stale(card, &file).is_some());

        file.threads.insert(
            "thread-1".to_string(),
            Observation {
                alive: true,
                last_seen_alive_at: Some(at(1, 0)),
                session_observation: Some("loaded".to_string()),
                observed_at: Some("2026-11-03T10:00:00+08:00".to_string()),
            },
        );
        assert!(stale(card, &file).is_none(), "loaded is not stale");
    }

    /// `observe(typed bool)` 是**显式证据**：它可以把展示态写成 loaded/not_loaded，
    /// 因为这些调用点都拿到了宿主明确给出的布尔值。
    #[test]
    fn kanban_observation_typed_observe_records_evidence() {
        let mut file = ObservationFile::default();
        file.observe("thread-1", true, at(1, 0));
        assert_eq!(
            file.threads["thread-1"].presentation(),
            SessionObservation::Loaded
        );
        file.observe("thread-1", false, at(1, 1));
        assert_eq!(
            file.threads["thread-1"].presentation(),
            SessionObservation::NotLoaded
        );
        assert_eq!(
            file.threads["thread-1"].presentation_at(),
            Some(at(1, 1)),
            "a not_loaded sighting carries its own time"
        );
        assert_eq!(
            file.threads["thread-1"].last_seen_alive_at,
            Some(at(1, 0)),
            "a dead sighting must not overwrite the last-alive time"
        );
    }
}
