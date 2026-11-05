# Managed runtime 默认生产接入与存量迁移设计补充

日期：2026-11-05
基线：`7c32a47c`
状态：用户已确认沿用既有生命周期语义；本文待书面审阅，尚未实施。

## 1. 已确认决策与范围

本设计补齐 [共享 daemon 生命周期设计](2026-11-02-shared-daemon-lifecycle-and-upgrade-design.md)
及 [前台 root 生命周期设计](2026-11-03-foreground-root-lifecycle-design.md) 的默认生产接入，
不重做已有 guardian、租约、进程托管或版本切换状态机。

用户于本次讨论明确确认继续沿用已有设计：

- macOS 关闭窗口只是隐藏；真正退出 App 才释放 App 的项目租约。
- App 退出回收它名下的工作，不影响同项目仍存活的 TUI。
- 最后一个前端离开后有界回收 runtime；插件安装、看板登记、历史记录不单独保活。
- 保留同一项目数据库、历史、交付与工作目录；不自动重放业务副作用。
- 明确手工启动的 daemon 保留 unmanaged 语义，不全局修改公开 CLI 行为。

本期交付是默认启动、租约、root 授权、插件入口、存量迁移与状态展示的纵向闭环。
不新增无人值守模式，不修改看板卡片业务，不操作远端进程，不自动部署或迁移当前
正在使用的正式实例。真实进程验收优先使用独立实例、独立项目和 runtime 目录。

## 2. 问题与证据

以下是本轮只读调查结果，而非实施验收记录：

- 本机 `.app` sidecar 自述版本为 `0.1.13-dev+7c32a47c`，SHA-256 与包内
  `runtime-sidecar.json` 一致；并非单纯旧二进制残留。
- 进程树为 desktop → app-server → `daemon serve`，没有 guardian；项目目录
  没有 `guardian-control.sock`。`yi-agent daemon status` 返回 daemon 正常运行。
- desktop、app-server、daemon 均未设置 `YI_AGENT_BRINGUP`。
- `attach.rs::BringUpPolicy::default_for_build` 返回 Embedded，
  `process_bringup_choice` 使用环境策略；`for_managed_frontend` 虽已存在，未在
  搜索到的生产调用点使用。
- `server/plugin_bridge.rs::spawn_resident_watch_loop` 经 `watch::ensure_daemons`
  调用 `daemon::ensure_running`，仍可能直接启动独立 `daemon serve`。
- `runtime_lifecycle.rs::SocketBackend::status` 只探测 guardian 控制 socket。
  没有 socket 返回 `None`，视图变成 unknown，而非检测到业务 daemon 不存在。
- `DesktopRuntimeManager::acquire_project` 在后端返回 `held=false` 时仍返回
  `Ok(())`；`useRuntimeLease.ts` 无从区分该结果与真实持有租约。
- 壳的 `bridge.rs::ShellIdentity` 和握手目前传 App run id、代际与期望构建；
  app-server 的 `shell_authorization.rs::ShellAuthorization` 已支持
  `root_authorizations`，但壳上述字段集未携带它。
- `lifecycle/migration.rs::MigrationPlan` 已有判定及 watchman 退役能力，当前
  搜索到的直接使用集中在测试，不能视为完整生产迁移已接通。

相关现有模块的历史测试只能证明模块自身，不证明最终 App 的默认接入已完成。

## 3. 方案选择

| 方案 | 收益 | 风险与结论 |
|---|---|---|
| 仅在 sidecar 设置 `YI_AGENT_BRINGUP=managed` | 改动最小，适合隔离实验 | 不解决存量占锁、租约时序、root 授权与旧自愈；不作为正式修复 |
| 接通 managed 项目准备闭环 | 复用已有协议与安全边界，默认行为可验收 | 需要统一入口及迁移；采用 |
| 维持 unmanaged，只改文案 | 消除错误提示 | 不提供已确认的退出保证；仅作为显式暂用分支 |

不将 `default_for_build` 全局改成 Managed。受管桌面、TUI、headless 入口显式
选择 `for_managed_frontend`；手工 daemon 和明确的 embedded 内部场景保持原语义。
生产选择不得依赖用户从终端注入环境变量。

## 4. 项目准备与所有权闭环

### 4.1 职责及管理入口

