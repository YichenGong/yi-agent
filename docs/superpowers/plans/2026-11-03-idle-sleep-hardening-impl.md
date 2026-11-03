# 任务运行防休眠加固 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 消除防休眠的网络误判、失败 turn 残留与探测退出阻塞，并让显式请求保活的后台进程参与 busy。

**Architecture:** 用 getifaddrs 双栈快照代替 route/TCP；Supervisor 将三态网络资格转为原策略布尔。PowerGuard 用一致输入快照与 Condvar 管理线程，turn 启动失败共享身份回滚，ProcessManager 保存显式进程保活意图供 app-server 聚合。

**Tech Stack:** Rust、libc/getifaddrs、std Mutex/Condvar/thread、Tokio/mpsc、serde、macOS IOKit；TypeScript/Vitest；不新增依赖。

## Global Constraints

- 唯一需求源：[spec](../specs/2026-11-03-idle-sleep-hardening-design.md)，基线 `f634664b`；本文件是待执行计划，不是实现报告。
- 保留 `eligible_network && (on_ac || busy)`、宽限边界 `<`、睡眠感知时钟、daemon 白名单和已有设置校验。
- 保留 macOS 单一持有者；不覆盖 CLI/TUI、Linux/Windows 真实断言、孤儿 daemon，不阻止合盖/手动/低电量强制睡眠/重启，不阻屏幕熄灭。
- 网络资格只表示本地链路，不表示互联网/provider 可达；Unknown eligible=true，成功无候选 Offline；虚拟/旁路前缀集合保持原规则。
- `process_start.prevent_idle_sleep` 严格 boolean、默认 false；与 on_exit/readiness/运行时长无关。
- PowerGuard 可控测试预算 1 秒，真机预算 2 秒，不是 OS 硬实时契约；输入采样仍是主循环 10s。
- 不升级依赖或工具链；IPv6 link-local 用 octets 判别，避免使用超过 Cargo.toml rust-version 的新稳定 API。
- 不在 main 改文件。实施时新建 `fix/idle-sleep-hardening` worktree；每个交付提交同步进度、`cargo fmt --all`、`just ci`；合并结果再 `just ci`。
- 所有新增用例前缀 `sleep_hardening_`，不得只因过滤命令 exit 0 就宣布覆盖。
- 本轮分三组可独立审查的交付：A 网络/线程（Tasks 1–2），B turn（Tasks 3–4），C 后台进程（Tasks 5–6）；Task 7 统一验收。不要顺带重构大模块。

## 文件职责与执行约定

所有 Rust 路径下文以 `R=yi-agent-rs/crates` 为前缀；命令默认从 **实施 worktree 根**执行。`R` 只是文档路径缩写，不是运行时变量。

| 文件 | 职责 |
|---|---|
| `yi-agent-rs/crates/yi-agent-power/src/probe.rs` | 地址/接口资格纯函数、三态、平台快照复制与释放 |
| `yi-agent-rs/crates/yi-agent-power/src/supervisor.rs` | 网络资格转换、Unknown 边沿日志与策略非回归 |
| `yi-agent-rs/crates/yi-agent-app-server/src/power_guard.rs` | 唯一断言线程、一致输入、可通知等待、join |
| `yi-agent-rs/crates/yi-agent-app-server/src/server/transport/mod.rs` | 原主循环一次采样、一次 update，不在这里做 FFI |
| 新 `yi-agent-rs/crates/yi-agent-app-server/src/server/turn/failure.rs` | 只做启动期失败收口，不接管正常 driver 持久化 |
| `yi-agent-rs/crates/yi-agent-app-server/src/server/turn/mod.rs` | RPC started/response/send 顺序和启动期失败处理 |
| `yi-agent-rs/crates/yi-agent-app-server/src/server/driver/launcher.rs` | 插件与 cold lead 复用 helper，warm 语义不动 |
| `yi-agent-rs/crates/yi-agent-tools/src/process/manager.rs` | 意图生命周期保存、DTO 输出、轻量活跃谓词 |
| `yi-agent-rs/crates/yi-agent-tools/src/process/tools.rs` | 默认/类型/schema/说明与 options 接线 |
| `yi-agent-rs/crates/yi-agent-app-server/src/power_busy.rs` | 会话状态与托管进程 OR |
| `desktop/src/lib/protocol.ts` | 手写 DTO 的向后兼容字段 |
| `docs/project-management/yi-agent-power.md`、`README.md`、`docs/bug-list.md` | 按实际测试证据更新，不以文档代替修复 |

测试命令模板（每个 Task 明确指定 crate）：

```sh
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:/opt/homebrew/bin:/sbin:$PATH"
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-power sleep_hardening_ -- --list
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-power sleep_hardening_ -- --nocapture
```

先从 `--list` 检查该 Task 的所有名字，再跑测试。新 API 初次导致的编译红只证明接口未实现；必须另记录旧行为兼容复现或保留 API 的产品变异红。Task 最后提交的统一动作：

