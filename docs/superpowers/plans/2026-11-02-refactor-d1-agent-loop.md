# D1 实施计划：agent 主循环拆分（`yi-agent-core/src/agent.rs`）

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 `run_loop()` 里 6 处逐字重复的取消序列收敛为一个 `abort_run()`，再把 `run_loop` 的 THINK / ACT / OBSERVE 三段搬成独立函数，使主循环从 714 行降到 200 行以内。

**Architecture:** 纯函数抽取，零逻辑变更。Task 1 消除重复（净减 24 行，语义完全等价）；Task 2/3 搬移代码（总行数不降，只是重新分布）。**关键设计约束：阶段状态用显式参数传递，不打包成 `&mut LoopState`**——`stall_retries` 与 `provider_turn_lease` 的作用域是「单次 THINK 尝试」，把它们提成跨阶段状态会改变重试预算与租约释放时机的正确性。

**Tech Stack:** Rust（edition 见 `crates/yi-agent-core/Cargo.toml`），tokio，`cargo test -p yi-agent-core`。

## Global Constraints

- **只搬不改**：禁止改逻辑、改文案、改变量名、改错误消息、改日志文本。
- **公共 API 冻结**：`Agent` 的 `pub` 方法签名与 `AgentEvent` 变体逐字不变。
- 测试模块**不搬**、断言**不改**（测试外移属于 C1，不在本计划）。
- 每个 Task 结束必须满足：
  - `cargo test -p yi-agent-core --lib` → **309 passed / 0 failed**
  - `cargo clippy --all-targets --all-features -- -D warnings` → 0 告警（`html5ever` 的 future-incompat 提示不算，那是依赖的）
  - `cargo fmt --all -- --check` → 通过
- 提交信息用 conventional commits，首行 ≤72 字符，**不写** `Co-Authored-By`。
- 环境（`cargo` 不在默认 PATH）：
  ```bash
  cd yi-agent-rs
  export PATH="/opt/homebrew/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
  ```

## 实测结构（这是全部行号断言的来源，已逐行核对）

| 结构 | 行范围 | 行数 |
|---|---|---|
| `async fn run_loop` 整体 | L933–L1646 | **714** |
| 循环体 `loop {` | L955 | — |
| ├ Check 1 + auto-compact + turn 计数 + `inject_pending` + 请求日志 | L956–L1029 | 74 |
| ├ `// 1. THINK` | **L1031–L1356** | **326** |
| ├ `// 2. Termination check` | **L1358–L1391** | **34** |
| ├ `// 3. ACT` | **L1392–L1621** | **230** |
| └ `// 4. OBSERVE` | **L1623–L1644** | **22** |

**`run_loop` 之外的三个相关局部变量，位置与本计划的处理方式：**

| 变量 | 声明 | 使用 | 处理 |
|---|---|---|---|
| `messages: Vec<Message>` | L946 | THINK 内 L1038/1041 读、L1291/1329 写；Termination L1359 读；OBSERVE L1643 写 | `think_phase` 收 `&mut`；`observe_phase` 收 `&mut` |
| `last_logged: usize` | L949 | L981 归零、L1026 读、L1029 设值，**全在 THINK 之前** | **不动**，留在 `run_loop` |
| `turn: u32` | L947 | 全线使用 | 按值传给各阶段（仅用于日志） |

**THINK 段内的两个局部变量，绝对不要外提：**

| 变量 | 位置 | 为什么不能外提 |
|---|---|---|
| `stall_retries: u16` | 声明于 L1063，在 THINK 段内 | 它是**单轮**的重试计数，每轮清零。提成跨轮状态会让重试预算永不重置 |
| `provider_turn_lease` | 声明于 L1065，`loop` 内每轮新建 | 租约必须在退避 sleep **之前** `drop(provider_turn_lease)`（L1129）。外提会拖长持有时间 |

**`end` 与 `last_usage` 不外传**：`last_usage` 只在 L1261 消费，`end` 只在 L1275–L1313 消费，两者都在 THINK 段内处理完毕。`ThinkOutcome` 只需要 `content`。

**⚠️ 两处日志文本相同但含义不同，必须保留区分：**
- L1133：provider 拒绝（429/5xx/网络）的重试退避 — `"agent loop cancelled during retry backoff"`
- L1245：流停滞（idle stall / request deadline）的重试退避 — `"agent loop cancelled during retry backoff"`

