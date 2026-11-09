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
    home_104.set_file_name(std::ffi::OsString::from_vec_bytes(widened));
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
        let runtime = PathBuf::from(std::ffi::OsString::from_vec_bytes(from_hex(
            &vector.runtime_hex,
        )));
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
trait OsStringBytes {
    fn from_vec_bytes(bytes: Vec<u8>) -> std::ffi::OsString;
}

impl OsStringBytes for std::ffi::OsString {
    fn from_vec_bytes(bytes: Vec<u8>) -> std::ffi::OsString {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(bytes)
    }
}
