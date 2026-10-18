//! 全局「最近目录」索引:`~/.yi-agent/workspaces.json`。
//!
//! 与 thread 数据分离——移除目录只动本文件,不碰 `<dir>/.yi-agent/`。
//! 读-改-写用进程内 `Mutex` 串行化,写入用 temp 文件 + rename 原子替换。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
struct WorkspacesFile {
    #[serde(default)]
    dirs: Vec<String>,
    /// 主根 → 附属根（均为 canonical 绝对路径）。旧文件无此字段，默认空。
    ///
    /// `skip_serializing_if` 让「没有任何关联」时文件保持旧形状 `{"dirs": [...]}`，
    /// 与改动前逐字节一致；`default` 保证旧文件零迁移读入。
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    links: std::collections::BTreeMap<String, Vec<String>>,
}

/// 关联被拒绝的原因。文案直接面向用户（UI 原样显示），故写清“为什么”。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkError(pub String);

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub struct WorkspaceIndex {
    path: PathBuf,
    lock: Mutex<()>,
}

/// 全局索引默认路径:`$HOME/.yi-agent/workspaces.json`。
pub fn default_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".yi-agent").join("workspaces.json")
}

impl WorkspaceIndex {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    /// 当前目录列表,最近的在前。
    pub fn list(&self) -> Vec<String> {
        self.read().dirs
    }

    /// 加入并置顶(去重)。path 应为已 canonicalize 的绝对路径。
    pub fn add(&self, path: &Path) -> std::io::Result<()> {
        let entry = path.to_string_lossy().to_string();
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut file = self.read();
        file.dirs.retain(|d| d != &entry);
        file.dirs.insert(0, entry);
        self.write(&file)
    }

    /// 从索引移除;不存在则幂等成功。
    pub fn remove(&self, path: &Path) -> std::io::Result<()> {
        let entry = path.to_string_lossy().to_string();
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut file = self.read();
        file.dirs.retain(|d| d != &entry);
        self.write(&file)
    }

    fn read(&self) -> WorkspacesFile {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(e) => {
                // NotFound 是正常空态;其它 IO 错误无法补救,只记日志后按空处理。
                if e.kind() != std::io::ErrorKind::NotFound {
                    eprintln!("[app-server] cannot read workspaces index: {e}");
                }
                return WorkspacesFile::default();
            }
        };
        serde_json::from_str(&raw).unwrap_or_else(|e| {
            eprintln!("[app-server] ignoring corrupt workspaces index: {e}");
            // 解析失败时把原文件挪到 .bak,避免随后的 add/remove 覆盖丢失内容。
            let mut bak = self.path.as_os_str().to_owned();
            bak.push(".bak");
            let _ = std::fs::rename(&self.path, PathBuf::from(bak));
            WorkspacesFile::default()
        })
    }

    fn write(&self, file: &WorkspacesFile) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(file)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        crate::thread_store::write_atomic(&self.path, &bytes)
    }

    /// 把 `child` 关联到 `primary` 之下。两边都必须是已存在的目录。
    ///
    /// 不变量：禁链式（主根不能是别人的附属根）、无环、一个附属根只属于一个主根、
    /// 主根 ≠ 附属根。`child` 若已有主根则改挂（重新父级），不报错。
    pub fn link(&self, primary: &Path, child: &Path) -> Result<(), LinkError> {
        let primary = canonical_dir(primary)?;
        let child = canonical_dir(child)?;
        if primary == child {
            return Err(LinkError("a folder cannot be linked to itself".into()));
        }
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut file = self.read();
        // 禁链式：primary 自己不能是别人的附属根。
        if file
            .links
            .values()
            .any(|kids| kids.iter().any(|k| k == &primary))
        {
            return Err(LinkError(format!(
                "{primary} is already linked into another project"
            )));
        }
        // 禁链式（另一半）：child 自己已经是主根（有附属根）时，不能再被挂到别人下面。
        if file.links.contains_key(&child) {
            return Err(LinkError(format!(
                "{child} is already a project root with linked folders"
            )));
        }
        // 环：child 已有附属根时，那个附属根不能是 primary 的后代。
        if let Some(existing) = file.links.get(&primary) {
            if existing.iter().any(|k| k == &child) {
                return Ok(()); // 幂等
            }
        }
        if child_is_primary_of(&file, &child, &primary) {
            return Err(LinkError(format!(
                "linking {child} under {primary} would create a cycle"
            )));
        }
        // 一个附属根只属于一个主根：先摘掉旧父级。
        for kids in file.links.values_mut() {
            kids.retain(|k| k != &child);
        }
        file.links.retain(|_, kids| !kids.is_empty());
        file.links.entry(primary).or_default().push(child);
        self.write(&file).map_err(|e| LinkError(e.to_string()))
    }

    /// 取消 `child` 的关联；不存在则幂等成功。
    pub fn unlink(&self, child: &Path) -> std::io::Result<()> {
        let entry = path_string(child);
        let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        let mut file = self.read();
        let mut changed = false;
        for kids in file.links.values_mut() {
            let before = kids.len();
            kids.retain(|k| k != &entry);
            changed |= kids.len() != before;
        }
        if !changed {
            return Ok(());
        }
        file.links.retain(|_, kids| !kids.is_empty());
        self.write(&file)
    }

    /// `path` 的主根（若它是附属根）。
    pub fn linked_to(&self, path: &Path) -> Option<String> {
        let entry = path_string(path);
        self.read()
            .links
            .iter()
            .find(|(_, kids)| kids.iter().any(|k| k == &entry))
            .map(|(primary, _)| primary.clone())
    }

    /// `primary` 的附属根列表。
    pub fn children(&self, primary: &Path) -> Vec<String> {
        let entry = path_string(primary);
        self.read().links.get(&entry).cloned().unwrap_or_default()
    }
}

