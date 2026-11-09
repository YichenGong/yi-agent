# Session 内容不完整／切回后增加：诊断记录

日期：2026-11-08
基线：`main` / `a0b6f9dc`。
隔离分支：`fix/diagnose-session-transcript`（已删除，其未提交材料见文末「材料位置」）。
范围：诊断，不是修复；未修改生产实现、未替换或重启正式 app，未提交或合并。

## 结论与证据等级

### 1. 已复现：历史窗口补一页后停住，数据存在但视口空白

实验使用真实 `Session`、`ChatView`、窗口 hook 和 CSS，Chrome headless，无后端、网络和 LLM。

条件：600 条消息，每条有 8 行追加文本；进入 A、B、再回 A，待布局稳定；将滚动位置移到历史中段。

最新运行结果：

| 指标 | 空白时 | 仅调用一次 `nudge()` 后 |
| --- | --- | --- |
| 内存消息数量 | 600 | 600 |
| scrollTop | 60802 | 60802 |
| 窗口区间（半开） | [340,600) | [140,540) |
| 可视消息 | 无 | A-297 至 A-302 |

空白时首个挂载消息在视口下方 8570px。一次重算恢复，后续三次重算窗口不再改变。

因果链：一次远距离滚动落到未挂载的占位区域 → 仅补前面 200 条 → 新窗口仍未覆盖视口 → 没有新滚动／条数变化／focus，就不继续计算。

代码：
- `desktop/src/hooks/useTranscriptWindow.ts:108-142`：每帧去重及单次计算。
- `desktop/src/hooks/useTranscriptWindow.ts:154-157`：自动触发依赖为 recompute、itemCount，不含窗口自身变化。
- `desktop/src/components/ChatView.tsx:588-600`：scroll 驱动 nudge。
- `desktop/src/components/ChatView.tsx:957-973`：用户发送乐观消息会强制贴底。

适用边界：这个实验解释长历史“内容存在但没显示”，以及发送消息后视口恢复；**没有单独复现“贴底发送后，必须切回才增加最新消息”**。实验中发送、追加消息后再切回，内存数量及可视最新 ID 相同。

反例：全新页面首次渲染预置的 600 条历史，向上滚动请求被补偿拉回底部，未出现同样空白。首次挂载后再填入大量历史也会混入高度估算／补偿变化。因此不能说所有首次进入或所有向上滚动都会触发分页停顿。

首次访问的补偿异常只作观察记录，尚未用单变量干预确定根因：
- `ChatView.tsx:533-534` 的高度簿在组件生命周期内保持，而不是按 thread 重建。
- `ChatView.tsx:628-663` 用前后整体 scrollHeight 差作补偿。
不把这两个代码疑点直接当作已证实现场根因。

### 2. 已复现：首次打开运行中会话，可能漏掉旧历史

真实 App + 可控 RPC 的诊断测试顺序：
1. t2 从未打开，服务端已有旧提问和尾部回答。
2. 正在看 t1 时，t2 的尾部 `item/completed` 正常到达。
3. 打开运行中的 t2。
4. App 发 `thread/readItems({threadId:t2,afterItemId:tail-answer})`。
5. 服务端语义只返回尾部之后，所以旧提问仍未显示。

这不需要发生任何通知丢失：客户端只是先收到新消息，却从未加载历史基线。

代码：
- `desktop/src/lib/session.ts:558,664,682`：upsert／live item／delta 都可能推进 lastServerItemId。
- `desktop/src/App.tsx:1403-1435`：桌面 running 会话依据该 ID 增量读，不 resume。
- `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread/content/mod.rs:312-323`：找到锚点时 skip(i+1)，排除锚点自身及其之前内容。

结论：**最后收到的消息 ID 不是历史完整的证明。** 这个缺陷会产生内容不全，但不保证靠再次切回就能补上。

### 3. 已验证机制：切回会主动补齐，因此可能看到更多正文和状态

第二个 App 诊断测试：先加载提问；随后模拟服务端已保存回答，但客户端未收到该回答；不切会话时没有回答；A→B→A 后，`thread/readItems(afterItemId:question)` 返回回答，回答出现。

这验证了切回补齐机制，**没有证明用户现场发生了丢帧**。

另有确定的状态重拉：
- `desktop/src/App.tsx:1224-1227`：重进会话会重新查询子 agent 与后台进程。
- `desktop/src/App.tsx:1403-1435`：桌面 running 正文增量补齐。
- idle warm 会话 `App.tsx:1437-1441` 仅切视图，不走同样的正文读取。

所以切回并非单纯重新绘制，内容和附属状态可能在此时从服务端补入。

### 4. 有代码依据、未证实现场：普通内容通知可静默丢失

