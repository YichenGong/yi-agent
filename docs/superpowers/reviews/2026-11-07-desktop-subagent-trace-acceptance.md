# 桌面端子 agent 轨迹面板：验收记录

- 日期：2026-11-07
- 分支：`feat/desktop-subagent-trace-redesign`（worktree `.worktrees/desktop-subagent-trace-redesign`）
- 规格：`docs/superpowers/specs/2026-11-07-desktop-subagent-trace-redesign-design.md`
- 计划：`docs/superpowers/plans/2026-11-07-desktop-subagent-trace-redesign-impl.md`
- 环境：macOS / Node v23.11.0 / Chrome 157.0.8103.38 / 900×600（面板设计的最小窗口）

本文记录**实际执行过**的证据。未执行的项一律写「未验」，不预填通过。

## 1. 验证命令与结果

| 命令 | 结果 |
|---|---|
| `cd desktop && npx tsc --noEmit` | exit 0 |
| `cd desktop && npx vitest run` | 172 文件 / 2522 tests passed |
| `cd desktop && npm run build` | 成功（256ms，仅有既有的 chunk 体积警告） |
| `cd desktop && npm run test:browser:subagent-trace` | 87 checks / 18 matrix cells，全部通过 |
| `cd yi-agent-rs && cargo test -p yi-agent-app-server trace_unwatch_` | 3 passed |
| `cd yi-agent-rs && cargo test -p yi-agent-app-server thread_delete_stops_all_watchers` | 1 passed |
| `cd yi-agent-rs && cargo fmt --all && just ci` | 见 §5 |

浏览器脚本的定向命令：

```bash
cd desktop && npx vitest run \
  src/components/SubagentRail.test.tsx src/components/SubagentTrace.test.tsx \
  src/components/TraceTimeline.test.tsx src/components/TraceEvent.test.tsx \
  src/components/TraceComposer.test.tsx src/components/ThreadDetailPanel.test.tsx \
  src/hooks/useTraceScroll.test.tsx src/hooks/useSubagentTrace.test.tsx \
  src/hooks/connectionLifecycle.test.ts src/hooks/useHandshakeRecovery.test.ts \
  src/hooks/useNotificationRouter.test.ts src/lib/subagentTraceModel.test.ts \
  src/lib/subagentTraceSubscription.test.ts src/lib/subagentTraceActions.test.ts \
  src/lib/subagents.test.ts
# → 15 files / 305 tests passed
```

## 2. 浏览器矩阵实际数据

18 种组合（主题 × 栏宽 × 字号），窗口固定 900×600。`panelOverflow` 与
`documentOverflow` 为横向溢出像素，`bodyHeight` 是轨迹正文滚动区的
`clientHeight`，`messageReachable` 要求 textarea 真实可聚焦、可点击且与固定
composer 一起落在面板可视范围内。

| 主题 | 栏宽 | 字号 | panelOverflow | documentOverflow | bodyHeight |
|---|---|---|---|---|---|
| dark | 260 | small | 0 | 0 | 328 |
| dark | 260 | standard | 0 | 0 | 252 |
| dark | 260 | large | 0 | 0 | 180 |
| dark | 360 | small | 0 | 0 | 388 |
| dark | 360 | standard | 0 | 0 | 364 |
| dark | 360 | large | 0 | 0 | 347 |
| dark | 720 | small | 0 | 0 | 388 |
| dark | 720 | standard | 0 | 0 | 380 |
| dark | 720 | large | 0 | 0 | 365 |
| light | 260 | small | 0 | 0 | 328 |
| light | 260 | standard | 0 | 0 | 252 |
| light | 260 | large | 0 | 0 | 180 |
| light | 360 | small | 0 | 0 | 388 |
| light | 360 | standard | 0 | 0 | 364 |
| light | 360 | large | 0 | 0 | 347 |
| light | 720 | small | 0 | 0 | 388 |
| light | 720 | standard | 0 | 0 | 380 |
| light | 720 | large | 0 | 0 | 365 |