两者文本碰巧一样，但属于两个不同的重试家族。若合并成一个 `reason` 常量，将来无法从日志区分是哪种退避被打断。Task 1 保留它们各自的调用（文本相同但调用点分离）。

## 基线（执行前记录，结束时对比）

```bash
cd yi-agent-rs
export PATH="/opt/homebrew/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"

cargo test -p yi-agent-core --lib 2>&1 | grep "^test result"          # 期望 309 passed / 0 failed
cargo test -p yi-agent-core --lib -- --list | sort > /tmp/core-names-before.txt
wc -l < /tmp/core-names-before.txt                                    # 记录总数
cargo test -p yi-agent-core --lib -- --list | grep -i cancel | sed 's/.*:://' | sort > /tmp/cancel-before.txt
wc -l < /tmp/cancel-before.txt                                        # 期望 11
```

## 定位脚本（每个 Task 开始前先跑，不要用上一轮的硬编码行号）

搬移会改变所有后续行号，所以**每个 Task 都用标记重新定位**。把这段存成 `/tmp/loc.py`：

```python
import sys
p = 'crates/yi-agent-core/src/agent.rs'
L = open(p).read().split('\n')
s = next(i for i, l in enumerate(L) if l.startswith('async fn run_loop'))
e = next(i for i in range(s + 1, len(L)) if L[i] == '}')
marks = {}
for i in range(s, e):
    t = L[i].strip()
    for key, tag in [('think', '// 1. THINK'), ('term', '// 2. Termination check'),
                     ('act', '// 3. ACT'), ('obs', '// 4. OBSERVE')]:
        if t.startswith(tag):
            marks[key] = i + 1
print(f"run_loop   L{s+1}-L{e+1}  = {e+1-s} 行")
print(f"THINK      L{marks['think']}-L{marks['term']-1} = {marks['term']-marks['think']} 行")
print(f"TERM       L{marks['term']}-L{marks['act']-1} = {marks['act']-marks['term']} 行")
print(f"ACT        L{marks['act']}-L{marks['obs']-1} = {marks['obs']-marks['act']} 行")
print(f"OBSERVE    L{marks['obs']}-L{e} = {e+1-marks['obs']} 行")
```

---

## File Structure

| 文件 | 职责 | 本计划的作用 |
|---|---|---|
| `crates/yi-agent-core/src/agent.rs` | `Agent` 类型、主循环、已提取的阶段助手 | 修改：新增 `abort_run` / `think_phase` / `act_phase` / `observe_phase`，缩减 `run_loop` |
| `docs/project-management/yi-agent-core.md` | 模块进度与代码位置索引 | 修改：同步行号引用 |

**不新建文件。** 拆文件属于 C 线，不在本计划。

---

## Task 1: 收敛 6 处取消序列为 `abort_run`

**Files:**
- Modify: `crates/yi-agent-core/src/agent.rs`

**Interfaces:**
- Consumes: `safe_cancel_truncate_len(&Session) -> usize`（L812）、`flush_unconsumed(&EventTx, &Option<InboxHandle>)`（L921）、`EventTx`（L867）、`AgentEvent::Cancelled`
- Produces: `async fn abort_run(tx: &EventTx, inbox: &Option<InboxHandle>, session: &Arc<Mutex<Session>>, turn: u32, reason: &'static str)`

**6 处调用点（实测，`safe_cancel_truncate_len(&session` 的行号）：** `971 / 1134 / 1163 / 1190 / 1248 / 1615`

- [ ] **Step 1: 记录基线**

```bash
cd yi-agent-rs
grep -n "safe_cancel_truncate_len(&session" crates/yi-agent-core/src/agent.rs
```

期望 6 行，行号如上。

- [ ] **Step 2: 新增 `abort_run`**

在 `flush_unconsumed` 函数结束后、`run_loop` 之前插入（用 `grep -n "^async fn flush_unconsumed\|^async fn run_loop"` 定位插入点）：

```rust
/// The single stop path for every cancellation point that has an assistant
/// message in flight: roll the session back to its last consistent boundary,
/// drain queued interjections so they are not silently lost, tell the
/// consumer, and let the caller `return`.
///
/// The six call sites differ only in their `tracing` text — and two of them
/// share a spelling on purpose while belonging to different retry families
/// (provider rejection vs. stream stall) — so `reason` is a parameter rather
/// than something decided here. The rollback length comes from
/// [`safe_cancel_truncate_len`], which already encodes "keep the run's user
/// prompt and every completed round-trip".
async fn abort_run(
    tx: &EventTx,
    inbox: &Option<InboxHandle>,
    session: &Arc<Mutex<Session>>,
    turn: u32,
    reason: &'static str,
) {
    info!(turn, "{reason}");
    let keep = safe_cancel_truncate_len(&session.lock().unwrap());
    session.lock().unwrap().truncate(keep);
    flush_unconsumed(tx, inbox).await;
    let _ = tx.send(AgentEvent::Cancelled).await;
}
```

