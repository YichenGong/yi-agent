//! busy 的三路并集与独立采样任务。
//!
//! `busy` 由三个来源并起来，缺一个就会漏掉一整类活：
//!
//! (a) **本进程的会话**：`threads` 表里非 `Idle` 或有 `active_turn_id` 的；
//! (b) **项目 daemon 里的子 agent / 看板 / 定时任务**：它们不出现在 `threads`
//!     表里，只按 (a) 判会让「看板在后台推进卡片」被判成无活；
//! (c) **远端近期交互**：非 local 客户端在 5 分钟内发过请求。
//!
//! 两处实现约束（plan 偏差 1/2）：
//!
//! - (b) 读句柄必须用 `current_handle_no_touch()`。`current()` 会刷新
//!   `last_active`，每 10s 探一次等于让 `evict_idle_runtimes` 永不触发。
//! - (b) 是同步阻塞 IPC，**只能**在独立 task 里跑。主循环拿到的是上一拍的结果。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use yi_agent_power::clock::{Clock, Stamp};
use yi_agent_subagent::binding::RuntimeBinding;

use crate::protocol::ThreadStatus;
use crate::server::ProjectRuntimes;
use crate::session::ThreadSession;

/// 远端交互窗口的历史默认值，仅测试与文档引用。
///
/// **生产代码不再读它**：窗口由设置下发（`power.remote_touch_minutes` × 60），
/// 装配点见 `server.rs` 的 `PowerGuard::spawn` 与 `power_ticker` 分支。
/// 保留常量是为了让「默认语义没被悄悄改掉」有一条可断言的锚点——
/// 它与 `yi_agent_runtime::settings::POWER_REMOTE_TOUCH_MINUTES_DEFAULT`
/// 必须一致，由 `remote_touch_expires_after_five_minutes` 钉住。
#[cfg(test)]
const REMOTE_TOUCH_WINDOW: Duration = Duration::from_secs(5 * 60);

/// 查 daemon 的回复截止时间。**必须**远小于 10s tick。
pub(crate) const DAEMON_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// 一个任务状态是否意味着「会自己继续推进」。
///
/// 口径是「**非终态 且 非 `queued`/`paused`**」，落在数据上就是只保留
/// `running` / `waiting_for_*`。
///
/// **不要手抄终态清单**：`yi_agent_store::repository` 的 `TERMINAL_TASK_STATES`
/// 与 `yi_agent_core::subagent::task::TaskState::is_terminal` 是那份权威清单，
/// 且两者被 `prune_treats_only_terminal_states_as_finished` 钉住一致。这里按
/// **白名单**写（只认四个在推进的状态），于是未知新状态自动落到「不算」——这正是
/// 想要的方向：判错的代价是睡了一觉（醒来可续），反过来是整夜不睡且无人察觉。
pub(crate) fn state_counts_as_busy(state: &str) -> bool {
    matches!(
        state,
        "running" | "waiting_for_resource" | "waiting_for_permission" | "waiting_for_children"
    )
}

/// (a) 本进程会话：与 `driver_tick` 的 idle 快照同一口径，只取反。
///
/// 判据是「非 `Idle`」而不是「`Running`」：`AwaitingApproval` 也算有活——
/// 那一刻 turn 没结束、上下文都在，机器睡了用户回来点批准，这条 turn 已经随
/// 睡眠死掉了。`active_turn_id.is_some()` 与它并列，覆盖「turn 已挂上但状态还
/// 没切成 Running」的短暂窗口。
pub(crate) fn local_busy(threads: &HashMap<String, ThreadSession>) -> bool {
    threads.values().any(|session| {
        session.active_turn_id.is_some()
            || !matches!(
                *session
                    .status
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
                ThreadStatus::Idle
            )
    })
}

/// (c) 远端近期交互。
///
/// 时间戳必须走 `clock` 的睡眠感知时钟：睡 8 小时后 `since()` 就是 8 小时，
/// 远超窗口，`remote_touch` 自动失效。**不能**用 `Instant::now()`——那只前进
/// 几分钟，会让一条早已失效的交互记录继续算作「刚刚有过交互」，使保活窗口
/// 形同虚设。
///
/// `window` 由设置下发（`power.remote_touch_minutes` × 60），生产路径上
/// 没有任何硬编码窗口；`REMOTE_TOUCH_WINDOW` 只剩测试引用。
pub(crate) fn remote_touch_busy(
    last_remote_touch: Option<Stamp>,
    clock: &dyn Clock,
    window: Duration,
) -> bool {
    last_remote_touch.is_some_and(|stamp| clock.since(stamp) < window)
}

