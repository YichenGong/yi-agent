//! 每个项目最多一条「合并在飞」安全记录（spec §8.1）。
//!
//! 它**不是**待重放的消息队列，而是**安全门**：证明某个动作可能已经把合并指令
//! 交给了宿主，因此在确认它没交出去之前，不能再派第二个。卡片归档、purge 都
//! 不会擦掉它——这正是它独立于卡片存在的原因。
//!
//! 三个 API 都**不自己取 `board.lock`**：调用者必须已经在 `persist::with_board`
//! 的锁内（或者明确知道自己在读一个原子的只读快照）。理由见 [`save`]：换个
//! action 覆盖别人的记录只可能是一次调度竞态，必须由同一把锁下的「先读后比」
//! 排掉。
//!
//! 读严格、写原子，是全模块的两条铁律：
//! - 坏 JSON、未知 version、语义不合法的记录一律**报错**，绝不按「没有记录」
//!   处理——那会让一个可能已经投递的合并被重复派发。
//! - 写用同目录临时文件 + `sync_all` + `rename`，读者永远看到完整文件。

use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

/// 记录的版本。只有 1 是已知格式；别的版本必须阻塞而不是猜。
pub const ATTEMPT_VERSION: u32 = 1;

/// 记录文件的固定名字（项目 state-dir 下）。
pub const ATTEMPT_FILE: &str = "merge.attempt.json";

/// 这条记录处在什么阶段。它描述的是**我们对投递结果的把握**，不是卡片状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergePhase {
    /// 已决定要派，动作尚未交出（journal 已落盘、Running 还没落盘的那个窗口）。
    Pending,
    /// 动作已交给宿主。此时宿主可能正在跑一个合并 turn。
    Accepted,
    /// 不知道动作有没有交出去（回报遗失、写盘失败、进程在中间崩溃）。
    /// 只能人工确认，绝不自动重试。
    Uncertain,
}

/// 项目级最小在飞记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeAttemptRecord {
    pub version: u32,
    pub action_id: String,
    pub card_id: String,
    /// 独立合并卡首次 Launch 时可以为空（它还不需要绑定原会话）；
    /// 实现卡的 `Accepted` 必须非空。
    pub thread_id: Option<String>,
    /// 派发时卡片目录的快照。实现卡是原开发树，独立卡是目标树。
    pub expected_workdir: PathBuf,
    pub phase: MergePhase,
    /// 卡片入队时刻。退回队列时用它**保留原顺位**。
    pub merge_queued_at: Option<DateTime<Local>>,
}

fn attempt_path(state_dir: &Path) -> PathBuf {
    state_dir.join(ATTEMPT_FILE)
}

/// 严格读一次。`NotFound` 才是 `None`；其余一切错误都向上报。
pub fn read(state_dir: &Path) -> Result<Option<MergeAttemptRecord>, String> {
    let path = attempt_path(state_dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "could not read the merge attempt record {}: {error}",
                path.display()
            ));
        }
    };
    let record: MergeAttemptRecord = serde_json::from_str(&text).map_err(|error| {
        format!(
            "the merge attempt record {} is corrupt: {error}; \
             refusing to treat it as absent",
            path.display()
        )
    })?;
    validate(&record)?;
    Ok(Some(record))
}

/// 语义校验。**不只是**「JSON 能解析」：一份语法正确但语义荒谬的记录同样危险。
pub fn validate(record: &MergeAttemptRecord) -> Result<(), String> {
    if record.version != ATTEMPT_VERSION {
        return Err(format!(
            "merge attempt record has unknown version {} (only {ATTEMPT_VERSION} is understood)",
            record.version
        ));
    }
    if record.card_id.trim().is_empty() {
        return Err("merge attempt record has an empty card id".to_string());
    }
    if record.action_id.trim().is_empty() {
        return Err("merge attempt record has an empty action id".to_string());
    }
    // 旧版覆写留下的 sentinel 不能被当成一条真实动作：它无法与任何一次本次
    // 派发对上，却会永久占着安全门。
    if record.action_id == LEGACY_ACTION_ID {
        return Err(format!(
            "merge attempt record carries the legacy action id {LEGACY_ACTION_ID:?}, \
             which is not an actionable dispatch; resolve it by hand"
        ));
    }
    if !record.expected_workdir.is_absolute() {
        return Err(format!(
            "merge attempt record names a relative workdir {:?}; \
             it must be an absolute snapshot",
            record.expected_workdir
        ));
    }
    // `Accepted` 说「动作已交出」。实现卡的合并必须绑定一个原会话；没有 thread
    // 的 Accepted 只可能来自独立 Launch 的旧路径或一次写坏，两种都不该被当成
    // 可继续的状态。
    if record.phase == MergePhase::Accepted
        && record
            .thread_id
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
    {
        return Err("merge attempt record is accepted but names no thread id; \
             only a standalone launch may be pending without one"
            .to_string());
    }
    Ok(())
}

