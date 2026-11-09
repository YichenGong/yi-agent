//! 插件侧对公共 daemon socket 契约的断言。
//!
//! 这里不使用任何宿主 crate：插件必须能独立安装。契约数据的唯一权威是
//! `plugins/contracts/fixtures/daemon-socket.json`，宿主侧有同形测试读同一份。

use std::path::{Path, PathBuf};

use serde::Deserialize;
use superpowers_kanban_ipc::daemon_socket::{
    DaemonSocketError, daemon_socket_for, daemon_socket_with_home, instance_home_from_values,
};

const CONTRACT: &str = include_str!("../../../../contracts/fixtures/daemon-socket.json");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Contract {
    #[serde(rename = "_comment")]
    comment: String,
    algorithm: String,
    instance_priority: Vec<String>,
    override_suffix: String,
    socket_directory: String,
    socket_file: String,
    max_socket_path_bytes: usize,
    vectors: Vec<Vector>,
    case_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Vector {
    runtime_hex: String,
    digest16: String,
    socket_hex: String,
}

fn contract() -> Contract {
    let contract: Contract = serde_json::from_str(CONTRACT).expect("contract parses");
    assert!(!contract.comment.is_empty());
    contract
}

fn from_hex(text: &str) -> Vec<u8> {
    assert_eq!(text.len() % 2, 0, "odd hex length: {text}");
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let pair = std::str::from_utf8(pair).unwrap();
            u8::from_str_radix(pair, 16).unwrap()
        })
        .collect()
}

fn short_root(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in("/tmp")
        .expect("short temp root in /tmp")
}

#[test]
fn short_and_deep_use_instance_socket_directory() {
    let home = Path::new("/");
    assert_eq!(
        daemon_socket_with_home(Path::new("/"), home).unwrap(),
        PathBuf::from("/sockets/8a5edab282632443/runtime.sock")
    );

    let root = short_root("kd");
    let canonical_root = std::fs::canonicalize(root.path()).unwrap();
    let h = canonical_root.join("i");
    let mut deep = canonical_root.clone();
    for _ in 0..40 {
        deep.push("deep");
    }
    for runtime in [canonical_root.join("r"), deep.join("r")] {
        let path = daemon_socket_with_home(&runtime, &h).unwrap();
        assert!(path.starts_with(&h.join("sockets")), "{path:?}");
        assert!(path.ends_with("runtime.sock"));
        assert!(path.as_os_str().as_encoded_bytes().len() <= 103);
    }
}

#[test]
fn instance_priority_and_suffix_are_exact() {
    let instance = std::ffi::OsStr::new("/instance/home");
    let override_home = std::ffi::OsStr::new("/override/home");
    let home = std::ffi::OsStr::new("/plain/home");
    let empty = std::ffi::OsStr::new("");

    // 显式三值：INSTANCE 原样返回，即使没带 `.yi-agent` 也不补后缀。
    assert_eq!(
        instance_home_from_values(Some(instance), Some(override_home), Some(home)).unwrap(),
        PathBuf::from("/instance/home")
    );
    // 空 INSTANCE 被跳过，进入 override 分支并补后缀。
    assert_eq!(
        instance_home_from_values(Some(empty), Some(override_home), Some(home)).unwrap(),
        PathBuf::from("/override/home/.yi-agent")
    );
    // 已经以 `.yi-agent` 结尾的 override 不再重复追加。
    assert_eq!(
        instance_home_from_values(
            Some(empty),
            Some(std::ffi::OsStr::new("/override/.yi-agent")),
            Some(home)
        )
        .unwrap(),
        PathBuf::from("/override/.yi-agent")
    );
    // 空 override 被跳过，落到 HOME。
    assert_eq!(
        instance_home_from_values(Some(empty), Some(empty), Some(home)).unwrap(),
        PathBuf::from("/plain/home/.yi-agent")
    );
    // 三者都不可用必须显式报错，不能猜一个地址。
    assert!(matches!(
        instance_home_from_values(Some(empty), Some(empty), Some(empty)),
        Err(DaemonSocketError::InstanceHomeUnavailable)
    ));
    assert!(matches!(
        instance_home_from_values(None, None, None),
        Err(DaemonSocketError::InstanceHomeUnavailable)
    ));
}

