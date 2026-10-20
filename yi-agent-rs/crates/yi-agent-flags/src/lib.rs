//! 功能开关：让功能先合入、后开启。

pub mod error;
pub mod registry;
pub mod store;

pub use error::FlagError;
pub use registry::{FlagDef, REGISTRY, find};
pub use store::{FlagView, Source, all_at, is_enabled, status, status_at, write_at};

/// 当前构建类型：`release` / `dev` / `nogit`。
///
/// 必须由 binary 在 `main` 启动时用 [`set_build_kind`] 注入——`build.rs` 的
/// `rustc-env` 只作用于 `yi-agent` 这个包，别的 crate 用 `option_env!` 读不到。
/// 没注入就按 `release` 处理：宁可开关全部失效，也不让没验证的功能漏给用户。
static BUILD_KIND: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
static VERSION: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();

pub fn set_build_info(kind: &'static str, version: &'static str) {
    let _ = BUILD_KIND.set(kind);
    let _ = VERSION.set(version);
}

pub fn build_kind() -> &'static str {
    BUILD_KIND.get().copied().unwrap_or("release")
}

/// 完整版本号（dev 构建带 `-dev+<sha>`）。未注入时退回本 crate 的 `Cargo.toml` 版本。
pub fn version() -> &'static str {
    VERSION.get().copied().unwrap_or(env!("CARGO_PKG_VERSION"))
}

/// 今天的日期，`YYYY-MM-DD`。
///
/// 测试可用 `YI_FLAGS_TODAY` 固定时间，避免「跨零点时测试忽然失败」。
pub fn today() -> String {
    if let Ok(fixed) = std::env::var("YI_FLAGS_TODAY") {
        if !fixed.is_empty() {
            return fixed;
        }
    }
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_civil(secs)
}

/// Unix 秒 → `YYYY-MM-DD`（UTC）。用 Howard Hinnant 的 civil_from_days 算法，
/// 避免为一行日期引入 chrono。
fn format_civil(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn today_is_iso_formatted() {
        let today = today();
        assert_eq!(today.len(), 10);
        assert_eq!(&today[4..5], "-");
        assert_eq!(&today[7..8], "-");
    }

    #[test]
    fn an_uninjected_build_kind_is_fail_closed() {
        // 本 crate 的测试从不注入构建信息，所以这里读到的就是缺省值。
        assert_eq!(build_kind(), "release");
        assert!(!version().is_empty());
    }

    #[test]
    fn a_fixed_date_wins() {
        // 环境变量是全局的，测试里只读不写；此处验证解析逻辑本身。
        assert_eq!(format_civil(0), "1970-01-01");
        assert_eq!(format_civil(1_700_000_000), "2023-11-14");
    }
}
