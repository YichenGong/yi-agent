use chrono::{DateTime, Local, NaiveTime, Weekday};
use serde::Deserialize;

/// 保守默认：配置缺失或损坏时使用。
pub const DEFAULT_MAX_TASKS: u16 = 3;

/// 推进循环的默认睡眠周期（秒）。与 supervisor 清单里历史硬编码的 10 保持一致。
pub const DEFAULT_INTERVAL_SECS: u64 = 10;

/// interval_secs 的合法上界：一小时。
const MAX_INTERVAL_SECS: u64 = 3600;

/// 设置写路径的校验失败原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsError(String);

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid kanban settings: {}", self.0)
    }
}

impl std::error::Error for SettingsError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CalendarError {
    Toml(String),
    Time(String),
    Day(String),
}

impl std::fmt::Display for CalendarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CalendarError::Toml(message) => write!(f, "invalid kanban.toml: {message}"),
            CalendarError::Time(message) => write!(f, "invalid time: {message}"),
            CalendarError::Day(message) => write!(f, "invalid day: {message}"),
        }
    }
}

impl std::error::Error for CalendarError {}

/// 一个并发窗口。区间为左闭右开；`all_day` 为真时忽略 `start`/`end`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConcurrencyWindow {
    pub days: Vec<Weekday>,
    pub start: NaiveTime,
    pub end: NaiveTime,
    pub all_day: bool,
    pub max_tasks: u16,
}

impl ConcurrencyWindow {
    fn matches(&self, now: DateTime<Local>) -> bool {
        use chrono::{Datelike, Timelike};
        if !self.days.contains(&now.weekday()) {
            return false;
        }
        if self.all_day {
            return true;
        }
        let time =
            NaiveTime::from_hms_opt(now.hour(), now.minute(), 0).expect("valid time components");
        // 左闭右开；end 用 "24:00" 时归一为次日 00:00，等价于到 23:59:59。
        if self.end == NaiveTime::MIN {
            return time >= self.start;
        }
        time >= self.start && time < self.end
    }
}

/// 时段并发日历。`limit_at` 返回给定时刻允许的**任务级**并发上限。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConcurrencyCalendar {
    pub default_max_tasks: u16,
    pub interval_secs: u64,
    pub windows: Vec<ConcurrencyWindow>,
}

impl Default for ConcurrencyCalendar {
    fn default() -> Self {
        Self {
            default_max_tasks: DEFAULT_MAX_TASKS,
            interval_secs: DEFAULT_INTERVAL_SECS,
            windows: Vec::new(),
        }
    }
}

impl ConcurrencyCalendar {
    /// 命中第一个匹配窗口；无命中取 `default_max_tasks`。
    pub fn limit_at(&self, now: DateTime<Local>) -> u16 {
        self.windows
            .iter()
            .find(|window| window.matches(now))
            .map(|window| window.max_tasks)
            .unwrap_or(self.default_max_tasks)
    }