- [ ] **Step 3: 替换第 1 处（auto-compact 中取消，原 L966–L974）**

原文：

```rust
            info!(turn, "agent loop cancelled during auto-compact");
            let keep = safe_cancel_truncate_len(&session.lock().unwrap());
            session.lock().unwrap().truncate(keep);
            flush_unconsumed(&tx, &inbox).await;
            let _ = tx.send(AgentEvent::Cancelled).await;
            return;
```

改为：

```rust
            abort_run(
                &tx,
                &inbox,
                &session,
                turn,
                "agent loop cancelled during auto-compact",
            )
            .await;
            return;
```

- [ ] **Step 4: 替换第 2 处（provider 拒绝的重试退避中取消，原 L1132–L1139）**

原文：

```rust
                                    _ = cancel_token.cancelled() => {
                                        info!(turn, "agent loop cancelled during retry backoff");
                                        let keep = safe_cancel_truncate_len(&session.lock().unwrap());
                                        session.lock().unwrap().truncate(keep);
                                        flush_unconsumed(&tx, &inbox).await;
                                        let _ = tx.send(AgentEvent::Cancelled).await;
                                        return;
                                    }
```

改为：

```rust
                                    _ = cancel_token.cancelled() => {
                                        abort_run(
                                            &tx,
                                            &inbox,
                                            &session,
                                            turn,
                                            "agent loop cancelled during retry backoff",
                                        )
                                        .await;
                                        return;
                                    }
```

- [ ] **Step 5: 替换第 3 处（provider 调用中取消，原 L1161–L1168）**

原文：

```rust
                    info!(turn, "agent loop cancelled during provider call");
                    let keep = safe_cancel_truncate_len(&session.lock().unwrap());
                    session.lock().unwrap().truncate(keep);
                    flush_unconsumed(&tx, &inbox).await;
                    let _ = tx.send(AgentEvent::Cancelled).await;
                    return;
```

改为：

```rust
                    abort_run(
                        &tx,
                        &inbox,
                        &session,
                        turn,
                        "agent loop cancelled during provider call",
                    )
                    .await;
                    return;
```

- [ ] **Step 6: 替换第 4 处（THINK 中取消，原 L1188–L1195）**

原文：

```rust
                    info!(turn, "agent loop cancelled during think");
                    let keep = safe_cancel_truncate_len(&session.lock().unwrap());
                    session.lock().unwrap().truncate(keep);
                    flush_unconsumed(&tx, &inbox).await;
                    let _ = tx.send(AgentEvent::Cancelled).await;
                    return;
```

改为：

```rust
                    abort_run(
                        &tx,
                        &inbox,
                        &session,
                        turn,
                        "agent loop cancelled during think",
                    )
                    .await;
                    return;
```

- [ ] **Step 7: 替换第 5 处（流停滞的重试退避中取消，原 L1244–L1253）**

原文（**注意它带一段两行注释，要一起搬走**）：

```rust
                        _ = cancel_token.cancelled() => {
                            info!(turn, "agent loop cancelled during retry backoff");
                            // Same policy as the other cancel points: keep the
                            // run's user prompt and completed round-trips.
                            let keep = safe_cancel_truncate_len(&session.lock().unwrap());
                            session.lock().unwrap().truncate(keep);
                            flush_unconsumed(&tx, &inbox).await;
                            let _ = tx.send(AgentEvent::Cancelled).await;
                            return;
                        }
```

改为（注释移到调用之前）：

```rust
                        _ = cancel_token.cancelled() => {
                            // Same policy as the other cancel points: keep the
                            // run's user prompt and completed round-trips.
                            abort_run(
                                &tx,
                                &inbox,
                                &session,
                                turn,
                                "agent loop cancelled during retry backoff",
                            )
                            .await;
                            return;
                        }
```

- [ ] **Step 8: 替换第 6 处（ACT 中取消，原 L1607–L1620）**

原文：

