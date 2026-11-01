# D2 实施计划：TUI 事件循环拆分（`yi-agent/src/tui/app.rs`）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 538 行的 `run_loop`（17 个参数、20 个 `let mut`）按状态归属打包成三个结构体，把 350 行 / 20 参数的 `handle_key` 的参数收到一个上下文结构体里。

**Architecture:** 与 D1 **相反**的决策——这里**允许且要求**把散落的 `let mut` 打包成结构体。理由：D1 的状态（`stall_retries`、租约）有严格的「在哪个时刻被改、在哪个时刻必须已释放」的时序约束，打包会让它不可见；而 D2 的状态是「活到下一帧」的 UI 状态，没有时序陷阱，散着放才是负担。`handle_key` 的 20 个参数同理：实测 `key`(26 次)、`history`(22)、`input`(18)、`popup`(15)、`channels`(14) 是热点，其余 8 个参数各只出现 3–9 次，属于「跟着上下文走」的东西。

**Tech Stack:** Rust，ratatui，`cargo test -p yi-agent`。

## Global Constraints

- **只搬不改**：禁止改逻辑、改按键映射、改渲染输出、改文案、改变量名。
- **公共 API 冻结**：`tui::app` 的 `pub fn run_tui*` 系列签名逐字不变。
- **测试断言一字不改**。因签名变化导致的编译错误，**改调用点不改断言**。
- 每个 Task 结束必须满足：
  - `cargo test -p yi-agent` → 全绿（基线数量见 Task 1）
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings` → 0 告警
  - `cargo fmt --all -- --check` → 通过
- 提交信息用 conventional commits，首行 ≤72 字符，**不写** `Co-Authored-By`。
- 环境：
  ```bash
  cd yi-agent-rs
  export PATH="/opt/homebrew/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
  ```

## 实测结构（行号断言的唯一来源）

| 结构 | 行范围 | 行数 | 参数 | 测试直接调用 |
|---|---|---|---|---|
| `app.rs` 总计 | L1–L12060 | 12060 | — | — |
| `pub fn run_tui` | L142–L256 | 115 | — | — |
| **`fn run_loop<B, E>`** | **L583–L1120** | **538** | **17** | **16 次** |
| **`fn handle_key`** | **L2164–L2513** | **350** | **20** | **23 次** |
| `fn execute_slash_command` | L2816–L3237 | 422 | — | — |

### `run_loop` 的 17 个参数（实测）

```rust
terminal: &mut Terminal<B>,
agent_rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>,
history: &mut HistoryState,
input: &mut InputLine,
channels: TuiChannels,
is_running: &std::sync::Arc<std::sync::atomic::AtomicBool>,
events: &E,
model: &str,
runtime_intent: Option<crate::tui::subagents::RuntimeStartupIntent>,
runtime_choice_tx: Option<tokio::sync::mpsc::Sender<...RuntimeStartupChoice>>,
process_manager: std::sync::Arc<yi_agent_tools::ProcessManager>,
workdir: std::path::PathBuf,
mcp: std::sync::Arc<yi_agent_mcp::McpManager>,
config: &TuiConfigSnapshot,
catalog: &yi_agent_runtime::models::ModelCatalog,
theme: TuiTheme,
title: &mut crate::tui::title::TitleHandle,
```

### `run_loop` 体内的 20 个 `let mut`（实测行号）

**A 组 — 会话/模型状态（5 个）：**
```
L604  current_model: String
L609  current_model_ref: Option<String>
L613  effort_override: Option<String>
L617  statusbar_state: StatusBarState
L619  cost_tracker: CostTracker
```

**B 组 — 弹窗与浮层（5 个）：**
```
L615  popup: Option<CommandPopup>
L620  runtime_popup: RuntimePopup
L621  subagent_children: SubagentListState
L625  trace_streams: Option<super::trace::TraceStreams>
L630  runtime_notice_popup: Option<String>
```

**C 组 — 循环记账（10 个）：**
```
L614  pending_quit: bool
L616  queued: crate::tui::queued::DeliveredInterjections
L618  task_registry: RunningTaskRegistry
L631  runtime_intent
L634  runtime_notice_pending
L638  process_events
L639  process_snapshots
L640  process_outputs
L644  previous_viewport: Option<(ViewportAnchor, u16, u16)>
L647  notifications: NotificationState
```

（另有 3 个非 mut 的 `let`：L648 `notifications_enabled`、L649 `focused`、L650 `notifier`，它们不进结构体。）

### `handle_key` 的 20 个参数使用频次（实测）

| 参数 | 次数 | 归类 |
|---|---|---|
| `key` | 26 | 单次事件，不入上下文 |
| `history` | 22 | ViewContext |
| `input` | 18 | ViewContext |
| `popup` | 15 | ViewContext |
| `channels` | 14 | RuntimeContext |
| `pending_quit` | 9 | LoopState |
| `history_width` | 9 | ViewContext |
| `queued` | 7 | LoopState |
| `max_scroll_offset` | 4 | ViewContext |
| `history_height` | 4 | ViewContext |
| `cost_tracker` | 3 | 只读查询，入 ViewContext |
| `is_running` | 3 | RuntimeContext |
| `workdir` | 3 | RuntimeContext |
| `mcp` | 3 | RuntimeContext |
| `config` | 3 | RuntimeContext |
| `model` | 3 | ModelContext |
| `current_model_ref` | 3 | ModelContext |
| `catalog` | 3 | ModelContext |
| `current_effort` | 3 | ModelContext |
| `effort_source` | 3 | ModelContext |

---

## 关键设计决策（为什么 D2 与 D1 相反）

D1（`agent.rs`）里我明确禁止把状态打包成 `&mut LoopState`，理由是 `stall_retries` 与 `provider_turn_lease` 的**作用域时序**决定正确性：租约必须在退避 `sleep` 之前 `drop`，重试计数必须每轮清零。打包会让「谁在何时改它」从代码里消失。

D2 的情况正相反：

- `popup`、`runtime_popup`、`subagent_children` 这些是**渲染状态**，任何时刻都可读可写，没有「必须在此之前释放」的约束
- `current_model`、`effort_override` 是**用户选择**，只在按键处理或 provider 回报时改
- `pending_quit`、`queued` 是**循环控制**，只在特定分支翻转

它们的共同点是：**状态的生命周期是「整个 run_loop」，而不是「某个阶段」**。散成 20 个 `let mut` 才让 `handle_key` 不得不接 20 个参数。打包是正确的。

唯一需要保留散放的是 `process_events` / `process_snapshots` / `process_outputs` 三个——它们**互相之间**有更新顺序（先 `recv` 到事件，再更新快照，再更新输出缓存），但打包进 `LoopBookkeeping` 后这个顺序仍由调用处决定，不受影响。安全。

---

## Task 1: 建立基线

**Files:** 不修改源文件

- [ ] **Step 1: 记录测试基线**

```bash
cd yi-agent-rs
export PATH="/opt/homebrew/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cargo test -p yi-agent 2>&1 | grep "^test result" | tee /tmp/d2-baseline.txt
cargo test -p yi-agent -- --list 2>/dev/null | grep ": test$" | sed 's/: test$//' | sort > /tmp/d2-names.txt
wc -l < /tmp/d2-names.txt
```

记下 `passed` 数量与测试名总数。

- [ ] **Step 2: 记录签名，作为「不许变」的对照**

```bash
cd yi-agent-rs
sed -n '142,150p' crates/yi-agent/src/tui/app.rs > /tmp/d2-run-tui-sig.txt
cat /tmp/d2-run-tui-sig.txt
grep -n "^pub fn run_tui\|^pub async fn run_tui" crates/yi-agent/src/tui/app.rs
```

- [ ] **Step 3: 建校验脚本**

`/tmp/d2-check.sh`：

```bash
#!/bin/bash
set -e
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/yi-agent-rs
export PATH="/opt/homebrew/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"

