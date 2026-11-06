# Desktop 子 agent 执行面板 spec 复审记录

日期：2026-11-07
范围：[设计](2026-11-07-desktop-subagent-trace-redesign-design.md)，不实施产品代码。

## 审查方法

按用户要求先反复检查 spec，再编写 [实施计划](../plans/2026-11-07-desktop-subagent-trace-redesign-impl.md)。使用真实源码核对、独立 UX/协议审查、逐项修订及最终一致性自审，不以最初的示意图代替协议事实。

先后收到：第一轮 UX 审查、第二轮交互/竞态审查、协议源码审查、最终收敛审查。审查代理只读，不修改文件。部分审查并行执行，因此以实际检查维度记录，不把同一个问题重复计算为多个已解决缺陷。

## 第一轮：产品可实现性与现有实现

| 发现 | 修订与位置 |
|---|---|
| 生产路径 objective/lastStep 恒空，图示不能当数据承诺 | §2、§3.1 标明实际 taskId + 状态降级；增强布局仅在有真实元数据时出现；不扩大协议补元数据 |
| 取消范围并非递归子树 | §5.2 只按返回范围呈现，合法请求当前只有目标自身，不暗示级联 |
| trace/unwatch 会误停列表和进程 watcher | §1、§4.2 纳入必要的局部 Rust 修复，thread/delete 仍全清 |
| 全量挂载没有长轨迹边界 | §3.2 首次200块、载早每批200、保全量数据；明确持续追加及用户全载后的DOM增长，不声称资源恒定 |
| 草稿归属不明确，组件卸载会丢稿 | §5.1 专用controller内存Map，不借用主会话draft；含服务器绑定代际 |
| 首次定位时序不确定 | §4.3 首次成功布局后定位；后续只尊重跟随意图 |
| 新造滚动阈值与项目实现冲突 | §4.3 复用useStickToBottom的4px相对位移规则，不修改共享hook |
| 可选链RPC可能产生假成功 | §5.1 无client/thread/task/有效绑定均reject ConnectionError；写回执不冒充执行完成 |

## 第二轮：竞态、焦点和验收

| 发现 | 修订与位置 |
|---|---|
| preview未返回前任务已结束，迟到token仍可能打开确认 | §5.2 回包重验拥有者、预览代际、finished/终态，不合条件丢token |
| confirm已发出时“放弃”可能被误读为撤销 | §5.2 在途禁放弃，明确等待响应；不能撤销已发请求 |
| 缺目标的ConnectionError契约未覆盖全部动作 | §5.1 明确onMessage/onCancel/onConfirmCancel全部遵守 |
| 确认区消失、载早按钮消失后的焦点未定义 | §3.3 条件恢复；不抢textarea焦点，最终载早聚焦新段锚点 |
| 重试可能复活上次失败缓冲 | §4.1 清未发布缓冲，新watch快照成为权威基线 |
| “首次200”不等于跟随态始终200 | §3.2、A03 明记1000块初始200、追加100为300，接受有界首屏而非固定总DOM |
| 服务端回归描述假设Harness能注入serve局部变量 | §4.2 改为生产实际调用的清理helper + server-local watch-map夹具，Harness只负责真实RPC幂等 |

## 协议复核：四项事实修正

1. highWaterId不是全局高水位。
   - 证据：`yi-agent-rs/crates/yi-agent-store/src/ipc/daemon.rs` 的 `ReadTaskTrace` 分支取 `rows.last().event_id`，空为0；`src/repository/trace.rs::trace_after` 按task过滤并按id排序。
   - 修订：§4.1/A06 写成当前任务快照末行ID，ID可不连续、不等于行数，不做缺号补拉。
2. live trace不保证完整投递。
   - 证据：`yi-agent-rs/crates/yi-agent-app-server/src/broadcast.rs::broadcast_for` 使用try_send；慢客户端可能丢帧或被摘除。
   - 修订：§4.1明确送达边界；§4.2提供ready态手动“刷新轨迹”，新快照校准，不新增轮询/心跳/缺号恢复协议。
3. 当前合法preview的taskIds恰好只有目标自身。
   - 证据：`src/rpc/agent.rs` 固定recursive:false；`yi-agent-store/src/repository/read.rs::task_tree_ids` 先验证目标存在，非递归时不展开后代。
   - 修订：§5.2/A10区分正常单目标与零个/多个的异常响应防御分支；未知目标预览报错。
4. 15秒UI提示不是服务端完成上界。
   - 证据：`yi-agent-store/src/ipc/client.rs` 的CLIENT_REQUEST_TIMEOUT为30秒且用于reply read；app-server快照通过同步IPC读取，处理请求可能排队。
   - 修订：§4.2明确锁必须等待底层settle，不能Promise.race释放栅栏，更不承诺30秒端到端完成。

## 最终收敛与自审修订

- 完整拥有者补serverBindingGeneration；可见状态核对owner，离开任务后的消息回执仍可结算原有效记录，但不能污染新视图/重建已删记录。
- 普通重连与换绑分开：普通重连保留选择/历史/草稿，握手完成后允许手动重试；换绑回列表并使相应记录失效。
- 补偿unwatch必须在原串行操作内结束，不能排到新watch后；网络失败时不承诺服务端立即释放。
- 完整任务信息是显式导航例外：保存阅读锚点、暂停跟随、展开后定位聚焦；收起恢复锚点。普通工具摘要/原始载荷展开仍保原位。
- 最终独立审查发现唯一当前watch超时后永久失效会妨碍迟到成功收敛：§4.2已修订为“15秒只提示；没有更新意图则迟到成功正常ready，有新意图才丢旧快照并先清理”。A06与plan Task4同步，写操作超时仍保留不确定结果和去重守卫。

## 计划一致性检查

- 每条A01–A12映射到具体任务及验证方式，真实几何不交给jsdom证明。
- 定义TraceTarget/TraceConnection/TraceOwner，订阅和动作共用，不复制另一套身份键。
- controller的connectionFor在Task6定义；Task10先登记连接来源，再wireClient闭包捕获，不能每帧读取当前client。
- Task9新详情先独立实现，Task10迁入正式SubagentTrace并同时改App/外壳props，保证中间提交可编译。
- watcher helper在server.rs显式导出；测试不要求新增生产测试RPC。
- UI identityKey复用完整绑定任务键、不含请求代际，刷新不重挂；动作与输入回调捕获展示target，不在迟到回调里偷读另一任务。
- 每个实现任务的定向测试不代替提交前cargo fmt/just ci；文档阶段按项目例外免产品测试。

## 检查结论与证据边界

最后一次独立只读复审在上述修订之后重新读取当前 spec，明确返回「No unresolved must-fix」，并复核了15秒迟到成功/串行栅栏、30秒IPC边界、高水位、watcher清理、非递归取消及A01–A12的一致性。本轮自审与该独立复审均未发现仍未解决的阻塞项。此结论表示当前规格可以进入实施，不表示实现已经完成，也不保证未来实现不会揭示新问题。

文档验证结果：`git diff --check`、相对链接/代码围栏/占位检查、11个任务×5步骤及A01–A12映射检查均通过；`cd yi-agent-rs && cargo fmt --all`通过，未产生源码变更。desktop条目保留未完成，索引121/137不变。

当前只修改设计、计划、审查和项目管理文档。执行文档diff/结构/链接/占位检查；没有把计划中的Vitest、Chrome、Rust回归或just ci说成已执行通过。原生WKWebView验收仍属于实施阶段，须单独记录。