/// 旧版（覆写 thread_id 的那一代）留下的保留 action id。
pub const LEGACY_ACTION_ID: &str = "legacy";

/// 原子写。调用者必须已在 `board.lock` 内**先读过**并确认可以覆盖。
///
/// 不做「静默覆盖另一条 action」的兜底：那是调度竞态，该由持锁的调用者排掉，
/// 而不是让写入口猜。
pub fn save(state_dir: &Path, record: &MergeAttemptRecord) -> Result<(), String> {
    validate(record)?;
    let json = serde_json::to_vec_pretty(record)
        .map_err(|error| format!("could not encode the merge attempt record: {error}"))?;
    std::fs::create_dir_all(state_dir).map_err(|error| {
        format!(
            "could not create the project state directory {}: {error}",
            state_dir.display()
        )
    })?;
    let path = attempt_path(state_dir);
    let temp = path.with_extension("json.tmp");
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&temp)
            .map_err(|error| format!("could not create {}: {error}", temp.display()))?;
        file.write_all(&json)
            .map_err(|error| format!("could not write {}: {error}", temp.display()))?;
        // 安全门的内容必须先落盘再 rename：否则崩溃可能留下一个空的「已重命名」
        // 文件，等于把安全门换成了一张白纸。
        file.sync_all()
            .map_err(|error| format!("could not flush {}: {error}", temp.display()))?;
    }
    std::fs::rename(&temp, &path).map_err(|error| {
        format!(
            "could not replace the merge attempt record {}: {error}",
            path.display()
        )
    })
}

