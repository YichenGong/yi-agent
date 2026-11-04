# 前台会话根任务生命周期与 watchdog 隔离设计

日期：2026-11-03
状态：已完成文档自审；仅设计，未实施。
设计基线：`30de1c65`（shared daemon lifecycle and upgrade 已合入）。

## 1. 问题与证据

用户观察：Kanban 启动的会话调用 `spawn_agent` 返回：

```text
invalid_state: a settled parent task (state: stalled) cannot spawn descendants
```

2026-11-03 的只读调查在 Kanban worktree 的 runtime SQLite 与会话历史中交叉确认：

| UTC 时间 | “看板观测修复”会话中的事实 |
|---|---|
| 08:27:51 | application root `c8934087-f369-4c22-b978-69477904a979` 启动 |
| 08:29:38 | 成功创建子任务 `cfd2af98-45fc-43f4-b9e7-247d85fbd4f9` |
| 08:33:00 | 父任务被判 `stalled`，子任务同时变为 `cancelled` |
| 之后 | coding 与只读 PONG spawn 均因父任务已结算被拒绝 |

父任务终止证据：`elapsed_secs = 308`、`last_meaningful_event_id = null`，
`last_meaningful_at` 仍为根任务创建时刻。来源是该 worktree 的
`.yi-agent/runtime/runtime.sqlite`，及
`.yi-agent/threads/thread-32a991af-bb29-4946-b51c-f2774f0d5e89.jsonl` 第一条 turn。
第二个 Kanban 会话（process selector contract）也有相同形态的根任务停滞记录。
这些本机记录不是仓库夹具；实施须建立脱敏、确定性的回归夹具。

普通会话成功样本在根任务创建后 46 秒即 spawn；普通会话也存在约 5 分钟后
`stalled` 的根任务。因此不能将问题定义成 Kanban 专属权限限制。

### 1.1 代码上的因果链

以下路径均相对仓库根，符号以设计基线为准：

- `yi-agent-rs/crates/yi-agent-store/src/schedule.rs`：
  `WatchdogLimits::default` 设置 300 秒 idle 和 2700 秒 wall-time；
  `evaluate_watchdog` 根据 `last_meaningful_at` 产生 `Stalled`。
- `yi-agent-rs/crates/yi-agent-store/src/repository/watchdog.rs`：
  `active_attempt_watchdogs` 未排除前台 application root。
- `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs`：worker 对成功工具结果调用
  `report_meaningful_progress`；桌面前台 driver 没有对应的根任务进展上报链。
- `yi-agent-rs/crates/yi-agent-app-server/src/server/driver/turn_driver.rs`：
  `activation_attempted` 仅允许首个 turn 激活，后续 turn 不同步根任务活动状态。
- `yi-agent-rs/crates/yi-agent-store/src/runtime.rs`：
  `record_watchdog_terminal` 结算父任务并级联处理后代；
  `spawn_child_with_objective` 拒绝已结算父任务；
  `activate_application_root` 不接受从 `stalled` 重新激活。

根因是**将长期存在的前台会话容器当作后台 worker attempt 计时结算**，而非
spawn 的终态闸门有误。前台 driver 与 daemon 的生命周期事实随之脱节。

### 1.2 自审发现的现有接口缺口

以下是待实施工作，不能描述为已接通：

- `yi-agent-lifecycle/src/types.rs` 只有 `RootAuthorization` 类型；现有 attach wire
  不携带它。`runtime.rs` 的 attach 以 `idempotency_key` 调用 `bind_root`，不构成可信 owner 认证。
- `runtime.rs::bind_root` 注释也说明 guardian 到业务 daemon 的私有认证通道尚待连接。
- guardian 与 coordinator 各自持有的 `RuntimeClients` 不是自动共享的权威；必须接通失效通知及提交屏障。
- `yi-agent-store/src/ipc/daemon.rs` 按连接并发处理请求，不存在可直接复用的全局串行入口。
- `execution_stopped` 无 pending stop 记录时返回 false；前台 root 不运行 daemon worker，
  不可用其 worker join 作为前台 turn 的停止证明。

## 2. 目标与非目标

### 2.1 目标

