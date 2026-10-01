# 每会话独立 root + 子任务容量可配

**目标：** 让桌面端（Tauri GUI + `yi-agent app-server` sidecar）的**每个会话**都有
自己的 subagent 配额，而不是同一工作目录下的所有会话共享一份；并把子任务并发容量
从硬编码常量接成配置项、把默认值提高。

**状态：** 已设计，待实现。

**相关文档：**
`docs/superpowers/specs/2026-08-09-runtime-scheduling-design.md`（资源调度与容量的原始设计）、
`docs/superpowers/specs/2026-09-30-desktop-subagent-delegation-design.md`（桌面端委派接线，§2.3 引入 per-cwd 缓存）、
`docs/superpowers/specs/2026-10-01-subagent-observation-design.md`（按会话展示子 agent，§2.4 记录了共享 root 约束）、
`docs/project-management/subagent-runtime.md`。

---

## 1. 问题

### 1.1 现象（用户报告）

桌面 app 上「总共只能起 4 个 subagent」，而不是每个会话至少能起 4 个。

### 1.2 根因（已核实）

**配额本身是按「每个 agent 任务」算的，不是全局：**

- `MAX_DIRECT_CHILDREN = 4`（`yi-agent-core/src/subagent/supervisor.rs:28`）；
- 判定在 `spawn_with_objective`（同文件 `:1100-1114`）：统计 `children_of(&parent_id)` 中
  **未终止**的直接子任务，达到 4 即 `SpawnError::DirectChildLimitReached`
  （错误串 `an agent may have at most four direct children`，`:41`）。

**但桌面端把同一工作目录下的所有会话接到了同一个 root 任务上：**

- app-server 的 runtime 缓存按 **canonical cwd** 为键，且有测试把这个行为钉死：
  `ProjectRuntimes`（`yi-agent-app-server/src/server.rs:72`）、
  `attach_cwd_runtime`（`:462`）、
  `two_threads_in_one_cwd_share_one_attached_runtime`（`:3143`，断言两个 thread 拿到
  同一个 `Arc`）；
- 每个 thread 注册的委派工具绑定的是**同一个 root 三元组**
  （`build_runtime_tooling` 取 `binding.current()?.task_id`，`:542` 调
  `register_attached_root_tools_in_thread`），`spawn_agent` 因此总是把
  `parent_task_id` 指向那个共享 root；
- `thread/start` 缺省 cwd 回退到 `cfg.workdir`，而桌面端 sidecar 的 cwd 是 `$HOME`
  （`desktop/src-tauri/src/bridge.rs:93-96`），所以「新建会话」默认全部落在同一目录。
  只有切到**另一个目录**才会拿到另一份额度。

**同一根因还带来三个反直觉后果：**

1. `wait_agent(mode=all)` 会把**别的会话**的子任务一并等进去（同一个 root 的 children）；
2. `inspect_agent` / `cancel_agent` 的授权判据是「目标在我的子树内」
   （`is_descendant_of`，`supervisor.rs:420`；`authorize_child_access`，
   `yi-agent-store/src/runtime.rs:2837`），共享 root 下**跨会话也成立**，即会话 A 能操作
   会话 B 的子 agent；
3. 断连回收、孤儿清扫、名额释放都以 root 为单位，会话之间互相牵连。

**并非配置问题：** 设计文档写的是「每 agent」，与实现意图一致
（`2026-08-09-runtime-scheduling-design.md:64` `max_direct_children_per_agent = 4`；
`2026-08-09-subagent-architecture-design.md:169` "maximum four direct children per agent"）。
偏差出在 `2026-09-30-desktop-subagent-delegation-design.md` §2.3 选择 per-cwd 缓存时，
只考虑了「app-server 是长驻多 cwd 进程」，未把「多会话共享 root 会共享子任务名额」纳入考量。
`2026-10-01-subagent-observation-design.md:169-183` 进一步把该约束记录在案：`thread_id`
标记**只用于展示**，明确「不改变任何准入/调度/审核语义」。

### 1.3 容量现状（已核实）

`ResourceCoordinator::default()`（`scheduler.rs:111-131`）写死以下容量：

