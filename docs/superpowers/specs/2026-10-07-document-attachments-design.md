# 文档附件（PDF / DOCX 等）读取设计

**目标：** 让用户在桌面端把本地文档（PDF、DOCX、RTF、HTML、TXT、MD、CSV）
附加到一条消息里，agent 按需读取其内容并据此回答。

**状态：** 设计已确认，待转实现计划。

**范围：** L1「只读」——文件进得来、agent 读得到、能答。
**不做** App 内文档渲染、划选/批注、实时协作、拖拽入口、base64 内联、
扫描版 PDF 的 OCR 或整页渲染。

---

## 1. 问题与已核实事实

当前端到端**不支持**任何文件附件。已核实的事实（含实测）：

- **消息只有文本一条通道**：线协议 `Item::UserMessage { id, text }` 是纯
  `String`（`yi-agent-rs/crates/yi-agent-app-server/src/protocol.rs:395`），
  前端 `Item` 联合同构（`desktop/src/lib/protocol.ts`）。
- **非 text block 被静默丢弃**：`extract_prompt` 只拼接
  `input:[{type:"text",text}]`（`server.rs:6067`）。
- **多模态底座已在**：core 有 `ContentBlock::Image`
  （`yi-agent-rs/crates/yi-agent-core/src/message.rs:26`），两个 provider 都已
  映射（`anthropic/types.rs:125`、`openai/types.rs:130`），且 `view_image`
  已把本地图片编码后送进上下文（`yi-agent-tools/src/fs/view_image.rs`）。
- **帧上限 1 MiB**：`MAX_FRAME_BYTES = 1024 * 1024`
  （`app-server/src/protocol.rs:8`）——附件不可能内联进一帧。
- **文件工具受 root 约束**：`resolve_and_check` 拒绝 root 外路径
  （`yi-agent-tools/src/fs/path_util.rs:11`，`PathEscapesRoot`）。
- **bash 沙箱只限写**：策略为 `(allow default)(deny network*)` 加写限制
  （`yi-agent-tools/src/sandbox.rs:147`），读不受限。
- **`<cwd>/.yi-agent/` 是既有约定**：`threads/`（`thread_store.rs:113`）、
  `runtime/`、`preferences.json`（`settings_store.rs:39`）都已在此；根仓库
  `.gitignore` 已含 `.yi-agent/`。
- **工具侧无任何 pdf/docx 依赖**（`yi-agent-tools/Cargo.toml` 核实）。

### 宿主自带能力的实测天花板

在「不打包解析器」前提下实测本机（macOS）：

| 能力 | 结果 |
|---|---|
| `textutil` 转 DOCX / RTF / HTML | 可以（实测通过） |
| `textutil` 转 PDF | **不行**（`Text encoding ... isn't applicable`） |
| `pdftotext` / `pdfinfo` / `qpdf` | **不存在**（poppler 未装） |
| `python3` 读 PDF | **不行**（系统 3.9，无 `Quartz`/`PDFKit`） |
| Spotlight `kMDItemTextContent` | **空**（PDF 均 `(null)`） |
| `sips` 渲染 PDF | 能出 PNG，**但只有第 1 页**（2 页 PDF 实测只产 1 张） |

结论：零依赖下 Office/纯文本类可读，**PDF 文本层读不了**。

## 2. 关键决策（逐条已确认）

1. **入口 v1 = 回形针 + 路径兜底**：原生文件选择器（复用已装的
   `@tauri-apps/plugin-dialog`，同 `desktop/src/App.tsx:147`）+ 用户直接给路径；
   拖拽推迟（与 iOS/远端 ws 路线不兼容）。
2. **附件只传路径，不作为内容**：`turn/start` 带附件 block，内容不进 prompt。
3. **agent 按需读**：新增 `read_document` 工具，由 agent 决定读多少/读哪几页
   （与 `read`/`view_image`/`grep` 同构）。避免「预抽取灌爆上下文」与
   「compact 吞掉附件内容」两个已知代价。
4. **PDF 破一次例**：只引入**纯 Rust 文本抽取 crate**（非渲染引擎、无系统
   依赖），让 PDF 文本层可读；不引入整页渲染，扫描版显式告知读不了。
5. **附件复制进工作区**：选文件即拷入 `<thread_cwd>/.yi-agent/attachments/`，
   三条读法（`read_document`/`view_image`/bash）行为一致，
   `resolve_and_check` 安全不变量一字不改。
6. **删 thread 时清理附件**（用户补充要求，见 §6）。

## 3. 协议改动

三处，均向后兼容：

**(1) `turn/start` 的 `input` 新增 block 变体：**

```json
{ "type": "attachment", "path": "<绝对路径>", "name": "报告.pdf", "mime": "application/pdf" }
```

`path` 是用户在文件选择器里选中的绝对路径（服务端负责复制）。

