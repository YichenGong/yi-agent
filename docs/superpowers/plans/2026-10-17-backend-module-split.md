# 后端巨型模块拆分与测试脚手架收敛 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 22,368 行的 `app-server/src/server.rs` 按 RPC 域拆分、内联测试目录化；收拢 store 测试的三个替身；`relay_parts` 单一来源；`handle_key`/`run_loop` 的 channel 打包。

**Architecture:** 纯搬移为主。新增 `src/rpc/` 每域一个模块，`serve()` 退化为 dispatch；`yi-agent-store/tests/common/` 收拢替身；`yi-agent-relay::parse_relay_parts` 收拢 relay URL 解析；`TuiChannels` 收拢 TUI 出向 channel。

**Tech Stack:** Rust（`yi-agent-app-server` / `yi-agent-store` / `yi-agent-relay` / `yi-agent`）。

**Spec:** `docs/superpowers/specs/2026-10-17-backend-module-split-design.md`

## Global Constraints

- 环境：`export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"`、`export TMPDIR=/Users/gongyichen/.yi-agent-tmp`，cargo 加 `--offline`。
- **不改任何 RPC 方法名与线上 JSON 形状**（纯搬移）。
- **macOS 的 BSD `sed` 不支持 `\b`**，会用 `perl -pi -e` 或 python。
- 行号以 `f490579f` 为准；改动前用 `grep -n` 复核锚点。
- 只显式 `git add <path>`。
- **本卡拥有 `main.rs` 与 `tui/app.rs`**（另一张"死代码/前端去重"卡刻意不碰它们，
  故两卡零文件重叠）。因此 `ControlCommand` → `DriverControl` 的改名**在本卡完成**
  （Task 5 的前置步骤），上述两个文件里的名字由本卡统一。

---

## File Structure

- `yi-agent-rs/crates/yi-agent-app-server/src/rpc/mod.rs` — 新增，dispatch + `AppState`。
- `yi-agent-rs/crates/yi-agent-app-server/src/rpc/{thread,agent,image,workspace,turn,process,plugin,onboarding,model,board,ui,pair}.rs` — 新增，每域一个。
- `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` — 只留 transport + dispatch。
- `yi-agent-rs/crates/yi-agent-store/tests/common/mod.rs` — 新增，测试替身。
- `yi-agent-rs/crates/yi-agent-relay/src/lib.rs` — 加 `parse_relay_parts`。
- `yi-agent-rs/crates/yi-agent/src/main.rs`、`.../app-server/src/server.rs` — 改委托。
- `yi-agent-rs/crates/yi-agent/src/tui/app.rs` — 加 `TuiChannels`；改 `crate::ControlCommand` 引用。
- `yi-agent-rs/crates/yi-agent/src/main.rs` — 把 `ControlCommand` 改名 `DriverControl`。

---

### Task 1: relay URL 解析单一来源（最小、最独立，先做）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-relay/src/lib.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs:195`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:1733`

**Interfaces:**
- Produces: `yi_agent_relay::parse_relay_parts(&str) -> anyhow::Result<(String, String)>`

- [ ] **Step 1: 记录基线**

```bash
cd yi-agent-rs
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
export TMPDIR=/Users/gongyichen/.yi-agent-tmp
cargo test -p yi-agent-relay --offline 2>&1 | grep "test result" | tail -2
cargo test -p yi-agent --offline relay_parts 2>&1 | grep "test result" | tail -2
```

Expected: relay 全绿（11 项）；`relay_parts` 2 项通过。

- [ ] **Step 2: 在 `yi-agent-relay` 加共享函数**

在 `crates/yi-agent-relay/src/lib.rs` 的 `#[cfg(test)] mod tests {` **之前**插入：

