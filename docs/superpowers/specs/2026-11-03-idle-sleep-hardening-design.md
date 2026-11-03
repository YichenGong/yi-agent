# 任务运行防休眠加固设计

日期：2026-11-03
基线提交：`f634664b`
状态：设计完成，待实现；用户已要求 spec 自检通过后直接编写 plan，本轮不实现。

## 1. 问题与证据

现有架构为 app-server 唯一持有 `PreventUserIdleSystemSleep`，`yi-agent-power`
负责策略、时钟、探针和断言。正常链路测试通过，但有下列缺口。

| ID | 缺口 | 已核实的证据 | 后果 |
|---|---|---|---|
| P1 | 网络在线误判 | `probe.rs:202–204` 只收集 IPv4；`:164–169` 探测网关 TCP 80，超时视为离线 | IPv6-only 或网关丢弃 TCP 80 时，实际可工作的任务超过宽限后失去保活 |
| P2 | 投递失败残留忙碌 | `server/turn/mod.rs:405–434` 先设 Running，send 失败只清 active_turn_id；`power_busy.rs:65–75` 将非 Idle 算忙；组长 cold 投递在 `server/driver/launcher.rs` prepare 后 send 失败未回滚 | 无任务的会话仍保活；cold 组长保留 turn 占位 |
| P3 | 退出/关闭延迟被探测拖住 | `probe.rs:222–225` 无超时 `route.output()`；`power_guard.rs:132–142` join 等整个 tick；退出测试 `:203` 关闭 enabled，绕过探测 | 退出和释放断言不受 50ms 轮询约束，route 卡住时没有上限 |
| P4 | 托管后台工作未参与 busy | `transport/mod.rs:668–678` 只并会话、daemon、远端交互，未读取 ProcessManager | turn 已结束但后台构建仍在运行时，电池下可能到宽限即释放 |

P1/P2/P4 为代码审查确认的场景，未声称已在真实网络或真实故障中复现。
P3 已在临时目录使用**原始 PowerGuard**与延迟 3 秒返回的 route 替身复现：
`drop during route probe: 3.008996166s`；临时替身不是生产 route 的实测耗时。

2026-11-03 基线验证：power 58 passed（含真实 IOKit acquire/release）、
app-server `power_` 19 passed / 1 ignored、runtime 开关和窗口 11 passed。
`pmset -g assertions` 确认运行实例持有正确类型的断言。上述绿灯不覆盖本设计的新场景。

## 2. 目标与不变量

- P1：IPv4、IPv6、双栈的可用本地链路都能提供保活资格；不再把某个 TCP 端口健康作为网络资格。
- P2：所有 prepare 后失败路径按 turn 身份撤销自己的占位；已宣告的 turn 有且只有一次失败收口。
- P3：电源线程不再执行网络连接或子进程；停机/关开关不依赖不可取消的网络等待。
- P4：显式请求保活的托管进程在活跃状态下参与 busy，终态不参与；常驻服务默认不保活。
- 保留策略：`eligible_network && (on_ac || busy)`，条件消失才进入既有宽限。
- 保留 macOS 单一持有者、睡眠感知时间、窗口边界 `<`、设置键及取值范围。
- 保留等待审批/等待子任务算忙，queued/paused/终态不算 daemon 忙。
- 不使用后台进程的 `on_exit`、`ready_pattern` 或运行时长猜测保活意图。
- 不扫描磁盘寻找未知 daemon；不为 app-server 退出后的孤儿进程保活。
- 不修改 pmset；不阻止合盖、手动睡眠、低电量强制睡眠、系统重启；屏幕照常熄灭。
- CLI/TUI、Linux/Windows 真实断言及睡醒自动续跑仍不是本次目标。

## 3. 方案选择

1. **推荐：被动链路资格 + 共用失败收口 + 显式进程意图。** 删除 route/TCP
   主动探测，一次 getifaddrs 快照支持双栈；改动局限于现有薄平台层，没有新增依赖。
   代价是能确认本地链路，不保证上游互联网可达。
2. 多端口/多个公网地址主动探测：仍然把某个服务或防火墙策略当作在线证据，
   需要超时、取消和缓存机制，不能根除 P1，不采用。
3. macOS NWPathMonitor：可提供系统路径状态，但需要异步回调、平台桥接和生命周期
   管理；对当前这轮修复不是必要条件，暂不引入。

