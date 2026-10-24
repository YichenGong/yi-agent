# 会话内搜索（Cmd+F）设计

日期：2026-10-24
状态：设计已由用户确认，待实现

> **后续变更（2026-10-25）**：本设计初稿把搜索放在 `session_search` flag 后面，
> 现已按用户要求去掉该 flag，功能转为**常开**：`yi-agent-flags` 注册表不再有这一
> 项，前端无条件注册 Cmd+F 并 `preventDefault()`。下文提到 flag 门控的段落仅作
> 设计沿革保留，实际行为以本节为准。


## 1. 要解决的问题

桌面端一条长会话可能有上百条消息。用户想回看之前某个结论（"上次那个报错是
什么原因"）时，只能靠肉眼上下来回翻。目前转录区没有任何查找能力：
`ChatView` 只负责渲染与自动贴底，`App` 里没有 document 级 keydown 监听，也没有
Tauri menu accelerator。Cmd+F 在 webview 里没有任何反应。

目标：在**当前会话内**按 Cmd+F 就地查找文本，行为对齐浏览器查找栏的肌肉记忆。

## 2. 范围

### 搜什么

只搜**用户消息**与 **agent 回复正文**的可见文本，包括：

- user bubble 的 `item.text`
- agentMessage 渲染出的 markdown 文本：正文、列表、表格单元格、代码块内文本、
  链接文字
- agent 消息里的附件/图片名、notice 胶囊、工具卡、思考块：**不搜**

不搜的部分有一个共同理由：它们要么不是"用户写出来给模型看的话"，要么默认折叠、
命中后需要额外的自动展开语义。本设计的搜索永远**不会展开任何折叠块**，
保证"搜到什么就看得见什么"。

### 不做什么

- 不做跨会话/全局搜索（侧栏搜索是另一件事）。
- 不做正则表达式、全词匹配、搜索历史。
- 不做 PDF 式"结果分布缩略图"或命中消息列表（见 §3 备选 B/C）。
- 不搜不可见内容：折叠的工具卡、折叠的思考块。

## 3. 形态决策：浏览器式查找条

备选与取舍：

| | A 查找条（采纳） | B 结果列表面板 | C 混合 |
|---|---|---|---|
| 形态 | 右上角浮条，输入即高亮 | 覆盖层列出命中消息 | A + 展开列表 |
| 符合 Cmd+F 预期 | 是 | 否（更像全局搜索） | 是 |
| 长会话全局视图 | 无 | 有 | 有 |
| 实现面积 | 最小 | 需消息摘要/定位/虚拟化 | 两套导航语义 |

选 A。B/C 的价值在"一眼看到命中散在哪几条消息"，但目前没有用户反馈证明长会话
真需要这层全局视图；flag 机制正好允许后续增量，届时 A 的计数旁加一个展开按钮即可。

### 关键交互

| 动作 | 结果 |
|---|---|
| Cmd+F（flag 开、对话视图、无模态弹窗） | 打开查找条并聚焦输入框；已开则聚焦并全选 |
| 输入 | 120ms 防抖后高亮全部命中，计数 `3/17`；0 命中显示"无结果" |
| Enter / 下一个按钮 | 当前命中 +1，回绕到 0 |
| Shift+Enter / 上一个按钮 | 当前命中 -1，回绕到末尾 |
| Esc | 关闭查找条并清除高亮，焦点还给打开前的元素 |
| 切换会话 | 自动关闭（旧 DOM 的序号没有意义） |
| 关闭查找条 | 所有 `<mark>` 消失，正文恢复原样 |

### 初始"当前命中"落在当前视口附近

打开查找条后，当前命中不取第一个，而取**视口附近**的第一个命中：在所有命中的
`mark` 中选择那个与滚动容器可视区距离最近的，优先取可视区内第一个；若都不在
可视区，取位于可视区下方最近的一个，否则取最后一个。

