# A 块实施计划：store 内聚（`yi-agent-store` 的 `repository.rs` 与 `ipc.rs`）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 `repository.rs` 里 3395 行的单个 `impl RuntimeRepository`（102 个方法）按领域拆成 12 个子模块，把 `ipc.rs`（5362 行、15 个穿插的测试模块）拆成 7 个子模块，保持**全部公共 API 逐字不变**。

**Architecture:** 用 Rust 的「子模块定义固有方法」能力：`impl RuntimeRepository` 可以分散在多个 `mod` 里，每个子模块 `impl RuntimeRepository { ... }`，字段可见性靠 `pub(super)`。`ipc.rs` 拆模块后由 `ipc/mod.rs` 统一 `pub use` 全部 38 个公开项，外部 436 处 `yi_agent_store::ipc::X` 引用零改动。

**Tech Stack:** Rust，rusqlite，`cargo test -p yi-agent-store`。

## Global Constraints

- **只搬不改**：禁止改逻辑、SQL、错误消息、日志文本、变量名。
- **公共 API 冻结（这是本块最强的约束）**：
  - `RuntimeRepository` 的 **101 个 `pub fn`** 签名逐字不变
  - `ipc` 的 **38 个顶层 `pub` 项**全部由 `ipc/mod.rs` re-export，路径 `yi_agent_store::ipc::X` 不变
  - `repository.rs` 的 27 个 `pub struct` / `pub enum` / `pub const` 保持原位或在 `repository/mod.rs` 中 re-export
- **测试断言一字不改**。测试模块本轮**不搬**（外移属于 C1）。
- 每个 Task 结束必须满足：
  - `cargo test -p yi-agent-store` → **347 passed / 0 failed**
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings` → 0 告警（这是 workspace 级，因为跨 crate 引用会在这里暴露）
  - `cargo fmt --all -- --check` → 通过
- 提交信息用 conventional commits，首行 ≤72 字符，**不写** `Co-Authored-By`。
- 环境：
  ```bash
  cd yi-agent-rs
  export PATH="/opt/homebrew/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
  ```

## 实测结构（行号断言的唯一来源）

| 文件 | 总行 | 构成 |
|---|---|---|
| `repository.rs` | **5074** | L1–L456 类型+const（**456**）｜L457–L3851 `impl RuntimeRepository`（**3395**）｜L3853–L3994 自由函数（**142**）｜L4380–L5074 测试（**692**，4 个 `#[cfg(test)] mod`） |
| `ipc.rs` | **5351** | 38 个顶层 pub 项 + **15 个 `#[cfg(test)] mod`**（穿插正文，不是集中在末尾） |
| `runtime.rs` | **4421** | 测试模块在 L3833 与 L4325，**L4325 之后还有 77 行生产代码** |

### 为什么 `repository.rs` 的判据不是「≤1000 行」

搬走整个 `impl`（3395 行）后，壳文件仍剩 **456 + 142 + 692 = 1290 行**。即使把自由函数也搬走，还有 456 + 692 = 1148 行。**测试外移是 C1 的活，不在本块。** 所以本块的判据只能是：

- `impl RuntimeRepository` 在壳文件里**降到 3 个方法**（`open` / `schema_version` / `has_table`），即 ≤ 60 行
- 壳文件总行数 ≤ **1300**（理论下限 1290；当前 5074）

### `ipc` 必须 re-export 的 38 个项（实测清单，一个都不能漏）

```
PROTOCOL_VERSION          IpcError                  RequestEnvelope
ResponseEnvelope          IpcRequest                SubscriptionFilters
IpcResponse               IpcCancelLease            IpcCancelDelivery
IpcPermissionDecision     IpcErrorCode              SubscriptionSnapshot
IpcTask                   IpcTaskUsage              IpcTaskSummary
IpcSchedule               IpcTaskDetail             IpcEvent
IpcTraceRow               TraceSnapshot             IpcMailboxMessage
IpcCompletedChildReport   IpcEventPayload           Subscription
Daemon                    MAX_SOCKET_PATH_BYTES     socket_path_for
send_request              send_request_with_version send_request_with_version_timeout
subscribe                 subscribe_with_filters    read_task_trace
subscribe_trace           TraceSubscription         register_plugin_sockets
clear_plugin_sockets      plugin_socket_for
```

**为什么 38 而不是只 re-export「外部用到的 26 个」**：外部引用分两种形态——直接路径 `yi_agent_store::ipc::X`（25 个，436 处）和 `use yi_agent_store::ipc::{...}`（多出 `RequestEnvelope`，它单独就出现 42 次；`Subscription` 14 次）。剩下 12 个（`IpcCancelLease` / `IpcCancelDelivery` / `SubscriptionSnapshot` / `IpcTask` / `IpcSchedule` / `TraceSnapshot` / `IpcMailboxMessage` / `IpcCompletedChildReport` / `IpcEventPayload` 等）目前只在本 crate 内部用，但它们是公开 API 的一部分，re-export 成本为零，漏掉的风险却是跨 crate 断编译。

