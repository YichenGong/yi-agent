//! 通用会话驱动接进 `serve` 之后的集成测试:真 `ServeLauncher` 起**可见**会话、
//! `driver_tick` 按 (项目, 插件) 拉拍、`append` 复用已有会话。
//!
//! 夹具尽量走**真实路径**:真 socket(按请求转发到绑定的假 daemon)、真
//! `ThreadStore`、真 `driver_tick`、真 launcher 起的会话。假的只有模型与「插件怎么
//! 回答 `driver/next`」。

use super::*;
use crate::server::tests::{build_slow_agent, build_test_agent, test_config};
use crate::test_support::isolated_models_path;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::session_driver::DriverState;

/// 一台听在项目 runtime socket 上的假 daemon。
///
/// 它做生产 daemon 对 `PluginQuery` 做的那一件事——把查询转给绑定
/// (`on_query`)的看板回答——并把每次插件调用记录在案,供断言。其余请求
/// (`Status` 探活、daemon attach 等)统一回 `internal`:这既是诚实的「我不是
/// 真 daemon」,也让 `attach_cwd_runtime` 按生产里的降级路径失败,而不必起一
/// 个真 daemon。
struct FakeDaemon {
    socket: PathBuf,
    calls: Arc<StdMutex<Vec<serde_json::Value>>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    _dir: tempfile::TempDir,
    project: PathBuf,
    #[allow(dead_code)]
    runtime_dir: PathBuf,
}

impl FakeDaemon {
    fn bind(
        on_query: impl Fn(&Path, &str, &serde_json::Value) -> Result<serde_json::Value, String>
        + Send
        + 'static,
    ) -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let runtime_dir = yi_agent_subagent::attach::project_runtime_directory(&project);
        std::fs::create_dir_all(&runtime_dir).unwrap();
        let socket = yi_agent_store::ipc::socket_path_for(&runtime_dir).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let query_project = project.clone();

        let calls: Arc<StdMutex<Vec<serde_json::Value>>> = Arc::new(StdMutex::new(Vec::new()));
        let recorded = Arc::clone(&calls);
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                if flag.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let request: serde_json::Value =
                    serde_json::from_str(&line).unwrap_or(serde_json::Value::Null);
                let command = &request["command"];
                let method = command["method"].as_str().unwrap_or_default().to_string();
                let value = if command["type"] == "PluginQuery" {
                    let plugin = command["plugin"].as_str().unwrap_or_default().to_string();
                    let params = command["params"].clone();
                    recorded.lock().unwrap().push(json!({
                        "plugin": plugin,
                        "method": method,
                        "params": params.clone(),
                    }));
                    // 宿主现在是通用的:任何声明了 `session_driver` 的插件都会被
                    // 问,故假 daemon 不再按插件名路由,只把请求转给绑定的回答。
                    match on_query(&query_project, &method, &params) {
                        Ok(result) => json!({ "type": "PluginResult", "value": result }),
                        // 被拒的查询在宿主侧呈现为传输错误(与「daemon 没有该插件的
                        // 路由」同型)——宿主据此放弃这一拍,不影响其它项目/插件。
                        Err(_) => {
                            json!({ "type": "Error", "code": "internal", "message": "refused" })
                        }
                    }
                } else if command["type"] == "Status" {
                    // `is_running` 靠这个探活。诚实回答它,否则恢复路径会把
                    // 「我没有真 daemon」误读成「项目没有看板」。
                    json!({ "type": "Status", "high_water_event_id": 0 })
                } else {
                    json!({ "type": "Error", "code": "internal", "message": "not a real daemon" })
                };
                let reply = json!({
                    "protocol_version": yi_agent_store::ipc::PROTOCOL_VERSION,
                    "request_id": request["request_id"],
                    "result": value,
                });
                let mut stream = stream;
                let _ = stream.write_all(reply.to_string().as_bytes());
                let _ = stream.write_all(b"\n");
            }
        });
        Self {
            socket,
            calls,
            stop,
            handle: Some(handle),
            _dir: dir,
            project,
            runtime_dir,
        }
    }

    fn calls(&self) -> Vec<serde_json::Value> {
        self.calls.lock().unwrap().clone()
    }

    fn calls_to(&self, method: &str) -> Vec<serde_json::Value> {
        self.calls()
            .into_iter()
            .filter(|call| call["method"] == method)
            .collect()
    }
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = std::os::unix::net::UnixStream::connect(&self.socket);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// `ServeLauncher` 需要的全部主循环局部量,一次性备齐。
struct Host {
    threads: HashMap<String, ThreadSession>,
    pending_activation: HashMap<String, Option<Arc<ThreadRoot>>>,
    process_watches: HashMap<String, ProcessWatch>,
    uploads: crate::image_upload::UploadRegistry,
    runtimes: ProjectRuntimes,
    thread_roots: ThreadRoots,
    cfg: RuntimeConfig,
    hub: Arc<crate::broadcast::Broadcaster>,
    turn_tx: mpsc::Sender<TurnEvent>,
    turn_rx: mpsc::Receiver<TurnEvent>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Decision>>>>,
    perm_seq: Arc<AtomicU64>,
    theme: crate::theme_tool::ThemeHandle,
    git_diff: crate::git_diff_tool::GitDiffHandle,
    workspaces: WorkspaceIndex,
    workdir: PathBuf,
    /// 委派链路要读的机器清单路径。指向本测试的隔离目录,避免读到开发者
    /// 真实的 `~/.yi-agent/models.json` 而改变解析出的 provider。
    models_path: PathBuf,
    /// 该 launcher 的组长信箱。测试不投递,故用一份空信箱即可。
    lead_inbox: crate::lead::LeadInbox,
    /// 该 launcher 的回信登记表,与信箱同一理由:测试不投递,空表即可。
    lead_replies: crate::lead::LeadReplies,
    /// 该 launcher 的组长写通道,同上:测试不投递,空信箱即可。
    lead_control: crate::lead_control::LeadControlInbox,
    /// 该 launcher 的写结果登记表,同一理由:测试不投递,空表即可。
    lead_control_replies: crate::lead_control::LeadControlReplies,
    /// 组长开关。默认关 = stable 行为；需要组长工具的用例显式置 true。
    lead_agent_enabled: bool,
    _workdir_dir: tempfile::TempDir,
    _theme_dir: tempfile::TempDir,
    _workspace_dir: tempfile::TempDir,
}