#[test]
fn canonical_spelling_and_missing_tail_match() {
    let root = short_root("kd-canon");
    let canonical_root = std::fs::canonicalize(root.path()).unwrap();
    let real = canonical_root.join("real");
    std::fs::create_dir_all(real.join("runtime")).unwrap();
    let link = canonical_root.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    // symlink、真实路径、相对路径 + 点段必须落到同一地址。
    let direct = daemon_socket_with_home(&real.join("runtime"), &canonical_root.join("i")).unwrap();
    let via_link =
        daemon_socket_with_home(&link.join("runtime"), &canonical_root.join("i")).unwrap();
    assert_eq!(direct, via_link);

    let dotted = canonical_root.join("real/./nested/../runtime");
    assert_eq!(
        daemon_socket_with_home(&dotted, &canonical_root.join("i")).unwrap(),
        direct
    );

    // symlink 后再 `..`：必须先词法处理，否则会落到 link 的父目录而非真实的父目录。
    let symlink_dotdot = link.join("..").join("real").join("runtime");
    assert_eq!(
        daemon_socket_with_home(&symlink_dotdot, &canonical_root.join("i")).unwrap(),
        direct
    );

    // 缺失尾部的 runtime 在创建前后解析一致；home 尾部缺失同理。
    let missing = canonical_root.join("missing/runtime");
    let before = daemon_socket_with_home(&missing, &canonical_root.join("new-home")).unwrap();
    std::fs::create_dir_all(&missing).unwrap();
    std::fs::create_dir_all(canonical_root.join("new-home")).unwrap();
    let after = daemon_socket_with_home(&missing, &canonical_root.join("new-home")).unwrap();
    assert_eq!(before, after, "缺失尾部创建前后必须解析一致");
}

#[test]
fn instances_and_runtimes_are_isolated() {
    let root = short_root("kd-iso");
    let canonical_root = std::fs::canonicalize(root.path()).unwrap();
    let runtime = canonical_root.join("r");
    let other_runtime = canonical_root.join("r2");
    let home = canonical_root.join("i");
    let other_home = canonical_root.join("i2");

    let base = daemon_socket_with_home(&runtime, &home).unwrap();
    assert_ne!(
        base,
        daemon_socket_with_home(&other_runtime, &home).unwrap()
    );
    assert_ne!(
        base,
        daemon_socket_with_home(&runtime, &other_home).unwrap()
    );
}

#[test]
fn non_utf8_and_length_are_raw_bytes() {
    use std::os::unix::ffi::OsStringExt;

    let root = short_root("kd-bytes");
    let canonical_root = std::fs::canonicalize(root.path()).unwrap();
    let home = canonical_root.join("i");
    let non_utf8 = canonical_root.join(std::ffi::OsString::from_vec(b"dir-\xff".to_vec()));
    let raw = daemon_socket_with_home(&non_utf8, &home).unwrap();
    assert!(raw.starts_with(home.join("sockets")));
    assert_ne!(
        raw,
        daemon_socket_with_home(&canonical_root.join("dir-?"), &home).unwrap(),
        "非 UTF-8 字节必须原样参与哈希，不能被替换字符吞掉"
    );

    // 精确构造 103 / 104 字节的候选地址。
    //
    // 布局固定：<home>/sockets/<16 hex>/runtime.sock，尾部 9+16+13 = 38 字节。
    // 用 canonical 短根来拼 home，避免 macOS 的 /tmp -> /private/tmp 让长度漂移。
    let limit = contract().max_socket_path_bytes;
    let tail_bytes = "/sockets/".len() + 16 + "/runtime.sock".len();
    let max_home_bytes = limit - tail_bytes;
    let mut home_103 = canonical_root.clone();
    assert!(home_103.as_os_str().as_encoded_bytes().len() < max_home_bytes);
    // 追加一个组件还要多一个分隔符 `/`。
    let pad = max_home_bytes - home_103.as_os_str().as_encoded_bytes().len() - 1;
    home_103.push("p".repeat(pad));
    std::fs::create_dir_all(&home_103).unwrap();
    let home_103 = std::fs::canonicalize(&home_103).unwrap();
    let candidate = daemon_socket_with_home(&canonical_root.join("r"), &home_103).unwrap();
    assert_eq!(candidate.as_os_str().as_encoded_bytes().len(), limit);

    let mut home_104 = home_103.clone();
    let name = home_104.file_name().unwrap().as_encoded_bytes().to_vec();
    let mut widened = name;
    widened.extend_from_slice(b"q");
    home_104.set_file_name(os_string_from_bytes(widened));
    std::fs::create_dir_all(&home_104).unwrap();
    let home_104 = std::fs::canonicalize(&home_104).unwrap();
    match daemon_socket_with_home(&canonical_root.join("r"), &home_104) {
        Err(DaemonSocketError::PathTooLong {
            bytes,
            limit: reported,
            path,
        }) => {
            assert_eq!(bytes, limit + 1);
            assert_eq!(reported, limit);
            assert!(path.starts_with(&home_104));
        }
        other => panic!("expected PathTooLong, got {other:?}"),
    }
}

