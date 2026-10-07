# 文档附件（PDF / DOCX 只读）实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 桌面端用户可通过文件选择器（或直接给路径）把本地文档附加到一条消息，agent 用新工具 `read_document` 按需读取内容并据此回答。

**Architecture:** `turn/start` 的 `input` 新增 `attachment` block（只传路径）；app-server 在起 turn 前把文件复制进 `<cwd>/.yi-agent/attachments/<thread_id>/`，并把附件清单以确定性文本拼进发给 agent 的 prompt，`Item::UserMessage.attachments` 只带元数据供 UI 渲染 chip；core 的 `Message`/`ContentBlock` 零改动。文件按 thread 目录整体隔离，随 `thread/delete` 一并清理。

**Tech Stack:** Rust（`yi-agent-tools` / `yi-agent-app-server`）、`pdf-extract 0.12`、`lopdf 0.45`、`unicode-normalization 0.1`、`zip 8`（仅 deflate）、`quick-xml 0.38`；前端 React 19 + TypeScript + Tailwind v4 + Vitest。

**设计文档：** `docs/superpowers/specs/2026-10-07-document-attachments-design.md`
**前置 spike（含实测陷阱与样本）：** `docs/research/2026-10-07-pdf-text-extraction-spike.md`、`docs/research/fixtures/2026-10-07-document-samples/`

## Global Constraints

- **协议字段命名**：`thread/*`、`turn/*`、`attachment` 一律 snake_case（`thread_id`、`call_id`）；**只有** `agent/*` 命名空间用 camelCase。`Item` 的判别标签是 camelCase `type`（`userMessage`）。
- **附件只传路径，绝不内联内容**：`MAX_FRAME_BYTES = 1 MiB`（`protocol.rs:8`）否掉了 base64 内联。
- **root 安全不变量一字不改**：所有附件读取都在 `ToolsContext.root()` 内，靠"复制进工作区"实现，**不得**给 `resolve_and_check` 开白名单例外。
- **附件目录布局固定**：`<cwd>/.yi-agent/attachments/<thread_id>/<sha256[0:8]>-<安全化文件名>`。`thread_id` 已由 `valid_id` 限定 `[A-Za-z0-9_-]{1,128}`（`thread_store.rs:523`），可安全充当路径分量。
- **按 thread 隔离，不做全局去重**：两个 thread 附同一文件各存一份；`删除 = 删除整个 thread 目录`，**不需要引用计数**。
- **`thread/clear` 不清附件**；只有 `thread/delete` 清。
- **PDF 中文必须经字符修复**：`NFKC + UCD EquivalentUnifiedIdeograph 映射`。裸抽取会把中文丢到部首码位（`⻓` U+2ED3 而非 `长` U+957F），导致检索/引用系统性失效。**不要**只做 NFKC（它不折叠 U+2E80–U+2EFF）。
- **不要把 `textutil` 等宿主命令硬编码进工具**：`textutil` 是 macOS 独占，`yi-agent-tools` 需跨平台。
- **空抽取不得静默返回**：无文本层 PDF 必须返回「疑似扫描件」提示，绝不返回空串。
- **默认值**：附件仅 `turn/start` 支持（`turn/interject` 不带）；一条消息可多附件；单文件上限 50 MiB，env `YI_AGENT_ATTACHMENT_MAX_BYTES` 可调。
- **测试要求**：Rust 用 `cd yi-agent-rs && cargo test -p <crate> <filter>`；前端用 `cd desktop && npx vitest run <file>`。TDD：先写失败测试。**不要**在多个 shell 同时跑 cargo test（见 CLAUDE.md，会锁竞争/OOM）。
- **提交前跑 `cd yi-agent-rs && cargo fmt --all`**；commit message 不写 `Co-Authored-By`。

---

## 文件结构

**后端（`yi-agent-rs/`）**

| 文件 | 职责 | 动作 |
|------|------|------|
| `crates/yi-agent-tools/src/fs/read_document.rs` | `ReadDocumentTool`：pdf/docx/html/纯文本抽取 + 字符修复 + 按页 | 新建 |
| `crates/yi-agent-tools/src/fs/unicode/EquivalentUnifiedIdeograph.txt` | UCD 等价表（vendor，`include_str!`） | 新建 |
| `crates/yi-agent-tools/src/fs/mod.rs` | 导出 `ReadDocumentTool` | 修改 |
| `crates/yi-agent-tools/src/lib.rs` | 注册 `read_document`（无条件，只读） | 修改 |
| `crates/yi-agent-tools/Cargo.toml` | 五个新依赖 | 修改 |
| `crates/yi-agent-tools/src/error.rs` | `DocumentParse` / `DocumentTooLarge` 变体 | 修改 |
| `crates/yi-agent-app-server/src/attachments.rs` | 复制/安全化/清单渲染/清理/解析 block | 新建 |
| `crates/yi-agent-app-server/src/lib.rs` | `mod attachments;` | 修改 |
| `crates/yi-agent-app-server/src/protocol.rs` | `Attachment` + `Item` 加 `attachments` | 修改 |
| `crates/yi-agent-app-server/src/session.rs` | `TurnPrompt.attachments` | 修改 |
| `crates/yi-agent-app-server/src/thread_store.rs` | `pub fn root()` | 修改 |
| `crates/yi-agent-app-server/src/server.rs` | block 解析、复制、清单、item、清理 | 修改 |

**前端（`desktop/src/`）**

| 文件 | 职责 | 动作 |
|------|------|------|
| `lib/protocol.ts` | `Attachment` 类型 + `Item` 加字段 | 修改 |
| `lib/attachmentLimits.ts` | 大小/扩展名常量与本地预检 | 新建 |
| `lib/session.ts` | 本地乐观气泡带 attachments | 修改 |
| `components/AttachmentChips.tsx` | 可删除附件 chip 行 | 新建 |
| `components/MessageInput.tsx` | 回形针 + chip + 只发附件可发送 | 修改 |
| `components/ChatView.tsx` | 用户气泡展示附件名 | 修改 |
| `App.tsx` | 多选选择器 + 附件状态 + 组装 blocks | 修改 |

---

## Task 1: `read_document` 工具（PDF 文本 + 字符修复）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-tools/src/fs/read_document.rs`
- Create: `yi-agent-rs/crates/yi-agent-tools/src/fs/unicode/EquivalentUnifiedIdeograph.txt`
- Modify: `yi-agent-rs/crates/yi-agent-tools/Cargo.toml`
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/error.rs`
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/fs/mod.rs`

**Interfaces:**
- Consumes: `crate::fs::path_util::resolve_and_check`、`crate::context::ToolsContext`、`crate::error::ToolsError`
- Produces: `pub struct ReadDocumentTool`（`ReadDocumentTool::new(ctx: Arc<ToolsContext>) -> Self`，实现 `yi_agent_core::Tool`，`name() == "read_document"`）；`pub(crate) fn repair_cjk(text: &str) -> String`；常量 `MAX_DOCUMENT_CHARS`

- [ ] **Step 1: 加依赖与 vendor UCD 表**

```bash
cd yi-agent-rs/crates/yi-agent-tools
cargo add pdf-extract@0.12 lopdf@0.45 unicode-normalization@0.1
cargo add zip@8 --no-default-features --features deflate
cargo add quick-xml@0.38
```

`zip` 必须收紧特性：默认特性会拉 `zstd-sys`（C 代码），与"不打包渲染引擎/不碰打包链路"的约束相悖。DOCX 自身只需 DEFLATE。

```bash
mkdir -p src/fs/unicode
curl -sS -L -o src/fs/unicode/EquivalentUnifiedIdeograph.txt \
  https://www.unicode.org/Public/UCD/latest/ucd/EquivalentUnifiedIdeograph.txt
wc -l src/fs/unicode/EquivalentUnifiedIdeograph.txt   # 期望 ~410
grep -E "^2ED3|^2EDA|^2EDB" src/fs/unicode/EquivalentUnifiedIdeograph.txt
```

期望输出含 `2ED3       ; 957F`、`2EDA       ; 9875`、`2EDB       ; 98CE`。该文件自带 Unicode 许可头，随仓库 vendor，**不要**运行时下载。

在 `src/error.rs` 的 `ToolsError` 末尾（`ImageDecode` 之后）加两个变体：

```rust
    #[error("document parse failed: {0}")]
    DocumentParse(String),

    #[error("document too large: {size} bytes (max {max})")]
    DocumentTooLarge { size: u64, max: u64 },
```

- [ ] **Step 2: 写失败测试**

