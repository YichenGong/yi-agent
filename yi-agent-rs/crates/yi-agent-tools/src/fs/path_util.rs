use std::path::{Path, PathBuf};
use crate::error::ToolsError;

/// Resolve `path` relative to `root`, then verify the canonicalized path
/// is still inside `root`. Prevents `../` escapes.
///
/// - Absolute paths are interpreted as-is (but must still be inside root).
/// - Relative paths are joined to root.
/// - Parent directories that don't exist yet cause an error (callers should
///   create them first when writing).
pub fn resolve_and_check(root: &Path, path: &str) -> Result<PathBuf, ToolsError> {
    let canonical_root = root.canonicalize().map_err(ToolsError::Io)?;

    let candidate = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        canonical_root.join(path)
    };

    // For paths that don't exist yet, canonicalize the parent.
    let (parent, file_name) = match candidate.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => (parent, candidate.file_name()),
        _ => return Ok(candidate),
    };

    let canonical_parent = match parent.canonicalize() {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Parent doesn't exist. Before returning NotFound, check lexically
            // whether the path escapes root (catches `../` escapes even when
            // the resolved parent doesn't exist on disk).
            let normalized = lexical_normalize(&candidate);
            if !normalized.starts_with(&canonical_root) {
                return Err(ToolsError::PathEscapesRoot(candidate));
            }
            return Err(ToolsError::NotFound(candidate));
        }
        Err(e) => return Err(ToolsError::Io(e)),
    };

    if !canonical_parent.starts_with(&canonical_root) {
        return Err(ToolsError::PathEscapesRoot(candidate));
    }

    let resolved = match file_name {
        Some(name) => canonical_parent.join(name),
        None => canonical_parent,
    };

    Ok(resolved)
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
                match components.last() {
                    Some(std::path::Component::Normal(_)) => {
                        components.pop();
                    }
                    _ => {}
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
        assert_eq!(resolved, tmp.path().join("file.txt").canonicalize().unwrap());
    }

    #[test]
    fn resolves_nested_relative_path() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        fs::write(tmp.path().join("sub/file.txt"), "hi").unwrap();
        let resolved = resolve_and_check(tmp.path(), "sub/file.txt").unwrap();
        assert_eq!(resolved, tmp.path().join("sub/file.txt").canonicalize().unwrap());
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
}
