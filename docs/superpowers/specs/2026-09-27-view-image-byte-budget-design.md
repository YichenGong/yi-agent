# view_image 编码后字节预算（自动降级）设计

**目标：** 让 `view_image` 产出的图片在 **base64 编码后**不超过一个可控的字节预算，
超限时自动、可见地降级，而不是把超大请求体发给上游被网关拒绝（413
`EntryTooBig`）。

**状态：** 设计已确认，待转实现计划。

**范围：** 只改 `view_image` 工具内部（`crates/yi-agent-tools`）。
**不做** provider 层改动、不做请求体级别的全局预算、不改 compaction 的图片
token 估算（见 §8）。

---

## 1. 问题

`docs/bug-list.md` 记录了一条真实故障：

> 读图片遇到 `Error: provider error: server error: unexpected status 413:
> {"error":{"message":"Entry too big ...","type":"upstream_error",
> "code":"EntryTooBig"}}`

根因已核实（`crates/yi-agent-tools/src/fs/view_image.rs`）：

- 唯一的体积闸门是 `MAX_IMAGE_BYTES = 20 MiB`，它检查的是 **读盘前原始文件
  大小**（`:152`），**不是编码后体积**。
- 维度闸门只按**最长边**判定（`:184`）。未超阈值时走
  “保留原字节 + base64”（`:186-192`）；超阈值时 `resize` 后重编码为 PNG
  （`:196-205`）。
- `original` 档最长边上限 6000px（`:19`）。一张 6000x4000 的**照片**重编码为
  PNG 后 base64 可达数十 MiB。

实测量化（PIL 合成样本，可复现；`MiB`，`b64` 为 base64 后字节数）：

| 样本 | 分辨率 | PNG raw | PNG b64 | JPEG q85 | JPEG q50 | JPEG q30 |
|---|---|---|---|---|---|---|
| 照片 | 6000x4000 | 53.56 | **71.41** | 6.68 | 1.96 | 1.03 |
| 照片 | 2048x1365 | 6.24 | 8.32 | 0.78 | 0.23 | 0.12 |
| 照片 | 1024x683 | 1.56 | 2.08 | 0.20 | 0.06 | 0.03 |
| 截图 | 6000x4000 | **0.08** | 0.10 | **1.86** | 1.57 | 1.43 |
| 截图 | 2048x1365 | 0.01 | 0.02 | 0.22 | 0.19 | 0.17 |

两个关键结论直接决定了设计：

1. **413 是"编码后体积"问题，不是"原始文件体积"问题。** 一张压缩良好的
   6000x4000 JPEG 原文件可能只有 5 MiB（能穿过 20 MiB 闸门），但解码后按
   `original` 档保留/重编码，base64 后仍可能远超网关上限。
2. **格式感知是必须的，不能无脑转 JPEG。** 同一张截图：PNG 0.10 MiB vs
   JPEG q85 1.86 MiB —— 转 JPEG 让截图大了约 18 倍。截图是大片纯色 + 锐利
   文字，正是 PNG 的主场。反过来，照片降 quality 收益极大（6.68 → 1.96）。

## 2. 目标与非目标

**目标**

- 工具返回的 `ContentBlock::Image` 的 base64 数据不超过一个默认预算。
- 超限时自动降级（格式/质量/分辨率阶梯），让模型仍能拿到图。
- 降级**可见**：label 里写明实际格式、质量与是否降采样。
- 预算可通过环境变量上浮，以适应不同网关。

**非目标（本次不做）**

- 不改 provider（`openai/types.rs`、`anthropic/types.rs`）序列化。
- 不做**整个请求体**的预算（一个 turn 内多张图叠加可能仍超）；登记为后续项。
- 不改 compaction 把 `ContentBlock::Image` 记为 0 token（`compact.rs:88`、
  `agent.rs:1239`）——独立 bug，已登记。
- 不做图片数量配额、不做用户侧图片入口（TUI 粘贴）。

## 3. 设计

### 3.1 预算定义

```rust
/// base64 编码后的字节上限（真正上 wire 的体积）。
const MAX_ENCODED_BASE64_BYTES: usize = 4 * 1024 * 1024; // ≈ 3 MiB 原始字节
/// 环境变量覆盖（解析失败则回退默认值，并记一条 warn）。
const MAX_ENCODED_ENV: &str = "YI_AGENT_VIEW_IMAGE_MAX_BASE64_BYTES";
```

