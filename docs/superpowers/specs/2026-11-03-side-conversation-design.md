# `/side` 与 `/btw`：桌面、手机、TUI 临时旁支会话设计

日期：2026-11-03
状态：设计基线，已进入 implementation plan 阶段（按用户要求；尚未实施）
范围：共享 core/runtime、tools、app-server、桌面与手机共用前端、原生 TUI。仅规格，不包含功能实现。
参考：本地 Codex `67f0994b53`；初始调研基线 `1b97509c`，文档合入前已与 main `810d789c` 复核。该基线新增的严格恢复入口会提升 partial，不能替代本规格要求的无副作用冷快照读取。

## 1. 目标与核心语义

用户在主任务执行期间，能够基于已有上下文另外问问题、连续追问，然后回到主任务；既不停止主任务，也不把旁问变成主任务的新指令。

三端必须满足同一组不变量：

1. `/side [问题]` 创建或打开当前主会话的临时旁支；`/btw [问题]` 是完全同义的别名，不是单轮快捷模式。
2. 主会话空闲、运行中、等待审批时都能进入旁支。创建、查看、返回、停止旁支均不得取消或恢复主 turn。
3. 新旁支继承创建时的一次安全上下文快照。旧上下文只是参考材料；只有旁支边界之后用户提交的内容是当前任务。
4. 旁支可多轮对话；每个父会话在同一个宿主进程内最多一个打开的旁支。不允许旁支再创建旁支。
5. 旁支回答、工具结果、错误、用量、插话和取消令牌均与主会话隔离。不自动回写或总结注入主会话。
6. 旁支是临时的，不进入普通会话列表、搜索、置顶、看板、组长管理与 `resume --last`；宿主退出后不能恢复。
7. 返回主会话仅切换视图，保留旁支；关闭旁支取消其运行并丢弃内容。两者必须是不同动作。
8. 本期旁支严格只读。需要修改文件、执行 shell 或委派任务时，提示用户返回主会话明确发起。

“不会打断”指不主动取消、暂停或替换主任务，不保证两路请求在供应商限流或宿主并发配额下完全不竞争吞吐。“临时”不是安全擦除或供应商零留存承诺；本地不持久保存旁支正文，模型请求仍受现有供应商数据策略约束。

## 2. 参考实现与当前项目事实

### 2.1 Codex 中的实际行为

以下路径相对 `/Users/gongyichen/Documents/TechnicalStuff/projects/OpenSource/codex`：

| 事实 | 代码证据 |
| --- | --- |
| `/side` 与 `/btw` 使用同一 dispatch，支持无参数与带问题 | `codex-rs/tui/src/chatwidget/slash_dispatch.rs` 的 `request_empty_side_conversation` 及 `SlashCommand::Side \| SlashCommand::Btw` 分支 |
| `/btw` 在默认命令列表作为别名隐藏，显式搜索时可见 | `codex-rs/tui/src/bottom_pane/command_popup.rs::ALIAS_COMMANDS` |
| 运行中创建不向父会话提交输入 | `codex-rs/tui/src/chatwidget/tests/side.rs::side_aliases_request_forked_questions_while_task_running` |
| 临时 fork、独立边界提示、继承历史仅作参考 | `codex-rs/tui/src/app/side.rs::side_fork_config`、`SIDE_BOUNDARY_PROMPT`、`SIDE_DEVELOPER_INSTRUCTIONS` |
| 支持切换保留与显式丢弃，父任务状态单独提示 | 同文件 `toggle_side_conversation`、`discard_side_thread`、`SideParentStatus` |
| 模型继承上下文，但 UI 不重放父历史 | 同文件 `install_side_thread_snapshot` |
| 不允许嵌套、重命名，禁止子 agent；默认非修改性探索，但允许边界后的明确修改请求 | 同文件提示词与 `codex-rs/tui/src/chatwidget/tests/side.rs` |

**有意差异：** yi-agent 首版不允许在旁支中修改，即使用户明确请求也不放宽。Codex 的“明确请求后可修改”与主任务共享工作区存在并发写风险；仅提示词也不是可靠的能力边界。本期选择可在三端一致测试的严格只读工具集。关闭/保留使用明确命令与按钮，不照搬 Codex 的 Ctrl+C 退出手势。

### 2.2 当前项目的接入约束

以下路径相对本仓库：

| 事实 | 当前代码位置与设计影响 |
| --- | --- |
| 持久 `/fork` 拒绝运行中与 awaiting approval | `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread/lifecycle/delete.rs` 的 `thread/fork`；不能直接复用 RPC 或放宽它的判定 |
| 已有共享 session 句柄与 provider 安全前缀 | `yi-agent-rs/crates/yi-agent-core/src/agent.rs::{Session, Agent::session_handle, fork_prefix_len, forkable_messages}`；复用尾部裸 tool-use 裁剪 |
| `ThreadSession` 当前未暴露 core session 句柄 | `yi-agent-rs/crates/yi-agent-app-server/src/session.rs::ThreadSession`；需发布只读快照来源与有效配置快照 |
| `Agent::start_run` 内部 spawn 后未返回任务句柄，且每次 run 重置 token | `yi-agent-rs/crates/yi-agent-core/src/agent.rs`；只 abort 外层消费者或提前取 token 均不能保证结束本轮 |
| 普通 driver 会 touch、标题生成、落盘、刷新 MCP/skills | `yi-agent-rs/crates/yi-agent-app-server/src/server/driver/turn_driver.rs`；旁支不能伪装成普通 thread |
| TUI 直接驱动 core，主 control 在 idle 接收，运行 select 另有入口 | `yi-agent-rs/crates/yi-agent/src/main.rs::run_tui_agent`；旁支调度不能等主 turn 结束 |
| TUI 审批先于输入路由，Enter 可直接批准 | `yi-agent-rs/crates/yi-agent/src/tui/app.rs::handle_key`；需显式审批/命令编辑模式，不能仅新增 slash 分支 |
| 桌面运行中支持插话，手机仍是 Stop-only | `desktop/src/components/MessageInput.tsx::{runSlash, handleSendClick}`、`desktop/src/App.tsx::onSlashCommand`；手机需独立旁问入口及定向命令例外 |
| 通知有订阅窗口；协议帧上限 1 MiB；scope 为 Observe/Control/Admin | `yi-agent-rs/crates/yi-agent-app-server/src/{broadcast.rs,protocol.rs}`；按父订阅路由、分页，不能杜撰 Read scope |
| 图片模型输入使用 `source`，`path` 仅作来源定位；文档清单是文本 | `yi-agent-core/src/message.rs::ContentBlock::Image`、`yi-agent-llm/src/{anthropic,openai}/types.rs`、`yi-agent-app-server/src/attachments.rs::prompt_with_attachments`（均在 `yi-agent-rs/crates/` 下） |
| 工具有来源/只读元数据，但 ToolsContext 的 roots 是固定 Vec | `yi-agent-core/src/tool.rs`、`yi-agent-tools/src/context.rs`；元数据不等于动态授权或安全文件打开 |
| read 工具目前有同步 IO，路径检查与打开分离 | `yi-agent-rs/crates/yi-agent-tools/src/fs/{read.rs,path_util.rs}`；不能宣称现成工具已保证硬超时、特殊文件安全或无路径竞争 |