```sh
(cd yi-agent-rs && cargo fmt --all && just ci) && git diff --check
# git add 仅本 Task Files 所列文件及对应进度文档；随后使用各 Task 的 commit message。
```

不要把约 5 分钟以上的 CI 当成一个 2–5 分钟编辑动作；它是单独等待门禁。锁排队按 CLAUDE.md 诊断，不删除锁文件。默认 mock，不加 `--ignored` 调真实 LLM。

---

### Task 1: 被动双栈网络资格与 Supervisor（P1）

**Files:** Modify/Test `R/yi-agent-power/src/{probe.rs,supervisor.rs}`；所有 NetProbe 实现通过 `git grep -n 'impl NetProbe\|impl .*NetProbe' -- yi-agent-rs` 找齐；更新 power 进度中的 P1 和旧网关说明。

**Interfaces:** Produces `pub struct InterfaceSnapshot { pub name: String, pub flags: u32, pub addresses: Vec<IpAddr> }`、`pub enum NetworkState { Online, Offline, Unknown }`、`NetworkState::eligible(self) -> bool`、`pub fn network_state(interfaces: &[InterfaceSnapshot]) -> NetworkState`、`NetProbe::state(&self) -> NetworkState`。删除旧 `online()` trait 方法，不并存两套资格；Policy::Snapshot.online 保持 bool。

- [ ] Step 1：在 probe.rs 写纯资格测试。先以兼容旧三元组输入复现 IPv6-only 没有 v4 的 Offline，再使用下面新接口测试。代表性完整测试：

```rust
#[test]
fn sleep_hardening_ipv6_only_is_online() {
    let interfaces = vec![InterfaceSnapshot {
        name: "en0".into(),
        flags: (libc::IFF_UP | libc::IFF_RUNNING) as u32,
        addresses: vec!["2001:db8::1234".parse().unwrap()],
    }];
    assert_eq!(network_state(&interfaces), NetworkState::Online);
}
#[test]
fn sleep_hardening_successful_empty_snapshot_is_offline() {
    assert_eq!(network_state(&[]), NetworkState::Offline);
    assert!(NetworkState::Unknown.eligible());
    assert!(!NetworkState::Offline.eligible());
}
```

同一表驱动测试逐行覆盖：v4 192.0.2.1/10.0.0.1、v6 fd00::1 为 Online；0.0.0.0、127.0.0.1、169.254.1.1、224.0.0.1、255.255.255.255、::、::1、fe80::1、ff02::1 单独出现为 Offline；DOWN、只有 UP、loopback、utun/awdl/llw/bridge 单独出现为 Offline。N1–N5。

- [ ] Step 2：运行 power 过滤测试与 --list，确认 IPv6 回归断言红；缺类型编译红之后还需把新 `network_state` 临时改成仅判 v4，确认 IPv6/ULA 两条能正常编译且断言红。
- [ ] Step 3：实现单播纯函数与三态：

```rust
impl NetworkState {
    pub fn eligible(self) -> bool { !matches!(self, Self::Offline) }
}
fn usable_address(address: &std::net::IpAddr) -> bool {
    match address {
        std::net::IpAddr::V4(ip) => !ip.is_unspecified()
            && !ip.is_loopback() && !ip.is_multicast()
            && !ip.is_link_local() && *ip != std::net::Ipv4Addr::BROADCAST,
        std::net::IpAddr::V6(ip) => {
            let b = ip.octets();
            !ip.is_unspecified() && !ip.is_loopback() && !ip.is_multicast()
                && !(b[0] == 0xfe && (b[1] & 0xc0) == 0x80)
        }
    }
}
```

`network_state` 用 `.any()` 检查 UP/RUNNING、非 LOOPBACK、`!is_virtual_interface`、任一 usable_address。成功无候选返回 Offline。给 NetworkState derive Debug/Clone/Copy/PartialEq/Eq。

- [ ] Step 4：替换平台采样。getifaddrs 返回错误或头为空为 Unknown；用 RAII 局部持有头指针，Drop 调 `libc::freeifaddrs`。遍历同名接口聚合 flags/地址；`ifa_addr` 为 null 跳过，按 sa_family 分支；v4 `Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr))`，v6 `Ipv6Addr::from(addr.sin6_addr.s6_addr)`。其他地址族不转型。无 unix 支持的构建返回 Unknown，不引入新的平台真实断言。

在 probe.rs 测试模块构造 sockaddr_in/sockaddr_in6，检查已知字节逐字恢复；同接口两节点保留两种地址，RAII 所有返回路径释放。释放测试用局部可注入采集器/释放计数，不伪造一个会被真实 freeifaddrs 释放的 Rust 堆地址。N7。

- [ ] Step 5：删除 route/TCP/缓存代码、parse_gateway、classify_connect_result 与失效测试。SystemNetProbe::new 不再含主动缓存。Supervisor 添加 `last_network_state: Option<NetworkState>`；每个 enabled、时钟有效的 tick 采样一次 state，Unknown 边沿打印，赋 Snapshot.online=state.eligible()。日志用 `fn entered_unknown(previous: Option<NetworkState>, next: NetworkState) -> bool` 纯函数测试 None→Unknown、Unknown→Unknown、Online→Unknown 三种；不改时钟失败释放语义。N6/N8。