在新建的 `read_document.rs` 末尾加：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn tool(tmp: &TempDir) -> ReadDocumentTool {
        ReadDocumentTool::new(Arc::new(ToolsContext::new(tmp.path().to_path_buf())))
    }

    #[test]
    fn repair_folds_radical_codepoints_to_han() {
        // 这三个码位来自 spike 实测：PDF 的 ToUnicode CMap 把字形映到部首区。
        assert_eq!(repair_cjk("增⻓"), "增长");      // U+2ED3 -> U+957F
        assert_eq!(repair_cjk("第⼆⻚"), "第二页");  // U+2F02/U+2EDA -> U+4E8C/U+9875
        assert_eq!(repair_cjk("⻛险"), "风险");      // U+2EDB -> U+98CE
    }

    #[test]
    fn repair_leaves_plain_han_untouched() {
        assert_eq!(repair_cjk("季度经营分析报告"), "季度经营分析报告");
    }

    #[tokio::test]
    async fn reads_a_pdf_and_reports_missing_text_layer() {
        let tmp = TempDir::new().unwrap();
        // 纯图 PDF（无字体、无文本层）——必须在 fixtures 里，避免测试依赖网络。
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../docs/research/fixtures/2026-10-07-document-samples/scanned.pdf");
        let bytes = std::fs::read(&src).unwrap();
        std::fs::write(tmp.path().join("scanned.pdf"), &bytes).unwrap();

        let result = tool(&tmp)
            .call(serde_json::json!({ "path": "scanned.pdf" }))
            .await;
        let text = format!("{result:?}");
        assert!(
            text.contains("扫描") || text.contains("文本层"),
            "empty text layer must be reported, got: {text}"
        );
    }

    #[tokio::test]
    async fn reads_a_chinese_pdf_with_fixed_characters() {
        let tmp = TempDir::new().unwrap();
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../docs/research/fixtures/2026-10-07-document-samples/cn-report.pdf");
        std::fs::copy(&src, tmp.path().join("cn-report.pdf")).unwrap();

        let result = tool(&tmp)
            .call(serde_json::json!({ "path": "cn-report.pdf" }))
            .await;
        let text = format!("{result:?}");
        assert!(text.contains("收入构成"), "missing 收入构成: {text}");
        assert!(text.contains("3280"), "missing table cell: {text}");
    }

    #[tokio::test]
    async fn pages_selects_only_that_page() {
        let tmp = TempDir::new().unwrap();
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../docs/research/fixtures/2026-10-07-document-samples/cn-report.pdf");
        std::fs::copy(&src, tmp.path().join("cn-report.pdf")).unwrap();

        let result = tool(&tmp)
            .call(serde_json::json!({ "path": "cn-report.pdf", "pages": "2" }))
            .await;
        let text = format!("{result:?}");
        assert!(text.contains("风险提示"), "page 2 content missing: {text}");
        assert!(!text.contains("收入构成"), "page 1 content leaked: {text}");
    }

    #[tokio::test]
    async fn rejects_a_path_outside_root() {
        let tmp = TempDir::new().unwrap();
        let result = tool(&tmp)
            .call(serde_json::json!({ "path": "/etc/passwd" }))
            .await;
        let text = format!("{result:?}");
        assert!(text.contains("escapes root"), "got: {text}");
    }
}
```

- [ ] **Step 3: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools read_document`
Expected: 编译失败（`ReadDocumentTool` / `repair_cjk` 未定义）

- [ ] **Step 4: 实现工具**

`read_document.rs` 开头与主结构：

```rust
//! `read_document`: 读取工作区内的文档（PDF / DOCX / HTML / 纯文本）。

use crate::context::ToolsContext;
use crate::error::ToolsError;
use crate::fs::path_util::resolve_and_check;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use unicode_normalization::UnicodeNormalization;
use yi_agent_core::{Tool, ToolMetadata, ToolResult, ToolSource};

/// UCD 等价表：部首码位 -> 统一汉字。spike 证明必需（见模块文档）。
const EQUIV_TABLE: &str = include_str!("unicode/EquivalentUnifiedIdeograph.txt");

/// 返回正文的字符上限，超出则截断并说明。
const DEFAULT_MAX_CHARS: usize = 100_000;
const MAX_CHARS_ENV: &str = "YI_AGENT_READ_DOCUMENT_MAX_CHARS";

pub struct ReadDocumentTool {
    ctx: Arc<ToolsContext>,
    max_chars: usize,
}

impl ReadDocumentTool {
    pub fn new(ctx: Arc<ToolsContext>) -> Self {
        Self {
            ctx,
            max_chars: resolve_max_chars(),
        }
    }
}

fn resolve_max_chars() -> usize {
    match std::env::var(MAX_CHARS_ENV) {
        Ok(raw) => raw.trim().parse::<usize>().ok().filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MAX_CHARS),
        Err(_) => DEFAULT_MAX_CHARS,
    }
}

/// 解析 UCD `EquivalentUnifiedIdeograph.txt` 的 `SRC ; DST  # comment` 行。
fn equivalence_table() -> &'static HashMap<char, char> {
    use std::sync::OnceLock;
    static TABLE: OnceLock<HashMap<char, char>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut m = HashMap::new();
        for line in EQUIV_TABLE.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let main = line.split('#').next().unwrap_or("");
            let mut parts = main.split(';').map(str::trim);
            if let (Some(a), Some(b)) = (parts.next(), parts.next()) {
                if let (Ok(src), Ok(dst)) =
                    (u32::from_str_radix(a, 16), u32::from_str_radix(b, 16))
                {
                    if let (Some(src), Some(dst)) = (char::from_u32(src), char::from_u32(dst)) {
                        m.insert(src, dst);
                    }
                }
            }
        }
        m
    })
}

/// NFKC 之后再映射部首码位。
///
/// **两步都要**：NFKC 只折叠康熙部首区（U+2F00–U+2FDF），不折叠 CJK 部首补充区
/// （U+2E80–U+2EFF，`⻓` U+2ED3 / `⻚` U+2EDA / `⻛` U+2EDB 都在这里）。spike
/// 实测：只用 NFKC 残留 5 个字符，两者合用后归零。
pub(crate) fn repair_cjk(text: &str) -> String {
    let table = equivalence_table();
    text.nfkc().map(|c| *table.get(&c).unwrap_or(&c)).collect()
}

#[derive(Deserialize)]
struct Args {
    path: String,
    /// `"3"` 或 `"1-5"`；仅 PDF 有效。
    #[serde(default)]
    pages: Option<String>,
}

/// 解析 `pages` 参数（1-based，闭区间）。非法值返回 `None`。
fn parse_pages(spec: &str) -> Option<(u32, u32)> {
    let spec = spec.trim();
    if let Some((a, b)) = spec.split_once('-') {
        let a = a.trim().parse::<u32>().ok()?;
        let b = b.trim().parse::<u32>().ok()?;
        if a >= 1 && b >= a {
            return Some((a, b));
        }
        return None;
    }
    let n = spec.parse::<u32>().ok()?;
    if n >= 1 { Some((n, n)) } else { None }
}
```

PDF 抽取与按页（`lopdf` 切单页后交 `pdf-extract`）：

```rust
fn extract_pdf(bytes: &[u8], pages: Option<(u32, u32)>) -> Result<(String, bool), ToolsError> {
    let doc = lopdf::Document::load_mem(bytes)
        .map_err(|e| ToolsError::DocumentParse(format!("pdf load failed: {e}")))?;
    let page_count = doc.get_pages().len() as u32;
    let (from, to) = pages.unwrap_or((1, page_count.max(1)));
    if page_count == 0 {
        return Ok((String::new(), false));
    }

    let mut out = String::new();
    let mut any_text = false;
    for n in from..=to.min(page_count) {
        let page_bytes = single_page_pdf(bytes, &doc, n)?;
        let text = pdf_extract::extract_text_from_mem(&page_bytes)
            .map_err(|e| ToolsError::DocumentParse(format!("pdf extract failed: {e}")))?;
        let text = repair_cjk(&text);
        if !text.trim().is_empty() {
            any_text = true;
        }
        out.push_str(&format!("\n--- page {n} ---\n"));
        out.push_str(text.trim_end());
        out.push('\n');
    }
    Ok((out, any_text))
}

/// 复制全部对象 + 新建单页 Catalog/Pages，得到只含第 `n` 页的 PDF。
fn single_page_pdf(
    _bytes: &[u8],
    doc: &lopdf::Document,
    n: u32,
) -> Result<Vec<u8>, ToolsError> {
    use lopdf::{dictionary, Object};
    let page_id = *doc
        .get_pages()
        .get(&n)
        .ok_or_else(|| ToolsError::DocumentParse(format!("pdf has no page {n}")))?;

    let mut out = lopdf::Document::with_version("1.5");
    for (id, obj) in doc.objects.iter() {
        out.objects.insert(*id, obj.clone());
    }
    let max_id = doc.objects.keys().map(|(num, _)| *num).max().unwrap_or(0);
    let pages_id = (max_id + 1, 0u16);
    let catalog_id = (max_id + 2, 0u16);
    out.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => vec![Object::Reference(page_id)],
            "Count" => 1,
        }),
    );
    out.objects.insert(
        catalog_id,
        Object::Dictionary(dictionary! {
            "Type" => "Catalog",
            "Pages" => Object::Reference(pages_id),
        }),
    );
    out.trailer.set("Root", Object::Reference(catalog_id));
    let mut buf = Vec::new();
    out.save_to(&mut buf)
        .map_err(|e| ToolsError::DocumentParse(format!("pdf save failed: {e}")))?;
    Ok(buf)
}
```

`call` 主流程与 `metadata`：

```rust
#[async_trait]
impl Tool for ReadDocumentTool {
    fn name(&self) -> &str {
        "read_document"
    }

    fn description(&self) -> &str {
        "Read a document in the workspace and return its text: PDF (text layer \
         only), DOCX (headings and tables kept as markdown), HTML, or plain text. \
         `pages` (e.g. \"3\" or \"1-5\") selects PDF pages. Scanned PDFs without a \
         text layer are reported as such instead of returning empty text."
    }

    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Relative or absolute path to a document within root" },
                "pages": { "type": "string", "description": "PDF pages to read, e.g. \"3\" or \"1-5\". Omit for all pages." }
            },
            "required": ["path"]
        })
    }

    async fn call(&self, args: Value) -> ToolResult {
        let args: Args = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => return ToolsError::ArgsParse(e).into(),
        };
        let resolved = match resolve_and_check(self.ctx.root(), &args.path) {
            Ok(p) => p,
            Err(e) => return e.into(),
        };
        let pages = match args.pages.as_deref() {
            Some(spec) => match parse_pages(spec) {
                Some(r) => Some(r),
                None => {
                    return ToolResult::error(format!(
                        "invalid pages spec {spec:?}: use \"3\" or \"1-5\""
                    ))
                }
            },
            None => None,
        };

        match self.extract(&resolved, pages).await {
            Ok(text) => ToolResult::text(text),
            Err(e) => e.into(),
        }
    }

    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            source: ToolSource::Builtin,
            requires_confirmation: false,
            read_only: true,
            version: None,
        }
    }
}