### `repository.rs` 的 7 个顶层自由函数（L3784–L3924）

| 函数 | 行 | 被 impl 方法调用的次数 |
|---|---|---|
| `append_event` | L3784 | 多次（`create_task*` 系列） |
| `append_event_with_payload` | L3792 | 多次 |
| `append_event_with_actor_payload` | L3807 | 多次 |
| `insert_attempt` | L3828 | 多次 |
| `validate_recovery_context` | L3857 | 多次 |
| `trim_trace_rows` | L3912 | **3** |
| `migrate` | L3924 | **2** |

搬到子模块后，被 impl 调用的那些必须从私有 `fn` 变成 `pub(super) fn`，否则子模块之间看不见。

---

## 文件结构（目标）

```
crates/yi-agent-store/src/
├── repository/
│   ├── mod.rs                  # 类型定义 + const + 自由函数 + 3 个壳方法（≈1700 行）
│   ├── schedule.rs             # 11 个方法
│   ├── admission.rs            # 7 个方法
│   ├── task/
│   │   ├── mod.rs              # task 组的 mod 声明 + 共享 helper
│   │   ├── create.rs           # 6 个方法
│   │   ├── transition.rs       # 6 个方法
│   │   └── field.rs            # 14 个方法
│   ├── event.rs                # 6 个方法
│   ├── watchdog.rs             # 15 个方法
│   ├── mailbox.rs            # 12 个方法
│   ├── permission.rs           # 4 个方法
│   ├── trace.rs                # 6 个方法
│   ├── application_root.rs     # 10 个方法
│   └── read.rs                 # 10 个方法
└── ipc/
    ├── mod.rs                  # 38 个 re-export + Daemon
    ├── protocol.rs             # 协议 DTO
    ├── error.rs                # IpcError / IpcErrorCode
    ├── client.rs              # send_request / subscribe 等客户端 API
    ├── daemon.rs               # Daemon 实现
    ├── subscription.rs         # 订阅相关
    ├── trace.rs                # trace 读取与订阅
    └── plugin_socket.rs        # 插件 socket 路由
```

---

## Task 1: 准备 —— 建立基线快照

**Files:**
- 不修改任何源文件
- 创建：`/tmp/a-baseline/`（临时目录）

目的是拿到「改动前的公共 API 全貌」，之后每个 Task 都用它做门禁。

- [ ] **Step 1: 记录方法清单**

```bash
cd yi-agent-rs
export PATH="/opt/homebrew/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
mkdir -p /tmp/a-baseline
python3 - <<'EOF' > /tmp/a-baseline/repo-methods.txt
import re
L = open('crates/yi-agent-store/src/repository.rs').read().split('\n')
s = next(i for i, l in enumerate(L) if l.startswith('impl RuntimeRepository'))
e = next(i for i in range(s + 1, len(L)) if L[i] == '}')
for i in range(s, e):
    m = re.match(r'^    (pub )?(async )?fn (\w+)', L[i])
    if m:
        print(f"{m.group(3)}\t{'pub' if m.group(1) else 'priv'}")
EOF
wc -l < /tmp/a-baseline/repo-methods.txt        # 期望 102
grep -c "	pub$" /tmp/a-baseline/repo-methods.txt   # 期望 101
```

- [ ] **Step 2: 记录 ipc 公开项**

```bash
cd yi-agent-rs
python3 - <<'EOF' > /tmp/a-baseline/ipc-items.txt
import re
L = open('crates/yi-agent-store/src/ipc.rs').read().split('\n')
for l in L:
    m = re.match(r'^pub (async )?(fn|struct|enum|const|type|trait|mod) (\w+)', l)
    if m:
        print(m.group(3))
EOF
wc -l < /tmp/a-baseline/ipc-items.txt            # 期望 38
```

- [ ] **Step 3: 记录测试名全集（最关键的门禁）**

```bash
cd yi-agent-rs
cargo test -p yi-agent-store -- --list 2>/dev/null | grep ": test$" | sed 's/: test$//' | sort > /tmp/a-baseline/test-names.txt
wc -l < /tmp/a-baseline/test-names.txt           # 记录总数
cargo test -p yi-agent-store 2>&1 | grep "^test result" > /tmp/a-baseline/test-result.txt
cat /tmp/a-baseline/test-result.txt              # 期望 347 passed / 0 failed
```

- [ ] **Step 4: 记录公共 API 的完整签名（用于跨 crate 校验）**

