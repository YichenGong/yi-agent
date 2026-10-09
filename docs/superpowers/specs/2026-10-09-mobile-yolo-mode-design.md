# 手机端切换权限模式（YOLO）授权 + 模式跨客户端同步 — 设计

日期：2026-10-09
状态：已批准（方案 A）
分支：`fix/mobile-yolo-mode`

## 1. 背景与问题

两个用户可见缺陷：

1. **手机端新建会话后把 mode 切到 YOLO，报「权限不足」**（`-32014 insufficient_scope`）。
2. **手机上打开某会话时，电脑端已置的 YOLO 状态「概率性」没有同步过来**，看起来像 Normal。

### 1.1 根因（systematic-debugging 结论）

**问题一** —— `thread/setPermissionMode` 被硬编码进 app-server 的 admin 门禁：

- `ADMIN_METHODS`（`yi-agent-rs/crates/yi-agent-app-server/src/server.rs:2278`）含
  `thread/setPermissionMode`；门禁在 `server.rs:2291`，对 `client_scope < Scope::Admin`
  回 `-32014`。
- 关键事实：**网络客户端（直连 ws / 经中继的手机）恒为 `Scope::Control`**；新配对设备
  一律 `Control`（`pairing.rs`「决策 A」），中继桥接身份也是 `Control`
  （`docs/relay-deploy.md:276`）。**只有桌面 stdio 是 `Admin`**（`server.rs:1464/1584`）。
- 故手机发出的 `thread/setPermissionMode` 100% 撞 `-32014`。
- 设计文档**有意**如此（`docs/superpowers/specs/2026-10-02-mobile-remote-access-design.md:310`：
  「`thread/setPermissionMode`=yolo 在 `control` 下返回 `-32014`，引导去桌面端授权」；
  `relay-deploy.md:622` 重申），但**代码里不存在任何「桌面端授权 / 设备提权」入口**——
  这条路径结构性走不通。`SettingsRemoteTab.tsx:40` 也把该拒绝当成常态文案。

**问题二** —— 模式变更没有跨客户端同步通道：

- `thread/resume` / `thread/start` 的**响应不含 `permission_mode`**，客户端只能从
  `thread/listAll` 回读（`desktop/src/App.tsx:627` 注释明说）。
- 协议通知全集（`protocol.rs:184` 起）有 `thread/started`、`thread/status/updated`、
  `thread/modelChanged`，**独缺 mode 变更通知**。
- 对比：模型切换有 `thread/modelChanged` 且服务端**直接广播**
  （`server.rs:5315`，注释明写「让所有订阅该 thread 的客户端（桌面 + 手机）立刻刷新」）；
  模式切换**不做这件事**（只 `set_permission_mode` 落盘 + `session.yolo.set`）。
- 于是手机的 mode chip 只在**打开会话那一刻**读一次快照，而这个快照是异步、可失败、
  可与桌面写入竞态的：`refreshThreads` 失败返回 `null` 即保持「未知」
  （`App.tsx:376-398`）；`selectThread` 的 `inFlightResume` 守卫还会让并发打开**跳过回读**
  （`App.tsx:617`）。桌面在手机之后改 YOLO，手机上现存的那条会话**永不更新**——这正是
  「概率出现、没同步过来」。

## 2. 决策（方案 A）

把 `thread/setPermissionMode` **从 admin 门禁移出，按 `Scope::Control` 放行**，与
`thread/setModel`（`server.rs:3254`）同档。

**理由**：文档承诺的「桌面端授权」入口不存在；`thread/setModel` 已开同一先例；Control
客户端本就拥有危害更大的能力（`turn/start` 能在 YOLO 会话上执行任意工具、批准任意工具）。
手机端 `ModeChip` 的二次确认弹窗保留。

**安全影响（明示）**：任何持有配对凭据的远程客户端可对**任意已知会话**开启 YOLO（跳过
工具审批、放开 OS 沙箱到全权）。缓解：配对凭据是持有者凭证；桌面可随时 `device/revoke`；
`thread/setPermissionMode` 本就只对**已被 resume/start 进内存的活跃线程**生效
（`server.rs:3405`），不能凭空拉起冷线程。

**同步修正文档**：`mobile-remote-access-design.md`、`relay-deploy.md` 把 setPermissionMode
从「control 下被拒」清单移除，并说明改档理由。

## 3. 方案

### 3.1 问题一：重定档到 Control

- `ADMIN_METHODS`（`server.rs:2278`）去掉 `"thread/setPermissionMode"`，`[&str; 5]` → `[&str; 4]`。
- `thread/setPermissionMode` 分支（`server.rs:3379`）开头加
  `if client_scope < Scope::Control { ... insufficient_scope(Scope::Control) ... continue; }`
  （与 `thread/setModel` 同款）。
