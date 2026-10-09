# LLM-Generated Thread Title Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 首轮对话结束后调用一次当前模型生成 thread 小标题，失败/超时/空结果时静默回退为现有的截断标题。

**Architecture:** 在 `yi-agent-core` 新增 `title.rs`（纯逻辑 + 一次 provider 调用），在 `ThreadStore` 上把 `touch` 改为返回"是否首次写入标题"并新增 CAS 写入 `set_title_if_unchanged`，最后在 `server.rs` 的 `persist_and_finish_turn` 里串联：`touch` → 若首轮则 5s 超时生成 → CAS 覆盖。

**Tech Stack:** Rust 2024 (workspace, rust-version 1.85)、tokio、async-trait、futures。测试用 `cargo test`；workspace 内 crate：`yi-agent-core`、`yi-agent-app-server`。

## Global Constraints

- Rust edition 2024，`rust-version = 1.85`，workspace resolver 2。所有 crate 用 workspace 依赖：`tokio.workspace = true`、`futures.workspace = true`、`async-trait`（core 已用）。
- 全部代码注释与提交信息用中文（与仓库现有风格一致），注释解释"为什么"。
- 提交前必须通过：`just fmt-check`（= `cargo fmt --all -- --check`）、`just lint`（= `cargo clippy --all-targets --all-features -- -D warnings`）、`just test`（= `cargo test --all-features --workspace`）。在 `yi-agent-rs/` 目录下执行。
- 标题长度上限 **30 字符**（与现有 `title_from` 一致）；材料每侧截断 **2000 字符**；`max_tokens = 64`；超时 **5 秒**。
- 不改协议（`protocol.rs`）、不改桌面端、不改 `thread/rename`、不新增配置项。复用 `config.model`。
- 生成失败一律**不打断 turn、不报错**，只记日志；标题回退为 `title_from(prompt)` 的截断值。
- 手动改名优先：写入 LLM 标题必须经 CAS，仅当当前标题仍等于本次写入的兜底值时才替换。

---

### Task 1: `yi-agent-core::title` —— 纯函数（材料拼接 + 标题清洗）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-core/src/title.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/lib.rs`（追加模块与导出）

**Interfaces:**
- Consumes: 无（本任务只落到纯函数）。
- Produces:
  - `pub fn build_title_material(user_text: &str, assistant_text: Option<&str>) -> String`
  - `pub fn sanitize_title(raw: &str) -> Option<String>`
  - `pub const TITLE_MAX_CHARS: usize = 30;`（模块级，供测试引用）
  - `pub const TITLE_MATERIAL_SIDE_CHARS: usize = 2000;`

- [ ] **Step 1: Write the failing tests**