- `broadcast.rs:197-199`：本地 reliable 客户端普通广播用 try_send，忽略满队列结果并保留登记。
- `server/rpc_io.rs:151-179`：Content 走普通广播，List/Global 走带背压广播。
- `server/driver/turn_driver.rs:721-725,794-795`：实时 item/completed 也走 write_notification；完成材料先存到 completed_items。

因此不是只有正文 delta 存在风险，完成项也可能没到前端而已保存在后端。

只读历史包含主日志和可用的运行中 checkpoint：`thread_store.rs:855-910`。它不是“只能等回合全部完成才能读”，但也不是实时内存快照。

现场 trace 里可见的 request failed 是 superpowers-kanban query socket 错误，不是内容读取失败证据。普通满队列丢帧缺乏日志，不能从现有 trace 确认或排除。

## 验证

验证（以下命令引用的脚本已随诊断 worktree 丢弃，**已不可重跑**，保留以备理解当时口径；
现行等价入口见本节末「材料位置」）：

```sh
PATH="/opt/homebrew/bin:$PATH" node desktop/scripts/browser/diagnose-session-check.mjs
PATH="/opt/homebrew/bin:$PATH" desktop/node_modules/.bin/vitest run --root desktop \
  src/App.test.tsx src/lib/session.test.ts src/components/ChatView.test.tsx \
  src/hooks/useTranscriptWindow.test.tsx src/hooks/useNotificationRouter.test.ts
git diff --check
```

结果：
- 真实浏览器脚本最后一轮 exit 0，断言空白存在以及显式重算恢复可视内容。
- 5 个测试文件、464 个测试通过，含两个新诊断用例。
- 新用例故意断言当前缺陷／补齐行为，不是修复后的回归绿灯；未来修复前应转换为正确行为的失败测试。
- 未运行 full CI 或 fresh Rust build；未修改 Rust 或生产前端逻辑。

材料位置：**材料未保留，已由 `session-completeness*` 取代。**

诊断当时用的 `diagnose-session.html` / `diagnose-session.tsx` /
`diagnose-session-check.mjs` 与 `App.test.tsx` 里的 `session completeness diagnostic`
suite 都是未提交的草稿，随隔离 worktree `fix/diagnose-session-transcript` 一并丢弃。
它们已被正式版取代：

- `desktop/scripts/browser/session-completeness.tsx`（5,787 B，草稿为 3,953 B）
- `desktop/scripts/browser/session-completeness-check.mjs`（25,087 B，草稿为 5,783 B）
- `desktop/scripts/browser/session-completeness.html`
- `docs/testing/session-completeness.md`

正式版由 `a57516a4`（`test: cover session display convergence in real browsers`）
落地，代码量约为草稿的四倍，且带正式文档。草稿里的两个诊断用例故意断言缺陷现状
（即上方「结果」第 3 条所说「不是修复后的回归绿灯」），缺陷修复后这些断言即为错误，
因此不宜进 `main`。

## 建议修复顺序（2026-11-09 状态回填）

> 本节写于诊断当时。缺陷已在 `a57516a4` / `c834768a` 一线修复，D01–D09 全部通过，
> 记录见 [`docs/testing/session-completeness.md`](../../testing/session-completeness.md)。
> 下方逐条回填当前状态，便于判断哪些建议仍待处理。


1. 窗口更新提交布局后，继续计算到覆盖视口／稳定；需测试停止条件、防止上下翻页振荡，不能简单加无界 rAF 循环。保持 initial60/page200/max400/buffer20 参数不变。
   **状态：已实施。** `desktop/src/lib/transcriptConvergence.ts` 的 `decideConvergence`（`:66`）提供停止条件，`useTranscriptWindow.ts:248` 调用；D01/D02/D03 覆盖。
2. 区分“完整历史基线已加载”和“收到过一个 live item”；首次读取不能仅因为存在 lastServerItemId 就跳过旧历史。避免用 thread/resume 修运行中会话，以免中断回合。
   **状态：未见对应实现。** 2026-11-09 在 `desktop/src/App.tsx`、`desktop/src/lib/session.ts` 中未找到区分「基线已加载」的标记；本报告 §2 所述路径（`session.ts` 的 upsert / live item / delta 推进 `lastServerItemId`）仍未改动。
3. 审核完成项投递可靠性与内容缺口重同步。delta 可以合并，但缺口必须可检测、完成项最终必须能收敛；只扩大队列不是完整修复。
   **状态：部分实施。** `broadcast.rs:198/201` 的 `try_send` 仍是普通广播主路径，但已加入 `:249` 的 `critical delivery stalled or closed; disconnecting for resync`，即用断开触发重同步。缺口「可检测」这一条仍待评估。
4. 若要精确归因用户现场，需记录显示前后内存 item 数、窗口区间、readItems 请求及返回 ID、通知投递失败计数；无需记录正文。现场未提供确切 thread ID、触发时刻及是切 app 还是切会话，因此本报告不声称已唯一定位那一次现场事件。
