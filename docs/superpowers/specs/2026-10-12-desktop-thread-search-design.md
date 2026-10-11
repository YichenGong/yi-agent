# 桌面端 / 手机端：会话搜索栏 — 设计

日期：2026-10-12
状态：待实现
分支：`feat/desktop-thread-search`
范围：仅前端 `desktop/`（`ThreadSidebar.tsx`、新增 `lib/threadSearch.ts`、`index.css` 及其测试）。不改 `App.tsx`、不改 Rust、不改协议。

---

## 1. 需求与已核实事实

用户需求：Mac 和手机端，在「+ New thread」**下方**、会话目录列表**上方**，需要一条搜索栏，
具备关键词匹配，方便找到对应的对话。

核实后的事实：

- 桌面端（Tauri）与手机端（配对 iPhone）**共用同一个侧栏组件**
  `desktop/src/components/ThreadSidebar.tsx`。手机端只有 `isMobile` 为真
  （`lib/useIsMobile.ts`，判定信号是 relay 绑定 + iOS UA），差异仅在长按菜单、抽屉
  定位与若干 `[data-mobile="true"]` CSS。**因此一处实现同时覆盖两端。**
- 侧栏顶部是 `ThreadSidebar.tsx:602` 的 `<div className="relative p-2">`，里面是
  「+ New thread」按钮（`602`–`665`，含自绘下拉菜单）。
- 其下是滚动列表容器 `ThreadSidebar.tsx:666` 的
  `<div className="flex-1 overflow-y-auto">`，其中依次渲染：空态提示、`Pinned` 分区
  （`670`）、各项目分组（`688`，含「看板」条目、「看板会话」折叠小节、直属会话行）。
- 列表数据来自 `thread/listAll`（`App.tsx:545`–`553`），**一次返回全部会话**
  （按 workspace 分组 + 顶层 `pinned` 数组）。服务端没有搜索类 RPC（app-server 现有
  方法只有 `thread/list` / `thread/listAll` / `resume` / `rename` / … 无 search），
  所以**过滤完全在客户端完成，不需要任何后端改动**。
- `ThreadSummary`（`lib/protocol.ts:212`）只有 `thread_id` / `cwd` / `model` /
  `title` / `pinned` 等字段，**不含消息正文**，故正文检索不在本次能力范围内。
- 手机端长按（= 右键）由 `lib/useLongPress.ts` 委托判定落点，只识别
  `[data-thread-row]`（会话行）与 `[data-ws-header]`（分组头）。搜索栏只要**不嵌在**
  这两个属性内，就永远不会被长按误判。
- 手机端 `index.css` 用 `[data-mobile="true"] .app-sidebar [data-thread-row]` 关掉了
  `user-select`；搜索栏不在该属性内，文字默认可选中。

## 2. 决策（方案 A：独立纯函数模块 + 过滤树）

**逻辑归属**：新增纯函数模块 `desktop/src/lib/threadSearch.ts`，导出
`filterThreadTree(groups, pinned, query)`，不含 React、不含 DOM。与仓库既有约定一致
（`boardIndex.ts`、`sidebarWidth.ts`、`relativeTime.ts` 同构），匹配规则可脱离 DOM 单测。

被否的备选（仅记录）：

- B「内联写在 `ThreadSidebar.tsx`」：`ThreadSidebar.tsx` 已 817 行，且匹配规则只能靠整棵
  DOM 渲染才能测。
- C「在 `App.tsx` 里过滤」：输入框在侧栏内部，为其提升状态到 `App` 无收益。

**结果展示**：保留既有分区结构做**过滤树**，不做扁平结果列表。被否备选（仅记录）：
扁平列表会丢掉 `Pinned` / 项目分组 / 「看板会话」小节这些语义，等于在侧栏里再写一套渲染。

**匹配规则**：按空白分词、逐词 AND、每词为不区分大小写的子串。

