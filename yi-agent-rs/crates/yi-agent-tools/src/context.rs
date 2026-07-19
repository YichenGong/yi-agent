use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Shared context for all builtin tools.
/// `root` constrains FS tool operations; `cwd` persists across BashTool calls.
pub struct ToolsContext {
    root: PathBuf,
    cwd: Mutex<PathBuf>,
}

impl ToolsContext {
    pub fn new(root: PathBuf) -> Self {
        let cwd = root.clone();
        Self { root, cwd: Mutex::new(cwd) }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn cwd(&self) -> PathBuf {
        self.cwd.lock().unwrap().clone()
    }

    pub fn set_cwd(&self, path: PathBuf) {
        *self.cwd.lock().unwrap() = path;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_initializes_cwd_to_root() {
        let ctx = ToolsContext::new(PathBuf::from("/tmp/foo"));
        assert_eq!(ctx.root(), Path::new("/tmp/foo"));
        assert_eq!(ctx.cwd(), PathBuf::from("/tmp/foo"));
    }

    #[test]
    fn set_cwd_updates_cwd() {
        let ctx = ToolsContext::new(PathBuf::from("/tmp/foo"));
        ctx.set_cwd(PathBuf::from("/tmp/bar"));
        assert_eq!(ctx.cwd(), PathBuf::from("/tmp/bar"));
        // root unchanged
        assert_eq!(ctx.root(), Path::new("/tmp/foo"));
    }
}
