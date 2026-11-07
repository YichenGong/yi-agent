# 长会话首屏：转录窗口化 + 向上分页 设计

日期：2026-11-06
状态：设计待评审

## 1. 问题

打开一条历史很长的会话，首屏明显比短会话慢。用户观察：「长历史的打开时间长，
短历史的打开时间快」，短会话接近「不到一秒」。

### 1.1 实测：慢在哪一段

测量方式与复现命令见 §7。release 二进制、stdio、本机真实会话日志
（最长 `thread-dd9ebf1d…`：3251 items / 34.8 MB）。

**服务端 `thread/resume`（真实 app-server，三次连测的稳定值）**

| 阶段 | 短会话 5 items | 长会话 3251 items |
|---|---|---|
| `initialize` | 7~9 ms | 7~9 ms |
| `thread/resume` 总计 | **55~59 ms** | **114~115 ms** |
| 首个回放帧 | 55~59 ms | 99~100 ms |
| 回放帧数（`items/completed` 条数） | 1 | **26** |
| 再次 resume（温会话） | 22 ms | 119 ms |

两点必须记清楚，否则后续会追错方向：

1. **服务端只差约 60 ms**，不是上百毫秒的量级。所以它不是主要瓶颈，
   不值得为它改架构。
2. **帧数是 26，不是「3251 / 200 ≈ 17」。** 服务端切块规则是
   「条数未达 `REPLAY_CHUNK_MAX_ITEMS`(200) **且** 字节未超 `REPLAY_CHUNK_BYTES`(256 KB)，
   先到者结块」（`yi-agent-rs/crates/yi-agent-app-server/src/server/turn/replay.rs:18-40`）。
   工具卡的 JSON 很大，实际基本按字节切，平均每帧约 125 条（实测块大小
   34~193 条、每块约 250~262 KB）。任何「按 200 items/帧推帧数」的算术都是错的。

   这个数字踩过一次坑：脚本原先统计**所有**通知帧（含 `thread/started` 与
   `token/usage`），报出 29；改成只数 `items/completed` 后是 26，与前端脚本
   复刻同一份日志的切块结果一致。两处现在互相印证。

**前端渲染（真实 items + 真实 react-markdown/remark-gfm/rehype-highlight，jsdom）**

用 `desktop/scripts/perf/open.mjs` 对同一份 3251 items 实测（含 1890 个工具卡的
JSON 序列化）。三次连测重复性很好（±3 ms）：

| 场景 | 耗时 | 挂载条数 | DOM 节点 |
|---|---|---|---|
| 一次性渲染全部（当前行为） | **773 ms** | 3251 | 11025 |
| 按真实切块规则分帧重渲染（26 帧） | **822 ms** | 3251 | 11025 |
| 只挂载最近 60 条（本设计首屏） | **21 ms** | 60 | 451 |
| 只挂载最近 400 条（卸载上限） | **112 ms** | 400 | 2334 |
| 参考：只挂载最近 30 条 | 9 ms | 30 | 189 |

**结论：差距的 90% 以上在客户端渲染，不在服务端。** 长会话比短会话多花的时间
里，服务端约占 60 ms，客户端约占 770~820 ms。因此本设计的杠杆是**减少挂载条数**，
不是改传输。

另需注意：**分帧回放并不比一次性渲染快**（822 vs 773 ms，三次连测同向）。每帧都要
重新协调一份越来越长的列表，而挂载的条目从头到尾都是全量。分帧只让内容更早出现，
它本身不是优化——本表量的是「全部落地」的总成本，不含「第一帧何时可见」。
后者是感知层面的另一件事，本设计不依赖它、也不优化它。

### 1.2 根因清单（按贡献排序）

1. **转录区无窗口化。** `ChatView` 的 `HistoryPrefix` 是
   `for (let i = 0; i < count; i += 1)` 一次性建全部 `ChatItem`
   （`desktop/src/components/ChatView.tsx:355`）。3251 条全量挂载 = 11025 个 DOM 节点，
   773 ms；只挂 60 条 = 451 个节点，21 ms。这是差距的绝对主项（§1.1）。
2. **桌面端没有内容缓存。** `prefillFromCache` / `persistContent` 全被
   `isRemoteClient()` 门控（`desktop/src/App.tsx:455`、`:953`）。手机能先画缓存
   再对账，桌面每次冷开都要等完整回放。
3. **滚动采集是 O(N) 且带强制布局。** `capture()` 对全部 `[data-item-id]` 逐个
   `getBoundingClientRect`（`ChatView.tsx:703`），3251 条即每帧 3251 次强制布局。
4. **`upsertItems` 是 O(N²)。** 每个 item 都从头 `findIndex`
   （`desktop/src/lib/session.ts:518`），3251 条 = 528 万次比较、41 ms。
5. **存储放大 3 倍。** 长会话文件 34.8 MB 中 items 只占 5.7 MB，`messages`
   占 24 MB——每一轮都存一份完整上下文快照（480 条 × 72 次）。

本设计的范围是 **1、3**（外加搜索下沉），并把 2 作为可选后续项。4 与 5 单列，
见 §8。

