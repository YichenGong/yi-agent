# 子 Agent TUI MVP 设计

**日期：** 2026-08-10

**状态：** 等待书面规格确认

## 目标

在现有 TUI 中交付第一个用户真正可运行的持久化子 Agent 垂直闭环。用户只需像
平时一样在对话中描述工作，前台应用 Agent 可以把任务委派给一个子 Agent，TUI
实时展示子任务进度，并提供内嵌的接受、返工和拒绝审核卡片。子 Agent 必须在隔离
的 Git worktree 中工作，并以真实 commit 向直接父任务交付。

日常开发流程不得要求用户使用 CLI 控制命令，也不要求记忆 `/delegate` 命令。
CLI 和 Slash 命令保留为辅助诊断和恢复手段。本 MVP 只调整实现顺序，不缩减最终
范围：完整任务树、定时调度、两级委派，以及其余 runtime 验收标准仍须在垂直闭环
跑通后继续完成。

## 主要用户流程

1. 用户启动 `yi-agent`，进入现有 TUI。
2. 如果本地 runtime 不可用，TUI 显示明确的启动确认。用户确认后，无需离开 TUI
   即可启动 runtime；用户拒绝后，当前对话仍可作为不带委派能力的普通单 Agent
   对话使用。
3. 用户输入正常请求，例如：“实现登录流程，如果合适就把其中一个明确部分交给子
   Agent。”
4. 前台根 Agent 自行决定是否调用 `spawn_agent`，用户不需要学习委派命令。
5. 对话中出现简洁的子任务卡片；用户继续聊天时，卡片根据 daemon 事件实时更新。
6. 子 Agent 产出有效 commit delivery 后，卡片转为审核卡片，展示 diff 和验证
   证据。
7. 用户直接在卡片上接受、用自然语言要求返工，或拒绝交付。确认接受后，只集成到
   根 Agent 的 integration worktree，绝不写入用户 checkout 或 `main`。

`/agents`、`/agent`、`/events`、`/diff`、`/review`、`/accept`、
`/rework` 和 `/reject` 继续用于高级检查和恢复，但不是主要使用路径。

## 交互模型

### 对话原生委派

现有前台应用 Agent 继续负责可见对话和 provider 流式输出。TUI 会话初始化时，
它挂接到 daemon 中的一个持久化根任务，并获得任务级 `spawn_agent`、
`send_message` 和 `wait_agent` 代理工具。这些工具通过 typed IPC 调用 daemon，
不能自行伪造调用方任务、会话或 capability 身份。

Agent 根据用户的自然语言请求和系统指令自行判断是否委派。TUI 不额外调用一次
LLM 做意图分类，也不自行解释任意用户文本。这样可以沿用正常的工具选择机制，并
避免出现第二套委派事实来源。

如果根任务挂接失败，TUI 显示具体 runtime 错误，同时保留普通单 Agent 对话能力。
它不得假装委派成功，也不得创建未被 runtime 跟踪的子任务。

### 内嵌任务卡片

runtime 任务状态以结构化 history cell 展示，而不是以普通文本注入 Agent 对话。
运行中的简洁卡片包含：

- 稳定的任务短 ID 和父任务关系；
- 简短目标；
- 状态和 attempt 编号；
- 已运行时间和当前等待的资源；
- 最近一次有效进度或错误；
- 展开详情的提示。

卡片以任务 ID 为键原地更新；每个进度事件不得追加一条新的聊天消息。展开后显示
worktree/branch、contract、mailbox 摘要、最近事件、预算和权限状态。runtime
cell 是用户可见的审计信息，但不会追加到根 LLM 的对话上下文中。

第一版 MVP 使用对话 cell 和可选的展开卡片，不做独立侧边栏或管理页面。这样可以
复用当前 TUI 的 history/cell 模型，并兼容较窄的终端宽度。

### 审核卡片

子任务进入 `AwaitingParentReview` 后，卡片展示：

- 固定的 delivery ID、base commit 和 head commit；
- branch 和 worktree 身份；
- 变更文件摘要及 diff 入口；
- 验证证据和已知限制；
- `A 接受`、`R 返工`、`X 拒绝` 和 `D Diff` 操作。

只有审核卡片获得焦点时，`A`、`R`、`X` 和 `D` 才会生效，因此不会干扰普通文本
输入。接受前先展示准确 commit、目标父 branch、相关 worktree 和操作范围。返工
会让普通输入框进入审核反馈模式；拒绝同样要求输入非空原因。按 Escape 可退出这些
模式且不产生任何状态变更。

确认 token 必须绑定当前任务、delivery、父集成目标和具体操作，并且会过期。任务
状态或已审核 HEAD 发生变化后，daemon 必须拒绝旧 token，TUI 随即刷新卡片。

