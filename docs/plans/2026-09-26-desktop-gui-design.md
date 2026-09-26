# Yi-Agent 桌面图形界面设计(macOS + Linux)

- 日期:2026-09-26
- 状态:已确认,待实现
- 参考:OpenAI Codex(`codex app-server` 架构)

## 1. 背景与目标

`yi-agent` 目前只有 CLI 和 ratatui TUI。本设计为它增加一个**原生桌面图形界面**,
先搭可端到端跑通的 baseline,再逐步补功能。

目标:

- macOS 上双击打开原生 `.app` 窗口(非浏览器),同时可迁移到 Linux。
- 能输入 prompt、流式看到 agent 输出、看到工具调用与结果。
- 危险工具触发**交互式权限审批**。
- 架构上解耦,将来 TUI / 其他前端能复用同一套协议层(对齐 codex 的 app-server 思路)。

## 2. 非目标(baseline 不做)

- 会话持久化 / 历史列表 / resume(framework 无持久化,thread 为内存态)。
- markdown 富渲染、代码高亮、diff 视图。
- 多会话标签页、多窗口、常驻 daemon。
- GUI 内编辑配置(复用 env / `.env`)。
- MCP、子 agent 任务树可视化。

以上均进入路线图(见 §11),不作为 baseline 交付。

## 3. 关键决策摘要

| 决策点 | 选择 | 理由 |
|---|---|---|
| GUI 技术栈 | **Tauri 2.x** | 跨平台(macOS + Linux)、Rust 后端、系统 WebView(非 Chromium)、bundle 小 |
| 前端 | **React + TS + Tailwind + Vite** | 生态大,markdown/高亮/diff 组件现成,TS 给协议类型安全 |
| 集成方式 | **app-server(codex 式)** | 长期最干净,多前端共享协议层 |
| 传输 | **stdio(JSONL)** | 一个 app 一个进程,生命周期好控 |
| 协议形态 | **精简 codex 式**(lifecycle 完整,item 类型先最小) | 保留演进空间,baseline 可控 |
| baseline 范围 | **最小闭环 + 权限审批** | 提前跑通 server→client 反向请求这条关键链路 |
| 配置复用 | **抽共享 crate `yi-agent-runtime`** | CLI 与 app-server 共用,避免逻辑漂移 |

## 4. 整体架构

```
┌──────────────────────────────────────────────┐
│  YiAgent.app (Tauri 2.x 原生窗口)             │
│  ┌────────────────────────────────────────┐  │
│  │ 前端 React + TS + Tailwind (系统 WebView)│  │
│  └──────────────┬─────────────────────────┘  │
│                 │ Tauri invoke / event        │
│  ┌──────────────┴─────────────────────────┐  │
│  │ Tauri Rust 后端 (src-tauri)             │  │
│  │  · 管理 sidecar 生命周期(启/停/重启)   │  │
│  │  · 桥接:JS ⇄ 子进程 stdio              │  │
│  └──────────────┬─────────────────────────┘  │
└─────────────────┼────────────────────────────┘
                  │ stdio · JSON-RPC 2.0 · JSONL
┌─────────────────┴────────────────────────────┐
│  yi-agent app-server (sidecar 子进程)         │
│  · initialize / thread / turn 状态机          │
│  · AgentEvent → item 通知(翻译层)           │
│  · 反向请求:权限审批(server→client)        │
└─────────────────┬────────────────────────────┘
                  │ 进程内直接调用
┌─────────────────┴────────────────────────────┐
│  yi-agent-runtime ★新共享 crate               │
│  · .env/env 加载 + provider 构造              │
│  · tool registry 装配 + AgentConfig           │
└─────────────────┬────────────────────────────┘
                  │
   yi-agent-core / -llm / -tools / -skills(现有)
```

**关键约束**:Tauri 后端**不链接** agent 代码,只做 sidecar 进程管理 + 协议桥接。
协议边界 = 进程边界。

