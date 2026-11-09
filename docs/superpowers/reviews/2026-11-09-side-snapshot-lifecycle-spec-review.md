# 一次性对话快照旁问规格复核记录

日期：2026-11-09

对象：[spec](../specs/2026-11-09-side-snapshot-lifecycle-design.md)

## 结论与证据边界

按用户“反复检查 spec，直到没有问题写 plan”的授权，完成产品语义、源码接线、竞态与跨模块一致性四组复核。下列已知阻塞均已修订，进入[实施计划](../plans/2026-11-09-side-snapshot-lifecycle-impl.md)阶段。

这是当前会话的源码与文档复核，不冒称独立 reviewer 意见；没有执行新行为测试，没有实现或合并生命周期功能。计划中的断言仍需实施时经历 RED/GREEN。

## 第一组：产品语义与旧规格覆盖

- 明确“只读”限制信息来源和副作用，不禁止 Control 用户连续提问；Observe 仍是设备授权，不能与快照只读混淆。
- 固定本次快照，Stop/Esc 只停回答，返回/切会话才销毁；再进入不能续旧问答、草稿或用量。
- 旧三端/repair 规格的返回保留、工具白名单、附件复制及动态 side prompt 已列为覆盖项，保留 dev-only 实验硬闸。
- 断网、后台、意外死亡分开，不承诺浏览器一定能通知宿主或供应商计算立即停止。

## 第二组：真实源码能力与来源

| 证据位置（相对 yi-agent-rs/crates） | 问题 | 修订/计划要求 |
|---|---|---|
| `yi-agent-runtime/src/side_conversation/policy.rs` | 注册文件和网络工具，单改 prompt 不构成隔离 | 空 registry，不继承父 tooling、审批或 inbox |
| `yi-agent-runtime/src/side_conversation/resources.rs::prepare` | 复制历史附件并建立 SideReadAccess | 旁问新路径只核算内嵌媒体，不访问附件、规则、skills |
| `yi-agent-app-server/src/side_conversation/source.rs::WorkspaceResolver` | 生产解析只有 cold，live helper 未装到 driver | 发布真实活跃父句柄；未落盘消息必须进入新快照 |
| 同文件 `source_from_live_agent` | 重新 build_provider/resolve_side_prompt | 复用父 provider/gate，读取已渲染有效配置缓存 |
| `yi-agent-core/src/agent.rs` 的请求构造、`live_settings.rs` | config() 是装配值；调用 live snapshot 又可能扫描 | 缓存父最近渲染有效 config，不以 side 触发 prompt source |
| `yi-agent-core/src/side_context.rs` / `message.rs` | Url 图片可由供应商隐式下载，ToolResult 可嵌套图片 | 递归替换 URL，保留 Base64，清补读 path |
| `yi-agent-runtime/src/side_conversation/source.rs::snapshot_with` | 克隆 source 后对共享 session.replace_messages，再创建独立 Arc | 删除对父的任何写入；父序列化历史逐字节不变断言 |

## 第三组：协议、竞态与资源真实销毁

- 初稿客户端随机操作 id + 有界 tombstone 在淘汰后不能可靠区分旧重试/首次创建。改为宿主 `side/prepare` 签票，未知票据永不创建。
- 票据未消费 60 秒过期；256/宿主、32/父是所有记录总上限。活代绑定固定保留；关闭记录最多 5 分钟且可提前淘汰，满时不挤掉活绑定。
- 在 Creating 前就绑定准确代次，多个 waiter 不能在前代失败/退出后自行 loop 建下一代；退出先于消费只撤票，不误杀其他端未知环境。
- 区分 `closing`（受理）、`closed`（排空并收据确认）、`failed`（残留隔离）。旧条件 close 和 close_all 共用清理协调器。
- `runner.rs` 当前 close 即使 last_turn_joined=false 也设 Closed；计划要求持有未结束 owner、join 证据后才成功。
- `resources.rs` 当前删除失败仍移除 slot；计划要求失败保留槽，按本代重试。
- `registry.rs` 当前 close 在移除前等待背压控制通知；计划要求清理独立，通知 1 秒预算，丢帧通过退出收据/list 对账。
- stop/join 沿用 5 秒，逐环境资源确认另给 5 秒总预算；超时不能提前释放父占位。源码追加核对发现 core `managed_run.rs::stop_and_join` 的 grace 只限制首次等待，abort后的join与children排空没有总预算；计划 Task5 增 side 专用 `stop_and_join_bounded` 并保留超时 owner，其他调用者语义不改。
- 增加 `side_snapshot_lifecycle_v1` 协商；旧无票据 open 拒绝。关闭 flag 时所有 side RPC 仍先 -32601，后台自己的清理不依赖 RPC 可用。

## 第四组：客户端与实现路径一致性

- 桌面 controller.back/selectMain 当前仅导航，必须统一进入销毁协调器；断网关闭记录不含问答或草稿，重连补 exit 在任何 side 恢复之前。
- 迟到 open、readItems、submit 和 notification 均需本次停留授权，不允许未知事件自动创建旧视图。
- StrictMode 订阅模拟卸载不算退出；应用 owner 的最终卸载检查可撤销，真实页面离开仅尽力通知。
- 本机 flag off 不能顺带 back 当前远端 side；Observe 本地离开不取得关闭权。
- TUI side_driver 当前 handle_command.await open 阻塞退出命令；创建改独立 owner task，UI 不凭任意 SideStatus 建视图。
- 核对真实 driver 路径为 `yi-agent-app-server/src/server/driver/turn_driver.rs` 与 `src/server/driver_resume.rs`，不是根目录下的同名文件。

## 计划自检要求

计划覆盖表逐项映射 spec §1–§10/验收矩阵；检查所有 Modify/Test 路径存在，Create 明确标记；接口定义与消费者使用同名同类型；不使用待填内容。实施入口只在用户选择执行方式后启动。

文档检查：相对链接、任务结构、路径清单、placeholder 扫描、`git diff --check`、纯文档范围和项目进度计数。全文件扫描发现进度正文中的 12 处历史失效链接，按 `git show HEAD:<file>` 基线比对，单独记录而不纳入本次修改范围；本轮新增/变更链接不得失效。自动检查结果由本轮实际执行输出为准，不将检查器通过等同于实现验收。
