# 图片读取（view_image 工具）设计

**目标：** 让模型能主动读取工作区里的图片文件（PNG/JPEG/GIF/WebP），并把
图片作为多模态内容块送进 LLM 上下文；Anthropic 与 OpenAI 两个 provider 都
生效。

**状态：** 设计已确认，待转实现计划。

**范围：** 只做「模型调用工具读图」这一条入口（新增 `view_image` 工具）。
**不做** TUI 剪贴板粘贴 / 用户直接附带图片 —— 那是另一条独立入口，另行迭代。

---

## 1. 问题

`docs/bug-list.md:10` 记录的开放问题「确认是否支持图片读取」。核实结论：
**端到端不支持**，只有类型层脚手架。

已核实的事实：

- **类型已留位**：`crates/yi-agent-core/src/message.rs:36-45` 定义了
  `ContentBlock::Image { source: ImageSource }` 与
  `ImageSource::{Base64, Url}`。
- **Anthropic 已能序列化**：`crates/yi-agent-llm/src/anthropic/types.rs:125`
  把 `ContentBlock::Image` 转成 `AnthropicContentBlock::Image`。
- **但没有任何生产代码构造图片块**：`grep` 确认 `ContentBlock::Image` 的
  构造只出现在 `message.rs` 的单测里（`:131`、`:148`）。
- **read 工具只读文本**：`crates/yi-agent-tools/src/fs/read.rs:109` 用
  `std::fs::read_to_string`，读二进制/图片会因非法 UTF-8 报 `ToolsError::Io`。
- **OpenAI 静默丢图**：`crates/yi-agent-llm/src/openai/types.rs:99-108`
  `extract_text` 的 `_ => None` 分支丢弃一切非文本块；`OpenaiContent`
  （`:41-46`）只有 `Text`/`ToolCalls`，没有图片变体。
- **无用户入口**：`Agent::run`（`agent.rs:339-364`）只接受 `String`；TUI
  `handle_paste`（`app.rs:1098-1113`）把粘贴内容当纯文本插入。
- 文档也确认这是缺口：`docs/project-management/yi-agent-core.md:35`
  「图片工具（`ContentBlock::Image` 已留类型，无对应 Tool 实现）」。

## 2. 参考实现（codex）

参考 `/Users/gongyichen/Documents/TechnicalStuff/projects/OpenSource/codex`：

- 有**独立工具** `view_image`（`core/src/tools/handlers/view_image.rs`），
  与 shell/read 分开。
- 工具只读原始字节，包成 `application/octet-stream` 的 data URL
  （`utils/image/src/lib.rs:53` `data_url_from_bytes`）；**真正的解码/缩放/
  重编码发生在历史插入路径**（`core/src/image_preparation.rs`）。
- 支持格式：PNG / JPEG / GIF / WebP（`image::guess_format`，见
  `utils/image/src/lib.rs:104-112`）。缩放上限 `MAX_DIMENSION = 2048`
  （`:23-32`）。
- 有 `detail`（`high` / `original`），`original` 由模型能力门控
  （`tools/src/image_detail.rs`）。
- 请求体里的图片是 Responses API 的 `input_image` 内容块。

yi-agent 与 codex 的**关键差异**：

1. yi-agent 的 provider 是 **Chat Completions（OpenAI）/ Messages
   （Anthropic）**，不是 Responses API。
2. codex 把解码/缩放推迟到历史插入；yi-agent 没有等价的统一插入钩子，
   因此本设计**把解码/缩放放在工具内部**（工具返回时图片已经是处理好的
   最终形态），逻辑更内聚。

## 3. 实现约束（已核实）

- **OpenAI 的 `tool` 角色消息不能带图片**：Chat Completions 中
  `role:"tool"` 的 content 只能是字符串或文本 part 数组，图片 part
  （`image_url`）只允许出现在 `role:"user"` 消息里。因此「工具返回图片」在
  OpenAI 侧必须改写。
- **Anthropic 的 `tool_result` 可以带图片**：`AnthropicContentBlock::ToolResult`
  的 `content` 是 `Vec<AnthropicContentBlock>`（`types.rs:40-44`），图片可
  直接嵌进去，无需改写。
- **Anthropic 无 `detail` 字段**：`detail` 只对 OpenAI 的 `image_url` 有意义；
  Anthropic 侧忽略。
- **system 消息里的图片会被 Anthropic 丢弃**（`types.rs:178-186` 只取文本）。
  本设计不产生 system 图片，不受影响。
