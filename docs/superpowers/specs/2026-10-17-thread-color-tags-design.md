# 设计：Thread 颜色标签

日期：2026-10-17
状态：已评审
关联：`docs/superpowers/specs/2026-10-09-thread-title-llm-design.md`（同为 thread 元数据扩展）；
`docs/superpowers/specs/2026-10-12-desktop-thread-search-design.md`（本设计复用其搜索管线）

## 1. 背景与问题

侧栏会话列表目前只有「项目分组 + 置顶 + 相对时间 + 状态点」这几个维度，用户无法
按自己的语义给会话做视觉标记。想标出「这条是紧急的」「那条是参考资料」只能靠改
标题（污染标题语义）或置顶（置顶只有一个槽位，多了就失去意义）。

macOS 的文件夹颜色标签提供了一个成熟范式：一个对象挂一个颜色，颜色在列表里以
小色点呈现，扫一眼即可分类。本设计把这一范式引入 Thread。

### 现状事实（已核对代码）

1. **前端共享。** Desktop（Tauri）与移动端（iOS WebView）复用同一份 React 代码
   `desktop/src`。TUI 是独立的 Rust 渲染（`yi-agent/src/tui/`），不读也不画侧栏，
   天然不受影响——本功能只落在共享前端 + app-server 协议上。
2. **桌面端会话行目前没有右键菜单。** `onContextMenu` 只挂在**工作区分组头**上
   （`desktop/src/components/ThreadSidebar.tsx:766`）；会话行菜单（重命名 / 置顶 /
   删除，`ThreadSidebar.tsx:510-561`）目前**只由移动端长按**触发
   （`useLongPress`，`desktop/src/lib/useLongPress.ts`）。因此「右键」这条交互在
   桌面端是**新增的接线**，菜单本体则复用既有那份。3. **Thread 元数据可零迁移扩展。** `ThreadMeta`
   （`yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs:27`）持久化在
   `<workdir>/.yi-agent/threads/<thread_id>.meta.json`，整体原子重写；所有后加字段
   一律 `#[serde(default)]`。`pin_seq` 就是先例，并有专门的「旧 meta 缺字段反序列化
   为 None」测试（`thread_store.rs:1388`）。结构体**没有** `deny_unknown_fields`，
   旧数据 / 旧客户端遇到新字段会被忽略，不会报错。
4. **标签定义必须全局，且已有全局落点。** Thread 分散在多个 workspace 目录下
   （`thread/listAll` 按 workspace 分组），而「红色叫什么名字」必须跨目录统一。
   `preferences.json`（`settings_store.rs:38`，读-改-写保留无关键 + 原子替换）即
   `<workdir>/.yi-agent/preferences.json`，在桌面端 `workdir` 是用户 home
   （`desktop/src-tauri/src/bridge.rs:212`），theme / watchman / relay_url 都已住在
   这个文件里，天然是全局宿主级偏好。标签定义放这里，语义与既有偏好一致。
5. **列表层的三条产出路径共用一个渲染函数。** `thread/list`（单目录）、
   `thread/listAll` 的分组项与 pinned 项，全部经 `thread_summary_json`
   （`server.rs:5943`）产出 wire 形状（调用点：`server.rs:3377 / 3434 / 3457 /
   3477`）。新增字段只需改这一个函数。
6. **元数据变更通知已有成熟范式。** `thread/titleChanged`
   （`protocol.rs:263`）是「会话元数据变更 → 列表层恒推 → 所有客户端就地落笔」的
   最近成例，`Delivery::List`（`protocol.rs:446`）。新标签通知照抄这一模式。

## 2. 目标与非目标

### 目标

1. 桌面端**右键**会话行、移动端**长按**会话行，都能在菜单里给该会话选一个颜色
   标签，或选「无」清除。
2. 侧栏会话行**行内**以一个小色点呈现该标签；无标签时不渲染任何东西。
3. 6 个固定颜色，**每个颜色的显示名可由用户在设置里自定义**（全局生效，所有会话
   共用同一套定义）。
4. 标签变更**即时同步到所有已连接客户端**（桌面改，手机立刻看到，反之亦然），
   无需重连。
5. 标签名纳入既有搜索的匹配文本（搜「紧急」即筛出所有红色会话）。

