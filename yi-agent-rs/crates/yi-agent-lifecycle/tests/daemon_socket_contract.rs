//! 宿主侧对公共 daemon socket 契约的断言。
//!
//! 两侧读同一份 `plugins/contracts/fixtures/daemon-socket.json`，各自调用**真实**
//! 的生产函数。任何一侧改了规则而没同步，这里或插件侧的同名测试就会红。

use std::path::{Path, PathBuf};

use serde::Deserialize;
use yi_agent_lifecycle::scope;

const CONTRACT: &str = include_str!("../../../../plugins/contracts/fixtures/daemon-socket.json");

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

fn path_from_hex(text: &str) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(std::ffi::OsString::from_vec(from_hex(text)))
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
    assert_eq!(contract.max_socket_path_bytes, scope::MAX_SOCKET_PATH_BYTES);

    for vector in &contract.vectors {
        let runtime = path_from_hex(&vector.runtime_hex);
        let socket = scope::daemon_socket(&runtime, Path::new("/"));
        assert_eq!(
            socket.as_os_str().as_encoded_bytes(),
            from_hex(&vector.socket_hex).as_slice(),
            "root vector drifted"
        );
        assert_eq!(
            socket
                .parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap(),
            vector.digest16
        );
    }

    // case ID 是跨端覆盖清单：删一个说明少了验证，加一个必须先补用例。
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
fn instance_home_suffix_rules_match_the_contract() {
    let contract = contract();
    let suffix = std::ffi::OsStr::new(&contract.override_suffix);

    // override 的末级已经是 `.yi-agent` 时不重复追加。
    assert_eq!(
        scope::instance_home_from_env(
            Some(std::ffi::OsStr::new("/override/.yi-agent")),
            Some(Path::new("/home"))
        )
        .unwrap(),
        PathBuf::from("/override/.yi-agent")
    );
    // 一般 override 追加一级后缀。
    let appended = scope::instance_home_from_env(
        Some(std::ffi::OsStr::new("/override")),
        Some(Path::new("/home")),
    )
    .unwrap();
    assert_eq!(appended.file_name().unwrap(), suffix);
    // 空 override 被跳过，落到 $HOME。
    assert_eq!(
        scope::instance_home_from_env(Some(std::ffi::OsStr::new("")), Some(Path::new("/home")))
            .unwrap(),
        Path::new("/home").join(".yi-agent")
    );
    // 两者都不可用返回 None（不是猜一个路径）。
    assert!(scope::instance_home_from_env(Some(std::ffi::OsStr::new("")), None).is_none());
}

#[test]
fn deep_runtimes_stay_short_and_separated() {
    let root = tempfile::Builder::new()
        .prefix("kd-host")
        .tempdir_in("/tmp")
        .expect("short temp root");
    let canonical = std::fs::canonicalize(root.path()).unwrap();
    let home = canonical.join("i");
    let mut deep = canonical.clone();
    for _ in 0..40 {
        deep.push("deep");
    }

    let short = scope::daemon_socket(&canonical.join("r"), &home);
    let long = scope::daemon_socket(&deep.join("r"), &home);
    assert_ne!(short, long, "不同 runtime 必须是不同 socket");
    for socket in [short, long] {
        assert!(socket.starts_with(home.join("sockets")), "{socket:?}");
        assert!(socket.as_os_str().len() <= scope::MAX_SOCKET_PATH_BYTES);
    }
    assert_ne!(
        scope::daemon_socket(&canonical.join("r"), &home),
        scope::daemon_socket(&canonical.join("r"), &canonical.join("i2")),
        "不同实例家目录必须隔离"
    );
}
