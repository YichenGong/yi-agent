# 把 Superpowers 看板纳入 `superpowers_kanban` 功能开关

日期：2026-10-21
状态：待实现

## 1. 要解决的问题

看板（`plugins/superpowers-kanban`）目前是**已发布**功能：v0.1.6、v0.1.7、v0.1.8
三个 tag 的 `plugins/` 目录都包含它。但它仍然处于快速演进期，用户希望：

1. 能在 App 设置页的「实验功能」Tab 里一键关掉整个看板；
2. 关掉后**真的不占资源**：插件 daemon 停下来，不再每 60s 空转；
3. 关掉后已存在的看板会话也不显示（但不删数据）；
4. worktree 不动——那是用户自己的目录，不属于"看板功能"。

第 1 条与现有机制有一个硬冲突，见下节。

## 2. 现状：两套"开关"，语义不同

| | `superpowers_kanban`（现有） | 新 flag |
|---|---|---|
| 存储 | `preferences.json`，**每项目一份** | `flags.json`，**全局一份** |
| 谁能改 | 经 daemon 的 `switch.write` | app-server 的 `flags/set`，**不经 daemon** |
| 关掉后 | daemon 仍跑（清单 `stop_when_disabled: false`） | 停进程 |
| stable 上 | 可用 | 恒为关 |

`yi-agent-supervisors/src/supervisor.rs:190` 的注释写明为什么现在是"off 也保持存活"：

> A process that must stay reachable to be turned back on is left running:
> "off" means "stop working", not "disappear".

这个理由对新 flag **不成立**：`flags/set` 走 app-server，不经过插件 daemon，
所以停掉 daemon 之后仍然能在设置页把它打开。这正是本设计的前提。

## 3. 决策：global gate + project gate

用户的口径：

> 全局开关一定是去管理这个功能总体有没有，然后项目里面是管项目里面有没有。
> 全局没有的话，项目就不应该有。全局有的话，那项目可以打开也可以关掉。

`effective = flag_global && project_switch`

这正是 `yi-agent-supervisors/src/switch.rs:resolve_bool` 已有的形状：项目层显式
设置优先，否则继承全局层，两层都缺省为关。只有一处改动——**全局层的来源从
`preferences.json` 换成 `flags.json`**。

关掉全局 flag 再打开时，**各项目恢复各自原有的开关状态**：flag 是纯闸门，
不写 `preferences.json`，因此项目层的数据从未被触碰。

## 4. 作用点

### 4.1 注册表（`yi-agent-flags/src/registry.rs`）

新增一条：

```rust
pub const SUPERPOWERS_KANBAN_FLAG: &str = "superpowers_kanban";
```

- `default: false` — 受 `every_flag_defaults_to_off` 强制，也符合"stable 上关掉"的决定
- `expires_on`:`2027-03-31`（看板仍在演进；到期测试会失败，届时要么删 flag 要么续期）
- `owner`：`yichen`
- `summary`：「Superpowers 看板：项目级看板、卡片会话与后台值守」

### 4.2 supervisor 起停（`yi-agent-supervisors`）

`SupervisorManifest.switch_key` 从 `String` 改为 `Option<String>`：

- `Some(key)`：该键**就是 flag 名**，全局层从 `flags.json` 读取。关 → 停进程。
- `None`：不受全局 flag 控制，行为与今天一致。

`switch.rs` 增加一个只读 `flags.json` 的读取函数，**直接复用
`yi_agent_flags::store::status_at`**。依赖图不成环（`flags → store`、
`supervisors → store`，两者平行），故 `yi-agent-supervisors` 新增一条对
`yi-agent-flags` 的依赖。

**不调用 `yi_agent_flags::is_enabled`**：它内部用进程级 `build_kind()`，在测试里
恒为 `"release"`，注入永远失效（与 `lead_gate` 同一个坑）。`Supervisor` 因此新增
一个 `build_kind: String` 字段，由 `Supervisor::new` 的调用方给出：生产传
`yi_agent_flags::build_kind()`，测试直接传 `"dev"` / `"release"`。