### 1.3 目标

必须先承认一个**下界**：端到端首屏包含服务端 `thread/resume`，实测长会话
114~115 ms、首个回放帧 99~100 ms（§1.1）。所以「总耗时 ≤150 ms」是做不到的，
写进目标只会让后续实现去追一个假数字。目标按两段拆：

| 段 | 现状 | 目标 | 依据 |
|---|---|---|---|
| 客户端渲染（items 已在手 → 首屏可见） | 773~822 ms | **≤150 ms** | 窗口 60 条实测 21 ms，留 7 倍余量给锚点恢复与首帧布局 |
| 端到端首屏 | 约 0.9 s（约 820 渲染 + 100 首帧） | **≤300 ms** | 服务端下界 100 ms + 客户端 150 ms + 会话切换开销 |

服务端那 100 ms **不在本设计范围内**（§1.1 已结论：不值得为它改架构）。
本设计只承诺把客户端那约 800 ms 压到 150 ms 以内，端到端因此从约 0.9 s 降到约 0.3 s。

- 滚动、搜索的耗时**不随会话长度增长**（O(挂载数) 与 O(命中数)，不再 O(items)）。
- 内容完整可达：未挂载的历史仍能被滚动到、被搜索到、被阅读位置恢复命中。
- 切换会话的耗时不受目标会话长度影响（原来长会话切进来要多等约 1 s）。

## 2. 核心模型：spacer 窗口

不做「自己接管 scrollTop 换算」的虚拟滚动，而是用两个占位元素撑住未挂载区间的高度，
让浏览器继续做原生滚动：

```
[spacerTop]      高度 = 未挂载历史的高度估算
  挂载区间 windowStart..windowEnd-1   ← 真实 ChatItem
  末尾那一条 = LiveTail（永远挂载，不参与窗口计算）
[spacerBottom]   高度 = 未挂载未来项的估算
```

**两种窗口形态**（`spacerBottom` 是否为 0 由它们决定）：

| 形态 | `end` | `spacerBottom` | 何时出现 |
|---|---|---|---|
| 跟随尾部 | `itemCount` | 0 | 冷启动、流式增长、用户贴底 |
| 定点开窗 | `focus.index + after` | 可能 > 0 | 恢复阅读位置、搜索跳转到历史中间 |

定点开窗**必须**让 `end < itemCount` 成为可能：否则从「用户上次读到第 500 条」
一路挂到第 3251 条，等于没做窗口化。因此向下滚动时也需要一页一页向尾部扩（规则 6）。
一旦扩到 `end === itemCount`，就回到「跟随尾部」形态，`spacerBottom` 归 0。

**尾部那条的位置（off-by-one 必须钉死）**：最后一条 item 由 `LiveTail` 单独渲染
（它要独立应用流式节流，见 `ChatView.tsx` 对 `LiveTail` 的说明），现行代码就是这么分的：
`ChatView.tsx:812` 传 `count={items.length - 1}`，`:819` 再渲染一个 `LiveTail`。

因此约定：

- `nextWindow` 的 `end` 是**逻辑终点，含尾部那条**，取值上界就是 `itemCount`。
- `HistoryWindow` 实际渲染 `items.slice(start, Math.min(end, itemCount - 1))`。
- 于是「首屏 60 条」= 59 条 `HistoryWindow` + 1 条 `LiveTail`。

`min(end, itemCount - 1)` 不能省：漏掉它，`end === itemCount` 时最后一条会被
`HistoryWindow` 和 `LiveTail` 各渲染一次（React key 相同也会报警）。这是实现时最容易
踩的一处，spec 在此固定。

好处：
- 滚动条比例真实（高度由 spacer 撑出），用户看到的滚动位置有意义。
- 不需要重写 `useStickToBottom` 的 `scrollHeight` 语义——spacer 是内容的一部分。
- `viewAnchor` 记录的「视口顶部那条消息」在语义上不变。

### 2.1 窗口参数

| 参数 | 值 | 依据 |
|---|---|---|
| 首屏条数 | 60 | 实测 21~22 ms（3251 items 全类型真实样本），接近一帧（16 ms）量级 |
| 向上每次追加 | 200 | 一页在惯性滚动内加载完（400 条实测 112 ms，仍在可接受范围）；不再声称与回放分帧一致——回放是按字节切的，平均约 125 条 |
| 视口上方缓冲 | 20 条 | 挂载区顶边进入视口即分页，避免用户看到 spacer |
| 视口下方缓冲 | 20 条 | 定点开窗形态下向下分页的触发距离 |
| 卸载阈值 | 挂载数 > 400 | 超出后卸载远端，防止「一路翻到顶」把全量找回 DOM |

400 的算术（**仅限「跟随尾部」形态**，`end` 钉在 `itemCount`）：首屏 60 → 上翻一页
260 → 上翻两页 460，此时超出 400，按规则 7 截 `start` 侧 60 条回到 400。
也就是说卸载从第二次上翻就开始，之后每翻一页卸载一页，挂载数稳定在 400。
「第三页才开始卸载」是错的：`60 + 200 + 200 = 460 > 400`。