```rust
/// 把中继端点拆成「去掉 `session` 的 URL」+「session id」。
///
/// 中继客户端（`yi-agent` 的 CLI 与 `yi-agent-app-server`）都要在连接前把
/// `session` 从 URL 上摘下来，否则 `run_client` 再补一次会出现两个 `session`
/// 参数。两份实现曾各持一份逐字相同的副本，这里收成单一来源。
pub fn parse_relay_parts(relay_url: &str) -> anyhow::Result<(String, String)> {
    let (base, query) = match relay_url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (relay_url, None),
    };
    let session = query
        .unwrap_or("")
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(key, _)| *key == "session")
        .map(|(_, value)| value.to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "relay endpoint `{relay_url}` is missing `?session=<id>`: \
                 the phone and the computer must agree on the same session id"
            )
        })?;

    let kept: Vec<&str> = query
        .unwrap_or("")
        .split('&')
        .filter(|pair| !pair.is_empty() && !pair.starts_with("session="))
        .collect();
    let url = if kept.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", kept.join("&"))
    };
    Ok((url, session))
}
```

> 该函数体是 `main.rs:195` 与 `server.rs:1733` 两份副本的**逐字复制**（已实测二者
> 除返回类型写法外逐字相同，含报错文案）。`anyhow` 已在 relay 的 `[dependencies]`。

- [ ] **Step 3: 两处改为委托**

`crates/yi-agent/src/main.rs`：把 `fn relay_parts(...) { … }` 整个函数体替换为

```rust
fn relay_parts(relay_url: &str) -> Result<(String, String)> {
    yi_agent_relay::parse_relay_parts(relay_url)
}
```

`crates/yi-agent-app-server/src/server.rs`：把 `fn parse_relay_url(...) { … }` 替换为

```rust
fn parse_relay_url(relay_url: &str) -> anyhow::Result<(String, String)> {
    yi_agent_relay::parse_relay_parts(relay_url)
}
```

（`yi-agent-relay` 已是这两个 crate 的依赖，无需改 `Cargo.toml`。）

- [ ] **Step 4: 编译 + 跑测试**

```bash
cd yi-agent-rs
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
export TMPDIR=/Users/gongyichen/.yi-agent-tmp
cargo check -p yi-agent -p yi-agent-app-server --offline 2>&1 | grep -E "^error" | head
cargo test -p yi-agent --offline relay_parts 2>&1 | grep "test result" | tail -2
cargo test -p yi-agent-relay --offline 2>&1 | grep "test result" | tail -2
```

Expected: 无 error；`relay_parts` 2 项通过；relay 11 项通过。**已实测此改法通过。**

- [ ] **Step 5: commit**

```bash
cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cargo fmt -p yi-agent -p yi-agent-app-server -p yi-agent-relay && cd ..
git add yi-agent-rs/crates/yi-agent-relay/src/lib.rs yi-agent-rs/crates/yi-agent/src/main.rs yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "refactor(relay): single source for relay URL session parsing"
```

---

### Task 2: `server.rs` 内联测试与夹具目录化（先拆测试，后拆逻辑）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:9481`（`tests_support`）、`:9560`（`mod tests`）

**Interfaces:**
- Produces: `yi-agent-app-server/src/test_support.rs`（原 `tests_support`）、
  `yi-agent-app-server/tests/`（原内联 `mod tests`）。

> **顺序理由**：逻辑拆分前先把 12,808 行测试挪走，`server.rs` 立刻从 22,368 降到
> 约 9,500 行，后续搬 handler 时的行号锚点才可控。

- [ ] **Step 1: 记录基线并冻结用例清单**

```bash
cd yi-agent-rs
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
export TMPDIR=/Users/gongyichen/.yi-agent-tmp
cargo test -p yi-agent-app-server --offline -- --list 2>/dev/null | grep ": test$" | sort > /tmp/appserver_tests_before.txt
wc -l /tmp/appserver_tests_before.txt
wc -l crates/yi-agent-app-server/src/server.rs
```

Expected: 记下用例数与 22,368 行。**该清单是后续所有搬移的验收基准。**

- [ ] **Step 2: 抽出 `tests_support` 到独立模块**

把 `server.rs:9481` 的 `pub(crate) mod tests_support { … }` 整体移到新文件
`crates/yi-agent-app-server/src/test_support.rs`（去掉外包的 `mod` 层，内容不变），
并在 `lib.rs` 加 `pub(crate) mod test_support;`。引用了它的地方（`ws.rs`、
`mod tests`）改为 `crate::test_support::…`。