- [ ] Step 6：在现有 Supervisor FakeClock/CountingAssertion 夹具中将 FixedNet(bool) 换三态，测试电池 busy + Online/Unknown 两拍 acquire、Offline 超过 grace release，以及 Unknown 不绕过 enabled=false/busy=false/on_ac=false。跑完整 power crate 与 app-server `power_`，确认没有遗漏 trait 实现。源码静态检索 probe.rs 不含 `TcpStream`、`Command::new`、`CONNECT_TIMEOUT`；此检查辅助 N6，不代替测试。
- [ ] Step 7：P1 进度写入测试名字、网络词义/虚拟唯一出口边界；CI 全绿后提交 `fix: use passive dual-stack sleep eligibility`。

### Task 2: 一致快照、通知等待与真实停机测试（P3）

**Files:** Modify/Test `R/yi-agent-app-server/src/power_guard.rs`；Modify `R/yi-agent-app-server/src/server/transport/mod.rs`；更新 power P3 与 docs/bug-list P3。

**Interfaces:** Consumes Task 1 Supervisor/NetProbe。Produces `PowerGuard::spawn(enabled: bool, grace_secs: u64) -> Self`、`PowerGuard::update(&self, busy: bool, enabled: bool, grace_secs: u64)`。删除三个 setter/remote_secs；主循环仍消费 remote_secs。新增私有 `run_worker(shared: Arc<(Mutex<GuardInputs>, Condvar)>, make_supervisor: impl FnOnce() -> Supervisor, after_tick: impl FnMut())`；工厂在线程中调用，不跨线程搬运非 Send FFI。`PowerGuard::spawn_with_factory(initial: GuardInputs, factory: impl FnOnce() -> Supervisor + Send + 'static, after_tick: impl FnMut() + Send + 'static) -> Self` 负责构造shared/spawn/保存handle，生产spawn调用它且after_tick为 `|| {}`，测试用channel观察首拍结束。测试不另写循环。

- [ ] Step 1：提取 worker 工厂 seam，不改旧轮询语义；测试假 clock/power/net/assertion 的真实线程。clock 每次 sample 增加 1 秒，assertion acquire 向 channel 发 `acquired`，release 增加 AtomicUsize；工厂里的假对象符合现有 Send/Sync trait，所有跨线程计数为 Arc。然后写已持断言禁用唤醒测试：

```rust
// 测试调用 PowerGuard::spawn_with_factory(initial, factory, after_tick)；
// factory 使用上段定义的 FakeClock、AlwaysOnline、Battery、CountingAssertion。
acquired_rx.recv_timeout(Duration::from_secs(1)).expect("must acquire first");
guard.update(false, false, 900);
released_rx.recv_timeout(Duration::from_secs(1)).expect("disable wakes worker");
drop(guard);
assert_eq!(releases.load(Ordering::SeqCst), 1);
```

`spawn_worker_for_test` 与生产都调用 run_worker；测试工厂显式满足 `FnOnce() -> Supervisor + Send + 'static`，在线程里创建所有假依赖。Policy 首拍只建立基线：测试输入初始 busy=true/enabled=true，接收首拍结束 hook 的 `tick_done` 后 `guard.update(true, true, 901)` 触发第二拍（假钟前进1秒），再等 acquired；不能直接期待首拍 acquire。首拍结束 hook 是测试局部回调，只观察生产循环，不替代 tick。不得测试独立重写的循环。旧 10s/只在stop轮询行为应在 disable预算断言红。G1 用 enabled=true/已经 acquire 后 Drop，不能继续旧 enabled=false 假覆盖。

- [ ] Step 2：运行 app-server `sleep_hardening_ -- --list`、过滤测试，记录 disable迟到的行为红。保持API的旧等待变异应能编译；回退谓词/notify分别记录 G2/G4 红。
- [ ] Step 3：实现快照与谓词等待核心，GuardInputs derive Clone/Copy/PartialEq/Eq：

```rust
#[derive(Clone, Copy, PartialEq, Eq)]
struct GuardInputs { busy: bool, enabled: bool, grace_secs: u64, stop: bool }
// 下段位于run_worker内，after_tick参数以mut绑定。
let mut supervisor = make_supervisor();
// update 持锁，只在三个业务值不同且 !stop 时替换，放锁后 notify_one。
// stop 只由 Drop 写 true，update 永远不能恢复它。
let (lock, cv) = &*shared;
loop {
    let snapshot = *lock.lock().unwrap_or_else(|p| p.into_inner());
    if snapshot.stop { break; }
    let grace = Duration::from_secs(snapshot.grace_secs);
    supervisor.set_windows(grace, slack_for(grace));
    supervisor.tick(snapshot.busy, snapshot.enabled);
    after_tick(); // 生产为空回调；测试只观察已结束的真实tick。
    let inputs = lock.lock().unwrap_or_else(|p| p.into_inner());
    if inputs.stop { break; }
    let _wait_result = cv.wait_timeout_while(inputs, SUPERVISOR_TICK,
        |current| !current.stop && *current == snapshot)
        .unwrap_or_else(|p| p.into_inner());
}
drop(supervisor);
```