**定点开窗形态下不保证是 `start` 侧**：规则 7 比的是视口到两端的距离，
从「离视口远的一端」截。所以断言不能写「截 start」，只能写下面这两条真正稳定的
不变量（§6.1 已按此改写）：

1. 截断后视口仍在挂载区内；
2. 截断后视口到两端的距离都 ≥ `buffer`（除非该端已到 0 / `itemCount` 边界）。

### 2.2 纯函数内核

新增 `desktop/src/lib/transcriptWindow.ts`，**零 React、零 DOM**，可完整单测：

```ts
export interface WindowState {
  /** 已挂载区间的起点（含）。 */
  start: number;
  /** 已挂载区间的终点（不含）。最后一条 item 恒由 LiveTail 渲染。 */
  end: number;
  /**
   * 是否跟随尾部。**必须是显式状态，不能由 `end === itemCount` 推导**：
   * 流式追加时 `itemCount` 先变大、`end` 还没跟上，两者不等恰恰说明「正在跟随」。
   * 用派生值判断会把每一次 delta 都读成「用户离开了尾部」，跟随就此关闭。
   */
  following: boolean;
}

/** 一次「把某个 index 摆进视口」的请求（锚点恢复、搜索跳转）。 */
export interface FocusRequest {
  index: number;
  /** 该 index 之上/之下额外保留多少条。 */
  before: number;
  after: number;
}

/**
 * 当前视口对应的挂载区状态（由组件层测量 DOM 得出）。
 *
 * 同时给「index 距离」和「像素 gap」：前者用于规则 7 决定截哪端，
 * 后者用于规则 5/6 决定是否分页。只给 index 会错——item 高度是重尾分布。
 */
export interface VisibleRange {
  first: number;
  last: number;
  /** 挂载区顶边到视口顶的距离（px，≥0）。0 表示顶边已进入视口。 */
  gapTopPx: number;
  /** 挂载区底边到视口底的距离（px，≥0）。 */
  gapBottomPx: number;
}

export interface WindowLimits {
  /** 冷启动首屏条数。 */
  initialSize: number;
  /** 向上/向下分页一页的条数。 */
  pageSize: number;
  /** 挂载上限，超出后从离视口远的一端卸载。 */
  maxMounted: number;
  /** 定点开窗时上下各留多少条（`FocusRequest` 的默认值）。 */
  buffer: number;
  /**
   * 分页触发的像素阈值，由组件层算：`buffer * HeightBook.median()`。
   * 纯函数不读 `HeightBook`，所以这里直接给结果。
   */
  bufferPx: number;
}

/**
 * 算出下一个窗口。**纯函数**：不读 DOM、不读时间、无副作用。
 *
 * 优先级：`focus` > 视口跟随尾部 > 分页。任一输入变化都应重新调用它，
 * 它自己决定要不要动——调用方不做区间算术。
 */
export function nextWindow(input: {
  current: WindowState | null;
  itemCount: number;
  limits: WindowLimits;
  /** 当前视口覆盖的 index 区间；未测量出时为 null。 */
  visible: VisibleRange | null;
  /** 一次性开窗请求（锚点恢复 / 搜索跳转）。 */
  focus?: FocusRequest | null;
}): WindowState;
```

规则（按优先级，依次应用）：

1. **不变量**：`0 <= start <= end <= itemCount`。`itemCount === 0` 时返回
   `{0, 0, following: true}`。
2. **初始**（`current === null`）：有 `focus` 走规则 3；否则 `following = true`、
   `end = itemCount`、`start = max(end - initialSize, 0)`。冷启动批量 60 条，
   **不是** `maxMounted`。
3. **`focus`**：`following = false`；`start = clamp(focus.index - focus.before, 0, itemCount - 1)`，
   `end = clamp(focus.index + focus.after + 1, start + 1, itemCount)`。
   `focus` 是一次性的，调用方在下一次调用时清掉——它不该继续影响后续分页。
   注意 `end` 可以小于 `itemCount`，这正是「定点开窗」形态。
4. **跟随尾部（保持，不吸附）**：`following` 为真时 `end = itemCount`（`itemCount`
   因流式增长变大，窗口自动跟上），`start` 不动。
   **绝不把 `following === false` 的窗口直接吸到尾部**：那会在一次渲染里挂载上千条。
   从定点开窗回到尾部只能经规则 6 一页页扩，且只有扩到 `end === itemCount` 时
   `following` 才转回真。
5. **向上分页**：`start > 0` 且 `visible.gapTopPx <= limits.bufferPx` 时，
   `start = max(start - pageSize, 0)`，并把 `following` 置假（用户明确往上读了）。
6. **向下分页**：`end < itemCount` 且 `visible.gapBottomPx <= limits.bufferPx` 时，
   `end = min(end + pageSize, itemCount)`。扩到 `itemCount` 后即进入规则 4 的形态，
   `spacerBottom` 归 0，并把 `following` 置真——用户滚回底部就是「要继续看最新」的信号，
   与 `useStickToBottom` 里「gap 回到阈值内则恢复跟随」（`useStickToBottom.ts:180`）
   是同一条语义，两处必须一致，否则会出现「窗口跟着尾部、但贴底意图已关」的撕裂状态。

