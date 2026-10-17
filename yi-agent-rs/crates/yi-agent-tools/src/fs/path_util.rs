use crate::error::ToolsError;
use std::path::{Path, PathBuf};

/// canonicalize 根集：能 canonicalize 的用规范路径，顺序保持（`[0]` 仍是主根），
/// 相同项去重。根不存在时回退原始路径——与 `SandboxPolicy::canonicalize_root` 同口径，
/// 保证"删除中的根"不会让所有路径解析直接报 IO 错。
pub fn canonicalize_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::with_capacity(roots.len());
    for root in roots {
        let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        if !out.contains(&canonical) {
            out.push(canonical);
        }
    }
    out
}

/// 路径是否落在根集中任一根之内。
fn contained_in_any(candidate: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| candidate.starts_with(root))
}

/// 词法兜底专用的包含检查：先找到 `normalized` 最深的**已存在**祖先并 canonicalize，
/// 再把尚未存在的尾部组件拼回去，最后做包含判定。
///
/// 动机（macOS）：`TempDir` 落在 `/var/folders/...`，而 `/var` 是指向 `/private/var`
/// 的符号链接。根集已被 `canonicalize_roots` 规范成 `/private/var/...`，但
/// "绝对路径按原样解释"的候选仍是 `/var/...`；此时纯词法前缀比较会把**根内**路径
/// 误判为逃逸。canonicalize 系统级符号链接消除这一失真。
///
/// 相比纯词法比较只会更准确、不会更宽松：拼回去的尾部组件都不含 `..`（调用方传入的是
/// `lexical_normalize` 的结果），因此不可能借由它逃逸；祖先的规范路径是操作系统的真实答案。
/// 真实逃逸仍然被拒绝——`/etc/passwd` 规范为 `/private/etc/passwd`，
/// `<primary>/../..` 规范为 `/private/var`，都不在根集内。
fn lexical_contained_in_any(normalized: &Path, roots: &[PathBuf]) -> bool {
    let mut suffix: Vec<&std::ffi::OsStr> = Vec::new();
    let mut cursor = normalized;
    loop {
        if let Ok(canonical) = cursor.canonicalize() {
            let resolved = suffix
                .iter()
                .rev()
                .fold(canonical, |acc, component| acc.join(component));
            return contained_in_any(&resolved, roots);
        }
        match (cursor.file_name(), cursor.parent()) {
            (Some(name), Some(parent)) if !parent.as_os_str().is_empty() => {
                suffix.push(name);
                cursor = parent;
            }
            // 连根都没有可 canonicalize 的祖先（或尾部是特殊组件）：退回纯词法比较。
            _ => return contained_in_any(normalized, roots),
        }
    }
}

/// 多根版 [`resolve_and_check`]：相对路径拼**主根**（`roots[0]`），
/// 包含检查要求落在**任一根**之内。
///
/// - Absolute paths are interpreted as-is (but must still be inside a root).
/// - Relative paths are joined to the primary root.
/// - Parent directories that don't exist yet cause an error (callers should
///   create them first when writing).
pub fn resolve_and_check_roots(roots: &[PathBuf], path: &str) -> Result<PathBuf, ToolsError> {
    let roots = canonicalize_roots(roots);
    let primary = roots
        .first()
        .cloned()
        .ok_or_else(|| ToolsError::PathEscapesRoot(PathBuf::from(path)))?;

    let candidate = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        primary.join(path)
    };

    // For paths that don't exist yet, canonicalize the parent.
    let (parent, file_name) = match candidate.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => (parent, candidate.file_name()),
        _ => {
            // Root path bypass: candidate.parent() is None or empty
            // (e.g. path == "/"). Canonicalize and verify containment.
            if let Ok(full_canonical) = candidate.canonicalize() {
                if !contained_in_any(&full_canonical, &roots) {
                    return Err(ToolsError::PathEscapesRoot(candidate));
                }
                return Ok(full_canonical);
            }
            return Ok(candidate);
        }
    };

    let canonical_parent = match parent.canonicalize() {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Parent doesn't exist. Before returning NotFound, check lexically
            // whether the path escapes root (catches `../` escapes even when
            // the resolved parent doesn't exist on disk).
            let normalized = lexical_normalize(&candidate);
            if !contained_in_any(&normalized, &roots) {
                return Err(ToolsError::PathEscapesRoot(candidate));
            }
            return Err(ToolsError::NotFound(candidate));
        }
        Err(e) => return Err(ToolsError::Io(e)),
    };

    if !contained_in_any(&canonical_parent, &roots) {
        return Err(ToolsError::PathEscapesRoot(candidate));
    }

    let resolved = match file_name {
        Some(name) => canonical_parent.join(name),
        None => canonical_parent,
    };

    // If the resolved path exists, verify it doesn't escape via symlink.
    // The parent-canonicalize above only checks the parent directory; a
    // symlink as the final component could still point outside the root set.
    if let Ok(full_canonical) = resolved.canonicalize() {
        if !contained_in_any(&full_canonical, &roots) {
            return Err(ToolsError::PathEscapesRoot(candidate));
        }
        return Ok(full_canonical);
    }

    Ok(resolved)
}

