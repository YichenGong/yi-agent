# 会话分组（Session Groups）设计

## 状态

设计已由用户逐项确认（见「决策记录」），等待 spec review。

## 背景

侧栏当前按 workspace 目录自动分组（`thread/listAll` 返回 `groups: WorkspaceGroup[]`），
会话另有颜色标签（`ThreadMeta.tag`，6 色）与置顶（`ThreadMeta.pin_seq`）。

但这两样都不能表达「这几条会话在改同一模块的不同维度」，用户在多条并行会话之间
来回切换时会记不住谁是谁。需要一种**纯视图层**的分组：把相关会话就近摆在一起，
用一个带标题和描述的框标出来。

关键约束（用户确认）：分组**不改变会话归属**，不跨 workspace，不进入模型上下文。

## 目标

1. 用户能在同一 workspace 内把多条会话收进一个有名字、有描述的框里，侧栏中连续显示。
2. 分组关系持久化在服务端，桌面端与手机端看到同一套分组。
3. 分组只影响侧栏渲染，不影响会话的 cwd、模型、提示词或运行时行为。

## 非目标

- 不做跨 workspace 的组。
- 不做单归属：一个会话可以同时属于多个组。
- 不做组内手动排序。
- 不把组名/描述注入模型上下文。
- 不改变置顶区与 workspace 分组的既有语义（置顶项仍不上浮过滤规则之外的行为）。

## 数据模型

### 存储位置

每个 workspace 一个文件：`<workspace>/.yi-agent/groups.json`。

选它而不是写进各 thread 的 `.meta.json`：一个文件就能列出某组的全部成员；删除会话时
只需改一处；也避免给 `ThreadMeta` 增加会触发 `updated_at` 语义讨论的字段。

### 形状

```json
{
  "version": 1,
  "groups": [
    {
      "id": "grp-<uuid>",
      "name": "登录模块",
      "description": "密码登录、扫码登录、SSO 三条线并行",
      "thread_ids": ["thread-a", "thread-b"]
    }
  ]
}
```

- `id`：服务端生成，`grp-` 前缀 + uuid v4，创建后不变。
- `name`：1..=24 个字符（按 `char` 计），去首尾空白后判定；空名拒绝。
- `description`：0..=120 个字符，可为空串。
- `thread_ids`：去重后的数组，成员数不限。**不是权威归属**——权威仍是每个 thread
  的 `cwd`，见「不变量」。
- `version`：当前固定 `1`。读到未知版本按空分组处理并 `tracing::warn!`，不阻断启动。

### 不变量

1. **同 workspace**：`thread_ids` 中每个会话的 `cwd` 必须等于该组所在 workspace 的路径。
   违反者在读取时被静默剔除（不报错、不阻塞列表）。
2. **成员存在**：不存在的 thread id 在读取时剔除。
   会话被删除时，`thread/delete` 同时从所在 workspace 的 `groups.json` 移除该 id。
3. **空组自动解散**：写入/删除后若 `thread_ids` 为空，该组从文件里移除。
4. **两处显示**：同一会话若被置顶，仍在顶部 Pinned 区出现一次；同时保留在组框内。
   这与现有的「置顶从各分组里过滤掉」规则**不同**，是本功能刻意引入的例外。
   组框内的会话在其 workspace 分组里不再重复显示。

## 协议

### 列表

`thread/listAll` 的每个 `WorkspaceGroup` 增加字段：

```json
{
  "workspace": "/path/to/proj",
  "exists": true,
  "threads": [ ... ],
  "groups": [
    { "id": "grp-…", "name": "登录模块", "description": "…", "threadIds": ["thread-a"] }
  ]
}
```

- `groups` 恒存在（无组时为 `[]`），旧客户端忽略即可。
- 服务端在 `build_list_all_response` 中按 workspace 读取 `groups.json`，剔除违反
  不变量的成员（跨 workspace、已不存在的 thread）。**过滤只发生在读取路径**：
  过滤后为空的组不下发，但磁盘上的条目保留到下一次写操作——写路径（`create` /
  `update` / `setMembers` / `delete` 与 `thread/delete` 的联动）在看到空成员表时
  才真正删除该组。这样读路径永远是纯读，不会因为一次列表请求而写盘。