/// (b) 项目 daemon 里是否有在推进的任务。
///
/// **同步阻塞**，最长 `DAEMON_PROBE_TIMEOUT`；只能从 `spawn_blocking` 调。
/// 任一项目有 running 类任务即 `true`。
///
/// 已知盲区（刻意）：`runtimes` 只含**已挂载过**的项目。看板在某个用户从没在
/// 本进程打开过的项目上推卡片时查不到——那种情况下判成「无活」让机器睡是正确
/// 行为；为覆盖率去扫磁盘上所有 `runtime.sock` 会把 10s tick 撑坏。
pub(crate) fn daemon_busy(runtimes: &ProjectRuntimes) -> bool {
    // 先取快照再放锁：不能在持锁期间做 IPC。
    let bindings: Vec<Arc<RuntimeBinding>> = runtimes
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .values()
        .filter_map(|entry| entry.as_ref().ok().cloned())
        .collect();

    bindings.iter().any(|binding| {
        // 关键：`current_handle_no_touch` 而不是 `current()`。
        let Ok(handle) = binding.current_handle_no_touch() else {
            return false;
        };
        let response = yi_agent_store::ipc::send_request_with_version_timeout(
            &handle.socket_path,
            yi_agent_store::ipc::PROTOCOL_VERSION,
            yi_agent_store::ipc::IpcRequest::ListTaskSummaries {
                session_id: None,
                active_only: true,
            },
            DAEMON_PROBE_TIMEOUT,
        );
        match response {
            Ok(yi_agent_store::ipc::IpcResponse::TaskSummaries { tasks }) => {
                // `active_only: true` 的 SQL 口径比我们需要的宽：它只排除
                // completed/completed_no_changes/failed/cancelled，仍会返回
                // blocked/stalled/timed_out/budget_exhausted/recovery_required
                // 这些「已经停下、要人介入才会动」的状态。必须再过滤一遍。
                tasks.iter().any(|task| state_counts_as_busy(&task.state))
            }
            // daemon 不在 / 超时 / 拒绝：按无活处理（宁可睡），不 panic，
            // 也不因为探测而拉起 daemon。
            _ => false,
        }
    })
}

/// 独立采样任务：把 `(b)` 从主循环里摘出去。
///
/// 主循环每拍只读最新值，取不到就沿用上一拍。这样最坏情况是「busy 晚一拍」，
/// 而不是「主循环卡 500ms」。
pub(crate) struct BusySampler {
    latest: Arc<StdMutex<bool>>,
    stop: Arc<AtomicBool>,
}