**(2) 服务端复制并替换为 in-root 路径**：`prepare_turn` 阶段把每个附件复制到

```
<thread_cwd>/.yi-agent/attachments/<thread_id>/<sha256[0:8]>-<安全化文件名>
```

**(3) `Item::UserMessage` / `UserInterjection` 新增 `attachments`：**

```rust
// protocol.rs
#[serde(default, skip_serializing_if = "Vec::is_empty")]
attachments: Vec<Attachment>,

pub struct Attachment { name: String, path: String, mime: Option<String>, size: u64 }
```

`path` 为 in-root 相对路径。`skip_serializing_if` 保证旧客户端/旧数据兼容。
前端 `desktop/src/lib/protocol.ts` 同步加同名字段（可选）。

## 4. agent 侧如何拿到路径（零 core 改动）

服务端把附件清单以**确定性格式拼进发给 agent 的 prompt**；而 `Item.text`
仍是用户原话（气泡只显示原话 + chip，不显示这段注入）。core 的
`Message`/`ContentBlock` **一行不改**。

```
用户消息:
<用户原话>

附件（可用 read_document 读取）:
- 报告.pdf (application/pdf, 123456 bytes) -> .yi-agent/attachments/<tid>/a1b2c3d4-报告.pdf
```

`extract_prompt`（`server.rs:6067`）改为识别 `type:"attachment"` 并记录；
「空 input」校验放宽，允许**只发附件、不打字**。

## 5. 新工具 `read_document`

- **签名**：`read_document(path, pages?)`；`pages` 形如 `"3"` 或 `"1-5"`，
  缺省整篇。
- **分派**：
  - `.pdf` → 纯 Rust 文本抽取（`pdf-extract`；按页用 `lopdf`）,**抽出的文本必须经字符修复**：`NFKC` + UCD `EquivalentUnifiedIdeograph` 映射，否则中文会落在部首码位（`⻓` U+2ED3 而非 `长` U+957F）导致检索/引用系统性失效（见 [spike](../research/2026-10-07-pdf-text-extraction-spike.md)）；该 UCD 表（410 行，Unicode 许可）随仓库 vendor；
  - `.docx` → zip + XML 抽取，**保留标题层级、表格、列表**并输出为 markdown
    （不压成一坨纯文本，否则表格会退化成无列的一串词）；
  - `.rtf` / `.html` / `.txt` / `.md` / `.csv` → 文本抽取。
- **元数据**：与 `view_image` 同构——`source: Builtin`、`read_only: true`、
  `requires_confirmation: false`。
- **边界**：无文本层的 PDF **明确返回**「pages X-Y 无文本层，疑似扫描件」，
  绝不返回空串让 agent 瞎猜。
- **预算**：设长度/页数预算，超预算返回前 N 页并提示「继续请指定 pages」，
  与 `view_image` 的体积预算（`view_image.rs:15`）同一思路。
- **刻意不 shell 调 `textutil`**：它是 macOS 独占，而 `yi-agent-tools` 需跨
  平台（Linux 打包在路线图上）。

## 6. 安全与清理

**(a) root 不变量不改**：复制进 workspace 后所有读取都在 root 内，
不引入任何白名单例外。

**(b) 文件名安全化**：剥离路径分隔符与控制字符，防目录穿越。

**(c) 按 thread 隔离存储**（**不得**用全内容哈希全局去重）：

```
<thread_cwd>/.yi-agent/attachments/<thread_id>/<sha256[0:8]>-<安全化文件名>
```

理由：全局哈希去重会让两个 thread 共享同一文件，删掉其一即误删另一方的附件。
按 thread 隔离后「删除 = 整个目录删除」，**无需任何引用计数**。thread 内重复
附加同一文件仍按哈希去重（同目录内）；跨 thread 各存一份，以少量磁盘冗余换掉
一整类误删 bug。

目录名用 `thread_id` 本身：`valid_id` 已限定 `[A-Za-z0-9_-]{1,128}`
（`thread_store.rs:523`），可安全充当路径分量，无需再转义。

**(d) 删 thread 时清理**（用户补充要求）：

- `ThreadStore` 的 root 是 `<cwd>/.yi-agent/threads/`（`thread_store.rs:113`），
  按位置挂在工作区；`thread/delete` 分支持有的正是该实例，故可**由 root 反推
  工作区**，给 `ThreadStore` 加一个 `root()` 访问器即可。**不改**
  `store_lookup` 的签名——`server.rs:753-757` 的注释明确警告「delete 分支里
  拿不到 cwd」，不要为此改签名。
- 清理动作 = `rm -rf .yi-agent/attachments/<thread_id>`，放在既有
  `interrupt_and_wait_for_persist` **之后**，避免与 driver 落盘竞态。