这些是调研事实，不代表本 spec 中的新接口已经存在。引用优先使用符号名；相对 crate 的简写路径不表示仓库根存在同名目录。

## 3. 方案选择与非目标

| 方案 | 优点 | 问题 | 决策 |
| --- | --- | --- | --- |
| 共享临时旁支运行器，宿主分别适配 | 多轮、运行中可用，三端共享语义 | 需明确 snapshot、事件和生命周期边界 | 采用 |
| 单次旁问卡片 | 实现简单 | 无法连续追问，不等价于参考功能 | 不采用 |
| 扩展普通持久 `/fork` | 可复用存储与列表 | 污染列表，运行中 fork 风险；临时语义难保证 | 不采用 |

非目标：持久保存/导出/转正、自动合并结论、消息级分叉、旁支嵌套、同父并行多个旁支、旁支工具审批和写操作、TUI 整体 RPC 化、不同宿主进程间共享临时旁支、主任务运行中附件插话协议扩展。用户可手工复制答案；本期不新增“一键回写”。

## 4. 命令与状态契约

### 4.1 三端命令完全一致

| 输入 | 位于主会话时 | 位于旁支时 |
| --- | --- | --- |
| `/side`、`/btw` | 有旁支则打开；否则创建空旁支，不调用模型 | 拒绝嵌套，提示使用 `/back`；保留草稿 |
| `/side 问题`、`/btw 问题` | 打开/创建后自动提交问题一次 | 拒绝嵌套，不把命令文本发给模型 |
| `/back` | 无操作，提示当前已是主会话 | 返回父视图，保留旁支及其在途 turn |
| `/side-close` | 关闭当前父会话的旁支（若存在） | 取消并关闭当前旁支，返回父视图 |
| 普通消息 | 沿用各端既有主会话发送规则 | 空闲时开启旁支 turn；运行中可编辑但不发送 |

命令按首个完整 token 精确匹配；`/sidebar` 不等于 `/side`，`/back`、`/side-close` 不接受参数；前后空白忽略。问题文本允许换行，空白问题等同无参数。`/side close` 是问“close”，不是关闭。无会话或有效安全历史为空时创建失败；压缩后的有效摘要可作为上下文，不要求当前历史仍含 user 角色。无旁支时 `/side-close` 幂等成功。

`/side` 带参自动发送，不同于 `/fork [message]` 的草稿语义，帮助须明确。既有旁支 running 时，带参入口拒绝并保留原始命令，不能变成主插话或自动排队。旁支不支持 interject；停止后用户主动再发，三端一致。

本期不接受旁支上传新附件。源输入框有待发附件时，带参或无参的键入旁问命令均拒绝并保留文本/附件；**独立旁问按钮不消费输入框**，可以打开空旁支，父草稿和附件留在父视图。继承历史附件按 §5.3 处理。

### 4.2 生命周期

资源状态为 `Creating → Open(Idle | Running | Failed) → Closing → Closed`。Running 可附带 stopping 标志；本轮实际 core 异步任务及转发子任务未结束前不能进入 Idle 或开始下一轮。`Visible/Hidden` 只是客户端视图状态。生命周期不变量约束同一宿主；不新增跨进程旁支同步，外部进程直接改动持久父文件无法获得同宿主事件级通知，后续读取按资源失效/版本检测处理。

- 一个父会话在一个 app-server 进程中只有一个旁支；桌面与手机共享。TUI 进程管理自己的旁支，不共享其它宿主内存。
- 重复打开只返回既有 id，不刷新上下文；需要新快照时先关闭再创建。
- turn 失败保留已显示内容，可再次提问；关闭不可恢复。关闭确认使用 §8 的服务端条件校验，不能仅凭客户端旧的 running 状态决定。
- 返回、切换普通会话、手机后台、WebSocket 断连均不关闭。父视图显示“旁问运行中/可继续”。
- Idle/Failed 最后一次成功 open 或受理 turn、turn 结束后空闲 30 分钟自动关闭（取较晚时刻）；list/readItems/订阅/后台轮询不续期。UI 显示过期时间；用户明确“继续旁问”执行 open 可续期。运行中不计空闲过期。单次 turn 最长 10 分钟，超时终止本轮，保留失败态。
- 每宿主最多 8 个资源槽，Creating、Open、Closing 及尚未排空的隔离清理资源都占槽；不能靠反复 close/open 绕过配额，不自动驱逐别的父会话。
- 父 turn 完成/失败/停止只更新顶部状态。父成功 clear/delete/worktree 回收使旁支失效；操作失败则旁支不变。副作用提交前设置生命周期屏障，提交成功撤销对应 generation、拒绝迟到创建，失败则解除屏障；不要求等文件清理完才响应父操作。
- 父 compact 不刷新旁支；模型、effort、权限、roots 更新不追溯放宽。撤销 roots 按 §6 收紧；父被卸载不等于删除，应保留最小生命周期/授权来源，不能悄悄失去撤销能力。
- 宿主退出取消全部旁支并有界等待；崩溃不恢复。**连接断开但宿主仍在**按 §8 对账恢复；只有确认 side 不存在或宿主已换代，才清掉本地临时内容并提示失效，不当普通冷 thread resume。

## 5. 上下文、模型与附件快照

### 5.1 原子取快照，不暂停主任务

新增 core `SideContextSnapshot` 与 `snapshot_for_side`（拟定符号），只负责内存数据，不依赖工具、RPC 或磁盘。

1. 宿主发布 `EffectiveSideSource`：session handle、实际 provider handle、model/effort/完整 AgentConfig、有效 system prompt、cwd/roots、附件目录元数据、source revision 与父 lifecycle generation。
2. 活跃父：在 session 短临界区克隆，配合 source revision 前后校验；发布者必须在配置切换时标记更新中并同步发布，使读取者不混用旧 provider 与新 config。有限重试失败返回 source_changed，不能无限自旋。不跨 await 持锁，不持锁 IO。
3. 冷父：独立读取持久 core messages 和元数据，用与普通装配共用的**无副作用配置解析函数**解析该父现有模型覆盖和项目规则；不调用 resume、不登记普通 driver、不 touch/生成标题/连 MCP。读前后校验宿主父修订及 meta/log/partial 的文件身份/长度/修改版本与 lifecycle generation，若父同时装载或更新则改从新 source 重试；不能只检查 content_epoch（目前仅 clear 推进）。ThreadStore::load 当前可合流 partial，但 read_log 会容忍损坏；旁支须以严格只读解码路径拒绝损坏/不完整记录，避免把静默跳过的数据当完整快照，且不 promote_partial、修复写回或取得父 writer lock。外部写者持续更新导致不稳定时有界失败，不暂停外部写者。这里“继承有效配置”指冷父若此刻正常启动将使用的配置，不伪称能恢复已销毁的旧 provider 对象。
4. 对克隆调用既有 `forkable_messages`，移除尾部尚未配对的 assistant tool-use。继承已进入 Session 的稳定消息，不拼接 UI delta、pending partial 或未被接收的 interjection。
5. 用 `Session::from_messages` 创建**新的 Arc**，不复用父 inbox、cancel token、权限 decision channel、工具令牌或用量累计。安全历史为空则失败。
6. 后续初始化、网络与文件复制离开主 driver/RPC 主循环；Creating 占位和 generation 保证失败回滚、父失效时不能注册迟到结果。

