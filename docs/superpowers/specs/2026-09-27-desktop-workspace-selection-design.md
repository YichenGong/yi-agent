# 桌面 GUI：工作目录选择 + 最近目录 设计

> 承接 `docs/project-management/desktop.md` 路线图 P2「工作区切换」：
> 让用户选择启动目录，并保留「最近目录」下拉供快速复用。
> 参考 VSCode 的 "Open Folder" + recent folders（本设计采用「每个对话独立目录」语义，
> 见 §3 关键决策）。

## 1. 背景与目标

**现状**：桌面端（`desktop/`，Tauri 2 + React 19）拉起 `yi-agent app-server` sidecar，
sidecar 的工作目录**写死为 `$HOME`**（`desktop/src-tauri/src/bridge.rs:93-94`，GUI 从不传
`--workdir`，只把进程 cwd 设为 home）。`workdir` 影响权限沙箱根、工具根、skills 路径、
`.env` 加载、thread 存储（`<workdir>/.yi-agent/threads/`）。`thread/start` 不接受 cwd，
每个 thread 都继承全局 `cfg.workdir`（`server.rs:256`）。用户没有任何界面可以选择目录。

**目标**：用户能手动选择工作目录；每个对话拥有**自己的**工作目录；最近用过的目录以
下拉形式保留，无需每次重选；侧栏按目录分组展示全部对话（可折叠）。

**成功判据**：
- 用户可从原生文件夹选择器选一个目录并新建对话；该对话的工具/权限沙箱根 = 所选目录。
- 侧栏按目录分组展示全部历史对话，分组可折叠；失效目录置灰。
- 关闭并重启 app，侧栏仍按目录分组、下拉仍记得之前用过的目录。
- 每个对话的目录相互隔离（目录 A 的对话看不到目录 B 的历史）。
- `cargo test -p yi-agent-app-server` 与 `desktop` 前端单测全绿；现有 CLI e2e 不回归。

## 2. 范围边界

**做什么：**
- 全局「最近目录」索引（app-server 侧持久化）+ 下拉复用
- app-server 新方法：`workspace/list`、`workspace/add`、`workspace/remove`、`thread/listAll`
- `thread/start` 接受可选 `cwd`；agent 与 `ThreadStore` 改为**按每个 thread 的 cwd 构建**
- 桌面端：原生文件夹选择器、目录下拉、侧栏按目录分组（可折叠）、组头右键菜单

**不做什么（YAGNI / 延后）：**
- 不做「在 UI 里给已有对话改目录」（要换目录就新建对话）
- 不做目录重命名/移动、收藏/置顶、跨目录搜索
- 不做 sidecar 按 selected dir 重启（本设计为 per-thread 目录，sidecar 沿用固定启动 cwd）
- 不做目录级配置 / 每目录独立的 model、api key 编辑（GUI 配置编辑仍属路线图）
- 不做多窗口共享（P3）

## 3. 关键决策

| 决策 | 选择 | 理由 |
|---|---|---|
| 目录粒度 | **每个对话独立目录** | 与用户诉求一致；不让整个窗口绑定单一目录 |
| 侧栏组织 | **按目录分组 + 可折叠**（方案 3） | 跨目录可见 + 结构清晰 |
| 最近目录归属 | **app-server 侧集中持久化** | 对话本就按目录存于 `<dir>/.yi-agent/`；一 RPC 即可跨目录列全部对话 |
| 目录列表内容 | **仅「用户打开/添加过的目录」** | 贴近 VSCode recent folders 语义：记的是打开过的文件夹 |
| 移除目录语义 | **仅从列表移除，不碰数据** | 「移除」是视图操作，不应隐式删数据 |
| 新建对话入口 | **全局按钮必须先选目录；组头右键则用该组目录** | 兼顾明确性与效率 |
| agent 构建 | 克隆 `cfg` 覆盖 `workdir` 后调 `bootstrap_agent` | `bootstrap_agent` 已全从 `cfg.workdir` 取值，无需改其签名 |
| 协议兼容 | 只新增方法，`PROTOCOL_VERSION` 保持 1 | 现有客户端不受影响 |

