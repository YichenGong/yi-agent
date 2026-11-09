# `/compact` 期间跨会话 Send 隔离修复设计

日期：2026-11-09

状态：草案，待用户审阅；本轮只写 spec，未实施新的生产修复。

代码证据基线：`dcfad0d6`。此前两次修复已经合入，但用户重新 build 后仍复现，**本问题未解决**。

## 1. 用户场景与证据边界

### 1.1 必须解决的原始场景

用户在当前 macOS App 的会话 A 输入 `/compact`，**点击 Send**。A 提交进入等待后，切换侧栏已有会话 B，或刚新建会话 C，输入非空普通消息，Send 都不能点击。启动后用户**没有关闭过旁问实验开关**。用户确认重新 build 仍复现，不能再用旧构建解释问题。

目标不是“后台请求最终能返回”，而是：A 压缩未完成时，B/C 的编辑器立即按它们自己的状态判定可发送，点击后请求进入正确会话并获服务端受理。A 自己保持防重复提交。

### 1.2 已确认事实与未确认部分

| 证据 | 结论 | 不能推出的结论 |
|---|---|---|
| 用户重建 App 后反馈；开关未关闭；已有与新建均失败 | 原始问题仍存在，用户操作路径必须纳入验收 | 不能据此指定某一个内部变量就是根因 |
| `bf280ccb`（合并 `4a1d055a`）把 MessageInput 的 submitting 改为按 conversationKey 的集合，并调整 selectMain 早退 | 已移除一处全局布尔锁；需要继续验证 key 的正确性 | Set 的存在不证明不同会话拿到了不同 key |
| `4bba732b`（合并 `dcfad0d6`）把 compact 的 reply 等待移出单 task RPC 主循环 | 已修复 provider 长等待堵住后续 RPC 的独立缺陷 | `thread/rename` 回包不证明 App 的 Send 可点、B 的 turn 已受理 |
| 本轮真实 App React 组件行为测试：旁问 capability 开启，先 t1→t2→t1，再点击 compact Send；压缩期间新建 t3 | 输入非空但 Send.disabled=true，可稳定重现一个前端身份缺陷 | 不能覆盖用户已有会话也失败的全部条件 |
| 相同准备，关闭旁问开关后再压缩，侧栏切 t2 | Send.disabled=true，是另一个已确认缺陷 | 用户没有关闭过开关，不能拿它替代原始复现 |
| 相同准备，开关不变，压缩期间普通侧栏切 t2 | 测试通过，Send 可点，turn/start 指向 t2 | mock RPC 下通过不等于生产 App 路径通过 |

本轮新增测试位于调查分支 `fix/compact-composer-identity` 的 `desktop/src/App.test.tsx`，名称为 `isolates a clicked /compact submission across %s navigation`。执行记录：

```sh
cd desktop
npx vitest run src/App.test.tsx -t 'isolates a clicked /compact' --reporter=dot
# 当前调查分支：1 passed / 2 failed / 291 skipped；连续两次结果相同。
```

这些测试目前保存在 `.worktrees/fix-compact-composer-identity` 调查工作区，尚未提交或合入基线；上述命令需在该 worktree 的 desktop 目录执行。它们使用真实 App/输入框/导航组件，但 mock RPC，不是 WKWebView + Tauri + sidecar 端到端证据。本轮真实 App UI 自动化因辅助访问权限不足（`-1719`）未执行；不得把未执行记为通过。

**尚未证实**：开关始终开启时，生产 App 普通已有会话为何仍被禁用。实现前必须补齐 §4 的证据，不能把两个已确认的局部缺陷当作完整根因。

## 2. 当前结构与可验证的缺口

以下行号对应上述基线，后续以符号名定位。

