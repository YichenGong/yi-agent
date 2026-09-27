# Desktop App:按线程的运行时 YOLO 模式

**目标:** 在 desktop app 的输入框旁边提供一个模式 chip,让用户可以对**当前线程**即时
开启/关闭 YOLO 模式。开启后行为与 CLI `--yolo` **完全一致**——跳过审批提示,并把 OS
沙箱放开为 `DangerFullAccess`;黑名单命令仍然硬拒。

**技术栈:** Rust(core / tools / runtime / app-server)+ Tauri 2 + React/TS + Vite +
Tailwind。

---

## 背景:现状与本设计的必要性

"yolo" 在这套代码里是**两层**独立机制,要做到与 CLI 一致,两层都要能运行时翻转:

1. **权限层** —— `PermissionChecker.yolo: bool`(`permission.rs:86`)。yolo 时跳过
   白名单(miss 时不再弹审批),但黑名单仍然硬拒(`permission.rs:118-119`)。
   构造后不可变,没有 setter。
2. **沙箱层** —— `SandboxPolicy.mode: SandboxMode`(`sandbox.rs:20-24`),默认
   `WorkspaceWrite`。执行时由 `SandboxPolicy::command()` 决定用 OS 沙箱包装器
   (`sandbox-exec` / `bwrap`)还是裸 `sh -c`(`sandbox.rs:61-68`)。

阻碍运行时切换的现状:

- app-server 在 `bootstrap_agent` 时把 `yolo`(`cfg.yolo`)一次性烧进 `PermissionChecker`,
  且 `server.rs:63-75` 的 factory 直接**丢弃**了 `built.permission` 句柄。
- Agent 被 move 进 driver task(`server.rs:429-442`),主循环无法再访问它。
- 沙箱在 bootstrap 时按值克隆进 `BashTool`(`bash.rs:22`)和 `ProcessManager`
  (`manager.rs:229`),**两者各持一份独立 policy**(`bootstrap.rs:163-177`),没有共享。
- 协议层没有任何方法可以设置 yolo,`thread/start` 也不接收该参数。
- 前端完全没有 yolo / permission UI,只有交互式 `ApprovalDialog`。

CLI 侧的 yolo 语义(作为对齐基准):

```rust
// config.rs:271-288
let yolo = overrides.yolo || overrides.skip_permissions || env YI_AGENT_YOLO == "true";
let sandbox = match overrides.sandbox {
    Some(mode) => mode,                                   // 显式 pin 优先
    None => match env YI_AGENT_SANDBOX {
        Ok(value) => parse(value),                        // env 次之
        Err(_) if overrides.yolo => DangerFullAccess,     // 仅无显式/无 env 时提权
        Err(_) => SandboxMode::default(),                 // 否则默认 WorkspaceWrite
    },
};
```

即:**显式 sandbox > env sandbox > (yolo ? DangerFullAccess : 默认)**。注意 yolo
**不会**覆盖显式或 env 指定的沙箱。

---

## 关键决策(来自 brainstorming)

| 决策点 | 结论 |
|---|---|
| 作用范围 | **按线程**(每个会话各自记住自己的模式) |
| 持久化 | **持久化到线程元数据**,重开 app 后恢复 |
| UI 形态 | 输入框底栏的**模式 chip** + 下拉;Normal 态低调,YOLO 态红色常显 |
| 开启确认 | 开启 YOLO 时弹**一次性确认框**;关回 Normal 无需确认 |
| yolo 语义 | **与 CLI 一致**:跳审批 + 沙箱放开为 `DangerFullAccess`,黑名单仍拒 |
| 核心机制 | **方案 A:共享原子标志 + 单点 setter**(不做 agent 重建) |

---

## 设计概览

单一事实来源:一个共享的 `Arc<AtomicBool>` 开关,permission 和 sandbox 读**同一个**
开关,无需维护两份互相同步的标志。切换时只需 `set(true/false)` 一处,即时生效。

```
                       ┌──────────────────────────┐
                       │  YoloSwitch(Arc<AtomicBool>) │  ← 单点:permission + sandbox 共享
                       └───────────┬──────────────┘
              ┌────────────────────┴─────────────────────┐
    PermissionChecker             SandboxController(base, promotable, switch)
      check() 读 switch              effective() = switch && promotable ? Danger : base
                                       ├─ BashTool.sandbox
                                       └─ ProcessManager.sandbox
```