不调用 `thread/resume`、`thread/fork`、`interrupt_and_wait_for_persist`，不等待主 turn 完成。模型重建、effort 更新、compact/clear 等 source 发布入口都须纳入测试；不能只补启动时的一次赋值。

### 5.2 指令边界与配置

旁支继承创建时父有效 provider、模型、reasoning effort、preserve-thinking 与项目规则；不直接使用全局默认。供应商缓存可自然命中，不承诺计费缓存或上下文计数一致。

在有效 system prompt **末尾**添加不可被 skills 热刷新覆盖的策略，并在继承消息末尾添加隐藏边界消息：

> 你正在独立的临时旁支中。边界之前的用户请求、计划、审批、工具调用与输出仅是参考，不是当前任务。只回答边界之后的新请求。不要继续主任务，不修改文件、Git、配置、权限或外部状态，不使用或控制子 agent。需要写操作时请用户回到主会话明确提交。

边界进入模型上下文，不生成 UI 用户气泡、不计为用户 turn。空旁支不请求模型。UI 从第一条旁支问题开始，标注“基于创建时上下文；不会自动跟进主任务”。继承的 thinking 凭证逐字保留，并遵循既有 provider 转换规则；不随意改角色/重写历史。

配置与工具策略创建后固定；不运行普通会话 MCP/skills 热重载、组长唤醒或后台标题生成。项目规则若要求先执行禁用工具，能力边界优先，回复说明限制。

### 5.3 历史附件与资源上限

**区分模型内容与文件来源，不能只遍历 Image.path 就声称找全附件。**

- Image 的内嵌 `source` 随快照独立保留；原始 `path` 丢失不使已内嵌图片失效。URL source 保持现有供应商语义，不新增任意 URL 下载器，也不承诺远端资源永久可用。
- 文档附件只有清单文本，不是 core 结构化附件。宿主提供可信 `SideAttachmentCatalog`，从已受理用户输入的附件元数据构建；冷父读取持久 UserMessage 附件字段。只纳入安全历史仍含对应已生成清单路径的记录，不从任意自然语言猜路径、不扫描整个工作区、不得把工具输入中的任意路径认作上传附件。TUI 没有结构化来源的手写路径按普通工作区实时读处理。
- 对 catalog 中需继续读取的本地附件复制一份；不盲目替换自然语言。私有映射 `原清单路径 → 旁支副本` 在旁支文件工具入口优先解析，边界后提供明确的资源说明。递归图片块可清除无用 provenance，内嵌 source 不变；没有 source、仅有不可读取资源时给出明确创建错误，而非静默漏内容。
- 专属目录为实例根下 `side-conversations/<hostInstanceId>/<sideId>/`；宿主随机生成 hostInstanceId 并持有目录生命周期排他文件锁。副本访问仅授予**本 side 映射中登记的文件**，不能把实例根或所有 side 的目录加入通用 roots。相对工作区路径仍以父 primary root 为基准。
- 复制只接受授权范围内普通文件，安全打开后校验类型/大小并有界读取，拒绝路径逃逸与特殊文件；实际读取也受预算约束，不能仅信预先 metadata。缺失的、确实需要文件读取的 catalog 附件导致创建失败。已内嵌图片不强制复制来源文件。
- 文件复制不在 session 锁或 RPC 主循环内执行；复制期间父 clear/delete 作废 generation 并清理。文件字节是复制时所读版本，不宣称与 session 原子一致。普通工作区文件仍是实时读取。
- 每旁支文件副本加内嵌二进制解码后合计最多 64 MiB；core 文本/工具历史和可见 transcript 分别最多 16 MiB，UTF-8/序列化开销计入各自预算；core 文本统计排除已计入二进制预算的图像 source 内容，不重复把 base64 当文本，实际内存仍受有限编码膨胀约束。创建超限拒绝并提示主会话压缩/减少附件，不自动截历史。后续轮次、工具结果、隐藏边界也计预算；需在进入无限队列前检查，不能先无限累积再判超限。
- 运行中越限时停止该轮、标记资源错误并保留已接受前缀；预留 4 KiB 控制/错误空间，保证终止消息可写入。工具输出在进入 Session 前有界，图片二进制与文本分开计数。新 turn 无余量则拒绝，不能借 compact 自动改变旁支上下文。
- 关闭逻辑立即隐藏内容；磁盘清理由 §7 的任务所有权约束决定。宿主启动只清理能取得排他生命周期锁的旧宿主目录，取不到锁跳过，不仅凭 PID/时间判断。临时 IO 是宿主管理行为，不授予模型写能力。诊断/缓存/持久 turn log 不记录完整旁支正文或密钥。

## 6. 只读能力与权限

共享 runtime 新增 `SideToolPolicy` 和 `build_side_tool_setup`，构建独立 registry，不从普通 registry 删几个危险名字后继续使用。

首版清单使用实际 builtin 名称：`read`、`read_document`、`view_image`、`glob`、`grep`、`web_fetch`、`web_search`。允许清单是能力上限，不强制启用；网络搜索沿用父生效的开关/凭据，关闭时不注册，不能因出现在清单就重新开启。登记还须满足 `ToolSource::Builtin` 与 `metadata.read_only == true`。未知工具默认拒绝；名称相同但来源不同也不能替换。网络只读指不提供修改 API 的能力，不保证网站自身没有日志等附带副作用。

明确不注册 write/edit、bash/shell、process、任意 MCP/plugin、skill 执行、worktree、spawn/send/cancel agent、浏览器自动化、权限或配置修改、mcp_reload。隐藏 schema 与执行拒绝双保险；伪造/幻觉名字只返回受控错误，不能绕到普通 registry。不以提示词、名字含 read、shell 过滤或 MCP readOnlyHint 充当能力边界。

**动态文件授权需要新执行适配，不是现有 checker 自动提供：**

