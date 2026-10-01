# Spec 4：插件化状态通道（通用 `plugin/query`）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 主程序彻底不认识看板——删掉 `yi-agent-board-ui`，看板状态一律经"通用 plugin/query 转发"从插件取。

**Architecture:** 新增一条**通用**通道，主程序只做转发、不解释内容：桌面/TUI → app-server `plugin/query` → daemon `IpcRequest::PluginQuery` → 插件 socket。看板语义（`list`/`enqueue`/`switch.read`/`switch.write`）只存在于**桌面端与插件之间**。插件从"只能发 IPC"变为"也能收"。

**Tech Stack:** Rust（插件独立 workspace + 主工作区）、Unix domain socket、既有 `IpcRequest`/`IpcResponse` 协议（带 `PROTOCOL_VERSION`）。

## Global Constraints

- 插件：**零** `yi-agent-*` 依赖（不得破坏）；`TMPDIR=/Users/gongyichen/.yi-agent-tmp`（socket 路径 >103 字节会失败）。
- 主工作区：`export PATH="/Users/gongyichen/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`，cargo 一律 `--offline`。
- desktop：`export PATH="/opt/homebrew/bin:$PATH"`，vitest `TMPDIR="$PWD/.tmpverify"`；worktree 需 `ln -sfn <主仓库>/desktop/node_modules node_modules`，用完 `rm -f`。
- **接口版本纪律**：`IpcRequest` 是 `#[serde(tag=...)]` 的枚举，加变体会改变线格式；`PROTOCOL_VERSION` 必须**同步递增**，且 daemon 对低版本请求的拒绝路径必须有测试。
- **安全边界**（spec §6 要求明确）：`PluginQuery` 只允许转发到**清单里声明了 `query_socket`** 且**daemon 正在守护**的插件；socket 路径**只能来自清单**，不接受请求方传入的任意路径（否则等于开放本地任意 socket 的代理）。
- 删除 `yi-agent-board-ui` 是**编译期验收**：主工作区里 `grep -r yi_agent_board_ui` 必须为空。

---

### Task 1: 协议：`PluginQuery` / `PluginResult`（含版本与安全边界）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`
- Test: 同文件内 `mod tests`

**Interfaces:**
- Produces: `IpcRequest::PluginQuery { plugin: String, method: String, params: Value }`
  与 `IpcResponse::PluginResult { value: Value }`；`PROTOCOL_VERSION` 递增。

- [ ] **Step 1: 写失败测试**

```rust
// 1. PluginQuery 能 round-trip 编解码（params 原样保留）
// 2. respod 对未声明的插件名返回结构化错误，而不是 panic / 空值
// 3. 旧 PROTOCOL_VERSION 的请求被拒绝（既有拒绝路径不得被新变体绕过）
```

- [ ] **Step 2: 运行确认失败** → FAIL
- [ ] **Step 3: 实现**
  - 加两个变体；`PROTOCOL_VERSION` +1。
  - `respond(...)` 里 `PluginQuery` 分支：查"插件名 → socket"映射；查不到 →
    `Err(IpcError::...)` 结构化错误；查到则转发（Task 3 落地真实转发，本步可先返回
    `unimplemented` 的结构化错误并测试它）。
- [ ] **Step 4: 运行确认通过** → PASS
- [ ] **Step 5: Commit**

---

### Task 2: 清单声明 `query_socket`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/src/manifest.rs`
- Modify: `plugins/superpowers-kanban/supervisors/superpowers-kanban.json`

**Interfaces:**
- Produces: `SupervisorManifest::query_socket: Option<PathBuf>`；占位符展开（`{state_dir}` 等）与 `args` 同款。

- [ ] **Step 1: 写失败测试**

```rust
// 1. 清单带 query_socket 时解析成功，且占位符被展开
// 2. 不带该字段时解析成功且为 None（向后兼容，旧清单不能因此失效）
// 3. 插件清单声明的 socket 路径 == <state_dir>/superpowers-kanban.sock
```

- [ ] **Step 2: 运行确认失败** → FAIL
- [ ] **Step 3: 实现**
- [ ] **Step 4: 运行确认通过** → PASS
- [ ] **Step 5: Commit**

---