### 非目标

- **不做按标签筛选的 UI**（侧栏顶部筛选条 / 只看某类）。本期标签纯做视觉标记 +
  搜索命中；存储格式（存颜色 id）已为其预留，将来加筛选无需数据迁移。
- **不做「多个标签」**。一个会话至多一个颜色；存储为单值，非数组。
- **不做标签的增删**。6 色固定，只能改名字，不能新增颜色或删除颜色。
- **不做「标记完成」勾选**（macOS 标签的 checkbox 语义，含标题划线 / 变灰等
  衍生表现）。本期不引入，`tag` 结构不需要为其改动。
- **不改 TUI**。TUI 不呈现标签。
- 不改会话的排序规则（标签不参与排序，置顶仍按 `pin_seq` 降序）。
- 不新增「标签」工具给 agent 调用（agent 不能给自己的会话打标签）。

## 3. 设计

### 3.1 色板：固定 6 色

固定顺序即菜单顺序，**id 是权威标识，颜色是渲染映射**：

| id | 默认名 | 色值（Tailwind） |
| --- | --- | --- |
| `red` | 红 | `red-400` |
| `orange` | 橙 | `orange-400` |
| `yellow` | 黄 | `yellow-400` |
| `green` | 绿 | `emerald-400` |
| `blue` | 蓝 | `blue-400` |
| `purple` | 紫 | `violet-400` |

**为什么去掉灰色**：macOS 默认 7 色含灰，但灰色在深色主题（本项目默认
`Theme::Dark`）下与「无标签」的视觉差异过小，反而制造歧义。6 色更清爽，也满足
「至少 5 个颜色」的要求。

**为什么 id 与显示名分离**：名字可改，若把名字当标识，改名就会让所有已打标签的
会话失去意义。id 恒定，名字只是渲染层查表。

色值集中定义在**一个**前端常量模块里（`desktop/src/lib/threadTags.ts`），
行内色点、菜单色点、设置页色点三处共用，避免三份字面量漂移。

### 3.2 数据模型

#### Thread 侧：`ThreadMeta` 新增字段

```rust
/// 会话的颜色标签 id（6 色之一）。`None` = 无标签。旧 meta 缺字段时默认 None。
#[serde(default)]
pub tag: Option<String>,
```

存储为**颜色 id 字符串**而非色值，也不是索引数字：字符串可读、可自解释、
不会被色板重排影响。

#### 全局侧：`preferences.json` 新增键

```json
{
  "theme": "dark",
  "tag_names": { "red": "紧急", "orange": "待办", "blue": "参考" }
}
```

- 语义：**逐键覆盖**。缺失的键回退到内置默认名（红 / 橙 / …）。所以用户改了 2 个
  颜色，文件里就只有这 2 个键，其余 4 个自动跟随默认。
- 与既有 `theme` / `board_watchman_enabled` / `relay_url` 同文件、同读写路径
  （`settings_store` 的读-改-写 + 原子替换），互不干扰。

### 3.3 后端

#### 3.3.1 `ThreadStore` 新增写入方法

```rust
/// 设置会话的颜色标签（None = 清除）。走 update_meta 的读-改-写锁与原子写。
/// 与 set_pin_seq 同：这是用户视图标记，**不刷新 updated_at**
/// （否则打标签会把会话顶到「最近活动」最前面，篡改时间语义）。
pub fn set_tag(&self, id: &str, tag: Option<&str>) -> io::Result<bool>
```

返回 `Ok(false)` 表示 thread 不存在（与 `set_pin_seq` 一致），供 RPC 层翻成
`unknown_thread`。

#### 3.3.2 RPC `thread/setTag`

```
params: { "threadId": "thread-...", "tag": "red" | null }
result: {}
```

三段式，与 `thread/setPinned`（`server.rs:4369`）同构：

1. **参数校验先于存在性**：`tag` 必须是 `null` 或 6 个合法 id 之一；否则
   `-32602 invalid params`。
2. 查 thread 是否存在（沿用 `store_lookup`）：不存在 → `unknown_thread`。
3. 落盘成功 → `ok`，并经 hub 广播 `thread/tagChanged`。

#### 3.3.3 通知 `thread/tagChanged`