- 实施最小侧用文件访问接口 `SideReadAccess`（tools）及 runtime 授权源，每次实际打开/开始扫描时求“创建时授权范围 ∩ 父当前授权范围”，保留原 primary root 解释相对路径。范围按路径包含关系求交，不是 Vec 元素相等比较；父新增根不继承，撤销作用于未来访问，无法收回已继承/已展示内容。
- read/read_document/view_image、glob/grep 均使用该入口；扫描过程不跟随越界链接，每次打开候选文件重新校验。仅在 registry 外先检查一个 path 参数不够。附件映射访问还须其原来源授权未撤销，副本不能成为撤销绕过。
- 采用基于目录句柄的安全路径打开，逐组件约束链接与最终文件类型，禁止检查后另开未经约束路径；支持的平台若不能提供该保证，该文件能力 fail closed，不回退字符串 starts_with。普通文件读取有字节/结果数/耗时预算；拒绝 FIFO、设备、socket。glob/grep 不扫描私有实例根。
- 不直接在 async executor 上运行当前同步文件解析。侧用适配把受限阻塞任务放入有配额的 blocking 执行通道，任务和文件句柄由资源 owner 追踪。blocking 工作每宿主最多 8 个、每 side 最多 2 个（包括停止/关闭后未结束者），等待队列也有界且可取消，超额拒绝，不在后台无限排队。超时可停止接收结果，但不谎称 Tokio abort 能杀掉已开始的 blocking IO。文档解析器不得调用被禁用 shell 或未受控子进程。
- 创建独立不可提权权限策略，YOLO 不扩大清单；额外根/提权直接工具拒绝，不弹审批。网络工具沿用现有网络策略、响应大小/超时，并仅暴露 builtin 实现，不引入浏览器/MCP 替代入口。

`ProviderTurnGate` 是 provider 请求准入而非父整轮锁。旁支不复用父**租约**；若宿主已有统一 gate 则按普通调度独立申请，不能为了即时回答绕过总并发/限流。无容量会等待并显示状态，等待可独立取消；有至少两路准入容量时，旁支应能在父请求未结束时开始，不人为串到父整轮完成。

## 7. 模块边界与运行流程

不新增 crate；按现有依赖方向分层：

| 层 | 拟定文件/接口 | 职责与依赖 |
| --- | --- | --- |
| core | `yi-agent-core/src/side_context.rs`、`agent.rs` | snapshot、边界、共享安全前缀；新增受管运行入口，保留普通运行 API 兼容 |
| tools | 侧用 `SideReadAccess` 与受限文件实现 | 动态授权、安全打开、有界 IO；不把 runtime 类型反向引入 tools/core |
| runtime | `yi-agent-runtime/src/side_conversation.rs` | `SideConversationRunner`、有效配置源、只读装配、独立 turn/资源 owner |
| app-server | `src/side_conversation.rs`、`src/rpc/side.rs` | 父子注册表、scope、事件转换/内存回放、多端发现/过期 |
| TUI | `yi-agent/src/tui/side.rs` 与 driver 适配 | 定向命令/事件、多视图、父状态；调用共享 runner |
| 前端 | `desktop/src/lib/sideConversation.ts`、`components/SideConversationHeader.tsx` | 临时 view、路由、对账；复用 ChatView 和消息组件，不直接把 side 通知喂给普通 Session.apply |

表内路径均相对 `yi-agent-rs/crates/`，app-server 行相对该 crate，前端相对仓库。

### 7.1 受管运行与关闭的实际保证

新增 core `run_managed_blocks → ManagedAgentRun`（拟定）：返回事件流、**实际 run_loop 的 JoinHandle/abort handle** 与本轮创建后的 cancel token，关闭/Drop 可 cancel + abort；不把消费者 task 当作 core task。普通 run/run_blocks 维持原兼容行为，共用内部构造但不顺带改其 drop 语义。

旁支资源 owner 持有 core 任务、provider 流、所有受限工具工作及临时目录 lease；不允许旁支路径再创建不被追踪的后台任务。特别是现有工具并发分支中的 ToolEvent forwarder（tokio::spawn，实际工具 future 由批次聚合等待）也要在 managed 模式收进任务集合，父 run_loop abort 后逐个取消/abort/join，不能依赖 JoinHandle 被 drop（那会 detach）。普通运行 API 保持兼容，但 managed 路径必须覆盖完整任务树。core 事件使用有界通道/预算准入，消费者退出使生产者停止；长 provider 等待和 backpressure 等待均可取消。拟定每轮事件队列最多 256 项且合计 1 MiB，单事件 128 KiB；文本 delta 可拆分，携带图片、工具参数/结果或 thinking 凭证的超大不可拆分事件须采用共享内容句柄传递并计入对应正文/二进制预算，不能截凭证或把完整 base64 塞入事件队列。不能无声丢掉权威事件，超限应受控停止本轮，给终态预留空间；预算发送器须主动触发本轮取消，不能依赖现有忽略 try_send 错误的调用点自行退出。provider 流累积区、工具参数/结果进入 Session 前同样校验 §5.3 预算，不能仅限制发给 UI 的副本。

`SideConversationRunner` 提供 start-turn、interrupt-turn、close，事件带 sideId/turnId。关闭先标记 Closing、拒绝新请求并撤销发布 generation，发送本轮 token 取消；最多等待 5 秒正常收尾，然后 abort **实际 core 异步任务**并等待其 JoinHandle 结束，切断 provider 流。`side/close` 不在 RPC 主循环里阻塞等待。

5 秒是转入强制取消的阈值，**不是所有 OS IO/文件删除在 5 秒内完成的保证**。逻辑 Closed、通知和响应可先完成；未结束的已开始 blocking 读取进入隔离清理队列，仅能读已授权文件，不能再发模型请求或 UI 事件。临时目录等最后一个 owner/工作句柄释放才删，配额与宿主文件锁保留到排空。清理失败记录不含正文的诊断并在下次启动回收；不得谎称已释放资源。实现测试必须证明 core 异步任务结束、阻塞读取受限且资源不会 use-after-delete。

interrupt 与 10 分钟超时使用相同的受管停止路径，但不关闭旁支：服务端先标 stopping 并立即应答已接收中断，异步结束后才发布本轮完成/Idle 或 Failed。再次发问前将仅该 side 的 Session 收敛至既有安全前缀（保留已接受用户问题和完整工具配对），避免强制 abort 留下悬空 tool-use；UI 保留已有输出并标明未完成。旧 turnId 的迟到工作只做清理，不写下一轮 Session/用量/UI。未取得供应商最终 usage 时显示未知/不完整标记，不伪造零消耗。

```text
主视图输入 /btw 问题
  → 先分类命令，不走主 input/interject/interrupt
  → 按 parent 建 Creating 占位（分配 sideId/generation）
  → 读取一致 EffectiveSideSource → 安全前缀
  → 后台装配资源映射 + 边界 + 只读 runner
  → 注册 Open → 客户端对账安装空 view → 提交问题一次
  → 独立受管运行 → 有界内存 transcript/事件 → 旁支视图
  → /back 仅切视图；/side-close 定向撤销/取消与清理
```

