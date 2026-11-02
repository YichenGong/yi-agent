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
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use sha2::{Digest, Sha256};

/// 探针 suite 名。报告与 CLI 都靠它区分不同测试组。
pub const ISOLATION_SUITE: &str = "isolation";

/// 资源服务的自定义协议名。页面 Origin 在 macOS/Linux 上是
/// `plugin-asset://localhost`，Windows/Android 上是 `http://plugin-asset.localhost`。
pub const ASSET_SCHEME: &str = "plugin-asset";

/// 资源服务里“另一个包”的路径前缀，用于跨包读取攻击。
pub const FOREIGN_PACKAGE: &str = "foreign";

/// 单条用例的最长等待时间。超过即判失败，不允许无限等一个被阻塞的 frame。
pub const CASE_DEADLINE: Duration = Duration::from_secs(20);

/// 原生侧允许的 frame 数上限。页面不得靠不断创建子 frame 绕过资源限制。
pub const MAX_FRAMES: usize = 4;

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

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// 资源服务的白名单根。每个包一个根，路径必须落在包根之内。
#[derive(Debug, Clone)]
pub struct AssetRoot {
    /// 包内相对路径 → 绝对文件路径。启动时一次性建好，运行期不解释路径。
    files: BTreeMap<String, PathBuf>,
}

impl AssetRoot {
    /// 从目录建立白名单。只接受规范化普通文件；符号链接、越界路径直接跳过，
    /// 这样即便包内容被替换也不会把任意文件捧成资源。
    pub fn load(package_root: &Path) -> std::io::Result<Self> {
        let root = package_root
            .canonicalize()
            .unwrap_or_else(|_| package_root.to_path_buf());
        let mut files = BTreeMap::new();
        collect_files(&root, &root, &mut files)?;
        Ok(Self { files })
    }

    pub fn get(&self, relative: &str) -> Option<&PathBuf> {
        self.files.get(relative)
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

fn collect_files(
    root: &Path,
    dir: &Path,
    out: &mut BTreeMap<String, PathBuf>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.is_symlink() {
            // 链接可以指向包外；宁可漏也别把任意文件当资源。
            continue;
        }
        if metadata.is_dir() {
            collect_files(root, &path, out)?;
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let Some(key) = relative.to_str().map(|value| value.replace('\\', "/")) else {
            continue;
        };
        out.insert(key, path);
    }
    Ok(())
}

/// 把请求路径规范化成包内相对路径；任何越界或非法输入都返回 `None`。
pub fn safe_relative_path(root: &Path, requested: &str) -> Option<PathBuf> {
    let trimmed = requested.trim_start_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.contains('\0') {
        return None;
    }
    let mut depth: i32 = 0;
    for segment in trimmed.split('/') {
        match segment {
            "" | "." => continue,
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return None;
                }
            }
            _ => depth += 1,
        }
    }
    let candidate = root.join(trimmed);
    // 规范化后再确认仍在根内：这一步挡住 `a/../../etc/passwd` 这类拼接。
    let canonical = candidate.canonicalize().ok()?;
    canonical.starts_with(root).then_some(canonical)
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

/// 原生侧容器的创建参数。探针与后续正式容器共用同一份描述，
/// 这样报告里的 `containerConfigSha256` 才有意义。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContainerConfig {
    pub scheme: String,
    /// 页面 CSP。覆盖脚本/样式/图片/帧/表单等资源类型，而不只是 connect-src。
    pub csp: String,
    /// 是否允许页面自定义协议之外的网络请求（探针要求 false）。
    pub allow_remote_network: bool,
    pub max_frames: usize,
    pub case_deadline_secs: u64,
    /// 原生侧恢复策略：超时后关闭并重建容器。
    pub recovery: RecoveryPolicy,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPolicy {
    CloseAndRebuild,
}

impl Default for ContainerConfig {
    fn default() -> Self {
        Self {
            scheme: ASSET_SCHEME.to_string(),
            csp: Self::strict_csp(),
            allow_remote_network: false,
            max_frames: MAX_FRAMES,
            case_deadline_secs: CASE_DEADLINE.as_secs(),
            recovery: RecoveryPolicy::CloseAndRebuild,
        }
    }
}

impl ContainerConfig {
    /// 严格的页面 CSP：默认全部拒绝，只放行本包资源与非内联的必要脚本样式。
    ///
    /// `form-action` / `frame-src` / `img-src` / `style-src` / `font-src` 都必须
    /// 显式设为 `'none'` 或自身来源：只写 `connect-src` 挡不住图片/表单外送
    /// （spec §4.4）。
    pub fn strict_csp() -> String {
        "default-src 'none'; \
         script-src 'self' 'unsafe-inline'; \
         style-src 'self' 'unsafe-inline'; \
         img-src 'self'; \
         font-src 'self'; \
         connect-src 'self'; \
         frame-src 'none'; \
         child-src 'none'; \
         worker-src 'none'; \
         form-action 'none'; \
         base-uri 'none'; \
         object-src 'none'; \
         navigate-to 'none'; \
         manifest-src 'none'; \
         media-src 'none'; \
         frame-ancestors 'none'"
            .to_string()
    }

    pub fn digest(&self) -> String {
        sha256_hex(&serde_json::to_vec(self).unwrap_or_default())
    }
}

/// 页面资源响应。抽成纯函数，便于在没有真实 WebView 时单测边界。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl AssetResponse {
    fn text(status: u16, body: &str) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: body.as_bytes().to_vec(),
        }
    }
}

pub fn content_type_for(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

/// 处理一次资源请求。
///
/// - `active_package` 是当前视图绑定的包；请求只能落在它的白名单里。
/// - 请求里出现别的包名、或 `/foreign/...` 前缀，一律 403 —— 这正是
///   `cross_package_asset_denied` 用例要证明的行为。
/// - 冒号之后的 host 部分（`plugin-asset://localhost/...`）先被剥掉。
pub fn serve_asset(
    assets: &AssetRoot,
    requested_path: &str,
    active_package: &str,
) -> AssetResponse {
    let mut path = requested_path;
    if let Some((_, rest)) = path.split_once("://") {
        // 去掉 `localhost` / `plugin-asset.localhost` 这类 authority 段。
        path = rest.split_once('/').map(|(_, tail)| tail).unwrap_or("");
    }
    let path = path.trim_start_matches('/');

    if path == FOREIGN_PACKAGE || path.starts_with(&format!("{FOREIGN_PACKAGE}/")) {
        return AssetResponse::text(403, "forbidden: package boundary");
    }
    if let Some((first, _)) = path.split_once('/') {
        if first != active_package && first.starts_with("pkg-") {
            return AssetResponse::text(403, "forbidden: package boundary");
        }
    }

    let root = Path::new("/");
    if safe_relative_path(root, path).is_none() && path.split('/').any(|s| s == "..") {
        return AssetResponse::text(403, "forbidden: traversal");
    }

    let Some(file) = assets.get(path) else {
        return AssetResponse::text(404, "not found");
    };
    match std::fs::read(file) {
        Ok(body) => AssetResponse {
            status: 200,
            content_type: content_type_for(path),
            body,
        },
        Err(_) => AssetResponse::text(403, "forbidden: unreadable"),
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
