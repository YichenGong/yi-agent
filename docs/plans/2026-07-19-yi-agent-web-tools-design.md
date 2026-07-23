# yi-agent Web Tools Implementation Design

**Date:** 2026-07-19
**Status:** Approved (brainstormed 2026-07-19)
**Scope:** `yi-agent-rs/crates/yi-agent-tools`（新增 `web/` 模块）

## Goal

在 `yi-agent-tools` 中新增 2 个 Web 工具,覆盖 coding agent 的网络信息获取能力:

| 工具 | 作用 |
|---|---|
| `WebFetch` | 抓取 URL 内容,HTML→Markdown 转换,截断后返回 |
| `WebSearch` | 通过搜索引擎查询关键词,返回结果列表 |

## Decisions (brainstorming 结论)

| # | 决策点 | 选择 | 理由 |
|---|---|---|---|
| 1 | 工具范围 | WebFetch + WebSearch | 覆盖查文档/搜错误信息场景;与 Claude Code 对齐 |
| 2 | HTML 处理 | HTML → Markdown(html2md crate) | LLM 读 markdown 效率高,token 少 |
| 3 | 搜索引擎架构 | WebSearchProvider trait + 多实现 | 扩展新引擎只加一个 struct |
| 4 | 首批引擎 | Bocha(中国境内 AI 搜索) | 中国可用,专为 LLM 优化;其他引擎通过 trait 扩展 |
| 5 | 调用策略 | 单主引擎(构造时指定) | YAGNI;多引擎聚合当前无需求 |
| 6 | HTTP 客户端 | reqwest(与 yi-agent-llm 一致) | 复用经验,rustls-tls,支持流式 |
| 7 | HTML→MD 库 | html2md crate | 专为 HTML→MD 设计,一行转换 |
| 8 | API key 管理 | 读环境变量(BOCHA_API_KEY) | 用户偏好;库从环境变量取 key |
| 9 | WebFetch 行为 | 纯抓取,返回 markdown(无 prompt 参数) | 符合库的定位,不引入 AI 处理逻辑 |
| 10 | WebFetch 截断 | 100KB(与 BashTool 一致) | 防塞爆 LLM 上下文 |
| 11 | SSRF 防护 | 不限制(库的职责) | 安全责任在调用方(CLI);库不耦合安全策略 |
| 12 | 请求配置 | 60s 超时 + 无限重定向 | 网页可能多重重定向,60s 给慢网站留余量 |
| 13 | 测试 | wiremock mock server | 纯本地,CI 可跑,不消耗 API 配额 |
| 14 | 注册 API | 扩展现有 register_builtin_tools | 与现有模式一致,调用方一行注册 |
| 15 | WebSearch count 默认值 | 25 | 用户指定 |

## Architecture

### 模块结构

```
yi-agent-rs/crates/yi-agent-tools/
├── Cargo.toml                          # 新增 reqwest, html2md 依赖
└── src/
    ├── lib.rs                          # register_builtin_tools 加 WebFetchTool + WebSearchTool
    ├── context.rs                      # 不变
    ├── error.rs                        # 新增 Web 相关 error variants
    ├── fs/                             # 不变
    ├── shell/                          # 不变
    └── web/
        ├── mod.rs                      # pub mod fetch/search/provider/bocha; pub use ...
        ├── fetch.rs                    # WebFetchTool + impl Tool
        ├── search.rs                   # WebSearchTool + impl Tool
        ├── provider.rs                 # WebSearchProvider trait + SearchResult struct
        └── bocha.rs                    # BochaSearchProvider + impl WebSearchProvider
```

### 数据流

**WebFetch 数据流:**

```
LLM 调 WebFetchTool(args: {url, max_length?})
  → 校验 URL scheme(http/https)
  → reqwest::Client (60s timeout, unlimited redirects)
      .get(url)
      .header(User-Agent, "yi-agent/0.1.1")
      .send()
  → 检查 Content-Type
      text/html  → 走 html2md 转换
      text/plain → 直接返回
      application/json → 直接返回
      其他       → 返回 error("unsupported content type: {type}")
  → 读取 body(最多 10MB,防超大页面)
  → html2md::parse_html(&html) → markdown
  → 截断到 100KB,保留头部
  → ToolResult::text(markdown)
```