| 资源键 | 值 | 是否真的被申请 | 位置 |
|---|---|---|---|
| `resident:global` | **16** | **是**（唯一的子任务并发闸门） | 定义 `scheduler.rs:134`；申请 `runtime.rs:1529` |
| `llm:{profile}` | 8 | 是（同一 API key 的并发模型请求） | `scheduler.rs:135-136`、`180-189`；申请 `runtime.rs:3802` |
| `llm-coordination:{profile}` | 1 | 是（协调预留） | 同上 |
| `coding:global` | ~~6~~ | **否 —— 惰性，从未被申请** | 仅定义于 `scheduler.rs:118` |
| `build:host` | ~~2~~ | **否 —— 惰性，从未被申请** | 仅定义于 `scheduler.rs:119` |
| 队列容量 | 64 | 是 | `scheduler.rs:128` |

补充事实：

- **root 不占常驻名额**，只有子任务占（`runtime.rs:1643`
  `is_subagent.then_some("resident:global")`）；
- `set_capacity` 已存在（`scheduler.rs:142`），配置 schema 里也早有
  `max_resident_subagents` 字段（`2026-08-09-runtime-scheduling-design.md:64`），
  所以这是**接线**工作，不是新造机制；
- `ResourceCoordinator::new()` 在生产代码里只有一处调用点
  （`yi-agent-store/src/runtime.rs:336`，`RuntimeCoordinator::open`）。

---

## 2. 目标与非目标

### 2.1 目标

1. **G1**：同一工作目录下的每个会话拥有**独立的 application root 任务**，于是各自有
   独立的 4 个直接子任务名额、独立的子树（`wait`/`inspect`/`cancel` 不再跨会话）、
   独立的 detach/回收生命周期。
2. **G2**：**继续共享一个 daemon**：同一项目仍只有一个 SQLite、一个调度器、一个
   workspace service。
3. **G3**：把 `resident:global` 接成可配置项，默认值从 16 提到 **64**。
4. **G4**：行为保持诚实——记录已发现的文档/实现偏差，不让下一个人再被误导。

### 2.2 非目标

1. **不**给每个会话起独立 daemon（用户已确认「共享 daemon」）。
2. **不**逐个会话隔离 `llm:{profile}`：它顶着上游并发限流，保持 8（含 1 协调预留）。
   被它挡住只是排队，不丢任务。
3. **不**把 `coding:global` / `build:host` 接成真的限制（见 §7 偏差记录）。
4. **不**改动 `MAX_DIRECT_CHILDREN = 4` 本身。
5. **不**改前端：`thread_id` 分组展示（`2026-10-01-subagent-observation-design.md`）已存在
   且继续正确。

---

## 3. 设计

### 3.1 G1 / G2：每会话独立 root，共享 daemon

**可行性（已核实）。** daemon 完全支持同一项目托管多个 root：
`application_root_attachments` 以 `idempotency_key` 为主键
（`repository.rs:4634`），`RuntimeCoordinator.supervisors` 按 `RootSessionId` 分表
（`runtime.rs:161`）。

**改动点：把「一个 cwd 一个 root」拆成「一个 cwd 一个 daemon/binding」+「一个会话一个 root」。**

1. **`ProjectRuntimes` 缓存语义不变**（仍按 `project_key(cwd)` 缓存，`server.rs:72`、
   `:86`、`:462`）：它代表「该项目的 daemon 与启动材料」，**继续共享**，这是 G2。
2. **新增按会话的 root 句柄。** 在 `yi-agent-subagent` 引入 `ThreadRoot`，持有：
   - 共享的 `Arc<RuntimeBinding>`（因此继承 daemon 自愈/替换能力）；
   - 该会话自己的 `AttachedRoot` 三元组（`session_id` / `task_id` / `capability`）；
   - 创建它的 `RuntimeConfig`（供 repair 后重新 attach）。
3. **attach 键改为按会话。** 现在的键是 `project:<pid>:<workdir>:<uuid>`
   （`attach.rs:145`，每次都新 UUID ⇒ 每次调用都新建 root）。改为
   **`thread:<thread_id>`**：稳定、跨会话唯一、重试幂等；daemon 重启后重连同一键会复用
   同一 root（`runtime.rs:723-761` 的既有复用路径）。
4. **工具绑定改为绑定 `ThreadRoot`。** 现在
   `register_application_subagent_tools_in_thread(binding, controller, thread_id)`
   （`lib.rs:1281`）把 `spawn_agent` 的 `parent_task_id` 取自 binding 的 root。
   改为取 `ThreadRoot.task_id`。`thread_id` 标记（`SpawnApplicationChild { thread_id }`）
   **保留**——它是给观察面用的附加信息，且现在是「每会话一个 root」之后的自然冗余，
   留着不冲突。
