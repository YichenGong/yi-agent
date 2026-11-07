# Desktop 子 agent 执行面板 plan 复审记录

日期：2026-11-07
范围：[实施计划](../plans/2026-11-07-desktop-subagent-trace-redesign-impl.md)，仅文档，不执行产品实现。
设计基线：[spec](../specs/2026-11-07-desktop-subagent-trace-redesign-design.md)。此前 [spec 复审](../specs/2026-11-07-desktop-subagent-trace-redesign-review.md) 的结论不替代本次 plan 复审。

## 方法与证据边界

- 对照当前隔离 worktree 的源码、类型、测试配置与 spec A01–A12，检查任务依赖、中间提交、接口闭环、竞态和可执行验收。
- 第一轮并行发出三个只读审查，其中界面审查完成；另外两个进入 recovery_required，未交报告，不记为通过。同步完成主审自查。
- 修订后重新发起协议/生命周期与全计划可执行性两路独立只读复审，不把旧稿的结论套到新稿。
- 审查意见先核实，不能机械照收。计划文档检查不等于代码类型检查或测试执行；不宣称 Vitest、Rust 回归、Chrome、WKWebView 或 just ci 已通过。

## 第一轮：确认并修订的问题

| ID | 必修问题与源码依据 | 计划修订 |
|---|---|---|
| P01 | 新 client 建立不等于握手成功。connectionLifecycle.ts::connectSlot 先 registry.put/wireClient；useHandshakeRecovery.ts 在完整握手末尾才设 connected | Task 6/10 增加 setConnectionReady；登记默认为 not-ready，替换旧 client 同步失效旧 owner，握手完成后才允许手动 retry/动作；不自动 watch |
| P02 | raw recovery 只收到 RequestFn，不能归属 client；connectRemoteTargets 另有一处 new RpcClient | Task 10 同步 App/ConnectionLifecycleDeps 的 recoveryRef 为 raw/client/slot，构造时闭包捕获来源；两条创建和握手路径均登记/标就绪，connectionFor 可按确切 client 查询，不读当前活动槽位猜来源 |
| P03 | 新 model 的 text 是摘要，旧 SubagentTrace 及测试依赖 bash(cargo build) 形式，只换 import 会丢工具名 | Task 2 同步适配旧组件 primitive props、name/state_note/raw 显示与测试，保留旧 props/整体展开门到 Task 10，提交前同时验证旧组件而非仅 tsc |
| P04 | React 测试默认环境未全局设为 jsdom，定时与 deferred 用例需要明确清理 | 基线补 DOM 文件环境头、cleanup、显式 imports、每例独立夹具、timer 恢复、pending settle/dispose reject；traceRow 具名返回 AgentTraceRow 并拒绝不可序列化 payload |
| P05 | isFollowing 是读 ref 的函数，不触发浮钮重绘；useStickToBottom.ts 返回接口没有通知订阅 | Task 8 本栏 hook 维护 following state，在共享 scroll listener 后读取意图；命令包装同步 state，不修改共享 hook |
| P06 | loading 空数组不能先初始化窗口；首次超时 error→ready 仍需首次定位，但 ready 后重试不能抢阅读 | Task 8 首次成功 ready 原子初始化最近200与布局后定位，成功空快照也记录；此前失败不算成功，此后刷新不重置窗口；补对应红绿用例 |
| P07 | TraceEvent 本地展开与父层 preserve 没有接口，异步 ResizeObserver 发生后才采锚点太晚 | Task 7/8 定义 TracePreserve，事件展开和 after 子任务折叠在 setState 前捕获；持续缓存 pre-layout anchor、观察内容/视口，暂停关闭原生 anchoring，程序 scroll 不误恢复跟随 |
| P08 | 完整信息导航只有方法名，没有信息区/标题/返回入口的 DOM 契约 | Task 8 定义 taskInfoRef/HeadingRef 和 handle 入口参数；Timeline 内部收起恢复独立保存锚点，仅当焦点处于卸载区才还给入口；只滚本栏，不牵动主对话 |
| P09 | preview 公开数据没有原 client 信息；断开不能让旧 token 借新连接提交，标断开也不等于底层写已结束 | Task 5 内部 confirmation 保存 client/connectionGeneration/target/previewEpoch；确认匹配原来源，断开立即废预览，但 pending 守卫等实际 settle 后再解锁；preview/confirm 的可见代际失效后，迟到回包只解原锁，不给重新打开的同 key 视图发布旧提示 |
| P10 | StrictMode “可重建或可恢复”未选具体生命周期策略，容易永久 dispose 后复用 | Task 6 固定同步 close + epoch/microtask 最终 dispose，StrictMode 重挂取消最终销毁；实际卸载清资源，不 dispose 主连接 |
| P11 | 仅 Tab 点击清 preview 会漏 ui/gitDiff/focus；router 目前直接 sess.setDetailTab | Task 10 增加统一 changeDetailTab 及显式依赖，UI/程序跳转都先 tabChanged 再设 Tab，保 watch/选择；定向测试保护 |
| P12 | useImeGuard 注释要求失焦复位；仅 compositionEnd 接线可能让后续 Enter 永久失效 | Task 9 明确 textarea onBlur=resetComposition，补 compositionStart→blur→focus→Enter 用例 |
| P13 | prose 内层固定样式可能盖外层字号；几何验收只有 boolean 名称容易假通过或靠裁剪掩盖溢出 | Task 7 本栏 wrapper 限定 .prose 字号继承；Task 11 定义真实 textarea/动作 rect、遮挡/裁剪/聚焦判据、正文 clientHeight 与非局部内容越界检查，不把隐藏内容算通过 |
| P14 | Timeline 与详情层空态责任模糊，错误时可能重复文案或藏历史 | Task 8/9 定义 status 唯一来源、after(preserve) 与同一滚动区；带历史的 loading/error 仍显示 blocks，Timeline 不另造空态 |
| P15 | mergeTraceEvent 接收 readonly rows 又承诺重复时返回旧引用，不能声明可写数组返回；状态表索引也不能误中 constructor 等继承属性 | §0 返回 readonly rows，与 snapshot 契约一致；Task 1 使用 own-property 查询并加原型名未知状态边界 |
| P16 | 实际 git check-ignore 表明 desktop/.artifacts 不被根目录规则覆盖，不能照审查报告假定它被忽略 | Task 11 默认报告改为仓库根 .artifacts/desktop-subagent-trace/report.json，并指定可执行忽略检查命令 |
| P17 | 完整信息入口卸载后回标题需要跨组件回调，同 client 重叠握手也不能仅靠连接代际判 ready | Task 8/9 明确 onFallbackFocus 从详情标题 ref 传至 hook；Task 10 start 返回握手 epoch，ready 只接纳该 client 最新 epoch |