初始化使用异步 completion 回到宿主事件循环登记，不能 await 一个还需同一 RPC 主循环处理其它请求才能完成的操作。创建请求方尚未拿到 sideId 时，重复 open 共享同一 completion；可用 list 取得 Creating 的 sideId 后显式 close。只离开等待 UI 不假装已取消创建，完成后仍可通过 list 发现；父失效/宿主退出可撤销 Creating，迟到 completion 只清理。open 具有有界初始化期限（30 秒），超时撤销占位；超时清理中的资源仍占槽。

## 8. app-server 协议与桌面/手机同步

### 8.1 新增 RPC（拟定协议）

独立 `side/` 命名空间；参数 camelCase，响应 snake_case。同步补齐 rpc `Domain::Side`/domain_for/dispatch、capability、协议类型/TS bindings 和 method/scope 基线。initialize 的 capabilities 对象增加 `side_conversation_v1: true`，同一响应新增随机 `sideHostInstanceId`，只作临时宿主换代标识；各条本机/远端连接分别解析、保存能力与宿主 id（包括旁路连接握手），不借机器级 globalReader 的答案。旧 server 缺能力时前端隐藏入口，键入命令显示不支持，不回退 fork 或把命令发给模型。

| 方法 | 参数 | 返回/说明 | 最低 scope |
| --- | --- | --- | --- |
| `side/open` | `{parentThreadId}` | 摘要：side_id、parent_thread_id、status、snapshot_revision、last_seq、active_turn_id、expires_at、generation；原子 create-or-get，不发问题 | Control |
| `side/list` | `{parentThreadId}` | 0 或 1 个当前旁支摘要，含 Creating/Closing 状态 | Observe |
| `side/readItems` | `{sideId, snapshotToken?, cursor?}` | §8.2 一致分页快照；不使用 afterItemId/afterSeq 增量回放 | Observe |
| `side/turnStart` | `{sideId, text, clientRequestId}` | `{turn_id}`；一个 side 同时至多一轮；非空 text 最多 32 KiB UTF-8；拒绝额外附件字段 | Control |
| `side/turnInterrupt` | `{sideId, turnId}` | `{}`；已结束的目标轮幂等，不取消后续 turn | Control |
| `side/close` | `{sideId, expectedActiveTurnId}` | 字段必填，可为 null；与服务端当前活跃轮不一致时拒绝并返回最新状态供确认 | Control |

所有方法复用现有认证与 Observe < Control < Admin。**现有 scope 是宿主级，不凭空假设已有按工作区/每父 ACL。** 本期沿用普通 thread 读取/控制可见范围；父存在/生命周期和 scope 均需校验。若后续引入细粒度 ACL，side 必须复用同一授权入口。通知出口也不得发给未认证连接；不把订阅当认证。

Observe 可发现/阅读，但不能创建、续期、发问、停止或关闭。Control 端以同宿主普通 thread 相同的信任范围共享旁支。关闭后 tombstone 最多 5 分钟，仅存父关联、sideId/generation、已受理 requestId、turnId 和请求内容摘要 hash，不存正文；用于幂等与状态错误，不可用来恢复内容。tombstone 每宿主最多 1024 份，超额按最旧淘汰；淘汰后旧 id 返回 side_not_found，不能为了幂等无限保存，也不能据此重建。

`clientRequestId` 必须为规范 UUID 字符串（36 ASCII 字符），服务端 sideId/turnId 同样使用 UUID，所有长字符串参数均先验证；每个 side 生命周期去重且随客户端重试保留：同 id 同摘要返回原 turnId，不再调用模型；不同摘要 invalid params。32 KiB 文本按原 UTF-8 字节计算摘要；不隐式 trim 后比较。每 side 最多受理 256 个不同 requestId/turn，达到后拒绝新 turn 并提示另开旁支，既有去重仍可用，防无界幂等表。先查去重再查 busy，已完成的同 id 重试也返回原 turnId。关闭后 turnStart 重试返回 side_not_found（可带原 turnId/accepted 标记），绝不能重建/重跑。

close 使用条件校验防跨设备竞态：Idle 传 null；Running 确认提示后传所确认 turnId。状态已变则返回 close_conflict，客户端重新读取并按当前状态确认，不能自动 force 重试。服务端串行校验并进入 Closing；重复 close 对 tombstone 幂等成功。父失效/TTL/shutdown 是宿主强制回收，不需客户端确认。

open 与提交分离：客户端先订阅、安装 view/回放基线，再 turnStart，避免首 token 无落点。并发 open 合并相同 Creating；Creating 可由 list 返回已分配 sideId，客户端此时只能查看状态/close（expectedActiveTurnId=null），readItems/turnStart 返回创建中错误，open 成功才可安装可发送视图。source 更新和 close/open 按 parent key/generation 串行化，Closing 未结束时拒绝重建，不能留下两个 runner。

### 8.2 通知、回放、帧预算与发现

新增 `side/changed`、`side/itemUpdated`、`side/turnCompleted`、`side/closed`，共同字段为 `parent_thread_id, side_id, generation, seq`；turn 通知再带 turn_id。seq 是每个 side generation 内的**状态修订序号**，递增，每个已发布修订恰好一个通知，不拿 token 数或 item 序号代替。changed 携带当前摘要（包括 active_turn_id/status/stopping/expires_at）；itemUpdated 携带 item；turnCompleted 携带最终状态/错误/usage 与累计值；closed 携带 reason。受理 turn 时先原子记录去重键、turnId、Running 状态并发布 changed，再启动运行；完成时先 flush 所有 item 更新，再发布含终态的 turnCompleted，最后才允许下轮受理。itemUpdated 是幂等 upsert 的当前完整 item，可节流合并尚未发布的 token 状态（最多每 50ms 一次，结束前强制 flush）；不把丢帧当成文本 delta 成功。

