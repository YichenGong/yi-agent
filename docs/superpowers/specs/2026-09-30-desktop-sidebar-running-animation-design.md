# 桌面侧栏 running 动画卡顿/重启 设计

**目标：** 修掉桌面端侧栏「thread 运行中」spinner 的两种可见故障——**卡住**与
**只播放很短一段就循环**——并消除造成它们的根因，而不是把动画换掉绕过。

**状态：** 设计已确认，实现中。

**范围：** 只改 `desktop/src`（前端渲染）。**不做**服务端改动：`thread/status/updated`
的翻转序列（running → awaiting_approval → running）本身是正确语义，不改。

---

## 1. 问题与证据

用户在桌面 App 里观察到：session 运行中时，侧栏的旋转指示器「会卡住，或者只播放
很短的一段，然后循环播放」。

用真实引擎（WKWebView，与 Tauri 在 macOS 上一致）加载**实际构建产物**
（`desktop/dist`，`__TAURI_INTERNALS__` 用桩驱动）复现，得到两个各自独立、
都成立的根因。

### 1.1 根因 A：每个 delta 重渲染整条对话，吃掉主线程预算

`ChatView` 用 `items.map(...)` 直接渲染，每条 item 的子树（agentMessage →
`react-markdown` + `rehype-highlight`；toolCall → `JSON.stringify(item.input, null, 2)`
卡片）**每个 delta 都重新执行**。带 `memo` 的只有 `MarkdownText`，而它恰好是唯一
必须重算的那条（文本在增长）；已定稿的历史消息与工具卡片被完整重解析 / 重新
序列化、重新 diff，毫无收益。

渲染器内量化（`desktop/` vitest，同一 `session.items` 实例原地追加，100 个 delta，
`react-dom` test renderer）：

| 场景 | 修复前 每个 delta 平均（首 10 → 末 10） | 修复前 100 delta 总计 |
|---|---|---|
| 400 条定稿消息 | 3.5ms → **10.9ms** | 739ms |
| 50 条定稿消息 | 3.2ms → **11.1ms** | 673ms |

60fps 每帧预算 ~16.7ms，单个 delta 就花掉 7–18ms。

**诚实补充（真实引擎实测的边界）**：用 WKWebView 加载构建产物回放合成流式回合
时，在可构造的会话规模下（≤1500 条定稿、大输入工具卡片、每 5–10ms 一个 delta），
**没有观测到该重渲染把动画帧压出可见缺口**——`gapP95` 实测约 26–48ms、动画时间轴
推进率仍 ≈ 1.0。原因是真实渲染被打包进 WebKit 单次任务提交，且这些规模的重复
渲染成本低于一帧预算。因此：

- 真实引擎里能**确证复现**的、决定性的故障是**根因 B**（见下，0.009 的推进率）；
- 根因 A 是**真实但次级**的开销：它稳定地吃掉 7–18ms/帧的预算，在更长会话、
  更大消息、更复杂 markdown（表格 / Mermaid / 大量高亮）下会越界；本 PR 顺手
  消掉这部分**冗余**工作（见 §2.1），但**不宣称**它能单独消除卡顿。

**仍未解决的残留（本 PR 有意不做）**：正在增长的那条消息每个 delta 都要
全量重解析 markdown（`react-markdown` 无增量解析）。实测大 delta 下会出现约
**100ms 的偶发长帧**（`gapMax`），两个构建都一样——这是「卡住」的残留来源，
要彻底消除需 delta 合并 / 增量 markdown，属独立议题（§2.3）。

### 1.2 根因 B：状态翻转把指示器卸载/重挂，动画从头开始（决定性）

`ThreadSidebar.renderThread` 对 `running` / `awaiting_approval` 用**两个互斥分支**
渲染两个不同的 `<span>`：

```tsx
{st === "running" && <span aria-label="Running" className="... animate-spin ..." />}
{st === "awaiting_approval" && <span aria-label="Awaiting approval" ... />}
```

React 无法复用这两个节点（类型、`aria-label` 不同），**状态每翻转一次就卸载旧节点、
挂载新节点**，新元素上的 CSS 动画从 `currentTime = 0` 重新开始。

这不是罕见路径：normal 权限模式下**每次需要确认的工具调用**都会走
`running → awaiting_approval → running`（`server.rs` 在写反向审批请求前推
`AwaitingApproval`，拿到决定后推回 `Running`）。