- `desktop/src/App.tsx:1010`：主会话正文、草稿读 `sess.currentId` 对应的 store view。
- `App.tsx:2858–2870`：composer key 优先取 `sideConversation.active` 的 main `hostKey/parentId`；指针为空才取 `activeSlotRef.current/sess.currentId`。同一主会话有两种编码来源。
- `App.tsx:2886–2910` 与 `lib/currentConversation.ts`：名义上是唯一会话投影，但 main parentId 仍优先读旁问控制器，可能与正文的 sess.currentId 不一致。
- `App.tsx:1212–1243`：侧栏 selectThread 更新 store/sess；只有找到旁问 binding 才调用 selectMain。
- `App.tsx:1571–1598`：newThread 直接更新 store/sess，没有同步旁问控制器 main 指针。
- `App.tsx:943–972`：关旁问开关先 back 再 dropHost；旧 main 指针可能留下，之后无 binding 的导航不会更新它。此缺陷是补充回归，不是用户主场景。
- `hooks/useSideConversationController.ts:599`：selectMain 的 main 早退比较 parentId，未比较 hostKey。跨宿主同 id 是待测试风险，不是已实测根因。
- `components/MessageInput.tsx:197–208`：锁按 key 占用；finally 用提交时捕获的 owner 释放。这个配对原则应保留，重点检查 owner 是否指向用户正在发送的会话。
- `MessageInput.tsx:407`：Send 禁用为五项条件的或：外部 disabled、当前 key sending（非旁问）、内容与附件均空、策略 stop、策略 blocked。必须分别观测，而不是只看到灰色就认定锁。
- `lib/rpc.ts:191–213`：pending 按请求 id；Tauri `bridge.rs:302` 的 rpc 负责写帧，不等待业务完成。尚无证据表明这里存在另一个 compact 全局等待锁。
- app-server `rpc/thread/settings/mod.rs:352–430`：reply_rx.await 已 detach，session_tx.send 仍在主循环 await。队列拥塞是否另有阻塞需要证据，不先扩大后端改动。

已经可以解释的机制：正文切到 C，但旁问 main 指针仍为 A，composer key 继续命中 A 的在途锁。这个解释覆盖新建红测试，**暂不覆盖用户已有会话失败的生产路径**。

## 3. 范围、方案与决策

### 3.1 目标与非目标

目标：定位完整生产场景；消除正文、草稿、提交目标与锁 owner 的身份分歧；保留同会话去重、旁问隔离和晚到结果隔离；验证真正的跨会话发送受理。

非目标：重写全局 session store、重做 compact 算法、改变协议、替换全部 RPC 调度、增加自动重发或强制压缩超时、重做旁问 UI、改变移动端运行中 Send/Stop 策略。本轮不构建或操作用户正在使用的正式 App。

### 3.2 三种方案

| 方案 | 优点 | 风险/取舍 |
|---|---|---|
| A：逐个导航入口补 selectMain、关闭开关清指针 | 改动小，能直接针对两个红测试 | 身份仍有多份来源；冷启动、无 binding、跨 slot 等入口继续依赖人工同步；无法解释完整用户反馈 |
| B：先诊断完整链路，再有界收口当前会话投影与提交 owner | 从消费者侧消除错误身份依赖；可独立验证；保留既有 store/controller | 需要梳理本次涉及的导航与晚到结果；不能在诊断缺失时宣称已解决 |
| C：重写 App 全局导航/会话状态机 | 理论上能集中状态 | 范围过大，回归风险高，与本次修复不成比例 |

推荐 **B**。先完成 §4 的诊断门禁，再按实测分支确认修复位置。如果生产证据指向另一个禁用条件或 IPC 阻塞，必须修订本 spec 的根因与范围再进入对应实现；不能无视证据强行只改 key。A 的局部修补可作为 B 的内部必要改动，不能作为结案依据。

## 4. 诊断阶段与实施准入

### 4.1 最小观测记录

在隔离诊断构建中显式启用本次观测，默认关闭。记录限制为本次运行内存环形缓冲最近 200 条；用户显式导出后才生成脱敏文件，不上传，不持久化消息正文。复用已有 trace 能覆盖的事件，不新增全局遥测系统。

一次 A compact → B/C 导航 → 发送尝试应关联下列事实：

1. 当前 slot、正文 thread、草稿 owner、CurrentConversation kind/owner、旁问 active kind/owner、当前 composer key、在途提交 owner。
2. 禁用理由集合（五项分别记录，非单一首个理由）、最终 sendDisabled、已提交 DOM 的 button.disabled；内容只记录“是否非空/是否有附件”。
3. 导航来源（冷启动恢复/侧栏已有/新建/宿主切换/旁问返回）、导航 revision、binding 是否存在、flag 值与 capability 是否可用。
4. 锁 acquire/release、提交 operation token、方法名、客户端请求 id；RPC 发起、transport.send 完成或失败、Tauri 写帧、server 收件/受理/回包。
5. A provider 进入等待及释放的时间点；B turn/start 的 thread 与受理事件，避免只观测请求被写入。

