# App subagent 启动策略与端到端恢复修复设计

日期：2026-11-09  
状态：作者多轮自审后无已知阻塞项；用户已允许审查后写 plan；未实施新增修复  
工作分支：`fix/managed-session-container-attach`  
基线：`a0b6f9dc` 加该 worktree 已有未提交修复；不是仅以该 commit 为完整代码基线。

## 1. 目标与边界

用户要求让桌面 App 能启动 subagent，并要求 ready for 合并时不要自行合并。
本设计承接本轮代码审查，修复剩余入口绕过、策略分歧和验收缺口。

成功的用户行为是：在隔离的真实 App 中打开项目会话，让模型调用
`spawn_agent` 创建一个 `read_only`、`fork=false` 的子代理，子代理实际执行
`echo hello`，父会话通过 `wait_agent` 收到成功终态及 `hello`。
工具出现在模型请求中、spawn 返回 task id、子任务完成三个事实缺一不可。

保持既有安全边界：

- managed 会话必须由壳租约、guardian 授权、当前 daemon 与当前 turn 栅栏准入。
- 不为恢复工具而降级 embedded、伪造 shell 授权或自动重放结果未知的 spawn。
- 不自动结束旧 daemon、插件、其他前端的工作；迁移必须由用户确认。
- 不操作正式 App、正式实例的模型配置或运行时；所有进程实验使用隔离实例。
- 本 spec 及后续实现均不得自行合并到 main。

不在本轮重做：会话容器角色、调度器、Kanban 状态机、递归深度、版本切换协议、
子代理轨迹 UI 或通用远程鉴权体系。CLI 七项修复作为已有前置，不重新实现。

## 2. 证据与已完成前置

### 2.1 已有修复，不作为本 spec 新成果

worktree 已修复 managed session container 使用 brokered `rootcap1:` capability、
headless run 复用已授权 root、session identity 文件名编码、root intent 一致性、
worker factory 的 socket 参数、lifeline hello 后清除 read timeout、首次授权预填 store。
已有真实模型 CLI 记录证明一次 `spawn_agent → wait_agent` 成功；这不证明 App 成功。

App watch 竞争源也已有局部修复：`watch::once_unattended_as` 接收 embedded 判据，
`plugin_bridge::spawn_resident_watch_loop` 从入口声明获取判据。
隔离侧车对照曾证明旧版本启动 embedded daemon，而修复版本未启动它。

本轮审查复跑：watch 回归 8/8、managed container 挂接 1/1、plugin_theme 19/19，
格式与 diff 检查通过。这是历史证据，不是未来实现的最终验收。

### 2.2 入口绕过：连接身份不等于宿主运行模式

代码锚点（行号是本轮 worktree 快照，实施后以符号为准）：

- `yi-agent-app-server/src/server/transport/mod.rs:865`：
  `PluginChannelState.managed_session` 来自当前 client 的 `shell_authorizations`。
- `yi-agent-app-server/src/server/transport/mod.rs::initialize`：
  只有 local stdio 连接可接受 shell 身份，远程连接不能获得这份身份。
- `yi-agent-app-server/src/rpc/plugin.rs:344–358`：
  仅 `managed_session || retired` 阻止 `ensure_daemon`。
- `yi-agent-app-server/src/server/transport/scoped.rs`：
  生产 `ensure_daemon` 注入 `daemon::ensure_running`，可启动 detached `daemon serve`。

所以 managed stdio+relay 宿主内，无 shell 授权的远程连接，在无退休标记实例上安装
旧协议插件，仍可进入 embedded 启动分支。若 managed daemon 尚未起来，embedded
可能先占业务 runtime；若 managed daemon 已回答，ensure 可能复用它而不新起进程，
因此不能把每次远程安装都说成必然启动第二个 daemon。
现有 plain-install 测试证明这个分支会调用 ensure，但尚无该混合宿主场景的进程复现。

权限纠正：`PluginChannelState.client_scope` 的 Admin 注释针对 `plugins/v1/*`。
本 spec 不把它当成旧 `plugin/install` 已有 Admin 门禁的证据；测试使用合法远程客户端，
修复不改变权限范围，也不接受远程 JSON 中伪造的宿主策略或 shell 身份。

### 2.3 策略分歧