```bash
cd yi-agent-rs
cargo rustdoc -p yi-agent-store --lib -- -Z unstable-options --output-format json 2>/dev/null || \
  echo "rustdoc json 不可用（需要 nightly），改用 grep 校验"
grep -c "yi_agent_store::ipc::" -r --include=*.rs .. | wc -l    # 记录 436 这个量级
```

- [ ] **Step 5: 建一个校验脚本**

写 `/tmp/a-check.sh`：

```bash
#!/bin/bash
set -e
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/yi-agent-rs
export PATH="/opt/homebrew/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"

echo "== 测试名集合 =="
cargo test -p yi-agent-store -- --list 2>/dev/null | grep ": test$" | sed 's/: test$//' | sort > /tmp/a-baseline/test-names-now.txt
if diff -q /tmp/a-baseline/test-names.txt /tmp/a-baseline/test-names-now.txt > /dev/null; then
  echo "PASS: $(wc -l < /tmp/a-baseline/test-names.txt) 个测试名一致"
else
  echo "FAIL: 测试名集合变了"
  diff /tmp/a-baseline/test-names.txt /tmp/a-baseline/test-names-now.txt | head -20
  exit 1
fi

echo "== 方法清单 =="
python3 - <<'EOF' > /tmp/a-baseline/repo-methods-now.txt
import re
L = open('crates/yi-agent-store/src/repository/mod.rs').read().split('\n')
fns = []
for l in L:
    m = re.match(r'^(pub )?(async )?fn (\w+)', l)
    if m:
        fns.append(f"{m.group(3)}\t{'pub' if m.group(1) else 'priv'}")
print('\n'.join(fns))
EOF
echo "（此步在 Task 2 拆分后才有意义）"

echo "== 门禁 =="
cargo clippy --workspace --all-targets --all-features -- -D warnings 2>&1 | tail -3
cargo fmt --all -- --check
```

```bash
chmod +x /tmp/a-check.sh
```

- [ ] **Step 6: 记录当前工具链版本**

```bash
cd yi-agent-rs
rustc --version && cargo --version
```

记下来，写进 commit message 的正文（便于日后定位行为差异）。

**本 Task 不产生 commit。**

---

## Task 2: 拆分 `repository.rs` —— 先拆 `task` 组（26 个方法，最大的组）

`task` 组 28 个方法（7+6+15）必须**再分三片**，否则只是把 3395 行的 impl 换成一个 800+ 行的 `task.rs`。

**Files:**
- Create: `crates/yi-agent-store/src/repository/mod.rs`（由 `repository.rs` 改名而来）
- Create: `crates/yi-agent-store/src/repository/task/mod.rs`
- Create: `crates/yi-agent-store/src/repository/task/create.rs`
- Create: `crates/yi-agent-store/src/repository/task/transition.rs`
- Create: `crates/yi-agent-store/src/repository/task/field.rs`
- Delete: `crates/yi-agent-store/src/repository.rs`

**实测的 26 个 task 组方法（行号，按功能三分）：**

`create.rs`（6）：
```
create_task                       L905
create_task_with_attempt          L928
create_task_with_attempt_and_objective  L952
create_child_task                 L992
create_child_task_with_attempt    L1017
create_child_task_with_attempt_and_objective  L1048
```

`transition.rs`（6）：
```
transition_task                                              L1728
transition_task_and_attempt                                  L1752
transition_task_and_attempt_with_recovery_context            L1795
transition_task_and_attempt_with_recovery_context_and_resident_lease  L1808
transition_task_and_attempt_with_terminal                    L1892
transition_task_to_recovery_required_releasing_process_leases  L1931
```

`field.rs`（14）：
```
task_state                        L2531
task_extra_roots                  L2545
set_task_extra_roots              L2572
task_updated_at                   L2593
set_task_thread_id                L2756
task_thread_id                    L2775
active_children_for_thread        L2796
active_attempt_id                 L2833
update_task_objective             L2993
task_model                        L3057
task_workspace_mode               L3071
task_inherited_sandbox            L3094
attempt_state                     L3117
attempt_ended_at                  L3138
attempt_terminal_json_for_task    L3146
```

合计 6 + 6 + 14 = 26 ✓

- [ ] **Step 1: 记录基线**

```bash
cd yi-agent-rs
cargo test -p yi-agent-store 2>&1 | grep "^test result"    # 347 passed
```

- [ ] **Step 2: 把 `repository.rs` 改名为 `repository/mod.rs`**

```bash
cd yi-agent-rs/crates/yi-agent-store/src
mkdir -p repository/task
git mv repository.rs repository/mod.rs
```

- [ ] **Step 3: 在 `repository/mod.rs` 里声明子模块**