```rust
            _ = cancel_token.cancelled() => {
                info!(turn, "agent loop cancelled during act");
                // Cancel during ACT: the session holds this run's user prompt,
                // every completed round-trip, and a trailing assistant(tool_use)
                // whose results were never observed. Dropping that last message
                // keeps the transcript valid (no unpaired tool_use) while
                // preserving the completed work.
                let keep = safe_cancel_truncate_len(&session.lock().unwrap());
                session.lock().unwrap().truncate(keep);
                flush_unconsumed(&tx, &inbox).await;
                let _ = tx.send(AgentEvent::Cancelled).await;
                return;
            }
```

改为（**保留那段解释「为什么丢弃最后一条是安全的」的注释**——它说明 ACT 阶段回滚的正确性，是有价值的设计说明）：

```rust
            _ = cancel_token.cancelled() => {
                // Cancel during ACT: the session holds this run's user prompt,
                // every completed round-trip, and a trailing assistant(tool_use)
                // whose results were never observed. Dropping that last message
                // keeps the transcript valid (no unpaired tool_use) while
                // preserving the completed work.
                abort_run(
                    &tx,
                    &inbox,
                    &session,
                    turn,
                    "agent loop cancelled during act",
                )
                .await;
                return;
            }
```

- [ ] **Step 9: 验证替换完整**

```bash
cd yi-agent-rs
grep -c "safe_cancel_truncate_len(&session" crates/yi-agent-core/src/agent.rs   # 期望 1（abort_run 内）
grep -c "abort_run(" crates/yi-agent-core/src/agent.rs                          # 期望 7（1 定义 + 6 调用）
```

- [ ] **Step 10: 测试 + 门禁**

```bash
cd yi-agent-rs
cargo test -p yi-agent-core --lib 2>&1 | grep "^test result"                   # 309 passed / 0 failed
cargo test -p yi-agent-core --lib -- --list | grep -i cancel | sed 's/.*:://' | sort > /tmp/cancel-after1.txt
diff /tmp/cancel-before.txt /tmp/cancel-after1.txt && echo "CANCEL IDENTICAL"
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -3
cargo fmt --all -- --check
```

- [ ] **Step 11: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/agent.rs
git commit -m "refactor(core): 收敛 run_loop 里 6 处重复的取消序列

每处都是「回滚 session → 冲刷 interjection → 发 Cancelled → return」，
逐字相同，只差日志文本，抽成 abort_run。

两处 retry backoff 的日志文本相同但属于不同重试家族（provider 拒绝 vs
流停滞），保持为各自独立的调用点，不合并成常量。