桌面壳持有 App run id、项目租约、心跳、已验证配套产物及退出屏障。app-server
负责执行带构建检查的 runtime bring-up 与业务接线，guardian 负责生命周期与进程
归属。Tauri 继续只依赖轻量 lifecycle crate，不链接 supervisors/store/SQLite。

壳经现有本地 stdio bridge 增加窄管理操作：准备项目、更新本地会话的 root 授权。
管理调用由壳协调，不能走业务 RPC 的自动重连重放队列。bridge 固定本地产物、
规范 scope 与 App 身份，禁止 WebView 提交 executable、build id 或授权 capability。
relay、websocket、SSH 槽位不接收本地壳授权，也不能触发本机迁移。

准备操作只启动或观察 runtime，不 attach root、不恢复 worker、不获得 App 的
可续租凭据。启动时使用现有协调锁、产物 pinning、schema/协议检查及 bootstrap
FD 规则；不能裸执行 `internal-runtime guardian`、只传环境变量冒充受管角色，
也不能将一次性 helper 的退出误当成完成生命周期交接。

### 4.2 顺序与成功条件

同一 canonical scope 的准备去重，共享一个进行中的结果；不同项目不互相阻塞。

1. 壳登记项目已打开，解析 scope、instance home，验证配套产物。
2. 调用本地准备操作，先探测业务 daemon 与 guardian；存在 unmanaged daemon
   则返回选择状态，不另起第二个 daemon，也不静默借用。
3. 无现有 runtime 时，经统一 bring-up 启动 guardian 和 daemon。首客户端窗口内
   不调度任务或运行插件；就绪回包须匹配 scope、构建和 daemon 代际。
4. 壳直接向 guardian 取得 App 租约并启动心跳，确认该租约实际存在。
5. 壳使用该租约请求绑定 root、scope、代际及 App run id 的非续租授权，通过本地
   bridge 更新 app-server 会话授权。沿用已有 root 授权协议，不另造 owner 体系。
6. app-server 在授权对应代际 attach root；业务 daemon 验证授权及 live owner，
   attach 成功后该 scope 才允许业务执行。

不能依赖 React effect 和首次 `thread/start` / `thread/resume` 恰好按顺序完成。
创建/恢复可执行 root、turn、委派及插件启动入口均等待同一 scope 的准备屏障；
静态历史查看与只读状态查询不因此创建租约。用户尚未提交的输入可保留为草稿，
不在准备完成后偷偷自动发送。

### 4.3 结果与前端状态

项目获取命令不再使用 `Result<(), _>` 表达全部状态。返回判别结果，包含规范 scope、
准备 attempt、状态、可读原因及安全重试类别；不向 WebView 返回任何授权凭据。

| 状态 | 权威含义 | 前端行为 |
|---|---|---|
| `starting` | 同 scope 的准备进行中 | 等待/显示进度，不标记已获取 |
| `managedReady` | 壳租约成立，代际与 root 授权接线已确认 | 才加入 owned，放行业务 |
| `legacyChoiceRequired` | 已观测到 unmanaged runtime | 提供显式暂用或迁移预览 |
| `blocked` | 兼容性、身份、路径或迁移条件不满足 | 显示具体原因，不降级 embedded |
| `unknown` | 无法核实或应答丢失 | 只读对账，不推断失败/成功 |

生命周期持有集合和业务 ready 状态分开：步骤 4 成功、步骤 5/6 失败时，壳仍必须
记录并能释放已有租约；不能因尚未 ready 而使其从退出屏障消失。放弃项目准备时
有界释放已取得的资源；尚无租约的首次启动依既有首客户端超时规则自行退出。

`useRuntimeLease` 不再乐观地把未持有结果当成成功。迟到结果必须核对 scope、
准备 attempt 和当前使用需求；用户已离开且没有后台工作/插件需求时释放，不能
因已不显示该项目就遗失真实租约。可重试的启动暂态允许总预算内退避重试；
业务消息、spawn、迁移确认不自动重发。

### 4.4 sidecar 重启、代际变化及退出

app-server 重启不改变 App run id、不重复 register、不释放壳租约。重连后壳从
自己持有的租约和 guardian 当前代际重新提供适用 root 授权；新项目无需重启整个
sidecar 才能取得授权。过期代际 capability 不能复用。