## 不照搬的审查建议

1. 不在 phase 每次离开 ready 时重置首次滚动。那会让刷新抢阅读，违背 spec §4.3；只在同 identity 首次成功或切任务/Tab重挂时定位。
2. 不为当前没有尾部淘汰的渐进窗口提升所有事件 expanded 状态。既有稳定 key 足以保留追加/载早后的本地状态；本次补的是展开前 preserve 的接口，不新增全局展开状态表。
3. MarkdownText 自己已包含 prose-img:max-w-full，不把“本栏不套 prose 导致图片没有宽度约束”当源码事实。局部 img 规则只是显式防御；真正缺口是 wrapper 内层字号继承与异步高度锚点。
4. 不要求把 restoreTaskInfo 公开给头部；收起按钮位于 Timeline 的完整信息区，内部调用即可，头部只需 showTaskInfo(entry) 及返回焦点契约。
5. 不把现有 children/list fake 必然变空当已证缺陷；保留该测试前置并明确不删 refreshSubagents，防迁移误破坏即可。

## 第二轮与最终收敛

修订后，全计划可执行性独立复审重新读取当前稿，明确返回 **No unresolved must-fix（对已核查范围）**。它核实了 Task 2 旧组件过渡适配、Task 9→10 可编译迁移边界、readonly 类型契约、两处 client 创建及 source-aware recovery、握手 epoch/显式依赖、统一 Tab 入口、Rust watcher helper 结构可行性、首次窗口/刷新阅读规则、following state、完整信息焦点回退、报告目录及链接。

额外协议专项在多次要求收敛后仍未交付，最终取消；等待接口还返回 Resource temporarily unavailable。它不计为审查通过、不引用不存在的报告。主审继续对照 spec 与 worktree 源码完成订阅栅栏、动作记录和连接生命周期自查。收敛后再明确了 P09 的取消回执可见代际规则，补 A→B→A 用例；未改产品方向或 RPC，最终自查未发现新的必修项。

独立审查没有执行编译/单测；CDP 实现细节、Tokio 夹具实际编译和 WKWebView 都属于实施验证，不在本次文档审查中伪称已证实。结论是**当前 plan 没有已知未解决的必修问题，可以作为实施输入**，不是保证未来代码没有缺陷。

## 实际文档验证

- `git diff --check`：通过。
- Python 文档检查：11 个任务按序、每个五个检查点共55个；A01–A12映射、相对链接、代码围栏、无 TODO/TBD/占位、最终接口关键字检查：通过。
- 项目管理条目保持 `[ ]`，desktop 索引保持 `121 / 137`：通过。
- `cd yi-agent-rs && cargo fmt --all`：通过，未产生源码变更。
- `git check-ignore -v .artifacts/desktop-subagent-trace/report.json`：命中仓库根 `/.artifacts/`；`desktop/.artifacts` 未被忽略的问题已修正。
- 当前 worktree 没有 Cargo target，无本轮编译产物需要清理。

本次只有文档变更，按项目例外未跑产品 CI/单测；没有运行 Vitest、tsc、Rust 回归、Chrome 或原生 WKWebView，不声称这些通过。不合并 main，不开始产品实现。