- [ ] **Step 3: 把内联 `mod tests` 迁到 `tests/`**

把 `server.rs:9560` 起的 `pub(crate) mod tests { … }` 内容整块移到
`crates/yi-agent-app-server/tests/rpc_e2e.rs`，改 `use` 前缀为 crate 名。
若其中含只用 `pub(crate)` 可见性的项导致集成测试无法编译，则改为
`src/rpc_tests.rs` + `#[cfg(test)] mod rpc_tests;`（就近方案）。

```bash
cd yi-agent-rs
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
export TMPDIR=/Users/gongyichen/.yi-agent-tmp
cargo test -p yi-agent-app-server --offline -- --list 2>/dev/null | grep ": test$" | sort > /tmp/appserver_tests_after.txt
diff /tmp/appserver_tests_before.txt /tmp/appserver_tests_after.txt && echo "TEST SET UNCHANGED"
wc -l crates/yi-agent-app-server/src/server.rs
```

Expected: `TEST SET UNCHANGED`；`server.rs` 降到约 9,500 行。

- [ ] **Step 4: 跑全量测试**

Run: `cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && export TMPDIR=/Users/gongyichen/.yi-agent-tmp && cargo test -p yi-agent-app-server --offline 2>&1 | grep -E "test result" | tail -6`

Expected: 全绿（用例数 = 基线）。

- [ ] **Step 5: commit**

```bash
cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cargo fmt -p yi-agent-app-server && cd ..
git add yi-agent-rs/crates/yi-agent-app-server/src/test_support.rs yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs yi-agent-rs/crates/yi-agent-app-server/src/lib.rs yi-agent-rs/crates/yi-agent-app-server/tests
git commit -m "refactor(app-server): move server.rs inline tests into their own module"
```

---

### Task 3: 按域拆 `serve()` 的 dispatch（逐域提交）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/mod.rs` + 12 个域模块
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`serve()` 的 `match method.as_str()`，基线行 2457-5587）

**Interfaces:**
- Produces: `rpc::dispatch(state, method, params) -> Option<Value>`（或等价）。

- [ ] **Step 1: 量清域与臂分布**

```bash
cd yi-agent-rs
awk 'NR>=2457 && NR<=5587' crates/yi-agent-app-server/src/server.rs | grep -oE '^\s+"[a-z_]+/' | sed 's/[[:space:]"]//g' | sort | uniq -c | sort -rn
```

Expected（基线实测）: `thread/`17、`agent/`7、`image/`5、`workspace/`3、`turn/`3、
`process/`3、`plugin/`3、`onboarding/`3、`model/`3、`board/`3、`ui/`2、`pair/`2、
`device/`2、`plugins/`1、`config/`1，合计 140 条臂。

- [ ] **Step 2: 建 `rpc/mod.rs` 与 `AppState`**

`AppState` 收拢原 `serve()` 里 handler 需要的可变状态（在 Step 3 拆第一个域时才
能确定精确字段集——**不要预先猜**，先按需搬运）。先写 dispatch 骨架：

```rust
//! RPC 方法到域 handler 的分派。每个域一个子模块，`server.rs` 只留 transport。

pub(crate) mod image;
pub(crate) mod model;
pub(crate) mod onboarding;
// …其余域逐个补齐

/// 一个 RPC 方法所属的域。
pub(crate) enum Domain { Image, Model, Onboarding, /* … */ Unknown }

pub(crate) fn domain_for(method: &str) -> Domain {
    match method.split_once('/').map(|(head, _)| head) {
        Some("image") => Domain::Image,
        Some("model") => Domain::Model,
        Some("onboarding") => Domain::Onboarding,
        // …
        _ => Domain::Unknown,
    }
}
```

- [ ] **Step 3: 逐域搬移（每个域一次提交）**

按"依赖最少"排序，建议顺序：`image` → `model` → `onboarding` → `ui`/`config` →
`board` → `plugin`/`plugins` → `pair`/`device` → `process` → `turn` → `workspace`
→ `agent` → `thread`（`thread/` 最大且最核心，放最后）。

每个域的做法：