本设计明确替代旧 spec 的网关 TCP 80、route 子进程、30 秒主动探测缓存要求。
**网络词义收窄为“本机存在可用本地链路”，不是“互联网或 provider 保证可达”。**
局域网仍有地址与活跃链路、但上游断开时允许保活；这是保任务优先的取舍，不是隐式兜底。

## 4. P1：被动网络资格

### 4.1 数据与接口

在 `yi-agent-power/src/probe.rs` 中引入按接口聚合的 `InterfaceSnapshot`：
`name: String`、`flags: u32`、`addresses: Vec<IpAddr>`。IPv6 scope id 不参与资格判定，
因为本次不建立 SocketAddr。保留一个纯函数处理地址和 flags，平台层只负责复制快照。

增加 `NetworkState::{Online, Offline, Unknown}`。`NetworkState::eligible()`：
Online/Unknown 为 true，Offline 为 false。`NetProbe` 返回这个三态，Supervisor 显式
转换成原有 Snapshot.online 布尔，Policy 不必知道采样错误。替换现有全部 NetProbe 实现和假探针。

Unknown 只用于 getifaddrs 失败或成功但返回空指针（无可信快照）；不能把“成功遍历、
但没有合格接口”包装成 Unknown。未知时允许保活，按原有开关/电源/busy 条件约束，
不是无条件 acquire。持续 Unknown 可以持续提供资格；日志在状态变成 Unknown 时记一次，
恢复后再次 Unknown 才再记，不每 10 秒刷屏。

### 4.2 合格接口与地址

合格接口必须同时满足：

- `IFF_UP` 与 `IFF_RUNNING` 都置位；不是 `IFF_LOOPBACK`；
- 不属于现有已知虚拟/旁路前缀集合（utun、awdl、llw、bridge 等保持原规则）；
- 至少一个合格单播地址：
  - IPv4：不是 unspecified、loopback、multicast、link-local 或有限广播地址；
  - IPv6：不是 unspecified、loopback、multicast 或 unicast link-local；ULA 和全局单播允许。

IPv4 私网地址允许，因为我们要保本地链路，不探测公网。
仅 IPv6 link-local 不提供资格，避免把未获得可路由地址的接口当在线。
地址族按 sockaddr.sa_family 分支，非空、族正确后再转 sockaddr_in/sockaddr_in6；
使用 sockaddr 的网络字节序构造 IpAddr。对链表无论成功、过滤为空或提前返回都正确 freeifaddrs。
接口名沿用排除表而不是新增物理网卡白名单。

快照采集成功且有任一合格接口 → Online；成功但无合格接口 → Offline。
不调用 route、ping、DNS、provider、relay 或 TCP；旧 parse_gateway、connect 分类函数及其
已失去产品意义的测试删除，不能让未使用的老逻辑继续暗示它仍是在线判据。

### 4.3 明确能力边界

IFF_RUNNING 是操作系统提供的链路运行证据，不是端到端互联网证明。真机要验证
macOS Wi-Fi 断开时的 flags/地址行为。只剩被排除的隧道或桥接接口仍按 Offline，
延续旧边界；本轮不解决桥接为唯一真实出口的网络拓扑。不能将此版本宣称为所有拓扑覆盖。

## 5. P2：共享且按身份回滚

### 5.1 共享 helper

把 `ServeLauncher::rollback_turn` / `fail_announced_turn` 的正确身份判据提取到
`server/turn/` 中可复用的 helper（具体文件 `failure.rs`），RPC 与 launcher 使用同一口径。

回滚条件：`session.active_turn_id.as_deref() == Some(failed_turn_id)`。
只有拥有该占位时才清 active_turn_id 并恢复 Idle；已经有另一个 turn 时，两者均不改变。
不要通过向主循环异步发送 Finished 来代替当前函数的即时回滚。

“已宣告”指 turn/started 已成功广播；发生在其前的 prepare 错误仍返回原有 RPC error，
不编造开始/完成事件。已宣告后、prompt 成功投递前的通知或 send 失败统一走失败收口。
失败收口使用一次 completed_at，并尽力 mark_turn_completed，广播 TurnCompleted(Failed,
error, completed_at)，然后身份回滚。广播/落盘失败不能阻止内存回滚，原错误仍向调用方传播。
不得 append 虚假的助手内容或删除用户上传文件；未用的 pending activation 保留给后续首次成功投递。