timeout 后放锁再新拍；变化在 tick 中发生时谓词立即返回。伪唤醒由 wait_timeout_while 重查，锁不跨 FFI。Drop置stop/notify/join；加入测试内部 Barrier 阻塞一次 probe，另线程 Drop，确认 Drop确实等待本次本地调用释放、没有后续 tick。测试完成必须释放 Barrier，即使断言失败也要用 RAII 信号清理，不能把测试自己挂死。G3。

- [ ] Step 4：transport 装配改 `spawn(initial_enabled, initial_grace_min*60)`，同拍改 `update(local || remote || daemon, enabled, grace_secs)`；remote_secs仍用于 remote_touch_busy。删除“原子不阻塞”“route 2s”“整个退出50ms”的错误注释。相同update不notify；短锁不执行I/O。
- [ ] Step 5：补真实循环 G1–G5：已acquire等待中Drop释放一次；disable跳过宽限；首拍Barrier期间stop；tick后stop；busy/enabled/grace一致输入；100次相同update与手动notify不引发额外tick（先由测试hook/channel确认进入wait，计数窗口用 recv_timeout，不盲sleep）；缩窗/扩窗/再启用仍走原Supervisor测试。工厂必须观察测试指定输入，不能固定忽略 enabled/grace。
- [ ] Step 6：运行 app-server完整lib、power全crate、runtime `power_windows`/`prevent_idle_sleep`。macOS真实 enabled=true guard 退出预算测试使用唯一label/同PID，若平台验证手工进行则记录待验收，不声称假依赖测试是IOKit实证。
- [ ] Step 7：更新P3与bug证据，CI后提交 `fix: wake the power worker on input changes`。

### Task 3: 共享失败收口与 RPC receiver 关闭回归（P2）

**Files:** Create `R/yi-agent-app-server/src/server/turn/failure.rs`、`R/yi-agent-app-server/src/server/turn/failure_tests.rs`；Modify `server/turn/mod.rs`；Test failure_tests；不改 rpc_io.rs 产品投递语义。

**Interfaces:** Produces `pub(crate) async fn rollback_turn(threads: &mut HashMap<String, ThreadSession>, hub: &Broadcaster, thread_id: &str, turn_id: &str) -> bool`，matching占位时清 None、按session.status调 update_status(Idle)，否则false。Produces `pub(crate) async fn fail_announced_turn(threads: &mut HashMap<String, ThreadSession>, hub: &Broadcaster, thread_id: &str, turn_id: &str, error: &str)`，一次now_millis→mark_turn_completed→TurnCompleted(Failed)→rollback。helper不得带用户内容append。

- [ ] Step 1：在 failure_tests.rs 构造最小真实 ThreadSession（字段逐项来自 session.rs，不用LLM）：四个mpsc通道、subagent watch、ThreadSession::new_status、YoloSwitch、ProcessManager、临时ThreadStore。先复制同模块现有session构造方法，在本文件定义 `fn test_session(dir: &Path, thread_id: &str) -> (ThreadSession, mpsc::Receiver<TurnPrompt>)`；store按ThreadStore现有 create/read_meta API登记thread，返回前不启动driver。可以用 serde 读最小合法 meta，避免手抄所有带默认字段；夹具实际内容：

```rust
fn test_session(dir: &std::path::Path, thread_id: &str)
    -> (ThreadSession, mpsc::Receiver<TurnPrompt>)
{
    let (prompt_tx, receiver) = mpsc::channel(8);
    let (interrupt_tx, _) = mpsc::channel(1);
    let (interject_tx, _) = mpsc::channel(1);
    let (session_tx, _) = mpsc::channel(1);
    let store = Arc::new(crate::thread_store::ThreadStore::new(dir));
    let meta = serde_json::from_value(serde_json::json!({
        "thread_id": thread_id, "cwd": dir.display().to_string(),
        "model": "test", "created_at": 1, "updated_at": 1
    })).unwrap();
    store.create(&meta).unwrap();
    (ThreadSession {
        thread_id: thread_id.into(), cwd: dir.display().to_string(), model: "test".into(),
        active_turn_id: None, yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
        process_manager: yi_agent_tools::ProcessManager::new(dir.to_path_buf()),
        prompt_tx, interrupt_tx, interject_tx, session_tx, store,
        status: ThreadSession::new_status(),
        subagent_tx: tokio::sync::watch::channel(crate::protocol::SubagentTotals::default()).0,
        writer_lock: None,
    }, receiver)
}
```

`#[cfg(test)] mod failure_tests;` 位于turn/mod.rs，`use super::*` 可访问prepare/start。

RPC回归使用以下实际闭通道动作，不注入整个假start函数：