- 判定对象是 **base64 字符串长度**（`data.len()`），因为那才是请求体里实际
  占用的字节数。
- 覆盖方式沿用本 crate 已有的 `std::env::var` 模式
  （见 `web/bocha.rs:55`）。
- 20 MiB 的 `MAX_IMAGE_BYTES` **保留不动**：它仍是解码前的内存保护闸门，
  与编码后预算职责不同，两者共存。

**预算的解析与注入（消除测试不确定性）：** 预算是**进程级**的，但每个工具
实例只解析一次：

- `fn resolve_budget() -> usize`：读 `MAX_ENCODED_ENV`，`parse::<usize>()`
  成功则用该值，否则用 `MAX_ENCODED_BASE64_BYTES`；解析失败记 `tracing::warn`。
- `ViewImageTool::new(ctx)` 内部调用 `resolve_budget()` 并把结果存进结构体
  字段 `budget: usize`（避免每次 `call` 重复读 env，也避免测试用
  `std::env::set_var` 带来的并发不确定）。
- 纯函数边界：`load_image(path: &Path, detail: ImageDetail, budget: usize)`
  接收显式预算。测试直接对该函数传值，完全绕开 env。

### 3.2 编码阶梯（format-aware）

```
输入：decoded: DynamicImage（原始像素）、source_format、源字节
effective_dim_cap = detail 对应上限（high=2048 / original=6000）
                   —— 只降不升，见 §3.4

步骤 0（原字节直通）
  若 longest_edge(decoded) <= effective_dim_cap
  且 base64(源字节).len() <= 预算
  → 直接返回源字节 + 源 MIME（当前行为，无损零重编码）

步骤 1（无损 PNG）
  在 effective_dim_cap 下 encode PNG（不缩放）
  若 base64 PNG <= 预算 → 返回 image/png

步骤 2（JPEG 质量阶梯）
  依次取 q = [85, 70, 55, 40]
  在 effective_dim_cap 下 encode JPEG(q)
  若 base64 JPEG(q) <= 预算 → 返回 image/jpeg，note = "jpeg q{q}"

步骤 3（降分辨率重试）
  effective_dim_cap = round(effective_dim_cap * 0.75)
  若 effective_dim_cap < MIN_DIMENSION(=512) → 进入步骤 4
  否则重复步骤 1-2（在 512 这一档也要完整试一次）

步骤 4（仍超预算）
  返回 ToolsError（复用现有变体，见 §3.5），错误文本只含数字，绝不回传图片字节
```

设计要点：

- **先试无损再试有损**：截图类图片在第 1 步就达标（0.10 MiB），完全不会走
  到 JPEG；只有大照片才会落到质量阶梯。
- **先降质量再降分辨率**：照片降 quality 的收益远大于降分辨率（6.68 → 1.96
  只需降质量），而分辨率是截图的刚需；因此质量阶梯排在分辨率阶梯之前。
- **FILTER 选择**：沿用现有 `FilterType::Triangle`（`:199`），保持与既有
  缩放行为一致。
- **JPEG 编码 API（已核实）**：`image::codecs::jpeg::JpegEncoder` 在 `jpeg`
  feature 下可用（`lib.rs:258`），质量用
  `JpegEncoder::new_with_quality(w, quality)` 指定
  （`codecs/jpeg/encoder.rs:398`，默认质量 75 见 `:392`）。写出走
  `DynamicImage::write_with_encoder(encoder)`（`images/dynimage.rs:1383`）。
- **PNG→JPEG 需去 alpha**：JPEG 不支持透明通道，编码前统一
  `DynamicImage::to_rgb8()`（`images/dynimage.rs:277`）。对本身是 RGB 的照片
  无影响；对带透明通道的图，透明区域会变黑/白（取 `to_rgb8` 的丢弃语义），
  **仅在第 1 步无损 PNG 也超预算时才会发生**，属于可接受的最后手段。

### 3.3 返回值与 label

`LoadedImage` 增加一个 `note: Option<String>` 字段，承载降级信息
（如 `"jpeg q70"`、`"downscaled"`，两者可叠加为 `"jpeg q55, downscaled"`）。

label 由

```
viewed {path} ({w}x{h}, {media_type})
```

扩展为（无降级时保持与原格式一致，不回归既有测试语义）：

```
viewed {path} ({w}x{h}, {media_type})                        // 未降级
viewed {path} ({w}x{h}, {media_type}, {note})                // 有降级
```

降级必须写进 label，理由：模型需要知道自己看到的不是原图（可能影响它对
细节的判断），用户也从 TUI 的那行文本里看得到发生了什么。

