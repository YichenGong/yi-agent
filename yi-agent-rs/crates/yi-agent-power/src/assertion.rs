//! 平台电源断言。
pub trait Assertion: Send {
    fn acquire(&mut self) -> bool;
    fn release(&mut self);
    fn is_held(&self) -> bool;
}

pub fn platform_assertion(label: &str) -> Box<dyn Assertion> {
    #[cfg(target_os = "macos")]
    {
        Box::new(MacAssertion::new(label))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = label;
        Box::new(NoopAssertion)
    }
}

pub struct NoopAssertion;

impl Assertion for NoopAssertion {
    fn acquire(&mut self) -> bool {
        true
    }
    fn release(&mut self) {}
    fn is_held(&self) -> bool {
        false
    }
}

#[cfg(target_os = "macos")]
pub use mac::MacAssertion;

#[cfg(target_os = "macos")]
mod mac {
    use super::Assertion;
    use std::ffi::{CString, c_char, c_void};

    type CFStringRef = *const c_void;
    type CFTypeRef = *const c_void;
    type CFAllocatorRef = *const c_void;
    type IOPMAssertionID = u32;
    type IOReturn = i32;

    const UTF8: u32 = 0x0800_0100;
    const LEVEL_ON: u32 = 255;
    const SUCCESS: IOReturn = 0;

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(
            alloc: CFAllocatorRef,
            s: *const c_char,
            encoding: u32,
        ) -> CFStringRef;
        fn CFRelease(cf: CFTypeRef);
    }

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(
            ty: CFStringRef,
            level: u32,
            name: CFStringRef,
            id: *mut IOPMAssertionID,
        ) -> IOReturn;
        fn IOPMAssertionRelease(id: IOPMAssertionID) -> IOReturn;
    }

    fn cfstring(text: &str) -> CFStringRef {
        let c = CString::new(text).unwrap_or_default();
        // SAFETY: `c` 是有效 NUL 结尾字符串，allocator 传 null 用默认分配器。
        unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), UTF8) }
    }

    pub struct MacAssertion {
        ty: CFStringRef,
        name: CFStringRef,
        id: IOPMAssertionID,
        held: bool,
    }

    // SAFETY: 字段只是 CF 句柄与整数；FFI 调用在 `&mut self` 下串行。
    unsafe impl Send for MacAssertion {}

    impl MacAssertion {
        pub fn new(label: &str) -> Self {
            Self {
                ty: cfstring("PreventUserIdleSystemSleep"),
                name: cfstring(label),
                id: 0,
                held: false,
            }
        }
    }

    impl Assertion for MacAssertion {
        fn acquire(&mut self) -> bool {
            let mut id: IOPMAssertionID = 0;
            // SAFETY: 两个 CFString 在 `new` 里创建；`&mut id` 指向本地可写变量。
            let rc = unsafe { IOPMAssertionCreateWithName(self.ty, LEVEL_ON, self.name, &mut id) };
            if rc == SUCCESS {
                self.id = id;
                self.held = true;
                true
            } else {
                self.held = false;
                false
            }
        }

        fn release(&mut self) {
            if !self.held {
                return;
            }
            // SAFETY: `self.id` 是 acquire 成功后由 IOKit 写入的有效 id，held 守卫保证只释放一次。
            let _ = unsafe { IOPMAssertionRelease(self.id) };
            self.held = false;
            self.id = 0;
        }

        fn is_held(&self) -> bool {
            self.held
        }
    }

    impl Drop for MacAssertion {
        fn drop(&mut self) {
            self.release();
            // SAFETY: 两个 CFString 由 Create 规则创建（+1），这里各 release 一次。
            unsafe {
                CFRelease(self.ty);
                CFRelease(self.name);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeAssertion {
        held: bool,
        acquires: usize,
        releases: usize,
        fail_next: bool,
    }

    impl FakeAssertion {
        fn new() -> Self {
            Self {
                held: false,
                acquires: 0,
                releases: 0,
                fail_next: false,
            }
        }
    }

    impl Assertion for FakeAssertion {
        fn acquire(&mut self) -> bool {
            self.acquires += 1;
            if self.fail_next {
                self.fail_next = false;
                self.held = false;
                return false;
            }
            self.held = true;
            true
        }
        fn release(&mut self) {
            self.releases += 1;
            self.held = false;
        }
        fn is_held(&self) -> bool {
            self.held
        }
    }

    #[test]
    fn acquire_then_release_flips_state() {
        let mut a = FakeAssertion::new();
        assert!(!a.is_held());
        assert!(a.acquire());
        assert!(a.is_held());
        a.release();
        assert!(!a.is_held());
    }

    #[test]
    fn release_is_idempotent_when_guarded_by_is_held() {
        let mut a = FakeAssertion::new();
        assert!(a.acquire());
        if a.is_held() {
            a.release();
        }
        if a.is_held() {
            a.release();
        }
        assert_eq!(a.releases, 1);
    }

    #[test]
    fn acquire_after_release_works_again() {
        let mut a = FakeAssertion::new();
        assert!(a.acquire());
        a.release();
        assert!(a.acquire());
        assert!(a.is_held());
        assert_eq!(a.acquires, 2);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_real_assertion_can_be_acquired_and_released() {
        let mut a = platform_assertion("yi-agent: smoke test");
        assert!(
            a.acquire(),
            "IOPMAssertionCreateWithName must return kIOReturnSuccess"
        );
        assert!(a.is_held());
        let listing = std::process::Command::new("pmset")
            .args(["-g", "assertions"])
            .output()
            .expect("pmset must run");
        let text = String::from_utf8_lossy(&listing.stdout);
        assert!(
            text.contains("yi-agent: smoke test"),
            "assertion must be named after our label"
        );
        a.release();
        assert!(!a.is_held());
    }
}
