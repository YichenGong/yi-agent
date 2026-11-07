# Desktop 子 agent 执行面板重新设计

日期：2026-11-07
状态：用户已确认示意方向；多轮 UX/协议复审意见已处理，实施计划已编写；尚未实现。
复审：[记录](2026-11-07-desktop-subagent-trace-redesign-review.md)
计划：[实施计划](../plans/2026-11-07-desktop-subagent-trace-redesign-impl.md)
分支：`feat/desktop-subagent-trace-redesign`

## 1. 目标与范围

把 desktop 右侧「子 agent」从平铺日志查看器改为「任务列表 + 执行时间线」。改善信息层级、窄栏阅读、实时跟随与下钻导航，不增加 RPC 方法、字段或第三方依赖。

保留三个 Tab、栏宽持久化与拖拽（260–720px，默认 360px）、现有消息投递与两阶段取消协议。此栏目前只在 desktop 显示，不新增手机入口，不修改 Git Diff/后台进程的内容布局。

必要的局部接线包括 trace 归属/快照合并，以及修正 `agent/trace/unwatch` 误停 children/process watcher 的服务端行为：只停止 trace watcher，其它 watcher 仍由既有 thread/delete、会话或服务器销毁路径清理。该修复不改变请求/响应形状，不改成轮询补救。

非目标：完整工具 stdout 获取、耗时/成本推断、搜索筛选控制台、任务重排、虚拟列表、主对话样式重构、独立 App 打包、补齐 daemon 任务目标元数据、改变同一 thread 的多客户端共享 watch 架构。

## 2. 当前事实与约束

- `desktop/src/components/SubagentRail.tsx`：厚边框卡片、英文状态、目标单行截断。
- `desktop/src/components/SubagentTrace.tsx`：默认摘要，二次点击才显示轨迹；发送失败仍会清稿。
- `desktop/src/components/ThreadDetailPanel.tsx`：提供外框、Tab 和返回列表。
- `desktop/src/lib/subagents.ts`：连续 assistant_text 合并，工具调用与结果独立成块；当前四个终态枚举未覆盖 core 的全部终态。
- `desktop/src/hooks/useSessionState.ts`：持有选择、读取快照和 watch，openDetail 每次重设单元素 detailStack，并不存在可用的父级历史栈。
- `desktop/src/hooks/useNotificationRouter.ts`：当前 trace 只核对 taskId，缺 threadId、连接身份与去重门禁。
- `yi-agent-rs/crates/yi-agent-app-server/src/rpc/agent.rs`：trace/watch 返回 `{rows, highWaterId}` 并替换同 thread 旧 watch；unwatch 当前还误停 children/process watcher。
- `server/watchers.rs` 的 children_for_thread 当前把 objective/lastStep 置为 None。真实数据缺失时必须降级为完整任务 ID 与状态；示意图中的目标/步骤不代表后端已提供。
- `yi-agent-core/src/subagent/trace.rs`：tool_result 含 name/is_error/summary；工具摘要在写入时最多 1024 字节，没有调用关联 ID、事件时间或可请求的完整输出。

因此：工具调用与结果保留为独立事件，结果显示自身工具名，不根据相邻位置或同名工具猜配对；只能称「结果摘要」。不由工具结果或 state_note 推断任务状态，不渲染伪造耗时/时间戳。「执行轨迹」不标榜实时连接健康；现有协议没有 trace 心跳或订阅失败通知，不能承诺探测后台静默停流。

## 3. 视觉结构

采用中性底色、细分隔线、少量圆角，不加渐变或密集装饰。沿用语义主题 token，新增局部 trace 状态前景 token（success/warning/danger），分别定义深浅主题颜色；文字对实际底色至少 4.5:1，状态始终带文字。装饰性的点、连接线、箭头不参与读屏。

### 3.1 任务列表

```text
┌─────────────────────────────────────┐
│ 子 agent       后台进程     Git Diff │
├─────────────────────────────────────┤
│ 子任务                    2 个进行中 │
│                                     │
│ ● 检查登录流程                    › │
│   正在读取 auth/session.ts          │
│   运行中                            │
│ ─────────────────────────────────── │
│ ● 补充接口测试                    › │
│   正在执行 npm test                 │
│   运行中                            │
│ ─────────────────────────────────── │
│ ✓ 梳理现有组件                    › │
│   已完成                            │
└─────────────────────────────────────┘
```

