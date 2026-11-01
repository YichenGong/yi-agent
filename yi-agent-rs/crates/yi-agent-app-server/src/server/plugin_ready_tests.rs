use super::*;

fn manifest(query_socket: Option<&str>) -> yi_agent_supervisors::manifest::SupervisorManifest {
    let socket = query_socket
        .map(|value| format!(r#","query_socket":"{value}""#))
        .unwrap_or_default();
    yi_agent_supervisors::manifest::SupervisorManifest::parse(&format!(
        r#"{{"name":"demo","command":"/bin/true","switch_key":"demo_on"{socket}}}"#
    ))
    .unwrap()
}

/// 没有查询通道的清单立刻算就绪：没有东西要等。
#[test]
fn a_manifest_without_a_query_socket_is_ready_at_once() {
    let dir = tempfile::TempDir::new().unwrap();
    let probes = std::sync::atomic::AtomicUsize::new(0);
    let ready = plugin_query_socket_ready(dir.path(), &manifest(None), &|_socket| {
        probes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        false
    });
    assert!(ready);
    assert_eq!(
        probes.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "nothing to wait for means no connect attempt"
    );
}

/// 深路径走回退规则：探测的必须是回退后的短路径，而不是插件 bind 不上的长路径。
/// 两边（supervisor 与插件）用同一条规则，这里钉的是「宿主探的就是插件会 bind 的」。
#[test]
fn a_deep_socket_path_is_probed_at_the_fallback_location() {
    let deep = std::path::PathBuf::from("/tmp")
        .join("x".repeat(120))
        .join("proj");
    let manifest = manifest(Some("{state_dir}/demo.sock"));
    let probed = std::sync::Mutex::new(None::<std::path::PathBuf>);
    plugin_query_socket_ready(&deep, &manifest, &|socket| {
        *probed.lock().unwrap() = Some(socket.to_path_buf());
        true
    });
    let probed = probed
        .into_inner()
        .unwrap()
        .expect("the deep path must still be probed");
    assert!(
        probed.as_os_str().len() <= yi_agent_supervisors::manifest::MAX_SOCKET_PATH_BYTES,
        "must probe the fallback, got {}",
        probed.display()
    );
}

/// 晚到的监听者必须被观察到：这正是 install 后立刻 query 的那个竞态。
#[test]
fn a_listener_that_appears_late_is_observed() {
    let dir = tempfile::TempDir::new().unwrap();
    // 与 supervisor 的布局规则一致：`{state_dir}` = `<workdir>/.yi-agent/<name>`。
    let state_dir = dir.path().join(".yi-agent").join("demo");
    std::fs::create_dir_all(&state_dir).unwrap();
    let socket = state_dir.join("demo.sock");
    let manifest = manifest(Some("{state_dir}/demo.sock"));
    let project = dir.path().to_path_buf();

    // 插件进程「稍后才 bind」：先让等待方开始轮询，再起监听者。
    let late_socket = socket.clone();
    let late = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(120));
        let listener = std::os::unix::net::UnixListener::bind(&late_socket).unwrap();
        // 保持监听直到等待方已经连上；随后释放。
        std::thread::sleep(Duration::from_millis(600));
        drop(listener);
    });

    let started = std::time::Instant::now();
    let ready = wait_for_plugin_query_socket_within(&project, &manifest, Duration::from_secs(3));
    late.join().unwrap();

    assert!(ready, "a listener that appeared late must be observed");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "must return as soon as it connects, took {:?}",
        started.elapsed()
    );
}

/// 真的轮询：socket 文件一直是陈旧的（无监听者），必须在期限内回 false 而不是挂住。
#[test]
fn a_never_appearing_listener_times_out_without_hanging() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("state")).unwrap();
    // 清单用 `{state_dir}` 指向真实目录，但那里没有监听者。
    let manifest = {
        let json = r#"{"name":"demo","command":"/bin/true","switch_key":"demo_on","query_socket":"{state_dir}/demo.sock"}"#;
        yi_agent_supervisors::manifest::SupervisorManifest::parse(json).unwrap()
    };
    let started = std::time::Instant::now();
    let ready =
        wait_for_plugin_query_socket_within(dir.path(), &manifest, Duration::from_millis(150));
    assert!(!ready);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the wait must be bounded, took {:?}",
        started.elapsed()
    );
}
