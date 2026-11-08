# Session 内容完整性与显示收敛修复设计

日期：2026-11-08
状态：已完成三轮设计复核，供用户审阅；未实施、未部署。
版本：rev2（2026-11-08）；[复核记录](../reports/2026-11-08-session-completeness-spec-review.md)。
范围：普通会话正文、阅读位置、子 agent 与后台进程摘要的更新完整性。

## 1. 背景与证据

用户确认存在以下问题：session 内容不全；发一句话后，切走再回来才出现更多正文或状态。

[诊断记录](../reports/2026-11-08-session-completeness-diagnosis.md) 基于 `a0b6f9dc`：

| 问题 | 证据等级 | 修复要求 |
| --- | --- | --- |
| 长历史滚动进占位区，窗口补一页后停住 | 真实 Chrome 已复现；600 条均在内存，scrollTop=60802，可视消息为空；仅增加一次 nudge，位置不变，出现第 297–302 条 | 显示自行收敛，不依赖下一次滚动或发送 |
| 首次加载／首次访问的滚动补偿异常 | 已观察到请求上翻后回到底部；高度簿与旧高度差是代码疑点，尚未完成单变量根因证明 | 必须先形成稳定失败用例，再修补偿；不能把它说成已唯一定位 |
| 首次打开运行中会话，旧历史被 live 尾部游标跳过 | 真实 App + 模拟 RPC 已复现；没有丢通知，只有先收到新 item、后打开会话 | 完整基线与 live 尾部游标分离 |
| 客户端缺回答，切回时 readItems 才补出 | 真实 App + 模拟未投递回答已验证机制 | 正文、附属状态自动刷新与对账 |
| 普通内容队列满时可丢帧，完成项也在这条路径 | 代码确认 try_send 结果被忽略；未确认用户某次现场真的队列满 | 完成材料最终收敛、缺口可发现，不能静默保持“已同步” |

当前主线 `eda677b7` 与诊断基线在本设计相关实现上无差异。本 spec 不重新归因此前已经修好的 spacer `shrink-0` 和窗口 owner 隔离缺陷，也不把 kanban query socket 错误当正文读取失败证据。

既有 [转录窗口 spec](2026-11-06-transcript-window-design.md) 的性能与原生滚动方向保持。本 spec 是完整性补充；冲突时以本文明确修正的测量、挂载上限、跟随和对账契约为准。

## 2. 目标、边界与方案选择

### 2.1 必须达成

1. 已在客户端的任一历史消息都能通过滚动、搜索、阅读位置恢复访问，不留下稳定空白占位区。
2. 用户阅读历史时，后台增长、高度估算修正和程序化补偿不把用户误拉回最新内容。
3. 运行中首次进入也能读完整历史，不 interrupt、不 resume、不重建正在工作的 agent。
4. 中间丢内容帧、最后一帧丢失、完成项丢失后，无需用户发送或切会话即可发现并修复。
5. 服务端完成材料、客户端内存、挂载窗口、附属状态分别有明确完整性判据，不再把“连接在线”当作“内容最新”。
6. 真正离线、旧协议或读取失败时诚实保留已有内容和草稿，不显示虚假的已同步状态。

### 2.2 不做

- 不替换 React、Tauri 或原生滚动，不全量挂载历史。
- 不修改模型上下文、会话 writer lock、guardian/租约、用户已读水位规则或 LLM 请求流程。
- 不调整已确认参数：initialSize=60、pageSize=200、maxMounted=400、buffer=20。
- 不引入永久事件日志、逐帧磁盘写入、所有后台会话的轮询，或扩大队列作为主修复。
- 不改旁支、子 agent 详情轨迹的业务协议；只借鉴其快照／实时对齐思想。
- 不自动重试发送、插话、审批等有副作用操作；对账只读，不会多启动一轮。
- 本文不是逐文件实施 plan，也不授权替换正式 app 或重启正在运行的实例。

### 2.3 取舍

| 方案 | 优点 | 缺点 | 决定 |
| --- | --- | --- | --- |
| 最小补丁：窗口变更后再 nudge；冷会话不带 afterItemId；完成项改可靠 | 改动少，可先止住已复现缺陷 | 旧历史插入顺序、快照与 delta 重复、丢最后一帧、冷加载补偿仍缺完整契约 | 可作分阶段落地，但不能按全部需求验收 |
| 完整性闭环：视口收敛 + 显式基线 + 一致只读快照 + 有界实时对齐 + 尾部探测 | 同时覆盖数据和显示，竞态可测试，丢帧可自愈 | 需要能力协商、新只读协议和小型同步控制器 | **推荐，作为最终验收标准** |
| 全量渲染／频繁全量读／无限放大队列 | 实现表面简单 | 性能退化、快照竞态不消失、内存与背压无界 | 不采用 |

## 3. 总体结构与术语

两个交付单元，共用端到端验收：

- **显示收敛单元**：窗口纯函数、DOM 测量、会话级布局账本与滚动协调。独立于 RPC，可以先实现。
- **数据收敛单元**：app-server 只读投影与投递元信息；前端普通会话同步控制器；附属摘要刷新。其协议和快照语义先于前端接线。

职责边界：