7. **卸载**：`end - start > maxMounted` 时，截到 `maxMounted`，**截哪端按以下顺序判定**
   （不能无条件截 `start`：用户往上读时那会卸掉他正在看的内容）：

   1. `following` 为真 → 截 `start`（用户在看最新）。
   2. `visible === null` → 截 `start`（尚未测量，保守保住尾部）。
   3. `visible.gapTopPx <= limits.bufferPx`（刚向上分页）→ 截 `end`。
      否则会把刚加载进来的历史立刻卸掉，形成「翻页—卸载—再翻页」的抖动。
   4. `visible.gapBottomPx <= limits.bufferPx`（刚向下分页）→ 截 `start`。
   5. 其余情况按距离：`nearStart = visible.first - start` 与
      `nearEnd = end - visible.last`，较大的一侧被截。

   第 3 条优先于第 5 条是实测出来的：只按距离判会在「向上翻页后视口位于新内容
   中间」时截 `start`，把刚拿到的内容丢掉。这条规则已用一份等高质量模型逐页跑过
   （12 页连续上翻），挂载数稳定在 400、`start` 每步减 200、无震荡。
8. **收缩**：`itemCount` 变小（`/clear`、compact）时，`end = min(end, itemCount)`、
   `start = min(start, end)`，不越界。

`visible` 由组件层给出（读 DOM），`nextWindow` 只消费它——这也是它保持纯函数的原因。

### 2.3 高度簿记

未挂载区间没有真实高度，必须估算。新增 `HeightBook`（同样零 DOM）：

```ts
export interface HeightBook {
  /** 记录一条真实测量值。 */
  record(id: string, height: number): void;
  /** 一条 item 的高度估计：有测量用测量，否则用中位数。 */
  estimate(id: string): number;
  /** 一段区间的总高度估计。 */
  estimateRange(ids: string[]): number;
  /** 当前中位数（用于诊断与测试）。 */
  median(): number;
}
```

中位数而非平均值：工具卡/长代码块是重尾分布，平均值会被少数超长项带偏，
中位数对「绝大多数消息是两三行」这一真实形状更稳。

**估算误差的修正**：向上分页时新挂载的真实高度与估算值必然有差。补偿方式：
挂载后测量真实总高，用「真实总高 − 估算总高」的差值反向调整 `scrollTop`，
使用户视觉上不跳动。该补偿在组件层完成（需要 DOM），但**差值计算属于
`HeightBook`**，可单测。

### 2.4 螺旋式收敛（要断言的是哪些）

翻得越多，测量样本越多，中位数越接近真实分布，因此误差补偿总体变小。第 1 次上翻
可能有几十像素位移，几次之后基本不可感知。

但**不能断言「第 N 次误差 < 第 N-1 次」**：item 高度是重尾分布，某一次的区间里
恰好挤了几个超长工具卡，误差就会反弹。这种断言在真实数据上会假红，在合成数据上
又会掩盖问题。可证伪的不变量是下面三条：

1. **完全测量即零误差**：一个区间的每一条都 `record` 过之后，
   `estimateRange` 必须等于精确总和（误差恒为 0）。这是硬不变量。
2. **样本增加不劣化中位数估计**：在固定分布上追加同分布样本，`median()` 与真实
   中位数之差的绝对值**不增大**（断言 `<=`，不是 `<`）。
3. **聚合误差下降**：取前若干页与后若干页的误差中位数，后者 ≤ 前者。
   这条量的是趋势，与单次波动无关。

**不断言绝对像素**，也不断言单步严格单调。

## 3. 与现有滚动机制的兼容

这是本设计风险最高的部分。现有三套机制都建立在「元素真实存在于 DOM」之上。

### 3.1 `useStickToBottom`（`desktop/src/lib/useStickToBottom.ts`）

它维护一个**贴底意图** `atBottomRef`，基线是 `lastTopRef` / `lastMaxRef`
（`scrollTop` 与 `scrollHeight - clientHeight`）。`onScroll` 只在
「`scrollTop` 相对基线向上超过 `SCROLL_UP_EPSILON_PX`，且 gap 大于阈值」时把意图置 false
（`useStickToBottom.ts:171`）；gap 回到阈值内则恢复 true。

**为什么窗口化会打坏它**：同一文件 `:66-68` 记录了本项目实测过的事实——
**WKWebView/Safari 不实现 scroll anchoring**。所以：

- 在视口**上方**挂载更多条目 → 内容变高，`scrollTop` 不变，用户看到的内容整体下移。
  必须由我们补 `scrollTop += Δ`；补偿是**向下**移动，不会误判成上翻，安全。
- 从视口**下方**卸载条目（规则 7 判据下截 `end` 侧）→ `scrollHeight` 变小，浏览器把
  `scrollTop` 往**下**夹，`onScroll` 会读到 `top < lowestPossible - eps` →
  **误判成用户上翻，跟随被永久关闭**。这是唯一真正危险的方向。