**WebSearch 数据流:**

```
LLM 调 WebSearchTool(args: {query, count?})
  → 读取 BOCHA_API_KEY 环境变量
      缺失 → ToolResult::error("BOCHA_API_KEY not set")
  → BochaSearchProvider::search(query, count)
      → 构造 JSON 请求体 {query, count, ...}
      → reqwest POST 到 https://api.bocha.cn/v1/web-search
      → 解析 JSON 响应 → Vec<SearchResult>
  → 格式化结果为文本
      1. {title}
         {url}
         {snippet}

      2. {title}
         {url}
         {snippet}
      ...
  → ToolResult::text(formatted)
```

### WebSearchProvider trait

```rust
#[async_trait]
pub trait WebSearchProvider: Send + Sync {
    fn name(&self) -> &str;
    async fn search(&self, query: &str, count: usize) -> Result<Vec<SearchResult>, ToolsError>;
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}
```

### WebSearchTool 持有主引擎

```rust
pub struct WebSearchTool {
    engine: Arc<dyn WebSearchProvider>,
}
```

构造时传入主引擎。`register_builtin_tools` 内部从环境变量读 API key,有 key 就构造 BochaSearchProvider 并注册 WebSearchTool,没 key 就跳过(WebSearch 不可用)。

## Types & Error Handling

### WebFetchTool

```rust
pub struct WebFetchTool {
    client: reqwest::Client,
}

impl WebFetchTool {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())  // 无限重定向
            .user_agent("yi-agent/0.1.1")
            .build()
            .expect("failed to build reqwest client");
        Self { client }
    }
}

#[derive(Deserialize)]
struct FetchArgs {
    url: String,
    #[serde(default)]
    max_length: Option<usize>,  // 默认 100KB
}
```

### WebSearchTool

```rust
pub struct WebSearchTool {
    engine: Arc<dyn WebSearchProvider>,
}

impl WebSearchTool {
    pub fn new(engine: Arc<dyn WebSearchProvider>) -> Self {
        Self { engine }
    }
}

#[derive(Deserialize)]
struct SearchArgs {
    query: String,
    #[serde(default)]
    count: Option<usize>,  // 默认 25
}
```

### SearchResult

```rust
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}
```

### WebSearchProvider trait

```rust
#[async_trait]
pub trait WebSearchProvider: Send + Sync {
    fn name(&self) -> &str;
    async fn search(&self, query: &str, count: usize) -> Result<Vec<SearchResult>, ToolsError>;
}
```

### BochaSearchProvider

```rust
pub struct BochaSearchProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,  // 默认 "https://api.bocha.cn/v1"
}

impl BochaSearchProvider {
    pub fn new(api_key: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build reqwest client");
        Self {
            client,
            api_key,
            base_url: "https://api.bocha.cn/v1".to_string(),
        }
    }

    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("BOCHA_API_KEY").ok()?;
        Some(Self::new(api_key))
    }
}
```

### ToolsError 新增 variants

```rust
#[derive(Debug, thiserror::Error)]
pub enum ToolsError {
    // ... 现有 variants 不变 ...

    #[error("http error: {0}")]
    Http(String),

    #[error("unsupported content type: {0}")]
    UnsupportedContentType(String),

    #[error("response too large: {0} bytes")]
    ResponseTooLarge(usize),

    #[error("search engine error: {0}")]
    SearchEngine(String),

    #[error("BOCHA_API_KEY not set")]
    MissingApiKey,
}
```

### reqwest::Error 处理

reqwest::Error 不直接 `#[from]`,因为要用 String 消息(与 yi-agent-llm 的 `ProviderError::Network` 模式一致):

```rust
// 在工具内部:
let resp = self.client.get(&url).send().await
    .map_err(|e| ToolsError::Http(e.to_string()))?;
```

## Web Tools

### WebFetchTool

**schema:**
```json
{
  "type": "object",
  "properties": {
    "url": { "type": "string", "description": "URL to fetch (http or https)" },
    "max_length": { "type": "integer", "description": "Max bytes to return, default 100KB" }
  },
  "required": ["url"]
}
```