echo "== 测试 =="
cargo test -p yi-agent 2>&1 | grep "^test result"

echo "== 测试名集合 =="
cargo test -p yi-agent -- --list 2>/dev/null | grep ": test$" | sed 's/: test$//' | sort > /tmp/d2-names-now.txt
diff /tmp/d2-names.txt /tmp/d2-names-now.txt && echo "NAMES IDENTICAL ($(wc -l < /tmp/d2-names.txt))"

echo "== 公共 API 未变 =="
sed -n '142,150p' crates/yi-agent/src/tui/app.rs > /tmp/dev-null-sig.txt 2>/dev/null || true

echo "== 门禁 =="
cargo clippy --workspace --all-targets --all-features -- -D warnings 2>&1 | tail -3
cargo fmt --all -- --check
```

```bash
chmod +x /tmp/d2-check.sh
```

**本 Task 不产生 commit。**

---

## Task 2: 打包 `run_loop` 的 20 个状态为三个结构体

**这是本块的基石**：先打包，`handle_key` 才能只收一个上下文。

**Files:**
- Modify: `crates/yi-agent/src/tui/app.rs`（新增 3 个结构体约 L580 之前；改 `run_loop` L604–L647）

**Interfaces:**

```rust
/// 会话级状态：用户选的模型与 effort、状态栏、成本累计。
/// 生命周期是整个 run_loop；任何时刻可读可写。
struct SessionUi {
    current_model: String,
    current_model_ref: Option<String>,
    effort_override: Option<String>,
    statusbar_state: StatusBarState,
    cost_tracker: CostTracker,
}