- 首次挂载（spacer 的估算高度换成真实高度）→ 同上，方向取决于估算偏大还是偏小。

**处理（只用现有 API，不新增「冻结」概念）**：每次窗口变更按固定顺序做一次事务：

1. 记下 `wasFollowing = isFollowing()`。
2. 改窗口，等布局落定（`requestAnimationFrame` 内读一次 `scrollHeight`）。
3. 若上方有新挂载：`scrollTop += (新 scrollHeight - 旧 scrollHeight)`；若上方只是
   估算换成真实（spacerTop 缩小），用 `HeightBook` 的「真实 − 估算」差值反向补。
4. 收尾二选一：`wasFollowing` 为真则 `scrollToBottom()`（它同时重设基线），
   否则 `releaseFollow()`（它把基线对齐到当前 `scrollTop` 与 `max`，且**不动视口**）。
   两者都顺带把 `lastTopRef`/`lastMaxRef` 修正到新布局，因此下一次 `onScroll`
   的基线是干净的。

**窗口的 `following`（§2.2）与贴底意图是两份状态，但必须同向。** 规则 5 置假时
要 `releaseFollow()`；规则 6 扩到尾部时 `scrollToBottom()` 或 `followIfAtBottom()`
会让贴底意图回真。两者由同一次事务一起改，不能只改一份——否则会出现「窗口停在
历史中间、但每个 delta 仍把你拽回底部」或反向的撕裂。`ChatView` 现有的
`scrollToBottom` / `releaseFollow` 调用点（发消息、切会话、恢复锚点）保持原样。

**顺序不能变**：必须在补偿之后才调 `releaseFollow()`。反过来会把「补偿前的旧基线」
留在 ref 里，下一次 `onScroll` 仍会误判。

"冻结"只是这段事务的名字，不需要新增 API；`releaseFollow` 的既有注释（`:45-51`）
和实现（`:128-136`）已经写明它就是「放弃跟随但不移动视口，用于恢复阅读位置」，
语义吻合。它的另一半作用恰好是本设计需要的：它把 `lastTopRef`/`lastMaxRef` 对齐到
「被浏览器夹过的」新位置，于是「卸载导致 `scrollHeight` 变小、`scrollTop` 被向下夹」
不会被下一次 `onScroll` 读成用户上翻。

### 3.2 `viewAnchor`（`desktop/src/lib/viewAnchor.ts`）

恢复逻辑依赖「按 `data-item-id` 找 DOM 节点」。锚点可能在未挂载区间，
届时 `findAnchor`/`restoreScrollTop` 都会落空。要注意现有代码**不会**因此报错，
而是走退路：`restoreScrollTop` 在 `box` 缺失时返回 `null`，调用方回退到底部
（`viewAnchor.ts:59-62`）。也就是说窗口化后，「锚点没挂载」会被静默降级成「跳到底部」——
这是必须消除的行为退化。

处理：恢复流程改为三步。

1. **先定位**：`anchor.itemId` → `session.items` 里的 index。数据层查找，不依赖 DOM。
   `index === -1` 才是「锚点真的没了」，回退底部。
2. **再开窗**：`FocusRequest { index, before: buffer, after: 3 * buffer }`
   （用 `WindowLimits.buffer` 表达，不写字面量），保证锚点落在挂载区间内。
3. **最后对位**：布局落定后按 `offset` 写入 `scrollTop`，完全复用
   `restoreScrollTop` 的算术（`top - anchor.offset`），只是 `box.top` 现在由
   `HeightBook` + 已测量节点拼出来，而不是全量 DOM。

这样「找不到节点」的两种语义被彻底分开：**没挂载**（先开窗）与**真的没了**
（`index === -1`，回退底部）。

### 3.3 `ResizeObserver` 漂移校正（`ChatView.tsx:648-653`）

现在只观察 `[data-item-id]`（`:652`），且观察集合在 effect 建立时**一次性枚举**。
窗口化后有两个问题：

- 新挂载的条目不在观察集合里，图片异步加载变高观察不到。
- 未挂载区没有节点可观察。

处理：观察对象改为 `spacerTop` / `spacerBottom` 两个元素（高度变化即「布局变了」，
触发一次估算校正），并在窗口变化后重新枚举挂载区的 `[data-item-id]`。
**不**尝试观察未挂载区——它们没有 DOM。

重新枚举需要有上界：只在 `start`/`end` 变化时重建观察集合，而不是每次渲染。

### 3.4 `capture()`（`ChatView.tsx:703`）

现在遍历全部 `[data-item-id]` 逐个 `getBoundingClientRect`（`:703-707`），3251 条即
每帧 3251 次强制布局和 3251 个 `ItemBox`。窗口化后天然只剩挂载区（≤400 条），
成本降到 O(400)。

**这不是完全等价，有一处语义差异必须写明**：视口只能出现在两个位置，
锚点候选也相应只有两种来源。