- `yi-agent-subagent/src/attach.rs:1279–1285::process_bringup_choice`：
  环境覆盖 → 入口声明 → build 默认（embedded）。
- 新 `is_managed_frontend()` 只读入口声明。
- `plugin_bridge.rs:339` 使用后者，未采用有效策略。

WS 入口显式设置 `YI_AGENT_BRINGUP=managed` 时，attach 和 watch 会给出不同答案。
stdio 入口覆盖 embedded 时，也会出现反向分歧。

### 2.4 存量 daemon 与错误就绪判断

`yi-agent-supervisors/src/daemon.rs:77–97::spawn_detached` 调用 setsid，
旧 embedded daemon 故意可在 App 退出后继续存在，禁止新启动不能消除已有占用。

`lifecycle/bringup.rs::ensure_managed_under_guard` 主要探测 guardian 控制通道；
`lifecycle/guardian.rs:1589–1605` 在启动 daemon 后用任意业务 `Status` 的成功
作为就绪条件，未在此处证明应答者就是本次启动的 daemon。
这可把仍存活的旧 daemon 误当新代际 ready，随后授权桥失败。
这是审查发现的既有闭环缺口，不宣称为 watch 修复新引入的回归。

已有 [生产接入设计](2026-11-05-managed-runtime-production-integration-design.md)
§4.2 已要求先探测业务 daemon 与 guardian、遇到 unmanaged 返回选择态。
本轮落实这条契约，不另建自动迁移方案。

### 2.5 验收假阳性

`managed_production_integration.rs::a_managed_sidecar_starts_no_daemon_alongside_the_guardian`
固定睡六秒后检查 socket 缺失和 sidecar 存活，没有观测 watch 首轮执行。
embedded detached 进程不由 guardian journal 记录，journal 为空不是独立排除证据。
源码 contains 测试也不能证明正确参数实际传到生产分支。

`_shell/prepare` 成功只证明部分管理通道可用，不能替代业务 readiness、rootReady 或
实际委派。此前曾写过这样的假阳性测试，变异回退仍通过，已删除，不再恢复。

## 3. 方案取舍

| 方案 | 收益 | 代价与结论 |
|---|---|---|
| 仅保留 watch 声明闸门 | 最小修改，已消除一个抢占源 | 远程 install 与环境覆盖仍可绕过；不采用为最终交付 |
| 统一有效策略与启动准入，补存量探测及正面验收 | 复用生命周期协议，直接闭合本轮缺口 | 需要贯穿装配、插件入口与 readiness；推荐 |
| 全面重构 guardian、迁移、插件协议 | 可统一更多历史接口 | 范围过大、重复既有设计；不采用 |

## 4. 设计：统一宿主策略，但保留连接授权

### 4.1 两个维度分别保存

宿主有效运行策略回答“此进程是否允许 embedded 自动启动”；连接 shell 授权回答
“此连接是否可由本地壳代表”。两者不得替代。

生产入口安装 context 后，以 `process_bringup_choice()` 解析一次完整
`BringUpChoice`（policy、expected artifact、instance_home），并传给
`RuntimeAttachments`，经 transport/thread 装配调用链传到
`thread_index::attach_delegation`。`production_factory` 只 bootstrap agent，不虚构其
内部 attach 调用；所有冷恢复/会话重建调用者也使用相同快照。
`assembly::attach_cwd_runtime`、watch、plugin handler、UI watchman 设置及后续 binding
repair 均复用这份快照。不得只给 watch/install 增加字段却让 attach 继续重读全局策略。
外部旧 API 可保留默认 wrapper，内部新增显式注入入口；测试可在同一进程构造两种
policy，不改全局环境或 OnceLock。环境覆盖是启动配置，不支持运行中热切换。

远程与本地连接共享宿主策略，远程 JSON 无权指定它。
`managed_session` 保留连接身份职责，不因宿主 managed 就赋予远程 shell 授权。
retired/freeze/RetirementGate 仍独立有效，policy=embedded 不得绕过它们。
`is_managed_frontend()` 若保留，只表达入口声明，不承担有效策略判定。

### 4.2 Embedded 启动准入契约

