# 会话累计用量统计 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让桌面端 `/usage` 与右下角状态栏统计并实时展示该会话的累计 input / cache read / output token，而不是最后一次 provider 调用的快照。

**Architecture:** 服务端 `Translator` 维护三段累加器（`committed` 已落盘轮次 + `turn` 本轮已结束调用 + `snapshot` 当前调用），每收到一个 `AgentEvent::Usage` 就重算总量并随 `thread/tokenUsage/updated` 推给客户端；每轮收尾把本轮之和写进 jsonl 的新字段 `usage_total`，重启后由 `ThreadStore` 跨轮求和恢复基线。桌面端 `Session` 同时保存快照（算上下文占用）与累计值（算用量展示），状态栏折叠态显示 `input / cache read / output` 三组带单位的滚动数字。

**Tech Stack:** Rust（`yi-agent-app-server`、ts-rs 绑定）、TypeScript + React 19、Vitest、cargo test。

**Spec:** `docs/superpowers/specs/2026-10-22-desktop-cumulative-usage-design.md`

## Global Constraints

- 工作目录：`.worktrees/cumulative-usage`（分支 `feat/cumulative-usage`）。**禁止在 `main` 上改代码或提交。**
- Rust 侧改动后必须跑 `cd yi-agent-rs && cargo fmt --all`，再提交。
- 改了 `protocol.rs` 必须重新生成 ts-rs 绑定：`cd yi-agent-rs && cargo test --all-features -p yi-agent-app-server export_bindings`，并把 `desktop/src/lib/generated/` 的 diff 一起提交（`just bindings-check` 会校验）。
- commit message 用 conventional commits，**不写** `Co-Authored-By`。
- 不要同时跑多个 `cargo test`（会争 workspace 锁 / OOM）。跑之前先 `ps aux | grep -v grep | grep cargo` 确认没有残留。
- 优先按 crate 跑测试，不用 `--workspace` 全量。
- 展示文案用中文，与既有 `/usage` 输出一致；数字用 `Intl.NumberFormat("en-US")` 千分位。
- 累计值一律用 `u64`（Rust）/ `number`（TS），不用 `u32`，避免长会话溢出。
- 老服务端/老日志缺字段时按「回退到最后一轮快照」处理，不得报错、不得出现第三种状态。

---

## 文件结构

**Rust — 存储层**

- `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs`（改）：新增 `UsageTotals`；`TurnLine::Turn` / `PartialTurn` 加 `usage_total`；`read_log` 返回 totals；`LoadedThread.totals`。

**Rust — 翻译层**

- `yi-agent-rs/crates/yi-agent-app-server/src/translate.rs`（改）：`Translator` 持有 `committed` / `turn` / `snapshot`，按调用边界结算，通知带总量；`reset_totals()`。

**Rust — 协议与驱动**

- `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs`（改）：`Notification::TokenUsage` 加四个 totals 字段。
- `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（改）：driver 用基线播种 translator；每轮回写 `usage_total`；`/clear` 后重置。
- `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs`（改）：`readItems` 响应加 `totals`；`resume` 回放带 totals。

**TS — 纯函数层**

- `desktop/src/lib/protocol.ts`（改）：`UsageTotals`、`WireTotals`、`wireTotalsToTotals`。
- `desktop/src/lib/compactNumber.ts`（新建）：`compact()` 与 `formatCompact()`。
- `desktop/src/lib/usageCommand.ts`（改）：`formatUsage` 接收 totals。
- `desktop/src/lib/session.ts`（改）：`usageTotals` 字段、通知更新、`adoptPersistedTotals`。

**TS — 组件层**

- `desktop/src/components/RollingNumber.tsx`（改）：`fractionDigits` / `unit`。
- `desktop/src/components/StatusBar.tsx`（改）：三组累计数字。
- `desktop/src/components/UsagePanel.tsx`（改）：累计明细。
- `desktop/src/App.tsx`（改）：两处 `readItems` 接 totals、`/usage` 传 totals。

---

### Task 1: `ThreadStore` 记录并汇总每轮累加值

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs:135-186`（`TurnUsage` 之后加 `UsageTotals`；`TurnLine` / `PartialTurn` / `LoadedThread` 加字段）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs:245-280`（`read_log`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs:300-350`（`load`）
- Test: 同文件 `#[cfg(test)] mod tests`

**Interfaces:**
- Produces:
  - `pub struct UsageTotals { pub input_tokens: u64, pub output_tokens: u64, pub cache_creation_input_tokens: u64, pub cache_read_input_tokens: u64 }`，实现 `Default + Clone + Copy + Debug + PartialEq + Eq + Serialize + Deserialize`。
  - `UsageTotals::add(&mut self, other: &UsageTotals)`、`UsageTotals::add_turn_usage(&mut self, u: &TurnUsage)`、`UsageTotals::is_empty(&self) -> bool`。
  - `TurnLine::Turn { items, usage: Option<TurnUsage>, usage_total: Option<TurnUsage>, messages }`。
  - `PartialTurn.usage_total: Option<TurnUsage>`。
  - `LoadedThread.totals: UsageTotals`。
  - `read_log` 私有签名变为 `-> (Vec<Item>, Vec<Message>, Option<TurnUsage>, UsageTotals)`。

- [ ] **Step 1: 写失败测试**

在 `thread_store.rs` 的测试模块里追加：

```rust
    #[test]
    fn totals_sum_usage_total_across_turns() {
        let (_dir, s) = store();
        s.create(&meta("t1")).unwrap();
        let with_totals = |totals: u32, last: u32| TurnLine::Turn {
            items: Vec::new(),
            usage: Some(TurnUsage {
                model: "m".into(),
                input_tokens: last,
                output_tokens: 1,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            }),
            usage_total: Some(TurnUsage {
                model: "m".into(),
                input_tokens: totals,
                output_tokens: 2,
                cache_creation_input_tokens: 3,
                cache_read_input_tokens: 4,
            }),
            messages: Vec::new(),
        };
        s.append_turn("t1", &with_totals(100, 50)).unwrap();
        s.append_turn("t1", &with_totals(200, 60)).unwrap();
        let loaded = s.load("t1").unwrap().unwrap();
        assert_eq!(loaded.totals.input_tokens, 300);
        assert_eq!(loaded.totals.output_tokens, 4);
        assert_eq!(loaded.totals.cache_creation_input_tokens, 6);
        assert_eq!(loaded.totals.cache_read_input_tokens, 8);
        // `usage` 语义不变：仍是最后一轮的最后一次快照。
        assert_eq!(loaded.usage.unwrap().input_tokens, 60);
    }

    #[test]
    fn totals_fall_back_to_last_snapshot_when_usage_total_missing() {
        // 老日志没有 `usage_total`：只能拿该行的 `usage` 近似。
        let (dir, s) = store();
        s.create(&meta("t2")).unwrap();
        let line = r#"{"type":"turn","items":[],"usage":{"model":"m","input_tokens":7,"output_tokens":2,"cache_creation_input_tokens":0,"cache_read_input_tokens":0},"messages":[]}"#;
        std::fs::write(dir.path().join(".yi-agent/threads/t2.jsonl"), format!("{line}\n")).unwrap();
        let loaded = s.load("t2").unwrap().unwrap();
        assert_eq!(loaded.totals.input_tokens, 7);
        assert_eq!(loaded.totals.output_tokens, 2);
    }

    #[test]
    fn totals_ignore_rows_without_usage() {
        let (_dir, s) = store();
        s.create(&meta("t3")).unwrap();
        s.append_turn(
            "t3",
            &TurnLine::Turn {
                items: Vec::new(),
                usage: None,
                usage_total: None,
                messages: Vec::new(),
            },
        )
        .unwrap();
        let loaded = s.load("t3").unwrap().unwrap();
        assert!(loaded.totals.is_empty());
    }
```