- **视口落在挂载区**：与现状完全等价，候选就是挂载区的 `[data-item-id]`。
- **视口落在 `spacerTop` 里**（用户把已挂载的历史全部滚到视口下方）：没有任何已挂载
  消息的顶边在视口顶之上，`findAnchor` 会退到「最靠上的已挂载那条」
  （`viewAnchor.ts:44-52` 的 `first` 退路）。那一条并不是用户真正在读的位置。

缓解：挂载区顶边一进入视口就触发向上分页（§2.1 的 `buffer`），使「视口落在 spacerTop
内」只持续一帧；分页到顶（`start === 0`）后，最靠上的已挂载条就是第一条 item，
退路语义正确。这个短暂偏差不进断言，但要在实现注释里保留原因。

## 4. 搜索下沉到数据层

现状：`useTranscriptSearch`（`desktop/src/hooks/useTranscriptSearch.ts`）在渲染后
用 `containerRef.querySelectorAll("mark.search-hit")` 扫 DOM（`:54`），
`mark` 由 `rehypeSearchMark` 在 markdown 管线里生成（`MarkdownText.tsx:80`）。
计数、`next`/`prev`、以及 `reselect` 全部基于这批 DOM 节点。

窗口化后未挂载的历史不在 DOM 里，这套机制会**静默缩小搜索范围**——不报错，
只是搜不到。这是必须解决的正确性问题，不是体验优化。

### 4.1 新分工

- **匹配**：在 `session.items` 上做，复用既有的 `findMatches`
  （`lib/searchHits.ts`）与 `itemText`，产出 `{ index, itemId, offset }[]`：
  `offset` 是这条 item 内第几个命中（同一条可能有多个，与现有
  「每个 `mark` 算一次命中」的计数口径保持一致）。
  计数、`next`/`prev`、当前命中全部基于这个数组。
- **高亮**：`rehypeSearchMark` 原样保留，只负责给已挂载区间内的命中加 `mark`。
  未挂载区间没有 `mark` 节点，是窗口化的必然结果，不影响计数。
- **跳转**：命中 index → `FocusRequest`（与 §3.2 同一套开窗）→ 布局落定 →
  在该 index 确实已挂载后，用 `scrollIntoView({ block: "center" })` 定位到具体
  `mark`；若该 item 内有多个命中，按 `offset` 选第几个 `mark`。

### 4.2 `reselect` 的语义必须改写（不能简单移植）

现在 `reselect` 用 `initialActiveIndex(rects, viewport)` 选「视口内第一个命中，
否则视口下方最近的一个」（`searchHits.ts:75-89`），这需要**每个命中的真实像素矩形**。
窗口化后绝大多数命中没有 DOM，算不出 `top`。

三个选项：

| 选项 | 做法 | 问题 |
|---|---|---|
| A | 只考虑已挂载的命中 | 冷启动时视口附近可能一个都没挂载，退化成「永远第一个」，与现有行为差异大 |
| B | 用 `HeightBook` 估算每个命中的 `top` 再喂给 `initialActiveIndex` | 估算误差会让"就近"选错；且要遍历全部命中算偏移，O(命中数) 但只做一次，可接受 |
| C | 改成按 index 就近 | **不等价**：命中密度不均（一条长消息里 20 个命中，中间隔 500 条无命中），index 距离与视觉距离无关 |

**选 B**：`initialActiveIndex` 的输入类型 `RelRect[]` 不变，只是 `top` 从
「测量」变成「`HeightBook` 估算 + 已挂载项实测修正」。这样：

- 选中的命中若已挂载 → 现有逻辑原样生效。
- 若未挂载 → 估算 `top` 仍能选出「视口附近那个」，然后开窗跳过去。
- 估算误差只影响「打开查找条时先选中哪个」，不影响命中总数，可接受。

`initialActiveIndex` 与 `findMatches` 都已存在且无 DOM 依赖，**不需要改它们**，
只需要新增「命中 → 估算矩形」的适配层（放 `transcriptWindow.ts`，可单测）。

### 4.3 重扫时机

`session.items` 是就地变更的（`items[i] = x` / `push`），不是不可变数组，
因此不能只依赖引用比较。

- `query` 变化（防抖 120ms，`SEARCH_DEBOUNCE_MS`）→ 重扫。
- `structuralVersion` 变化 → 重扫。
- **纯文本 delta（`textVersion` 变化）→ 不重扫**。理由：流式期间桌面端文本按
  `useThrottledText` 的 60ms 窗口更新，重扫 3251 条是纯浪费；搜索是用户主动行为，
  短暂滞后可接受。代价：生成中的最后一条消息可能搜不到刚出现的文本，直到本轮结束。

这个滞后是**明确接受的取舍**，须写进测试断言（断言扫描次数，不描述体感）。

### 4.4 `useTranscriptSearch` 的改造边界

它现在是「DOM 扫描器」，改造后是「数据层匹配 + DOM 高亮标注」两条腿：

- 入参新增 `items`、`itemCount`、`structuralVersion`、`textVersion`，
  以及一个「请求开窗到 index」的回调。