1. 普通桌面、Kanban 驱动桌面、TUI、启用委派的 headless run 的 supported managed 路径使用同一契约。
2. 合法存活的前台会话跨越 300 秒、2700 秒后仍能创建子任务；等待用户、权限或子任务不触发父终态级联。
3. root 存活由可信 owner 生命周期决定，不由模型产出、工具输出或 UI 在线决定。
4. 回合 running / idle 与 owner 存活分开表达；release、租约 expiry 仍有有界回收。
5. 历史误结算 `stalled` root 有显式、可审计的后继 attempt 恢复；旧 attempt 与 child 不复活。
6. child、孙任务和后台 scheduled root 保留现有 watchdog 与终态保护。

### 2.2 非目标

- 不改变 Kanban 卡片状态机、并发名额、自动合并或提示词。
- 不放宽 `ParentSettled`，不通过重启 daemon、改 SQLite 或提高 idle 阈值解决。
- 不把心跳算作 progress，不给后台 worker 无限续命。
- 不新增第二套客户端心跳、租约或跨进程 owner 身份体系；补齐现有体系的授权/回收通路。
- 不把 owner 存活当作模型无卡死的证明；provider/tool 超时与用户取消独立生效。
- 不部署产物、不迁移正在运行的 unmanaged 实例、不调用真实 LLM。
- 不提供任意终态重试或仅凭 thread id 的跨 owner 接管。

## 3. 方案取舍

| 方案 | 优点 | 问题 | 决定 |
|---|---|---|---|
| 增大或清空所有任务的 idle limit | 改动小 | 混淆会话与执行，削弱子任务检测，wall-time 仍会结算父任务 | 拒绝 |
| 前台成功工具上报根进展 | 与 worker 类似 | 用户等待、长工具和回合空闲仍误杀 | 不作为根存活机制 |
| application root 绑定 owner 生命周期，worker 保留进展 watchdog | 明确归属，复用租约回收 | 须补齐授权、准入屏障、活动与恢复契约 | 采用 |

本需求是同一生命周期不变量的纵向修复，授权闭环、root 策略和宿主接入相互依赖，
不得只交付“跳过 watchdog”即开放此能力。模型执行故障仍由执行层检测。

## 4. 身份、权威与作用域

### 4.1 三个独立维度

| 维度 | 权威事实 | 不能推导的结论 |
|---|---|---|
| Owner 存活与准入 | guardian 租约及 daemon 已确认的 root 绑定 | 心跳不代表执行有进展 |
| 会话活动 | 前台 driver 真实的 turn 开始、等待、结束 | idle 不代表会话已结束或应取消 child |
| 执行进展 | worker 的现有有效事件 | 前台无进展不代表 owner 已死 |

复用 `yi-agent-lifecycle/src/{leases.rs,client.rs}` 的 `LeaseTable`、`FrontendLease`，
及 `yi-agent-store/src/lifecycle.rs` 的 `RuntimeClients`。当前心跳间隔 5 秒、租约窗口
30 秒，仍以既有实现为准；EOF 不立即退出也不续租，release/expire 不允许心跳复活。

### 4.2 必须补齐的可信授权通路

1. App 的可续租 grant 留在桌面壳；TUI/headless 留在各自 host。禁止交给模型、插件、app-server。
2. 宿主通过已认证的 business lease 向 guardian 申请限定 scope、daemon generation、
   root、owner 的 `RootAuthorization`。`LeaseOnly` 不可申请。授权秘密仅存必要内存、Debug 脱敏。
3. guardian 用创建 daemon 时建立的私有通道安装/撤销授权。此通道与工具 managed-spawn
   通道分离，工具不能发送授权安装消息；daemon 不信任公开 RPC 中自报的 client id。
4. 新 root 的建立采用 prepare/bind：宿主为稳定 thread key 发起授权准备，daemon 分配或查到
   root 后，guardian 核实 scope/workspace/会话权限，再安装精确 root 授权并提交绑定。
   pending root 不开放委派、不启动 worker，不进入执行预算计时；它也不获得长期存活权，
   由有界 reservation deadline、失败回滚和 startup 清理负责回收，不留执行资源。