impl ReadDocumentTool {
    async fn extract(
        &self,
        path: &Path,
        pages: Option<(u32, u32)>,
    ) -> Result<String, ToolsError> {
        let size = std::fs::metadata(path).map_err(ToolsError::Io)?.len();
        if size > MAX_DOCUMENT_BYTES {
            return Err(ToolsError::DocumentTooLarge { size, max: MAX_DOCUMENT_BYTES });
        }
        let bytes = std::fs::read(path).map_err(ToolsError::Io)?;
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();

        let (text, any_text) = match ext.as_str() {
            "pdf" => extract_pdf(&bytes, pages)?,
            "docx" => (extract_docx(&bytes)?, true),
            "html" | "htm" => {
                let raw = String::from_utf8_lossy(&bytes);
                (html2md::parse_html(&raw), true)
            }
            _ => {
                let raw = String::from_utf8(bytes).map_err(|_| {
                    ToolsError::DocumentParse(format!(
                        "{} is not a UTF-8 text document; supported: pdf, docx, html, txt, md, csv",
                        path.display()
                    ))
                })?;
                (raw, true)
            }
        };

        if !any_text {
            return Ok(format!(
                "read {}: no text layer found (looks like a scanned image); \
                 PDF rendering/OCR is not supported",
                path.display()
            ));
        }

        let mut body = text;
        let total = body.chars().count();
        if total > self.max_chars {
            body = body.chars().take(self.max_chars).collect();
            body.push_str(&format!(
                "\n[truncated: showed {shown} of {total} chars; narrow with `pages` or read a smaller range]",
                shown = self.max_chars,
            ));
        }
        Ok(format!("read {} ({total} chars)\n{body}", path.display()))
    }
}
```

`extract_docx`（zip + quick-xml，保留标题与表格）：

```rust
/// DOCX -> markdown。标题走 `<w:pStyle w:val="Heading1">`，表格输出 markdown 表。
///
/// **两个实测陷阱**（spike §4）：
/// 1. `<w:pStyle .../>` 是自闭合标签，quick-xml 发 `Event::Empty` 而非
///    `Start`；只接 `Start` 会静默丢掉全部标题层级。
/// 2. quick-xml 0.38 用 `BytesText::xml_content()`，`unescape()` 已移除。
fn extract_docx(bytes: &[u8]) -> Result<String, ToolsError> {
    use quick_xml::events::Event;
    use quick_xml::Reader;
    use std::io::Read;

    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| ToolsError::DocumentParse(format!("docx zip failed: {e}")))?;
    let mut xml = String::new();
    zip.by_name("word/document.xml")
        .map_err(|e| ToolsError::DocumentParse(format!("docx missing document.xml: {e}")))?
        .read_to_string(&mut xml)
        .map_err(|e| ToolsError::DocumentParse(format!("docx xml read failed: {e}")))?;

    fn style_val(e: &quick_xml::events::BytesStart) -> Option<String> {
        e.attributes().flatten().find_map(|a| {
            (a.key.as_ref() == b"w:val").then(|| String::from_utf8_lossy(&a.value).into_owned())
        })
    }

    fn heading_level(style: &str) -> Option<usize> {
        if !style.to_ascii_lowercase().contains("heading") {
            return None;
        }
        let digits: String = style
            .chars()
            .rev()
            .take_while(|c| c.is_ascii_digit())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        digits.parse::<usize>().ok().filter(|n| *n > 0)
    }

    let mut reader = Reader::from_str(&xml);
    reader.config_mut().trim_text(false);
    let mut out = String::new();
    let (mut in_text, mut in_cell) = (false, false);
    let (mut run, mut para) = (String::new(), String::new());
    let mut para_style: Option<String> = None;
    let (mut cells, mut cell) = (Vec::<String>::new(), String::new());
    let mut table_row = 0usize;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => match e.name().as_ref() {
                b"w:p" => {
                    para.clear();
                    para_style = None;
                }
                b"w:pStyle" => para_style = style_val(&e),
                b"w:t" => {
                    in_text = true;
                    run.clear();
                }
                b"w:tbl" => table_row = 0,
                b"w:tr" => cells.clear(),
                b"w:tc" => {
                    in_cell = true;
                    cell.clear();
                }
                _ => {}
            },
            // 自闭合标签（`<w:pStyle .../>`）走这里，不能漏。
            Ok(Event::Empty(e)) if e.name().as_ref() == b"w:pStyle" => {
                para_style = style_val(&e);
            }
            Ok(Event::Text(t)) if in_text => {
                run.push_str(&t.xml_content().unwrap_or_default());
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"w:t" => {
                    in_text = false;
                    para.push_str(&run);
                    if in_cell {
                        cell.push_str(&run);
                    }
                }
                b"w:p" => {
                    if !in_cell {
                        let line = para.trim();
                        if !line.is_empty() {
                            match para_style.as_deref().and_then(heading_level) {
                                Some(level) => {
                                    out.push_str(&"#".repeat(level.min(6)));
                                    out.push(' ');
                                    out.push_str(line);
                                }
                                None => out.push_str(line),
                            }
                            out.push_str("\n\n");
                        }
                    }
                }
                b"w:tc" => {
                    in_cell = false;
                    cells.push(std::mem::take(&mut cell).trim().to_string());
                }
                b"w:tr" => {
                    if !cells.is_empty() {
                        out.push_str("| ");
                        out.push_str(&cells.join(" | "));
                        out.push_str(" |\n");
                        if table_row == 0 {
                            out.push('|');
                            for _ in &cells {
                                out.push_str(" --- |");
                            }
                            out.push('\n');
                        }
                        table_row += 1;
                    }
                    cells.clear();
                }
                b"w:tbl" => out.push('\n'),
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(e) => return Err(ToolsError::DocumentParse(format!("docx xml failed: {e}"))),
            _ => {}
        }
    }
    Ok(out)
}
```

顶部再加一个常量：

```rust
/// 文档文件大小上限（与附件上限无关，防止工具读到超大文件）。
const MAX_DOCUMENT_BYTES: u64 = 50 * 1024 * 1024;
```

`fs/mod.rs` 加模块与导出：

```rust
pub mod read_document;
...
pub use read_document::ReadDocumentTool;
```

- [ ] **Step 5: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools read_document`
Expected: 5 个测试全 PASS

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-tools/
git commit -m "feat(tools): add read_document for PDF/DOCX/HTML/plain text"
```

---

## Task 2: 注册 `read_document` 并加 DOCX 测试

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-tools/src/lib.rs:70-105`（`register_builtin_tools_with_controller`）
- Test: `yi-agent-rs/crates/yi-agent-tools/src/fs/read_document.rs`（同文件内测试模块）

**Interfaces:**
- Consumes: Task 1 的 `ReadDocumentTool`
- Produces: `read_document` 进入每个 builtin 工具集（含只读会话）；`lib.rs` 重新导出 `ReadDocumentTool`

- [ ] **Step 1: 写失败测试**

在 `read_document.rs` 的 `tests` 模块追加（需 `use crate::register_builtin_tools;` 视导入而定，直接写全路径）：

```rust
    #[test]
    fn docx_keeps_headings_and_tables() {
        let tmp = TempDir::new().unwrap();
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../docs/research/fixtures/2026-10-07-document-samples/cn-report.docx");
        std::fs::copy(&src, tmp.path().join("cn-report.docx")).unwrap();

        let tool = ReadDocumentTool::new(Arc::new(ToolsContext::new(tmp.path().to_path_buf())));
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.call(serde_json::json!({ "path": "cn-report.docx" })));
        let text = format!("{result:?}");
        assert!(text.contains("# 季度经营分析报告"), "heading lost: {text}");
        assert!(text.contains("| 华东 | 3280 | 15.2% |"), "table row lost: {text}");
    }

    #[test]
    fn builtin_registration_includes_read_document() {
        let tmp = TempDir::new().unwrap();
        let mut registry = yi_agent_core::ToolRegistry::new();
        crate::register_builtin_tools(&mut registry, tmp.path().to_path_buf());
        assert!(
            registry.get("read_document").is_some(),
            "read_document must be registered for every builtin tool set"
        );
    }
```

若 `ToolRegistry` 没有 `get`，用仓库已有的查询方法替代（`grep -n "impl ToolRegistry" -A 30 yi-agent-core/src/tool.rs` 确认后再写）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools builtin_registration_includes_read_document`
Expected: FAIL（`read_document` 未注册）

- [ ] **Step 3: 注册**

`lib.rs`：导出并注册（与 `ViewImageTool` 并列，**无条件**注册——它是只读工具，只读会话也应可用）：

```rust
pub use fs::{EditTool, GlobTool, GrepTool, ReadDocumentTool, ReadTool, ViewImageTool, WriteTool};
```

在 `register_builtin_tools_with_controller` 中 `ViewImageTool` 之后加：

```rust
    // 文档读取是只读的，与 view_image 一样在只读会话里保留。
    registry.register(Arc::new(ReadDocumentTool::new(ctx.clone())));
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-tools read_document && cargo test -p yi-agent-tools builtin_registration`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-tools/
git commit -m "feat(tools): register read_document in the builtin tool set"
```

---

## Task 3: 协议加 `Attachment`

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs:392-418`（`Item`）
- Test: 同文件 `#[cfg(test)] mod tests`

**Interfaces:**
- Produces: `pub struct Attachment { pub name: String, pub path: String, pub mime: Option<String>, pub size: u64 }`；`Item::UserMessage` / `Item::UserInterjection` 各多一个 `attachments: Vec<Attachment>` 字段（`#[serde(default, skip_serializing_if = "Vec::is_empty")]`）

- [ ] **Step 1: 写失败测试**

在 `protocol.rs` 的 tests 模块加：

```rust
    #[test]
    fn user_message_attachments_round_trip() {
        let item = Item::UserMessage {
            id: "user-1".into(),
            text: "总结这份文件".into(),
            attachments: vec![Attachment {
                name: "报告.pdf".into(),
                path: ".yi-agent/attachments/t1/a1b2c3d4-报告.pdf".into(),
                mime: Some("application/pdf".into()),
                size: 1234,
            }],
        };
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["attachments"][0]["name"], "报告.pdf");
        assert_eq!(json["attachments"][0]["size"], 1234);
        assert_eq!(serde_json::from_value::<Item>(json).unwrap(), item);
    }

    #[test]
    fn empty_attachments_are_omitted_on_the_wire() {
        // 旧前端/旧服务端兼容：没有附件时不得多出一个 null 字段。
        let item = Item::UserMessage {
            id: "user-2".into(),
            text: "hi".into(),
            attachments: Vec::new(),
        };
        let json = serde_json::to_value(&item).unwrap();
        assert!(json.get("attachments").is_none(), "got: {json}");
        // 旧线协议（缺字段）必须仍能反序列化。
        let legacy: Item =
            serde_json::from_str(r#"{"type":"userMessage","id":"u","text":"hi"}"#).unwrap();
        assert_eq!(legacy, item);
    }
```