最不利组合是 260px 栏宽 + 大字号（180px），仍高于规格要求的 120px。`bodyHeight`
随栏宽变化，是因为窄栏里头部标题/步骤换行更多，把固定头部撑高了；这与「正文至少
可见 120px」的判据一致。18 种组合里 `messageReachable` 全为 `true`。

对比度采样（面板内所有渲染出来的叶子文本，合成到实际背景后按 WCAG 相对亮度计算）：
深色 54 个样本、浅色 64 个样本，**低于下限的为 0**。

## 3. A01–A12 逐条证据

| ID | 证据 |
|---|---|
| A01 | `src/lib/subagents.test.ts`（14）钉住 15 个状态的中文标签/终态/活动、未知状态回退、`TERMINAL_STATES` 与状态表同源；`src/components/SubagentRail.test.tsx`（14）覆盖空态、活动计数、终态不再显示「当前步骤」；浏览器 `missing objective/lastStep say so instead of inventing a task id` 断言真实空元数据渲染「服务端未提供」。 |
| A02 | `src/components/TraceEvent.test.tsx`（21）覆盖连续文本合并、Markdown、破坏 JSON/null/数组/类型错/标签不符全部回退原文入口；浏览器 `long prose, tokens and paths all reach the DOM`、`an expanded summary admits it is not the full output`、`undecodable rows keep a raw-payload entry`、`raw payload expands to the original text`。 |
| A03 | `src/lib/subagentTraceModel.test.ts`（37）覆盖工具名、结果摘要阈值与 Unicode 边界、错误首行与空摘要；`src/components/TraceTimeline.test.tsx`（22）覆盖 1000→200、载早每批 +200、追加不收窄；浏览器 `opening a 1000-block history mounts only the newest 200`（实测 200）、`load earlier mounts one more 200-block batch`（200→400）、`appending pushes the tail without shrinking the visible window`（400→480）、`every block is reachable by loading earlier in batches`（累计 747）。 |
| A04 | `src/hooks/useTraceScroll.test.tsx`（15）覆盖首次贴底、同块增长、展开/收起锚点、程序滚动守卫、任务信息往返、卸载清理；浏览器 `a real wheel gesture pauses following`、`same-block growth does not move a paused anchor`、`expanding a summary keeps the reading anchor`、`collapsing again keeps the reading anchor`、`scrolling back to the end resumes following`、`an image that grows after load keeps the reading position`（锚点误差均 ≤2px）。 |
| A05 | `src/components/SubagentTrace.test.tsx`（25）覆盖父/子导航、缺失父级、自引用过滤、焦点落点；`ThreadDetailPanel.test.tsx`（10）覆盖返回列表路径；浏览器键盘段 `Tab moves to a named control without leaving the panel`。 |
| A06 | `src/lib/subagentTraceSubscription.test.ts`（37）覆盖 watch 权威快照、缓冲高水位、丢帧、15 秒提示不释放栅栏、迟到成功收敛；`useSubagentTrace.test.tsx`（23）覆盖 loading/ready-empty/error/retry；浏览器 `loading names the operation instead of showing an empty list`、`an error keeps the history it already has`、`a dropped connection says so and offers a retry`。 |
| A07 | `subagentTraceSubscription.test.ts` 的 deferred 竞态段覆盖 A→B、A→B→关闭、关闭→重开、同任务重试、旧 client/slot 同名 ID 不污染；`connectionLifecycle.test.ts` 与 `useHandshakeRecovery.test.ts` 覆盖来源登记先于 `wireClient`、握手就绪顺序；`App.test.tsx`（281）覆盖切会话后旧回包不出现。 |
| A08 | `cargo test -p yi-agent-app-server trace_unwatch_`（3 passed）证明 unwatch 只摘 trace、保留 children/process watcher 且幂等；`thread_delete_stops_all_watchers`（1 passed）证明删除会话清理全部 watcher；`useNotificationRouter.test.ts` 与 `App.test.tsx` 覆盖所有退出路径与 Tab 切换保留 watch。 |
| A09 | `src/lib/subagentTraceActions.test.ts`（55）覆盖 revision 清稿、失败留稿、IME/空白/双击、跨导航 pending、删除后迟到回包；`TraceComposer.test.tsx`（23）覆盖 Enter/Shift+Enter/IME 守卫、在途仍可编辑、同步防重。 |
| A10 | `subagentTraceActions.test.ts` 覆盖 preview→confirm 两阶段、单目标非递归、TTL 从请求起点计时、过期/终态/导航作废、确认在途禁放弃、失败必须重预览、15 秒提示；浏览器 `the confirmation names the real scope and keeps the textarea`、`the confirmation opens with focus on the harmless choice`、`dismissing closes the confirmation and restores the action row`。 |
| A11 | 见 §2：18 组合全部零横向溢出、正文 ≥120px（实测最低 180px，出现在 260px 栏宽 + 大字号）、消息框可达、深浅主题对比度 0 个低于下限。 |
| A12 | 浏览器 `the timeline is a named list, not a live region`（h2 + `role="log"` + ol/li，且 log 不是 live region）、`the message box has an independent label`、`status and alerts use one polite region each`、`every expandable control exposes aria-expanded and an existing target`、`reduced motion is honoured without breaking the panel`；`TraceComposer.test.tsx` 覆盖确认区焦点落「放弃」与关闭后的焦点去处。 |