**折叠处理**：搜索期间**忽略**折叠状态（命中即展开）；清空后原样恢复，因为折叠 Set 全程不被修改。

## 3. 改动

### 3.1 新增 `lib/threadSearch.ts`

```ts
export interface ThreadSearchResult {
  pinned: ThreadSummary[];
  groups: WorkspaceGroup[];
  /** 查询非空但无任何命中；用于区分「没输入」与「搜不到」。 */
  empty: boolean;
}

export function filterThreadTree(
  groups: WorkspaceGroup[],
  pinned: ThreadSummary[],
  query: string,
): ThreadSearchResult;
```

语义：

1. `query.trim()` 为空 ⇒ 原样返回 `groups` 与 `pinned`，`empty: false`；调用方据此跳过
   全部过滤逻辑（不进入匹配路径，行为与改动前逐字节一致）。
2. 按 `/\s+/` 分词；**每个词都必须命中**（AND）；每词判定为「`toLowerCase()` 后的子串」。
3. 判定一个词是否命中某分组（`WorkspaceGroup.workspace`）：命中 `basename(workspace)`
   或完整 `workspace` 路径。
4. 判定一个词是否命中某会话（`ThreadSummary`）：命中其渲染标题文本
   （`title ?? "(untitled)"`，与 `ThreadSidebar.tsx:442` 的渲染文本一致），或命中其所属目录路径。
   所属目录的取法**按数据来源区分**，这是明确定义：
   - 分组内的会话（`groups[].threads`）：用该分组的 `workspace`（分组头展示的就是它的 basename）。
   - `pinned` 数组里的会话：用该会话自己的 `cwd`（它在 `pinned` 数组里不挂靠任何分组对象）。
   - 两者在实际数据里是同一个路径，分开写只是为了在两个数据源上都可判定，不依赖调用顺序。
   - **刻意不搜 `thread_id`**：它在界面上不可见。
5. **目录命中 ⇒ 该目录下全部会话都保留**，而不是只保留标题也含该词的。搜 `projA` 应该把
   该项目整体交给用户。
6. 过滤后：`pinned` 全空则返回空数组（现有 `pinned.length > 0` 守卫使整段分区消失）；
   某分组一行不剩 ⇒ 该分组从结果里移除（组头一并消失）；「看板会话」小节内的卡片会话
   同样参与过滤，无存活则小节不渲染。
7. `empty` 仅在查询非空且 `pinned` 与所有分组均为空时为 `true`。

### 3.2 `components/ThreadSidebar.tsx`

1. 新增状态 `const [query, setQuery] = useState("")`。
2. 在 `602` 的 `p-2` 区块**之后**、`666` 的滚动容器**之前**，插入兄弟节点搜索栏区块：
   - `<input type="search">`，`aria-label="搜索会话"`，
     `placeholder="搜索会话…"`，`value={query}`，`onChange` 直接 `setQuery`。
   - 非空时右侧显示清除按钮，`aria-label="清除搜索"`，`type="button"`，点击清空。
   - `Escape` 清空（`onKeyDown`，不 `preventDefault`、不做 IME 特判：清除动作幂等，
     中文输入法组合期按 Esc 取消候选也不会造成损坏）。
   - **外层不加 `tabIndex`**：现有测试用 `container.querySelectorAll("div[tabindex]")[0]`
     取第一个组头（`ThreadSidebar.test.tsx:159`），加了会让该选择器选错。
   - **本区块内不加任何 `aria-haspopup`**：现有测试用
     `button[aria-haspopup="menu"]` 定位 New thread 触发器（`:74`）。
3. 过滤在**渲染时**计算，不写进 state：

   ```ts
   const filtered = filterThreadTree(groups, pinned, query);
   const searching = query.trim() !== "";
   const groups2 = filtered.groups;      // 渲染用
   const pinned2 = filtered.pinned;      // 渲染用
   ```

   这样服务端列表更新后过滤结果自动跟随，不存在两份列表副本不同步的问题。
