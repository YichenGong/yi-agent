# 看板会话归拢到所属项目（可折叠小节）Design

> 状态：设计已定稿，待用户 review 后进入 writing-plans。

## 1. 背景与问题

看板卡片被调度器起成一个**用户可见的会话**时，它的工作目录是插件预建的隔离 worktree
`<项目>/.worktrees/kanban/<slug>`（`plugins/superpowers-kanban/.../worktree.rs::worktree_path`）。
宿主的内核 `start_thread_core` 会把这个 cwd 记进**全局「最近目录」索引**
（`yi-agent-rs/crates/yi-agent-app-server/src/server.rs:5285`，`WorkspaceIndex`），而桌面侧栏的分组
（`thread/listAll`，`server.rs:2390`）就是按这个索引里的目录一一成组的。

结果：**每张卡片都会在侧栏里变成一个「新项目」**，与它真正所属的项目并列。头部越跑越乱，
而用户想要的是「看板创建的会话归拢在所属项目下，可折叠、可展开」。

实证（本机）：

- `~/.yi-agent/workspaces.json` 里多出一条
  `.../yi-agent/.worktrees/kanban/2026-10-03-board-smoke-…`。
- 卡片会话 `thread-ab348572…` 的 `meta.json`/`jsonl` 存在于**该 worktree 自己的**
  `<worktree>/.yi-agent/threads/` 下。

### 1.1 关键约束（决定实现方式）

`WorkspaceIndex` 是**一物两用**的：

1. 喂侧栏分组（`thread/listAll`，`server.rs:2390`）；
2. 定位会话存储目录——`find_thread_dir`（`server.rs:4854`）与 `store_for`（`server.rs:4863`）/`store_lookup`（`server.rs:4874`）都**扫描这个索引**来找某 `thread_id` 的 store，`thread/resume` 依赖它。

因此**不能**简单地「把卡片 worktree 从索引里移除」——那会让冷会话 resume 找不到文件。
正确做法是**保留「可发现」（喂 lookup），只改「分组」（喂侧栏）**。

## 2. 目标

- 侧栏里，看板卡片会话**归到其所属项目组**下，不再各自成为顶层项目。
- 项目组内提供一个**可折叠的「看板会话」小节**，展开即列出该项目由看板创建的会话。
- 与插件解耦：分组所需的归属事实由**宿主**记录，**插件关闭/卸载后仍然成立**。

## 3. 范围

### 做什么

1. **会话归属落盘**：`ThreadMeta` 增加可选 `board_project`（项目根绝对路径）与 `card_id`；
   调度器起卡片会话的路径写入。`ThreadSummary` wire 带出这两字段，desktop 协议同步。
2. **分组归拢**：`thread/listAll` 读某目录的会话时，凡带 `board_project` 的**归到该项目组**；
   某个目录若只产出这类会话，则不再作为顶层组输出。项目组缺失时按 `board_project` 合成。
3. **侧栏小节**：项目组内（有看板时）增加可折叠的「看板会话」小节，收纳该项目 `card_id`
   非空的会话；点击照常打开会话。

### 不做什么（本轮）

- 不写数据迁移脚本；历史会话没有字段就按普通会话显示。
- 不做跨项目的独立「看板」折叠区（此前讨论过的 C 方案）。
- 不改插件、不改看板状态机、不改 `board.*` 协议。
- 不在「看板被移除」时追删/迁移已存在的卡片会话归属。
- 不新增设置项。

## 4. 设计

### 4.1 数据：会话归属（单元 1）

`yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs::ThreadMeta` 增两个可选项：

```rust
/// 该会话由看板创建时，记下它所属的项目根（绝对路径）。None = 普通会话。
#[serde(default)]
pub board_project: Option<String>,
/// 该会话对应的看板卡 id。None = 普通会话。
#[serde(default)]
pub card_id: Option<String>,
```

两个字段都以 `#[serde(default)]` 落地，保证旧 `meta.json` 反序列化不受影响（与既有
`permission_mode`/`pin_seq` 同一手法）。

**写入点**：卡片会话唯一的创建路径是 `ServeLauncher::launch_inner`
（`server.rs:5044`）→ `start_thread_core`（`server.rs:5212`）。项目根在 `run_once`
里以 `project: &Path` 现成可得（`card_scheduler.rs:102`），`card_id` 在 `LaunchRequest`
里（`card_scheduler.rs:55`）。做法：

- `LaunchRequest` 增 `board_project: String` 字段（`run_once` 里以 `project` 填充）。
- `start_thread_core` 增两个参数 `board_project: Option<&str>`、`card_id: Option<&str>`，
  在构造 `ThreadMeta` 时直接写入。RPC 路径（`thread/start`）传 `None`；卡片路径传
  `Some(...)`。**采用此式**：创建即带，没有「先写后改」的中间态。

  （不采用「事后由 `launch_inner` 改写 `meta.json`」的备选：那会引入一个短暂的
  「会话已存在但归属未落」的窗口，且与「创建即完整」的既有约定相悖。）

