# 会话置顶（Session Pin）

日期：2026-10-02
状态：待实施

## 1. 背景与目标

macOS 桌面端侧栏按工作目录分组列出会话，组内按 `updated_at` 降序排列。常用会话
会被新会话不断挤下去，用户每次都要滚动去找。

**目标**：让用户能把常用会话「置顶」，置顶会话集中展示在侧栏顶部的独立分区，
跨工作目录可见，且可按用户意愿手动排序。

**成功标准**：

1. 任意会话可一键置顶 / 取消置顶。
2. 置顶会话出现在侧栏顶部一个固定的「Pinned」分区，不再出现在其原工作目录分组。
3. 置顶分区内的顺序由用户拖拽决定，重开 App 后保持不变。
4. 置顶状态由服务端持久化（与 `permission_mode` 同级），不依赖前端本地存储。

## 2. 已定决策

| # | 决策 |
|---|---|
| 1 | 置顶会话集中到侧栏**顶部独立分区**，并从其原工作目录分组中移走（不在两处重复）。 |
| 2 | 置顶状态**持久化在服务端** `ThreadMeta`（`.meta.json`），新增 RPC 读写。 |
| 3 | 触发方式：会话行悬停时显示**图钉按钮**，点击切换置顶 / 取消置顶。不做右键菜单入口。 |
| 4 | 置顶分区支持**手动拖拽排序**，顺序持久化。 |
| 5 | 新置顶的会话置于分区**最顶部**；取消后重新置顶也回到最顶部（不记忆旧位置）。 |
| 6 | 拖拽**仅限置顶分区内部**：不能把非置顶会话拖入分区，也不能把置顶会话拖出分区来取消置顶。取消置顶只通过图钉按钮。 |
| 7 | 拖拽排序**手写实现**（复用侧栏调宽的手写事件风格），**不新增第三方依赖**。 |
| 8 | 无置顶会话时，整个 Pinned 分区**不渲染**。 |

## 3. 范围

**本 spec 做**：

- `ThreadMeta` 新增 `pin_seq` 字段（含旧数据兼容）。
- 服务端 RPC：`thread/setPinned`、`thread/reorderPinned`。
- `thread/list` / `thread/listAll` 的 `ThreadSummary` 带出 `pinned`。
- `thread/listAll` 额外返回排好序的顶层 `pinned` 数组。
- 桌面端：`ThreadSummary.pinned` 类型、`App` 侧栏分组拆分、`ThreadSidebar` 顶部分区、
  图钉按钮、分区内拖拽排序。

**本 spec 不做**：

- 右键菜单形式的置顶入口。
- TUI 侧的置顶 UI（服务端已具备能力，TUI 后续接入）。
- 跨设备同步（由服务端持久化天然覆盖，但不额外做冲突合并）。
- 「置顶分区」折叠状态的持久化（沿用现有分组折叠的内存态）。
- 多选 / 批量置顶。

## 4. 数据模型与服务端

### 4.1 存储字段

`yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs` 的 `ThreadMeta` 新增：

```rust
/// 置顶顺序键：`Some` 表示已置顶，`None`（含旧 meta 缺字段）表示未置顶。
/// 数值越大越靠前（顶部）。
#[serde(default)]
pub pin_seq: Option<i64>,
```

**为什么只有一个字段**：`Option<i64>` 本身既表达「是否置顶」（`is_some`）又表达
「分区内位置」。若再加一个独立的 `pinned: bool`，两者必须永远保持一致
（取消置顶时同时清 `pin_seq`），多一个可能漂移的不变量。单个字段消除该风险。

**旧数据兼容**：`#[serde(default)]` → 缺字段的旧 `.meta.json` 反序列化为 `None`，
即「未置顶」，无需迁移。

**排序契约**：置顶分区按 `pin_seq` **降序**排列——数值越大越靠前。这样新置顶项
只要取当前时间戳（见 4.3），天然落在最顶部。若出现相同 `pin_seq`（不同工作目录
各自写下同一毫秒的时间戳），按 `updated_at` 降序、`thread_id` 升序打破平局，
保证顺序确定。

### 4.2 排序算法与 `ThreadStore` 新增方法

会话按工作目录**分目录存储**，而置顶会话是**跨目录**的，因此「重排」不是任何单个
`ThreadStore` 的方法。拆成两层：

1. 一个**纯函数**（放 `thread_store.rs` 顶层，`pub(crate)`，可独立单测）负责算出
   每个 id 的目标 `pin_seq`；不碰文件系统。
2. `ThreadStore` 只保留一个按 id 写入的方法，调用方按 id 所在目录分别调用。