理由：长会话里第一个命中通常在几万像素之外，打开就跳到最顶部对"我正在读中间
这段"的用户是纯粹的位置丢失。浏览器查找栏也是这个行为。

实现上是纯函数：输入每个 mark 的 `offsetTop`/`offsetHeight` 与容器
`scrollTop`/`clientHeight`，输出序号。可单测，无 DOM 依赖。

## 4. Flag

### 注册项

`yi-agent-rs/crates/yi-agent-flags/src/registry.rs` 的 `REGISTRY`（`registry.rs:35`）
新增第 4 项：

```rust
FlagDef {
    name: SESSION_SEARCH_FLAG,
    summary: "会话内搜索：Cmd+F 在转录里查找文本",
    owner: "yichen",
    expires_on: "2027-04-30",
    default: false,
},
```

并导出 `pub const SESSION_SEARCH_FLAG: &str = "session_search";`。

现有四条硬性测试自动覆盖新项（默认必须 false、过期日未到、名字 snake_case、
日期 ISO）。

### 不需要服务端 gate 模块

`session_groups` 有 `groups_gate.rs`，因为分组要拦服务端下发与写入。本功能纯前端，
没有任何服务端行为要拦，闸门就是前端读回来的布尔值。`yi-agent-flags` 只负责
注册与存取，符合它"不做 RPC/UI 入口"的边界。

### release 封堵链（已现成，不新增代码）

1. `store.rs::status_at` 首行：`build_kind != "dev"` 直接返回 `(false, Source::BuildKind)`。
2. release 下 `flags/*` 方法不存在，返回 `-32601`（`flags_rpc.rs`）。
3. `App.tsx` 握手读 flags 的 `try/catch` 把失败兜成 `false`（fail-closed，`App.tsx:1480`）。
4. `SettingsDialog.tsx:135` 的 `visibleTabs` 让实验页在非 dev 构建下根本不渲染。

## 5. 实时生效链路

照抄 `session_groups` 的通路（`App.tsx:1473` 的一次性读取 +
`App.tsx:458` 的 `flagsCall` 回灌）：

- 握手那次 `flags/list` 里多读一个 `session_search`，写入新 state
  `sessionSearchEnabled`（初值 `false`）。
- `flagsCall` 在 `flags/set` 成功后重读 `flags/list` 并
  `setSessionSearchEnabled(read("session_search"))`。

于是在实验页拨开立即可用、拨关立即失效，不需要重启。这是 `flagsCall` 注释
（`App.tsx:440`）已写明的设计意图。回灌的是**权威值**而非请求参数，宿主拒绝
（到期/未注册/构建硬闸）时界面不会显示一个不存在的状态。

## 6. 高亮架构

### 决策：在 React 树内由 rehype 插件生成 `<mark>`

不用 `TreeWalker` 往已渲染 DOM 里插 `<mark>`。那种做法在这里有两个硬伤：

1. React 会尝试更新/删除已被搬进 `<mark>` 的文本节点，抛 `NotFoundError`。
   流式期间 `LiveTail` 每帧都在改尾部文本节点，冲突概率不低。
2. 关闭搜索需逐个还原 DOM；漏一处就留下永久污染（比如 mark 标签留在
   `HistoryPrefix` 的冻结子树里）。

rehype 插件方案让 React 拥有这些节点：关闭搜索 = `query` 置空 → 自然重渲染，
零还原逻辑。

### 数据流

```
query(防抖后) ──► ChatView
                    ├─ HistoryPrefix / LiveTail → ChatItem
                    │     ├─ agentMessage  → <AgentMessage text query />  (MarkdownText 加 query prop)
                    │     └─ userMessage   → <HighlightText text={item.text} query />  (保留 whitespace-pre-wrap)
                    └─ useTranscriptSearch 扫 DOM → 计数 / 当前序号 / scrollIntoView
```

