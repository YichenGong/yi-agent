# compaction 图片 token 估算（显式估值）设计

**目标：** 把 compaction 对 `ContentBlock::Image` 的 token 估算从 **0** 改为
**显式估值**，使压缩的保留预算诚实反映图片占用，而不是把图片当“免费”。

**状态：** 设计已确认，待转实现计划。

**范围：** 只改 `crates/yi-agent-core` 的 token 估算。
**不做** provider 层改动、不做请求体级别的全局预算、不改 `view_image`
工具内部（那是另一篇：`2026-09-27-view-image-byte-budget-design.md`）。

**借鉴自 codex**（`codex-rs`，已核实）：codex 的估算器对图片给**非零**显式
估值，而不是 0 —— `core/src/context_manager/history.rs:526-530`
（`RESIZED_IMAGE_BYTES_ESTIMATE = 7373` 字节，注释说明按 4 bytes/token 约
1844 tokens）、`:656` 的 `image_data_url_estimate_adjustment` 把 base64
payload 字节替换为每图固定估值（`original` 档另走 patch 估算，`:616`）。

---

## 1. 问题

`crates/yi-agent-core/src/compact.rs:88`：

```rust
ContentBlock::Image { .. } => 0,
```

`estimate_block_tokens` 把图片估为 0 token。它被用于压缩的保留预算：

- `extract_complete_tool_units` 为每个工具单元算 `token_estimate`
  （`compact.rs:205`：`unit_messages.iter().map(estimate_message_tokens).sum()`），
  `estimate_message_tokens` 又汇总 `estimate_block_tokens`（`compact.rs:93`）。
- 保留选择按该估值对照预算（`compact.rs:229-234`，默认
  `DEFAULT_COMPACT_TOOL_BUDGET_TOKENS = 12_000`，`compact.rs:31`）。

后果：含图的工具单元估值被系统性低估（图照按 0 计），因此：

- **保留端**：压缩会保留比预算本意更多的内容，压缩后历史仍偏大。
- **估算端**：`retained_tool_tokens`（`compact.rs:54`）等指标失真。

同一根因的另一处：`agent.rs:1239` 的 UI prefill 估算也把图片记为 0
（`ContentBlock::Image { .. } => {}`）。该处**只影响 UI 显示**，不参与
compaction 触发或保留，但为一致性建议一并改。

## 2. 关键限制（必须先说清楚，避免误期）

**本改动不是 413 的直接修复。** 依据 `compact.rs:229-234`：

```rust
if selected.is_empty() && unit.token_estimate > budget {
    selected.push(unit);        // 最新单元即使单独超预算也照样保留
    used = unit.token_estimate;
    break;
}
```

保留选择是**从最新往回**遍历的，**最新的那个工具单元无条件保留**（即便它
单个就超预算）。所以：把图片从 0 改成显式估值，**不会**把“最新那张图”挤出
保留窗口。它只会让**较旧**的含图单元更早被摘要替换（`compact.rs:405` 的
`[图片]` 占位），从而使**压缩后的历史更小**。

这与实测一致（`~/.yi-agent/trace/session-20260927-111749.jsonl`）：413 的
元凶图片旧到足以被摘要成 `[图片]`，所以 `/compact` 当时确实解了卡；真正的
兜底是**请求体预算**（见 `2026-09-27-view-image-byte-budget-design.md` §8）。

因此本 spec 的定位是：**让核算诚实**（正确性修复），不是“防 413”。

## 3. 设计

### 3.1 估值来源

不引入解码开销。图片在 `ContentBlock::Image { source: ImageSource::Base64 { .. } }`
里携带 base64 `data: String`，但**不能用 `data.len()/4` 当 token 估值**：模型
不是按 base64 字符数读图，而是按 **patch/面积** 计价（codex 亦如此，
`history.rs:533-545`）。对一个 4 MiB base64 图，`len()/4` 会给出 ~1M token 的
荒谬估值，反而把工具预算整个吃掉。

采用 codex 同款的**固定每图估值**：

