# Desktop GUI P1 后半 — Markdown 富渲染 + 代码高亮 + 用量与成本面板 设计

- 日期:2026-09-27
- 状态:已确认,待实现
- 参考:`docs/superpowers/plans/2026-09-26-desktop-gui-design.md` §11(P1 路线图)
- 前置:`feat/desktop-gui-p1-persistence` 已合入 main(会话持久化 + 历史侧栏)

## 1. 背景与目标

P1 路线图(design §11)包含:会话持久化、历史侧栏、**markdown 富渲染 + 代码高亮**、
**reasoning/thinking 展示**、**用量与成本面板**。前两项已交付。

本轮交付后两项中的两项:

1. **agentMessage 富渲染**:把 `ChatView` 里 `whitespace-pre-wrap` 的纯文本渲染
   换成 markdown 渲染 + 代码块语法高亮。
2. **用量与成本面板**:协议层补上 cache token;`StatusBar` 的用量区可点击展开详情
   面板,显示 input / output / cache read / cache write 与**估算成本**。

**reasoning/thinking 展示不在本轮**:core 的 `ProviderEvent`(`provider.rs:42-63`)
与 `AgentEvent`(`agent.rs:197-279`)当前**没有任何 thinking 变体**,provider 也未解析
thinking block。要展示 reasoning 必须先给 core + provider + protocol + translate +
前端逐层加支持,是跨 4 层的大改,单独一轮处理。

## 2. 非目标

- reasoning / thinking 展示(见 §1)。
- LaTeX / 数学公式渲染。
- diff 视图、文件树(P2)。
- 消息虚拟列表(当前直接渲染 `items`,数量级不构成瓶颈)。
- 累计成本(跨 turn 累加)与预算告警——语义与 resume 去重未定义,留作后续。
- 配置界面改 model / api key。

## 3. 关键决策摘要

| 决策点 | 选择 | 理由 |
|---|---|---|
| markdown 渲染 | `react-markdown` | React 组件式、默认安全(不渲染原始 HTML)、与 remark/rehype 生态直连 |
| GFM 扩展 | `remark-gfm` | 表格/删除线/任务列表/自动链接,agent 输出常见 |
| 代码高亮 | `rehype-highlight`(highlight.js) | 同步、可直接插入 rehype 管道、无需 async highlighter |
| markdown 样式 | `@tailwindcss/typography`(`prose prose-invert`) | 少写自定义 CSS,与 Tailwind v4 `@plugin` 契合 |
| 外链行为 | `tauri-plugin-opener` | 点击链接在系统浏览器打开,避免 webview 被导航走 |
| 流式性能 | `React.memo` 按 `text` 记忆 | 只有正在增长的最后一条 item 重解析 |
| 成本估算 | 内置定价表 + 未知模型返回 `null` | 展示为 `—`,不猜 |
| 成本口径 | 当前上下文用量快照 | 累计成本语义未定,不做 |

## 4. 新增依赖与版本

`desktop/package.json` dependencies:

| 包 | 用途 |
|---|---|
| `react-markdown` | markdown → React |
| `remark-gfm` | GFM 语法 |
| `rehype-highlight` | 代码块高亮(基于 highlight.js) |
| `highlight.js` | 提供 `github-dark.css` 主题(只引 CSS) |
| `@tauri-apps/plugin-opener` | 外链用系统浏览器打开 |

devDependencies:

| 包 | 用途 |
|---|---|
| `@tailwindcss/typography` | `prose` 排版 |
| `jsdom` | 组件测试的 DOM 环境(仅 `MarkdownText.test.tsx` 文件级启用) |
| `@testing-library/react` | 组件渲染断言 |

`desktop/src-tauri/Cargo.toml` 增加 `tauri-plugin-opener = "2"`。

> 依赖版本以实现时 `npm install <pkg>` 解析到的实际版本为准,锁进 `package-lock.json`。

## 5. markdown 渲染设计

### 5.1 组件

