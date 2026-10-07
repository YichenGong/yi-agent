# Spike：纯 Rust 中文 PDF / DOCX 文本抽取（2026-10-07）

**问题：** 文档附件设计（`docs/superpowers/specs/2026-10-07-document-attachments-design.md`）
的 §9 把「中文 PDF 纯 Rust 抽取是否可用」列为全案最大风险，要求在写实现计划前先验证。

**结论：可行，但必须带一步字符修复。** 裸抽取器会把中文丢到 Unicode「部首」码位；
加 NFKC + UCD `EquivalentUnifiedIdeograph` 映射后完全修复。推荐按原设计（PDF 文本层可读、
DOCX 保留标题/表格）推进，不退回备选。

---

## 1. 方法

- **样本**：用 Chrome headless（`--print-to-pdf`）从 HTML 生成真实中文 PDF
  （`spike/samples/cn-report.pdf`，271 KB，2 页，含 3 列表格、标题、多段中文）。
  同一内容另造最小 DOCX（zip + XML）用于 DOCX 路。
- **扫描件样本**：`sips` 把渲染页转成纯图 PDF（`/Font` 计数为 0）验证无文本层行为。
- **候选**：`pdf-extract 0.12.1`（已内置 `lopdf`）+ `unicode-normalization 0.1.25`；
  DOCX 用 `zip 8.6.0`（仅 deflate）+ `quick-xml 0.38.4`。
- 探针：`spike/probe`（scratch，不入版本控制）。

## 2. 结果

### 2.1 PDF 结构：可用

两页全部抽出；表格以空格分隔的文本行保留（`华东 3280 15.2%`），数字与百分号精确。

### 2.2 中文正确性：裸抽取**系统性错误**（关键发现）

裸 `pdf-extract` 输出中 **25 个字符**落在 Unicode「部首」区块，而非标准汉字：

| 抽出的码位 | 应为 | 例 |
|---|---|---|
| `⻓` U+2ED3（CJK 部首补充） | `长` U+957F | `增⻓` / `同⽐增⻓` |
| `⻚` U+2EDA | `页` U+9875 | `第⼆⻚` |
| `⻛` U+2EDB | `风` U+98CE | `⻛险提示` |
| 康熙部首区（U+2F00–U+2FDF） | 对应汉字 | `收⼊` / `⼀、` / `⼆、` |

后果：`text.contains("收入构成")`、`contains("风险提示")`、`contains("毛利率")`
全部为 `false`——**agent 若按原样读取，中文检索与原文引用会系统性失效**。

**归因（已定位到 PDF 源头，不是抽取器的锅）：** 解压该 PDF 的 ToUnicode CMap，
里面写的就是 `<2ED3>`（`⻓`），**没有** `<957F>`（`长`）；`2EDA`、`2EDB` 同理，而
`957F`/`9875`/`98CE` 在所有 CMap 中出现 0 次。即生产工具（此处 Chrome）把字形映到了
部首码位；`pdf-extract` 只是忠实还原。

### 2.3 修复：NFKC + `EquivalentUnifiedIdeograph` 表

- **NFKC 单独不够**：它折叠了康熙部首区（U+2F00–U+2FDF），但**不折叠 CJK 部首补充区
  （U+2E80–U+2EFF）**，`⻓`/`⻚`/`⻛` 仍残留。
- **补上 UCD 表后归零**：`unicode.org/Public/UCD/latest/ucd/EquivalentUnifiedIdeograph.txt`
  （410 行，Unicode 许可）正好定义 `2ED3 ; 957F`、`2EDA ; 9875`、`2EDB ; 98CE`、
  `2F0A ; 5165` 等映射。
- 组合后：**残余部首字符 25 → 0**，全部探针短语命中（`收入构成`/`同比增长`/`毛利率`/
  `研发投入`/`供应链波动`/`第二页：风险提示`），表格数字原样保留。
- 一处预期副作用：NFKC 把全角标点规范成 ASCII（`：`→`:`），属正常且无害。

### 2.4 按页抽取：可行（spec 的 `pages` 参数）

`pdf-extract` 无按页 API，但用 `lopdf` 可行：`Document::get_pages()` 拿页号 →
复制全部对象 + 新建单页 `Catalog`/`Pages` → `save_to` 得单页 PDF → 交抽取器。

实测 2 页样本：**第 1 页 215 字符**（含 `收入构成`/`3280`，不含第 2 页内容）；
**第 2 页 58 字符**（含 `风险提示`/`汇率波动`，不含表格）。隔离干净，可支撑 `pages:"1-5"`。

### 2.5 扫描件（无文本层）：返回空，须显式识别

纯图 PDF（0 字体）抽取**不报错**，只返回空/空白文本。故 `read_document` 必须检测
`text.trim().is_empty()` 并返回「pages X-Y 无文本层，疑似扫描件」——**绝不能把空串
交给 agent**。这与 spec §5 的边界承诺一致。

### 2.6 DOCX：可用

最小 DOCX（标题 + 三行表格 + 中文段落）实测输出：

```markdown
# 季度经营分析报告

本报告总结了第三季度的经营情况，总收入较上季度增长百分之十二。

## 一、收入构成

| 地区 | 收入（万元） | 同比增长 |
| --- | --- | --- |
| 华东 | 3280 | 15.2% |
| 华北 | 1975 | 8.7% |
```

标题层级（`<w:pStyle w:val="Heading1">` → `#`）与表格（markdown + 分隔行）都保住，
中文无乱码（DOCX 走 XML，不经 CMap，**没有** PDF 那类部首问题）。

## 3. 依赖与打包（对「不打包渲染引擎」约束的核对）

- **无渲染引擎**：依赖中无 mupdf / pdfium，符合原约束。
- **`zip` 必须收紧特性**：默认特性会拉 `zstd-sys`（C 代码，经 `cc`）与 bzip2/`zopfli`。
  用 `default-features = false, features = ["deflate"]` 后，`zstd`/`-sys` 全部消失
  （只余纯 Rust 的 `zopfli` 作为 deflate 后端），覆盖 DOCX 自身只需 DEFLATE 的场景。
- **`lopdf` 的 `core-foundation-sys`**：来自 `lopdf → chrono → iana-time-zone`，
  是 macOS target-gated 的 FFI 声明，**Linux 上不编译**，不构成跨平台阻塞。
- **`EquivalentUnifiedIdeograph.txt` 需随仓库 vendor**（整表 410 行、Unicode 许可，
  文件头自带许可声明），不放运行时下载。

## 4. 给实现者的坑（实测踩到）

1. **quick-xml 0.38 移除了 `BytesText::unescape()`**，改用 `.xml_content()`
   （`quick-xml-0.38.4/src/events/mod.rs:638`）。
2. **`<w:pStyle .../>` 是自闭合标签**，quick-xml 发 `Event::Empty` 而非 `Event::Start`；
   只处理 `Start` 会**静默丢掉全部标题层级**（踩过：输出无 `#`）。两者都要接。
3. `pdf-extract` 无按页 API，按页切片走 `lopdf`（见 §2.4）。
4. 抽取空文本不报错，必须显式判定（见 §2.5）。

## 5. 对设计的回填

- spec §9 的风险**解除**，改为记录本 spike 结论。
- spec §5 新增一条硬要求：**PDF 文本必须经 `NFKC + EquivalentUnifiedIdeograph` 修复**
  （否则中文检索/引用系统性失效）；并 vendor 该 UCD 表。
- `read_document` 的空抽取判定与「疑似扫描件」提示，由 §2.5 确认为必做。
