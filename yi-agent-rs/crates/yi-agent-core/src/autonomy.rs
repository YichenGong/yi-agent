//! 运行时可翻转的 yolo 开关,由权限层与沙箱层共享。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

// SeqCst: 与代码库其它标志位保持一致;这是安全敏感闸门,刻意用最强序,勿降级为 Relaxed。
/// 单一事实来源:permission 与 sandbox 读同一个原子标志。
#[derive(Clone, Debug)]
pub struct YoloSwitch(Arc<AtomicBool>);

impl YoloSwitch {
    pub fn new(on: bool) -> Self {
        Self(Arc::new(AtomicBool::new(on)))
    }

    /// 读取当前开关值;读到的是最近一次 `set` 的结果,非阻塞。
    pub fn get(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// 写入开关值;所有共享同一开关的克隆都会立即看到新值。
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

    #[test]
    fn new_true_starts_on() {
        let s = YoloSwitch::new(true);
        assert!(s.get()); // 构造为 true 时初始即开启
    }

    #[test]
    fn distinct_instances_are_independent() {
        let a = YoloSwitch::new(false);
        let b = YoloSwitch::new(false);
        a.set(true);
        assert!(!b.get()); // 不同实例互不影响
    }
}