`Item` 需要 `PartialEq` 才能 `assert_eq!`；若当前没有，在 `#[derive(...)]` 上加 `PartialEq`（`Attachment` 也要）。`Item` 现在派生 `Debug, Clone, Serialize, Deserialize`（`protocol.rs:392`）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server user_message_attachments_round_trip`
Expected: 编译失败（`Attachment` 未定义 / 字段缺失）

- [ ] **Step 3: 加类型与字段**

在 `protocol.rs` 的 `Item` 定义之前加：

```rust
/// 一条消息附带的文档（只带元数据与 in-root 路径，**不带内容**）。
///
/// `path` 相对工作区根，形如 `.yi-agent/attachments/<thread_id>/<hash>-<name>`；
/// 服务端在起 turn 前把用户选中的文件复制到该位置，工具据此读取。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    pub name: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    pub size: u64,
}
```

`Item` 两个变体各加字段（`skip_serializing_if` 保证旧客户端兼容）：

```rust
    UserMessage {
        id: String,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<Attachment>,
    },
```

```rust
    UserInterjection {
        id: String,
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<Attachment>,
    },
```

`Item` 的 derive 加 `PartialEq`。

- [ ] **Step 4: 修所有构造点并确认编译**

Run: `cd yi-agent-rs && cargo build -p yi-agent-app-server 2>&1 | grep -E "^error|-->" | head -40`

逐处补 `attachments: Vec::new()`。已知构造点（`grep -rn "Item::UserMessage" crates/yi-agent-app-server/src/ | grep -v "id, \.\."` 复核）：
`thread_store.rs:700,720,760,775,977,997,1051,1281`；`protocol.rs:527,639`；`server.rs:4718,5911,11020,11035,11120,11136`。

结构化模式匹配处也要加 `..`（例如 `server.rs:11585` 的 `Item::UserMessage { text, .. }` 已经用了 `..`，无需改）。已知用 `..` 的：`server.rs:5355`。

- [ ] **Step 5: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server protocol`
Expected: PASS（含两个新测试）

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/
git commit -m "feat(app-server): add attachments to user message items"
```

---

## Task 4: `attachments` 模块（复制、安全化、清单、清理）

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/attachments.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/lib.rs`

**Interfaces:**
- Consumes: `crate::protocol::Attachment`、`sha2`（已在 `Cargo.toml:34`）
- Produces:
  - `pub const MAX_ATTACHMENT_BYTES: u64`、`pub const MAX_ATTACHMENT_BYTES_ENV: &str`
  - `pub fn resolve_max_bytes() -> u64`
  - `pub fn sanitize_filename(name: &str) -> String`
  - `pub fn stored_relative_path(thread_id: &str, stored_name: &str) -> String`
  - `pub fn store_attachment(cwd: &Path, thread_id: &str, src: &Path, max_bytes: u64) -> Result<Attachment, AttachmentError>`
  - `pub fn remove_attachments(cwd: &Path, thread_id: &str)`
  - `pub fn parse_input(params: &serde_json::Value) -> (String, Vec<String>)`
  - `pub fn prompt_with_attachments(text: &str, atts: &[Attachment]) -> String`
  - `pub enum AttachmentError { TooLarge { size: u64, max: u64 }, NotAFile(PathBuf), Io(std::io::Error) }`

- [ ] **Step 1: 写失败测试**

新建 `attachments.rs`，末尾：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_separators_and_control_chars() {
        assert_eq!(sanitize_filename("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize_filename("报告:最终版.pdf"), "报告_最终版.pdf");
        assert_eq!(sanitize_filename("   "), "attachment");
        assert!(!sanitize_filename("a/b").contains('/'));
    }

    #[test]
    fn stored_name_is_hash_prefixed_in_a_per_thread_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("报告.pdf");
        std::fs::write(&src, b"hello").unwrap();

        let att = store_attachment(tmp.path(), "thread-1", &src, 1024).unwrap();
        assert!(att.path.starts_with(".yi-agent/attachments/thread-1/"), "{}", att.path);
        assert!(att.path.ends_with("-报告.pdf"), "{}", att.path);
        assert_eq!(att.size, 5);
        assert_eq!(att.name, "报告.pdf");
        let on_disk = tmp.path().join(&att.path);
        assert!(on_disk.is_file(), "{on_disk:?} missing");
    }

    #[test]
    fn the_same_file_attached_twice_reuses_one_copy() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("a.txt");
        std::fs::write(&src, b"same").unwrap();
        let first = store_attachment(tmp.path(), "t1", &src, 1024).unwrap();
        let second = store_attachment(tmp.path(), "t1", &src, 1024).unwrap();
        assert_eq!(first.path, second.path);
    }

    #[test]
    fn different_threads_get_separate_copies() {
        // 这是「删一个 thread 不能误删另一个的附件」的结构保证。
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("a.txt");
        std::fs::write(&src, b"same").unwrap();
        let a = store_attachment(tmp.path(), "ta", &src, 1024).unwrap();
        let b = store_attachment(tmp.path(), "tb", &src, 1024).unwrap();
        assert_ne!(a.path, b.path);
        remove_attachments(tmp.path(), "ta");
        assert!(!tmp.path().join(&a.path).exists());
        assert!(tmp.path().join(&b.path).is_file(), "tb's copy must survive");
    }

    #[test]
    fn oversized_file_is_refused() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("big.bin");
        std::fs::write(&src, vec![0u8; 100]).unwrap();
        match store_attachment(tmp.path(), "t1", &src, 10) {
            Err(AttachmentError::TooLarge { size, max }) => {
                assert_eq!(size, 100);
                assert_eq!(max, 10);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn removing_a_thread_also_drops_an_empty_attachments_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("a.txt");
        std::fs::write(&src, b"x").unwrap();
        store_attachment(tmp.path(), "t1", &src, 1024).unwrap();
        remove_attachments(tmp.path(), "t1");
        assert!(!tmp.path().join(".yi-agent/attachments").exists(), "leftover shell dir");
    }

    #[test]
    fn removing_a_missing_thread_is_a_no_op() {
        let tmp = tempfile::TempDir::new().unwrap();
        remove_attachments(tmp.path(), "never-existed"); // 不得 panic
    }

    #[test]
    fn parse_input_splits_text_and_attachment_paths() {
        let params = serde_json::json!({
            "threadId": "t1",
            "input": [
                { "type": "attachment", "path": "/tmp/a.pdf" },
                { "type": "text", "text": "总结一下" },
                { "type": "attachment", "path": "/tmp/b.docx" }
            ]
        });
        let (text, paths) = parse_input(&params);
        assert_eq!(text, "总结一下");
        assert_eq!(paths, vec!["/tmp/a.pdf".to_string(), "/tmp/b.docx".to_string()]);
    }

    #[test]
    fn parse_input_allows_attachment_only_messages() {
        let params = serde_json::json!({
            "input": [{ "type": "attachment", "path": "/tmp/a.pdf" }]
        });
        let (text, paths) = parse_input(&params);
        assert!(text.is_empty());
        assert_eq!(paths.len(), 1);
    }

    #[test]
    fn prompt_lists_attachments_deterministically() {
        let atts = vec![Attachment {
            name: "报告.pdf".into(),
            path: ".yi-agent/attachments/t1/a1b2c3d4-报告.pdf".into(),
            mime: Some("application/pdf".into()),
            size: 1234,
        }];
        let prompt = prompt_with_attachments("总结一下", &atts);
        assert!(prompt.starts_with("总结一下"));
        assert!(prompt.contains("read_document"));
        assert!(prompt.contains("报告.pdf"));
        assert!(prompt.contains(".yi-agent/attachments/t1/a1b2c3d4-报告.pdf"));
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server attachments::`
Expected: 编译失败（模块不存在）

- [ ] **Step 3: 实现**

```rust
//! 附件落盘、安全化、清单渲染与清理。
//!
//! 附件复制进「该 thread 自己的工作区目录」而不是全局哈希去重目录：
//! 全局去重会让两个 thread 共享同一文件，删掉其一即误删另一方的附件。按
//! thread 隔离后「删除 = 删除整个目录」，无需引用计数（见设计 §6）。

use crate::protocol::Attachment;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const MAX_ATTACHMENT_BYTES: u64 = 50 * 1024 * 1024;
pub const MAX_ATTACHMENT_BYTES_ENV: &str = "YI_AGENT_ATTACHMENT_MAX_BYTES";

#[derive(Debug)]
pub enum AttachmentError {
    TooLarge { size: u64, max: u64 },
    NotAFile(PathBuf),
    Io(std::io::Error),
}

/// 解析大小上限；缺省 50 MiB。非法值回退默认（不 panic）。
pub fn resolve_max_bytes() -> u64 {
    match std::env::var(MAX_ATTACHMENT_BYTES_ENV) {
        Ok(raw) => raw.trim().parse::<u64>().ok().filter(|n| *n > 0)
            .unwrap_or(MAX_ATTACHMENT_BYTES),
        Err(_) => MAX_ATTACHMENT_BYTES,
    }
}

/// 去掉路径分隔符与控制字符，避免目录穿越；空名回退 `attachment`。
pub fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c.is_control() || c == ':' {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.');
    if trimmed.is_empty() {
        "attachment".to_string()
    } else {
        trimmed.to_string()
    }
}

/// `path` 字段的值：相对工作区根，供手写路径与工具使用。
pub fn stored_relative_path(thread_id: &str, stored_name: &str) -> String {
    format!(".yi-agent/attachments/{thread_id}/{stored_name}")
}

fn thread_dir(cwd: &Path, thread_id: &str) -> PathBuf {
    cwd.join(".yi-agent").join("attachments").join(thread_id)
}

/// 复制一份附件进工作区，返回其元数据。
///
/// 文件名形如 `<sha256[0:8]>-<安全化原名>`：同一 thread 内重复附加同一内容会
/// 命中同一路径（幂等覆盖），跨 thread 则各存一份。
pub fn store_attachment(
    cwd: &Path,
    thread_id: &str,
    src: &Path,
    max_bytes: u64,
) -> Result<Attachment, AttachmentError> {
    let meta = std::fs::metadata(src).map_err(AttachmentError::Io)?;
    if !meta.is_file() {
        return Err(AttachmentError::NotAFile(src.to_path_buf()));
    }
    if meta.len() > max_bytes {
        return Err(AttachmentError::TooLarge { size: meta.len(), max: max_bytes });
    }
    let bytes = std::fs::read(src).map_err(AttachmentError::Io)?;

    let original = src
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("attachment");
    let safe = sanitize_filename(original);
    let digest = Sha256::digest(&bytes);
    let prefix = &format!("{digest:x}")[..8];
    let stored_name = format!("{prefix}-{safe}");

    let dir = thread_dir(cwd, thread_id);
    std::fs::create_dir_all(&dir).map_err(AttachmentError::Io)?;
    std::fs::write(dir.join(&stored_name), &bytes).map_err(AttachmentError::Io)?;

    let mime = mime_for(Path::new(&safe));
    Ok(Attachment {
        name: original.to_string(),
        path: stored_relative_path(thread_id, &stored_name),
        mime,
        size: meta.len(),
    })
}

/// 按扩展名给一个提示性 MIME（不追求完备，仅用于 UI 与清单展示）。
fn mime_for(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let mime = match ext.as_str() {
        "pdf" => "application/pdf",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "txt" | "md" | "csv" => "text/plain",
        "html" | "htm" => "text/html",
        "rtf" => "application/rtf",
        _ => return None,
    };
    Some(mime.to_string())
}

/// 删除该 thread 的全部附件；尽力而为——目录不存在不算失败，空壳目录顺手收走。
pub fn remove_attachments(cwd: &Path, thread_id: &str) {
    let dir = thread_dir(cwd, thread_id);
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        if e.kind() != std::io::ErrorKind::NotFound {
            eprintln!("[app-server] failed to remove attachments for {thread_id}: {e}");
        }
    }
    // 不留空壳：与 settings_store 清理空 `.yi-agent` 目录的做法一致。
    let _ = std::fs::remove_dir(cwd.join(".yi-agent").join("attachments"));
}

/// 从 `turn/start` 的 params 里拆出用户文本与附件源路径。
///
/// 非 text / attachment 的 block 仍被忽略（与既有 `extract_prompt` 同口径）。
pub fn parse_input(params: &serde_json::Value) -> (String, Vec<String>) {
    let Some(input) = params.get("input").and_then(|v| v.as_array()) else {
        return (String::new(), Vec::new());
    };
    let mut text = String::new();
    let mut paths = Vec::new();
    for block in input {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    text.push_str(t);
                }
            }
            Some("attachment") => {
                if let Some(p) = block.get("path").and_then(|p| p.as_str()) {
                    paths.push(p.to_string());
                }
            }
            _ => {}
        }
    }
    (text, paths)
}