```rust
/// 计算重排后的 `pin_seq` 赋值。`order` 为**从顶到底**的完整有序 id 列表，
/// `current` 为这些 id 当前的 `pin_seq`。返回 (id, 新 seq) 列表。
///
/// 算法：取 `current` 中现有 `pin_seq` 的**互异**值升序得 `seqs`；将 `order`
/// 从顶到底依次赋值为 `seqs` 的从大到小（`order[0]` 拿最大值）。复用既有互异
/// 数值做双射，**永不产生新的重复值**，且与 §4.1 的降序契约一致。
/// `current` 里为 `None` 的项（异常态）补 `now_millis() + 递增偏移` 后纳入 `seqs`。
pub(crate) fn assign_pin_seqs(
    order: &[String],
    current: &HashMap<String, Option<i64>>,
) -> Vec<(String, i64)>;
```

`ThreadStore` 新增（沿用既有 `update_meta`，持 `meta_lock` 做读-改-写）：

```rust
/// 写入置顶顺序键：`Some(seq)` = 置顶，`None` = 取消置顶。
///
/// 有意**不**刷新 `updated_at`：置顶是设置变更而非会话活动，不应影响按
/// `updated_at` 排序的普通分组顺序（与 `set_permission_mode` 同理）。
/// 返回 false 表示 thread 不存在或 meta 不可读。
pub fn set_pin_seq(&self, id: &str, seq: Option<i64>) -> io::Result<bool>;
```

重排的落盘由 RPC 处理函数完成：对 `assign_pin_seqs` 的每个 `(id, seq)`，用
`store_for(workspaces, cfg, id)` 定位其 store 后调用 `set_pin_seq`。

- 写入条目数取决于位移跨度，最坏 `O(n)`（把最后一项拖到最前，会整体下移）；
  相邻两项交换只改 2 条。置顶集合通常很小，写放大可接受。
- 若 `S` 内存在 `pin_seq = None` 的项（异常态：调用方声称已置顶但存储无值），
  为该 id 的当前值补 `now_millis() + 递增偏移` 后再纳入 `seqs`。

### 4.3 RPC 接口

对齐既有 `thread/setPermissionMode` 的写法：用 `store_lookup` 定位 store，
错误码用 `RpcError::unknown_thread` / `RpcError::invalid_params` / `RpcError::internal`。

新增共享 helper（`server.rs`）：`collect_pinned(workspaces) -> Vec<(String /*id*/, i64 /*seq*/, i64 /*updated_at*/)>`，
遍历 `workspaces.list()` 下每个目录的 `ThreadStore`，收集 `pin_seq.is_some()` 的
thread。`thread/listAll` 的顶层 `pinned` 与该 helper、`thread/reorderPinned` 的校验共用它。

**`thread/setPinned`**

- 参数：`{ "threadId": string, "pinned": boolean }`。
- `pinned = true`：`store.set_pin_seq(id, Some(now_millis()))`。
- `pinned = false`：`store.set_pin_seq(id, None)`。
- 成功返回 `{}`；`threadId` 未知（store 返回 false）返回 `unknown_thread`；
  `pinned` 缺失或非布尔返回 `invalid_params`。
- 因 §4.1 的降序契约，`now_millis()` 是当前最大值 → 新置顶项**天生位于最顶部**，
  无需任何后置重排。

**`thread/reorderPinned`**

- 参数：`{ "threadIds": string[] }`，**从顶到底**的完整有序置顶 id 列表。
- 校验（用 `collect_pinned`）：`threadIds` 中每个 id 均属当前已置顶集合，
  且**无重复**、**集合与当前全部置顶会话完全一致**（不缺、不多）。
  任一不满足返回 `invalid_params`，不做任何写入。
- 通过后按 §4.2 算法重写 `pin_seq`，跨工作目录逐条用 `store_for` 写盘，返回 `{}`。
- 并发说明：若在客户端取列表与提交重排之间，另一客户端改变了置顶集合，
  校验失败 → `invalid_params`；客户端应重拉列表（见 §6）。

**`thread/list` / `thread/listAll` 的 `ThreadSummary`**

新增字段 `"pinned": bool`（值 = `pin_seq.is_some()`），在现有显式 `json!` 映射处补上。

**`thread/listAll` 顶层新增 `pinned` 数组**

响应由 `{ "groups": WorkspaceGroup[] }` 变为
`{ "groups": WorkspaceGroup[], "pinned": ThreadSummary[] }`。

- `pinned`：全部置顶会话，按 §4.1 排序契约排序（从顶到底）。
- `groups`：**保持现状**——置顶会话仍留在其工作目录分组内（各项 `pinned: true`）。
  是否把置顶项从分组里隐藏是**客户端的渲染决策**，服务端不做移除。