impl Host {
    fn new() -> Self {
        let workdir_dir = tempfile::TempDir::new().unwrap();
        let workdir = workdir_dir.path().to_path_buf();
        let theme_dir = tempfile::TempDir::new().unwrap();
        let workspace_dir = tempfile::TempDir::new().unwrap();
        let (turn_tx, turn_rx) = mpsc::channel::<TurnEvent>(8);
        Self {
            threads: HashMap::new(),
            pending_activation: HashMap::new(),
            process_watches: HashMap::new(),
            uploads: crate::image_upload::UploadRegistry::new(),
            runtimes: Arc::new(StdMutex::new(HashMap::new())),
            thread_roots: Arc::new(StdMutex::new(HashMap::new())),
            cfg: test_config(),
            hub: Arc::new(crate::broadcast::Broadcaster::new()),
            turn_tx,
            turn_rx,
            pending: Arc::new(Mutex::new(HashMap::new())),
            perm_seq: Arc::new(AtomicU64::new(1)),
            theme: crate::theme_tool::ThemeHandle::new(theme_dir.path().to_path_buf()),
            git_diff: crate::git_diff_tool::GitDiffHandle::new(),
            workspaces: WorkspaceIndex::new(workspace_dir.path().join("workspaces.json")),
            workdir,
            models_path: isolated_models_path(theme_dir.path()),
            lead_inbox: crate::lead::LeadInbox::new(),
            lead_replies: crate::lead::LeadReplies::new(),
            lead_control: crate::lead_control::LeadControlInbox::new(),
            lead_control_replies: crate::lead_control::LeadControlReplies::new(),
            lead_agent_enabled: false,
            _workdir_dir: workdir_dir,
            _theme_dir: theme_dir,
            _workspace_dir: workspace_dir,
        }
    }

    fn launcher<'a, F>(&'a mut self, build_agent: &'a F) -> ServeLauncher<'a, F>
    where
        F: Fn(
            Option<yi_agent_core::Session>,
            &Path,
            crate::thread_store::ThreadMode,
        ) -> anyhow::Result<BuiltAgent>,
    {
        ServeLauncher {
            lead_inbox: &self.lead_inbox,
            lead_replies: &self.lead_replies,
            lead_control: &self.lead_control,
            lead_control_replies: &self.lead_control_replies,
            lead_agent_enabled: self.lead_agent_enabled,
            lead_target: None,
            threads: &mut self.threads,
            pending_activation: &mut self.pending_activation,
            process_watches: &mut self.process_watches,
            uploads: &mut self.uploads,
            runtimes: &self.runtimes,
            thread_roots: &self.thread_roots,
            models_path: &self.models_path,
            cfg: &self.cfg,
            hub: &self.hub,
            turn_tx: &self.turn_tx,
            pending: &self.pending,
            permission_timeout: PERMISSION_TIMEOUT,
            perm_seq: &self.perm_seq,
            theme: &self.theme,
            git_diff: &self.git_diff,
            workspaces: &self.workspaces,
            build_agent,
        }
    }

    /// 像真主循环那样收 `TurnEvent::Finished`:清除该 thread 的
    /// `active_turn_id`,直到看到目标 thread 的完成事件。
    ///
    /// 不这样做,`active_turn_id` 永远不会被清——在 `serve` 里那是主循环的
    /// 活儿,这条测试没有主循环,必须自己复刻。
    async fn settle_turn(&mut self, thread_id: &str, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return self.is_idle(thread_id);
            }
            match tokio::time::timeout(remaining, self.turn_rx.recv()).await {
                Ok(Some(TurnEvent::Finished {
                    thread_id: id,
                    turn_id,
                    reply: _,
                    lead_request_id: _,
                })) => {
                    if let Some(session) = self.threads.get_mut(&id)
                        && session.active_turn_id.as_deref() == Some(turn_id.as_str())
                    {
                        session.active_turn_id = None;
                    }
                    if id == thread_id {
                        return true;
                    }
                }
                Ok(None) => return false,
                Err(_) => return self.is_idle(thread_id),
            }
        }
    }

    /// 该 thread 此刻是否**空闲**(`Idle` 且无活跃 turn)——与 `driver_tick`
    /// 采集的快照用的是同一条判据。
    fn is_idle(&self, thread_id: &str) -> bool {
        self.threads.get(thread_id).is_some_and(|session| {
            matches!(
                *session.status.lock().unwrap_or_else(|p| p.into_inner()),
                ThreadStatus::Idle
            ) && session.active_turn_id.is_none()
        })
    }

    /// 跑一轮真 `driver_tick`，用本 fixture 的全部局部量。
    ///
    /// 与主循环的 tick 分支同一形态：驱动记账由调用方持有(跨 tick 存活),
    /// 其余局部量按 `&mut` 顺序借给 launcher。
    async fn tick<F>(
        &mut self,
        resident_dir: &Path,
        drivers: &mut HashMap<(PathBuf, String), DriverState>,
        build_agent: &F,
    ) where
        F: Fn(
            Option<yi_agent_core::Session>,
            &Path,
            crate::thread_store::ThreadMode,
        ) -> anyhow::Result<BuiltAgent>,
    {
        super::driver_tick(
            resident_dir,
            drivers,
            &mut self.threads,
            &mut self.pending_activation,
            &mut self.process_watches,
            &mut self.uploads,
            &self.runtimes,
            &self.thread_roots,
            &self.models_path,
            &self.cfg,
            &self.hub,
            &self.turn_tx,
            &self.pending,
            PERMISSION_TIMEOUT,
            &self.perm_seq,
            &self.theme,
            &self.git_diff,
            &self.workspaces,
            build_agent,
            &self.lead_inbox,
            &self.lead_replies,
            &self.lead_control,
            &self.lead_control_replies,
            self.lead_agent_enabled,
        )
        .await;
    }
}

/// 一个声明了 `session_driver` 的驱动插件名。
///
/// 只是**名字**：宿主对插件的了解止于「清单里 `session_driver: true`」，
/// 没有任何插件专有词汇。
const DRIVER_PLUGIN: &str = "driver";

/// 把一个声明了 `session_driver` 的插件装进项目，并让发现环看得见这个项目。
///
/// 两处都只写**通用**事实：清单里的通用字段 `session_driver` 与通用常驻登记表
/// （`resident::require`）。宿主不该认识 `name` 之外的任何插件专有内容。
fn install_driver_plugin(project: &Path, resident_dir: &Path, name: &str) {
    let supervisors = project.join(".yi-agent/supervisors");
    std::fs::create_dir_all(&supervisors).unwrap();
    std::fs::write(
        supervisors.join(format!("{name}.json")),
        json!({
            "name": name,
            "command": format!("/bin/{name}"),
            "args": [],
            "switch_key": format!("{name}_on"),
            "restart_backoff_ms": 1000,
            "restart_backoff_max_ms": 30000,
            "stop_when_disabled": false,
            "query_socket": format!("{{state_dir}}/{name}.sock"),
            "session_driver": true,
        })
        .to_string(),
    )
    .unwrap();
    yi_agent_store::resident::require(resident_dir, project, name).unwrap();
}