```rust
#[serde(rename = "thread/tagChanged")]
TagChanged { thread_id: String, tag: Option<String> },
```

- `thread_key()` → `Some(thread_id)`
- `delivery()` → `Delivery::List`（与 `titleChanged` / `modelChanged` 同族：
  会话元数据变更，列表层恒推，窗口外会话也要送达）
- 参数 snake_case（`thread_id` / `tag`），与同族一致
- **携带权威值**而非「该重拉了」提示：一个短字符串，就地落笔比重拉全量
  `thread/listAll` 便宜。

#### 3.3.4 列表输出

`thread_summary_json`（`server.rs:5943`）新增一行：

```rust
"tag": m.tag,
```

一处修改，`thread/list`、`thread/listAll` 分组、pinned 三条路径同时生效。

#### 3.3.5 标签名的读写（走既有 `ui/settings/*`）

- `ui/settings/read`（`server.rs:2479`）回包增加 `"tag_names"`：磁盘上**用户实际
  留下的覆盖项**（可能只有 1 个键，也可能为空对象）。**不是**合并后的完整映射——
  默认名由前端唯一持有（见 §3.4.1），服务端不重复定义一套中文默认名，避免两处
  默认名必须手工保持同步。
- `ui/settings/write`（`server.rs:2506`）接受可选 `tag_names`。校验：
  - 键必须是 6 个合法 id 之一，否则 `-32602`；
  - 值必须是字符串，`trim` 后非空，长度上限 24 字符（防超长撑破菜单），否则
    `-32602`；
  - 校验通过后把这些键**逐个合并**进已存映射（不删除未提及的键），再原子落盘。
- 名字变更成功后广播一条**独立**通知 `ui/tagNames/updated`：

```rust
/// 标签显示名（颜色 id → 名称）已变。与 `ui/settings/updated`（主题）同为
/// **全局**帧：改名字是对所有会话生效的宿主级偏好，任何线程上下文都无关。
#[serde(rename = "ui/tagNames/updated")]
TagNamesUpdated { tag_names: std::collections::BTreeMap<String, String> },
```

  - `thread_key()` → `None`；`delivery()` → `Delivery::Global`。
  - **为什么不挤进 `UiSettingsUpdated`**：那条通知由主题 watcher 从
    `broadcast::Sender<Theme>` 通道泵出（`server.rs:2127`），语义是「主题变了」；
    而标签名只经 `ui/settings/write` 变更、没有 watcher。硬塞进去会让该通知的值
    时而带 `tag_names`、时而不带（watcher 路径没有它），语义含混。独立通知还
    省掉了改动既有变体及其多个构造点。
  - 携带**权威覆盖映射**（而非「该重读了」提示）：内容很短，就地落笔比重读
    `ui/settings/read` 便宜，与 `thread/titleChanged` 同一取向。

### 3.4 前端

#### 3.4.1 类型与常量

- `protocol.ts`：`ThreadSummary` 增加 `tag?: string | null`；
  `Notification` 联合类型增加 `thread/tagChanged`
  （`{ thread_id: string; tag: string | null }`）与 `ui/tagNames/updated`
  （`{ tag_names: Record<string, string> }`）。
- 新模块 `desktop/src/lib/threadTags.ts`：纯函数 + 常量，无 React、无 RPC：
  - `TAG_IDS`（6 个 id 的固定顺序数组）
  - `DEFAULT_TAG_NAMES`（id → 默认中文名）；`TAG_DOT_CLASS`（id → 色点 class）
  - `isValidTagId(v)`：类型守卫，用于渲染前过滤脏数据
  - `resolveTagNames(overrides)`：把服务端给的**部分覆盖**合成完整 6 键映射，
    缺失键回退 `DEFAULT_TAG_NAMES`。这是**默认名的唯一权威定义处**——Rust 侧
    不做同样的事（否则两套中文默认名必须手工保持一致）。
  - `tagLabel(id, names)`：查表取名，未知 id / 缺失时回退默认名

#### 3.4.2 行内色点

`ThreadSidebar` 的 `renderThread`：在 `StatusBadge` 之后、标题之前插入

