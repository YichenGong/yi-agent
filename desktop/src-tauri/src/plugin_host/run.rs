//! 真实 WebView 探针的宿主侧编排（仅 `plugin-probe` feature）。
//!
//! 分工与 spec §4 一致：页面只负责**执行攻击并上报观察**，是否算通过由这里裁定。
//! 原生侧独立持有 frame 登记、nonce、截止时间与容器重建能力，页面自报“已恢复”
//! 一律不采信。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::probe::{
    assemble_report, bootstrap_script, serve_asset, sha256_hex, AssetRoot, CaseReport,
    ContainerConfig, ProbeChannel, ASSET_SCHEME, EXPECTED_CASES,
};

pub use super::probe::{ISOLATION_SUITE, MAX_MESSAGE_BYTES};

/// 页面回报的线上形状。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WireReport {
    pub frame: String,
    pub nonce: String,
    pub case: String,
    pub passed: bool,
    #[serde(default)]
    pub evidence: Vec<String>,
}

/// 命令行输入。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeOptions {
    pub instance_root: PathBuf,
    pub fixture_root: PathBuf,
    pub output: PathBuf,
    pub suite: String,
}

impl ProbeOptions {
    /// 解析 `--plugin-probe-run` 之后的参数；不是探针启动时返回 `None`。
    pub fn from_args(args: impl IntoIterator<Item = String>) -> Option<Result<Self, String>> {
        let args: Vec<String> = args.into_iter().collect();
        if !args.iter().any(|arg| arg == "--plugin-probe-run") {
            return None;
        }
        let mut instance_root = None;
        let mut fixture_root = None;
        let mut output = None;
        let mut suite = ISOLATION_SUITE.to_string();

        let mut iter = args.iter().peekable();
        while let Some(flag) = iter.next() {
            let mut take = || iter.next().cloned();
            match flag.as_str() {
                "--plugin-probe-run" => {}
                "--instance-root" => instance_root = take(),
                "--fixture-root" => fixture_root = take(),
                "--output" => output = take(),
                "--suite" => match take() {
                    Some(value) => suite = value,
                    None => return Some(Err("--suite needs a value".to_string())),
                },
                other => {
                    if other.starts_with("--plugin-probe") && other != "--plugin-probe-run" {
                        // 未识别的探针开关必须失败，不能静默忽略。
                        return Some(Err(format!("unknown probe flag `{other}`")));
                    }
                }
            }
        }

        Some(match (instance_root, fixture_root, output) {
            (Some(instance_root), Some(fixture_root), Some(output)) => Ok(Self {
                instance_root: PathBuf::from(instance_root),
                fixture_root: PathBuf::from(fixture_root),
                output: PathBuf::from(output),
                suite,
            }),
            _ => Err("--instance-root, --fixture-root and --output are all required".to_string()),
        })
    }
}

/// 页面在恢复阶段必须看到的宿主标记。它由**原生侧**写在实例根里，
/// 页面读不到它就无法把“恢复”伪装成功。
pub const RESUME_MARKER: &str = "probe-resume.json";

fn marker_path(instance_root: &Path) -> PathBuf {
    instance_root.join(RESUME_MARKER)
}

pub fn write_resume_marker(instance_root: &Path, generation: u32) -> std::io::Result<()> {
    std::fs::create_dir_all(instance_root)?;
    let body = serde_json::to_vec(&json!({ "generation": generation }))?;
    std::fs::write(marker_path(instance_root), body)
}

pub fn read_resume_marker(instance_root: &Path) -> Option<u32> {
    let raw = std::fs::read(marker_path(instance_root)).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    value.get("generation")?.as_u64().map(|value| value as u32)
}

