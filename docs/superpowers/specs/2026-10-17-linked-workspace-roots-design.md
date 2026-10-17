# 关联工作区根（多文件夹项目）设计

日期：2026-10-17

## 背景与问题

桌面端侧栏按目录分组，每个会话（thread）绑定唯一 `cwd`。用户想在多个文件夹里做同一个项目（例如前端在 `frontend/`、后端在 `backend/`）时，没有任何办法表达"这两个目录属于同一个项目"：会话的沙箱、FS 工具、权限检查器全部只认一个根目录，跨目录读写会被拒绝。

## 目标

- 目录级关联：用户可把目录 B 关联到主根 A，形成"一个项目"。
- 真·多根会话：以 A 为 `cwd` 的会话，其 `read` / `write` / `edit` / `glob` / `grep` / `bash` / `process_start` 对 A 与 B 都生效。
- 子 agent 继承：`spawn_agent` 派生的子任务同样获得根集，避免"同一件事里能力悄悄变窄"。
- 侧栏可见：主根组头下缩进显示附属根，用户一眼看出这些目录是一伙的。
- 运行期稳定：根集在创建会话时固化；改关联只影响之后新建的会话。

## 非目标

- 主根下的 `.yi-agent/project.json` 项目文件与团队共享（关联是本机用户的一次点选，不随仓库走）。
- 链式 / 多层关联（只允许一层）与会话内运行时加挂目录。
- 附属根作为会话 `cwd`（附属根小节不提供「New thread here」）。
- 附属根的 worktree 隔离与租约（见"已知限制"）。

## 关键决策与理由

| 决策 | 选择 | 理由 |
| --- | --- | --- |
| 目标形态 | 目录关联 + 会话真多根 | 仅侧栏分组无法解决"跨目录改代码"的实际问题 |
| 项目模型 | 主根 + 附属根，侧栏组头 = 主根 | 复用现有"按目录分组"结构；会话 `cwd` 必须唯一（它决定 agent、store、skills、权限基点、git 上下文） |
| 生效时机 | 创建会话时固化 | 运行中改可写根会让"本回合能写哪些目录"不可预测，且与 resume 后行为不一致 |
| 关联存储 | 扩展现有本机索引 `~/.yi-agent/workspaces.json` | 复用已有的原子写与损坏兜底；无需新增项目文件协议，也不受 git 分支 / worktree 切路径影响 |
| 附属根能否作 cwd | 不能 | 避免同一项目出现两种基点，导致 resume、看板、worktree 既有路径分叉 |
| 根集传递 | 按调用传（thread-local 槽位 + 根会话属性），**不塞进 `cfg`** | `production_factory` 按 `(session, cwd, mode)` 每次调用，而 daemon 的 worker factory 按**项目目录**建、被同目录多会话共享；走 `cfg` 会让 A 会话关联的 B 泄漏给该目录其它会话派生的组件 |
| 子 agent 继承 | 根集挂在根会话上，后代自动继承 | 每个 spawn 入口都要转发会随层级增加漏点；根会话属性天然覆盖孙任务，且与"创建时固化"模型一致 |

## 架构

### 1. 存储：`WorkspaceIndex`（`yi-agent-rs/crates/yi-agent-app-server/src/workspace_index.rs`）

`~/.yi-agent/workspaces.json` 从 `{"dirs": [...]}` 升级为：

```json
{ "dirs": ["/a", "/b", "/d"], "links": { "/a": ["/b"] } }
```

- `links: BTreeMap<String, Vec<String>>`，`#[serde(default)]`；旧文件零迁移，读法不变。
- 新增 `link(primary, child)` / `unlink(child)` / `linked_to(path)` / `children(primary)`，读写沿用现有 `Mutex` 串行化 + temp 文件 rename 原子写（`write_atomic`）。
- 写入前 canonicalize，并要求两者都是已存在的目录。

**不变量**（在 `WorkspaceIndex` 内强制，违反即返回错误让 RPC 回 `-32602`）：

1. 只允许一层：主根不能同时是别人的附属根（禁链式）。
2. 无环。
3. 一个附属根全局只属于一个主根。
4. 主根 ≠ 附属根。

**关键约束**：`dirs` 必须继续包含附属根。`thread/listAll` 靠 `workspaces.list()` 发现冷会话（`server.rs` 的 `thread/listAll` 分支遍历 `workspaces.list()`），把附属根从 `dirs` 摘掉会让它的会话直接消失。"附属根不作为顶层组"是**显示层**行为，不是存储层行为。