#[test]
fn root_vector_and_fixture_schema_are_pinned() {
    let contract = contract();
    assert_eq!(contract.algorithm, "sha256-canonical-runtime-first8");
    assert_eq!(
        contract.instance_priority,
        ["YI_AGENT_INSTANCE_HOME", "YI_AGENT_HOME", "HOME"]
    );
    assert_eq!(contract.override_suffix, ".yi-agent");
    assert_eq!(contract.socket_directory, "sockets");
    assert_eq!(contract.socket_file, "runtime.sock");
    assert_eq!(contract.max_socket_path_bytes, 103);

    for vector in &contract.vectors {
        let runtime = PathBuf::from(os_string_from_bytes(from_hex(&vector.runtime_hex)));
        let socket = daemon_socket_with_home(&runtime, Path::new("/")).unwrap();
        assert_eq!(
            socket.as_os_str().as_encoded_bytes(),
            from_hex(&vector.socket_hex).as_slice(),
            "root vector drifted"
        );
        // 目录名本身就是 16 个十六进制字符，不能再按字节解码一次。
        let directory = socket.parent().unwrap().file_name().unwrap();
        assert_eq!(
            directory.to_str().unwrap(),
            vector.digest16,
            "digest16 drifted"
        );
    }

    // 每个 case ID 都必须被明确的用例消费；新增 ID 却没人测就会在这里失败。
    let expected: std::collections::BTreeSet<&str> = [
        "short",
        "deep",
        "instance_priority",
        "override_suffix",
        "empty",
        "relative",
        "symlink",
        "dot_segments",
        "symlink_dotdot",
        "missing_tail",
        "isolated",
        "tmpdir",
        "non_utf8",
        "deleted_cwd",
        "limit_103",
        "limit_104",
    ]
    .into_iter()
    .collect();
    let actual: std::collections::BTreeSet<&str> =
        contract.case_ids.iter().map(String::as_str).collect();
    assert_eq!(actual, expected);
}

#[test]
fn daemon_socket_for_reads_the_process_environment() {
    // 生产包装只读三个环境变量并委托纯函数，不做全局 set_var。
    let socket = daemon_socket_for(Path::new("/")).expect("HOME or override resolves");
    assert!(socket.ends_with("runtime.sock"));
}

// 小工具：把 raw bytes 变成 OsString，避免测试里到处写平台相关的 trait 导入。
fn os_string_from_bytes(bytes: Vec<u8>) -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(bytes)
}

// ===== 跨进程真实函数对照 =====
//
// 插件这里调**真实** `daemon_socket_for`，宿主侧由一个独立构建的 example 调它
// **真实** `scope::daemon_socket`。两侧输出同一套 JSON 投影，逐 case 比较；两个
// crate 之间没有任何依赖。测试是 `#[ignore]`，因为它需要一个外部构建好的探针，
// 必执行的 gate 会显式跑它，不允许它被悄悄跳过。

#[derive(Debug, Clone, PartialEq, Eq)]
enum Projection {
    Socket(Vec<u8>),
    HomeUnavailable,
    PathTooLong { bytes: usize, limit: usize },
}

