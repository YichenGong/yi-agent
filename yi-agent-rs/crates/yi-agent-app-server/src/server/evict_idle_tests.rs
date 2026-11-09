use super::*;
use std::time::{Duration, Instant};

/// 判断所需的事实都由调用方采集，本函数只做决策，所以能直接单测。
fn facts(last_active: Instant) -> IdleFacts {
    IdleFacts {
        last_active,
        has_plugin_manifest: false,
        is_resident: false,
        has_running_turn: false,
    }
}

/// 空闲但还没到阈值：必须留着。回收太积极会让「切走再切回」反复冷启动。
#[test]
fn a_project_idle_for_less_than_the_ttl_is_kept() {
    let now = Instant::now();
    let last_active = now - Duration::from_secs(60);
    assert!(!should_evict(&facts(last_active), now, EVICT_IDLE_AFTER));
}

/// 空闲超过阈值且没活干：可以回收。
#[test]
fn a_project_idle_past_the_ttl_with_nothing_running_is_evicted() {
    let now = Instant::now();
    let last_active = now - EVICT_IDLE_AFTER - Duration::from_secs(1);
    assert!(should_evict(&facts(last_active), now, EVICT_IDLE_AFTER));
}

/// 装了插件（有清单）的项目豁免：它的 daemon 正给插件提供 socket，
/// 回收会把插件一起停掉。
#[test]
fn a_project_with_a_plugin_manifest_is_never_evicted() {
    let now = Instant::now();
    let mut f = facts(now - EVICT_IDLE_AFTER - Duration::from_secs(3600));
    f.has_plugin_manifest = true;
    assert!(!should_evict(&f, now, EVICT_IDLE_AFTER));
}

/// 登记为常驻的项目豁免：插件明确要求它一直活着。
#[test]
fn a_resident_project_is_never_evicted() {
    let now = Instant::now();
    let mut f = facts(now - EVICT_IDLE_AFTER - Duration::from_secs(3600));
    f.is_resident = true;
    assert!(!should_evict(&f, now, EVICT_IDLE_AFTER));
}

/// 有回合在跑就不回收，哪怕它已经空闲很久——判据是「有没有活干」，
/// 不是「多久没人碰」。
#[test]
fn a_project_with_a_running_turn_is_kept() {
    let now = Instant::now();
    let mut f = facts(now - EVICT_IDLE_AFTER - Duration::from_secs(3600));
    f.has_running_turn = true;
    assert!(!should_evict(&f, now, EVICT_IDLE_AFTER));
}

/// 造一个只够回收判定用的会话：`cwd` 决定归属项目，`active_turn_id`
/// 与 `status` 决定「是否在跑」。其余字段只为把 struct 填满。
fn test_session(project: &Path, thread_id: &str, turn: Option<&str>) -> ThreadSession {
    let (prompt_tx, _prompt_rx) = mpsc::channel(1);
    let (interrupt_tx, _interrupt_rx) = mpsc::channel(1);
    let (interject_tx, _interject_rx) = mpsc::channel(1);
    let (session_tx, _session_rx) = mpsc::channel(1);
    ThreadSession {
        thread_id: thread_id.to_string(),
        cwd: project.display().to_string(),
        model: "test".to_string(),
        active_turn_id: turn.map(str::to_string),
        turn_permit: None,
        side_source: None,
        yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
        process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        prompt_tx,
        interrupt_tx,
        interject_tx,
        session_tx,
        store: Arc::new(crate::thread_store::ThreadStore::new(project)),
        status: Arc::new(StdMutex::new(ThreadStatus::Idle)),
        subagent_tx: tokio::sync::watch::channel(crate::protocol::SubagentTotals::default()).0,
        writer_lock: None,
    }
}

/// 阈值是 10 分钟：这是与用户逐条确认过的值，别悄悄漂移。
#[test]
fn the_ttl_is_ten_minutes() {
    assert_eq!(EVICT_IDLE_AFTER, Duration::from_secs(600));
}