    /// 解析 TOML 配置。
    pub fn from_toml(input: &str) -> Result<Self, CalendarError> {
        let file: CalendarFile =
            toml::from_str(input).map_err(|error| CalendarError::Toml(error.to_string()))?;
        let windows = file
            .window
            .into_iter()
            .map(RawWindow::into_window)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            default_max_tasks: file.default_max_tasks.unwrap_or(DEFAULT_MAX_TASKS),
            interval_secs: file.interval_secs.unwrap_or(DEFAULT_INTERVAL_SECS),
            windows,
        })
    }

    /// 读取配置；文件缺失、不可读或损坏一律回退默认，绝不 panic。
    /// 新名优先、旧名回退、都无则默认。
    ///
    /// `superpowers-kanban.toml` 是迁移后的名字；`kanban.toml` 是旧名，
    /// 仅在旧文件存在且新文件不存在时读取。**从不修改或删除旧文件。**
    pub fn load_preferring_new(state_dir: &std::path::Path) -> Self {
        let new = state_dir.join("superpowers-kanban.toml");
        if new.is_file() {
            return Self::load_or_default(&new);
        }
        let legacy = state_dir.join("kanban.toml");
        if legacy.is_file() {
            return Self::load_or_default(&legacy);
        }
        Self::default()
    }

    pub fn load_or_default(path: &std::path::Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_toml(&text).unwrap_or_else(|error| {
                tracing_fallback(&error, path);
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    /// 按规范形式渲染成 TOML。窗口的 `days` 压缩成 `Mon-Fri` 这类简写；
    /// 语义等价的往返才是目标，不追求与原文件逐字节相同。
    pub fn to_toml(&self) -> String {
        let mut out = format!(
            "default_max_tasks = {}\ninterval_secs = {}\n",
            self.default_max_tasks, self.interval_secs
        );
        for window in &self.windows {
            out.push_str("\n[[window]]\n");
            out.push_str(&format!("days = \"{}\"\n", render_days(&window.days)));
            if window.all_day {
                out.push_str("all_day = true\n");
            } else {
                out.push_str(&format!("start = \"{}\"\n", render_time(window.start)));
                out.push_str(&format!("end = \"{}\"\n", render_time(window.end)));
            }
            out.push_str(&format!("max_tasks = {}\n", window.max_tasks));
        }
        out
    }

    /// 面向 UI / RPC 的设置载荷。
    pub fn settings_json(&self) -> serde_json::Value {
        let windows: Vec<serde_json::Value> = self
            .windows
            .iter()
            .map(|window| {
                serde_json::json!({
                    "days": render_days(&window.days),
                    "start": render_time(window.start),
                    "end": render_time(window.end),
                    "all_day": window.all_day,
                    "max_tasks": window.max_tasks,
                })
            })
            .collect();
        serde_json::json!({
            "default_max_tasks": self.default_max_tasks,
            "interval_secs": self.interval_secs,
            "windows": windows,
        })
    }

    /// 从设置载荷构建并校验。任何非法字段都在此拒绝——调用方据此保证零落盘。
    pub fn from_settings_json(value: &serde_json::Value) -> Result<Self, SettingsError> {
        let default_max_tasks = value
            .get("default_max_tasks")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| SettingsError("default_max_tasks must be a positive integer".into()))?;
        if default_max_tasks < 1 {
            return Err(SettingsError("default_max_tasks must be >= 1".into()));
        }
        let interval_secs = value
            .get("interval_secs")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| SettingsError("interval_secs must be an integer".into()))?;
        if !(1..=MAX_INTERVAL_SECS).contains(&interval_secs) {
            return Err(SettingsError(format!(
                "interval_secs must be in [1, {MAX_INTERVAL_SECS}], got {interval_secs}"
            )));
        }
        let raw_windows = value
            .get("windows")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| SettingsError("windows must be an array".into()))?;
        let mut windows = Vec::with_capacity(raw_windows.len());
        for (index, raw) in raw_windows.iter().enumerate() {
            windows.push(
                decode_window(raw)
                    .map_err(|error| SettingsError(format!("window #{index}: {error}")))?,
            );
        }
        Ok(Self {
            default_max_tasks: default_max_tasks as u16,
            interval_secs,
            windows,
        })
    }

    /// 原子写：临时文件 + rename。只写新名 `superpowers-kanban.toml`。
    pub fn save(&self, state_dir: &std::path::Path) -> std::io::Result<()> {
        std::fs::create_dir_all(state_dir)?;
        let path = state_dir.join("superpowers-kanban.toml");
        let tmp = state_dir.join(format!(
            "superpowers-kanban.toml.{}.tmp",
            std::process::id()
        ));
        std::fs::write(&tmp, self.to_toml())?;
        std::fs::rename(&tmp, &path)
    }
}

fn tracing_fallback(error: &CalendarError, path: &std::path::Path) {
    eprintln!(
        "superpowers board: {error}; falling back to default_max_tasks={DEFAULT_MAX_TASKS} ({})",
        path.display()
    );
}

#[derive(Debug, Deserialize)]
struct CalendarFile {
    default_max_tasks: Option<u16>,
    interval_secs: Option<u64>,
    #[serde(default)]
    window: Vec<RawWindow>,
}

#[derive(Debug, Deserialize)]
struct RawWindow {
    days: Option<String>,
    start: Option<String>,
    end: Option<String>,
    #[serde(default)]
    all_day: bool,
    max_tasks: u16,
}

impl RawWindow {
    fn into_window(self) -> Result<ConcurrencyWindow, CalendarError> {
        let all_day = self.all_day;
        let days = self
            .days
            .as_deref()
            .map(parse_days)
            .transpose()?
            .unwrap_or_else(|| {
                // 未声明 days 视为每天。
                vec![
                    Weekday::Mon,
                    Weekday::Tue,
                    Weekday::Wed,
                    Weekday::Thu,
                    Weekday::Fri,
                    Weekday::Sat,
                    Weekday::Sun,
                ]
            });
        let (start, end) = if all_day {
            (NaiveTime::MIN, NaiveTime::MIN)
        } else {
            (
                parse_time(self.start.as_deref().unwrap_or("00:00"))?,
                parse_time(self.end.as_deref().unwrap_or("24:00"))?,
            )
        };
        Ok(ConcurrencyWindow {
            days,
            start,
            end,
            all_day,
            max_tasks: self.max_tasks,
        })
    }
}