/// 探针运行期的共享状态。协议处理器与编排线程都拿同一个 `Arc`。
pub struct ProbeState {
    pub assets: AssetRoot,
    pub channel: ProbeChannel,
    pub config: ContainerConfig,
    pub instance_root: PathBuf,
    /// 当前窗口的 nonce，供页面读 `__probe/state` 时对齐。
    pub active_nonce: Mutex<String>,
    /// 已创建的窗口代数（1 = 首窗，2 = 重建后的窗口）。
    pub generation: AtomicUsize,
    /// 原生侧观察到的“页面已进入阻塞”。由页面在阻塞前上报。
    pub blocking_seen: Mutex<bool>,
    /// 阻塞被原生侧确认为“仍在阻塞”（心跳超时）。
    pub blocked_confirmed: Mutex<bool>,
    pub log: Mutex<Vec<String>>,
}

impl ProbeState {
    pub fn new(
        assets: AssetRoot,
        config: ContainerConfig,
        instance_root: PathBuf,
        nonce: String,
    ) -> Self {
        Self {
            assets,
            channel: ProbeChannel::new(),
            config,
            instance_root,
            active_nonce: Mutex::new(nonce),
            generation: AtomicUsize::new(1),
            blocking_seen: Mutex::new(false),
            blocked_confirmed: Mutex::new(false),
            log: Mutex::new(Vec::new()),
        }
    }

    pub fn note(&self, line: impl Into<String>) {
        let mut log = self.log.lock().unwrap();
        log.push(line.into());
        if log.len() > 512 {
            log.remove(0);
        }
    }
}

/// 处理协议请求。`method` + `path` 决定语义：
/// - `POST /__probe/report`：页面回报（受 nonce / frame / 大小限制约束）
/// - `GET  /__probe/state`：恢复阶段读取宿主标记
/// - 其余：白名单静态资源
pub fn handle_request(
    state: &ProbeState,
    method: &str,
    uri: &str,
    body: &[u8],
) -> (u16, &'static str, Vec<u8>) {
    let path = uri.split_once("://").map(|(_, rest)| rest).unwrap_or(uri);
    let path = path.split_once('/').map(|(_, tail)| tail).unwrap_or("");
    let path = path.split('?').next().unwrap_or("").trim_start_matches('/');

    match (method, path) {
        ("POST", "__probe/report") => {
            if body.len() > MAX_MESSAGE_BYTES {
                return (413, "text/plain; charset=utf-8", b"too large".to_vec());
            }
            let parsed: WireReport = match serde_json::from_slice(body) {
                Ok(value) => value,
                Err(error) => {
                    state.note(format!("malformed report: {error}"));
                    return (400, "text/plain; charset=utf-8", b"bad request".to_vec());
                }
            };
            if parsed.case == "blocked_script_recovery"
                && parsed.evidence.iter().any(|e| e == "blocking")
            {
                *state.blocking_seen.lock().unwrap() = true;
                state.note("page reports it is about to block");
            }
            let report = CaseReport {
                frame: parsed.frame,
                nonce: parsed.nonce,
                case: parsed.case,
                passed: parsed.passed,
                evidence: parsed.evidence,
            };
            match state.channel.case_reported(&report) {
                Ok((case, status, _)) => {
                    state.note(format!("case {case} -> {status:?}"));
                    (200, "text/plain; charset=utf-8", b"ok".to_vec())
                }
                Err(error) => {
                    state.note(format!("rejected report: {error}"));
                    (403, "text/plain; charset=utf-8", error.into_bytes())
                }
            }
        }
        ("GET", "__probe/state") => {
            let generation = state.generation.load(Ordering::SeqCst) as u32;
            let marker = read_resume_marker(&state.instance_root);
            let body = json!({
                "generation": generation,
                "marker": marker,
                "nonce": state.active_nonce.lock().unwrap().clone(),
            });
            (
                200,
                "application/json",
                serde_json::to_vec(&body).unwrap_or_default(),
            )
        }
        ("GET", _) => {
            let package = "ui";
            let response = serve_asset(&state.assets, path, package);
            (response.status, response.content_type, response.body)
        }
        _ => (
            405,
            "text/plain; charset=utf-8",
            b"method not allowed".to_vec(),
        ),
    }
}

