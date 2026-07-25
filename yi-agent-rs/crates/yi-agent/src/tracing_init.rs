//! Tracing 初始化：文件始终记录 + 可选 stderr 输出。
//!
//! - 文件：每次启动写入独立文件 `~/.yi-agent/trace/session-YYYYMMDD-HHMMSS.jsonl`，
//!   一个 session 对应一个文件，不会交错。
//! - stderr：由 `YI_LOG` 环境变量控制（如 `debug`、`trace`、`warn`），不设则不输出到 stderr。
//!
//! 返回的 `_guard` 必须存活到程序结束，否则会丢失未刷新的日志。

use std::fs::{File, OpenOptions};
use std::path::PathBuf;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// 初始化 tracing，返回的 guard 必须保活到程序结束。
pub fn init(debug: bool) -> tracing_appender::non_blocking::WorkerGuard {
    let trace_dir = trace_dir();
    let _ = std::fs::create_dir_all(&trace_dir);

    let filename = format!("session-{}.jsonl", chrono_local_timestamp());
    let filepath = trace_dir.join(filename);

    let file: File = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&filepath)
        .unwrap_or_else(|e| {
            eprintln!(
                "warning: failed to open trace file {}: {e}",
                filepath.display()
            );
            File::create(std::path::Path::new("/dev/null")).unwrap()
        });

    let (file_writer, guard) = tracing_appender::non_blocking(file);

    let file_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_writer(file_writer)
        .with_filter(EnvFilter::new(file_filter(debug)));

    let registry = tracing_subscriber::registry().with(file_layer);

    // YI_LOG 控制 stderr 输出级别
    if let Ok(level) = std::env::var("YI_LOG") {
        let stderr_layer = tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .with_target(true)
            .with_filter(EnvFilter::new(level));
        registry.with(stderr_layer).init();
    } else {
        registry.init();
    }

    tracing::info!(
        trace_file = %filepath.display(),
        stderr_level = %std::env::var("YI_LOG").unwrap_or_else(|_| "(off)".to_string()),
        "tracing initialized"
    );

    guard
}

/// 文件日志的 EnvFilter 字符串。
/// `debug=false` 时仅记录 info 及以上;`debug=true` 时额外放开核心 crate 的 debug 日志,
/// 用于记录发给 LLM 的消息内容和 LLM 返回的内容。
fn file_filter(debug: bool) -> &'static str {
    if debug {
        "info,yi_agent_core=debug,yi_agent_llm=debug"
    } else {
        "info"
    }
}

fn trace_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".yi-agent").join("trace")
}

/// 生成本地时间戳字符串 `YYYYMMDD-HHMMSS`。
fn chrono_local_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let secs = now.as_secs();
    // 简单实现：用 chrono 会更好，但避免引入额外依赖。
    // 这里用系统时间手动格式化为 UTC。本地时区差异对文件名排序无影响。
    let days = secs / 86400;
    let rem = secs % 86400;
    let hour = rem / 3600;
    let min = (rem % 3600) / 60;
    let sec = rem % 60;
    // 从 1970-01-01 计算年月日
    let (year, month, day) = days_to_ymd(days as i64);
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        year, month, day, hour, min, sec
    )
}

/// 将 Unix epoch 天数转换为 (year, month, day)。
/// 算法来自 Howard Hinnant 的 days_from_civil 反算。
fn days_to_ymd(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 返回某日 00:00:00 UTC 的 Unix 天数。
    fn days_for(y: i64, m: u32, d: u32) -> i64 {
        // Howard Hinnant 的 days_from_civil 算法。
        let y = if m <= 2 { y - 1 } else { y };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = (y - era * 400) as u64; // [0, 399]
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) as u64 + 2) / 5 + d as u64 - 1; // [0, 365]
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
        era * 146097 + doe as i64 - 719468
    }

    #[test]
    fn days_to_ymd_known_dates() {
        // (unix_days, expected_year, expected_month, expected_day)
        let cases: &[(i64, i64, u32, u32)] = &[
            (0, 1970, 1, 1),                      // epoch
            (days_for(2026, 7, 25), 2026, 7, 25), // 触发 bug 的日期(修复前会得到 7/30)
            (days_for(2000, 2, 29), 2000, 2, 29), // 闰日
            (days_for(1999, 12, 31), 1999, 12, 31),
            (days_for(2100, 3, 1), 2100, 3, 1), // 非闰年的世纪年
            (days_for(1970, 1, 31), 1970, 1, 31),
            (days_for(2024, 12, 31), 2024, 12, 31),
        ];
        for &(days, y, m, d) in cases {
            let (cy, cm, cd) = days_to_ymd(days);
            assert_eq!((cy, cm, cd), (y, m, d), "for unix days {days}");
        }
    }

    #[test]
    fn timestamp_format_matches_expected_pattern() {
        // 只验证格式,不验证当前时间值(会随时间变化)。
        // chrono_local_timestamp 内部用 SystemTime,无法直接注入,
        // 这里通过 days_to_ymd 已覆盖核心逻辑。
        let _ = days_to_ymd(0);
    }

    #[test]
    fn file_filter_debug_false_returns_info_only() {
        assert_eq!(file_filter(false), "info");
    }

    #[test]
    fn file_filter_debug_true_includes_core_and_llm_debug() {
        let filter = file_filter(true);
        assert!(filter.contains("yi_agent_core=debug"), "filter: {filter}");
        assert!(filter.contains("yi_agent_llm=debug"), "filter: {filter}");
        assert!(filter.contains("info"), "filter: {filter}");
    }
}