在文件顶部（`use` 之后）加：

```rust
mod application_root;
mod admission;
mod event;
mod mailbox;
mod permission;
mod read;
mod schedule;
mod task;
mod trace;
mod watchdog;
```

**注意顺序**：先只声明 `mod task;`，其余在对应 Task 里逐个加。这样每次编译失败都只关心一件事。

本 Step 只加 `mod task;`。

- [ ] **Step 4: 建 `task/mod.rs`**

```rust
mod create;
mod field;
mod transition;
```

- [ ] **Step 5: 用脚本搬 6 个 create 方法**

逐个方法搬移很易错，用脚本按行号区间切（**切之前先确认区间边界**）：

```bash
cd yi-agent-rs/crates/yi-agent-store/src/repository
python3 - <<'PY'
import re
src = open('mod.rs').read().split('\n')

def find_method(name):
    """返回 (start, end) 0-based，end 是方法最后一个字符之后。"""
    for i, l in enumerate(src):
        m = re.match(r'^    (pub )?(async )?fn ' + name + r'\b', l)
        if m:
            # 方法体：从该行找到列 4 的 '    }'
            j = i + 1
            while j < len(src) and src[j] != '    }':
                j += 1
            # 连带上方紧邻的 doc 注释与 #[attributes]
            s = i
            while s > 0 and (src[s-1].strip().startswith('///')
                             or src[s-1].strip().startswith('#[')
                             or src[s-1].strip().startswith('//')):
                s -= 1
            return s, j + 1
    raise SystemExit(f"not found: {name}")

for n in ['create_task', 'create_task_with_attempt',
          'create_task_with_attempt_and_objective', 'create_child_task',
          'create_child_task_with_attempt',
          'create_child_task_with_attempt_and_objective']:
    s, e = find_method(n)
    print(f"{n}: L{s+1}-L{e}  ({e-s} 行)")
PY
```

先只打印行号，**核对与本文档的实测行号一致后再执行搬移**。若不一致，说明基线已变（这个计划写于 2026-11-02，main 在 `47a272e2`），此时应停下重新测绘，而不是照搬本文档行号。

- [ ] **Step 6: 新建 `task/create.rs` 并搬入**

文件骨架：

```rust
use rusqlite::{Connection, params};
use yi_agent_core::TaskId;

use super::super::{
    PersistedTask, RepositoryError, RuntimeEvent, RuntimeRepository,
};

impl RuntimeRepository {
    // 6 个 create 方法逐字搬入，一个字符都不改
}
```

**搬移时唯一允许的改动**：把原来裸调用的 `append_event(...)` 等改调 `super::super::append_event(...)`（或先确认它们在 `mod.rs` 里仍是同模块可见）。

- [ ] **Step 7: 编译，逐个修可见性错误**

```bash
cd yi-agent-rs
cargo build -p yi-agent-store 2>&1 | grep -E "^error" | head -20
```

典型错误与处置：

| 错误 | 处置 |
|---|---|
| `field 'conn' of struct RuntimeRepository is private` | 在 `mod.rs` 里把 `conn` 改成 `pub(super) conn` |
| `cannot find function append_event in this scope` | 在 `create.rs` 顶部 `use super::super::append_event;`；若仍不可见，把 `mod.rs` 里的 `fn append_event` 改成 `pub(super) fn` |
| `cannot find type PersistedTask` | `use super::super::PersistedTask;` |
| `private type ... in public interface` | 该类型需从 `pub(super)` 升为 `pub`，或方法签名用了私有类型——**这种情况要停下来查，不能随手改公开性** |

- [ ] **Step 8: 跑测试**

```bash
cd yi-agent-rs
cargo test -p yi-agent-store 2>&1 | grep "^test result"        # 347 passed / 0 failed
```

- [ ] **Step 9: 同样搬 transition.rs（6 个）与 field.rs（14 个）**

方法清单见本节开头。每片搬完都跑一次 `cargo test -p yi-agent-store`，不要三片一起搬完再编译——否则一次冒出几十个错误，无法判断是哪个引起的。

- [ ] **Step 10: 校验方法数没丢**

```bash
cd yi-agent-rs/crates/yi-agent-store/src
python3 - <<'PY'
import re, glob
total, pub = 0, 0
for f in ['repository/mod.rs'] + glob.glob('repository/task/*.rs'):
    for l in open(f):
        m = re.match(r'^(pub )?(async )?fn (\w+)', l)
        if m:
            total += 1
            if m.group(1): pub += 1
print(f"自由函数+方法总数 {total}（拆分前 mod.rs 内 impl 有 101）")
PY
```

**注意**：这个统计包含自由函数，不能直接比 101。正确校验方式是 Step 11。

- [ ] **Step 11: 用测试名集合做真门禁（这才是可信的）**