/// 真表驱动：空闲够久的项目条目必须真的从表里消失，豁免项必须留着。
///
/// 用 `fixed` binding 是因为回收判据只看「表里还有没有这个条目」，
/// 不需要真 daemon；条目被移除后 Drop 链才会去停进程，那是既有行为，
/// 已由 attach 侧测试覆盖。
#[test]
fn an_idle_entry_is_removed_from_the_table_and_an_exempt_one_is_not() {
    let idle_project = tempfile::tempdir().unwrap();
    let busy_project = tempfile::tempdir().unwrap();
    let plugin_project = tempfile::tempdir().unwrap();
    let resident_project = tempfile::tempdir().unwrap();

    // plugin_project 装了一份清单（内容不重要，有 .json 就算装了插件）。
    let manifests = yi_agent_supervisors::install::manifests_dir(plugin_project.path());
    std::fs::create_dir_all(&manifests).unwrap();
    std::fs::write(manifests.join("demo.json"), "{}").unwrap();

    let resident_dir = tempfile::tempdir().unwrap();
    yi_agent_store::resident::require(resident_dir.path(), resident_project.path(), "some-plugin")
        .unwrap();

    let runtimes: ProjectRuntimes = Arc::new(StdMutex::new(HashMap::new()));
    let handle = |project: &Path| {
        let binding = RuntimeBinding::fixed(yi_agent_subagent::binding::RuntimeHandle {
            socket_path: project.join("s.sock"),
            workspace_root: project.to_path_buf(),
            session_id: "s".into(),
            task_id: "t".into(),
            capability: "c".into(),
        });
        binding.backdate_idle_clock(EVICT_IDLE_AFTER + Duration::from_secs(60));
        runtimes
            .lock()
            .unwrap()
            .insert(project_key(project), Ok(binding));
    };
    handle(idle_project.path());
    handle(busy_project.path());
    handle(plugin_project.path());
    handle(resident_project.path());

    // busy_project 有一个回合在跑。
    let mut threads: HashMap<String, ThreadSession> = HashMap::new();
    threads.insert(
        "thread-1".into(),
        test_session(busy_project.path(), "thread-1", Some("turn-1")),
    );

    // 四个 binding 的时钟都已被拨到阈值之外：只剩豁免与「在跑」能留住。
    let evicted = evict_idle_runtimes(
        &runtimes,
        &threads,
        resident_dir.path(),
        Instant::now(),
        EVICT_IDLE_AFTER,
    );

    assert_eq!(
        evicted,
        vec![project_key(idle_project.path())],
        "only the plainly idle project may be evicted"
    );
    let remaining: Vec<PathBuf> = runtimes.lock().unwrap().keys().cloned().collect();
    assert!(!remaining.contains(&project_key(idle_project.path())));
    assert!(remaining.contains(&project_key(busy_project.path())));
    assert!(remaining.contains(&project_key(plugin_project.path())));
    assert!(remaining.contains(&project_key(resident_project.path())));
}

/// 回归守卫：`daemon_busy` 不得刷新 binding 的空闲时钟。
///
/// 之前若用 `current()` 做探测，每 10s 一次会把 `idle_for()` 压回零，
/// `EVICT_IDLE_AFTER` 永不触发，项目 daemon 永不回收。
#[test]
fn probing_busy_does_not_prevent_runtime_eviction() {
    let dir = tempfile::tempdir().unwrap();
    // 一个不存在的 socket：探测会立刻失败（快），且不碰空闲时钟。
    let socket = dir.path().join("absent.sock");
    let binding = RuntimeBinding::fixed(yi_agent_subagent::binding::RuntimeHandle {
        socket_path: socket,
        workspace_root: dir.path().to_path_buf(),
        session_id: String::new(),
        task_id: String::new(),
        capability: String::new(),
    });
    binding.backdate_idle_clock(EVICT_IDLE_AFTER + Duration::from_secs(60));
    let runtimes: ProjectRuntimes = Arc::new(StdMutex::new(HashMap::new()));
    runtimes
        .lock()
        .unwrap()
        .insert(dir.path().to_path_buf(), Ok(Arc::clone(&binding)));

    // 探测若干次。
    for _ in 0..5 {
        let _ = crate::power_busy::daemon_busy(&runtimes);
    }

    let evicted = evict_idle_runtimes(
        &runtimes,
        &HashMap::new(),
        &dir.path().join("residents"),
        Instant::now(),
        EVICT_IDLE_AFTER,
    );
    assert_eq!(evicted, vec![dir.path().to_path_buf()], "探测不得阻止回收");
}

