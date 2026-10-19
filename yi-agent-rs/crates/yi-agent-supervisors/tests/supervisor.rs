use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use yi_agent_supervisors::manifest::SupervisorManifest;
use yi_agent_supervisors::supervisor::{Layout, Supervisor};

/// Serializes the tests in this file that spawn a child and then wait for it.
///
/// The test harness runs the tests in a file in parallel (one thread per test,
/// up to the core count). With several `/bin/sh` children forked at once, a
/// loaded machine can take seconds to schedule any of them: measured marker
/// delays crossed 3s with eight concurrent spawns, well past the old fixed 2s
/// budget — that is what made these tests flaky. One spawn at a time keeps the
/// delay near half a second. The wide budget in [`wait_until`] is the backstop
/// for load this lock cannot see (other test binaries running under
/// `cargo test --workspace`, CI neighbours).
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

/// Hold this for the whole test body; it releases on drop.
fn spawn_serial_guard() -> MutexGuard<'static, ()> {
    // Tolerate poisoning: one test panicking must not turn every later test
    // into a confusing PoisonError.
    SPAWN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Budget for "wait until the child has done something".
///
/// A wide safety net, not a guess. A fixed 2s encoded an assumption about how
/// fast the OS starts a shell; under load that assumption is false, and the
/// test flakes. We poll for the real condition and give it far more room than
/// any plausible scheduling delay, so only a genuine bug (the child never runs)
/// reaches the deadline.
const CHILD_START_BUDGET: Duration = Duration::from_secs(20);