```tsx
{isValidTagId(t.tag) ? (
  <span
    data-thread-tag={t.tag}
    aria-label={tagLabel(t.tag, tagNames)}
    title={tagLabel(t.tag, tagNames)}
    className={`size-2 shrink-0 rounded-full ${TAG_DOT_CLASS[t.tag]}`}
  />
) : null}
```

- 规格与既有未读点一致（`size-2 rounded-full`），视觉上同一族。
- **合法性防御**：`t.tag` 不是合法 id 时（手工改坏 meta / 旧字段）不渲染，而不是
  渲染成透明 / 错色。`isValidTagId` 同时充当类型守卫，故 `TAG_DOT_CLASS[t.tag]`
  的索引是类型安全的。
- 无标签 → 渲染 `null`，**布局逐像素不变**（不占位）。

#### 3.4.3 打开菜单：桌面右键（新增）+ 移动长按（沿用）

- 会话行新增 `onContextMenu`：`e.preventDefault()` 后 `setMenu({ kind: "thread",
  id })`，与分组头的既有写法一致（`ThreadSidebar.tsx:766`），并同样
  `setNewMenuOpen(false)` 保证三种菜单互斥。
- 移动端**不改**：`useLongPress` 的职责只是「解析落点 → 打开 `kind: "thread"`
  菜单」，菜单内容变化对它透明。
- 键盘可达性：与分组头同法，行上支持 Enter/Space 打开本行菜单（
  `aria-haspopup="menu"` + `aria-expanded`）。

#### 3.4.4 菜单结构

在既有三项（重命名 / 置顶 / 删除）**之上**增加一个标签区，中间以分隔线隔开：

```
┌──────────────────────┐
│ ● 红  ● 橙  ● 黄 ... │  ← 6 个色点，当前项带描边高亮
│        无标签         │  ← 清除
├──────────────────────┤
│ 重命名               │
│ 置顶 / 取消置顶       │
│ 删除                 │
└──────────────────────┘
```

- 每个色点的 `aria-label` 用**当前自定义名**（不是「红」，而是用户改过的「紧急」），
  这样改过名后读屏与悬停提示自动跟上。
- 当前选中项：色点加描边环（`ring-2`）与 `aria-checked="true"`，色点本身保持原色
  （不用 ✓ 覆盖颜色，那会让 6 个色点在选中态失去颜色区分）。
- 点选后关闭菜单（复用 `closeMenus`）并发 `thread/setTag`。
- 移动端菜单更窄：6 个色点一行 `flex` 横向排布，`gap-2`，不换行。

#### 3.4.5 状态更新路径（App 层）

`onSetTag(id, tag)`，照 `onTogglePin`（`App.tsx:1257`）的成例：

1. `clientRef.current.request("thread/setTag", { threadId: id, tag })`
2. 成功 → `refreshThreads()` + `force(...)`
3. 失败 → `setCurrentError(formatError(e))` + 同样重拉（**不做乐观更新**，
   UI 不能停在「看起来打上了」的状态）

`thread/tagChanged` 通知的处理，仿 `thread/titleChanged`（`App.tsx:1412`）：
就地把 `groups` 与 `pinned` 里该 thread 的 `tag` 换掉。本地发起的操作走 RPC 回包
+ 重拉，远端发起的走通知，两条路径不重复也不冲突。

`store`（`threadStore.ts`）**不**需要动：标签不是「会话运行状态」，与 `status` /
`unread` / `mode` 那类逐会话视图状态不同，它只影响列表行渲染，权威值随
`thread/list(All)` 的 `ThreadSummary` 一起来。这避免了在两个数据源之间同步。

#### 3.4.6 设置页：「标签」Tab

`SettingsDialog` 的 `TABS` 表增加一项 `{ id: "tags", label: "标签" }`，新组件
`SettingsTagsTab.tsx`：

- 6 行，每行 = 色点 + 文本输入框（值为当前名，`placeholder` 为默认名）。
- 提交时机：失焦（`onBlur`）与回车（Enter，需过 IME 守卫，复用
  `useImeGuard`——中文输入法确认 Enter 不能提交）。
- 提交走 `ui/settings/write { tag_names: {...} }`；成功后用回包/通知刷新本地。
- 校验失败（空串）时内联提示且不落盘，回填旧值。
- 该 Tab 复用对话框既有的 Tab 键盘导航（roving tabIndex）机制，无需特殊处理。