**行为:**
1. 解析 URL,校验 scheme 是 `http` 或 `https`,否则返回 `ToolResult::error("unsupported scheme: {scheme}")`
2. `reqwest GET` 请求,60s 超时,无限重定向
3. 检查 `Content-Type`:
   - `text/html` → 读 body(最多 10MB,防超大页面)→ `html2md::parse_html()` 转 markdown
   - `text/plain` → 读 body,直接用
   - `application/json` → 读 body,直接用(JSON 本身可读)
   - 其他 → 返回 `ToolResult::error("unsupported content type: {type}")`
4. 截断到 `max_length`(默认 100KB),保留头部,加 `[truncated: showed {N} of {M} bytes]` 标记
5. 返回 `ToolResult::text(content)`
- `metadata`: `read_only = true`, `requires_confirmation = false`

### WebSearchTool

**schema:**
```json
{
  "type": "object",
  "properties": {
    "query": { "type": "string", "description": "Search query" },
    "count": { "type": "integer", "description": "Max results, default 25" }
  },
  "required": ["query"]
}
```

**行为:**
1. 解析 `query` 和 `count`(默认 25)
2. 调用 `self.engine.search(query, count).await`
3. 格式化结果:
   ```
   1. {title}
      {url}
      {snippet}

   2. {title}
      {url}
      {snippet}
   ```
4. 0 结果 → 返回 `ToolResult::text("no results")`(不是错误)
5. 引擎错误 → `ToolResult::error(...)`
- `metadata`: `read_only = true`, `requires_confirmation = false`

## BochaSearchProvider 实现细节

### Bocha API 规格