- **`ContentBlock::Image` 的现有解构点大多用 `{ .. }`**：`compact.rs:88`、
  `compact.rs:405`、`agent.rs:1078`、`app-server/translate.rs:287` 均为
  `ContentBlock::Image { .. }`，加字段不会破坏；只有
  `anthropic/types.rs:125` 与 `message.rs:131,148` 需要显式补字段。
- **compaction 把图片记为 0 token**：`compact.rs:88` `Image => 0`、
  `agent.rs:1078` 跳过图片。图片真正进入上下文后会低估 token，可能延迟
  auto-compact 触发。**本次不改**，只登记到 `bug-list.md`。
- **base64 目前不是直接依赖**：`Cargo.lock` 里有（reqwest 传递依赖），
  `yi-agent-tools/Cargo.toml` 需要显式声明。`image` 目前完全不在依赖树里。

## 4. 设计

### 4.1 数据模型（`yi-agent-core/src/message.rs`）

`ContentBlock::Image` 增加 `detail`：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ImageDetail {
    #[default]
    High,
    Original,
}

pub enum ContentBlock {
    // ...
    Image {
        source: ImageSource,
        #[serde(default)]
        detail: ImageDetail,
    },
}
```

`detail` 是**工具产出的语义信息**（缩放档位），同时供 OpenAI provider 映射到
wire 字段。Anthropic 忽略它。

同步更新 `anthropic/types.rs:125` 的解构（`ContentBlock::Image { source, .. }`）
与 `message.rs` 的两个测试。

### 4.2 新增 `view_image` 工具（`yi-agent-tools`）

新文件 `crates/yi-agent-tools/src/fs/view_image.rs`，在 `lib.rs:66` 附近与
`read` 并列注册（`metadata()`：`read_only: true`、
`requires_confirmation: false`、`source: Builtin`）。

**参数**

| 参数 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `path` | string | 是 | 工作区内的相对/绝对路径，经 `resolve_and_check` 限根 |
| `detail` | string | 否 | `high`（默认）或 `original`；其他值报参数错误 |

**流程**

1. `resolve_and_check(ctx.root(), path)` 限根；`std::fs::metadata` 确认是
   普通文件（不存在 → `ToolsError::NotFound`，目录 → 错误）。
2. `std::fs::read` 读字节；超过 `MAX_IMAGE_BYTES = 20 * 1024 * 1024` → 错误。
3. `image::guess_format(&bytes)` 识别格式；只接受 PNG/JPEG/GIF/WebP，其他
   → 错误（提示「unsupported image format」）。
4. `image::load_from_memory_with_format` 解码。
5. 若最长边 > 阈值（`high` = 2048，`original` = 6000）→ 等比缩放
   （`DynamicImage::resize`，保持宽高比）。
6. 编码：
   - **未缩放** → 保留原始字节 + 由格式推出的 MIME（无损、零重编码开销）
   - **缩放后** → 重编码为 PNG（`image/png`）
7. `base64::engine::general_purpose::STANDARD.encode` 编码。
8. 返回 `ToolResult`，`content` = `[Text(label), Image{ source: Base64{media_type, data}, detail }]`，
   其中 `label` 形如 `viewed src/logo.png (240x160, image/png)`，给模型一个
   可引用的路径与尺寸上下文。

**新增依赖**（`yi-agent-tools/Cargo.toml`）：

```toml
image = { version = "0.25", default-features = false, features = ["png", "jpeg", "gif", "webp"] }
base64 = "0.22"
```

### 4.3 Provider 序列化

**Anthropic**（`anthropic/types.rs`）：已支持，仅补 `detail` 解构；`detail`
不映射到 wire（Anthropic 无该字段）。

**OpenAI**（`openai/types.rs`）：新增图片内容 part：

```rust
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenaiContentPart {
    Text { text: String },
    ImageUrl { image_url: OpenaiImageUrl },
}

#[derive(Serialize)]
struct OpenaiImageUrl {
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}
```

`OpenaiContent` 增加数组变体（`Parts(Vec<OpenaiContentPart>)`），与现有
`Text(String)` 用 `#[serde(untagged)]` 共存。

- **user 消息**：若含图片 → 输出 `Parts`（文本 part + 每个图片一个
  `ImageUrl` part，`url` 为 `data:<mime>;base64,<data>`，`detail` 取
  `ImageDetail` 的小写名）；若纯文本 → 维持现有 `Text(String)`（不破坏既有
  序列化与测试）。
