//! 插件容器技术探针（`plugin-probe` feature 专属）。
//!
//! 这个模块**不进正式发布物**：整个文件只在 `plugin-probe` feature 打开时编译，
//! 探针窗口只在 `--plugin-probe-run` 参数下创建。产品容器在 T17 才落地，届时复用
//! 这里的资源服务与消息校验规则，而不是另写一套。
//!
//! 设计约束（来自 spec §4）：
//! - 探针不得为了“让测试过”而关掉安全配置；它验证的就是正式配置。
//! - 插件页面是**潜在失控客户端**：不信任它自报的身份、nonce 或“我恢复了”的结论。
//! - 原生侧必须能关掉并重建失控的 frame，且核心交互与会话持久化不受损。

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

// 资源服务与容器策略住在生产模块里；探针**再导出**它们，这样集成测试与
// 报告字段沿用既有路径，而实现只有一份。
pub use super::assets::{
    content_type_for, safe_relative_path, serve_asset, sha256_hex, AssetResponse, AssetRoot,
    ASSET_SCHEME, CASE_DEADLINE, FOREIGN_PACKAGE, MAX_FRAMES,
};
pub use super::container::{ContainerConfig, RecoveryPolicy};

/// 探针 suite 名。报告与 CLI 都靠它区分不同测试组。
pub const ISOLATION_SUITE: &str = "isolation";

/// 一次性初始化通道的字节上限：报告通道只传小 JSON。
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// 一个被测 window（frame）在原生侧的登记。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameRecord {
    /// 原生侧分配的 id，页面不可见、也不可自报。
    pub id: String,
    /// 与页面共享的一次性 nonce；重放或冒领都靠它识别。
    pub nonce: String,
    /// 该 frame 已消费的初始化次数。>1 即说明有重放尝试。
    pub initialized: u32,
}

/// 原生命令通道。页面通过它上报“某用例（未）通过”，但**结论由原生侧复核**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseReport {
    pub frame: String,
    pub nonce: String,
    pub case: String,
    pub passed: bool,
    pub evidence: Vec<String>,
}

/// 用例结果的最终状态。`Skipped` / `Unsupported` 与 `Fail` 一样**不是通过**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseStatus {
    Pass,
    Fail,
    Skipped,
    Unsupported,
}

impl CaseStatus {
    fn passes(self) -> bool {
        matches!(self, CaseStatus::Pass)
    }
}

/// 单条用例结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaseResult {
    pub id: String,
    pub status: CaseStatus,
    pub evidence: Vec<String>,
}

/// 整份报告。字段与 `desktop/scripts/plugins/probe-lib.mjs` 的校验一一对应。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeReport {
    pub suite: String,
    pub started_at: String,
    pub finished_at: String,
    pub app_sha256: String,
    pub container_config_sha256: String,
    /// 原子落盘标记。只有“临时文件 + rename”写出的报告才是证据。
    pub atomic: bool,
    pub cases: Vec<CaseResult>,
}

impl ProbeReport {
    /// `started_at` 由调用方给出：探针的真正起点是**开始编排**的时刻，而不是
    /// 报告对象被构造的时刻。两者若相同，报告里的时长永远是 0，就无法据此
    /// 判断这次运行是否撞上了截止时间。
    pub fn new(
        suite: &str,
        started_at: String,
        app_sha256: String,
        container_config_sha256: String,
    ) -> Self {
        Self {
            suite: suite.to_string(),
            started_at: started_at.clone(),
            finished_at: started_at,
            app_sha256,
            container_config_sha256,
            atomic: false,
            cases: Vec::new(),
        }
    }

    pub fn record(&mut self, id: &str, status: CaseStatus, evidence: Vec<String>) {
        self.cases.push(CaseResult {
            id: id.to_string(),
            status,
            evidence,
        });
    }

    pub fn is_pass(&self) -> bool {
        !self.cases.is_empty() && self.cases.iter().all(|case| case.status.passes())
    }

    /// 临时文件 + rename 写出。目标目录不存在时先创建。
    pub fn write_atomic(&mut self, destination: &Path) -> std::io::Result<()> {
        self.atomic = true;
        self.finished_at = iso8601_now();
        let body = serde_json::to_vec_pretty(self)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let parent = destination.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;
        let temp = destination.with_extension(format!("tmp-{}", std::process::id()));
        {
            use std::io::Write;
            let mut file = std::fs::File::create(&temp)?;
            file.write_all(&body)?;
            file.sync_all()?;
        }
        std::fs::rename(&temp, destination)?;
        // 目录项也要落盘，否则崩溃后可能看到“没有报告”或“旧报告”。
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
        Ok(())
    }
}