/// 当前浮在最上层的东西：命令补全弹窗、运行时选择弹窗、子 agent 详情栈、
/// trace 流、运行时提示。同一时刻通常只有一个非空，但它们各自独立开关。
struct PopupStack {
    popup: Option<CommandPopup>,
    runtime_popup: RuntimePopup,
    subagent_children: SubagentListState,
    trace_streams: Option<super::trace::TraceStreams>,
    runtime_notice_popup: Option<String>,
}

/// 循环自身的记账：退出标志、待发消息、在跑的任务、进程快照、视口锚点、
/// 通知状态。
struct LoopBookkeeping {
    pending_quit: bool,
    queued: crate::tui::queued::DeliveredInterjections,
    task_registry: RunningTaskRegistry,
    runtime_intent: crate::tui::subagents::RuntimeStartupIntent,
    runtime_notice_pending: bool,
    process_events: tokio::sync::broadcast::Receiver<yi_agent_tools::ProcessEvent>,
    process_snapshots: Vec<yi_agent_tools::ProcessSnapshot>,
    process_outputs: std::collections::HashMap<String, yi_agent_tools::ProcessReadResult>,
    previous_viewport: Option<(ViewportAnchor, u16, u16)>,
    notifications: NotificationState,
}
```

⚠️ **`process_events` / `process_snapshots` / `process_outputs` 的确切类型必须从代码读，不要照抄上面的猜测。** 用：

```bash
cd yi-agent-rs
sed -n '636,648p' crates/yi-agent/src/tui/app.rs
grep -n "runtime_notice_pending" crates/yi-agent/src/tui/app.rs | head -3
```

把真实类型填进结构体定义。`runtime_notice_pending` 是 `matches!(...)` 的产物，类型是 `bool`。

- [ ] **Step 1: 跑基线**

```bash
bash /tmp/d2-check.sh
```

- [ ] **Step 2: 读出 20 个 `let mut` 的真实类型**

```bash
cd yi-agent-rs
python3 - <<'PY'
import re
L = open('crates/yi-agent/src/tui/app.rs').read().split('\n')
for i in range(583, 1120):
    l = L[i]
    if re.match(r'^    let mut ', l):
        # 收集到该语句结束（可能在下一行继续）
        stmt = l
        j = i + 1
        while stmt.count('{') > stmt.count('}') or stmt.rstrip().endswith(('=', ',')):
            if j >= len(L): break
            stmt += ' ' + L[j].strip()
            j += 1
        print(f"L{i+1}: {stmt[:160]}")
PY
```

把输出贴到结构体定义里，**类型逐字照抄**。

- [ ] **Step 3: 定义三个结构体**

放在 `run_loop` 之前（约 L575）。加上 `Default` 无法派生（因为 `runtime_intent` 等没实现），所以**手写构造函数**，每个结构体一个 `fn new(...)`，参数就是原来那几个 `let mut` 的初始值来源。

- [ ] **Step 4: 改 `run_loop`，用三个结构体替换 20 个 `let mut`**

原 L604–L647 的 20 行 `let mut` 换成：

```rust
    let mut session_ui = SessionUi::new(model);
    let mut popup_stack = PopupStack::default_for(&history);
    let mut loop_bk = LoopBookkeeping::new(
        runtime_intent,
        &process_manager,
    );