- 保留服务端顺序，不按状态自动排序。目标最多两行，缺失/空白时显示 taskId；列表截断内容的完整值能在详情通过键盘可达的展开入口阅读。
- 上图展示已有 objective/lastStep 时的增强布局；当前生产数据为空时实际行是「● task-a31f… / 运行中」，不再加一行重复的「等待进展」占位。详情缺步骤时也省略步骤行，时间线自身承担进展阅读。本次不承诺从 trace 自动生成任务标题，也不把用量或事件正文伪装成目标。
- 非终态在 lastStep 非空时显示步骤；终态隐藏旧步骤。暂停的任务只显示「已暂停」而不说正在工作。
- 「进行中」仅统计 queued/running/waiting_for_resource/waiting_for_permission/waiting_for_children；paused 与未知状态不计入，不将「不是已知终态」误算成活动。
- 所有当前 core 状态共享同一呈现规则，来源 `TaskState::is_terminal` 与 supervisor 的 task_state_label：

| 原值 | 中文 | 终态 | 强调 |
|---|---|---|---|
| queued | 排队中 | 否 | 中性 |
| running | 运行中 | 否 | warning |
| waiting_for_resource | 等待资源 | 否 | warning |
| waiting_for_permission | 等待授权 | 否 | warning |
| waiting_for_children | 等待子任务 | 否 | warning |
| paused | 已暂停 | 否 | 中性 |
| completed | 已完成 | 是 | success |
| completed_no_changes | 已完成，无变更 | 是 | success |
| blocked | 已阻塞 | 是 | danger |
| stalled | 已停滞 | 是 | danger |
| timed_out | 已超时 | 是 | danger |
| budget_exhausted | 预算耗尽 | 是 | danger |
| failed | 失败 | 是 | danger |
| cancelled | 已取消 | 是 | 中性 |
| recovery_required | 需要恢复 | 是 | danger |

未知状态保留原值/空值显示「状态未知」，使用中性色，不推定终态或活动。这里的终态仅描述当前执行状态，不承诺容器生命周期已关闭；是否能投递消息仍由服务端裁决。

- 整行使用原生 button，保留 aria-current；hover、键盘 focus 清楚。
- 空态：「暂无子 agent」+「委派子任务后，执行进展会显示在这里」，不提供虚构创建按钮。

### 3.2 详情

```text
┌─────────────────────────────────────┐
│ 子 agent       后台进程     Git Diff │
├─────────────────────────────────────┤
│ ‹ 全部任务  /  父任务（有则显示）    │
│ 检查登录流程                        │
│ ● 运行中                  task-a31f │
│ 正在读取 auth/session.ts            │
├─────────────────────────────────────┤
│ 执行轨迹                            │
│ ○ 我先检查会话恢复和鉴权入口。      │
│ │                                   │
│ ├─ 读取文件 · read                  │
│ │  src/auth/session.ts              │
│ │                                   │
│ ├─ 结果摘要 · read              展开 │
│ │  已读取文件                       │
│ │                                   │
│ ○ 找到一处可能导致重复刷新的逻辑。  │
│ │                                   │
│ ● 等待新的执行进展…                │
│                                     │
│ 子任务 2                         收起│
│ ● 验证 token 刷新                  ›│
│                    [ 回到最新 ↓ ]   │
├─────────────────────────────────────┤
│ 向这个 agent 发送消息…              │
│ 停止任务                     发送 ↑ │
└─────────────────────────────────────┘
```