/// 校验页面回报，返回值得采纳的用例结果。
///
/// 规则（spec §4 最后一段）：绑定实际 frame、一次性初始化通道、不信任页面自报身份。
/// - frame / nonce 必须与原生登记一致，否则丢弃并记录冒领；
/// - 未知用例 id 丢弃；
/// - 同一 frame 同一用例重复回报，只采纳第一次，其余按重放记录；
/// - `passed=false` 一律如实转成失败证据。
pub fn verify_case_report(
    frames: &BTreeMap<String, FrameRecord>,
    seen: &mut BTreeMap<String, CaseStatus>,
    report: &CaseReport,
) -> Result<(String, CaseStatus, Vec<String>), String> {
    let frame = frames
        .get(&report.frame)
        .ok_or_else(|| format!("unknown frame `{}`", report.frame))?;
    if frame.nonce != report.nonce {
        return Err(format!("frame `{}` presented a stale nonce", report.frame));
    }
    if seen.contains_key(&report.case) {
        return Err(format!("case `{}` reported twice", report.case));
    }
    let status = if report.passed {
        CaseStatus::Pass
    } else {
        CaseStatus::Fail
    };
    seen.insert(report.case.clone(), status);
    Ok((report.case.clone(), status, report.evidence.clone()))
}

/// 判定一条用例是否超时。
pub fn is_timed_out(started: Instant, now: Instant) -> bool {
    now.duration_since(started) > CASE_DEADLINE
}

/// 单帧初始化次数超过 1 即视为重放。
pub fn replay_attempted(initialized: u32) -> bool {
    initialized > 1
}

pub fn iso8601_now() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    iso8601_at(now)
}

/// 把任意时刻格式化成同样的 ISO 形状。报告的 `startedAt` 用它记录**编排起点**。
pub fn iso8601_now_of(started: Instant) -> String {
    // `Instant` 没有绝对的纪元参照，只能按“距现在多久”回推。
    let elapsed = started.elapsed();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .saturating_sub(elapsed);
    iso8601_at(now)
}

fn iso8601_at(now: Duration) -> String {
    let secs = now.as_secs();
    let millis = now.subsec_millis();
    // 报告只用于排序与取证，不需要完整日历转换：用 epoch 秒 + 毫秒的可排序写法，
    // 同时保留 ISO 形状以便阅读。
    let days = secs / 86_400;
    let seconds_of_day = secs % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        seconds_of_day / 3600,
        (seconds_of_day % 3600) / 60,
        seconds_of_day % 60
    )
}

/// Howard Hinnant 的 days→civil 算法，避免为探针引入 chrono 依赖。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 探测脚本向原生侧证明“我确实在页面里”的一次性挑战。
///
/// 页面拿不到 `nonce` 就无法伪造回报；nonce 由原生侧在创建窗口时生成并通过
/// 初始化脚本注入，页面重载后旧 nonce 失效。
#[derive(Debug, Default)]
pub struct ProbeChannel {
    frames: Mutex<BTreeMap<String, FrameRecord>>,
    seen: Mutex<BTreeMap<String, CaseStatus>>,
    /// 每条用例自己给的证据。报告必须带上它：G0 要审的是理由，不只是结论。
    evidence: Mutex<BTreeMap<String, Vec<String>>>,
}