### 3.4 `detail` 语义

`detail` 仍是**起始**分辨率上限：

- `high` = 2048，`original` = 6000。
- 预算驱动的降级可以在其之上**进一步降低** `effective_dim_cap`。
- 永不**上采样**：若源图最长边本就小于当前 cap，不放大。

这是对"预算优先"的明确表态：`original` 表示"允许到 6000px，但受预算约束"，
而不是"无论如何都必须 6000px"。

### 3.5 错误

复用现有 `ToolsError::ImageTooLarge { size, max }`（`error.rs:54`），在
所有阶梯耗尽时抛出，`size` = 最后一个候选的 base64 长度，`max` = 预算。
不新增变体，减少改动面；语义（"图太大"）与新场景吻合。

错误文本只包含数字，**绝不回传二进制或图片字节**（沿用原 spec §4.4）。

### 3.6 依赖

无需新增依赖：

- `image` 的 `jpeg` feature 已开启（`Cargo.toml:35` 的
  `features = ["png","jpeg","gif","webp"]`），`JpegEncoder` 可用。
- `base64` 已在（`Cargo.toml:36`）。

## 4. 边界与已知限制

- **GIF 动图**：直通路径（步骤 0）保留原始字节 → 动画保留。一旦需要重编码
  （步骤 1+），`image` 只解码首帧 → **动画丢失**为静态图。这是既有行为
  （当前超维度的 GIF 也一样），本次不新增损失，但在 label 的 `note` 里对
  GIF 降级标注 `gif→png` / `gif→jpeg`，使损失可见。
- **预算粒度是单图**：一个 turn 内多张图/多个 tool result 叠加进同一请求体
  仍可能超网关上限。登记为后续项（§8）。
- **每张图最多尝试**：`(1 PNG + 4 JPEG) × 分辨率级数`（已含最低 512 档）次
  编码。下限 512、步长 0.75，从 6000 起最多约 10 级，最坏约 50 次编码。
  每次编码在大图上开销可观，但分辨率逐级下降使总成本有上界；实现时只解码
  一次，后续仅做 encode。

## 5. 测试

先写失败测试再实现（TDD）。新增到
`crates/yi-agent-tools/src/fs/view_image.rs` 的 `#[cfg(test)] mod tests`：

**测试图片必须用高熵噪声**，否则 `image` crate 的 PNG 压缩会把合成图压到几
KB，永远触不到预算（实测：现有测试用的 `Rgb([x%256, y%256, 128])` 渐变图，
3000x1000 的 PNG 只有 0.010 MiB）。新增一个 `write_noisy_png(path, w, h)`
辅助函数，用确定性伪随机填充（如 xorshift）生成不可压缩图。

1. `view_image_shrinks_oversize_photo_to_fit_budget`
   写 3000x2000 噪声 PNG，调用时传一个较小的显式预算（如 256 KiB）。断言：
   成功、返回的 base64 `data.len() <= budget`、且解码后是合法图片。
2. `view_image_keeps_lossless_png_when_it_fits`
   小尺寸噪声 PNG，预算充足。断言：`media_type == "image/png"` 且字节与源文件
   一致（直通，无重编码）。
3. `view_image_prefers_png_for_screenshots_over_jpeg`
   大片纯色 + 少量线条的 4000x3000 PNG（截图层级），预算设在"PNG 能过、但
   若先转 JPEG 会因体积暴涨而更差"的区间。断言：`media_type == "image/png"`
   （不得因为超维度就盲目转 JPEG）。
4. `view_image_label_notes_degradation`
   触发降级的场景。断言：`content[0]`（Text label）包含降级标记
   （如 `jpeg` 与/或 `downscaled`）。
5. `view_image_budget_env_override_is_honored`
   直接验证 `resolve_budget()`：设置 env 后断言解析出该值；env 非法时回退
   默认值。**不改进程 env 去跑 `call()`**，避免并发不确定（§3.1 已把预算改为
   构造期注入）。
6. `view_image_falls_back_to_error_when_budget_unreachable`
   极小预算（如 1 KiB）。断言：`is_error`，且错误文本不含 base64 数据。
7. 既有 10 个测试保持通过（回归）：尤其
   `view_image_original_detail_keeps_more_resolution`（3000x1000 渐变图，
   体积远低于默认预算 → 仍应保持 3000px 不缩放）与
   `view_image_resizes_large_image`（high 档 2048）。

