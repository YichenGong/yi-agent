//! 主动观察：别的进程改了 `settings.json` 时，本进程也能在下一秒内采用。
//!
//! 为什么需要它：`LiveSettings::commit` 只唤醒**本进程**的读者（它直接
//! `notify()`）。手机端保存、另一个 sidecar 保存、或者用户手改了文件，都不会
//! 经过本进程的 `commit`，所以只有周期重读才能发现。
//!
//! 三条硬约束（见 plan Task 11）：
//!
//! 1. **不依赖调用线程的 executor。** daemon 是标准线程 + 间歇
//!    `runtime.block_on`，`tokio::spawn` 在空闲期根本不被驱动。所以每个
//!    `LiveSettings` 的第一个订阅起一个命名线程，线程里自建一个 Tokio runtime
//!    跑 `observer_loop`。同步宿主（TUI 装配、daemon）因此也能观察。
//! 2. **只发失效信号，不执行 consumer。** 观察线程只 `refresh()` + 唤醒 watch，
//!    绝不在线程里跑会话逻辑或等 ACK。
//! 3. **最后一个租约退出必须能停下线程。** 线程持有 `Arc<LiveSettings>`，但它是
//!    `Weak` 升级 + 取消令牌驱动，故不形成「Arc 永不为零 → 线程永不退出」的环。
//!
//! 本地 `commit` 已经直接 `notify()`，因此观察线程**不订阅自己的 watch**（否则
//! 每次本地保存都会多触发一次周期刷新，形成反馈）。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::LiveSettings;

const STARTING: u8 = 0;
const RUNNING: u8 = 1;
const STOPPED: u8 = 2;

/// 正常轮询间隔。spec 要求「正常至多 1 秒」。
pub(crate) const OBSERVE_INTERVAL: Duration = Duration::from_secs(1);

/// 一个订阅。Drop 即退订；最后一个订阅退出时观察线程收到取消并结束。
///
/// `shutdown()` / `shutdown_blocking()` 用于宿主在退出路径上**等**线程真的结束
/// （不在 reactor 上同步 join，也不在观察线程自身 join）。
pub struct SettingsObservationLease {
    heartbeat: Arc<ObservationHeartbeat>,
}

impl SettingsObservationLease {
    /// 异步宿主用：取消并把 join 交给阻塞线程池。
    pub async fn shutdown(self) {
        let heartbeat = Arc::clone(&self.heartbeat);
        drop(self);
        let _ = tokio::task::spawn_blocking(move || heartbeat.join()).await;
    }

    /// 同步宿主（daemon 线程、进程退出）用：直接 join。
    ///
    /// 绝不能在观察线程自身里调用——那会自我 join 并永远挂住。
    pub fn shutdown_blocking(self) {
        let heartbeat = Arc::clone(&self.heartbeat);
        drop(self);
        heartbeat.join();
    }

    /// 测试/诊断用：观察线程是否仍在运行。
    pub fn is_running(&self) -> bool {
        self.heartbeat.is_running()
    }
}

impl Drop for SettingsObservationLease {
    fn drop(&mut self) {
        if self.heartbeat.release() {
            // 最后一个租约：动手停线程，但不在这里 join（Drop 可能发生在 reactor
            // 上）。线程自己会观察到取消并退出；显式 shutdown 才等待。
            self.heartbeat.cancel();
        }
    }
}

impl std::fmt::Debug for SettingsObservationLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SettingsObservationLease")
            .field("running", &self.is_running())
            .finish()
    }
}