- 固定紧凑导航与任务摘要，时间线是唯一主纵向滚动区，底部消息区固定。子任务、完整目标、完整 ID、完整步骤放主滚动内容内；不增加嵌套纵向滚动区。
- 标题默认自然换行至最多三行，步骤最多两行，头部 ID 仅显示短形式。超出时提供「查看完整任务信息」按钮，展开主滚动区顶部的完整信息并聚焦该区；完整字符串可选择复制，不依赖 title 或悬停。头部截断不丢失可访问内容。
- 支持最小桌面窗口 900×600：大字号下默认主滚动区仍至少可见 120px；textarea 默认两行，可增长到四行，继续输入在输入控件内滚动（输入控件滚动不算第二个正文滚动区）。确认区以紧凑布局替代动作行，不无限增高。
- 进入后立即展示时间线，删除整体「展开轨迹」开关。
- assistant_text 复用 MarkdownText；连续且有效的文本合并，不额外套气泡。局部 wrapper 约束 prose 的宽度/间距，不修改主对话 MarkdownText 的全局样式。
- tool_call 卡片保留原始工具名及服务端摘要；已知 read/write/edit/bash 可添加「读取文件/写入文件/编辑文件/执行命令」辅助标签，其它保留工具名，不把摘要当 JSON 输入再推断。
- tool_result 标题「结果摘要 · 工具名」。按显式换行计算逻辑行，超过 3 行或 240 个 Unicode code point 时默认折叠；预览取第一逻辑行前 240 个 code point，截断标省略号。不按浏览器实际折行决定初始折叠，避免栏宽/字号变化反复重置。
- is_error=true 时错误标识与首行默认可见；首行为空时显示「工具返回错误（无摘要）」；空成功摘要显示「工具已返回（无摘要）」。可展开收到的全部摘要，但提示「仅展示服务端提供的摘要」，不称完整输出。工具摘要也可能已在服务端截断，不根据恰好 1024 字节猜截断标识。
- state_note 使用低强调注记。payload 必须为非数组对象，已知 kind 校验必需字段类型及 type 标签（若存在则必须一致）；不合法时显示事件类型与原始载荷入口，不丢弃。未知 kind 同样回退原始文本；不要把原始载荷当 Markdown/HTML 渲染。
- 长行就地换行或代码区局部横向滚动，不能使整栏横向溢出。稳定 key 由任务身份与事件首 eventId 派生；按值 memo 的定稿事件在追加时不重挂、不重置展开状态。
- 长轨迹采用渐进展示而非丢数据：所有行保留并折叠成块，首次只挂载最新 200 块；顶部显示「已显示 X / Y 段」及「载入更早 200 段」。每次向前扩展最多 200 块并保持原首个可见事件偏移；无更早内容则隐藏入口。流式追加不自动收窄已展开的可见范围，也不按行数裁掉内容；切任务重置为最近 200 块。此策略限制首屏 DOM，不限制内存或用户主动加载全部后的 DOM，后者作为已知边界，不宣称无限长轨迹有固定资源占用。展开内容状态由稳定 key 保留，文本合并以全量行完成后再取块窗口，不在窗口边界截断消息。
- 时间线尾部只有已知活动状态才显示「等待新的执行进展…」；paused 显示已暂停；终态显示对应终态文字；row 缺失/未知不显示运行中或无限等待假象。空态与 loading/error 独立（见 §4）。

### 3.3 可访问性

使用语义标题、命名滚动区、ol/li 时间线、原生 button/textarea。展开按钮有 aria-expanded/aria-controls；子任务区同样可键盘折叠。消息框有独立 label，不只依赖 placeholder；发送/停止与确认动作有明确可访问名称。状态反馈用单个 polite status 区，错误用 alert，不把整条流式轨迹设为 live region。导航后聚焦任务标题；返回列表聚焦原任务按钮（已消失则聚焦列表标题）；展开原文后焦点不丢。取消确认打开时焦点落在「放弃」而非危险动作，关闭回到停止按钮；任务已结束导致按钮消失时回到标题。确认成功/失败/过期/放弃/终态替换确认区时，仅当焦点确在将卸载或禁用的控件内，才回到仍存在的停止按钮，否则回任务标题；流式或终态通知不得抢走 textarea 的焦点。载入更早入口最终消失时，若焦点在该按钮则聚焦新载入批次首段的非 Tab 顺序锚点，否则保持原焦点。遵循 prefers-reduced-motion，不要求脉冲/旋转动画。

## 4. 数据、订阅、滚动与导航

### 4.1 身份与订阅序列

详情的拥有者是 `{slot, serverBindingGeneration, clientIdentity, connectionGeneration, threadId, taskId, requestGeneration}`。导航发起时同步更新拥有者 ref（不等 useEffect）；所有可见响应、错误与事件都核对完整拥有者。消息/取消回执另按其捕获的操作记录与绑定生命周期结算，允许离开任务后更新原记录，但不得写进另一拥有者的可见状态或已删除记录。事件同时匹配来源 client/slot、params.threadId、params.taskId、row.taskId，不能只比较 taskId。

以 `trace/watch` 为首次/重试的唯一基线请求（它已返回完整快照），不再先 read 后忽略 watch 快照。`trace/read` 协议保留，前端此流程不再调用。watch 待回包时缓冲匹配任务的事件，ack 后按下列规则发布：

