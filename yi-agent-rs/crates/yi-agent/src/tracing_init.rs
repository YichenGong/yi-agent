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
pub fn init() -> tracing_appender::non_blocking::WorkerGuard {
    let trace_dir = trace_dir();
    let _ = std::fs::create_dir_all(&trace_dir);

    let filename = format!(
        "session-{}.jsonl",
        chrono_local_timestamp()
    );
    let filepath = trace_dir.join(filename);

    let file: File = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&filepath)
        .unwrap_or_else(|e| {
            eprintln!("warning: failed to open trace file {}: {e}", filepath.display());
            File::create(std::path::Path::new("/dev/null")).unwrap()
        });

    let (file_writer, guard) = tracing_appender::non_blocking(file);

    let file_layer = tracing_subscriber::fmt::layer()
        .json()
        .with_writer(file_writer)
        .with_filter(EnvFilter::new("info"));

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
    let doe = (z - era * 146096) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}