净减 24 行，取消语义不变：11 个 cancel 测试名与结果完全一致。"
```

---

## Task 2: 抽取 `think_phase`

**Files:**
- Modify: `crates/yi-agent-core/src/agent.rs`

**Interfaces:**
- Consumes: `Arc<dyn Provider>`、`Arc<ToolRegistry>`、`AgentConfig`、`CancellationToken`、`Option<Arc<dyn ProviderTurnGate>>`、`EventTx`、`Option<InboxHandle>`、`Arc<Mutex<Session>>`
- Produces:
  ```rust
  async fn think_phase(
      tx: &EventTx,
      inbox: &Option<InboxHandle>,
      session: &Arc<Mutex<Session>>,
      provider: &Arc<dyn Provider>,
      tools: &Arc<ToolRegistry>,
      config: &AgentConfig,
      cancel_token: &CancellationToken,
      provider_turn_gate: &Option<Arc<dyn ProviderTurnGate>>,
      messages: &mut Vec<Message>,
      turn: u32,
  ) -> Option<Vec<ContentBlock>>   // None = 已停机（事件已发出、session 已回滚），调用方直接 return
  ```

**为什么不返回 `end` / `last_usage` / `stall_retries`：** 实测三者在 THINK 段内就消费完毕（`last_usage` 于 L1261、`end` 于 L1275–L1313 的 `stop_reason` 处理），且 `stall_retries` 是单轮局部计数。返回它们会诱导调用方建立不该存在的跨轮状态。

- [ ] **Step 1: 重新定位边界**

```bash
cd yi-agent-rs
python3 /tmp/loc.py
```

记录 THINK 与 TERM 的行号（搬移前的第一轮应为 THINK L1031–L1356、TERM L1358–L1391）。

- [ ] **Step 2: 记录基线**

```bash
cd yi-agent-rs
cargo test -p yi-agent-core --lib -- --list | sort > /tmp/core-before2.txt
```

- [ ] **Step 3: 用脚本抽出原段，不要手工重打 326 行**

```bash
cd yi-agent-rs
python3 - <<'EOF'
p='crates/yi-agent-core/src/agent.rs'
L=open(p).read().split('\n')
s=next(i for i,l in enumerate(L) if l.strip().startswith('// 1. THINK'))
e=next(i for i in range(s,len(L)) if L[i].strip().startswith('// 2. Termination check'))
open('/tmp/think_body.rs','w').write('\n'.join(L[s:e]))
print(f"抽出 L{s+1}-L{e}，共 {e-s} 行 -> /tmp/think_body.rs")
EOF
wc -l /tmp/think_body.rs
```

- [ ] **Step 4: 对抽出的段做机械变换，写进新函数**

新函数插到 `abort_run` 之后。函数体 = `/tmp/think_body.rs` 的内容，套在 `Some(...)` 返回里。

**机械变换清单（共 4 条，除此外一个字都不改）：**

1. **裸露的 `return;` → `return None;`**。THINK 段内共 **17 处** `return;`（实测行号 1071/1079/1125/1138/1149/1151/1167/1181/1183/1194/1240/1252/1280/1309/1325/1345/1354，搬移后行号会变，用 `grep -n '^\s*return;'` 在 `/tmp/think_body.rs` 里数）。**唯一例外**：`abort_run(...).await; return;` 那 4 处（原 1138/1167/1194/1252）也要改成 `return None;`——它们同样是「已停机」。
2. **3 处 `continue;` 保持不变**（原 1141/1255/1334）。它们在 THINK 的 `loop { }` 内部，`continue` 作用于该内部循环，不是 `run_loop`。**这是最容易出错的地方：不要改成别的。**
3. **参数去引用符**：函数体内原来写 `&tx` / `&inbox` / `&session` / `&provider` / `&tools` / `&config` / `&cancel_token` / `&provider_turn_gate` 的地方，因为现在拿到的是引用，改成直接 `tx` / `inbox` / `session` / `provider` / `tools` / `config` / `cancel_token` / `provider_turn_gate`。
4. **段尾返回 `content`**：原段结尾是 `let (content, end, last_usage) = loop { ... };` 加上 `stop_reason` 处理。处理完后**追加一行**：

   ```rust
   Some(content)
   ```

   并删掉不再需要的 `end` / `last_usage` 绑定（它们已在段内消费）。若编译器提示未使用，改成 `let (content, _end, _last_usage)` 会破坏「不改代码」的约束——优先检查是否真的没用到，`last_usage` 在 L1261 用到，`end` 在 L1275 起用到，所以绑定本身必须保留，只是不要把它们放进返回值。

- [ ] **Step 5: `run_loop` 里换成一次调用**

原 THINK 段（从 `// 1. THINK` 注释起，到 `// 2. Termination check` 之前）整段替换为：

```rust
        // 1. THINK
        let Some(content) = think_phase(
            &tx,
            &inbox,
            &session,
            &provider,
            &tools,
            &config,
            &cancel_token,
            &provider_turn_gate,
            &mut messages,
            turn,
        )
        .await
        else {
            return;
        };
```

**注意：`// 2. Termination check` 段原样保留在 `run_loop` 里**，它读 `content`、含 `continue`（回到外层 `loop`）与 `return`，语义上是主循环的中枢，抽出去会让控制流变成两个函数的隐式契约。

- [ ] **Step 6: 编译并修可见性**

```bash
cd yi-agent-rs
cargo build -p yi-agent-core 2>&1 | head -40
```

常见报错与处置：
- `use of moved value: messages` → 检查是否漏了 `&mut messages`
- `cannot find type ContentBlock` → 同文件内已导入，若报错说明段内用了别的类型，按提示补 `use`

- [ ] **Step 7: 测试**

```bash
cd yi-agent-rs
cargo test -p yi-agent-core --lib 2>&1 | grep "^test result"      # 309 passed / 0 failed
```

- [ ] **Step 8: 确认测试名集合完全未变**

```bash
cd yi-agent-rs
cargo test -p yi-agent-core --lib -- --list | sort > /tmp/core-after2.txt
diff /tmp/core-before2.txt /tmp/core-after2.txt && echo "NAMES IDENTICAL"
```

- [ ] **Step 9: 确认 THINK 段确实搬走了**

```bash
cd yi-agent-rs
python3 /tmp/loc.py
```

期望：`run_loop` 从 714 行降到约 388 行（714 − 326 + 13 行调用点）。

- [ ] **Step 10: 门禁**