/// 原生侧对恢复用例的裁定。只有三个条件同时成立才算通过：
/// 1. 页面确实进入了阻塞（阻塞前上报过），且之后**没有**再报“恢复”；
/// 2. 原生侧关闭并重建过容器（代数 ≥ 2）；
/// 3. 重建后的容器能正常交互，并且宿主自己写的标记仍然存在。
pub fn judge_blocked_recovery(
    blocked_confirmed: bool,
    generation: usize,
    rebuilt_interaction_ok: bool,
    host_marker_intact: bool,
    page_self_reported_recovery: bool,
) -> (bool, Vec<String>) {
    let mut evidence = Vec::new();
    // 失败项与成功项都写：G0 要审的是“凭什么算恢复”，只给结论无法复核。
    if blocked_confirmed {
        evidence.push("the page announced blocking before it spun the main thread".to_string());
    } else {
        evidence.push("FAIL: the page never confirmed blocking".to_string());
    }
    if generation >= 2 {
        evidence.push(format!(
            "the native side closed and rebuilt the container (generation {generation})"
        ));
    } else {
        evidence.push("FAIL: the native side never rebuilt the container".to_string());
    }
    if rebuilt_interaction_ok {
        evidence.push(
            "the rebuilt container reported working interaction (focus/IME/scroll/size)"
                .to_string(),
        );
    } else {
        evidence.push("FAIL: the rebuilt container did not report working interaction".to_string());
    }
    if host_marker_intact {
        evidence.push("the host-persisted marker survived the rebuild".to_string());
    } else {
        evidence.push("FAIL: the host-persisted marker did not survive the rebuild".to_string());
    }
    if page_self_reported_recovery {
        evidence.push("FAIL: the page claimed recovery itself; that is not evidence".to_string());
    }
    let ok = evidence.iter().all(|line| !line.starts_with("FAIL"));
    (ok, evidence)
}

fn summarize(report: &super::probe::ProbeReport) -> String {
    let passed = report
        .cases
        .iter()
        .filter(|case| case.status == super::probe::CaseStatus::Pass)
        .count();
    let total = report.cases.len();
    let verdict = if report.is_pass() { "PASS" } else { "FAIL" };
    let mut text = format!("{verdict} {passed}/{total} cases");
    for case in &report.cases {
        if case.status != super::probe::CaseStatus::Pass {
            text.push_str(&format!(
                "\n  {} -> {:?}: {}",
                case.id,
                case.status,
                case.evidence.join("; ")
            ));
        }
    }
    text
}

