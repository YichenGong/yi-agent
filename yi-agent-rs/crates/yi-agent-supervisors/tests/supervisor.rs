use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

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

fn write_manifest(dir: &Path, name: &str, command: &Path, switch_key: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let manifest = format!(
        r#"{{"name":"{name}","command":"{}","args":[],"switch_key":"{switch_key}","restart_backoff_ms":50,"restart_backoff_max_ms":200}}"#,
        command.display()
    );
    std::fs::write(dir.join(format!("{name}.json")), manifest).unwrap();
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

#[test]
fn the_layout_state_dir_uses_the_superpowers_kanban_name() {
    let dir = tempfile::tempdir().unwrap();
    let layout = Layout::for_workdir(dir.path());
    assert_eq!(
        layout.state_dir,
        dir.path().join(".yi-agent/superpowers-kanban")
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
    let state_dir = layout.state_dir.clone();

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
