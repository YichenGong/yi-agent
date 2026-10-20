# 组长功能纳入功能开关 — 设计

日期：2026-10-20
状态：待批准
相关：[功能开关与 dev 版独立编译](./2026-10-20-feature-flags-design.md)

## 1. 问题

组长（lead agent）是一整套跨层能力：7 个工具、1 个 RPC 入口、记忆账本、索引视野、
后台 tick、桌面端侧栏入口与对话标识。它已经完整实现并合进 `main`，但**没有任何开关**——
只要项目里有组长，所有会话都能用。

这与「用户看到的功能都是确认没问题的」冲突：组长还在验证期，不应该在 stable 上
对所有人默认开启。

## 2. 目标与非目标

### 目标

1. 用一个开关 `lead_agent` 管住组长**全部**功能，默认关。
2. stable 上不可能打开（沿用现有硬闸，无需新机制）。
3. 关掉后：行为与「组长功能不存在」一致——工具不可见、RPC 不可用、UI 不显示。
4. 已有组长会话的数据**不删**；关掉时按普通会话显示。

### 非目标

- 不拆分开关（不做 `lead_tools_enabled` / `lead_ui_enabled` 这类细分）。
- 不做热加载：不给已装配的会话重建工具集加通道（见 §6）。
- 不迁移、不删除 `lead_memory` 落盘文件。
- 不改组长的任何业务逻辑。

## 3. 决策

| # | 决策 | 理由 |
|---|---|---|
| L1 | **单一开关** `lead_agent` | 组长是一套连贯体验。拆开只会得到「有工具没入口」或「有入口没能力」的半残状态，用户无法据此做判断 |
| L2 | 开关判定**收敛到两处**：`build_runtime_tooling` 与 `thread/lead` handler | 分散到每个工具注册点必然漏。这两处就是全部能力的总闸 |
| L3 | 判定式写成 `is_lead_thread(...) && flags::is_enabled("lead_agent")` | 开关**只能关、不能开**——它不能把一个普通会话变成组长 |
| L4 | `thread/lead` 在关掉时返回 `-32601`（method not found） | 与 `flags/*` 在 stable 上的做法一致。前端不需要判断权限，只需看 `flags/list` |
| L5 | 已有组长会话的 `role` 字段**不动** | 那是数据不是行为。删了它等于丢用户数据，且开关重新打开后无法恢复 |
| L6 | 关掉时 `lead_memory` 不注入、不写入 | 「不注入」由 L2 自然实现（工具没注册）；「不写入」因为唯一写入口就是 `LeadRememberTool` |
| L7 | 后台 tick（`lead_loop`）在开关关闭时不跑 | 否则会继续驱动一个用户看不见的组长会话，白耗 token |
| L8 | 前端靠 `flags/list` 的 `lead_agent.enabled` 决定渲染 | 与 `buildKind` 同一个信息源，不另开 RPC |
| L9 | 惰性建组长的两个入口（`thread/lead`、`thread/listAll` 的 `scan_workspace_metas`）也加门禁 | 否则**关掉开关后每次列表刷新仍会为每个空项目种一个组长**——用户看不见，但它真实存在、占 thread、重新打开开关时全部复活 |

## 4. 架构

```
                    flags.json / YI_AGENT_FLAG_LEAD_AGENT / 构建类型硬闸
                                      │
                           yi_agent_flags::is_enabled("lead_agent")
                                      │
                ┌─────────────────────┴─────────────────────┐
                │                                           │
   build_runtime_tooling()                        thread/lead handler
   （会话装配期，thread/start 与 resume 都经过）    （每次调用现读）
                │                                           │
      ┌─────────┴─────────┐                         关闭 → -32601
      │                   │
  普通会话              组长会话
  ask_lead            7 个工具全注册
  （仅当开关开）
```

### 4.1 后端判定点

`yi-agent-rs/crates/yi-agent-app-server/src/server.rs` 的 `build_runtime_tooling`
是**唯一**的工具注册点。当前结构：

```rust
if !is_lead && !is_plugin_worktree { register(AskLeadTool) }
if is_lead { register(LeadRememberTool); ... register(LeadRejectTool) }
```

改为：

```rust
let lead_enabled = yi_agent_flags::is_enabled(LEAD_AGENT_FLAG);
if lead_enabled && !is_lead && !is_plugin_worktree { register(AskLeadTool) }
if lead_enabled && is_lead { register(7 个工具) }
```

`is_lead` 的计算保持不变（落盘 `ThreadMeta.role` 是唯一权威，见
`lead_memory::is_lead_thread`）。开关只做**与**运算，不参与「谁是组长」的判定。

### 4.2 RPC 判定点

`yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs` 的 `"thread/lead"` 分支
开头加：

```rust
if !yi_agent_flags::is_enabled(LEAD_AGENT_FLAG) {
    write_response(..., err_response(id, RpcError::method_not_found("thread/lead"))).await?;
    return Ok(Flow::Continue);
}
```