复用本模块既有的 `store()` 与 `meta(id)` helper（`thread_store.rs:978-1008`），不要新造 meta 构造方式。既有测试里所有 `TurnLine::Turn { .. }` 字面量构造都要补 `usage_total: None`——先做这一步再跑测试（`cargo test` 的编译错误会逐条指出位置）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_store::tests::totals -- --nocapture`
Expected: 编译失败，报 `UsageTotals` / `usage_total` 不存在。

- [ ] **Step 3: 实现**

在 `TurnUsage`（`thread_store.rs:137-147`）之后加：

```rust
/// 一个 thread 的累计用量。`u64`：长会话里 input 很容易超过 `u32` 上限。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTotals {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

impl UsageTotals {
    pub fn add_turn_usage(&mut self, usage: &TurnUsage) {
        self.input_tokens += usage.input_tokens as u64;
        self.output_tokens += usage.output_tokens as u64;
        self.cache_creation_input_tokens += usage.cache_creation_input_tokens as u64;
        self.cache_read_input_tokens += usage.cache_read_input_tokens as u64;
    }

    pub fn add(&mut self, other: &UsageTotals) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_creation_input_tokens += other.cache_creation_input_tokens;
        self.cache_read_input_tokens += other.cache_read_input_tokens;
    }

    pub fn is_empty(&self) -> bool {
        self.input_tokens == 0
            && self.output_tokens == 0
            && self.cache_creation_input_tokens == 0
            && self.cache_read_input_tokens == 0
    }
}
```

`TurnLine::Turn` 加：

```rust
        /// 本轮**所有** provider 调用的累加值；老日志缺失时由 `usage` 近似。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage_total: Option<TurnUsage>,