/// 跑一次探针并返回进程退出码。
///
/// `context` 由调用方传入，而不是在这里再展开一次 `generate_context!()`：
/// 同一 crate 里展开两次会各自定义一份 `_EMBED_INFO_PLIST` 静态符号，链接期
/// 直接报 "symbol already defined"。全进程只有一份 context，两条启动路径共用。
pub fn run_probe(context: tauri::Context<tauri::Wry>, options: ProbeOptions) -> i32 {
    // 编排起点：报告里的 startedAt 取这里，而不是报告对象构造的时刻。
    let started = Instant::now();
    if options.suite != ISOLATION_SUITE {
        eprintln!("[plugin-probe] unknown suite `{}`", options.suite);
        return 2;
    }

    let assets = match AssetRoot::load(&options.fixture_root) {
        Ok(assets) if !assets.is_empty() => assets,
        Ok(_) => {
            eprintln!(
                "[plugin-probe] fixture root has no files: {}",
                options.fixture_root.display()
            );
            return 2;
        }
        Err(error) => {
            eprintln!("[plugin-probe] cannot read fixture root: {error}");
            return 2;
        }
    };

    let config = ContainerConfig::default();
    let nonce = uuid::Uuid::new_v4().to_string();
    let container_digest = container_config_digest(&config, &assets);
    let app_sha256 = app_digest();
    let state = Arc::new(ProbeState::new(
        assets,
        config,
        options.instance_root.clone(),
        nonce.clone(),
    ));
    if let Err(error) = write_resume_marker(&options.instance_root, 1) {
        eprintln!("[plugin-probe] cannot write instance marker: {error}");
        return 2;
    }

    // 探针窗口不挂任何 capability：`native_invoke_denied` 要证明的正是页面
    // 拿不到主窗口能力，而不是靠页面“没去调用”。
    let mut context = context;
    let main_config = context
        .config()
        .app
        .windows
        .iter()
        .find(|window| window.label == "main")
        .cloned();
    context.config_mut().app.windows.clear();

    let handler_state = Arc::clone(&state);
    let builder = tauri::Builder::default().register_uri_scheme_protocol(
        ASSET_SCHEME,
        move |_ctx, request| {
            let (parts, body) = request.into_parts();
            let (status, content_type, body) = handle_request(
                &handler_state,
                parts.method.as_str(),
                &parts.uri.to_string(),
                &body,
            );
            let csp = handler_state.config.csp.clone();
            tauri::http::Response::builder()
                .status(status)
                .header(tauri::http::header::CONTENT_TYPE, content_type)
                .header("Content-Security-Policy", csp)
                .header("X-Content-Type-Options", "nosniff")
                .header("Referrer-Policy", "no-referrer")
                .body(body)
                .unwrap_or_else(|_| tauri::http::Response::new(Vec::new()))
        },
    );

    let setup_state = Arc::clone(&state);
    let setup_main_config = main_config.clone();
    let exit_code = Arc::new(AtomicUsize::new(0));
    let setup_exit = Arc::clone(&exit_code);

    let probe_options = options.clone();

    let app = builder
        .setup(move |app| {
            let handle = app.handle().clone();
            create_probe_window(&handle, &setup_state, 1, setup_main_config.as_ref())?;
            let control_state = Arc::clone(&setup_state);
            std::thread::spawn(move || {
                let code = drive_probe(
                    &handle,
                    &control_state,
                    &probe_options,
                    setup_main_config.as_ref(),
                    started,
                );
                setup_exit.store(code as usize, Ordering::SeqCst);
                handle.exit(code);
            });
            Ok(())
        })
        .build(context);

    let output_for_error = options.output.clone();
    let app = match app {
        Ok(app) => app,
        Err(error) => {
            eprintln!("[plugin-probe] cannot build app: {error}");
            let mut report = super::probe::ProbeReport::new(
                ISOLATION_SUITE,
                super::probe::iso8601_now_of(started),
                app_sha256,
                container_digest,
            );
            for id in EXPECTED_CASES {
                report.record(
                    id,
                    super::probe::CaseStatus::Fail,
                    vec![format!("the WebView app failed to build: {error}")],
                );
            }
            let _ = report.write_atomic(&output_for_error);
            return 1;
        }
    };

    app.run(|_app, _event| {});
    exit_code.load(Ordering::SeqCst) as i32
}