impl BusySampler {
    /// 启动采样任务。`runtimes` 是共享表，任务自己 clone 一份 `Arc`。
    pub(crate) fn spawn(runtimes: ProjectRuntimes) -> Self {
        let latest = Arc::new(StdMutex::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let writer = Arc::clone(&latest);
        let stop_flag = Arc::clone(&stop);
        // `spawn_blocking` 里用 `std::thread::sleep` 而不是 `tokio::time::sleep`：
        // 那是阻塞线程池，用 async sleep 会白占一个 worker。
        tokio::task::spawn_blocking(move || {
            while !stop_flag.load(Ordering::Relaxed) {
                let busy = daemon_busy(&runtimes);
                *writer
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = busy;
                std::thread::sleep(DAEMON_PROBE_TIMEOUT);
            }
        });
        Self { latest, stop }
    }

    /// 非阻塞读取：永远返回上一拍的值，绝不等待。
    pub(crate) fn latest(&self) -> bool {
        *self
            .latest
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Drop for BusySampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Task 8 Step 3：纯函数口径（6 条） ----

    #[test]
    fn running_and_waiting_tasks_count_as_busy() {
        for state in [
            "running",
            "waiting_for_resource",
            "waiting_for_permission",
            "waiting_for_children",
        ] {
            assert!(state_counts_as_busy(state), "{state} is work in progress");
        }
    }

    /// 回归哨兵：排队而已，睡着后醒来照跑，不丢进度。
    #[test]
    fn queued_and_paused_do_not_count_as_busy() {
        assert!(!state_counts_as_busy("queued"));
        assert!(!state_counts_as_busy("paused"));
    }

    /// 回归哨兵：`blocked` 是「阻塞等人」，可以让机器无限期不睡。
    #[test]
    fn terminal_and_blocked_states_do_not_count_as_busy() {
        for state in [
            "completed",
            "completed_no_changes",
            "blocked",
            "stalled",
            "timed_out",
            "budget_exhausted",
            "failed",
            "cancelled",
            "recovery_required",
            "recovery_gated",
            "recovery_attested",
        ] {
            assert!(
                !state_counts_as_busy(state),
                "{state} must not keep the machine awake"
            );
        }
    }

    /// 新增状态默认按「不算」处理——偏向允许睡眠。判错代价只是睡一觉。
    #[test]
    fn an_unknown_state_does_not_count_as_busy() {
        assert!(!state_counts_as_busy("some_future_state"));
    }

    #[test]
    fn remote_touch_expires_after_five_minutes() {
        assert_eq!(REMOTE_TOUCH_WINDOW, Duration::from_secs(300));
        // 与 runtime 侧的默认值同源：两处各自定义（crate 之间无依赖），
        // 这条断言防止它们各自漂移。
        assert_eq!(
            REMOTE_TOUCH_WINDOW.as_secs(),
            yi_agent_runtime::settings::POWER_REMOTE_TOUCH_MINUTES_DEFAULT * 60,
            "the power_busy fallback and the settings default must agree"
        );
    }

    /// 探测超时必须远小于 10s tick。
    #[test]
    fn the_daemon_probe_timeout_is_far_below_the_tick() {
        assert!(DAEMON_PROBE_TIMEOUT <= Duration::from_millis(500));
    }

    // ---- Task 8 Step 6：静默 daemon 不阻塞（1 条） ----

    /// 一个「收下请求但永不回复」的 daemon 不让探测卡住超过超时。
    /// 这条是本功能对主循环的唯一硬承诺。
    #[test]
    fn a_silent_daemon_costs_at_most_the_probe_timeout() {
        use std::io::Read;
        use std::os::unix::net::UnixListener;

        let dir = tempfile::TempDir::new().unwrap();
        let socket = dir.path().join("runtime.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                // 收下请求但一个字都不回。
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                std::thread::sleep(Duration::from_secs(5));
            }
        });

        let runtimes: ProjectRuntimes = Arc::new(StdMutex::new(HashMap::new()));
        let binding = RuntimeBinding::fixed(yi_agent_subagent::binding::RuntimeHandle {
            socket_path: socket,
            workspace_root: dir.path().to_path_buf(),
            session_id: String::new(),
            task_id: String::new(),
            capability: String::new(),
        });
        runtimes
            .lock()
            .unwrap()
            .insert(dir.path().to_path_buf(), Ok(binding));

        let started = std::time::Instant::now();
        assert!(
            !daemon_busy(&runtimes),
            "a silent daemon must not count as busy"
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(1500),
            "probe must be bounded by its 500ms timeout, took {elapsed:?}"
        );
        let _ = server.join();
    }

    // ---- Task 9 Step 6：local_busy 与 remote_touch_busy 的真实用例（9 条） ----

    /// 造一个只够 busy 判定用的会话。
    fn session(status: ThreadStatus, turn: Option<&str>) -> ThreadSession {
        let (prompt_tx, _) = tokio::sync::mpsc::channel(1);
        let (interrupt_tx, _) = tokio::sync::mpsc::channel(1);
        let (interject_tx, _) = tokio::sync::mpsc::channel(1);
        let (session_tx, _) = tokio::sync::mpsc::channel(1);
        ThreadSession {
            thread_id: "t".to_string(),
            cwd: std::env::temp_dir().display().to_string(),
            model: "test".to_string(),
            active_turn_id: turn.map(str::to_string),
            turn_permit: None,
            yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
            process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
            prompt_tx,
            interrupt_tx,
            interject_tx,
            session_tx,
            store: Arc::new(crate::thread_store::ThreadStore::new(&std::env::temp_dir())),
            status: Arc::new(std::sync::Mutex::new(status)),
            subagent_tx: tokio::sync::watch::channel(crate::protocol::SubagentTotals::default()).0,
            // busy 判定不碰会话写锁；这里只是补字段。
            writer_lock: None,
        }
    }

    fn table(sessions: Vec<ThreadSession>) -> HashMap<String, ThreadSession> {
        sessions
            .into_iter()
            .enumerate()
            .map(|(i, session)| (format!("t{i}"), session))
            .collect()
    }

    #[test]
    fn an_empty_table_is_not_busy() {
        assert!(!local_busy(&table(vec![])));
    }

    #[test]
    fn a_running_session_is_busy() {
        assert!(local_busy(&table(vec![session(
            ThreadStatus::Running,
            Some("turn-1")
        )])));
    }

    /// `AwaitingApproval` 也算有活：那一刻 turn 没结束、上下文都在。机器睡了，
    /// 用户回来点批准时这条 turn 已经随睡眠死掉了。
    #[test]
    fn awaiting_approval_is_busy() {
        assert!(local_busy(&table(vec![session(
            ThreadStatus::AwaitingApproval,
            Some("turn-1")
        )])));
    }

