# 请求体超限：图片字节预算与分级降级设计

日期：2026-11-07
状态：设计已评审，待实施

## 1. 症状

用户在某条会话里持续收到：

```
provider error: invalid request: unexpected status 413:
{"error":{"message":"Request body too large. Please reduce the request body size.",
"type":"upstream_error","code":"413001"}}
```

会话标题「根据户型图在 Blender 复刻 A 反户型平面」，工作目录
`InteriorDesign`。

### 1.1 现场数据

`InteriorDesign/.yi-agent/threads/thread-42208b79-140a-40e0-82d5-3455db7e554a.jsonl`
已达 **207 MB**。失败发生在该会话 turn 137
（trace `session-20261106-165749.jsonl`，2026-11-07T09:46:36）。

| 指标 | 数值 | 取法 |
|---|---|---|
| 该轮消息数 | 172 | jsonl `messages` 数组长度 |
| 序列化后请求体 | **69.3 MB** | `len(json.dumps(messages))` |
| 其中图片 base64 | **69.14 MB（99.8%）** | 累加 `Image.source.Base64.data` 长度 |
| 该轮估算 token | **81,195** | 复刻 `estimate_block_tokens` 口径 |
| auto-compact 阈值 | **160,000** | `yi-agent-app-server/src/model_rpc.rs:516` |

结论：**不是 token 超限。** 压缩按 token 判定没到阈值，而网关按**字节**拒绝，
两个口径差约 1000 倍。

### 1.2 为什么 413 不可重试

`401/404/413` 等未列举 4xx 在 `openai/error.rs:31` 与 `anthropic/error.rs:29`
的兜底分支被归为 `InvalidRequest`（确定性错误）；`agent.rs:875` 的
`retryable_provider_error_cause` 对 `InvalidRequest` 返回 `None`（`agent.rs:880`）。
该判定是正确的：原样重发同样的 69 MB 必然再次失败。**不能靠加重试解决。**

## 2. 根因

### 2.1 入库预算形同不设

`image_prep.rs:17` `DEFAULT_BASE64_BUDGET = 4 MiB`。该会话四张图的实测：

| 文件 | 原始 | base64 | 是否原样直通 |
|---|---|---|---|
| furnished_plan.png | 2.93 MB | 3.91 MB | 是（3.91 < 4.00） |
| furnished_axon.png | 2.17 MB | 2.90 MB | 是 |
| detail_baths.png | 1.47 MB | 1.96 MB | 是 |
| detail_entry.png | 1.44 MB | 1.92 MB | 是 |

`encode_within_budget`（`image_prep.rs:134`）的「步骤 0：原字节直通」
（`image_prep.rs:145-152`）只判是否超预算，未超即完全不重编码。四张图恰好都
卡在 4 MiB 线内，**缩放阶梯一次都没走到**。

### 2.2 同一张图重复嵌入，放大 4.7 倍

失败轮 25 个图片块只对应 **6 个不同 path**：

```
9x 3.91MB  furnished_plan.png
6x 1.96MB  detail_baths.png
5x 1.92MB  detail_entry.png
2x 2.90MB  furnished_axon.png
2x 2.13MB  detail_master.png
1x 1.74MB  view_entry.png
```

嵌入合计 68.35 MB，去重后仅 14.55 MB。模型每次重新 `view_image` 同一 path，
历史就多一份完整副本。

### 2.3 token 估算与字节完全脱钩

`compact.rs:38` `IMAGE_TOKEN_ESTIMATE = 1844`（源自 codex 的
`RESIZED_IMAGE_BYTES_ESTIMATE = 7373` 假设），而我们实际 wire 上是 2~4 MB
base64。`agent.rs:2218` 的 `estimate_prefill_tokens` 同口径。
**每张图按 1844 token 计，真实字节却可能是 4 MB。**

### 2.4 发送侧无字节闸

`openai/client.rs:245` 只记录 `msg_count`，不记录 body 字节；序列化后直接
POST，超限只能等网关拒绝。`compact.rs:428` 在生成摘要时把图片写成
`[图片]` 占位——但压缩本身没被触发，这道兜底没生效。