| 单元 | 负责 | 不负责 |
| --- | --- | --- |
| Session | 有序安装权威 items、实时更新、本地乐观项与草稿保留 | 发 RPC、决定是否已经读完整历史 |
| 普通会话 SyncController（新建，名称可在 plan 中按项目惯例确定） | 基线、序号、缓冲、读取合并、重试、失效与归属 | DOM 滚动、启动或停止 agent |
| ReadView（app-server 内部只读投影） | 同一时刻的权威 items、活动 item、元信息和序号 | 改模型 messages、控制 agent、持久事件回放服务 |
| TranscriptWindow | 挂载区间计算和有限收敛调度 | 证明服务端历史已经完整 |
| ScrollCoordinator | 跟随意图、可视锚点、单次位置补偿 | 依据整体高度差猜测用户意图 |
| 附属摘要刷新 | 子 agent/process 列表的归属、freshness 和小范围探测 | 子 agent 详情 watch、process/read 既有输出轮询 |

关键区分：

- `liveLastItemId`：最近收到的 item 身份，只作兼容/诊断，**不能证明连续完整**。
- `baseline`：一次完整、一致的普通转录快照已安装；缓存有内容、items 非空、收到 completed 都不等于有基线。
- `view_seq`：同一 `(host_instance_id, threadId, content_epoch, view_generation)` 下，普通会话可见状态的单调序号。
- `view_generation`：只读投影重建/回收后再创建时生成的新 UUID；不递增 content_epoch，不改变模型上下文。它避免热会话被回收后 seq 从 0 重用，让旧缓存或 token 被误当连续。所有快照、probe、序号帧及客户端同步键都包含它。
- `content_epoch`：沿用既有可见集合世代（例如 clear）。它不是逐帧序号。
- `synced`：完整基线 + 基线之后连续的已应用帧 + 没有已知 dirty/overflow；仅表示已收敛到已知水位，不承诺未来永不变化。
- `following`：用户阅读意图，不能由 end===itemCount 或整体 scrollHeight 变化推导。

## 4. 显示层修复

### 4.1 测量必须反映真实视口

当前 `ChatView.measureVisible` 把挂载首末行当成可视首末行，且恒挂的 LiveTail 会污染历史窗口下边界。修正为：

1. 每行带绝对 item index；不能用 DOM 序号加 start 推算尾部索引。
2. 分开测量历史窗口首末边界、真正与 viewport 相交的首末消息、LiveTail 是否可视。
3. 无相交行是明确的 `outside-window` 状态，不伪造 visible.first/last。
4. 上下 gap 保留符号；不把“窗口上边界在视口上方”夹成 0。
5. LiveTail 在历史窗口外时，不用它的底边决定是否向下翻历史；窗口和 tail 两个不相邻区间必须分别描述。
6. DOM 扫描只覆盖挂载消息，稳态 O(400)。不能每帧扫描整个 Session。

### 4.2 有界、可取消的窗口收敛

触发：真实滚动、窗口提交、items 结构变化、挂载行高度变化、视口尺寸变化、focus、进入会话。

一次调度周期：

1. 合并触发，每帧最多一次测量；读取当前 owner、items generation、阅读意图。
2. 若视口已覆盖且缓冲足够，停止，无无效 setState/rAF。
3. 若视口在占位区，选择朝向视口的方向，每步推进一页；提交后**主动安排下一次测量**，不等第二个 scroll。
4. 一次周期内先朝同一方向覆盖视口，再做缓冲与远端卸载；防止上翻—截掉新页—再上翻振荡。
5. 区间与几何均无进展，或同一区间反复出现时，不继续循环：记录 stalled；用当前高度账本定位估计目标 index，发一次 focus 重定位，再重新测量。
6. 对固定条数 N，一次顺序补页周期最多 `ceil(N/200)+2` 次有效开窗。达到预算仍无覆盖，退出周期并标记显示异常，提供“重定位当前阅读位置”入口，不能后台无限转。
7. 新用户滚动、focus、thread/epoch 切换使旧周期作废；卸载、StrictMode 清理时取消帧并清空帧号。
8. 视口不可布局（hidden、尺寸为 0）暂停；重新可见时再调度，不按 0 高度扩窗。

预算是安全上限，不是要求长跳转逐页遍历：已知高度可在结构变化时建立位置索引并直接 focus 到估计位置。实现不得为改善远距离跳转破坏原生滚动或增大全局窗口参数。

### 4.3 挂载上限与跟随

- 总实际消息 DOM 不超过 400，**包括**恒挂 LiveTail；tail 在窗口之外时为它预留 1 个名额，不能出现 400+1。
- 首次无锚点首屏最多 60 条，包含 tail；tail 不重复渲染。
- 读取到历史尾部不等于用户要贴底。ScrollCoordinator 的显式意图是窗口 following 的唯一来源。
- 用户手动回到底部、主动发送、显式“回到最新”可以恢复跟随；历史页追加、程序化补偿、只读对账不能恢复跟随。
- 同一 item 非尾部增长也触发尺寸更新和正确重绘；不要求新增 item 才刷新。

### 4.4 高度与阅读锚点