## 5. 仓库布局

```
yi-agent/
├── yi-agent-rs/crates/
│   ├── yi-agent/            # CLI(+ 新增 `app-server` 子命令)
│   ├── yi-agent-runtime/    # ★新:共享配置与装配(CLI/app-server 共用)
│   ├── yi-agent-app-server/ # ★新:协议类型 + stdio 传输 + 请求处理
│   └── ...(现有 crates)
└── desktop/                 # ★新:Tauri 应用(独立于 Rust workspace)
    ├── src/                 # React 前端
    ├── src-tauri/           # Tauri Rust 后端
    └── package.json
```

`desktop/src-tauri` 是**独立 Cargo 项目**,不并入 `yi-agent-rs` workspace,避免耦合。

## 6. 协议设计(JSON-RPC 2.0 over stdio)

### 6.1 帧格式

换行分隔 JSON(JSONL),一行一条消息,UTF-8,单帧上限 1 MB(与现有 daemon IPC 一致)。
保留 `"jsonrpc":"2.0"` 头,方便标准工具调试。

```jsonc
// 请求        {"jsonrpc":"2.0","id":1,"method":"turn/start","params":{...}}
// 成功响应    {"jsonrpc":"2.0","id":1,"result":{...}}
// 错误响应    {"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"...","data":{...}}}
// 通知(无 id) {"jsonrpc":"2.0","method":"item/delta","params":{...}}
// 反向请求    {"jsonrpc":"2.0","id":"s1","method":"item/toolCall/requestApproval","params":{...}}
```

### 6.2 生命周期

```
initialize → initialized → thread/start → turn/start → (item/* 通知) → turn/completed
```

### 6.3 方法清单(baseline)

| 方向 | 方法 | 说明 |
|---|---|---|
| C→S | `initialize` | 握手,返回 `serverInfo` / `protocolVersion` / `capabilities` |
| C→S | `initialized` | 握手完成通知 |
| C→S | `thread/start` | 建会话,返回 `{id,cwd,model}`;发 `thread/started` |
| C→S | `turn/start` | `{threadId,input:[{type:"text",text}]}`;发 `turn/started` |
| C→S | `turn/interrupt` | 中断当前 turn |
| C→S | `config/read` | 读有效配置(model/provider/workdir,api_key 脱敏) |
| S→C | `thread/started` / `turn/started` | 生命周期通知 |
| S→C | `item/started` / `item/delta` / `item/completed` | item 流式更新 |
| S→C | `turn/completed` | `{status:"completed"\|"interrupted"\|"failed", usage?, error?}` |
| S→C | `thread/tokenUsage/updated` | 用量 |
| S→C | `error` | 非致命错误 |
| S→C 请求 | `item/toolCall/requestApproval` | 权限审批,client 必须回 `{decision}` |

### 6.4 Item 类型(baseline)

- `userMessage`
- `agentMessage`(带 `item/delta` 流式)
- `toolCall`(含 `callId`、`name`、`input`、`status`、`result`、`output`)

### 6.5 AgentEvent → 协议映射

| AgentEvent | 协议 |
|---|---|
| `AssistantText` / `DecodeDelta` | `agentMessage` + `item/delta` |
| `ToolCall` | `item/started`(toolCall) |
| `ToolOutputDelta` | `item/delta` |
| `ToolResult` / `ToolExit` / `ToolTimeout` | `item/completed` |
| `Done` / `Cancelled` / `Error` | `turn/completed` |
| `Usage` | `thread/tokenUsage/updated` |
| `PermissionRequest` | 反向请求 `item/toolCall/requestApproval` |
| `PermissionResolved` | 通知 |

### 6.6 错误码

标准:`-32700`(解析失败)/ `-32600`(非法请求)/ `-32601`(方法不存在)/
`-32602`(参数非法)/ `-32603`(内部错误)。
应用码:`-32010` 未初始化、`-32011` thread 不存在、`-32012` turn 进行中。