/// 把附件清单拼进发给 agent 的 prompt。
///
/// `Item.text` 仍然只是用户原话（气泡只显示原话 + chip），这段注入只出现在
/// 送给模型的消息里。
pub fn prompt_with_attachments(text: &str, attachments: &[Attachment]) -> String {
    if attachments.is_empty() {
        return text.to_string();
    }
    let mut out = String::from(text);
    out.push_str("\n\nAttached files (read them with read_document):\n");
    for a in attachments {
        let mime = a.mime.as_deref().unwrap_or("application/octet-stream");
        out.push_str(&format!("- {} ({mime}, {} bytes) -> {}\n", a.name, a.size, a.path));
    }
    out
}
```

`lib.rs` 加：`pub mod attachments;`（放在 `mod broadcast;` 附近，与其他模块同列）。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server attachments::`
Expected: 10 个测试全 PASS

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/
git commit -m "feat(app-server): store, list and clean up thread attachments"
```

---

## Task 5: `TurnPrompt` 携带附件

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/session.rs:14-24`（`TurnPrompt`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（9 个构造点 + driver 解构）

**Interfaces:**
- Produces: `TurnPrompt { turn_id, prompt, activate, attachments: Vec<Attachment> }`；driver 持有该 `Vec<Attachment>` 以便落盘时写进开启项

- [ ] **Step 1: 加字段并编译**

`session.rs`：

```rust
pub struct TurnPrompt {
    pub turn_id: String,
    pub prompt: String,
    /// 该 thread 若已 attach 到项目 runtime，则带上它自己的 root，在**首个 turn** 激活。
    pub activate: Option<Arc<yi_agent_subagent::thread_root::ThreadRoot>>,
    /// 本轮用户附加的文档；落盘时写进开启的 `userMessage` item 供回放渲染。
    pub attachments: Vec<crate::protocol::Attachment>,
}
```

Run: `cd yi-agent-rs && cargo build -p yi-agent-app-server 2>&1 | grep -E "^error|server.rs:|session.rs:" | head -30`

逐处补 `attachments: Vec::new()` 或传真实值。构造点：`server.rs:5596`（board 路径）、`6000`（`start_turn_core`）、`9626`、`9747`、`9891`、`10281`、`11297`（测试）。**测试里**统一用 `attachments: Vec::new()`。

- [ ] **Step 2: driver 解构处接住**

`server.rs:4898` 的模式改为：

```rust
        let Some(TurnPrompt {
            turn_id,
            prompt,
            activate,
            attachments: turn_attachments,
        }) = turn_prompt
        else {
            break; // prompt_rx 关闭:driver 收尾退出
        };
```

`turn_attachments` 要活到落盘处；在 `let user_prompt = prompt.clone();` 附近加：

```rust
        let user_attachments = turn_attachments.clone();
```

- [ ] **Step 3: 跑测试确认没回归**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server`
Expected: PASS（既有测试全绿）

- [ ] **Step 4: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/
git commit -m "feat(app-server): carry attachments on the turn prompt"
```

---

## Task 6: `turn/start` 解析附件、复制、注入 prompt、写入 item

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`extract_prompt`、`TurnPrepareError`、`PreparedTurn`、`prepare_turn_core`、`opening_user_item`、`build_partial`、`persist_and_finish_turn`）
- Test: `server.rs` 的 `mod tests`（照 `turn_start_emits_full_notification_sequence`，`server.rs:9342` 的风格）

**Interfaces:**
- Consumes: Task 4 的 `attachments::*`、Task 5 的 `TurnPrompt.attachments`
- Produces: `opening_user_item(turn_id: &str, text: &str, attachments: Vec<Attachment>) -> Item`；`build_partial(turn_id, user_prompt, user_attachments: &[Attachment], completed_items, messages, usage)`；`persist_and_finish_turn(..., user_attachments: &[Attachment], ...)`；`TurnPrepareError::InvalidAttachment(String)`

- [ ] **Step 1: 写失败测试**

在 `server.rs` 的 tests 里加（照既有 `turn_start_emits_full_notification_sequence` 的握手写法）：

```rust
    #[tokio::test]
    async fn turn_start_copies_attachments_and_echoes_them_on_the_item() {
        let h = /* 既有测试 harness 构造，见 turn_start_emits_full_notification_sequence */;
        let tid = h.start_thread().await;
        // 源文件放在工作区外，模拟「从 Downloads 选文件」。
        let outside = tempfile::TempDir::new().unwrap();
        let src = outside.path().join("报告.pdf");
        std::fs::write(&src, b"%PDF-1.4 fake").unwrap();

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"attachment","path":"{}"}},{{"type":"text","text":"总结一下"}}]}}}}"#,
            src.display()
        ))
        .await;

        // 找到 item/started 里的 userMessage，断言收到了附件元数据。
        let item = h.await_item_started("userMessage").await;
        assert_eq!(item["attachments"][0]["name"], "报告.pdf");
        let rel = item["attachments"][0]["path"].as_str().unwrap();
        assert!(rel.starts_with(".yi-agent/attachments/"), "{rel}");

        // 磁盘上确实复制成功（相对 thread 的 cwd）。
        assert!(h.cwd().join(rel).is_file(), "attachment was not copied");
        // 气泡正文是用户原话，不含注入的清单。
        assert_eq!(item["text"], "总结一下");
    }

    #[tokio::test]
    async fn turn_start_accepts_an_attachment_only_message() {
        // 不打字、只发附件也必须能起 turn（空 input 校验放宽）。
        let h = /* harness */;
        let tid = h.start_thread().await;
        let src = /* 临时文件 */;
        let (id, resp) = h.exchange(&format!(
            r#"{{"jsonrpc":"2.0","id":9,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"attachment","path":"{}"}}]}}}}"#,
            src.display()
        ))
        .await;
        assert!(resp.get("error").is_none(), "attachment-only start must succeed: {resp:?}");
        let _ = id;
    }

    #[tokio::test]
    async fn turn_start_rejects_an_unreadable_attachment() {
        let h = /* harness */;
        let tid = h.start_thread().await;
        let (_id, resp) = h.exchange(&format!(
            r#"{{"jsonrpc":"2.0","id":10,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"attachment","path":"/definitely/not/here.pdf"}},{{"type":"text","text":"hi"}}]}}}}"#
        ))
        .await;
        let err = resp["error"]["message"].as_str().unwrap_or_default().to_string();
        assert!(err.contains("attachment"), "got: {err}");
    }
```

harness 方法与字段以既有测试为准（`h.exchange` / `h.read_value` / 起 thread 的 helper）；若某 helper 不存在，用 `turn_start_emits_full_notification_sequence` 里的原样写法。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server turn_start_copies_attachments`
Expected: FAIL（附件未被复制、item 无 attachments）

- [ ] **Step 3: 实现**

**(a)** `extract_prompt` 改为委托（保持行为：只有文本、trim 后为空则 `None`）：

```rust
/// 从 `turn/start` 的 params 提取用户文本:`input:[{type:"text",text}]` 拼接。
///
/// 非 text 的 block（含 attachment）由 `attachments::parse_input` 另行处理；
/// 这里只看文本，空文本返回 `None`。**注意**：附件-only 的消息在这里是 `None`，
/// 是否放行由调用方结合附件列表决定（见 `prepare_turn_core`）。
fn extract_prompt(params: &serde_json::Value) -> Option<String> {
    let (text, _paths) = crate::attachments::parse_input(params);
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}
```

**(b)** `TurnPrepareError` 加变体，并更新 `turn_prepare_rpc_error`：

```rust
    /// 附件不可读 / 超出上限 / 不是文件。
    InvalidAttachment(String),