### 2. 关联的下发：`thread/start` 与 `thread/resume`

- `ThreadMeta` 新增 `extra_roots: Vec<String>`（`#[serde(default)]`，空 = 单根会话，旧 meta 零迁移）。
- `thread/start`：解析 `cwd` 后读 `links[cwd]`，过滤掉已不存在的目录，写进 meta。
- `thread/resume`：读 `meta.extra_roots` 重建沙箱 / 权限 / 工具根集。

**故意的不对称**：`cwd` 已删必须报错（既有行为，它是权威基点）；附属根已删只静默丢弃、不阻断 resume——它是能力增量，不该让会话打不开。

### 3. 线协议（`yi-agent-app-server/src/server.rs`、桌面 `desktop/src/lib/protocol.ts`）

- `workspace/list` 每项增加 `linked_to: Option<String>`（附属根指向主根；顶层为 `null`/缺省）。
- `workspace/link` `{primary, child}` → `{}`。
- `workspace/unlink` `{child}` → `{}`（幂等，不存在也算成功，与 `workspace/remove` 同口径）。
- 前端 `Workspace` 接口新增 `linked_to?: string | null`。

### 4. 根集传递（避开 `cfg` 陷阱）

仓库已有按调用传会话级参数的先例：`with_session_model_ref` / `with_session_reasoning_effort`（thread-local 槽位）。根集沿用同一模式：

- 新增 `with_session_extra_roots(roots, || ...)` 槽位。
- `thread/start` / `thread/resume` 调工厂时包一层。
- `production_factory` 读槽位：写入 `thread_cfg.sandbox_writable_roots`（沙箱层，管 `bash`/`process_start`），并作为**独立参数**传给工具集构造。
- FS 层不复用 `sandbox_writable_roots`：worker 路径会把 `.git` 相关目录放进可写根，复用会让 FS 工具顺带可写那些目录，属于没人要求过的行为扩张。
- `attach_delegation` → `build_runtime_tooling` 同路传根集，保证委派路径也是多根。

### 5. 工具层：单根 → 根集（`yi-agent-rs/crates/yi-agent-tools/`）

现状：沙箱启动层（`SandboxPolicy`）**已支持**多根（`writable_roots: Vec<PathBuf>`），但 FS 工具层是单根（`ToolsContext.root: PathBuf`，`resolve_and_check` / `resolve_for_write` 做单根包含检查）。只接沙箱层会导致 `bash` 能写副目录而 `write`/`edit` 不能——同一 agent 内部自相矛盾，必须两层一起改。

- `ToolsContext`：`root: PathBuf` → `roots: Vec<PathBuf>`（`roots[0]` = 主根，相对路径的基点）。保留 `root()` 返回 `roots[0]`（现有调用点不破），新增 `roots()`。
- `path_util`：`resolve_and_check` / `resolve_for_write` 收根集，包含检查从"落在唯一根内"改为"落在任一根内"。
  **四条逃逸检查逐条保留**：终 component canonicalize、父目录 canonicalize、词法归一、绝对路径原样。
  **相对路径永远只拼主根**，不做多根猜测——否则同一个 `src/main.rs` 指向哪个根就是歧义。
- `glob` / `grep`：`strip_prefix(root)` 改为"命中哪个根就相对哪个根"。
- `read` / `view_image` / `read_document` / `edit` / `write`：把 `ctx.root()` 换成根集，逻辑不动。
- `PermissionChecker`：`workdir` → `roots`；前缀白名单的 `strip_prefix(workdir)` 改为按命中的那个根取基点。保留原单根构造以免影响无关调用点。

**语义扩张（明示）**：相对路径白名单（如 `src/**`）会对所有关联根生效。这是有意的——用户关联 B 的本意就是"B 是这个项目的一部分"。

### 6. 子 agent 继承

根集作为**根会话（root session）属性**：`AttachApplicationRoot` 增加可选 `extra_roots`，落进 root 任务行；后代通过既有的继承传播路径读取，孙任务自动继承，无需在每层 spawn 上转发。

- `InheritedSandbox` 枚举**不动**（其 kebab 串有精确断言测试）；新增平行的可选字段（如 `extra_roots`）沿同链路走：`AttachApplicationRoot` → root 任务行新列（沿用既有的 `ALTER TABLE` 迁移写法）→ `WorkerStart` → worker 工具注册。
- **FS 层必须一起继承**：`worker_tool_registry` 把继承来的根集同时传给沙箱**和**内置工具注册。否则子 agent 的 `bash` 能写 B、`write`/`edit` 写不了。
- **ReadOnly 子 agent**：继承根集只扩大读的路径解析，不进可写根；沙箱保持 `ReadOnly`，`write`/`edit` 依旧不注册。