1. 快照 rows 中仅接纳当前 taskId 的行；以 eventId 去重、升序。
2. 用快照覆盖同 ID 行，合并缓冲中 eventId > highWaterId 的行；高水位以内以权威快照为准。
3. 后续 live 行按 eventId 幂等合入、升序；重复不触发可见更新。highWaterId 是当前任务快照最后一行的 eventId（空快照为 0），而非全局最大 eventId；事件 ID 本身可不连续。它不等于行数，不按缺号请求缺失事件。agent/trace/event 使用内容层非阻塞广播，出站队列满时可能丢帧或摘除慢客户端；本设计不保证 live 事件全部送达、不以缺号报错。重新 watch 时用权威快照校准。
4. 事件不可变：除快照优先规则外同 ID 不重复拼接文本。切任务清空上一拥有者的行及缓冲。同一选择重试先清未发布缓冲，新 watch 快照成为新基线；不能在其后重新发布失败前的半截缓冲。已有 ready 历史可保留到新基线成功，但与新缓冲隔离。

所有本前端的 watch/unwatch 变更按 clientIdentity 串行排队（可用内部 promise chain；前一个失败不阻塞队列）。每个排队操作执行前核对最新意图；过期 watch 未发出则跳过，已发出则等待 settle 后才能发新 watch 或补偿 unwatch。这样 A→B→关闭、关闭→重开、同任务重试均不能被旧清理解除新订阅。

请求代际保护只能防本前端竞态，不解决服务器跨客户端按 thread 共享 watch 的架构；本次不引入服务端客户端租约。UI 不声称同 thread 多客户端可以各自独立观看不同 trace。

已发出但过期的 watch 若 ack 成功，其补偿 unwatch 必须在该串行操作结束前完成，不能追加到已排队的新 watch 后面；发送前确认 cleanup 的 client/thread 归属。同 thread 的 A→B 可以直接由紧随其后的 B watch 替换 A，不必先 unwatch；如果补偿失败而连接仍有效，保留清理错误并在后续最新 watch 的成功替换后收敛，不把旧错误写到新视图。若无下一 watch，报告清理失败但本地视图仍关闭；不能承诺网络失败时服务端监听必然立即释放。

### 4.2 生命周期与错误

- 本地状态 loading/ready/error：loading「正在读取执行轨迹…」；watch ack 成功且无行「暂无执行轨迹」；初始请求失败显示内联错误+「重试」，不写进主对话错误横幅。ready 状态也提供低强调、键盘可达的「刷新轨迹」动作，等同同选择重试；已在 loading 时禁用重复请求，不改变当前阅读意图。
- ready 后连接丢失，保留已加载轨迹并显示「连接已断开，轨迹可能未更新」与重试入口；重试遇不到连接展示 formatError，不假成功。无需自动轮询、不得自动重放消息或取消。普通重连使旧连接拥有者失效、保留选择/历史/草稿并显示重试；握手后的新 client 可供用户重试建立新拥有者，不自动重订阅。服务器绑定目标变化则按下述退出路径回列表，并按 §5 清相应草稿。
- 订阅意图超过 15 秒未 settle 时结束 loading，显示「读取超时，正在等待上一次订阅请求完成」；超时本身不使唯一当前请求失效。若未登记更新意图，迟到成功仍发布权威快照并清超时提示，失败显示错误与重试；仍当前的事件缓冲保留到 settle。若期间导航/重试已更新意图，旧请求过期不接纳其快照，settle 后先清理再执行最新意图。底层 promise 仍占队列，不能用 Promise.race 释放序列锁让旧请求迟到创建 watch。重试可登记为最新意图，但在旧请求 settle 前不再发送订阅变更。用户始终可关闭面板；永不 settle 的旧连接只能等待既有连接退出/重建来释放，本次不为附属栏强制 dispose 整条主会话连接。15 秒包括排队等待，加载 timer 及缓冲在意图失效时清理。服务端快照当前经同步 IPC 读取，IPC reply read timeout 为 30 秒（非端到端完成上界）；同 server 的其它 RPC、包括补偿 unwatch，可能因此排队超过前端 15 秒。队列栅栏不得假设能在 15 秒内返回。
- 消息/预览/取消确认同样在 15 秒后给出「响应超时，结果尚不确定」提示，但保留对应底层在途写操作的同步守卫直到 settle/连接销毁，不自动重试或允许重复写。迟到成功只结算其原记录；已超时的消息保留草稿不补清，已超时的预览丢弃 token；取消最终状态仍由通知裁决。
- 回列表、两种关闭入口（面板关闭/状态栏收起）、切主会话、新会话、删除当前会话、切活动 slot、重配、App 卸载，都同步使当前拥有者失效，并在旧 client 的队列里清理其 trace watch；不存在旧连接时只做本地清理，不在新连接上 unwatch 旧会话。
- 仅切到 diff/process Tab 时保留当前选择和 watch；回到子 agent Tab 重挂展示，重新定位最新。明确这是本次的简单策略，不在隐藏 Tab 中反复订阅。
- 返回列表/关闭后重新打开面板默认列表，不复活上次 watch；草稿仍按 §5 保留。
- `agent/trace/unwatch` 的服务端修复仅移除 trace_watches[threadId]；保留 children_watches/process_watches，并保留幂等 stopped 响应。thread/delete 与服务器退出仍清理所有相关 watcher，回归测试分别覆盖。服务端测试可使用 server 内部 watch-map 夹具，把现有清理代码抽为真实分派调用的两个窄 helper：stop_trace_watch 与 stop_thread_watches。注入可观察存活/通知转发的 trace/children/process task，验证前者仅 abort trace、后者终止全部，其它 thread 不受影响；现有 Harness 覆盖真实 unwatch 的 stopped 响应与重复调用幂等。无需给生产服务器增加测试注入 RPC，也不假设现有 Harness 能直接访问 serve 局部变量。定向命令：cargo test -p yi-agent-app-server trace_unwatch_ 与 cargo test -p yi-agent-app-server thread_delete_stops_all_watchers。