1. 高度账本、上次布局测量、补偿状态属于 `(slot/host binding, threadId, content_epoch, layoutSignature)`；layoutSignature 至少区分可用宽度与影响排版的设置。离开可丢弃，不新增无限全局缓存。
2. 同一访问中使用稳定账本；中位数变化形成合并后的 heightRevision，让 spacer、测量和补偿在同一个布局周期更新。ResizeObserver 只观测挂载行，零高／非有限值不入账。
3. **每次布局提交维护上一份几何**，不能只在 win.start/end 变化时更新 prevHeight；否则初次中位数收敛的高度差会被积攒到下一次用户滚动。
4. 布局变化前保留真实可视消息 ID 与 viewport-relative offset；变化后按该消息的实际坐标恢复。优先保消息位置，不把整体 scrollHeight 差都解释为“在上方新增”。
5. 一次提交最多一个位置写入者：focus/入场锚点恢复 > 用户阅读锚点补偿 > 用户仍 following 时贴底。所有程序化位置写入统一标记，不能被 scroll 监听当成上翻／回底意图。
6. 空白占位区没有可视消息时，**不采集远处挂载行作为锚点**，不把巨大的 offset 写入记忆；保留最近有效锚点及本次滚动目标用于开窗。不能用 null 覆盖已有锚点并冒充贴底。
7. 冷回放尚未装入目标消息时，保留入场锚点，按 item ID 请求开窗；待目标真正挂载且偏移可容纳后恢复，不能因首个不完整 chunk 将位置永久夹到较小 scrollHeight。
8. 目标被权威快照删除时，才按既有约定回到最新，并告知阅读位置已不可恢复；换 thread 或 clear 后旧补偿回调不写新会话。

首次补偿异常必须先在真实浏览器形成判别性失败测试；本文上述约束是修复边界，不是把高度簿等疑点冒充已证实唯一根因。

## 5. 数据层：完整基线、一致快照与实时对齐

### 5.1 客户端身份与状态

同步键为 `(slot/host binding, connection generation, threadId, content_epoch)`；同名 thread 不跨宿主混用。

控制器在已知基线后将 view_generation 纳入校验身份；它与 content_epoch 分开，投影回收重建不会假借 epoch 改变。控制器维护：baselinePresent、baselineSeq、appliedSeq、dirty、reconciling、pendingReasons，以及读取期间的有界帧缓存。视图同步态为 `unknown / syncing / synced / stale / failed`，与 running/idle/awaiting_approval 分开。

规则：

- 首次进入、缓存被裁剪、epoch 变化、连接换代或已知缺口，没有可信基线：走完整只读快照，不带 liveLastItemId 跳过头部。
- 完整缓存可以立即展示，但新连接上仍需验证；旧缓存缺少新水位字段一律不作为完整证明。
- live 帧可以显示最新输出，但不能把 baselinePresent 从 false 置 true。
- 单会话最多一个读取周期；重复触发合并原因。dirty 在读取中再次发生，保留到当前周期结束，不得被 finally 无条件清掉。
- 换代、移除会话、dispose 后迟到回包丢弃；后台同宿主会话可以按其自己的键更新内存，不能更新其他会话面板。

### 5.2 新只读协议：保留旧 readItems 兼容

新增能力 `thread_transcript_sync_v1`，支持 read、probe、release 三个只读方法。Rust 协议为真源，TS 绑定生成；不能只手写前端新类型。

**`thread/transcript/read`**

- 首次请求：`{threadId}`，只返回一致快照的第一页。
- 续页请求：`{threadId, snapshotToken, cursor}`，只读同一份快照，不重新 load 新时刻。
- 每页元信息统一 snake_case：`host_instance_id, thread_id, content_epoch, view_generation, source, snapshot_seq, snapshot_token, next_cursor, total_items, items, metadata`；请求仍为 camelCase（`threadId, snapshotToken, cursor`），沿用既有请求风格。所有页携带同一份 metadata：`status, active_turn_id, last_turn_status, last_error, retrying, usage, totals, subagent_totals, info, permission_mode, reasoning_effort, last_read_at, last_completed_at`。只恢复这些当前状态，不重放审批请求或触发已读副作用。
- cursor、token 绑定客户端授权和 thread/epoch/宿主运行代，opaque；跨客户端、跨 thread 或过期 token 返回结构化错误，不降级成新快照中间页。
- 同一快照 pages 拼接是权威的 item 顺序，ID 唯一；最后一页才证明捕获材料完整。`source` 在所有页保持一致，为 `hot_projection/local_disk/external_disk`；外部或无法证明本宿主管理的活动写入标 external_disk，客户端只作受限展示，不能在第一次 probe 前暂时冒充 synced。空快照是合法的一页。
- 默认每页最多 200 items 且目标 256 KiB；计量使用 UTF-8 序列化字节数，包括完整 JSON-RPC envelope 和元信息，不能用 JS 字符数。单个更大 item 可独占一页，但整帧必须 ≤ 1 MiB（既有 `MAX_FRAME_BYTES`），超限返回 `item_too_large`，**不能截断文本后声称完整**。
- 一个客户端最多 2 份存活快照；单份序列化材料上限 64 MiB、有效期 60 秒；客户端单周期最多 1024 页、64 MiB 拼接材料。超预算或过期保留旧视图，退出读取并报可重试原因；不能安装半份基线。宿主热投影与快照的共享材料合计预算 256 MiB（引用共享只计一次），编码工作区也预留预算，先准入再分配。
- 新增只读资源释放 `thread/transcript/release({threadId,snapshotToken,installed?:boolean})`，返回 `{released:boolean}`，幂等、绑定同一客户端；installed 默认 false，true 仅确认该 token 的 snapshot_seq 已原子安装，用于清除 ≤ 该水位的 dirtyThrough，不能清除更新的失效。读到最后一页不提前销毁 token，客户端校验安装或失败后 finally release。取消是本地取消资格，不假装 RpcClient 已支持远端 abort；release 发不出去时由 disconnect/60 秒 TTL 回收。每页/probe 请求 10 秒超时、单快照周期 60 秒截止；超时注销该 pending，不将迟到只读响应永久留在 buffered。