### 5.2 各入口

- **RPC start_turn_core**：维持正常成功时既有帧顺序：started → opening item → Running →
  成功响应 → send。因为成功响应已经发出，send 失败不得发送第二个同 id RPC 响应；
  以失败完成通知终结该已宣告 turn，并恢复 Idle。函数正常返回主循环，不伪装成 transport
  fatal error 把整个 app-server 退出。通知或响应写失败沿用原错误传播，但先尽力回滚。
- **插件 launch_inner**：复用共享 helper，保留原有失败返回及已宣告失败通知行为。
- **组长 cold 投递**：prepare 已占位但尚未宣告；send 失败仅身份回滚并返回现有投递错误，
  不发一个没有 started 的完成通知、不 mark_turn_completed。等待方仍由外层错误路径收口。
- **组长 warm 投递**：没有在本路径 prepare/占用 turn；保持已有队列与 send 失败返回。
  不能借此次改动把它改成 RPC turn_in_progress 检查，也不能回滚它正在执行的旧 turn。
- **成功 send 后**：本轮已由 driver 接手，不能再由启动方发 failed 或回滚。

helper 的“completed_at 落盘”也补齐 launcher 原先只广播未写完成水位的同形缺口，
但不更改正常 turn 的内容持久化、未读判据或权限策略。

## 6. P3：电源线程生命周期

删除网络主动等待后，生产 tick 只含短本地系统调用（时钟、getifaddrs、电源来源和 IOKit），
不增加另一个探测线程、route timeout 子进程或后台脱离句柄。

把 PowerGuard 分散原子参数换成一个共享 `Mutex<GuardInputs>` 与 Condvar：
`busy: bool`、`enabled: bool`、`grace_secs: u64`、`stop: bool`。移除未消费的 remote_secs；
remote 窗口仍由 app-server 主循环用于 remote_touch_busy，不能丢失它的设置接线。

- 主循环一次 `update(busy, enabled, grace_secs)` 提交一致快照，值有变化时通知线程。
- 线程拿快照立即放锁，执行本地 tick；**不持 GuardInputs 锁做 probe 或 FFI**。
- 用 Condvar 等待至下一次 10s 周期，或输入变化/stop；检查谓词应对伪唤醒，
  不因高频相同更新 busy-loop。睡眠恢复仍由既有 Clock 计时。
- Drop 在同一把锁下置 stop、notify、join；线程 tick 前后都检查 stop；观察到 stop 后
  不再执行新的 tick/acquire，已有断言由 Supervisor Drop 释放，join 返回时释放已完成。
- 若 stop 在一次正在执行的本地 tick 中到达，不宣称可强行中断 OS 调用；等待该次调用
  返回后退出。不设计杀线程、不泄漏 join handle，也不脱离带断言的线程。

延迟契约：从 **PowerGuard 收到 update/stop** 开始，不再额外等待完整 10s 周期或任何网络
超时。可控假依赖测试在正常调度下以 **1 秒**为退出/关闭测试上限，真实平台测试上限
**2 秒**；这些是测试预算，不是 OS 调度/系统调用永不挂起的硬实时承诺。
设置页保存到主循环读到新值仍最多等待一个 10s 采样周期（主循环可运行的前提下），
不能将 worker 的及时唤醒写成设置保存 50ms 生效。

## 7. P4：托管进程显式保活意图

### 7.1 输入与读回

新增可选工具参数 `process_start.prevent_idle_sleep: boolean`，默认 false。
字段是进程生命周期内不可变的意图，不是另一套电源策略。只建议有限后台构建、测试、
批处理设置 true；HTTP dev server、watcher 等常驻服务默认 false；参数说明写入工具 schema。
不新增“按时长自动识别有限任务”或修改现有 on_exit 默认 kill。

字段贯穿 ProcessStartArgs → ProcessStartOptions → ManagedProcess，且在
ProcessStartResult、ManagedProcessSnapshot（process_list / process/list）、ProcessReadResult
中读回同名 boolean，方便确认实际请求意图；所有构造点、测试 fixture 同步补默认 false。
桌面 `ProcessInfo` / `ProcessReadResult` 手写协议类型新增可选 boolean，缺失时按 false，
兼容旧 sidecar。当前 process DTO 不在 ts-rs protocol.rs 中，不能误称只跑 bindings-check 就覆盖它。
本轮不增加 UI 开关、保活状态面板或新的 RPC。

