# Spec: 让内嵌 daemon 也监督插件 — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 任何**拥有** daemon 的进程都监督插件；借用别人 daemon 的进程不监督。修掉"在 TUI / 桌面里看板装了却不动"的缺口。

**Architecture:** 监督循环的机制（`yi-agent-supervisors`）与转发表（`yi-agent-store::ipc`）都已存在，不动。新增的只是"把二者接起来"的胶水，落在 `yi-agent-runtime`（`yi-agent` 与 `yi-agent-subagent` 的共同上游，不制造依赖环）。两个内嵌 daemon 的触碰点各加一行调用。

**Tech Stack:** Rust（主工作区）。无新依赖、无新 RPC、无新持久化。

## Global Constraints

- 主工作区：`export PATH="/Users/gongyichen/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`，cargo 一律 `--offline`，从 `yi-agent-rs/` 跑。
- `TMPDIR=/Users/gongyichen/.yi-agent-tmp`（socket 路径 103 字节上限）。
- **不改** `yi-agent-supervisors` 与 `yi-agent-store::ipc` 的任何公开语义（`serve_supervisor` 的现有实现即为参考实现）。
- **规则不许放宽**：只有 `Daemon::start_with_factory` 返回 `Ok(_)`（"我起的"）才监督；`AlreadyRunning` 一律不监督。
- 测试不得读写真实 `$HOME`（用临时 HOME）；不得污染真实项目状态目录。

---

### Task 1: `yi-agent-runtime` 新增监督胶水

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-runtime/src/supervise.rs`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/lib.rs`（挂模块）
- Modify: `yi-agent-rs/crates/yi-agent-runtime/Cargo.toml`（加 `yi-agent-supervisors`、`yi-agent-store`）

**Interfaces:**
- Produces:
  - `pub struct SuperviseHandle`，含 `pub fn is_supervising(&self) -> bool` 与 `pub fn stop(self)`。
  - `pub fn serve(workdir: &Path) -> SuperviseHandle`——后台线程，500ms 一轮，
    每轮 `reconcile()` 后 `register_plugin_sockets(supervisor.query_sockets())`；
    停止时 `clear_plugin_sockets()` 再 `stop_all()`；线程体 `catch_unwind`。

- [x] **Step 1: 写失败测试**

```rust
// 1. 清单 + 开关就位 → serve() 后假子进程被拉起（沿用 supervisors 测试里的
//    "假命令写 marker 文件"手法，避免依赖真插件二进制）。
// 2. 句柄 stop() 后：子进程被清理，且 PLUGIN_SOCKETS 被清空。
// 3. 清单不存在的目录：serve() 不 panic，is_supervising() 仍为 true（线程在跑、无事可做）。
```

- [x] **Step 2: 运行确认失败** → FAIL
- [x] **Step 3: 实现**（把 `main.rs::serve_supervisor` 的逻辑搬来，签名与语义保持一致）
- [x] **Step 4: 运行确认通过** → PASS
- [x] **Step 5: Commit**

---

### Task 2: `daemon serve` 改用共享实现

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`

**Interfaces:**
- Consumes: Task 1 的 `serve`。
- Produces: 删除 `main.rs` 里重复的 `serve_supervisor`/`SupervisorHandle`，
  `DaemonAction::Serve` 改调 `yi_agent_runtime::supervise::serve(&workdir)`。

- [x] **Step 1: 现有测试必须继续通过**（`main.rs` 里那个"清单 + 开关 → marker 文件"的测试即是回归网）
- [x] **Step 2: 替换实现，确认既有测试仍 PASS**（行为不变，只是换了落点）
- [x] **Step 3: Commit**

---

### Task 3: TUI 内嵌 daemon 接线

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`（`AttachedTuiRuntime` 与 `attach_tui_runtime`）

**Interfaces:**
- Consumes: Task 1 的 `serve`。
- Produces: `AttachedTuiRuntime` 增 `supervisor: Option<SuperviseHandle>`；
  仅当 `embedded_daemon.is_some()` 时启动；会话结束时停止。

- [x] **Step 1: 写失败测试**——`embedded_daemon = Some` 时 `is_supervising()` 为真；
  `AlreadyRunning`（模拟：先把 daemon 起在别处）时 `is_supervising()` 为假。
- [x] **Step 2: 运行确认失败** → FAIL
- [x] **Step 3: 实现**
- [x] **Step 4: 运行确认通过** → PASS
- [x] **Step 5: Commit**

---

### Task 4: 桌面内嵌 daemon 接线

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/attach.rs`（`AttachedProjectRuntime` 与 `attach_project_runtime`）
- Modify: `yi-agent-rs/crates/yi-agent-subagent/Cargo.toml`

**Interfaces:**
- Consumes: Task 1 的 `serve`。
- Produces: `AttachedProjectRuntime` 增 `supervisor: Option<SuperviseHandle>`；
  仅当 `embedded_daemon.is_some()` 时启动；binding 停止时一并停止。

- [x] **Step 1: 写失败测试**——同上两条（拥有的监督 / 借用的不监督）。
- [x] **Step 2: 运行确认失败** → FAIL
- [x] **Step 3: 实现**
- [x] **Step 4: 运行确认通过** → PASS
- [x] **Step 5: Commit**

---

### Task 5: 真实插件集成验证 + 全量回归

- [ ] **Step 1: 集成测试**——隔离项目目录 + 清单 + 开关 + 内嵌 daemon：
  断言 `superpowers-kanban run` 进程出现，且经 daemon 的 `PluginQuery` 能取到 `switch.read`。
- [ ] **Step 2: 全量回归**——主工作区（套件数不减）、插件 115 测试、桌面 282 测试、
  `grep -rn "yi_agent_board_ui"` 为空。
- [ ] **Step 3: 手工冒烟**——本仓库 TUI 里让那张冒烟卡从 `pending` 变成队列中的卡。
- [ ] **Step 4: Commit**

---

## 验收（spec §4）

1. 机制层单测：起/停/清表。
2. 规则层单测：借用的 daemon 不监督。
3. 真实插件集成：进程被拉起 + `PluginQuery` 通。
4. 全量回归全绿，`yi_agent_board_ui` 引用仍为空。
5. 手工冒烟：冒烟卡被消费。

## 本次不做（写下来，避免遗忘）

- 多进程共享监督者的选举/仲裁。
- 改插件、清单格式、`plugin/query` 协议。
- 改 `daemon serve` 的既有行为。
- 桌面/TUI 的 UI 文案改动。
