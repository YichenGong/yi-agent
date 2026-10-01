# Spec: 插件侧 socket 路径回退 — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 深路径仓库里，插件的查询 socket 能 bind 上、且与宿主转发表里的路径**是同一条**；插件也能连上宿主的 daemon。

**Architecture:** 两条规则并存（spec §3.3）：宿主 daemon socket 沿用 `yi-agent-<sha256(runtime_dir)16>`；插件通道 socket 新增 `plugin-<sha256(直接路径)16>`，宿主与插件共用同一条（哈希同一输入 → 同一结果）。前缀/哈希输入的不同是**有意的**，由测试锁住。

**Tech Stack:** Rust（插件独立 workspace + 主工作区各改一处）。插件保持零 `yi-agent-*` 依赖。

## Global Constraints

- 主工作区：`export PATH="/Users/gongyichen/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`，cargo 一律 `--offline`，从 `yi-agent-rs/` 跑。
- 插件：`cd plugins/superpowers-kanban`。
- `TMPDIR=/Users/gongyichen/.yi-agent-tmp`。
- 上限常量 `103`（Unix domain socket 路径上限）。
- **两套规则不得合并**：`yi-agent-`（哈希 runtime_dir）与 `plugin-`（哈希直接路径）用途不同，注释里写明理由。
- 插件**零** `yi-agent-*` 依赖（规则靠手写复刻）。
- 测试不得依赖长 `$TMPDIR` 之外的环境假设；用临时目录构造"刚好越界"的路径。

---

### Task 1: 插件侧规则 + 两处调用点

**Files:**
- Create: `plugins/superpowers-kanban/crates/superpowers-kanban-ipc/src/socket.rs`
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-ipc/src/client.rs`（`socket_path`）
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-ipc/src/server.rs`（`socket_path`）
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-ipc/src/lib.rs`（挂模块）
- Modify: `plugins/superpowers-kanban/crates/superpowers-kanban-ipc/Cargo.toml`（加 `sha2`）

**Interfaces:**
- Produces:
  - `pub fn plugin_socket_for(direct: &Path) -> Result<PathBuf, SocketPathError>`——
    规则 spec §3.2（`plugin-` 前缀，哈希直接路径，回退后仍超长则 `Err`）。
  - `pub fn daemon_socket_for(runtime_dir: &Path) -> Result<PathBuf, SocketPathError>`——
    复刻宿主 `socket_path_for`（`yi-agent-` 前缀，哈希 runtime_dir）。

- [x] **Step 1: 写失败测试**（socket.rs 内）
  1. 短路径直通，返回原路径；
  2. 长路径回退到 `temp_dir()/plugin-<16hex>.sock`，且**确定**（两次同值）；
  3. 回退结果与宿主 `yi-agent-` 前缀**不同名**；
  4. 回退后仍超长 → `Err`；
  5. `daemon_socket_for` 对 `LONG_RUNTIME_DIR` 的样例值与宿主 `socket_path_for` **一致**
     （跨端一致性靠这条锁住）。
- [x] **Step 2: 运行确认失败** → FAIL
- [x] **Step 3: 实现**
- [x] **Step 4: 运行确认通过** → PASS
- [x] **Step 5: Commit**

---

### Task 2: 宿主侧 `query_socket_path` 也走回退

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/src/manifest.rs`
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/Cargo.toml`（加 `sha2`）

**Interfaces:**
- Consumes: Task 1 定下的规则（宿主侧手写复刻，不改宿主原有 `yi-agent-` 规则）。
- Produces: `query_socket_path(...) -> Option<Result<PathBuf, …>>` 或等价形态——
  展开后按 `plugin-` 规则解析。

- [x] **Step 1: 写失败测试**——深 `state_dir` 下，展开结果等于
  `temp_dir()/plugin-<sha256(展开路径)16>.sock`。
- [x] **Step 2: 运行确认失败** → FAIL
- [x] **Step 3: 实现**
- [x] **Step 4: 运行确认通过** → PASS
- [x] **Step 5: Commit**

---

### Task 3: 跨端一致性测试（宿主 ↔ 插件）

**Files:**
- Test: `plugins/superpowers-kanban/crates/superpowers-kanban-ipc/tests/`（新文件）或主工作区集成测试

- [x] **Step 1: 写测试**——对同一深 `state_dir`：
  宿主 `query_socket_path` 的结果 == 插件 `server::socket_path` 的结果。
  两端各写死一条**期望值**（同一字符串），任一侧规则漂移都会炸。
- [x] **Step 2: 运行确认通过**（若失败，说明两侧规则没对齐，回 Task 1/2 修）
- [x] **Step 3: Commit**

---

### Task 4: 真实插件的深路径集成 + 手工冒烟

- [x] **Step 1: 集成测试**——长路径项目 + 内嵌 daemon + 真清单：
  断言插件进程被拉起、**socket 被 bind 上**、`PluginQuery` 能取到 `switch.read`。
- [x] **Step 2: 全量回归**——主工作区、插件 115+、桌面 282、`grep -rn yi_agent_board_ui` 为空。
- [x] **Step 3: 手工冒烟**——本仓库（深路径）重启 TUI，`/superpowers-kanban` 看到队列；
  那张冒烟卡从 `queued` 真正跑起来（用户已认领此步）。
- [x] **Step 4: Commit**

---

## 验收（spec §4）

1. 规则单测：直通 / 回退 / 确定 / 不撞名 / 仍超长报错。
2. 跨端一致：宿主与插件对同一深路径得出同一条。
3. 真实插件 + 深路径：socket bind 成功且查询通道通。
4. 全量回归全绿，插件仍零 `yi-agent-*` 依赖。
5. 手工冒烟：看板 UI 看到队列，冒烟卡真的跑起来。

## 本次不做

- 不改宿主 `socket_path_for`（daemon 自己的 socket 已正确）。
- 不改插件→daemon 的线协议。
- 不改清单里 `query_socket` 的模板写法。
- #3（看板按项目目录组织）另开一轮，不在本 plan。

---

### Task 5: 协议版本对齐（实施中发现，spec §6）

- [x] 插件 `PROTOCOL_VERSION` 1 → 2（与宿主对齐）
- [x] 宿主侧防漂移测试：读插件 `wire.rs` 比对，目录缺失跳过
- [x] 反向验证：插件退回 1 时测试红
- [x] 提交

## 实施结果

- 深路径（188 字节）真实闭环：入队 → 消费 → 预建 worktree → **Launched / running**
- 三处修复：插件 socket 回退、宿主转发表回退、协议版本对齐
- 两把锁：socket 契约夹具（两端共读）、协议版本比对（宿主读插件源码）
- 另记录一个非代码问题：`cp` 覆盖安装触发 macOS 签名缓存拒绝，需重签名