新增 `desktop/src/components/MarkdownText.tsx`:

```tsx
import { memo } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeHighlight from "rehype-highlight";
import "highlight.js/styles/github-dark.css";

/**
 * agentMessage 的 markdown 渲染。按 `text` 记忆:流式时只有正在增长的
 * 那条消息会重解析,历史消息命中 memo 不重渲染。
 *
 * 不启用 rehype-raw —— react-markdown 默认丢弃原始 HTML,天然防 XSS。
 */
export const MarkdownText = memo(
  function MarkdownText({ text }: { text: string }) {
    return (
      <div className="prose prose-invert max-w-none prose-pre:bg-neutral-900">
        <ReactMarkdown
          remarkPlugins={[remarkGfm]}
          rehypePlugins={[rehypeHighlight]}
          components={{ a: ExternalLink }}
        >
          {text}
        </ReactMarkdown>
      </div>
    );
  },
  (prev, next) => prev.text === next.text,
);
```

`ExternalLink` 为自定义 `a` 组件:`onClick` 里 `e.preventDefault()`,调用
`@tauri-apps/plugin-opener` 的 `open(href)`;`href` 缺失或非 http(s) 时不拦截。
非 Tauri 环境(浏览器里跑 vitest/开发)下 `open` 会 reject,组件需 `catch` 后
静默,保证不崩。

### 5.2 接线

`ChatView.tsx:41-49` 的 `agentMessage` 分支由:

```tsx
<div className="... font-mono text-sm whitespace-pre-wrap ...">{item.text}</div>
```

改为 `<MarkdownText text={item.text} />`。`userMessage` 保持纯文本气泡
(用户输入不按 markdown 渲染,避免误格式化)。

### 5.3 样式接入(Tailwind v4)

- `desktop/src/index.css` 增加 `@plugin "@tailwindcss/typography";`
  (Tailwind v4 的插件引入语法)。
- 高亮主题:直接 `import "highlight.js/styles/github-dark.css"`,在 `MarkdownText.tsx`
  顶部引入。该 CSS 定义 `.hljs` 及各类 token 颜色,配合 `prose-pre:bg-neutral-900`
  形成深色代码块。

### 5.4 流式与边界

- 流式中未闭合的代码围栏会短暂渲染为普通段落,闭合后自动修正;可接受。
- 大代码块逐字重高亮有开销(每条 delta 重跑 highlight.js)。缓解:见 §10 风险。
- 空字符串渲染为空,不产生额外 DOM 噪声。

## 6. 用量与成本面板设计

### 6.1 数据流

```
core TokenUsage { input_tokens, output_tokens,
                  cache_creation_input_tokens, cache_read_input_tokens }   (provider.rs:33-40)
   ↓ AgentEvent::Usage
translate.rs  →  thread/tokenUsage/updated { input_tokens, output_tokens,
                                             cache_creation_input_tokens,
                                             cache_read_input_tokens }
   ↓ 通知
session.ts    →  session.usage = { model, input, output, cacheRead, cacheWrite }
   ↓ props
StatusBar     →  紧凑显示 + 点击展开 UsagePanel
```

### 6.2 协议变更(`yi-agent-app-server/src/protocol.rs`)

`Notification::ThreadTokenUsageUpdated` 增加两个字段:

```rust
ThreadTokenUsageUpdated {
    thread_id: String,
    model: String,
    input_tokens: u32,
    output_tokens: u32,
    #[serde(default)]
    cache_creation_input_tokens: u32,   // 写入 cache 的 token(Anthropic cache write)
    #[serde(default)]
    cache_read_input_tokens: u32,       // 命中 cache 读取的 token
},
```

`#[serde(default)]` 保证与旧帧的兼容(缺字段按 0)。

### 6.3 翻译层(`translate.rs:236-243`)

`AgentEvent::Usage { model, usage }` 分支把 `usage.cache_creation_input_tokens` /
`usage.cache_read_input_tokens` 一并写入通知(当前被丢弃)。