/// 只有当前记录**确实属于**这次 `(card, action)` 时才清掉。
///
/// 返回 `true` 表示清掉了。返回 `false` 表示记录属于别的动作（或不在了）——
/// 调用者据此知道「我没有权利清这条」，绝不能把别人留下的安全门拆了。
///
/// 为什么必须 CAS：迟到的旧结果、另一个插件的动作、用户手工恢复，都会带着
/// 已经过期的 action id 回来。无条件清门会让一个可能仍在飞的合并失去保护。
pub fn clear_if_matches(state_dir: &Path, card_id: &str, action_id: &str) -> Result<bool, String> {
    let Some(current) = read(state_dir)? else {
        return Ok(false);
    };
    if current.card_id != card_id || current.action_id != action_id {
        return Ok(false);
    }
    let path = attempt_path(state_dir);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "could not clear the merge attempt record {}: {error}",
            path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn record(phase: MergePhase) -> MergeAttemptRecord {
        MergeAttemptRecord {
            version: ATTEMPT_VERSION,
            action_id: "action-1".to_string(),
            card_id: "card-1".to_string(),
            thread_id: Some("thread-1".to_string()),
            expected_workdir: PathBuf::from("/tmp/project/card-1"),
            phase,
            merge_queued_at: Local.with_ymd_and_hms(2026, 10, 1, 9, 0, 0).single(),
        }
    }

    #[test]
    fn corrupt_attempt_is_not_an_empty_merge_gate() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(ATTEMPT_FILE), b"{").unwrap();
        let outcome = read(dir.path());
        assert!(
            outcome.is_err(),
            "a corrupt record must block, not read as 'no merge in flight': {outcome:?}"
        );
    }

    #[test]
    fn a_missing_record_is_the_only_absent_case() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(dir.path()).unwrap(), None);
        std::fs::remove_dir_all(dir.path()).unwrap();
        assert_eq!(
            read(dir.path()).unwrap(),
            None,
            "a missing dir is absent too"
        );
    }

    #[test]
    fn an_unreadable_record_reports_instead_of_looking_absent() {
        let dir = tempfile::tempdir().unwrap();
        // 把记录位置占成目录：读它会得到 EISDIR，绝不是 NotFound。
        std::fs::create_dir(dir.path().join(ATTEMPT_FILE)).unwrap();
        let outcome = read(dir.path());
        assert!(
            outcome.is_err(),
            "a record that exists but cannot be read must not be treated as absent: {outcome:?}"
        );
    }

    #[test]
    fn a_saved_record_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        for phase in [
            MergePhase::Pending,
            MergePhase::Accepted,
            MergePhase::Uncertain,
        ] {
            let saved = record(phase.clone());
            save(dir.path(), &saved).unwrap();
            assert_eq!(read(dir.path()).unwrap(), Some(saved));
        }
    }

    #[test]
    fn a_pending_record_may_name_no_thread() {
        let dir = tempfile::tempdir().unwrap();
        let mut pending = record(MergePhase::Pending);
        pending.thread_id = None;
        save(dir.path(), &pending).unwrap();
        assert_eq!(read(dir.path()).unwrap().unwrap().thread_id, None);
    }

    #[test]
    fn an_accepted_record_must_name_its_thread() {
        let dir = tempfile::tempdir().unwrap();
        let mut accepted = record(MergePhase::Accepted);
        accepted.thread_id = None;
        assert!(save(dir.path(), &accepted).is_err());
    }

    #[test]
    fn an_unknown_version_is_refused_on_both_sides() {
        let dir = tempfile::tempdir().unwrap();
        let mut future = record(MergePhase::Pending);
        future.version = ATTEMPT_VERSION + 1;
        assert!(save(dir.path(), &future).is_err());

        // 绕过 save 的校验,直接落一份未来版本的记录:读也必须拒绝。
        future.version = 99;
        std::fs::write(
            dir.path().join(ATTEMPT_FILE),
            serde_json::to_vec(&future).unwrap(),
        )
        .unwrap();
        assert!(read(dir.path()).is_err());
    }

    #[test]
    fn the_legacy_sentinel_is_not_an_actionable_record() {
        let dir = tempfile::tempdir().unwrap();
        let mut legacy = record(MergePhase::Uncertain);
        legacy.action_id = LEGACY_ACTION_ID.to_string();
        std::fs::write(
            dir.path().join(ATTEMPT_FILE),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();
        let outcome = read(dir.path());
        assert!(
            outcome.is_err(),
            "the legacy sentinel cannot be matched by a real dispatch: {outcome:?}"
        );
    }

    #[test]
    fn a_relative_workdir_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut relative = record(MergePhase::Pending);
        relative.expected_workdir = PathBuf::from("card-1");
        assert!(save(dir.path(), &relative).is_err());
    }

    #[test]
    fn an_empty_identity_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut no_card = record(MergePhase::Pending);
        no_card.card_id = "  ".to_string();
        assert!(save(dir.path(), &no_card).is_err());
        let mut no_action = record(MergePhase::Pending);
        no_action.action_id = "".to_string();
        assert!(save(dir.path(), &no_action).is_err());
    }

    #[test]
    fn clearing_only_takes_effect_for_the_matching_action() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &record(MergePhase::Accepted)).unwrap();

        assert!(
            !clear_if_matches(dir.path(), "card-1", "someone-else").unwrap(),
            "another action must not be able to clear this gate"
        );
        assert!(
            !clear_if_matches(dir.path(), "card-2", "action-1").unwrap(),
            "the wrong card must not clear this gate"
        );
        assert!(read(dir.path()).unwrap().is_some(), "the gate still stands");

        assert!(clear_if_matches(dir.path(), "card-1", "action-1").unwrap());
        assert_eq!(read(dir.path()).unwrap(), None);
        // 再清一次是幂等的 false,不是错误。
        assert!(!clear_if_matches(dir.path(), "card-1", "action-1").unwrap());
    }

    #[test]
    fn a_failed_write_keeps_the_previous_record() {
        let dir = tempfile::tempdir().unwrap();
        let original = record(MergePhase::Accepted);
        save(dir.path(), &original).unwrap();

        // 把临时文件位置占成目录,让下一次写入在 rename 之前就失败。
        let temp = dir.path().join(ATTEMPT_FILE).with_extension("json.tmp");
        std::fs::create_dir(&temp).unwrap();
        let mut replacement = record(MergePhase::Uncertain);
        replacement.action_id = "action-2".to_string();
        assert!(save(dir.path(), &replacement).is_err());

        assert_eq!(
            read(dir.path()).unwrap(),
            Some(original),
            "a failed write must not destroy the record it was replacing"
        );
    }

    #[test]
    fn saving_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &record(MergePhase::Pending)).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");
    }
}