**`thread/view/probe`**

- 输入 `{threadId}`；返回宿主运行代、epoch、view_generation、最新 view_seq、status/active_turn_id，及独立 `children`/`processes` 摘要水位（`revision, freshness, checked_at`；freshness 为 `fresh/stale/unknown`）。冷会话无活跃投影时只做常数个文件 stat，材料指纹变化即换 view_generation，并要求重新捕获；不能扫 jsonl 或仍回旧 seq。
- probe 不扫描整段 jsonl；当前可见附属摘要如已过期，在后台任务中做有界权威读取，不能在 RPC 主循环阻塞 IPC。既有 `watch_children` 已每 500 ms 查询 daemon 的 `ListTaskSummaries`，复用这个源与最近成功时间，不假设存在新的 daemon 轻量水位方法。
- 对应 v1 列表查询通过可选请求字段 `includeSync:true` 返回 `sync:{revision,freshness,checked_at}`；未带字段的旧响应保持原形。查询失败时 v1 返回结构化 `source_unavailable`，不把失败折为 fresh 空表。process 未知 thread 同样在 v1 区分 unknown 与已验证空列表。
- 每个 revision 在其对应摘要改变时递增，独立于正文 seq；不声称正文与两个附属列表是跨子系统的同一事务。相同权威快照不会递增，失败保留旧摘要并转 stale，通知丢失不影响 probe 读取权威水位。

旧 `thread/readItems(afterItemId)` 的线上语义不偷偷改变。新控制器在 v1 下不拿旧 afterItemId 修缺口；既有调用点逐一收口，避免 running/remote/idle warm 各自留下一套补齐逻辑。

### 5.3 快照来源：磁盘历史 + 当前可见投影

现有 `ThreadStore::load` 合流主日志与 checkpoint，但 `build_partial` 不含进行中的流式文本，所以新快照不能只包装旧磁盘读取。

ReadView 必须具备：

1. 热会话的历史基线在 driver 准备时建立；普通会话事件更新同一投影后才发布。历史材料可共享不可变引用，不复制模型 messages；流式只更新活动 item，不逐帧复制整段历史。
2. 从当前消息投影短临界区捕获一致 `(items, metadata, snapshot_seq)`，然后在锁外分页和编码。不持广播全局锁、agent 业务锁跨网络 await。
3. 冷会话使用新增严格只读材料读取，不直接把容错 `ThreadStore::load/read_log` 结果当完整。只读 items/必要元信息，不解析或复制模型 messages/凭证；损坏已提交行、损坏存在的 partial/meta 返回 `corrupt_material`，不能跳行后标完整。后台读取前后校验 meta/log/partial 的文件身份、长度、mtime 与本进程材料 revision，排除 append、原子替换、truncate、delete 和 partial 提交窗口；稳定读取失败最多捕获 3 次，然后返回 `snapshot_stale`。仅与既有 store 的短锁/本进程读 revision 协调，不改执行 writer lock，不持锁跨整段 I/O。外部 writer 只保证捕获到稳定已落盘材料，不能承诺尚未落盘的其他进程流式正文；probe 标 `source=external_disk`，不冒充本宿主热投影。
4. 进行中的 agentMessage/thinking 等可见 item 使用稳定协议 ID；Translator 在首个 reasoning delta 预分配 thinking ID，v1 帧 `sync.reasoning_item_id` 携带它，最终 ThinkingBlock 重用该 ID。旧通知 params 与旧客户端本地 reasoning 路径保持原形。快照只含可展示明文，绝不含 signature/encrypted_content/redacted data；redacted 终项按权威遮蔽语义替换，不能用“取较长文本”恢复已遮蔽内容。
5. turn 结束、取消、异常退出和附属 metadata 更新都进入同一投影顺序。区分“执行已结束”和“持久化成功”：append 失败不能提供假的 persisted 保证；热投影仍可读，重启后未落盘内容不作恢复承诺。
6. 已捕获 token 的分页内容不可变；clear/delete 使其不能继续覆盖新 epoch，并取消旧客户端安装资格。此只读服务不能改变 writer lock 或 running turn。

### 5.4 序号与合并 delta

v1 普通会话可见事件携带 `(host_instance_id, thread_id, content_epoch, view_generation, seq_from, seq_to)`：

