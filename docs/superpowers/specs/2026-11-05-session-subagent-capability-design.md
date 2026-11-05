# 会话 subagent 能力与执行状态解耦

日期：2026-11-05
状态：已完成主会话三轮书面/代码对照复核；实施计划已编写；未实施

## 1. 问题与证据

用户在 Kanban 会话调用 `spawn_agent` 时遇到：

```text
daemon rejected spawn request: invalid_state:
a settled parent task (state: stalled) cannot spawn descendants
```

2026-11-05 对项目 `.yi-agent/runtime/runtime.sqlite` 的只读调查确认：

- 会话 `thread-9c28e004-3a29-4fe4-b532-0a5d167b572d` 的 attachment 指向 root
  `6abe22b0-2128-4c4b-9b3e-85e7fb775373`。
- root 于 06:53:50 UTC 创建，06:59:00 UTC 写入 `task_stalled`。
- 事件证据为 `elapsed_secs = 309`、`last_meaningful_event_id = null`；
  该 attempt 的 `max_idle_time_secs = 300`。
- 调查时项目库有 134 条 stalled application root，`foreground_root_state` 为 0 行。
  数量是当时的数据库快照，不代表 134 个当前活跃会话，也不能单凭空表判断授权链路断点。

代码链路（以本设计起点 `7c32a47c` 为准）：

1. `yi-agent-store/src/runtime.rs::spawn_child_with_objective` 预检查父状态，
   `yi-agent-core/src/subagent/supervisor.rs::spawn_with_objective` 再作权威检查。
2. `classify_terminal` 把 stalled、completed、blocked、failed、cancelled 等
   八类状态视为 Settled，拒绝继续派生。
3. application root 的重新 attach 复用旧 task；activate 不支持从 stalled 激活。
   `yi-agent-app-server/src/server/driver/turn_driver.rs` 对 activation 失败只记警告，
   因而会话可能仍能对话，委派却已不可用。
4. 现有 `repository/watchdog.rs` 只豁免 `classification = managed` 的 root。
   embedded/legacy application root 仍可能按 worker 的无进展预算结算。

已有的 Settled 检查解决了真实问题：执行父终结后新生的 child 会错过终态级联，
而结果又无法投递给终态 recipient。因此不能无差别删除该检查。

## 2. 已确认的需求边界

用户确认的目标是：**会话的新一轮委派不能被旧 root 的执行终态永久封禁；
已结束、失败或取消的子代理仍不能继续 spawn。**

本设计必须做到：

- 普通会话与 Kanban 会话使用相同的委派规则。
- desktop/app-server、TUI、headless 的 application root 均有明确的会话容器角色；
  managed 与 embedded 两种部署都覆盖，不能仅修 guardian 路径。
- 会话空闲、跨回合、App/daemon 重启以及历史 root 的旧终态不构成永久禁用原因。
- 新请求仍须通过当前身份、权限、工作目录、资源配额和深度检查。
- 取消仍有效；迟到请求、旧身份、未知停止状态不能借自动恢复绕过。
- 旧 child、attempt、事件、报告和交付记录保留；不自动重跑旧任务。

非目标：改变 Kanban 卡片状态机、自动合并、增加递归深度、重写资源调度器、
自动接管他人会话、自动重发结果未知的 spawn，或移除 guardian 权限与回收机制。

## 3. 方案取舍

| 方案 | 优点 | 问题 | 决定 |
|---|---|---|---|
| root 直接跳过 ParentSettled | 局部改动小 | watchdog、投递与级联仍按执行终态处理，留下不一致 | 不采用 |
| 终态 root 自动 retry | 可复用 attempt | 将会话能力继续绑在执行状态上；可能误启 worker、误重跑旧工作 | 不采用 |
| 会话容器角色与执行状态独立 | 对症解决各部署与历史会话问题，保留执行约束 | 需贯穿持久化、超管、IPC 和前端绑定 | 采用 |

现有 `RuntimeCoordinator::retry_task` 会在 factory 可用时启动 worker，
不能作为“会话恢复委派”的快捷实现。