- bootstrap 时创建**一份** `YoloSwitch`,同时交给 permission checker 和沙箱 controller。
- `bootstrap_agent` 通过 `AgentBootstrap` 把该 handle 暴露给 app-server。
- app-server 存入 `ThreadSession`;RPC `thread/setPermissionMode` 调 `handle.set_yolo()`。
- 开关在**每次**工具调用 / 每次 bash spawn 时才读,故当前 turn 内后续调用立即按新模式走。

---

## 第 1 部分:核心——共享 yolo 开关(tools + core)

### 1.1 `YoloSwitch`(放在 `yi-agent-tools`,底层 crate,core 已依赖)

```rust
#[derive(Clone, Debug)]
pub struct YoloSwitch(Arc<AtomicBool>);

impl YoloSwitch {
    pub fn new(on: bool) -> Self;
    pub fn get(&self) -> bool;
    pub fn set(&self, on: bool);
}
```

放在 tools 而非 core:core 依赖 tools,而沙箱在 tools、权限在 core,开关类型必须能被
两者引用。

### 1.2 `PermissionChecker`(`permission.rs:84-90`)

- 字段 `yolo: bool` → `yolo: YoloSwitch`。
- `check()`(`:108-134`)内 `if self.yolo { ... }` → `if self.yolo.get() { ... }`。
- 新增 `is_yolo(&self) -> bool`。
- 语义不变:yolo 只绕过白名单,**黑名单仍硬拒**(`:118-119`,由 `:136-155` 保证)。

### 1.3 `SandboxPolicy` / `SandboxController`(`sandbox.rs:20-70`)

```rust
#[derive(Clone, Debug)]
pub struct SandboxController {
    switch: YoloSwitch,   // 与 permission 共享
    base: SandboxMode,    // 非 yolo 时的模式(= cfg.sandbox)
    promotable: bool,     // 显式/env 沙箱未设置时为 true
}

impl SandboxController {
    pub fn effective(&self) -> SandboxMode {
        if self.switch.get() && self.promotable {
            SandboxMode::DangerFullAccess
        } else {
            self.base
        }
    }
}
```

- `SandboxPolicy { mode, writable_roots }` → `{ controller, writable_roots }`。
- `mode()` / `command()` 改读 `self.controller.effective()`。
- `allows_writes()` 保持按 `base` 计算(默认 `WorkspaceWrite` → write/edit 始终注册);
  yolo 只往「更放开」方向走,不会撤销工具面。
- `platform_command(mode, ...)` 的两个调用点改为传入 `effective()`,其余逻辑不动。

### 1.4 CLI 优先级保真(`config.rs`)

为了「关闭 yolo 后能正确回到 base」且不静默提权,`RuntimeConfig` 新增一个派生字段:

```rust
/// yolo 是否允许把沙箱提权到 DangerFullAccess。显式或 env 指定沙箱时为 false。
pub sandbox_promotable: bool,
```

在 `config.rs:271-288` 解析沙箱的同一段赋值:

```rust
sandbox_promotable = explicit_sandbox.is_none() && env_sandbox.is_none();
```

`base` 语义保持不变(仍是 `sandbox`)。CLI/TUI/headless 老路径不切换,构造 controller
时 `switch = new(cfg.yolo)`、`base = cfg.sandbox`,行为与现在逐字节一致,零回归。

### 1.5 bootstrap 里共享一份(`bootstrap.rs:142-185`)

`build_tool_setup_in` 建一个 `YoloSwitch`,据此构造**一个** `SandboxController`,把它的
clone 同时传给:
- `register_builtin_tools_with_sandbox` → `BashTool`(新增接受 controller 的重载,
  保留旧签名给 `main.rs` / `subagent_runtime.rs` 等无切换需求的调用点);
- `ProcessManager::with_sandbox`(`bootstrap.rs:170-178`)。

`bootstrap_agent` 收到的 `mode: PermissionMode` 仍决定 `switch` 初值
(`Interactive → cfg.yolo`,`AutoAllow → true`),并把 controller 传入 `SandboxPolicy`。

