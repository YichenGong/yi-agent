//! 隔离 CLI 进程回归：真实二进制、真实 socket、真实退出码。
//!
//! # 这些测试在防什么
//!
//! 1. **插件探的不是宿主那条 socket**（A10）。旧布局是 `<runtime>/runtime.sock`
//!    或 TMPDIR 回退；新布局是 `<instance_home>/sockets/<hash16>/runtime.sock`。
//!    只在旧地址起桩、只在新地址起桩，结果必须完全不同——所以这里同时 bind
//!    旧的两个候选并保持沉默，只在新地址应答。
//! 2. **一次失败就让位**（A12）。连续失联才退出，一次成功必须清零。
//! 3. **配置错了却留下副作用**（A13）。地址解析失败必须发生在建 state、拿锁、
//!    开查询服务之前。
//! 4. **探活改写了看板**。探测是只读的；已绑定的卡片状态、次序、thread 都不能动。

#[path = "support/daemon.rs"]
mod support;

use std::time::{Duration, Instant};

use support::{ChildGuard, StatusStub, StubReply, TestLayout};

/// 从「首次 probe 之前」的标记开始计时的空载预算。
const EMPTY_LOOP_BUDGET: Duration = Duration::from_secs(12);
/// 标记本身出现的预算：启动期迁移/查询若卡住，要和失联窗口分开报。
const STARTUP_BUDGET: Duration = Duration::from_secs(5);

fn instance_home(layout: &TestLayout) -> std::path::PathBuf {
    layout.home.clone()
}

/// 子进程视角的地址：必须用**隔离的** instance home 算，不能用测试进程自己的
/// HOME（那会算到真实 `~/.yi-agent` 下，bind 出来的桩永远没人连）。
fn daemon_socket(layout: &TestLayout) -> std::path::PathBuf {
    superpowers_kanban_ipc::daemon_socket::daemon_socket_with_home(&layout.runtime, &layout.home)
        .expect("daemon socket resolves for the isolated instance home")
}

/// 旧布局的两个候选：`<runtime>/runtime.sock` 与 TMPDIR 下的哈希回退。
/// 它们都 bind 监听但从不答复——若插件还在探旧地址，就永远等不到应答。
fn bind_legacy_decoys(layout: &TestLayout) -> Vec<StatusStub> {
    let direct = layout.runtime.join("runtime.sock");
    let mut stubs = vec![StatusStub::start(&direct, vec![StubReply::Silent])];

    // TMPDIR 回退：插件旧实现哈希的是 runtime 目录本身。
    let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
    sha2::Digest::update(&mut hasher, layout.runtime.as_os_str().as_encoded_bytes());
    let digest = sha2::Digest::finalize(hasher);
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let fallback = layout
        .home
        .join("tmp")
        .join(format!("yi-agent-{}.sock", &hex[..16]));
    stubs.push(StatusStub::start(&fallback, vec![StubReply::Silent]));
    stubs
}

/// A10：只有新地址应答时，插件必须保持同一个 PID 并持续探测；旧地址一次都不该被碰。
#[test]
fn new_socket_keeps_same_pid_and_old_sockets_are_untouched() {
    let layout = TestLayout::new();
    layout.with_enabled_state();
    let socket = daemon_socket(&layout);
    let stub = StatusStub::start(&socket, vec![StubReply::Status]);
    let decoys = bind_legacy_decoys(&layout);

    let mut child = ChildGuard::spawn(&layout, Some(&instance_home(&layout)), &[]);
    let first_pid = child.pid();

    assert!(
        stub.wait_for_requests(6, Instant::now() + Duration::from_secs(12)),
        "the new socket must receive at least 6 probes; log:\n{}",
        child.log()
    );
    assert_eq!(child.pid(), first_pid, "the plugin must not restart");
    assert!(
        child.try_wait().is_none(),
        "the plugin exited while the daemon was answering:\n{}",
        child.log()
    );
    for (index, decoy) in decoys.iter().enumerate() {
        assert_eq!(
            decoy.requests(),
            0,
            "legacy candidate #{index} must never be probed"
        );
    }
    assert!(
        child.log().contains(&socket.display().to_string()),
        "the startup line must name the socket actually probed:\n{}",
        child.log()
    );
}