fn projection(value: &serde_json::Value) -> Projection {
    match value["error"].as_str() {
        None => {
            let hex = value["socket_hex"].as_str().expect("socket_hex");
            Projection::Socket(from_hex(hex))
        }
        Some("instance_home_unavailable") => Projection::HomeUnavailable,
        Some("path_too_long") => Projection::PathTooLong {
            bytes: value["bytes"].as_u64().expect("bytes") as usize,
            limit: value["limit"].as_u64().expect("limit") as usize,
        },
        Some(other) => panic!("unknown error from contract probe: {other}"),
    }
}

/// 解析 worker 输出里的唯一标记行。
fn parse_marker(stdout: &[u8]) -> serde_json::Value {
    let text = String::from_utf8_lossy(stdout);
    let markers: Vec<&str> = text
        .lines()
        .filter_map(|line| line.strip_prefix("KANBAN_CONTRACT_RESULT="))
        .collect();
    assert_eq!(
        markers.len(),
        1,
        "exactly one result marker, got {markers:?}"
    );
    serde_json::from_str(markers[0]).expect("marker is JSON")
}

/// 插件侧 worker：以子进程重入本测试二进制，调用真实生产函数。
///
/// 没有 `KANBAN_CONTRACT_RUNTIME_HEX` 时立即返回，常规 `cargo test` 不受影响。
#[test]
fn plugin_environment_worker() {
    let Some(runtime_hex) = std::env::var_os("KANBAN_CONTRACT_RUNTIME_HEX") else {
        return;
    };
    let runtime_hex = runtime_hex.to_string_lossy().into_owned();
    if std::env::var_os("KANBAN_CONTRACT_DELETE_CWD").is_some() {
        let cwd = std::env::current_dir().expect("cwd");
        std::fs::remove_dir(&cwd).expect("delete the dedicated empty cwd");
    }
    let runtime = PathBuf::from(os_string_from_bytes(from_hex(&runtime_hex)));
    let value = match daemon_socket_for(&runtime) {
        Ok(socket) => serde_json::json!({
            "socket_hex": socket.as_os_str().as_encoded_bytes()
                .iter().map(|b| format!("{b:02x}")).collect::<String>(),
            "error": null,
        }),
        Err(DaemonSocketError::InstanceHomeUnavailable) => serde_json::json!({
            "socket_hex": null, "error": "instance_home_unavailable",
        }),
        Err(DaemonSocketError::PathTooLong { bytes, limit, path }) => serde_json::json!({
            "socket_hex": null,
            "error": "path_too_long",
            "bytes": bytes,
            "limit": limit,
            "path_hex": path.as_os_str().as_encoded_bytes()
                .iter().map(|b| format!("{b:02x}")).collect::<String>(),
        }),
    };
    use std::io::Write as _;
    println!("KANBAN_CONTRACT_RESULT={value}");
    std::io::stdout().flush().unwrap();
}