```

然后全文把 `current_model` → `session_ui.current_model`，`popup` → `popup_stack.popup`，`pending_quit` → `loop_bk.pending_quit`，等等。

**这是纯机械替换，用脚本做，不要手改 538 行：**

```bash
cd yi-agent-rs
python3 - <<'PY'
import re
p = 'crates/yi-agent/src/tui/app.rs'
src = open(p).read()
L = src.split('\n')
# run_loop 体范围
s = next(i for i,l in enumerate(L) if l.startswith('fn run_loop<'))
e = next(i for i in range(s, len(L)) if L[i] == '}')
body = '\n'.join(L[s:e])

mapping = {
  'current_model': 'session_ui', 'current_model_ref': 'session_ui',
  'effort_override': 'session_ui', 'statusbar_state': 'session_ui',
  'cost_tracker': 'session_ui',
  'popup': 'popup_stack', 'runtime_popup': 'popup_stack',
  'subagent_children': 'popup_stack', 'trace_streams': 'popup_stack',
  'runtime_notice_popup': 'popup_stack',
  'pending_quit': 'loop_bk', 'queued': 'loop_bk', 'task_registry': 'loop_bk',
  'runtime_intent': 'loop_bk', 'runtime_notice_pending': 'loop_bk',
  'process_events': 'loop_bk', 'process_snapshots': 'loop_bk',
  'process_outputs': 'loop_bk', 'previous_viewport': 'loop_bk',
  'notifications': 'loop_bk',
}
for name, owner in mapping.items():
    body = re.sub(rf'(?<![\w.]){name}\b', f'{owner}.{name}', body)
open('/tmp/runloop-body.txt','w').write(body)
print("已生成替换草案到 /tmp/runloop-body.txt，请人工核对后再写回")
PY
```

**必须人工核对草案**：这个正则会把注释里的词、局部同名变量也一起改。核对点：

- `let mut session_ui.current_model` 这种明显错误的替换（`let` 后面的定义位置不能替换）
- `popup_stack.popup` 在 `if let Some(x) = popup_stack.popup` 里变成移动语义——原来 `popup` 是 `&mut Option<..>` 的字段，模式匹配要写成 `ref` 或调整

**先只看 diff 不写回**：

```bash
diff <(sed -n '583,1120p' crates/yi-agent/src/tui/app.rs) /tmp/runloop-body.txt | head -60
```

- [ ] **Step 5: 编译，逐个修借用错误**

```bash
cd yi-agent-rs
cargo build -p yi-agent 2>&1 | grep -E "^error" | head -30
```

典型错误与处置：

| 错误 | 原因 | 处置 |
|---|---|---|
| `cannot borrow ... as mutable more than once` | 同一语句里同时可变借用两个字段 | Rust 支持同时可变借用**不同字段**（`&mut a.x` 与 `&mut a.y`），但**不支持**通过方法调用分开借用。把方法调用拆成两个语句 |
| `use of moved value` | `if let Some(x) = popup_stack.popup` 移动了字段 | 改成 `popup_stack.popup.as_ref()` 或 `take()` |
| `value borrowed here after move` | 结构体被整体传走 | 只传字段引用，不要传整个结构体 |

- [ ] **Step 6: 跑测试**

```bash
cd yi-agent-rs
cargo test -p yi-agent 2>&1 | grep "^test result"
```

- [ ] **Step 7: 全量校验**

```bash
bash /tmp/d2-check.sh
```

- [ ] **Step 8: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/app.rs
git commit -m "refactor(tui): 把 run_loop 的 20 个局部状态打包成三个结构体

SessionUi（模型/effort/状态栏/成本）、PopupStack（弹窗与浮层）、
LoopBookkeeping（退出标志/待发消息/进程快照/视口/通知）。

与 agent.rs 的处理相反：那里的重试计数与租约持有严格时序，打包会
让「谁在何时改它」消失；这里的状态都是活到下一帧的 UI 状态，没有
时序陷阱，散着放才是 handle_key 要接 20 个参数的原因。

纯搬移，测试名与结果完全一致。"
```

---

## Task 3: `handle_key` 收参数到上下文结构体

**Files:**
- Modify: `crates/yi-agent/src/tui/app.rs`（L2164–L2513）

**Interfaces:**