基于博查 AI 官方文档(https://bocha-ai.feishu.cn/wiki/RXEOw02rFiwzGSkd9mUcqoeAnNK)。

- **Endpoint**: `POST https://api.bocha.cn/v1/web-search`
- **认证**: `Authorization: Bearer {API_KEY}`
- **API key 获取**: https://open.bocha.cn
- **响应格式**: 兼容 Bing Search API

### API 请求

```rust
// 请求体
#[derive(Serialize)]
struct BochaRequest {
    query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<bool>,  // 默认 true(返回摘要)
    #[serde(skip_serializing_if = "Option::is_none")]
    count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    freshness: Option<String>,  // 默认 "noLimit"
}

// POST https://api.bocha.cn/v1/web-search
// Headers: Authorization: Bearer {api_key}, Content-Type: application/json
```

### API 响应解析

```rust
#[derive(Deserialize)]
struct BochaResponse {
    code: u16,
    #[serde(default)]
    msg: Option<String>,
    data: Option<BochaData>,
}

#[derive(Deserialize)]
struct BochaData {
    #[serde(default)]
    web_pages: Option<WebPages>,  // serde rename to snake_case
}

#[derive(Deserialize)]
struct WebPages {
    #[serde(default)]
    value: Vec<WebPageValue>,
}

#[derive(Deserialize)]
struct WebPageValue {
    name: String,       // 标题
    url: String,        // URL
    #[serde(default)]
    snippet: String,    // 简短描述
    #[serde(default)]
    summary: String,    // 文本摘要(summary=true 时)
}
```

### 转成 SearchResult

```rust
// BochaSearchProvider::search 内部
let results: Vec<SearchResult> = response
    .data
    .and_then(|d| d.web_pages)
    .map(|wp| wp.value)
    .unwrap_or_default()
    .into_iter()
    .map(|v| SearchResult {
        title: v.name,
        url: v.url,
        snippet: if v.summary.is_empty() { v.snippet } else { v.summary },
    })
    .collect();
```

优先用 `summary`(更完整的摘要),fallback 到 `snippet`。

### 错误映射

| HTTP 状态 | Bocha code | 映射 | 处理 |
|---|---|---|---|
| 200 | 200 | 正常 | 解析结果 |
| 401 | 401 | `ToolsError::SearchEngine("invalid API key")` | 提示检查 key |
| 403 | 403 | `ToolsError::SearchEngine("insufficient balance")` | 余额不足 |
| 429 | 429 | `ToolsError::SearchEngine("rate limited")` | 频率限制 |
| 500 | 500 | `ToolsError::SearchEngine("server error: {msg}")` | 服务异常 |
| 其他 | - | `ToolsError::SearchEngine("unexpected: {status}")` | 未知错误 |

### 默认请求参数

- `summary`: `true`(返回摘要,更适合 LLM)
- `freshness`: `"noLimit"`(默认值,搜索算法自动改写时间范围效果更好)
- `count`: 由 LLM 传入,默认 25

## Dependencies

`yi-agent-rs/crates/yi-agent-tools/Cargo.toml` 新增:

```toml
[dependencies]
# 现有依赖不变...

# Web 工具新增
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
html2md = "0.1"

[dev-dependencies]
# 现有不变...
wiremock = "0.6"
```

### 设计要点

- `reqwest` 配置与 `yi-agent-llm` 一致:关 default features,启用 `json` + `rustls-tls`
- `html2md` 选当前稳定版
- `wiremock` 只在 dev-dependencies,用于 mock HTTP server 测试

## Registration API

`register_builtin_tools` 扩展:

```rust
pub fn register_builtin_tools(registry: &mut ToolRegistry, root: PathBuf) {
    let ctx = Arc::new(ToolsContext::new(root));
    // FS + Shell 工具
    registry.register(Arc::new(ReadTool::new(ctx.clone())));
    registry.register(Arc::new(WriteTool::new(ctx.clone())));
    registry.register(Arc::new(EditTool::new(ctx.clone())));
    registry.register(Arc::new(GlobTool::new(ctx.clone())));
    registry.register(Arc::new(GrepTool::new(ctx.clone())));
    registry.register(Arc::new(BashTool::new(ctx)));

    // Web 工具
    registry.register(Arc::new(WebFetchTool::new()));
    if let Some(bocha) = BochaSearchProvider::from_env() {
        registry.register(Arc::new(WebSearchTool::new(Arc::new(bocha))));
    }
    // BOCHA_API_KEY 未设则不注册 WebSearchTool,agent 可用但搜索不可用
}
```

## Testing

用 `wiremock` mock HTTP server。

### WebFetchTool 测试

- `fetch_html_returns_markdown` — mock 返回 HTML,验证返回 markdown
- `fetch_plain_text` — mock 返回 text/plain,验证直接返回
- `fetch_json_content` — mock 返回 application/json,验证直接返回
- `fetch_unsupported_content_type` — mock 返回 image/png,验证返回 error
- `fetch_truncates_large_response` — mock 返回 >100KB HTML,验证截断标记
- `fetch_invalid_scheme` — URL 为 ftp://,验证返回 error
- `fetch_follows_redirect` — mock 返回 302 到另一 endpoint,验证跟随

### WebSearchTool 测试(mock Bocha API)

- `search_returns_results` — mock 返回正常响应,验证格式化输出
- `search_no_results` — mock 返回空 value 数组,验证返回 "no results"
- `search_invalid_key` — mock 返回 401,验证返回 error
- `search_rate_limited` — mock 返回 429,验证返回 error
- `search_uses_summary_field` — mock 返回含 summary 字段的响应,验证优先用 summary

## Implementation Order

(供 writing-plans 参考,非本设计文档约束)

1. `error.rs` — 新增 Web 相关 error variants
2. `web/provider.rs` — WebSearchProvider trait + SearchResult
3. `web/fetch.rs` — WebFetchTool
4. `web/bocha.rs` — BochaSearchProvider
5. `web/search.rs` — WebSearchTool
6. `web/mod.rs` — 模块导出
7. `lib.rs` — 扩展 register_builtin_tools
8. tests — wiremock 集成测试

## Out of Scope (YAGNI)

- **多引擎聚合** — 单主引擎够用,多引擎同时查询 YAGNI
- **图片/视频搜索** — Bocha 视频搜索暂未开放,图片搜索对 coding agent 用处小
- **WebFetch 的 prompt 参数** — 库不做 AI 处理,由 LLM 自行处理抓取内容
- **SSRF 防护** — 库的职责不包含安全策略,由 CLI/调用方负责
- **请求重试** — YAGNI;调用方处理
- **缓存** — 不缓存请求结果,避免复杂度
- **流式响应** — 网页内容一次性返回,不支持流式
- **cookie/session** — 不维护会话状态
- **代理配置** — 不支持 HTTP 代理(环境变量 `HTTPS_PROXY` 由 reqwest 自动识别,但不在工具层显式配置)
