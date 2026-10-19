# 空会话引导设计（首屏不空白 + 最近项目快速入口）

## 状态

设计草案，等待用户 review。

## 背景与问题

桌面端刚打开、尚未选中会话时，主区域完全空白：`ChatView` 拿到空数组、`MessageInput` 被 `disabled`，没有任何提示告诉用户下一步该点什么。用户要求：空状态要有引导（点 New Thread），并且加入"默认项目目录的记忆与快速入口"。

现状证据：

1. **主区域无占位**：`desktop/src/App.tsx:2196-2290`，当 `sess.currentId === null` 时，`ChatView` 的 `items` 是 `current?.session.items ?? []`，`MessageInput` 的 `disabled={current === null}`。两者都不渲染任何空状态。
2. **`ChatView` 没有空状态分支**：`desktop/src/components/ChatView.tsx:446-460` 只在 `items.length > 0` 时渲染历史与前缀，空列表时只剩一个空容器。
3. **侧栏是唯一入口**：`desktop/src/components/ThreadSidebar.tsx:1034-1050` 有「+ New thread」；`:365-372` 的 `onNewThreadClick` 在 `workspaces.length === 0` 时直接走原生选择器。
4. **初始化后不建会话**：`desktop/src/App.tsx:1403-1410` 在握手成功后只查 `onboarding/status`，不创建会话，因此首屏状态就是"无会话"。
5. **最近目录数据已经存在**：`yi-agent-rs/crates/yi-agent-app-server/src/workspace_index.rs` 维护 `~/.yi-agent/workspaces.json`，`list()` 返回的 `dirs` 中**最近使用的在前**（`workspace_index.rs:55-75`，`add` 先 `retain` 再 `insert(0, ...)`）。`workspace/list` RPC 返回 `{path, exists, linked_to}`（`rpc/workspace.rs:29-54`）。
6. **新建会话会自动登记目录**：`newThread(cwd)`（`App.tsx:736-790`）调 `thread/start`，服务端 `workspaces.add`（`crates/yi-agent-app-server/src/server.rs:6023` 附近）；`onBrowse`（`App.tsx:883-896`）先 `workspace/add` 再 `newThread`。
7. **客户端已有最近目录状态**：`desktop/src/hooks/useSessionState.ts:69` 的 `workspaces` 与 `refreshWorkspaces()`（`:131-136`），`App.tsx:1410` 在初始化后已调用一次。

## 目标

1. 没有选中会话时，主区域显示一块空状态卡片，明确告诉用户可以做什么，而不是空白。
2. 空状态提供两个动作：新建会话（带目录选择）、去配置模型（仅当模型未配置时出现）。
3. 空状态展示最近使用的项目目录，点击即在该目录新建会话；不需要先打开原生选择器。
4. 记忆"最近使用的项目目录"由服务端索引承担，客户端不新增持久化。
5. 不自动创建会话：用户没点之前不产生空会话。

## 非目标

- 不改侧栏结构、分组、置顶逻辑。
- 不新增"默认项目目录"概念（不引入 `default_workspace` 字段）；"默认"通过最近列表的排序表达。
- 不改 `thread/start` 的目录解析（`resolve_thread_cwd`）。
- 不做引导式的多步 tour（只做一块静态空状态卡片）。
- 不改手机端首屏列表缓存（`initialListCache`）。

## 设计

### 1. 空状态的判定

条件（全部满足才显示）：

- `sess.currentId === null`（没有选中会话）；
- 且当前不在看板视图（`board.selectedBoard === null`，看板本身已有自己的空状态）；
- 且不在加载/恢复中（避免恢复会话时闪一下空状态）。

实现位置：`App.tsx` 主区域。把当前 `<> <ChatView/> <MessageInput/> </>` 的分支拆成三臂：

- `sess.currentId !== null`：现状不变；
- `sess.currentId === null && board.selectedBoard === null`：渲染新的 `EmptySessionState`；
- 其余（看板）：现状不变。

`MessageInput` 在没有会话时**不渲染**（而不是渲染成 disabled）。理由是：一个禁用的输入框在空白页上会让人误以为"输入框坏了"，而空状态卡片已经提供了明确的动作。

### 2. 组件

新增 `desktop/src/components/EmptySessionState.tsx`，纯展示组件，props：

```ts
type EmptySessionStateProps = {
  /** 最近使用的目录，最近的在前；已不存在的目录也会传入，由组件置灰。 */
  workspaces: Workspace[];
  /** 是否需要先配置模型（由 onboarding 状态推导）。 */
  needsModel: boolean;
  onCreate: (cwd?: string) => void;   // 无 cwd = 打开原生选择器
  onOpenSettings: () => void;        // 打开设置里的「模型」Tab
  onForget: (path: string) => void;  // 从最近目录里移除
};
```