**为什么服务端不把置顶项从分组里摘掉**：app-server 同时服务桌面端、TUI 与
CLI 客户端。若在服务端摘除，任何尚不认识置顶概念的客户端（旧桌面端版本、
TUI）会直接丢失这些会话。保留在分组内并打标记，则旧客户端行为完全不变，
新桌面端自行过滤展示，属于向后兼容的渐进增强。

## 5. 桌面端

### 5.1 类型（`desktop/src/lib/protocol.ts`）

`ThreadSummary` 新增：

```ts
/** 是否置于侧栏顶部的 Pinned 分区。旧服务端缺省视为 false。 */
pinned?: boolean;
```

### 5.2 数据流（`desktop/src/App.tsx`）

- 新增 state：`const [pinned, setPinned] = useState<ThreadSummary[]>([])`。
- `refreshThreads` 与首屏初始化读取 `{ groups, pinned }`：`setGroups(groups)`、
  `setPinned(pinned ?? [])`，`store.seed` 仍喂入 `groups.flatMap(g => g.threads)`
  （置顶项仍在分组内，seed 覆盖全集，不漏）。
- 首屏自动选中的会话：优先 `pinned[0]`，否则 `groups.flatMap(g=>g.threads)[0]`。
- `modeForThread` 目前只扫 `groups`；改为扫 `[...groups.flatMap(g=>g.threads), ...pinned]`，
  否则置顶会话的权限模式回读会落空。

新增两个回调：

```ts
/** 置顶 / 取消置顶；成功后重拉列表，以服务端顺序为权威。 */
const onTogglePin = async (id: string, next: boolean) => {
  const c = clientRef.current;
  if (!c) return;
  try {
    await c.request("thread/setPinned", { threadId: id, pinned: next });
    // 新置顶项由服务端排序契约天然落在最顶，无需额外重排。
    await refreshThreads();
  } catch (e) {
    setCurrentError(formatError(e));
    await refreshThreads(); // 与服务端对齐
  }
};

/** 置顶分区内拖拽排序落盘；乐观更新 + 失败回滚重拉。 */
const onReorderPinned = async (orderedIds: string[]) => {
  const c = clientRef.current;
  if (!c) return;
  const prev = pinned;
  // 乐观更新：立刻按新顺序重排本地 pinned 数组。
  const byId = new Map(prev.map((t) => [t.thread_id, t]));
  setPinned(orderedIds.map((id) => byId.get(id)).filter((t): t is ThreadSummary => !!t));
  try {
    await c.request("thread/reorderPinned", { threadIds: orderedIds });
  } catch (e) {
    setCurrentError(formatError(e));
    setPinned(prev); // 回滚
    await refreshThreads(); // 与服务端对齐
  }
};
```

`ThreadSidebar` 新增 props：`pinned: ThreadSummary[]`、`onTogglePin`、`onReorderPinned`。

### 5.3 侧栏（`desktop/src/components/ThreadSidebar.tsx`）

**Pinned 分区**：渲染在滚动容器内最顶部、所有工作目录分组之上。

- `pinned.length === 0` 时整段不渲染（决策 8）。
- 标题行「Pinned」沿用现有分组头的视觉：可折叠箭头（折叠态仅内存，不持久化），
  但**不带**右键菜单（分组菜单里的「New thread here / Remove」对置顶分区无意义）。
- 分区内按传入的 `pinned` 数组顺序渲染（服务端已排序，前端不再排）。

**从分组中隐藏置顶项**：渲染每个工作目录分组时，对其 `g.threads` 做
`filter(t => !t.pinned)`。分组头始终照常渲染（与现有「目录为空仍显示组头」一致），
因此某目录的会话全部置顶时，该组仅显示组头，无需特判。

**图钉按钮**：在会话行 `renderThread` 中，`×` 删除按钮左侧新增图钉按钮。

- 未置顶：悬停行时才可见（与 `×` 的 `group-hover` 显隐一致），点击 → `onTogglePin(id, true)`。
- 已置顶：**常亮可见**并高亮（如琥珀色实心），点击 → `onTogglePin(id, false)`。
- 用 `<button type="button">` + `aria-pressed` + `aria-label`（"Pin thread" / "Unpin thread"），
  并 `e.stopPropagation()` 防止触发行选中。
- 编辑标题（`editingId === t.thread_id`）期间不显示图钉按钮。

**分区内拖拽排序**：

- 置顶分区内的会话行加 `draggable`（编辑标题时禁用）。
- 手写 HTML5 拖拽事件（`onDragStart` / `onDragOver` / `onDrop` / `onDragEnd`），
  不引入 dnd-kit 等依赖。
- 组件内 state 记录 `draggingId` 与 `dropIndex`；`onDragOver` 时计算插入位置，
  渲染插入指示线；`onDrop` 时把 `draggingId` 从数组中取出、插入到 `dropIndex`，
  生成新 `orderedIds` 调用 `onReorderPinned`。