## 3. 关键约束：前端不依赖消息里的 base64

`desktop/src/components/ChatView.tsx:221`（用户附件）与
`desktop/src/components/ToolCallCard.tsx:98`（工具产出图）都只取
`img.path`，经 `image/read` 分片读回**原文件**渲染。

因此消息里的 base64 **纯粹是给模型看的**：缩小或替换它，对用户界面零影响，
用户始终看到原图。这是本设计敢于在请求侧降级的前提。

## 4. 设计原则

1. **归档保真**：磁盘上的历史（`.jsonl`）**永不修改**。所有降级只作用于
   发往 provider 的请求副本。
2. **默认不缩**：正常会话（未超预算）一个字节不动。缩放是超限时才启用的
   降级手段，不是默认行为。
3. **从无损到有损**：先做完全无损的去重，再逐级引入损失。
4. **测量而非猜测**：以序列化后的真实字节为准，不依赖 token 估算。

### 4.1 为什么不做「入库时缩小」

| 方案 | 归档 | 可逆性 | 质量决定 |
|---|---|---|---|
| 入库缩放 | 永久失去原图 | 不可逆 | 替用户提前决定 |
| 发送时投影 | 原图完整保留 | 每次可重算 | 只在超限时才决定 |

实测像素差（furnished_plan.png，放大回原尺寸比对）：

| 上限 | 尺寸 | 平均像素差 |
|---|---|---|
| 1536 | 1208×1536 | 0.18% |
| 1024 | 805×1024 | 0.31% |
| 768 | 604×768 | 0.43% |

肉眼近乎无差，但**不是 0**。本会话的精确尺寸来自 PDF 文本而非图像像素；
换一张只有图上标注毫米数的图纸，缩到 1024 可能真的读不出。**默认缩图等于
替用户做了一个他可能不同意的质量决定**，因此只在必要时做。

## 5. 分级降级阶梯

在 `think_phase` 构造请求副本处（`agent.rs:1151`，`request_messages` 之后、
`ProviderRequest` 之前）插入一次投影，按真实字节逐级降级，**能停在哪一级
就停在哪一级**：

| 级别 | 动作 | 质量损失 | 触发 |
|---|---|---|---|
| 0 | 原样发送 | 无 | body ≤ 软阈值 |
| 1 | 同 path 去重（旧的换 `[图片已省略: path]`） | **无** | body > 软阈值（2 MB） |
| 2 | 从最旧开始逐张缩到 1024 长边，每降一张检查是否收敛，收敛即停 | 0.31% | 仍 > 硬阈值（8 MB） |
| 3 | 缩不动的图（无实现 / 解码失败 / 缩了不省字节）换占位 | 只剩路径 | 仍 > 硬阈值 |
| 4 | 降级到底仍超限 → 本地报错，**不发送** | — | 仍 > 硬阈值 |

**关键修正（实现期发现）**：最初设计的「保留最近 3 张原图」在第 2 级里是**硬保护**，
这会让阶梯不收敛。用实测形状量化：每张图 3.91 MB base64、6 个不同 path、硬阈值 8 MB，
去重后仍 23.5 MB；保住 3 张原图已 11.7 MB > 8 MB，而阶梯没有下一步 —— 请求照旧发出去，
413 照旧发生。**「保留最近 N 张」必须是优先级而非绝对约束**：第一轮跳过它们，若缩小
其余图片仍不收敛，第二轮连受保护的图一起降级。代价是极端形状下最新几张也会掉到
1024 长边，但 0.31% 的像素差远好于 413 让整轮对话彻底卡死。

原设计的「第 4 级触发 `compact_session`」被**移除**：`compact_session` 会永久替换
会话历史（`compact.rs:449`，且不归档原文），用它来救一次请求体超限的代价过大，
不该在「只是发送超限」这个层次上自动触发。压缩仍由既有的 token 阈值路径负责。