```bash
cd yi-agent-rs
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -3
cargo fmt --all -- --check
```

- [ ] **Step 11: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/agent.rs
git commit -m "refactor(core): 把 THINK 阶段抽成 think_phase

单轮 THINK（租约 → provider 调用 → 重试退避 → 流累积 → stop_reason
处理）整段搬进独立函数，run_loop 减少 326 行。

stall_retries 与 provider_turn_lease 保持为函数内局部：前者是单轮重试
计数，后者必须在退避前 drop，外提会破坏这两条时序。messages 用 &mut
显式传入，last_logged 留在 run_loop（它只在 THINK 之前使用）。

纯搬移，309 个测试名与结果完全一致。"
```

---

## Task 3: 抽取 `act_phase` 与 `observe_phase`

**Files:**
- Modify: `crates/yi-agent-core/src/agent.rs`

**Interfaces:**
- Consumes: `checked_uses: Vec<(String, String, Value)>`（ACT 内构造）、`denied_results: Vec<(String, ToolResult)>`（ACT 内构造）、`tool_call_tokens`、权限相关参数
- Produces:
  ```rust
  async fn act_phase(
      tx: &EventTx,
      inbox: &Option<InboxHandle>,
      session: &Arc<Mutex<Session>>,
      tools: &Arc<ToolRegistry>,
      tool_call_tokens: &ToolCallTokens,
      permission_checker: &Option<Arc<crate::permission::PermissionChecker>>,
      decision_rx: &Option<DecisionRx>,
      cancel_token: &CancellationToken,
      tool_uses: Vec<(String, String, Value)>,
      turn: u32,
  ) -> Option<Vec<(String, Option<crate::tool::ToolResult>)>>   // None = 取消

  fn observe_phase(
      messages: &mut Vec<Message>,
      session: &Arc<Mutex<Session>>,
      results: Vec<(String, Option<crate::tool::ToolResult>)>,
      denied_results: Vec<(String, crate::tool::ToolResult)>,
  )
  ```

**实测类型（不要凭印象改）：**
- `denied_results: Vec<(String, ToolResult)>` — **无 Option**，一律是已构造的拒绝结果
- `join_all` 的产出 `Vec<(String, Option<ToolResult>)>` — **有 Option**，`None` 表示「事件通道已断，这个工具的结果不再回传」
- `observe_phase` 里的 `filter_map` 依赖那个 `None`。**绝对不能提前 flatten**，否则「通道断开」和「工具失败」会被混为一谈。

- [ ] **Step 1: 重新定位边界**

```bash
cd yi-agent-rs
python3 /tmp/loc.py
```

记录 ACT 与 OBSERVE 的行号。

- [ ] **Step 2: 记录基线**

```bash
cd yi-agent-rs
cargo test -p yi-agent-core --lib -- --list | sort > /tmp/core-before3.txt
```

- [ ] **Step 3: 抽出 ACT 段**

```bash
cd yi-agent-rs
python3 - <<'EOF'
p='crates/yi-agent-core/src/agent.rs'
L=open(p).read().split('\n')
s=next(i for i,l in enumerate(L) if l.strip().startswith('// 3. ACT'))
e=next(i for i in range(s,len(L)) if L[i].strip().startswith('// 4. OBSERVE'))
open('/tmp/act_body.rs','w').write('\n'.join(L[s:e]))
print(f"抽出 L{s+1}-L{e}，共 {e-s} 行 -> /tmp/act_body.rs")
EOF
```

- [ ] **Step 4: 对 ACT 段做机械变换，写进 `act_phase`**

**变换清单（3 条）：**

1. **移出 `denied_results` 的声明**：原 L1397 `let mut denied_results: Vec<(String, ToolResult)> = Vec::new();` 留在函数内（它属于 ACT）。
2. **取消分支改 `return None`**：原 L1607–1620 的 `select!` 取消臂，`abort_run(...).await; return;` → `abort_run(...).await; return None;`
3. **段尾返回 `Some(results)`**：原段结尾是 `let results = tokio::select! { ... };`，把 `denied_results` 与 `results` 一起返回：

   ```rust
   Some((results, denied_results))
   ```

   返回类型写 `Option<(Vec<(String, Option<crate::tool::ToolResult>)>, Vec<(String, crate::tool::ToolResult)>)>`。

   若两元素元组读起来别扭，可定义一个局部结构体 `ActOutcome { results, denied }`——但**不要**把 `results` 的类型简化掉 `Option`。

4. **`info_span!` / `.instrument()` 原样保留**（原 L1496–L1602 的工具调用包装），它们决定了 trace 里每个工具调用的 span 归属。

- [ ] **Step 5: 抽出 OBSERVE 段，写进 `observe_phase`**

```bash
cd yi-agent-rs
python3 - <<'EOF'
p='crates/yi-agent-core/src/agent.rs'
L=open(p).read().split('\n')
s=next(i for i,l in enumerate(L) if l.strip().startswith('// 4. OBSERVE'))
# OBSERVE 是 run_loop 的最后一段，结尾是列 0 的 '}'
e=next(i for i in range(s,len(L)) if L[i]=='}')
open('/tmp/observe_body.rs','w').write('\n'.join(L[s:e]))
print(f"抽出 L{s+1}-L{e}，共 {e-s} 行 -> /tmp/observe_body.rs")
EOF
```

`observe_phase` 是**同步函数**（无 `await`）。原段 22 行逐字搬入，仅把 `results` / `denied_results` 改成参数名，其余不动。

- [ ] **Step 6: `run_loop` 里替换 ACT + OBSERVE**

原 ACT + OBSERVE 两段（从 `// 3. ACT` 到 `run_loop` 结束）替换为：