| 有效策略 / 实例状态 / 连接 | watch 自动 ensure | 旧 plugin/install ensure、查询等待、安装 watchman |
|---|---|---|
| managed，无标记，本地 shell | 禁止 | 禁止 |
| managed，无标记，远程/普通连接 | 禁止 | 禁止 |
| embedded，无标记，普通连接 | 保留既有行为 | 保留既有行为 |
| embedded，无标记，shell 授权连接 | 保留既有 watch 行为 | 保留既有 managed_session 禁止分支 |
| 任意 policy，已退休/冻结/迁移闸竞争 | 禁止 | 不得启动或恢复旧自愈 |

旧 plugin/install 已完成清单与开关落盘后，若自动启动不允许，返回既有
`startup: "awaiting-demand"`、`queryable: false`；不调用 ensure，不等插件查询 socket，
不安装旧 watchman，不承诺后台自动重试。保留其原有 managed_session 禁止条件。
本修复的迁移闸约束针对自动启动副作用，不把已落盘安装回滚成未安装，不新造 deferred
响应字段。处于冻结或取闸超时的安装也以 awaiting-demand 如实说明未启动。

`ui/settings/write` 的 watchman 偏好仍按用户意图落盘；不允许启用时回现有
`ok: true` 加 warning，不安装登录项。禁用只卸载登录项，不借机杀 daemon。
wrapper/helper 内的 HOME 与 instance_home 必须来自调用方注入，不能重新读取真实 HOME。

### 4.3 闸所有权和锁顺序

watch tick、plugin 自动启动收尾、UI watchman 启用各自至多获取一次
`RetirementGate`，在阻塞池内获取，取闸后重查 retired/frozen；只在允许时执行副作用。
plugin 的闸覆盖 ensure、query-readiness 等待与 watchman install，不能只护其中一步。
helper 的 under-gate 入口接受持有闸的引用，不再次 acquire；独立 wrapper 才负责取闸。
managed policy 直接拒绝自动启动，不等待迁移闸。

锁顺序是实例闸 → scope 协调锁（若需要）；不得持 scope 锁反取实例闸。
既有 `migration_gateway::SystemWorld::take_locks` 取 scope 后立即 drop gate，
而 freeze 稍后才落盘，存在重新准入空窗。本轮最小修正为：gateway 所取实例闸
随确认上下文持有到 freeze 成功持久化，再释放；确认失败/提前返回由 RAII 释放。
不得在已持 scope 后重新取闸，亦不得把壳 root/lease 回调纳入这段锁区间。
后续 freeze 标记负责阻止新启动。测试必须证明一次在途启动持闸时迁移排空等待，
迁移写入 freeze 后新 tick/安装不能启动。不能用 `is_held()` 的一次读代替持闸。
启动及等待共用一次有界预算，不叠加多个完整预算；等待网络/进程时不持主 RPC
循环或全局 registry mutex。预算必须传进ensure、query、watchman/launchctl实际入口，不能
仅timeout一个仍在后台执行的blocking future；闸随实际worker结束释放，超时不得提前
释放并允许后续复活。connect/read/write及子进程等待有绝对deadline；本地文件IO不承诺
内核硬截止，返回后复核预算，过期不发起下一副作用，副作用结果未知则保留对账责任。
失败cleanup可另有显式有界预算，但不能延长成功等待或偷偷重试启动。

检查 watch、plugin/install、`server/watchman.rs` 与 UI 设置所有恢复旧自愈的调用点。
手工 `daemon serve`、独立 embedded CLI 与已存在 launchd job 保持原有边界；本次不
自动卸载已存在 job。managed 插件仍由真实 demand 驱动，不因安装创建租约或工作。

## 5. 设计：存量探测与就绪证明

### 5.1 启动前探测

同一 canonical scope 的协调锁内，以 fixture/调用方注入的 instance_home 解析控制
和业务 socket。复用 `ControlProbe`，为业务端增加可注入的只读探测结果
Absent / Answered / Unresponsive（拟新增内部类型，不是现有 wire 类型）。

1. 业务端 Answered、guardian 缺失：返回现有 `ManualActionRequired`，明确旧业务
   daemon 需本地用户选择；不创建 guardian、selection 或第二个 daemon。
2. guardian 可回答：按现有身份兼容检查及当前 daemon 健康检查解释，不把控制
   Connected 当 rootReady。桥断开/业务缺失时降为 blocked，不创建第二个 guardian。