## 4. 本轮验收发现并修复的真实缺陷

真实浏览器与补写的 jsdom 红测各抓到一个此前测试套件漏掉的缺陷。两处都先有失败
证据、再修、再验证。

### 4.1 ResizeObserver 闩锁吞掉用户滚动（`useTraceScroll.ts`）

现象：真实滚轮把视口上移后约 400ms，视口自己跳回底部，用户无法阅读历史。

根因：`pendingFollowRef` 本意是「有一次程序贴底尚未被 scroll 事件消化」，但它在
**贴底是空操作**（视口已经在底部，赋值 `scrollTop` 不产生 scroll 事件）时也被置位，
于是永久挂着；用户下一次真实滚动被 `if (pendingFollowRef.current) return` 直接吞掉，
`anchorRef` 保持陈旧值，下一次 ResizeObserver 回调就把视口恢复到底部那个旧锚点。

修复：闩锁只在「这次贴底真的移动了视口」时置位；暂停分支显式清掉它，让锚点恢复后
的 scroll 事件照常更新意图。

证据：

- 新增 `src/hooks/useTraceScroll.test.tsx › keeps the reader's place when a pin produced no scroll event`，修复前 `expected 800 to be 400` 失败，修复后通过；
- 修复前浏览器 `a real wheel gesture pauses following` 失败；修复后通过；
- 回退该修复时上述两条用例同时变红（已做变异验证）。

### 4.2 深/浅主题的小字对比度不达 4.5:1（`index.css`、`highlight-light.css`）

浏览器实测（合成实际背景后）：

- 深色 `--fg-subtle #737373`：surface 4.18、panel 3.78、raised 3.19；
- 深色 `--fg-faint #525252`：surface 2.53、panel 2.29、raised 1.94；
- 浅色 `--fg-subtle #737373`：panel 4.35；
- 浅色代码高亮 `.hljs-comment #6a737d`：panel 4.42、raised 3.82；
- 浅色代码高亮 `.hljs-keyword #d73a49`：panel 4.20、代码块底 4.30。

面板里 12px 的元信息、`等待新的执行进展…`、`结果摘要` 的工具名都落在这些档位上。
修复后取值与最差背景下的对比度：