```

```rust
        TurnPrepareError::InvalidAttachment(msg) => RpcError::invalid_params(msg),
```

**(c)** `PreparedTurn` 加字段：

```rust
    pub(crate) attachments: Vec<crate::protocol::Attachment>,
```

**(d)** `prepare_turn_core` 核心改动：

```rust
    let (text, source_paths) = crate::attachments::parse_input(params);
    let text_opt = if text.trim().is_empty() { None } else { Some(text) };
    if text_opt.is_none() && source_paths.is_empty() {
        return Err(TurnPrepareError::EmptyInput);
    }

    let turn_id = format!("turn-{}", uuid::Uuid::new_v4());

    // 取该 thread 的 cwd + 状态句柄（借用先结束）。
    let (prompt_tx, status_handle, cwd) = {
        let Some(session) = threads.get_mut(&thread_id) else {
            return Err(TurnPrepareError::UnknownThread(thread_id));
        };
        if session.active_turn_id.is_some() {
            return Err(TurnPrepareError::TurnInProgress(thread_id));
        }
        (session.prompt_tx.clone(), Arc::clone(&session.status), session.cwd.clone())
    };

    // 复制附件进工作区。放在占 active_turn_id **之前**：失败要能干净地拒绝，
    // 不留一个已占用但起不来的 turn。
    let max_bytes = crate::attachments::resolve_max_bytes();
    let mut attachments = Vec::with_capacity(source_paths.len());
    for source in &source_paths {
        match crate::attachments::store_attachment(
            std::path::Path::new(&cwd),
            &thread_id,
            std::path::Path::new(source),
            max_bytes,
        ) {
            Ok(att) => attachments.push(att),
            Err(crate::attachments::AttachmentError::TooLarge { size, max }) => {
                return Err(TurnPrepareError::InvalidAttachment(format!(
                    "attachment {source} is {size} bytes, over the {max} byte limit"
                )));
            }
            Err(crate::attachments::AttachmentError::NotAFile(path)) => {
                return Err(TurnPrepareError::InvalidAttachment(format!(
                    "attachment {} is not a regular file",
                    path.display()
                )));
            }
            Err(crate::attachments::AttachmentError::Io(e)) => {
                return Err(TurnPrepareError::InvalidAttachment(format!(
                    "could not read attachment {source}: {e}"
                )));
            }
        }
    }

    // 占位（复制成功后）。
    {
        let Some(session) = threads.get_mut(&thread_id) else {
            return Err(TurnPrepareError::UnknownThread(thread_id));
        };
        if session.active_turn_id.is_some() {
            return Err(TurnPrepareError::TurnInProgress(thread_id));
        }
        session.active_turn_id = Some(turn_id.clone());
    }

    let activate = pending_activation.get(&thread_id).cloned().flatten();
    // prompt 送进模型时带上附件清单；气泡文本仍是原话（见 opening_user_item）。
    let agent_prompt = crate::attachments::prompt_with_attachments(
        text_opt.as_deref().unwrap_or(""),
        &attachments,
    );
    Ok(PreparedTurn {
        thread_id,
        turn_id,
        prompt_tx,
        prompt: agent_prompt,
        display_text: text_opt.unwrap_or_default(),
        attachments,
        activate,
        status_handle,
    })
```

`PreparedTurn` 因此需要两个字段：`pub(crate) display_text: String`（气泡用）与 `prompt: String`（送模型用）。

**(e)** `opening_user_item` 加参数：

```rust
fn opening_user_item(
    turn_id: &str,
    text: &str,
    attachments: Vec<crate::protocol::Attachment>,
) -> crate::protocol::Item {
    crate::protocol::Item::UserMessage {
        id: format!("user-{turn_id}"),
        text: text.to_string(),
        attachments,
    }
}
```

`start_turn_core` 里两处调用（`server.rs:5981` 附近）改用 `&prepared.display_text` 与 `prepared.attachments.clone()`；`TurnPrompt` 构造加 `attachments: prepared.attachments.clone()`。

**(f)** `build_partial` 与 `persist_and_finish_turn` 加 `user_attachments: &[Attachment]` 参数，并在构造 `Item::UserMessage` 处填入 `user_attachments.to_vec()`（`server.rs:4718`、`server.rs:4675`）。三个调用点（`4780`、`4817`、`5298`）与 `build_partial` 的调用点补上 driver 持有的 `user_attachments`。

**(g)** `turn/interject` 分支：**不带附件**（设计 §8）。保持其 `extract_prompt` 用法，其 `Item::UserInterjection` 构造填 `attachments: Vec::new()`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server`
Expected: 全绿（含 3 个新测试）

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/
git commit -m "feat(app-server): copy attachments and inject their manifest into the prompt"
```

---

## Task 7: `thread/delete` 清理附件

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs`（加 `root()`）
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs:3355-3358`（delete 分支）
- Test: `server.rs` 的 tests

**Interfaces:**
- Consumes: Task 4 的 `attachments::remove_attachments(cwd, thread_id)`
- Produces: `ThreadStore::root(&self) -> &Path`

- [ ] **Step 1: 写失败测试**

```rust
    #[tokio::test]
    async fn deleting_a_thread_removes_its_attachments() {
        let h = /* harness */;
        let tid = h.start_thread().await;
        let src = /* 临时 pdf 文件 */;
        // 起一个带附件的 turn，确保附件已复制。
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"attachment","path":"{}"}},{{"type":"text","text":"hi"}}]}}}}"#,
            src.display()
        ))
        .await;
        let att_dir = h.cwd().join(".yi-agent/attachments").join(&tid);
        assert!(att_dir.is_dir(), "attachment dir should exist before delete");

        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":5,"method":"thread/delete","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;

        assert!(!att_dir.exists(), "attachments must be reclaimed with the thread");
    }

    #[tokio::test]
    async fn clearing_a_thread_keeps_its_attachments() {
        // 设计 §6(e)：/clear 截断历史但会话还在，附件不清。
        let h = /* harness */;
        let tid = h.start_thread().await;
        let src = /* 临时 pdf 文件 */;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":3,"method":"turn/start","params":{{"threadId":"{tid}","input":[{{"type":"attachment","path":"{}"}},{{"type":"text","text":"hi"}}]}}}}"#,
            src.display()
        ))
        .await;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"method":"thread/clear","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;

        let att_dir = h.cwd().join(".yi-agent/attachments").join(&tid);
        assert!(att_dir.is_dir(), "thread/clear must not remove attachments");
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server deleting_a_thread_removes_its_attachments`
Expected: FAIL（附件目录仍在）

- [ ] **Step 3: 实现**

`thread_store.rs` 加访问器（`root` 是私有字段，delete 分支拿不到 cwd——`server.rs:753-757` 的注释明确警告不要为此改 `store_lookup` 签名，故由 root 反推）：

```rust
    /// 存储根（`<cwd>/.yi-agent/threads`）。
    ///
    /// 供调用方从 root 反推会话工作区——`thread/delete` 分支拿不到 cwd，附件
    /// 清理需要它（见设计 §6(d)）。
    pub fn root(&self) -> &Path {
        &self.root
    }
```

`server.rs` delete 分支，在 `thread_store.delete(...)` 之后、`write_response` 之前插入：

```rust
                        // 收走该会话的附件：与 thread 文件同一生命周期。放在
                        // delete 之后（driver 已结束落盘），避免竞态。工作区被
                        // 挪走时 `remove_attachments` 尽力而为，不让删除失败。
                        if let Some(cwd) = thread_store.root().parent().and_then(|p| p.parent()) {
                            crate::attachments::remove_attachments(cwd, &thread_id);
                        }
```

`root()` 是 `<cwd>/.yi-agent/threads`，`parent()` 两次得到 `<cwd>`。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server`
Expected: 全绿（含 2 个新测试）

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs && cargo fmt --all
git add crates/yi-agent-app-server/
git commit -m "feat(app-server): reclaim a thread's attachments on delete"
```

---

## Task 8: 前端协议类型与本地预检

**Files:**
- Modify: `desktop/src/lib/protocol.ts`（`Item` 联合）
- Create: `desktop/src/lib/attachmentLimits.ts`
- Test: `desktop/src/lib/attachmentLimits.test.ts`

**Interfaces:**
- Produces: `Attachment` 接口；`Item` 的 `userMessage` / `user_interjection` 变体加 `attachments?: Attachment[]`；`MAX_ATTACHMENT_BYTES`、`ATTACHMENT_EXTENSIONS`、`attachmentProblem(path: string, size: number) => string | null`、`fileNameOf(path: string) => string`、`PendingAttachment` 接口

- [ ] **Step 1: 写失败测试**

`desktop/src/lib/attachmentLimits.test.ts`：

```ts
import { describe, expect, it } from "vitest";
import {
  ATTACHMENT_EXTENSIONS,
  attachmentProblem,
  fileNameOf,
  MAX_ATTACHMENT_BYTES,
} from "./attachmentLimits";

describe("fileNameOf", () => {
  it("takes the last path segment on posix and windows", () => {
    expect(fileNameOf("/Users/me/报告.pdf")).toBe("报告.pdf");
    expect(fileNameOf("C:\\Users\\me\\report.docx")).toBe("report.docx");
  });
});

describe("attachmentProblem", () => {
  it("accepts a supported document under the size limit", () => {
    expect(attachmentProblem("/tmp/报告.pdf", 1024)).toBeNull();
  });

  it("rejects an unsupported extension", () => {
    const problem = attachmentProblem("/tmp/movie.mov", 1024);
    expect(problem).toContain("mov");
  });

  it("rejects a file over the limit and names the limit", () => {
    const problem = attachmentProblem("/tmp/报告.pdf", MAX_ATTACHMENT_BYTES + 1);
    expect(problem).not.toBeNull();
    expect(problem).toContain("50 MB");
  });

  it("is case-insensitive about the extension", () => {
    expect(attachmentProblem("/tmp/REPORT.PDF", 1)).toBeNull();
  });
});