```

`PartialTurn` 加同款字段。`LoadedThread` 加 `pub totals: UsageTotals,`。

`read_log` 改为累计：

```rust
    fn read_log(&self, id: &str) -> (Vec<Item>, Vec<Message>, Option<TurnUsage>, UsageTotals) {
        let mut items: Vec<Item> = Vec::new();
        let mut messages: Vec<Message> = Vec::new();
        let mut usage: Option<TurnUsage> = None;
        let mut totals = UsageTotals::default();
        match std::fs::read_to_string(self.log_path(id)) {
            Ok(text) => {
                for line in text.lines() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<TurnLine>(line) {
                        Ok(TurnLine::Turn {
                            items: turn_items,
                            usage: turn_usage,
                            usage_total: turn_total,
                            messages: turn_messages,
                        }) => {
                            items.extend(turn_items);
                            if !turn_messages.is_empty() {
                                messages = turn_messages;
                            }
                            // 累计优先取本轮累加值；老日志没有就退回该轮最后快照。
                            if let Some(total) = &turn_total {
                                totals.add_turn_usage(total);
                            } else if let Some(last) = &turn_usage {
                                totals.add_turn_usage(last);
                            }
                            if turn_usage.is_some() {
                                usage = turn_usage;
                            }
                        }
                        Err(e) => {
                            eprintln!("[app-server] skipping corrupt thread log line ({id}): {e}");
                        }
                    }
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("[app-server] failed to read thread log ({id}): {e}"),
        }
        (items, messages, usage, totals)
    }
```

`load` 里解构改为四元组，partial 合流处补：

```rust
                if let Some(total) = &partial.usage_total {
                    totals.add_turn_usage(total);
                } else if let Some(last) = &partial.usage {
                    totals.add_turn_usage(last);
                }
```

并返回 `totals`。两处只要 items 的调用点（`thread_store.rs:578`、`:769`）改成 `let (items, _, _, _) = self.read_log(id);`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_store`
Expected: 全绿（含既有 20+ 用例）。

- [ ] **Step 5: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/cumulative-usage
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs
git commit -m "feat(app-server): 落盘每轮累加用量并在读取时跨轮汇总"
```

---

### Task 2: 协议字段与 ts-rs 绑定

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs:450-465`（字段）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs:1013-1030`（既有测试的构造点）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs:646-660`（resume 回放）
- Regenerate: `desktop/src/lib/generated/Notification.ts`、`NotificationEnvelope.ts`

**Interfaces:**
- Produces（线形状，snake_case）：
  ```json
  { "method": "thread/tokenUsage/updated",
    "params": { "thread_id": "…", "model": "…",
      "input_tokens": 0, "output_tokens": 0,
      "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
      "total_input_tokens": 0, "total_output_tokens": 0,
      "total_cache_creation_input_tokens": 0, "total_cache_read_input_tokens": 0 } }
  ```

- [ ] **Step 1: 加字段并更新注释**

```rust
    /// 用量更新通知。
    ///
    /// `input_tokens` 等四个字段是**本轮最后一次 provider 调用**的快照（Anthropic
    /// 把一次调用拆成 `message_start` 的 input/cache 与 `message_delta` 的 output，
    /// 翻译层按字段合并），上下文占用据此计算。
    ///
    /// `total_*` 是该 thread 的**累计**用量（已落盘轮次 + 本轮已结束调用 + 当前调用），
    /// 每收到一次 provider 用量事件都会重算并推送，故界面上的累计数字会随每次调用增长。
    #[serde(rename = "thread/tokenUsage/updated")]
    TokenUsage {
        thread_id: String,
        model: String,
        input_tokens: u32,
        output_tokens: u32,
        cache_creation_input_tokens: u32,
        cache_read_input_tokens: u32,
        #[ts(type = "number")]
        total_input_tokens: u64,
        #[ts(type = "number")]
        total_output_tokens: u64,
        #[ts(type = "number")]
        total_cache_creation_input_tokens: u64,
        #[ts(type = "number")]
        total_cache_read_input_tokens: u64,
    },
```

`#[ts(type = "number")]` 让 ts-rs 不把 `u64` 渲染成 `bigint`（前端用 `number` 即可，counts 远小于 2^53）。

- [ ] **Step 1b: 补齐另外两处构造点（否则编译不过）**

`protocol.rs:1013` 的既有测试：

```rust
    #[test]
    fn token_usage_notification_includes_cache_fields() {
        let n = Notification::TokenUsage {
            thread_id: "t1".into(),
            model: "m".into(),
            input_tokens: 10,
            output_tokens: 3,
            cache_creation_input_tokens: 100,
            cache_read_input_tokens: 200,
            total_input_tokens: 1_000,
            total_output_tokens: 300,
            total_cache_creation_input_tokens: 500,
            total_cache_read_input_tokens: 700,
        };
        let v: Value = serde_json::to_value(NotificationEnvelope::new(&n)).unwrap();
        assert_eq!(v["method"], "thread/tokenUsage/updated");
        assert_eq!(v["params"]["input_tokens"], 10);
        assert_eq!(v["params"]["total_input_tokens"], 1_000);
        assert_eq!(v["params"]["total_output_tokens"], 300);
        assert_eq!(v["params"]["total_cache_creation_input_tokens"], 500);
        assert_eq!(v["params"]["total_cache_read_input_tokens"], 700);
    }
```

`rpc/thread.rs:646` 的 resume 回放（`UsageTotals` 在 Task 1 已就绪；若不设 `loaded.totals`，
先写 `let totals = crate::thread_store::UsageTotals::default();` 并在 Task 4 换成 `loaded.totals`）：

```rust
            if let Some(u) = loaded.usage {
                let totals = loaded.totals;
                write_notification(
                    state.hub,
                    &Notification::TokenUsage {
                        thread_id: thread_id.clone(),
                        model: u.model,
                        input_tokens: u.input_tokens,
                        output_tokens: u.output_tokens,
                        cache_creation_input_tokens: u.cache_creation_input_tokens,
                        cache_read_input_tokens: u.cache_read_input_tokens,
                        total_input_tokens: totals.input_tokens,
                        total_output_tokens: totals.output_tokens,
                        total_cache_creation_input_tokens: totals.cache_creation_input_tokens,
                        total_cache_read_input_tokens: totals.cache_read_input_tokens,
                    },
                )
                .await?;
            }
```

`translate.rs` 的既有测试（`:1245`、`:1274`、`:1299`、`:1337`、`:1377`、`:1423`）都是
`Notification::TokenUsage { .. }` 的**匹配**而非构造（`match &out[0]`），补不补字段取决于
是否用 `..`；若某处是完整解构，补 `total_*: _`。

- [ ] **Step 2: 重新生成绑定**

Run: `cd yi-agent-rs && cargo test --all-features -p yi-agent-app-server export_bindings`
Expected: PASS，`desktop/src/lib/generated/Notification.ts` 与 `NotificationEnvelope.ts` 出现四个 `total*` 字段。

- [ ] **Step 3: 校验生成物一致且编译通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
Expected: 全绿。

- [ ] **Step 4: 提交**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs desktop/src/lib/generated
git commit -m "feat(app-server): 用量通知携带会话累计 token"
```

---

### Task 3: `Translator` 按调用边界累加并推送总量

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/translate.rs:29-120`（`UsageSnapshot` 后加累加逻辑、`Translator` 字段与 `new`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/translate.rs:353-363`（`Usage` 分支）
- Test: 同文件 `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `crate::thread_store::UsageTotals`（Task 1）。
- Produces:
  - `Translator::new(thread_id: String, baseline: UsageTotals) -> Self`（签名变更，所有既有调用点补 `UsageTotals::default()`）。
  - `Translator::turn_totals(&self) -> UsageTotals` — 本轮所有**已结束**调用的和（含当前 snapshot），供落盘。
  - `Translator::reset_totals(&mut self)` — 清零三段。
  - `Translator::totals(&self) -> UsageTotals` — `committed + turn + snapshot`，节点头用。

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn totals_grow_on_message_start_before_output_arrives() {
        let mut t = Translator::new("th".into(), UsageTotals::default());
        t.set_turn("a".into());
        let out = t.on_event(AgentEvent::Usage {
            model: "m".into(),
            usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 0,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: Some(40),
                reasoning_tokens: None,
            },
        });
        match &out[0] {
            Notification::TokenUsage {
                input_tokens,
                total_input_tokens,
                total_cache_read_input_tokens,
                ..
            } => {
                assert_eq!(*input_tokens, 100);
                assert_eq!(*total_input_tokens, 100);
                assert_eq!(*total_cache_read_input_tokens, 40);
            }
            other => panic!("expected TokenUsage, got {other:?}"),
        }
    }

    #[test]
    fn totals_accumulate_across_calls_within_a_turn() {
        let mut t = Translator::new("th".into(), UsageTotals::default());
        t.set_turn("a".into());
        // 调用 1：input 100 / output 0，随后补 output 10。
        t.on_event(AgentEvent::Usage {
            model: "m".into(),
            usage: TokenUsage { input_tokens: 100, output_tokens: 0, ..Default::default() },
        });
        t.on_event(AgentEvent::Usage {
            model: "m".into(),
            usage: TokenUsage { input_tokens: 0, output_tokens: 10, ..Default::default() },
        });
        // 调用 2：input 200。
        let out = t.on_event(AgentEvent::Usage {
            model: "m".into(),
            usage: TokenUsage { input_tokens: 200, output_tokens: 0, ..Default::default() },
        });
        match &out[0] {
            Notification::TokenUsage { total_input_tokens, total_output_tokens, .. } => {
                assert_eq!(*total_input_tokens, 300);
                assert_eq!(*total_output_tokens, 10);
            }
            other => panic!("expected TokenUsage, got {other:?}"),
        }
    }

    #[test]
    fn totals_keep_previous_turns_after_set_turn() {
        let mut t = Translator::new("th".into(), UsageTotals::default());
        t.set_turn("a".into());
        t.on_event(AgentEvent::Usage {
            model: "m".into(),
            usage: TokenUsage { input_tokens: 100, output_tokens: 5, ..Default::default() },
        });
        t.set_turn("b".into());
        let out = t.on_event(AgentEvent::Usage {
            model: "m".into(),
            usage: TokenUsage { input_tokens: 7, output_tokens: 0, ..Default::default() },
        });
        match &out[0] {
            Notification::TokenUsage { total_input_tokens, total_output_tokens, .. } => {
                assert_eq!(*total_input_tokens, 107);
                assert_eq!(*total_output_tokens, 5);
            }
            other => panic!("expected TokenUsage, got {other:?}"),
        }
    }

    #[test]
    fn reset_totals_zeroes_everything_and_emits_a_zero_notice() {
        let mut t = Translator::new("th".into(), UsageTotals::default());
        t.set_turn("a".into());
        t.on_event(AgentEvent::Usage {
            model: "m".into(),
            usage: TokenUsage { input_tokens: 100, output_tokens: 5, ..Default::default() },
        });
        let out = t.reset_totals();
        assert!(out.iter().any(|n| matches!(
            n,
            Notification::TokenUsage { total_input_tokens: 0, total_output_tokens: 0, .. }
        )));
        assert!(t.totals().is_empty());
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib translate::tests::totals_`
Expected: 编译失败（`UsageTotals` 未导入 / `new` 参数不符 / 通知无 `total_*` 字段）。

- [ ] **Step 3: 实现**

`Translator` 字段：

```rust
    /// 本次 driver 启动前已落盘轮次的累计（`/clear` 才清零）。
    committed: UsageTotals,
    /// 本轮已结束调用的累计；`set_turn` 时并入 `committed` 后清零。
    turn: UsageTotals,
```

`new`：

```rust
    pub fn new(thread_id: String, baseline: UsageTotals) -> Self {
        Self {
            thread_id,
            turn_id: String::new(),
            next_item: 1,
            active_agent_msg: None,
            tool_items: HashMap::new(),
            usage: UsageSnapshot::default(),
            committed: baseline,
            turn: UsageTotals::default(),
        }
    }
```

`set_turn` 在清空 snapshot 前结算：

```rust
    pub fn set_turn(&mut self, turn_id: String) {
        // 上一轮的收尾调用若还没结算（例如 turn 以 Error 结束、没有 Done 通知），
        // 在这里补上，保证累计不丢数。
        self.settle_snapshot();
        self.committed.add(&self.turn);
        self.turn = UsageTotals::default();
        self.snapshot_reset();
        self.turn_id = turn_id;
    }
```

其中：

```rust
    fn settle_snapshot(&mut self) {
        if self.usage.is_empty() {
            return;
        }
        self.turn.input_tokens += self.usage.input_tokens as u64;
        self.turn.output_tokens += self.usage.output_tokens as u64;
        self.turn.cache_creation_input_tokens += self.usage.cache_creation_input_tokens as u64;
        self.turn.cache_read_input_tokens += self.usage.cache_read_input_tokens as u64;
    }

    fn snapshot_reset(&mut self) {
        self.usage = UsageSnapshot::default();
    }

    fn totals(&self) -> UsageTotals {
        let mut out = self.committed;
        out.add(&self.turn);
        out.input_tokens += self.usage.input_tokens as u64;
        out.output_tokens += self.usage.output_tokens as u64;
        out.cache_creation_input_tokens += self.usage.cache_creation_input_tokens as u64;
        out.cache_read_input_tokens += self.usage.cache_read_input_tokens as u64;
        out
    }

    pub fn turn_totals(&self) -> UsageTotals {
        let mut out = self.turn;
        out.input_tokens += self.usage.input_tokens as u64;
        out.output_tokens += self.usage.output_tokens as u64;
        out.cache_creation_input_tokens += self.usage.cache_creation_input_tokens as u64;
        out.cache_read_input_tokens += self.usage.cache_read_input_tokens as u64;
        out
    }

    pub fn reset_totals(&mut self) -> Vec<Notification> {
        self.committed = UsageTotals::default();
        self.turn = UsageTotals::default();
        self.snapshot_reset();
        vec![self.usage_notification()]
    }
```

`UsageSnapshot` 加 `fn is_empty(&self) -> bool`（四个计数全 0 视为空）。

`Usage` 事件分支改为「先判边界、结算旧快照、再合并新值」：

```rust
            AgentEvent::Usage { model, usage } => {
                // `input_tokens > 0` 即新的一次 provider 调用（与 `merge` 的判据一致）：
                // 先把上一次调用结算进本轮累计，再让 merge 整体替换快照。
                if usage.input_tokens > 0 {
                    self.settle_snapshot();
                }
                self.usage.merge(&model, &usage);
                out.push(self.usage_notification());
            }
```

`usage_notification()`：

```rust
    fn usage_notification(&self) -> Notification {
        let totals = self.totals();
        Notification::TokenUsage {
            thread_id: self.thread_id.clone(),
            model: self.usage.model.clone(),
            input_tokens: self.usage.input_tokens,
            output_tokens: self.usage.output_tokens,
            cache_creation_input_tokens: self.usage.cache_creation_input_tokens,
            cache_read_input_tokens: self.usage.cache_read_input_tokens,
            total_input_tokens: totals.input_tokens,
            total_output_tokens: totals.output_tokens,
            total_cache_creation_input_tokens: totals.cache_creation_input_tokens,
            total_cache_read_input_tokens: totals.cache_read_input_tokens,
        }
    }
```

`Notification::TokenUsage` 的 `total_*` 字段已在 Task 2 加好，本任务只消费。

既有测试里的 `Translator::new("th1".into())` 全部改为 `Translator::new("th1".into(), UsageTotals::default())`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib translate`
Expected: 全绿（含既有 30+ 用例，尤其 `usage_carries_cache_tokens` 与连续调用相关用例）。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-app-server/src/translate.rs
git commit -m "feat(app-server): 翻译层按 provider 调用累加用量并随通知推送总量"
```

---

### Task 4: driver 播种基线、落盘本轮累加值、`/clear` 归零

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:4460-4520`（`persist_and_finish_turn` 加参数）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:4415-4440`（`build_partial` 加参数）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:4916-4920`（translator 播种）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:4982-4990`（本轮记录）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:5187-5210`（通知分支）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:5300-5315`（checkpoint 传参）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:5370-5425`（命令路径与常规收尾）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:4687,4729,4856`（其余三处调用点补 `None`）

**Interfaces:**
- Consumes: `Translator::new(id, baseline)`、`Translator::turn_totals()`、`Translator::reset_totals()`（Task 3）；`LoadedThread.totals`（Task 1）。
- Produces: jsonl 新轮次带 `usage_total`；`/clear` 后通知全零。

- [ ] **Step 1: 写失败测试（`rpc_tests.rs`）**

在既有 `read_items_returns_persisted_items_and_slices_after_id` 附近加：

```rust
/// 累计 input 必须不小于最后一轮快照，且四类累计都要落盘、可读回。
/// 证明 `/usage` 统计的是「所有 provider 调用之和」，不是最后一次请求的上下文长度。
#[tokio::test(flavor = "multi_thread")]
async fn read_items_returns_persisted_totals() {
    let mut h = Harness::with_factory(build_usage_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":31,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;
    for _ in 0..16 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            break;
        }
    }

    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":32,"method":"thread/readItems","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let v = read_response(&mut h, 32).await;
    let totals = v["result"]["totals"]
        .as_object()
        .unwrap_or_else(|| panic!("readItems 必须回传累计用量: {v}"));
    let usage = v["result"]["usage"].as_object().expect("usage");
    assert!(
        totals["input_tokens"].as_u64().unwrap_or(0) >= usage["input_tokens"].as_u64().unwrap_or(0),
        "累计值不得小于最后一轮快照: {v}"
    );
    assert!(totals["output_tokens"].as_u64().unwrap_or(0) > 0, "{v}");
}