- 拖拽只在置顶分区内生效：非置顶会话行不设 `draggable`，其 `onDragOver` 不处理。
- 普通点击选中行为不受影响（HTML5 拖拽在无实质位移时仍派发 click；沿用现有
  `e.detail > 1` 双击保护，不冲突）。

## 6. 错误处理与边界

1. **`threadId` 未知**（会话被删除）：`setPinned` 返回 `unknown_thread`，
   前端提示并重拉列表。
2. **拖拽落盘失败**：乐观更新回滚到拖拽前顺序，并重拉列表与服务端对齐。
3. **并发重排冲突**（§4.3）：`invalid_params` → 前端重拉列表。
4. **旧数据**：缺 `pin_seq` 的 meta 视为未置顶，无迁移。
5. **旧客户端**（不认识 `pinned`）：`listAll` 的 `groups` 行为不变，置顶会话照常显示，
   无回归；额外字段被忽略。
6. **删除会话**：`thread/delete` 删除整个 meta 文件，`pin_seq` 一并消失，无需额外清理。
7. **`pin_seq` 平局**：由 `(updated_at desc, thread_id asc)` 打破，顺序确定；
   下一次重排即消除平局。

## 7. 测试计划

### 7.1 服务端（Rust）

`thread_store.rs` 单测：

- 新建 meta 默认 `pin_seq == None`。
- `set_pin_seq(id, Some(x))` 后 `pin_seq == Some(x)`，且 **`updated_at` 不变**。
- `set_pin_seq(id, None)` 后 `pin_seq == None`。
- 反序列化旧 meta（无 `pin_seq` 字段）→ `None`，不报错。
- `assign_pin_seqs` 复用互异序列：给定 `order` 与各 id 的当前 `pin_seq`，
  输出序列严格互异且按降序还原出 `order`。
- `assign_pin_seqs` 对 `pin_seq = None` 的项补号后仍互异、顺序正确。
- `assign_pin_seqs` 幂等：以当前顺序为 `order` 再算一次，输出的 seq 序列与输入一致。
- 未知 id 的 `set_pin_seq` 返回 `false`。

`server.rs` 集成测（沿用现有 in-process JSON-RPC 测试脚手架）：

- `thread/setPinned(true)` 后，`thread/listAll` 的顶层 `pinned` 含该 thread 且排在最前，
  同时该 thread 仍在其 workspace group 内并带 `pinned: true`。
- `thread/setPinned(false)` 后，顶层 `pinned` 不再含它，其 group 内条目 `pinned: false`。
- 连续置顶两个 thread：后置顶者在顶层 `pinned` 的第一位。
- `thread/setPinned` 参数非法（`pinned` 非布尔）→ `invalid_params`；未知 id → `unknown_thread`。
- `thread/reorderPinned` 后顶层 `pinned` 顺序等于请求顺序。
- `thread/reorderPinned` 传入未置顶 id、缺项、重复 id → `invalid_params` 且不写入。
- 冷 thread（未 resume）的 `setPinned` 也能按全局索引定位并落盘。

### 7.2 桌面端（Vitest / Testing Library）

`ThreadSidebar.test.tsx`：

- `pinned` 非空时渲染「Pinned」分区，且分区内顺序等于传入数组顺序。
- `pinned` 为空时**不渲染**「Pinned」分区。
- 置顶会话从原工作目录分组中消失（分组头仍在）。
- 未置顶行的图钉按钮点击 → `onTogglePin(id, true)`；已置顶行点击 → `onTogglePin(id, false)`。
- 已置顶行的图钉按钮常亮（`aria-pressed="true"`）。
- 图钉按钮点击不触发 `onSelect`（`stopPropagation` 生效）。
- 置顶分区内拖拽（`dragstart` → `dragover` 到目标行 → `drop`）→ 以新顺序调用 `onReorderPinned`。
- 非置顶行不可拖拽。

`App.test.tsx`（沿用现有 fake client）：

- 首屏 `listAll` 返回 `pinned` → 传给侧栏并渲染。
- 点击图钉 → 发出 `thread/setPinned`，随后重拉 `thread/listAll`。
- `thread/reorderPinned` 失败 → `pinned` 顺序回滚到拖拽前。

## 8. 约束

- 不新增 npm 依赖，不新增 Rust 依赖。
- 服务端 meta 写入沿用 `update_meta` / `write_atomic`，保持原子写与 `meta_lock` 串行化。
- 提交前在 `yi-agent-rs/` 下跑 `cargo fmt --all`。
- 所有改动走 worktree 分支，不直接改 `main`；commit 用 conventional commits，
  不写 `Co-Authored-By` 行。