### 4.3 滚动

首次进入任务，watch 首次 loading→ready 后在布局提交后的 useLayoutEffect 定位最新一次；同一选择的失败重试首次取得快照也执行一次。后续事件/重叠快照只尊重已有跟随意图，不重复强制贴底。

复用现有 `useStickToBottom` 的相对位移判定与 4px 贴底容差（不是原稿的 64px），不改变共享 hook 或主聊天行为。以用户滚动前已记录的跟随意图为准，不能在内容增长后用新 scrollHeight 反推用户离开；用户真实上翻并离开底部 4px 时暂停，主动滚回 4px 容差内或点击「回到最新」恢复。跟随使用即时滚动。

暂停时出现不占正文流的「回到最新」，不遮住末尾事件/子任务（留足底部空隙）；新事件和同一文本块增长均不能抢滚动。展开/收起工具摘要、原始载荷或子任务区时保持阅读锚点，不因程序滚动事件自动恢复跟随。点击「查看完整任务信息」是显式导航例外：保存当前事件锚点并暂停跟随，展开后滚到并聚焦完整信息区；用该区的收起按钮收起时恢复保存的锚点（已不存在则取最近可见事件），只有焦点在被卸载区内才回到展开入口。恢复锚点不自动恢复跟随，用户回到底部/点回最新再恢复。删除未展开内容不可导致焦点落空。

字体、栏宽、窗口尺寸和异步 Markdown 图片改变高度：跟随态保持底部，暂停态保持首个可见事件及其相对顶部偏移；必要时用 ResizeObserver，在卸载时清理。切任务/Tab 重挂不恢复旧滚动位置。不得仅依赖 rows.length 判断增长，因为连续 assistant_text 可能合为同一块。

### 4.4 导航

「全部任务」回列表。parentTaskId 在当前 railRows 中可找到、非自身才显示单级可点击父入口；缺失不显示虚假路径，不创建历史栈。目标缺失降级 ID。循环数据不自动递归遍历，子任务过滤掉自身。

直接子任务使用「子任务 N」可折叠区，默认展开紧凑行，行数不限，由主滚动区承载。切任务默认展开。父/子入口使用同一个 open 操作，因此必须遵守 §4.1 序列门禁。

## 5. 消息、草稿与停止操作

### 5.1 消息与草稿