- 普通转录、reasoning、用量及该会话状态事件在投影应用时按 thread 串行分配序号；一个事件 `from=to`。
- 合并 delta 的范围必须连续，且同一 thread/epoch/item；跨 item、状态屏障或不连续范围先 flush，不能伪称一个连续区间。
- 同一序号发布给多个客户端不重新分配；订阅筛选不改变序号；全局/UI/附属列表提示不参与普通 view_seq。
- 客户端 `from===appliedSeq+1` 才能连续应用。`to<=appliedSeq` 丢弃重复；from 更大说明缺口；范围跨过 appliedSeq 的半个合并 delta **不能截字符串猜测**，标 dirty 并重读快照。
- 序号按 JS 安全整数上限校验；不得接受 NaN、负数、不合法范围或在溢出时回绕。宿主在运行代变更后重新建立 seq 空间，不跨运行代比较。

订阅与恒推状态的边界：保留 `Delivery::List` 恒推，包括 `ThreadStatusUpdated` 和 `TurnCompleted`。未订阅或尚无基线时，这些帧只更新列表状态、置 dirty，不拿它们推断正文连续性。

- 本地 `Feed::All` 保持全收，不能为打开一条会话改成 Only。v1 `thread/subscribe` 请求可带 `mode:"all"`（仅本地允许），ACK 返回 `subscription_generation` 与 `mode`；远端仍用 `threadIds` 整体替换。未协商 v1 的请求/响应保持原形。
- 全客户端的订阅只由一个 SubscriptionCoordinator 写入；它合并普通 LRU、旁问父会话、子 agent 轨迹持有的 thread，序列化替换请求。远端唯一 ID≤16：当前普通会话和活动详情优先，先裁掉最旧无持有 LRU；超过上限拒绝新持有并显示原因，不能悄悄取消现有活动详情。不能保留旁问的 `[parentId]` 独立替换路径。
- v1 能力在每客户端 `initialize.params.capabilities.thread_transcript_sync_v1` 登记，响应新增 `transcriptHostInstanceId` 与 capability；它复用同一进程宿主身份，不造第二套 UUID。新 envelope 顶层可选 `sync`，包含统一 snake_case 水位字段及 subscription_generation，旧 params 原形不改。
- 监听 ready 和缓存同步建立在首次订阅请求之前，不是等 ACK 才开始缓存。宿主先完成过滤/顺序切换再回复 ACK，ACK 之前到达的新 generation 帧也缓存；先前 generation 的帧不能更新已安装基线。连接换代和取消订阅使原基线需重新验证。
- ReadView 的 apply→分配 seq→发布意图为一个不跨 await 的串行提交；跨 driver/主循环/status 源不允许解锁后再各自广播导致 seq 倒序。同一客户端/thread 出口按发布顺序写入，响应与其他 thread 可穿插。
- 捕获快照在同一提交边界封闭当前 delta 合并范围：未发布范围 ≤ snapshot_seq 必须 flush/seal，后续 delta 从 snapshot_seq+1 开始新范围。无需等待网络腾位才捕获。部分重叠仍作为异常自愈，不把高频正常生成变成必然反复重读。

### 5.5 安装快照与并行实时帧

顺序固定：同步建立监听与有界缓存 → 等待监听 ready／远端订阅 ACK → 捕获并读完同一 token → 原子安装 → 应用水位后的连续实时帧。ACK 前到达的帧也进入缓存，不在 ACK 后才创建缓存。

1. 读取期间保留现有界面；实时帧可更新临时显示，但序号帧必须同时缓存，不能把临时更新当完整基线。
2. 按服务端顺序原子安装全部权威 items；不是把漏掉的老消息 append 到现有尾部。本地 notice/config/help/usage 卡片作为独立 overlay 保留，记录它之前的服务端 ID，安装后锚到该 ID 后；目标消失则置于权威尾部，不能因此丢提示。乐观消息使用本地提交 ID，与 `turn/start` 回包的 `user-<turn_id>`、`turn/interject` 回包的 `interjection_id` 显式关联；回包到达后按 ID 认领，未知/未确认项保留，不能仅因旧历史有相同文本就删除新提问。旧客户端文字匹配不作为 v1 原子安装的认领证明。草稿与附件不由快照清空，断线不自动重发。
3. 丢弃 `seq_to<=snapshot_seq` 的缓存帧；从 snapshot_seq+1 连续应用。部分重叠的合并帧转 dirty；不能把已经包含在快照里的 delta 再追加一次。
4. 缓冲上限 256 帧或 2 MiB，任一到达即 overflow。不能静默丢缓存、继续标 synced；中止此次完整性声明，合并安排新快照。
5. 多次读取必须维护请求 generation，旧成功不能覆盖新结果；usage/totals/status 按相同快照水位与实时后缀安装，不让旧快照把新状态回退。
6. 快照安装 bump structuralVersion；仅文字／尺寸变化继续通知 textVersion/高度测量，避免“内存完整但 DOM 陈旧”。
7. dirty 记录已知缺口水位与本地失效 revision；捕获的 snapshot_seq 已覆盖的 dirtyThrough 可以清除，读取后新发生且未覆盖的失效不能被 finally 清掉。已知 probe 水位大于 snapshot_seq 且缓存后缀不连续时保持 stale，不把一致旧快照当最新。附属摘要 dirty 不强制重读正文。

### 5.6 状态恢复与冷读取的证明边界