/// 子进程里跑一个 `#[ignore]` 探针,取回它的 stdout+stderr 合并文本。
///
/// **为什么非得开子进程**:libtest 默认把 `eprintln!` 收进线程私有的捕获缓冲
/// (`std::io::set_output_capture`),根本不写 fd 2——父进程怎么重定向 fd 2 都
/// 看不到(实测两种模式见 `task-6-report.md` 的 FIX ROUND 1)。子进程加
/// `--nocapture` 后 `eprintln!` 才落回真 fd 2,这时才可观察。
///
/// 探针用 `#[ignore]` 标记:常规 `cargo test` 不会直接跑它(它只负责「在子进程
/// 里制造一次 tick」),由本函数的调用方显式带 `--ignored --exact` 拉起。
fn logs_of_ignored_probe(probe: &str) -> String {
    let exe = std::env::current_exe().expect("the test binary must be locatable");
    let output = std::process::Command::new(exe)
        .args([
            "--exact",
            probe,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .output()
        .expect("the test binary must be runnable");
    let mut text = String::from_utf8_lossy(&output.stderr).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stdout));
    assert!(
        output.status.success(),
        "the probe {probe} must pass; its output was:\n{text}"
    );
    text
}

/// 探针(默认跳过,由 `logs_of_ignored_probe` 在子进程里拉起):
/// 项目在常驻登记表里、但没有任何声明 `session_driver` 的插件 → 一拍 tick。
#[test]
#[ignore]
fn probe_a_registered_project_without_a_driver_plugin() {
    let resident_dir = tempfile::TempDir::new().unwrap();
    let project_dir = tempfile::TempDir::new().unwrap();
    // 登记为常驻,却不写任何清单目录:这正是「注册过了却没人可问」。
    yi_agent_store::resident::require(resident_dir.path(), project_dir.path(), "some-plugin")
        .unwrap();
    let mut host = Host::new();
    let mut drivers: HashMap<(PathBuf, String), DriverState> = HashMap::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(host.tick(resident_dir.path(), &mut drivers, &build_test_agent));
}

/// 探针(默认跳过):插件清单在、可查询,但 daemon/套接字拨不通 → 一拍 tick。
#[test]
#[ignore]
fn probe_a_driver_plugin_that_cannot_be_reached() {
    let resident_dir = tempfile::TempDir::new().unwrap();
    let project_dir = tempfile::TempDir::new().unwrap();
    // 目录就叫 `project`,好让日志断言能核对「这一行点了名」而不是碰巧匹配。
    let project = project_dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    // 清单写了 `query_socket`,却没有 daemon 听在那个 socket 上。
    install_driver_plugin(&project, resident_dir.path(), DRIVER_PLUGIN);
    let mut host = Host::new();
    let mut drivers: HashMap<(PathBuf, String), DriverState> = HashMap::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(host.tick(resident_dir.path(), &mut drivers, &build_test_agent));
}

/// F1:登记为常驻却没有驱动插件的项目,必须在日志里说明「这一拍没问任何人」。
///
/// 这是「静默停摆」与「问了、失败了」在运维眼里唯一的分界:若这一行不存在,
/// 一个健康的 daemon 永远不被问起也不留痕。
#[test]
fn a_registered_project_without_a_driver_plugin_says_nothing_was_asked() {
    let logs = logs_of_ignored_probe(
        "server::session_driver_tests::probe_a_registered_project_without_a_driver_plugin",
    );
    assert!(
        logs.contains("has no session_driver plugin, so nothing was asked"),
        "the never-asked case must leave a log line: {logs}"
    );
}

/// F1:问不通的 (项目, 插件) 必须在日志里各留一行,而不是无声跳过。
///
/// `driver_step` 把查询失败整体吞成「不推进」,故这一行只能由 tick 侧留下。
#[test]
fn a_driver_plugin_that_cannot_be_reached_leaves_a_log_line() {
    let logs = logs_of_ignored_probe(
        "server::session_driver_tests::probe_a_driver_plugin_that_cannot_be_reached",
    );
    assert!(
        logs.contains("driver tick: could not reach")
            && logs.contains(DRIVER_PLUGIN)
            && logs.contains("/project:"),
        "a failed query must name the plugin and the project: {logs}"
    );
}

/// 探针(默认跳过):查询**成功**、字面也读得出来,但对不上契约——键名漂移
/// (`type` 而非契约里的 `kind`)。一拍 tick。
#[test]
#[ignore]
fn probe_a_driver_answer_the_host_cannot_parse() {
    let daemon = FakeDaemon::bind(|_project, _method, _params| {
        Ok(json!({ "action_id": "a1", "type": "launch", "workdir": "/w" }))
    });
    let resident_dir = tempfile::TempDir::new().unwrap();
    install_driver_plugin(&daemon.project, resident_dir.path(), DRIVER_PLUGIN);
    let mut host = Host::new();
    let mut drivers: HashMap<(PathBuf, String), DriverState> = HashMap::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(host.tick(resident_dir.path(), &mut drivers, &build_test_agent));
}

/// 探针(默认跳过):插件答契约里的 `{"action": null}`——正常空闲。
#[test]
#[ignore]
fn probe_a_driver_that_answers_the_contract_null() {
    let daemon = FakeDaemon::bind(|_project, _method, _params| Ok(json!({ "action": null })));
    let resident_dir = tempfile::TempDir::new().unwrap();
    install_driver_plugin(&daemon.project, resident_dir.path(), DRIVER_PLUGIN);
    let mut host = Host::new();
    let mut drivers: HashMap<(PathBuf, String), DriverState> = HashMap::new();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(host.tick(resident_dir.path(), &mut drivers, &build_test_agent));
}

