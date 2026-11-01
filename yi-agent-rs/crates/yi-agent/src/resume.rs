//! `resume` / `fork` / `threads` 三个子命令的**解析与展示**部分。
//!
//! 落盘语义一律在 `TuiSessionStore` 与 `ThreadStore`；这里只做「把用户写的
//! id 解析成完整 id」「按目录挑最近一条」「把会话列表打成人能读的表格」。
//! 子命令的编排（打开 store、装配 TUI）留在 `main.rs`，因为它要复用
//! `run_agent` 的整套装配逻辑。

use std::path::Path;

use yi_agent_app_server::thread_store::{ThreadMeta, ThreadStore};

/// id 解析失败。
#[derive(Debug)]
pub enum ResolveError {
    NotFound(String),
    Ambiguous(Vec<String>),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::NotFound(spec) => {
                write!(f, "找不到会话 {spec}（用 `yi-agent threads` 查看）")
            }
            ResolveError::Ambiguous(candidates) => {
                writeln!(f, "前缀有歧义，候选：")?;
                for c in candidates {
                    writeln!(f, "  {c}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ResolveError {}

/// 把「完整 id 或唯一前缀」解析成完整 id。
///
/// 支持前缀是为了省打字（会话 id 是 uuid，完整抄一遍不现实）。**歧义必须报错
/// 而不是随便挑一个**：挑错了会把历史接在另一个话题上，用户直到看到上下文
/// 不对才发现。
pub fn resolve_thread_id(store: &ThreadStore, spec: &str) -> Result<String, ResolveError> {
    let all = store.list().unwrap_or_default();
    if all.iter().any(|m| m.thread_id == spec) {
        return Ok(spec.to_string());
    }
    let matches: Vec<String> = all
        .iter()
        .filter(|m| m.thread_id.starts_with(spec))
        .map(|m| m.thread_id.clone())
        .collect();
    match matches.len() {
        0 => Err(ResolveError::NotFound(spec.to_string())),
        1 => Ok(matches.into_iter().next().expect("len == 1")),
        _ => Err(ResolveError::Ambiguous(matches)),
    }
}

/// `--last`：同目录最近一条。
///
/// 比较前先 `canonicalize`，否则符号链接与相对路径写法匹配不上——
/// 同一目录两种写法会让人以为「最近会话不见了」。
pub fn pick_last(store: &ThreadStore, workdir: &Path) -> Option<ThreadMeta> {
    let want = workdir
        .canonicalize()
        .unwrap_or_else(|_| workdir.to_path_buf());
    let mut all = store.list().ok()?;
    all.sort_by_key(|m| std::cmp::Reverse(m.updated_at));
    all.into_iter().find(|m| {
        // 损坏重建出来的老会话 cwd 是空串，按「与当前目录相同」处理：
        // 它本来就只能属于当前工作区（meta 就在这里的 `.yi-agent/threads` 下）。
        if m.cwd.is_empty() {
            return true;
        }
        Path::new(&m.cwd)
            .canonicalize()
            .map(|p| p == want)
            .unwrap_or(false)
    })
}

/// `updated_at` 是 Unix 毫秒。项目里没有既有的时间格式化函数，这里给一个最小
/// 的本地时间渲染。
///
/// 走 `libc::localtime_r` 而不是引入 `chrono`：`libc` 已是本 crate 的依赖
/// （TUI 的 OSC 11 探测在用），不必为一个展示用时间戳新增依赖。
pub fn format_millis_local(ms: i64) -> String {
    let secs = ms / 1000;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let time: libc::time_t = secs as libc::time_t;
    let ok = unsafe { !libc::localtime_r(&time, &mut tm).is_null() };
    if !ok {
        return format!("{secs}");
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store_with(ids: &[(&str, i64, &str)]) -> (TempDir, ThreadStore) {
        let dir = TempDir::new().unwrap();
        let store = ThreadStore::new(dir.path());
        for (id, updated, cwd) in ids {
            let now = *updated;
            store
                .create(&ThreadMeta {
                    thread_id: (*id).to_string(),
                    cwd: (*cwd).to_string(),
                    model: "m".into(),
                    created_at: now,
                    updated_at: now,
                    title: None,
                    permission_mode: yi_agent_app_server::thread_store::ThreadMode::Normal,
                    pin_seq: None,
                    driver_project: None,
                    driver_ref: None,
                    model_ref: None,
                    reasoning_effort: None,
                    tag: None,
                    preserve_thinking: true,
                    extra_roots: Vec::new(),
                    role: None,
                    content_epoch: 0,
                    last_item_id: None,
                    anchor_sha: None,
                    last_read_at: 0,
                    last_completed_at: 0,
                    forked_from: None,
                })
                .unwrap();
        }
        (dir, store)
    }

    #[test]
    fn exact_id_wins_over_prefix() {
        let (_d, store) = store_with(&[("thread-abc", 1, "/tmp"), ("thread-abcdef", 2, "/tmp")]);
        assert_eq!(
            resolve_thread_id(&store, "thread-abc").unwrap(),
            "thread-abc"
        );
    }

    #[test]
    fn unique_prefix_resolves() {
        let (_d, store) = store_with(&[("thread-abc", 1, "/tmp"), ("thread-xyz", 2, "/tmp")]);
        assert_eq!(resolve_thread_id(&store, "thread-a").unwrap(), "thread-abc");
    }

    #[test]
    fn ambiguous_prefix_lists_candidates() {
        let (_d, store) = store_with(&[("thread-abc", 1, "/tmp"), ("thread-abd", 2, "/tmp")]);
        match resolve_thread_id(&store, "thread-ab") {
            Err(ResolveError::Ambiguous(c)) => assert_eq!(c.len(), 2),
            other => panic!("期望 Ambiguous，实际 {other:?}"),
        }
    }

    #[test]
    fn unknown_id_is_not_found() {
        let (_d, store) = store_with(&[("thread-abc", 1, "/tmp")]);
        assert!(matches!(
            resolve_thread_id(&store, "thread-nope"),
            Err(ResolveError::NotFound(_))
        ));
    }

    #[test]
    fn pick_last_prefers_newest_in_the_same_directory() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let cwd_str = cwd.display().to_string();
        let store = ThreadStore::new(&cwd);
        for (id, updated) in [
            ("thread-old", 10_i64),
            ("thread-new", 30),
            ("thread-mid", 20),
        ] {
            store
                .create(&ThreadMeta {
                    thread_id: id.into(),
                    cwd: cwd_str.clone(),
                    model: "m".into(),
                    created_at: updated,
                    updated_at: updated,
                    title: None,
                    permission_mode: yi_agent_app_server::thread_store::ThreadMode::Normal,
                    pin_seq: None,
                    driver_project: None,
                    driver_ref: None,
                    model_ref: None,
                    reasoning_effort: None,
                    tag: None,
                    preserve_thinking: true,
                    extra_roots: Vec::new(),
                    role: None,
                    content_epoch: 0,
                    last_item_id: None,
                    anchor_sha: None,
                    last_read_at: 0,
                    last_completed_at: 0,
                    forked_from: None,
                })
                .unwrap();
        }
        let picked = pick_last(&store, &cwd).expect("must pick one");
        assert_eq!(picked.thread_id, "thread-new");
    }

    #[test]
    fn pick_last_ignores_a_thread_from_another_directory() {
        let dir = TempDir::new().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let store = ThreadStore::new(&cwd);
        store
            .create(&ThreadMeta {
                thread_id: "thread-elsewhere".into(),
                cwd: "/definitely/not/here".into(),
                model: "m".into(),
                created_at: 99,
                updated_at: 99,
                title: None,
                permission_mode: yi_agent_app_server::thread_store::ThreadMode::Normal,
                pin_seq: None,
                driver_project: None,
                driver_ref: None,
                model_ref: None,
                reasoning_effort: None,
                tag: None,
                preserve_thinking: true,
                extra_roots: Vec::new(),
                role: None,
                content_epoch: 0,
                last_item_id: None,
                anchor_sha: None,
                last_read_at: 0,
                last_completed_at: 0,
                forked_from: None,
            })
            .unwrap();
        assert!(
            pick_last(&store, &cwd).is_none(),
            "别的目录的会话不该被 --last 选中"
        );
    }
}