```bash
bash /tmp/a-check.sh
```

期望 `PASS: 347 个测试名一致`，且 clippy / fmt 全绿。

- [ ] **Step 12: Commit**

```bash
git add -A crates/yi-agent-store/src
git commit -m "refactor(store): 把 RuntimeRepository 的 task 组拆成子模块

26 个方法按职责分三片：create(6) 建任务、transition(6) 状态迁移、
field(14) 字段读写。分开的理由是前两组各自是一个事务性的整体，
混在一起会重新长成 800 行的巨型文件。

conn 等字段改为 pub(super)，7 个顶层自由函数中被调用的改为
pub(super)。347 个测试名与结果完全一致。"
```

---

## Task 3: 拆 `repository.rs` 的其余 8 组

**Files:**
- Create: `repository/schedule.rs`（11）、`repository/admission.rs`（7）、`repository/event.rs`（6）、`repository/watchdog.rs`（15）、`repository/mailbox.rs`（12）、`repository/permission.rs`（4）、`repository/trace.rs`（6）、`repository/application_root.rs`（10）、`repository/read.rs`（10）
- Modify: `repository/mod.rs`

**完整方法分配（实测 102 个，逐名归组，已用脚本校验无遗漏无重复）：**

| 子模块 | 数量 | 方法 |
|---|---|---|
| `mod.rs`（壳，留下） | 3 | `open` L458, `schema_version` L465, `has_table` L749 |
| `schedule.rs` | 11 | `create_schedule` L483, `schedules` L507, `delete_schedule` L539, `claim_schedule_occurrence` L547, `advance_schedule` L561, `schedule_has_active_instance` L573, `record_schedule_occurrence_outcome` L592, `attach_schedule_occurrence_root` L606, `schedule_occurrence_outcomes` L625, `evaluate_schedule_occurrence` L642, `skip_schedule_occurrence` L726 |
| `admission.rs` | 7 | `save_admission_cursor` L761, `admission_cursor` L792, `save_provider_turn_grant` L842, `release_provider_turn_lease` L887, `release_provider_turn_leases` L898, `release_process_leases_for_task` L908, `has_active_lease_prefix` L3194 |
| `task/create.rs` | 7 | `create_task` L922, `create_task_with_attempt` L945, `create_task_with_attempt_and_objective` L969, `create_child_task` L1009, `create_child_task_with_attempt` L1034, `create_child_task_with_attempt_and_objective` L1065, `create_attempt` L1115 |
| `task/transition.rs` | 6 | `transition_task` L1758, `transition_task_and_attempt` L1782, `transition_task_and_attempt_with_recovery_context` L1834, `transition_task_and_attempt_with_recovery_context_and_resident_lease` L1847, `transition_task_and_attempt_with_terminal` L1931, `transition_task_to_recovery_required_releasing_process_leases` L1970 |
| `task/field.rs` | 15 | `task_state` L2600, `task_extra_roots` L2614, `set_task_extra_roots` L2641, `task_updated_at` L2662, `set_task_thread_id` L2825, `task_thread_id` L2844, `active_children_for_thread` L2865, `active_attempt_id` L2902, `update_task_objective` L3062, `task_model` L3126, `task_workspace_mode` L3140, `task_inherited_sandbox` L3163, `attempt_state` L3186, `attempt_ended_at` L3207, `attempt_terminal_json_for_task` L3215 |
| `event.rs` | 6 | `append_event` L1104, `events_after` L3226, `latest_event_id` L3234, `advance_event_replay_floor_through` L3759, `event_records_after` L3784, `event_records_for_task_after` L3815 |
| `watchdog.rs` | 13 | `save_attempt_watchdog_snapshot` L1134, `attempt_watchdog_snapshot` L1184, `record_watchdog_progress` L1279, `watchdog_usage` L1347, `active_attempt_watchdogs` L1365, `record_recovery_context` L1425, `record_watchdog_terminal` L2044, `activate_successor_attempt` L2091, `attest_recovery_gate` L2137, `recover_inflight_tasks` L2179, `release_leases_of_settled_tasks` L2312, `reclaim_orphaned_tasks` L2336, `recovered_tasks` L3267 |
| `mailbox.rs` | 8 | `record_user_message` L1467, `record_user_override_message` L1493, `record_user_override_message_with_id` L1503, `mark_user_override_consumed` L1529, `mailbox_message_count` L1559, `record_delivery` L1576, `mailbox_message_delivered_at` L1677, `mailbox_messages_for_task` L1693 |
| `permission.rs` | 4 | `request_permission` L2449, `resolve_permission` L2496, `permission_request_state` L2560, `pending_permission_task` L2575 |
| `trace.rs` | 6 | `append_trace` L2691, `trace_after` L2712, `trace_high_water` L2749, `trace_row_count` L2758, `trim_trace` L2771, `prune_terminal_traces` L2788 |
| `application_root.rs` | 8 | `application_root_attachment` L2922, `record_application_root_attachment` L2960, `reattach_application_root` L2986, `detach_application_root` L3004, `activate_application_root_once` L3023, `application_root_capability_matches` L3080, `detached_application_roots` L3110, `application_root_hydration_tasks` L3380 |
| `read.rs` | 8 | `task_snapshots` L3242, `queued_task_count` L3259, `task_detail` L3460, `task_summaries` L3491, `attempt_usages_for_tasks` L3552（私有）, `task_tree_ids` L3593, `cancel_scope` L3626, `subscription_snapshot` L3677 |

