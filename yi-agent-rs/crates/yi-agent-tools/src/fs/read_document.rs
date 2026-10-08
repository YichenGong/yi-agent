//! `read_document`: 读取工作区内的文档（PDF / DOCX / HTML / 纯文本）。
//!
//! 两条实测结论约束实现（见 `docs/research/2026-10-07-pdf-text-extraction-spike.md`）：
//!
//! 1. **中文 PDF 必须修字符**。生产工具把字形映到 Unicode 部首码位（如 `⻓` U+2ED3
//!    而非 `长` U+957F），裸抽取器原样还原，`contains("收入构成")` 会系统性失效。
//!    修复需 **NFKC + UCD `EquivalentUnifiedIdeograph` 映射**两步：NFKC 只折叠康熙
//!    部首区（U+2F00–U+2FDF），不折叠 CJK 部首补充区（U+2E80–U+2EFF）。
//! 2. **空抽取必须显式识别**。扫描件（无文本层）抽取不报错、只返回空串，绝不能把
//!    空串交给 agent。

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

/// 文档文件大小上限（与附件上限无关，防止工具读到超大文件）。
const MAX_DOCUMENT_BYTES: u64 = 50 * 1024 * 1024;

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
        Ok(raw) => raw
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MAX_CHARS),
        Err(_) => DEFAULT_MAX_CHARS,
    }
}

/// 解析一个十六进制码位（UCD 表里的 token）。
fn parse_hex(token: &str) -> Option<u32> {
    u32::from_str_radix(token.trim(), 16).ok()
}