/// `/clear` 截断日志后累计必须归零，否则重启前后数字会对不上。
#[tokio::test(flavor = "multi_thread")]
async fn clear_resets_persisted_totals() {
    let mut h = Harness::with_factory(build_usage_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":41,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"text","text":"hi"}}]}}}}"#
    ))
    .await;
    for _ in 0..16 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            break;
        }
    }
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":42,"method":"thread/clear","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let _ = read_response(&mut h, 42).await;
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":43,"method":"thread/readItems","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    let v = read_response(&mut h, 43).await;
    assert_eq!(v["result"]["totals"]["input_tokens"].as_u64().unwrap_or(1), 0, "{v}");
}

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib read_items_totals`
Expected: FAIL —— `result.totals` 为 null（响应还没带该字段）。

- [ ] **Step 3: 实现**

(a) `persist_and_finish_turn` 签名加 `usage_total: Option<crate::thread_store::TurnUsage>`（紧跟 `last_usage` 之后），`record` 里写：

```rust
    let record = crate::thread_store::TurnLine::Turn {
        items,
        usage: last_usage,
        usage_total,
        messages: persisted_messages,
    };
```

(b) `build_partial` 加同款参数并写进 `PartialTurn { .., usage_total }`。

(c) driver 启动：

```rust
    // 累计基线：本 thread 已落盘轮次之和。失败按 0 处理——最坏是首次打开时
    // 累计偏小，绝不能让 driver 起不来。
    let baseline = store
        .load(&thread_id)
        .ok()
        .flatten()
        .map(|loaded| loaded.totals)
        .unwrap_or_default();
    let mut translator = Translator::new(thread_id.clone(), baseline);
```

(d) 每轮循环内新增：

```rust
        // 本轮结束时的累加值（所有 provider 调用之和），落盘供下次启动恢复基线。
        let mut turn_usage_total: Option<crate::thread_store::TurnUsage> = None;