- 独立内存 transcript/translator，复用 Item 判别形状；不含父历史/隐藏边界，不调用普通 thread translator 的持久副作用。前端 side reducer/adaptor 明确处理新通知，不能假装 Session.apply 已认识它。
- **每个 side 可见 Item 序列化最多 128 KiB**，长文本在翻译时按 UTF-8 边界拆成有稳定 id 和顺序的连续显示项；工具卡参数/结果展示有界，超限显示截断标记，core 保留内容仍受 §5.3 独立预算。图片二进制不进入 side 通知或回放，工具卡仅有描述/元数据；本期不增加可读任意 side 私有路径的 image/read 后门。模型使用图像不等于 UI 必须下载原图。
- 所有响应/通知序列化后的完整信封不超过 512 KiB，低于现有 1 MiB MAX_FRAME_BYTES，给 relay/转义留余量；分页按实际序列化字节分包，不只按条数。错误/用户输入展示也受单 item 上限。
- readItems 首次无 token/cursor，在注册表锁内捕获一致的不可变 transcript 索引与 status/usage/active_turn_id、accepted_requests（requestId→turnId）及 snapshot_seq，并包含创建时脱敏 effective_config（模型、effort、preserve-thinking、只读标记与工具名，不含密钥/完整 prompt）；大字符串使用不可变版本共享，不在锁内序列化 16 MiB。返回 `{items, snapshot_seq, status, usage, active_turn_id, accepted_requests, effective_config, snapshot_token, next_cursor}`；第一页携带状态/去重表/脱敏配置，后续可只返回 items/token/cursor/水位，也可重复同一快照元数据；不得重读当前父配置代替旁支固定配置。cursor 绑定 token，不能与其它 side/快照混用。token 每页成功读取后续期 60 秒、绝对上限 5 分钟，每连接/side 最多一个，每宿主最多 8 份，超限返回 busy；过期须重开快照，不能拼不同时间页。最后一页、连接断开或 side 关闭即释放快照引用；保留快照总字节独立受 128 MiB 宿主上限约束，不能仅限制 token 个数而允许旧 item 版本无限保留。
- 快照后仍可产生通知。客户端先订阅父并缓存该 side 通知，再分页收集；最后原子替换本地基线为 snapshot_seq，忽略 <= 水位的通知，将更高序号按连续顺序应用。同 item 新版本可替代旧版本，不能追加重复文本。出现缺号、缓存越限或快照过期，禁发并重新全量对账；不能简单取 max(seq) 跳过缺帧。通知缓存最多 256 帧/2 MiB，溢出丢弃缓存并标记需全量快照。
- side/closed 为终态，即便前序有缺号也立刻丢弃该 generation 本地内容及在途分页，不等待一个已不存在的快照。普通更新不得复活已关闭 generation。
- 所有通知以**父 threadId**为订阅过滤 key，负载明确 sideId。桌面 Feed::All 直接收到，手机把当前父 pin 在既有订阅窗口，不额外占 side 名额。
- 打开父或重连：先父订阅，再 list，再分页对账。list 空、确定 side_not_found 或父已不存在时清掉旧临时 view；仅网络失败则保留内存占位、标记未同步并禁发，不误报已关闭。宿主重启不复用随机 sideId。
- 连接存活也可能丢最后一帧；旁支可见且连接可用时每 5 秒 list 比较 last_seq；有差异或前台唤醒即对账。后台无需持续 poll，不续期。正常发问只在已同步态允许，busy 仍以服务端为准。
- 两端同时发问只接收一个 turn，另一端保留草稿。任一端关闭会通知另一端回父视图。closed reason 包括 user_closed、expired、parent_cleared、parent_deleted、host_shutdown、resource_lost；父 worktree 回收归 resource_lost。
- thread/listAll 不增加旁支，不改变父未读/排序；旁支状态/用量不复用父状态。用量从零累计，继承上下文不继承父历史费用；input estimate 单独计算，父 usage/subagent totals 不混入。全局供应商账单/宿主实际总消耗仍可包含这些真实调用，但不得保存旁支正文。TUI 共享这些预算与去重/状态语义而不实现 WS 分页；使用同一有界 transcript 翻译规则供 HistoryState 渲染，不能变成无界的独立转录实现。

### 8.3 错误与未确定提交

复用 unknown_thread、turn_in_progress、invalid params、insufficient scope 表达对应情况。新增错误通过 RpcError 统一分配无冲突码，`data.reason` 稳定区分 side_not_found、parent_not_started（无有效历史）、side_limit、snapshot_too_large、source_changed、side_closing、close_conflict、snapshot_expired、side_creating、request_limit、resource_limit。实现前核对全量错误码表，不猜数值。父因 cold 解析配置失败给明确错误，不降级猜模型。

失败不改变父状态。创建失败回滚；open 成功但发送明确失败则保留旁支，把**问题正文**放进旁支草稿并提示尚未发送，原命令只按提交快照清除，不能覆盖新编辑。网络结果不确定时保留同 clientRequestId/text 为“待确认”而不是可随手再发的新草稿：重连先用 accepted_requests 确认，或同 id 重试受理；在确认前不生成新 id。服务端明确未受理且不 busy 后才让用户重试；side 失效不自动向新旁支发送。

## 9. 桌面与手机交互

### 9.1 共用视图模型

保留普通 `ThreadStore.currentId` 作为父身份，新增 `activeConversation = main(hostKey, parentId) | side(hostKey, parentId, sideId, generation)`。hostKey 绑定连接注册表 profile 和握手 sideHostInstanceId；本机/远端项目不同宿主的 sideId、能力、token、去重表不可混用。所有异步操作捕获来源 RpcClient/hostKey，不能改用当前连接或 globalReader；相同宿主重连可重新绑定 transport，宿主换代清理旧 side。临时 store 不混普通摘要、IndexedDB/localStorage transcript、会话搜索或操作历史正文。

复用 ChatView、消息组件、Markdown 和只读工具卡，顶栏显示“旁问 · 基于创建时上下文”、父状态、只读、返回/关闭。旁支起始不重放父历史。返回后父滚动锚点、草稿、未发附件、实时进度仍在。

draft、发送锁、乐观消息、usage、错误、滚动锚点都按 conversation key 隔离。异步回包捕获 parentId/sideId/generation 和提交快照，不能用当前全局 view 清草稿。通知先按 namespace 分发，side 不进普通 ThreadStore.apply/自动建 view/缓存。旁支不调用父 markRead、resume、模型切换、右侧 agent trace/watch、进程管理、gitDiff 操作或附件上传；既有主后台订阅可以继续，数据不渲染成旁支卡片。外部“聚焦主会话/Git Diff”请求显式切回 main，不能悄悄对着旁支执行父操作。

Observe 端可通过 list/readItems 的“查看旁问”入口打开已有视图，不调用 open；创建/续期/发送/Stop/close 控件禁用。服务端仍执行 scope 校验。选择器改只读显示继承模型/effort/权限；旁支 `/config` 只展示脱敏 side 生效配置，`/usage`（TUI `/cost`）只统计 side。允许 `/help`、`/config`、`/usage`（TUI 为 `/cost`）、`/back`、`/side-close`；TUI 额外保留应用级退出命令/按键。拒绝 clear/compact/fork、配置/权限变更及其它管理命令并保留输入；帮助按场景过滤，别名不导致分组重复。

### 9.2 桌面

- 父空闲/运行中/等待审批均有“旁问/继续旁问”按钮；键入命令支持 Enter、Send、**确认执行的补全动作**，单纯 Tab 填入候选不自动请求模型。命令先分类，不调用 onInterrupt。
- 运行中白名单明确加入旁问命令，既有 clear/compact/fork 拒绝不变；默认建议只展示 side，搜索 btw 可见，help btw 指向 side。
- 旁支独立 Stop；返回不 Stop。顶部“主任务等待审批/需要输入”是回父链接，按父 UI 审批，不能把父 approval id 交旁支。
- Creating 显示等待；切回父不取消创建，完成后显示可继续入口；重复点击合并。关闭确认使用服务端返回的当前 turn 身份。

### 9.3 手机（必须同期交付）

现有运行中普通消息 Stop-only 保留；**旁问是显式例外**，不是开启手机全部插话。