导出用运行内别名替代 thread/host/slot id，保留相等关系和顺序；不记录完整路径、relay 参数、鉴权 token/secret、消息或附件内容。operation token 仅为本地非鉴权操作编号。客户端与服务端采用同一导出映射关联请求。DOM disabled=true 时原生 click 不会发出，不能等待 click handler 才采集状态，必须在相关渲染提交与导航完成时采样。

### 4.2 根据证据分流

| 观测 | 下一步 |
|---|---|
| B/C 内容非空，sending=true，key/owner 仍为 A | 将具体初始化/导航路径固化为行为红测试，进入 §5 身份收口 |
| B/C 外部 disabled 或 policy blocked/stop 为真 | 找出是哪份会话状态造成条件，补对应红测试；不直接删禁用条件 |
| React 计算可发送但 DOM disabled=true | 检查实际挂载输入框、render/commit 与实例关系，先取得最小 DOM 复现 |
| DOM 可发送，点击没有客户端 request | 排查事件/提交准入/异常路径，不能再称为单纯按钮 disabled |
| request 已发而桥/server/受理缺失 | 沿实际缺失的一跳排查；包含队列投递等待，但不先假定主循环仍堵 |

准入要求：在开关始终开启、点击 Send、已有与新建都覆盖的实际 App 路径捕获失败，写出可观测因果链，并把能模拟的触发条件固化为失败测试。无需索取用户私密历史；允许隔离实例使用合成足够长的历史与可控 provider。若隔离实例不复现，征得用户同意后只在其复现时采集脱敏状态，不擅自重启或操作其会话。

只获两个局部红测试时，可以规划调查和已确认缺陷的工作，但**不得把原始问题标记为根因已查明或修复完成**。用户审阅 spec 后的计划首先安排诊断门禁；生产修复按上述证据准入。

## 5. 推荐的有界身份与提交设计

本节是推荐方案的设计约束，最终修复是否足以解决完整问题受 §4 约束。

### 5.1 当前会话的单一投影

主会话以当前可见宿主 slot 与选中 thread 为权威来源，不再从旁问控制器的历史 main parentId 推导 composer owner。复用 `CurrentConversation`，使正文、草稿、输入策略、提交目标、锁 key 从同一份已验证投影产生。

- main：当前 slot 的稳定连接身份 + 当前选中 thread；未选中/当前 store view 不存在时不可发送，不生成可提交 owner。
- side：继续要求精确 hostKey、parentId、sideId、generation 与 store view 一致，关闭或换代进入 lost。
- lost：显示原因，禁止发送；绝不自动 fallback 到父会话。

为 main 的连接身份提供一处稳定编码出口。同一 slot 在旁问 binding 首次出现、消失或 flag 切换时 key **不能改变**；不同 slot 上相同 thread id 必须不同。连接替换/重配用当前 RPC client 的生命周期 epoch 区分旧新 owner，普通重渲染、binding 加载不能增加 epoch。若现有 slot 已具备等价生命周期身份，复用它，不再创建第二套连接 registry。epoch 只用于客户端本地隔离，不改线协议。

旁问保持既有完整身份；若 hostKey 在新连接可重复，则同样纳入连接 epoch。main 与 side key 前缀不同，不共用锁。所有消费 composer key 的旁问锚点/结构版本等调用点需逐一核对，不能为了 main 改 key 破坏旁问代际隔离。

### 5.2 导航与控制器职责

应用一个有界的主会话选择入口，用于本次涉及的侧栏、创建成功、冷启动恢复、删除当前会话与 slot 切换。它更新当前选择，并同步使旧旁问导航失效；无 binding 时也必须推进 navigationRevision，不能靠“等 binding 出来再 selectMain”保证安全。