5. **激活（activate）仍在该会话首个 turn**（`server.rs:2471-2477` 的
   `binding.activate(&objective)`），但改为激活**该会话自己的 root**；objective 仍是
   首轮 prompt。
6. **detach 改为按会话，再按 cwd 收尾。** 现在 `detach_unused_runtimes`
   （`server.rs:491`）按「该 cwd 还有没有活着的 thread」决定是否 detach 那一个 root。
   改为：删除某 thread 时 detach **它的** root 并回收其 worktree；当该 cwd 已无任何
   thread 时，才断开整个 binding（daemon 归属不变）。
7. **孤儿清扫语义。** 必须点明一个正面后果：现在「会话结束」就等于「它的 root 结束」，
   于是该会话的未完成子任务会被既有清扫真正回收；今天共享 root 下反而**收不掉**
   （root 还活着）。这条需要一个 store 层用例钉死（§5）。

**不变量（实现必须守住）：**

- **project 归属。** 每个 root 都 `project_workspace_matches(requested_workspace, recorded)`
  校验（`runtime.rs:727-736`），沿用即可，不新增规则。
- **workspace service 按 root session 注册**：`application_root_workspace_services`
  以 `root_session_id` 为键（`runtime.rs:747-750`），多 root 天然各自一项。
- **同一会话重复 attach 幂等**：同 `thread:<id>` 键返回同一 root。

### 3.2 G3：`resident:global` 可配，默认 64

链路：**配置 → daemon 的 coordinator 容量**。

1. `ResourceCoordinator::default()` 的 `resident:global` 改为取
   `DEFAULT_GLOBAL_RESIDENT_SUBAGENTS`，并把该常量从 16 提到 **64**
   （`scheduler.rs:134`；`:114-117` 改为用常量而非字面量）。
2. 增加构造入口 `ResourceCoordinator::with_resident_capacity(units: u16)`，
   由 `RuntimeCoordinator::open`（`runtime.rs:336`）从该 daemon 的配置取用。
   配置落点：`RuntimeConfig` 增加 `max_resident_subagents: u16`
   （`yi-agent-runtime/src/config.rs:13`），env 覆盖名 `YI_AGENT_MAX_RESIDENT_SUBAGENTS`，
   缺省 **64**，与既有 `YI_AGENT_MAX_TURNS` 等 env 的读取方式一致。
   `set_capacity`（`scheduler.rs:142`）已能改容量，新入口只是给它一个显式的、带默认值的
   构造语义，不得绕过 cursor 恢复（`runtime.rs:337-346`）。
3. `llm:{profile}`（8 + 1 预留）**不动**（§2.2 第 2 条）。

**接线验收判据（避免又变成一个没人读的常量）：** 必须有一个测试证明
「配置值真的流到了 coordinator 的 `capacity("resident:global")`」，
而不只是断言常量等于 64（§5）。

### 3.3 G4：记录偏差

在 `docs/project-management/subagent-runtime.md` 与该 spec 中记一条已知偏差：
设计文档声明的 `max_coding_agents = 6` 与 `max_host_build_jobs = 2`
（`2026-08-09-runtime-scheduling-design.md:64-70`）**当前未被强制**——
`coding:global` / `build:host` 只有容量定义、没有申请方（§1.3）。
本期**不修**，只记录，避免下一个人据文档推断行为。

---

## 4. 数据流与生命周期

```text
thread/start(cwd)
  └─ attach_cwd_runtime(project_key(cwd))        ← 仍按 cwd 缓存（共享 daemon）
  └─ ThreadRoot::attach(key="thread:<thread_id>", workspace=project)
        └─ IpcRequest::AttachApplicationRoot { idempotency_key, workspace }
              └─ daemon: 新建或复用该键对应的 root（独立 session/task/capability）
  └─ 该 thread 的委派工具绑定到 ThreadRoot

首个 turn
  └─ ThreadRoot.activate(objective = 首轮 prompt)

spawn_agent（在该会话内）
  └─ SpawnApplicationChild { parent_task_id = 本会话 root, thread_id = 本会话 }
        └─ MAX_DIRECT_CHILDREN=4 按「本会话 root」计数（不再是全目录合计）

thread/delete
  └─ ThreadRoot.detach()  → 回收该会话 worktree（root 死亡 ⇒ 其未完成子任务可被清扫）
  └─ 若该 cwd 已无 thread → 断开 binding（daemon 归属）
```

---

## 5. 验证策略

每层都要可跑的判据，不靠肉眼。

**A. 核心容量（`yi-agent-core`）**