**出站**：`thread_summary_json`（`server.rs:4173`）增 `board_project`、`card_id`；
`desktop/src/lib/protocol.ts` 的 `ThreadSummary` 同步增两个可选字段。

### 4.2 分组：归拢但不破 lookup（单元 2）

改 `thread/listAll`（`server.rs:2390`）的聚合逻辑：

- 遍历 `workspaces.list()` 的每个目录时，读到的每个 `ThreadMeta` 先按 `board_project` 分流：
  - `board_project = Some(p)` → 归入 `p` 对应的组（`p` 可能不等于当前扫描目录）；
  - `None` → 归入当前扫描目录组。
- 组装：先按「索引里、非纯卡片目录」的目录建组（保持既有顺序与 `exists` 语义），再把
  卡片会话并入其项目组；若某 `board_project` 尚无组，则新建一组（`exists: true`）。
- **不改变**：目录仍保留在 `WorkspaceIndex` 中（lookup 不变）；`find_thread_dir`/`store_for`
  一字不改。

> 说明：卡片 worktree 目录会继续出现在 `workspaces.json` 里，这是**有意的**——冷会话定位
> 依赖它。侧栏不再为它建「顶层组」即可。

### 4.3 侧栏：可折叠「看板会话」小节（单元 3）

在 `desktop/src/components/ThreadSidebar.tsx` 的组渲染里（`renderBoardEntry` 之后、
普通 thread 列表之前），当该 `g.workspace` 有看板**且** `g.threads` 中存在 `card_id` 非空者时，
渲染一个可折叠小节：

- 头部：`▸/▾ 看板会话 (<n>)`。折叠状态用**独立**的 per-workspace 集合，键为
  `board:${workspace}`，**不与项目组的折叠状态共用**——否则折叠项目组会连带吞掉
  该小节、而浏览器记忆的组折叠与该小节互相干扰。caret 交互复用现有组折叠的样式与键盘行为。
- 展开内容：仅 `card_id` 非空的会话，用现有 `renderThread` 渲染（标题沿用「看板 · <spec stem>」）。
- **默认折叠**（无持久化的展开记录时视为折叠）。
- 无卡片会话时不显示该小节。

卡片 worktree 的顶层组已由单元 2 消除，所以此处只是「把同一批会话收进小节」。

## 5. 边界与取舍

| 场景 | 行为 |
|---|---|
| 插件关闭 / 卸载 | 仍归组（归属是宿主写的，与插件无关） |
| app / daemon 重启后 | 仍归组（`meta.json` 落盘） |
| 看板被移除 | 会话继续留在该项目组（`board_project` 仍指向它）；不追删 |
| 历史会话（无字段） | 按普通会话显示，不被误收 |
| 卡片 worktree 被删 | 该会话不可 resume（既有行为，本设计不改变） |

## 6. 验收

1. `thread/listAll` 单测：一张卡片会话（`board_project = 项目`、`cwd = worktree`）出现在
   **项目组**下，且**不**产生 `worktree` 顶层组。
2. 单测：某目录仅含卡片会话时，不作为顶层组输出。
3. 单测：普通会话（无字段）行为与今天完全一致（分组、顺序不回归）。
4. desktop 组件测试：项目组内有卡片会话时渲染「看板会话」小节，默认折叠，展开后可见；
   无卡片会话时不渲染。
5. 协议测试：`thread/listAll` 的条目带出 `board_project`/`card_id`。
6. 回归：`cargo test -p yi-agent-app-server`、`cd desktop && npx tsc --noEmit && npx vitest run`。

**验证命令：**

```
cd yi-agent-rs && cargo test -p yi-agent-app-server
cd desktop && npx tsc --noEmit && npx vitest run
```

## 7. 涉及文件

- `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs`（`ThreadMeta` 新字段）
- `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`
  （`thread_summary_json`、`thread/listAll`、`start_thread_core`/`launch_inner`）
- `yi-agent-rs/crates/yi-agent-app-server/src/card_scheduler.rs`（`LaunchRequest.board_project`）
- `desktop/src/lib/protocol.ts`（`ThreadSummary`）
- `desktop/src/components/ThreadSidebar.tsx`（可折叠小节）

## 8. 风险

| 风险 | 处置 |
|---|---|
| 改动分组逻辑可能触碰既有分组/顺序回归 | 单测覆盖「普通会话零回归」（验收 3） |
| 卡片 worktree 仍在索引内，侧栏若遗漏过滤会重复出现 | 单测断言 «不成顶层组»（验收 1、2） |
| 旧 `meta.json` 反序列化 | 两字段 `#[serde(default)]`，旧数据回 `None` |
| 归属字段与插件状态可能不一致（卡被删/看板移除） | 明确接受：归属只表示「由谁创建」，不追插件状态 |