describe("ATTACHMENT_EXTENSIONS", () => {
  it("covers the formats read_document supports", () => {
    for (const ext of ["pdf", "docx", "txt", "md", "csv", "html", "htm", "rtf"]) {
      expect(ATTACHMENT_EXTENSIONS).toContain(ext);
    }
  });
});
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/attachmentLimits.test.ts`
Expected: FAIL（模块不存在）

- [ ] **Step 3: 实现**

`desktop/src/lib/attachmentLimits.ts`：

```ts
/**
 * 附件的前端预检。
 *
 * 服务端是权威（它会拒绝并给出错误），这里只做**发送前**的就地提示，省掉一次
 * 注定失败的往返。上限与服务端的 `MAX_ATTACHMENT_BYTES` 保持一致
 * （`yi-agent-app-server/src/attachments.rs`）。
 */

export const MAX_ATTACHMENT_BYTES = 50 * 1024 * 1024;

/** 与 `read_document` 支持的格式一致。 */
export const ATTACHMENT_EXTENSIONS = [
  "pdf",
  "docx",
  "txt",
  "md",
  "csv",
  "html",
  "htm",
  "rtf",
] as const;

/** 尚未发送的附件：只有本地路径与文件名，服务端元数据要发送后才产生。 */
export interface PendingAttachment {
  path: string;
  name: string;
  size: number;
}

/** 路径的最后一段（兼容 `/` 与 `\`）。 */
export function fileNameOf(path: string): string {
  const parts = path.split(/[\\/]/);
  return parts[parts.length - 1] || path;
}

function extensionOf(path: string): string {
  const name = fileNameOf(path);
  const dot = name.lastIndexOf(".");
  return dot < 0 ? "" : name.slice(dot + 1).toLowerCase();
}

/** 返回不可发送的原因；可发送返回 null。 */
export function attachmentProblem(path: string, size: number): string | null {
  const ext = extensionOf(path);
  if (!(ATTACHMENT_EXTENSIONS as readonly string[]).includes(ext)) {
    return `不支持的文件类型：${ext || "（无扩展名）"}`;
  }
  if (size > MAX_ATTACHMENT_BYTES) {
    return "文件超过 50 MB 上限";
  }
  return null;
}
```

`desktop/src/lib/protocol.ts` 加类型与字段：

```ts
/** 一条消息附带的文档元数据（内容不进协议，agent 用 read_document 读）。 */
export interface Attachment {
  name: string;
  /** 相对工作区根的路径，形如 `.yi-agent/attachments/<thread_id>/<hash>-<name>`。 */
  path: string;
  mime?: string;
  size: number;
}
```

`Item` 的 `userMessage` 与 `user_interjection` 变体各加：

```ts
      attachments?: Attachment[];
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/lib/attachmentLimits.test.ts && npx tsc --noEmit`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add desktop/src/lib/
git commit -m "feat(desktop): attachment protocol types and local pre-checks"
```

---

## Task 9: 附件 chip 与输入框回形针

**Files:**
- Create: `desktop/src/components/AttachmentChips.tsx`
- Modify: `desktop/src/components/MessageInput.tsx`
- Test: `desktop/src/components/AttachmentChips.test.tsx`、`desktop/src/components/MessageInput.test.tsx`

**Interfaces:**
- Consumes: Task 8 的 `fileNameOf`
- Produces: `AttachmentChips({ attachments, onRemove })`；`MessageInput` 新增 props `attachments: PendingAttachment[]`、`onPickFiles: () => void`、`onRemoveAttachment: (path: string) => void`

`PendingAttachment` 由 Task 8 的 `attachmentLimits.ts` 导出。

- [ ] **Step 1: 写失败测试**

`desktop/src/components/AttachmentChips.test.tsx`：

```tsx
import { describe, expect, it, vi } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { AttachmentChips } from "./AttachmentChips";

describe("AttachmentChips", () => {
  it("renders one chip per attachment with its name", () => {
    render(
      <AttachmentChips
        attachments={[
          { path: "/tmp/报告.pdf", name: "报告.pdf", size: 2048 },
          { path: "/tmp/说明.docx", name: "说明.docx", size: 512 },
        ]}
        onRemove={() => {}}
      />,
    );
    expect(screen.getByText("报告.pdf")).toBeTruthy();
    expect(screen.getByText("说明.docx")).toBeTruthy();
  });

  it("reports the removed path", () => {
    const onRemove = vi.fn();
    render(
      <AttachmentChips
        attachments={[{ path: "/tmp/报告.pdf", name: "报告.pdf", size: 2048 }]}
        onRemove={onRemove}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: /移除 报告.pdf/ }));
    expect(onRemove).toHaveBeenCalledWith("/tmp/报告.pdf");
  });

  it("renders nothing when there are no attachments", () => {
    const { container } = render(<AttachmentChips attachments={[]} onRemove={() => {}} />);
    expect(container.firstChild).toBeNull();
  });
});
```

`MessageInput.test.tsx` 追加两例：

```tsx
  it("sends when only files are attached and the text box is empty", async () => {
    const onSend = vi.fn().mockResolvedValue(true);
    render(
      <MessageInput
        {...baseProps}
        onSend={onSend}
        value=""
        attachments={[{ path: "/tmp/报告.pdf", name: "报告.pdf", size: 10 }]}
        onPickFiles={() => {}}
        onRemoveAttachment={() => {}}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "发送" }));
    expect(onSend).toHaveBeenCalled();
  });

  it("keeps the send button disabled with neither text nor files", () => {
    render(
      <MessageInput
        {...baseProps}
        value=""
        attachments={[]}
        onPickFiles={() => {}}
        onRemoveAttachment={() => {}}
      />,
    );
    expect(screen.getByRole("button", { name: "发送" })).toBeDisabled();
  });
```

`baseProps` 与按钮可访问名以既有 `MessageInput.test.tsx` 为准（先读该文件，照抄既有渲染 helper 与 `aria-label`；下节说明发送按钮的既有 label 是 `Send`/`Stop` 文案，按实际值写断言）。

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/AttachmentChips.test.tsx`
Expected: FAIL（组件不存在）

- [ ] **Step 3: 实现**

`desktop/src/components/AttachmentChips.tsx`：

```tsx
import { fileNameOf } from "../lib/attachmentLimits";
import type { PendingAttachment } from "../lib/attachmentLimits";

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/** 输入框上方待发送的附件行；每个 chip 可单独移除。 */
export function AttachmentChips({
  attachments,
  onRemove,
}: {
  attachments: PendingAttachment[];
  onRemove: (path: string) => void;
}) {
  if (attachments.length === 0) return null;
  return (
    <div className="flex flex-wrap gap-1 px-2 pt-2" data-testid="attachment-chips">
      {attachments.map((a) => (
        <span
          key={a.path}
          className="flex items-center gap-1 rounded border border-line bg-raised px-2 py-0.5 text-xs text-fg-muted"
        >
          <span className="max-w-[16rem] truncate" title={a.path}>
            {a.name || fileNameOf(a.path)}
          </span>
          <span className="text-fg-subtle">{formatSize(a.size)}</span>
          <button
            type="button"
            aria-label={`移除 ${a.name || fileNameOf(a.path)}`}
            className="text-fg-subtle hover:text-red-400"
            onClick={() => onRemove(a.path)}
          >
            ×
          </button>
        </span>
      ))}
    </div>
  );
}
```

（`PendingAttachment` 与 `fileNameOf` 都从 `../lib/attachmentLimits` 导入；该文件同时导出这个接口。）

`MessageInput.tsx` 改动：

1. props 加 `attachments: PendingAttachment[]`、`onPickFiles: () => void`、`onRemoveAttachment: (path: string) => void`。
2. `handleSend` 的放行条件从 `!text.trim()` 改为 `(!text.trim() && attachments.length === 0)`；`onSend(text)` 之后仅当 `ok` 时 `onDraftChange("")`——**附件清理由父级在成功时做**（见 Task 10），因为「发送成功」是父级的知识。
3. 输入区上方渲染 `<AttachmentChips attachments={attachments} onRemove={onRemoveAttachment} />`。
4. 在工具栏加回形针按钮：

```tsx
          <button
            type="button"
            aria-label="附加文件"
            title="附加文件"
            className="rounded px-2 py-0.5 text-fg-muted hover:text-fg"
            onClick={onPickFiles}
            disabled={disabled}
          >
            附加文件
          </button>
```

用纯文本标签，与仓库其余 UI 一致（**不使用 emoji**）；`aria-label` 是测试的锚点。

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/AttachmentChips.test.tsx src/components/MessageInput.test.tsx`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add desktop/src/components/
git commit -m "feat(desktop): attachment chips and file-picker button in the composer"
```

---

## Task 10: App 接线（多选选择器、发送 blocks、乐观气泡）

**Files:**
- Modify: `desktop/src/App.tsx`（`pickFilesToAttach`、pending 状态、`send`、`MessageInput` 接线）
- Modify: `desktop/src/lib/session.ts`（`addUserMessage` 带 attachments）
- Test: `desktop/src/App.test.tsx`、`desktop/src/lib/session.test.ts`

**Interfaces:**
- Consumes: Task 8 的 `attachmentProblem`、Task 9 的 `PendingAttachment` / `MessageInput` props
- Produces: `Session.addUserMessage(text: string, attachments?: Attachment[])`

- [ ] **Step 1: 写失败测试**

`desktop/src/lib/session.test.ts` 追加：

```ts
  it("keeps attachments on the optimistic user bubble", () => {
    const s = new Session();
    s.addUserMessage("总结一下", [
      { name: "报告.pdf", path: ".yi-agent/attachments/t1/a1b2-报告.pdf", size: 10 },
    ]);
    expect(s.items[0]).toMatchObject({
      type: "userMessage",
      text: "总结一下",
      attachments: [{ name: "报告.pdf" }],
    });
  });