/// 订阅计数 + 取消 + join 句柄。
///
/// 计数用独立原子，**不用 `Arc` 强引用数**：观察线程自己持有 `LiveSettings` 的
/// `Arc`（否则在两次轮询之间实例可能被回收），用 Arc 计数会被它永久抬高一个。
struct ObservationHeartbeat {
    subscribers: std::sync::Mutex<usize>,
    cancel: CancellationToken,
    join: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    /// 0 = 正在启动，1 = 运行中，2 = 已停止。
    ///
    /// 三态而不是 `running: bool`：判「旧线程是否已经退出」时必须能区分
    /// 「还没跑起来」和「已经退出」。用 bool 会让一次紧跟其后的
    /// `start_observing` 误判刚起的线程为死线程，把它 cancel+join 掉。
    state: AtomicU8,
    /// 线程启动失败时的诊断（不阻止按需读取，也不谎称观察正常）。
    degraded: AtomicBool,
}

impl ObservationHeartbeat {
    fn release(&self) -> bool {
        let mut count = self
            .subscribers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *count = count.saturating_sub(1);
        *count == 0
    }

    fn cancel(&self) {
        self.cancel.cancel();
        // 不 join：见 `SettingsObservationLease::drop` 的说明。线程退出时会把
        // `running` 置回 false，显式 shutdown 才会 join。
    }

    fn join(&self) {
        self.cancel.cancel();
        let handle = self
            .join
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(handle) = handle {
            // 自我 join 会永久挂住；宁可报错也不静默卡死。
            if handle.thread().id() == std::thread::current().id() {
                debug_assert!(false, "observer joined itself");
                return;
            }
            let _ = handle.join();
        }
    }

    fn is_running(&self) -> bool {
        self.state.load(Ordering::Acquire) != STOPPED
    }

    fn has_exited(&self) -> bool {
        self.state.load(Ordering::Acquire) == STOPPED
    }

    fn is_degraded(&self) -> bool {
        self.degraded.load(Ordering::Acquire)
    }
}

/// 每个 `LiveSettings` 一个观察线程的登记处。
///
/// 存在意义是让「最后租约退出 → 新租约到来」串行：必须等旧线程真的退出，才能
/// 起新线程，否则会出现短暂的双 observer 并且两个都会在下一轮写同一份缓存。
static OBSERVERS: std::sync::Mutex<
    Option<std::collections::HashMap<std::path::PathBuf, Arc<ObservationHeartbeat>>>,
> = std::sync::Mutex::new(None);

fn observers() -> std::sync::MutexGuard<
    'static,
    Option<std::collections::HashMap<std::path::PathBuf, Arc<ObservationHeartbeat>>>,
> {
    OBSERVERS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl LiveSettings {
    /// 开始（或复用）本实例的主动观察，返回一个订阅租约。
    ///
    /// 同步可调用：装配层（daemon 线程、TUI 装配）都不在 async 上下文里。
    pub fn start_observing(self: &Arc<Self>) -> SettingsObservationLease {
        let mut registry = observers();
        let map = registry.get_or_insert_with(std::collections::HashMap::new);

        // 旧线程已经自行退出（`Drop` 只取消、不 join）：先把它收尾，避免
        // 同时存在两个观察线程。
        if let Some(previous) = map.get(&self.path).cloned()
            && previous.has_exited()
        {
            previous.join();
            map.remove(&self.path);
        }

        let heartbeat = map
            .get(&self.path)
            .cloned()
            .unwrap_or_else(|| {
                let heartbeat = Arc::new(ObservationHeartbeat {
                    subscribers: std::sync::Mutex::new(0),
                    cancel: CancellationToken::new(),
                    join: std::sync::Mutex::new(None),
                    state: AtomicU8::new(STARTING),
                    degraded: AtomicBool::new(false),
                });
                let handle = spawn_observer(Arc::clone(self), Arc::clone(&heartbeat));
                match handle {
                    Ok(handle) => {
                        *heartbeat
                            .join
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(handle);
                    }
                    Err(error) => {
                        // 起不了线程：标记降级，但绝不阻止按需读取。租约照发，
                        // 所以调用方不会因为诊断问题而起不来。
                        heartbeat.degraded.store(true, Ordering::Release);
                        tracing::warn!(
                            path = %self.path.display(),
                            error = %error,
                            "settings observer thread failed to start; live cross-process refresh is degraded"
                        );
                    }
                }
                map.insert(self.path.clone(), Arc::clone(&heartbeat));
                heartbeat
            });

        *heartbeat
            .subscribers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) += 1;
        SettingsObservationLease { heartbeat }
    }

    /// 本实例的观察是否正在运行（诊断用；没有租约时为 false）。
    pub fn observation_running(&self) -> bool {
        observers()
            .as_ref()
            .and_then(|map| map.get(&self.path))
            .is_some_and(|heartbeat| heartbeat.is_running())
    }

    /// 观察线程启动失败时为真（诊断用）。
    pub fn observation_degraded(&self) -> bool {
        observers()
            .as_ref()
            .and_then(|map| map.get(&self.path))
            .is_some_and(|heartbeat| heartbeat.is_degraded())
    }
}