```

在 `Notification::TokenUsage { .. }` 分支里，除既有 `last_usage` 赋值外，同时用 `translator.turn_totals()` 刷新本轮累加值：

```rust
                                    last_usage = Some(crate::thread_store::TurnUsage {
                                        model: model.clone(),
                                        input_tokens: *input_tokens,
                                        output_tokens: *output_tokens,
                                        cache_creation_input_tokens: *cache_creation_input_tokens,
                                        cache_read_input_tokens: *cache_read_input_tokens,
                                    });
                                    let totals = translator.turn_totals();
                                    turn_usage_total = Some(crate::thread_store::TurnUsage {
                                        model: model.clone(),
                                        input_tokens: totals.input_tokens as u32,
                                        output_tokens: totals.output_tokens as u32,
                                        cache_creation_input_tokens: totals
                                            .cache_creation_input_tokens as u32,
                                        cache_read_input_tokens: totals.cache_read_input_tokens as u32,
                                    });
```

> 累加值本身用 `u64` 存，落到既有 `TurnUsage`（`u32`）时按饱和转换，避免一个超长 turn 溢出成小数字：
> `totals.input_tokens.min(u32::MAX as u64) as u32`，四个字段一致处理。

(e) checkpoint 调用点传 `turn_usage_total.clone()`；常规收尾传 `turn_usage_total.take()`。

(f) `apply_session_command` 前后判断 clear：

```rust
        let clearing = matches!(
            &pending_session_command,
            Some(SessionCommand::Clear { .. })
        );
        if let Some(command) = pending_session_command.take() {
            agent = apply_session_command(/* … */).await;
            if clearing {
                for n in translator.reset_totals() {
                    let _ = write_notification(&hub, &n).await;
                }
            }
            // …既有收尾逻辑
        }
```

空闲路径（driver 顶部 `session_rx.recv()` 分支）同样处理：

```rust
            Some(command) = session_rx.recv() => {
                let clearing = matches!(&command, SessionCommand::Clear { .. });
                agent = apply_session_command(/* … */).await;
                if clearing {
                    for n in translator.reset_totals() {
                        let _ = write_notification(&hub, &n).await;
                    }
                }
                continue;
            }
```

(g) `thread/readItems` 响应加 totals（`rpc/thread.rs`）：`ok_response(id, json!({ "items": items, "usage": loaded_usage, "totals": loaded.totals }))`。`loaded.totals` 可能已被 `loaded.usage` 取走部分字段——先 `let loaded_totals = loaded.totals;` 再 move items。

(h) `thread/resume` 回放已在 Task 2 接好；这里确认它取的是 `loaded.totals`（不是默认值）。

```rust
            if let Some(u) = loaded.usage {
                let totals = loaded.totals;
                write_notification(
                    state.hub,
                    &Notification::TokenUsage {
                        thread_id: thread_id.clone(),
                        model: u.model,
                        input_tokens: u.input_tokens,
                        output_tokens: u.output_tokens,
                        cache_creation_input_tokens: u.cache_creation_input_tokens,
                        cache_read_input_tokens: u.cache_read_input_tokens,
                        total_input_tokens: totals.input_tokens,
                        total_output_tokens: totals.output_tokens,
                        total_cache_creation_input_tokens: totals.cache_creation_input_tokens,
                        total_cache_read_input_tokens: totals.cache_read_input_tokens,
                    },
                )
                .await?;
            }
```

（`loaded.usage` 为 `None` 但 `totals` 非空时也应推一条全零快照 + 非零 totals，避免客户端看不到累计；实现为 `let model = loaded.usage.as_ref().map(|u| u.model.clone()).unwrap_or_default();` 后用 `usage` 的有无决定四个快照字段。）

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib "read_items_returns_persisted_totals"
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib clear_resets_persisted_totals`
再跑：`cd yi-agent-rs && cargo test -p yi-agent-app-server --lib`
Expected: 全绿。若 `/clear` 相关既有测试因新增通知帧数变化而红，按「多一帧 totals 通知」更新断言，不要放宽语义。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all && cd ..
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs
git commit -m "feat(app-server): driver 恢复累计基线并把本轮累加值落盘"
```

---

### Task 5: 桌面端类型与 `Session` 累计状态

**Files:**
- Modify: `desktop/src/lib/protocol.ts:142-175`
- Modify: `desktop/src/lib/session.ts:97`、`:246`、`:354-362`、`:650-660`
- Test: `desktop/src/lib/session.test.ts`

**Interfaces:**
- Produces:
  - `export interface UsageTotals { input: number; output: number; cacheRead: number; cacheWrite: number }`
  - `export interface WireTotals { input_tokens: number; output_tokens: number; cache_read_input_tokens?: number; cache_creation_input_tokens?: number }`
  - `export function wireTotalsToTotals(t: WireTotals | null | undefined): UsageTotals | null`
  - `Session.usageTotals: UsageTotals | null`、`Session.adoptPersistedTotals(t: UsageTotals | null): void`

- [ ] **Step 1: 写失败测试**

在 `session.test.ts` 追加：

```ts
  it("keeps cumulative totals from the usage notification", () => {
    const s = new Session();
    s.apply({
      method: "thread/tokenUsage/updated",
      params: {
        thread_id: "t",
        model: "m",
        input_tokens: 10,
        output_tokens: 2,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 5,
        total_input_tokens: 100,
        total_output_tokens: 20,
        total_cache_creation_input_tokens: 1,
        total_cache_read_input_tokens: 50,
      },
    } as Notification);
    expect(s.usage).toEqual({ model: "m", input: 10, output: 2, cacheRead: 5, cacheWrite: 0 });
    expect(s.usageTotals).toEqual({ input: 100, output: 20, cacheRead: 50, cacheWrite: 1 });
  });

  it("does not overwrite live totals with the persisted baseline", () => {
    const s = new Session();
    s.adoptPersistedTotals({ input: 7, output: 1, cacheRead: 0, cacheWrite: 0 });
    expect(s.usageTotals?.input).toBe(7);
    s.apply({
      method: "thread/tokenUsage/updated",
      params: {
        thread_id: "t", model: "m",
        input_tokens: 1, output_tokens: 1,
        cache_creation_input_tokens: 0, cache_read_input_tokens: 0,
        total_input_tokens: 8, total_output_tokens: 2,
        total_cache_creation_input_tokens: 0, total_cache_read_input_tokens: 0,
      },
    } as Notification);
    s.adoptPersistedTotals({ input: 7, output: 1, cacheRead: 0, cacheWrite: 0 });
    expect(s.usageTotals).toEqual({ input: 8, output: 2, cacheRead: 0, cacheWrite: 0 });
  });
```

（`apply` 的调用形状照抄同文件既有用例。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/session.test.ts`
Expected: FAIL —— `usageTotals` 不存在。

- [ ] **Step 3: 实现**

`protocol.ts`：

```ts
/** 会话累计用量（客户端形状）。 */
export interface UsageTotals {
  input: number;
  output: number;
  cacheRead: number;
  cacheWrite: number;
}

/** `thread/readItems` 回包里的累计用量（线形状）。 */
export interface WireTotals {
  input_tokens: number;
  output_tokens: number;
  cache_read_input_tokens?: number;
  cache_creation_input_tokens?: number;
}

export function wireTotalsToTotals(t: WireTotals | null | undefined): UsageTotals | null {
  if (!t) return null;
  return {
    input: t.input_tokens,
    output: t.output_tokens,
    cacheRead: t.cache_read_input_tokens ?? 0,
    cacheWrite: t.cache_creation_input_tokens ?? 0,
  };
}
```

`session.ts`：