```

`desktop/src/App.test.tsx` 追加（照既有 `send` 相关用例的 mock 风格）：

```tsx
  it("sends attachments as input blocks before the text block", async () => {
    // ...按既有用例搭好 client mock 与当前会话...
    // 选中一个附件后发送。
    await act(async () => {
      pickFilesMock.mockResolvedValue(["/tmp/报告.pdf"]);
      // 触发回形针
      fireEvent.click(screen.getByRole("button", { name: "附加文件" }));
    });
    await act(async () => {
      fireEvent.change(screen.getByRole("textbox"), { target: { value: "总结一下" } });
      fireEvent.click(screen.getByRole("button", { name: "发送" }));
    });

    const call = requestMock.mock.calls.find(([m]) => m === "turn/start");
    expect(call?.[1]).toEqual({
      threadId: expect.any(String),
      input: [
        { type: "attachment", path: "/tmp/报告.pdf" },
        { type: "text", text: "总结一下" },
      ],
    });
  });

  it("surfaces a local pre-check failure instead of sending", async () => {
    // 选一个 .mov：不得发出 turn/start，且把原因写进会话错误。
    // ...
  });
```

（这两例的具体 mock 组织方式照抄 `App.test.tsx` 里既有的 `send` 用例——那里已经 mock 了 `rpc` 与 `@tauri-apps/plugin-dialog`。）

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/lib/session.test.ts`
Expected: FAIL（`addUserMessage` 不接受第二参数）

- [ ] **Step 3: 实现**

`session.ts`：

```ts
  addUserMessage(text: string, attachments: Attachment[] = []): void {
    this.items.push({
      type: "userMessage",
      id: nextLocalId(),
      text,
      ...(attachments.length > 0 ? { attachments } : {}),
    });
  }
```

（`dropLocalUserMessage` 匹配 `text` 即可，附件不影响回滚。）

`App.tsx`：

```ts
  /** 每个会话待发送的附件（按会话隔离，与草稿同理）。 */
  const [pending, setPending] = useState<Record<string, PendingAttachment[]>>({});

  /** 多选文件；逐个本地预检，不合格的就地报错，合格的加入待发送。 */
  const pickFilesToAttach = async (): Promise<void> => {
    const id = store.currentId;
    if (!id) return;
    if (isRemoteClient()) return; // iOS/远端没有原生选择器
    const { open } = await import("@tauri-apps/plugin-dialog");
    const picked = await open({
      directory: false,
      multiple: true,
      filters: [{ name: "文档", extensions: [...ATTACHMENT_EXTENSIONS] }],
    });
    const paths = Array.isArray(picked) ? picked : picked ? [picked] : [];
    if (paths.length === 0) return;

    const problems: string[] = [];
    const accepted: PendingAttachment[] = [];
    for (const p of paths) {
      const name = fileNameOf(p);
      // 桌面端拿不到 file size（dialog 只给路径）：大小交给服务端权威校验，
      // 这里只做扩展名预检，避免把不支持的类型发出去。
      const problem = attachmentProblem(p, 0);
      if (problem) {
        problems.push(`${name}：${problem}`);
        continue;
      }
      accepted.push({ path: p, name, size: 0 });
    }
    setPending((prev) => ({ ...prev, [id]: [...(prev[id] ?? []), ...accepted] }));
    if (problems.length > 0) {
      store.view(id).session.lastError = problems.join("；");
      force((v) => v + 1);
    }
  };

  const removePendingAttachment = (path: string): void => {
    const id = store.currentId;
    if (!id) return;
    setPending((prev) => ({ ...prev, [id]: (prev[id] ?? []).filter((a) => a.path !== path) }));
  };
```

`send` 改为接收附件并组装 blocks：

```ts
  const send = async (text: string): Promise<boolean> => {
    const id = store.currentId;
    const c = clientRef.current;
    if (!id || !c) return false;
    const files = pending[id] ?? [];
    if (!text.trim() && files.length === 0) return false;

    const session = store.view(id).session;
    session.addUserMessage(
      text,
      files.map((f) => ({ name: f.name, path: f.path, size: f.size })),
    );
    force((v) => v + 1);

    const params = {
      threadId: id,
      input: [
        ...files.map((f) => ({ type: "attachment" as const, path: f.path })),
        { type: "text" as const, text },
      ],
    };
    // ...其余（方法选择与两跳自愈）保持不变...
  };
```

成功返回 `true` 时清空该会话的 pending（在 `return true` 前）：

```ts
        await c.request(method, params);
        setPending((prev) => ({ ...prev, [id]: [] }));
        return true;
```

失败路径保持原样（回滚乐观气泡），**且不清 pending**——用户的附件要留着重试。

`MessageInput` 接线（`App.tsx:1328` 附近）：

```tsx
              <MessageInput
                turnActive={current?.session.turnActive ?? false}
                onSend={send}
                onInterrupt={interrupt}
                mode={current?.mode ?? null}
                onModeChange={setThreadMode}
                onSlashCommand={(name, args) => void onSlashCommand(name, args)}
                value={current?.draft ?? ""}
                onDraftChange={changeDraft}
                attachments={currentId ? (pending[currentId] ?? []) : []}
                onPickFiles={() => void pickFilesToAttach()}
                onRemoveAttachment={removePendingAttachment}
                disabled={current === null}
              />
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx tsc --noEmit && npx vitest run src/App.test.tsx src/lib/session.test.ts`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add desktop/src/
git commit -m "feat(desktop): attach documents to a message and send them as blocks"
```

---

## Task 11: 用户气泡展示附件

**Files:**
- Modify: `desktop/src/components/ChatView.tsx`（`ChatItem` 的 `userMessage` 分支）
- Test: `desktop/src/components/ChatView.test.tsx`

**Interfaces:**
- Consumes: Task 8 的 `Attachment` 类型
- Produces: 用户气泡内该条消息的附件名列表

- [ ] **Step 1: 写失败测试**

```tsx
  it("lists the attachments on a user bubble", () => {
    render(
      <ChatView
        items={[
          {
            type: "userMessage",
            id: "u1",
            text: "总结一下",
            attachments: [{ name: "报告.pdf", path: ".yi-agent/attachments/t1/x-报告.pdf", size: 10 }],
          },
        ]}
      />,
    );
    expect(screen.getByText("总结一下")).toBeTruthy();
    expect(screen.getByText(/报告.pdf/)).toBeTruthy();
  });

  it("renders a user bubble without attachments unchanged", () => {
    render(<ChatView items={[{ type: "userMessage", id: "u2", text: "hi" }]} />);
    expect(screen.getByText("hi")).toBeTruthy();
  });
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd desktop && npx vitest run src/components/ChatView.test.tsx`
Expected: FAIL（附件名不可见）

- [ ] **Step 3: 实现**

`ChatView.tsx` 的 `userMessage` 分支：

```tsx
    case "userMessage":
      return (
        <div className="my-1 max-w-[80%] self-end rounded-lg bg-blue-600 px-3 py-2 text-sm whitespace-pre-wrap text-white">
          {item.attachments && item.attachments.length > 0 && (
            <div className="mb-1 flex flex-wrap gap-1">
              {item.attachments.map((a) => (
                <span
                  key={a.path}
                  className="rounded bg-blue-700/60 px-1.5 py-0.5 text-xs text-blue-50"
                  title={a.path}
                  data-testid="bubble-attachment"
                >
                  {a.name}
                </span>
              ))}
            </div>
          )}
          {item.text}
        </div>
      );
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cd desktop && npx vitest run src/components/ChatView.test.tsx`
Expected: PASS

- [ ] **Step 5: 提交**

```bash
git add desktop/src/components/
git commit -m "feat(desktop): show attachments on the user bubble"
```

---

## Task 12: 全量回归与路线图同步

**Files:**
- Modify: `docs/project-management/desktop.md`（`[ ]` → `[x]` + 判据）
- Modify: `docs/project-management/yi-agent-tools.md`（登记 `read_document`）
- Modify: `docs/project-management/yi-agent-app-server.md`（登记协议变更）

**Interfaces:**
- Consumes: 前 11 个任务的成果
- Produces: 三份模块文档与实现一致

- [ ] **Step 1: 跑后端全量测试**

Run: `ps aux | grep -v grep | grep -E "cargo|rustc" ; cd yi-agent-rs && cargo test -p yi-agent-tools && cargo test -p yi-agent-app-server`
Expected: 全绿。**先确认没有残留 cargo 进程**（CLAUDE.md：并发跑 cargo test 会锁竞争/OOM）。被中断过的话先清理僵尸测试二进制。

- [ ] **Step 2: 跑前端全量测试与构建**

Run: `cd desktop && npx vitest run && npx tsc --noEmit && npm run build`
Expected: 全绿（既有 622 例 + 本次新增）

- [ ] **Step 3: 更新项目文档**

`docs/project-management/desktop.md`：把 P2 里的「文档附件输入（PDF / DOCX 等，L1 只读）」从 `[ ]` 改成 `[x]`，判据写成本次实际实现的位置（`read_document` 工具、`attachments.rs`、`App.tsx` 的 `pickFilesToAttach`）与验证命令。

`docs/project-management/yi-agent-tools.md`：新增 `read_document` 条目（格式覆盖、字符修复、按页、预算），判据 `cargo test -p yi-agent-tools read_document`。

`docs/project-management/yi-agent-app-server.md`：记录 `turn/start` 的 `attachment` block、`Item.attachments` 字段、附件落盘布局与 `thread/delete` 清理。

- [ ] **Step 4: 跑 `cargo fmt` 并提交文档**

```bash
cd yi-agent-rs && cargo fmt --all && cargo fmt --all -- --check
git add docs/project-management/
git commit -m "docs: record document attachments as delivered"
```

---

## 已知限制（实现时不要试图消除，写进文档即可）

- 扫描版 PDF 与整页 PDF 渲染不支持；返回「疑似扫描件」提示。
- `.rtf` 按原始文本返回（含 RTF 标记），不做 RTF 解析。`.html` 走 `html2md`。
- 附件不随 `thread/clear` 清理，只随 `thread/delete`。
- 工作区内的孤立 `attachments/` 目录不做后台 GC。
- `turn/interject` 不接受附件。
- 前端 dialog 只给路径、不给大小，故大小上限**由服务端权威判定**；前端只预检扩展名。
