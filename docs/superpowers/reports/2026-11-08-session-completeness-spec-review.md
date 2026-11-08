# Session 内容完整性 Spec 三轮复核记录

日期：2026-11-08。范围：[Spec rev2](../specs/2026-11-08-session-completeness-repair-design.md) 与配套 [数据/总入口 Plan](../plans/2026-11-08-session-completeness-repair-impl.md)、[显示 Plan](../plans/2026-11-08-session-display-convergence-impl.md)。

## 结论与证据等级

经过代码对照、并发/失败推演、反例及文档一致性检查，修订后**未发现仍阻塞编写 Plan 的设计问题**。这是本会话的多轮自审，不是独立审查者意见，不保证实施中不会发现新问题。

本轮没有修实现、运行修复单测、启动生产 app 或做真机验收。首次补偿疑点仍须在实施时取得稳定判别性红测试；所有 D01–D09、S01–S12、R01–R05 和部署门禁仍未执行。只读诊断及旧464passed不能代替新矩阵。

基线：文档分支第一版 commit `7a2534a6`，实施代码基于当时 `eda677b7`；本轮对照主分支 `d3031786`，相关 `desktop/src`、app-server/src、Tauri bridge 与 `eda677b7` 无差异。之后若 main 漂移，实施前重新校对。

## 第一轮：代码对照（可实施性与兼容）

| 发现 | 仓库证据 | rev2修订及Plan落点 |
| --- | --- | --- |
| reasoning delta没有稳定server item ID，终项才分配 | `translate.rs:437–463`，`protocol.rs` ReasoningDelta | 首delta预分配ID，v1 sync带ID，最终重用；旧params不改。B1/B3/B8 |
| 旧load容错跳坏行/partial，且会读模型messages | `thread_store.rs` TurnLine/PartialTurn/LoadedThread、load/read_log/read_partial | 严格展示材料DTO，IgnoredAny跳messages，坏已提交材料明确失败。B2 |
| 磁盘主日志与partial不是只凭一次load就能证明一致 | `thread_store.rs` append_turn/write_partial/clear_partial/truncate/delete | 共享root/thread mutation revision+在途守卫+前后指纹，最多3捕获；外部writer受限。B2/B5 |
| WS及普通队列上限已有1MiB/256/16，不能无限加大 | `protocol.rs` MAX_FRAME_BYTES，`broadcast.rs` CLIENT_QUEUE/MAX_SUBSCRIPTIONS | UTF8全envelope计量，所有帧/材料/编码先预算准入；超限不截断冒充完整。B4/B5/B9 |
| 本地默认Feed::All，旁问与LRU均直接整体替换订阅 | `broadcast.rs` Feed，`App.tsx`普通/旁问/重连订阅 | 单个SubscriptionCoordinator，local all、remote holds+LRU≤16，ACKgeneration。B7/B10 |
| Tauri listen异步但request可能先发，无读超时 | `desktop/src/tauriTransport.ts`、`lib/rpc.ts` | ready门、只读10秒、迟到响应有界清理、dispose清timer。B6 |
| children watcher只有既有权威列表IPC，不存在轻量daemon水位方法 | `server/watchers.rs:25`、`rpc/agent.rs:35` | 复用500ms源/最近成功时间，后台查询，includeSync水位；失败不是fresh空表。B11 |
| cold idle阅读旧流程可准备执行上下文；正文就绪不等于agent就绪 | `App.tsx` select/send，`rpc/turn.rs` | v1读取无控制动作；显式发送保留上下文准备/epoch约束；显示同步与发送gate分开。B10 |

表中Rust路径均相对 `yi-agent-rs/crates/yi-agent-app-server/src/`。这些是现有实现证据，不是新增功能已实现。

## 第二轮：并发时序与故障推演