    /// 覆盖「turn 已挂上但状态还没切成 Running」的短暂窗口。
    #[test]
    fn an_active_turn_with_an_idle_status_is_still_busy() {
        assert!(local_busy(&table(vec![session(
            ThreadStatus::Idle,
            Some("turn-1")
        )])));
    }

    #[test]
    fn an_idle_session_without_a_turn_is_not_busy() {
        assert!(!local_busy(&table(vec![session(ThreadStatus::Idle, None)])));
    }

    /// 表里只要有一个在跑就是 busy。
    #[test]
    fn one_busy_session_makes_the_whole_table_busy() {
        let sessions = vec![
            session(ThreadStatus::Idle, None),
            session(ThreadStatus::Running, Some("turn-2")),
        ];
        assert!(local_busy(&table(sessions)));
    }

    /// 远端交互窗口内为真、过期后为假。
    ///
    /// `FixedClock` 的 `since` 直接返回预设值——这正是要测的自由度：
    /// 「时间过去多久」在真实时钟下要真等，这里可构造。
    struct FixedClock(Duration);

    impl Clock for FixedClock {
        fn now(&self) -> Option<Stamp> {
            Some(Stamp::from_duration(Duration::ZERO))
        }
        fn since(&self, _stamp: Stamp) -> Duration {
            self.0
        }
        fn wall(&self) -> Option<Stamp> {
            Some(Stamp::from_duration(Duration::ZERO))
        }
        fn mono_trusted(&self) -> bool {
            true
        }
    }

    #[test]
    fn remote_touch_expires_at_the_window_boundary() {
        let stamp = Stamp::from_duration(Duration::ZERO);
        // 边界取 `<`：恰好 5 分钟已过期。
        assert!(remote_touch_busy(
            Some(stamp),
            &FixedClock(Duration::from_secs(299)),
            REMOTE_TOUCH_WINDOW
        ));
        assert!(!remote_touch_busy(
            Some(stamp),
            &FixedClock(REMOTE_TOUCH_WINDOW),
            REMOTE_TOUCH_WINDOW
        ));
    }

    /// 回归哨兵：从没收到过远端帧就是 `None`，必须判假——
    /// 若写成 `is_none_or`，一台纯桌面使用的机器会恒为 busy、电池下永不释放。
    #[test]
    fn no_remote_touch_ever_is_not_busy() {
        assert!(!remote_touch_busy(
            None,
            &FixedClock(Duration::ZERO),
            REMOTE_TOUCH_WINDOW
        ));
    }

    /// 回归哨兵（spec 明确点名）：**stdio 的本地桌面客户端不得刷新远端时间戳。**
    ///
    /// 这条判据编码在 `server.rs` 的入站帧分支里（`if client != ClientId::local()`），
    /// 不在本模块内，所以测试只能钉住「不写入 ⇒ 不 busy」这一半语义；另一半由
    /// `ClientId::local()` 的语义保证（`broadcast.rs` 里 stdio 是唯一的 local）。
    ///
    /// 为什么值得单独一条：若漏了这个排除，桌面每发一条消息都会把时间戳刷成
    /// `now`，`remote_touch` 恒为真 ⇒ `ok` 恒为真 ⇒ 电池下永不释放，
    /// 而用户从界面上完全看不出来。
    #[test]
    fn a_local_stdio_client_does_not_keep_the_machine_awake() {
        // 模拟「只有 stdio 本地客户端在发请求」：时间戳从未被写入。
        let never_touched: Option<Stamp> = None;
        // 过了任意长时间仍然是 false。
        for secs in [0u64, 60, 3600, 8 * 3600] {
            assert!(
                !remote_touch_busy(
                    never_touched,
                    &FixedClock(Duration::from_secs(secs)),
                    REMOTE_TOUCH_WINDOW
                ),
                "a local-only session must never look like remote activity (t={secs}s)"
            );
        }
    }

    /// 窗口真的被参数化了，而不是还在读 `REMOTE_TOUCH_WINDOW`。
    #[test]
    fn a_custom_remote_window_expires_at_its_own_boundary() {
        let stamp = Stamp::from_duration(Duration::ZERO);
        let window = Duration::from_secs(120);
        // 第 119 秒仍在窗口内。
        assert!(remote_touch_busy(
            Some(stamp),
            &FixedClock(Duration::from_secs(119)),
            window
        ));
        // 第 120 秒边界到期（判定取 `<`）。
        assert!(!remote_touch_busy(
            Some(stamp),
            &FixedClock(Duration::from_secs(120)),
            window
        ));
    }
}