/// "Mon-Fri,Sun" 形式：逗号分隔，每段可为 `X`、`X-Y` 或 `X..Y` 之外的简写。
fn parse_days(input: &str) -> Result<Vec<Weekday>, CalendarError> {
    let order = [
        Weekday::Mon,
        Weekday::Tue,
        Weekday::Wed,
        Weekday::Thu,
        Weekday::Fri,
        Weekday::Sat,
        Weekday::Sun,
    ];
    let mut indices = std::collections::BTreeSet::new();
    for part in input.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        if let Some((from, to)) = part.split_once('-') {
            let from = weekday_index(from.trim())?;
            let to = weekday_index(to.trim())?;
            if from > to {
                return Err(CalendarError::Day(format!("reversed range: {part}")));
            }
            indices.extend(from..=to);
        } else {
            indices.insert(weekday_index(part)?);
        }
    }
    if indices.is_empty() {
        return Err(CalendarError::Day("no days given".into()));
    }
    Ok(indices.into_iter().map(|index| order[index]).collect())
}

fn weekday_index(name: &str) -> Result<usize, CalendarError> {
    match name.to_ascii_lowercase().as_str() {
        "mon" | "monday" => Ok(0),
        "tue" | "tuesday" => Ok(1),
        "wed" | "wednesday" => Ok(2),
        "thu" | "thursday" => Ok(3),
        "fri" | "friday" => Ok(4),
        "sat" | "saturday" => Ok(5),
        "sun" | "sunday" => Ok(6),
        other => Err(CalendarError::Day(other.to_string())),
    }
}

/// 解析 `HH:MM`；"24:00" 归一为 `NaiveTime::MIN`（表示当日末尾）。
fn parse_time(input: &str) -> Result<NaiveTime, CalendarError> {
    if input.trim() == "24:00" {
        return Ok(NaiveTime::MIN);
    }
    NaiveTime::parse_from_str(input.trim(), "%H:%M")
        .map_err(|error| CalendarError::Time(format!("{input}: {error}")))
}

/// `Vec<Weekday>` 压成 `Mon-Fri` 形式的简写。
fn render_days(days: &[Weekday]) -> String {
    use Weekday::*;
    let order = [Mon, Tue, Wed, Thu, Fri, Sat, Sun];
    let names = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let present: Vec<bool> = order.iter().map(|day| days.contains(day)).collect();
    let mut parts = Vec::new();
    let mut i = 0;
    while i < 7 {
        if !present[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i + 1 < 7 && present[i + 1] {
            i += 1;
        }
        if i == start {
            parts.push(names[start].to_string());
        } else {
            parts.push(format!("{}-{}", names[start], names[i]));
        }
        i += 1;
    }
    parts.join(",")
}

/// 反渲染 `HH:MM`；当日末尾（`NaiveTime::MIN`）写回 `24:00`。
fn render_time(time: NaiveTime) -> String {
    if time == NaiveTime::MIN {
        return "24:00".to_string();
    }
    time.format("%H:%M").to_string()
}

/// 解一个 `windows` 数组元素；非法即报错。
fn decode_window(value: &serde_json::Value) -> Result<ConcurrencyWindow, String> {
    let all_day = value
        .get("all_day")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let raw = RawWindow {
        days: value
            .get("days")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        start: value
            .get("start")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        end: value
            .get("end")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        all_day,
        max_tasks: value
            .get("max_tasks")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| "max_tasks must be a positive integer".to_string())?
            as u16,
    };
    if raw.max_tasks < 1 {
        return Err("max_tasks must be >= 1".into());
    }
    let window = raw.into_window().map_err(|error| error.to_string())?;
    // `NaiveTime::MIN` 是 "24:00" 的归一形式（当日末尾），恒晚于任何 start。
    if !window.all_day && window.end != NaiveTime::MIN && window.end <= window.start {
        return Err("start must be before end".into());
    }
    Ok(window)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Timelike};
    use std::path::Path;

    fn at(month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, month, day, hour, minute, 0)
            .single()
            .expect("valid local time")
    }

    /// 2026-10-01 是周四，2026-10-03 是周六。
    const SPEC_CONFIG: &str = r#"
default_max_tasks = 3

[[window]]
days = "Mon-Fri"
start = "09:00"
end = "24:00"
max_tasks = 3

[[window]]
days = "Mon-Fri"
start = "00:00"
end = "09:00"
max_tasks = 10