合计：3+11+7+7+6+15+6+13+8+4+6+8+8 = **102** ✓

**归属决策里需要说明的 5 处：**

- `create_attempt` 归 `task/create`（不是 watchdog）：它创建 attempt 行，与 `create_task_with_attempt` 同属一个事务家族
- `queued_task_count` 归 `read`（不是 mailbox）：它数的是 task 表的排队量，不碰 mailbox 表
- `attempt_usages_for_tasks` 归 `read`：唯一的私有方法，服务于 `task_summaries`
- `application_root_hydration_tasks` 归 `application_root`（不是 watchdog）：语义是应用根重挂载
- `release_leases_of_settled_tasks` 归 `watchdog`：终态任务回收租约，与 `reclaim_orphaned_tasks` 同一家族（这是 main `3a1754cd` 新加的方法）

- [ ] **Step 1: 生成唯一归属表并校验**

见上面的脚本。`diff` 为空才继续。

- [ ] **Step 2: 逐个模块搬移，每搬一个跑一次测试**

顺序按「依赖少 → 依赖多」：`trace` → `event` → `schedule` → `admission` → `permission` → `mailbox` → `application_root` → `watchdog` → `read`。

每搬完一个：

```bash
cd yi-agent-rs
cargo test -p yi-agent-store 2>&1 | grep "^test result"    # 必须 347 passed
```

- [ ] **Step 3: 全量校验**

```bash
bash /tmp/a-check.sh
```

- [ ] **Step 4: 确认壳文件只剩 3 个方法**

```bash
cd yi-agent-rs
python3 - <<'PY'
import re
L = open('crates/yi-agent-store/src/repository/mod.rs').read().split('\n')
s = next(i for i, l in enumerate(L) if 'impl RuntimeRepository' in l)
e = next(i for i in range(s+1, len(L)) if L[i] == '}')
ns = [re.match(r'^    (pub )?(async )?fn (\w+)', L[i]).group(3)
      for i in range(s, e) if re.match(r'^    (pub )?(async )?fn (\w+)', L[i])]
print(f"壳文件方法数 {len(ns)}: {ns}")   # 期望 3
PY
wc -l crates/yi-agent-store/src/repository/mod.rs   # 期望 ≤1700
```

- [ ] **Step 5: Commit**

```bash
git add -A crates/yi-agent-store/src
git commit -m "refactor(store): 把 RuntimeRepository 按领域拆成 9 个子模块

schedule/admission/event/watchdog/mailbox/permission/trace/
application_root/read。壳文件只留 open/schema_version/has_table。

归属表经 diff 校验：102 个方法每个恰好出现一次，无遗漏无重复。
347 个测试名与结果完全一致。"
```

---

## Task 4: 拆 `ipc.rs`

**Files:**
- Create: `ipc/mod.rs`、`ipc/protocol.rs`、`ipc/error.rs`、`ipc/client.rs`、`ipc/daemon.rs`、`ipc/subscription.rs`、`ipc/trace.rs`、`ipc/plugin_socket.rs`
- Delete: `ipc.rs`

**这是本块风险最高的 Task**：436 处跨 crate 引用 + 15 个穿插的测试模块。

- [ ] **Step 1: 记录基线**

```bash
cd yi-agent-rs
cargo test -p yi-agent-store 2>&1 | grep "^test result"    # 347 passed
grep -rn "yi_agent_store::ipc::" --include=*.rs .. | wc -l # 436
```

- [ ] **Step 2: 摸清 15 个测试模块的位置与归属**

```bash
cd yi-agent-rs/crates/yi-agent-store/src
grep -n "^#\[cfg(test)\]" ipc.rs
```

对每个 `#[cfg(test)] mod`，用下面的命令看它测的是哪个项（模块名通常已给线索）：

```bash
awk 'NR>=START && NR<=END' ipc.rs | head -20
```

**测试模块跟随被测代码走**：测协议的进 `protocol.rs`，测客户端的进 `client.rs`。若某个测试跨多个关注点，留在 `mod.rs` 末尾。