/// 第三种无声失败:查询**成功**、字面可读,却是宿主解析不出的形状。它与
/// 「插件说无事可做」在 `driver_step` 里同型(都回 `None`),不在这里分辨就每
/// 3s 无声重复一次。日志须点名插件、项目,并带上原答的顶层键供人看漂移。
#[test]
fn an_unparseable_driver_answer_leaves_a_log_line() {
    let logs = logs_of_ignored_probe(
        "server::session_driver_tests::probe_a_driver_answer_the_host_cannot_parse",
    );
    assert!(
        logs.contains("cannot parse")
            && logs.contains(DRIVER_PLUGIN)
            && logs.contains("/project answered driver/next")
            && logs.contains("action_id")
            && logs.contains("type"),
        "an unparseable answer must name the plugin, the project and the keys: {logs}"
    );
    assert!(
        !logs.contains("could not reach"),
        "答案读得出来,这不是「问不通」那一类: {logs}"
    );
}

/// 反向对照:契约的 `{"action": null}` 是**正常空闲**,不得被当成断链。
///
/// 这条与上一条互为闸门:若把「说不清」与「无事可做」混为一谈(例如去掉
/// `is_explicit_no_action` 判定),上一条仍会通过而这一条会因每拍一行噪音而红。
#[test]
fn a_contract_null_answer_is_not_reported_as_drift() {
    let logs = logs_of_ignored_probe(
        "server::session_driver_tests::probe_a_driver_that_answers_the_contract_null",
    );
    assert!(
        !logs.contains("cannot parse"),
        "契约的空闲形状是正常态,不该被打成断链: {logs}"
    );
}

/// 一轮 tick：插件回 launch/append → 宿主真的按它拉会话，且**不调用**任何
/// `board.*` 方法名。
///
/// 这是「宿主不再说看板话」这条约束的**载荷断言**：记录到的每一次插件调用都
/// 必须是通用的 `driver/next`。第一拍插件给一个 `launch`（宿主起可见会话），
/// 首轮跑完后第二拍它给一个 `append`（宿主复用同一个会话再投一轮）——两个动作
/// 都经 `driver_step` + 真 `ServeLauncher` 落地，插件同时从 `threads[].idle`
/// 拿到了宿主唯一能给的通用事实。
#[tokio::test(flavor = "multi_thread")]
async fn a_tick_drives_the_plugin_through_driver_next_only() {
    let mut host = Host::new();
    let workdir = host.workdir.to_string_lossy().to_string();
    let work_path = workdir.clone();
    let rounds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = Arc::clone(&rounds);
    // 每拍请求里 `threads[0].idle` 的原样记录(缺省记 `null`)。
    let idle_seen: Arc<StdMutex<Vec<serde_json::Value>>> = Arc::new(StdMutex::new(Vec::new()));
    let recorded_idle = Arc::clone(&idle_seen);
    // 每拍请求里 `sessions.running` 的原样记录(缺省记 `null`)。
    let running_seen: Arc<StdMutex<Vec<serde_json::Value>>> = Arc::new(StdMutex::new(Vec::new()));
    let recorded_running = Arc::clone(&running_seen);
    let daemon = FakeDaemon::bind(move |_project, method, params| {
        if method != "driver/next" {
            return Ok(json!({ "ok": true }));
        }
        // 原样记下本拍请求里的 `threads[0].idle`(本拍尚无会话时是 `null`),
        // 供 F4 断言「宿主经 tick 送出的是真 idle,不是常量」。
        recorded_idle
            .lock()
            .unwrap()
            .push(params["threads"][0]["idle"].clone());
        recorded_running
            .lock()
            .unwrap()
            .push(params["sessions"]["running"].clone());
        match seen.fetch_add(1, Ordering::SeqCst) {
            // 第一拍：起一个会话。
            0 => Ok(json!({
                "action_id": "a1",
                "kind": "launch",
                "workdir": work_path,
                "title": "driven · card-1",
                "objective": "do the first thing",
            })),
            // 第二拍：往它已经起了的那个会话里追加一轮。thread_id 只可能来自
            // 宿主上一拍回报的 `results`（`threads[]` 也带了它）。
            1 => {
                let thread_id = params["threads"][0]["thread_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                Ok(json!({
                    "action_id": "a2",
                    "kind": "append",
                    "thread_id": thread_id,
                    "objective": "round two",
                }))
            }
            _ => Ok(serde_json::Value::Null),
        }
    });
    let resident_dir = tempfile::TempDir::new().unwrap();
    install_driver_plugin(&daemon.project, resident_dir.path(), "driver");
    let mut drivers: HashMap<(PathBuf, String), DriverState> = HashMap::new();

    // 第一拍：launch。
    host.tick(resident_dir.path(), &mut drivers, &build_test_agent)
        .await;
    assert_eq!(host.threads.len(), 1, "the launch registers one session");
    let thread_id = host.threads.keys().next().cloned().unwrap();
    assert!(
        host.threads[&thread_id].active_turn_id.is_some(),
        "the first turn must be占位, otherwise the plugin sees it idle at once"
    );
    assert!(
        host.settle_turn(&thread_id, Duration::from_secs(10)).await,
        "the driver must consume the plugin's objective and finish"
    );

    // 第二拍：append 到同一个会话。
    host.tick(resident_dir.path(), &mut drivers, &build_test_agent)
        .await;

    // 第三拍：只问不做（插件回 `null`）。它让第二拍动手后的空闲事实有一次
    // 上送机会——`append` 在那拍里刚投递完，此刻该会话正忙（见下面的 F4 断言）。
    host.tick(resident_dir.path(), &mut drivers, &build_test_agent)
        .await;

    let calls = daemon.calls();
    assert_eq!(
        calls.len(),
        3,
        "one plugin call per tick, three ticks: {calls:?}"
    );
    let methods: Vec<&str> = calls
        .iter()
        .map(|call| call["method"].as_str().unwrap_or_default())
        .collect();
    assert!(
        methods.iter().all(|method| *method == "driver/next"),
        "the host drives the generic protocol and nothing else: {methods:?}"
    );
    assert!(
        methods.iter().all(|method| !method.starts_with("board.")),
        "no board vocabulary may leave the host: {methods:?}"
    );
    // 上面 `all(== "driver/next")` 已经蕴含「至少问过一次」,故不再单列
    // `calls_to("driver/next").is_empty()` 断言——它对本用例是冗余的。

    assert_eq!(host.threads.len(), 1, "append must reuse the session");
    assert!(
        host.threads[&thread_id].active_turn_id.is_some(),
        "the appended round must be delivered to the same session"
    );
    // F4:`threads[].idle` 是**经 tick 上送的真事实**,不是常量。三拍快照依次是
    // 「还没有会话(无 thread → 数组下标缺省 `null`)」「首轮已收尾(空闲 `true`)」
    // 「刚追加的一轮仍在飞(忙 `false`)」——同一字段三种取值,常量填不出来。
    let idle_series: Vec<serde_json::Value> = idle_seen.lock().unwrap().clone();
    assert_eq!(
        idle_series,
        vec![serde_json::Value::Null, json!(true), json!(false)],
        "the tick must wire real idle, not a constant: {idle_series:?}"
    );
    // 同一份快照里的全机运行数也必须是真事实:三拍依次是「没有会话 → 0」
    // 「首轮已收尾 → 0」「刚追加的一轮在飞 → 1」。看板靠这个数算全机余量,
    // 填常量就等于把配额判定废掉。
    let running_series: Vec<serde_json::Value> = running_seen.lock().unwrap().clone();
    assert_eq!(
        running_series,
        vec![json!(0), json!(0), json!(1)],
        "the tick must report the machine-wide running count: {running_series:?}"
    );
    assert!(
        host.settle_turn(&thread_id, Duration::from_secs(10)).await,
        "the appended round must run and finish"
    );
}