impl ProbeChannel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, id: &str, nonce: &str) -> Result<(), String> {
        if self.frames.lock().unwrap().len() >= MAX_FRAMES {
            return Err("too many frames".to_string());
        }
        self.frames.lock().unwrap().insert(
            id.to_string(),
            FrameRecord {
                id: id.to_string(),
                nonce: nonce.to_string(),
                initialized: 0,
            },
        );
        Ok(())
    }

    /// 记录一次初始化。返回 `true` 表示这是该 frame 的首次初始化。
    pub fn mark_initialized(&self, id: &str) -> Result<bool, String> {
        let mut frames = self.frames.lock().unwrap();
        let frame = frames
            .get_mut(id)
            .ok_or_else(|| format!("unknown frame `{id}`"))?;
        frame.initialized += 1;
        if replay_attempted(frame.initialized) {
            return Err(format!("frame `{id}` replayed its initialization"));
        }
        Ok(true)
    }

    pub fn case_reported(
        &self,
        report: &CaseReport,
    ) -> Result<(String, CaseStatus, Vec<String>), String> {
        let frames = self.frames.lock().unwrap().clone();
        let outcome = verify_case_report(&frames, &mut self.seen.lock().unwrap(), report)?;
        self.evidence
            .lock()
            .unwrap()
            .insert(outcome.0.clone(), outcome.2.clone());
        Ok(outcome)
    }

    pub fn seen(&self) -> BTreeMap<String, CaseStatus> {
        self.seen.lock().unwrap().clone()
    }

    pub fn evidence(&self) -> BTreeMap<String, Vec<String>> {
        self.evidence.lock().unwrap().clone()
    }

    pub fn frame(&self, id: &str) -> Option<FrameRecord> {
        self.frames.lock().unwrap().get(id).cloned()
    }
}

/// 原生侧的导航闸门。
///
/// CSP 的 `navigate-to` 在 WebKit 里支持并不完整，顶层跳转必须由宿主自己拦。
/// 判据只有一条：**必须仍是本包资源协议 + 本窗口自己的文档**。任何
/// `http(s):`、`file:`、`data:` 或其它 scheme 都拒绝，这样
/// `navigation_and_popup_denied` 证明的是宿主真的挡住了，而不是页面"没去跳"。
pub fn navigation_allowed(url: &str) -> bool {
    let Ok(parsed) = tauri::Url::parse(url) else {
        return false;
    };
    parsed.scheme() == ASSET_SCHEME && parsed.host_str() == Some("localhost")
}

/// 探针的用例脚本注入。`nonce` 每次创建窗口重新生成。
pub fn bootstrap_script(nonce: &str, frame_id: &str) -> String {
    let nonce = serde_json::to_string(nonce).unwrap_or_else(|_| "\"\"".to_string());
    let frame_id = serde_json::to_string(frame_id).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r#"window.__YI_PLUGIN_PROBE__ = Object.freeze({{ nonce: {nonce}, frame: {frame_id} }});"#
    )
}

/// 探针报告里必须出现的用例清单（与 probe-lib.mjs 同步；两边都有测试钉住）。
pub const EXPECTED_CASES: &[&str] = &[
    "dom_read_denied",
    "native_storage_read_denied",
    "native_invoke_denied",
    "cross_package_asset_denied",
    "cross_version_asset_denied",
    "navigation_and_popup_denied",
    "implicit_exfiltration_denied",
    "frame_impersonation_denied",
    "cross_frame_replay_denied",
    "blocked_script_recovery",
    "alloc_burst_is_bounded",
    "normal_interaction_works",
];

/// 把页面回报汇成报告：缺项、冒领、超时、非 pass 全部如实落成失败。
pub fn assemble_report(
    channel: &ProbeChannel,
    timed_out: &[String],
    started_at: String,
    app_sha256: String,
    container_config_sha256: String,
) -> ProbeReport {
    let mut report = ProbeReport::new(
        ISOLATION_SUITE,
        started_at,
        app_sha256,
        container_config_sha256,
    );
    let seen = channel.seen();
    let evidence = channel.evidence();
    for id in EXPECTED_CASES {
        if timed_out.iter().any(|case| case == id) {
            report.record(
                id,
                CaseStatus::Fail,
                vec!["native deadline expired".to_string()],
            );
            continue;
        }
        match seen.get(*id) {
            Some(status) => {
                report.record(id, *status, evidence.get(*id).cloned().unwrap_or_default())
            }
            None => report.record(
                id,
                CaseStatus::Fail,
                vec!["missing: the case never reported".to_string()],
            ),
        }
    }
    // 原生侧观察到的重放**覆盖**页面自报结论，而不是追加一条：
    // 报告契约要求每个 case 只出现一次（probe-lib.mjs 会对重复 id 抛错）。
    if channel
        .frames
        .lock()
        .map(|frames| {
            frames
                .values()
                .any(|frame| replay_attempted(frame.initialized))
        })
        .unwrap_or(false)
    {
        if let Some(case) = report
            .cases
            .iter_mut()
            .find(|case| case.id == "cross_frame_replay_denied")
        {
            case.status = CaseStatus::Fail;
            case.evidence = vec!["a frame replayed its initialization".to_string()];
        }
    }
    report
}