- 父运行中保留原 Stop，另有“旁问/继续旁问”，点击不消费父草稿、不停主任务。
- 父输入框精确 `/side [问题]`、`/btw [问题]`、`/back`、`/side-close` 可显示专用执行按钮；普通文字/其它命令不能绕过 Stop-only。
- runSlash、handleSendClick 及所有禁用判定共用策略；不能外面加按钮而里面继续全拦截。等待审批也须有独立可达入口，不强迫用户先处理父审批。
- Enter 始终换行，不新增 Enter-to-send；点击命令按钮执行。旁支空闲有 Send、运行中是自身 Stop，可编辑但不发送。
- 返回/关闭为触控按钮，窄屏同一聊天区切换，不强制双栏。已有宿主返回手势只等价 back，不销毁旁支。
- 后台/前台、重连按 §8 对账，不调用 parent resume 来打开旁支；普通父内容的既有安全补齐流程不在本期重写。

本设计不整体推翻 `2026-11-03-desktop-mid-turn-input-design.md` 的手机普通发送约束。

## 10. TUI 接入

### 10.1 Driver 与事件隔离

新增定向 `ConversationEvent { target: Main | Side(sideId, generation), event }`。明确 multiplexer 合流；不能把 side AgentEvent 原样塞进单路 agent_rx，使父 Done/Cancelled 改旁支 running。

旁支独立 control/调度任务，与主运行 select 并行；创建直接读取已发布 EffectiveSideSource，不排到主重建型 DriverControl。主 input/interject/interrupt/工具 kill/decision 仍走主专属通道；side input/interrupt 走 runner。主 TurnLog/TuiSessionStore 只接主事件，side 不 append_turn/touch/promote_pending，不占父 writer lock，不新增可 resume thread。退出提示仍指真实主 session id。

### 10.2 UI、审批与按键

Main/Side 各持 HistoryState、InputLine、viewport、cost/statusbar、running、插话记账与错误。父工具/进程/agent 弹窗只在主视图可操作，进入 side 先收起父弹窗，不丢父任务。

- 命令先于普通消息投递判定，运行中不先 interrupt。顶部/底部标注旁问只读、父 running/awaiting approval/finished/failed；小宽度可裁剪，不遮输入。
- back 保留两套 history/draft/viewport；父后台输出只更新父 history。主权限请求在 side 只显示来源提示，不抢焦点，必须 back 才审批。
- **父等待审批时可进入旁问：** 主视图审批态按 `/` 显式进入独立命令编辑模式，显示“命令，不会批准”；保存原草稿。此模式只接收 side/btw/back/side-close/help/退出命令，Enter 执行命令，绝不落入审批默认允许分支；Esc 只退出命令编辑恢复审批。未处于命令模式时原审批数字/Enter 含义不变。side 视图按键绝不能经过父 pending_permission 分支。
- Esc 顺序：弹窗/补全/审批命令编辑优先关闭；无上述 UI 时仅 interrupt 当前会话活跃轮。side idle 不关闭/退出。返回用 back，避免混淆。
- Ctrl+C 保留应用两步退出语义：第一下显示确认，并仅取消**当前可见会话**的活跃轮（当前既有行为），不是返回也不同时停父；第二下执行全应用退出，取消两路。Ctrl+Q 与 `/quit` 是应用级直接退出，两路同样清理。退出键在审批/命令编辑态也可达，不能被字符拦截或管理命令禁用吞掉。
- 终端通知分别保留主/旁来源；后台父完成/审批仍通知。标题 running 取任一路仍运行，不因 side 完成掩盖父。主题、resize、CJK 换行、缓存上限对两份视图同样适用，不复制 renderer。

## 11. 故障、竞争与安全不变量

| 场景 | 必须行为 |
| --- | --- |
| 快照时父产出 tool-use/result | 锁内克隆 + 既有安全前缀；无悬空 tool-use，父照常执行 |
| 创建时父重建/冷装载 | source 版本与父 generation 一致；有限重试，不混 provider/config |
| 创建中关闭父/clear/delete | 先生命周期屏障，再提交作废；迟到结果清理，不留幽灵 side |
| 两端同时 open/send | 单 runner；一个受理 turn，另一个 busy 保留草稿；不串父插话 |
| 旧 turn interrupt / close 迟到 | interrupt 匹配 turnId；close 条件冲突拒绝，不误停新轮/父 |
| 关闭时工具阻塞 | 实际 core task abort/join；blocking 工作隔离追踪，目录延迟清理且占槽 |
| 断线提交结果不确定 | 同 requestId 对账/重试，不自动生成新请求或新旁支 |
| 流式通知缺帧/分页中更新 | 固定 snapshot_seq + 连续通知；缺号重新全量，终态直接关闭 |
| 父 roots 撤销/目录回收 | 未来访问收紧；副本不绕授权；worktree 回收使旁支失效 |
| 模型要求写/启动 agent | registry 缺失且执行拒绝，解释回父，不靠模型自觉 |
| side 视图父等待审批 | 来源提示；back 后审批，命令 Enter 不默认批准 |

只能实时只读观察工作区，不能保证与创建时工作区一致。“上下文快照”不是文件系统快照或 worktree 隔离。

## 12. 测试与可验证验收

默认全部用 mock provider，无真实 key。下列测试名均为**待实施验收目标**，不是本次文档已通过的测试。

### 12.1 core/tools/runtime

- `side_snapshot_trims_unpaired_tool_use_without_touching_parent`、`side_snapshot_is_independent_and_marks_history_reference_only`：父字节等价、Arc 不同、边界存在；空旁支不请求。
- `side_uses_effective_model_and_keeps_boundary_after_prompt_refresh`：活跃/冷父、模型/effort/compact 重建路径版本一致，thinking 凭证不变。
- `side_managed_close_joins_actual_core_task`：阻塞 provider 下 close 终止实际任务及工具 forwarder；只 drop 外层 stream 不算；token 必须属于 run 后本轮。中断后下一轮 Session 无悬空工具对，未完成旧工作不能污染新轮。
- `side_registry_rejects_mutation_even_with_yolo`：写/shell/MCP/plugin/skill/process/agent 不可达，来源与元数据双校验。
- `side_read_access_rechecks_roots_and_blocks_path_races`：范围交集、撤销、附件副本授权、链接替换、FIFO/设备、glob/grep 候选与 parser 路径都覆盖。
- `side_cancel_does_not_cancel_parent` 与反向取消；独立 token/inbox/event；`side_respects_shared_provider_admission`：两路容量可并发，一路容量等待不绕 gate、等待可取消。
- `side_inherits_inline_images_and_copies_catalog_files`：内嵌图源丢失不误拒、文档 catalog/映射、冷父恢复来源、越界/确需文件丢失/超额明确失败。
- `side_blocking_cleanup_keeps_lease_and_resource_slot`：已开始 blocking 工作不假称 abort，关闭逻辑可完成，文件/槽/锁延迟释放。
- 配额、预算（包括 core 内部通道/图片）、TTL、10 分钟 turn 与 5 秒取消阈值均用受控时钟/同步栅栏，不靠 sleep 猜竞态。