## 4. 存储设计

### 4.1 全局「最近目录」索引（新）

```
~/.yi-agent/workspaces.json
```

```json
{ "dirs": ["/Users/x/projectB", "/Users/x/projectA"] }
```

- 有序：最近使用的排前面。`workspace/add` 去重后置顶。
- 与 thread 数据**分离**：移除目录 = 删此项，不碰 `<dir>/.yi-agent/`。
- 读写用一个进程内 `Mutex` 串行化（同 `ThreadStore.meta_lock` 思路）；
  写入用 temp 文件 + rename 原子替换（复用 `thread_store` 的 `write_atomic` 模式）。

### 4.2 每个目录的对话（沿用现有）

```
<dir>/.yi-agent/threads/
  <thread_id>.jsonl       # 只追加
  <thread_id>.meta.json   # 可变，含 cwd 字段
```

格式**不变**。`ThreadMeta.cwd` 已存在，天然承载「该对话属于哪个目录」，无需迁移。

### 4.3 关键约束

- 对话的目录归属以 `ThreadMeta.cwd` 为准；`ThreadSession.cwd` 作为内存中的权威索引键。
- 目录索引与对话数据在存储上分离，但 `thread/listAll` **以索引为驱动**：只遍历
  `workspaces.json` 里的目录（失效目录跳过），不主动扫描磁盘上的其它目录。
  「索引里没有该目录」即该目录的对话不出现在侧栏——这与 §3「移除仅从列表移除」一致
  （移除 = 隐藏，不动数据）。副作用：升级安装若 `$HOME/.yi-agent/threads/` 已有对话
  而未进索引，初始侧栏看不到；缓解（启动时用 `cfg.workdir` 播种索引）见
  `docs/project-management/desktop.md`「已知限制 / P2 待办」。

## 5. 协议设计

### 5.1 方法清单（新增/改动）

| 方法 | 参数 | 结果 | 错误 |
|---|---|---|---|
| `workspace/list` | `{}` | `{workspaces:[{path,exists}]}` | — |
| `workspace/add` | `{path}` | `{path}` | `-32602` 非目录/不存在 |
| `workspace/remove` | `{path}` | `{}` | — |
| `thread/listAll` | `{}` | `{groups:[WorkspaceGroup]}` | — |
| `thread/start`（改） | `{cwd?}` | `{thread_id,cwd,model}` | `-32602` cwd 非法 |

```
WorkspaceGroup = { workspace: string, exists: bool, threads: [ThreadSummary] }
ThreadSummary  = { thread_id, cwd, model, created_at, updated_at, title }   // 同 thread/list
```

- `workspace/list`：只读全局索引；`exists` 由服务端 stat 判定，供 UI 置灰失效目录。
- `workspace/add`：`canonicalize` 后校验是目录 → 置顶入索引。不展开 `~`（前端 dialog 返回绝对路径）。
- `workspace/remove`：从索引删除；不存在则幂等成功。
- `thread/listAll`：遍历全局索引里每个目录的 `ThreadStore::list()` 合并；失效目录跳过。
  组内按 `updated_at` 降序（复用现有排序），组间按「目录在索引中的顺序」排。
- `thread/start`：`cwd` 缺省用 `cfg.workdir` 兜底（保持旧的单目录行为与测试兼容）；
  提供时 `canonicalize` + 校验是目录 → 用它构建 agent/store → 写 `ThreadMeta.cwd`
  → 顺带确保该目录进索引（等价 `workspace/add`）。

### 5.2 保持不变

- `thread/resume`：已从 `meta.cwd` 读目录（`server.rs:359-363`），仅修正其 agent 构建路径
  按同一 cwd（见 §6(a)）；`meta.cwd` 为空时仍用 `cfg.workdir` 兜底（损坏重建路径）。