3. 两端都 Absent、scope claim/schema/产物检查通过：正常 managed 启动。
4. 任一端 Unresponsive：在同一 deadline 内重探；预算耗尽返回 blocked/manual，
   不把超时当不存在。socket 文件本身不证明活进程。

`HeldScope` 迁移启动也调用同一检查：旧 daemon 停止证明未通过，就不能进入新启动。
允许继续的是可证明无人服务的残留节点，而不是任意 unlink 活 socket。
只读 probe 不创建 runtime、不发 Stop、不签发 root、不修改历史。
预检挡不住非合作旧进程的竞态，因此不能取代下一节的私有确认。

### 5.2 私有 daemon ready 协议

本轮明确使用已有 bootstrap/lifeline FD，不增添业务 IPC 身份下载接口，
不使用 `BridgeFrame::Prepare` 创建探测 root。

拟新增内部协议：

- guardian 在 `LifelineHello` 增加 `startup_nonce`（新构建 managed role 必须存在，
  使用独立随机 nonce），发 hello 成功后等待一次 `DaemonReady`。
- `DaemonReady` 携带 nonce、scope_key、daemon_instance_id、build_id、business_protocol。
  不携带 generation_secret、capability 或 resume secret；可安全脱敏诊断。
- daemon 验证 hello 与 invocation scope 后，成功取得 runtime 独占锁、绑定业务
  listener、构建 ledger 与 foreground bridge 服务及 managed supervision 后，才写 Ready。
  写Ready并恢复共享FD的临时timeout后，安装反方向授权reader；安装失败立即关闭lifeline
  并退出，由guardian撤销ready。构建失败/取锁失败/缺 FD 时不写 Ready。
- guardian 只读自己的私有 FD，核对所有字段与本次 pinning，利用 registry 中
  process birth identity 核对本次 daemon 仍活着，随后才安装 `self.daemon`、ledger、
  bridge 和 lifecycle readiness。任意公开 Status 成功不能替代此确认。

认证来源是仅发给本次托管子进程的继承 FD，而不是公开 nonce；不把这份证明描述成
隔离同 UID 恶意进程。业务可用的证据是持独占锁的 daemon 已 bind 后在私有 FD 上确认，
结合出生身份；外部恶意 unlink/rebind 不在本期威胁模型内。

Ready 是 daemon → guardian 方向的首个且本期唯一生命周期帧；guardian启动阶段
唯一reader消费它，运行中用监督循环的非消费EOF观测撤ready，不虚构已有长期reader。
后续若新增生命周期帧需另行定义dispatcher，不允许两个reader竞争。daemon写Ready并恢复
临时timeout后才安装反方向授权reader，避免共享FD timeout影响已运行reader。
registry 的既有 durable-spawn barrier 不等于业务 Ready，保留两者。

Ready沿用lifeline的control codec envelope及CONTROL_VERSION校验，16KiB上限含4byte
big-endian长度header；不是裸JSON。nonce通过lifecycle已有OS密码随机依赖的公共helper生成，
随机失败正常拒绝启动，不从generation_secret派生，也不暗增supervisors依赖。

guardian 写 hello 与读 Ready、daemon 写 Ready 使用各自一次有界 startup 预算
（guardian 是整体权威 deadline，daemon 不在协议里交换跨进程 monotonic Instant）；
完整帧读取有上限，部分帧/EOF/错误字段/超时全 fail closed。读写超时设置不得残留
影响长期 lifeline 的共享 FD；临时超时必须恢复。registry mutex 只用于短查询，
不能持锁等 Ready，否则后续 managed spawn/reclaim 会被阻塞。

此确认只用于新启动的配套 daemon；已有不同 build 的 guardian 继续走现有兼容/
版本选择，不强迫旧进程补发启动帧。新 guardian 启动不支持 nonce 的旧 daemon 应
在 pinning/确认阶段被拒绝，不能超时后回退任意 Status。
失败时只回收本次 registry 可证明归属的失败子进程，不停止先前 daemon。
spawn必须复用原deadline；hello写失败及所有spawn后错误归一回收，先完成bridge等
fallible准备再提交self字段，不允许partial ready；cleanup未证明成功则报告unknown。
运行中 EOF/daemon 退出必须撤销当前代际 ready，沿用现有死亡监控并补非消费EOF观测；
不能只在启动时检查。

### 5.3 用户确认迁移与不支持的旧实例