| 反例 | 修订后的不变量 | 任务/判据 |
| --- | --- | --- |
| 投影被回收又重建，epoch不变而seq从头，旧客户端误判重复 | view_generation独立UUID；正文页/probe/seq都带generation并校验 | B3/B5/B9，S08 |
| 两producer先分配seq后分别await广播造成倒序 | apply→seq→提交发布意图同串行短临界区，网络泵外置 | B3/B4，S05/R01 |
| 捕获时coalescer仍有跨snapshot边界范围，正常流式不断产生部分重叠 | 捕获seal/flush旧range，不等网络；后续从snapshot_seq+1 | B3/B4/B5/B9，S05 |
| 新订阅帧先于ACK到达，ACK后才缓存丢第一帧 | listener/cache同步先建，再请求/ready/ACK，捕获在ACK后 | B6/B7/B9，S01/R03 |
| 安装期间再次dirty，finally清掉更新的缺口 | dirtyThrough+本地失效revision，仅清快照覆盖部分 | B4/B9，S07 |
| 每5秒更高probe重置重试预算，长期生成导致无限大快照 | initial+3 retries，总4捕获/120秒；新seq只提高目标，不reset | B9，S07/R04 |
| token最后一页自动删除，客户端没机会ACK安装或取消释放 | release幂等，installed仅确认该seq；finally release+TTL/disconnect | B5/B9，S06/R02 |
| 本地慢端满队列，新增无限send await卡取消；另设无限意图队列爆内存 | 独立有界出口，Full留dirty元信息；16超限连接级失效；WS10秒断线 | B4/B12，R01/R02 |
| 旧客户端不识别invalidated，新增完成项满队列又不能无限缓存 | 保留旧关键背压；新增完成路径独立有界，准入失败明确断线 | B4/B12，R01/S10 |
| 读取期间又发同一句，新快照按文本匹配删新乐观问句 | local提交ID绑定start/interject响应serverID；未知项保留，草稿不清 | B8/B10，S09 |
| 返回权威redacted终项时merge较长文本泄露先前reasoning | v1终项权威替换，不较长优先；不输出凭证 | B3/B8，S03/R05 |
| A→B→A或两宿主同threadID，旧列表/正文回包覆盖新面板 | source/connection/request generation+revision；layoutOwner含宿主binding | A3/A4/B9/B11，D04/S08/S12 |

## 第三轮：显示反例、预算与文档自审

- 真实相交可视范围与历史首末边界分开，tail不吞历史bottom gap；绝对index直接由DOM属性提供，不再二次base换算（A1）。
- post-commit补测受单方向cycle和 `ceil(N/200)+2` 预算约束；null布局暂停，不持续rAF；无进展最多一次focus后显式stalled（A2/A4）。
- maxMounted400包括tail，tail脱离窗口时保留1名额；逻辑到尾不能重启following（A1/A3）。
- 单个位置写入者，区分viewport-relative offset与内容坐标anchorTop；上方/下方增高不能统一用总高度差；恢复需可容纳才消费pending（A3）。
- heightRevision/布局owner/每次布局几何维护和mounted-row observer分工明确；静止不空转，hidden恢复显式触发（A4）。
- 显示和数据分别写Plan，App/ChatView共享修改点串行集成；所有26个验收ID可追溯到Task（A5/B12）。
- 把“两个只读方法”修为read/probe/release三个；§5.5顺序与§5.4缓存前置一致；§5.1/§5.4补generation；每页带一致source，外部/无法证明归属的活动writer不能先synced再等probe；旧协议可靠满队列边界明确，避免无界缓存。
- 自审时修正路径：useStickToBottom在lib，RPC在thread/content，driver命令/落盘在server/turn/{command,persist}.rs，WS/scoped出口在transport既有模块，不存在独立websocket.rs；Harness不暴露threads，示例用真实probe。所有新接口标明新增而非已有API。
- 编写Plan时的测试代码是未来实施起点，**没有编译执行这些示例**。实际类型变化需在Task开始重新对照；导入/空壳编译错误不能代替行为红证据。

## 本轮验证记录

1. 运行设计时序模型：连续/重复/部分重叠/缺口、snapshot seal、dirty水位、重试总4次且高seq不reset、generation换代、400tail预算、CJK UTF8计量，**11项断言通过**。这是纯设计反例检查，不是生产代码测试。
2. 最终静态校验输出：290个可解析相对链接，新增失效0；进度文档中16个既有失效链接未改动（不把它们算本轮新增问题）。显示Plan 5 Tasks/25个未执行步骤，数据Plan 12 Tasks/60个未执行步骤；26个D/S/R验收ID全部映射。desktop计数131/151（20未完成、1已放弃不计总数），app-server86/88（2未完成），索引一致；`git diff --check` exit0。
3. 仅Markdown改动，补工具链PATH后 `cargo fmt --all` exit0；随后 `git diff --exit-code -- yi-agent-rs desktop` exit0，确认格式化没留下实现diff。没有运行just ci/实现单测，也没有替换正式app。
4. 首次校验脚本因旧文档跨行括号误解析失败，首次fmt因cargo-fmt不在PATH失败；修正单行链接解析/明确忽略既有失效及工具链PATH后重跑成功。上述是校验环境问题，不是修复实现测试结果。

## 未完成门禁（不是遗留设计问题）

- [ ] 首次补偿D05形成真实几何、单变量可回退即红的用例。
- [ ] A/B任务各自红绿、预算压力、全D/S/R与变异验证。
- [ ] 完整stdio/Tauri/WS/relay链路透传与慢端取消响应证据。
- [ ] WKWebView/iOS真机及独立实例验收，保持生产配对不变。
- [ ] 实施提交及用户授权合并后全CI；本轮不自动实施、合并、推送或部署。