/// A12：没有 daemon 时，三次连续失联后退出，并释放单实例锁。
#[test]
fn three_misses_exit_and_release_lock() {
    let layout = TestLayout::new();
    layout.with_enabled_state();

    let mut child = ChildGuard::spawn(&layout, Some(&instance_home(&layout)), &[]);
    assert!(
        child.wait_for_log(
            "daemon probe loop starting",
            Instant::now() + STARTUP_BUDGET
        ),
        "the loop-start marker must appear within {STARTUP_BUDGET:?}:\n{}",
        child.log()
    );
    // 从标记起算失联窗口：初始化耗时不计入，但第一次超时也不能被漏掉。
    let deadline = Instant::now() + EMPTY_LOOP_BUDGET;
    let status = child.wait_before(deadline).unwrap_or_else(|| {
        panic!(
            "the plugin must exit within {EMPTY_LOOP_BUDGET:?} after the loop marker:\n{}",
            child.log()
        )
    });
    assert_eq!(status.code(), Some(0), "log:\n{}", child.log());

    let log = child.log();
    for miss in 1..=3 {
        assert!(
            log.contains(&format!("misses={miss}/3")),
            "miss {miss} must be reported:\n{log}"
        );
    }
    assert_eq!(
        log.matches("daemon probe failed").count(),
        3,
        "exactly three failures, not more:\n{log}"
    );
    assert!(
        log.contains(&layout.runtime.display().to_string()),
        "diagnostics must name the runtime dir:\n{log}"
    );
    assert!(
        log.contains("No such file or directory") || log.contains("Connection refused"),
        "diagnostics must carry the concrete errno:\n{log}"
    );
    assert!(
        superpowers_kanban_runner::single_instance::acquire(&layout.state).is_some(),
        "the instance lock must be released on exit"
    );
}

/// 开关关闭的孤儿照样要探活：否则它永远发现不了 daemon 已死，一直占着锁。
#[test]
fn disabled_switch_still_probes() {
    let layout = TestLayout::new();
    layout.with_disabled_state();

    let mut child = ChildGuard::spawn(&layout, Some(&instance_home(&layout)), &[]);
    assert!(
        child.wait_for_log(
            "daemon probe loop starting",
            Instant::now() + STARTUP_BUDGET
        ),
        "log:\n{}",
        child.log()
    );
    let status = child
        .wait_before(Instant::now() + EMPTY_LOOP_BUDGET)
        .unwrap_or_else(|| {
            panic!(
                "a disabled board must still exit after three misses:\n{}",
                child.log()
            )
        });
    assert_eq!(status.code(), Some(0), "log:\n{}", child.log());
    assert_eq!(
        child.log().matches("daemon probe failed").count(),
        3,
        "log:\n{}",
        child.log()
    );
}

/// 一次成功必须清零连续计数；否则偶发抖动会和真实失联累加。
#[test]
fn success_resets_the_streak() {
    let layout = TestLayout::new();
    layout.with_enabled_state();
    let socket = daemon_socket(&layout);
    // 失败, 失败, 成功, 失败, 失败, 失败 -> 诊断 1,2,1,2,3
    let stub = StatusStub::start(
        &socket,
        vec![
            StubReply::Close,
            StubReply::Close,
            StubReply::Status,
            StubReply::Close,
            StubReply::Close,
            StubReply::Close,
        ],
    );

    let mut child = ChildGuard::spawn(&layout, Some(&instance_home(&layout)), &[]);
    assert!(
        child.wait_for_log(
            "daemon probe loop starting",
            Instant::now() + STARTUP_BUDGET
        ),
        "log:\n{}",
        child.log()
    );
    let status = child
        .wait_before(Instant::now() + EMPTY_LOOP_BUDGET)
        .unwrap_or_else(|| panic!("the plugin must exit:\n{}", child.log()));
    assert_eq!(status.code(), Some(0), "log:\n{}", child.log());

    let log = child.log();
    let misses: Vec<&str> = log
        .lines()
        .filter(|line| line.contains("daemon probe failed"))
        .map(|line| {
            line.split("misses=")
                .nth(1)
                .and_then(|rest| rest.split('/').next())
                .expect("misses=N in the diagnostic")
        })
        .collect();
    assert_eq!(
        misses,
        ["1", "2", "1", "2", "3"],
        "a success must reset the streak; log:\n{log}"
    );
    assert!(
        stub.requests() >= 6,
        "the stub must have served at least 6 probes, got {}",
        stub.requests()
    );
}

/// 沉默对端：不能靠服务端主动 close 充数，客户端必须自己在读阶段超时。
#[test]
fn silent_status_peer_exits_under_empty_loop_budget() {
    let layout = TestLayout::new();
    layout.with_enabled_state();
    let socket = daemon_socket(&layout);
    let stub = StatusStub::start(&socket, vec![StubReply::Silent]);

    let mut child = ChildGuard::spawn(&layout, Some(&instance_home(&layout)), &[]);
    assert!(
        child.wait_for_log(
            "daemon probe loop starting",
            Instant::now() + STARTUP_BUDGET
        ),
        "log:\n{}",
        child.log()
    );
    let started = Instant::now();
    let status = child
        .wait_before(started + EMPTY_LOOP_BUDGET)
        .unwrap_or_else(|| {
            panic!(
                "a silent peer must not keep the plugin alive:\n{}",
                child.log()
            )
        });
    assert_eq!(status.code(), Some(0), "log:\n{}", child.log());

    let log = child.log();
    assert_eq!(
        log.matches("daemon probe failed").count(),
        3,
        "three silent probes, three diagnostics:\n{log}"
    );
    assert!(
        log.contains("timeout during read"),
        "the diagnostic must name the read-stage timeout, not just 'daemon is gone':\n{log}"
    );
    assert!(
        !log.contains("daemon is gone"),
        "the old evidence-free wording must be gone:\n{log}"
    );
    assert!(
        stub.requests() >= 3,
        "the silent peer must have accepted three probes, got {}",
        stub.requests()
    );
    assert!(
        superpowers_kanban_runner::single_instance::acquire(&layout.state).is_some(),
        "the lock must be released"
    );
}