/// 核心行为:launcher 的一个 `launch` 动作落成一个**登记在册、driver 活着、
/// 首轮已投递、标题已落盘**的可见会话。
#[tokio::test(flavor = "multi_thread")]
async fn the_launcher_starts_a_registered_session_with_a_title_and_a_first_turn() {
    let mut host = Host::new();
    let workdir = host.workdir.clone();
    let thread_id = {
        let mut launcher = host.launcher(&build_test_agent);
        launch_via_driver(
            &mut launcher,
            &workdir.to_string_lossy(),
            "driven · card-1",
            "Implement the plan for card-1",
            (None, None),
        )
        .await
        .expect("the launch must succeed for a healthy factory")
    };

    assert_eq!(host.threads.len(), 1, "the driven thread is registered");
    let session = host
        .threads
        .get(&thread_id)
        .expect("the session is visible");
    assert_eq!(session.cwd, workdir.to_string_lossy());
    assert!(
        session.active_turn_id.is_some(),
        "the first turn must be占位, otherwise the session looks idle instantly"
    );
    assert_eq!(
        session
            .store
            .load(&thread_id)
            .unwrap()
            .and_then(|loaded| loaded.meta.title),
        Some("driven · card-1".to_string()),
        "the sidebar needs a name, not a raw thread id"
    );

    // driver 真的收了首轮并收尾:主循环会收到 `Finished`,清除占位。
    assert!(
        host.settle_turn(&thread_id, Duration::from_secs(10)).await,
        "the driver must consume the prompt and finish"
    );
    assert!(host.pending_activation.contains_key(&thread_id));
}

/// build 失败时绝不谎报:没有 thread 被登记。
#[tokio::test(flavor = "multi_thread")]
async fn a_failing_build_leaves_no_session_behind() {
    let mut host = Host::new();
    let workdir = host.workdir.clone();
    let build_failure =
        |_s: Option<yi_agent_core::Session>, _cwd: &Path, _m: crate::thread_store::ThreadMode| {
            Err::<BuiltAgent, _>(anyhow::anyhow!("no model for you"))
        };
    {
        let mut launcher = host.launcher(&build_failure);
        let error = launch_via_driver(
            &mut launcher,
            &workdir.to_string_lossy(),
            "driven · card-1",
            "Implement the plan for card-1",
            (None, None),
        )
        .await
        .expect_err("a factory failure must surface, not be swallowed");
        assert!(error.to_string().contains("no model"), "{error}");
    }
    assert!(host.threads.is_empty(), "a failed launch registers nothing");
}

/// 驱动器端到端:插件经 `driver/next` 交出一个 `launch`,宿主把它起成
/// **可见**会话——客户端的 hub 里出现 thread-keyed 的开启项,且首个内容项
/// 就是本轮 objective(用户提问),不是 agent 的 item。
///
/// 走**真 `driver_tick` + 真 `ServeLauncher` + 真 socket 转发**,是接线的
/// 最小完整闭环。插件同时从 `threads[].idle` 得知这一轮是否跑完。
#[tokio::test(flavor = "multi_thread")]
async fn a_driven_session_is_visible_and_its_first_content_item_is_the_objective() {
    let rounds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen_rounds = Arc::clone(&rounds);
    let work_path = Arc::new(StdMutex::new(String::new()));
    let handed_work = Arc::clone(&work_path);
    let daemon = FakeDaemon::bind(move |_project, method, _params| match method {
        "driver/next" => {
            if seen_rounds.fetch_add(1, Ordering::SeqCst) > 0 {
                return Ok(serde_json::Value::Null);
            }
            Ok(json!({
                "action_id": "a1",
                "kind": "launch",
                "workdir": handed_work.lock().unwrap().clone(),
                "title": "driven · card-1",
                "objective": "Implement the plan for card-1",
            }))
        }
        _ => Ok(json!({ "ok": true })),
    });

    let mut host = Host::new();
    // 监听宿主 hub:被驱动会话是 thread-keyed 的 content 通知,注册一个客户端
    // 就能看到驱动器起会话时到底发了什么、按什么顺序。
    let mut frames = host
        .hub
        .register(crate::broadcast::ClientId::ws(uuid::Uuid::new_v4()));
    let workdir = host.workdir.clone();
    *work_path.lock().unwrap() = workdir.to_string_lossy().to_string();
    let resident_dir = tempfile::TempDir::new().unwrap();
    install_driver_plugin(&daemon.project, resident_dir.path(), DRIVER_PLUGIN);
    let mut drivers: HashMap<(PathBuf, String), DriverState> = HashMap::new();

    host.tick(resident_dir.path(), &mut drivers, &build_test_agent)
        .await;

    assert_eq!(host.threads.len(), 1, "exactly one visible session");
    let thread_id = host.threads.keys().next().cloned().unwrap();

    // 被驱动会话对客户端「可见」的前提是 hub 里有它的 item;其中最关键的
    // 是**本轮开启项**(用户提问=插件给的 objective)必须排在 agent 任何 item
    // 之前——修复前它只在落盘时补发,点开会话会在最底部才看到开场白。
    let mut seen: Vec<(String, Option<String>, Option<String>)> = Vec::new();
    while let Ok(v) = frames.try_recv() {
        let method = v["method"].as_str().unwrap_or_default().to_string();
        let item = &v["params"]["item"];
        let itype = item["type"].as_str().map(str::to_string);
        let itext = item["text"].as_str().map(str::to_string);
        seen.push((method, itype, itext));
    }
    let first_content = seen
        .iter()
        .find(|(_, t, _)| t.is_some())
        .expect("the launch must emit at least one item frame");
    assert_eq!(
        first_content.1.as_deref(),
        Some("userMessage"),
        "the driven session's first content item must be the opening objective, \
         not an agent item: {seen:?}"
    );
    assert_eq!(
        first_content.0, "item/started",
        "the opening item starts the transcript: {seen:?}"
    );

    // 插件在下一拍必须看到这个会话,且能读出「闲不闲」这个通用事实。
    assert_eq!(
        drivers[&(daemon.project.clone(), DRIVER_PLUGIN.to_string())]
            .threads
            .len(),
        1,
        "the host must remember the thread it launched for the plugin"
    );
    assert!(
        host.settle_turn(&thread_id, Duration::from_secs(10)).await,
        "the driven thread must finish its first turn"
    );
}