### 7.2 活跃与聚合

`ProcessManager::has_sleep_preventing_processes()` 是轻量只读谓词：在已有进程锁内检查，
不构造全量 list 快照、不复制日志、不执行 IPC 或子进程。

仅 `prevent_idle_sleep == true` 且 `Starting | Running | Ready` 算忙。
`Exited | Killed | FailedToStart` 一律不算；ready_timeout 警告但进程仍在跑时仍算忙。
多个进程/会话取 OR；终态进程仍保留 list 记录也不保活。

`local_busy` 保持现有 turn/status 判定，再 OR 上每个会话 ProcessManager 的轻量谓词。
全局 busy 继续与 daemon 和 remote touch 合并，不在另一个线程读取 threads。
对后台进程开始/结束仍按主循环 10s 周期采样，不承诺即时更新；已有宽限远大于该误差。
当它是最后一个 busy 来源时，下一次成功采样确认终态后不再刷新 last_ok，按原有窗口释放。
插电且在线时依然可以一直持有，这是策略，不是进程状态未清理。

`on_exit=keep` 不等于 keep_awake；explicit true 的 keep 进程只在其所属 app-server 会话
仍在表中且进程仍活跃时参与。删除/退出会话后不让它成为独立持有者。
CLI/TUI 和 daemon 内 worker 若收到这个字段，仅保存/读回意图，不新增其真实断言持有者；
app-server 以外的进程活跃状态不在本次聚合范围内。这一覆盖边界必须在工具说明写明。

## 8. 验收矩阵（实现后必须新增，不代表已通过）

每个测试名使用 `sleep_hardening_` 前缀，便于避免过滤命令跑零条的假绿。

| ID | 场景 | 必须断言 |
|---|---|---|
| N1 | IPv6-only/global 或 ULA，UP+RUNNING | Online，在线电池有活 Hold |
| N2 | 双栈/IPv4 私网 | Online |
| N3 | unspecified/loopback/multicast/link-local/broadcast | 不独立提供资格 |
| N4 | 无 RUNNING、DOWN、仅旁路/隧道 | 成功快照返回 Offline |
| N5 | 快照失败/空指针 vs 成功但空候选 | 前者 Unknown eligible，后者 Offline |
| N6 | 网关丢弃 80、provider 不通 | 不调用它们，合格链路仍 Online |
| N7 | getifaddrs IPv6/重复接口 | 正确读地址字节、聚合且内存释放 |
| N8 | Unknown 连续/恢复再未知 | 日志按状态边沿，不每拍刷屏 |
| T1 | RPC receiver 已关闭 | 已宣告 failed 一次，无第二个 RPC 响应，Idle/None/local_busy=false |
| T2 | opening/status/response 广播失败 | 原错误仍传播；自己的占位被回滚 |
| T3 | 另一个 turn 已占位 | 回滚旧 turn 不改新 turn_id/Running |
| T4 | 插件 launch send 失败 | 仍返回原失败，failed 一次，完成水位同源 |
| T5 | cold 组长 send 失败 | 回滚自己的占位，投递失败；无伪造完成通知 |
| T6 | warm 组长排队/发送失败 | 不改既有正在运行 turn，不改变队列语义 |
| T7 | notify/store 失败 | 仍完成内存回滚，原始错误不被覆盖 |
| G1 | enabled=true、已持断言的 worker Drop | 不等 10s；join 后释放恰一次 |
| G2 | worker 等待时 enabled=false | 被通知唤醒，跳过宽限释放 |
| G3 | 首拍中 stop、tick 后 stop | 不启动后续 tick，最终释放，无脱离线程 |
| G4 | 参数快照更新/相同更新/伪唤醒 | 参数不混版，无丢唤醒或忙循环 |
| G5 | 缩窗/扩窗/再启用 | 既有 policy 语义不变 |
| B1 | 工具参数省略/false/true | 默认 false、严格 boolean、各响应读回一致 |
| B2 | true 的 Starting/Running/Ready | 算忙，Ready 不能被当终态 |
| B3 | true 的 Exited/Killed/FailedToStart | 不算忙，即使 list 保留记录 |
| B4 | false 的常驻进程 | 不算忙；on_exit/ready 不改变意图 |
| B5 | 会话 Idle/None，true 后台进程仍活跃 | local_busy=true；电池在线 Hold |
| B6 | 最后一个 true 进程结束 | local_busy=false；宽限到期释放；其他 busy 源仍可保活 |
| B7 | 旧 sidecar 不返回字段 | 桌面解析兼容，缺值按 false |
| B8 | 非 app-server worker / 删除会话 / app-server 退出 | 不新增独立断言，不为孤儿保活 |