### 7. 桌面 UI（`desktop/src/components/ThreadSidebar.tsx`、`desktop/src/App.tsx`）

- 组头右键菜单（现有「New thread here」/「创建·移除看板」/「Remove from list」）新增「关联文件夹…」：调原生目录选择器 → `workspace/link` → 刷新会话与工作区列表。失败走既有错误横幅。
- 主根组头之下**缩进**渲染每个附属根小节（自己的折叠 caret 与会话行）；附属根不再出现在顶层。
- 附属根小节菜单只有「取消关联」（它不能作 `cwd`，故不提供「New thread here」）。
- **嵌套在过滤之后做**：先按现有 `filterThreadTree` 过滤成扁平 groups，再按 `linked_to` 归拢。搜索、置顶、看板会话三个既有分支都不改。嵌套纯函数放 `workspaceGroups.ts`（该文件已有纯函数 + 单测）。
- **dangling link 自愈**：`workspace/list` 只在主根同时存在于索引里时才上报 `linked_to`。于是「Remove from list」掉主根后附属根自动回到顶层；主根加回来则关联显示自动恢复（`links` 记录不被 `remove` 抹掉，与"移除只是视图操作、不删数据"口径一致）。
- 附属根目录被删沿用既有 `(missing)` 显示；看板项目列表行为不变。

## 错误处理

- `workspace/link` 拒绝项一律 `-32602` + 原因：不是目录、主根 == 附属根、主根本身已是别人的附属根（禁链式）、附属根已有主根、会成环。UI 直接显示服务端原因。
- `workspace/unlink` 幂等。
- resume 时附属根已删：静默丢弃、不阻断；`cwd` 已删仍报错。
- 运行中附属根被删：沙箱根的 canonicalize 已有回退，不 panic；FS 层报明确的 not found，**绝不静默写到别的目录**。
- 符号链接：link 时即 canonicalize 存储；包含检查始终对"存储的规范根"与"候选路径的 canonical"比对，逃逸检查强度与今天一致。
- `workspaces.json` 损坏：沿用既有整文件兜底 + `.bak`；`links` 字段畸形走同一条路。

## 已知限制

1. **附属根没有 worktree 隔离**：Coding 子 agent 的隔离来自 worktree，只覆盖主根。附属根 B 不在任何仓库里，子 agent 是**就地写 B**。两个子 agent 同时写 B 会冲突，且主 agent 的集成步骤不会检测到（租约只覆盖主根）。缓解：这类任务串行委派。把租约扩展到附属根属于后续独立 spec。
2. 附属根变化不影响已创建的会话（设计如此）。

## 测试

**Rust**
- `workspace_index`：link / unlink / 四条不变量 / 旧文件零迁移 / 损坏兜底（现有测试保持绿）。
- `path_util`：多根包含检查 + 四条逃逸检查 × 两个根各一遍（`../`、绝对路径越界、终 component 符号链接、中间目录符号链接）。
- `permission`：根集下的前缀白名单解析。
- app-server：`thread/start` 写 `extra_roots`、`resume` 重建、`spawn` 继承端到端（照 `a_git_project_gets_the_delegation_tools` 写法）。
- **泄漏负例（守卫 §4 的 cfg 陷阱）**：同一 `cwd` 两个会话、关联不同 → 互不影响。

**桌面**
- `workspaceGroups` 嵌套纯函数；`threadSearch` 嵌套场景；`ThreadSidebar` 菜单项（关联 → 选择器 → RPC）；`App` 接线。

**验证命令**
- `cd yi-agent-rs && cargo test -p yi-agent-tools && cargo test -p yi-agent-core && cargo test -p yi-agent-app-server && cargo test -p yi-agent-store && cargo fmt --all`
- `cd desktop && npx tsc --noEmit && npm test`

## 交付顺序

拆成两个可独立验证的工作流：

1. **工作流 1**：索引 + 协议 + 多根工具层（沙箱 / FS / 权限）+ 侧栏 UI。
2. **工作流 2**：子 agent 继承（依赖工作流 1 的根集解析函数）。

两者可分批合入。按 CLAUDE.md，同一 PR / commit 内同步更新 `docs/project-management/`（`desktop.md`、`yi-agent-app-server.md`、`yi-agent-tools.md`）与 `README.md` 模块索引计数。