### 5.1 阈值

| 名称 | 值 | 理由 |
|---|---|---|
| 软阈值 | 2 MB | 去重无损，可以早在体积失控前做 |
| 硬阈值 | 8 MB | 网关真实上限未知（只知道 69 MB 被拒），保守取值 |
| 保留原图张数 | 最近 3 张 | 未定论，见 §9 |

两者均可配置：新增 `AgentConfig` 字段
`request_body_soft_limit: usize` / `request_body_hard_limit: usize`，
默认 2 MiB / 8 MiB。网关若反馈真实上限，只调配置不改代码。

### 5.2 「最近 3 张」的判定

按 `ContentBlock::Image.path` 从历史末尾向前数，前 3 个**不同 path** 记录为
「受保护」；第一轮降级跳过它们。

同一 path 的早期副本已在第 1 级被去重，故此处只剩唯一一份。无 `path` 的图片
（旧序列化数据、直接构造的测试数据）不参与去重，也**永不占位**——模型无法
重取，丢掉等于永久丢失。这类图只能靠缩放降级；连缩放都不可用时，由第 4 级
本地报错兜底。

### 5.3 为什么可以把压缩排除在阶梯之外

`compact.rs:428` 的 `format_messages_for_summary` 把图片统一写成
`[图片]`，摘要请求是**纯文本单条消息**。实测该会话 172 条历史（69 MB 图片）
对应的摘要 prompt 实测仅 **0.151 MB**（复刻 `format_messages_for_summary` 口径
统计该轮 172 条历史）。**压缩请求不会被 413 卡住**，因此可以在
故障发生后作为自救手段运行。

## 6. 组件

### 6.1 `yi-agent-core::image_projection`（新增模块）

纯逻辑，无图像编解码依赖：

```rust
pub struct ProjectionLimits {
    pub soft_limit: usize,
    pub hard_limit: usize,
    pub keep_full_images: usize,
}

pub enum ProjectionStep { AsIs, Deduped, Downscaled, Placeheld }

pub struct Projection {
    pub messages: Vec<Message>,   // 可直接序列化的降级副本
    pub step: ProjectionStep,     // 实际走到第几级（取最重的一步），供日志/事件
    pub bytes_before: usize,
    pub bytes_after: usize,
    pub placeheld: usize,
    pub downscaled: usize,
}

pub fn project(messages: &[Message], limits: &ProjectionLimits) -> Projection;
pub fn project_with(
    messages: &[Message],
    limits: &ProjectionLimits,
    downscaler: Option<&dyn ImageDownscaler>,
) -> Projection;
```

**与初稿的差异**：初稿是 `plan_projection` 返回
`dedup_targets` / `downscale_targets` 两个下标向量，由调用方执行。实现改为
**就地改写副本并返回结果**，因为判定与执行要交替进行——每降一张图都得重新
量字节才能知道要不要继续，先算完整计划再执行无法表达「收敛即停」。

字节大小直接量 base64 长度（无损，无需解码）。

### 6.2 图像降级的注入点

core 不依赖 `image` crate（`yi-agent-core/Cargo.toml` 无该依赖），因此按
既有 trait 注入先例（`ProviderTurnGate`：`agent.rs:33`）新增：

```rust
pub trait ImageDownscaler: Send + Sync {
    /// 把 base64 图片缩到 `max_edge` 长边，返回新的 base64 与 media_type。
    /// `None` = 本实现不支持，调用方按第 3 级处理。
    fn downscale(&self, data: &str, media_type: &str, max_edge: u32) -> Option<(String, String)>;
}
```

实现放 `yi-agent-tools::image_downscale::ImagePrepDownscaler`（复用
`image_prep.rs` 已有的 `resize_to_cap` + `jpeg_encode`，两者为此上调为
`pub(crate)`），core 侧 `AgentConfig` 持有
`Option<Arc<dyn ImageDownscaler>>`；未注入时第 2 级自动退化为第 3 级。