1. 从 `serve()` 的 `match` 中**整段剪切**该前缀的所有臂（含臂体），粘到
   `src/rpc/<domain>.rs` 的 `pub(crate) async fn handle(state: &mut AppState, method: &str, params: Value) -> …`，
   保留内层 `match method` 与其臂体**逐字不变**。
2. `serve()` 里换成 `rpc::Domain::<X> => rpc::<x>::handle(&mut state, method, params).await?,`。
3. `cargo check -p yi-agent-app-server --offline`；把 handler 需要的局部变量提升为
   `AppState` 字段（编译器会逐一指出）。
4. `cargo test -p yi-agent-app-server --offline -- --list | grep ": test$" | sort | diff /tmp/appserver_tests_before.txt -` 必须为空。
5. commit：`refactor(app-server): move <domain> RPC handlers into src/rpc/<domain>.rs`。

- [ ] **Step 4: 收尾——`serve()` 只剩 transport + dispatch**

```bash
cd yi-agent-rs
python3 - <<'PY'
src=open('crates/yi-agent-app-server/src/server.rs').read()
i=src.index('pub(crate) async fn serve<F>(')
seg=src[i:i+200000]
end=seg.index(') ->')
print("serve params:", seg[seg.index('<F>(')+4:end].count(',')+1)
PY
grep -c "" crates/yi-agent-app-server/src/server.rs
```

Expected: `serve` 参数数不变（15，除非顺手 bundle 化——那属另一卡）；文件行数
从 9,500 进一步下降（handler 已外移）。`serve()` 本体应从 3,454 行降到 **< 300**。

- [ ] **Step 5: 全量验证**

```bash
cd yi-agent-rs
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
export TMPDIR=/Users/gongyichen/.yi-agent-tmp
cargo test --workspace --offline 2>&1 | grep -E "test result: FAILED|^error" | head
cargo clippy --workspace --all-targets --offline -- -D warnings 2>&1 | grep -E "^error|^warning: unused" | head
```

Expected: 无失败、无警告（除 `html5ever v0.22.5` 的 future-incompat 提示）。

---

### Task 4: store 测试脚手架收敛

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-store/tests/common/mod.rs`
- Modify: `yi-agent-rs/crates/yi-agent-store/tests/{runtime_ipc,runtime_coordinator,subagent_fork_ipc,subagent_fork_upload,subagent_runtime_e2e}.rs`

**Interfaces:**
- Produces: `common::read_only_workspace(...)`、`common::StaticWorkspaceService`。

- [ ] **Step 1: 记录基线**

```bash
cd yi-agent-rs
grep -rn "fn read_only_workspace" crates/yi-agent-store/tests/ | wc -l
grep -rn "struct StaticWorkspaceService" crates/yi-agent-store/tests/ | wc -l
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
export TMPDIR=/Users/gongyichen/.yi-agent-tmp
cargo test -p yi-agent-store --offline 2>&1 | grep -E "test result" | tail -6
```

Expected: 8 处 `read_only_workspace`、3 处 `StaticWorkspaceService`；测试全绿。

- [ ] **Step 2: 新建 `common/mod.rs`**

以 `subagent_runtime_e2e.rs:37` 那份为蓝本（实测其签名带
`repository_root`/`parent`/`TaskId`，最完整）：

```rust
//! store 集成测试共用的替身。
//!
//! 原先五个测试文件各持一份逐字相同的 `read_only_workspace` 与
//! `StaticWorkspaceService`，这里收成单一来源。

use std::path::Path;
use yi_agent_core::subagent::task::WorkspaceLeaseId;
use yi_agent_core::subagent::worker::{WorkerError, WorkerWorkspace};
use yi_agent_core::TaskId;