- 组内 `threadIds` 的顺序无意义；渲染顺序由客户端按会话现有排序决定。

### 变更 RPC

均要求 `Scope::Control`（与 `thread/rename`、`thread/setTag` 同档）。

| 方法 | 参数 | 结果 |
| --- | --- | --- |
| `thread/group/create` | `{ workspace, name, description?, threadIds }` | `{ group }` |
| `thread/group/update` | `{ workspace, groupId, name?, description? }` | `{}` |
| `thread/group/setMembers` | `{ workspace, groupId, threadIds }` | `{}` |
| `thread/group/delete` | `{ workspace, groupId }` | `{}` |

校验顺序（与 `setTag` 一致：参数校验先于存在性）：

1. `workspace` 必须是已索引目录，否则 `-32602`。
2. `name` 超长或去空白后为空 → `-32602`；`description` 超长 → `-32602`。
3. `threadIds` 含非本 workspace 的会话 → `-32602`，错误信息带上违规 id。
4. 组不存在 / `groupId` 不属于该 workspace → `-32602`，message 为
   `unknown group: {groupId}`。与 thread 不存在的 `RpcError::unknown_thread`
   （自定义业务码）刻意区分：这里不新增错误码，参数错误一律 `-32602`。

### 通知

变更后广播：

```json
{ "method": "thread/groupsChanged", "params": { "workspace": "/path/to/proj" } }
```

只带 workspace，不带全量分组：客户端收到后重拉 `thread/listAll` 或按 workspace 增量
拉取（实现计划选择其一；本 spec 只要求「客户端最终看到一致状态」）。

## 服务端实现

新增模块 `yi-agent-app-server/src/group_store.rs`，与 `settings_store.rs` 同款约定：

- `load(workspace: &Path) -> Vec<Group>`：缺文件返回空表（不打日志）；读不出或解析失败
  `tracing::warn!` 并返回空表。
- `save(workspace: &Path, groups: &[Group]) -> io::Result<()>`：临时文件 + rename 原子写。
- `mutate(workspace, f: impl FnOnce(&mut Vec<Group>)) -> io::Result<()>`：读-改-写，
  进程内用 `Mutex<()>` 串行化（每 workspace 一把，存 `HashMap<PathBuf, Arc<Mutex<()>>>`
  或复用全局单锁，实现计划决定）。

`thread/delete` 在删除成功后调用一次 `group_store::remove_thread`，失败只记日志，
不影响删除结果。

## 客户端实现

### 数据流

`useSessionState` 的 `groups` 已带 `WorkspaceGroup[]`；新增的 `groups` 字段随之到达。
不需要新状态：分组是纯派生视图。

新增纯函数模块 `desktop/src/lib/sessionGroups.ts`：

- `layoutWorkspace(...)`：输入一个 `WorkspaceGroup` 与折叠集合，输出「渲染行」序列：
  组框（含成员）与未归组会话交错，归组会话只出现一次。
- `boxOrder(...)`：组框在其 workspace 内的插入位置——按框内**最靠前**会话的现有排序
  位置落位（用户已确认不引入手动排序，因此用「最近更新倒序」的既有顺序）。
- `membersOf(group, threads)`：按 `threads` 的现有顺序返回成员，保证与侧栏其他行同序。

纯函数先行、单测覆盖，再接 UI（与 `workspaceGroups.ts`、`threadSearch.ts` 同一做法）。

### 渲染

`ThreadSidebar.tsx` 的 `renderGroupNode` 内部：

- 组框：`rounded-md border border-line-strong` 容器，左边缘 1px 强调色条。
- 框头：折叠箭头 + 组名（始终显示，`truncate`）+ 成员数徽标；展开时才渲染描述
  （`text-xs text-fg-subtle`，两行截断）。
- 框体：成员行走现有会话行组件，缩进一档。
- 折叠状态：本地 `useState`，不持久化（与 workspace 折叠一致，现有 `collapsed` 集合）。
- 搜索期间：不折叠（与现有分组同一口径），且组框只显示命中成员；框内全不命中时
  整个框隐藏。

### 建组与编辑