4. 折叠判定在搜索期间短路（**不修改 Set**）：

   ```ts
   const isCollapsed = searching ? false : collapsed.has(g.workspace);
   const pinnedCollapsed2 = searching ? false : pinnedCollapsed;
   const isBoardCollapsed = searching ? false : !boardCollapsed.has(g.workspace);
   ```

   清空查询后视图原样恢复——`collapsed` / `boardCollapsed` / `pinnedCollapsed` 全程未被触碰。
5. 空态提示：`searching && filtered.empty` ⇒ 显示 `无匹配会话`；否则维持既有
   `选择一个目录开始`（仅在 `groupCount(groups) === 0` 时）。
6. 搜索期间隐藏「看板」条目行（`renderBoardEntry`）：它是项目入口而非会话，本次找的是会话。
   即 `{!isCollapsed && kanbanItemFor(...) && !searching && renderBoardEntry(...)}`。
7. 给重命名输入框（`ThreadSidebar.tsx:405`）加一个标记属性 `data-rename-input=""`。
   **这是必需的**：现有 5 处测试用**裸选择器**取重命名输入框——`ThreadSidebar.test.tsx:82`、`:89`、
   `:93`、`:791` 的 `container.querySelector("input")`，以及 `App.test.tsx:458`、`:481` 的
   `document.querySelector("input")`。搜索框是文件里**更早**出现的 `<input>`，会把这 6 处一起选错。
   加属性后把这些选择器改为 `[data-rename-input]`；属性本身不改任何行为与像素。
   **不要**改这些断言为 `input[type="search"]`——它们断言的是「重命名输入框存在/消失」，指向搜索框
   会让断言恒真（搜索框常在），等于把测试废掉。
8. 分组头、会话行、拖拽、长按、id 归属、看板小节等其余渲染逻辑一律不变（第 7 点那个标记属性除外）。

### 3.3 `index.css`

新增一条手机端专属规则，放在既有 `[data-mobile="true"]` 分组内：

```css
[data-mobile="true"] .app-sidebar input[type="search"] {
  font-size: 16px;
}
```

原因：iOS 在聚焦字号 < 16px 的输入框时会自动放大页面，而 `useIsMobile.ts` 的 `lockZoom`
只能禁用用户的**手动**捏合缩放，挡不住这条自动放大。桌面端不匹配 `[data-mobile]`，
逐字节无影响。

### 3.4 测试

**新增 `lib/threadSearch.test.ts`**（纯函数，node 环境）：

- 大小写不敏感（`PROJA` 命中 `projA`）。
- 多词 AND：两词分别命中标题与路径 ⇒ 保留；有一词不命中 ⇒ 剔除。
- 目录命中 ⇒ 该分组全部会话保留。
- `title` 为 `null` ⇒ 用 `"(untitled)"` 参与匹配。
- 查询为空 / 只有空白 ⇒ `groups`、`pinned` 原样返回，`empty === false`。
- 无命中 ⇒ 所有分组被移除，`empty === true`。
- 一行不剩的分组被移除；`pinned` 全不命中 ⇒ 返回空数组。

**`ThreadSidebar.test.tsx` 新增用例**：

- 搜索框渲染位置：在 New thread 按钮之后、第一个会话行之前（用 `compareDocumentPosition`
  或节点顺序断言）。
- 输入关键词 ⇒ 不匹配的行消失、匹配的行保留；空分组的组头消失。
- 已折叠分组在搜索期间自动展开（命中行可见）；点击清除后恢复为折叠。
- 清除按钮：仅在有内容时出现，点击后列表恢复完整。
- `Escape` 清空查询。
- 无匹配时显示 `无匹配会话`。
- 搜索期间「看板」条目行隐藏。

  注意：`type="search"` 的输入框在 testing-library 里 role 是 **`searchbox`**（不是
  `textbox`），用例用 `getByLabelText("搜索会话")` 或 `getByRole("searchbox")` 取它。