### Task 3: daemon 转发（唯一一处碰 socket 的代码）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-store/src/ipc.rs`（或新建 `plugin_forward.rs`）
- Test: `yi-agent-rs/crates/yi-agent-store/tests/`（起真 socket 的对测）

**Interfaces:**
- Consumes: Task 1 的变体、Task 2 的 `query_socket`。
- Produces: 把 `PluginQuery` 转发到清单声明的 socket，把插件的回包包成
  `IpcResponse::PluginResult { value }`；任何失败都是结构化 `IpcError`。

- [ ] **Step 1: 写失败测试**（起一个假插件 socket，断言：原样转发、原样返回、错误可区分）
- [ ] **Step 2: 运行确认失败** → FAIL
- [ ] **Step 3: 实现**（注意：`respond` 目前是同步的；socket 往返要有**超时**，否则插件卡住会拖死 daemon 请求线程）
- [ ] **Step 4: 运行确认通过** → PASS
- [ ] **Step 5: Commit**

---

### Task 4: 插件侧查询服务端

**Files:**
- Create: `plugins/superpowers-kanban/crates/superpowers-kanban-ipc/src/server.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs`
  （`run` 循环里起监听线程）

**Interfaces:**
- Produces: 插件监听 `<state_dir>/superpowers-kanban.sock`，实现四个方法：
  `list` / `enqueue` / `switch.read` / `switch.write`。复用 Task 2（Spec 2）已下沉的
  `card_id_for` / `deliver_card` / `write_layer` / `read_layer`，以及 `persist::load_board`。

- [ ] **Step 1: 写失败测试**（对 `dispatch(method, params) -> Result<Value, String>` 的纯函数测试；
  每个方法一组：`list` 返回卡片数组；`enqueue` 校验失败返回错误且不落盘；
  `switch.read` 返回解析后的值与来源；`switch.write` 写项目层）
- [ ] **Step 2: 运行确认失败** → FAIL
- [ ] **Step 3: 实现**（dispatch 与 socket 循环分开：socket 层只做编解码，逻辑全在可测的 dispatch）
- [ ] **Step 4: 运行确认通过** → PASS
- [ ] **Step 5: Commit**

---

### Task 5: app-server + 桌面端改道

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（4 条 `board/*` → `plugin/query`）
- Modify: `desktop/src/lib/superpowersKanbanSwitch.ts`（改调 `plugin/query`，参数带
  `plugin: "superpowers-kanban"`）
- Modify: `desktop/src/App.tsx`、相关测试

**Interfaces:**
- Produces: 桌面端经 `plugin/query` 拿状态；**不存在**"插件未安装"以外的看板语义在 app-server 里。

- [ ] **Step 1: 写失败测试**（桌面端：转发参数正确；插件未安装 → 显示"插件未安装"文案）
- [ ] **Step 2: 运行确认失败** → FAIL
- [ ] **Step 3: 实现**
- [ ] **Step 4: 运行确认通过** → PASS
- [ ] **Step 5: Commit**

---

### Task 6: TUI 改道 + 删除 `yi-agent-board-ui`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/superpowers_kanban.rs`
- Delete: `yi-agent-rs/crates/yi-agent-board-ui/`
- Modify: 两处 `Cargo.toml`（去掉依赖）、工作区 `Cargo.toml`

- [ ] **Step 1: TUI 改经 daemon → 插件取状态、投递、读写开关**
- [ ] **Step 2: 删除 crate 与依赖**
- [ ] **Step 3: 编译期验收**

Run: `grep -rn "yi_agent_board_ui\|yi-agent-board-ui" yi-agent-rs/ --include=*.rs --include=*.toml`
Expected: 空

- [ ] **Step 4: 全量回归**（主工作区 60 套件 + 插件 + 桌面端）
- [ ] **Step 5: Commit**

---

## 验收（spec §7）

1. `plugin/query` 转发并原样返回（Task 3 测试）。
2. 插件未安装 → 结构化错误 + 桌面端文案（Task 5 测试）。
3. 主程序侧**不存在**直接读 `board.json` 的路径（Task 6 的 grep 验收）。
4. TUI 与桌面读到的卡片与插件自持状态一致（Task 6 的手工冒烟：`add` 后两端都看到同一张卡）。