- 普通 view_seq 覆盖显式集合：item started/completed/batch/delta、reasoning delta、turn started/completed、thread status、usage、retry/returned interjection、快照 metadata 中的设置/已读水位变更；未纳入快照的 UI focus、旁问、trace、列表提示、瞬时 notice 不分配普通 seq。工具 stdout delta 如现有 UI 不显示，投影按同样 no-op 语义处理但连续消费序号；完整工具卡以 completed 为准。不能出现某个 seq 代表快照与 Session 都无法恢复的状态变化。
- 冷读取增加独立 material revision/in-flight mutation guard：在 append、partial 写/清、truncate、delete、epoch 变化前后标记，读取仅在无在途修改且前后 revision/文件指纹相同时接受。守卫按同一存储根/thread 共享，不是各 `ThreadStore::new` 私有计数；只服务读一致性，不改变执行锁。回收空闲投影后新建 view_generation；有 token/热 driver 的投影不能回收。
- 同宿主冷转热先沿用已有投影/基线及 generation，再开启第一条 live；不能热化后重复装入已提交 items。外部 writer 不能受本进程 guard 约束，外部磁盘来源只作受限展示，不标 v1 synced，也不承诺其多文件跨写事务或未落盘文本完整。材料源变为本宿主热投影后重新建立可信基线。
- ThreadView 新增正文同步态而非直接复用既有发送门控 `syncState`。epoch/stale_context 的发送限制保留；正文暂时 stale 不错误改变 turnActive，不把内容加载失败变成永久禁用插话。snapshot.metadata 中的 active_turn_id/status/terminal 信息修复 UI 状态，不能仅装 items 而留下 Stop/重试徽章。

## 6. 投递可靠性与自动对账

### 6.1 完成材料必须可靠

不改 `Delivery::Content` 的订阅语义，另设投递可靠性分类：

- 必须可靠：item/started、item/completed、批量完整 items、普通会话状态／回合生命周期、同步失效提示。
- 可合并、允许短时缺口：高频文字／工具输出 delta，但必须通过序号和 probe 发现并收敛。

v1 的“可靠”是**完成材料最终可恢复**：正常走有序可靠出口；若有界出口耗尽，则必须保留权威材料并记录非静默失效，恢复后用快照收敛，不能承诺无限慢客户端逐帧必收。既有队列容量仍为 256，不用扩大队列兜底。

- 不在 driver、RPC 主循环或 ReadView 锁内新增无限 `send().await`。每客户端出口泵负责等待网络；所有生产发布入口同步提交顺序意图。短暂满时 delta 可丢但计数，完整项/状态不能被当作普通 delta 丢弃后继续声称 synced。
- v1 本地出口 Full：非阻塞登记每个受影响 thread 的 dirtyThrough；出口泵每次排空后优先合并发送定向 `thread/transcript/invalidated`（含最新水位及 reason），客户端快照确认覆盖水位后才清除 dirty。无空位时保留 dirty 意图，不再 try_send 同一提示。dirty 元信息按最多 16 订阅 thread 管理；本地 All 超过上限合并为连接级 invalidated，不能丢掉某会话失效，也不保留无限帧副本。
- v1 WS 出口等待最多 10 秒；停滞或可靠意图准入失败则显式断开该客户端、重连后重建基线，快端与业务取消不等待慢端。出口 Closed 按真实断线清理。
- 未协商客户端保留旧 envelope/订阅和既有关键帧背压；新增完成项可靠路径由独立出口处理，队列与在途材料合计仍有界。新增完成意图无法准入时显式关闭客户端，不能静默丢项或建立无限二级队列；旧本地连接须由现有连接恢复路径提示断线，不能继续报在线。既有关键帧背压可保留，但不能向业务循环增加新的等待路径。旧协议不承诺 v1 丢帧证明，不能借兼容分支绕过新客户端矩阵。
- 可靠通知、回包、反向审批保持每客户端出口的既有顺序约束；同步失效只触发只读恢复，不重启 sidecar、不取消正在工作的 agent。R01 同时验证“完成项收到或明确进入恢复路径”与取消/快速 probe 响应。

### 6.2 不依赖用户切换的触发

| 触发 | 行为 |
| --- | --- |
| 首次进入／缓存未知或裁剪／重连或 epoch 变化 | 完整只读快照 |
| 序号缺口、部分重叠、帧缓存 overflow、定向 dirty 提示 | 合并启动只读对账 |
| turn 结束或从 running 转 idle | 当前可见会话立即 probe，对不上则对账；不等切回 |
| app/window 回到可见且连接可用 | 立即 probe 普通会话与当前附属摘要，不为刷新内容强制重启 sidecar |
| 当前可见会话持续在线 | 每 5 秒一次轻量 probe，即使界面本地误以为 idle，也检查尾帧丢失 |
| 子 agent/process 提示或其 revision 改变 | 仅重拉相应摘要，不全量读正文 |

后台会话靠通知维护，dirty 保留，进入时验证；不为侧栏全部会话建 5 秒 timer。process/read 既有详情输出轮询不叠加第二套。

### 6.3 有界请求与失败表现