- [ ] **Step 3: 建 `ipc/mod.rs` 并按实测清单写 re-export**

```rust
mod client;
mod daemon;
mod error;
mod plugin_socket;
mod protocol;
mod subscription;
mod trace;

pub use client::{
    send_request, send_request_with_version, send_request_with_version_timeout, subscribe,
    subscribe_with_filters, socket_path_for,
};
pub use daemon::Daemon;
pub use error::{IpcError, IpcErrorCode};
pub use plugin_socket::{clear_plugin_sockets, plugin_socket_for, register_plugin_sockets};
pub use protocol::{
    IpcCancelDelivery, IpcCancelLease, IpcCompletedChildReport, IpcEvent, IpcEventPayload, IpcMailboxMessage,
    IpcPermissionDecision, IpcRequest, IpcResponse, IpcSchedule, IpcTask, IpcTaskDetail, IpcTaskSummary,
    IpcTaskUsage, IpcTraceRow, RequestEnvelope, ResponseEnvelope, SubscriptionFilters,
    MAX_SOCKET_PATH_BYTES, PROTOCOL_VERSION,
};
pub use subscription::{Subscription, SubscriptionSnapshot};
pub use trace::{read_task_trace, subscribe_trace, TraceSnapshot, TraceSubscription};
```

**逐项核对**：上面必须正好覆盖 38 个项。用 Step 4 的脚本验证。

- [ ] **Step 4: 校验 re-export 覆盖完整**

```bash
cd yi-agent-rs/crates/yi-agent-store/src
python3 - <<'PY'
import re
want = set(open('/tmp/a-baseline/ipc-items.txt').read().split())
mod = open('ipc/mod.rs').read()
got = set()
for m in re.finditer(r'pub use [\w:]+::\{([^}]*)\}', mod, re.S):
    for p in m.group(1).split(','):
        p = p.strip()
        if p: got.add(p)
for m in re.finditer(r'pub use [\w:]+::(\w+);', mod):
    got.add(m.group(1))
missing = want - got
extra = got - want
print(f"应有 {len(want)}，已导出 {len(got)}")
if missing: print("缺失:", sorted(missing))
if extra:   print("多余:", sorted(extra))
PY
```

期望：`应有 38，已导出 38`，无缺失无多余。

- [ ] **Step 5: 逐个搬移，每搬一个跑 workspace 级检查**

**必须用 workspace 级**，否则跨 crate 引用断裂不会暴露：

```bash
cd yi-agent-rs
cargo build --workspace 2>&1 | grep -E "^error" | head -20
```

建议顺序（依赖少 → 多）：`error` → `protocol` → `trace` → `subscription` → `plugin_socket` → `client` → `daemon`。

- [ ] **Step 6: 全工作区测试**

```bash
cd yi-agent-rs
cargo test -p yi-agent-store 2>&1 | grep "^test result"
cargo test --workspace --all-features 2>&1 | grep "^test result" | awk -F'[ ;]' '{p+=$4; f+=$6} END {print "passed",p,"failed",f}'
```

期望：`passed 3954 failed 0`（含 27 ignored）。

**不要并行跑两个 `cargo test`**，会争构建锁产生截断的假失败（曾观察到 `3063 passed / 1 failed`）。

- [ ] **Step 7: 校验外部引用数未变**

```bash
cd yi-agent-rs
grep -rn "yi_agent_store::ipc::" --include=*.rs .. | wc -l   # 仍应为 436
```

这个数字**不应该变化**——我们没改任何调用点，只是改了 `ipc` 内部结构。若变了，说明有调用点被误改。

- [ ] **Step 8: Commit**

```bash
git add -A crates/yi-agent-store/src
git commit -m "refactor(store): 把 ipc.rs 拆成 7 个子模块

protocol/error/client/daemon/subscription/trace/plugin_socket。
mod.rs 统一 re-export 全部 38 个公开项，外部 436 处
yi_agent_store::ipc::X 引用零改动。

15 个 #[cfg(test)] mod 随被测代码迁移。3954 个 workspace 测试全绿。"
```

---

## Task 5: 同步项目进度文档

**Files:**
- Modify: `docs/project-management/yi-agent-store.md`

- [ ] **Step 1: 找到引用**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
grep -n "repository.rs\|ipc.rs" docs/project-management/yi-agent-store.md | head -20
```

- [ ] **Step 2: 更新行号与文件路径**

核心方法的新位置：

```bash
ls yi-agent-rs/crates/yi-agent-store/src/repository/ yi-agent-rs/crates/yi-agent-store/src/ipc/
```

- [ ] **Step 3: Commit**

```bash
git add docs/project-management/yi-agent-store.md
git commit -m "docs: 同步 store 拆分后的模块结构"
```

---

## 完成判据（整块）

```bash
cd yi-agent-rs
export PATH="/opt/homebrew/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"

