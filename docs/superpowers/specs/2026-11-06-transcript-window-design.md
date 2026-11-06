# 长会话首屏：转录窗口化 + 向上分页 设计

日期：2026-11-06
状态：设计待评审

## 1. 问题

打开一条历史很长的会话，首屏明显比短会话慢。用户观察：「长历史的打开时间长，
短历史的打开时间快」，短会话接近「不到一秒」。

### 1.1 实测：慢在哪一段

测量方式见 §7。release 二进制 + 本机真实会话日志（最长 3251 items / 34.8 MB）。

**服务端 `thread/resume`（真实 app-server，stdio）**

| 阶段 | 短会话 5 items | 长会话 3251 items |
|---|---|---|
| `initialize` | 16 ms | 16 ms |
| `thread/resume` 总计 | 109 ms | 259 ms |
| 首个回放帧 | 108 ms | 220 ms |
| 回放帧数（200 items/帧） | 4 | 29 |
| 再次 resume（温会话） | 56 ms | 249 ms |

**结论：服务端只差 150 ms。** 服务端不是主要瓶颈，不需要为它做架构改动。

**前端渲染（真实 items + 真实 react-markdown/remark-gfm/rehype-highlight，jsdom）**

用 `desktop/scripts/perf/open.mjs` 对同一份 3251 items 的会话实测（含 1890 个工具卡的
JSON 序列化，因此比早先只算 markdown 的小样本更贵）：

| 场景 | 耗时 | 挂载条数 | DOM 节点 |
|---|---|---|---|
| 一次性渲染全部（当前行为） | 1043 ms | 3251 | 11025 |
| 按真实 200-items/帧分帧重渲染（当前实际回放路径） | 1250 ms | 3251 | 11025 |
| 只挂载最近 60 条（本设计首屏） | **35 ms** | 60 | 451 |
| 只挂载最近 400 条（卸载上限） | 163 ms | 400 | 2334 |
| 参考：只挂载最近 30 条 | 13 ms | 30 | 189 |

两组样本（1361 markdown-only 与 3251 全类型）给出一致的形状：**全量渲染比窗口化贵
30 倍**，且分帧回放比一次性还慢。

分帧回放实测**比一次性渲染还慢**：每帧都要重新协调一份越来越长的列表，而挂载的
条目从头到尾都是全量。分帧只让内容更早出现，它本身不是优化——真正的杠杆是
**减少挂载条数**。

### 1.2 根因清单（按贡献排序）

1. **转录区无窗口化。** `ChatView` 的 `HistoryPrefix` 是
   `for (let i = 0; i < count; i += 1)` 一次性建全部 `ChatItem`
   （`desktop/src/components/ChatView.tsx:353`）。1361 条 markdown 全量解析，
   这是 14 倍差距的来源。
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

- 打开一条 3251 items 的会话，**首屏可交互 ≤ 150 ms**（当前 1506 ms）。
- 滚动、搜索、切换会话的耗时**不随会话长度增长**。
- 内容完整可达：未挂载的历史仍能被滚动到、被搜索到、被阅读位置恢复命中。

## 2. 核心模型：spacer 窗口

不做「自己接管 scrollTop 换算」的虚拟滚动，而是用两个占位元素撑住未挂载区间的高度，
让浏览器继续做原生滚动：

```
[spacerTop]      高度 = 未挂载历史的高度估算
  挂载区间 windowStart..windowEnd   ← 真实 ChatItem
[spacerBottom]   高度 = 未挂载未来项的估算（通常为 0，尾部永远挂载）
[LiveTail]
```

好处：
- 滚动条比例真实（高度由 spacer 撑出），用户看到的滚动位置有意义。
- 不需要重写 `useStickToBottom` 的 `scrollHeight` 语义——spacer 是内容的一部分。
- `viewAnchor` 记录的「视口顶部那条消息」在语义上不变。

### 2.1 窗口参数

| 参数 | 值 | 依据 |
|---|---|---|
| 首屏条数 | 60 | 实测 54 ms，接近感知下限 |
| 向上每次追加 | 200 | 与回放分帧大小一致，一页在惯性滚动内加载完 |
| 视口上方缓冲 | 20 条 | 上滑时先有内容，避免空白闪烁 |
| 视口下方缓冲 | 20 条 | 下滑同理 |
| 卸载阈值 | 挂载数 > 400 | 超出后卸载远端，防止「一路翻到顶」把全量找回 DOM |

400 的取值：60 首屏 + 200 页 × 2 已翻页 = 460，再往上卸载最早的一页。这样
「连续翻两页」不会触发卸载，第三页开始卸载最远的，DOM 节点数稳定在 400 量级。

### 2.2 纯函数内核

新增 `desktop/src/lib/transcriptWindow.ts`，**零 React、零 DOM**，可完整单测：

```ts
export interface WindowState {
  /** 已挂载区间的起点（含）。 */
  start: number;
  /** 已挂载区间的终点（不含）。 */
  end: number;
}

export function clampWindow(
  state: WindowState,
  itemCount: number,
  limits: { maxMounted: number },
): WindowState;
```