```rust
/// 单张图片的模型可见 token 估值。
/// 取自 codex RESIZED_IMAGE_BYTES_ESTIMATE = 7373 字节，按 4 bytes/token
/// 上取整得 1844（codex-rs core/src/context_manager/history.rs:526-530）。
pub const IMAGE_TOKEN_ESTIMATE: usize = 1844;
```

`estimate_block_tokens` 改为：

```rust
ContentBlock::Image { .. } => IMAGE_TOKEN_ESTIMATE,
```

### 3.2 是否需要 `original` 档的差异化估值

codex 对 `detail: original` 走 patch 估算（`history.rs:616-647`，需解码取
宽高 + LRU 缓存）。**本次不做**，理由：

- 我们的 `ContentBlock::Image` 不存宽高，取宽高需解码 base64，成本与复杂度
  显著上升。
- `IMAGE_TOKEN_ESTIMATE` 的固定值足以达成目标（“从 0 变成一个非零的、量级
  正确的估值”）。
- YAGNI：patch 级精度对 12,000 token 的工具预算而言收益有限。

登记为后续可选项（§5）。

### 3.3 UI prefill 估算对齐

`agent.rs:1239`（`ContentBlock::Image { .. } => {}`）改为累加同一常量：

```rust
crate::message::ContentBlock::Image { .. } => total += IMAGE_TOKEN_ESTIMATE,
```

该处只喂 UI 的 prefill 显示，改动无行为风险；对齐后 TUI 显示更接近真实。

### 3.4 边界

- **零成本**：不改任何编码/解码路径，仅改两个 match 分支的返回值。
- **图片省略场景**：`view_image` 超预算时不再附带 Image 块（见另一篇 spec
  §3.5），此时不计入本估值——自然，因为块本身不存在。
- **COMPAT**：`retained_tool_tokens` 等指标会变大（更诚实），依赖这些数字
  的**测试若断言具体数值需同步更新**（见 §4）。

## 4. 测试

先写失败测试再实现（TDD），加到 `crates/yi-agent-core/src/compact.rs` 的
`#[cfg(test)] mod tests`（现有测试在此）：

1. `estimate_block_tokens_counts_image_as_nonzero`
   构造含 `ContentBlock::Image` 的 `Message`/工具单元，断言估值 > 0，且等于
   `IMAGE_TOKEN_ESTIMATE`（文本另计）。
2. `compaction_retains_fewer_older_units_when_images_present`
   构造若干含图工具单元 + 一个小工具预算，断言压缩后**较旧的**含图单元被
   摘要替换、`retained_tool_tokens` 如实反映估值（对比改动前会保留更多）。
3. `newest_tool_unit_is_still_retained_even_if_over_budget`
   固化 §2 的行为：最新单元即使 `token_estimate > budget` 仍被保留——防止
   未来误以为改了估算就能挤掉最新图。
4. 既有 compact 测试保持通过；若因估值变化导致 `retained_tool_tokens` 断言
   漂移，按新语义更新（不弱化断言）。

## 5. 待登记（本次不修）

- **`original` 档 patch 级估值**：codex 的做法（`history.rs:616-647`）；需要
  解码取宽高 + 缓存。收益有限，暂不做。
- **默认 `compact_tool_budget_tokens` 不一致**：`agent.rs:118` 与
  `runtime/config.rs:357` 为 12,000，但 `yi-agent/src/main.rs:1568` 覆写为
  4,096。与本 spec 无依赖，但属潜在意外，建议登记到 `docs/bug-list.md`。
- **请求体级预算**：真正的 413 兜底，登记在
  `2026-09-27-view-image-byte-budget-design.md` §8。

## 6. 改动文件

- `crates/yi-agent-core/src/compact.rs`（`IMAGE_TOKEN_ESTIMATE` + 估算分支 + 测试）
- `crates/yi-agent-core/src/agent.rs`（`:1239` UI prefill 对齐）

**不改**：`yi-agent-tools`、provider 层。

## 7. 验收

```bash
cargo test -p yi-agent-core --lib compact
```