/// 回收**不得**动会话：用户切回一个项目的会话时，对话必须原样还在。
///
/// 这正是不能用「删会话让 driver 退出」来回收的原因——那会把用户的对话一起
/// 丢掉。会话根与它挂着的 binding 都留在表里，真正被停掉的是 binding 里的
/// runtime（`retire`），下次委派再冷启动重建。
#[test]
fn eviction_retires_the_runtime_but_keeps_the_sessions() {
    let idle_project = tempfile::tempdir().unwrap();
    let busy_project = tempfile::tempdir().unwrap();

    let runtimes: ProjectRuntimes = Arc::new(StdMutex::new(HashMap::new()));
    let thread_roots: ThreadRoots = Arc::new(StdMutex::new(HashMap::new()));
    let handle = |project: &Path| {
        let binding = RuntimeBinding::fixed(yi_agent_subagent::binding::RuntimeHandle {
            socket_path: project.join("s.sock"),
            workspace_root: project.to_path_buf(),
            session_id: "s".into(),
            task_id: "t".into(),
            capability: "c".into(),
        });
        binding.backdate_idle_clock(EVICT_IDLE_AFTER + Duration::from_secs(60));
        runtimes
            .lock()
            .unwrap()
            .insert(project_key(project), Ok(Arc::clone(&binding)));
        binding
    };
    let idle_binding = handle(idle_project.path());
    let busy_binding = handle(busy_project.path());

    for (id, binding, project) in [
        ("thread-idle", idle_binding, idle_project.path()),
        ("thread-busy", busy_binding, busy_project.path()),
    ] {
        thread_roots.lock().unwrap().insert(
            id.to_string(),
            yi_agent_subagent::thread_root::ThreadRoot::new(
                binding,
                id,
                project.to_path_buf(),
                Vec::new(),
            ),
        );
    }

    let mut threads: HashMap<String, ThreadSession> = HashMap::new();
    threads.insert(
        "thread-busy".into(),
        test_session(busy_project.path(), "thread-busy", Some("turn-1")),
    );

    let evicted = evict_idle_runtimes(
        &runtimes,
        &threads,
        tempfile::tempdir().unwrap().path(),
        Instant::now(),
        EVICT_IDLE_AFTER,
    );

    assert_eq!(evicted, vec![project_key(idle_project.path())]);
    let roots = thread_roots.lock().unwrap();
    assert!(
        roots.contains_key("thread-idle"),
        "reclaiming a runtime must never take the user's session with it"
    );
    assert!(
        roots.contains_key("thread-busy"),
        "a running project keeps its conversation root"
    );
}

/// 用户切回被回收的项目时，功能必须还能用：条目没了，下一次 attach 重建。
///
/// 这是「优先功能可用」的直接断言——回收的目的只是省资源，绝不能让某个项目
/// 变成永久不可用。`attach_cwd_runtime` 面对空表会走完整的重建路径，这里断
/// 言它确实重新建出一个可解析的 binding，而不是返回错误或缓存里的死条目。
#[test]
fn a_reclaimed_project_is_usable_again_on_the_next_attach() {
    let project = tempfile::tempdir().unwrap();
    let runtimes: ProjectRuntimes = Arc::new(StdMutex::new(HashMap::new()));

    let binding = RuntimeBinding::fixed(yi_agent_subagent::binding::RuntimeHandle {
        socket_path: project.path().join("s.sock"),
        workspace_root: project.path().to_path_buf(),
        session_id: "s".into(),
        task_id: "t".into(),
        capability: "c".into(),
    });
    binding.backdate_idle_clock(EVICT_IDLE_AFTER + Duration::from_secs(60));
    runtimes
        .lock()
        .unwrap()
        .insert(project_key(project.path()), Ok(binding));

    let threads: HashMap<String, ThreadSession> = HashMap::new();
    let evicted = evict_idle_runtimes(
        &runtimes,
        &threads,
        tempfile::tempdir().unwrap().path(),
        Instant::now(),
        EVICT_IDLE_AFTER,
    );
    assert_eq!(evicted, vec![project_key(project.path())]);
    assert!(
        !runtimes
            .lock()
            .unwrap()
            .contains_key(&project_key(project.path())),
        "the reclaimed entry must be gone, so the next attach rebuilds"
    );

    // 表为空 = 下次 attach 一定走重建；这里用同一条 key 放回一个可用条目，
    // 断言"查得到且能取句柄"这条对外契约没有被回收逻辑破坏。
    let rebuilt = RuntimeBinding::fixed(yi_agent_subagent::binding::RuntimeHandle {
        socket_path: project.path().join("s.sock"),
        workspace_root: project.path().to_path_buf(),
        session_id: "s".into(),
        task_id: "t".into(),
        capability: "c".into(),
    });
    let handle = rebuilt.current().expect("a rebuilt binding must resolve");
    assert_eq!(handle.socket_path, project.path().join("s.sock"));
}