### Runtime 连接状态

状态区展示 `runtime: connected`、`starting`、`disconnected` 或
`resyncing`，连接成功时同时显示 resident 和 queued 数量。在 TUI 内启动 runtime
始终是用户明确确认的动作。不能仅因为 Agent 尝试委派，就静默启动 daemon。

TUI 持有带版本号的事件订阅，并保存最近的持久化 cursor。事件只更新本地只读
projection。重连或收到 `ResyncRequired` 时，TUI 丢弃本地 projection，并用
daemon 最新 snapshot 完整替换；不得在本地推断任务状态变化。订阅任务必须运行在
渲染/输入循环之外，避免缓慢或不可用的 daemon 阻塞键盘输入和 Agent 流式渲染。

## Runtime 与应用边界

### 根任务挂接

一次 typed attach/start 请求负责创建根任务，在第一轮开始时记录自然语言目标，并
返回不透明的任务级 capability 集合，供前台 Agent 工具使用。请求携带由 TUI 生成
的幂等键，传输失败后重试不得创建重复的根会话。

应用侧 root adapter 向 daemon 报告生命周期、用量、权限等待、安全检查点、子任务
等待和完成状态。SQLite 与 daemon supervisor 始终是任务状态的权威来源；TUI
history model 只是 projection。关闭 TUI 或连接断开时，必须触发现有 pause/drain
协议，不能把仍在活动的根任务标成成功完成。

### 权限通道

前台根任务继续使用现有 TUI 权限交互。daemon 托管的子任务通过 typed IPC 把权限
请求发送到同一个可见交互队列。每张卡片都包含任务 lineage 和请求的 tool/input，
让用户能够区分根任务与子任务请求。

TUI 通过 `ResolvePermission` 发送决定；daemon 先持久化决定，再唤醒等待中的
worker。安全保留操作不得由父 Agent 代替用户批准。`--yolo` 保持现有的显式语义，
不能因为 daemon 或子任务在后台运行就自动启用。

## Git Worktree 所有权

用户 checkout 只作为已选定 commit base 的只读来源。coding session 要求 Git
checkout 干净。runtime 创建以下结构：

```text
用户 checkout（runtime 永不写入）
  根任务 integration worktree 与 branch
    子任务 delivery worktree 与 branch
```

根 worktree 从用户 checkout 记录的 HEAD 创建。子 worktree 从直接父任务记录的
干净 HEAD 创建。branch 名称和路径只能由 daemon 生成的 session/task ID 派生，
不能使用原始目标文本。daemon 必须先持久化 repository root、worktree path、
branch、直接父 branch 和 base commit，再启动相应 worker。

每个 Agent 使用自己持久化的 workspace path。内置文件系统和 shell 工具、恢复
检查、Git 证据收集和嵌套委派都必须使用同一路径。不得把 daemon 的全局配置目录
复用为所有 worker 的 workspace。

如果源 checkout 不干净或不是 Git repository，TUI 在任何 worker/provider 副作用
发生前禁用 coding delegation，并给出可操作的说明；普通只读聊天仍可使用。
runtime 不得 stash、复制、reset、强制移除或写入用户 checkout。

## Commit Delivery

非根 coding worker 成功结束 Agent turn 后，worker factory 通过受信任的 worktree
service 检查其专属 worktree。有效 delivery 必须满足：

- worktree 干净；
- 当前 branch 与预期子 branch 一致；
- HEAD 与记录的 base 不同；
- 记录的 base 是 HEAD 的 ancestor；
- commit 在检查时仍可从精确 HEAD 到达；
- task contract/runtime 收集了非空验证证据。

factory 随后报告真实 `DeliveryReport`，其中包含固定 commit、base、workspace
lease 和验证证据。对于要求 commit 的 coding child，不得报告
`CompletedWithoutDelivery`。worktree 脏、缺少 commit、branch 不匹配或 ancestry
无效时，任务必须以包含 worktree 证据的持久化失败结束，不能伪装成成功的空交付。

根任务完成的语义不同：它的 integration branch 是 session 结果，不会自动合并到
用户 checkout。TUI 显示该 branch/worktree，供用户后续明确验证。

## 审核与集成

TUI 卡片操作和 Slash 兜底命令编码为相同的 typed `Review` IPC 请求。它们只能作用
于 `AwaitingParentReview` 当前指向的 delivery；过期 delivery 或发生变化的 child
HEAD 必须被拒绝。

接受包含两个不同的持久化事实：

1. 本地用户批准固定的 delivery；
2. 受信任的 daemon integration service 使用 `merge --no-ff` 把该精确 commit
   合并到直接父任务 worktree，并记录成功的验证证据。