```rust
let (session, receiver) = test_session(dir.path(), "thread-a");
drop(receiver);
let mut threads = HashMap::from([("thread-a".into(), session)]);
let hub = Arc::new(crate::broadcast::Broadcaster::new());
let client = crate::broadcast::ClientId::local();
let mut output = hub.register(client.clone());
let result = start_turn_core(&mut threads, &HashMap::new(),
    &mut crate::image_upload::UploadRegistry::new(), &hub, &client,
    &serde_json::json!({"threadId":"thread-a","input":[{"type":"text","text":"hello"}]}),
    crate::protocol::RequestId::Num(7)).await;
assert!(result.is_ok());
assert!(threads["thread-a"].active_turn_id.is_none());
assert_eq!(*threads["thread-a"].status.lock().unwrap(), ThreadStatus::Idle);
assert!(!crate::power_busy::local_busy(&threads));
let mut frames = Vec::new();
while let Ok(frame) = output.try_recv() { frames.push(frame); }
assert_eq!(frames.iter().filter(|f| f.get("id") == Some(&serde_json::json!(7))).count(), 1);
assert_eq!(frames.iter().filter(|f| f["method"] == "turn/completed" && f["params"]["status"] == "failed").count(), 1);
```

RequestId::Num(i64) 为真实数字枚举，不用字符串代替数字 id。断言 started、opening item、Running、response 的成功顺序仍与基线相同，failed在response之后。T1。

- [ ] Step 2：运行app-server过滤用例，旧实现应因 Running和缺failed断言红；不是依赖missing helper编译红。给回滚身份写 `active="new"`/failed="old" 测试，保持None/Idle与Some(new)/Running两种分支明确。
- [ ] Step 3：实现共享helper。mark_turn_completed错误日志与persist.rs一致，通知 `.await`结果忽略但不能跳过rollback；rollback先按thread匹配清active，再从该session clone status，放threads借用后await update_status（此函数先改内存再广播）。不依赖传入一个可能是旧session的status_handle；不将Finished投回队列。
- [ ] Step 4：start_turn_core把prepare后的通知/response阶段放入一个返回Result的async块；started成功后立即置announced=true，其前失败只rollback。保存thread_id/turn_id供失败时使用，构造TurnPrompt时clone turn_id避免move后丢身份：

```rust
let thread_id = prepared.thread_id.clone();
let turn_id = prepared.turn_id.clone();
// started/opening/Running/response 若 Err(error)：
// announced ? fail_announced_turn(..., &error.to_string()).await : rollback_turn(...).await;
// return Err(error)，保持原始错误。
// send Err：fail_announced_turn(..., "turn driver closed before prompt delivery").await;
// return Ok(Some(thread_id))，不发第二个Response、不终止共享server。
```

正常send成功不调用helper；不remove pending_activation，不再次消费upload。
- [ ] Step 5：T2真实错误用可靠local响应通道关闭：write_response(local)会Err；WS关闭在现有rpc_io.rs是Ok，不把它当失败。opening/status通知只可能序列化失败、广播Closed被忽略，故新增局部 `#[cfg(test)]` 出站阶段seam（实例参数/closure，不用全局static环境）允许分别在started后opening、Running、response返回原始注入error。精确接口为私有 `#[derive(Clone, Copy)] enum StartStage { Opening, Running, Response }` 与 `start_turn_core_inner`（start_turn_core全部原参数加 `fault: Option<StartStage>`），公开包装永远传None，测试直接调同一inner传Some。每个指定阶段执行真实函数前 `if matches!(fault, Some(StartStage::Opening)) { return Err(anyhow::anyhow!("injected opening failure")); }`，Running/Response分别同样分支和对应文案；此错误必须落入同一失败收口块，不在外层直接return绕过回滚。生产包装与None路径测试都保留。不新增全局环境变量、static hook或依赖。生产仍使用原三个函数，注入运行同一async流程。通知不可投递与store不能写测试仍assert内存Idle/None、原错误不被覆盖；T7用失败store路径或局部完成水位写入closure，不改产品存储格式。
- [ ] Step 6：断言failed completed_at等于store的last_completed_at；store失败不append内容、不阻止回滚；旧turn身份不能清new；notify失败仍回滚。完整app-server lib与已有turn/未读水位测试通过。
- [ ] Step 7：P2进度先保持未完成并记录RPC已覆盖、launcher待Task4，CI后提交 `fix: settle failed rpc turn delivery by identity`。

### Task 4: 插件与 cold lead 复用收口，warm 非回归（P2）

**Files:** Modify `R/yi-agent-app-server/src/server/driver/launcher.rs`；Test `R/yi-agent-app-server/src/server/session_driver_tests.rs`；测试字段构造点 Modify `R/yi-agent-app-server/src/server/driver/tick.rs` 与 `R/yi-agent-app-server/src/server/transport/mod.rs`（仅 `#[cfg(test)]` 默认None）；power模块P2与bug-list P2。

**Interfaces:** Consumes Task3 `turn::failure::{rollback_turn,fail_announced_turn}`；ServeLauncher launch/deliver_to_lead公开签名与成功结果不变。