```ts
  /** 会话累计用量；老服务端不回传时为 null，界面回退到快照。 */
  usageTotals: UsageTotals | null = null;
```

`reset()` 里加 `this.usageTotals = null;`。

`thread/tokenUsage/updated` 分支追加：

```ts
        this.usageTotals = {
          input: notification.params.total_input_tokens,
          output: notification.params.total_output_tokens,
          cacheRead: notification.params.total_cache_read_input_tokens,
          cacheWrite: notification.params.total_cache_creation_input_tokens,
        };
```

新增方法（紧跟 `adoptPersistedUsage`）：

```ts
  /** 用服务端持久化的累计值垫底；已有 live 值时绝不覆盖。 */
  adoptPersistedTotals(t: UsageTotals | null): void {
    if (t === null || this.usageTotals !== null) return;
    this.usageTotals = t;
  }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/session.test.ts && npx tsc --noEmit`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/lib/protocol.ts desktop/src/lib/session.ts desktop/src/lib/session.test.ts
git commit -m "feat(desktop): 会话保存服务端累计用量"
```

---

### Task 6: `App.tsx` 接入累计值

**Files:**
- Modify: `desktop/src/App.tsx:672-681`（远端补齐分支）
- Modify: `desktop/src/App.tsx:717-726`（运行中桌面分支）
- Test: `desktop/src/App.test.tsx`

**Interfaces:**
- Consumes: `wireTotalsToTotals`、`Session.adoptPersistedTotals`（Task 5）。
- 本任务**不动** `/usage` 分支：命令正文与新函数签名在 Task 7 一并改，避免中途留下半改状态。
- Produces: 两处 `readItems` 请求的类型参数变为 `{ items: Item[]; usage?: WireUsage | null; totals?: WireTotals | null }`。

- [ ] **Step 1: 写失败测试**

在 `App.test.tsx` 里追加：

```tsx
  it("adopts cumulative totals from readItems", async () => {
    // 运行中的会话只走只读 readItems（绝不 resume），它是这条路径上唯一的用量来源。
    state.listStatus = "running";
    state.dataSources["thread/readItems"] = () => ({
      items: [],
      totals: {
        input_tokens: 4321,
        output_tokens: 99,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 1000,
      },
    });
    render(<App />); // no persisted config → desktop
    // 状态栏的读屏文本是精确值，不受滚动数字 DOM 影响。
    await waitFor(() =>
      expect(document.querySelector(".sr-only")?.textContent).toContain("4,321 input"),
    );
  });

Expected: FAIL。

- [ ] **Step 3: 实现**

两处请求类型与回接：

```ts
              const r = await c0.request<{
                items: Item[];
                usage?: WireUsage | null;
                totals?: WireTotals | null;
              }>(/* … */);
              const v = store.peek(id);
              if (v) {
                v.session.upsertItems(r.items);
                v.session.adoptPersistedUsage(wireUsageToUsage(r.usage));
                v.session.adoptPersistedTotals(wireTotalsToTotals(r.totals));
                force((n) => n + 1);
              }
```

导入行补 `wireTotalsToTotals`（`App.tsx:33` 附近已有 `wireUsageToUsage` 的导入）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/App.test.tsx && npx tsc --noEmit`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "feat(desktop): /usage 与补齐路径接入会话累计用量"
```

---

### Task 7: `/usage` 输出累计口径

**Files:**
- Modify: `desktop/src/lib/usageCommand.ts`
- Modify: `desktop/src/App.tsx:1665-1686`（`/usage` 分支）
- Test: `desktop/src/lib/usageCommand.test.ts`、`desktop/src/App.test.tsx`

**Interfaces:**
- Produces: `formatUsage(snapshot: Usage | null, totals: UsageTotals | null, opts: { compactThreshold: number | null }): string`

- [ ] **Step 1: 写失败测试**

```ts
  const totals = { input: 1_234_567, output: 45_678, cacheRead: 1_200_000, cacheWrite: 0 };

  it("prints cumulative lines and keeps context occupancy from the snapshot", () => {
    const out = formatUsage(base, totals, { compactThreshold: 160_000 });
    expect(out).toContain("累计 input: 1,234,567  output: 45,678");
    expect(out).toContain("累计 cache read: 1,200,000");
    expect(out).toContain("累计 tokens: 2,480,245");
    // 上下文仍按最后一次调用算，不能用累计 input。
    expect(out).toContain("上下文: 12,345 / 160,000 (7.7%)");
  });

  it("hides cache write when its cumulative value is zero", () => {
    const out = formatUsage(base, totals, { compactThreshold: null });
    expect(out).not.toContain("cache write");
  });

  it("shows cache write when it is non-zero", () => {
    const out = formatUsage(base, { ...totals, cacheWrite: 900 }, { compactThreshold: null });
    expect(out).toContain("累计 cache write: 900");
  });

  it("falls back to the last snapshot when totals are missing", () => {
    const out = formatUsage(base, null, { compactThreshold: 160_000 });
    expect(out).toContain("累计 input: 12,345");
    expect(out).not.toContain("缓存");
  });
```

（`累计 tokens` = 1,234,567 + 45,678 + 1,200,000 + 0 = 2,480,245。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/usageCommand.test.ts`
Expected: FAIL（签名与文案不符）。

- [ ] **Step 3: 实现**

```ts
export function formatUsage(
  snapshot: Usage | null,
  totals: UsageTotals | null,
  opts: { compactThreshold: number | null },
): string {
  // 老服务端不回传累计值时用最后一轮快照兜底：宁可数字偏小，也不显示「没有用量」。
  const sum: UsageTotals = totals ?? {
    input: snapshot?.input ?? 0,
    output: snapshot?.output ?? 0,
    cacheRead: snapshot?.cacheRead ?? 0,
    cacheWrite: snapshot?.cacheWrite ?? 0,
  };
  const model = snapshot?.model ?? "未知";
  const limit = opts.compactThreshold;
  // 上下文占用的分子必须是**最后一次调用**的 input，不能用累计值：累计 input 是
  // 历史所有请求之和，会远超上下文上限，百分比没有任何意义。
  const contextInput = snapshot?.input ?? 0;
  const context =
    limit !== null && limit > 0
      ? `上下文: ${thousands(contextInput)} / ${thousands(limit)} (${((contextInput / limit) * 100).toFixed(1)}%)`
      : `上下文: ${thousands(contextInput)}`;
  const total = sum.input + sum.output + sum.cacheRead + sum.cacheWrite;
  const lines = [
    `模型: ${model}`,
    `累计 input: ${thousands(sum.input)}  output: ${thousands(sum.output)}`,
    `累计 cache read: ${thousands(sum.cacheRead)}`,
  ];
  // 用户只关注 cache read：cache write 为零时整行不出现。
  if (sum.cacheWrite > 0) lines.push(`累计 cache write: ${thousands(sum.cacheWrite)}`);
  lines.push(
    `累计 tokens: ${thousands(total)}`,
    context,
    `估算成本: ${formatCost(estimateCost({ model, ...sum }))}`,
  );
  return lines.join("\n");
}
```

`App.tsx` 的 `/usage` 分支改为：无快照也无累计 → 才说「本会话还没有用量记录」；
有累计就把两样都交给 `formatUsage`。

```ts
      case "usage": {
        const u = session.usage;
        const totals = session.usageTotals;
        if (!u && !totals) {
          session.notice("本会话还没有用量记录");
          break;
        }
        // 上下文上限按**当前会话**取（与 `/config` 同一条路）：进程级快照的
        // compact_threshold 来自 sidecar 启动配置，与这个会话实际生效的模型无关。
        let threshold: number | null = null;
        try {
          const cfg = await c.request<Record<string, unknown>>(
            "config/read",
            configReadParams(id),
          );
          const v = cfg.compact_threshold;
          threshold = typeof v === "number" && v > 0 ? v : null;
        } catch {
          // 读配置失败不该吞掉用量：分母未知就只显示 token 数。
        }
        session.notice(formatUsage(u, totals, { compactThreshold: threshold }));
        break;
      }