规则：
- `start >= 0`，`end <= itemCount`，`start <= end`。
- 尾部永远挂载：`end === itemCount`（除非调用方显式指定 focus 区间）。
- `end - start > maxMounted` 时，从 `start` 侧卸载，保持 `end` 不变
  （用户正在看底部，底部不能动）。
- `itemCount` 缩小时（会话被 clear/compact）区间随之收缩，不越界。

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

### 2.4 螺旋式收敛

翻得越多，测量样本越多，中位数越准，误差补偿越小。第 1 次上翻可能有几十像素
位移，第 3 次之后基本不可感知。这一点必须在测试里固定为结构性断言（断言
「第 N 次的误差 < 第 N-1 次」），**不断言绝对像素**。

## 3. 与现有滚动机制的兼容

这是本设计风险最高的部分。现有三套机制都建立在「元素真实存在于 DOM」之上。

### 3.1 `useStickToBottom`（`desktop/src/lib/useStickToBottom.ts`）

它靠 `scrollHeight` / `clientHeight` / `scrollTop` 三者关系维护「贴底意图」，
并用「视口相对上次位置是否向上移动」判定用户上翻。

- spacer 是内容的一部分，`scrollHeight` 语义不变，**判据不需要改**。
- 新增风险：窗口变化本身会改 `scrollHeight`（挂载/卸载让真实高度替换估算高度）。
  若不处理，`onScroll` 会把这次变化读成「用户上翻/内容增长」。
- 处理：窗口重算前后**冻结意图**。组件在布局变更前记下 `isFollowing()`，
  变更后若原本贴底则 `scrollToBottom()`，否则 `releaseFollow()` 并把基线
  重置到新位置。冻结窗口内不接受新的意图写入。

### 3.2 `viewAnchor`（`desktop/src/lib/viewAnchor.ts`）

恢复逻辑依赖「按 `data-item-id` 找 DOM 节点」。锚点可能在未挂载区间，
届时找不到节点会被误判为「锚点真的没了」→ 回退到底部。

处理：恢复流程改为两段：
1. **先定位**：`anchor.itemId` → `session.items` 里的 index（数据层查找，
   不依赖 DOM）。
2. **再开窗**：以该 index 为中心开窗（前 20 / 后 40），保证锚点落在挂载区间内。
3. **最后对位**：等布局落定后按 `offset` 写入 `scrollTop`，完全复用现有
   `restoreScrollTop`。

这样「找不到节点」的两种语义被彻底分开：**没挂载**（先开窗）与
**真的没了**（`index === -1`，回退底部）。

### 3.3 `ResizeObserver` 漂移校正（`ChatView.tsx:652`）

现在只观察挂载区的 `[data-item-id]`。未挂载区变高（图片异步加载、工具卡展开）
观察不到，估算值会陈旧。

处理：观察对象增加两个 spacer。spacer 高度变化即视为「布局变了」，触发一次
估算校正。**不**把未挂载区纳入观察——它们没有 DOM 节点可观察。

### 3.4 `capture()`（`ChatView.tsx:703`）

现在遍历全部 `[data-item-id]` 求 `getBoundingClientRect`。

处理：只遍历挂载区间。这在语义上**完全等价**——锚点只取自「视口顶部可见的那条」，
而未挂载区永远不在视口内。成本从 O(N) 降到 O(视口)，3251 条时降约 40 倍。

## 4. 搜索下沉到数据层

现状：`useTranscriptSearch` 扫 DOM 的 `mark.search-hit`（`SEARCH_HIT_SELECTOR`）。
窗口化后未挂载的历史搜不到，与「搜索覆盖全量」的要求冲突。

### 4.1 新分工

- **匹配**：在 `session.items` 上做（数据层），产出命中列表
  `{ index, itemId, ... }[]`。计数、上一个/下一个都基于它。
- **高亮**：`rehypeSearchMark` 保留，只负责给已挂载区间内的命中加 `mark`。
  未挂载区间的命中没有 mark 节点，这是窗口化的必然结果，不影响计数。
- **跳转**：命中项 index → 开窗（与 §3.2 同一套开窗逻辑）→ 布局落定 →
  `scrollIntoView` 或按 `HeightBook` 估算的偏移写入 `scrollTop`。

### 4.2 重扫时机

`session.items` 是就地变更的（`push` / `items[i] = x`），不是不可变数组，
因此不能只依赖引用比较。

- `query` 变化（防抖 120ms）→ 重扫。
- `structuralVersion` 变化 → 重扫。
- **纯文本 delta（`textVersion` 变化）→ 不重扫**。理由：流式生成期间每 60ms
  一次 delta，重扫 3251 条是纯浪费；搜索是用户主动行为，短暂滞后可接受。
  代价：生成中的最后一条消息可能搜不到刚出现的文本，直到本轮结束。

这个滞后是**明确接受的取舍**，须写进 UI 提示与测试断言。

## 5. 数据流

```
App（宿主）
  ├─ session.items ────────────────┐
  ├─ anchorFor(threadId)            │
  └─ searchQuery ──────────────┐    │
                               ↓    ↓
ChatView
  ├─ useTranscriptWindow(items, anchor, searchHit)
  │     → { start, end, spacerTop, spacerBottom }
  ├─ HistoryWindow（挂载区，memo）
  ├─ spacerTop / spacerBottom
  └─ LiveTail（永久挂载）
```

