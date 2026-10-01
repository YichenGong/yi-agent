use std::path::Path;
use std::time::{Duration, Instant};

use yi_agent_supervisors::supervisor::{Layout, Supervisor};

/// 写一个假子进程：它在 `marker` 处写自己的 pid，然后睡到被杀。
fn write_fake_child(dir: &Path, marker: &Path) -> std::path::PathBuf {
    let script = dir.join("child.sh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\necho $$ > {}\nsleep 30\n", marker.display()),
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

fn wait_for(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[test]
fn a_switch_on_spawns_the_child() {
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let marker = workdir.join("child.pid");
    let child = write_fake_child(workdir, &marker);
    let layout = layout_for(workdir);
    write_manifest(&layout.manifests_dir(), "demo", &child, "demo_on");
    // 打开项目层开关
    std::fs::create_dir_all(workdir.join(".yi-agent")).unwrap();
    std::fs::write(
        workdir.join(".yi-agent/preferences.json"),
        r#"{"demo_on":true}"#,
    )
    .unwrap();

    let mut supervisor = Supervisor::new(layout);
    supervisor.reconcile();
    assert!(
        wait_for(&marker, Duration::from_secs(2)),
        "child should start"
    );
    supervisor.stop_all();
}

#[test]
fn a_switch_off_keeps_the_child_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let marker = workdir.join("child.pid");
    let child = write_fake_child(workdir, &marker);
    let layout = layout_for(workdir);
    write_manifest(&layout.manifests_dir(), "demo", &child, "demo_on");

    let mut supervisor = Supervisor::new(layout);
    supervisor.reconcile();
    std::thread::sleep(Duration::from_millis(150));
    assert!(
        !marker.exists(),
        "child must not start while the switch is off"
    );
    supervisor.stop_all();
}

#[test]
fn turning_the_switch_off_stops_a_running_child() {
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let marker = workdir.join("child.pid");
    let child = write_fake_child(workdir, &marker);
    let layout = layout_for(workdir);
    write_manifest(&layout.manifests_dir(), "demo", &child, "demo_on");
    std::fs::create_dir_all(workdir.join(".yi-agent")).unwrap();
    let prefs = workdir.join(".yi-agent/preferences.json");
    std::fs::write(&prefs, r#"{"demo_on":true}"#).unwrap();

    let mut supervisor = Supervisor::new(layout);
    supervisor.reconcile();
    assert!(
        wait_for(&marker, Duration::from_secs(2)),
        "child should start"
    );

    std::fs::write(&prefs, r#"{"demo_on":false}"#).unwrap();
    supervisor.reconcile();
    assert!(
        supervisor.running_count() == 0,
        "turning the switch off must stop the child"
    );
}

#[test]
fn a_crashed_child_is_restarted_after_the_backoff() {
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
    std::fs::create_dir_all(workdir.join(".yi-agent")).unwrap();
    std::fs::write(
        workdir.join(".yi-agent/preferences.json"),
        r#"{"demo_on":true}"#,
    )
    .unwrap();

    let mut supervisor = Supervisor::new(layout);
    let deadline = Instant::now() + Duration::from_secs(3);
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
    let dir = tempfile::tempdir().unwrap();
    let workdir = dir.path();
    let marker = workdir.join("child.pid");
    let child = write_fake_child(workdir, &marker);
    let layout = layout_for(workdir);
    write_manifest(&layout.manifests_dir(), "demo", &child, "demo_on");
    std::fs::create_dir_all(workdir.join(".yi-agent")).unwrap();
    std::fs::write(
        workdir.join(".yi-agent/preferences.json"),
        r#"{"demo_on":true}"#,
    )
    .unwrap();

    let mut supervisor = Supervisor::new(layout);
    supervisor.reconcile();
    assert!(wait_for(&marker, Duration::from_secs(2)));
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
    if !wait_for(&marker, Duration::from_secs(5)) {
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