## 4. 三个独立维度

### 4.1 角色：SessionContainer 与 Execution

引入独立、持久化的 task 角色：

- `SessionContainer`：由 application root attach 路径建立，承载会话的 child
  归属、mailbox、查询与委派，不运行 daemon worker。
- `Execution`：worker 实际执行的任务，包括 child、grandchild、scheduled root
  和非 application 的执行 root。

角色不得从 `parent_id IS NULL`、cwd、thread id 字符串或 managed 分类猜测。
子代理即使包装了 `ThreadRoot::from_handle`，也仍是 Execution。
新建路径由 daemon 写入角色；调用者不能在 spawn body 中自行声明容器身份。

角色以 task 的独立字段持久化，默认 Execution。现存有效 application attachment
是旧容器角色的迁移依据，详见 §8。数据库 schema 版本由实施时按主线实际版本分配。

### 4.2 授权与准入

managed 继续使用 guardian 的 Business lease、私有授权桥和 root/driver capability。
embedded 使用已有 application capability 与本地 binding 生命周期，不把它描述成
已经获得 guardian 认证，也不凭公开 thread key 获得新授权。

“是会话容器”只决定生命周期语义，不代表有调用权限：

- 身份无效、owner 已释放、会话已关闭、root 冻结或无权接管时继续拒绝。
- managed 认证失败不能切换成 embedded 来绕过拒绝。
- 无 guardian 的合法 embedded 部署不能因为缺少 managed 分类而被 worker 预算判死。
- 角色本身不能授权新 owner、生成停止证明或证明旧执行已经结束。

embedded 新建容器由宿主先生成随机接续凭证并安全落盘，再以其摘要参与原子创建；
只有创建成功者获得该容器的身份，创建失败者不得覆盖既有摘要。命中既有 attachment
时，公开 thread key 只能定位，不能取回秘密。宿主将接续凭证保存在自己的
会话存储（私有字段、文件权限 0600），不能放进模型上下文、工具参数或公开查询。
该长期凭证只授权身份接续，不直接授权 spawn；每代 daemon 仍须签发短期绑定/turn 能力。
已持有的历史 application capability 可作为一次性升级证明，成功后撤销其业务写权限；
若升级前未持有且无 guardian 授权，不得靠重新 attach 下载秘密，返回
`session_identity_required`，由用户显式走既有受信恢复渠道。损坏/丢失凭证不自动强占。
这不试图隔离本机同 UID 的恶意进程，但必须杜绝只知道 thread key 的业务调用者接管。
managed 路径只接受现有 guardian 授权，绝不接受 embedded 凭证作为替代。

新凭证只有摘要写入 runtime；正常应答中不返回长期秘密。创建/升级的 operation id
与预存凭证绑定，应答丢失时同一宿主可证明身份并取得幂等结果；丢失宿主凭证则明确
阻塞并走受信恢复，不能用安全性换“无条件恢复”。

### 4.3 执行状态与历史

`TaskState` 和 attempt 继续描述 Execution 的运行结果，不作为容器能力的开关。
容器活动使用独立的 idle/running/unknown 快照，关闭与准入使用独立生命周期记录。

现有历史容器的 `tasks.state_json` 和已关闭 attempt 保留作为兼容历史快照：
不将 stalled 覆写为 running，不把已关闭 attempt 重新打开，不为恢复委派调用 retry。
容器的新活动、准入、能力及报告投递必须读取容器记录，不把历史 state_json 当现状。
观察接口明确区分 `role`、容器活动与 `historical_execution_state`，避免 UI 显示矛盾状态。
对 Execution 的状态序列化和查询含义保持不变。

## 5. 会话委派上下文与过期请求

### 5.1 daemon 持有的上下文

每个容器有独立的委派绑定记录，关联容器 task id，并保存：

- 当前 owner/binding epoch 与会话是否关闭；
- daemon generation、当前宿主执行代次；
- 当前 turn id 及其是否允许发起委派；
- 已提交绑定/恢复操作的幂等标识。