/// 把父 workspace 的路径原样继承的可读写替身。
pub fn read_only_workspace(
    repository_root: &Path,
    parent: Option<&WorkerWorkspace>,
    _task_id: &TaskId,
) -> Result<WorkerWorkspace, WorkerError> {
    let path = parent
        .map(|workspace| workspace.path.clone())
        .unwrap_or_else(|| repository_root.to_path_buf());
    Ok(WorkerWorkspace {
        lease_id: WorkspaceLeaseId::new(),
        repository_root: repository_root.to_path_buf(),
        path,
        branch: String::new(),
        // …逐字搬移原实现剩余字段
    })
}
```

> 字段与 `use` 清单以 `subagent_runtime_e2e.rs:37` 的原实现为准，逐字搬移。
> `StaticWorkspaceService` 的三份有差异，搬移时以 `runtime_ipc.rs:137` 那份
> （结构最简）为基，另两份的额外字段做成可选参数。

- [ ] **Step 3: 各测试文件改用 `common`**

每个文件顶部加 `mod common;`，删除本地 `fn read_only_workspace` /
`struct StaticWorkspaceService`，调用点改 `common::…`。

- [ ] **Step 4: 验证**

```bash
cd yi-agent-rs
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
export TMPDIR=/Users/gongyichen/.yi-agent-tmp
cargo test -p yi-agent-store --offline 2>&1 | grep -E "test result|^error" | tail -6
grep -rn "fn read_only_workspace" crates/yi-agent-store/tests/ 
```

Expected: 全绿；`read_only_workspace` 只剩 `common/mod.rs` 一处。

- [ ] **Step 5: 合并 `yi-agent/tests` 的两个 `AppServer`**

以 `board_watchman_e2e.rs:137` 那份（更完整）为基，移入
`crates/yi-agent/tests/common/mod.rs`；`board_e2e.rs:57` 的本地定义删除，
`use common::AppServer;`。两处注释择并集保留。

```bash
cd yi-agent-rs
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
export TMPDIR=/Users/gongyichen/.yi-agent-tmp
cargo test -p yi-agent --offline --test board_e2e --test board_watchman_e2e 2>&1 | grep -E "test result|^error" | tail -4
```

Expected: 两个测试目标全绿。

- [ ] **Step 6: commit**

```bash
cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cargo fmt -p yi-agent-store -p yi-agent && cd ..
git add yi-agent-rs/crates/yi-agent-store/tests yi-agent-rs/crates/yi-agent/tests
git commit -m "test(store): share the workspace/worker test doubles via tests/common"
```

---

### Task 5: `TuiChannels` 打包（降 `handle_key` 参数）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/tui/app.rs`（`handle_key:1818`、`run_loop:317`、`run_tui_with_backend_and_events:241`）

**Interfaces:**
- Produces: `pub(crate) struct TuiChannels { input, interrupt, kill, control, decision }`。

- [ ] **Step 0: 先改名 `ControlCommand` → `DriverControl`**

`main.rs:2509` 的 `enum ControlCommand`（TUI→driver 重建指令）与
`control_commands.rs:7` 的同名类型不同义，先消除同名。引用面（基线实测）：

```
main.rs                     18 处  `ControlCommand`（含定义）
tui/app.rs                 101 处  `crate::ControlCommand`   ← 同一类型，必须一起改
tui/slash.rs                19 处  `control_commands::ControlCommand`  ← 不许动
control_commands.rs          4 处  ← 不许动
control_commands_tests.rs    3 处  ← 不许动
```

```bash
cd yi-agent-rs
# macOS 的 BSD sed 不支持 `\b`（会静默不改），必须用 perl。
perl -pi -e 's/\bControlCommand\b/DriverControl/g' crates/yi-agent/src/main.rs
perl -pi -e 's/crate::ControlCommand/crate::DriverControl/g' crates/yi-agent/src/tui/app.rs
echo "main.rs DriverControl=$(grep -c 'DriverControl' crates/yi-agent/src/main.rs) residual=$(grep -c 'ControlCommand' crates/yi-agent/src/main.rs)"
echo "app.rs residual=$(grep -c 'crate::ControlCommand' crates/yi-agent/src/tui/app.rs || true)"
```

Expected: `main.rs DriverControl=18 residual=0`；`app.rs residual=0`。**已实测**
该改法后 `cargo test -p yi-agent` 635 测试全绿。

- [ ] **Step 1: 记录基线**

