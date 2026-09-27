# compaction 图片 token 估算 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 compaction 对 `ContentBlock::Image` 的 token 估算从 0 改为显式常量 1844，使保留预算诚实反映图片占用；并对齐 `agent.rs` 的 UI prefill 估算。

**Architecture:** 在 `crates/yi-agent-core/src/compact.rs` 新增 `pub const IMAGE_TOKEN_ESTIMATE`，替换 `estimate_block_tokens` 里图片分支的 `0`。`crate::compact::IMAGE_TOKEN_ESTIMATE` 被 `agent.rs:1239` 的 prefill 估算复用。改动为零编码/解码成本的纯估值修正。

**Tech Stack:** Rust 2024 edition、`yi-agent-core`（无新依赖）。

**设计文档：** `docs/superpowers/specs/2026-09-27-compaction-image-token-estimate-design.md`

**工作目录：** 新建 worktree（如 `.worktrees/feat/compaction-image-token-estimate`，
分支同名）。Rust 命令在 `yi-agent-rs/` 下执行。

**关键限定（照抄 spec §2）：** 保留选择是**从最新往回**遍历，**最新的那个工具单元
即便单独超预算也照样保留**（`compact.rs:229-234`）。因此本改动**不会**把"最新那张图"
挤出保留窗口，只会让**较旧**的含图单元更早被摘要替换。它是"让核算诚实"的正确性修复，
**不是 413 的直接修复**。Task 2 的测试专门固化这一行为，防止误判。

---

## Global Constraints

- 不在 `main` 上改代码；先建 worktree + 分支（CLAUDE.md）。
- Commit message 不写 `Co-Authored-By`；用 conventional commits；首行 ≤72 字符。
- 提交前跑 `cargo fmt --all`（在 `yi-agent-rs/` 下）。
- 不要并发跑 `cargo test`；不要跑 `--workspace`。
- 估值常量固定为 `1844`（来源：codex `RESIZED_IMAGE_BYTES_ESTIMATE = 7373` 字节 ÷ 4 bytes/token 上取整）。
- 不改编码/解码路径；不改 `view_image`（属另一计划）。

---

## File Structure

- `yi-agent-rs/crates/yi-agent-core/src/compact.rs` — 常量 + 估算分支 + 测试。
- `yi-agent-rs/crates/yi-agent-core/src/agent.rs` — `estimate_prefill_tokens` 图片分支对齐。
- `docs/bug-list.md` — 登记 `compact_tool_budget_tokens` 默认值不一致。

---

## Task 1: `IMAGE_TOKEN_ESTIMATE` 与 compaction 估算

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/compact.rs:31`（常量区）
- Modify: `yi-agent-rs/crates/yi-agent-core/src/compact.rs:88`（`estimate_block_tokens`）
- Test: `yi-agent-rs/crates/yi-agent-core/src/compact.rs`（`mod tests`，起始 `:471`）

**Interfaces:**
- Produces: `pub const IMAGE_TOKEN_ESTIMATE: usize = 1844;`
  （`pub` 以便 `agent.rs` 复用；同 crate 内 `crate::compact::IMAGE_TOKEN_ESTIMATE`）

- [ ] **Step 1: 写失败测试**

加到 `compact.rs` 的 `mod tests`：

```rust
    #[test]
    fn image_block_estimates_nonzero_tokens() {
        let img = ContentBlock::Image {
            source: crate::message::ImageSource::Base64 {
                media_type: "image/png".into(),
                data: "AAAA".into(),
            },
            detail: crate::message::ImageDetail::High,
        };
        assert!(IMAGE_TOKEN_ESTIMATE > 0);
        assert_eq!(estimate_block_tokens(&img), IMAGE_TOKEN_ESTIMATE);
    }

    #[test]
    fn older_image_unit_is_summarized_when_image_cost_counted() {
        // 最新单元总被保留；本测试锁定"较旧含图单元会在计入图片成本后被挤出"。
        let image_result = |id: &str| {
            Message::tool_results(vec![ContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: vec![ContentBlock::Image {
                    source: crate::message::ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: "AAAA".into(),
                    },
                    detail: crate::message::ImageDetail::High,
                }],
                is_error: false,
            }])
        };
        let mut messages = vec![Message::user("task")];
        // old：纯文本，约 200 tokens。
        messages.push(Message::assistant(vec![planned_tool_use("old")]));
        messages.push(Message::tool_results(vec![planned_tool_result(
            "old",
            "x".repeat(800),
        )]));
        // new：仅一张图。
        messages.push(Message::assistant(vec![planned_tool_use("new")]));
        messages.push(image_result("new"));

        // 预算 2000：new 估 1844 保留；old(≈200) 会因 1844+200 > 2000 被挤出。
        let compacted = build_compacted_messages(
            &plan_compaction(&messages, 20_000, 2_000).expect("must compact"),
            "summary",
        )
        .unwrap();

        let retained_ids = compacted
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(retained_ids, vec!["new"]);
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p yi-agent-core --lib compact::tests::image_block_estimates_nonzero_tokens compact::tests::older_image_unit_is_summarized_when_image_cost_counted 2>&1 | tail -25`
Expected: FAIL（`cannot find value IMAGE_TOKEN_ESTIMATE`；第二个测试因图片记 0 而保留 `["old","new"]`）

- [ ] **Step 3: 实现**

`compact.rs:31` 附近（`DEFAULT_COMPACT_TOOL_BUDGET_TOKENS` 之后）加：

```rust
/// 单张图片的模型可见 token 估值。
///
/// 来源：codex `RESIZED_IMAGE_BYTES_ESTIMATE = 7373` 字节
/// （`codex-rs core/src/context_manager/history.rs:526-530`），
/// 按 4 bytes/token 上取整得 1844。
pub const IMAGE_TOKEN_ESTIMATE: usize = 1844;
```

`estimate_block_tokens`（`:88`）改为：

```rust
        ContentBlock::Image { .. } => IMAGE_TOKEN_ESTIMATE,
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p yi-agent-core --lib compact 2>&1 | tail -25`
Expected: PASS（新增 2 个 + 既有 compact 测试全绿；若有 `retained_tool_tokens` 相关的既有断言语义漂移，按新语义更新数值但**不弱化**断言）

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-core/src/compact.rs
git commit -m "fix(core): count images in compaction token estimate"
```