- `thread/rename` / `thread/delete` / `turn/*` / `config/read`：接口不变。
- `thread/started` 通知的 `cwd` 字段形态不变。
- `thread/list` 退役为内部函数（单目录列表），由 `thread/listAll` 对外取代。

错误码沿用现有约定（`protocol.rs`）：`-32010` 未初始化、`-32011` 未知 thread、
`-32012` turn 进行中、`-32602` 参数非法。

## 6. app-server 内部设计（核心改动）

### (a) agent 工厂改为带 cwd

现状 `server.rs:53-64` 的工厂闭包固定用全局 `cfg.workdir`；改为接受 cwd：

```rust
// 工厂签名: Fn(Option<Session>, &Path) -> Result<BuiltAgent>
move |session, cwd| {
    let mut thread_cfg = cfg_base.clone();
    thread_cfg.workdir = cwd.to_path_buf();      // 权限/沙箱/工具/skills 全自动跟随
    let built = bootstrap_agent(&thread_cfg, Interactive)?;
    Ok(BuiltAgent { agent: apply_session(built.agent, session), .. })
}
```

`bootstrap_agent` **不改**：它已从 `cfg.workdir` 取权限检查器（`bootstrap.rs:239/302`）、
工具根（`:135`）、skills 路径（`:339`），覆盖后即按 thread 目录构建。`RuntimeConfig` 已是 `Clone`。

> 顺带修现存隐患：`thread/resume` 目前也用全局 cfg 构建 agent，却把 `meta.cwd` 写进
> `session.cwd`——目录分化后 resume 的 agent 会跑在 `$HOME` 而非对话真实目录。改签名后
> start/resume 两条路都按正确 cwd。

### (b) `ThreadStore` 按 thread 构建

现状 `server.rs:109` 只建一个绑 `cfg.workdir` 的全局 store。改为每个 thread 用自己的
`Arc<ThreadStore::new(cwd)>`：
- `thread/start` / `thread/resume`：按解析出的 cwd 建 store，连同 agent 交给 driver。
- driver 收到的 store 从「全局」变为「本 thread 的」。
- 按 thread 操作的 `rename` / `delete` / `interrupt_and_wait_for_persist` 等，从
  `session.cwd` 派生该 thread 的 store，不再用全局 `store`。

### (c) thread 路由

- `ThreadSession.cwd` 为权威索引键（已有字段）。
- 全局「最近目录」索引由 app-server 持有（`Arc<WorkspaceIndex>`），供 `workspace/*` 与
  `thread/listAll` 使用；`thread/start` 成功后写入。

## 7. 前端设计

### 7.1 依赖与权限

- Rust：`desktop/src-tauri/Cargo.toml` 加 `tauri-plugin-dialog`；`lib.rs` 注册插件。
- 前端：`package.json` 加 `@tauri-apps/plugin-dialog`。
- `desktop/src-tauri/capabilities/default.json` 加 `dialog:allow-open`。

### 7.2 组件

- **`ThreadSidebar.tsx`（改）**：props 由扁平 `threads` 改为 `groups: WorkspaceGroup[]` + `currentId`
  + 回调。每组渲染组头（折叠 caret、路径、右键菜单）+ 组内对话列表。
  - 组内每条沿用现有交互（选中/双击重命名/删除）。
  - 折叠状态为本地 `useState`（纯视图状态，不持久化）。
  - 组头右键菜单：`New thread here`（带该 dir 调 `thread/start`）、`Remove from list`
    （调 `workspace/remove`）。用自绘轻量 context menu，不引入 UI 库。
  - 失效组（`exists:false`）路径置灰、组内仍可查看但提示目录不存在。