复用 `runtime_migration_preview`、`runtime_migration_confirm`、
`runtime_migration_operation`、`lifecycle/migration_gateway.rs` 的证据和幂等协议。

存在旧 daemon 只足以阻止新启动，不足以授权迁移。只有原协议能证明 PID 出生身份、
可执行产物、停止/排空及所有复活源时才显示可确认迁移；旧 daemon 只有 Status、
证据不足或有未证明结束的工作时必须显示人工处理原因，不自动按 PID 杀它。
“不支持安全迁移”是明确结果，不允许绕过来满足验收。

本地壳核实 scope 和已验证目标产物，远程连接不能确认本机迁移。
预览绑定 token/证据，确认绑定 operation id；重复确认或回答丢失只查询同一操作，
不第二次 stop。取消预览无副作用；已确认并执行的操作不能被 UI Cancel 自动回滚。

可迁实例确认成功后保留数据库、历史及工作目录，重新取得当前租约和 root 授权，
再验证新委派。unknown/legacy embedded-in-host 分支保持原有手动或暂用选择语义。
如果旧 daemon 不支持安全迁移，用户明确完成手动停止后，由只读探测证明两端
Absent 再正常启动；自动流程本身不得代做手动停止。
不重跑历史 child，不重用旧 capability。只补通既有状态/迁移 UI，不创建第二套界面。

## 6. 设计：失败可见且不伪装恢复

区分 `managementConnected`、`lifecycleReady`、`rootReady`、`delegationReady`。
这些是状态含义，不为每阶段新造持久化状态机。

### 6.1 无 scope 也能显示错误

`useRuntimeLease` 向调用方返回按 project 归属的 acquire/blocked 错误及 retry 动作。
UI 提示不依赖 `RuntimeVersionBanner` 已取得 scope：抛异常、未取得 scope 或 blocked
回答均能显示项目、失败阶段及非敏感原因。沿用现有 banner 布局/命令适配器；必要时
补齐本地 legacy migration 的接线，不能误将 version-switch 横幅当现成 migration UI。
远程客户端无本地 acquire/迁移按钮，不授予本地壳权限。

错误关联 canonical project、attempt id（后端提供时）、本地请求序号与已知 daemon
代际。切项目后的迟到错误不能覆盖新项目；较旧请求不得覆盖较新成功。离开项目后
迟到的有效租约按既有规则释放。错误文本白名单/脱敏，不直接显示含 secret 的原始帧。
root/工具装配失败复用 `Activation.tooling_error` 及既有会话通知渠道呈现，不能仅写日志。

租约持有与业务 ready 分离。native `ProjectState.held` 可在 blocked 时仍为 true；
`settle/settle_with`必须在返回前回填held并记录同一view，不能只改record_view。
注册查询返回scope key，native按只读canonical解析project别名，不mkdir；前端不用路径
直接匹配scope集合。每项目release/acquire同链串行，A→B→A迟到结果不得释放后来租约；
release失败或非空failures保留unknown对账责任。
acquire 回包补充经过 native 核实的 held 信息，前端不能继续只凭 managedReady
判断应否释放。旧回包缺字段或 unknown 时通过已有本地注册查询对账，不从 blocked
猜无租约；held 只用于拥有/释放，不允许 root 或模型工具准入。已持有但未 ready 的
项目需显示原因、可明确 retry，离开时释放；迟到回答同样处理，不能留下幽灵租约。

### 6.2 显式恢复而非自动重放

用户 retry、成功迁移或明确手动处理后，触发 wanted 项目一次新的 acquire，即使
wanted-set 未变化。starting 可按后端 retry 提示进行有界重探；blocked/manual 不无限
循环启动，不因重渲染不断 acquire。操作回答丢失只查询同一 migration operation。