**唯一装配点**：`yi-agent-runtime::bootstrap::build_agent_config`
（`bootstrap.rs:417`）。该函数是所有 `AgentConfig` 的集中构造处，在此注入
`Some(Arc::new(ImagePrepDownscaler::new()))`，其余调用点走
`..Default::default()` 不重复设置——避免各入口漂移。

输出统一为 JPEG q80、长边 ≤1024。选 JPEG 而非 PNG：无损格式在
「1700×2160 → 1024」这个尺度上仍需 ~1 MB，而有损只剩 50 KB 量级，对
「让模型还能认出图里画的是哪个房间」足够（实测像素差 0.31%）。

**缓存**：以 `(base64 长度, 前 64 字符哈希, max_edge)` 为键记忆化，`None`
结果同样缓存。3 MB PNG 解码+缩放约 100~300 ms，25 张就是几秒；不缓存会让
「按需缩图」变成每轮卡几秒，比 413 更糟。用前 64 字符哈希而非全量哈希，是为
了避免对整段 base64 做一次 O(n) 计算。

### 6.3 与 `view_image` 的关系

`view_image`（`fs/view_image.rs:113`）产出图片块时**保持现状**——仍按
4 MiB 预算入库、仍写完整 base64。本设计不改入库行为（§4.1），第 1~3 级
全部发生在发送投影阶段。

唯一例外：`DEFAULT_BASE64_BUDGET` 维持不变，但第 2 级的 1024 上限不复用
`HIGH_MAX_DIMENSION = 2048`（`image_prep.rs:23`），由
`ProjectionLimits` 独立指定。

### 6.4 可观测性

每次降级发一条 `AgentEvent::Notice`（Info 级，与既有降级提示同级），例如
`已省略 18 张历史图片、缩放 4 张（请求体 69.3 MB → 1.2 MB）`。
项目既定原则：**静默降级不可接受**。同时 `tracing::info!` 记录
`body_bytes_before` / `body_bytes_after` / `steps`，供后续诊断网关真实上限。

## 7. 数据流

```
think_phase
  ├─ messages（session 权威副本，不动）
  ├─ strip_thinking_except_last_assistant（既有，agent.rs:1153）
  └─ project_request_images  ← 新增
       ├─ 序列化测字节
       ├─ 级 1：去重（无损）
       ├─ 级 2：超「最近 3 张」→ ImageDownscaler
       ├─ 级 3：更旧 → 占位
       └─ 级 4/5 交回 think_phase 决定压缩或报错
  └─ ProviderRequest { messages: projected }
```

`.jsonl` 落盘路径（`thread_store.rs:553` `TurnLine::Turn { messages }`）
**完全不受影响**，仍写入完整历史。

## 8. 测试

TDD，先写失败测试。全部为纯逻辑测试，不需要真实网关。

### 8.1 `image_projection` 单测（12 个，全绿）

1. `under_soft_limit_returns_as_is` —— 小历史零改动（**验证默认不缩**）
2. `dedup_keeps_newest_copy_per_path` —— 同 path 只留最新 1 份
3. `dedup_is_lossless_when_soft_threshold_is_the_only_trigger` —— 去重达标即停，不缩
4. `keeps_newest_three_images_at_full_resolution` —— 第一轮保住最近 3 张
5. `images_without_path_are_counted_but_never_placeheld` —— 无 path 永不被替换
6. `projection_never_mutates_the_source_messages` —— 入参不被修改
7. `plan_matches_measured_failure_shape` —— 复刻真实失败轮（6 个 path、25 块、
   69.3 MB），断言压到 8 MB 以下
8. `downscales_instead_of_placeholder_when_an_implementation_is_injected`
9. `downscaler_absent_falls_back_to_placeholder`
10. `unsupported_downscaler_falls_back_to_placeholder`
11. `newest_images_keep_full_resolution_while_it_suffices` —— 只需降旧图即可收敛时，
    最新图逐字保留
12. `protection_yields_when_downscaling_the_rest_does_not_converge` ——
    **不收敛时保护让位**（这条守的是阶梯必然收敛）