- 出参的 `total`/`active`/`next`/`prev` 签名不变（`App.tsx:864` 的调用点尽量不动）。
- `SEARCH_HIT_SELECTOR` 保留：它仍用于「在当前挂载区里找 mark 做高亮标注」。

`App.test.tsx` / `useTranscriptSearch.test.tsx` 里断言 DOM 扫描行为的用例需要
按新语义改写，这是本设计唯一的测试语义变更。

## 5. 数据流

```
App（宿主）
  ├─ session.items ────────────────┐
  ├─ anchorFor(threadId)            │
  └─ searchQuery ──────────────┐    │
                               ↓    ↓
ChatView
  ├─ useTranscriptWindow(items, anchor, searchHit)
  │     → { start, end, visible, spacerTop, spacerBottom }
  ├─ spacerTop
  ├─ HistoryWindow（memo，渲染 items[start..min(end, itemCount-1))）
  ├─ spacerBottom
  └─ LiveTail（渲染 items[itemCount-1]，独立于窗口）
```

`useTranscriptWindow` 是新增的 hook，负责：
- 维护 `WindowState`，每次渲染调用 `nextWindow` 重算
- 测量 `visible`（读 DOM，只测挂载区）
- 把 `anchor.itemId` / 搜索命中 index 翻译成一次 `FocusRequest` 并消费掉
- 与 `useStickToBottom` 做窗口变更补偿事务（§3.1）

它**不含**高度簿记（那是 `HeightBook`）与纯计算（那是 `transcriptWindow.ts`）。

## 6. 验证

### 6.1 结构性不变量（进 CI）

一律断言结构，不断言毫秒（机器快慢会飘）。

| 不变量 | 载体 |
|---|---|
| 3000 items 冷启动只挂 60 条（59 HistoryWindow + 1 LiveTail） | `ChatView.test.tsx`：建 3000 items，断言 `[data-item-id]` 数为 60 |
| 上翻一页 → 260，且 `following` 转假 | `transcriptWindow.test.ts` |
| 翻一页就停：`scrollTop` 补偿后 gap 回到阈值外，连续两次调用区间不变 | `transcriptWindow.test.ts`（喂补偿后的 gap，断言不动） |
| 冷启动不翻页：初始 `gapTopPx` 约为窗口高度，断言首次调用后仍只挂 `initialSize` | `transcriptWindow.test.ts` |
| 连续上翻 12 次，挂载数恒 ≤ `maxMounted`、`start` 单调递减、无回翻 | `transcriptWindow.test.ts`（等高质量模型逐页跑，断言序列） |
| 上翻超过 `maxMounted` 时截 `end` 侧（不卸用户正在读的） | `transcriptWindow.test.ts` |
| 定点开窗后**不会**一步吸到尾部 | `transcriptWindow.test.ts`：`end < itemCount` 且 `current.end` 不变 |
| `following` 为真时 `itemCount` 增长，`end` 跟随、`start` 不动 | `transcriptWindow.test.ts` |
| `following` 为假时 `itemCount` 增长，`end` 不动 | `transcriptWindow.test.ts` |
| 收缩到小于窗口起点时重置为尾部小窗口（不留空窗口） | `transcriptWindow.test.ts` |
| 纯文本 delta 不改变窗口 | 同上 |
| 锚点在未挂载区时先开窗再对位 | `ChatView.test.tsx` |
| 数据层搜索能命中未挂载区间 | `useTranscriptSearch.test.tsx` |
| 命中未挂载项时开窗到该位置 | 同上 |
| 纯文本 delta 不触发重扫 | 同上（断言扫描次数） |
| 完全测量后误差为 0 | `HeightBook` 单测（硬不变量） |
| 追加同分布样本时中位数误差不增大；前后若干页误差中位数后者 ≤ 前者 | `HeightBook` 单测（断言 `<=`，不断言单步严格下降） |

### 6.2 性能参考（不进 CI，可复算）

沿用 `desktop/scripts/perf/` 的既有做法：只读脚本、消费真实生产代码、
数字只记录不进断言。脚本是 `desktop/scripts/perf/open.mjs`，用法与产出见 §7。

**优化后实测**（同一份 3251 items 日志，Node 23 + jsdom，本机跑 3 次）：

| 场景 | 耗时 | 挂载条数 | DOM 节点 |
|---|---|---|---|
| 全量一次性渲染（优化前） | 888–1099 ms | 3251 | 11025 |
| 真实分帧回放（优化前冷启动路径） | 937–1310 ms / 26 帧 | 3251 | 11025 |
| 窗口化冷启动（首屏 60 条） | 27–43 ms | 60 | 451 |
| **窗口化冷启动 + 上翻到 cap** | **203–240 ms / 3 帧** | **401** | **4456** |

「上翻到 cap」这一行复用生产 `nextWindow`（脚本 `import` 真实 `.ts`），按
"视口贴住挂载区顶边"逐页请求，而不是手写一份翻页规则——测的必须是真算法。
`mounted=401` = `maxMounted` 400 + 恒由尾部单独渲染的最后一条。