- 更新 `ADMIN_METHODS` 上方注释，说明改档与原因。

**不做**：不引入「设备能力位」、不改 `Scope` 枚举、不改其余四项 admin 方法。

### 3.2 问题二：模式跨客户端同步

**3.2.1 新通知 `thread/permissionModeChanged`**

- `protocol.rs` 新增变体 `PermissionModeChanged { thread_id: String, mode: ThreadMode }`，
  serde rename `thread/permissionModeChanged`。`mode` 复用 `crate::thread_store::ThreadMode`
  （serde lowercase，与 `ThreadSummary` 一致）。
- `thread_key()` 加该变体 → `Some(thread_id)`。
- `delivery()` 归 `Delivery::List`（**恒推**）——理由同 `ModelChanged`：mode 是会话元数据
  （列表层），而远程客户端只订阅少量暖会话，若按内容层过滤，窗口外会话的模式变更就永远
  收不到（与「未读蓝点」同一课，见 `protocol.rs` 的 `Delivery::List` 注释）。它只带 mode、
  不带正文。

**3.2.2 服务端广播点**

- `thread/setPermissionMode` 分支：落盘后 `write_notification(hub, &Notification::PermissionModeChanged { thread_id, mode })`。
- 覆盖双向：桌面开 YOLO → 手机同步；手机开 YOLO → 桌面 chip 同步。

**3.2.3 `thread/resume` / `thread/start` 响应带 `permission_mode`**

- 两个分支的 `ok_response` json 各加 `"permission_mode": mode`
  （start 用局部 `mode`；resume 用 `loaded.meta.permission_mode`）。
- 手机打开会话时从**响应**即得权威值（单次请求、原子），消除对 listAll 快照的依赖与竞态。

### 3.3 客户端（`desktop/src`）

- `protocol.ts`：`Notification` union 加
  `{ method: "thread/permissionModeChanged"; params: { thread_id: string; mode: ThreadMode } }`。
- `threadStore.ts` `applyNotification`：加分支写 `view(id).mode = n.params.mode`（与
  `thread/modelChanged` 同款，含「无 info 也不崩」）。
- `App.tsx`：
  - `selectThread` resume 成功后，从 resume **响应**读 `permission_mode` 写 `view.mode`，
    不再依赖「跳过回读」的 warm 守卫。
  - `newThread` 从 `thread/start` 响应读 `permission_mode`。
  - 保留 `modeForThread` 的 listAll 回读作为**幂等兜底**（warm 切回、不 resume 的场景）。

### 3.4 不变项 / YAGNI

- 不改 `Scope`、不新增能力位、不把手机设 Admin。
- 不改 `ADMIN_METHODS` 剩余四项（`thread/delete`、`process/kill`、`pair/create`、`device/revoke`）。
- 响应新增字段与新增通知均为**向后兼容**（旧客户端忽略未知字段/通知）。

## 4. 测试

**Rust（app-server）**

- Control scope 客户端可 `setPermissionMode`（修复前 `-32014`）；新增 Observe 被拒 `-32014`。
- `thread/permissionModeChanged` 的 wire 形状（method 名、`params.thread_id`/`params.mode`）
  + `delivery() == List` + `thread_key()` 为 thread。
- 集成（merged harness，stdio=Admin + ws=Control，同
  `merged_loop_fans_out_stdio_notifications_to_ws_client` 范式）：stdio 改 mode 后 ws 客户端
  收到 `thread/permissionModeChanged`；ws（手机）改 mode 后 stdio 侧收到。
- `thread/start`、`thread/resume` 响应含 `permission_mode`；resume 一个持久化为 yolo 的
  线程返回 `"yolo"`。
- 回归：`a_control_client_cannot_delete_a_thread` 等其余 admin 门禁不变。

**前端（vitest）**

- `threadStore`：`thread/permissionModeChanged` 写对应 view.mode；未知 thread 不崩。
- `App`：resume 响应含 `permission_mode: "yolo"` 时 chip 显示 YOLO（不依赖 listAll 回读）。
- 既有 `setPermissionMode` 参数/失败语义用例保持。

## 5. 验证命令

- `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
- `cd yi-agent-rs && cargo test -p yi-agent-app-server`
- `cd desktop && npx tsc --noEmit && npx vitest run`
- `cd yi-agent-rs && cargo fmt --all`

## 6. 发布注意

- 协议新增 1 条通知 + 2 个响应字段，均向后兼容。
- 安全姿态变化须在文档层通知：手机现在可切 YOLO。