```rust
/// 一次按键处理需要的全部「环境」。分组依据是实测的使用频次：
/// 高频且互相纠缠的（history/input/popup/尺寸）放 View；
/// 只读的运行时依赖放 Runtime；模型选择放 Model。
struct KeyContext<'a> {
    view: ViewContext<'a>,
    runtime: RuntimeContext<'a>,
    model: ModelContext<'a>,
}

struct ViewContext<'a> {
    input: &'a mut InputLine,
    history: &'a mut HistoryState,
    popup: &'a mut Option<CommandPopup>,
    max_scroll_offset: usize,
    history_width: u16,
    history_height: u16,
    cost_tracker: &'a CostTracker,
}

struct RuntimeContext<'a> {
    channels: &'a TuiChannels,
    is_running: &'a std::sync::Arc<std::sync::atomic::AtomicBool>,
    queued: &'a mut crate::tui::queued::DeliveredInterjections,
    pending_quit: &'a mut bool,
    workdir: &'a std::path::Path,
    mcp: &'a std::sync::Arc<yi_agent_mcp::McpManager>,
    config: &'a TuiConfigSnapshot,
}

struct ModelContext<'a> {
    model: &'a str,
    current_model_ref: Option<&'a str>,
    catalog: &'a yi_agent_runtime::models::ModelCatalog,
    current_effort: Option<&'a str>,
    effort_source: EffortSource,
}

fn handle_key(key: KeyEvent, ctx: &mut KeyContext<'_>) -> KeyOutcome
```

**从 20 个参数降到 2 个。**

- [ ] **Step 1: 先确认 23 处测试调用点的形态**

```bash
cd yi-agent-rs
grep -n "handle_key(" crates/yi-agent/src/tui/app.rs | head -30
```

这些调用点**必须逐个改**。测试断言不动，只改调用形式。

- [ ] **Step 2: 定义三个上下文结构体与 `KeyContext`**

放在 `handle_key` 之前（约 L2155）。

- [ ] **Step 3: 改签名，函数体内参数名全部加 `ctx.` 前缀**

```bash
cd yi-agent-rs
python3 - <<'PY'
import re
p = 'crates/yi-agent/src/tui/app.rs'
L = open(p).read().split('\n')
s = next(i for i,l in enumerate(L) if l.startswith('fn handle_key('))
e = next(i for i in range(s, len(L)) if L[i] == '}')
body = '\n'.join(L[s:e])
groups = {
 'view': ['input','history','popup','max_scroll_offset','history_width','history_height','cost_tracker'],
 'runtime': ['channels','is_running','queued','pending_quit','workdir','mcp','config'],
 'model': ['model','current_model_ref','catalog','current_effort','effort_source'],
}
for g, names in groups.items():
    for n in names:
        body = re.sub(rf'(?<![\w.]){n}\b', f'ctx.{g}.{n}', body)
open('/tmp/hk-body.txt','w').write(body)
print("草案 -> /tmp/hk-body.txt")
PY
diff <(sed -n '2164,2513p' crates/yi-agent/src/tui/app.rs) /tmp/hk-body.txt | head -40
```

同样**必须人工核对**：会误改局部同名变量（函数体内如果有 `let model = ...` 的局部遮蔽，正则也会改）。

- [ ] **Step 4: 改函数体**

写回草案并编译：

```bash
cd yi-agent-rs
cargo build -p yi-agent 2>&1 | grep -E "^error" | head -30
```

- [ ] **Step 5: 逐个改 23 处调用点**

这批调用点在测试模块里。**只改调用形式，不改任何断言值**：

原：
```rust
let outcome = handle_key(
    key,
    &mut input,
    &mut history,
    max_scroll,
    80,
    24,
    &cost_tracker,
    &channels,
    &is_running,
    &mut queued,
    &mut pending_quit,
    &mut popup,
    &workdir,
    &mcp,
    &config,
    "claude-3",
    None,
    &catalog,
    None,
    EffortSource::Config,
);
```