fn spawn_observer(
    live: Arc<LiveSettings>,
    heartbeat: Arc<ObservationHeartbeat>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    let cancel = heartbeat.cancel.clone();
    std::thread::Builder::new()
        .name("yi-agent-settings-observer".to_string())
        .spawn(move || {
            // 线程自建 runtime：调用方的线程可能没有 executor，或者（daemon）
            // 只是间歇 block_on。
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    heartbeat.degraded.store(true, Ordering::Release);
                    tracing::warn!(error = %error, "settings observer runtime failed");
                    return;
                }
            };
            heartbeat.state.store(RUNNING, Ordering::Release);
            runtime.block_on(observer_loop(live, cancel));
            heartbeat.state.store(STOPPED, Ordering::Release);
        })
}

/// 周期刷新，直到取消。
///
/// 提出来是为了让测试能在暂停的 runtime 里直接 `spawn` 它，用虚拟时钟验证
/// 「999ms 不刷新、1000ms 刷新」，而不是拿真实时钟 + sleep 冒充。
pub(crate) async fn observer_loop(live: Arc<LiveSettings>, stop: CancellationToken) {
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(OBSERVE_INTERVAL) => {
                // 只做短事务：读文件 + 校验 + 发布。`refresh` 自己发 watch 失效；
                // 内容没变时它不会唤醒任何人（`changed_since_last_publish`）。
                let _ = live.refresh();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live_settings::LiveSettings;
    use serde_json::json;

    fn write(path: &std::path::Path, body: serde_json::Value) {
        std::fs::write(path, serde_json::to_vec(&body).unwrap()).unwrap();
    }

    /// 本进程 `commit` 必须立刻唤醒读者，不等轮询周期。
    ///
    /// 这条只证明"本地保存不用等 1 秒"，不证明观察器有效——下面另有外部替换
    /// 的周期测试。
    #[tokio::test(start_paused = true)]
    async fn a_local_commit_wakes_without_waiting_for_the_poll_tick() {
        let dir = tempfile::tempdir().unwrap();
        let live = LiveSettings::shared(dir.path().join("settings.json"));
        let stop = CancellationToken::new();

        let mut changes = live.subscribe();
        live.commit(
            &json!({ "tui.terminal_title": "false" }),
            crate::settings::store::PatchMode::ReplaceProvided,
        )
        .unwrap();
        changes.changed().await.unwrap();
        assert_eq!(
            live.refresh().settings.tui.terminal_title.as_deref(),
            Some("false")
        );
        stop.cancel();
    }

    /// 外部改写（另一个进程、手机端、手改文件）只在下一次周期被看到：
    /// 999ms 不刷新，1000ms 刷新。用虚拟时钟，不拿真实 sleep 冒充。
    #[tokio::test(start_paused = true)]
    async fn an_external_edit_is_seen_at_the_next_tick() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        write(&path, json!({}));

        let live = LiveSettings::shared(path.clone());
        // 先建立"已发布"基线，这样后面的变化能被判定为变化。
        live.refresh();
        let mut changes = live.subscribe();

        let stop = CancellationToken::new();
        let observer = tokio::spawn(observer_loop(Arc::clone(&live), stop.clone()));
        // 虚拟时钟下必须先让任务被 poll 一次，它的 `sleep` 才会以"现在"为基准
        // 注册到期时刻；否则后面 `advance(999ms)` 时基准还停在未注册状态，
        // 整轮推进就少了一拍。
        settle().await;

        // 外部进程落盘（不经过本进程的 commit，因此不会 notify）。
        write(&path, json!({ "agent": { "max_turns": 77 } }));

        tokio::time::advance(Duration::from_millis(999)).await;
        tokio::task::yield_now().await;
        // 只读**已发布**快照：`refresh()` 自己会重读文件，用它当判据等于
        // 把被测对象当成测量工具。
        assert_eq!(
            live.published()
                .as_ref()
                .and_then(|s| s.settings.agent.max_turns),
            None,
            "999ms 内不应发布：那说明周期比 1 秒短"
        );

        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        // 让出给观察任务，直到它真的发布。**不用** `timeout(1s, changed())`：
        // 在 `start_paused` 下两者同为 1 秒，自动推进可能先让 timeout 到期，
        // 于是测试变成"谁先被调度"的竞态。
        drive_until(|| {
            live.published()
                .as_ref()
                .and_then(|s| s.settings.agent.max_turns)
                .is_some()
        })
        .await;
        let _ = &mut changes;
        assert_eq!(
            live.published()
                .as_ref()
                .and_then(|s| s.settings.agent.max_turns),
            Some(77)
        );

        stop.cancel();
        observer.await.unwrap();
    }

    /// 坏文件 → 降级；文件修好 → 自愈。健康变化也必须唤醒读者（同 revision）。
    #[tokio::test(start_paused = true)]
    async fn a_corrupt_file_degrades_and_heals_on_the_next_tick() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        write(&path, json!({ "agent": { "max_turns": 12 } }));

        let live = LiveSettings::shared(path.clone());
        assert_eq!(live.refresh().settings.agent.max_turns, Some(12));
        let mut changes = live.subscribe();

        let stop = CancellationToken::new();
        let observer = tokio::spawn(observer_loop(Arc::clone(&live), stop.clone()));
        settle().await;

        std::fs::write(&path, b"{ not json").unwrap();
        tokio::time::advance(OBSERVE_INTERVAL).await;
        drive_until(|| {
            live.published()
                .is_some_and(|s| s.health.state == crate::live_settings::HealthState::Degraded)
        })
        .await;
        let _ = &mut changes;
        let degraded = live.published().expect("published");
        assert_eq!(
            degraded.health.state,
            crate::live_settings::HealthState::Degraded
        );
        // 最后有效值仍在用：降级不等于清空。
        assert_eq!(degraded.settings.agent.max_turns, Some(12));

        write(&path, json!({ "agent": { "max_turns": 13 } }));
        tokio::time::advance(OBSERVE_INTERVAL).await;
        drive_until(|| {
            live.published().is_some_and(|s| {
                s.health.state == crate::live_settings::HealthState::Healthy
                    && s.settings.agent.max_turns == Some(13)
            })
        })
        .await;
        let healed = live.published().expect("published");
        assert_eq!(
            healed.health.state,
            crate::live_settings::HealthState::Healthy
        );
        assert_eq!(healed.settings.agent.max_turns, Some(13));

        stop.cancel();
        observer.await.unwrap();
    }

    /// 删掉文件 = 回默认（不是错误）。
    #[tokio::test(start_paused = true)]
    async fn a_deleted_file_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        write(&path, json!({ "agent": { "max_turns": 12 } }));

        let live = LiveSettings::shared(path.clone());
        assert_eq!(live.refresh().settings.agent.max_turns, Some(12));
        let mut changes = live.subscribe();

        let stop = CancellationToken::new();
        let observer = tokio::spawn(observer_loop(Arc::clone(&live), stop.clone()));
        settle().await;

        std::fs::remove_file(&path).unwrap();
        tokio::time::advance(OBSERVE_INTERVAL).await;
        drive_until(|| {
            live.published()
                .is_some_and(|s| s.settings.agent.max_turns.is_none())
        })
        .await;
        let _ = &mut changes;
        assert_eq!(
            live.published()
                .as_ref()
                .and_then(|s| s.settings.agent.max_turns),
            None
        );

        stop.cancel();
        observer.await.unwrap();
    }

    /// 两个租约共享一个线程；取消一个另一个仍有效；最后一个退出后线程停下。
    ///
    /// 用 deadline 轮询 `is_running`，不用固定 sleep：启动线程本身有调度延迟。
    #[tokio::test(flavor = "multi_thread")]
    async fn leases_share_one_observer_and_the_last_one_stops_it() {
        let dir = tempfile::tempdir().unwrap();
        let live = LiveSettings::shared(dir.path().join("settings.json"));

        let first = live.start_observing();
        let second = live.start_observing();
        wait_until(Duration::from_secs(5), || first.is_running())
            .await
            .expect("the observer thread starts");
        assert!(second.is_running());
        assert!(live.observation_running());

        drop(first);
        // 还有一个租约：线程必须继续。
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            second.is_running(),
            "cancelling one lease must not stop the observer"
        );

        second.shutdown_blocking();
        wait_until(Duration::from_secs(5), || !live.observation_running())
            .await
            .expect("the last lease stops the observer");
    }

    /// 最后租约退出后重新订阅：新线程必须真的跑起来（旧线程已 join）。
    #[tokio::test(flavor = "multi_thread")]
    async fn resubscribing_after_the_last_lease_starts_a_fresh_observer() {
        let dir = tempfile::tempdir().unwrap();
        let live = LiveSettings::shared(dir.path().join("settings.json"));

        let first = live.start_observing();
        wait_until(Duration::from_secs(5), || first.is_running())
            .await
            .unwrap();
        first.shutdown_blocking();
        wait_until(Duration::from_secs(5), || !live.observation_running())
            .await
            .unwrap();

        let second = live.start_observing();
        wait_until(Duration::from_secs(5), || second.is_running())
            .await
            .expect("a fresh observer starts");
        second.shutdown_blocking();
    }

    /// 观察器真的能把外部变化送到「没有自己 block_on 循环」的同步宿主：
    /// `start_observing` 返回后不驱动任何 future，文件变化仍被发布。
    #[tokio::test(flavor = "multi_thread")]
    async fn a_synchronous_host_observes_without_driving_an_executor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        write(&path, json!({}));

        let live = LiveSettings::shared(path.clone());
        live.refresh();
        let mut changes = live.subscribe();

        let lease = live.start_observing();
        write(&path, json!({ "agent": { "max_turns": 91 } }));

        // 本测试自身不驱动观察线程：它有自己的 runtime。
        tokio::time::timeout(Duration::from_secs(5), changes.changed())
            .await
            .expect("the observer publishes externally")
            .expect("watch alive");
        assert_eq!(
            live.published()
                .as_ref()
                .and_then(|s| s.settings.agent.max_turns),
            Some(91)
        );
        lease.shutdown_blocking();
    }

    /// 让新 spawn 的任务被 poll 一次（`start_paused` 下注册它的定时器）。
    async fn settle() {
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    /// 让出调度直到条件成立。配合 `start_paused` 的虚拟时钟使用。
    async fn drive_until(mut condition: impl FnMut() -> bool) {
        for _ in 0..10_000 {
            if condition() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("the observer never published the expected snapshot");
    }

    async fn wait_until(deadline: Duration, mut condition: impl FnMut() -> bool) -> Result<(), ()> {
        let start = std::time::Instant::now();
        while start.elapsed() < deadline {
            if condition() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Err(())
    }
}