/// 解析 UCD `EquivalentUnifiedIdeograph.txt` 的行：`SRC ; DST  # comment`，源码位可为
/// 区间形式 `SRC..SRC2`。
///
/// **区间行必须展开**：表里有 5 条（`2E8C..2E8D ; 5C0F` 等），
/// `u32::from_str_radix("2E8C..2E8D", 16)` 是 `Err`，只处理单码位形式会静默丢弃整段
/// 映射；而这些码位 NFKC 并不折叠（各自 fold 到自身），缺陷会原样进入 agent 输出，
/// 与 `⻓` U+2ED3 属同一类问题。
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
            let (Some(src), Some(dst)) = (parts.next(), parts.next()) else {
                continue;
            };
            let Some(dst) = parse_hex(dst).and_then(char::from_u32) else {
                continue;
            };
            match src.split_once("..") {
                // 区间行：`src` 到 `src2` 的每个码位都映射到同一个目标码位。
                Some((a, b)) => {
                    let (Some(a), Some(b)) = (parse_hex(a), parse_hex(b)) else {
                        continue;
                    };
                    for code in a..=b {
                        if let Some(src) = char::from_u32(code) {
                            m.insert(src, dst);
                        }
                    }
                }
                None => {
                    if let Some(src) = parse_hex(src).and_then(char::from_u32) {
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

/// 抽取 PDF 文本层。返回 `(正文, 是否抽到任何非空白文本)`。
///
/// 单页切片走 `lopdf`：`pdf-extract` 没有按页 API（spike §2.4）。
fn extract_pdf(bytes: &[u8], pages: Option<(u32, u32)>) -> Result<(String, bool), ToolsError> {
    let doc = lopdf::Document::load_mem(bytes)
        .map_err(|e| ToolsError::DocumentParse(format!("pdf load failed: {e}")))?;
    let page_count = doc.get_pages().len() as u32;
    if page_count == 0 {
        return Ok((String::new(), false));
    }
    let (from, to) = pages.unwrap_or((1, page_count));
    if from > page_count {
        return Err(ToolsError::DocumentParse(format!(
            "pdf has {page_count} page(s); page {from} does not exist"
        )));
    }

    let mut out = String::new();
    let mut any_text = false;
    for n in from..=to.min(page_count) {
        let page_bytes = single_page_pdf(&doc, n)?;
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
fn single_page_pdf(doc: &lopdf::Document, n: u32) -> Result<Vec<u8>, ToolsError> {
    use lopdf::{Object, dictionary};
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

/// DOCX -> markdown。标题走 `<w:pStyle w:val="Heading1">`，表格输出 markdown 表。
///
/// **两个实测陷阱**（spike §4）：
/// 1. `<w:pStyle .../>` 是自闭合标签，quick-xml 发 `Event::Empty` 而非
///    `Start`；只接 `Start` 会静默丢掉全部标题层级。
/// 2. quick-xml 0.38 用 `BytesText::xml_content()`，`unescape()` 已移除。
fn extract_docx(bytes: &[u8]) -> Result<String, ToolsError> {
    use quick_xml::Reader;
    use quick_xml::events::Event;
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
                    ));
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
    async fn extract(&self, path: &Path, pages: Option<(u32, u32)>) -> Result<String, ToolsError> {
        let size = std::fs::metadata(path).map_err(ToolsError::Io)?.len();
        if size > MAX_DOCUMENT_BYTES {
            return Err(ToolsError::DocumentTooLarge {
                size,
                max: MAX_DOCUMENT_BYTES,
            });
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
                "read {}: no text layer found (无文本层，疑似扫描件); \
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn tool(tmp: &TempDir) -> ReadDocumentTool {
        ReadDocumentTool::new(Arc::new(ToolsContext::new(tmp.path().to_path_buf())))
    }

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../docs/research/fixtures/2026-10-07-document-samples")
            .join(name)
    }

    #[test]
    fn repair_folds_radical_codepoints_to_han() {
        // 这三个码位来自 spike 实测：PDF 的 ToUnicode CMap 把字形映到部首区。
        assert_eq!(repair_cjk("增⻓"), "增长"); // U+2ED3 -> U+957F
        assert_eq!(repair_cjk("第⼆⻚"), "第二页"); // U+2F02/U+2EDA -> U+4E8C/U+9875
        assert_eq!(repair_cjk("⻛险"), "风险"); // U+2EDB -> U+98CE
    }

    /// UCD 表的区间行（`SRC..SRC2 ; DST`）必须展开成逐个码位；这些码位 NFKC 不折叠，
    /// 漏掉就会原样进入 agent 输出（与 `⻓` U+2ED3 同一缺陷类）。
    #[test]
    fn repair_expands_ucd_range_lines() {
        // 下面是 5 条区间行各自的端点。
        assert_eq!(repair_cjk("\u{2E8C}"), "\u{5C0F}"); // 2E8C..2E8D ; 5C0F
        assert_eq!(repair_cjk("\u{2E8D}"), "\u{5C0F}");
        assert_eq!(repair_cjk("\u{2EA4}"), "\u{722B}"); // 2EA4..2EA5 ; 722B
        assert_eq!(repair_cjk("\u{2EA5}"), "\u{722B}");
        assert_eq!(repair_cjk("\u{2EBE}"), "\u{8279}"); // 2EBE..2EC0 ; 8279
        assert_eq!(repair_cjk("\u{2EC0}"), "\u{8279}");
        assert_eq!(repair_cjk("\u{2ECC}"), "\u{8FB6}"); // 2ECC..2ECE ; 8FB6
        assert_eq!(repair_cjk("\u{2ECE}"), "\u{8FB6}");
        assert_eq!(repair_cjk("\u{31D2}"), "\u{4E3F}"); // 31D2..31D3 ; 4E3F
        assert_eq!(repair_cjk("\u{31D3}"), "\u{4E3F}");
    }

    #[test]
    fn repair_leaves_plain_han_untouched() {
        assert_eq!(repair_cjk("季度经营分析报告"), "季度经营分析报告");
    }

    #[test]
    fn parses_page_specs() {
        assert_eq!(parse_pages("3"), Some((3, 3)));
        assert_eq!(parse_pages("1-5"), Some((1, 5)));
        assert_eq!(parse_pages(" 2 "), Some((2, 2)));
        // 非法：0、倒序、空、非数字。
        assert_eq!(parse_pages("0"), None);
        assert_eq!(parse_pages("5-1"), None);
        assert_eq!(parse_pages(""), None);
        assert_eq!(parse_pages("abc"), None);
    }

    #[tokio::test]
    async fn reads_a_pdf_and_reports_missing_text_layer() {
        let tmp = TempDir::new().unwrap();
        // 纯图 PDF（无字体、无文本层）——必须在 fixtures 里，避免测试依赖网络。
        let bytes = std::fs::read(fixture("scanned.pdf")).unwrap();
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
        std::fs::copy(fixture("cn-report.pdf"), tmp.path().join("cn-report.pdf")).unwrap();

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
        std::fs::copy(fixture("cn-report.pdf"), tmp.path().join("cn-report.pdf")).unwrap();

        let result = tool(&tmp)
            .call(serde_json::json!({ "path": "cn-report.pdf", "pages": "2" }))
            .await;
        let text = format!("{result:?}");
        assert!(text.contains("风险提示"), "page 2 content missing: {text}");
        assert!(!text.contains("收入构成"), "page 1 content leaked: {text}");
    }

    #[tokio::test]
    async fn rejects_a_page_beyond_the_document() {
        let tmp = TempDir::new().unwrap();
        std::fs::copy(fixture("cn-report.pdf"), tmp.path().join("cn-report.pdf")).unwrap();

        let result = tool(&tmp)
            .call(serde_json::json!({ "path": "cn-report.pdf", "pages": "99" }))
            .await;
        let text = format!("{result:?}");
        // 越界页必须报错，不能伪装成「扫描件」。
        assert!(text.contains("does not exist"), "got: {text}");
        assert!(!text.contains("扫描"), "must not look like a scan: {text}");
    }

    #[tokio::test]
    async fn truncates_long_documents_and_says_so() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("long.txt"), "甲".repeat(50)).unwrap();
        let tool = ReadDocumentTool {
            ctx: Arc::new(ToolsContext::new(tmp.path().to_path_buf())),
            max_chars: 10,
        };

        let result = tool.call(serde_json::json!({ "path": "long.txt" })).await;
        let text = format!("{result:?}");
        assert!(text.contains("truncated"), "got: {text}");
        assert!(text.contains("showed 10 of 50"), "got: {text}");
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

    /// spike §4 陷阱 1 + 2 的回归测试：自闭合 `<w:pStyle .../>` 必须产出标题层级。
    #[tokio::test]
    async fn reads_docx_keeping_headings_and_tables() {
        let tmp = TempDir::new().unwrap();
        std::fs::copy(fixture("cn-report.docx"), tmp.path().join("cn-report.docx")).unwrap();

        let result = tool(&tmp)
            .call(serde_json::json!({ "path": "cn-report.docx" }))
            .await;
        let text = format!("{result:?}");
        assert!(text.contains("# 季度经营分析报告"), "h1 lost: {text}");
        assert!(text.contains("## 一、收入构成"), "h2 lost: {text}");
        assert!(
            text.contains("| 地区 | 收入（万元） | 同比增长 |"),
            "table header lost: {text}"
        );
        assert!(
            text.contains("| --- | --- | --- |"),
            "table rule lost: {text}"
        );
        assert!(
            text.contains("| 华东 | 3280 | 15.2% |"),
            "table row lost: {text}"
        );
    }

    #[tokio::test]
    async fn reads_html_as_markdown() {
        let tmp = TempDir::new().unwrap();
        std::fs::copy(fixture("report.html"), tmp.path().join("report.html")).unwrap();

        let result = tool(&tmp)
            .call(serde_json::json!({ "path": "report.html" }))
            .await;
        let text = format!("{result:?}");
        assert!(text.contains("收入构成"), "html body missing: {text}");
    }

    #[tokio::test]
    async fn reads_plain_text() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("note.md"), "# 标题\n正文").unwrap();

        let result = tool(&tmp)
            .call(serde_json::json!({ "path": "note.md" }))
            .await;
        assert!(
            matches!(&result.content[0], yi_agent_core::ContentBlock::Text(t) if t.contains("正文"))
        );
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn rejects_an_invalid_pages_spec() {
        let tmp = TempDir::new().unwrap();
        std::fs::copy(fixture("cn-report.pdf"), tmp.path().join("cn-report.pdf")).unwrap();

        let result = tool(&tmp)
            .call(serde_json::json!({ "path": "cn-report.pdf", "pages": "9-2" }))
            .await;
        let text = format!("{result:?}");
        assert!(text.contains("invalid pages spec"), "got: {text}");
    }

    #[tokio::test]
    async fn reports_non_utf8_text_document() {
        let tmp = TempDir::new().unwrap();
        // 0xFF 不在合法 UTF-8 中；无扩展名路径走纯文本分支。
        std::fs::write(tmp.path().join("blob.txt"), [0xff, 0xfe, 0x00]).unwrap();

        let result = tool(&tmp)
            .call(serde_json::json!({ "path": "blob.txt" }))
            .await;
        let text = format!("{result:?}");
        assert!(text.contains("not a UTF-8 text document"), "got: {text}");
    }

    #[test]
    fn name_and_metadata() {
        let tmp = TempDir::new().unwrap();
        let tool = tool(&tmp);
        assert_eq!(tool.name(), "read_document");
        let meta = tool.metadata();
        assert!(meta.read_only);
        assert!(!meta.requires_confirmation);
        assert_eq!(meta.source, ToolSource::Builtin);
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
}