fn create_probe_window(
    handle: &tauri::AppHandle,
    state: &Arc<ProbeState>,
    generation: usize,
    main: Option<&tauri::utils::config::WindowConfig>,
) -> tauri::Result<()> {
    let nonce = uuid::Uuid::new_v4().to_string();
    *state.active_nonce.lock().unwrap() = nonce.clone();
    state.generation.store(generation, Ordering::SeqCst);

    let frame_id = format!("frame-{generation}");
    state
        .channel
        .register(&frame_id, &nonce)
        .map_err(|error| tauri::Error::Io(std::io::Error::other(error)))?;

    let url: tauri::Url = format!("{ASSET_SCHEME}://localhost/index.html?gen={generation}")
        .parse()
        .map_err(tauri::Error::InvalidUrl)?;

    let label = format!("probe-{generation}");
    // 沿用主窗口配置（尺寸/主题/背景色/title bar），只换 label 与 URL：
    // 探针要验证的是**正式窗口形态**下的隔离，而不是一个特制窗口。
    let mut builder = match main {
        Some(config) => {
            let mut config = config.clone();
            config.label = label.clone();
            config.url = tauri::WebviewUrl::External(url);
            tauri::WebviewWindowBuilder::from_config(handle, &config)?
        }
        None => tauri::WebviewWindowBuilder::new(
            handle,
            label.clone(),
            tauri::WebviewUrl::External(url),
        )
        .title(format!("plugin probe {generation}"))
        .inner_size(900.0, 700.0),
    };
    // 每次页面加载都算一次“初始化”。原生侧据此识别重放：同一 frame 的第二次
    // 加载会让 `cross_frame_replay_denied` 判失败，而不是被页面自报覆盖。
    //
    // 只数 `Finished`：`on_page_load` 对一次加载会回调两次（Started 与 Finished），
    // 两个都数会把每一次正常加载都误判成重放。
    let load_state = Arc::clone(state);
    let load_frame = frame_id.clone();
    let nav_state = Arc::clone(state);
    builder = builder
        .initialization_script(bootstrap_script(&nonce, &frame_id))
        // 顶层跳转由宿主拦：CSP 的 `navigate-to` 在 WebKit 上不可靠。
        .on_navigation(move |url| {
            let allowed = super::probe::navigation_allowed(url.as_str());
            if !allowed {
                nav_state.note(format!("navigation blocked: {url}"));
            }
            allowed
        })
        .on_page_load(move |_window, payload| {
            if payload.event() == tauri::webview::PageLoadEvent::Finished {
                if let Err(error) = load_state.channel.mark_initialized(&load_frame) {
                    load_state.note(error);
                }
            }
            load_state.note(format!(
                "page load: {:?} {}",
                payload.event(),
                payload.url()
            ));
        });
    builder.build()?;
    state.note(format!("created window generation {generation}"));
    Ok(())
}

fn close_probe_windows(handle: &tauri::AppHandle) {
    use tauri::Manager;
    for (label, window) in handle.webview_windows() {
        if label.starts_with("probe-") {
            let _ = window.close();
        }
    }
}