#[test]
#[ignore = "requires separately built host probe"]
fn cross_process_daemon_socket_contract() {
    let probe = std::env::var_os("KANBAN_HOST_SOCKET_PROBE")
        .expect("KANBAN_HOST_SOCKET_PROBE is required; no skip permitted");
    assert!(
        std::path::Path::new(&probe).is_file(),
        "host probe missing: {}",
        std::path::Path::new(&probe).display()
    );

    // 先确认根向量：两个真实函数都必须给出 fixture 里的值。
    let mut command = std::process::Command::new(&probe);
    command
        .arg("2f")
        .env_clear()
        .env("YI_AGENT_INSTANCE_HOME", "/");
    let output = run_contract_child(command, std::time::Duration::from_secs(5));
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        value["socket_hex"],
        serde_json::Value::String(
            "/sockets/8a5edab282632443/runtime.sock"
                .as_bytes()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        )
    );

    let contract = contract();
    let root = short_root("kd-cross");
    let canonical = std::fs::canonicalize(root.path()).unwrap();
    let home = canonical.join("i");
    std::fs::create_dir_all(home.join("tmp")).unwrap();

    let mut consumed = std::collections::BTreeSet::new();
    let runtime = canonical.join("r");
    consumed.insert("short");
    compare_case(&probe, &runtime, &home, None);

    // deep：地址长度与项目深度无关。
    let mut deep = canonical.clone();
    for _ in 0..40 {
        deep.push("deep");
    }
    consumed.insert("deep");
    compare_case(&probe, &deep.join("r"), &home, None);

    // 实例优先级 / 后缀 / 空值 / 缺 home。
    for (id, instance, override_home, home_value, expect_home) in [
        (
            "instance_priority",
            Some("/i1"),
            Some("/o1"),
            Some("/h1"),
            true,
        ),
        ("override_suffix", None, Some("/o2"), Some("/h2"), true),
        ("empty", Some(""), Some(""), Some(""), false),
    ] {
        consumed.insert(id);
        let home = if expect_home {
            Some(home_value.unwrap())
        } else {
            None
        };
        compare_env_case(&probe, &runtime, instance, override_home, home);
    }

    // relative / dot_segments / symlink / symlink_dotdot / missing_tail / isolated / tmpdir。
    consumed.extend(["relative", "dot_segments", "symlink", "symlink_dotdot"]);
    let real = canonical.join("real");
    std::fs::create_dir_all(real.join("runtime")).unwrap();
    std::os::unix::fs::symlink(&real, canonical.join("link")).unwrap();
    compare_case(&probe, &real.join("runtime"), &home, None);
    compare_case(&probe, &canonical.join("real/./x/../runtime"), &home, None);
    compare_case(&probe, &canonical.join("link/runtime"), &home, None);
    compare_case(&probe, &canonical.join("link/../real/runtime"), &home, None);

    consumed.insert("missing_tail");
    compare_case(
        &probe,
        &canonical.join("later/runtime"),
        &canonical.join("later-home"),
        None,
    );

    consumed.insert("isolated");
    compare_case(&probe, &canonical.join("r2"), &canonical.join("i2"), None);

    // tmpdir：TMPDIR 不参与业务地址；两个 child 用不同 TMPDIR 必须同址。
    consumed.insert("tmpdir");
    let a = run_env_case(
        &probe,
        &runtime,
        Some("/"),
        None,
        None,
        Some(canonical.join("t1")),
    );
    let b = run_env_case(
        &probe,
        &runtime,
        Some("/"),
        None,
        None,
        Some(canonical.join("t2")),
    );
    assert_eq!(projection(&a), projection(&b));
    assert!(matches!(projection(&a), Projection::Socket(_)));

    // non_utf8：原始字节必须原样参与哈希。
    consumed.insert("non_utf8");
    let non_utf8 = {
        let mut bytes = canonical.join("dir-").into_os_string().into_encoded_bytes();
        bytes.push(0xff);
        PathBuf::from(os_string_from_bytes(bytes))
    };
    compare_case(&probe, &non_utf8, &home, None);

    // deleted_cwd：同一路径的专用空目录，host 先删自己 cwd 再解析，parent 重建后
    // 插件 child 再删自己 cwd 解析；两边的 runtime 相对字节完全一致。
    consumed.insert("deleted_cwd");
    let dedicated = canonical.join("cwd");
    std::fs::create_dir_all(&dedicated).unwrap();
    let mut host = std::process::Command::new(&probe);
    host.arg("72")
        .arg("--delete-cwd")
        .env_clear()
        .env("YI_AGENT_INSTANCE_HOME", "/")
        .current_dir(&dedicated);
    let host_out = run_contract_child(host, std::time::Duration::from_secs(5));
    assert!(
        host_out.status.success(),
        "{}",
        String::from_utf8_lossy(&host_out.stderr)
    );
    let host_projection = projection(&serde_json::from_slice(&host_out.stdout).unwrap());
    std::fs::create_dir_all(&dedicated).unwrap();
    let mut plugin = std::process::Command::new(std::env::current_exe().unwrap());
    plugin
        .args(["--exact", "plugin_environment_worker", "--nocapture"])
        .env_clear()
        .env("KANBAN_CONTRACT_RUNTIME_HEX", "72")
        .env("KANBAN_CONTRACT_DELETE_CWD", "1")
        .env("YI_AGENT_INSTANCE_HOME", "/")
        .current_dir(&dedicated);
    let plugin_out = run_contract_child(plugin, std::time::Duration::from_secs(10));
    assert!(
        plugin_out.status.success(),
        "{}",
        String::from_utf8_lossy(&plugin_out.stderr)
    );
    assert_eq!(
        host_projection,
        projection(&parse_marker(&plugin_out.stdout))
    );
    std::fs::remove_dir_all(&dedicated).ok();

    // limit_103 / limit_104：精确边界，按 bytes 而非 display 长度比较。
    let tail = "/sockets/".len() + 16 + "/runtime.sock".len();
    let base_pad =
        contract.max_socket_path_bytes - tail - canonical.as_os_str().as_encoded_bytes().len() - 1;
    let mut exact = canonical.clone();
    exact.push("p".repeat(base_pad));
    std::fs::create_dir_all(&exact).unwrap();
    let exact = std::fs::canonicalize(&exact).unwrap();

    consumed.insert("limit_103");
    let a = run_home_case(&probe, &runtime, &exact, None);
    let b = run_plugin_home_case(&runtime, &exact, None);
    assert_eq!(a, b, "limit_103");
    assert!(matches!(a, Projection::Socket(_)), "limit_103: {a:?}");

    consumed.insert("limit_104");
    let mut over = exact.clone();
    let mut name = over.file_name().unwrap().as_encoded_bytes().to_vec();
    name.push(b'q');
    over.set_file_name(os_string_from_bytes(name));
    std::fs::create_dir_all(&over).unwrap();
    let a = run_home_case(&probe, &runtime, &over, None);
    let b = run_plugin_home_case(&runtime, &over, None);
    assert_eq!(a, b, "limit_104");
    assert!(
        matches!(a, Projection::PathTooLong { bytes, .. } if bytes == 104),
        "limit_104: {a:?}"
    );

    assert_eq!(
        consumed,
        contract.case_ids.iter().map(String::as_str).collect(),
        "every fixture case must be consumed by a real cross-process comparison"
    );
}