guardian 启动端 bootstrap/lifeline 的实际语义必须由真实进程测试确认：启动它的
sidecar 退出不得误停仍服务活跃 App/TUI 的 runtime。若现有启动连接不满足此不变量，
须补齐交接或消除对启动者的永久依赖，不能以“租约在壳里”替代进程证据。

Cmd+Q 首先冻结新的准备、sidecar 重生和业务准入，再按既有全局预算并行释放。
准备与退出竞态中，迟到取得的租约必须撤销/回收；不得在 App 已退出后放行业务。
任何未核实的回收仍报告失败，不扩大既有强制终止权限。

## 5. 存量迁移与显式暂用

### 5.1 预览与授权

迁移独立于普通版本重启。无 guardian 的旧 daemon 不能使用需 guardian 租约的
重启确认入口。新增迁移预览/确认管理操作，复用现有 migration/retirement 模块。

预览只读，核对 daemon 身份、scope/instance、运行工作、内嵌或独立模式，以及
resident 循环、watchman、自动安装入口等复活来源。无法完整统计时明确展示未知。
预览令牌绑定发起壳、scope、旧进程身份、影响修订和目标构建；确认使用预先记录
的 operation id。身份、工作或目标变化使预览失效，取消不修改任何状态。

退役标记是 instance 级，而 runtime 是 scope 级。预览必须列出本实例其他常驻
登记及停止无人值守复活的影响；用户确认这一范围后才执行。不能用“迁移当前
项目”的按钮悄悄改变整个实例的自愈策略。

### 5.2 执行与中断恢复

确认后在受控协调流程内：

1. 重新核对预览证据，冻结本实例旧自愈入口并建立持久退役栅栏。
2. 核实 loaded job 与 plist 均属于本安装后，退役其 watchman；保留插件登记配置。
3. 只对已核实且可独立停止的旧 daemon 发有序停止，确认运行锁和旧执行实际退出。
4. 保留原数据库，经正常 managed bring-up、壳租约及 root 授权流程进入 ready。

`MigrationPlan::execute` 的 watchman 退役返回不等于 daemon 已停止，也不等于项目
已 ready；编排器必须单独观测上述全部步骤。LegacyEmbedded 要求宿主合作退出，
不可通过杀 TUI 迁移；其他实例/其他用户/不可核实进程返回人工处理。

失败记录具体阶段。退役后若新启动失败，保持旧自愈已冻结，显示“迁移未完成”及
显式重试入口，不自动恢复无人值守启动；重试先检查已完成步骤及当前进程状态。
确认应答丢失只按原 operation id 查询，不重新发出停止指令。操作记录不落租约
秘密，重启应用后仍能识别未完成迁移，不能虚报成功或默认再执行一次。

用户可明确选择暂用可兼容的旧 daemon，持续显示 unmanaged 例外。该选择仅允许
已有 legacy 能力，不授予 managed 身份、不绕过协议/schema 不兼容、不冒充自动
回收保证。未知身份不能作为“已兼容”的暂用路径。

## 6. 统一生产入口与插件需求

- App 项目 attach、repair、插件操作接入同一准备屏障。TUI/headless 的受管运行
  选择相同 managed 策略，自持租约与授权，不依赖桌面壳。
- `daemon::ensure_running` 保留给明确手工/legacy 用途；新受管业务不能从插件
  RPC 或 repair 绕过去。所有调用点需逐一列出用途并以测试证明策略。
- managed 接管后的 resident/watchman/自动安装路径继续检查退役栅栏；干净实例
  不能先无条件恢复旧登记、再等用户打开项目才切 managed。
- 插件需求绑定真实 live client，复用 `PluginDemandTable`；打开看板/实际请求
  获得需求，离开但仍有该客户端工作时继续持有，结束或 release 后撤销。配置不是
  demand，需求不是额外伪造前端租约。壳判断是否释放项目时须计入实际插件需求，
  不能只看会话列表；否则无会话的看板页会被误回收。
- 多客户端共享插件时只撤销离开客户端的 demand，最后一个需求撤销才停止对应
  插件；最后一个前端退出仍回收全部默认托管工作。

## 7. 状态展示、可观测性与开发模式