```bash
cd yi-agent-rs
grep -c "let (input_tx" crates/yi-agent/src/tui/app.rs
grep -n "fn handle_key\|fn run_loop\|fn run_tui_with_backend_and_events" crates/yi-agent/src/tui/app.rs
grep -rn "run_tui_with_backend_and_events" crates/ | wc -l
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
export TMPDIR=/Users/gongyichen/.yi-agent-tmp
cargo test -p yi-agent --offline --lib 2>&1 | grep "test result" | tail -2
```

Expected: 74 处样板；`handle_key` 25 参、`run_loop` 19 参、11 参函数被调 32 次；
测试全绿。

- [ ] **Step 2: 定义 `TuiChannels`**

在 `app.rs` 的 `handle_key` 之前加：

```rust
/// `run_loop` / `handle_key` 共用的出向 channel 集合。
///
/// 这些 channel 在整场 TUI 会话里成组存在、成组传递，故打包成一个结构体，
/// 避免每加一个能力就给 `handle_key` 再加一个参数（它已因此长到 25 个）。
pub(crate) struct TuiChannels {
    pub input: tokio::sync::mpsc::Sender<String>,
    pub interrupt: tokio::sync::mpsc::Sender<()>,
    pub kill: tokio::sync::mpsc::Sender<String>,
    /// TUI→driver 的重建指令（Step 0 已改名为 `DriverControl`）。
    pub control: tokio::sync::mpsc::Sender<crate::DriverControl>,
    pub decision: tokio::sync::mpsc::Sender<(u64, yi_agent_core::permission::Decision)>,
}
```

- [ ] **Step 3: 替换 `handle_key` 的 5 个 channel 参数**

把 `handle_key` 签名中的 `input_tx`/`interrupt_tx`/`kill_tx`/`control_tx`/
`decision_tx` 五个参数换成 `channels: &TuiChannels`，函数体内
`input_tx.send(…)` 等改为 `channels.input.send(…)`。

- [ ] **Step 4: 顺带在 `run_loop` 里构造并透传**

`run_loop` 同样把 5 个参数收成 `channels: TuiChannels`，在
`run_tui_with_backend_and_events` 里构造：

```rust
    let channels = TuiChannels {
        input: input_tx.clone(),
        interrupt: interrupt_tx.clone(),
        kill: kill_tx.clone(),
        control: control_tx.clone(),
        decision: decision_tx.clone(),
    };
```

- [ ] **Step 5: 收敛测试样板（可选但推荐）**

在 `#[cfg(test)] mod tests` 里加：

```rust
/// 一套与 `run_loop` 配套的 channel，供测试构造 `TuiChannels`。
/// 原先是每个用例手抄 5 行 `let (…_tx, _rx) = mpsc::channel(...)`（本文件 74 处）。
fn channels() -> (TuiChannels, TokioTestReceivers) { /* … */ }
```

把 74 处样板替换为 `let (channels, _rx) = channels();`。**若这一步导致测试改动
面过大**，可留作后续；Task 的验收不依赖它。

- [ ] **Step 6: 验证**

```bash
cd yi-agent-rs
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
export TMPDIR=/Users/gongyichen/.yi-agent-tmp
python3 - <<'PY'
src=open('crates/yi-agent/src/tui/app.rs').read()
i=src.index('fn handle_key(')
seg=src[i:i+6000]; end=seg.index(') ->')
p=seg[seg.index('(')+1:end]
depth=0;n=0
for ch in p:
    if ch in '(<[': depth+=1
    elif ch in ')>]': depth-=1
    elif ch==',' and depth==0: n+=1
print("handle_key params:", n+1)
PY
cargo test -p yi-agent --offline --lib 2>&1 | grep -E "test result|^error" | tail -3
```

Expected: `handle_key params` ≤ 21（去掉 5 个 channel、加 1 个 bundle 后）；
测试全绿且用例数不减。

- [ ] **Step 7: commit**

```bash
cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && cargo fmt -p yi-agent && cd ..
git add yi-agent-rs/crates/yi-agent/src/tui/app.rs yi-agent-rs/crates/yi-agent/src/main.rs
git commit -m "refactor(tui): rename the driver control enum and bundle the outbound channels"
```