### 8.2 集成测试（core，mock provider）

13. `request_body_stays_under_hard_limit_with_many_images` —— 驱动 `run`，
    捕获 `ProviderRequest`，断言序列化后 ≤ 硬阈值
14. `session_history_is_not_mutated_by_projection` —— 投影后
    `session.messages()` 的**既有前缀**逐字不变（运行会正常追加本轮消息），
    且三张图的 payload 长度仍是原始值（**归档保真**）
15. `oversized_body_is_refused_locally_instead_of_hitting_the_gateway` ——
    降级到底仍超限时本地报 `AgentError::RequestTooLarge`，且
    `recorded_requests()` 为空（**没有发出去**）

### 8.3 tools 侧（4 个，全绿）

16. `downscales_a_large_image_within_the_edge_cap` —— 3000×3000 → 长边 ≤1024，
    且输出更小
17. `repeated_calls_hit_the_cache` —— 记忆化生效，缓存只 1 条
18. `a_small_image_is_not_worth_downscaling` —— 小图重编码会变大，返回 `None`
19. `invalid_base64_is_reported_as_unsupported`

## 9. 未决与风险

1. **网关真实字节上限未知。** 只知 69 MB 被拒、2 MB 以下从未被拒。因此硬阈值
   取保守的 8 MB，且第 4 级「本地报错」是唯一确定的兜底。实施后应从 trace 的
   `bytes_before` 分布反推真实上限，再回调阈值。
2. **保留最近 3 张是经验值，且是可让位的优先级。** 未做实验验证「3」是否够。
   在不收敛的形状下最新几张也会被缩到 1024——这是刻意取舍：0.31% 像素差
   vs 413 让整轮对话卡死。
3. **缩放器未注入时的行为。** `AgentConfig::image_downscaler` 默认 `None`，
   此时第 2 级不可用，只能靠占位（会丢图）。生产路径已在
   `build_agent_config` 注入，但直接构造 `AgentConfig::default()` 的代码
   （测试、side conversation）没有缩放能力——这符合预期，测试不需要真缩图。

## 10. 旧会话的处理决策

出问题的 207 MB 会话（`thread-42208b79-…`）**不做迁移、不改动**
（2026-11-07 确认）。

理由：第 1~3 级都在**发送时**投影，只压请求副本，磁盘上的历史仍是 207 MB，
resume 时仍要全量读取。写一次迁移虽能压小文件，但收益不抵成本，且改动历史
有损坏数据的风险。该会话已完成其定位用途，建议直接弃用。

## 11. 不做的事

- 不改 `.jsonl` 写入格式与内容（归档保真）
- 不改 `view_image` 的入库预算（§4.1）
- 不把 `IMAGE_TOKEN_ESTIMATE` 改成按字节（那是启发式修正，字节闸是硬约束；
  如后续发现压缩触发过晚再单独评估）
- 不迁移历史会话（见 §10）

## 12. 验收

```bash
cd yi-agent-rs
cargo test -p yi-agent-core --lib image_projection      # 12 个单测
cargo test -p yi-agent-core --lib                       # 388 passed
cargo test -p yi-agent-tools --lib image_downscale       # 4 个
cargo fmt --all -- --check
just ci
```

已实测通过（2026-11-07）：core 388 passed / 0 failed，tools 4 passed / 0 failed，
`cargo clippy -p yi-agent-core -p yi-agent-tools -p yi-agent-runtime --all-targets`
零警告。

验收判据（人工）：

1. 在「根据户型图在 Blender 复刻 A 反户型平面」这类会话里继续多轮 view_image，
   不再出现 413；状态栏出现 `请求体 X → Y（省略 N 张、缩放 M 张历史图片）`。
2. 桌面端聊天里的历史图片仍正常显示（按 `path` 读回原图，不受降级影响）。
3. 落盘 `.jsonl` 里的图片 base64 仍是原始大小（`wc -c` 对比降级前后不变）。