- [ ] Step 1：复用 session_driver_tests.rs 已有 Host::new、Host::launcher 与cold/warm lead夹具。插件prepare后send失败使用实例级 `#[cfg(test)] before_prompt_send: Option<fn(&mut PreparedTurn)>` 字段，在两处真实prepare完成后、各自真实send前调用一次；PreparedTurn改为mut绑定。callback只替换本次prepared的发送端为关闭接收端的真实mpsc，不模拟send结果：

```rust
fn close_prepared_delivery(prepared: &mut PreparedTurn) {
    let (tx, rx) = mpsc::channel(1);
    drop(rx);
    prepared.prompt_tx = tx;
}
```

`git grep -n 'ServeLauncher {'` 的五处构造（driver/tick、Host::launcher、transport三处）加 `#[cfg(test)] before_prompt_send: None`，单例测试launcher设Some(close_prepared_delivery)。cold prepare后使用同一callback；warm不调用hook（无prepared），测试通过替换其既有session.prompt_tx为已关闭端验证真实send失败。正常driver/host按现有夹具shutdown清理；不替换prepare/launcher整条逻辑，不使用全局hook。

每条真实入口执行后必须有以下断言，tid/turn_id取该入口的实际准备结果：

```rust
assert!(result.is_err());
assert!(host.threads[&tid].active_turn_id.is_none());
assert_eq!(*host.threads[&tid].status.lock().unwrap(), ThreadStatus::Idle);
// warm测试不同：
assert_eq!(host.threads[&tid].active_turn_id.as_deref(), Some(existing_turn_id.as_str()));
assert_eq!(*host.threads[&tid].status.lock().unwrap(), ThreadStatus::Running);
```

T4断言failed一次与水位同源；T5断言无TurnCompleted、无完成水位更新；T6真实warm成功投递两封排队不prepare新占位，失败不清旧turn。cold失败红是基线真实缺陷；插件水位红是同形缺口。
- [ ] Step 2：运行过滤测试并记录上述行为红；warm是保护性绿，不伪称全部基线红。
- [ ] Step 3：删launcher私有两helper，所有原调用改共享helper；cold send Err前调用rollback，保留原字符串 `lead driver is gone before its first prompt`。warm分支保持原样。通道send成功后不补失败；准备前错误/已占位busy错误不回滚。
- [ ] Step 4：跑完整session_driver相关测试、lead_与app-server完整lib；检查外层阻塞ask_lead仍收到投递错误，pending activation留给后续成功首次投递。变异：删除cold rollback→T5红，删mark→T4红，无条件rollback→T3/T6红。
- [ ] Step 5：P2全部判据通过才勾选，CI后提交 `fix: share launcher turn failure cleanup`。

### Task 5: 保存显式进程意图与轻量谓词（P4 数据层）

**Files:** Modify/Test `R/yi-agent-tools/src/process/{manager.rs,tools.rs}`；Modify options构造点 `R/yi-agent-runtime/src/bootstrap.rs`、`R/yi-agent-app-server/src/server/tests/process.rs`；tools模块交叉引用与power P4记录。

**Interfaces:** Adds `prevent_idle_sleep: bool` to ProcessStartOptions、ManagedProcess、ManagedProcessSnapshot、ProcessStartResult、ProcessReadResult；ProcessStartArgs使用 `#[serde(default)]`。Produces `pub fn ProcessManager::has_sleep_preventing_processes(&self) -> bool`。

- [ ] Step 1：tools.rs参数反序列化/schema测试无需新API可先红：

```rust
#[test]
fn sleep_hardening_schema_exposes_explicit_boolean() {
    let tool = ProcessStartTool::new(ProcessManager::new(std::env::temp_dir()));
    let schema = tool.schema();
    assert_eq!(schema["properties"]["prevent_idle_sleep"]["type"], "boolean");
    assert_eq!(schema["properties"]["prevent_idle_sleep"]["default"], false);
    assert!(!schema["required"].as_array().unwrap().iter().any(|v| v == "prevent_idle_sleep"));
}
```

省略/false/true反序列化依次false/false/true，字符串"true"/null/数字1拒绝。未知字段忽略的旧serde行为不能被当作严格boolean验证。
- [ ] Step 2：运行tools过滤测试，schema缺字段为业务红；之后补模型字段与fixture，跑保持API的“丢options字段”变异确认读回红。
- [ ] Step 3：贯穿所有DTO/构造，start时 `prevent_idle_sleep: opts.prevent_idle_sleep`；snapshot/read/start result全部复制原意图，不按当前state覆盖成false。schema说明：

```json
{"type":"boolean","default":false,"description":"Request idle-sleep protection for finite background work while the owning app-server session is alive. Default false; do not enable for persistent servers/watchers. Independent of readiness and on_exit. Other entrypoints only retain this intent; they do not own a sleep assertion."}
```

只 `command` required。使用 `git grep -n 'ProcessStartOptions {' -- yi-agent-rs` 同步manager::start_options、tools::call、runtime bootstrap与app-server tests/process已知3处，以及实施基线之后新增构造点。