授权秘密只留在当前有效持有者和运行时的能力表，不能由上述公开字段推导。
重启后旧秘密不作为新授权；持久记录只用于恢复事实、关闭准入和判断冲突。
尽量复用已有 generation/binding/driver 栅栏，embedded 补齐等价的本地边界，
不再建立一套与 guardian 相互竞争的 managed 授权来源。

### 5.2 宿主开始回合

application 会话在每个回合开始、模型可能调用工具前，确保有有效委派上下文：

1. 定位本会话稳定的容器；校验现有 application 身份及项目范围。
2. 确认当前 owner/宿主绑定可用，必要时执行安全重连，不能强占其他活 owner。
3. 为本回合取得 daemon 确认的 turn 栅栏，再开放会话委派调用。
4. 准备失败时不宣称委派已恢复；对话可继续，但宿主返回明确的委派不可用原因。

不是每回合新建 root，也不是每回合重启 child。无身份变更时复用当前宿主绑定；
turn 栅栏变化不撤销仍合法运行的 child 自身能力。

### 5.3 工具调用与取消边界

会话侧的 spawn、fork 上传、消息和其他写操作携带发起它的回合/执行栅栏。
工具调用开始时捕获该上下文，不能在旧调用被拒后从共享对象取新 turn 的秘密再冒充新调用。

回合结束或取消时关闭该回合的新写准入；之后的迟到 spawn 被拒绝。
正常回合结束不自动取消后台 child：它们仍属于会话，由新回合查询、接收结果或取消。
显式取消会话子树、关闭会话或释放 owner 时，仍按已有取消范围和停止证明回收，
不能用“容器可继续使用”替代物理停止。

具体语义必须区分：结束/中断当前 turn 只撤销该 turn 的写准入；`cancel_agent`
撤销所选 child 的执行及后代，不永久关闭会话；临时 detach/宿主退出关闭当前绑定，
可在身份和停止边界满足后重新绑定；明确的会话关闭/删除写入持久 tombstone，不能被
普通 attach、begin-turn 或 daemon 重启清除。取消已接受的 spawn 按提交先后处理：
关闭先提交则 spawn 拒绝，spawn 先提交则它属于之后取消/释放必须覆盖的执行集合。
宿主本地先封禁本轮调用，daemon 在同一准入屏障内提交关闭；无法确认 daemon 已关闭
时不能报告“关闭已生效”，更不能开放新回合。物理停止与资源释放仍另行验证。

Execution 的请求继续使用自身 task/attempt 能力；已终止 child 即使仍持有旧 token
也不能派生，且不得转走 application root 入口。

## 6. 统一准入、watchdog 与投递

### 6.1 spawn 准入

公共边界先鉴权，再按 daemon 记录的角色分流：

- SessionContainer：检查当前会话/回合上下文与准入，不因历史 TaskState 拒绝。
- Execution：保留 ParentSettled、当前 attempt/能力以及执行准入检查。
- 两者均保留深度、child 上限、全局队列/运行容量、sandbox、coding Git 条件和工作目录检查。

store 预检查和 supervisor 权威检查使用同一角色规则，不能只改一层。
fork 内容只有在必要校验通过后才消费，拒绝路径不白白丢弃上传上下文。

容器不得借普通 `SpawnChild`、`SendMessage`、`BeginForkUpload` 或取消/交付等旧入口
绕过 turn 准入；daemon 根据真实角色统一检查所有写路由。Execution 即使使用 application
入口也不能通过鉴权。turn 的捕获点在 `Agent::run` 的不可变本轮上下文；工具开始执行时
从该上下文取快照，不能从一个可被新回合覆盖的“current turn”槽位读取。上传的
begin/chunk/finish/consume 都绑定同一 generation/binding/turn，跨回合 token 拒绝；
关闭后只允许不产生业务副作用的上传清理。只读与后台 child 完成通知不要求旧 turn 仍开放。