5. 同一 owner 可幂等续接自己的绑定；不同活跃 owner 不得仅凭 key 取走 capability。
   旧 root capability 不可由公开 attach key 派生或直接返还给未授权调用者。
6. guardian 的 release/expiry/maintenance 必须关闭 daemon 的相应准入并收到确认，
   随后枚举、回收已接受的任务；断连不能虚报回收完成。daemon 对授权桥失联立即停止新业务准入，
   对存量执行按既有租约截止与回收协议处理，不新建另一套 heartbeat。
7. lease resume 的 credential epoch 仅保护控制连接；不得因此重置全部 root 的绑定。
   root 的 `binding_epoch` 仅在授权撤销/重新绑定时递增，daemon generation 变化则重新授权。

租约验证和关闭通知必须经过实际跨进程通道测试，不能只在两个独立内存表各测一次。

### 4.3 root 分类与兼容边界

`application_root_attachments.root_task_id` 与所属 session 的对应关系是前台分类依据。
`depth == 0`、无 parent、cwd 含 `kanban`、thread id 非空均不是充分条件。

- foreground application root：前台宿主运行模型，daemon root 是委派树容器。
- worker / scheduled root：daemon/executor 实际调度，保留执行预算。
- 无 attachment 且未声明 foreground 的 scheduled root 是正常执行任务，不算损坏记录。
- 已声明 foreground 但 attachment/session 关系矛盾时，拒绝副作用并诊断，不猜测豁免。
- pending（认证prepare中的新root）或已有managed身份但本代尚未重新授权的root处于 admission closed；不能永久豁免后继续派发。
- 旧attachment的迁移只回填前台角色，不自动切换managed预算策略；legacy仍按Execution策略。
  转为managed须经过认证prepare及必要停止边界，不能只改分类就接管活动执行。

启动顺序分为 inspect/bootstrap 与业务绑定：允许无业务授权的只读能力探测、启动确认；
须先获得 business lease、完成授权再激活 foreground root，避免“先 attach 才能注册”的循环。

本次完整保证限定于 supported managed 三端入口。embedded/legacy 无可信 owner 时保留原
执行预算和既有委派语义，协商返回 `foreground_lifecycle_unsupported`，明确本缺陷尚未修复；
不是禁止所有旧聊天或旧委派，也不能静默永久豁免。新增兼容适配须另审其有界死亡通知。

## 5. watchdog 策略

对已分类且经可信绑定激活的 foreground root，不使用 `WatchdogLimits` 的 idle、wall-time、
turn/token/cost/retry/rework 预算结算容器；前台自身 `AgentConfig` 和 provider/tool 超时不变。

1. 候选快照查询与最终终态提交共享分类策略。
2. 旧快照提交时在短提交屏障及事务内重核 active attempt、分类、绑定版本；不能误结算根容器。
3. owner 已失效的 managed root 走 lifecycle 回收，而不是退回 worker watchdog 伪造 `stalled`。
4. 迁移后未授权 root 不接受业务；授权缺口不能导致一个仍在运行的失主树无限保活。
5. 跳过判定不生成伪 progress，不刷新 `last_meaningful_at` 或 usage，不取消后代；旧 watchdog 行保留审计。
6. 后台 child、孙任务、scheduled root 的候选、预算及级联行为不变。

## 6. 回合与会话活动

### 6.1 独立于 TaskState 的快照

持久逻辑字段：root/session、active attempt、daemon generation、`binding_epoch`、
`driver_epoch`、`turn_id`（可空）、`activity_seq`、phase、最后变更时间。
phase：`idle`、`running`、`waiting_for_children`、`waiting_for_permission`、`unknown`。
未知等待细节可维持 running；崩溃/重启后的历史 running 只能展示 unknown，不伪造 idle。

活动不把 TaskState 改成 completed/paused/stalled，不触发 child 取消。第一次激活按现有语义
将 root 置 running；之后无论 turn idle 还是 waiting，TaskState 仍 running。objective 只写一次。

### 6.2 driver 绑定与活动同步

- 宿主先调用 `BindApplicationRootDriver`，认证 root 授权；daemon 返回当前 attempt、
  binding epoch、服务器签发的 driver epoch、last activity seq，以及仅供 driver 的窄 capability。