- **`App.tsx`（改）**：
  - `refreshThreads` 改用 `thread/listAll`；state 由 `ThreadSummary[]` 变 `WorkspaceGroup[]`。
  - `newThread(cwd?: string)` 接受目录。
  - 新增 `addWorkspace(path)` → `workspace/add` 后刷新。
  - 顶部「+ New thread」：弹目录下拉（`workspace/list` 结果 + `Browse…` 调 dialog →
    `workspace/add`）；选定后带 cwd 调 `thread/start`。
  - **启动流程**：`initialize` → `workspace/list` + `thread/listAll` → 有历史对话则 resume
    最近一条；否则显示空态引导选目录（**不再自动在 `$HOME` 建对话**）。
- **`StatusBar`**：`cwd` 继续取 `threadInfo.cwd`，现在会正确显示每条对话自己的目录。

### 7.3 竞态与交互一致性

沿用 P1 持久化的既有约束：`turnActive` 期间禁用侧栏切换/删除；resume 前同步
`session.reset()`；切换一次只允许一个。新增：组头右键新建/移除操作在 `busy` 时禁用。

## 8. 错误处理

| 场景 | 行为 |
|---|---|
| `workspace/add` 路径不存在/非目录 | server `-32602`；前端提示、不入索引 |
| `thread/start` cwd 非法 | server `-32602`；前端不建对话 |
| 目录已失效 | `workspace/list` 返回 `exists:false`，UI 置灰；`thread/listAll` 扫描时跳过 |
| resume 时 `meta.cwd` 已失效 | server 返回明确错误，UI 提示「工作目录已不存在」，**不静默回退 `$HOME`** |
| 索引文件损坏 | 反序列化失败按空列表 + stderr 日志，不阻断启动 |
| 索引并发写 | 进程内 `Mutex` 串行化 + 原子替换 |
| 磁盘写失败 | server 返回错误；thread 创建本身尽力而为（记 stderr，同 P1 策略） |

## 9. 测试策略

- **Rust 单测（app-server）**：
  - `workspace/add|list|remove`：增删、置顶去重、失效标记、损坏索引容错。
  - `thread/start` 带 cwd：`ThreadMeta.cwd` 正确；agent 权限沙箱根 = 该 cwd（tempdir 断言）。
  - `thread/listAll`：跨多目录分组正确、组内排序、失效目录跳过。
  - `thread/resume`：按 `meta.cwd` 构建 agent（修正后的路径）。
- **回归**：所有依赖全局 `cfg.workdir` 的旧测试应仍通过（缺省 cwd 时行为不变）。
- **前端 Vitest**：分组/折叠/排序纯函数；`newThread(cwd)` 组装 params；失效目录渲染。
- **手动验收**：打包 `.app`，分别选两个目录各建对话；重启后侧栏仍按目录分组、下拉仍记得两目录。
- **文档**：更新 `docs/project-management/desktop.md`（把 `desktop.md:63` 工作区切换标为完成并
  给判据）、`docs/project-management/yi-agent-app-server.md`（新方法）、`README.md` 计数 ——
  按 CLAUDE.md 同一 commit 提交。

## 10. 实现分期

1. **后端协议 + per-thread 构建**：`workspace/*`、`thread/start` cwd、`thread/listAll`、
   按 cwd 建 agent/store + Rust 测试。
2. **前端 UI**：侧栏分组、目录下拉、原生 dialog、组头右键菜单 + 前端测试。
3. **文档**：同步 `docs/project-management/` 与 `README.md`。

## 11. 风险与缓解

| 风险 | 缓解 |
|---|---|
| 改 agent 工厂签名牵动 start/resume 两条路径 | 先加「cwd 传递正确」单测锁定，再改实现 |
| 全局 store 改 per-thread 漏改某处（rename/delete） | 用 `ThreadSession.cwd` 统一派生；补 delete 活跃 thread 的磁盘断言测试 |
| 旧测试依赖全局 workdir | 缺省 cwd 兜底保持旧行为；跑 `-p yi-agent-app-server` 全量回归 |
| 跨目录扫描在目录失效时卡顿 | 失效目录先 stat 跳过，不做深扫 |
| 前端引入 dialog 插件权限疏漏 | capabilities 显式加 `dialog:allow-open`，手动验收选目录路径 |