- 常驻两行 textarea + 发送按钮；Enter 发送，Shift+Enter 换行，复用 useImeGuard/isImeEnter。空白消息或该任务发送中禁用发送；pending 期间仍可编辑下一段文字。
- 草稿键为 `{slot, serverBindingGeneration, threadId, taskId}`，不是单纯 threadId/taskId，以免不同服务器同名 ID 串稿。serverBindingGeneration 仅在 slot 绑定目标变化或重配时增加；同一目标的普通传输重连不改变它，与每次连接的 connectionGeneration 分开。由本次专用 trace controller 的内存 Map 持有，组件卸载/切 Tab/关面板/切任务/切会话不丢；重配清空全部，删除会话清该绑定下对应记录，移除 slot 清该 slot 记录，App 退出全部释放，不写磁盘。主会话的 ThreadStore.draft 不参与，也不得被改动。
- 正文发送捕获目标 client、身份键、draftRevision 和提交文本。成功只在原键且 revision 未变时清该草稿；用户编辑过的新文本保留，即使文本后来又改回同样字符串也以 revision 判断。失败保留文本，错误绑定原任务记录；不在当前别的任务显示旧错误或抢焦点。
- 每任务至多一个在途消息，重复 Enter/点击用同步 ref 守卫；预览/确认也按任务与操作类型保留在途记录，切走再切回不能绕过守卫。回执不是已执行，只显示「消息已入队」。前端不自动重试写操作，避免重复投递；请求失败（含结果未知）不自动清稿。记录删除/绑定失效带生命周期代际，迟到回执不得重建已删草稿或错误记录，也不能删除另一次发送的 pending 标记。
- onMessage/onCancel/onConfirmCancel 在无 client、无当前 thread、无当前 task 或绑定已变时均 reject ConnectionError，通过 formatError 展示。不得用可选链 undefined 当成功。终态消息是否接受仍由服务端裁决，不显示“重新启动”承诺。

### 5.2 停止操作

- row.finished=true 隐藏停止按钮。row 缺失/未知可以尝试预览，但 UI 不推断任务在执行。
- 点击停止仅请求 cancel/preview，原位确认。当前 recursive:false，合法请求恰好返回目标自身一个 taskId；未知任务预览直接报错，不是返回空列表。数量精确来自去重后的 preview.taskIds；以下零个/多个仅是异常响应的防御展示，不是当前协议的可达取消范围：0 个显示「当前没有可取消的任务」且不启用确认。一个显示「停止该任务？」；多个显示「将停止 N 个任务」，只按返回范围说明，不暗示一定递归取消（现有 RPC 发送 recursive:false）。
- 预览回包再次核对拥有者、预览代际与当前任务状态；若 row.finished 已变真、状态已明确终态或导航已使预览失效，丢弃 token，不重新打开确认。有 pending 预览/确认时同步禁用重复操作；消息区域/草稿保留。确认区替代底部动作行，textarea 不卸载。
- 保存 token、任务身份、预览请求发出时 performance.now() 以及 expiresInSecs；本地 deadline = 请求发出时刻 + ttl，TTL 非有限正数视为不可确认。相比从响应到达计时，该规则不会额外延长有效期；服务端校验仍为权威。
- 过期立即禁用确认，显示「确认已过期，请重新预览」和重新预览入口。切任务、回列表、切 Tab、关面板、切会话、换绑定或收到明确终态时作废预览/确认 UI；已发出的取消不能撤销，但其迟到回执不影响新视图。
- 点击确认仍用同一 token 与捕获的 client/身份；成功只显示「取消请求已提交」，终态等待服务端。失败显示错误并丢弃 token，必须重新预览，避免重复使用已消费令牌。放弃只撤销尚未提交的本地确认，不发 cancel。确认请求在途时禁用「放弃」并显示「取消请求已提交，等待响应」，不暗示能撤回已发请求；超时显示结果未确定且继续禁止重复确认。迟到回执不重新打开确认。

## 6. 组件职责与实现边界

- SubagentRail：表头、任务列表、空态。
- SubagentTrace：头部、时间线、子任务区与动作区组合；拆分 TraceTimeline/TraceEvent/TraceComposer，避免所有状态聚集在单文件。
- 共享状态 helper：上表中文/终态/活动与语义色；修正 TERMINAL_STATES 并核对其所有调用点，不复制两套状态枚举。
- 纯函数：解析/折叠 trace、合并快照与事件、折叠预览、令牌有效期计算；保持原始文本回退。迁移 toTraceLine/foldTraceRows 及其测试的所有调用点。
- 专用 useSubagentTrace/controller：拥有 selection、读取状态、request/client 代际、串行队列、事件接纳、草稿与动作状态。App/useSessionState 仅编排导航/生命周期，useNotificationRouter 委派 trace 事件与连接断开，不直接 setTraceRows。
- RPC 服务端：只修 trace unwatch 的资源边界并增加回归测试，不变更协议、daemon 或其它模块业务行为。

