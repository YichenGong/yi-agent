use std::path::Path;

use include_dir::{Dir, include_dir};
use sha2::{Digest, Sha256};

static ASSETS_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/src/assets");

/// Install bundled system skills into `cache_root`.
///
/// For each skill under `src/assets/`, writes its SKILL.md (and any other files)
/// to `<cache_root>/<skill-name>/`. If the target file already exists and has
/// the same content (by SHA-256), it is skipped. Otherwise it is overwritten.
///
/// Errors are returned; the caller should log and continue on failure.
pub fn install_system_skills(cache_root: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(cache_root)?;

    for entry in ASSETS_DIR.dirs() {
        let name = entry
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        let target_dir = cache_root.join(&name);
        std::fs::create_dir_all(&target_dir)?;
        write_dir_recursive(entry, &target_dir)?;
    }
    Ok(())
}

fn write_dir_recursive(dir: &Dir, target: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(target)?;
    for file in dir.files() {
        // file.path() is relative to the include root (e.g. "skill-creator/SKILL.md"),
        // so use just the file name to avoid double-nesting.
        let file_name = file.path().file_name().unwrap();
        let dest = target.join(file_name);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = file.contents();
        if let Ok(existing) = std::fs::read(&dest) {
            if hash(&existing) == hash(content) {
                continue; // same content, skip
            }
        }
        std::fs::write(&dest, content)?;
    }
    for sub in dir.dirs() {
        let sub_name = sub
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        write_dir_recursive(sub, &target.join(sub_name))?;
    }
    Ok(())
}

fn hash(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn installs_to_empty_dir() {
        let tmp = TempDir::new().unwrap();
        install_system_skills(tmp.path()).unwrap();
        assert!(tmp.path().join("skill-creator/SKILL.md").is_file());
        assert!(tmp.path().join("skill-installer/SKILL.md").is_file());
    }

    #[test]
    fn skips_unchanged_content() {
        let tmp = TempDir::new().unwrap();
        install_system_skills(tmp.path()).unwrap();
        let path = tmp.path().join("skill-creator/SKILL.md");
        let mtime1 = std::fs::metadata(&path).unwrap().modified().unwrap();
        // Run again; should skip writing
        install_system_skills(tmp.path()).unwrap();
        let mtime2 = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(mtime1, mtime2);
    }

    #[test]
    fn overwrites_changed_content() {
        let tmp = TempDir::new().unwrap();
        install_system_skills(tmp.path()).unwrap();
        let path = tmp.path().join("skill-creator/SKILL.md");
        // Modify it
        std::fs::write(&path, "modified content").unwrap();
        // Run again; should overwrite
        install_system_skills(tmp.path()).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("Skill Creator")); // back to bundled content
    }

    #[test]
    fn creates_target_dir_if_missing() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("nested/deep");
        install_system_skills(&target).unwrap();
        assert!(target.join("skill-creator/SKILL.md").is_file());
    }
}