**预算可注入**：实现把预算作为参数（默认值由构造期解析的常量/env 决定），
测试直接传值，从而完全绕开进程级 env 的不确定性（见 §3.1）。

## 6. 改动文件

- `crates/yi-agent-tools/src/fs/view_image.rs`（阶梯 + label + 测试）
- `docs/project-management/yi-agent-tools.md`（feature 行补充预算行为）
- `docs/bug-list.md`（把 413 那条从"待办"改为已修，附验证命令）

**不改**：`yi-agent-llm`、`yi-agent-core`、provider 层。

## 7. 验收

```bash
cargo test -p yi-agent-tools --lib fs::view_image
```

全部通过（既有 10 个 + 新增约 6 个），且新增测试能证明：
(a) 超限照片被降进预算；(b) 能无损时不做有损；(c) 截图不被无谓转 JPEG；
(d) 降级在 label 里可见；(e) 预算不可达时报错且不回传字节。

## 8. 待登记（本次不修）

- **请求体级预算**：单图有预算，但一个 turn 内多张图（多个 `view_image`
  调用、或与文本叠加）合计仍可能超网关上限，需要 provider/agent 层的请求体
  预算，属独立迭代。以下是**实测**（trace 证据）对该场景的机制说明。

  **实测复现**（`~/.yi-agent/trace/session-20260927-111749.jsonl`，同一天真实
  会话）：连续两次 `view_image`（11:51:38）后，下一次 THINK 请求即 413。
  `msg_count` 74 → 发出请求 → 413；重试一轮 → 再次 413（两次 `provider call
  failed: unexpected status 413`）；该会话**从未触发过 auto-compact**（trace
  里 `AutoCompacting` 计数为 0）；用户执行 `/compact` 后 `msg_count` 75 → 18，
  随后请求成功（12:01:24 `provider call_stream returned Ok`）。**结论：413
  确实会卡住，且唯一的出路是用户手动 `/compact` 或 `/clear`。**

  **为什么 auto-compact 没救场**（三条独立机制，均已核实）：

  1. **触发依赖真实 usage，而 413 在 usage 到达前就终止整轮。**
     `maybe_auto_compact` 用 `snapshot.last_input_tokens()`（`agent.rs:1142`），
     后者只由真实 API usage 写入（`agent.rs:699`；另有 resume 路径
     `app-server/src/server.rs:536`）。启发式 `estimate_prefill_tokens` 与触发
     无关（仅 UI 事件）。而 413 经 `map_status_error` 落到 `_ => Server(...)`
     （`openai/error.rs:23`），不在重试白名单内（`agent.rs:650-651` 只重试
     `Stalled` / `Network`），随即走终结分支 `agent.rs:709-715` 直接 `return`
     —— 既不记录 usage，也不会再进入下一轮 `maybe_auto_compact`。
  2. **字节超限不必然伴随 token 超阈值。** `compact_threshold` 是 **token**
     数（`agent.rs:1140`，运行时算作 `effective_context_length * ratio / 100`，
     `runtime/config.rs:247`）。图片是 MB 级字节但 token 很少；本次请求的
     实际量级与其文本/工具历史对比，说明**字节与 token 是两个正交维度**，
     所以"字节超限"不会自动反映到 token 阈值上。
  3. **保留预算把图片记为 0。** `estimate_block_tokens` 的
     `ContentBlock::Image { .. } => 0`（`compact.rs:88`）使含图的 tool unit
     在 `compact_tool_budget_tokens`（默认 12,000）下看起来"免费"，加上
     最近单元按 recency 保留。但需注意：**/compact 实测确实解了卡**，说明
     本例中那些图大概率落入了**被摘要替换掉的旧单元**（旧图折叠为
     `[图片]`，`compact.rs:405`），而非被保留。因此本条不如 1、2 强：它
     只在图片位于保留窗口内时才成立。

  **结论**：多图/大图的唯一可靠补救是**请求体预算**（或修正图片 token
  估算），不能依赖 auto-compact；手动 `/compact` 是现有逃生口，但要求用户
  知道该这么做，且会丢掉上下文细节。

- compaction token 估算把 `ContentBlock::Image` 记为 0 token
  （`compact.rs:88`、`agent.rs:1239`）——已在 `docs/bug-list.md` 登记。
  注意其中 `agent.rs:1239` 那处**只影响 UI 的 prefill 估算**（不参与 compact
  触发）；真正有行为后果的是 `compact.rs:88` 的保留预算估算。