- 新增 `desktop/src/lib/rehypeSearchMark.ts`：rehype 插件，遍历 text 节点，按
  大小写不敏感的子串切分，命中段换成 `<mark className="search-hit">`。
  必须排在 `rehype-highlight` **之后**运行（高亮插件会先把代码切成多个 span）。
- `MarkdownText` 增加可选 `query` prop，memo 比较器从 `prev.text === next.text`
  （`MarkdownText.tsx:80`、`:97`）改为同时比较 `query`。
- `ChatView` 的 `ChatItem` memo 比较器从 `(item, text)` 扩展为
  `(item, text, query)`；`HistoryPrefix` / `LiveTail` 把 `query` 透传下去。

**`HistoryPrefix` 的冻结前提不受破坏**：`query` 只在用户敲字时变化，流式 delta
期间 query 不变，前缀照旧不重渲染。这是本方案能成立的关键。

### 当前命中与滚动

不做进 React。渲染后由 `useTranscriptSearch` 在滚动容器里
`querySelectorAll("mark.search-hit")`，按 DOM 顺序：

- 总数喂给 `3/17`；
- 给第 N 个加 `data-search-active="true"`（CSS 换个更显眼的底色）；
- 只在 N 变化时 `scrollIntoView({ block: "center" })`，避免每次扫描都跳。

"总数/当前"由渲染后的扫描单向前馈，不会与渲染互相驱动成环。

### 已知限制

跨 markdown 行内元素边界的匹配做不到：`foo **bar** baz` 里搜 `foo bar` 无命中。
这与逐文本节点匹配的常规实现一致，写进文档不隐藏。

### 性能

每次改动 query 会让所有可见消息重新解析 markdown。三条控制：

- 输入 120ms 防抖；
- 前缀消息的 `ChatItem` 仍按 `(item, text, query)` memo，query 不变时零额外开销；
- 计数扫描是 `querySelectorAll`，与消息数线性相关，不涉及 reflow 循环
  （`scrollIntoView` 只在当前序号变化时调用一次）。

## 7. 组件边界

| 单元 | 职责 | 依赖 |
|---|---|---|
| `lib/searchHits.ts` | `normalizeQuery` / `findMatches` / `initialActiveIndex` 纯函数 | 无 |
| `lib/rehypeSearchMark.ts` | rehype 插件，text 节点切分出 hit | `searchHits` |
| `components/HighlightText.tsx` | 用户气泡的纯文本高亮（不解析 markdown） | `searchHits` |
| `components/SearchBar.tsx` | 输入框 + 计数 + 上/下 + 关闭 | 受控组件，无业务逻辑 |
| `hooks/useTranscriptSearch.ts` | query 状态、DOM 扫描、当前序号夹取、滚动 | `searchHits` |
| `ChatView` | 新增 `query` prop 并透传 | 上述 |
| `App` | Cmd+F 监听、flag 门、查找条落位 | 上述 |

### ChatView 结构零改动

现有测试 `ChatView.test.tsx:113` 断言 `container.firstElementChild` 就是滚动容器
（且 class 含 `overflow-y-auto` / `overflow-x-hidden` / `min-w-0`）。因此
**不在 ChatView 根节点加包裹层**。查找条由 `App` 渲染在已有的
`relative flex min-h-0 min-w-0 flex-1 flex-col` 容器里（`App.tsx:2388`），
与该容器内已有的 `ApprovalDialog` 同层。ChatView 只新增一个 `query` prop。

浮层用绝对定位而非占位 flex 行，避免改变容器高度触发 `useStickToBottom` 的
`ResizeObserver` 重新贴底、把用户视野拽走。

### Cmd+F 监听的进入条件

document 级 keydown，全部满足才拦截并 `preventDefault()`：

1. `session_search` flag 为 true；
2. 有打开的会话（`sess.currentId !== null`）；
3. 不在看板视图（`board.selectedBoard === null`）——看板不是转录；
4. 没有模态弹窗：`!prefs.settingsOpen && !current?.approval`。