- [ ] Step 4：实现谓词，不list：

```rust
pub fn has_sleep_preventing_processes(&self) -> bool {
    self.lock_processes().values().any(|p| p.prevent_idle_sleep
        && matches!(p.status, ProcessStatus::Starting | ProcessStatus::Running | ProcessStatus::Ready))
}
```

manager内部测试直接构造/改变记录status覆盖全部6态、true/false、keep/kill与ready_timeout，不修改线程并发状态机。Starting和FailedToStart可能是短暂/无持久记录，纯状态测试覆盖，不为测试创造新的产品失败记录策略。
- [ ] Step 5：真实有限命令采用已有start_options：`printf READY; read line`，stdin当前为null会立即退出，**不要用它保持活跃**；用临时文件门闩 `printf READY; while [ ! -e "$DONE" ]; do sleep 0.02; done`，env DONE为TempDir路径，ready_pattern=READY。测试等start返回Ready后读list/read/start一致true，写DONE，使用已有wait_until有界确认终态后谓词false；断言ready终态时意图仍true。RAII/manager shutdown确保失败时子进程回收。false常驻对照用sleep30但最后kill，ready timeout不算失败终态。B1–B4。
- [ ] Step 6：跑tools完整process测试、runtime完整lib、app-server process测试，CI后提交 `feat: retain explicit background sleep intent`。P4仍未勾选，待Task6聚合。

### Task 6: app-server 聚合与旧 desktop DTO 兼容（P4 接线）

**Files:** Modify/Test `R/yi-agent-app-server/src/power_busy.rs`；Test `R/yi-agent-app-server/src/server/tests/process.rs`；Modify `desktop/src/lib/protocol.ts`；Test `desktop/src/lib/processes.test.ts`、`desktop/src/components/{ProcessDetail,ProcessRail}.test.tsx`；power P4、tools/app-server交叉引用。

**Interfaces:** Consumes manager谓词；local_busy签名不变。Adds `prevent_idle_sleep?: boolean` to desktop ProcessInfo/ProcessReadResult；当前UI不消费此意图，不为默认false新增没消费者的运行时normalizer。

- [ ] Step 1：沿用power_busy.rs真实ThreadSession测试fixture，设active=None/status=Idle，session.manager启动Task5门闩true进程：

```rust
assert!(session.active_turn_id.is_none());
assert_eq!(*session.status.lock().unwrap(), ThreadStatus::Idle);
let threads = HashMap::from([(session.thread_id.clone(), session)]);
assert!(local_busy(&threads));
// 写DONE并有界等待manager终态后：
assert!(!local_busy(&threads));
```

基线旧local_busy应红；false进程保持false，其他会话Running/审批中仍true，删除含true进程的会话后不再聚合（测试自己负责清理keep进程）。B5/B6/B8。
- [ ] Step 2：运行app-server过滤确认后台busy缺口红，然后在existing `.any`中增加 `|| session.process_manager.has_sleep_preventing_processes()`。保留status锁poison恢复，锁不跨manager遍历；写注释说明三路busy内部新增进程意图，主循环10s采样/最后busy终态宽限从观察到终态后开始。
- [ ] Step 3：power Supervisor FakeClock推进验证Online+电池+busy两拍Hold；后台终态后busyfalse、grace前保持、等于grace释放；其他busy源true不释放。不得真实等15分钟替代策略测试。
- [ ] Step 4：app-server `process/list`/`process/read`本来序列化manager DTO，新增集成测试验证它们都带true，false默认可读。process/updated通知不新增字段/RPC，无UI开关。
- [ ] Step 5：desktop手写两DTO加可选字段和“旧sidecar缺值false”的注释。测试保留原无字段fixture，新增true/falsefixture，运行真实partition/ProcessDetail/ProcessRail不崩溃、原显示不变；类型代码：

```ts
// 两个DTO均新增，不改其他required字段。
prevent_idle_sleep?: boolean;
// 当前没有UI消费者；将来消费时必须使用 info.prevent_idle_sleep ?? false。
```

测试 `expect(info.prevent_idle_sleep ?? false).toBe(false)`只是兼容约定，必须同时运行原实际组件无字段fixture；不能称它证明新增UI功能。B7。
- [ ] Step 6：跑app-server完整lib、tools process、power crate；`cd desktop && npm ci && npx tsc --noEmit && npx vitest run`。DTO不是ts-rs自动生成，不靠bindings-check替代桌面检查。变异去掉local_busy OR→B5红；终态仍true→B6红；false变默认true→B1/B4红。
- [ ] Step 7：P4自动化验收通过才登记实现，保留人工项待验收；CI后提交 `feat: include opted-in managed work in power busy`。

### Task 7: 全矩阵、人工边界与交付门禁

**Files:** Modify `docs/project-management/{yi-agent-power,yi-agent-tools,yi-agent-app-server,README}.md`、`docs/bug-list.md`；Create `docs/research/2026-11-03-idle-sleep-hardening-verification.md`。不新增CI豁免，不改pmset/真实HOME配置。

