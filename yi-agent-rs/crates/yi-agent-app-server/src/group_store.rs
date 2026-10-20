//! 会话分组（session groups）的持久化。
//!
//! 每个 workspace 一个文件 `<workspace>/.yi-agent/groups.json`：
//!
//! ```jsonc
//! { "version": 1, "groups": [ { "id": "grp-…", "name": "登录模块",
//!   "description": "…", "thread_ids": ["thread-a"] } ] }
//! ```
//!
//! 选一个文件而不是写进各 thread 的 `.meta.json`：一个文件就能列出某组的全部
//! 成员，删会话时只需改一处。**分组不是归属**——权威归属仍是每个 thread 的
//! `cwd`，故成员表在读取时会被按 workspace 过滤（见 `rpc::thread` 的
//! `build_list_all_response`）。
//!
//! 写路径与 `settings_store` 同款：读-改-写 + 「临时文件 + rename」原子替换。
//! 缺文件是首次使用的正常情形，不打日志；文件存在却读不出/解析失败则
//! `tracing::warn!` 并回退空表——坏分组绝不阻断侧栏。

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 组名上限（按 `char` 计，非字节）。
pub const NAME_MAX_CHARS: usize = 24;

/// 描述上限（按 `char` 计）。
pub const DESCRIPTION_MAX_CHARS: usize = 120;

/// 磁盘文件的版本号。读到未知版本按空表处理。
const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub thread_ids: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    groups: Vec<Group>,
}

fn groups_path(workspace: &Path) -> PathBuf {
    workspace.join(".yi-agent").join("groups.json")
}

/// 读该 workspace 的全部分组。缺文件 / 不可读 / 损坏 / 未知版本一律空表。
pub fn load(workspace: &Path) -> Vec<Group> {
    let path = groups_path(workspace);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            tracing::warn!(
                error = %error,
                path = %path.display(),
                "could not read session groups; treating as none"
            );
            return Vec::new();
        }
    };
    match serde_json::from_str::<File>(&text) {
        Ok(file) if file.version == VERSION => file.groups,
        Ok(file) => {
            tracing::warn!(
                version = file.version,
                path = %path.display(),
                "unknown session-groups file version; treating as none"
            );
            Vec::new()
        }
        Err(error) => {
            tracing::warn!(
                error = %error,
                path = %path.display(),
                "could not parse session groups; treating as none"
            );
            Vec::new()
        }
    }
}

/// 整体重写该 workspace 的分组表。
pub fn save(workspace: &Path, groups: &[Group]) -> io::Result<()> {
    let dir = workspace.join(".yi-agent");
    std::fs::create_dir_all(&dir)?;
    let text = serde_json::to_string_pretty(&File {
        version: VERSION,
        groups: groups.to_vec(),
    })
    .map_err(io::Error::other)?;
    crate::thread_store::write_atomic(&groups_path(workspace), text.as_bytes())
}

/// 读-改-写。进程内以全局 `Mutex` 串行化并发写。
///
/// 读与写都在锁内：只锁写会让「读-改-写」中间被另一个写者插进来，后写者用旧
/// 快照覆盖前者的结果。
pub fn mutate(workspace: &Path, f: impl FnOnce(&mut Vec<Group>)) -> io::Result<()> {
    let _guard = write_lock();
    let mut groups = load(workspace);
    f(&mut groups);
    save(workspace, &groups)
}

/// 该 workspace 的写锁。
///
/// 分组写操作很少（用户手动建/改组），一把进程级全局锁足够：**不**为每个目录
/// 各建一把，否则要么把锁表永远留在内存里、要么得在 `MutexGuard<'static>` 上
/// 用 `Box::leak`（那是内存泄漏）。锁序只有一把，也就不存在死锁。
fn write_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// 组名校验：去首尾空白，非空且不超过 [`NAME_MAX_CHARS`] 个字符。
pub fn validate_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("group name must not be empty".to_string());
    }
    if name.chars().count() > NAME_MAX_CHARS {
        return Err(format!(
            "group name must be at most {NAME_MAX_CHARS} characters"
        ));
    }
    Ok(name.to_string())
}

/// 描述校验：允许空串，不超过 [`DESCRIPTION_MAX_CHARS`] 个字符。
pub fn validate_description(raw: &str) -> Result<String, String> {
    if raw.chars().count() > DESCRIPTION_MAX_CHARS {
        return Err(format!(
            "group description must be at most {DESCRIPTION_MAX_CHARS} characters"
        ));
    }
    Ok(raw.to_string())
}

/// 新组的 id。
pub fn new_id() -> String {
    format!("grp-{}", uuid::Uuid::new_v4())
}