真实引擎端到端（构建产物 + 桩 `__TAURI_INTERNALS__`，8s，delta/10ms）：

| 场景 | 动画时间轴净推进 / 墙钟 | 切换后 `currentTime` 回零次数 |
|---|---|---|
| 修复前，审批每 400ms 一轮 | **0.009** | 52 |
| 修复前，审批每 1000ms 一轮 | 0.023 | 20 |
| **修复后，审批每 400ms 一轮** | **1.000** | **0** |
| **修复后，审批每 1000ms 一轮** | **1.000** | **0** |

修复前 8 秒里动画只净推进约 180ms：**连续播放不到 0.2 秒就被重置，如此往复**——
正是用户描述的「只播放很短的一段，然后循环播放」。

### 1.3 排除项（已验证不是原因）

- 父组件重渲染会让 CSS 动画重置吗？**不会。** 实测父容器反复重渲染同一元素
  （含兄弟子树增删），`getAnimations()[0].currentTime` 持续推进，无回退。
- 极端的 CPU 抢占（补间式长任务，静止元素）会让动画重置吗？**不会**，只暂停
  （`currentTime` 停在原地）。
- 运行中线程的重命名、切换、折叠呢？元素保持 mounted，**不重置**（对照实验）。
- 生成 CSS 里的 `@keyframes spin` 与 `--animate-spin: spin 1s linear infinite`
  正确；系统未开「减弱动态效果」（`reduceMotion` 未设置，`prefers-reduced-motion`
  相关的 `motion-reduce`/`motion-safe` 并未被用到，故此项不参与）。

因此**不采用**「换掉 CSS 动画」「加 `will-change`/`translateZ` 强制独立合成层」这类
绕过手段：前者是症状掩盖，后者换不来正确性（LAYER 合成同样受主线程提交约束）。

---

## 2. 修复

### 2.1 定稿 item 不随流式 delta 重渲染（根因 A）

- `MarkdownText.tsx` 新增 `AgentMessage`：把会话气泡的**静态包装**
  （`my-1 max-w-[90%] self-start`）与 `MarkdownText` 一起 memo 掉。此前 memo 只覆盖
  到 `MarkdownText`，外层 `div` 仍在每次 delta 重建。
- `ChatView.tsx`：抽取 `ToolCallRow = memo(ToolCallCard)`，把 memo 边界放在**叶子
  组件与其值 props** 上。
  **为什么不是 memo 整条 item**：`item/delta` 是**原地改写**尾条 agentMessage 的
  `text`，按对象身份 memo 会永远命中、把流式文本冻住（第一版就是这么错的，测试
  `keeps the streamed text visible` 当场抓住）。传 `text` 这个**原始值**给
  `AgentMessage`，React 默认的浅比较才能发现变化，同时其余没动的消息原样跳过。

  实测收益（`desktop/` vitest，400 条定稿 + 100 个 delta）：

  | 渲染路径 | 每个 delta 平均耗时 | 100 delta 总计 |
  |---|---|---|
  | 修复前（全量） | 3.5ms → 10.9ms | 739ms |
  | 修复后（memo 定稿） | **~0.1ms** | **~9ms** |

  注：上面是渲染器（jsdom test renderer）量化。真实引擎里这部分开销被 WebKit 的
  批处理吸收，可构造规模内未观测到可见缺口（见 §1.1 的诚实补充），所以这是
  **消除冗余工作**而非已证实的单独修复。

### 2.2 指示器跨状态翻转保持挂载 + 保持动画（根因 B，决定性）

`ThreadSidebar.tsx` 用一个**固定结构的 `StatusBadge`** 取代两个互斥分支：

- 无论 `running` 还是 `awaiting_approval`，都渲染**同一个 `<span>`**（同一类型、
  同一 `aria-label="Thread status"`），只切换 class（旋转环 / 琥珀圆点）。React 复用
  同一 DOM 元素。
- **`animate-spin` 在两个状态下都保留**。这一条是实测逼出来的，不是想当然：
  第一版只在 `running` 时挂 `animate-spin`，结果真实引擎里 400ms 一轮的审批循环
  仍然 **推进率 0.008、`currentTime` 回零 0 次但 886 帧 `rotation = 0`**——因为
  「移出 → 移入」`animate-spin` 同样会销毁并重建 CSS 动画，时间轴照样归零。
  旋转一个正圆在视觉上静止，所以琥珀点保留该动画毫无损失，却换来整段翻转期间
  时间轴连续。