### 6.2 watchdog、启动恢复与级联

所有预算适用性判断读取角色，不只改 watchdog 候选 SQL：

- 容器不受 worker idle/wall-time/turn/token 预算结算，候选扫描和终态提交均检查。
- 旧扫描结果不能在角色迁移后将容器结算。
- 容器不进入 worker 的启动恢复、孤儿任务结算或自动启动 worker 路径。
- child、grandchild、scheduled root 等 Execution 保留原预算、恢复和终态级联。
- 容器的明确关闭/删除仍关闭准入、按范围取消后代；历史 worker 终态不再作为其级联触发器。

### 6.3 mailbox 与结果归属

SessionContainer 在会话开放时可接收所属 child 的完成通知，不能因旧 TaskState 为
stalled/completed 而触发 RecipientTerminal。消息邻接/子树权限不变。

新回合可以 wait/inspect 旧 child 的结果和交付；历史报告不因重连被清空。
关闭后的容器不接受新的业务消息，已有结果仍可经授权的历史查询读取。
Execution 的终态 recipient 规则保持有效。

## 7. App / daemon 重启与安全接续

### 7.1 只重启前端

容器 id 与 child 归属保持稳定。新前端重新证明身份并建立新的宿主绑定；
旧宿主/旧 turn 的写请求拒绝，不能直接沿用持久化的旧能力。
其他仍活跃的会话不受影响。

### 7.2 daemon 重启

generation 更新，旧绑定准入关闭。容器角色与历史保留，不把容器本身停放成
recovery_required。Execution 仍走已有恢复流程，不自动续跑。

新宿主可在旧执行已证明停止、权限有效时自动接续委派，无需用户手改状态。
daemon 退出不代表它启动的所有工具进程都已退出；有未知工具组或旧前台执行时，
返回结构化阻塞原因，不能以缓存丢失或超时冒充停止证明。

停止边界按代次与执行身份划分，而非“整棵会话树必须全停”：替换同一宿主绑定时，
必须证明旧宿主前台执行/工具组已停止或从未启动；正常回合结束后仍合法运行的独立
child 继续由 daemon 管理，不为新 turn 强行取消。daemon 重启后的旧 child 保持
recovery_required 并隔离、继续占用未证明释放的资源；若其影响范围与新工作冲突则
阻塞新写入（尤其共享目录写入），无冲突且停止边界独立时不把它当整个会话永久锁。
unknown 执行登记必须保留，缺记录不是停止证明；显式破坏性恢复仍需确认。
兼容 `RootRef.attempt_id` 对容器只用于核对历史快照/寻址，不创建或重开 attempt；
容器授权栅栏以独立 binding/driver/turn 为准，Execution 的 attempt 栅栏不变。

### 7.3 幂等与并发

容器绑定/历史兼容操作按稳定 operation id 幂等；同旧 epoch 的并发接续最多一个提交。
角色、准入、身份代次切换在现有提交屏障与数据库事务内重核，内存只发布已提交结果。
不持有全局 gate 等待网络、LLM、工具或线程退出。

spawn 仍是有副作用的请求：应答丢失不自动重发，不把“已修连接”说成“上次没执行”。
幂等重连成功也不能使旧回合 spawn 在新回合获得重放资格。

## 8. 历史 root 与迁移

迁移在 watchdog/startup sweep 之前完成，仅回填角色及准入关闭的容器记录；
关闭准入不等于持久会话关闭，不能为升级批量写入关闭 tombstone：

1. 有完整且一致 application attachment、task 为 root、session 对应正确的记录，
   标为 SessionContainer；detached attachment 不自动变 attached。
2. 其他 task 默认 Execution。相互矛盾、同 task 不一致归属等损坏 attachment
   不猜测、不自动修复，返回可诊断的绑定错误。
3. 不修改历史 attempt、事件、终态证据，不重启 child，不回滚已有级联取消。
4. 仅迁移角色不开放准入；宿主接续仍需当前身份与旧执行停止边界。
5. 旧 root 的 stalled/completed/failed/recovery_required/cancelled 均不单独构成
   新回合永久封禁原因；显式会话关闭/删除、有效 owner 撤销仍是独立的拒绝依据。