fn run_contract_child(
    mut command: std::process::Command,
    timeout: std::time::Duration,
) -> std::process::Output {
    use std::io::Read as _;

    let mut stdout = tempfile::tempfile().expect("stdout tempfile");
    let mut stderr = tempfile::tempfile().expect("stderr tempfile");
    let out = stdout.try_clone().unwrap();
    let err = stderr.try_clone().unwrap();
    command
        .stdout(std::process::Stdio::from(out))
        .stderr(std::process::Stdio::from(err));
    let mut child = command.spawn().expect("spawn contract child");
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let mut text = String::new();
            stderr.read_to_string(&mut text).ok();
            panic!("contract child timed out after {timeout:?}; stderr: {text}");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    // 子进程写的是同一个 file description，偏移已经移到末尾；读之前必须回到起点。
    use std::io::Seek as _;
    stdout.seek(std::io::SeekFrom::Start(0)).unwrap();
    stderr.seek(std::io::SeekFrom::Start(0)).unwrap();
    let mut stdout_bytes = Vec::new();
    stdout.read_to_end(&mut stdout_bytes).ok();
    let mut stderr_bytes = Vec::new();
    stderr.read_to_end(&mut stderr_bytes).ok();
    std::process::Output {
        status,
        stdout: stdout_bytes,
        stderr: stderr_bytes,
    }
}