/// `alive` 是宿主唯一能回答的并发事实:它必须跟着 `threads` 表走。会话还在
/// → 插件继续占名额;会话从表里消失(结束/被杀) → 下一拍报 `alive: false`,
/// 插件才会释放名额、把卡片标 stale。
///
/// 若这里退化(比如每拍照写 `true`,或干脆不发),死会话会永久占着名额,而
/// 卡片永远显示「正在运行」——正是用户抱怨的那类误报。
#[tokio::test(flavor = "multi_thread")]
async fn a_thread_that_left_the_host_table_is_reported_dead_on_the_next_tick() {
    let workdir = tempfile::TempDir::new().unwrap();
    let work_path = workdir.path().to_string_lossy().to_string();
    let rounds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen_rounds = Arc::clone(&rounds);
    let daemon = FakeDaemon::bind(move |_project, method, _params| match method {
        "driver/next" => {
            if seen_rounds.fetch_add(1, Ordering::SeqCst) > 0 {
                return Ok(json!({ "action": null }));
            }
            Ok(json!({
                "action_id": "a1",
                "kind": "launch",
                "workdir": work_path,
                "title": "driven · card-1",
                "objective": "do the first thing",
            }))
        }
        _ => Ok(json!({ "ok": true })),
    });

    let mut host = Host::new();
    let resident_dir = tempfile::TempDir::new().unwrap();
    install_driver_plugin(&daemon.project, resident_dir.path(), DRIVER_PLUGIN);
    let mut drivers: HashMap<(PathBuf, String), DriverState> = HashMap::new();
    let key = (daemon.project.clone(), DRIVER_PLUGIN.to_string());

    host.tick(resident_dir.path(), &mut drivers, &build_test_agent)
        .await;
    let started = drivers[&key].threads.clone();
    assert_eq!(started.len(), 1, "the launch must be recorded");
    assert!(started[0].alive, "a session we just started is alive");

    // 模拟会话结束:主循环把它从 `threads` 表里摘掉。
    let thread_id = started[0].thread_id.clone();
    host.threads.remove(&thread_id);
    host.tick(resident_dir.path(), &mut drivers, &build_test_agent)
        .await;

    assert_eq!(
        drivers[&key].threads[0].thread_id, thread_id,
        "the host still remembers it is chasing this thread"
    );
    assert!(
        !drivers[&key].threads[0].alive,
        "a thread no longer in the host table must be reported dead"
    );
}