- 更新既有用例里的 6 处裸 `input` 选择器为 `[data-rename-input]`：`ThreadSidebar.test.tsx:82`、
  `:89`、`:93`、`:791`，以及 `App.test.tsx:458`、`:481`。断言语义不变——它们的本意始终是
  「重命名输入框存在 / 已消失」，只是选择器原先靠「侧栏里只有一个输入框」这一巧合成立。
- 这 6 处改完后，在 `ThreadSidebar.test.tsx` 新增一条**判别力用例**：搜索栏有内容时双击某个会话行
  进入重命名，`[data-rename-input]` 必须仍能选中该行输入框（防止未来有人把标记属性挪走、
  又退回裸 `input` 选择器）。

**既有断言**：`ThreadSidebar.test.tsx` 全部现有用例不得改动且必须通过（尤其
`div[tabindex]`、`button[aria-haspopup="menu"]`、`[role="separator"]` 三个选择器契约）。

**`mobileLayout.test.ts` 新增一条**：真实 `index.css` 中存在
`[data-mobile="true"] .app-sidebar input[type="search"]` 的 `font-size: 16px` 规则，
锁住 iOS 聚焦自动放大不回归。

## 4. 边界情况

- **搜索时列表为空**：`empty` 为真 ⇒ 显示 `无匹配会话`，不显示 `选择一个目录开始`。
- **`groupCount(groups) === 0` 且未搜索**：维持原提示不变。
- **服务端列表在搜索期间更新**：无需额外处理，过滤在渲染时计算，自动跟随最新数据。
- **重命名正在进行的行**：`editingId` 与 `query` 互相独立；被搜索过滤掉的行即使正处于编辑态
  也随过滤消失，清空查询后重新出现（编辑态本身未被重置，`editingId` 仍是原值）。
- **手机端键盘**：输入框在 `aside` 内、不在滚动容器里，抽屉滚动时它保持钉在顶部；
  输入框自身可获焦、可吸附输入法。
- **长按/右键**：搜索栏不含 `[data-thread-row]` / `[data-ws-header]`，手机长按判定返回
  `null`，不会弹出任何菜单。
- **`prefers-reduced-motion`**：本次不新增动画，无需处理。

## 5. 不做

- 不搜消息正文 / 工具输出——列表数据里没有正文，需要新增服务端索引与 RPC，属独立需求。
- 不做模糊（fzf 式子序列）匹配、不做高亮、不做搜索历史、不做快捷键唤起。
- 不做扁平结果视图。
- 不改 `App.tsx`、不改 Rust、不改 JSON-RPC 协议。
- 不改任何既有渲染逻辑、颜色 token、分区顺序。
- 不改变桌面端既有像素表现（除新增的搜索栏本身）。

## 6. 验收标准

1. Mac 与手机端侧栏中，「+ New thread」下方、会话目录列表上方，均有且只有一条搜索栏。
2. 输入关键词后，会话树按「标题 / 目录名 / 完整路径」做**不区分大小写**的子串匹配；多个
   关键词以空白分隔、**逐词 AND**。
3. 命中目录 ⇒ 该目录下全部会话保留。
4. 过滤后不剩会话的分组连组头一起消失；`Pinned` 区与「看板会话」小节同样过滤、空了即消失；
   搜索期间「看板」条目行隐藏。
5. 搜索期间命中行所在的折叠分组自动展开；清空查询后恢复原有折叠视图。
6. 搜索栏具备 `type="search"`、`aria-label="搜索会话"`、`aria-label="清除搜索"` 的清除按钮、
   `Escape` 清空；无匹配时显示 `无匹配会话`。
7. 手机端聚焦搜索框时页面**不**自动放大（`[data-mobile="true"]` 下 16px 规则生效）。
8. `desktop/` 下 `npm test` 全绿：`threadSearch.test.ts` 新用例、`ThreadSidebar.test.tsx`
   新用例与**全部既有用例**、`mobileLayout.test.ts` 新断言。