6. 仅凭历史 cancelled 标签无法区分预算级联与用户关闭时，不猜测可复活；
   用持久 attachment/关闭/owner 事实及安全接续检查决定，证据不足则明确阻塞。

这不是把旧执行恢复为 running：旧 attempt 仍结束，新建的是会话的当前委派绑定。
不要求为“仅建立委派能力、没有破坏性动作”进行旧 spec 的 stalled retry 确认。
若需要停止仍活动的旧执行、取消/回收旧子树，沿用已有预览确认与停止证明；
不可把这些动作隐藏在一次自动 spawn 中。

## 9. 兼容与部署

本设计替代 [前台根生命周期设计](./2026-11-03-foreground-root-lifecycle-design.md)
中“只有 managed 容器免预算，legacy application root 继续按 Execution 处理”的边界，
以及“旧 stalled 容器只能通过新 attempt 才能恢复委派”的要求。
其 guardian 授权、租约退出、driver 栅栏、停止双证据、显式破坏性恢复规则仍有效。

角色、会话上下文和相关 IPC 能力必须成套启用并广告版本化能力。
旧客户端不能在新模式下通过缺少回合栅栏的写请求绕过取消；返回明确的升级/能力不匹配错误。
新客户端连接旧 daemon 不承诺已修复此问题，也不静默 fallback 成无栅栏写入。
不得为升级自动停止有活工作且身份不明的 daemon，或回退数据库 schema。

普通会话与 Kanban 共用 application 装配路径；Kanban 不增加专用豁免、不卡片 state
补丁。TUI/headless 的固定 binding 也须保留容器与 Execution 的区分。

## 10. 错误与可观测性

按真实失败边界返回稳定的结构化原因：旧回合/旧身份、会话关闭、owner 冲突、
停止未证明、权限拒绝、协议不匹配，以及原有资源/Git/目录错误。
具体 IPC code 在实施计划中与现有错误枚举对齐，不用自由文本判断恢复分支。

日志包含容器 id、角色、generation、绑定代次、turn id 与失败阶段，秘密必须脱敏。
不把旧 worker 的 terminal 标签当作容器当前可用性的唯一展示。
“能打开历史会话”与“委派上下文已就绪”必须有独立可验证的宿主结果。

## 11. 验收矩阵

以下均为待实现判据，不是已通过的测试报告。统一测试名前缀建议为
`session_delegation_lifecycle`；实施计划须为各行指定实际测试名并检查非零命中。

| 编号 | 可验证结果 | 层级 |
|---|---|---|
| C1 | 普通/Kanban 容器角色一致；scheduled root、child、grandchild 仍为 Execution | core/store/装配 |
| C2 | managed 与 embedded 容器跨 301/2701 秒、progress 为零仍可正常 spawn | coordinator/IPC |
| C3 | child 无进展仍 stalled，耗尽预算仍终止并级联，不能用 ThreadRoot 包装成容器 | core/store |
| C4 | 旧 stalled/completed/failed/recovery_required root 可安全建立新委派上下文，旧 state/attempt/事件不变 | SQLite/IPC |
| C5 | 关闭/删除或无权限的旧会话拒绝；历史 cancelled 不能掩盖显式关闭 | migration/IPC |
| C6 | 旧 root 有未证明停止的执行时接续阻塞；仅打开会话/只读 inspect 不停止旧任务 | lifecycle/IPC |
| C7 | 正常结束回合后迟到 spawn/fork/消息写入被拒，新回合可 spawn，不自动重放旧请求 | subagent/driver |
| C8 | 已取消或完成的 Execution 父仍 ParentSettled；无论走哪个 IPC 入口都不能冒用 root | core/IPC |
| C9 | 正常回合结束不取消合法 child；下一回合仍可 wait/inspect/review/cancel，报告归属正确 | coordinator/工具 |
| C10 | 会话容器接收结果不受旧 terminal 标签阻断；终态 Execution recipient 仍拒绝 | mailbox/IPC |
| C11 | 前端重启、daemon 重启后稳定定位原容器，新身份可委派，旧身份拒绝，旧 child 不自动续跑 | 持久库/进程集成 |
| C12 | 并发接续只提交一次；事务失败不发布内存成功；丢失 spawn 应答不自动产生第二个 child | 故障注入/IPC |
| C13 | watchdog 旧快照、release/关闭与 spawn 的竞态不创建失主 child，不提前释放未停止资源 | coordinator/并发屏障 |
| C14 | 两会话同目录独立配额/子树，接续 A 不撤销 B；权限、Git、sandbox、深度上限不变 | 工具/IPC |
| C15 | desktop/app-server、TUI、headless 真实入口均传递容器上下文；后台 worker 入口仍是 Execution | 各入口集成 |
| C16 | 缺字段旧写协议明确拒绝；managed 授权拒绝不降级绕过；embedded 无 guardian 仍可合法工作 | 兼容/授权 |
| C17 | 迁移先于 sweep；重复迁移幂等；损坏 attachment 不晋升、不改旧证据 | migration |