[[window]]
days = "Sat,Sun"
all_day = true
max_tasks = 10
"#;

    #[test]
    fn workday_daytime_is_three() {
        let calendar = ConcurrencyCalendar::from_toml(SPEC_CONFIG).unwrap();
        assert_eq!(calendar.limit_at(at(10, 1, 9, 0)), 3);
        assert_eq!(calendar.limit_at(at(10, 1, 12, 30)), 3);
        assert_eq!(calendar.limit_at(at(10, 1, 23, 59)), 3);
    }

    #[test]
    fn workday_early_morning_is_ten() {
        let calendar = ConcurrencyCalendar::from_toml(SPEC_CONFIG).unwrap();
        assert_eq!(calendar.limit_at(at(10, 1, 0, 0)), 10);
        assert_eq!(calendar.limit_at(at(10, 1, 8, 59)), 10);
    }

    #[test]
    fn the_0900_boundary_is_exclusive_on_the_early_side() {
        let calendar = ConcurrencyCalendar::from_toml(SPEC_CONFIG).unwrap();
        assert_eq!(calendar.limit_at(at(10, 1, 8, 59)), 10);
        assert_eq!(calendar.limit_at(at(10, 1, 9, 0)), 3);
    }

    #[test]
    fn weekends_are_ten_all_day() {
        let calendar = ConcurrencyCalendar::from_toml(SPEC_CONFIG).unwrap();
        // 2026-10-03 is a Saturday, 2026-10-04 a Sunday.
        assert_eq!(calendar.limit_at(at(10, 3, 3, 0)), 10);
        assert_eq!(calendar.limit_at(at(10, 3, 14, 0)), 10);
        assert_eq!(calendar.limit_at(at(10, 4, 14, 0)), 10);
    }

    #[test]
    fn a_calendar_without_windows_falls_back_to_the_default() {
        let calendar = ConcurrencyCalendar::from_toml("default_max_tasks = 7").unwrap();
        assert_eq!(calendar.limit_at(at(10, 1, 12, 0)), 7);
    }

    #[test]
    fn prefers_the_new_calendar_file_over_the_legacy_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("kanban.toml"), "default_max_tasks = 3\n").unwrap();
        std::fs::write(
            dir.path().join("superpowers-kanban.toml"),
            "default_max_tasks = 10\n",
        )
        .unwrap();
        assert_eq!(
            ConcurrencyCalendar::load_preferring_new(dir.path()).default_max_tasks,
            10
        );
    }

    #[test]
    fn falls_back_to_the_legacy_calendar_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("kanban.toml"), "default_max_tasks = 3\n").unwrap();
        assert_eq!(
            ConcurrencyCalendar::load_preferring_new(dir.path()).default_max_tasks,
            3
        );
    }

    #[test]
    fn broken_toml_is_rejected_by_from_toml() {
        assert!(ConcurrencyCalendar::from_toml("this is not toml = = =").is_err());
    }

    #[test]
    fn a_broken_file_loads_the_conservative_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("kanban.toml");
        std::fs::write(&path, "this is not toml = = =").unwrap();
        let calendar = ConcurrencyCalendar::load_or_default(&path);
        assert_eq!(calendar.limit_at(at(10, 1, 12, 0)), 3);
        assert_eq!(calendar.default_max_tasks, 3);
    }

    #[test]
    fn a_missing_file_loads_the_conservative_default() {
        let calendar = ConcurrencyCalendar::load_or_default(Path::new("/nonexistent/kanban.toml"));
        assert_eq!(calendar.limit_at(at(10, 1, 12, 0)), 3);
        assert_eq!(calendar.default_max_tasks, 3);
    }

    #[test]
    fn all_day_windows_ignore_start_and_end() {
        let config = r#"
default_max_tasks = 1

[[window]]
days = "Wed"
all_day = true
max_tasks = 5
"#;
        let calendar = ConcurrencyCalendar::from_toml(config).unwrap();
        // 2026-10-07 is a Wednesday.
        assert_eq!(calendar.limit_at(at(10, 7, 0, 0)), 5);
        assert_eq!(calendar.limit_at(at(10, 7, 23, 59)), 5);
    }

    #[test]
    fn a_window_excludes_its_own_end_time() {
        // 单窗口、非 "24:00" 的 end：没有任何后续窗口能遮蔽 `end` 时刻的比较。
        let config = r#"
default_max_tasks = 1

[[window]]
days = "Mon"
start = "09:00"
end = "12:00"
max_tasks = 7
"#;
        let calendar = ConcurrencyCalendar::from_toml(config).unwrap();
        // 2026-10-05 是周一；12:00 落在 [09:00, 12:00) 之外，回落 default_max_tasks。
        assert_eq!(
            calendar.limit_at(at(10, 5, 12, 0)),
            1,
            "end 是排他的：12:00 不应命中该窗口"
        );
        assert_eq!(
            calendar.limit_at(at(10, 5, 11, 59)),
            7,
            "11:59 仍在 [09:00, 12:00) 之内"
        );
    }

    #[test]
    fn the_first_matching_window_wins() {
        let config = r#"
default_max_tasks = 1

[[window]]
days = "Thu"
start = "00:00"
end = "24:00"
max_tasks = 4

[[window]]
days = "Thu"
start = "12:00"
end = "13:00"
max_tasks = 99
"#;
        let calendar = ConcurrencyCalendar::from_toml(config).unwrap();
        assert_eq!(
            calendar.limit_at(at(10, 1, 12, 30)),
            4,
            "earlier window wins"
        );
    }

    #[test]
    fn limit_at_ignores_seconds() {
        let calendar = ConcurrencyCalendar::from_toml(SPEC_CONFIG).unwrap();
        let nine = at(10, 1, 9, 0).with_second(59).unwrap();
        assert_eq!(calendar.limit_at(nine), 3);
    }

    #[test]
    fn a_calendar_without_interval_secs_defaults_to_ten() {
        let calendar = ConcurrencyCalendar::from_toml("default_max_tasks = 3").unwrap();
        assert_eq!(calendar.interval_secs, DEFAULT_INTERVAL_SECS);
    }

    #[test]
    fn interval_secs_round_trips_through_toml() {
        let calendar = ConcurrencyCalendar::from_toml("interval_secs = 30").unwrap();
        assert_eq!(calendar.interval_secs, 30);
        let text = calendar.to_toml();
        let reparsed = ConcurrencyCalendar::from_toml(&text).unwrap();
        assert_eq!(reparsed.interval_secs, 30);
    }

    #[test]
    fn to_toml_round_trips_windows_semantically() {
        let calendar = ConcurrencyCalendar::from_toml(SPEC_CONFIG).unwrap();
        let reparsed = ConcurrencyCalendar::from_toml(&calendar.to_toml()).unwrap();
        assert_eq!(reparsed.default_max_tasks, calendar.default_max_tasks);
        assert_eq!(reparsed.interval_secs, calendar.interval_secs);
        assert_eq!(reparsed.windows.len(), calendar.windows.len());
        for (a, b) in reparsed.windows.iter().zip(calendar.windows.iter()) {
            assert_eq!(a.days, b.days);
            assert_eq!(a.start, b.start);
            assert_eq!(a.end, b.end);
            assert_eq!(a.all_day, b.all_day);
            assert_eq!(a.max_tasks, b.max_tasks);
        }
    }

    #[test]
    fn settings_json_round_trips_through_the_calendar() {
        let calendar = ConcurrencyCalendar::from_toml(SPEC_CONFIG).unwrap();
        let json = calendar.settings_json();
        let rebuilt = ConcurrencyCalendar::from_settings_json(&json).unwrap();
        assert_eq!(rebuilt, calendar);
    }

    #[test]
    fn from_settings_json_rejects_an_out_of_range_interval() {
        let json = serde_json::json!({
            "default_max_tasks": 3,
            "interval_secs": 0,
            "windows": []
        });
        let error = ConcurrencyCalendar::from_settings_json(&json).unwrap_err();
        assert!(error.to_string().contains("interval_secs"), "{error}");
    }

    #[test]
    fn from_settings_json_rejects_a_zero_max_tasks() {
        let json = serde_json::json!({
            "default_max_tasks": 0,
            "interval_secs": 10,
            "windows": []
        });
        assert!(ConcurrencyCalendar::from_settings_json(&json).is_err());
    }

    #[test]
    fn from_settings_json_rejects_a_reversed_window() {
        let json = serde_json::json!({
            "default_max_tasks": 3,
            "interval_secs": 10,
            "windows": [
                { "days": "Mon", "start": "12:00", "end": "09:00", "max_tasks": 3 }
            ]
        });
        assert!(ConcurrencyCalendar::from_settings_json(&json).is_err());
    }

    #[test]
    fn save_then_load_prefers_the_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let calendar =
            ConcurrencyCalendar::from_toml("interval_secs = 45\ndefault_max_tasks = 7").unwrap();
        calendar.save(dir.path()).unwrap();
        assert!(dir.path().join("superpowers-kanban.toml").is_file());
        let loaded = ConcurrencyCalendar::load_preferring_new(dir.path());
        assert_eq!(loaded.interval_secs, 45);
        assert_eq!(loaded.default_max_tasks, 7);
    }
}