与 §1.1 的 773 ms 对照：首屏从 ~780 ms 降到 ~30 ms（约 26×），且无论会话多长
都只挂 60 条；即便用户一路翻到卸载上限，也只到 ~220 ms。绝对数值仍不可等同于
真机（§7 已说明诚实边界），可迁移的是**比值**与**结构量**（挂载条数、DOM 节点数、
帧数），它们与会话长度解耦。

### 6.3 回归面

现有必须保持全绿的测试：
- `ChatView.test.tsx`（贴底、锚点恢复、流式跟随、搜索高亮）
- `session.test.ts`（`structuralVersion` / `textVersion`）
- `useTranscriptSearch.test.tsx`
- `App.test.tsx`（宿主接线）

## 7. 测量方法（可复现）

```bash
# 服务端：真实 app-server + 真实会话日志。
# --project 必须指向「会话日志所属的仓库」，不是 worktree。
# app-server 按 project 解析线程目录；在 worktree 里不带 --project 会全部报
# `unknown thread`（实测踩过）。--threads 单独指向日志目录。
M=/path/to/main/checkout
python3 scripts/bench_resume.py \
  --project "$M" \
  --threads "$M/.yi-agent/threads" \
  --bin "$M/yi-agent-rs/target/release/yi-agent"

# 前端：真实 react-markdown 管线 + 真实 items 快照。
# 自动沿祖先目录找日志，worktree 会回溯到主仓库，无需参数。
cd desktop && node scripts/perf/open.mjs
```

两个脚本都已落地，均为**只读**（不写仓库、不写会话）：

- `scripts/bench_resume.py` —— spawn 真实 `yi-agent app-server --listen stdio://`，
  发 `initialize` → 对最长与一条短会话各发两次 `thread/resume`，打印总时长与首帧
  时长。worktree 里没有自己的 `.yi-agent/threads`，故用 `--threads` 指向主仓库的
  真实日志。本机复现结果：短 55~59 ms / 长 114~115 ms（与 §1.1 同形状）。
  **注意 `--project` 不能省**：app-server 按 project 解析线程目录，在 worktree 里跑
  而不带 `--project <主仓库>` 会全部报 `unknown thread`——这是实测踩到的坑，
  上面命令块里的注释就是为此写的。
- `desktop/scripts/perf/open.mjs` —— 自动定位真实会话日志（沿祖先目录找，并按
  worktree 的 `.git` 文件回溯到主仓库），用 jsdom + 真实插件测「全量 / 分帧 /
  窗口 60 / 窗口 400 / 窗口 30」五种策略。找不到日志时**如实报告并退出 0**，
  不构造假数据（与既有 `checkpoint.mjs` 同一规则）。

**诚实边界**：前端数字跑在 jsdom 下，不是 WKWebView。绝对数值不可等同于真机，
可信的是**增长趋势与倍数关系**。

## 8. 明确不在本设计内

三项独立问题，各自单列，不混进本设计：

1. **`upsertItems` 的 O(N²)**（`session.ts:518`）：换成 `Map<id, index>` 即可，
   改动小但会触及去重语义，单独一个 PR。
2. **`read_log` 的存储放大**（34.8 MB 中 24 MB 是重复的 context 快照）：
   需要数据迁移，风险最高，单独一个 PR。
3. **桌面端复用 IndexedDB 内容缓存**（去掉 `isRemoteClient()` 门控）：
   收益大（第二次打开接近瞬时）但涉及缓存失效与世代对账语义，单独一个 PR。

## 9. 风险与缓解

| 风险 | 影响 | 缓解 |
|---|---|---|
| spacer 估算高度严重失真 | 滚动条跳动、锚点偏移 | 中位数 + 螺旋收敛；§2.4 的三条可证伪不变量 |
| 与 `useStickToBottom` 意图冲突 | 流式时被拽回底部或不再跟随 | §3.1 的补偿事务（尤其「卸下方」方向）+ 既有全部贴底测试保持绿 |
| 图片异步加载改变挂载区高度 | 锚点漂移 | 复用既有 `ResizeObserver` 校正，观察对象加 spacer |
| 搜索滞后于流式文本 | 生成中搜不到最新几个字 | 明确接受的取舍，UI 提示 + 测试固定 |
| 窗口化引入的回归难以定位 | 长会话专门问题 | 结构性不变量测试先行（TDD），每项先红后绿 |

## 10. 实施顺序

1. `transcriptWindow.ts`（`nextWindow`，含上述 8 条规则的正反用例）+ `HeightBook`（纯函数，全部单测）
2. `useTranscriptWindow` hook + `ChatView` 接线（只做窗口化，不动搜索）
3. 修 `capture()` 与 `ResizeObserver` 的观察对象
4. 搜索下沉到数据层 + 开窗跳转
5. 用 `desktop/scripts/perf/open.mjs` 与 `scripts/bench_resume.py` 记录优化前后数字
   （两者已随本设计落地，此步只跑不改）

每步先写失败测试。第 3 步与第 4 步互相独立。