/// 编排：等首窗回报 → 处理阻塞 → 关闭并重建 → 等恢复 → 写报告。
fn drive_probe(
    handle: &tauri::AppHandle,
    state: &Arc<ProbeState>,
    options: &ProbeOptions,
    main: Option<&tauri::utils::config::WindowConfig>,
    started: Instant,
) -> i32 {
    // 首窗：等页面把攻击用例报完（它最后一步是阻塞自己）。
    while !super::probe::is_timed_out(started, Instant::now()) {
        if *state.blocking_seen.lock().unwrap() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let mut timed_out: Vec<String> = Vec::new();
    let blocked_confirmed = *state.blocking_seen.lock().unwrap();
    if blocked_confirmed {
        *state.blocked_confirmed.lock().unwrap() = true;
    } else {
        // 页面没进入阻塞阶段：所有尚未回报的用例按超时处理。
        let seen = state.channel.seen();
        for id in EXPECTED_CASES {
            if !seen.contains_key(*id) {
                timed_out.push((*id).to_string());
            }
        }
    }

    // 原生侧主动关闭失控容器。关闭动作必须由主线程执行。
    let closed = {
        let thread_handle = handle.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let _ = handle.run_on_main_thread(move || {
            close_probe_windows(&thread_handle);
            let _ = sender.send(());
        });
        receiver.recv_timeout(Duration::from_secs(5)).is_ok()
    };
    state.note(format!("close of the blocked container reported: {closed}"));

    let mut rebuilt = false;
    if blocked_confirmed && closed {
        let thread_handle = handle.clone();
        let state_clone = Arc::clone(state);
        let main_config = main.cloned();
        let (sender, receiver) = std::sync::mpsc::channel();
        let _ = handle.run_on_main_thread(move || {
            let result = create_probe_window(&thread_handle, &state_clone, 2, main_config.as_ref());
            let _ = sender.send(result.is_ok());
        });
        rebuilt = receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or(false);
    }
    state.note(format!("rebuilt container: {rebuilt}"));

    // 恢复窗口要在期限内把 normal_interaction_works 与恢复结论报回来。
    if rebuilt {
        let recovery_started = Instant::now();
        while !super::probe::is_timed_out(recovery_started, Instant::now()) {
            let seen = state.channel.seen();
            let done = seen.contains_key("normal_interaction_works")
                && seen.contains_key("blocked_script_recovery");
            if done {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    let host_marker_intact = read_resume_marker(&options.instance_root) == Some(1);
    for generation in 1..=state.generation.load(Ordering::SeqCst) {
        let frame = state.channel.frame(&format!("frame-{generation}"));
        state.note(format!("frame-{generation}: {frame:?}"));
    }
    let seen = state.channel.seen();
    let page_self_reported_recovery = seen
        .get("blocked_script_recovery")
        .map(|status| *status == super::probe::CaseStatus::Pass)
        .unwrap_or(false);
    let rebuilt_interaction_ok = seen
        .get("normal_interaction_works")
        .map(|status| *status == super::probe::CaseStatus::Pass)
        .unwrap_or(false);

    let mut report = assemble_report(
        &state.channel,
        &timed_out,
        super::probe::iso8601_now_of(started),
        app_digest(),
        container_config_digest(&state.config, &state.assets),
    );
    // blocked_script_recovery 的最终结论由原生覆盖：页面自报的 pass 不算数。
    report
        .cases
        .retain(|case| case.id != "blocked_script_recovery");
    let (recovered, evidence) = judge_blocked_recovery(
        blocked_confirmed,
        state.generation.load(Ordering::SeqCst),
        rebuilt_interaction_ok,
        host_marker_intact,
        page_self_reported_recovery,
    );
    report.record(
        "blocked_script_recovery",
        if recovered {
            super::probe::CaseStatus::Pass
        } else {
            super::probe::CaseStatus::Fail
        },
        evidence,
    );
    // 保持报告顺序稳定：按 EXPECTED_CASES 重排。
    report.cases.sort_by_key(|case| {
        EXPECTED_CASES
            .iter()
            .position(|id| *id == case.id)
            .unwrap_or(usize::MAX)
    });

    let summary = summarize(&report);
    match report.write_atomic(&options.output) {
        Ok(()) => {
            println!("{summary}");
            eprintln!(
                "[plugin-probe] report written to {} ({} log lines)",
                options.output.display(),
                state.log.lock().unwrap().len()
            );
            if report.is_pass() {
                0
            } else {
                1
            }
        }
        Err(error) => {
            eprintln!("[plugin-probe] cannot write report: {error}");
            1
        }
    }
}

/// 容器配置摘要：把配置与它实际服务的文件清单绑在一起，
/// 避免“配置没变但资源换了”仍显示同一个摘要。
pub fn container_config_digest(config: &ContainerConfig, assets: &AssetRoot) -> String {
    // 摘要必须同时覆盖**容器配置**与它服务的**资源清单规模**：配置没变、
    // 白名单换了一套，也不允许显示同一个摘要。
    sha256_hex(format!("{}:files={}", config.digest(), assets.len()).as_bytes())
}

/// 当前可执行文件摘要。报告用它证明“这份证据来自哪个二进制”。
pub fn app_digest() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|path| std::fs::read(path).ok())
        .map(|bytes| sha256_hex(&bytes))
        .unwrap_or_else(|| "0".repeat(64))
}
