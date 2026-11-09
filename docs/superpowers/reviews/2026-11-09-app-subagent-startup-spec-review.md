# App subagent 启动修复 spec / plan 多轮自审记录

日期：2026-11-09  
范围：[spec](../specs/2026-11-09-app-subagent-startup-repair-design.md)、[implementation plan](../plans/2026-11-09-app-subagent-startup-repair-impl.md)。
基线：`fix/managed-session-container-attach`，`a0b6f9dc` 加此前未提交修复；当前 worktree 而不是主线接口。

## 方法与证据边界

- 作者由主会话完成多轮源码对照、故障/并发推演和文档自检；本轮没有可用的 spawn_agent 审查工具，没有独立子代理审查，不冒称独立 review 通过。
- 用户要求“反复检查 spec，收敛后可以写 plan”，本轮只修改文档，不实施新增修复、不提交混合旧代码、不合并。
- 文档自审只能得出“无已知设计阻塞项”，不能证明未来实现绝对正确，不能证明 App 功能已恢复。
- 源码锚点采用模块/符号，避免后续实施移动行号导致假精确。

## 第一轮：接口与兼容性

| ID | 核实的问题 | 修订位置 |
|---|---|---|
| S01 | process_bringup_choice 有环境优先级，watch 只读入口声明；只传 policy 丢 expected/home 会使 attach/repair 分歧 | spec §4.1；plan Task1 完整 BringUpChoice 快照、纯解析矩阵与生产调用链 |
| S02 | plugin install 落盘后既有响应是 awaiting-demand，不存在可随意复用的 deferred；UI 设置既有 ok+warning | spec §4.2 给出确定响应，不回滚已安装清单；plan Task2 |
| S03 | wrapper/under-gate helper 不分会二次 acquire；迁移与启动需要明确锁顺序和有界等待 | spec §4.3；plan Task2 明确一次持闸、闸内重查、预算与 RAII |
| S04 | “优先复用握手”不能指导实现；公开 Status 无 daemon generation | spec §5.2 定义拟新增 private lifeline DaemonReady、nonce/identity、fail-closed、版本兼容；plan Task4 |
| S05 | 活旧 daemon 只证明需要阻止 launch，Status-only 不足授权迁移或杀 PID | spec §5.1/5.3；plan Task3/5 分安全迁移和手动处理 |
| S06 | worktree 无 just test-live、test-live.sh、nextest live profile，不能照搬主线命令 | spec §8；plan Task7 明确拟新增独立显式入口，不回迁整套分级 |

依据：subagent `attach::process_bringup_choice`；app-server `rpc/plugin.rs`、`rpc/ui.rs`、`server/watchman.rs`；supervisors `lifecycle/{bringup,guardian,migration_gateway,migration_operation}.rs`；worktree justfile 与 `.config/nextest.toml`。

## 第二轮：故障、并发与可执行验收

| ID | 核实的问题 | 修订位置 |
|---|---|---|
| S07 | production_factory 只 bootstrap；真正 attach 在 thread_index::attach_delegation 调 assembly | spec §4.1；plan Task1 不虚构 factory attach；覆盖 transport/WS/冷恢复调用者 |
| S08 | gateway 取 scope 后 drop gate，freeze 在 coordinator 稍后执行；允许自动启动再次进入 | spec §4.3；plan Task2 gate 随 HeldScope 到 durable freeze 后释放，pre-freeze 屏障测试 |
| S09 | assembly 长期缓存 durable failure；迁移后租约成功也可能继续缺工具 | spec §6.2；plan Task5 认证代际恢复、按项目失效、下一显式 turn 重建，不打断 busy turn |
| S10 | useRuntimeLease catch 吞错误；RuntimeVersionBanner 没 scope/status 时不 render | spec §6.1；plan Task5 无 scope issue、错误归属、迟到过滤、独立 retry revision |
| S11 | Sidecar::read_frame 用 blocking read_line，读后才检查时间，silent peer 可永久等 | spec §7.1；plan Task6 reader channel/poll 真 deadline、write预算、stderr drain及清理 |
| S12 | Sidecar::call 丢非同id帧，可能丢反向 root intent；网关一次read+统一done不是模型驱动验收 | spec §7.1；plan Task6 反向壳授权 dispatcher、完整HTTPbody、父子工具状态与实际echo witness |
| S13 | Ready 与后续帧必须不竞争；跨进程不能传 monotonic Instant；临时timeout会影响共享FD | spec §5.2；plan Task4 单writer/先Ready、guardian先消费、整体deadline循环、timeout恢复 |

