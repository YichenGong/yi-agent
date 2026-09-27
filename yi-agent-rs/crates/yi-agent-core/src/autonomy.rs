//! 运行时可翻转的 yolo 开关,由权限层与沙箱层共享。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// 单一事实来源:permission 与 sandbox 读同一个原子标志。
#[derive(Clone, Debug)]
pub struct YoloSwitch(Arc<AtomicBool>);

impl YoloSwitch {
    pub fn new(on: bool) -> Self {
        Self(Arc::new(AtomicBool::new(on)))
    }

    pub fn get(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub fn set(&self, on: bool) {
        self.0.store(on, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_and_flip() {
        let s = YoloSwitch::new(false);
        assert!(!s.get());
        s.set(true);
        assert!(s.get());
        s.set(false);
        assert!(!s.get());
    }

    #[test]
    fn clones_share_one_flag() {
        let a = YoloSwitch::new(false);
        let b = a.clone();
        a.set(true);
        assert!(b.get()); // 克隆共享同一开关
    }
}