/// 剔除不满足 `is_member` 的成员，并删除因此变空的组。
///
/// 顺手去重并保持首次出现顺序：`Vec::dedup` 只合并**相邻**重复，而文件是手改
/// 或旧版本写下的，重复项未必相邻。
pub fn prune(groups: &mut Vec<Group>, is_member: impl Fn(&str) -> bool) {
    for group in groups.iter_mut() {
        let mut seen = std::collections::HashSet::new();
        group
            .thread_ids
            .retain(|id| is_member(id) && seen.insert(id.clone()));
    }
    groups.retain(|group| !group.thread_ids.is_empty());
}

/// 从所有组里移除某个 thread，删空的组整组移除。
///
/// 由 `thread/delete` 调用：会话文件已删，成员表不能留悬空 id。
pub fn remove_thread(workspace: &Path, thread_id: &str) -> io::Result<()> {
    mutate(workspace, |groups| prune(groups, |id| id != thread_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(id: &str, name: &str, ids: &[&str]) -> Group {
        Group {
            id: id.to_string(),
            name: name.to_string(),
            description: String::new(),
            thread_ids: ids.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn a_missing_file_loads_as_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(load(dir.path()).is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::TempDir::new().unwrap();
        let groups = vec![group("grp-a", "登录模块", &["thread-a", "thread-b"])];
        save(dir.path(), &groups).unwrap();
        let back = load(dir.path());
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].id, "grp-a");
        assert_eq!(back[0].name, "登录模块");
        assert_eq!(back[0].thread_ids, vec!["thread-a", "thread-b"]);
    }

    /// 落盘形状是磁盘契约：`thread_ids` 必须是 snake_case，未知键必须能读进来。
    #[test]
    fn on_disk_shape_uses_snake_case_and_tolerates_unknown_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        let dir_path = dir.path().join(".yi-agent");
        std::fs::create_dir_all(&dir_path).unwrap();
        std::fs::write(
            dir_path.join("groups.json"),
            r#"{"version":1,"groups":[{"id":"grp-a","name":"x","description":"","thread_ids":["thread-a"],"future_key":1}]}"#,
        )
        .unwrap();
        let groups = load(dir.path());
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].thread_ids, vec!["thread-a"]);
    }

    #[test]
    fn a_corrupt_file_loads_as_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let dir_path = dir.path().join(".yi-agent");
        std::fs::create_dir_all(&dir_path).unwrap();
        std::fs::write(dir_path.join("groups.json"), "{not json").unwrap();
        assert!(load(dir.path()).is_empty());
    }

    #[test]
    fn mutate_reads_then_writes() {
        let dir = tempfile::TempDir::new().unwrap();
        save(dir.path(), &[group("grp-a", "a", &["thread-a"])]).unwrap();
        mutate(dir.path(), |groups| {
            groups[0].name = "b".to_string();
        })
        .unwrap();
        assert_eq!(load(dir.path())[0].name, "b");
    }

    #[test]
    fn remove_thread_drops_the_member_and_any_emptied_group() {
        let dir = tempfile::TempDir::new().unwrap();
        save(
            dir.path(),
            &[
                group("grp-a", "a", &["thread-a", "thread-b"]),
                group("grp-b", "b", &["thread-a"]),
            ],
        )
        .unwrap();
        remove_thread(dir.path(), "thread-a").unwrap();
        let back = load(dir.path());
        assert_eq!(back.len(), 1, "只剩非空组");
        assert_eq!(back[0].id, "grp-a");
        assert_eq!(back[0].thread_ids, vec!["thread-b"]);
    }

    #[test]
    fn prune_drops_non_members_and_emptied_groups() {
        let mut groups = vec![
            group("grp-a", "a", &["thread-a", "thread-x"]),
            group("grp-b", "b", &["thread-x"]),
        ];
        prune(&mut groups, |id| id != "thread-x");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].thread_ids, vec!["thread-a"]);
    }

    #[test]
    fn name_validation_trims_and_bounds_by_chars() {
        assert_eq!(validate_name("  登录  ").unwrap(), "登录");
        assert!(validate_name("   ").is_err());
        assert!(validate_name(&"字".repeat(NAME_MAX_CHARS)).is_ok());
        assert!(validate_name(&"字".repeat(NAME_MAX_CHARS + 1)).is_err());
    }

    #[test]
    fn description_validation_allows_empty_and_bounds_by_chars() {
        assert_eq!(validate_description("").unwrap(), "");
        assert!(validate_description(&"字".repeat(DESCRIPTION_MAX_CHARS)).is_ok());
        assert!(validate_description(&"字".repeat(DESCRIPTION_MAX_CHARS + 1)).is_err());
    }

    #[test]
    fn new_id_is_prefixed_and_unique() {
        let a = new_id();
        assert!(a.starts_with("grp-"));
        assert_ne!(a, new_id());
    }
}