### 6.4 持久化(`thread_store.rs:28-33`)

`TurnUsage` 增加两个字段,同样 `#[serde(default)]`:

```rust
pub struct TurnUsage {
    pub model: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
    #[serde(default)]
    pub cache_read_input_tokens: u32,
}
```

旧的 `<thread_id>.jsonl`(无这两字段)仍可反序列化;`thread/resume` 回放用量时
(`server.rs` 的 resume 分支)一并带上,前端 resume 后用量面板即完整。

### 6.5 前端类型与状态

`desktop/src/lib/protocol.ts`:

```ts
export interface Usage {
  model: string;
  input: number;
  output: number;
  cacheRead: number;
  cacheWrite: number;
}
```

`desktop/src/lib/session.ts` 的 `thread/tokenUsage/updated` 分支把
`cache_read_input_tokens` / `cache_creation_input_tokens` 映射为
`cacheRead` / `cacheWrite`。

### 6.6 定价表与成本估算(`desktop/src/lib/pricing.ts`)

```ts
/** 单价:USD / 1M tokens。按模型 id 前缀匹配,取最长匹配。 */
interface Price { input: number; output: number; cacheRead: number; cacheWrite: number; }

const PRICES: Array<[prefix: string, price: Price]> = [
  ["claude-opus-4",   { input: 15,   output: 75, cacheRead: 1.5,  cacheWrite: 18.75 }],
  ["claude-sonnet-4", { input: 3,    output: 15, cacheRead: 0.3,  cacheWrite: 3.75 }],
  ["claude-3-5-sonnet",{ input: 3,   output: 15, cacheRead: 0.3,  cacheWrite: 3.75 }],
  ["claude-3-5-haiku",{ input: 0.8,  output: 4,  cacheRead: 0.08, cacheWrite: 1 }],
  ["gpt-4o-mini",     { input: 0.15, output: 0.6, cacheRead: 0.075, cacheWrite: 0 }],
  ["gpt-4o",          { input: 2.5,  output: 10, cacheRead: 1.25, cacheWrite: 0 }],
  ["gpt-4.1",         { input: 2,    output: 8,  cacheRead: 0.5,  cacheWrite: 0 }],
  ["o3",              { input: 10,   output: 40, cacheRead: 2.5,  cacheWrite: 0 }],
  ["o1",              { input: 15,   output: 60, cacheRead: 7.5,  cacheWrite: 0 }],
];

/** 返回估算成本(USD);未知模型返回 null(UI 显示 "—")。 */
export function estimateCost(u: Usage): number | null;
```

匹配规则:遍历 `PRICES`,取 `u.model.startsWith(prefix)` 且 `prefix` 最长的项。
成本 = `(input*input + output*output + cacheRead*cacheRead + cacheWrite*cacheWrite) / 1e6`。

> 单价以实现时官方定价为准;实现任务需核对并在此表留下注释标注核对日期。

### 6.7 UI

- `StatusBar.tsx`:用量区从纯文本 `{input} in / {output} out` 改为一个按钮,点击
  切换 `UsagePanel` 展开态(本地 `useState`)。
- 新增 `desktop/src/components/UsagePanel.tsx`:浮层,列出

  | 项 | 值 |
  |---|---|
  | Input | `{input} tokens` |
  | Output | `{output} tokens` |
  | Cache read | `{cacheRead} tokens` |
  | Cache write | `{cacheWrite} tokens` |
  | 模型 | `{model}` |
  | 估算成本 | `≈ $0.0123` 或 `—` |

  成本格式:`< $0.01` 时显示 `< $0.01`,否则保留 4 位小数。
- 面板展示**当前上下文用量快照**(input = 上下文规模,output = 最近一次响应)及其成本。

## 7. Tauri 外链能力

- `desktop/src-tauri/Cargo.toml` 加 `tauri-plugin-opener = "2"`。
- `desktop/src-tauri/src/lib.rs` 在 builder 上 `.plugin(tauri_plugin_opener::init())`。
- `desktop/src-tauri/capabilities/default.json` 的 `permissions` 增加
  `"opener:default"`(或精确到 `"opener:allow-open-url"`)。