- 传输重连用同一 bind operation id 返回同一绑定/last seq；新进程或接替 driver 必须证明旧
  driver 已停止后才递增 driver epoch。每个 epoch 的 seq 从返回值 + 1 开始，不从客户端任意归零。
- `SetApplicationRootActivity` 携带上述栅栏、turn id、seq、phase。相同 seq/内容幂等；
  同 seq 异内容拒绝，旧 seq 回报当前版本。旧 epoch/attempt/generation 一律拒绝。
- turn 开始只允许 idle→running；同一 driver 不并行开启第二 turn。finish 必须匹配当前 turn，
  更大 seq 的旧 turn finish 也不得覆盖新 turn。等待态只能关联当前 turn。
- root 写操作使用 driver 窄 capability 并验证 epoch；child 自身使用其既有树权限，不能冒充 driver。
  turn token 不能安装授权或恢复 root，模型只能获得其工具所需的子集。

每个实际 turn 开始及正常结束/取消/错误的 finally 路径同步；SIGKILL 无法保证 finally，
由 driver 断连标记 unknown 及宿主的停止/重新绑定处理。UI/relay 连接不是 driver 控制连接。
app-server、TUI、headless 使用共享封装，Kanban 不加分支。

状态上报用有界 I/O；同一 turn 的中间等待态可合并，开始/结束边界保序，不跨 turn 合并。
含糊失败保留同seq/同payload待定消息；重连查询快照后重送或确认，不跳号覆盖旧turn。
上报失败诊断可见但不续租、不自动取消 child。首次激活/绑定未完成不能宣布委派可用；
活动失联只关闭本driver委派，不以错误传播中止普通聊天。
重试仅限按稳定操作 id/版本幂等的操作；spawn/message 等含糊失败不得自动重放。

## 7. 正常离开、断连与重启

- turn idle：owner 仍有效，child 可继续，下一 turn 可派发。
- UI/relay/WS 短断连：不等于 owner 或 driver 离开，不回收根树。
- lifecycle EOF：沿用租约窗口；短断连不续租、不立即 release。
- driver 死亡但 App owner 仍活：活动 unknown，撤销旧 driver 准入；child 不仅因 driver EOF 被取消。
  新 driver 经停止证明接续；长期无 driver 的会话仍由 owner 决定 detach，不靠 progress 计时。
- 关闭/删除一会话：明确 detach 其 root，不 release 同 App 的其他会话；同时停止该会话前台执行。
- release/expiry：先关该 owner 的准入，停止前台 turn、daemon worker 和各工具组，按既有
  `10s grace + 5s confirm` 总窗口回收；缺任一停止证明必须返回不完整结果，不释放仍占用的执行资源。
- daemon 重启/升级：授权默认失效、准入关闭；保留 root 身份、终态历史与幂等结果，重新确认
  owner/driver 后接续未结算容器。旧 generation 消息失效。前台 root 不得被通用 worker
  `recover_inflight_tasks`/orphan sweep 改写成另一种可自动运行的任务；其后代仍按原恢复规则处理。
- owner A 离开而 B 有有效 business lease 时，不 drain B 或全 scope。

## 8. 历史 stalled root 的显式恢复

### 8.1 范围与权限

spawn、attach、只读查询、活动同步、心跳均不得复活终态。仅接受有持久 watchdog
`Stalled` 证据的 application root；completed/failed/cancelled/recovery_required 等不在范围。

支持两种授权来源：当前仍有效的原 owner；或宿主持有该会话原有的不可猜测 root capability，
在旧 owner 已被权威撤销、旧执行已证明停止后，由 guardian 绑定到新的 business owner。
后者是本需求内的受控重新绑定，不是仅凭 thread id/workspace 接管。为避免“先停止才能授权、先授权才能停止”循环，
guardian 可先发行 RecoveryOnly 权限，仅可 inspect/freeze/stop/prepare/query，不能bind driver、spawn、活动写或启动工具；
停止证明齐全后才在 daemon 恢复事务内更新新owner绑定；guardian随后凭私有桥核实提交结果，
幂等签发新 Business 授权，daemon 安装完成前准入仍关闭。跨进程失败按同operation补完授权，不再建attempt；
不能假设guardian与SQLite有跨进程原子事务。旧 credential/key 不足以授权。
旧会话没有可验证 capability 的，返回 `root_recovery_authority_missing`，明确要求人工迁移，
不设管理员 SQL 快捷路径。活跃异 owner 的 root 不允许抢占。