---

## 第 2 部分:协议层 + 持久化 + app-server

### 2.1 持久化字段(`thread_store.rs:16-25`)

```rust
/// 线程的自主权模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThreadMode {
    #[default]
    Normal,
    Yolo,
}
```

`ThreadMeta` 追加:

```rust
#[serde(default)]
pub permission_mode: ThreadMode,
```

`#[serde(default)]` 保证旧 `.meta.json` 反序列化不失败(对齐 `TurnUsage` 先例,
`thread_store.rs:33-38`)。**不复用** bootstrap 的 `PermissionMode{Interactive,AutoAllow}`
——语义不同,另起 `ThreadMode`。同步更新 `rebuild_meta`(`:371-386`)与测试 helper
(`:399-408`)。

### 2.2 ThreadStore 写入(`thread_store.rs:234-241`)

仿 `rename` 增加 `set_permission_mode`,走同一把 `update_meta` 锁,避免与并发 rename
互相覆盖。`load` 读入 `LoadedThread` 供 resume 使用。

### 2.3 bootstrap 返回句柄

- `AgentBootstrap`(`bootstrap.rs:202-212`)新增 `yolo: YoloSwitch`(共享 Arc 的 clone)。
- `BuiltAgent`(`server.rs:39-45`)新增 `yolo: YoloSwitch` 字段。

### 2.4 ThreadSession 挂句柄(`session.rs:15-28`)

新增 `yolo: YoloSwitch`,在 `thread/start`(`server.rs:411-422`)与 `thread/resume`
(`:561-572`)插入。

### 2.5 factory 按线程设模式

闭包签名 `Fn(Option<Session>, &Path)` → `Fn(Option<Session>, &Path, ThreadMode)`。
闭包内:

```rust
thread_cfg.workdir = cwd.to_path_buf();
thread_cfg.yolo = (mode == ThreadMode::Yolo);
```

沙箱不必单独设置——`promotable` 逻辑在 controller 内统一处理,resume 一个 yolo 线程即
自动得到 `DangerFullAccess`。`thread/start` 建新线程时 mode 恒为 `Normal`(由 chip 翻转)。

### 2.6 新 RPC `thread/setPermissionMode`(插在 `thread/rename` 附近,`server.rs:639`)

- 入参 `{ threadId, mode: "normal"|"yolo" }`;`require_thread_id`(`server.rs:1207-1224`)
  解析 threadId;mode 非法 → `invalid_params`。
- 线程在内存中:`session.yolo.set(on)`,**即时生效**(开关在每次工具调用 / bash spawn 时读)。
- 持久化:`store.set_permission_mode(...)`,best-effort。
- 线程不存在 → `unknown_thread`。UI chip 只作用于当前打开的线程(必然在内存),冷线程不处理。
- 返回 `{}`。

### 2.7 wire 暴露

- `thread/list`(`:294-306`)与 `thread/listAll`(`:325-356`)映射各加 `permission_mode`。
- TS `ThreadSummary`(`protocol.ts:99-107`)加 `permission_mode: "normal" | "yolo"`。
- 前端新增 `threadSetPermissionModeParams(threadId, mode)` helper(对齐
  `threadStart.ts:1-4` 风格)。

**不做** `thread/start` 收 mode 参数——建线程后由 chip 翻转即可(YAGNI)。

---

## 第 3 部分:前端交互

### 3.1 组件 `desktop/src/components/ModeChip.tsx`

放在 `MessageInput.tsx` 底栏(`:29-55`)Send 按钮左侧。

- **Normal 态**:低调灰色 chip(盾牌/gauge 图标 + "Auto"),不抢视线。
- **YOLO 态**:chip 变**红色 + "YOLO"**,常驻可见,持续提示当前处于无审批状态。
- 点击展开下拉:`Normal` / `YOLO` 两项,当前项打勾。
- 选 `YOLO` → 先弹**确认框**;选 `Normal` → 立即切换,无确认。
- 确认框仿 `ApprovalDialog.tsx` 形态(overlay + 主 UI `inert`),文案说明:跳过审批、
  沙箱放开为完全访问、**黑名单命令仍拒绝**。