旁问控制器继续负责打开、进入、返回旁问及异步 open 竞争；其 main 指针可服务返回信息，但不得作为可见 main 的锁或提交目标权威。selectMain 的幂等判断须比较完整宿主与 parent 身份。不能仅在渲染中无条件同步 controller 造成 effect 循环。

进入 B/C 后，旧 A 的 pending side/open 响应不得把界面拉回 A。关闭 flag 只撤销旁问视图/绑定，不改变当前 main 的身份、不清全部提交锁。当前 thread 删除后清选择并明确不可发送；不要借此解开别的会话操作。

### 5.3 提交 owner 与生命周期

点击 Send 或 Enter 的同步准入阶段捕获完整 owner、目标 RPC client、提交内容版本与 operation token，再进行任何 await。当前 owner 已占用时拒绝重复请求；其他 owner 不受影响。

MessageInput 保留按 owner 的在途集合原则。完成、失败的释放必须核对自己那次 operation token；只释放捕获 owner，不读回调时的“当前会话”。正常路径若无需改变现有 Set，可保留现有实现；清理/重连后允许相同业务会话再次提交时，必须有 epoch/token 防晚到旧 finally 释放新锁。

草稿只在提交对应 owner 且版本/内容仍符合清理条件时清理。A 回包只更新 A 的 compact notice/history，不清 B 的文字、附件，不解 B 的锁，不改变当前导航。side 的目标换代/关闭后结果不进入新代或父会话。

断线 reject 该 client/epoch 的 pending，并清理对应本地等待；其他活跃连接不受影响。重连后旧结果不作用于新 epoch。compact 的服务端操作可能仍在执行，所以 UI 不能把断线或停止等待说成“已取消压缩”，也不能自动重发 compact。重新发送只由用户动作发起，继续接受服务端忙碌/拒绝校验。

### 5.4 禁用理由保持一处计算

MessageInput 从同一份计算同时获得理由集合、最终 disabled 与诊断值，不再复制五项条件。空输入、无会话、旁问 lost、现有策略限制仍是合法禁用；此修复不是把所有 Send 永远启用。

A compact 的提交锁只影响 A 的 Send；输入框仍可编辑，导航不禁用。保留桌面 main 运行中插话、移动端运行中 Stop、side 运行中 Stop 与 `/back` 的现有语义。`failed`、`not_reduced`、RPC error 均结束本次前端等待并显示 A 的真实结果，不冒充成功。

## 6. 测试矩阵与判别性要求

每条跨会话测试必须保持 A 的 compact pending，先确认 A Send 被其自身锁住，再在目的会话输入非空消息，断言按钮可用并点击，核对真实组件产生的 turn/start 目标。不只测纯 key helper，不用 rename 代替发送，不提前释放 A。

| 编号 | 场景与必须断言 |
|---|---|
| U01 | main 身份：binding 未加载→加载→消失/flag变化不换 owner；不同 slot 同 id 不同 owner；连接替换 epoch 不同 |
| U02 | 单一投影：main view/draft/policy/提交目标/key 一致；side 精确匹配可用；closed/换代为 lost，无 parent fallback |
| U03 | 同 owner 双击、重复 Enter 与同 tick 点击/Enter 混用仅一请求；A/B 两个 owner 可同时占用；旧 token 不能释放新操作 |
| U04 | A 晚到 success/failed/not_reduced/RPC reject 不清 B 的文字/附件，不解 B 锁；只更新 A；断线后旧 epoch 回包不落新会话 |
| A01 | capability/flag 开启且启动后不拨开关：冷启动直接 compact，切已有 B，点击发送；还需覆盖导航建立 main 指针后的相同路径 |
| A02 | 同 A01 条件：compact 后新建 C；覆盖无 workspace 的 native picker 和已有 workspace 直接创建；C 可发且目标正确 |
| A03 | compact 后多次 A→B→C→A；B/C 可发，回 A 仍防重复，A 结束后只有其锁解除 |
| A04 | 保留已确认 new、flag-off 红测试；另测 capability 不支持与开关切回；这些都是补充矩阵，不替代 A01/A02 |
| A05 | Click 与 Enter 均走实际 MessageInput；保留 textarea 编辑、空稿禁用、side `/back`/Stop、mobile Stop 语义 |
| A06 | pending side open 后选已有/新建/无 binding 的 main；晚到 open 不拉回旧父；跨 slot 相同 parentId 不触发错误早退 |
| A07 | 删除 A/切 slot/断线重连时只清对应 owner；当前投影可发送性准确；旧请求不能污染新 owner |
| R01 | 可控 provider 确认 A 已进入摘要长等待；真实 app-server 同一连接上 B turn/start 在释放 A 之前获受理，响应/事件属于 B |
| R02 | A/B 各自 compact 在途时，空闲 C 可发送；A/B 结束顺序相反也各自收尾。若生产确有额外队列阻塞，再增精确拥塞红测试 |