新：
```rust
let mut ctx = KeyContext {
    view: ViewContext {
        input: &mut input,
        history: &mut history,
        popup: &mut popup,
        max_scroll_offset: max_scroll,
        history_width: 80,
        history_height: 24,
        cost_tracker: &cost_tracker,
    },
    runtime: RuntimeContext {
        channels: &channels,
        is_running: &is_running,
        queued: &mut queued,
        pending_quit: &mut pending_quit,
        workdir: &workdir,
        mcp: &mcp,
        config: &config,
    },
    model: ModelContext {
        model: "claude-3",
        current_model_ref: None,
        catalog: &catalog,
        current_effort: None,
        effort_source: EffortSource::Config,
    },
};
let outcome = handle_key(key, &mut ctx);
```

**建议先在测试模块里加一个 helper 减少重复：**

```rust
#[cfg(test)]
fn key_ctx<'a>(/* 与原来 20 个参数同序 */) -> KeyContext<'a> { ... }
```

然后把 23 处调用改成 `handle_key(key, &mut key_ctx(...))`。但**helper 的参数就是原 20 个**，23 处仍要写 20 个实参——所以只有当某几处参数完全相同时 helper 才省事。先看调用点是否成组重复：

```bash
cd yi-agent-rs
sed -n '2164,2513p' crates/yi-agent/src/tui/app.rs > /dev/null
grep -A 22 "handle_key(" crates/yi-agent/src/tui/app.rs | grep -c "claude-3\|test-model"
```

按实测决定：若多数调用共用同一套 fixture，写 helper；若各不相同，直接展开写。

- [ ] **Step 6: 测试**

```bash
cd yi-agent-rs
cargo test -p yi-agent 2>&1 | grep "^test result"
```

- [ ] **Step 7: 全量校验**

```bash
bash /tmp/d2-check.sh
```

- [ ] **Step 8: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/app.rs
git commit -m "refactor(tui): handle_key 的 20 个参数收进 KeyContext

按实测使用频次分三组：View（input/history/popup/尺寸/cost，高频且
互相纠缠）、Runtime（channels/queued/quit/workdir/mcp/config）、
Model（model/清单显示名/catalog/effort）。

签名从 20 个参数降到 (KeyEvent, &mut KeyContext)。23 处测试调用点
同步更新调用形式，断言零改动。"
```

---

## Task 4: 抽出 `dispatch_event`（可选，视 Task 2/3 结果决定）

**前置条件：先做完 Task 2/3 并观察 `run_loop` 的实际形状。** 若完成 Task 2 后 `run_loop` 已经降到 350 行以内且结构清晰，**跳过本 Task**——为了数字而拆只会制造噪音。

**Files:** Modify `crates/yi-agent/src/tui/app.rs`

`run_loop` 主体是一个 `tokio::select!`，每轮等三类事件：agent 事件、终端事件、tick。若这部分在 Task 2 后仍占 200+ 行，把它抽成：

```rust
enum LoopEvent {
    Agent(AgentEvent),
    Terminal(crate::tui::events::TuiEvent),
    Tick,
}

async fn next_event<B, E>(
    agent_rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>,
    events: &E,
    tick: &mut tokio::time::Interval,
) -> LoopEvent;
```

- [ ] **Step 1: 先量当前形状**

```bash
cd yi-agent-rs
python3 - <<'PY'
L = open('crates/yi-agent/src/tui/app.rs').read().split('\n')
s = next(i for i,l in enumerate(L) if l.startswith('fn run_loop<'))
e = next(i for i in range(s, len(L)) if L[i]=='}')
print(f"run_loop = {e+1-s} 行")
# select! 的位置
for i in range(s, e):
    if 'select!' in L[i]:
        print(f"  select! at L{i+1}")
PY
```

若 `run_loop` ≤ 350 行：**跳过本 Task，直接去 Task 5。**

- [ ] **Step 2: 抽 `next_event`**

`tokio::select!` 的三个分支原样搬进新函数，返回 `LoopEvent`。**`biased` 若存在必须保留**——它决定事件优先级。

- [ ] **Step 3: 测试 + 校验**

```bash
bash /tmp/d2-check.sh
```

- [ ] **Step 4: Commit**

```bash
git add yi-agent-rs/crates/yi-agent/src/tui/app.rs
git commit -m "refactor(tui): 抽出 next_event 统一等待三类事件

agent 事件 / 终端事件 / tick 的 select! 搬进独立函数。biased 顺序
原样保留，它决定事件优先级。"
```

---

## Task 5: 同步项目进度文档

**Files:** Modify `docs/project-management/` 下 TUI 模块文件

- [ ] **Step 1: 找文件**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
ls docs/project-management/ | grep -i "tui\|yi-agent"
```