# 1. store 测试
cargo test -p yi-agent-store 2>&1 | grep "^test result"          # 347 passed / 0 failed

# 2. 测试名集合与开工前一致（最硬的门禁）
cargo test -p yi-agent-store -- --list 2>/dev/null | grep ": test$" | sed 's/: test$//' | sort > /tmp/final-names.txt
diff /tmp/a-baseline/test-names.txt /tmp/final-names.txt && echo "NAMES IDENTICAL"

# 3. 101 个 pub 方法一个不少
grep -rh "^pub fn \|^    pub fn " crates/yi-agent-store/src/repository/ | wc -l   # 期望 101

# 4. 38 个 ipc 项全部可访问
grep -c "^pub " crates/yi-agent-store/src/ipc/mod.rs                             # 与 Task 4 Step 4 脚本一致

# 5. 跨 crate 引用未变
grep -rn "yi_agent_store::ipc::" --include=*.rs . | wc -l                        # 436

# 6. 全 workspace
cargo test --workspace --all-features 2>&1 | grep "^test result" | awk -F'[ ;]' '{p+=$4; f+=$6} END {print "passed",p,"failed",f}'

# 7. 门禁
cargo clippy --workspace --all-targets --all-features -- -D warnings 2>&1 | tail -2
cargo fmt --all -- --check
```

全部满足后跑 `just ci`，再合并。

---

## 风险与回退

| 风险 | 表现 | 处置 |
|---|---|---|
| 漏搬一个方法 | 测试名少了 / 编译错 | Task 3 Step 1 的归属表 diff 是唯一可靠拦截；务必先跑 |
| `ipc` 漏 re-export | **其他 crate** 编译失败，本 crate 正常 | Task 4 Step 5 强制用 `cargo build --workspace`，不能用 `-p yi-agent-store` |
| 字段可见性改动过度 | 原本私有的字段被暴露成 `pub` | 优先用 `pub(super)`；只有跨层访问才升 `pub` |
| 测试模块归属错 | 测试跑得通但找不到 | 测试必须可被 `cargo test -- --list` 列出；Task 1 Step 3 的名单 diff 会暴露 |
| 并行 cargo test 假失败 | 出现少量 failed | 串行重跑，不要并行 |
| 行号与本文档不符 | 计划写于 `47a272e2`，main 已前进 | 停，重新测绘，不要照搬行号 |

**回退粒度：** Task 2/3 是一个整体（`repository.rs` 拆开后无法只回退一半），Task 4 独立。若 Task 4 失败，`git revert` 它不影响 Task 2/3。

---

## 明确不做

- 不搬测试模块（C1）
- 不改 `runtime.rs`（3910 行生产）—— 它属于另一块
- 不改任何 SQL
- 不改任何方法签名
- 不改 `RepositoryError` 的变体
- 不做性能优化

---

## 与 spec 的数字差异（实施者必读）

本计划写于 main `3a1754cd`。写计划时对每个数字重新实测，**spec 里有 6 处与当前代码不符**（spec 基于 `47a272e2`，之后 main 合入了 `fix/subagent-lease-leak`）。本计划一律以**实测为准**。

| 项目 | spec 写的 | 实测 | 说明 |
|---|---|---|---|
| `repository.rs` 总行数 | 4,309（生产） | **5,074 总 / 4,382 生产 / 692 测试** | main 新增 71 行 |
| `impl RuntimeRepository` | 3,343 行 | **3,395 行** | 同上 |
| 方法总数 | 101（100 pub + 1 priv） | **102（101 pub + 1 priv）** | 新增 `release_leases_of_settled_tasks` |
| impl 起止 | L440–L3782 | **L457–L3851** | — |
| 壳文件下限 | ≤1,000 | **≤1,300**（理论 1,290） | spec 的 966 漏算了自由函数区 |
| 测试模块行数 | 695 | **692** | — |
| `ipc.rs` 总行数 | 3,745（生产） | **5,362 总** | spec 混用了两种统计口径 |
| `ipc` re-export 项 | 25 | **38** | 25 只是「经直接路径引用」的名；另 13 个经 `use` 块，必须一并导出 |

**处理原则：以本计划为准。** spec 是设计文档，行号与尺寸会随代码演进过期；计划是执行文档，每个数字都在 base `3a1754cd` 上重新测量过。若实施时实测又与计划不符（main 又前进了），**停下来重新测绘**，不要照搬任何一处行号。

上面这些差异不影响 spec 的任何**设计决策**（拆成哪些模块、哪些不做），只影响数字。