- `memo` 用自定义比较器按**语义**判定（空 / running / awaiting_approval），忽略
  `statuses` / `unread` Map 的引用变化——`App` 每次通知都新建这两个 Map，否则 memo
  形同虚设。
- 可访问性：`aria-label="Thread status"` + `data-status` + `title`；只出现一次，
  无重复读出。

### 2.3 不做的事

- **不做**delta 节流/合并（例如 `requestAnimationFrame` 攒批）。*正在增长的那条*
  消息每个 delta 仍要全量重解析 markdown（`react-markdown` 无增量解析），实测大
  delta 下会有约 **100ms 的偶发长帧**（两个构建一样）。要消除它需要增量 markdown
  或 delta 合并，属独立议题，本 PR 不做——见 §4 残留。
- **不换**指示器形态（不放 emoji 帧、不改骨架 / 进度条）。用户要的是这个 spinner 正常。

### 2.4 取消的范围（避免过度设计）

初版还把 `ThreadRow` / `GroupRow` 从内联 `renderThread` 里提出来各自 memo，理由是
「稳定 key 防止整行被重挂」。**实测推翻了必要性**：React 对 `key={g.workspace}` /
`key={t.thread_id}` 的已有 key 会正常复用元素，且脚本不会在翻转中换分组 key；真实
引擎里并不存在这条重挂路径。该改动已回退，只保留 §2.1/§2.2 两处——把修复面压到最小。

---

## 3. 验收

**自动化（确定性，跑得动）**

- `cd desktop && npx vitest run src/components/ChatView.test.tsx` —
  `does not re-render settled items while the trailing message streams`（修复前
  工具卡片重渲染 6 次，修复后 1 次）；`still re-renders a tool card whose status changed`；
  `keeps the streamed text visible`（这条是防线：按对象身份 memo 会把流式文本冻住，
  修复前该测试即失败）。
- `cd desktop && npx vitest run src/components/ThreadSidebar.test.tsx` —
  `keeps the running badge mounted across a status flip`（修复前是**新** DOM 节点，
  修复后是**同一**节点）；`shows an amber badge for an awaiting-approval thread`
  同时断言 `animate-spin` 仍在（防「移出再移入」回归）。
- `cd desktop && npx vitest run`（全量）、`npx tsc --noEmit`、`npm run build`。

**真实引擎（取证，已跑）**

WKWebView 加载 `desktop/dist`，桩 `__TAURI_INTERNALS__` 回放合成流式回合，采样
指示器 `getAnimations()[0].currentTime`、`requestAnimationFrame` 帧间隔与元素身份
（harness 见交付说明）：

| 场景 | 修复前 动画推进/墙钟 | 修复后 |
|---|---|---|
| 审批每 250ms 一轮 + delta/10ms | 0.005（81 次归零） | **1.000（0 次）** |
| 审批每 400ms 一轮 + delta/10ms | 0.008（51 次归零） | **1.000（0 次）** |
| 审批每 1000–1500ms 一轮 + delta/10ms | 0.023–0.034（13–20 次归零） | **1.000（0 次）** |
| 纯流式（含 1500 条定稿） | ≈1.0 | ≈1.0 |

**A/B 方法**：修复前 = `git archive 6a79318 desktop` 单独构建；修复后 = 本分支构建。
两者同一 harness、同一合成脚本，唯一变量是源码。

---

## 4. 风险与残留

- **memo 比较器写错会让卡片「不更新」**：`ToolCallCard` 按 `item` 对象身份 memo——
  这个身份可靠的**前提**是 `Session.apply` 在 `item/completed` 时用**新对象**替换槽位
  （`this.items[index] = item`）。若将来有人改成原地改 `status`，卡片会不刷新。
  `still re-renders a tool card whose status changed` 正是这条前提的可执行断言。
- **残留：增长中的消息仍全量重解析**。实测大 delta 下约 100ms 的偶发长帧，两个构建
  都有。彻底解决要 delta 合并 / 增量 markdown，未做。
- **`aria-label` 变更**：`Running` / `Awaiting approval` 合并为 `Thread status`
  （+ `data-status`）。任何依赖旧 label 的地方需同步；仓库内已同步
  `ThreadSidebar.test.tsx`。
- **`animate-spin` 常驻于 awaiting_approval**：视觉上无差别（旋转正圆），代价是一个
  不会被观察到的合成器动画；收益是翻转期间时间轴连续。