### 8.2 停止证明

恢复准备先封闭该 root 的写准入，并要求宿主停止旧 turn/工具：

- 对仍存活的 driver：取消并 await turn 结束，核验该 turn 的托管工具组停止，然后提交
  绑定到 root/attempt/driver epoch 的 quiescence 证据。此时不得继续在同一旧 turn 调 spawn。
- driver 进程已退出：guardian 以受管理的进程身份及工具组记录证明退出；PID 不存在不等于证明。
- daemon child：用现有 worker join 与工具组停止双证据；root 无 daemon worker，不要求虚构 root join。
- 重启后使用带进程身份的回收/停止记录重新核验；内存 pending stop 丢失不等于已停止。
  缺失注册记录而不能重建证明时返回 `stop_unproven`，列出阻塞实体，保持准入关闭。

证明覆盖的执行集合必须封闭：先冻结 root，再枚举；已准入但尚未启动的 child 也在集合中，
worker 启动必须再次检查冻结栅栏。证明持久保存非秘密身份/结果，不能接受 RPC 自报 `stopped=true`。

### 8.3 预览、确认、提交

1. 宿主先使用只读 inspect 展示恢复原因/影响，不冻结执行；用户选择“准备恢复（停止旧执行）”后，
   独立于模型工具调用 `PrepareApplicationRootRecovery`，核验权限、冻结并停止旧执行；
   返回 root、旧 attempt、原因、受影响 child/工具及停止缺口。仅准备成功不创建 successor。
   取消最终确认不创建后继，但不会逆转已经发生的冻结/停止；界面必须预先说明。
   长准备异步返回 Preparing，宿主以相同operation id查询Ready/Blocked/Committed，不能阻塞guardian心跳reader。
2. daemon 在停止已确认后签发一次确认 token（60 秒），绑定 root/expected attempt、
   generation、binding/driver epoch、影响集合版本和稳定 operation id。展示由可信宿主完成；
   恢复 API 不注册为模型工具，不能以请求 body 的 `confirmed=true` 替代宿主确认。
3. 用户确认后调用 `RecoverApplicationRoot`。在短提交屏障及单一事务中重核全部栅栏、
   stopped 集合及 watchdog 证据，以 expected attempt 作 CAS，创建 attempt number + 1。
4. 旧 attempt 保留 stalled、证据与时间；任务 active attempt 指向后继，其 TaskState 为 running，
   活动 idle、turn 为空，driver epoch 更新；新 driver 窄 capability 必须重新签发，旧 driver/task 写权限失效。
   不启动 daemon worker，不重复改写 objective。
5. 同一 operation id/同参数重送返回同一结果（即使 token 已消费或响应丢失）；同 id 异参数拒绝。
   查结果仍须有效会话授权。幂等结果随 root 历史保留，不因 60 秒 token 到期而删除。
   generation 改变后须重新授权查询原结果，不能据旧 token 再创建 attempt。
6. 不同 operation 竞争旧 attempt 最多一个提交。事务失败不消费确认、不发布成功内存状态；
   事务提交后进程崩溃由持久事实 hydration 修复，提交后发布失败则关闭 root 准入并重载，不能报成功。
7. 取消预览/过期不取消“已停止”事实，但不复活终态、不创建 attempt；可重新预览。
8. 旧 child、delivery/report/worktree 历史可读，不重跑/重挂/重放失败 spawn。用户另起新 turn 决定后续。

桌面必须提供会话恢复动作；TUI/headless 提供控制命令/API，能展示原因、影响和阻塞实体。
现有 `resume_task` 会启动 worker，不得未经区分复用。恢复不能靠更换 thread id 或重启绕过。

## 9. 并发与提交屏障