/// 多根版 [`resolve_for_write`]。
///
/// Like `resolve_and_check_roots` but allows the target path (and parent dirs)
/// to not exist yet. Used by WriteTool which creates parent directories as
/// needed. Security is enforced via lexical normalization: `../` escapes are
/// still rejected, and if the path *does* exist, symlink escapes are caught via
/// canonicalize.
pub fn resolve_for_write_roots(roots: &[PathBuf], path: &str) -> Result<PathBuf, ToolsError> {
    let roots = canonicalize_roots(roots);
    let primary = roots
        .first()
        .cloned()
        .ok_or_else(|| ToolsError::PathEscapesRoot(PathBuf::from(path)))?;

    let candidate = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        primary.join(path)
    };

    // If the full path already exists, canonicalize and verify containment.
    // This catches symlink escapes on the final component.
    if let Ok(full_canonical) = candidate.canonicalize() {
        if !contained_in_any(&full_canonical, &roots) {
            return Err(ToolsError::PathEscapesRoot(candidate));
        }
        return Ok(full_canonical);
    }

    // Path doesn't exist - try canonicalizing parent to catch symlink escapes
    // in intermediate directories (e.g., evil_dir -> /etc, then writing
    // evil_dir/passwd would escape via the symlinked parent).
    if let Some(parent) = candidate.parent() {
        if !parent.as_os_str().is_empty() {
            if let Ok(canonical_parent) = parent.canonicalize() {
                if !contained_in_any(&canonical_parent, &roots) {
                    return Err(ToolsError::PathEscapesRoot(candidate));
                }
                // Parent exists and is inside a root - safe to write here
                let file_name = candidate.file_name().unwrap_or_default();
                return Ok(canonical_parent.join(file_name));
            }
        }
    }

    // Parent doesn't exist either - use lexical normalization as last resort
    // to verify the path stays within the root set without touching the fs.
    // 词法兜底前先把"最深已存在祖先"的规范路径拼回来，避免 `/var` 这类系统级
    // 符号链接造成的词法前缀误判（详见 `lexical_contained_in_any`）。
    let normalized = lexical_normalize(&candidate);
    if !lexical_contained_in_any(&normalized, &roots) {
        return Err(ToolsError::PathEscapesRoot(candidate));
    }

    // 返回值与单根版有差异（brief 要求记录）：原单根 `resolve_for_write` 的最后一步是
    // `Ok(candidate)`（未归一化），多根版返回 `normalized`；多根版更严格——归一化后的路径
    // 仍须落在根集之内，因此这里的归一化不是放宽而是收紧。
    Ok(normalized)
}

/// 单根包装：等价于把 `root` 当作唯一根，供尚未迁移的调用点使用。
///
/// Resolve `path` relative to `root`, then verify the canonicalized path
/// is still inside `root`. Prevents `../` escapes and symlink escapes.
///
/// - Absolute paths are interpreted as-is (but must still be inside root).
/// - Relative paths are joined to root.
/// - Parent directories that don't exist yet cause an error (callers should
///   create them first when writing).
pub fn resolve_and_check(root: &Path, path: &str) -> Result<PathBuf, ToolsError> {
    resolve_and_check_roots(&[root.to_path_buf()], path)
}