### 6.7 范围说明

无持久化 → thread 是 app-server 进程内的内存 `Session`,重启即丢失。
`thread/list` / `resume` / `fork` 与历史回放全部进路线图。

## 7. app-server 内部设计

### 7.1 crate 结构

```
yi-agent-app-server/src/
├── lib.rs         # 公开 run() 入口
├── protocol.rs    # JSON-RPC 信封 + 请求/响应/通知类型(serde)
├── transport.rs   # stdio 读写 + JSONL 分帧 + 1MB 上限
├── server.rs      # 主循环:读请求 → 分发 → 写响应
├── session.rs     # thread 状态:Session + Agent + 当前 turn
├── translate.rs   # AgentEvent → 协议通知(翻译层)
└── main.rs        # 二进制入口(或由 CLI `app-server` 子命令调用)
```

### 7.2 并发模型

- **reader task**:独占 stdin,读一行 → 解析 → 投递到内部 channel。
- **writer**:`Mutex<Stdout>` 串行化输出,保证不交错。
- **turn driver task**:每轮 `turn/start` spawn 一个,消费 `agent.run()` 返回的
  `BoxStream<AgentEvent>`,经 `translate.rs` 转成通知写出;`tokio::select!` 监听中断信号。
- **单 turn 保护**:同一 thread 同时只允许一个活跃 turn,冲突返回 `-32012`。

### 7.3 反向请求(权限审批)—— 关键链路

1. driver 收到 `AgentEvent::PermissionRequest { request_id, tool_name, tool_input, kind, .. }`。
2. 发 JSON-RPC **请求** `item/toolCall/requestApproval`(id = `"perm-{request_id}"`),
   把 oneshot sender 存进 `pending: HashMap<String, oneshot::Sender<Decision>>`。
3. reader 收到 client 的**响应**(id 匹配)→ 解析 decision → 经 oneshot 回传。
4. driver 把 `(request_id, Decision)` 写入 `decision_tx`(即 `Agent::with_permission` 的 channel)。

### 7.4 取消

`turn/interrupt` → `agent.cancel()`。

> ⚠️ 已知坑:`Agent::run()` 会**重置** cancel token,必须在 `run()` **之后**捕获 token
> 再注册中断,否则取消无效(见 CLAUDE.md「cargo test 执行」小节记录)。

### 7.5 日志

`tracing` 全部走 **stderr**;stdout 只用于协议流,绝不能被日志污染。
stdin EOF 或收到 `shutdown` → 优雅退出,清理子进程。

## 8. 共享 runtime crate(`yi-agent-runtime`)

### 8.1 目的

把 CLI binary crate 里的配置加载与 Agent 装配逻辑抽出来,让 CLI 和 app-server 共用,
杜绝逻辑漂移。

### 8.2 迁移内容

| 来源 | 迁入 |
|---|---|
| `crates/yi-agent/src/config.rs` 的 `Config` 结构、`.env`/env 解析、默认值 | `RuntimeConfig` + 加载器 |
| `main.rs` 的 provider 字符串匹配构造 | `build_provider()` |
| `main.rs` 的 `build_headless_setup`(tool registry + system prompt) | `build_tools()` / `build_system_prompt()` |
| `main.rs` 的 `PermissionChecker` 构造 | `bootstrap_agent()` |

**关键边界**:clap 的 `Cli` 结构**留在 binary crate**(CLI 专属),runtime crate 只吃
纯数据 `ConfigOverrides`,由 binary 侧做 `Cli → ConfigOverrides` 适配。runtime crate 不依赖 clap。

### 8.3 公开 API