fn compare_case(
    probe: &std::ffi::OsStr,
    runtime: &std::path::Path,
    home: &std::path::Path,
    _t: Option<()>,
) {
    let hex: String = runtime
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let mut host = std::process::Command::new(probe);
    host.arg(&hex)
        .env_clear()
        .env("YI_AGENT_INSTANCE_HOME", home);
    let host_out = run_contract_child(host, std::time::Duration::from_secs(5));
    assert!(
        host_out.status.success(),
        "{}",
        String::from_utf8_lossy(&host_out.stderr)
    );
    let mut plugin = std::process::Command::new(std::env::current_exe().unwrap());
    plugin
        .args(["--exact", "plugin_environment_worker", "--nocapture"])
        .env_clear()
        .env("KANBAN_CONTRACT_RUNTIME_HEX", &hex)
        .env("YI_AGENT_INSTANCE_HOME", home);
    let plugin_out = run_contract_child(plugin, std::time::Duration::from_secs(10));
    assert!(
        plugin_out.status.success(),
        "{}",
        String::from_utf8_lossy(&plugin_out.stderr)
    );
    assert_eq!(
        projection(&serde_json::from_slice(&host_out.stdout).unwrap()),
        projection(&parse_marker(&plugin_out.stdout)),
        "runtime {runtime:?} home {home:?}"
    );
}

fn run_env_case(
    probe: &std::ffi::OsStr,
    runtime: &std::path::Path,
    instance: Option<&str>,
    override_home: Option<&str>,
    home: Option<&str>,
    tmpdir: Option<std::path::PathBuf>,
) -> serde_json::Value {
    let hex: String = runtime
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let mut host = std::process::Command::new(probe);
    host.arg(&hex).env_clear();
    if let Some(v) = instance {
        host.env("YI_AGENT_INSTANCE_HOME", v);
    }
    if let Some(v) = override_home {
        host.env("YI_AGENT_HOME", v);
    }
    if let Some(v) = home {
        host.env("HOME", v);
    }
    if let Some(v) = tmpdir {
        host.env("TMPDIR", v);
    }
    let out = run_contract_child(host, std::time::Duration::from_secs(5));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn compare_env_case(
    probe: &std::ffi::OsStr,
    runtime: &std::path::Path,
    instance: Option<&str>,
    override_home: Option<&str>,
    home: Option<&str>,
) {
    let host = run_env_case(probe, runtime, instance, override_home, home, None);
    let plugin = run_plugin_env_case(runtime, instance, override_home, home, None);
    assert_eq!(projection(&host), plugin);
}

fn run_plugin_env_case(
    runtime: &std::path::Path,
    instance: Option<&str>,
    override_home: Option<&str>,
    home: Option<&str>,
    tmpdir: Option<std::path::PathBuf>,
) -> Projection {
    let hex: String = runtime
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let mut plugin = std::process::Command::new(std::env::current_exe().unwrap());
    plugin
        .args(["--exact", "plugin_environment_worker", "--nocapture"])
        .env_clear()
        .env("KANBAN_CONTRACT_RUNTIME_HEX", &hex);
    if let Some(v) = instance {
        plugin.env("YI_AGENT_INSTANCE_HOME", v);
    }
    if let Some(v) = override_home {
        plugin.env("YI_AGENT_HOME", v);
    }
    if let Some(v) = home {
        plugin.env("HOME", v);
    }
    if let Some(v) = tmpdir {
        plugin.env("TMPDIR", v);
    }
    let out = run_contract_child(plugin, std::time::Duration::from_secs(10));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    projection(&parse_marker(&out.stdout))
}

fn run_home_case(
    probe: &std::ffi::OsStr,
    runtime: &std::path::Path,
    home: &std::path::Path,
    t: Option<()>,
) -> Projection {
    let _ = t;
    let hex: String = runtime
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let mut host = std::process::Command::new(probe);
    host.arg(&hex)
        .env_clear()
        .env("YI_AGENT_INSTANCE_HOME", home);
    let out = run_contract_child(host, std::time::Duration::from_secs(5));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    projection(&serde_json::from_slice(&out.stdout).unwrap())
}

fn run_plugin_home_case(
    runtime: &std::path::Path,
    home: &std::path::Path,
    t: Option<()>,
) -> Projection {
    let _ = t;
    let hex: String = runtime
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let mut plugin = std::process::Command::new(std::env::current_exe().unwrap());
    plugin
        .args(["--exact", "plugin_environment_worker", "--nocapture"])
        .env_clear()
        .env("KANBAN_CONTRACT_RUNTIME_HEX", &hex)
        .env("YI_AGENT_INSTANCE_HOME", home);
    let out = run_contract_child(plugin, std::time::Duration::from_secs(10));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    projection(&parse_marker(&out.stdout))
}