条件 4 用 App 已有的 state，**不查 DOM**。原打算用 `[aria-modal="true"]` 作判据，
但实测只有 `Modal.tsx:61`（设置页）与 `ProcessDetail.tsx:137` 带这个属性，
`ApprovalDialog` 只有 `role="dialog"`（`ApprovalDialog.tsx:59`）——用它就会在
"等审批"时把 Cmd+F 放进一个被遮罩盖住的转录区。state 判据既覆盖全部弹层，
也不依赖某个组件有没有记得写 ARIA 属性。

监听器要能读到**最新**的这几项，不能被首次渲染的闭包钉死。做法与
`App.tsx:440` 的 `flagsCall` 同源：把判据收敛进一个 ref（或让 effect 依赖
这几个值从而重装监听），不要图省事用空依赖数组 + 直接闭包读 state。

焦点恢复：打开前记住 `document.activeElement`，Close/Esc 时还给它。
Esc 复用 `ui/useEscapeKey.ts`（`active = searchOpen`）。

## 8. 测试

### Rust

`cargo test -p yi-agent-flags`：新注册项被 `the_session_search_flag_is_registered`
覆盖（断言 name、`default == false`、summary 非空、`expires_on == "2027-04-30"`）。

### 前端（`cd desktop`）

```
npx vitest run src/lib/searchHits.test.ts \
  src/lib/rehypeSearchMark.test.ts \
  src/components/HighlightText.test.tsx \
  src/components/SearchBar.test.tsx \
  src/hooks/useTranscriptSearch.test.ts \
  src/components/ChatView.test.tsx \
  src/components/SettingsFlagsTab.test.tsx \
  src/App.test.tsx
npx tsc --noEmit
```

必须覆盖的断言：

- `findMatches`：大小写不敏感；一条消息内多次命中；中文；空 query 返回空数组；
  命中位置与长度正确（用于切分）。
- `rehypeSearchMark`：给定 markdown 生成对应数量的 `mark.search-hit`；
  代码块内文本也被标记；query 为空时**不产生任何 mark**（这是"关闭即还原"的判据）。
- `initialActiveIndex`：视口内第一个优先；全在下方时取最近的；全在上方时取最后。
- `ChatView`：给定 query 后 `mark.search-hit` 数量正确；第 N 个带
  `data-search-active`；query 清空后 mark 全消失；**流式追加文本时前缀的
  `AgentMessage` 渲染次数不因 query 未变而增加**（复用现有 `calls.text` 计数器）。
- `App`：mock `flags/list` 返回 `session_search: true` 时 Cmd+F 出现查找条；
  为 `false` 时按 Cmd+F 不出现、且未调用 `preventDefault`；
  设置页打开时 Cmd+F 不出现查找条。

## 9. 文档

- 本文件。
- `docs/project-management/desktop.md`：Features 加一条 `[x]`，带可验证命令。
- `docs/project-management/yi-agent-flags.md`：加第 4 个 flag 条目与判据。
- `docs/project-management/README.md`：同步模块计数（若有变化）。

## 10. 决策记录

| # | 决策 | 取值 | 理由 |
|---|---|---|---|
| 1 | 搜索范围 | 仅用户消息 + agent 正文 | 覆盖绝大多数需求，且保证"搜到即看得见"，无需自动展开折叠块 |
| 2 | 交互形态 | A 浏览器式查找条 | 符合 Cmd+F 肌肉记忆；B/C 的全局视图等有真实需求再加 |
| 3 | 初始当前命中 | 当前视口附近的第一个 | 长会话里跳回顶部等于位置丢失 |
| 4 | 高亮实现 | rehype 插件生成 mark | 避免 React 与手改 DOM 冲突，关闭即自然还原 |
| 5 | 查找条落位 | App 已有 `relative` 容器，ChatView 零结构改动 | `ChatView.test.tsx:113` 依赖根节点即滚动容器 |
| 6 | flag 名 | `session_search` | 与 `session_groups` 同前缀；发布后不可改名 |