- **best-effort**：目录不存在（工作区被挪走）不使删除失败，记日志放行。
- 删完若 `attachments/` 为空，连空目录一并移除（与 `settings_store` 清理空
  `.yi-agent` 目录的既有做法一致），不在用户仓库里留残壳。

**(e) `thread/clear` 不清理附件**：`/clear` 截断日志但 thread 仍在、会话可
继续使用；只有 `thread/delete` 才清理附件。

**(f) 既有现状声明**：app-server 本就在用户项目里写 `<cwd>/.yi-agent/`
（`threads/`、`runtime/`、`preferences.json`），附件只是加入这个既有约定。
文档中写明用户仓库应忽略 `.yi-agent/`。

## 7. 前端

- 输入框加回形针按钮，走 `@tauri-apps/plugin-dialog`（`multiple: true` +
  扩展名过滤）。
- 选中后渲染可删除 chip（文件名 + 大小 + 类型）；发送时组装
  `input = [attachment..., text]`。
- 用户气泡内展示附件 chip，点击用既有 opener 交给系统默认 App 打开。
- 本地预检：超上限或路径不可读，**发送前**就地提示；服务端拒绝走既有错误横幅。

## 8. 默认值

- 附件**仅 `turn/start` 支持**；`turn/interject` 不带附件。
- 一条消息允许**多个附件**。
- 单文件上限 **50 MiB**，经 `YI_AGENT_ATTACHMENT_MAX_BYTES` 可调（沿用
  `view_image` 的 `YI_AGENT_VIEW_IMAGE_MAX_BASE64_BYTES` 命名风格，
  `view_image.rs:18`）。

## 9. 风险与前置 spike（已完成，风险解除）

**最大风险已 spike 验证通过**：真实中文 PDF（Chrome 生成、含表格、2 页）抽取可用，
裸抽取器的中文系统性错误**已定位并修复**。详见
[spike 报告](../research/2026-10-07-pdf-text-extraction-spike.md)。

- 候选：`pdf-extract 0.12.1` + `unicode-normalization 0.1.25`；按页用 `lopdf 0.45`。
- **必须做的字符修复**（见 §5）：裸抽取会把 25 个中文丢到 Unicode 部首码位
  （`⻓` U+2ED3 而非 `长` U+957F 等），导致 `收入构成`/`风险提示` 等检索全 `false`。
  源头是 PDF 自带 ToUnicode CMap 就映到部首码位（非抽取器 bug，已解 CMap 证实）。
- `NFKC + UCD EquivalentUnifiedIdeograph 映射`修复后残余归零，全部短语命中。
- 按页隔离实测干净（第 1 页 215 字符 / 第 2 页 58 字符）。
- 依赖符合「不打包渲染引擎」：无 mupdf/pdfium；`zip` 须收紧为
  `default-features = false, features = ["deflate"]` 以避开 `zstd-sys`。

故**不退回** §2.4 的备选，PDF 文本层按承诺可读。

## 10. 测试判据

- **Rust 单测**：PDF 有/无文本层、DOCX 表格与标题保留、超预算截断、root 外
  路径拒绝、文件名安全化。
- **app-server**：`turn/start` 带 attachment → 确实发生复制 + item 携带
  attachments + prompt 含附件清单；`extract_prompt` 不再静默丢附件；
  `thread/delete` → 对应 `attachments/<thread_id>/` 被移除；
  `thread/clear` → 附件**保留**。
- **前端**：chip 增删、blocks 组装、attachments 渲染。
- **回归**：`cargo test -p yi-agent-app-server -p yi-agent-tools` +
  `cd desktop && npx vitest run && npx tsc --noEmit && npm run build`。
- **依赖**（已由 spike 定案，进 `yi-agent-tools/Cargo.toml`）：`pdf-extract 0.12`、
  `lopdf 0.45`、`unicode-normalization 0.1`、`zip 8`（**`default-features = false,
  features = ["deflate"]`**，否则会拉 `zstd-sys` C 代码）、`quick-xml 0.38`。
  复制/哈希逻辑落在 app-server，`sha2 = "0.10"` **已在该 crate 的依赖里**
  （`yi-agent-app-server/Cargo.toml:34`），无需新增。
- **实测陷阱**（务必写进实现）：quick-xml 0.38 用 `.xml_content()` 而非
  `.unescape()`；`<w:pStyle .../>` 是自闭合标签、发 `Event::Empty`，只处理
  `Event::Start` 会静默丢掉全部标题层级；无文本层 PDF 抽取**不报错**、返回空，
  必须显式判定。详见 spike 报告 §4。

## 11. 已知限制

- 扫描版 PDF 与整页 PDF 渲染 v1 不支持。
- 附件复制会占用工作区磁盘；清理绑定在 `thread/delete`，工作区内的孤立
  `attachments/` 不做后台 GC。
- 用户在会话存续期间删除工作区，历史回放中的附件 chip 仍在、打开会失败。