---

## Task 2: 对齐 UI prefill 估算

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/agent.rs:1239`
- Test: `yi-agent-rs/crates/yi-agent-core/src/agent.rs`（`mod tests`）

**Interfaces:**
- Consumes: `crate::compact::IMAGE_TOKEN_ESTIMATE`（Task 1）
- Produces: `estimate_prefill_tokens` 对图片块累加同一常量

- [ ] **Step 1: 写失败测试**

加到一个 `#[cfg(test)] mod tests`（agent.rs 已有）：

```rust
    #[test]
    fn prefill_estimate_counts_image_tokens() {
        let req = crate::provider::ProviderRequest {
            model: "test-model".into(),
            system: None,
            messages: vec![crate::message::Message {
                role: crate::message::Role::User,
                content: vec![crate::message::ContentBlock::Image {
                    source: crate::message::ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: "AAAA".into(),
                    },
                    detail: crate::message::ImageDetail::High,
                }],
            }],
            tools: Vec::new(),
            params: crate::provider::GenParams::default(),
        };
        assert_eq!(
            estimate_prefill_tokens(&req),
            crate::compact::IMAGE_TOKEN_ESTIMATE as u32
        );
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p yi-agent-core --lib agent::tests::prefill_estimate_counts_image_tokens 2>&1 | tail -20`
Expected: FAIL（`left: 0, right: 1844`）

- [ ] **Step 3: 实现**

`agent.rs:1239` 改为：

```rust
                crate::message::ContentBlock::Image { .. } => {
                    total += crate::compact::IMAGE_TOKEN_ESTIMATE as u32;
                }
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p yi-agent-core --lib agent::tests::prefill_estimate_counts_image_tokens 2>&1 | tail -15`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-core/src/agent.rs
git commit -m "fix(core): count image tokens in prefill estimate"
```

---

## Task 3: 登记预算默认值不一致 + 全 crate 回归

**Files:**
- Modify: `docs/bug-list.md`（新增一条待办）

- [ ] **Step 1: 追加 bug-list 条目**

在 `docs/bug-list.md` 末尾追加：

```markdown
- [ ] `compact_tool_budget_tokens` 默认值不一致：`agent.rs:118` 与
  `runtime/config.rs:357` 为 `12_000`，但 `yi-agent/src/main.rs:1568` 覆写为
  `4096`。非阻塞，但属潜在意外，需确认哪一个是期望默认。
```

（若该文件按主题分组，插入到合适的"配置"小节；保持既有格式。）

- [ ] **Step 2: 全 crate 回归**

Run: `cargo test -p yi-agent-core --lib 2>&1 | tail -15`
Expected: 全绿。

- [ ] **Step 3: fmt 检查**

Run: `cd yi-agent-rs && cargo fmt --all -- --check`
Expected: 无输出。

- [ ] **Step 4: 提交**

```bash
git add docs/bug-list.md
git commit -m "docs: log compact_tool_budget_tokens default mismatch"
```

---

## 验收（对应 spec §7）

```bash
cd yi-agent-rs && cargo test -p yi-agent-core --lib compact
```

必须证明：图片估算非零且等于 `IMAGE_TOKEN_ESTIMATE`；含图场景下较旧的工具单元
会因计入图片成本而被摘要替换；最新的超预算工具单元仍被整体保留（既有测试
`newest_oversized_tool_unit_is_retained_whole` 保持通过）。