### 3.2 状态流(`App.tsx`)

- 维护「当前线程 mode」,来源:`thread/started` 通知 / `thread/listAll` / resume 响应 /
  set 成功后的本地更新。
- 将 `mode` + `onSetMode` 传给 `MessageInput` → `ModeChip`。
- `onSetMode` 调 `client.request("thread/setPermissionMode", {threadId, mode})`,**成功才**
  更新本地状态。
- 切换线程时 chip 自动反映该线程持久化的 mode。

---

## 数据流

```
用户点 chip → 下拉选 YOLO → 确认框 → 确认
  → onSetMode  →  RPC thread/setPermissionMode {threadId, "yolo"}
    → app-server: threads[threadId].yolo.set(true)   // 即时
    → ThreadStore.set_permission_mode(threadId, Yolo) // 持久化
    → 返回 {}  → 前端把本地 mode 设为 "yolo"(chip 变红)
```

下一次工具调用:`PermissionChecker.check()` 读到 switch=true → 跳过审批(黑名单仍拒);
下一次 bash spawn:`SandboxController.effective()` 返回 `DangerFullAccess` → 裸 `sh -c`。

---

## 错误处理

| 情况 | 处理 |
|---|---|
| RPC 失败 | 前端内联报错,**保持原状态不变** |
| 确认框取消 | 不发起任何调用 |
| mode 值非法 | 后端 `invalid_params` |
| threadId 不存在 | 后端 `unknown_thread` |
| 旧 `.meta.json` 无字段 | `#[serde(default)]` → `Normal` |
| 正在运行的子进程 | 切换不回溯影响已 spawn 的进程(合理) |

---

## 测试计划

**core / tools**
- `permission.rs`:运行时翻转(开→放行、关→需确认);翻转后黑名单仍拒。
- `sandbox.rs`:`effective()` 尊重 `promotable`;翻转后 `command()` 在 wrapper 与裸 `sh`
  之间切换。
- `bash.rs:562-586` 已有 macOS 沙箱用例,新增运行时翻转变体(先 WorkspaceWrite 拒工作区
  外写 → 翻 yolo → 放行)。

**app-server**
- `setPermissionMode` 生效且写入 meta。
- resume 一个 yolo 线程,构造出的 checker `is_yolo()` 为真、沙箱为 DangerFullAccess。
- `invalid_params` / `unknown_thread` 分支。
- `thread/list` 暴露 `permission_mode`。

**thread_store**
- `permission_mode` serde 往返;旧 meta 向后兼容。
- `set_permission_mode` 与 rename 并发不丢更新。

**前端**
- `protocol.test.ts`:新字段 / helper。
- ModeChip:下拉展开、YOLO 触发确认、取消不调用、确认后调用并变红。

**可选(真实 LLM,`#[ignore]`)**
- `e2e_real.rs` 加一条:yolo 线程无审批写入工作区外。

---

## 文档更新(CLAUDE.md 要求,同一 PR)

- `docs/project-management/` 对应模块(desktop / app-server)登记该 feature 与可验证判据。
- `README.md` 模块索引表更新「完成 / 总计」计数。

---

## 非目标(YAGNI)

- 不做 `thread/start` 的 mode 参数。
- 不做前端「全局模式」开关(范围已定为按线程)。
- 不把 yolo 做成「danger full access」以外的模式(如 read-only 切换)。
- 不改变黑名单语义——任何模式下黑名单都硬拒。

---

## 风险与注意事项

- **沙箱提权是安全敏感路径**:靠 `promotable` 保证任何显式/env 指定的沙箱都不会被 UI
  静默提权;这是本设计的安全兜底,不可简化掉。
- **两份 policy 必须共享同一 controller**,否则 bash 与 process_start 行为不一致。
- **`ThreadMode` 与 `PermissionMode` 命名易混**:前者是持久化的线程模式,后者是 bootstrap
  的构造模式,务必分开。
- **app-server 的 `.env` 路径可达**:`config::load` 会加载 `$HOME/.yi-agent/.env`
  (`config.rs:78-95,125-176`),故 `YI_AGENT_SANDBOX` 在 desktop 下可影响 `base`——
  正是保留 `promotable` 的原因。