不能依赖“IPC 串行”。新增 coordinator 内短时 admission/commit gate，参与路径包括授权安装/
撤销、root bind/detach、spawn 的持久准入、watchdog 提交、恢复、maintenance。

- 不持有 gate 做网络 I/O、provider/tool 调用、等待 child/worker join、物理停止；WaitAgent 不占 gate。
- 锁序统一为 session supervisor（异步获取，若该操作需要）→短 commit gate→clients→repository。
  gate 内不反向等待 supervisor；release 先在 gate 内关准入并取稳定集合，放锁后通知/停止 supervisor。
- spawn 先准备非执行资源，短 gate 内重核权限/epoch/截止并持久登记 child 及归属，再解锁启动；
  启动前还需核验 child 未取消。提交时登记 starting execution（含取消token/完成witness），
  factory未返回handle时也必须能被release枚举和等待；不能仅靠“再检查一次”关闭check/start竞态。
  release前已登记者必被枚举，release后不得登记，失败准备须清理。
- guardian 与 daemon 的授权桥对每个 scope 排序安装/撤销，桥失效关闭准入；授权验证不得给
  daemon超过已验证租约剩余期限的本地有效期。验证采用daemon发出的单次nonce及发送时刻t0，
  截止以t0+guardian验证时剩余租期保守计算，不能用receipt time重新起算；旧seq/撤销后的消息不能续命。
  guardian不持租约表锁等待daemon应答。过期/撤销不允许请求到达时的旧检查绕过提交校验。
  release 只有 daemon 关闭确认及物理回收结果齐全才报告成功；网络失败返回不完整而非开放重试准入。
- SQLite 事务与 expected attempt/binding epoch CAS 是持久权威；内存 reducer 必须与提交结果一致。
  watchdog、恢复、显式取消互斥裁决；任一栅栏变化即拒绝，不产生半个 successor。
- 状态/错误结构化：`foreground_lifecycle_unsupported`、`root_recovery_required`、`stale_attempt`、
  `stale_driver`、`admission_closed`、`stop_unproven`；不靠英文字符串匹配驱动自动恢复。
- 日志只含非秘密 task/attempt/owner/generation/phase/原因；新增字段不存 grant、授权秘密或完整模型输入。

## 10. 数据与兼容性

分类/绑定版本、活动、恢复操作及停止证据通过 schema migration，版本由实施时分配。
旧 attachment 可回填 foreground 分类，但不能回填“授权已验证”；孤立/矛盾关系列诊断。
旧 stalled attempt 不自动改写；无 attachment 的 scheduled root 保留执行分类和原预算。

新增活动/恢复表不存凭据，授权秘密只在内存；已有 root capability 存储方式不是本次的
全库秘密存储重构，但不能复制到新表或日志。重启重新发授权，持久 epoch 只作栅栏，不作凭据。

控制通道和业务 IPC 各自能力/版本协商；先用旧端可识别的探测，再发送新 variant。
不能仅递增版本而忽略旧反序列化器 `deny_unknown_fields` 的拒绝行为。旧端明确降级，
不自动重启/升级 daemon，不收到 Unsupported 后假装成功。

## 11. 验收矩阵

以下 C1–C16 是未来实施验收，不表示目前已通过。用注入时钟、临时 SQLite/socket/Git、
mock provider；不等真实 5/45 分钟，不使用用户运行数据或 API key。