- **tool 消息**（`Role::Tool`，`:192-215`）：每个 `ToolResult` 先输出一条
  `role:"tool"` 消息（content = 文本；图片替换为占位文本，如
  `[image attached in the following message]`），**若该 ToolResult 含图片**，
  再紧跟一条 `role:"user"` 消息（`Parts`，只含 `ImageUrl` part）。
  这样既满足 OpenAI「每个 tool_call 有配对 tool 消息」的要求，又把图片放进了
  允许图片的 user 消息。

### 4.4 错误处理

工具错误统一走 `ToolsError`（`tools/error.rs`）→ `ToolResult::error`，与现有
工具一致。新增/复用：

- 路径不存在 → `ToolsError::NotFound`
- 目录 / 非文件 → `ToolsError::Io(InvalidInput)`
- 超大文件 → 新增 `ToolsError::ImageTooLarge { size, max }`
- 格式不支持 / 解码失败 → 新增 `ToolsError::ImageDecode(String)`
- `detail` 取值非法 → `ToolsError::ArgsParse`

错误只返回文本，**绝不回传二进制或图片字节**。

## 5. 测试

先写失败测试再实现。

**`view_image.rs`（单测）**

1. `view_image_returns_image_block_for_png` —— 现场用 `image` crate 生成
   小 PNG，断言 `ToolResult.content` 含一个 `ContentBlock::Image`，其
   `media_type == "image/png"`、data 可 base64 解码回合法 PNG
2. `view_image_accepts_jpeg` —— 生成 JPEG，`media_type == "image/jpeg"`
3. `view_image_resizes_large_image` —— 生成 3000x1000 PNG，`high` 下
   解码结果最长边 == 2048 且宽高比不变
4. `view_image_original_detail_keeps_more_resolution` —— 同一大图，
   `original` 下最长边 > 2048（或未被裁到 2048）
5. `view_image_rejects_unsupported_format` —— 写一段非图片字节（如纯文本
   `.txt`），断言 `is_error`
6. `view_image_rejects_oversize_file` —— 写入 > 20MB 文件，断言 `is_error`
7. `view_image_missing_file_errors`
8. `view_image_rejects_invalid_detail` —— `detail: "low"` 报参数错误
9. `view_image_escapes_root` —— 路径越界被 `resolve_and_check` 拒绝
10. `view_image_label_mentions_path_and_dimensions`

**`openai/types.rs`（单测）**

11. `user_message_with_image_serializes_content_parts` —— user 消息含
    Text+Image，断言 `content` 是数组，含 `{"type":"image_url","image_url":{"url":"data:image/png;base64,...","detail":"high"}}`
12. `tool_result_with_image_splits_into_tool_then_user` —— 断言产出两条消息：
    第一条 `role:"tool"`（content 为字符串、含占位符），第二条 `role:"user"`
    含 `image_url` part，且 `tool_call_id` 配对正确
13. `tool_result_without_image_stays_string` —— 回归：纯文本 tool result
    仍是 `content: "<string>"`（既有行为不回归）

**`anthropic/types.rs`（单测）**

14. `tool_result_image_preserved` —— `ToolResult` 内嵌 Image，序列化后
    `content[..]` 含 `{"type":"image","source":{"type":"base64",...}}`

**（可选）wiremock 集成**：在 `yi-agent-llm` 的 mock 测试里断言 OpenAI 请求体
出现 `image_url`。

## 6. 范围

**改动文件**

- `crates/yi-agent-core/src/message.rs`（`ImageDetail` + 字段）
- `crates/yi-agent-tools/src/fs/view_image.rs`（新增）、`fs/mod.rs`、`lib.rs`、
  `error.rs`、`Cargo.toml`
- `crates/yi-agent-llm/src/openai/types.rs`、`anthropic/types.rs`
- `docs/bug-list.md`、`docs/project-management/yi-agent-core.md`、
  `docs/project-management/yi-agent-tools.md`、`docs/project-management/yi-agent-llm.md`、
  `README.md`（计数同步）

**不做**

- TUI 剪贴板粘贴图片、用户消息直接带图（另一条入口）
- Responses API 迁移
- `detail` 的模型能力门控（项目目前没有 modality 元数据；`detail` 仅作为
  缩放档位与 OpenAI wire 字段）
- 修正 compaction 的图片 token 估算（仅登记 bug）
- 图片生成 / 编辑

## 7. 待登记 bug（本次不修）

- compaction token 估算把 `ContentBlock::Image` 记为 0 token
  （`compact.rs:88`、`agent.rs:1078`），图片进入上下文后可能低估、延迟
  auto-compact 触发。