```

`App.test.tsx` 补一条：无数据时仍提示「本会话还没有用量记录」（既有用例按新签名不受影响，
若它 mock 了 `thread/tokenUsage/updated`，同步补四个 `total_*` 字段）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/usageCommand.test.ts src/App.test.tsx && npx tsc --noEmit`
Expected: PASS（含既有 4 条用例，需按新签名补第二参）。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/lib/usageCommand.ts desktop/src/lib/usageCommand.test.ts desktop/src/App.tsx desktop/src/App.test.tsx
git commit -m "feat(desktop): /usage 输出会话累计 input/cache/output"
```

---

### Task 8: 数字缩写纯函数 `compactNumber`

**Files:**
- Create: `desktop/src/lib/compactNumber.ts`
- Test: `desktop/src/lib/compactNumber.test.ts`

**Interfaces:**
- Produces:
  ```ts
  export interface CompactNumber { value: number; fractionDigits: number; unit: "" | "k" | "M" | "G" }
  export function compact(n: number): CompactNumber
  export function formatCompact(n: number): string
  ```

- [ ] **Step 1: 写失败测试**

```ts
describe("compact", () => {
  it("keeps small numbers as-is", () => {
    expect(compact(0)).toEqual({ value: 0, fractionDigits: 0, unit: "" });
    expect(compact(999)).toEqual({ value: 999, fractionDigits: 0, unit: "" });
  });
  it("uses one decimal for thousands", () => {
    expect(compact(12_345)).toEqual({ value: 12345, fractionDigits: 1, unit: "k" });
    expect(compact(1_200_000)).toEqual({ value: 12, fractionDigits: 2, unit: "M" });
  });
  it("rolls over to the next unit instead of printing 1000.0k", () => {
    expect(compact(999_950)).toEqual({ value: 1, fractionDigits: 2, unit: "M" });
  });
  it("formats with the unit attached", () => {
    expect(formatCompact(12_345)).toBe("12.3k");
    expect(formatCompact(1_200_000)).toBe("1.20M");
    expect(formatCompact(999)).toBe("999");
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/compactNumber.test.ts`
Expected: FAIL —— 模块不存在。

- [ ] **Step 3: 实现**

```ts
/**
 * 把大数字压成「尾数 + 小数位 + 单位」，供状态栏的翻滚数字使用。
 *
 * 单位不参与滚动：滚动的只有尾数，单位作为固定文本跟在后面。`value` 是尾数
 * **放大 10^fractionDigits 后**的整数（如 12.3k → `value: 123, fractionDigits: 1`），
 * 调用方直接交给 `RollingNumber` 逐位滚动，再在末位前插小数点。
 */
export interface CompactNumber {
  value: number;
  fractionDigits: number;
  unit: "" | "k" | "M" | "G";
}

/** 某档单位下的尾数（已放大），四舍五入。 */
function mantissa(magnitude: number, factor: number, digits: number): number {
  return Math.round((magnitude / factor) * 10 ** digits);
}

export function compact(n: number): CompactNumber {
  const magnitude = Math.max(0, Math.trunc(n));
  if (magnitude < 1_000) return { value: magnitude, fractionDigits: 0, unit: "" };
  if (magnitude < 1_000_000) {
    const value = mantissa(magnitude, 1_000, 1);
    // 999,950 四舍五入成 1000.0k 很怪：改用上一级单位（1.00M）。
    if (value >= 10_000) {
      return { value: mantissa(magnitude, 1_000_000, 2), fractionDigits: 2, unit: "M" };
    }
    return { value, fractionDigits: 1, unit: "k" };
  }
  if (magnitude < 1_000_000_000) {
    const value = mantissa(magnitude, 1_000_000, 2);
    if (value >= 100_000) {
      return { value: mantissa(magnitude, 1_000_000_000, 2), fractionDigits: 2, unit: "G" };
    }
    return { value, fractionDigits: 2, unit: "M" };
  }
  // G 已是顶层：即便进位后是 1000G，也没有更大的单位可退，保持原样。
  return { value: mantissa(magnitude, 1_000_000_000, 2), fractionDigits: 2, unit: "G" };
}

export function formatCompact(n: number): string {
  const { value, fractionDigits, unit } = compact(n);
  const digits = String(value).padStart(fractionDigits + 1, "0");
  if (fractionDigits === 0) return `${digits}${unit}`;
  return `${digits.slice(0, -fractionDigits)}.${digits.slice(-fractionDigits)}${unit}`;
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/compactNumber.test.ts`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/lib/compactNumber.ts desktop/src/lib/compactNumber.test.ts
git commit -m "feat(desktop): 新增 token 数字缩写纯函数"
```

---

### Task 9: `RollingNumber` 支持小数位与单位

**Files:**
- Modify: `desktop/src/components/RollingNumber.tsx:36-56`、`:140-170`
- Test: `desktop/src/components/RollingNumber.test.tsx`

**Interfaces:**
- Consumes: `CompactNumber`（Task 8）。
- Produces: `RollingNumber({ value, usageKey, fractionDigits = 0, unit = "" })`。`value` 仍是「放大后的整数」。

- [ ] **Step 1: 写失败测试**

```tsx
  it("renders a decimal point before the last fractionDigits digits", () => {
    const { container } = render(<RollingNumber value={123} usageKey="a" fractionDigits={1} unit="k" />);
    // 数字仍是逐位滚动（3 位），小数点插在倒数第 1 位之前，单位在最后。
    expect(container.textContent).toBe("12.3k");
    expect(container.querySelectorAll(".rolling-digit")).toHaveLength(3);
  });

  it("keeps the default rendering byte-identical", () => {
    const { container } = render(<RollingNumber value={1234} usageKey="a" />);
    expect(container.textContent).toBe("1234");
  });
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/RollingNumber.test.tsx`
Expected: FAIL。

- [ ] **Step 3: 实现**

`splitDigits` 不变（仍传放大后的整数）。渲染时按 `index` 判断插入点：

```tsx
  const pointAt = fractionDigits > 0 ? digits.length - fractionDigits : -1;
  return (
    <span className="rolling-number" aria-hidden="true">
      {digits.map((digit, index) => (
        <span key={index} className="rolling-digit-wrap">
          {index === pointAt && <span className="rolling-point">.</span>}
          <span className="rolling-digit">{/* 既有数字带 */}</span>
        </span>
      ))}
      {unit && <span className="rolling-unit">{unit}</span>}
    </span>
  );
```

不要新增包裹层改变既有 DOM 形态（`rolling-number` 下的 `rolling-digit` 选择器被测试与 CSS 依赖）。改为在 `digits.map` 里用 `Fragment` 输出「点 + 数字格」：

```tsx
      {digits.map((digit, index) => (
        <Fragment key={index}>
          {index === pointAt && <span className="rolling-point">.</span>}
          <span className="rolling-digit">{/* 既有内容 */}</span>
        </Fragment>
      ))}
      {unit && <span className="rolling-unit">{unit}</span>}
```

CSS（`desktop/src/index.css`，追加在 `.rolling-strip` 之后）：

```css
.rolling-point { display: inline-block; width: 0.4ch; text-align: center; }
.rolling-unit { margin-left: 0.1ch; }
```

`usageKey` / 动画逻辑不动。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/RollingNumber.test.tsx && npx tsc --noEmit`
Expected: PASS（既有 6 条用例保持绿）。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/components/RollingNumber.tsx desktop/src/index.css desktop/src/components/RollingNumber.test.tsx
git commit -m "feat(desktop): 状态栏滚动数字支持小数位与单位"
```

---

### Task 10: 状态栏三组累计数字

**Files:**
- Modify: `desktop/src/components/StatusBar.tsx:1-110`
- Modify: `desktop/src/components/UsagePanel.tsx`
- Modify: `desktop/src/App.tsx:2442-2452`（传参）
- Test: `desktop/src/components/StatusBar.test.tsx`

**Interfaces:**
- Consumes: `UsageTotals`（Task 5）、`compact` / `formatCompact`（Task 8）、`RollingNumber` 新参数（Task 9）。
- Produces: `StatusBar({ cwd, status, totals, fallbackUsage, usageId, actions, onRepair })`。

- [ ] **Step 1: 写失败测试**

```tsx
  it("shows cumulative input / cache read / output with units", () => {
    const { container } = render(
      <StatusBar
        cwd="/w"
        status="connected"
        totals={{ input: 12_345, output: 678, cacheRead: 120_000, cacheWrite: 0 }}
        usageId="t"
      />,
    );
    // 三组滚动数字：12.3k in / 120.0k cache / 678 out
    expect(container.querySelectorAll(".rolling-number")).toHaveLength(3);
    expect(container.textContent).toContain("12.3k");
    expect(container.textContent).toContain("120.0k");
    expect(container.querySelector(".sr-only")?.textContent).toBe(
      "12,345 input / 120,000 cache read / 678 output tokens",
    );
  });

  it("falls back to the snapshot when totals are absent", () => {
    const { container } = render(
      <StatusBar
        cwd="/w"
        status="connected"
        totals={null}
        fallbackUsage={{ model: "m", input: 1234, output: 5678, cacheRead: 0, cacheWrite: 0 }}
        usageId="t"
      />,
    );
    expect(container.textContent).toContain("1234");
  });
```

既有 5 条用例里的 `usage={...}` 改成 `totals={...}`（数值语义相同），`opens the usage panel on click` 与向上展开两条改用 `totals`。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/StatusBar.test.tsx`
Expected: FAIL。

- [ ] **Step 3: 实现**

```tsx
const shown = totals ?? (fallbackUsage
  ? { input: fallbackUsage.input, output: fallbackUsage.output,
      cacheRead: fallbackUsage.cacheRead, cacheWrite: fallbackUsage.cacheWrite }
  : null);
```

渲染（`shown` 非空时）：

```tsx
          <button
            type="button"
            className="inline-flex cursor-pointer items-center gap-1 font-mono hover:text-fg"
            aria-expanded={showUsage}
            aria-controls="usage-panel"
            aria-label="Token usage details"
            onClick={() => setShowUsage((v) => !v)}
          >
            <span className="sr-only">
              {thousands(shown.input)} input / {thousands(shown.cacheRead)} cache read /{" "}
              {thousands(shown.output)} output tokens
            </span>
            <span aria-hidden="true" className="inline-flex items-center gap-1">
              <RollingToken value={shown.input} usageKey={usageId} />
              <span>in /</span>
              <RollingToken value={shown.cacheRead} usageKey={usageId} />
              <span>cache /</span>
              <RollingToken value={shown.output} usageKey={usageId} />
              <span>out</span>
            </span>
          </button>
```

其中 `RollingToken` 是同文件的小封装：

```tsx
function RollingToken({ value, usageKey }: { value: number; usageKey?: string | null }) {
  const c = compact(value);
  return <RollingNumber value={c.value} fractionDigits={c.fractionDigits} unit={c.unit} usageKey={usageKey} />;
}
```

> `compact()` 的 `value` 已是「放大后的整数」（如 12.3k → `value: 123, fractionDigits: 1`），直接交给 `RollingNumber`。

`UsagePanel` 改为接收 `totals` 与 `snapshot`：标题 `Usage (this session)`，行改为累计 Input / Output / Cache read /（cacheWrite > 0 时）Cache write / Total / Model / Est. cost，总数与成本按累计值计算。

`App.tsx`：

```tsx
            totals={current?.session.usageTotals ?? null}
            fallbackUsage={current?.session.usage ?? null}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/StatusBar.test.tsx src/components/RollingNumber.test.tsx && npx tsc --noEmit`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add desktop/src/components/StatusBar.tsx desktop/src/components/UsagePanel.tsx desktop/src/App.tsx desktop/src/components/StatusBar.test.tsx
git commit -m "feat(desktop): 状态栏显示会话累计 input/cache/output"
```

---

### Task 11: 文档与全量校验

**Files:**
- Modify: `docs/project-management/desktop.md`（`/usage` 与状态栏两条验收项）
- Modify: `docs/project-management/README.md`（若计数变化）

- [ ] **Step 1: 更新验收项**

把第 67 行（`/usage` 取代 `/cost`）的判据补上累计口径，并新增一条：

```markdown
- [x] `/usage` 与右下角状态栏统计**会话累计**用量（input / cache read / output），每次 provider 调用都会实时增长 — 三段口径 `committed + turn + snapshot`（`yi-agent-rs/crates/yi-agent-app-server/src/translate.rs` 的 `Translator::{totals,turn_totals,reset_totals}`）；每轮累加值落盘为 `usage_total`（`thread_store.rs` 的 `TurnLine::Turn`），重启后由 `read_log` 跨轮求和恢复；通知 `thread/tokenUsage/updated` 新增 `total_*` 四个字段（`protocol.rs`）；前端 `Session.usageTotals`（`desktop/src/lib/session.ts`）+ 状态栏三组带单位滚动数字（`StatusBar.tsx` / `RollingNumber.tsx` / `compactNumber.ts`）。判据：`cd desktop && npx tsc --noEmit && npx vitest run src/lib/compactNumber.test.ts src/lib/session.test.ts src/lib/usageCommand.test.ts src/components/StatusBar.test.tsx src/components/RollingNumber.test.tsx src/App.test.tsx`；`cd yi-agent-rs && cargo test -p yi-agent-app-server --lib` — [设计](../superpowers/specs/2026-10-22-desktop-cumulative-usage-design.md)
```

- [ ] **Step 2: 全量校验**

Run:
```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server --lib && cargo fmt --all -- --check
cd ../desktop && npx tsc --noEmit && npx vitest run
```
Expected: 全绿。

- [ ] **Step 3: 提交**

```bash
git add docs/project-management/desktop.md docs/project-management/README.md
git commit -m "docs: 登记会话累计用量统计"
```

---

## 收尾

全部任务完成后：跑 `just bindings-check`（在 `yi-agent-rs/` 下）确认生成物无漂移，然后按 `superpowers:finishing-a-development-branch` 合并回 `main`（`git merge --no-ff feat/cumulative-usage`），合并后删除分支与 worktree。