| token | 新值 | 最差背景对比度 |
|---|---|---|
| 深色 `--fg-subtle` | `#9b9b9b` | 5.44（raised） |
| 深色 `--fg-faint` | `#8f8f8f` | 4.68（raised） |
| 浅色 `--fg-subtle` | `#5c5c5c` | ≥6.2（panel） |
| 浅色 `--fg-faint` | `#6b6b6b` | ≥5.3（panel） |
| 浅色 `.hljs-comment` | `#5f676e` | 4.57（raised） |
| 浅色 `.hljs-keyword` | `#c21e2b` | 4.74（raised） |

两档之间仍保留层级差（`fg` 7.85 / `fg-muted` 7.11 / `fg-subtle` 5.44 /
`fg-faint` 4.68 on raised 的 dark 主题）。这是**全局 token**，会影响主对话的次级
文字，属于有意的可读性修正，不是只给面板打补丁。

证据：修复前浏览器 `dark theme text meets its contrast floor` 报 13 处不达标、
`light theme text meets its contrast floor` 报 5 处不达标；修复后两者均为 0 处，
采样量 54 / 64。

### 4.3 确认区打开时焦点落在危险动作（`TraceComposer.tsx`）

A12 要求「取消确认打开时焦点落在『放弃』而非危险动作」。此前实现只处理了关闭时
的焦点去处，没有处理打开时的初始焦点。

修复：确认区首次出现时，若焦点本来就在这一栏（用户点了「停止任务」），把焦点交给
「放弃」；焦点在别处时不抢。关闭时的陈旧标记用 `onBlur`/`onFocus` 维护，避免
「用户又点回 textarea 后确认区一关就把人拽走」。

证据：`TraceComposer.test.tsx` 新增 3 条用例（打开落「放弃」、打开时不从别处抢、
用户回到 textarea 后关闭不拽人），均做过变异验证（去掉实现即变红）。浏览器
`the confirmation opens with focus on the harmless choice` 通过。

## 5. 提交门禁结果

`cd yi-agent-rs && cargo fmt --all` 执行成功、无文件被改动。

`just ci` 覆盖的 18 道门禁（`fmt-check`、`lint`、`test`、`build`、
`bindings-check`、`secret-check`、`npm-version-check`、`ios-version-check`、
`release-env-check`、`tag-gate-check`、`plugin-release-check`、
`build-env-check`、`model-setup-docs-check`、`desktop-target-dir-check`、
`rebuild-install-check`、`msrv-check`、`eval-baseline-check`、
`nightly-workflow-check`、`audit-deps-check`、`audit`）全部通过，**exit 0**，
末行输出 `CI passed`。其中 `msrv-check` 用声明的 1.89.0 真编通过，
`eval-baseline` 32 passed，`audit` 仅 1 条既有的 `chacha20` yanked 提示（在豁免清单内）。

**`just ci` 不含桌面端门禁**（`justfile:230` 起说明：CI 镜像没有 Node，塞进去会
拖垮流水线），所以前端另外单独跑了 `just desktop-check`（`npm ci` +
`npx tsc --noEmit` + `npx vitest run`）：**exit 0，172 文件 / 2522 tests passed**。
真实浏览器脚本也复跑了一次：87 checks / 18 matrix cells 全通过。

## 6. 未验项（明确记录，不当作已通过）

以下项只能在原生 WKWebView / 真机上判断，Chrome 不能替代，本轮**未验证**：

- 原生 WKWebView 的深浅主题观感与系统外观联动；
- 原生窗口缩放到 900×600 时的实际排版（Chrome 用 `Emulation.setDeviceMetricsOverride` 模拟）；
- 真机 IME（中文输入法候选词确认时的 Enter 语义）——`useImeGuard` 有 jsdom 单测，
  但真实 WKWebView 的 `keyCode === 229` 路径未在真机复现；
- 触控/触控板惯性滚动下的暂停手感；
- 大字号档位在真机上的滚动性能（长轨迹 747 块时的帧率）。

因此 `docs/project-management/desktop.md` 中「子 agent 轨迹详情」条目保持 `[x]`
（其判据是可自动化的单测 + Chrome 几何，均已通过），但上述五项作为已知未验边界
记录在此，不声称真机通过。
