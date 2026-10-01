use chrono::{DateTime, Local, NaiveTime, Weekday};
use serde::Deserialize;

/// 保守默认：配置缺失或损坏时使用。
pub const DEFAULT_MAX_TASKS: u16 = 3;

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
    pub windows: Vec<ConcurrencyWindow>,
}

impl Default for ConcurrencyCalendar {
    fn default() -> Self {
        Self {
            default_max_tasks: DEFAULT_MAX_TASKS,
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
            windows,
        })
    }

    /// 读取配置；文件缺失、不可读或损坏一律回退默认，绝不 panic。
    pub fn load_or_default(path: &std::path::Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_toml(&text).unwrap_or_else(|error| {
                tracing_fallback(&error, path);
                Self::default()
            }),
            Err(_) => Self::default(),
        }
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
}
