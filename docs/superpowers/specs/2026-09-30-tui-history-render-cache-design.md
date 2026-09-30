# TUI 历史渲染缓存设计（长会话不再卡顿）

**目标：** 消除 TUI 在长会话下的卡顿。卡顿的根因不在对话长度本身，而在渲染路径把
**整个 scrollback** 重新解析、重新折行的次数与对话长度成正比——每帧 7 次。

**状态：** 已实现。

**相关文档：** `docs/project-management/yi-agent-tui.md`。

---

## 1. 问题

### 1.1 现象

对话变长后 TUI 明显卡顿：按键回显、滚动、流式输出都变得迟滞。debug 构建下（`cargo
build` 的默认产物，也是开发时常跑的形态）400 回合左右单帧约 190ms，而主循环的 poll
超时是 33ms（目标 30fps），CPU 被钉死在重绘上。

### 1.2 根因

`HistoryState::flattened_line_count` 对每个 cell 调用 `line_count`，而 `line_count`
调用 `lines()`——即**完整的 markdown 解析 + 按显示宽度折行**，只为拿一个行数。

而 app.rs 的空闲帧（无事件）会走 7 次全量遍历：

```
app.rs:331  text_width()              -> flattened_line_count()   # 1
app.rs:333  reconcile_scroll_offset() -> max_scroll_offset()      # 2
app.rs:332                           -> text_width() 内再调一次   # 3
app.rs:405  text_width()                                          # 4
app.rs:407  restore_viewport_anchor() -> flattened_line_count()   # 5
app.rs:412  capture_viewport_anchor() -> flattened_line_count()   # 6
history.rs  render -> flattened_lines() -> 每个 cell lines()      # 7 + 全量重渲染
```

实测（release，120x40 终端）：400 回合 = 3200 显示行，单次全量遍历 2.1ms、单次全量
渲染 4.7ms，七次合计约 19ms；debug 下约 190ms。

另外两条放大因素：

- 恢复的长会话或超长单条消息代价极高：1MB 的 assistant 消息 = 8715 行，单次渲染
  release 就 27ms、debug 400ms。
- 显示行数与字节数之比约 1:120（markdown 一个词折一行），所以卡顿阈值比预想低。

---

## 2. 设计

### 2.1 核心：按宽度缓存每个 cell 的渲染结果

`HistoryState` 内新增 `cache: RefCell<HistoryCache>`：

- `entries: Vec<CachedCell>`，每个 `CachedCell` 保存该 cell 已渲染的
  `Vec<Line<'static>>` 与一个 `fingerprint`。
- `cumulative: Vec<usize>` / `part_offsets: Vec<usize>`：行数与起点的 O(1) 查表，
  供 `flattened_line_count` / `max_scroll_offset` / `locate_line` 使用。
- `reflected_generation`：该 cache 对应的内容版本号。

`RefCell` 是必需的：读取路径（`text_width`、`flattened_line_count`……）只拿
`&self`，但首次访问需要渲染并写入。

`ensure_cache(width)` 是唯一的入口：

1. 宽度变了 → 丢弃全部已渲染行（折行与宽度绑定），但保留 fingerprint。
2. **快路径**：若 `cache.width == width && reflected_generation ==
   content_generation && entries.len() == cells.len()`，直接返回。空闲帧走到这里
   即 O(1)。
3. 否则收集 fingerprint 变化的 cell，在**释放 borrow 之后**渲染（渲染走 `&self`，
   持锁跨渲染会 panic），再写回。

### 2.2 变更检测：内容版本号 + fingerprint

- `content_generation: Cell<u64>` 由所有改 `cells` 的路径递增：`push`、
  `push_event`、`clear`、`invalidate_cell`。这是快路径的依据。
- `fingerprint` 兜住"原地修改"这一类：流式 `AssistantText` 追加、`ToolCall` 状态
  变化、权限 `resolved`、两个 `expanded` 开关。缓存命中要求 fingerprint **相等且**
  `lines.is_some()`（失效会清空 lines 而保留 fingerprint）。

fingerprint 每帧每 cell 都要算一次，所以它自身必须廉价：只做长度哈希与 enum 判别式
哈希。**不能**在里面序列化 `ToolCall.input` 的 JSON——那等于又渲染一遍 cell，实测会
把 400 回合的帧从 0.09ms 拉回 9ms。

### 2.3 宽度判定要 memo

`text_width(area_width, viewport_height)` 会先探测 `area_width - 1`，再决定是否保留
一列给滚动条。若内容**放得下**，它返回 `area_width`，而 draw 也按 `area_width` 渲染；
下一帧又回到 `area_width - 1` 探测——如此反复，等于每帧把整个 scrollback 重折一次。

因此把判定结果记为 `(area_width, viewport_height, text_width, generation)` 元组：
- `generation` 只随**内容**变化，**不随宽度**变化（这是关键，早期实现把它绑到宽度上，
  导致 memo 每帧被作废）。
- 判定完成后把 cache 留在 `text_width` 这个宽度上，让 draw 直接命中。

### 2.4 只渲染可见窗口，且不 clone

`render` 不再构造"全量 lines 向量"，而是用 `part_offsets` 二分定位可见区间的每个绝对行
号（`locate_line`），只把可见行交给 ratatui：

- 命中 cell 自身行 → 借用（`LineRef::Borrowed`）。
- 命中语义空行 spacer → 借用共享的 `SPACER_LINE` 静态常量。
- 仅当该行被选中（需要加 `REVERSED`）才 clone 一次。

这样 rollout 阶段的每帧 clone 量从 O(总行数) 降到 O(可见行数)。

---

## 3. 渲染输出不变

这是纯性能重构，屏幕上的字节必须与重构前一致。为此：

- `fingerprint` 命中即等价于"输入未变"，而渲染函数是纯函数；
- 测试直接对比"缓存路径"与"从零重渲"的 `Line`（含 style 与 span），要求逐字节相等；
- 另有一组测试覆盖失效路径：流式追加、折叠展开、权限请求展开、宽度变化后重折。

---

## 4. 结果

稳态空闲帧（release，120x40；每帧 cell 重渲染次数与全量重建次数）：

| 会话规模 | 显示行数 | 重构前 | 重构后 |
|---|---|---|---|
| 100 回合 | 800 | ~1.3ms | 0.14ms |
| 400 回合 | 3200 | ~4.7ms | 0.09ms |

重构前"前 100 回合的成本"比"后 400 回合"还高，说明成本几乎与内容无关，只剩 ratatui
自身的绘制。debug 构建下 400 回合从约 190ms 降到约 1.5ms。

首帧仍需一次全量渲染（400 回合约 2.7ms release，1MB 单条消息约 15ms debug），
这是进入长会话时的一次性成本；此后不再随长度增长。

## 5. 未做

- **未丢弃历史**：本次改动不设 cell 上限、不丢最老内容。缓存之后离屏内容几乎不产生
  每帧成本，如仍需省内存，再单独做（YY，等有内存数据再说）。
- **未改 LLM 上下文**：TUI 显示与发给模型的 message 是两套东西；省 token 是
  `yi-agent-core/compact.rs` 的范围，与本次卡顿无关。