放在 `resolve_thread_cwd` **之前**：关掉时不该因为参数不全而报 `invalid_params`，
那会让前端以为是自己传错了。

### 4.3 惰性建组长

组长 meta 有两个「顺手创建」的入口，都必须加门禁——否则关掉开关后它们仍在后台
造会话：

| 入口 | 位置 | 现状 |
|---|---|---|
| `thread/lead` | `rpc/thread.rs:328` | 前端补建循环显式调它 |
| `thread/listAll` | `rpc/thread.rs:2019` 的 `scan_workspace_metas` | **每个空项目目录**都会种一个组长 |

第二个最隐蔽：它跑在 `spawn_blocking` 里，与 RPC 参数无关，只与「这个目录有没有
会话」有关。不加门禁的话，用户关掉开关、重启 app，`thread/listAll` 依然会为每个
空项目建组长。

两处都加：

```rust
if yi_agent_flags::is_enabled(LEAD_AGENT_FLAG) {
    let _ = crate::lead::ensure_lead_meta(&store, path);
}
```

`scan_workspace_metas` 是纯阻塞函数（只碰磁盘），`is_enabled` 读 `flags.json`
也是一次磁盘读，不会引入新的阻塞风险。

### 4.4 后台 tick

`lead_loop.rs` 只是**纯判定函数**（`decide_wake` / `render_digest`），真正的循环
内联在主循环的 `select!` 里：`server.rs` 的 `_ = lead_ticker.tick() => { ... }`
臂（30s 一次），以及 `lead_tick_once()`。

门禁加在**臂内**，而不是 `lead_loop.rs`——那是个不 import 任何外部状态的纯函数
模块，把开关塞进去会破坏它的可测性：

```rust
_ = lead_ticker.tick() => {
    if !yi_agent_flags::is_enabled(LEAD_AGENT_FLAG) {
        continue;
    }
    // …原有逻辑
}
```

每次 tick 现读（不是启动时缓存），使开关变更有 30s 内的即时效果。

### 4.5 前端

两个改动点，都在 `App.tsx`：

1. **补建组长的循环**（`leadEnsured` 那段，`App.tsx:1463`）要加门禁：开关关闭时
   不去 `thread/lead`。否则每次启动都会对每个项目建一个组长会话——用户看不见它，
   但它真实存在且会占一个 thread。
2. **侧栏渲染**：`ThreadSidebar` 加 `leadAgentEnabled` prop，`false` 时不渲染组长
   行（`renderLeadRow`）与 `LeadAvatar`。

`ChatView` 不涉及——`LeadAvatar` 只被 `ThreadSidebar` 引用，已核实。

值来自 `flags/list`（与设置页「实验功能」Tab 同一份数据），在握手时读一次，
与 `buildKind` 同一时机，不新增 RPC。

组长的会话本身**仍然出现在列表里**（L5）：`findLeadThread` 只是决定「顶部多一行」，
普通列表里的那条与会话数据都不动——这就是「按普通会话显示」。

## 5. 开关定义

`yi-agent-rs/crates/yi-agent-flags/src/registry.rs`：

```rust
pub const LEAD_AGENT_FLAG: &str = "lead_agent";

// 在 REGISTRY 里加一项：
FlagDef {
    name: LEAD_AGENT_FLAG,
    summary: "组长（lead agent）：项目级常驻会话，负责分派与回收子任务",
    owner: "yichen",
    expires_on: "2027-01-31",
    default: false,
}
```

`expires_on` 取 3 个月后：验证期结束要么删掉这个 flag（功能转正），要么续期。

### 5.1 名字的稳定性

`lead_agent` 一旦发布就是 `flags.json` 的键，不能改名。这个约束由
`registry.rs` 的注释与 `names_are_unique_and_snake_case` 守护。

## 6. 为什么不热加载

工具集在会话装配期（`attach_delegation` → `build_runtime_tooling`）一次性装进
`ToolRegistry`，此后该会话的 `Agent` 持有的是 `Arc<Registry>` 快照。要热加载必须：

1. 给 `ThreadSession` 增加「重建工具集」的通道；
2. 处理重建时正在进行的 turn（工具句柄可能被持有）；
3. 保证重建后 `CallerContext`（`thread_id` / `project_key` 绑定）不指向旧会话。

这是三件事，且第 2 件有并发风险。收益只是「省一次切会话」，不划算。

**用户实际体验**：打开开关后，新会话立刻具备组长能力；已开着的会话切走再切回来
（触发 `thread/resume`）即可——`resume` 本来就会重建 agent 与工具集。

## 7. 边界与错误处理