- 同一键最多一个 probe、一个 snapshot 周期；同一摘要最多一个列表读取，读取中再次 invalidated 留待下一轮。
- 初次尝试之后最多 3 次自动重试，间隔 1/2/5 秒（最多 4 次捕获周期），包括缓存溢出、部分重叠和普通 I/O 失败；同一自动 episode 最长 120 秒。随后显示“内容可能不完整，点击重试”，保留原视图，周期性轻量 probe 仍运行。只有成功安装可信基线、新连接或用户显式重试才重置预算；更高服务端水位只更新已知目标，不能每次 delta/5 秒 probe 重开预算形成无限大快照。
- 断线、hidden 或离开会话取消可见轮询；挂起中的结果只能写其仍有效的宿主／thread 身份，不写新面板。恢复可见时从 probe 开始。
- 初次没内容的读取失败显示加载失败和重试；已有内容的失败显示非阻断 stale 提示，不清屏、不编造空列表、不将失败当 synced。
- UI 同步状态与 turnActive 分离；内容 dirty 不能取消回合或悄悄丢弃输入。不放宽既有 stale_context/epoch 发送限制，也不因后台显示修复永久禁用运行中插话。
- 子 agent/process 列表独立标 fresh/stale/failed；失败保留旧列表，不能靠“整个 session 已同步”冒充列表最新。列表响应的 request generation + revision 防止 A→B→A 或旧连接迟到覆盖。

## 7. 协议兼容与发布

- 新宿主 + 新前端使用 v1；新宿主保留旧 readItems 与未协商客户端的旧通知形状，不强制旧客户端识别新字段/方法。
- 新前端 + 旧宿主：不假装有 seq 或活动快照；冷 running 使用不带 afterItemId 的完整旧 readItems，完成／前台恢复可读已落盘完整项。旧 readItems 无快照水位，**不能宣称已实现 v1 的并行流式精确对齐**；显示旧宿主同步能力受限提示，推荐升级宿主。
- 旧缓存可以作为视觉垫片；新增可信字段缺失或 trimmed 视为需全量。不能把一次握手返回 none 解释为历史完整／最新。
- UI/sidecar/relay 分步升级必须测试组合；本设计不要求 relay 理解正文 seq，它需完整透传字段并沿用有界帧投递。
- 部署验收使用独立 bundle id 和绝对路径 YI_AGENT_HOME；不自动改正式 relay 配对或重启正式 app。

## 8. 验收矩阵

以下是未来修复必须通过的判据，**不是目前已通过的修复测试**。

### 显示 D 组（真实 CSS/布局，不用 jsdom 冒充）

| ID | 场景 | 必须断言 |
| --- | --- | --- |
| D01 | 600 条长消息，A→B→A 后一次滚到中段，不再发送／滚动 | 自动覆盖 viewport；数据数量不变；固定几何下不移走滚动目标 |
| D02 | 3000 条，拖到顶部及任意中段；从 focus 窗口向下越过 bottom spacer | 没有稳定空白；有限步停止；恒挂 tail 不阻止历史下翻 |
| D03 | 同一会话连续上翻／下翻／方向反转 | 不翻页振荡，远端卸载不删除正阅读内容 |
| D04 | 同长 A/B、长短 A/B、不同宽度、clear、StrictMode | owner/账本/旧帧隔离，回调不写别的会话 |
| D05 | 首次已有历史；首次空→分块回放；初始中位高度变化后用户上翻 | 用户上翻不因累计旧高度差回到底部；先失败再修复 |
| D06 | 本地发送后贴底；正常流式、非 tail 正文增长、完成项替换 | 新输出无需切回；用户读历史时不抢位置 |
| D07 | 冷回放恢复长 offset，目标在尚未加载的一页 | 等目标真正可恢复后定位；不提前永久 clamp；有效锚点不被远处行覆盖 |
| D08 | 历史远离 tail；长代码块、图片迟到、折叠、resize、hidden→visible | 总 item DOM≤400（tail 计入）；真实尺寸变化正确补偿；不可布局时不空转 |
| D09 | 触发停止，随后不再有输入或尺寸改变 | 无持续 rAF/setState；超预算进入可见异常态，不无限循环 |

### 同步 S 组（真实 App/Session/控制器与协议接线）

| ID | 场景 | 必须断言 |
| --- | --- | --- |
| S01 | 冷 running 会话先收到 live 尾部，再首次打开 | 旧提问和回答全部存在且按权威顺序；无 resume/interrupt |
| S02 | 中间项丢失，但又收到更晚 item | 发现 seq 缺口；全量修洞，不仅读取尾部之后；无重复 ID |
| S03 | 部分 delta 丢失；锚点 item 自身文本不完整 | 同 ID 全文恢复，不遗漏锚点本身；完成后等于权威文字 |
| S04 | 最后一帧／completed／idle 通知没送到，且后续无事件 | 不切会话；5 秒 probe 在下一次成功响应发现水位差并启动修复 |
| S05 | 快照读取期间 delta 在捕获前、捕获后、跨合并范围到达 | 不重复追加、不断尾；部分重叠转重读；文字和顺序等于投影 |
| S06 | 多页期间继续生成，空快照、大 item、token 过期、页预算／内存预算超限 | 页来自同一 token；错误不安装半基线；完整态不能假绿 |
| S07 | 读取中再次 dirty；256 帧或2 MiB 缓存到限 | dirty 不被 finally 清空；有界重新捕获或诚实 failed |
| S08 | A→B→A、两个宿主同 thread ID、重连、clear/delete | 旧回包／旧 seq／旧 epoch 不能污染当前视图 |
| S09 | 用户本地乐观项、重复问同一句、读取期间新发送 | 草稿保留；不误删新乐观项；不会补齐引发第二次 turn/start |
| S10 | 旧缓存 trimmed／未知字段、旧宿主、新宿主配旧前端 | 不虚构可信基线；兼容降级如 §7，明确能力不足 |
| S11 | 服务端执行完成早于持久化，append 失败或 sidecar 重启 | 显示与持久化保证分开；不 resume 正在工作的 agent；断线自动换代 |
| S12 | 子 agent/process 更新提示丢失或慢列表回包、A→B→A | probe 修复摘要；generation/revision 防回退；正文和详情不受无关列表失败牵连 |