只有两者都成功后，任务才能进入 accepted。单纯批准只能唤醒或通知直接父任务，
不能宣称已经集成。合并前一刻，integration service 必须重新验证父任务所有权、
记录的 base、父 worktree 干净状态、子 worktree 干净状态，以及固定的 child HEAD。
随后运行 contract 定义的 MVP 验证命令，至少包含 `git diff --check`。

合并或验证失败时，必须记录 Git 证据并保留相关 worktree。成功产生的 merge 不能被
静默 reset。返工先持久化反馈，再通过受控 admission 路径启动 successor attempt；
只有 admission 结果不再含糊后，才恰好一次地交付持久化反馈。如果父 HEAD 已变化，
successor 从新的父 base 创建全新 worktree，并保留旧历史。拒绝会记录原因，并保留
未合并的子 worktree。

任何审核操作都不得把当前 feature branch 或 runtime integration branch 合并到
`main`。

## 恢复与失败语义

现有受控恢复 gate 仍是权威路径。daemon 重启后，workspace 身份、worktree 身份、
checkpoint 状态、已注册工具及 Git HEAD/status 必须完成 attestation，恢复的 worker
才能获得工具或调用 provider。

在实现垂直闭环之前，当前 review checkpoint 必须关闭两个已知正确性缺口：

- 即使持久化 `RecoveryRequired` 也失败，含糊的 rework fallback 仍必须释放或以
  持久化方式作废 SQLite 中的 `resident:*` lease；
- admission 后的 worker factory 启动失败必须在关闭 attempt 为 failed 的同时，把
  具体 factory error 保留在终止证据中。

MVP 会直接经过相同的 admission、rework 和 restart 路径，因此这两项是实现前置。

## 辅助控制面

Slash 命令覆盖所有 MVP 操作，并在卡片已经不可见时提供恢复入口。现有 CLI 命令
继续服务于自动化、诊断和测试，但普通 TUI 开发不依赖 CLI。生成式帮助从同一份
command metadata 同时说明卡片快捷键和 Slash 兜底命令。

## 端到端验证

确定性的 MVP 测试使用临时 Git repository、脚本化 mock provider、真实
daemon/IPC stack 和 TUI application reducer；绝不调用真实 LLM API。测试必须证明
以下流程：

1. 初始化并 commit 一个干净的用户 checkout；
2. 打开 TUI，并在其中明确启动/挂接本地 runtime；
3. 提交普通自然语言 coding 请求；
4. root provider 选择 `spawn_agent`，随后等待；
5. 对话中出现内嵌子任务卡片，并在不污染 LLM 上下文的情况下持续更新；
6. child 只写自己的 worktree，完成 commit 后结束；
7. 卡片转为包含真实固定 delivery 的审核卡片；
8. 接受当前聚焦卡片后，该精确 commit 通过 merge commit 集成到根任务
   integration worktree；
9. 用户 checkout 及其 branch/HEAD 保持不变；
10. SQLite 记录批准、集成证据、终止状态和已释放的 resident 所有权；
11. daemon 重启并重新挂接后，TUI 从持久化状态替换 projection，且不会重放已经
    接受的 delivery。

独立的确定性路径还要证明：自然语言返工意见会到达 successor attempt，拒绝后未
合并 worktree 会被保留。失败测试覆盖：用户拒绝启动 runtime、daemon 断线/重同步、
源 checkout 脏、child 没有 commit、已审核 HEAD 变化、merge conflict、验证失败、
权限拒绝和 recovery conflict。渲染测试覆盖窄终端与常规终端宽度、审核卡片焦点
操作以及输入隔离。

## MVP 完成边界

只有正常 TUI 对话流程真正经过 real daemon 和真实 Git 操作、确定性端到端测试
通过，并且 core/store/tools/application 的聚焦测试套件串行通过，才能宣称 MVP
完成。直接调用 coordinator 的单元测试或只覆盖 Slash 命令的流程只能作为辅助
证据，不能替代 TUI→Agent→daemon→child→Git 的端到端测试。

以下内容仍属于完整子 Agent runtime milestone，但不阻塞第一个可运行垂直闭环：

- 独立的完整任务树侧边栏和更丰富的导航；
- root→child→leaf 两级端到端委派与直接父任务集成；
- 使用同一 worker pipeline 执行的自然语言定时任务；
- checkpoints 1–20 的其余审计项；
- strict Clippy 和项目管理文档更新；
- 最终用户验证及明确授权的 merge 决策。

任何 MVP 操作都不得把当前 feature branch 或 runtime integration branch 合并到
`main`。