| 场景 | 行为 |
|---|---|
| stable 构建，`flags.json` 里写了 `lead_agent: true` | 硬闸压成 `false`（`status_at` 第一行），组长不可用 |
| 开关关，`thread/lead` 被调用 | `-32601`，且发生在参数校验之前 |
| 开关关，但会话 `role == "lead"` 已在盘上 | 列表照常返回该 thread（`role` 字段也在），前端按普通会话渲染 |
| 开关关，`thread/listAll` 被调用 | 不种新组长；已有的组长 meta 照常列出 |
| 开关关，`ask_lead` 被模型调用 | 工具根本没注册，模型看不到它；若历史里有残留调用，按「未知工具」处理（既有行为） |
| 开关从开到关（运行中） | 已装配的会话继续拥有工具，直到下次 resume；新建会话没有。这是 L2 的已知边界 |
| 开关从关到开（运行中） | 同上：新会话立即有，老会话需 resume |
| `flags.json` 损坏 | 视为全关（既有行为），组长随之关闭 |
| 组长记忆账本已存在但开关关 | 文件原样保留，不读不写 |

## 8. 测试策略

**单元（`yi-agent-flags`）**：不需要新增——`lead_agent` 与其他 flag 走同一套
`status_at` / `write_at` 测试。

**app-server（`--lib`）**：

| 测试 | 断言 |
|---|---|
| `lead_tools_are_absent_when_the_flag_is_off` | 开关关时，组长会话的 registry 里没有 `lead_remember` |
| `lead_tools_are_present_when_the_flag_is_on` | 开关开时，7 个工具都在 |
| `a_normal_thread_gets_ask_lead_only_when_the_flag_is_on` | 普通会话同理 |
| `the_flag_cannot_turn_a_normal_thread_into_a_lead` | 开关开 + 普通会话 → 仍然没有 `lead_remember` |
| `thread_lead_is_method_not_found_when_the_flag_is_off` | `-32601` |
| `list_all_does_not_seed_a_lead_when_the_flag_is_off` | 空项目目录 `listAll` 后仍无 `role == "lead"` 的 thread |
| `list_all_still_seeds_a_lead_when_the_flag_is_on` | 既有行为保持 |
| `thread_lead_still_works_when_the_flag_is_on` | 既有测试保持通过 |

测试注入方式：`build_runtime_tooling`、`thread/lead` handler、`scan_workspace_metas`
都改为接受一个 `lead_enabled: bool` 参数（调用点传 `is_enabled(...)`），测试直接
传值，不改进程环境变量——与 `handle_flags_request_at` 同一手法，避免并行测试之间
因共享 `flags.json` 或环境变量而竞态。

**desktop（vitest）**：

| 测试 | 断言 |
|---|---|
| `hides the lead row when the flag is off` | 侧栏无组长行 |
| `shows the lead row when the flag is on` | 侧栏有组长行 |
| `keeps an existing lead thread visible but unmarked when off` | 会话仍在列表里，无 `LeadAvatar` |

**端到端（可选，手动）**：

```bash
YI_AGENT_HOME=<实例> yi-agent flags set lead_agent on
# 新建会话 → 侧栏出现组长行；问组长有回应
YI_AGENT_HOME=<实例> yi-agent flags set lead_agent off
# 切走再切回 → 侧栏组长标识消失
```

## 9. 影响面

| 文件 | 改动 |
|---|---|
| `crates/yi-agent-flags/src/registry.rs` | 加 `LEAD_AGENT_FLAG` 常量与注册项 |
| `crates/yi-agent-app-server/src/server.rs` | `build_runtime_tooling` 加开关门禁 |
| `crates/yi-agent-app-server/src/rpc/thread.rs` | `thread/lead` 与 `scan_workspace_metas` 两处门禁 |
| `crates/yi-agent-app-server/src/server.rs` 的 `lead_ticker.tick()` 臂 | 开关关时不驱动 |
| `crates/yi-agent-app-server/Cargo.toml` | 已有 `yi-agent-flags` 依赖，无需改 |
| `desktop/src/App.tsx` | 读 `flags/list` 存 `leadAgentEnabled`；`leadEnsured` 补建循环加门禁；透传给侧栏 |
| `desktop/src/components/ThreadSidebar.tsx` | 加 `leadAgentEnabled`，关时隐藏组长行与 `LeadAvatar` |
| `docs/project-management/yi-agent-flags.md` | 加一条注册项说明 |
| `docs/project-management/*`（组长相关模块） | 标注「受 `lead_agent` 开关控制」 |

## 10. 判定点总表

| # | 位置 | 关掉后的行为 |
|---|---|---|
| 1 | `server.rs` `build_runtime_tooling` | 7 个工具都不注册 |
| 2 | `rpc/thread.rs` `thread/lead` | `-32601` |
| 3 | `rpc/thread.rs` `scan_workspace_metas` | 不种组长 |
| 4 | `server.rs` `lead_ticker.tick()` 臂 | 不驱动 |
| 5 | `App.tsx` `leadEnsured` 补建循环 | 不发 `thread/lead` |
| 6 | `ThreadSidebar` | 不渲染组长行与 `LeadAvatar` |

1-4 是硬闸（后端），5-6 是呈现层。即使 5-6 有 bug，1-4 仍保证能力不可用。

## 11. 未决问题

无。三个前置决策（单开关、保留数据、不迁移）已确认。
