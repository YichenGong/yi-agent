//! 回归：所有解析控制 socket 的地方必须算出同一个路径。
//!
//! 调用方分散在多个 crate（app-server、subagent、前端），各自只持有 `runtime_dir`。
//! 一旦有人绕过 `control_path` 直接 join，就会出现「guardian 绑一个、客户端连
//! 另一个」，症状正是那次 30 秒超时而不是一条可读的错误。

use std::path::Path;

#[test]
fn every_resolver_agrees_on_one_control_socket() {
    let runtime_dir = Path::new("/tmp/agreement-project/.yi-agent/runtime");
    let home = Path::new("/home/alice/.yi-agent");

    let canonical = yi_agent_lifecycle::scope::guardian_socket(runtime_dir, home);

    // 1. supervisors 的门面：`bringup::control_path` 走 `RuntimeScope`，这正是
    //    guardian 和 launcher 实际走的那条路，也是本次的真回归点。
    let project = Path::new("/tmp/agreement-project");
    let scope = yi_agent_lifecycle::scope::resolve(runtime_dir, project, home).unwrap();
    assert_eq!(
        yi_agent_supervisors::lifecycle::bringup::control_path(&scope),
        canonical,
        "the launcher must probe the socket the guardian binds"
    );

    // 2. `admission` / `legacy_evidence` / `runtime.rs` / `thread_root` /
    //    `frontend_lease` 都是薄转发，最终落在 `scope::control_path`。它读环境变量，
    //    所以只断言形状一致，不比对具体哈希 —— 比对具体值要改进程环境，在 edition
    //    2024 下是 unsafe，而且会和并行用例互相干扰。
    let via_env = yi_agent_lifecycle::scope::control_path(runtime_dir);
    assert_eq!(
        via_env.file_name(),
        canonical.file_name(),
        "every resolver must agree on the socket's name"
    );
    assert_eq!(
        via_env.parent().unwrap().parent().unwrap().file_name(),
        Some(std::ffi::OsStr::new("sockets")),
        "every resolver must agree on the socket's shape: {}",
        via_env.display()
    );
}

/// 两个入参决定这一个路径，且规范化后一致：同一个 runtime 目录的两种拼法必须
/// 落到同一个 `sockets/<hash>/`，否则同一个项目的两个客户端会各连一个 socket。
#[test]
fn a_socket_path_is_decided_by_the_canonical_pair_only() {
    let home = Path::new("/home/alice/.yi-agent");
    let plain = yi_agent_lifecycle::scope::guardian_socket(Path::new("/tmp/project/runtime"), home);
    let messy = yi_agent_lifecycle::scope::guardian_socket(
        Path::new("/tmp/project/./nested/../runtime"),
        home,
    );
    assert_eq!(
        plain, messy,
        "two spellings of one runtime directory must share one socket"
    );

    let other_home = Path::new("/home/alice/.yi-agent-dev");
    assert_ne!(
        plain,
        yi_agent_lifecycle::scope::guardian_socket(Path::new("/tmp/project/runtime"), other_home),
        "two instances must not share one socket"
    );
}