当前事实：A04 的两个局部用例为红；一个 warm-sidebar 用例为绿。A01 全条件、完整 A02 及 R01/R02 的 Send 链路尚未完成。

判别性：针对实际根因单点回退，关键测试必须在同一行为断言变红。至少核对“main owner 恢复读旧 controller 指针”会使新建用例红、“导航 revision 无 binding 时不推进”会使晚到 open 用例红。不得只修改 mock 返回值制造失败。原先后端 rename 测试作为独立回归保留，不能充当 R01。

实现后的常规门禁：`cd desktop && npx vitest run && npx tsc --noEmit && npm run build`；按变更范围运行 controller/MessageInput/App/RPC 定向测试。涉及 Tauri 时在 `desktop/src-tauri` 跑格式、lint、相关测试；涉及 Rust workspace 时先按 crate 验证，再在 `yi-agent-rs` 执行 `cargo fmt --all && just ci`，合并后再跑 `just ci`。纯文档提交免代码 CI。

## 7. 真实 App 验收与完成条件

独立 macOS App 使用独立 bundle id 与 `YI_AGENT_HOME`，不覆盖正式 App，不共享 relay/运行状态；按项目打包流程生成带前端与匹配 sidecar 的正式 bundle。记录 build 标识仅为证据关联，不再归因用户使用旧构建。

验收使用真实 WKWebView、MessageInput、导航、Tauri bridge、stdio app-server 与合成历史。可控 provider fixture 只替代 LLM 远端，把 A 挂在真实摘要等待处，其余链路不 mock。无需真实 API 消耗即可确认跨会话受理。需要辅助访问时先取得权限/用户许可；无法驱动 UI 时人工执行并保留同样证据，未执行如实标记 UNEXECUTED。

| 编号 | 必须取得的验收证据 | 当前状态 |
|---|---|---|
| M01 | 启动后旁问 flag 持续开启，A 点击 compact Send；已有 B 输入非空，button.disabled=false，点击后的 turn 在 A provider 释放前受理 | UNEXECUTED |
| M02 | 同一 pending 窗口新建 C，确认正文/草稿/owner/请求目标均为 C，Send 可点且在 A 释放前受理 | UNEXECUTED |
| M03 | 重复 warm 导航、返回 A、再往 B/C；A 自己不能重复 compact，B/C 各自独立；A 回包不清 B/C 草稿 | UNEXECUTED |
| M04 | side 返回、无 binding、flag 切换、slot 切换和重连补充验收；lost 不落父，新 epoch 不受旧回包影响 | UNEXECUTED |

M01/M02 是用户主场景，不能降为可选项，也不能用“B 最终在 A 完成后发出”替代“在 A 未完成时受理”。测试等待预算用于限定测试失败，不给生产 compact 增加强制超时。

完成须同时满足：

1. 完整用户场景的失败证据与因果链已经记录，相关回归在修复前红、修复后绿。
2. §6 适用矩阵与代码门禁通过，生产修复范围与诊断结论一致。
3. M01–M04 真实 App 验收通过，证据明确包含 A 未完成时间窗与 B/C 的正确目标及受理。
4. 更新项目进度并引用验证命令/记录。任何主场景或真实 App 验收未执行，feature 保持 `[ ]`，不得宣称“修复已生效”。

## 8. 本轮文档状态

本 spec 只定义调查准入、推荐设计与完成判据。已有两次修复和局部红/绿测试如实保留，不新增第三次生产补丁。待用户审阅后再写实施计划，计划必须把 §4 的完整场景诊断放在生产修复之前。