### 投递与可观察性 R 组

| ID | 场景 | 必须断言 |
| --- | --- | --- |
| R01 | 本地/WS 出口满时发送 started、completed、状态帧 | 完成材料收到或明确进入可恢复失效路径；快端与取消不等待慢端；订阅过滤仍正确，旧客户端无静默假绿 |
| R02 | delta Full、提示期间再次 Full、出口 Closed | 一条合并定向 dirty 提示最终送达或断线；无静默假在线；计数可查 |
| R03 | 真实 stdio→Tauri 事件桥→前端；WS/relay 组合 | 新字段透传；监听就绪后才读；处理 emit 失败／监听窗口，不只测 Broadcaster |
| R04 | 假时钟 5 秒 probe、退避上限、dispose/hidden/离开 | 单 flight；timer 被清理；全部后台会话没有定时器风暴 |
| R05 | 日志开启 | 仅 ID/seq/epoch/count/bytes/timing/reason；无正文、工具输出、密钥、relay token |

关键回归必须做回退即红：去掉 post-commit 重算→D01；恢复整体高度差／旧几何→D05；冷读带 live 游标→S01；去掉 seq 对齐→S05；静默丢 overflow→S07；关闭可见尾部 probe→S04；完成项退回 try_send→R01。

## 9. 真机与收尾门禁

1. Chromium 浏览器验收不能替代 macOS WKWebView。独立 app 上验 D01/D05/D06/D07，并区别“切 session”和“切 app/window”两条路径。
2. 真实 stdio 链路灌入大量工具输出，同时读取历史和生成正文；主动丢弃测试帧，确认无需切回就补齐，输入和 Stop 不被卡住。
3. iOS/WS/relay 在弱网、前后台切换、重连时验 S04/S05/S08，原配对信息不变；没有真机结果保持未完成。
4. 修复代码提交前和合并后分别运行 `cd yi-agent-rs && cargo fmt --all && just ci`；前端 `npx tsc --noEmit`、相关 Vitest、build 和真实浏览器脚本。Rust 优先按 yi-agent-app-server crate 跑，不默认裸全 workspace cargo test。
5. 本次仅 Spec 复核、Plan 与诊断归档，不要求跑实现 CI；文档链接、验收编号、进度计数必须校验。诊断里的 464 个通过用例不代表此矩阵通过，故所有实现项保持 `[ ]`。

## 10. 实施分段与风险

分段建议（不是实施任务清单）：

- 第一段：将诊断脚本转换成正确行为的失败回归，修显示收敛与补偿，独立验 D 组。
- 第二段：ReadView、序号、可靠投递与 v1 快照协议；先用真实服务端 mock 事件证明快照边界和无控制副作用。
- 第三段：普通会话 SyncController、原子有序安装、能力兼容、可见 probe 与附属摘要刷新，接入 App/通知/缓存路径。
- 第四段：组合并发、压力、变异验证和隔离真机验收，更新项目进度后才宣告全部修复。

主要风险及约束：

- **镜像成本**：ReadView 不复制模型 messages，不逐 delta 全量 clone，快照预算必须生效。历史基线尽量复用共享不可变材料，热投影随既有热会话生命周期释放；额外投影材料纳入宿主 256 MiB 同步内存总预算（与 token 材料合计，共享引用只计一次）。超预算不能驱逐正在工作的 agent 或静默裁剪基线；标该视图不可完整读取、保留旧显示并返回结构化错误。
- **高频事件导致持续重新对账**：保留连续范围与可靠完成项，缓存超限有界失败；禁止无限快照重试。
- **补偿与跟随互相抢位置**：单个位置写入者，用户意图单一真源；仅加 rAF 无法解决这个问题。
- **新旧协议混用**：能力开关决定是否使用 v1，旧结果不能作为带水位快照安装。
- **传输层仍丢尾帧**：probe 是端到端保底，不能只修队列、假设 Tauri emit 或监听永不失败。
- **长期会话资料超预算**：明确失败并保留已有可读内容，不静默裁剪；扩大快照预算或长期分段基线是后续单独设计，不在此处偷偷加入。

## 11. 评审与完成状态

本文在用户要求下经过三轮复核并修订为 rev2；逐项发现、修订及反例见 [复核记录](../reports/2026-11-08-session-completeness-spec-review.md)。配套 [实施总入口与数据 Plan](../plans/2026-11-08-session-completeness-repair-impl.md)、[显示 Plan](../plans/2026-11-08-session-display-convergence-impl.md) 只规定未来实施，不代表已修复。若决定只做最小补丁，必须明确缩小验收范围，不能把它记为本 spec 全部完成。

当前产物：修订后的设计、复核记录、实施 Plan 及可追溯诊断记录。代码修复、上述 D/S/R 矩阵、正式部署均未执行。