```rust
pub struct RuntimeConfig {
    pub provider: ProviderKind,     // Anthropic | OpenAi
    pub model: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub workdir: PathBuf,
    pub system_prompt: Option<String>,
    pub max_turns: Option<u32>,
    pub compact_threshold: Option<u32>,
    pub compact_user_budget_tokens: usize,
    pub compact_tool_budget_tokens: usize,
    pub skills_catalog_budget: usize,
    pub sandbox: SandboxMode,
    pub yolo: bool,
}

pub struct ConfigOverrides { /* 来自 CLI flag 或 GUI 请求 */ }

impl RuntimeConfig {
    /// 优先级:显式 override > 真实 env > workdir/.yi-agent/.env > ~/.yi-agent/.env > 默认值
    pub fn load(overrides: ConfigOverrides) -> Result<Self, RuntimeError>;
    /// 供 config/read 返回给 GUI 的安全视图(api_key 脱敏)
    pub fn redacted_view(&self) -> serde_json::Value;
}

pub fn build_provider(cfg: &RuntimeConfig) -> Result<Arc<dyn Provider>, RuntimeError>;
pub fn build_tools(cfg: &RuntimeConfig) -> Result<Arc<ToolRegistry>, RuntimeError>;
pub fn build_system_prompt(cfg: &RuntimeConfig) -> String;

pub struct AgentBootstrap {
    pub agent: Agent,
    pub permission: Arc<PermissionChecker>,
    pub decision_tx: mpsc::Sender<(u64, Decision)>,
}
pub fn bootstrap_agent(cfg: &RuntimeConfig, mode: PermissionMode)
    -> Result<AgentBootstrap, RuntimeError>;
```

### 8.4 配置来源(baseline)

GUI 暂不做配置编辑界面,app-server 复用 env / `.env` 加载(与 CLI 完全一致)。
`config/read` 只读回有效配置(脱敏)。UI 改 model / api key 属于路线图。

### 8.5 重构影响

`crates/yi-agent` 的 `config.rs` 瘦身为 `Cli → ConfigOverrides` 适配层;`main.rs` 的
setup 函数改为调用 runtime crate。CLI 行为必须**逐字节不变**(现有 e2e 测试即回归网)。

## 9. Tauri 应用设计(`desktop/`)

### 9.1 Rust 后端(`src-tauri/`)—— 纯桥接,不含 agent 逻辑

- **sidecar 生命周期**:启动时用 Tauri sidecar API spawn `yi-agent app-server --listen stdio://`;
  退出/窗口关闭时 kill;崩溃时重启并通知前端。
- **stdin 写入**:暴露 `invoke("rpc", {method, params})`,后端分配 `id`、写一行 JSON 到子进程 stdin。
- **stdout 读取**:后台 task 逐行读,解析后分两类转发:
  - 通知/响应 → 发 Tauri event `app-server://message`(前端按 `id` 或 `method` 分发)。
  - 反向请求(权限审批)→ 发 event `app-server://request`,前端弹窗,答复走
    `invoke("rpc_respond", {id, result})`。
- **重启/错误**:子进程退出时发 `app-server://status`,前端显示"连接已断开,重连中"。

### 9.2 前端(`src/`)—— 只依赖协议,不依赖 Rust 类型

```
src/
├── lib/rpc.ts          # 连接封装:invoke + event 监听,id 关联请求/响应,Promise 化
├── lib/protocol.ts     # 协议 TS 类型(手写,与 protocol.rs 对齐)
├── lib/session.ts      # 会话状态机:items 列表、流式 delta 合并、turn 状态
├── components/
│   ├── ChatView.tsx    # item 列表渲染(文本/工具调用)
│   ├── MessageInput.tsx# 输入框 + 发送 + 中断
│   ├── ToolCallCard.tsx# 工具调用卡片,输入/输出可折叠
│   └── ApprovalDialog.tsx # 权限审批弹窗(允许一次/始终允许/拒绝)
├── App.tsx
└── main.tsx
```

### 9.3 baseline UI 布局

- 顶部工具栏:workdir 显示 + 模型名 + 状态点。
- 中间对话区:流式文本 + 工具调用卡片。
- 底部输入框 + 发送/中断。
- 模态审批弹窗。