线程测试使用注入 clock/probe/assertion 的真实线程循环，Barrier/channel 确认“已 acquire”
或“已进入 tick/等待”之后触发 stop；不能以盲 sleep 或 enabled=false 代替 G1。
不得用其他正在运行实例的相同 label 证明自己的断言，真机测试 label 必须唯一。

手动 macOS 验收：隔离 YI_AGENT_HOME，不退出正式实例、不修改 pmset；观察本测试 PID
的断言。真实 app-server 沿用固定生产 label，必须按自己的独立 PID 识别，不能借正式实例的同名断言证明通过；唯一 label 由真机 worker 测试的 assertion 工厂注入，不新增生产 label 设置。验证 IPv6-only、网关 80 丢包但链路可用、电池下后台有限工作/结束/宽限、Wi-Fi
断开后释放、禁用/退出时释放。需要真人改变网络、电源或等宽限的步骤记录为人工验收，
本轮文档不声称已完成。不因 main 的历史人工结果而勾选新版本通过。

## 9. 交付与非回归门禁

对应 plan：`../plans/2026-11-03-idle-sleep-hardening-impl.md`。
实施任务至少覆盖 probe、Supervisor、共享 turn 失败 helper、PowerGuard、ProcessManager、
app-server 接线、桌面手写 DTO、文档进度与回归测试；不升级依赖或工具链。

实施提交前 `cargo fmt --all` 与 `just ci`，合并结果再跑 `just ci`。
因为改动手写桌面 DTO，另跑 `cd desktop && npx tsc --noEmit && npx vitest run`。
新增缺陷回归测试必须先以兼容旧 API 的复现或保持 API 的变异证明业务断言红，再证明修复后绿；保护性非回归和新增契约测试可基线绿或先接口编译红，必须如实分类。逐项记录变异验证，不能只报总通过数。

进度统一在 `docs/project-management/yi-agent-power.md` 登记 P1–P4 四项未完成需求，
README 计数相应为 8/12；tools 和 app-server 模块增加交叉引用，不重复计算同一需求。
`docs/bug-list.md` 登记 P1/P2/P3 为未修复，P4 作为覆盖增强留在模块文件。
实施完成才将各需求及对应 bug 勾选，不能把“spec/plan 写完”当产品问题已修复。

## 10. Spec 自检记录

- 第 1 轮（需求、范围、术语）：逐项对照 P1–P4 与 N/T/G/B 矩阵；将“网络在线”明确为本地链路资格，删除主动探测缓存需求，明确 Unknown 的持续语义、虚拟唯一出口盲区和后台默认 false。没有将被动采样包装成互联网可达证明。
- 第 2 轮（真实调用图、错误与生命周期）：复查 RPC、插件、组长 warm/cold；明确成功响应已发时不能补第二个响应，warm 路径不能回滚旧 turn，完成水位与通知同源，通知/落盘失败不能阻止内存回滚；退出预算不冒充 50ms 硬实时保证。
- 结构校验：无占位内容或进行中状态；基线引用的现有源文件存在。对应 plan 在下一阶段创建，不能因编写顺序将尚未创建的 plan 误报为缺失实现。
- 复跑隔离文档 worktree 的 power 基线：58 passed / 0 failed。该结果只证明基线，不代表新增矩阵已实现。
- 第 3 轮（spec/plan 交叉复核）：28 个矩阵 ID 均映射至 plan；补齐 Policy 首拍不 acquire 的测试前提，区分接口编译红/缺陷回归红/保护性绿，并明确固定生产 label 按 PID 验收、唯一 label 仅在 worker 工厂注入。章节顺序、相对链接与 8/12 进度计数经脚本校验通过。
- 当前未发现阻塞编写 plan 的需求矛盾；真机网络 flags 行为仍作为实现阶段的人工验收，不声称纸面审查能代替系统实测。