在新建的 `yi-agent-rs/crates/yi-agent-core/src/title.rs` 末尾写入：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn material_joins_both_sides_with_labels() {
        let m = build_title_material("帮我修一下登录页的报错", Some("好的，我看下 AuthForm。"));
        assert!(m.starts_with("用户提问：帮我修一下登录页的报错"));
        assert!(m.contains("助手回复：好的，我看下 AuthForm。"));
    }

    #[test]
    fn material_omits_assistant_when_absent() {
        let m = build_title_material("你好", None);
        assert!(m.starts_with("用户提问：你好"));
        assert!(!m.contains("助手回复"), "absent assistant must be omitted");
    }

    #[test]
    fn material_truncates_each_side() {
        let long = "字".repeat(TITLE_MATERIAL_SIDE_CHARS + 500);
        let m = build_title_material(&long, Some(&long));
        // 每侧最多 2000 字符：总字符数上界 = 两侧材料 + 两个标签 + 一个空行。
        assert!(m.chars().count() <= TITLE_MATERIAL_SIDE_CHARS * 2 + "用户提问：助手回复：\n\n".chars().count());
    }

    #[test]
    fn sanitize_trims_and_takes_first_nonempty_line() {
        assert_eq!(sanitize_title("  修复登录报错  \n解释一下").as_deref(), Some("修复登录报错"));
    }

    #[test]
    fn sanitize_strips_wrapping_quotes_and_prefix() {
        assert_eq!(sanitize_title("\"修复登录报错\"").as_deref(), Some("修复登录报错"));
        assert_eq!(sanitize_title("「修复登录报错」").as_deref(), Some("修复登录报错"));
        assert_eq!(sanitize_title("标题：修复登录报错").as_deref(), Some("修复登录报错"));
        assert_eq!(sanitize_title("Title: fix login").as_deref(), Some("fix login"));
    }

    #[test]
    fn sanitize_caps_length() {
        let long = "字".repeat(100);
        let out = sanitize_title(&long).unwrap();
        assert_eq!(out.chars().count(), TITLE_MAX_CHARS);
    }

    #[test]
    fn sanitize_blank_returns_none() {
        assert_eq!(sanitize_title("   \n  "), None);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p yi-agent-core title::tests 2>&1 | tail -20`
Expected: 编译失败，提示 `cannot find function build_title_material` / `sanitize_title`（模块尚无实现）。

- [ ] **Step 3: Write minimal implementation**

在 `title.rs` 顶部（`#[cfg(test)]` 之前）写入：

```rust
//! 首轮结束后生成 thread 标题：材料拼接与标题清洗。
//!
//! 与 `compact.rs` 同风格：纯逻辑（本模块）与"调用 provider 生成标题"分开放，
//! 便于单测覆盖清洗规则；生成失败由调用方静默回退到截断标题。

/// 标题字符数上限，与 `thread_store::title_from` 的截断上限保持一致。
pub const TITLE_MAX_CHARS: usize = 30;

/// 材料每一侧（提问 / 回复）截断后的最大字符数。
pub const TITLE_MATERIAL_SIDE_CHARS: usize = 2000;

/// 把首轮的两侧文本拼成标题模型的输入材料。
///
/// `assistant_text` 为 `None`（首轮无文本回复，例如纯工具调用）时省略该段。
pub fn build_title_material(user_text: &str, assistant_text: Option<&str>) -> String {
    let truncate = |s: &str| -> String { s.chars().take(TITLE_MATERIAL_SIDE_CHARS).collect() };
    let mut out = format!("用户提问：{}", truncate(user_text));
    if let Some(a) = assistant_text {
        out.push_str("\n\n助手回复：");
        out.push_str(&truncate(a));
    }
    out
}

/// 把模型返回的原始文本清洗成一个可用标题。
///
/// 步骤：去首尾空白 → 取首个非空行 → 去包裹引号 → 去 `标题:` / `Title:` 前缀
/// → 截断至 [`TITLE_MAX_CHARS`]。结果为空返回 `None`（调用方据此回退兜底标题）。
pub fn sanitize_title(raw: &str) -> Option<String> {
    let first_line = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    let mut text = first_line.trim_matches(|c| matches!(c, '"' | '\'' | '「' | '」' | '“' | '”'));
    for prefix in ["标题:", "标题：", "Title:", "title:"] {
        if let Some(rest) = text.strip_prefix(prefix) {
            text = rest.trim();
            break;
        }
    }
    let capped: String = text.chars().take(TITLE_MAX_CHARS).collect();
    let capped = capped.trim().to_string();
    if capped.is_empty() {
        None
    } else {
        Some(capped)
    }
}
```

在 `yi-agent-rs/crates/yi-agent-core/src/lib.rs` 里，`pub mod subagent;` 之后加 `pub mod title;`，并在 `pub use compact::{...};` 之后加：

```rust
pub use title::{TITLE_MATERIAL_SIDE_CHARS, TITLE_MAX_CHARS, build_title_material, sanitize_title};
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p yi-agent-core title::tests 2>&1 | tail -20`
Expected: 7 个测试全部 PASS。

- [ ] **Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/title.rs yi-agent-rs/crates/yi-agent-core/src/lib.rs
git commit -m "feat(core): title material builder and sanitizer"
```

---

### Task 2: `yi-agent-core::title::generate_title` —— 调用 provider

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-core/src/title.rs`
- Modify: `yi-agent-rs/crates/yi-agent-core/src/lib.rs`（追加导出）

**Interfaces:**
- Consumes: Task 1 的 `build_title_material`、`sanitize_title`。
- Produces:
  - `pub const TITLE_INSTRUCTIONS: &str`
  - `pub async fn generate_title(provider: &std::sync::Arc<dyn Provider>, config: &AgentConfig, user_text: &str, assistant_text: Option<&str>) -> Result<Option<String>, AgentError>`

- [ ] **Step 1: Write the failing tests**

把 Task 1 的 `mod tests` 中的 `use super::*;` 替换为：

```rust
    use super::*;
    use crate::{
        agent::{AgentConfig, AgentError},
        message::{ContentBlock, Message},
        provider::{Provider, ProviderError, ProviderEvent, ProviderRequest, ProviderResponse, StopReason},
    };
    use async_trait::async_trait;
    use futures::stream::{BoxStream, StreamExt};

    /// 固定返回一段文本的 provider。
    struct FixedProvider(String);

    #[async_trait]
    impl Provider for FixedProvider {
        async fn call_stream(
            &self,
            _req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            let events = vec![
                ProviderEvent::TextDelta(self.0.clone()),
                ProviderEvent::Stop { reason: StopReason::EndTurn },
            ];
            Ok(futures::stream::iter(events).boxed())
        }
    }

    /// 每次调用都失败的 provider。
    struct FailingProvider;

    #[async_trait]
    impl Provider for FailingProvider {
        async fn call_stream(
            &self,
            _req: ProviderRequest,
        ) -> Result<BoxStream<'static, ProviderEvent>, ProviderError> {
            Err(ProviderError::Network("boom".into()))
        }
    }

    fn core_config() -> AgentConfig {
        AgentConfig::default()
    }

    #[tokio::test]
    async fn generate_title_returns_sanitized_title() {
        let provider: std::sync::Arc<dyn Provider> =
            std::sync::Arc::new(FixedProvider("\"修复登录报错\"".into()));
        let out = generate_title(&provider, &core_config(), "登录页报错", Some("好"))
            .await
            .unwrap();
        assert_eq!(out.as_deref(), Some("修复登录报错"));
    }

    #[tokio::test]
    async fn generate_title_blank_response_is_none() {
        let provider: std::sync::Arc<dyn Provider> =
            std::sync::Arc::new(FixedProvider("   ".into()));
        let out = generate_title(&provider, &core_config(), "hi", None).await.unwrap();
        assert_eq!(out, None);
    }

    #[tokio::test]
    async fn generate_title_propagates_provider_error() {
        let provider: std::sync::Arc<dyn Provider> = std::sync::Arc::new(FailingProvider);
        let err = generate_title(&provider, &core_config(), "hi", None).await;
        assert!(matches!(err, Err(AgentError::Provider(_))));
    }
```

注：`ProviderResponse`、`Message`、`ContentBlock` 在此模块中可能未被直接引用；若 clippy 因未使用导入告警，删掉未用的那几项即可（保留 `ProviderEvent`、`ProviderRequest`、`ProviderError`、`StopReason`、`Provider`、`async_trait`、`BoxStream`、`StreamExt`）。

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p yi-agent-core title::tests 2>&1 | tail -20`
Expected: 编译失败，提示 `cannot find function generate_title`。

- [ ] **Step 3: Write minimal implementation**

在 `title.rs` 的 `sanitize_title` 之上插入（并让文件顶部有对应 `use`）：

```rust
use std::sync::Arc;

use crate::{
    agent::{AgentConfig, AgentError},
    message::Message,
    provider::{Provider, ProviderRequest},
};

/// 标题生成系统提示：约束模型只吐一个短标题。
pub const TITLE_INSTRUCTIONS: &str = "\
你会得到一个会话的开头（用户提问，可能附带助手回复）。请为它生成一个简短的标题。
要求：
- 只输出标题本身，不要换行、不要引号、不要“标题：”之类前缀、不要任何解释。
- 语言跟随用户提问：中文提问用中文标题，英文提问用英文标题。
- 不超过 20 个汉字（或约 40 个英文字符）。";

/// 用当前模型生成一个 thread 标题。
///
/// `assistant_text` 为 `None` 时只用用户提问。返回 `Ok(None)` 表示模型产出
/// 无法作为标题（空/全空白）；provider 报错时返回 `Err`，由调用方决定回退。
pub async fn generate_title(
    provider: &Arc<dyn Provider>,
    config: &AgentConfig,
    user_text: &str,
    assistant_text: Option<&str>,
) -> Result<Option<String>, AgentError> {
    let mut params = config.gen_params.clone();
    // 标题极短：限死输出上限，避免个别模型长篇大论。
    params.max_tokens = Some(64);
    let response = provider
        .call(ProviderRequest {
            model: config.model.clone(),
            system: Some(TITLE_INSTRUCTIONS.to_string()),
            messages: vec![Message::user(build_title_material(user_text, assistant_text))],
            tools: vec![],
            params,
        })
        .await
        .map_err(AgentError::Provider)?;
    let text = response
        .content
        .iter()
        .filter_map(|block| match block {
            crate::message::ContentBlock::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    Ok(sanitize_title(&text))
}
```

在 `lib.rs` 的 title 导出里追加 `generate_title` 与 `TITLE_INSTRUCTIONS`：

```rust
pub use title::{
    TITLE_INSTRUCTIONS, TITLE_MATERIAL_SIDE_CHARS, TITLE_MAX_CHARS, build_title_material,
    generate_title, sanitize_title,
};
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p yi-agent-core title::tests 2>&1 | tail -20`
Expected: 10 个测试全部 PASS。

- [ ] **Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-core/src/title.rs yi-agent-rs/crates/yi-agent-core/src/lib.rs
git commit -m "feat(core): generate_title via provider with sanitization"
```

---

### Task 3: `ThreadStore::touch` 返回是否首轮写入

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs:410-423`（`touch`）
- Test: 同文件内 `mod tests`（`touch_sets_title_only_when_absent_and_bumps_updated_at` 附近）

**Interfaces:**
- Produces: `pub fn touch(&self, id: &str, title_hint: Option<&str>) -> io::Result<bool>`（`true` = 本次写入了标题）。
- Consumes: 无。

- [ ] **Step 1: Update the failing test**

把现有测试 `touch_sets_title_only_when_absent_and_bumps_updated_at` 中两次 `s.touch(...)` 调用改为断言返回值：

```rust
    #[test]
    fn touch_sets_title_only_when_absent_and_bumps_updated_at() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        let wrote = s.touch("thread-a", Some("  first   message  ")).unwrap();
        assert!(wrote, "first touch must report that it wrote a title");
        let m = s.load("thread-a").unwrap().unwrap().meta;
        assert_eq!(m.title.as_deref(), Some("first message"));
        assert!(m.updated_at > 1, "updated_at must be bumped");

        let wrote_again = s.touch("thread-a", Some("ignored")).unwrap();
        assert!(!wrote_again, "an existing title must not be overwritten");
        assert_eq!(
            s.load("thread-a").unwrap().unwrap().meta.title.as_deref(),
            Some("first message"),
            "an existing title must not be overwritten"
        );
    }
```

并把 `touch_unknown_id_is_silent_noop` 改为断言返回 `false`：

```rust
    #[test]
    fn touch_unknown_id_is_silent_noop() {
        let (_d, s) = store();
        assert!(!s.touch("nope", Some("x")).expect("unknown id must not error"));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p yi-agent-app-server thread_store::tests::touch 2>&1 | tail -20`
Expected: 编译失败（`touch` 返回 `io::Result<()>`，`assert!(wrote, ...)` 类型不符）。

- [ ] **Step 3: Write minimal implementation**

把 `thread_store.rs` 的 `touch` 改为：

```rust
    /// 每 turn 完成时调用:更新 `updated_at`,并在 `title` 仍为 `None` 时用
    /// `title_hint`(本轮 prompt)填充。
    ///
    /// 返回 `true` = 本次确实写入了标题(即该 thread 的首轮);调用方据此决定是否
    /// 再发起一次 LLM 标题生成。未知 id 或 meta 不可读时静默返回 `Ok(false)`。
    pub fn touch(&self, id: &str, title_hint: Option<&str>) -> io::Result<bool> {
        let mut wrote = false;
        self.update_meta(id, |meta| {
            meta.updated_at = now_millis();
            if meta.title.is_none() {
                if let Some(hint) = title_hint {
                    let t = title_from(hint);
                    if !t.is_empty() {
                        meta.title = Some(t);
                        wrote = true;
                    }
                }
            }
        })?;
        Ok(wrote)
    }
```

（`update_meta` 的闭包为 `FnOnce` 且可能不执行写入；`wrote` 用 `&mut` 捕获，`update_meta` 调用后仍可读。若 `update_meta` 签名要求 `Fn` 而非 `FnOnce`，改用 `std::cell::Cell<bool>` 捕获。先按上面的 `&mut wrote` 写法尝试编译。）

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p yi-agent-app-server thread_store::tests 2>&1 | tail -20`
Expected: 全 PASS。

- [ ] **Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs
git commit -m "feat(app-server): touch reports whether it wrote the title"
```

---

### Task 4: `ThreadStore::set_title_if_unchanged` —— CAS 写入

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs`（新增方法，紧随 `rename` 之后）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs`（`fn title_from` 提升为 `pub(crate) fn title_from`）
- Test: 同文件内 `mod tests`

**Interfaces:**
- Consumes: `update_meta`、`rename` 旁的 `set_permission_mode` 模式。
- Produces:
  - `pub fn set_title_if_unchanged(&self, id: &str, expected: &str, new: &str) -> io::Result<bool>`
  - `pub(crate) fn title_from(hint: &str) -> String`

- [ ] **Step 1: Write the failing tests**

在 `thread_store.rs` 的 `mod tests` 里（`rename_unknown_returns_false` 附近）新增：

```rust
    #[test]
    fn set_title_if_unchanged_replaces_on_match() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        s.touch("thread-a", Some("placeholder")).unwrap();
        let replaced = s
            .set_title_if_unchanged("thread-a", "placeholder", "智能标题")
            .unwrap();
        assert!(replaced);
        assert_eq!(
            s.load("thread-a").unwrap().unwrap().meta.title.as_deref(),
            Some("智能标题")
        );
    }

    #[test]
    fn set_title_if_unchanged_yields_to_concurrent_rename() {
        let (_d, s) = store();
        s.create(&meta("thread-a")).unwrap();
        s.touch("thread-a", Some("placeholder")).unwrap();
        // 用户抢先改名：当前标题不再是 expected，CAS 必须放弃覆盖。
        s.rename("thread-a", "user title").unwrap();
        let replaced = s
            .set_title_if_unchanged("thread-a", "placeholder", "智能标题")
            .unwrap();
        assert!(!replaced);
        assert_eq!(
            s.load("thread-a").unwrap().unwrap().meta.title.as_deref(),
            Some("user title")
        );
    }

    #[test]
    fn set_title_if_unchanged_unknown_id_returns_false() {
        let (_d, s) = store();
        assert!(!s.set_title_if_unchanged("nope", "x", "y").unwrap());
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p yi-agent-app-server thread_store::tests::set_title 2>&1 | tail -20`
Expected: 编译失败，提示 `no method named set_title_if_unchanged`。

- [ ] **Step 3: Write minimal implementation**

在 `rename` 方法之后插入：

```rust
    /// 条件写入标题：仅当当前 `title` 恰好等于 `expected` 时替换为 `new`。
    ///
    /// 用于"LLM 生成标题"覆盖"兜底截断标题"：以兜底值为 CAS 凭据，用户若在两次
    /// 写入之间手动 `rename`，此处便会放弃覆盖——手动改名优先。返回 `Ok(false)`
    /// 表示未替换（标题已变、或 thread 不存在）。
    pub fn set_title_if_unchanged(&self, id: &str, expected: &str, new: &str) -> io::Result<bool> {
        let mut replaced = false;
        self.update_meta(id, |meta| {
            if meta.title.as_deref() == Some(expected) {
                meta.title = Some(new.to_string());
                meta.updated_at = now_millis();
                replaced = true;
            }
        })?;
        Ok(replaced)
    }
```

并把 `fn title_from(hint: &str) -> String` 改为 `pub(crate) fn title_from(hint: &str) -> String`。

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p yi-agent-app-server thread_store::tests 2>&1 | tail -20`
Expected: 全 PASS。

- [ ] **Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs
git commit -m "feat(app-server): CAS title write that yields to manual rename"
```

---

### Task 5: 在 `persist_and_finish_turn` 里串联生成

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`persist_and_finish_turn` 签名 + 体内；两处调用点）

**Interfaces:**
- Consumes: Task 1/2 的 `yi_agent_core::title::{generate_title, TITLE_INSTRUCTIONS}` 与 `build_title_material`；Task 3 的 `touch -> io::Result<bool>`；Task 4 的 `set_title_if_unchanged`、`pub(crate) title_from`。
- Produces: 无新公共接口（内部行为变化）。

- [ ] **Step 1: Write the failing test**

在 `server.rs` 的 `mod tests` 里新增 mock provider 与集成测试。**判据要点**：主 agent 的对话调用也带 system prompt（`config.system_prompt`），所以区分"标题请求"只能靠精确比较 `system == Some(TITLE_INSTRUCTIONS)`，绝不能用 `system.is_some()`。provider 放在其它 mock 旁边：

```rust
    /// 返回固定标题文本的 provider:用于断言首轮标题来自 LLM。
    struct TitleProvider;

    #[async_trait]
    impl yi_agent_core::Provider for TitleProvider {
        async fn call_stream(
            &self,
            req: yi_agent_core::provider::ProviderRequest,
        ) -> Result<
            futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
            yi_agent_core::provider::ProviderError,
        > {
            // 标题请求（system 恰为 TITLE_INSTRUCTIONS）返回固定标题；
            // 其余（主 agent 对话）返回正文 "ok"。
            let text = if req.system.as_deref() == Some(yi_agent_core::title::TITLE_INSTRUCTIONS)
            {
                "修复登录报错"
            } else {
                "ok"
            };
            let events = vec![
                yi_agent_core::provider::ProviderEvent::TextDelta(text.into()),
                yi_agent_core::provider::ProviderEvent::Stop {
                    reason: yi_agent_core::provider::StopReason::EndTurn,
                },
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn first_turn_title_comes_from_llm() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(TitleProvider);
        let mut h = Harness::with_config(
            cfg,
            move |session, cwd, mode| {
                build_test_agent_with_provider(provider.clone(), session, cwd, mode)
            },
            PERMISSION_TIMEOUT,
        );
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
            .await;
        let tid = read_thread_start_response(&mut h, 2).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","text":"登录页报错了"}}}}"#
        ))
        .await;
        // 读到 turn/completed 后，meta 标题应已是 LLM 给出的标题。
        // 复用仓库既有的等待助手：轮询直到该 thread 的 meta.title 出现。
        let mut title = None;
        for _ in 0..40 {
            let store = crate::thread_store::ThreadStore::new(dir.path());
            if let Ok(Some(loaded)) = store.load(&tid) {
                if let Some(t) = loaded.meta.title {
                    title = Some(t);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(title.as_deref(), Some("修复登录报错"));
        h.shutdown().await;
    }
```

再新增一个"失败回退"测试，用 `RejectingTitleProvider`（正文正常、但标题请求返回错误）:

```rust
    #[tokio::test(flavor = "multi_thread")]
    async fn first_turn_title_falls_back_when_generation_fails() {
        // 用 RejectingTitleProvider：正文正常、但标题请求（带 system）返回错误。
        let dir = tempfile::TempDir::new().unwrap();
        let mut cfg = test_config();
        cfg.workdir = dir.path().to_path_buf();
        let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(RejectingTitleProvider);
        let mut h = Harness::with_config(
            cfg,
            move |session, cwd, mode| {
                build_test_agent_with_provider(provider.clone(), session, cwd, mode)
            },
            PERMISSION_TIMEOUT,
        );
        initialize(&mut h).await;
        h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
            .await;
        let tid = read_thread_start_response(&mut h, 2).await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","text":"登录页报错了"}}}}"#
        ))
        .await;
        let mut title = None;
        for _ in 0..40 {
            let store = crate::thread_store::ThreadStore::new(dir.path());
            if let Ok(Some(loaded)) = store.load(&tid) {
                if let Some(t) = loaded.meta.title {
                    title = Some(t);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        // 回退为 title_from("登录页报错了") —— 即原样截断。
        assert_eq!(title.as_deref(), Some("登录页报错了"));
        h.shutdown().await;
    }
```

并加对应 provider 与测试夹具（放在 mock 群旁）：

```rust
    /// 正文正常返回；但标题请求（带 system）返回错误，用于覆盖回退路径。
    struct RejectingTitleProvider;

    #[async_trait]
    impl yi_agent_core::Provider for RejectingTitleProvider {
        async fn call_stream(
            &self,
            req: yi_agent_core::provider::ProviderRequest,
        ) -> Result<
            futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
            yi_agent_core::provider::ProviderError,
        > {
            if req.system.as_deref() == Some(yi_agent_core::title::TITLE_INSTRUCTIONS) {
                return Err(yi_agent_core::provider::ProviderError::Network("title boom".into()));
            }
            let events = vec![
                yi_agent_core::provider::ProviderEvent::TextDelta("ok".into()),
                yi_agent_core::provider::ProviderEvent::Stop {
                    reason: yi_agent_core::provider::StopReason::EndTurn,
                },
            ];
            Ok(Box::pin(futures::stream::iter(events)))
        }
    }

    /// 用指定 provider 构造 agent，其余与 `build_test_agent` 一致。
    fn build_test_agent_with_provider(
        provider: Arc<dyn yi_agent_core::Provider>,
        session: Option<yi_agent_core::Session>,
        _cwd: &std::path::Path,
        _mode: crate::thread_store::ThreadMode,
    ) -> anyhow::Result<BuiltAgent> {
        let config = yi_agent_core::AgentConfig {
            model: tests_support::TEST_MODEL.to_string(),
            ..yi_agent_core::AgentConfig::default()
        };
        let mut agent = yi_agent_core::Agent::new(
            provider.clone(),
            Arc::new(yi_agent_core::ToolRegistry::new()),
            config.clone(),
        );
        apply_session(&mut agent, session);
        Ok(BuiltAgent {
            agent,
            provider,
            config,
            decision_tx: None,
            decision_rx: None,
            catalog: None,
            yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
            process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        })
    }
```

注：`BuiltAgent` 的字段以 `build_test_agent`（server.rs:8843-8875）为准原样照抄；若该结构还有计划中未列出的字段，一并补齐。`TitleProvider`/`RejectingTitleProvider` 的签名按现有 mock（如 `SlowProvider`）照抄。

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p yi-agent-app-server first_turn_title 2>&1 | tail -30`
Expected: FAIL —— 标题仍是截断值（`first_turn_title_comes_from_llm` 得到 `登录页报错了` 而非 `修复登录报错`），且 `TitleProvider`/`RejectingTitleProvider`/`build_test_agent_with_provider` 可能需要先补齐才能编译。

- [ ] **Step 3: Write minimal implementation**

3a. 在 `server.rs` 顶部附近加超时常量（与其它常量同处）：

```rust
/// 首轮 LLM 标题生成的超时：超时即回退兜底标题，绝不卡住 turn 收尾。
const TITLE_TIMEOUT: Duration = Duration::from_secs(5);
```

3b. `persist_and_finish_turn` 增加两个参数（放在 `status` 之前，紧随 `hub`/`turn_tx` 之后便于阅读）：

```rust
    provider: &Arc<dyn yi_agent_core::Provider>,
    config: &yi_agent_core::AgentConfig,
```

3c. 把 `persist_and_finish_turn` 体内 `touch` 那段改为：

```rust
    if let Err(e) = store.append_turn(thread_id, &record) {
        eprintln!("[app-server] failed to persist turn {thread_id}: {e}");
    } else if let Some(prompt) = user_prompt {
        match store.touch(thread_id, Some(prompt)) {
            Ok(true) => {
                // 首轮：拿兜底截断值作 CAS 凭据，尝试用 LLM 标题覆盖。
                let fallback = crate::thread_store::title_from(prompt);
                let assistant = completed_items_agent_text(&items);
                match tokio::time::timeout(
                    TITLE_TIMEOUT,
                    yi_agent_core::title::generate_title(
                        provider,
                        config,
                        prompt,
                        assistant.as_deref(),
                    ),
                )
                .await
                {
                    Ok(Ok(Some(title))) => {
                        if let Err(e) =
                            store.set_title_if_unchanged(thread_id, &fallback, &title)
                        {
                            eprintln!("[app-server] failed to set LLM title {thread_id}: {e}");
                        }
                    }
                    Ok(Ok(None)) => {}
                    Ok(Err(e)) => {
                        tracing::info!(%e, %thread_id, "title generation failed; keeping fallback");
                    }
                    Err(_) => {
                        tracing::info!(%thread_id, "title generation timed out; keeping fallback");
                    }
                }
            }
            Ok(false) => {}
            Err(e) => eprintln!("[app-server] failed to update meta for {thread_id}: {e}"),
        }
    }
```

3d. 新增小工具函数（文件内，`persist_and_finish_turn` 附近）：

```rust
/// 从本轮 items 里取第一条助手文本：作为标题生成材料的"助手回复"侧。
fn completed_items_agent_text(items: &[crate::protocol::Item]) -> Option<String> {
    items.iter().find_map(|item| match item {
        crate::protocol::Item::AgentMessage { text, .. } if !text.trim().is_empty() => {
            Some(text.clone())
        }
        _ => None,
    })
}
```

注意：`items` 已经把 user 消息放在首位（`persist_and_finish_turn` 里 `completed_items` 前插了 user）；`completed_items_agent_text(&items)` 用整个 `items` 也无妨，因为它只找 `AgentMessage`。

3e. 更新两处调用点，补传 `&provider`、`&config`：

- 约 5269（`SessionCommand::Clear`）与 5307（`Compact`）：这两处是 `apply_session_command` 内、参数名就叫 `provider`/`config`，直接加 `provider, config,`。
- 约 5850（常规收尾）：`&provider, &config,`。
- 另需检查 5860/5850 之外是否还有第三处调用（如 6420 附近）。用 `grep -n "persist_and_finish_turn(" server.rs` 找齐全部调用点并逐一补齐。

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p yi-agent-app-server first_turn_title 2>&1 | tail -30`
Expected: 两个测试 PASS。

- [ ] **Step 5: Commit**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs
git commit -m "feat(app-server): generate thread title via one LLM call on first turn"
```

---

### Task 6: 全量校验

**Files:** 无改动（仅验证）。

- [ ] **Step 1: 格式化 + clippy + 全量测试**

Run（在 `yi-agent-rs/` 下）: `just fmt-check && just lint && just test 2>&1 | tail -40`
Expected: 三者全部通过，无 clippy 警告，测试全绿。

- [ ] **Step 2: 确认无回归**

检查 `thread_store` 与 `server` 既有测试（尤其 `touch`、`thread/rename`、`/clear`、`/compact` 相关）仍通过——`just test` 已覆盖。

- [ ] **Step 3: Commit（若 fmt 有自动改动）**

```bash
git add -A && git commit -m "chore: fmt after title feature" || true
```