### 9.4 UI 基线取舍

markdown 先用等宽纯文本渲染(保留代码块缩进),富渲染 / 代码高亮 / diff 进路线图。
虚拟列表、多会话标签页同理。

### 9.5 打包

macOS 出 `.app` / `.dmg`;Linux 出 `.deb` / `.AppImage`。`yi-agent` 二进制作为 sidecar
打进 bundle。

## 10. 错误处理与测试

### 10.1 错误处理

- **协议层**:非法 JSON → `-32700`;未知 method → `-32601`;参数不合法 → `-32602`;
  未初始化就调方法 → `-32010`。错误响应后连接**不中断**。
- **agent 层**:`AgentError` 映射到 `turn/completed {status:"failed", error}`;
  provider 网络错误透传为可读 message。
- **权限层**:审批超时/无响应 → 视为拒绝(默认安全)。
- **进程层**:app-server 崩溃 → Tauri 重启 sidecar + 前端横幅提示;
  stdout 出现非 JSON 行 → 丢弃并记 stderr 日志,不崩。
- **前端**:RPC 失败 reject Promise,UI 显示错误气泡,不卡死。

### 10.2 测试策略(对齐项目现有分级)

- **协议单测(Tier 0,mock)**:`protocol.rs` 信封 serde 往返;
  `translate.rs` 的 `AgentEvent → 通知` 映射表逐条断言。
- **app-server 集成测试(Tier 0)**:用 mock `Provider`(wiremock)喂假事件流,
  起 server、喂 stdin 请求、断言 stdout 通知序列;重点覆盖权限反向请求闭环。
- **runtime crate 回归**:现有 CLI e2e 全绿,证明重构未改行为。
- **前端单测**:`session.ts` 的 delta 合并状态机(vitest)。
- **手工冒烟**:Tauri 起 app → 发 prompt → 流式输出 → 触发工具调用 → 弹审批 → 完成。
- **真实 LLM(Tier 2,`#[ignore]`)**:端到端经 GUI 协议跑一轮,CI 不跑。

## 11. 路线图

- **P0 baseline**:本设计 §1–§10 全部内容。
- **P1**:会话持久化(`thread/list` / `resume` / `delete`)、历史侧栏、
  markdown 富渲染 + 代码高亮、reasoning/thinking 展示、用量与成本面板。
- **P2**:文件树 / 工作区切换、diff 视图(`turn/diff/updated`)、compact 手动控制、
  图片附件输入、多 thread 标签页。
- **P3**:MCP 集成、子 agent 任务树可视化(复用现有 daemon IPC)、
  Unix socket / websocket 传输、多窗口共享 daemon、Linux 打包验证。

## 12. 成功判据(baseline)

- 双击 `.app` 打开原生窗口,无浏览器 chrome。
- 输入 prompt → 流式文本逐字出现 → 工具调用卡片显示输入/输出。
- 危险工具触发审批弹窗,选"允许一次"后继续执行,选"拒绝"后 agent 收到拒绝。
- 点中断能立即停止当前 turn。
- `cargo test -p yi-agent-app-server` 与 `cargo test -p yi-agent-runtime` 全绿;
  现有 CLI e2e 不回归。

## 13. 风险与缓解

| 风险 | 缓解 |
|---|---|
| `Agent::run()` 重置 cancel token 导致中断失效 | 在 `run()` 之后捕获 token;加针对性测试 |
| stdout 被日志污染破坏协议 | `tracing` 强制走 stderr;transport 层对非 JSON 行容错 |
| 重构 CLI config 引入行为回归 | 现有 e2e 测试作回归网;先抽 crate 再改调用点 |
| Tauri sidecar 打包 / target-triple 命名踩坑 | 尽早跑通一次 macOS bundle 冒烟 |
| WebKitGTK 在 Linux 缺失 | 文档标注依赖;P3 阶段验证各发行版 |