区分 starting、确实未运行、已识别 unmanaged、unknown、构建不一致和受管 ready。
只有识别到旧/unmanaged 身份才显示相应例外；unknown 不能再使用“旧版本启动”
作为通用解释。读取状态仍只读，不获取租约、不启动进程、不触发迁移。

前后端观测包含 scope、准备 attempt、App run id、daemon 代际、构建及失败阶段；
不得记录 credential/capability、API key 或 relay session。只读业务 Status 与
生命周期 Status 分开记录，便于解释“业务在运行但不受管理”。

bundle 校验失败或 dev 无已验证产物时给出 blocked/unknown 和原因，不拿壳版本号
当 daemon 身份、不自动换成任意本地可执行文件。开发者可显式运行已有 legacy
开发流程，必须标明例外，不能据此宣称正式 managed 接入验收成功。

## 8. 验收矩阵与发布门禁

以下均为待实施验收项，不是本轮已通过的测试。单测覆盖状态机和失败分支，真实
进程用例覆盖 socket、租约、父子进程与锁；最终独立 `.app` 再覆盖打包接线。

| ID | 场景 | 必须观测的结果 |
|---|---|---|
| A01 | 干净项目默认启动，无手工环境变量 | 一 guardian/一业务 daemon、实际壳租约、root 授权及 attach 成功 |
| A02 | 多窗口、同项目并发、软链路径 | 单一 canonical scope、去重准备，无重复客户端/daemon |
| A03 | 无 guardian，首次 acquire 或启动暂态 | 不虚报 owned/ready，完成后可进入 ready，不永久漏租约 |
| A04 | App 与 TUI 共用，重启最初启动者的 sidecar | App 身份与租约不变；其他客户端、guardian/daemon 不误停 |
| A05 | 快速切项目、准备中离开、准备中 Cmd+Q | 迟到授权不放行业务，无遗失租约，无退出后启动工作 |
| A06 | 两个客户端有工作，退出 App | App 工作实际回收，TUI root/worker 不受影响 |
| A07 | 最后一个客户端退出，旧 resident 仍有登记 | daemon/执行器/插件按既有预算退出；跨 resident 轮询无复活 |
| A08 | 旧独立 daemon 迁移预览取消 | 零停止/退役副作用，原工作仍在 |
| A09 | 确认迁移、状态变化、内嵌 TUI、外部 watchman | 陈旧预览拒绝；可迁移者保库接管；不可证明者不强停 |
| A10 | 迁移各阶段失败、应答丢失、应用重启 | 能解释阶段；同 operation 对账，无重复停止或假成功 |
| A11 | 实例有多个项目，迁移其中一个 | 预览列全实例退役影响；未确认不得写实例退役标记 |
| A12 | 插件安装/看板页无会话/repair/退出 | 入口不绕过准备屏障；live demand 可保留，配置不单独保活 |
| A13 | 新项目后置授权、代际切换、sidecar 重连 | 无需重启整个 App；拒绝旧 capability，无业务重放 |
| A14 | unknown/unmanaged/starting/不兼容 | 文案与权限相符，状态轮询不启动或续租 |
| A15 | 最终独立 App，红叉/Dock/Cmd+Q | 隐藏保留工作、恢复原状、真退出回收；不共享正式实例状态 |
| A16 | 数据保留及手工 CLI | 原历史/交付可读，手工 daemon 语义不变，受管入口无隐式 embedded 回退 |

实现提交需新增确定性回归，并运行相关 crate、Tauri、前端检查及项目规定的
`cargo fmt --all`、`just ci`（提交前及合并后）。不能用 mock 测试代替 A04–A11、
A15 等进程/原生验收，也不以横幅消失作为成功判据。不调用真实 LLM 即可覆盖
生命周期核心，真实任务冒烟若需要另行授权且记录成本与环境。

## 9. 交付与审阅边界

实施顺序为：准备/租约/授权闭环 → 存量迁移与退役 → 插件及跨前端入口收口 →
真实进程及最终 App 验收。各组可分 commit，但不能仅完成第一组就声称生产接入完成。
本补充不将已有底层模块完成记录删除；另在 desktop 进度登记“默认生产接入”未完成。
本文书面审阅通过后再编写实施计划，未获授权不改正式安装包、不停止当前 daemon。