/// canonicalize 并确认是目录。失败时给出面向用户的文案。
fn canonical_dir(path: &Path) -> Result<String, LinkError> {
    match std::fs::canonicalize(path) {
        Ok(p) if p.is_dir() => Ok(path_string(&p)),
        _ => Err(LinkError(format!("not a folder: {}", path.display()))),
    }
}

/// 与 `add` / `remove` 同一口径：canonicalize 不成功时回退原始串。
fn path_string(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string()
}

/// `child` 是否是 `primary` 某个祖先的主根位置（即把 child 挂到 primary 下会成环）。
fn child_is_primary_of(file: &WorkspacesFile, child: &str, primary: &str) -> bool {
    let mut cursor = primary.to_string();
    // 只允许一层，但用循环防御畸形文件里的深链。
    let mut guard = 0;
    while guard < 64 {
        guard += 1;
        let Some(kids) = file.links.get(&cursor) else {
            return false;
        };
        if kids.iter().any(|k| k == child) {
            return true;
        }
        match kids.first() {
            Some(next) => cursor = next.clone(),
            None => return false,
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn index() -> (TempDir, WorkspaceIndex) {
        let dir = TempDir::new().unwrap();
        let idx = WorkspaceIndex::new(dir.path().join("workspaces.json"));
        (dir, idx)
    }

    /// 索引存的是 canonical 绝对路径（macOS 上 `/tmp` → `/private/tmp`），
    /// 断言时同口径比对，避免把平台符号链接差异当成逻辑差异。
    fn canon(p: &str) -> String {
        std::fs::canonicalize(p)
            .unwrap()
            .to_string_lossy()
            .to_string()
    }

    #[test]
    fn add_then_list_round_trips() {
        let (_d, idx) = index();
        idx.add(Path::new("/tmp/a")).unwrap();
        idx.add(Path::new("/tmp/b")).unwrap();
        assert_eq!(idx.list(), vec!["/tmp/b".to_string(), "/tmp/a".to_string()]);
    }

    #[test]
    fn add_dedupes_and_moves_to_front() {
        let (_d, idx) = index();
        idx.add(Path::new("/tmp/a")).unwrap();
        idx.add(Path::new("/tmp/b")).unwrap();
        idx.add(Path::new("/tmp/a")).unwrap();
        assert_eq!(idx.list(), vec!["/tmp/a".to_string(), "/tmp/b".to_string()]);
    }

    #[test]
    fn remove_deletes_entry() {
        let (_d, idx) = index();
        idx.add(Path::new("/tmp/a")).unwrap();
        idx.add(Path::new("/tmp/b")).unwrap();
        idx.remove(Path::new("/tmp/a")).unwrap();
        assert_eq!(idx.list(), vec!["/tmp/b".to_string()]);
    }

    #[test]
    fn remove_missing_is_ok() {
        let (_d, idx) = index();
        assert!(idx.remove(Path::new("/tmp/nope")).is_ok());
    }

    #[test]
    fn list_missing_file_is_empty() {
        let (_d, idx) = index();
        assert!(idx.list().is_empty());
    }

    #[test]
    fn corrupt_file_is_treated_as_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("workspaces.json");
        std::fs::write(&path, b"{not json").unwrap();
        let idx = WorkspaceIndex::new(path);
        assert!(idx.list().is_empty());
    }

    #[test]
    fn persists_dirs_json_shape() {
        let (d, idx) = index();
        idx.add(Path::new("/tmp/a")).unwrap();
        let raw = std::fs::read_to_string(d.path().join("workspaces.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value, serde_json::json!({ "dirs": ["/tmp/a"] }));
    }

    #[test]
    fn creates_nested_parent_dir() {
        let dir = TempDir::new().unwrap();
        let idx = WorkspaceIndex::new(dir.path().join("nested/deep/workspaces.json"));
        idx.add(Path::new("/tmp/a")).unwrap();
        assert_eq!(idx.list(), vec!["/tmp/a".to_string()]);
    }

    #[test]
    fn corrupt_file_is_backed_up() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("workspaces.json");
        std::fs::write(&path, b"{not json").unwrap();
        let idx = WorkspaceIndex::new(path.clone());
        idx.add(Path::new("/tmp/a")).unwrap();
        assert_eq!(idx.list(), vec!["/tmp/a".to_string()]);
        let mut bak = path.as_os_str().to_owned();
        bak.push(".bak");
        assert!(PathBuf::from(bak).exists());
    }

    #[test]
    fn link_records_child_under_primary() {
        let (_d, idx) = index();
        std::fs::create_dir_all("/tmp/wsidx-a").unwrap();
        std::fs::create_dir_all("/tmp/wsidx-b").unwrap();
        idx.link(Path::new("/tmp/wsidx-a"), Path::new("/tmp/wsidx-b"))
            .unwrap();
        assert_eq!(
            idx.children(Path::new("/tmp/wsidx-a")),
            vec![canon("/tmp/wsidx-b")]
        );
        assert_eq!(
            idx.linked_to(Path::new("/tmp/wsidx-b")),
            Some(canon("/tmp/wsidx-a"))
        );
        assert_eq!(idx.linked_to(Path::new("/tmp/wsidx-a")), None);
    }

    #[test]
    fn link_rejects_self() {
        let (_d, idx) = index();
        std::fs::create_dir_all("/tmp/wsidx-self").unwrap();
        let err = idx
            .link(Path::new("/tmp/wsidx-self"), Path::new("/tmp/wsidx-self"))
            .unwrap_err();
        assert!(err.0.contains("itself"), "unexpected: {}", err.0);
    }

    #[test]
    fn link_rejects_chain() {
        let (_d, idx) = index();
        for d in ["/tmp/wsidx-p", "/tmp/wsidx-c", "/tmp/wsidx-g"] {
            std::fs::create_dir_all(d).unwrap();
        }
        idx.link(Path::new("/tmp/wsidx-p"), Path::new("/tmp/wsidx-c"))
            .unwrap();
        // c 已是附属根，不能再当 g 的主根（禁链式）。
        let err = idx
            .link(Path::new("/tmp/wsidx-c"), Path::new("/tmp/wsidx-g"))
            .unwrap_err();
        assert!(err.0.contains("linked"), "unexpected: {}", err.0);
    }

    #[test]
    fn link_rejects_making_a_primary_into_a_child() {
        let (_d, idx) = index();
        for d in ["/tmp/wsidx-root", "/tmp/wsidx-mid", "/tmp/wsidx-grand"] {
            std::fs::create_dir_all(d).unwrap();
        }
        // mid 已是主根（挂着 grand）。
        idx.link(Path::new("/tmp/wsidx-mid"), Path::new("/tmp/wsidx-grand"))
            .unwrap();
        // 再把 mid 挂到 root 下就成了两层链。
        let err = idx
            .link(Path::new("/tmp/wsidx-root"), Path::new("/tmp/wsidx-mid"))
            .unwrap_err();
        assert!(err.0.contains("already"), "unexpected: {}", err.0);
        // 拒绝必须是原子的：不该留下半个关联。
        assert!(idx.children(Path::new("/tmp/wsidx-root")).is_empty());
        assert_eq!(
            idx.children(Path::new("/tmp/wsidx-mid")),
            vec![canon("/tmp/wsidx-grand")]
        );
    }

    #[test]
    fn link_rejects_cycle() {
        let (_d, idx) = index();
        for d in ["/tmp/wsidx-x", "/tmp/wsidx-y"] {
            std::fs::create_dir_all(d).unwrap();
        }
        idx.link(Path::new("/tmp/wsidx-x"), Path::new("/tmp/wsidx-y"))
            .unwrap();
        // y 是附属根，当主根即链式；反过来 x 当 y 的附属根即环。
        let err = idx
            .link(Path::new("/tmp/wsidx-y"), Path::new("/tmp/wsidx-x"))
            .unwrap_err();
        assert!(
            err.0.contains("linked") || err.0.contains("already"),
            "unexpected: {}",
            err.0
        );
    }

    #[test]
    fn link_reparents_child_to_new_primary() {
        let (_d, idx) = index();
        for d in ["/tmp/wsidx-p1", "/tmp/wsidx-p2", "/tmp/wsidx-kid"] {
            std::fs::create_dir_all(d).unwrap();
        }
        idx.link(Path::new("/tmp/wsidx-p1"), Path::new("/tmp/wsidx-kid"))
            .unwrap();
        idx.link(Path::new("/tmp/wsidx-p2"), Path::new("/tmp/wsidx-kid"))
            .unwrap();
        assert!(idx.children(Path::new("/tmp/wsidx-p1")).is_empty());
        assert_eq!(
            idx.children(Path::new("/tmp/wsidx-p2")),
            vec![canon("/tmp/wsidx-kid")]
        );
    }

    #[test]
    fn unlink_is_idempotent() {
        let (_d, idx) = index();
        std::fs::create_dir_all("/tmp/wsidx-u").unwrap();
        idx.unlink(Path::new("/tmp/wsidx-u")).unwrap();
        idx.link(Path::new("/tmp/wsidx-u"), Path::new("/tmp/wsidx-u2"))
            .unwrap_err(); // u2 不存在
        idx.unlink(Path::new("/tmp/wsidx-u2")).unwrap();
    }

    #[test]
    fn legacy_file_without_links_still_reads() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("workspaces.json");
        std::fs::write(&path, br#"{"dirs":["/tmp/legacy"]}"#).unwrap();
        let idx = WorkspaceIndex::new(path);
        assert_eq!(idx.list(), vec!["/tmp/legacy".to_string()]);
        assert_eq!(idx.linked_to(Path::new("/tmp/legacy")), None);
    }

    #[test]
    fn links_persist_json_shape() {
        let (d, idx) = index();
        std::fs::create_dir_all("/tmp/wsidx-ja").unwrap();
        std::fs::create_dir_all("/tmp/wsidx-jb").unwrap();
        idx.add(Path::new("/tmp/wsidx-ja")).unwrap();
        idx.link(Path::new("/tmp/wsidx-ja"), Path::new("/tmp/wsidx-jb"))
            .unwrap();
        let raw = std::fs::read_to_string(d.path().join("workspaces.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            v["links"][canon("/tmp/wsidx-ja")][0],
            canon("/tmp/wsidx-jb")
        );
    }
}