验证使用临时数据库/socket/工作目录，不修改生产 runtime。时间判据用可控时钟，
不能让测试真睡 300/2700 秒。负测试需覆盖删除角色检查、恢复状态门控、接受旧栅栏
等变异，证明测试确实约束目标语义。

## 12. 交付与后续流程

本次交付只包含书面 spec、实施计划、模块需求记录和索引更新，不包含代码修复或已通过声明。
按用户“反复检查 spec，无问题后写 plan”的要求，先完成三轮复核再编写计划；
计划拆分持久角色、容器策略、会话栅栏、历史接续、三端接线和回归验证等有依赖的步骤。

实现阶段遵循 worktree、TDD、代码审查和 `just ci` 门禁；真实 LLM 不作为默认测试依赖。
完成必须以 C1–C17 的实际证据为准，不能仅凭当前会话一次 spawn 成功宣称修复。

## 13. 2026-11-05 复核记录

两条独立只读评审请求均被当前 daemon 以本问题拒绝，没有评审 agent 启动。
以下为主会话三轮审查，不冒称独立评审或运行验证：

| 轮次 | 代码对照与反例 | 发现与处理 |
|---|---|---|
| 1：角色与持久化 | `core/subagent/{task,supervisor}.rs`、`store/repository/{application_root,foreground,watchdog}.rs`、`store/runtime.rs` | 确认 worker 同样可包装 ThreadRoot；显式角色与历史状态分离不可省略；补清 RootRef 的旧 attempt 只作兼容寻址，不重开 attempt |
| 2：授权、停止与重启 | `store/ipc/{authority,protocol,daemon}.rs`、`store/runtime/foreground.rs`、`lifecycle/foreground.rs` | 原文未明确接续凭证取得方式及停止集合；补入 embedded 私有凭证、只知 thread key 不可接管、旧能力一次性升级、独立 child 与旧宿主停止边界、冲突隔离 |
| 3：回合与并发反例 | `core/{agent,tool}.rs`、`subagent/{lib,thread_root}.rs`、`app-server/server/driver/turn_driver.rs`、CLI `main.rs` | 补入 run 级不可变工具快照、全写路由/上传阶段栅栏、detach/close 区别、关闭提交先后及失联时不宣称成功；反查新文本未放松 Execution 终态与 guardian 授权 |

复核后未发现阻止编写计划的设计矛盾。旧 embedded 会话缺少接续凭证时仍可能返回
身份阻塞，这是安全边界，不承诺所有历史会话均能无条件自动恢复。该场景必须在实现
验收中显式覆盖并清楚告知用户；不得改生产数据库或重新下载旧秘密来伪造恢复。
实施中的接口可行性仍由编译、红绿测试和代码审查证明；若发现与这些约束冲突，先修订设计。