/// 解析失败必须早于一切状态副作用：exit 1 且 state 目录/锁/查询 socket 都不存在。
#[test]
fn missing_instance_home_fails_before_state_side_effects() {
    let layout = TestLayout::new();
    // 不预建 state，也不写开关：任何写操作都会被发现。
    let mut child = ChildGuard::spawn(&layout, None, &[]);
    let status = child
        .wait_before(Instant::now() + STARTUP_BUDGET)
        .expect("the plugin must fail fast without HOME");
    assert_eq!(status.code(), Some(1), "log:\n{}", child.log());

    let log = child.log();
    assert!(
        log.contains("cannot resolve the instance home"),
        "the diagnostic must name the instance-home failure:\n{log}"
    );
    assert!(
        !layout.state.exists(),
        "state dir must not be created on a resolution failure"
    );
    assert!(
        !superpowers_kanban_core::layout::project_preferences_path(&layout.state).exists(),
        "preferences must not be created either"
    );
    assert!(
        !layout.project.join(".yi-agent").exists(),
        "no .yi-agent tree may appear"
    );
    assert!(
        !layout.runtime.join("runtime.sock").exists(),
        "no query socket may be bound"
    );
}

/// 同一前提，另一种解析失败：超长实例路径必须报告字节数与上限。
#[test]
fn long_instance_home_fails_before_state_side_effects() {
    let layout = TestLayout::new();
    let long = layout.root.join("a".repeat(140));

    let mut child = ChildGuard::spawn(&layout, Some(&long), &[]);
    let status = child
        .wait_before(Instant::now() + STARTUP_BUDGET)
        .expect("the plugin must fail fast on an over-long instance path");
    assert_eq!(status.code(), Some(1), "log:\n{}", child.log());

    let log = child.log();
    assert!(
        log.contains("over the 103-byte limit"),
        "the diagnostic must state the limit:\n{log}"
    );
    assert!(
        log.split("is ")
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .is_some(),
        "the diagnostic must state the actual byte count:\n{log}"
    );
    assert!(!layout.state.exists(), "state dir must not be created");
    assert!(
        !layout.project.join(".yi-agent").exists(),
        "no .yi-agent tree may appear"
    );
}

/// 探测是只读的：已绑定的卡片状态、次序、thread ID 一个都不能变。
#[test]
fn bound_card_state_is_not_changed_by_probing() {
    let layout = TestLayout::new();
    layout.with_enabled_state();
    let socket = daemon_socket(&layout);
    let stub = StatusStub::start(&socket, vec![StubReply::Status]);

    let cards: Vec<serde_json::Value> = ["running", "queued", "needs_you"]
        .iter()
        .enumerate()
        .map(|(index, state)| {
            serde_json::json!({
                "id": format!("card-{index}"),
                "state": state,
                "order": index + 1,
                "spec_path": "docs/spec.md",
                "plan_path": "docs/plan.md",
                "enqueued_at": chrono::Local::now().to_rfc3339(),
                "thread_id": format!("isolated-thread-{index}"),
            })
        })
        .collect();
    std::fs::create_dir_all(&layout.state).unwrap();
    std::fs::write(
        layout.state.join("board.json"),
        serde_json::to_vec(&serde_json::json!({"cards": cards, "next_order": 4})).unwrap(),
    )
    .unwrap();

    let mut child = ChildGuard::spawn(&layout, Some(&instance_home(&layout)), &[]);
    assert!(
        stub.wait_for_requests(6, Instant::now() + Duration::from_secs(12)),
        "the plugin must probe repeatedly while cards are bound:\n{}",
        child.log()
    );
    assert!(
        child.try_wait().is_none(),
        "the plugin must still be running:\n{}",
        child.log()
    );

    let text = std::fs::read_to_string(layout.state.join("board.json")).unwrap();
    let board: serde_json::Value = serde_json::from_str(&text).unwrap();
    let after = board["cards"].as_array().expect("cards array");
    assert_eq!(after.len(), 3, "no card may be dropped: {text}");
    for (index, state) in ["running", "queued", "needs_you"].iter().enumerate() {
        let card = after
            .iter()
            .find(|card| card["id"] == format!("card-{index}"))
            .unwrap_or_else(|| panic!("card-{index} disappeared: {text}"));
        assert_eq!(
            &card["state"], state,
            "state of card-{index} changed: {text}"
        );
        assert_eq!(
            card["order"],
            serde_json::json!(index + 1),
            "order of card-{index} changed: {text}"
        );
        assert_eq!(
            card["thread_id"],
            format!("isolated-thread-{index}"),
            "thread of card-{index} changed: {text}"
        );
    }
}