#### 3.4.7 搜索

`threadSearch.ts` 的 `haystack`（`threadSearch.ts:18`）把标签名并入匹配文本：

```ts
const name = isValidTagId(thread.tag) ? names[thread.tag] : null;
return [thread.title ?? "(untitled)", name ?? "", basename(ws), ws].join("\n").toLowerCase();
```

因此搜「紧急」= 筛出所有红色会话。这是「不做筛选 UI」方案下几乎零成本的收益。
`filterThreadTree` 需新增一个「标签名表」入参（当前签名只有 groups / pinned /
query），由 `ThreadSidebar` 从 App 传入。

#### 3.4.8 `tagNames` 状态的来源与更新

标签名表在 App 层持有（`const [tagNames, setTagNames] = useState(DEFAULT_TAG_NAMES)`），
三条更新路径：

1. **首屏 / 每次重连握手**：`ui/settings/read` 已在握手序列中调用
   （`App.tsx:1486`），把 `tag_names` 一并读出，经 `resolveTagNames` 合成完整
   映射后 `setTagNames`。字段缺失或形状不对（旧服务端）→ 全默认名。
2. **本端改名字**：设置页提交成功后，本地立即用 `resolveTagNames(提交后的合并
   映射)` 更新；服务端回归的广播是权威的第二道确认。
3. **远端改名字**：收到 `ui/tagNames/updated` → `resolveTagNames(n.params.tag_names)`
   → `setTagNames`。

**不需要 `touched` 守卫**（对比 theme / watchman 的 `themeTouchedRef` 处理）：那两个
偏好是「用户可能正在拨开关」的控件，迟到的 read 会覆盖正在进行的操作；而标签名只在
模态设置页里编辑，且改名的写路径（`ui/settings/write`）本身权威、并会广播新值，
迟到的 read 与它竞争的最坏结果是短暂显示旧名，下一次通知即纠正。故此处从简。

**不需要 `touched` 守卫**（对比 theme / watchman 的 `themeTouchedRef` 处理）：那两个
偏好是「用户可能正在拨开关」的控件，迟到的 read 会覆盖正在进行的操作；而标签名只在
模态设置页里编辑，且改名的写路径（`ui/settings/write`）本身权威、并会广播新值，
迟到的 read 与它竞争的最坏结果是短暂显示旧名，下一次通知即纠正。故此处从简。

渲染侧：`ThreadSidebar` 与 `SettingsTagsTab` 都接 `tagNames` 作为 prop；
`ThreadSidebar` 再把它传给 `filterThreadTree`。

### 3.5 数据流

```
用户在菜单点「红」
  → App.onSetTag(id, "red")
     → RPC thread/setTag
        → server: 校验 id ∈ 6 色 → store_lookup → ThreadStore::set_tag
           → update_meta 读-改-写锁 + 原子写 <id>.meta.json
        → 广播 thread/tagChanged（Delivery::List，所有客户端）
     → 本端：refreshThreads() + force（服务端为权威）
  → 远端（手机）：收到 thread/tagChanged → 就地改本地 tag → 行内色点即时变化

用户改标签名「红」→「紧急」
  → 设置页 ui/settings/write { tag_names: { red: "紧急" } }
     → settings_store 读-改-写 preferences.json（逐键合并，保留既有覆盖）
     → 广播 ui/tagNames/updated（Delivery::Global，带权威覆盖映射）
  → 两端：resolveTagNames 合成完整映射 → 菜单 aria-label、行内色点 title、
     搜索文本全部即时跟上
  → 已打 red 的会话**不受影响**（存的是 id，不是名字）
```

### 3.6 兼容与边界