```rust
        // 3. ACT - permission check + parallel execution
        let Some((results, denied_results)) = act_phase(
            &tx,
            &inbox,
            &session,
            &tools,
            &tool_call_tokens,
            &permission_checker,
            &decision_rx,
            &cancel_token,
            tool_uses,
            turn,
        )
        .await
        else {
            return;
        };

        // 4. OBSERVE - feed results back in tool_use_id order
        observe_phase(&mut messages, &session, results, denied_results);
```

- [ ] **Step 7: 编译**

```bash
cd yi-agent-rs
cargo build -p yi-agent-core 2>&1 | head -40
```

- [ ] **Step 8: 测试 + 名称 diff**

```bash
cd yi-agent-rs
cargo test -p yi-agent-core --lib 2>&1 | grep "^test result"      # 309 passed / 0 failed
cargo test -p yi-agent-core --lib -- --list | sort > /tmp/core-after3.txt
diff /tmp/core-before3.txt /tmp/core-after3.txt && echo "NAMES IDENTICAL"
```

- [ ] **Step 9: 验证 `run_loop` 降到 200 行以内**

```bash
cd yi-agent-rs
python3 /tmp/loc.py
```

期望：`run_loop` ≤ 200 行（714 − 326 − 230 − 22 + 约 40 行调用点 ≈ 176）。

- [ ] **Step 10: 验证生产代码总行数**

```bash
cd yi-agent-rs
python3 - <<'EOF'
L=open('crates/yi-agent-core/src/agent.rs').read().rstrip('\n').split('\n')
i=next(k for k,l in enumerate(L) if l.strip()=='#[cfg(test)]')
print(f"生产代码 {i} 行（判据 ≤ 1900）")
EOF
```

- [ ] **Step 11: 门禁**

```bash
cd yi-agent-rs
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -3
cargo fmt --all -- --check
```

- [ ] **Step 12: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/agent.rs
git commit -m "refactor(core): 把 ACT 与 OBSERVE 阶段抽成独立函数

run_loop 现在只剩主循环骨架（约 176 行）：THINK 取回复、Termination
判断、ACT 执行工具、OBSERVE 回填结果，每段生命周期与状态都只在段内存活。

ActOutcome.results 保留 Option：原代码用 filter_map 过滤 None（表示事件
通道已断），提前 flatten 会让「通道断开」和「工具失败」混为一谈。

纯搬移，309 个测试名与结果完全一致。"
```

---

## Task 4: 同步项目进度文档

**Files:**
- Modify: `docs/project-management/yi-agent-core.md`

- [ ] **Step 1: 找到现有引用**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
grep -n "agent.rs" docs/project-management/yi-agent-core.md | head -20
```

- [ ] **Step 2: 取拆分后的真实行号**

```bash
grep -n "async fn run_loop\|async fn think_phase\|async fn act_phase\|fn observe_phase\|async fn abort_run" \
  yi-agent-rs/crates/yi-agent-core/src/agent.rs
```

- [ ] **Step 3: 按输出更新文档里的 `file.rs:line` 引用**

只改行号与函数名，不改任何功能描述文字。

- [ ] **Step 4: Commit**

```bash
git add docs/project-management/yi-agent-core.md
git commit -m "docs: 同步 agent.rs 拆分后的代码位置引用"
```