成功确认新代际后，失效该 canonical project 的 durable failure 与过期 runtime binding
缓存（`assembly::attach_cwd_runtime` 当前会长期记住非 retryable 失败），并标记该项目
会话工具装配需要重建；不能清空其他项目缓存。正在执行的 turn 不被换掉或重放，
下一次显式新 turn/会话 resume 前重建工具并重新取得当前 root/turn 授权，沿用会话历史、
模型和 sandbox 参数。恢复入口必须在持agent的driver内、模型请求之前，而非只改RPC
排队状态；首次降级无root及activation_attempted已置位的会话也能显式重新attach。
同步foreground/root、工具registry、process manager、MCP、delegation及children watch，
ThreadSession的process/*查询和process watcher必须共享同一有效manager，不保留旧watch；
保留liveSession Arc；busy时仅延期重核，不声称已完成安全迁移。
失败仍保持无委派并显示原因，旧 capability 不跨代复用。

模型工具集只能在真实委派准入满足后宣称可用；未 ready 不伪造成功。
合法委派仍使用既有审核/sandbox 规则。日志不能输出模型 key、root capability、
bootstrap secret 或 resume secret。

## 6.3 追加缺陷（实施期发现）：project attach 的授权来源缺失

A9 的实现过程暴露了一个与 §2–§6 全部内容正交的缺陷，且它是 App 里
`spawn_agent` 消失的**直接**原因。前几节修的是「策略选错导致根本不起受管 runtime」，
这一条修的是「受管 runtime 起来了，但桌面端第一次连接就被拒」。

### 6.3.1 故障链

桌面启动顺序由协议固定，不可重排：

1. 壳经私有通道 `_shell/prepare` 让 guardian 起 daemon；
2. `prepare` 连接成功后，壳用 `FrontendLease::register` 为项目登记 *business 租约*，
   `client_run_id` 是本次 App 启动的 run id（`ensure_project` →
   `SocketBackend::register`）；
3. 业务 `initialize` 带上同一个 run id（`shellAuthorization`）；
4. 首个 `thread/start` 触发 `attach_delegation`，其中第一步是 **project attach**
   （`AttachApplicationRoot`，key 形如 `project:<pid>:<path>:<uuid>`）；
5. 之后才会为该会话的 root 单独申请 capability。

`crates/yi-agent-store/src/runtime.rs::authorize_attach_key` 先用
`ledger.resolve(key)` 解析 attach key。project attach 的 key 不是 capability，
必然解析失败；兜底走 `AttachAuthority::admits(client)`，而该集合由
`refresh_attach_authority` 从 `ledger.authorized_clients()` 派生，后者原本**只**
从 `by_client`（root 绑定）派生。

第 4 步时没有任何 root 绑定——会话还没拿到 capability——于是集合为空，attach 被拒：

```
runtime root rejects new work: no root authorization matches this capability
```

`attach_delegation` 对失败降级：保留原 agent、记 `tooling_error`、不注册委派工具。
用户看到的就是工具列表里没有 `spawn_agent`。

责任提交：`b8b5d325 feat: carry per-root authority from the desktop shell to the daemon`
引入 root 授权闸，未给 project attach 留出「租约已存在、root 尚未存在」这种合法时序。

### 6.3.2 为什么既有测试没抓住

`managed_attach_authority.rs` 的 fixture 在 attach 之前就 `apply(Bind)` 了一条
root 绑定，等于把时间线跳到了第 5 步。真实桌面永远不会那么做。测试因此是**构造上
的假阳性**：它验证的是「有绑定时 attach 通过」，而真实场景是「没有绑定时 attach 必须
通过」。

### 6.3.3 修法：为租约建立独立的授权来源

关键约束是「不能因为 project attach 需要放行，就把 capability 语义整体放宽」。
所以新增的是**第二个授权来源**，不是放宽第一个：

- `AuthorizationLedger` 新增 `leased_clients: BTreeSet<ClientRunId>`；
- 线协议 `AuthorizationUpdate` 新增 `ClientLeased { client_run_id }`；
- guardian 在 `ControlRequest::Register` 成功后调用
  `announce_client_lease`：先本地记账，再通过 lifeline 推给 daemon（best effort，
  与既有 `push_authorization` 同一策略）；
- `authorized_clients()` 返回 **root 授权 client ∪ 业务租约 client**；
- `RevokeClient` 同时清 `leased_clients` 并落 tombstone，迟到的 `ClientLeased` 被拒；
- daemon 重启时 `start_daemon` 重放 live lease，否则新 generation 的空 ledger 会让
  App 的下一次 attach 再次被拒。

安全边界必须同时成立，缺一即等于把租约变成通配授权：

| 性质 | 为什么不能松 |
|---|---|
| `ClientLeased` 不授予任何 root | `AttachOwner::Project` 的 `authorized_root()` 仍为 `None`，daemon 自铸 root id；install/bind/spawn 仍要求真 capability |
| `LeaseOnly` 观察者不推送 | 只观察不授权工作，推送它等于让旁观者打开 root |
| `RevokeClient` 后迟到帧被拒 | tombstone 记录的是决策，重复帧是陈旧而更新 |
| 未知 client 仍被拒 | 否则「报自己的 id」就成了 attach 陌生人的凭据 |

### 6.3.4 验收

- 新增 `the_apps_first_conversation_attaches_its_delegation_runtime`
  （`crates/yi-agent/tests/managed_production_integration.rs`）：真实进程、真实私有
  通道 bootstrap、真实 `_shell/prepare`、真实 `FrontendLease::register`、真实
  `initialize` + `thread/start`，并由测试扮演壳应答 `_shell/authorizeRoot`。
  断言 trace 无 `no root authorization matches this capability`、无
  `subagent delegation unavailable`，且 `runtime.sqlite` 的
  `application_root_attachments` 非空。
- 判别性已验证：把 `authorized_clients()` 的租约合并临时回退，该测试立即变红，报的
  正是 App 里那条拒绝；恢复后转绿。
- 反向边界各有单测：`a_lease_opens_a_connection_but_authorizes_no_root`、
  `releasing_the_lease_stops_the_project_attach`、
  `a_lease_only_observer_is_not_announced_to_the_daemon`、
  `registering_a_business_lease_tells_the_daemon_before_any_root_exists`。
- 真实 socket 层：`runtime_ipc` 的同名用例改用 `ClientLeased` 复现真实时序。

## 7. 验收矩阵

以下为实施时必须新增/加强的判据；不是已经存在的测试名或已完成结果。

| 编号 | 场景 | 必须观测的事实 |
|---|---|---|
| A1 | stdio managed 宿主、无退休标记、resident 登记 | watch 首轮实际进入并拒绝启动；ensure 调用 0 |
| A2 | 同一宿主的远程连接旧 plugin/install | 安装落盘、awaiting-demand、ensure/watchman 调用 0 |
| A3 | entry 声明 × 环境 managed/embedded/未设置 | 生产解析与显式注入结果一致；watch/install/attach 使用相同有效 policy |
| A4 | embedded 普通连接，无标记 | 原 ensure、自愈语义仍有效，作为正面 witness |
| A5 | retirement/freeze/迁移闸竞争 | 未启动新旧自愈；迁移能排空在途自动启动 |
| A6 | 无 guardian、旧 embedded 活着 | 返回选择/blocked，未启动第二个 daemon；旧 PID 与工作仍活着 |
| A7 | 旧 Status 应答者与新 daemon 启动竞争 | 新 daemon 未认证/已退出时 guardian 不能报 ready；不杀旧应答者 |
| A8 | 可安全迁移与 Status-only 不可迁两分支 | 取消预览无副作用；安全确认幂等且历史保留；不可迁保持 manual，用户手动处理后才重探恢复 |
| A9 | 真侧车本地授权 + 可控模型网关 | 模型请求含 spawn_agent；子代理实际 echo hello；父 wait 得成功终态 |
| A10 | 打包真实 App + 真实模型 | 真实窗口完成同一 echo hello 闭环，不借 CLI run 替代 |
| A11 | runtime/root 失败、迟到回答及代际恢复 | 无 scope 也显示原因；旧请求不覆盖新成功；新 turn 恢复工具且不重放旧 turn、无秘密泄漏 |
| A12 | 真实 App 启动顺序（bootstrap→prepare→登记租约→initialize→thread/start），见 §6.3 | 首个会话的 project attach 被接纳：trace 无 root 授权拒绝、无 delegation unavailable，`application_root_attachments` 非空；临时回退租约授权来源时该判据必须变红 |

### 7.1 判别性要求

- A1 必须以可注入 tick 完成观测、诊断事件或等价确定性屏障确认首轮执行；
  “侧车活着 + 睡六秒”不算。纯 contains 测试只可作辅助。
- A2 必须实际通过生产共享宿主的远程 dispatch，或等价传输夹具覆盖同一装配；
  只构造 managed_session=true 的本地 handler 测试不算。
- A6/A7 要有活旧 daemon 的正面 witness，不能只造残留 socket 文件。A7必须直接
  驱动guardian start_daemon，或在预检Absent后的屏障注入旧responder，记录spawn及Ready
  等待确实进入；被A6预检拦住的测试不能算Ready回归或Ready→Status变异红。
- daemon journal 只证明托管进程记录，不证明不存在 detached 进程；应结合启动
  调用观测、隔离 PID/进程归属及认证应答判断，不能用全机 ps 零命中当唯一定论。
- socket 路径由 fixture 的 instance_home/runtime_dir 显式计算，不能读取测试进程真实 HOME。
- A9 可控网关必须检查模型实际收到的工具，再分别驱动父 spawn/wait 与子 echo；
  不由测试替模型直接调用 spawn 来冒充工具装配成功。壳模拟器必须处理反向
  root intent/授权请求，持真实App/Business FrontendLease和heartbeat，依据本地bootstrap
  epoch、scope、client_run_id校验后向guardian签发真实root授权；不能把prepare或bootstrap
  secret当lease/capability。不可由现有 `Sidecar::call` 丢弃所有非同 id 帧。reader 应以
  poll 或 reader-thread/channel 实现真正 deadline，silent/partial peer 也能退出；
  当前 blocking read_line 后才检查时间的 helper 不满足。网关完整读取 HTTP body
  并区分父/子对话，断言实际子工具事件及父 wait，不用统一 done 响应替代。
- A7 补充 nonce/scope/instance/build/protocol 不匹配、截断/超长帧、EOF、迟到 Ready、
  子进程已退出、旧版本无 Ready、超时恢复及运行中撤销 ready；均不能回退公开 Status。
- A5 在取 scope 后、写 freeze 前插入确定性屏障，证明新启动不能穿过该空窗；
  重复取闸、竞争超时、失败释放和其他 scope 的等待均有界。
- 逐条回退策略传递、远程 install 闸门、启动认证检查：相应测试必须红；恢复后绿。
  已有有效测试保留，不重复执行全部历史变异来制造“新增成果”。

### 7.2 原生 App 与真实模型

产物必须先构建前端，再构建 sidecar 与 tauri bundle；核对 bundle 内 sidecar 的
build id/哈希与本次代码。使用独立绝对路径 YI_AGENT_HOME、bundle identifier、
模型清单、项目/runtime，禁止共用正式 relay session。

测试 prompt 使用唯一标记，明确让子代理 echo hello；父收到的文本不能作为唯一证据，
还要核对 daemon child 的实际工具事件、退出结果与 wait 终态。
保留脱敏的 App/sidecar/guardian 日志及 task/turn id 供关联。

没有模型 key：live 记录 SKIPPED，不计通过。没有 GUI 自动化权限：采用人工操作
真实窗口并采集相同事件证据；侧车协议验收只能计 A9，不能冒充 A10。
测试完成先停测试 desktop 父进程再清理子进程与隔离模型秘密。

## 8. 验证入口与交付

局部回归按修改 crate 跑，使用 all-features 和项目分级测试入口。
推荐现有入口：`just test-crate yi-agent-supervisors`、
`just test-crate yi-agent-app-server`、`just test-crate yi-agent-subagent`；
真实进程夹具：`cargo test --all-features -p yi-agent --test managed_production_integration`。
新增测试的稳定名称、真实 App 验收命令与日志路径由后续 plan 列明，不能写成已有命令。

实现提交前跑 `cargo fmt --all` 和完整 `just ci`。CI 不调用真实模型；
此 worktree 没有 `just test-live` 或 nextest live profile，不把主线命令当成本地已有。
本期使用 plan 明列的拟新增独立显式 live 入口；真实模型不进默认测试，
无 key 记 SKIPPED，不计通过，不顺便回迁整套主线测试分级。
本轮只写 spec/plan，因此不跑编译/CI，也不把历史 CI 绿算成新增实现验收。

完成条件：A1–A11 的适用项全部有对应证据、审查 Important 问题解决、最终代码 CI 全绿，
同步 project-management 与 bug-list 的状态及计数。A10 未验收或 live SKIPPED 时必须
如实记录剩余边界，不标记“App subagent 已正常”。

交付停在 ready for 用户决定合并，保留 worktree/分支。用户最新指令已允许
反复审查后编写独立 implementation plan；本轮不执行代码修复，也不自行合并。