- 多选：会话行增加选择态（`Ctrl/Cmd+点击` 进入多选，`Shift+点击` 区间选择）。
  多选激活时，侧栏底部出现操作条：「新建组（n）」。
- 新建：点击后弹出就地输入框填组名（必填）与描述（可选），Enter 保存，Esc 取消。
- 编辑：点击框头组名进入就地编辑（`contentEditable` 或 input 覆盖），Enter / 失焦保存，
  Esc 恢复。描述同理，点击即编辑。超长输入按 `maxLength` 截断。
- 成员增删：会话行右键菜单增加「加入组…」与「移出当前组」；组框右键菜单提供
  「重命名」「编辑描述」「删除组」。

## 错误处理

- 分组读写失败绝不让侧栏变空：读失败按「无组」渲染，写失败在侧栏底部提示一次
  （复用现有错误 toast/红字机制，实现计划确定挂点）。
- 客户端收到 `thread/groupsChanged` 后重拉失败：保留当前视图，下次列表刷新时收敛。

## 测试

服务端（`cargo test -p yi-agent-app-server`）：

1. `group_store` 往返：写后读、原子写、缺文件返回空、损坏 JSON 返回空并 warn。
2. `create` 写入后 `thread/listAll` 的对应 workspace 分组带出该组。
3. 跨 workspace 的 `threadIds` 被拒绝（`-32602`），磁盘不变。
4. 删除会话后该 id 从 `groups.json` 移除；成员被删空后组消失。
5. 超长/空名、超长描述被拒绝。
6. 变更广播 `thread/groupsChanged` 且参数只含 workspace。
7. 旧 `groups.json` 含未知字段/未知版本：读取不 panic，返回空或忽略未知键。

客户端（`npm test`）：

1. `layoutWorkspace` 纯函数：归组会话不重复出现、多组交错顺序、置顶会话在框内保留。
2. 组框渲染：标题常显、描述仅展开时显示、折叠可切换、成员数正确。
3. 多选 + 新建组：发出正确的 RPC 参数，成功后侧栏出现框。
4. 搜索过滤：框内只剩命中成员；全不命中时框隐藏。
5. 就地编辑：Enter 保存、Esc 取消、超长被截断。

## 决策记录

| 问题 | 决定 |
| --- | --- |
| 语义 | 临时折叠视图，不改归属 |
| 持久化 | 服务端，两端同步 |
| 归属 | 多对多：一个会话可属于多个组 |
| 存储 | 单一 `groups.json`（`{id,name,threadIds}`） |
| 跨 workspace | 不允许 |
| 展示位置 | workspace 分组内部画框，就近聚拢 + 标题/描述 |
| 建组 | 多选会话后「新建组」 |
| 重复显示 | 组内只显示一次；置顶仍可重复一次 |
| 组内排序 | 沿用现有「最近更新倒序」，不做手动排序 |
| 名称/描述上限 | 24 / 120 字符，成员数不限 |
| 生命周期 | 成员删空即自动解散 |
| 描述显隐 | 仅展开时显示 |
| 编辑方式 | 就地编辑，Enter 或失焦保存 |
| 注入上下文 | 不做 |

## 实验开关（`session_groups`，2026-10-22 追加）

整个分组能力由 `yi-agent-flags` 注册表里的 `session_groups` 管，默认**关**。
关掉之后：

- 服务端 `thread/listAll` 每目录下发空 `groups`，四个 `thread/group/*` 写臂
  一律 `-32602`；
- 前端不渲染组框、不显示「新建组」动作条、隐藏「加入 / 移出分组」入口，成员
  会话退回普通行（**关的是框，不是会话**）；
- 磁盘上的 `session-groups.json` **不被删除**，重新打开立刻恢复。

开→关是纯前端隐藏，不需重拉列表；关→开必须重拉一次 `thread/listAll`——关着
期间服务端根本不下发组表，只把开关置 true 只会得到一片空白。

| 问题 | 决定 |
| --- | --- |
| 开关粒度 | 整个分组能力一个 flag，不做「只关建组」这类子开关 |
| 默认值 | 关（注册表铁律） |
| 关闭时的数据 | 保留，不删 |
| 关闭时的会话 | 照常列出，只是不在框里 |
| 关闭时写请求 | 拒绝（`-32602`），不静默成功 |