| 编号 | 场景与可验证结果 | 测试层 |
|---|---|---|
| C1 | 普通/Kanban 同一宿主入口，owner 合法存活；推进 301/2701 秒后可 spawn，无根 watchdog 终态 | coordinator + app-server mock |
| C2 | 前台连续成功工具但不报 root progress，跨阈值仍正常，未伪造 progress | driver + IPC |
| C3 | root 等 child 超过 300 秒，child 有进展且预算未尽；父不 stalled、不误级联，结果可读 | coordinator + worker fixture |
| C4 | root idle 跨 300/2700 秒，下一 turn 可 spawn；objective、旧 child 历史不变 | 三端共享封装 |
| C5 | 后台 child/孙任务/无 attachment 的 scheduled root 仍 stalled/timeout；显式取消级联不变 | store + core |
| C6 | 旧 watchdog 快照在分类/绑定提交后不得误结算；活动变化不能让 worker 获豁免 | repository + coordinator |
| C7 | release/expire 与 spawn 竞态无失主 child；双停止证据前不释放资源，owner B 不受影响 | 实际 guardian-daemon bridge |
| C8 | relay/WS 断开不回收；租约短断重连按既有窗口；旧 driver/乱序 finish 不覆盖新 turn | lifecycle + driver |
| C9 | 持久 stalled root 经确认恢复产生唯一后继，旧证据不变、child 不重跑，新 turn 可 spawn | IPC + subagent |
| C10 | 无确认、跨活跃 owner、伪 capability、旧代/attempt、非 stalled、停止未确认均拒绝；重送/并发幂等 | IPC + supervisors |
| C11 | 重启保留分类、活动版本、恢复结果；授权前关闭准入，历史活动 unknown；不泄漏新增秘密 | migration + IPC |
| C12 | desktop/TUI/headless managed 三端覆盖；legacy 保留原策略并明确降级，不偷偷重启 daemon | 三端入口集成 |
| C13 | 公开 attach key/client id 不能绑定或取走 root；LeaseOnly/工具通道不能发行授权；有效租约可闭合绑定 | guardian + 临时真实 socket |
| C14 | bootstrap→注册→prepare/bind 无循环；桥中断/授权撤销关闭准入；lease resume 不打断合法 root | guardian-daemon 集成 |
| C15 | driver 崩溃、接替、daemon 重启前后，旧消息拒绝；无停止记录不当作停止；前台 root 不被 worker 恢复误处理 | driver + startup sweep |
| C16 | 停止准备期间新派发被拦截；恢复响应丢失、提交前失败/提交后崩溃、确认到期、缺历史授权均有确定结果 | 故障注入 + 持久恢复 |

用例统一 `foreground_root_lifecycle` 前缀/模块并核对非零命中；现有回归保留原名。
建议实施后执行（相对 `yi-agent-rs/`）：

```bash
cargo test -p yi-agent-store --features foreground-test-support --test runtime_coordinator foreground_root_lifecycle
cargo test -p yi-agent-store --features foreground-test-support --test runtime_ipc foreground_root_lifecycle
cargo test -p yi-agent-subagent foreground_root_lifecycle
cargo test -p yi-agent-app-server foreground_root_lifecycle
cargo test -p yi-agent --bin yi-agent foreground_root_lifecycle
cargo test -p yi-agent-lifecycle foreground_root_lifecycle
cargo test -p yi-agent-supervisors --features yi-agent-store/foreground-test-support foreground_root_lifecycle
cargo test -p yi-agent-lifecycle --test client_leases
cargo test -p yi-agent-supervisors --test client_reclamation
cargo fmt --all
just ci
```

文档检查不能替代实现验收；合并结果须再跑 `just ci`。

## 12. 文档自审与交付边界

本轮仅 spec、[implementation plan](../plans/2026-11-03-foreground-root-lifecycle-impl.md) 与项目进度登记，不含代码、schema 执行或运行实例操作。
计划自审后同步澄清了nonce租期校验、RecoveryOnly停止权限、starting执行登记、legacy迁移、只读inspect/异步prepare
及活动失联的降级边界。SQL示例只在临时内存数据库验证，不是项目schema迁移或实现验收。

- 第一轮对照源码纠正：授权类型≠接通、IPC 并发≠串行、root join≠前台停止证明。
- 第二轮按状态机补齐：bootstrap 顺序、seq/driver 代际、硬退出 unknown、旧 owner 受控重新绑定、
  恢复预览/幂等、重启 sweep 分类、legacy 降级边界、无 attachment 的 scheduled root 正常分类。
- 第三轮检查：release/spawn 两种顺序、旧 watchdog 快照、旧 finish 更大 seq、响应丢失、
  commit 前后崩溃、缺停止证据均有明确拒绝或接续结果；未留下影响实施的未决分支。

上述是设计自审结论，不是运行时正确性证明。实施遇到与基线不同的接口，须先修订设计和计划，
不得把未接通接口写成现成事实，也不得静默缩减为只修 Kanban 或只延长超时。