### 12.2 app-server

- `side_open_during_running_turn_does_not_interrupt_or_persist`：父 mock 阻塞中可创建；无父 interrupt/resume/fork，无新普通 meta/jsonl。
- `side_open_during_parent_approval_keeps_decision_routing`；`side_open_cold_parent_has_no_resume_side_effects`。
- `side_concurrent_open_returns_one_runner`、`side_request_id_deduplicates_turn_start`、`side_rejects_second_inflight_turn`；关闭后同 id 不重跑、256 上限。
- `side_events_follow_parent_subscription_without_polluting_parent`：多客户端/Observe、事件/usage/未读/普通列表隔离；多宿主连接能力/实例/事件/RPC 不串线。
- `side_reconnect_replays_current_items_without_duplicate_text`：重复、缺号、尾帧丢失、分页同时更新、快照过期/缓存溢出、终态打断分页。
- `side_frames_stay_within_serialized_budget`：长 Unicode/转义、工具输出/图像/大输入、512 KiB 信封上限与 128 KiB item 边界。
- `side_close_never_cancels_parent_and_drops_late_events`、`side_close_rejects_stale_turn_confirmation`：close/clear/delete/create、父操作失败和屏障回滚。
- Observe 不能 open/续期/send/stop/close；不伪造不存在的 per-workspace ACL。认证失败不能读通知；未知 side/host 重启按错误契约。
- TTL 不被 list/read 续命、8 槽包含清理中资源、Creating 超时及按 sideId 取消、严格冷读损坏/partial/外部写者竞争、文件复制失败、父卸载/回收、shutdown 均不泄漏无主任务。

### 12.3 桌面与手机

- canonical side/alias/help/精确 token；带参自动发送与 fork 草稿分别测试；缺 capability 不回退发模型。
- 桌面运行中 Enter/Send/确认补全不 interrupt；Tab 只填入；嵌套/待发附件/busy 拒绝保留输入；独立按钮不消费父草稿。
- 手机父 running 保留 Stop-only，旁问按钮/精确命令可用、Enter 换行；父 awaiting approval 也可直接旁问。
- 两套 draft/scroll/usage/error/Stop/审批隔离；side 不进父缓存/markRead/trace/watch/gitDiff/模型变更链路。
- 关闭确认竞态、另一端关闭、后台唤醒、网络失败与宿主重启区分、窄屏布局，覆盖 App/组件测试。
- 自动提交明确失败恢复正文；未确定提交保留同 id 待确认；迟到回包不清新编辑/另一 conversation 草稿，双击不重复调用。

### 12.4 TUI

- `tui::side::tests::side_command_while_main_running_does_not_interject_or_interrupt`。
- `tui::side::tests::background_parent_events_do_not_change_side_history_or_running`。
- `back_restores_main_draft_and_viewport`、`side_esc_cancels_only_side`。
- `parent_approval_command_mode_does_not_approve_on_enter`、`parent_approval_cannot_be_answered_from_side`：审批态可创建/返回，数字审批与模式 Enter 隔离。
- `side_events_never_reach_main_turn_log`、`quit_cleans_both_runners`：两次 Ctrl+C、Ctrl+Q、quit 和审批态退出。
- 主持久会话 e2e 检查 side 正文不进父 jsonl，主继续完成/resume 正确；ratatui TestBackend 覆盖窄终端、CJK、主题、resize/标题状态。

### 12.5 实施后的验证命令

```sh
cd yi-agent-rs
cargo test -p yi-agent-core --lib side_
cargo test -p yi-agent-tools --lib side_
cargo test -p yi-agent-runtime --lib side_
cargo test -p yi-agent-app-server --lib side_
cargo test -p yi-agent --bin yi-agent tui::side
cargo test -p yi-agent --test e2e_resume
cargo fmt --all
just ci
cd ../desktop
npx tsc --noEmit
npx vitest run
npm run build
```

实施提交前与合并后按项目规定各跑一次 just ci，实际门禁以 justfile 为准，不能用过滤测试替代全量。真实 API 冒烟可选，不是默认门槛。

人工必须覆盖：桌面启动长任务 → 手机旁问/追问 → 桌面看到同一旁支 → 手机返回不停止两路 → 仅停止旁支 → 主任务仍完成；TUI 独立跑同样流程，并验证父审批、Esc、退出/resume。还须覆盖从空闲冷父创建、两端关闭确认冲突。三端任一缺入口或停止串线不得整体完成。

## 13. 实施切分与项目进度

同源功能分四个顺序交付单元，三端验收后才整体完成：

1. core/tools/runtime：安全快照、有效来源、受管运行、只读访问/预算、资源 owner。
2. app-server：父关系/生命周期屏障、独立 RPC/事件、分页对账/能力声明、多端、TTL/清理。
3. 桌面/手机：共用临时 view、手机例外、命令/按钮/对账/审批来源。
4. TUI：定向通道与双视图、审批命令模式、后台事件/持久化隔离、按键与通知。

用户已要求进入计划阶段，实施依照 [implementation plan](../plans/2026-11-03-side-conversation-impl.md)，仍须代码评审与实际验收。本期仅对受管运行和侧用文件工具做必要公共底座修改，不顺带重构全部普通 driver/工具权限/TUI 客户端。

在 `docs/project-management/{yi-agent-core,yi-agent-tools,yi-agent-runtime,yi-agent-app-server,desktop,yi-agent-tui}.md` 登记未完成项并链接 spec；README 同步总数。判据为 §12 测试/命令，不把文档落盘标记为功能实现。

## 14. 书面审查与自检

审查按代码事实、跨模块契约、端到端场景反复进行；“无新发现”仅代表当前审查范围内无已知阻断，不等于实现已验证或形式化证明。

- 第一轮代码核对修正：Observe scope、core detached task 的实际终止、图片 source/文本附件区别、静态 roots 不能动态撤权、provider gate 租约语义、TUI 审批拦截。
- 第二轮协议与生命周期复查已修正：512 KiB 帧/分页、不可变快照水位与连续事件、尾帧补齐、close 条件确认、requestId 摘要/上限/未确定提交、capability/domain 注册、工具 forwarder 的任务归属。
- 第三轮端到端复查已修正：冷父严格读取/版本校验、Creating 取消/超时、父操作屏障/失败回滚、清理资源槽/lease、多宿主连接归属、Observe 只读入口、中断后下一轮的安全历史。
- 收敛复核：按桌面/手机/TUI 的创建、提问、返回、停止、重连、关闭、父审批与退出流程逐项复核，当前未发现新的阻断性设计问题。实现与性能/真机体验仍须按 §12 验证，不能用本条代替测试。
- 三端命令、附件草稿、返回/关闭、审批模式、后台父事件、退出与持久化隔离均有显式路径约束。
- 所有接口/测试为拟新增，设计已进入计划阶段；本次没有功能实现或新功能测试通过声明。