- `ResourceCoordinator` 缺省 `capacity("resident:global") == DEFAULT_GLOBAL_RESIDENT_SUBAGENTS`，
  且常量值为 64。
- 新构造入口按参数生效（`set_capacity` 已有等价能力，新增入口不得绕过）。

**B. 接线（`yi-agent-store`）**

- **决定性用例**：以自定义 `max_resident_subagents` 建 `RuntimeCoordinator::open`，
  断言 `capacity("resident:global")` 等于该值（证明配置真的流通，而不是常量恰好相等）。
- **多 root 共存**：同一 workspace、两个不同 `idempotency_key` 各 attach 一个 root，
  两个 root 各自可 spawn 到 4 个直接子任务（合计 8），互不占用对方名额；第 5 个被拒且
  错误串仍是 `at most four direct children`。
- **会话结束即回收**：detach 会话 A 的 root 后，A 的未完成子任务可被孤儿清扫回收，
  而 B 的 root 与其子任务不受影响。

**C. 应用层（`yi-agent-app-server`）**

- 现有 `two_threads_in_one_cwd_share_one_attached_runtime`（`server.rs:3143`）**语义修订**：
  两个 thread 仍共享**一个 binding/daemon**，但**必须拿到不同的 root task id**。
  该测试是本次改动的核心判据（回退即失败）。
- 两个 thread 的 `spawn_agent` 工具携带不同的 `parent_task_id`。
- 跨会话操作被拒：会话 B 对会话 A 子任务调 `inspect_agent`/`cancel_agent` 应失败
  （此前因共享 root 会成功）。
- 降级不变：attach 失败仍只记 trace、保留原 agent（`server.rs:133-144` 语义不动）。

**D. 端到端**

- `cargo test -p yi-agent-store --test runtime_ipc`、`cargo test -p yi-agent-app-server`。
- `cd desktop && npx tsc --noEmit && npm test`（预期零前端改动）。

**已知环境限制（与改动无关，但会影响本机跑测试）：** 本机 socket 路径超过
`MAX_SOCKET_PATH_BYTES = 103`（`ipc.rs:860`），部分 app-server/store 集成测试在本机
`WORKSPACE` 路径下无法运行（`docs/bug-list.md` 有同源记载）。这些测试需在短路径环境
（或把临时目录置于短路径）下验证，**不得**把「跑不起来」当成「通过」。

---

## 6. 兼容性与迁移

- **IPC 协议不变**：`AttachApplicationRoot` / `SpawnApplicationChild` 等请求形状不动
  （`thread_id` 字段保持可选，旧客户端仍可省略）。
- **TUI 行为不变**：TUI 一进程一项目，其 `RuntimeBinding::fixed`/`managed` 路径与
  单 root 语义不受影响（`binding.rs:46-82`）。
- **旧数据**：`application_root_attachments` 里的旧行（`project:<pid>:...` 键）按既有
  孤儿清扫处理，不需要迁移；新键 `thread:<id>` 与之并存。
- **桌面端已有对话**：升级后新开的会话各自拿到独立 root；已存在的 warm thread 在下次
  attach 时按新键建自己的 root。

---

## 7. 已知偏差记录（G4）

| 项 | 文档声明 | 实际 | 处置 |
|---|---|---|---|
| `max_coding_agents` | 6（`runtime-scheduling-design.md:64`） | `coding:global` 容量已定义但无人申请，未强制 | 本期不修，记录 |
| `max_host_build_jobs` | 2（同上） | `build:host` 同上，未强制 | 本期不修，记录 |
| `max_direct_children_per_agent` | 4（同上） | 实现一致（`supervisor.rs:28`），但桌面端因共享 root 表现为「每目录合计 4」 | 本 spec 修复 |

---

## 8. 风险

1. **每会话一个 root ⇒ root 数量上升。** root 不占常驻名额（§1.3），但每个 root 会建
   supervisor 与（coding 时）worktree 租约。缓解：root 仍**原地运行、不建 worktree**
   （`runtime.rs:715-716` `root_mode = ReadOnly`），成本主要是一条 supervisor 记录。
2. **回收时机变了。** 会话结束会真的回收其子树（这是期望行为），但依赖 detach 路径正确。
   缓解：§5 的 B-3 与 C 用例钉死「A 回收、B 不受影响」。
3. **配置接线的默认值变化（16 → 64）会放大并发。** 上游限流由 `llm:{profile}=8` 兜住，
   超出部分排队而非失败。缓解：保留该闸门不动；如出现 provider 429，属 provider 侧容量
   问题，另行处理。