---

## 完成判据（整块）

```bash
cd yi-agent-rs
export PATH="/opt/homebrew/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"

# 1. 测试全绿且数量不减
cargo test -p yi-agent-core --lib 2>&1 | grep "^test result"                    # 309 passed / 0 failed

# 2. 测试名集合与开工前完全一致
cargo test -p yi-agent-core --lib -- --list | sort > /tmp/core-final.txt
diff /tmp/core-names-before.txt /tmp/core-final.txt && echo "ALL NAMES IDENTICAL"

# 3. 取消语义零变化
cargo test -p yi-agent-core --lib -- --list | grep -i cancel | sed 's/.*:://' | sort > /tmp/cancel-final.txt
diff /tmp/cancel-before.txt /tmp/cancel-final.txt && echo "CANCEL IDENTICAL"     # 11 个

# 4. 重复序列已收敛
grep -c "safe_cancel_truncate_len(&session" crates/yi-agent-core/src/agent.rs    # 期望 1
grep -c "abort_run(" crates/yi-agent-core/src/agent.rs                          # 期望 7

# 5. 结构指标
python3 /tmp/loc.py                                                              # run_loop ≤ 200

# 6. 门禁
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -2
cargo fmt --all -- --check
```

全部满足后，在 worktree 里跑 `just ci`，再 `git merge --no-ff` 到 `main`。

---

## 风险与回退

| 风险 | 表现 | 处置 |
|---|---|---|
| THINK 内 3 处 `continue` 被误改 | 编译通过但重试逻辑失效，`agent_cancels_during_stall_backoff` 可能超时 | 撤 Task 2 单独 commit；用 `grep -n '^\s*continue;' /tmp/think_body.rs` 核对应为 3 |
| 17 处 `return;` 漏改或多改 | 编译报类型不匹配（`()` vs `Option`） | 编译器会全列出，逐个改；`abort_run` 那 4 处也要改 |
| `ActOutcome.results` 提前 flatten | `agent_cancel_during_act_rolls_back`* 或工具失败路径测试失败 | 检查 `results` 是否仍是 `Vec<(String, Option<_>)>` |
| 抽取改变 future drop 时机 | `agent_cancel_while_provider_call_is_blocking` 挂起或失败 | 回退 Task 2；这是唯一会真的改变运行时的风险 |
| 构建锁导致假失败 | 出现 `3063 passed / 1 failed` 之类 | 确认没有并行跑第二个 `cargo test`，串行重跑 |

**回退粒度：** 每个 Task 一个 commit。Task 1 与 Task 2/3 无依赖关系；Task 2 与 Task 3 互相独立（ACT 段不依赖 `think_phase` 的存在）。

---

## 明确不做（本计划的边界）

- 不拆 `agent.rs` 文件（那是 C 线）
- 不搬测试模块（C1）
- 不改 `Agent` 的任何 `pub` 方法
- 不优化任何性能
- 不动 `maybe_auto_compact`（它在 THINK 之外，L964）
- 不动 `// 2. Termination check` 段（L1358–L1391，留在 `run_loop`）

---

## 与 spec 的数字差异（实施者必读）

本计划写于 main `3a1754cd`。`agent.rs` 未被 `fix/subagent-lease-leak` 触及，但实测仍与 spec 有 3 处差异：

| 项目 | spec 写的 | 实测 | 说明 |
|---|---|---|---|
| `run_loop` 行数 | 718 | **714**（L933–L1646） | 差 4 行 |
| THINK 段 | 361 行 | **326 行**（L1031–L1356） | spec 把「Termination check」算进了 THINK |
| ACT 段 | 213 行 | **230 行**（L1392–L1621） | — |
| OBSERVE 段 | 30 行 | **22 行**（L1623–L1644） | — |
| 「Termination check」段 | 未单列 | **34 行**（L1358–L1391） | **必须留在 `run_loop`**，含 `continue` 与 `return` |
| `last_logged` 归属 | 列为 THINK 参数 | **在 THINK 之前**（L949/L981/L1029） | 本计划已修正 |
| `stall_retries` / 租约 | 列为跨阶段状态 | **THINK 段内局部**（L1063/L1065） | 本计划已修正 |

**最后两行是最重要的修正**：spec 假设把 `last_logged` / `stall_retries` / `provider_turn_lease` 作为阶段参数传递，但实测它们都不需要跨阶段。按 spec 原样做会引入不存在的耦合。

**处理原则：以本计划为准。**