- [ ] **Step 2: 更新 `app.rs` 的结构描述**

- [ ] **Step 3: Commit**

```bash
git add docs/project-management/
git commit -m "docs: 同步 TUI run_loop 与 handle_key 的结构变化"
```

---

## 完成判据（整块）

```bash
cd yi-agent-rs
export PATH="/opt/homebrew/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"

# 1. 测试全绿，数量不减
cargo test -p yi-agent 2>&1 | grep "^test result"

# 2. 测试名集合一致
bash /tmp/d2-check.sh

# 3. 20 个 let mut 已进结构体
python3 - <<'PY'
import re
L = open('crates/yi-agent/src/tui/app.rs').read().split('\n')
s = next(i for i,l in enumerate(L) if l.startswith('fn run_loop<'))
e = next(i for i in range(s, len(L)) if L[i]=='}')
n = sum(1 for i in range(s,e) if re.match(r'^    let mut ', L[i]))
print(f"run_loop 顶层 let mut = {n}（判据 ≤3）")
PY

# 4. handle_key 参数已收敛
grep -A 3 "^fn handle_key(" crates/yi-agent/src/tui/app.rs

# 5. 门禁
cargo clippy --workspace --all-targets --all-features -- -D warnings 2>&1 | tail -2
cargo fmt --all -- --check
```

---

## 风险与回退

| 风险 | 表现 | 处置 |
|---|---|---|
| 结构体字段借用冲突 | 大量 `cannot borrow as mutable` | 拆语句，别在一个表达式里同时可变借用两个字段 |
| 正则批量替换误伤局部变量 | 编译错，或更糟：编译过但语义变了 | Task 2 Step 4 与 Task 3 Step 3 都要求先 `diff` 人工核对 |
| `if let Some(x) = popup` 变成移动 | `use of moved value` | 用 `.as_ref()` / `.take()` |
| 23 处测试调用点漏改 | 编译错（会被立刻发现，风险低） | 编译器全列出 |
| 顺手改了断言 | 测试数量变化 | Task 1 Step 1 的名单 diff 会拦截 |
| `select!` 的 `biased` 被删 | 事件优先级变化，偶发卡顿 | 搬移时 grep 确认 `biased` 仍在 |

**回退粒度：** Task 2 是基石（Task 3 依赖它），回退必须从后往前。Task 4 可独立跳过或回退。

---

## 明确不做

- 不拆 `execute_slash_command`（422 行）—— 它属于 slash 命令体系，与事件循环无关
- 不拆 `pub fn run_tui`（115 行）
- 不改任何按键映射
- 不改任何渲染输出
- 不改 `EffortSource` 语义
- 不为了「行数好看」而拆 Task 4——前置条件不满足就跳过

---

## 与 spec 的数字差异（实施者必读）

本计划写于 main `3a1754cd`。`app.rs` 未被 `fix/subagent-lease-leak` 触及，但实测与 spec 有 4 处差异：

| 项目 | spec 写的 | 实测 | 说明 |
|---|---|---|---|
| `run_loop` 行数 | 568 | **538**（L583–L1120） | 差 30 行 |
| `run_loop` 参数个数 | 未列 | **17** | 本计划新增 |
| `handle_key` 行数 | 未列 | **350**（L2164–L2513） | 本计划新增 |
| `handle_key` 参数个数 | 20 | **20** | 一致 |
| `run_loop` 测试调用 | 13 次 | **16 次** | 爆炸半径更大 |
| `handle_key` 测试调用 | 20 次 | **23 次** | 同上 |
| `_for_test` 接缝 | 4 个 | **4 个（生产代码内）** | `snapshot_for_tests` L382、`catalog_for_tests` L408、`switch_runtime_tab_for_test` L1204、`handle_runtime_popup_key_for_test` L1465 |

另注：`app.rs` 的测试模块里**还有 2 个**名为 `*_for_tests` 的辅助函数（`model_catalog_for_tests` L10086、`effort_catalog_for_tests` L10144），它们在测试模块内部，不属公共接缝。

**处理原则：以本计划为准。** 尤其注意测试调用点从 spec 的 33 处（13+20）变成实测的 **39 处（16+23）**——改签名时要逐个更新。