/// Poll `condition` every 10ms until it holds or the budget runs out.
fn wait_until(condition: impl Fn() -> bool, what: &str) -> bool {
    let deadline = Instant::now() + CHILD_START_BUDGET;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            eprintln!("timed out after {CHILD_START_BUDGET:?} waiting for {what}");
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The pid the fake child echoed into `marker`, once it is there in full.
fn marker_pid(marker: &Path) -> Option<u32> {
    std::fs::read_to_string(marker).ok()?.trim().parse().ok()
}

/// Wait until the child has written its pid marker.
///
/// The condition is the child's own output, not elapsed time. Requiring a
/// numeric pid (not merely `path.exists()`) means a stray or half-written file
/// cannot satisfy the wait — the child process must actually have run.
fn wait_for_pid_marker(marker: &Path) -> bool {
    wait_until(|| marker_pid(marker).is_some(), "the child's pid marker")
}

/// 写一个假子进程：它把 pid 原子地写进 `marker`，然后睡到被杀。
///
/// The write goes through a temp file + rename so a reader never observes a
/// partially written marker: `marker` either is absent or holds a full pid.
fn write_fake_child(dir: &Path, marker: &Path) -> std::path::PathBuf {
    let script = dir.join("child.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > {m}.tmp\nmv {m}.tmp {m}\nsleep 30\n",
            m = marker.display()
        ),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();
    script
}

/// Write a manifest for a fake child. `extra` is appended to the JSON object,
/// which keeps the pinned tests' shape while letting a caller add fields such as
/// `query_socket`.
fn write_manifest_json(dir: &Path, name: &str, command: &Path, switch_key: &str, extra: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let manifest = format!(
        r#"{{"name":"{name}","command":"{}","args":[],"switch_key":"{switch_key}","restart_backoff_ms":50,"restart_backoff_max_ms":200{extra}}}"#,
        command.display()
    );
    std::fs::write(dir.join(format!("{name}.json")), manifest).unwrap();
}

fn write_manifest(dir: &Path, name: &str, command: &Path, switch_key: &str) {
    write_manifest_json(dir, name, command, switch_key, "");
}

/// Same as [`write_manifest`], but the manifest also declares `query_socket`, and
/// the *parsed* manifest is returned so the test can resolve the socket with the
/// exact same call the supervisor makes (`query_socket_path`) instead of
/// hand-writing a path the implementation might not probe.
fn write_manifest_with_query_socket(
    dir: &Path,
    name: &str,
    command: &Path,
    switch_key: &str,
    query_socket: &str,
) -> SupervisorManifest {
    write_manifest_json(
        dir,
        name,
        command,
        switch_key,
        &format!(r#","query_socket":"{query_socket}""#),
    );
    let text = std::fs::read_to_string(dir.join(format!("{name}.json"))).unwrap();
    SupervisorManifest::parse(&text).unwrap()
}

/// The socket path the supervisor will probe for `manifest` in `layout`.
///
/// Deliberately routed through the production resolver: the listener must be
/// bound exactly where the probe connects, and `query_socket_path` may apply a
/// length fallback, so a hand-written guess would be able to pass while the
/// real contract is broken.
fn probe_path(manifest: &SupervisorManifest, layout: &Layout) -> PathBuf {
    manifest
        .query_socket_path(
            &layout.workdir,
            &layout.state_dir(&manifest.name),
            &layout.runtime_dir,
        )
        .expect("manifest declares a query_socket")
        .expect("tempdir path is short enough to stay as declared")
}

fn layout_for(workdir: &Path) -> Layout {
    Layout::for_workdir(workdir)
}

fn enable(workdir: &Path, switch_key: &str) {
    std::fs::create_dir_all(workdir.join(".yi-agent")).unwrap();
    std::fs::write(
        workdir.join(".yi-agent/preferences.json"),
        format!(r#"{{"{switch_key}":true}}"#),
    )
    .unwrap();
}

#[test]
fn a_switch_on_spawns_the_child() {
    let _guard = spawn_serial_guard();
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let marker = workdir.join("child.pid");
    let child = write_fake_child(workdir, &marker);
    let layout = layout_for(workdir);
    write_manifest(&layout.manifests_dir(), "demo", &child, "demo_on");
    // 打开项目层开关
    enable(workdir, "demo_on");

    let mut supervisor = Supervisor::new(layout);
    supervisor.reconcile();
    assert!(
        wait_for_pid_marker(&marker),
        "child should start and write its pid"
    );
    supervisor.stop_all();
}

#[test]
fn a_switch_off_keeps_the_child_stopped() {
    let _guard = spawn_serial_guard();
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let marker = workdir.join("child.pid");
    let child = write_fake_child(workdir, &marker);
    let layout = layout_for(workdir);
    write_manifest(&layout.manifests_dir(), "demo", &child, "demo_on");

    let mut supervisor = Supervisor::new(layout);
    supervisor.reconcile();
    // Negative check: we are proving nothing starts, so there is no positive
    // condition to poll for. The wait only has to outlast a spawn that should
    // not happen; with the switch off, `reconcile` never spawns at all.
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        !marker.exists(),
        "child must not start while the switch is off"
    );
    supervisor.stop_all();
}

#[test]
fn turning_the_switch_off_stops_a_running_child() {
    let _guard = spawn_serial_guard();
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let marker = workdir.join("child.pid");
    let child = write_fake_child(workdir, &marker);
    let layout = layout_for(workdir);
    write_manifest(&layout.manifests_dir(), "demo", &child, "demo_on");
    let prefs = workdir.join(".yi-agent/preferences.json");
    enable(workdir, "demo_on");

    let mut supervisor = Supervisor::new(layout);
    supervisor.reconcile();
    assert!(
        wait_for_pid_marker(&marker),
        "child should start and write its pid"
    );

    std::fs::write(&prefs, r#"{"demo_on":false}"#).unwrap();
    supervisor.reconcile();
    assert!(
        supervisor.running_count() == 0,
        "turning the switch off must stop the child"
    );
}

#[test]
fn a_manifest_that_must_stay_reachable_runs_even_while_disabled() {
    let _guard = spawn_serial_guard();
    // `stop_when_disabled:false` 的进程即便开关关着也必须在跑：它声明了自己是
    // 查询通道，而通道是唯一能报告开关、并把开关再打开的东西。把它停掉，"关"
    // 就变成单向门——桌面端正是这样卡在「插件未安装」上的。
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let marker = workdir.join("child.pid");
    let child = write_fake_child(workdir, &marker);
    let layout = layout_for(workdir);
    std::fs::create_dir_all(layout.manifests_dir()).unwrap();
    std::fs::write(
        layout.manifests_dir().join("reachable.json"),
        format!(
            r#"{{"name":"reachable","command":"{}","args":[],"switch_key":"demo_on","stop_when_disabled":false}}"#,
            child.display()
        ),
    )
    .unwrap();
    // 开关缺省即关闭；没有 preferences.json。

    let mut supervisor = Supervisor::new(layout);
    supervisor.reconcile();
    assert!(wait_for_pid_marker(&marker), "必须起，否则没法再把开关打开");
    supervisor.stop_all();
}

#[test]
fn a_crashed_child_is_restarted_after_the_backoff() {
    let _guard = spawn_serial_guard();
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let counter = workdir.join("starts");
    // 每次启动把计数 +1，然后立即退出（模拟崩溃）。
    let child = workdir.join("crash.sh");
    std::fs::write(
        &child,
        format!("#!/bin/sh\nprintf x >> {}\nexit 1\n", counter.display()),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&child).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&child, perms).unwrap();
    }
    let layout = layout_for(workdir);
    write_manifest(&layout.manifests_dir(), "demo", &child, "demo_on");
    enable(workdir, "demo_on");

    let mut supervisor = Supervisor::new(layout);
    // Condition-based: poll until the child has crashed-and-restarted three
    // times, with the shared wide budget rather than a tight 3s cap that a
    // loaded machine can miss.
    let deadline = Instant::now() + CHILD_START_BUDGET;
    while Instant::now() < deadline {
        supervisor.reconcile();
        std::thread::sleep(Duration::from_millis(20));
        if std::fs::read_to_string(&counter)
            .map(|s| s.len())
            .unwrap_or(0)
            >= 3
        {
            break;
        }
    }
    supervisor.stop_all();
    let starts = std::fs::read_to_string(&counter).unwrap_or_default().len();
    assert!(
        starts >= 3,
        "a crashing child must be restarted, got {starts}"
    );
}

#[test]
fn stop_all_reaps_every_child() {
    let _guard = spawn_serial_guard();
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let marker = workdir.join("child.pid");
    let child = write_fake_child(workdir, &marker);
    let layout = layout_for(workdir);
    write_manifest(&layout.manifests_dir(), "demo", &child, "demo_on");
    enable(workdir, "demo_on");

    let mut supervisor = Supervisor::new(layout);
    supervisor.reconcile();
    assert!(wait_for_pid_marker(&marker));
    supervisor.stop_all();
    assert_eq!(supervisor.running_count(), 0);
}

/// 磁盘契约：清单名 `superpowers-kanban` 必须派生出与通用化之前**逐字相同**的
/// 目录，否则既有安装的清单（含插件据此反推的项目根与偏好文件）会指错地方。
#[test]
fn the_layout_state_dir_follows_the_manifest_name_and_keeps_the_contract() {
    let dir = tempfile::tempdir().unwrap();
    let layout = Layout::for_workdir(dir.path());
    assert_eq!(
        layout.state_dir("superpowers-kanban"),
        dir.path().join(".yi-agent/superpowers-kanban"),
        "the on-disk contract must not move"
    );
    assert_ne!(
        layout.state_dir("superpowers-kanban"),
        layout.state_dir("some-other-plugin"),
        "each manifest name gets its own directory"
    );
}

/// 深项目路径下，转发表必须给出**插件真正 bind 的那个位置**。
///
/// 这条把两端串起来：宿主按契约规则解析，插件在同一位置 bind。
/// 只要有一侧规则漂移，这里就对不上——比两端各自单测更有价值。
#[test]
fn a_deep_workdir_publishes_the_path_the_plugin_actually_binds() {
    let _guard = spawn_serial_guard();
    let dir = tempfile::tempdir().unwrap();
    // 构造一个足够深的 workdir，让 <state_dir>/superpowers-kanban.sock 越界。
    let long = "a-rather-long-segment".repeat(3);
    let workdir = dir.path().join(long).join("deep-project");
    std::fs::create_dir_all(&workdir).unwrap();

    let marker = workdir.join("child.pid");
    let child = write_fake_child(&workdir, &marker);
    let layout = layout_for(&workdir);
    let state_dir = layout.state_dir("superpowers-kanban");

    // 清单声明查询 socket（与真实清单同款模板），并打开开关。
    std::fs::create_dir_all(layout.manifests_dir()).unwrap();
    std::fs::write(
        layout.manifests_dir().join("superpowers-kanban.json"),
        format!(
            r#"{{"name":"superpowers-kanban","command":"{}","args":[],"switch_key":"superpowers_kanban","query_socket":"{{state_dir}}/superpowers-kanban.sock"}}"#,
            child.display()
        ),
    )
    .unwrap();
    std::fs::write(
        workdir.join(".yi-agent/preferences.json"),
        r#"{"superpowers_kanban":true}"#,
    )
    .unwrap();

    let direct = state_dir.join("superpowers-kanban.sock");
    assert!(
        direct.as_os_str().len() > 103,
        "precondition: 直接路径必须越界，实际 {} 字节：{}",
        direct.as_os_str().len(),
        direct.display()
    );

    let mut supervisor = Supervisor::new(layout_for(&workdir));
    supervisor.reconcile();
    if !wait_for_pid_marker(&marker) {
        supervisor.stop_all();
        panic!("子进程没起来");
    }

    let sockets = supervisor.query_sockets();
    supervisor.stop_all();

    let (name, path) = sockets
        .iter()
        .find(|(name, _)| name == "superpowers-kanban")
        .unwrap_or_else(|| panic!("转发表里没有这个插件：{sockets:?}"));

    assert_eq!(name, "superpowers-kanban");
    assert!(
        path.as_os_str().len() <= 103,
        "转发表的路径必须能 bind，实际 {} 字节：{}",
        path.as_os_str().len(),
        path.display()
    );
    assert!(
        !path.starts_with(&state_dir),
        "深路径下必须挪出 state_dir，否则插件 bind 不上"
    );
    // 从第一性原理独立推导期望名（不复用被测代码）：纯 sha256 + 前缀。
    // 这是第三种实现，任一侧漂移都会与它对不上。
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(direct.as_os_str().as_encoded_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let expected = format!("plugin-{}.sock", &hex[..16]);

    assert_eq!(
        path.file_name().unwrap().to_string_lossy(),
        expected,
        "宿主转发表与插件的规则不一致"
    );
}

/// A live listener on the declared query socket means an instance already
/// answers, so the supervisor must not spawn a duplicate.
///
/// This is the whole point of probing before spawning: a *healthy orphan* holds
/// the socket, and without the early return the supervisor would keep adding a
/// second instance on every reconcile pass. The other tests in this file declare
/// no `query_socket`, so `query_socket_path` returns `None`, `answers()` is
/// always `false`, and that branch never runs. Here the socket is bound for
/// real, so the probe is exercised end to end: the switch is ON and the child
/// would start were it not for the probe.
#[test]
fn a_live_listener_on_the_query_socket_suppresses_the_spawn() {
    let _guard = spawn_serial_guard();
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let marker = workdir.join("child.pid");
    let child = write_fake_child(workdir, &marker);
    let layout = layout_for(workdir);
    let manifest = write_manifest_with_query_socket(
        &layout.manifests_dir(),
        "demo",
        &child,
        "demo_on",
        "{workdir}/.yi-agent/query.sock",
    );
    enable(workdir, "demo_on");

    // Bind exactly where the supervisor probes. Create the parent first: `bind`
    // does not create directories.
    let socket = probe_path(&manifest, &layout);
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();

    let mut supervisor = Supervisor::new(layout_for(workdir));
    // Negative check, so no positive condition to poll for. Reconcile repeatedly
    // over a window far longer than a healthy `/bin/sh` child needs to write its
    // marker (well under 100ms): a spawn would have landed by now.
    for _ in 0..12 {
        supervisor.reconcile();
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !marker.exists(),
        "a live listener on the query socket must suppress the spawn, but a child started"
    );
    assert_eq!(
        supervisor.running_count(),
        0,
        "nothing may be recorded as running when an instance already answers"
    );
    supervisor.stop_all();
}

// --- 子进程日志（`Layout::logs_dir` / `ChildLog`） ---

#[test]
fn a_childs_stderr_lands_in_its_log_file() {
    let _guard = spawn_serial_guard();
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    // 子进程往 **stderr** 写一个独特标记，另往 stdout 写一个，用来证明只有
    // stderr 被接进日志（stdout 仍被吞掉）。
    let child = workdir.join("noisy.sh");
    std::fs::write(
        &child,
        "#!/bin/sh\nprintf 'child-stdout-marker\\n'\nprintf 'child-stderr-marker\\n' 1>&2\nsleep 30\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&child).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&child, perms).unwrap();
    }
    let layout = layout_for(workdir);
    write_manifest(&layout.manifests_dir(), "demo", &child, "demo_on");
    enable(workdir, "demo_on");

    let mut supervisor = Supervisor::new(layout.clone());
    supervisor.reconcile();

    let log = layout.logs_dir().join("demo.log");
    assert!(
        wait_until(
            || std::fs::read_to_string(&log)
                .map(|text| text.contains("child-stderr-marker"))
                .unwrap_or(false),
            "the child's stderr line to reach its log file"
        ),
        "a child's stderr must be recorded, not discarded"
    );

    let text = std::fs::read_to_string(&log).unwrap();
    assert!(
        !text.contains("child-stdout-marker"),
        "stdout must stay swallowed: only stderr is a reporting channel, got: {text:?}"
    );
    supervisor.stop_all();
}

#[test]
fn the_supervisor_records_when_it_starts_a_child() {
    let _guard = spawn_serial_guard();
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let marker = workdir.join("child.pid");
    let child = write_fake_child(workdir, &marker);
    let layout = layout_for(workdir);
    write_manifest(&layout.manifests_dir(), "demo", &child, "demo_on");
    enable(workdir, "demo_on");

    let mut supervisor = Supervisor::new(layout.clone());
    supervisor.reconcile();

    let log = layout.logs_dir().join("demo.log");
    assert!(
        wait_until(
            || std::fs::read_to_string(&log)
                .map(|text| text.contains("started demo") && text.contains("pid"))
                .unwrap_or(false),
            "the supervisor's start event to reach the log"
        ),
        "the log must record the start, including the pid"
    );
    supervisor.stop_all();
}

#[test]
fn removing_a_manifest_records_why_the_child_stopped() {
    let _guard = spawn_serial_guard();
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let marker = workdir.join("child.pid");
    let child = write_fake_child(workdir, &marker);
    let layout = layout_for(workdir);
    write_manifest(&layout.manifests_dir(), "demo", &child, "demo_on");
    enable(workdir, "demo_on");

    let mut supervisor = Supervisor::new(layout.clone());
    supervisor.reconcile();
    assert!(wait_for_pid_marker(&marker), "child should start");

    // 清单消失（插件被卸载）→ 必须记下是**这个**原因，而不是"开关关闭"。
    std::fs::remove_file(layout.manifests_dir().join("demo.json")).unwrap();
    supervisor.reconcile();

    let log = layout.logs_dir().join("demo.log");
    assert!(
        wait_until(
            || std::fs::read_to_string(&log)
                .map(|text| text.contains("manifest removed"))
                .unwrap_or(false),
            "the stop reason to reach the log"
        ),
        "a stop caused by a removed manifest must say so"
    );
}

#[test]
fn switching_off_records_a_different_reason_than_removing_the_manifest() {
    let _guard = spawn_serial_guard();
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let marker = workdir.join("child.pid");
    let child = write_fake_child(workdir, &marker);
    let layout = layout_for(workdir);
    write_manifest(&layout.manifests_dir(), "demo", &child, "demo_on");
    let prefs = workdir.join(".yi-agent/preferences.json");
    enable(workdir, "demo_on");

    let mut supervisor = Supervisor::new(layout.clone());
    supervisor.reconcile();
    assert!(wait_for_pid_marker(&marker), "child should start");

    std::fs::write(&prefs, r#"{"demo_on":false}"#).unwrap();
    supervisor.reconcile();

    let log = layout.logs_dir().join("demo.log");
    assert!(
        wait_until(
            || std::fs::read_to_string(&log)
                .map(|text| text.contains("switch off"))
                .unwrap_or(false),
            "the switch-off reason to reach the log"
        ),
        "a stop caused by the switch must be distinguishable from a removed manifest"
    );
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(
        !text.contains("manifest removed"),
        "the manifest is still there; the reason must not be misattributed: {text:?}"
    );
}

#[test]
fn a_childs_exit_is_recorded_once_not_once_per_tick() {
    let _guard = spawn_serial_guard();
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let child = workdir.join("crash.sh");
    std::fs::write(&child, "#!/bin/sh\nexit 1\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&child).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&child, perms).unwrap();
    }
    let layout = layout_for(workdir);
    std::fs::create_dir_all(layout.manifests_dir()).unwrap();
    // 长退避：整个观察窗口都落在退避里，于是每次 reconcile 都观察到「已退出」。
    // 60s 远大于下面 20 拍的墙钟耗时——CI 上被负载拖慢时 20 x 50ms 可能已经
    // 跑了几秒，2s 的窗口会让子进程合法重启，把这条断言变成时序竞态。
    std::fs::write(
        layout.manifests_dir().join("demo.json"),
        format!(
            r#"{{"name":"demo","command":"{}","args":[],"switch_key":"demo_on","restart_backoff_ms":60000,"restart_backoff_max_ms":60000}}"#,
            child.display()
        ),
    )
    .unwrap();
    enable(workdir, "demo_on");

    let mut supervisor = Supervisor::new(layout.clone());
    // 20 拍 x 50ms 约 1s，全程 < 60s 退避：同一退出事实被观察约 20 次。
    for _ in 0..20 {
        supervisor.reconcile();
        std::thread::sleep(Duration::from_millis(50));
    }

    let log = layout.logs_dir().join("demo.log");
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let count = text.matches("exited demo").count();
    assert_eq!(
        count, 1,
        "the same exit must be recorded once, not once per tick: {text:?}"
    );
    supervisor.stop_all();
}

#[test]
fn an_unwritable_log_does_not_stop_the_child_from_starting() {
    let _guard = spawn_serial_guard();
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let marker = workdir.join("child.pid");
    let child = write_fake_child(workdir, &marker);
    let layout = layout_for(workdir);
    write_manifest(&layout.manifests_dir(), "demo", &child, "demo_on");
    enable(workdir, "demo_on");

    // 让日志路径不可用：`logs` 处放一个**文件**而不是目录，`create_dir_all` 必失败。
    // 守护子进程是本职，记日志是附加——这条路径绝不能阻断 spawn。
    std::fs::write(layout.logs_dir(), "not a directory").unwrap();

    let mut supervisor = Supervisor::new(layout);
    supervisor.reconcile();

    assert!(
        wait_for_pid_marker(&marker),
        "an unusable log path must never stop the child from starting"
    );
    supervisor.stop_all();
}

// --- 转发表对「健康孤儿」的登记（daemon 重启后仍可查询） ---

/// daemon 重启后，上一代 daemon 留下的插件进程仍活着并在 socket 上应答。
///
/// `ensure_running` 见到应答就**故意不 spawn**（避免两个实例抢锁），因此它不会
/// 进入 `running`。若转发表只按 `running` 过滤，这条唯一能回答查询的路由就会被
/// 漏掉，`plugin/query` 会一直回 `plugin_unavailable`，直到那个进程碰巧退出。
#[test]
fn a_live_orphan_that_answers_is_published_after_a_daemon_restart() {
    let _guard = spawn_serial_guard();
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let marker = workdir.join("child.pid");
    let child = write_fake_child(workdir, &marker);
    let layout = layout_for(workdir);
    let manifest = write_manifest_with_query_socket(
        &layout.manifests_dir(),
        "demo",
        &child,
        "demo_on",
        "{workdir}/.yi-agent/query.sock",
    );
    enable(workdir, "demo_on");

    // 模拟「上一代 daemon 已经把它拉起来过」：在探测位置真的 bind。
    let socket = probe_path(&manifest, &layout);
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();

    let mut supervisor = Supervisor::new(layout_for(workdir));
    supervisor.reconcile();

    assert_eq!(
        supervisor.running_count(),
        0,
        "the healthy orphan must not be spawned again"
    );
    assert!(
        !marker.exists(),
        "no child may be started when the orphan already answers"
    );

    let sockets = supervisor.query_sockets();
    let (name, path) = sockets
        .iter()
        .find(|(name, _)| name == "demo")
        .unwrap_or_else(|| {
            panic!("a live orphan must stay queryable after a daemon restart: {sockets:?}")
        });
    assert_eq!(name, "demo");
    assert_eq!(path, &socket);
    supervisor.stop_all();
}

/// 声明了查询通道、但没人在听：不该进转发表。
///
/// 路由的唯一意义是「能连上」；把连不上的路径登进去只会让查询等到超时。
#[test]
fn a_declared_socket_with_no_listener_is_not_published() {
    let _guard = spawn_serial_guard();
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let child = write_fake_child(workdir, &workdir.join("child.pid"));
    let layout = layout_for(workdir);
    // 开关关着，故 reconcile 不会起子进程；socket 也没人 bind。
    let _manifest = write_manifest_with_query_socket(
        &layout.manifests_dir(),
        "demo",
        &child,
        "demo_on",
        "{workdir}/.yi-agent/query.sock",
    );

    let mut supervisor = Supervisor::new(layout_for(workdir));
    supervisor.reconcile();

    assert!(
        supervisor.query_sockets().is_empty(),
        "a socket nobody listens on must not be published"
    );
    supervisor.stop_all();
}

/// 清单没声明查询通道：即使那个路径上碰巧有人在听，也不该登记。
///
/// 路由来自清单的**声明**，不是来自「某个文件恰好存在」——否则宿主就变成了
/// 任意本地 socket 的代理。
#[test]
fn a_manifest_without_a_query_socket_is_never_published() {
    let _guard = spawn_serial_guard();
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let marker = workdir.join("child.pid");
    let child = write_fake_child(workdir, &marker);
    let layout = layout_for(workdir);
    write_manifest(&layout.manifests_dir(), "demo", &child, "demo_on");
    enable(workdir, "demo_on");

    // 在常见位置放一个真在监听的 socket。
    let stray = workdir.join(".yi-agent").join("query.sock");
    std::fs::create_dir_all(stray.parent().unwrap()).unwrap();
    let _listener = std::os::unix::net::UnixListener::bind(&stray).unwrap();

    let mut supervisor = Supervisor::new(layout_for(workdir));
    supervisor.reconcile();
    assert!(wait_for_pid_marker(&marker), "the child should be running");

    assert!(
        supervisor.query_sockets().is_empty(),
        "a manifest that declares no query channel must never be published: {:?}",
        supervisor.query_sockets()
    );
    supervisor.stop_all();
}