/// 单根包装：等价于把 `root` 当作唯一根，供尚未迁移的调用点使用。
///
/// Like `resolve_and_check` but allows the target path (and parent dirs) to
/// not exist yet. Used by WriteTool which creates parent directories as needed.
pub fn resolve_for_write(root: &Path, path: &str) -> Result<PathBuf, ToolsError> {
    resolve_for_write_roots(&[root.to_path_buf()], path)
}

/// Lexically normalize a path by resolving `.` and `..` components
/// without touching the filesystem. Used as a fallback check when
/// `canonicalize()` fails because the path doesn't exist.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                // Only pop normal components; never pop past root.
                if let Some(std::path::Component::Normal(_)) = components.last() {
                    components.pop();
                }
            }
            c => components.push(c),
        }
    }
    components.iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn resolves_relative_path_inside_root() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("file.txt"), "hi").unwrap();
        let resolved = resolve_and_check(tmp.path(), "file.txt").unwrap();
        assert_eq!(
            resolved,
            tmp.path().join("file.txt").canonicalize().unwrap()
        );
    }

    #[test]
    fn resolves_nested_relative_path() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        fs::write(tmp.path().join("sub/file.txt"), "hi").unwrap();
        let resolved = resolve_and_check(tmp.path(), "sub/file.txt").unwrap();
        assert_eq!(
            resolved,
            tmp.path().join("sub/file.txt").canonicalize().unwrap()
        );
    }

    #[test]
    fn rejects_dotdot_escape() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        // Try ../../etc/passwd
        let result = resolve_and_check(tmp.path(), "sub/../../etc/passwd");
        assert!(matches!(result, Err(ToolsError::PathEscapesRoot(_))));
    }

    #[test]
    fn returns_not_found_for_missing_parent() {
        let tmp = TempDir::new().unwrap();
        let result = resolve_and_check(tmp.path(), "nonexistent/file.txt");
        assert!(matches!(result, Err(ToolsError::NotFound(_))));
    }

    #[test]
    fn rejects_absolute_path_outside_root() {
        let tmp = TempDir::new().unwrap();
        let result = resolve_and_check(tmp.path(), "/etc/passwd");
        assert!(matches!(result, Err(ToolsError::PathEscapesRoot(_))));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        let tmp = TempDir::new().unwrap();
        // Create a file outside root to target via symlink.
        let outside = TempDir::new().unwrap();
        let outside_file = outside.path().join("secret.txt");
        fs::write(&outside_file, "secret").unwrap();

        // Create a symlink inside root pointing to the outside file.
        let link_path = tmp.path().join("evil_link");
        std::os::unix::fs::symlink(&outside_file, &link_path).unwrap();

        // The symlink is inside root lexically, but canonicalizes outside.
        let result = resolve_and_check(tmp.path(), "evil_link");
        assert!(matches!(result, Err(ToolsError::PathEscapesRoot(_))));
    }

    #[test]
    fn resolve_for_write_allows_nonexistent_parent() {
        let tmp = TempDir::new().unwrap();
        let result = resolve_for_write(tmp.path(), "nonexistent/dir/file.txt");
        assert!(result.is_ok());
    }

    #[test]
    fn resolve_for_write_rejects_dotdot_escape() {
        let tmp = TempDir::new().unwrap();
        let result = resolve_for_write(tmp.path(), "sub/../../etc/passwd");
        assert!(matches!(result, Err(ToolsError::PathEscapesRoot(_))));
    }

    #[test]
    fn resolve_for_write_rejects_absolute_outside_root() {
        let tmp = TempDir::new().unwrap();
        let result = resolve_for_write(tmp.path(), "/etc/passwd");
        assert!(matches!(result, Err(ToolsError::PathEscapesRoot(_))));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_for_write_rejects_symlink_escape() {
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let outside_file = outside.path().join("secret.txt");
        fs::write(&outside_file, "secret").unwrap();

        let link_path = tmp.path().join("evil_link");
        std::os::unix::fs::symlink(&outside_file, &link_path).unwrap();

        let result = resolve_for_write(tmp.path(), "evil_link");
        assert!(matches!(result, Err(ToolsError::PathEscapesRoot(_))));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_for_write_rejects_symlink_dir_escape() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        // Create a symlink dir inside root pointing outside
        symlink(outside.path(), tmp.path().join("evil_dir")).unwrap();
        let result = resolve_for_write(tmp.path(), "evil_dir/passwd");
        assert!(matches!(result, Err(ToolsError::PathEscapesRoot(_))));
    }

    #[test]
    fn roots_resolve_relative_against_primary() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        fs::write(a.path().join("file.txt"), "hi").unwrap();
        let roots = canonicalize_roots(&[a.path().to_path_buf(), b.path().to_path_buf()]);
        let resolved = resolve_and_check_roots(&roots, "file.txt").unwrap();
        assert_eq!(resolved, a.path().join("file.txt").canonicalize().unwrap());
    }

    #[test]
    fn roots_accept_absolute_path_inside_secondary() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        fs::write(b.path().join("sib.txt"), "hi").unwrap();
        let roots = canonicalize_roots(&[a.path().to_path_buf(), b.path().to_path_buf()]);
        let target = b.path().join("sib.txt").to_string_lossy().to_string();
        let resolved = resolve_and_check_roots(&roots, &target).unwrap();
        assert_eq!(resolved, b.path().join("sib.txt").canonicalize().unwrap());
    }

    #[test]
    fn roots_reject_dotdot_escape_out_of_all_roots() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        let roots = canonicalize_roots(&[a.path().to_path_buf(), b.path().to_path_buf()]);
        let result = resolve_and_check_roots(&roots, "../escape.txt");
        assert!(matches!(result, Err(ToolsError::PathEscapesRoot(_))));
    }

    #[test]
    fn roots_reject_absolute_outside_all_roots() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        let roots = canonicalize_roots(&[a.path().to_path_buf(), b.path().to_path_buf()]);
        let result = resolve_and_check_roots(&roots, "/etc/passwd");
        assert!(matches!(result, Err(ToolsError::PathEscapesRoot(_))));
    }

    #[test]
    fn roots_reject_symlink_escape_from_secondary() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        let secret = TempDir::new().unwrap();
        fs::write(secret.path().join("secret.txt"), "no").unwrap();
        std::os::unix::fs::symlink(secret.path(), b.path().join("evil")).unwrap();
        let roots = canonicalize_roots(&[a.path().to_path_buf(), b.path().to_path_buf()]);
        let result =
            resolve_and_check_roots(&roots, &format!("{}/evil/secret.txt", b.path().display()));
        assert!(matches!(result, Err(ToolsError::PathEscapesRoot(_))));
    }

    #[test]
    fn roots_write_allows_nonexistent_parent_in_secondary() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        let roots = canonicalize_roots(&[a.path().to_path_buf(), b.path().to_path_buf()]);
        let target = b
            .path()
            .join("new/dir/file.txt")
            .to_string_lossy()
            .to_string();
        let resolved = resolve_for_write_roots(&roots, &target).unwrap();
        assert_eq!(resolved, b.path().join("new/dir/file.txt"));
    }

    #[test]
    fn roots_reject_symlinked_parent_escape_on_write() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), b.path().join("link")).unwrap();
        let roots = canonicalize_roots(&[a.path().to_path_buf(), b.path().to_path_buf()]);
        let target = format!("{}/link/passwd", b.path().display());
        let result = resolve_for_write_roots(&roots, &target);
        assert!(matches!(result, Err(ToolsError::PathEscapesRoot(_))));
    }

    #[test]
    fn empty_roots_is_an_error() {
        let result = resolve_and_check_roots(&[], "file.txt");
        assert!(result.is_err());
    }

    #[test]
    fn roots_write_rejects_symlink_escape_from_secondary() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        let secret = TempDir::new().unwrap();
        fs::write(secret.path().join("secret.txt"), "no").unwrap();
        // 终组件符号链接：写在次根内，指向根外。
        std::os::unix::fs::symlink(secret.path().join("secret.txt"), b.path().join("evil.txt"))
            .unwrap();
        let roots = canonicalize_roots(&[a.path().to_path_buf(), b.path().to_path_buf()]);
        let target = b.path().join("evil.txt").to_string_lossy().to_string();
        let result = resolve_for_write_roots(&roots, &target);
        assert!(matches!(result, Err(ToolsError::PathEscapesRoot(_))));
    }

    #[test]
    fn empty_roots_is_an_error_for_write() {
        let result = resolve_for_write_roots(&[], "file.txt");
        assert!(result.is_err());
    }
}