`stop_when_disabled` 保留，但**只在 `switch_key` 为 `None` 时才有意义**：
声明了 flag 的进程，flag 关就停——这是本次要的行为。该字段的文档注释必须改写，
说明这个优先级，否则后人会以为它仍然能挡住 flag。

清单 `plugins/superpowers-kanban/supervisors/superpowers-kanban.json`：

```json
"switch_key": "superpowers_kanban",
"stop_when_disabled": true
```

> 注意：这里 `switch_key` 的语义从「preferences 键」变成了「flag 名」。两者
> 恰好同名，所以清单文件的字面量不变，但读取位置变了。必须在此文档和清单注释
> 里写清楚，否则这是最容易被误读的一处。

### 4.3 UI 隐藏

**前端 `useBoardState`**：新增 `kanbanEnabled: boolean` 入参。为 false 时：

- 不调 `listBoards`
- 不注册 2 秒轮询
- `boardPluginMissing` 保持 false（不是"插件缺失"，是"功能关闭"——两者 UI 说法不同）

**`App.tsx`**：握手时与 `flags/list` 同一次请求里取 `superpowers_kanban`
（现在已经在读 `lead_agent`，扩一项即可），传给 `useBoardState` 与 `ThreadSidebar`。

**`ThreadSidebar`**：新增 `kanbanEnabled?: boolean`（缺省 `true`）。为 false 时
不渲染「看板会话」小节，且把 `driver_ref` 非空的会话从普通分组里一并过滤——
理由与组长那条一致：只藏小节不剔行，会让这些会话出现在项目分组里。

**`SuperpowersKanbanView` / 设置里与看板相关的入口**：不渲染。

### 4.4 宿主转发拒绝（`server.rs`）

`plugin/query` 转发前，若 `plugin == "superpowers-kanban"` 且 flag 关，直接回
`plugin_unavailable`，不发 IPC。理由：daemon 已停，硬要转发会得到超时或
"连接失败"，措辞会把用户引向"插件坏了"，而真相是"功能关着"。

## 5. 不做的事

- **不删数据**：看板会话、worktree、`preferences.json` 里的项目开关一律不动。
- **不迁移** `preferences.json` 的旧键：它继续作为项目层生效。
- **不改 stable 的构建闸门**：`store.rs:75` 的 `build_kind != "dev"` 硬闸不动，
  接受 stable 上看板不可用（用户已确认这是预期）。
- **不做热加载**：flag 在装配期读取，改完需重启 app-server。

## 6. 风险

**风险 1：stable 用户升级后看板消失。** 这是本设计的直接后果（用户已确认接受）。
缓解：`flags/list` 在 stable 上是 `-32601`，用户看不到任何开关，也就看不到"为什么
没了"。**这一点必须在 CHANGELOG 里写明**。

**风险 2：`switch_key` 语义变更。** 同名字段从 preferences 键变成 flag 名。
清单文件字面量没变，所以 diff 里看不见——最容易漏。缓解：改 `SupervisorManifest`
的字段文档，明确写"这是 flag 名，从 flags.json 读"；`switch.rs` 里旧的
`resolve_bool(global, project)` 保留（项目层仍读 preferences），只在 global 形参
处换来源。

**风险 3：flag 关时 daemon 被停，`plugin/list` 仍列着它。** `plugin/list` 读的是
清单目录，文件还在，所以会列出来。缓解：`plugin/list` 增加 `enabled` 字段，
前端据此显示为"已关闭"而非"已安装未运行"。

## 7. 验收

```bash
cd yi-agent-rs
cargo test -p yi-agent-flags                 # 含 superpowers_kanban 注册断言
cargo test -p yi-agent-supervisors           # 含 flag 控制起停
cargo test -p yi-agent-app-server --lib      # 含 plugin/query 拒绝
cargo test -p yi-agent --bin yi-agent
cd ../desktop && npx vitest run && npx tsc --noEmit
```

端到端（dev 构建）：

```bash
./scripts/rebuild.sh cli
yi-agent-dev flags set superpowers_kanban on
yi-agent-dev flags status superpowers_kanban   # on
# 关掉后：ps 里 superpowers-kanban 进程消失；设置页 UI 消失
yi-agent-dev flags set superpowers_kanban off
```