依据：app-server `server/{runtime_binding,thread_index,assembly}.rs`；desktop `hooks/{useRuntimeLease,useRuntimeLifecycle}.ts`、`App.tsx`、RuntimeVersionBanner；CLI `tests/managed_production_integration.rs`、`managed_runtime.rs`；supervisors `migration_gateway::SystemWorld::take_locks` 和 `migration_operation::MigrationCoordinator::confirm`。

## 第三轮：修订 spec 的全文契约复查

重新检查入口/连接授权独立性、plugin 落盘与准入响应、闸所有权、业务 probe 三态、Ready 起止条件、可迁/不可迁边界、错误恢复、新 turn 不重放、A1–A11 的判别性及真实 App 证据边界。

结果：第三轮结束时没有新增已知阻塞项，进入 plan。自动检查确认 A1–A11 各出现一次、相对链接存在、无占位符，模块计数为 68/81。随后 plan 的类型核查发现下列租约问题，已反馈修订 spec，因此不把第三轮结论当最终稿免检依据。

## Plan 第一轮：源码签名与状态语义复查，反馈 spec

| ID | 核实的问题 | 修订 |
|---|---|---|
| P01 | WatchmanInstall 实际为 Result<(), String>，不是 io/anyhow | Task2 helper 精确改成 Result<(), String> |
| P02 | native ProjectState.held 可在 blocked 时为 true，前端 isHeldLease 只看 managedReady 会漏释放 | spec §6.1 增加 held/ready 分离；Task5 native可选held回填、旧回包注册对账、held-blocked释放测试与root拒绝 |
| P03 | business protocol 确认为 u32，ArtifactIdentity 是 min/max范围，Ready要指定配套期望 | Task4 用 artifact.business_protocol_max；DaemonInstanceId/BuildId沿用现有别名 |
| P04 | 桌面 DOM 测试需要显式 jsdom，默认环境不能假定 | Task5 加环境头、显式imports、cleanup与timer恢复 |
| P05 | 现有辅助函数路径不精确会让执行者找错类型 | Task1/4 钉 RuntimeBinding、LifelineHello、HeldScope 的实际定义模块；错误路径不再引用 |

依据：app-server `server.rs::WatchmanInstall`；native `runtime_lifecycle.rs::{ProjectAcquireView,ProjectState,record_view}`、held-but-blocked现有测试；frontend `lib/runtimeLease.ts::isHeldLease`；lifecycle `types.rs::ArtifactIdentity`、`authorization.rs::LifelineHello`；现有 `useRuntimeScopeKey.test.tsx`。

## Plan 第二轮与当时收敛（历史结论，本轮已重新复审）

- A1–A11 都对应 plan Task 及可观测判据，见 plan 末尾对照表；A10 明确真实窗口和真实模型、缺 key 不计通过。
- 拟新增接口不写成已有：BringUpChoice纯解析、显式attach、HeldScope闸字段、BusinessProbe、DaemonReady、held反馈、issue/retry、SidecarFrames、App脚本。
- 提交/CI步骤和已有未提交修复的归属分开；不使用全量stage、不运行实施步骤、不自行合并。
- 正式实例隔离、身份匹配、私有帧顺序、失败只回收本次child、无Status fallback、busy turn不重放、unknown租约对账都已在任务中覆盖。
- 自审收敛结论：**当前 spec 与 plan 无已知设计阻塞项，可供用户审阅并进入后续实施；不是代码正确性或 App live 验收结论。**

## 验证记录

本轮运行了 spec/plan 文档检查：验收项唯一性、相对链接、占位符、模块 checkbox/README计数、任务覆盖，以及 `git diff --check`。具体输出由当前会话最终验证命令确认；不将未运行的 Rust/Vitest/CI/live 命令标为通过。

历史代码回归仅在 spec §2.1 引用，不能充当未来 Task 的 green。新增代码还需执行 plan 的 red→green→变异、最终 CI 与 A10 验收。

用户随后要求继续反复审查plan。本轮新发现与修订见 [plan 源码与执行边界复审](2026-11-09-app-subagent-startup-plan-review.md)；上文结论是旧稿历史记录，不能代替新稿检查。