/// 真主循环接线:启动 `serve` 后,3s tick 把插件经 `driver/next` 交出的动作
/// 落成会话——证明驱动器确实挂在主循环上,而不是只在单测里可调。
///
/// 首个 tick 被刻意推迟到 3s(见 `DRIVER_TICK` 的说明),故这里等到 15s。
#[tokio::test(flavor = "multi_thread")]
async fn the_serve_loop_tick_launches_a_driven_session() {
    let workdir_dir = tempfile::TempDir::new().unwrap();
    let workdir = workdir_dir.path().canonicalize().unwrap();
    let work_path = workdir.to_string_lossy().to_string();
    let rounds = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen_rounds = Arc::clone(&rounds);
    let daemon = FakeDaemon::bind(move |_project, method, _params| match method {
        "driver/next" => {
            if seen_rounds.fetch_add(1, Ordering::SeqCst) > 0 {
                return Ok(serde_json::Value::Null);
            }
            Ok(json!({
                "action_id": "a1",
                "kind": "launch",
                "workdir": work_path,
                "title": "driven · card-1",
                "objective": "do the first thing",
            }))
        }
        _ => Ok(json!({ "ok": true })),
    });

    let mut h = crate::server::tests::Harness::new();
    // 发现环的两个通用来源:常驻登记表(harness 的隔离目录)与插件清单
    // (项目自己的 `.yi-agent/supervisors/`)。
    install_driver_plugin(&daemon.project, h.resident_dir.path(), DRIVER_PLUGIN);
    crate::server::tests::initialize(&mut h).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut launched = false;
    while tokio::time::Instant::now() < deadline {
        let asked = !daemon.calls_to("driver/next").is_empty();
        let sessions = crate::thread_store::ThreadStore::new(&workdir)
            .list()
            .unwrap_or_default();
        if asked && !sessions.is_empty() {
            launched = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        launched,
        "the serve tick must drive the plugin and start the session it asks for; \
         plugin calls so far: {:?}",
        daemon.calls()
    );

    h.shutdown().await;
}

/// 构造一个**新建会话**动作,并走真正的 `SessionLauncher` 入口。
///
/// `origin` 是插件的归属值（`driver_project`, `driver_ref`）；传 `None` 复刻旧
/// 插件（不发这两个键）的兼容路径，传 `Some` 复刻新版插件。
async fn launch_via_driver<F>(
    launcher: &mut ServeLauncher<'_, F>,
    workdir: &str,
    title: &str,
    objective: &str,
    origin: (Option<&str>, Option<&str>),
) -> anyhow::Result<String>
where
    F: Fn(
        Option<yi_agent_core::Session>,
        &Path,
        crate::thread_store::ThreadMode,
    ) -> anyhow::Result<BuiltAgent>,
{
    SessionLauncher::launch(
        launcher,
        &DriverAction {
            action_id: "act-launch".into(),
            kind: DriverActionKind::Launch {
                workdir: workdir.to_string(),
                title: title.to_string(),
                objective: objective.to_string(),
                driver_project: origin.0.map(str::to_string),
                driver_ref: origin.1.map(str::to_string),
                env: Vec::new(),
            },
        },
    )
    .await
}

/// **本任务的核心断言**:一次 `append` 动作必须**复用**已有会话——
/// 不新建 thread,原 thread 上多出一轮;且因为走的是「无响应帧」路径,
/// 只发 thread-keyed 通知。
#[tokio::test(flavor = "multi_thread")]
async fn an_append_action_reuses_the_existing_thread() {
    let mut host = Host::new();
    let workdir = host.workdir.clone();
    let workdir = workdir.to_string_lossy().to_string();
    let mut frames = host
        .hub
        .register(crate::broadcast::ClientId::ws(uuid::Uuid::new_v4()));
    // launcher 的客户端身份就是 `ClientId::local()`(`launch_inner` 里构造),而
    // **响应帧**经 `hub.reply(client, …)` 定向投递——**未登记的 client 会被直接
    // 丢弃**(`Broadcaster::reply` 对未知 id 返回 `Closed`)。所以只注册一个 ws
    // client 的旧版断言,只能证明「广播里没有响应帧」,证明不了「宿主没发响应帧」。
    // 把 local 也登记进来,响应帧才会落进一个真正被观察的队列。
    let mut local_frames = host
        .hub
        .register_reliable(crate::broadcast::ClientId::local());

    // ① 先起一个会话(launch 动作)。
    let thread_id = {
        let mut launcher = host.launcher(&build_test_agent);
        launch_via_driver(
            &mut launcher,
            &workdir,
            "看板 · card-1",
            "do the first thing",
            (None, None),
        )
        .await
        .expect("a healthy factory must start the session")
    };
    assert_eq!(
        host.threads.len(),
        1,
        "the launch registers exactly one thread"
    );
    assert!(
        host.threads
            .get(&thread_id)
            .is_some_and(|s| s.active_turn_id.is_some()),
        "the first turn must be占位 before the append"
    );
    // 收尾首轮:主循环语义是「收 Finished → 清占位」。不这样做,`append`
    // 会撞上 `TurnInProgress`,测的就不是 append 了。
    assert!(
        host.settle_turn(&thread_id, Duration::from_secs(10)).await,
        "the first turn must finish before the append"
    );

    // ② 再追一轮(append 动作):**同一个** thread_id,不新铸 uuid。
    let appended = {
        let mut launcher = host.launcher(&build_test_agent);
        SessionLauncher::launch(
            &mut launcher,
            &DriverAction {
                action_id: "act-append".into(),
                kind: DriverActionKind::Append {
                    thread_id: thread_id.clone(),
                    objective: "now merge it".to_string(),
                },
            },
        )
        .await
        .expect("appending to a live thread must succeed")
    };

    assert_eq!(appended, thread_id, "append must return the same thread");
    assert_eq!(host.threads.len(), 1, "no second thread may appear");
    let session = host.threads.get(&thread_id).expect("the session survives");
    assert!(
        session.active_turn_id.is_some(),
        "the existing thread must have received a new (occupied) turn"
    );

    // 盘上也只有这一个会话:append 没有多写一份 meta。
    let metas = crate::thread_store::ThreadStore::new(Path::new(&workdir))
        .list()
        .unwrap();
    assert_eq!(
        metas.len(),
        1,
        "append must not create a second session on disk"
    );

    // ③ 可见性走 thread-keyed 通知(spec §4.4):append 轮必须与首轮一样,发出
    //    `turn/started`(桌面用它断开 reasoning run 并置 `turnActive`)以及开启项
    //    (用户提问=插件给的 objective)。`turn/started` 是**内容通知**,不是 RPC
    //    响应帧——它经 `broadcast` 扇出,而非 `reply` 定向应答。
    let mut seen = Vec::new();
    while let Ok(v) = frames.try_recv() {
        assert!(
            v.get("method").is_some(),
            "the launcher must emit notifications only, never a response frame: {v}"
        );
        let item = &v["params"]["item"];
        seen.push((
            v["method"].as_str().unwrap_or_default().to_string(),
            item["text"].as_str().map(str::to_string),
        ));
    }
    assert!(
        seen.iter()
            .any(|(method, text)| method == "item/started"
                && text.as_deref() == Some("now merge it")),
        "the appended objective must open the new turn: {seen:?}"
    );
    assert!(
        seen.iter().any(|(method, _)| method == "turn/started"),
        "the append must announce the turn it starts (spec §4.4 visibility): {seen:?}"
    );

    // ④ 现在这个断言才真的能证明「没有响应帧」:launcher 的客户端身份是
    //    `ClientId::local()`,上面已把它登记进 hub。**响应帧**经 `hub.reply`
    //    定向投递,未登记的 client 会被静默丢弃——旧版只注册 ws client,响应帧
    //    根本不会出现在任何被观察的队列里,那句断言其实只覆盖了广播。
    //
    //    这里对 local 队列用「有没有 `id`」来判:响应信封(`ResponseEnvelope`)
    //    必带 `id` 且无 `method`,通知信封反之。local 同时也会收到广播,所以不能
    //    断言它「空」——但可以断言它里面**没有一帧是响应**。
    let mut local_seen = Vec::new();
    while let Ok(v) = local_frames.try_recv() {
        local_seen.push(v);
    }
    assert!(
        local_seen.iter().all(|v| v.get("id").is_none()),
        "the launcher must not answer any RPC client: a response frame reached local: {local_seen:?}"
    );

    // 正向对照(判别力证明):这条观察通道**确实**看得见响应帧。手工写一帧响应
    // 给 local,它必须落到同一个队列里——否则上面的断言就是在断言「没观察」。
    //
    // 先把队列排空:被追加的那一轮还在跑,它的 thread-keyed 广播帧会随时插进
    // local 的队列;不清空的话,下面取到的「第一帧」可能是一条通知,这条对照
    // 就会因时序而偶发失败。
    while local_frames.try_recv().is_ok() {}
    write_response(
        &host.hub,
        &crate::broadcast::ClientId::local(),
        ok_response(RequestId::Num(1), json!({})),
    )
    .await
    .expect("the observed local client is registered, so the reply must land");
    // 排空只清掉「此刻已在队列里」的帧,写入之后仍可能有广播抢在响应前面落地,
    // 故不取「下一帧」,而是有界地找那帧带 `id` 的响应。
    let mut probe = None;
    for _ in 0..64 {
        match local_frames.try_recv() {
            Ok(frame) if frame.get("id").is_some() => {
                probe = Some(frame);
                break;
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    let probe = probe.expect("a written response must land in the observed local queue");
    assert!(
        probe.get("method").is_none(),
        "the control frame must look like a response, not a notification: {probe}"
    );

    // 收尾这一轮,确认它真的被 driver 消费(而不只是占了个位)。
    assert!(
        host.settle_turn(&thread_id, Duration::from_secs(10)).await,
        "the appended turn must run and finish"
    );
    assert!(
        host.is_idle(&thread_id),
        "after the appended turn settles the thread is idle again"
    );
}

/// **追问轮撞上忙会话**(spec §10 的 append TOCTOU 行):宿主必须干净地拒绝,
/// 且**不得碰**那个活 turn 的任何记账。
///
/// 这是 Finding 1 的回归测试。旧实现在 `prepare_and_deliver` 的失败分支里清
/// `active_turn_id`,而 `prepare_turn_core` 只在**最后一步**才写占位——失败返回
/// 全都发生在那之前,故那次清零只能清掉**别人的**占位:这里的活 turn 会因此
/// 变成无主 turn(`turn/cancel` 静默失效、`is_idle` 谎报空闲),与 spec §10
/// 「`ok:false`、零副作用」相悖。
///
/// 用 `build_slow_agent`:`SlowProvider` 的 delta 流不收敛,首轮**永不结束**,
/// 于是「有活 turn」是这一条测试真实造出来的状态(而非靠手写占位)。
#[tokio::test(flavor = "multi_thread")]
async fn an_append_into_a_busy_thread_fails_without_touching_the_live_turn() {
    let mut host = Host::new();
    let workdir = host.workdir.clone();
    let workdir = workdir.to_string_lossy().to_string();

    // ① 起一个会话,它的首轮**一直在跑**(slow provider)。
    let thread_id = {
        let mut launcher = host.launcher(&build_slow_agent);
        launch_via_driver(
            &mut launcher,
            &workdir,
            "看板 · card-1",
            "run forever",
            (None, None),
        )
        .await
        .expect("a healthy factory must start the session")
    };
    let live_turn = host
        .threads
        .get(&thread_id)
        .and_then(|s| s.active_turn_id.clone())
        .expect("the slow first turn must be占位");
    assert!(
        !host.is_idle(&thread_id),
        "a thread with a live turn must not be reported idle"
    );

    // ② 现在对它做一次 `append`:必须失败,而且是「turn 撞车」这个具体原因。
    let error = {
        let mut launcher = host.launcher(&build_slow_agent);
        SessionLauncher::launch(
            &mut launcher,
            &DriverAction {
                action_id: "act-append".into(),
                kind: DriverActionKind::Append {
                    thread_id: thread_id.clone(),
                    objective: "a second round".to_string(),
                },
            },
        )
        .await
        .expect_err("an append into a busy thread must be refused, not queued")
    };
    assert!(
        error.to_string().contains("turn already in progress"),
        "the plugin must learn the real reason (busy, not unknown thread): {error}"
    );

    // ③ **本测试的核心**:那个活 turn 的记账原封不动。
    let session = host.threads.get(&thread_id).expect("the session survives");
    assert_eq!(
        session.active_turn_id.as_deref(),
        Some(live_turn.as_str()),
        "a refused prepare must not untrack the live turn it never owned"
    );
    assert!(
        !host.is_idle(&thread_id),
        "the busy session must still be reported busy afterwards"
    );
    assert_eq!(host.threads.len(), 1, "no session may be created or lost");

    // ④ 被拒的那次调用**没有**为该 thread 开新的一轮(没有第二个占位)。
    //    只此一处活跃 turn id,且仍是原来那个。
    assert_eq!(
        host.threads
            .get(&thread_id)
            .map(|s| s.active_turn_id.clone()),
        Some(Some(live_turn)),
        "exactly the original live turn remains"
    );
}

/// 未知 thread 的 `append` 必须**明确失败**、且不留下任何副作用:插件据此
/// 决定重试或放弃(`{ok:false, error}`),宿主不得替它改写状态。
#[tokio::test(flavor = "multi_thread")]
async fn an_append_to_an_unknown_thread_fails_without_side_effects() {
    let mut host = Host::new();
    let mut frames = host
        .hub
        .register(crate::broadcast::ClientId::ws(uuid::Uuid::new_v4()));

    let mut launcher = host.launcher(&build_test_agent);
    let error = SessionLauncher::launch(
        &mut launcher,
        &DriverAction {
            action_id: "act-append".into(),
            kind: DriverActionKind::Append {
                thread_id: "thread-does-not-exist".into(),
                objective: "merge it".to_string(),
            },
        },
    )
    .await
    .expect_err("an unknown thread must not be silently accepted");

    assert_eq!(
        error.to_string(),
        "unknown thread: thread-does-not-exist",
        "the error must name the thread the plugin asked about"
    );
    assert!(host.threads.is_empty(), "no session may be created");
    assert!(
        frames.try_recv().is_err(),
        "a refused append must not emit any frame"
    );
}

/// `driver/next` 的 `launch` 动作带上插件的归属值（`driver_project` 项目根 /
/// `driver_ref` 驱动方句柄，spec §5.5），宿主必须把它们**原样落进 meta**：
/// 少 `driver_project`，`thread/listAll` 就把它折不出项目组；少 `driver_ref`，
/// 桌面侧栏就不收它——这正是「被驱动会话在侧栏不可见」的根因。这里同时钉住
/// 「会话仍然可见、首轮仍然投递」。
#[tokio::test(flavor = "multi_thread")]
async fn a_launch_action_records_the_driver_origin_and_still_starts_a_turn() {
    let mut host = Host::new();
    let workdir = host.workdir.clone();
    let workdir = workdir.to_string_lossy().to_string();
    let thread_id = {
        let mut launcher = host.launcher(&build_test_agent);
        launch_via_driver(
            &mut launcher,
            &workdir,
            "看板 · card-1",
            "do the first thing",
            (Some("/proj"), Some("card-1")),
        )
        .await
        .expect("a healthy factory must start the session")
    };

    let metas = crate::thread_store::ThreadStore::new(Path::new(&workdir))
        .list()
        .unwrap();
    assert_eq!(metas.len(), 1);
    assert_eq!(
        metas[0].driver_ref.as_deref(),
        Some("card-1"),
        "归属句柄必须落进 meta，否则桌面侧栏收不到这条会话"
    );
    assert_eq!(
        metas[0].driver_project.as_deref(),
        Some("/proj"),
        "归属项目根必须落进 meta，否则 listAll 折不进项目组"
    );
    assert_eq!(
        metas[0].title.as_deref(),
        Some("看板 · card-1"),
        "the plugin's title still lands in the meta"
    );
    assert!(
        host.threads
            .get(&thread_id)
            .is_some_and(|s| s.active_turn_id.is_some()),
        "the first turn must still be occupied"
    );
    assert!(
        host.settle_turn(&thread_id, Duration::from_secs(10)).await,
        "the driver must consume the delivered objective"
    );
}