| 情形 | 行为 |
| --- | --- |
| 旧 meta 无 `tag` 字段 | `#[serde(default)]` → `None`，无标签。零迁移 |
| 旧服务端（无 tag 字段） | 前端 `ThreadSummary.tag` 为 `undefined` → 按无标签渲染 |
| 旧客户端收到 `tag` 字段 | 忽略（结构体无 `deny_unknown_fields`），不影响 |
| meta 里 `tag` 是未知 id | 不渲染色点；搜索不并入；不报错、不崩 |
| `preferences.json` 里 `tag_names` 是坏 JSON | `load_tag_names` `tracing::warn!` 后返回空映射 → 前端全用默认名（与同文件 `load()` 的日志契约一致，便于诊断「标签名总是复位」） |
| `tag_names` 某键值超长 / 空串 | `ui/settings/write` 拒绝，`-32602`，磁盘不变 |
| 会话被删除 | 其 tag 随 meta 文件一起消失，无残留 |
| 同一会话两端同时改 | 后到的写赢（读-改-写锁串行），两端最终由各自重拉 / 通知收敛到同一值 |
| 打标签是否会顶到列表前面 | 否。`set_tag` 不刷新 `updated_at`，时间语义不被篡改 |
| 旧客户端收到 `ui/tagNames/updated` | 未知通知方法，忽略（前端 `if (n.method === ...)` 逐个分支，无 else 兜底报错） |

## 4. 测试

### Rust（`cargo test -p yi-agent-app-server`）

`thread_store.rs`：

1. `set_tag_sets_value_and_does_not_bump_updated_at`——打标签后 `tag == Some("red")`
   且 `updated_at` 不变（照 `set_pin_seq_sets_value_without_bumping_updated_at` 写）。
2. `set_tag_none_clears_it`。
3. `set_tag_unknown_id_returns_false`。
4. `legacy_meta_without_tag_deserializes_to_none`——旧格式 JSON（无 `tag` 键）
   读出来是 `None`。

`settings_store.rs`：

5. `load_tag_names_returns_empty_map_when_absent`——无 `tag_names` 键 → 空映射
   （不是错误、不是 panic）。
6. `load_tag_names_returns_only_stored_overrides`——只写 1 个键 → 读回只有 1 个键
   （**不**在后端补齐 6 个默认名）。
7. `save_tag_names_merges_and_preserves_other_keys`——先存 `theme` + `relay_url`，
   再 `save_tag_names({red})`，断言三者都在；再 `save_tag_names({blue})`，断言
   `red` 与 `blue` 都在（合并而非替换）。
7a. （`settings_store::warning_tests`）`a_missing_tag_names_file_is_not_logged` /
   `an_unreadable_tag_names_file_is_logged_and_falls_back_to_empty` /
   `a_corrupt_tag_names_file_is_logged_and_falls_back_to_empty`——读侧日志契约与
   `load()` 对齐：缺文件静默，读失败 / 解析失败各打一条可区分的 `tracing::warn!`。

`server.rs`（RPC 层，照 `set_pinned_*` 三个测试写）：

8. `set_tag_persists_and_reports_in_list`——设 `red` 后 `thread/listAll` 里该条
   `tag == "red"`。
9. `set_tag_null_clears`。
10. `set_tag_rejects_unknown_color_and_unknown_thread`——`tag: "chartreuse"` →
    `-32602`；未知 threadId → `unknown_thread`。
11. `set_tag_broadcasts_tag_changed`——断言收到 `thread/tagChanged` 且参数正确
    （wire 形状：snake_case）。
12. `ui_settings_read_returns_tag_names_overrides`——磁盘只覆盖 1 个键时，回包
    `tag_names` 恰有那 1 个键（默认名不来自后端）。
13. `ui_settings_write_rejects_invalid_tag_name_key_and_empty_value`。
14. `ui_settings_write_broadcasts_tag_names_updated`——写名字后收到
    `ui/tagNames/updated`，带权威映射（wire 形状：`ui/tagNames/updated` +
    `params.tag_names`）。

`protocol.rs`：

15. `tag_changed_notification_carries_method_and_tag`——wire 形状断言
    （`thread/tagChanged`、`params.thread_id`、`params.tag`；`tag: null` 时
    `params.tag == null`），且 `delivery() == List`、`thread_key() == Some(id)`。
16. `tag_names_updated_notification_is_global`——`ui/tagNames/updated`、
    `params.tag_names`、`delivery() == Global`、`thread_key() == None`。

### TypeScript（`npm test` / vitest，在 `desktop/`）