内容结构（自上而下）：

1. 标题：「开始一个新会话」
2. 说明一句：「选择一个项目目录，或从最近使用里挑一个。」
3. 主按钮：「新建会话…」（调用 `onCreate(undefined)` → `onBrowse`）
4. 最近目录列表（最多 6 项，`workspaces.slice(0, 6)`）：
   - 每项显示 `basename(path)` + 完整路径做 `title`；
   - `exists === false` 的置灰禁用，附「目录不存在」；
   - 点击 → `onCreate(path)` → `newThread(path)`（服务端会顺带登记，`App.tsx:736-790`）；
   - 右侧一个「移除」按钮（`aria-label` 明确），调用 `onForget` → 现有 `removeWorkspace`（`App.tsx:905-917`）。
   - 列表为空时不渲染该区块。
5. 当 `needsModel === true`：一条醒目提示「还没有配置模型，新建会话无法开始对话」+「去配置」按钮（`onOpenSettings`）。

不引入新的 RPC：`workspace/list`、`workspace/remove`、`thread/start` 都已具备。

### 3. 数据与刷新

- `workspaces` 直接复用 `useSessionState` 的 `workspaces`。
- 现有刷新点已经覆盖：初始化后（`App.tsx:1410`）、`onBrowse`/`onNew`/`removeWorkspace`/`link`/`unlink` 之后。
- 补一个刷新点：切回"无会话"状态时（例如关闭最后一个会话或点 logo 回到首页）调用 `refreshWorkspaces()`，保证列表反映最新使用顺序。若不存在"回到无会话"的入口，则不需要。

### 4. `needsModel` 的来源

复用既有 onboarding 状态：`desktop/src/hooks/useSettingsState.ts:46-52` 已有 `onboarding.needed && !dismissed` 的判定，`App.tsx:1403-1410` 已读取 `onboarding/status`。空状态不自己发请求，直接用同一份状态。与配套的模型门禁 spec 一致：`needed` 为真即说明清单里没有可用默认条目。

注意：向导被「稍后设置」关掉后，`dismissed` 为真、`needed` 仍为真。此时**不**再弹全屏向导，但空状态上的提示仍然显示——这正是"跳过设置后怎么把用户带回来"的答案。因此 `needsModel` 判定用 `onboarding.needed`，不叠加 `dismissed`。

### 5. 空状态与 `ChatView`

不改 `ChatView` 的空列表行为：`ChatView` 只在有会话时挂载。这样避免在有会话但消息还没到（加载中）时错误地显示"开始一个新会话"。

## 错误处理与边界

| 场景 | 表现 |
| --- | --- |
| RPC 未连接 | 空状态仍渲染；点击动作走现有错误路径（`setCurrentError`） |
| 最近目录全部不存在 | 列表项全部置灰；主按钮仍可用（原生选择器） |
| 远程客户端（无原生 dialog） | `pickDirectory` 返回 `null`（`App.tsx:855-862`）；此时空状态的主按钮不渲染或禁用，只保留最近目录入口 |
| 最近目录包含当前项目的关联根（`linked_to` 非空） | 空状态只列顶层目录（`linked_to == null`），避免同一项目出现两次 |
| 看板视图打开 | 不渲染空状态 |

## 测试与验收

1. `sess.currentId === null` 且无看板时渲染标题「开始一个新会话」与主按钮（`EmptySessionState.test.tsx` 新增）。
2. 传入 3 个最近目录，按传入顺序渲染；点击第 2 个触发 `onCreate("/path/2")`。
3. `exists === false` 的目录项 `disabled`，并带「目录不存在」。
4. `linked_to` 非空的目录不渲染。
5. `needsModel === true` 时出现「去配置」按钮；`false` 时不出现。
6. `App.test.tsx`：初始化后（无会话）断言空状态出现，且 `MessageInput` 不在 DOM 中。
7. 命令：`cd desktop && npx vitest run src/components/EmptySessionState.test.tsx src/App.test.tsx src/components/ThreadSidebar.test.tsx` 与 `npx tsc --noEmit`。

## 与其它设计的关系

- 模型门禁 spec（`2026-10-19-model-configuration-gate-design.md`）负责"没模型不能开始对话"的行为与错误码；本设计只负责把用户指回配置入口。
- 本设计不改 `subagent` 任何行为。