`useTranscriptWindow` 是新增的 hook，负责：
- 维护 `WindowState`
- 监听滚动容器，判断「将到顶/将到底」
- 触发向上分页 / 卸载远端
- 与 `useStickToBottom` 协调意图冻结

它**不含**高度簿记（那是 `HeightBook`）与纯计算（那是 `transcriptWindow.ts`）。

## 6. 验证

### 6.1 结构性不变量（进 CI）

一律断言结构，不断言毫秒（机器快慢会飘）。

| 不变量 | 载体 |
|---|---|
| 首屏只挂载 60 条 + 尾部 | `ChatView.test.tsx`：建 3251 items，断言 DOM 里 `[data-item-id]` 数为 60 |
| 上翻一次挂载 260 条 | 同上，模拟滚到顶，断言 260 |
| 超过 400 条后卸载远端，`end` 不动 | `transcriptWindow.test.ts` |
| 纯文本 delta 不改变窗口 | `transcriptWindow.test.ts` |
| 锚点在未挂载区时先开窗再对位 | `ChatView.test.tsx` |
| 数据层搜索能命中未挂载区间 | `useTranscriptSearch.test.ts` |
| 命中未挂载项时开窗到该位置 | 同上 |
| 纯文本 delta 不触发重扫 | 同上（断言扫描次数） |
| 误差随翻页次数收敛 | `HeightBook` 单测，断言单调下降 |

### 6.2 性能参考（不进 CI，可复算）

沿用 `desktop/scripts/perf/` 的既有做法：只读脚本、消费真实生产代码、
数字只记录不进断言。新增 `desktop/scripts/perf/open.mjs`：
用冻结的真实 items 快照跑「打开一次」的全流程计时，输出首屏/滚动/DOM 节点数。

### 6.3 回归面

现有必须保持全绿的测试：
- `ChatView.test.tsx`（贴底、锚点恢复、流式跟随、搜索高亮）
- `session.test.ts`（`structuralVersion` / `textVersion`）
- `useTranscriptSearch.test.tsx`
- `App.test.tsx`（宿主接线）

## 7. 测量方法（可复现）

```bash
# 服务端：真实 app-server + 真实会话日志
python3 scripts/bench_resume.py --threads ~/.yi-agent/threads

# 前端：真实 react-markdown 管线 + 真实 items 快照
cd desktop && node scripts/perf/open.mjs
```

两个脚本都已落地，均为**只读**（不写仓库、不写会话）：

- `scripts/bench_resume.py` —— spawn 真实 `yi-agent app-server --listen stdio://`，
  发 `initialize` → 对最长与一条短会话各发两次 `thread/resume`，打印总时长与首帧
  时长。worktree 里没有自己的 `.yi-agent/threads`，故用 `--threads` 指向主仓库的
  真实日志。本机复现结果：短 122 ms / 长 196 ms（与 §1.1 同形状）。
- `desktop/scripts/perf/open.mjs` —— 自动定位真实会话日志（沿祖先目录找，并按
  worktree 的 `.git` 文件回溯到主仓库），用 jsdom + 真实插件测「全量 / 分帧 /
  窗口 60 / 窗口 400 / 窗口 30」五种策略。找不到日志时**如实报告并退出 0**，
  不构造假数据（与既有 `checkpoint.mjs` 同一规则）。

服务端脚本的做法：spawn `yi-agent app-server --listen stdio://`，依次发
`initialize` → `thread/resume`，记录总时长与首帧时长。前端脚本的做法：从
`.yi-agent/threads/*.jsonl` 取最长会话的 items，用 jsdom + 真实
`react-markdown` + `remark-gfm` + `rehype-highlight` 渲染，分别测全量、
分帧、窗口化三种策略。

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
| spacer 估算高度严重失真 | 滚动条跳动、锚点偏移 | 中位数 + 螺旋收敛；测试断言收敛性 |
| 与 `useStickToBottom` 意图冲突 | 流式时被拽回底部或不再跟随 | 意图冻结 + 既有的全部贴底测试保持绿 |
| 图片异步加载改变挂载区高度 | 锚点漂移 | 复用既有 `ResizeObserver` 校正，观察对象加 spacer |
| 搜索滞后于流式文本 | 生成中搜不到最新几个字 | 明确接受的取舍，UI 提示 + 测试固定 |
| 窗口化引入的回归难以定位 | 长会话专门问题 | 结构性不变量测试先行（TDD），每项先红后绿 |

## 10. 实施顺序

1. `transcriptWindow.ts` + `HeightBook`（纯函数，全部单测）
2. `useTranscriptWindow` hook + `ChatView` 接线（只做窗口化，不动搜索）
3. 修 `capture()` 与 `ResizeObserver` 的观察对象
4. 搜索下沉到数据层 + 开窗跳转
5. `scripts/perf/open.mjs` 记录优化前后数字

每步先写失败测试。第 3 步与第 4 步互相独立。