## 7. 验收

测试使用 mock RPC/deferred promises/fake timers，不调用真实 LLM 或生产会话。

| ID | 必须验证的结果 |
|---|---|
| A01 | 列表所有状态标签/终态/活动计数、空态、未知状态、无 objective/lastStep 的实际数据形状 |
| A02 | 默认时间线、连续有效文本合并、Markdown；JSON 破损/null/数组/类型错/标签不符均保留原文入口 |
| A03 | 工具调用/结果不猜配对、结果工具名、长摘要阈值与 Unicode 边界、错误首行/空摘要；追加不重挂定稿或重置折叠；1000 块初始只挂载 200、再追加 100 块可见范围为 300、逐批载入更早可访问全部、保持锚点且追加不丢块；持续追加导致 DOM 增长是已知边界，非固定上限承诺 |
| A04 | 首次定位、追加及同块增长跟随、上翻不抢滚动、回最新、展开锚点、ResizeObserver/图片/栏宽/字体变化 |
| A05 | 列表→任务→子任务→父任务→列表；缺失父级/自引用安全回退；焦点可达且不丢 |
| A06 | loading/ready-empty/error/retry、watch ack 前事件缓冲、快照/事件重叠去重、任务快照末行 highWaterId 不等于行数且 ID 可不连续、丢帧不报错且手动刷新快照补回、重试丢旧缓冲；15 秒排队/请求超时提示、无新意图迟到成功恢复、锁不提前释放、过期补偿先于新 watch |
| A07 | deferred 控制 A→B、A→B→关闭、关闭→重开 A、同任务重试、旧 client/slot 的同名 ID 事件/快照/失败不污染当前视图 |
| A08 | 所有退出/换绑路径失效与串行清理；Tab 切换保留 watch 但清预览；trace unwatch 不停列表/进程 watcher，thread/delete 仍全部清理 |
| A09 | 成功入队清提交版本草稿、发送中修改不被清、失败留稿、IME/空白/双击、任务/会话/slot 同名 ID 不串稿、无连接不假成功、草稿清理生命周期；超时不补清草稿、跨导航 pending 与删除后迟到回包守卫 |
| A10 | preview 在先、合法目标恰好返回自身一个 taskId、未知目标报错、零个/多个只做防御单测且不暗示递归、放弃、请求起点 TTL、预览延迟已过期、非正 TTL、到期/终态/导航作废（含预览回包竞态）、确认在途不得撤回、防重复与失败必须重预览、15 秒超时及迟到响应 |
| A11 | 深浅主题 × 260/360/720px × 小/标准/大字号，最小窗口 900×600；长中文目标/ID/路径/表格/代码/错误/多个子任务，零整栏横向溢出、主区默认≥120px、动作可达、对比度≥4.5:1 |
| A12 | 键盘 Tab/Enter/Space、aria-expanded/controls、独立输入 label、焦点恢复、消息/status/alert、reduced-motion；流式文本不全量读屏播报 |

执行 `cd desktop && npx tsc --noEmit && npm test && npm run build`，以及定向 app-server Rust 测试。提交实现前按项目要求执行 `cd yi-agent-rs && cargo fmt --all && just ci`，合并后再次执行 just ci；纯设计/计划文档提交免 CI 与单测。

A04/A11 的真实几何不能由 jsdom 证明：复用 `desktop/scripts/browser/` 中 Node 原生 WebSocket + CDP 的独立测试页模式，为真实 Trace 组件与项目 CSS 增加可重复脚本，不引入浏览器自动化依赖。测试使用独立 Chrome profile、清理子进程，不调用生产 sidecar；该自动化不是用户拒绝的 visual companion。保留屏幕尺寸、主题、字号、几何/对比度和通过/失败输出作证据。原生 WKWebView 人工验收单独记录，不能把 Chrome 当真机；无法执行则明确未验。

交付时同步 desktop 模块文件、索引计数与验收记录，列实际命令结果。自动化通过但原生尚未验时分别说明，不笼统宣称全部验收通过。