17. `threadTags.test.ts`：`TAG_IDS` 恰 6 个且顺序固定；`DEFAULT_TAG_NAMES` 键集与
    `TAG_IDS` 一致、每个值非空；`TAG_DOT_CLASS` 键集同上；`isValidTagId` 接受 6 个
    id、拒绝未知串 / `null` / `undefined` / 数字 / 对象；`resolveTagNames({})` 等于
    全默认；`resolveTagNames({ red: "紧急" })` 其余 5 个仍是默认名（不因部分覆盖而
    丢键）；`tagLabel` 未知 id 回退默认名而不抛。
18. `ThreadSidebar.test.tsx`：有 tag → 渲染色点（`[data-thread-tag="red"]`）且
    `aria-label` 是当前名；无 tag → 无该节点（并断言标题节点不受影响）；非法 tag
    （`tag: "chartreuse"`）→ 无该节点。
19. `ThreadSidebar.test.tsx`：桌面右键会话行 → 打开菜单（`[data-thread-menu]`）；
    点「红」→ `onSetTag(id, "red")`；点「无标签」→ `onSetTag(id, null)`；当前 tag
    对应的色点 `aria-checked="true"`，其余为 `false`。
20. `ThreadSidebar.test.tsx`：移动端长按 → 同一菜单（回归：确认复用而非新开）。
21. `threadSearch.test.ts`：搜标签名命中对应会话；改过名后按新名命中（旧名不再
    命中）；非法 tag 不影响搜索（不崩、不误命中）。
22. `SettingsTagsTab.test.tsx`：改名字失焦 → 发 `ui/settings/write` 且
    `params.tag_names` 只含改动键；空串 → 不提交且内联报错；IME 组合中的 Enter
    不提交。
23. `App.test.tsx`：收到 `thread/tagChanged` → 对应行的色点更新（通知路径）；
    收到 `ui/tagNames/updated` → 行内色点的 `title` 变成新名。

### 手工验证

24. 桌面右键 → 打「红」→ 行内立刻出红点；改成「蓝」→ 红点变蓝点；「无标签」→
    色点消失。重启 app → 标签仍在（落盘生效）。
25. 手机连同一 app-server：桌面改标签，手机侧栏**无需重连**即变色；反向亦然。
26. 设置里把「红」改成「紧急」：两端菜单提示与搜索（搜「紧急」）立即跟上；
    已打红色的会话颜色不变。
27. 打标签**不会**把会话顶到「最近更新」的最前面（`updated_at` 未变）。

## 5. 影响面

| 层 | 文件 | 改动 |
| --- | --- | --- |
| 存储 | `thread_store.rs` | `ThreadMeta.tag`；`set_tag`；旧 meta 容错测试 |
| 存储 | `settings_store.rs` | `load_tag_names` / `save_tag_names`（逐键合并，保留无关键） |
| 协议 | `protocol.rs` | `Notification::TagChanged`（List）+ `Notification::TagNamesUpdated`（Global） |
| 服务 | `server.rs` | `thread/setTag` 分支；`ui/settings/read\|write` 扩展；`thread_summary_json` 加 `tag`；`ui/tagNames/updated` 广播点 |
| 前端类型 | `lib/protocol.ts` | `ThreadSummary.tag`；两条新通知的联合类型 |
| 前端常量 | `lib/threadTags.ts`（新） | 6 色 id / 默认名 / 色值 / `resolveTagNames` / `isValidTagId` / `tagLabel` |
| 前端 UI | `components/ThreadSidebar.tsx` | 行内色点；`onContextMenu`；菜单标签区；传 `tagNames` |
| 前端 UI | `components/SettingsTagsTab.tsx`（新）、`SettingsDialog.tsx` | 新 Tab |
| 前端逻辑 | `lib/threadSearch.ts` | `haystack` 并入标签名；签名加 `tagNames` 入参 |
| 前端逻辑 | `App.tsx` | `onSetTag`；两条通知的处理；`tagNames` 状态与握手加载 |
| 文档 | `docs/project-management/desktop.md`、`yi-agent-app-server.md` | 按项目规范同步进度 |

## 6. 未决 / 留待后续

- **按标签筛选 UI**（侧栏筛选条）——本期明确不做。存储层（存 id）已为其预留。
- **多于一个标签**——`tag` 改为 `tags: Vec<String>` 即可扩展，本期不做。
- **标签的颜色自定义**（不只改名，还改色值）——本期固定 6 色，不做。
- **「已完成」勾选语义**——本期不做。