**Interfaces:** Consumes Tasks1–6实现与测试；Produces 可追溯的矩阵/红绿/变异/真机证据与最终需求状态。

- [ ] Step 1：记录每条 N/T/G/B 对应测试完整名字与实际输出。覆盖映射：Task1 N1–N8；Task2 G1–G5；Task3 T1–T3/T7；Task4 T4–T6；Task5 B1–B4；Task6 B5–B8。一个表测试覆盖多行可以，但输出需指出输入。
- [ ] Step 2：逐组做保持API的变异检查，记录运行命令/失败断言/恢复后的绿。新增schema/缺类型初次编译红单列“接口红”，不能写成产品行为证据。新增未知资格/同快照属于增强，既有保护性绿可以如实写绿，不强行每个用例都基线红。
- [ ] Step 3：运行以下门禁，并保存exit code和实际测试数（--list不得为零）：

```sh
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-power
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-tools --lib process::
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-app-server --lib
cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-runtime --lib
(cd desktop && npx tsc --noEmit && npx vitest run)
(cd yi-agent-rs && cargo fmt --all && just ci)
git diff --check
```

justfile当前ci包含19个依赖，执行入口仍是just ci；不复制旧16道清单。任何失败加载systematic-debugging，不能改成更窄测试宣称门禁通过。

- [ ] Step 4：人工macOS验收前用TempDir隔离YI_AGENT_HOME。真机 worker 的 `spawn_with_factory` 使用 `platform_assertion` 注入包含PID/唯一suffix的测试label；真实隔离app-server仍用固定生产label，以自己的PID过滤，不新增环境变量/设置改变生产label。不改正式实例、不在全局进程内修改PATH做并发探针测试。启动隔离app-server后查 `pmset -g assertions`仅匹配自己PID，保留截图/文本/版本：
  1. IPv6-only global/ULA、UP+RUNNING、有任务且电池应持断言；实际无线flags不符设计时先停交付，回到spec修正资格，不偷偷绕过RUNNING。
  2. 网关80丢包/provider不可达但本地链路在：仍持，明确这是本地链路契约。不得为了验收改变全机防火墙；需专用测试网络，没有则待验收。
  3. true有限后台任务在turn结束后仍持，终态且无其他busy后过设置宽限释放；false常驻不刷新busy；插电仍持为预期。
  4. Wi-Fi真正断开、采样无合格链路→Offline→宽限释放；Unknown不误称Offline。
  5. enabled=false与退出释放，仅从guard观察update/stop时计算2秒预算；设置保存另记录10s采样延迟，不合成50ms承诺。
  6. 屏幕可熄灭；合盖/手动睡眠不在断言保证范围。不需要危险地强制系统睡眠来验证非目标。
  无法切电源/网络的项目明确写待人工，不填通过；如未完成必要真机项，最终状态注明“自动化通过，真机待验收”，不宣称全部验收。

- [ ] Step 5：核对power 12项中P1–P4按各自完成证据勾选，README计数从实际状态计算；不重复计tools/app-server引用。P1/P2/P3 bug项只有对应修复验证通过才能勾选。旧power网关描述/断言ignored错误说明一并改成真实版本行为。spec/plan作为设计基线保持历史，自检与实施结果另放verification，避免篡改“本轮未实现”记录。
- [ ] Step 6：CI通过后提交 `docs: record idle-sleep hardening verification`；使用requesting-code-review审查所有变更，修正后重跑门禁。按finishing-a-development-branch及项目要求合回main，main `just ci`通过后移除实施worktree/分支。不打包/安装覆盖正式app，不清理其他正在开发worktree的target。

每个 ID 的明确映射如下（测试名字在实施验证文档逐项登记）：

| Task | 验收 ID |
|---|---|
| 1 | N1、N2、N3、N4、N5、N6、N7、N8 |
| 2 | G1、G2、G3、G4、G5 |
| 3 | T1、T2、T3、T7 |
| 4 | T4、T5、T6 |
| 5 | B1、B2、B3、B4 |
| 6 | B5、B6、B7、B8 |

## Plan 自检记录

- 第1轮（spec覆盖）：逐项映射 N1–N8、T1–T7、G1–G5、B1–B8；补齐所有ProcessStartOptions构造点、非app-server边界、旧desktop DTO、Unknown日志边沿与IPv6释放责任。
- 第2轮（可执行性/签名）：检查实际rpc_io：通知Closed被忽略、WS响应Closed返回Ok，不能靠关闭WS伪造通知失败；改用local响应真实失败及实例级局部注入。run_worker在线程内创建Supervisor，避免跨线程移动非Send断言；匹配身份后从session取status，不传可能过期的handle。
- 第3轮（测试可信度）：删除“编译失败即业务红”的误承诺；明确基线缺陷红、保护性绿和保持API变异，等待测试用channel/Barrier且有清理；后台门闩不使用EOF立即退出的stdin read。
- 当前没有阻塞实施的纸面矛盾；网络flags、物理网络/电源与真实退出预算仍需真机证据。所有checkbox未勾选，表示本轮没有执行产品计划。