- 前端 `import { open } from "@tauri-apps/plugin-opener";`

## 8. 安全

- **不启用 `rehype-raw`**:react-markdown 默认不渲染 markdown 中的原始 HTML,
  `<script>` / `onerror` 等被丢弃。这是本设计防 XSS 的核心。
- 外链仅允许 `http:` / `https:` 前缀交给 opener,其它协议(如 `javascript:`)不拦截、
  交由默认行为(即不打开)。
- 定价表是静态常量,无注入面。

## 9. 测试策略

### 9.1 Rust(Tier 0,mock)

- `protocol.rs`:serde 往返,断言 `thread/tokenUsage/updated` 新字段序列化/反序列化;
  旧 JSON(无新字段)反序列化后两字段为 0。
- `translate.rs`:`Usage` 事件 → 通知,断言 cache 两字段透传。
- `thread_store.rs`:`TurnUsage` 旧格式行可加载(字段为 0);新字段往返一致。

### 9.2 前端纯逻辑(vitest,node env)

- `pricing.test.ts`:
  - 已知模型(claude-sonnet-4)按四项加权算出预期成本;
  - 未知模型返回 `null`;
  - 前缀最长匹配(如 `gpt-4o-mini` 不误匹配 `gpt-4o`)。
- `session.test.ts`:`thread/tokenUsage/updated` 带 cache 字段时 `usage` 正确映射;
  缺字段时 `cacheRead`/`cacheWrite` 为 0。

### 9.3 前端组件(vitest,jsdom,文件级)

- `MarkdownText.test.tsx`(文件头 `/** @vitest-environment jsdom */`):
  - 围栏代码块渲染出 `code`(带 `hljs` class);
  - GFM 表格渲染出 `<table>`;
  - 原始 HTML `<script>alert(1)</script>` **不**产生 `<script>` 元素(防 XSS 回归);
  - 相同 `text` 重复渲染命中 memo(可用引用相等或渲染次数断言,若不可靠则省略该断言)。

> 全局 vitest 仍是 node env;仅该文件通过文件级注释启用 jsdom,不改 `vite.config.ts`。

### 9.4 手工冒烟

`npm run tauri dev` → 发一条会返回 markdown 的 prompt → 代码块高亮正确 →
点外链在系统浏览器打开 → 展开用量面板看到 token 明细与成本。

## 10. 风险与缓解

| 风险 | 缓解 |
|---|---|
| 流式逐字重跑 highlight.js 造成卡顿 | `React.memo` 限制重渲染范围到最后一条;若仍卡,后续可对未闭合围栏跳过高亮(本轮不做) |
| 定价表过期 / 不准 | 表内注释标注核对日期;未知模型显示 `—` 而非猜测;成本标注"估算" |
| `tauri-plugin-opener` 打包/权限配置踩坑 | capability 精确到 `opener:allow-open-url`;手工冒烟覆盖点链接 |
| 旧 `.jsonl` 反序列化失败 | 新字段全部 `#[serde(default)]`;专门加旧格式加载测试 |
| react-markdown 大版本 API 变动 | 锁 `package-lock.json`;组件测试覆盖核心渲染路径 |

## 11. 成功判据

- agent 返回的 markdown 在 GUI 中渲染为标题/列表/表格,代码块有语法高亮。
- markdown 中的原始 `<script>` 不被执行(组件测试断言)。
- 点击消息里的 http(s) 链接在系统浏览器打开。
- `StatusBar` 用量区可点击展开面板,显示 input/output/cache read/cache write 与估算成本;
  未知模型成本显示 `—`。
- `cargo test -p yi-agent-app-server` 全绿;`npx vitest run` 全绿;`npm run build` 通过。
- 现有 CLI e2e 不回归(本轮只加字段,不改行为)。
