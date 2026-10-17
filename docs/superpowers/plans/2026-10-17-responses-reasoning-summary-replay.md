# Responses reasoning item 回放补 `summary` 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复 `/v1/responses` 通道在工具循环第二轮重放 reasoning item 时缺少上游必需的 `summary` 字段导致的 400。

**Architecture:** 让携带 reasoning 凭证的类型（`ThinkingToken::Encrypted`）同时保存 `summary` 数组（逐字保真），流式解析时捕获、回放时原样带上；对缺 `summary` 的旧数据在回放端兜底为空数组 `[]`。

**Tech Stack:** Rust 2024、serde、serde_json、tokio（既有）。

## Global Constraints

- 提交前在 `yi-agent-rs/` 下跑 `cargo fmt --all`。
- 严禁在 `main` 上直接改；本计划在 worktree `.worktrees/fix-responses-reasoning-summary`（分支 `fix/responses-reasoning-summary`）内执行。
- commit message 用 conventional commits，不写 `Co-Authored-By`。
- 凭证逐字保真（设计 §1.3 铁律）：`encrypted_content` 与 `summary` 都不得重编码、不得过滤。

---

## 背景（实测结论，2026-10-17）

真实往返（`gpt-6-luna`，`llmapi.horizon.auto/v1`）实测：

| 重放形态 | 结果 |
|---|---|
| `reasoning` + `encrypted_content`，**无 `summary`**（当前代码） | **400 `Missing required parameter: 'input[1].summary'`** |
| `reasoning` + `encrypted_content` + `summary:[]` | 200 |
| `reasoning` + `encrypted_content` + 非空 `summary:[{type,text}]` | 200 |
| `summary` 传字符串而非数组 | 400（必须是对象数组） |
| 丢弃 reasoning item | 200 |

上游返回的 reasoning item 形状：`{id, type, content, encrypted_content, summary}`，`summary` 常见为 `[]`，**但必须显式发这个空数组，不能省略字段**。

## 文件结构

- `yi-agent-rs/crates/yi-agent-core/src/message.rs`：`ThinkingToken::Encrypted` 增载 `summary`；向后兼容反序列化。
- `yi-agent-rs/crates/yi-agent-llm/src/openai/responses/stream.rs`：解析 reasoning item 时捕获 `summary`。
- `yi-agent-rs/crates/yi-agent-llm/src/openai/responses/types.rs`：回放 reasoning item 时带上 `summary`。
- `yi-agent-rs/crates/yi-agent-llm/src/anthropic/types.rs`：`Encrypted(_)` 的匹配改新形状（仍是不可达/过滤路径）。

---

### Task 1: 回放必带 `summary`（最小修复，先锁 400）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/responses/types.rs`
- Test: 同文件 `mod tests`

**Interfaces:**
- Consumes: `ContentBlock::Thinking { token: ThinkingToken::Encrypted(...) }`
- Produces: reasoning item 恒含 `"summary": [...]`

- [ ] **Step 1: 写失败测试**

在 `types.rs` 的 `mod tests` 增一条：断言回放出的 reasoning item 一定带 `summary`（现有 token 形状）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p yi-agent-llm --lib openai::responses::types`
Expected: FAIL —— `summary` 字段缺失。

- [ ] **Step 3: 最小实现**

回放时补 `"summary": []`。

- [ ] **Step 4: 跑测试确认通过**

Run: 同上。Expected: PASS。

- [ ] **Step 5: commit**

`fix(llm): replay a summary on responses reasoning items`

---

### Task 2: 凭证携带并逐字回放真实 `summary`（B 方案内核）

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/message.rs`
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/responses/stream.rs`
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/openai/responses/types.rs`
- Modify: `yi-agent-rs/crates/yi-agent-llm/src/anthropic/types.rs`
- Test: 各文件 `mod tests`

**Interfaces:**
- Produces:
  - `ThinkingToken::Encrypted { encrypted_content: String, summary: Vec<serde_json::Value> }`
  - `ProviderEvent::ThinkingBlock { token: ThinkingToken::Encrypted { .. } }` 携带捕获到的 `summary`
  - 回放：`{"type":"reasoning","encrypted_content":..., "summary": <捕获值或 []>}`

- [ ] **Step 1: 写失败测试**（捕获端 + 回放端）
- [ ] **Step 2: 跑测试确认失败**
- [ ] **Step 3: 实现类型变更 + 捕获 + 回放**
- [ ] **Step 4: 跑测试确认通过**
- [ ] **Step 5: commit**

---

### Task 3: 旧数据向后兼容

**Files:** `yi-agent-rs/crates/yi-agent-core/src/message.rs`（测试）

- [ ] **Step 1: 写失败测试**：`{"Encrypted":"legacy"}` 反序列化为 `Encrypted { encrypted_content:"legacy", summary:[] }`。
- [ ] **Step 2: 跑测试确认失败**
- [ ] **Step 3: 实现自定义 `Deserialize`（同时接受字符串与对象形态）**
- [ ] **Step 4: 跑测试确认通过**
- [ ] **Step 5: commit**

---

### Task 4: 真实往返验证 + 文档

- [ ] 真实网关跑一次「带工具的第 2 轮」（修复前 400 → 修复后 200）。
- [ ] 更新 `docs/superpowers/specs/2026-10-15-cot-round-trip-and-anthropic-thinking-design.md` §8.2 记录该缺陷与修复。
- [ ] `docs/bug-list.md` 补条目。

## Self-Review

- 规格覆盖：Task 1–3 覆盖「回放必带 summary」+「逐字保真携带」+「向后兼容」；Task 4 覆盖实测与文档。
- 类型一致：`Encrypted { encrypted_content, summary }` 三处使用点统一（capture / replay / anthropic filter）。
- 无占位符。
