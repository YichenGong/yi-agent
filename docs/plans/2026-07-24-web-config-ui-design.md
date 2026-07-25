# Design: WebUI for yi-agent Environment Variable Management

## Context

yi-agent 目前有 14 个环境变量控制其行为（provider、model、API key、workdir 等），全部通过进程环境或 CLI 参数配置，没有 .env 文件支持。用户需要一个好看的 WebUI 来管理这些配置。

## Scope

- 仅管理 yi-agent 定义的 14 个环境变量
- 写入 `.env` 文件，yi-agent 启动时读取
- 作为 `yi-agent web` 子命令启动
- 默认端口 7292

## Architecture

新增 crate `yi-agent-web`，放在 `yi-agent-rs/crates/` 下，作为 workspace 成员。提供 axum Web 服务器，内嵌单页 HTML 前端。

**启动流程**：
- 用户执行 `yi-agent web [--port 7292] [--host 127.0.0.1]`
- clap 解析 `web` 子命令
- 调用 `yi_agent_web::serve(host, port, env_path)` 启动 HTTP 服务器
- 浏览器访问 `http://127.0.0.1:7292` 打开配置页面

**.env 支持变更**：
- 给 `yi-agent` crate 加 `dotenvy` 依赖
- 在 `config::load()` 最开头调用 `dotenvy::from_path(&workdir.join(".env"))`，从 .env 文件加载环境变量
- 已存在于进程环境中的变量优先（dotenvy 默认行为），.env 只填充缺失项

**依赖关系**：
```
yi-agent (bin) → yi-agent-web (new crate)
                → axum, tokio, serde_json
yi-agent (bin) → dotenvy (new dep)
```

## .env File Format

文件位置：`YI_AGENT_WORKDIR/.env`。如果不存在则自动创建。

标准 dotenv 格式，按分组注释组织：

```env
# === Provider ===
YI_AGENT_PROVIDER=anthropic
MODEL_API_KEY=sk-ant-xxxxx
MODEL_API_URL=https://api.anthropic.com
YI_AGENT_MODEL=claude-sonnet-4-20250514

# === Agent ===
YI_AGENT_MAX_TURNS=20
YI_AGENT_WORKDIR=/home/user/project
YI_AGENT_SYSTEM_PROMPT=
YI_AGENT_COMPACT_THRESHOLD=100000
YI_AGENT_COMPACT_KEEP_TURNS=4

# === Anthropic Provider ===
ANTHROPIC_API_KEY=
ANTHROPIC_BASE_URL=https://api.anthropic.com

# === OpenAI Provider ===
OPENAI_API_KEY=
OPENAI_BASE_URL=https://api.openai.com

# === Tools ===
BOCHA_API_KEY=
```

**读写策略**：
- 读：Web 后端解析 .env 文件，保留注释和分组结构，返回 JSON 给前端
- 写：Web 后端重写整个 .env 文件（保留分组注释，不保留用户手写的额外注释）

**掩码规则**：key 类字段返回 `前4字符 + *** + 后4字符`，若不足 12 字符则全 `***`。前端编辑时，掩码值不回写，只有用户输入新值才更新。

## HTTP API

### GET /api/config

返回 14 个变量的当前值、默认值、分组、类型和描述：

```json
{
  "groups": [
    {
      "name": "Provider",
      "vars": [
        {
          "key": "YI_AGENT_PROVIDER",
          "value": "anthropic",
          "default": "anthropic",
          "type": "select",
          "options": ["anthropic", "openai"],
          "description": "LLM provider backend",
          "masked": false
        },
        {
          "key": "MODEL_API_KEY",
          "value": "sk-a***xxxx",
          "default": null,
          "type": "secret",
          "description": "API key for the LLM provider",
          "masked": true
        }
      ]
    }
  ],
  "envPath": "/home/user/project/.env"
}
```

### PUT /api/config

接收部分字段更新，只写入用户实际修改的变量：

```json
{
  "updates": [
    { "key": "YI_AGENT_MODEL", "value": "claude-sonnet-4-5" },
    { "key": "MODEL_API_KEY", "value": "sk-new-key-here" }
  ]
}
```

对 `type: secret` 字段：如果 `value` 等于当前掩码值（或为空），跳过不写；只有值不同才更新。

### 错误响应

```json
{ "error": "Failed to write .env file: Permission denied" }
```

## Frontend

单个 `index.html` 文件，通过 axum `/` 路由直接返回。内嵌 CSS 和 JS，无外部依赖。

**布局**：
- 顶部：标题 "yi-agent 配置" + .env 文件路径显示
- 主体：按分组排列的表单卡片，每个分组一个区块
- 底部：保存按钮 + 未保存提示

**字段类型渲染**：

| type | 渲染方式 |
|------|---------|
| `select` | `<select>` 下拉框，options 来自 API |
| `secret` | `<input type="password">` + 显示/隐藏切换按钮 |
| `text` | `<input type="text">` |
| `number` | `<input type="number">` |
| `path` | `<input type="text">` + 目录选择提示 |

**交互行为**：
- 页面加载时 `fetch('/api/config')` 填充表单
- 任意字段修改时标记"未保存"，保存按钮高亮
- 点击保存：`PUT /api/config`，只发送修改过的字段
- secret 字段：掩码值显示为占位符，用户点击输入框才清空可编辑，留空表示不修改
- 保存成功后显示 toast 提示，更新"未保存"状态

**样式**：深色主题，等宽字体显示路径和 key 名，表单紧凑排列。无框架，约 200 行 CSS + 150 行 JS。

## Crate Structure

```
yi-agent-rs/crates/yi-agent-web/
├── Cargo.toml
├── src/
│   ├── lib.rs          # pub async fn serve(host, port, env_path)
│   ├── api.rs          # HTTP handler 函数
│   ├── env_file.rs     # .env 读写解析
│   ├── config_meta.rs  # 14 个变量的元数据定义
│   └── assets/
│       └── index.html  # 内嵌前端页面
```

**关键文件内容**：

`config_meta.rs` — 集中定义所有变量元数据：

```rust
pub struct VarMeta {
    pub key: &'static str,
    pub default: Option<&'static str>,
    pub var_type: VarType,      // Select / Secret / Text / Number / Path
    pub group: &'static str,
    pub description: &'static str,
    pub options: &'static [&'static str],  // 仅 Select
}

pub static ALL_VARS: &[VarMeta] = &[ /* 14 条 */ ];
```

`env_file.rs` — 读写 .env，保留分组注释：

```rust
pub fn read(path: &Path) -> Result<HashMap<String, String>>;
pub fn write(path: &Path, updates: &[(String, String)]) -> Result<()>;
```

`lib.rs` — 服务器入口：

```rust
pub async fn serve(host: &str, port: u16, env_path: PathBuf) -> Result<()> {
    let app = Router::new()
        .route("/", get(index_html))
        .route("/api/config", get(get_config).put(put_config))
        .with_state(AppState { env_path });
    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
```

**yi-agent bin 集成**：

`config.rs` — 新增 dotenvy 加载：

```rust
// load() 函数最开头
let env_path = workdir.join(".env");
let _ = dotenvy::from_path(&env_path);
// 然后正常读 env::var(...)
```

`main.rs` — clap 子命令：

```rust
#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

enum Command {
    /// Start web config UI
    Web {
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long, default_value = "7292")]
        port: u16,
    },
}
```

## Variable Metadata

14 个变量的完整定义：

| Key | Group | Type | Default | Description |
|-----|-------|------|---------|-------------|
| `YI_AGENT_PROVIDER` | Provider | select | `anthropic` | LLM provider backend |
| `MODEL_API_KEY` | Provider | secret | (required) | API key for the LLM provider |
| `MODEL_API_URL` | Provider | text | provider default | API endpoint URL override |
| `YI_AGENT_MODEL` | Provider | text | provider default | Model identifier string |
| `YI_AGENT_MAX_TURNS` | Agent | number | `20` | Max agent turns per conversation |
| `YI_AGENT_WORKDIR` | Agent | path | current_dir | Working directory for file tools |
| `YI_AGENT_SYSTEM_PROMPT` | Agent | text | (none) | Custom system prompt override |
| `YI_AGENT_COMPACT_THRESHOLD` | Agent | number | `100000` | Token threshold for auto-compact |
| `YI_AGENT_COMPACT_KEEP_TURNS` | Agent | number | `4` | Turns retained during compaction |
| `ANTHROPIC_API_KEY` | Anthropic | secret | (none) | Anthropic provider API key |
| `ANTHROPIC_BASE_URL` | Anthropic | text | `https://api.anthropic.com` | Anthropic API base URL |
| `OPENAI_API_KEY` | OpenAI | secret | (none) | OpenAI provider API key |
| `OPENAI_BASE_URL` | OpenAI | text | `https://api.openai.com` | OpenAI API base URL |
| `BOCHA_API_KEY` | Tools | secret | (none) | Bocha web search API key |

## Testing

**单元测试**（`env_file.rs`）：
- 解析标准 .env 文件，正确提取 key-value
- 解析带 `#` 注释和空行的文件，跳过非内容行
- 写入后重新读取，值一致
- 空 .env 文件处理
- 特殊字符值（含 `=`、空格、引号）

**单元测试**（`config_meta.rs`）：
- 所有 14 个变量都有元数据定义
- key 与实际代码中 `env::var()` 读取的名称一一对应

**集成测试**（`api.rs`）：
- `GET /api/config` 返回正确分组结构
- `PUT /api/config` 写入后，再 GET 验证值更新
- secret 字段返回掩码值，PUT 掩码值不触发写入
- 写入不存在的路径返回错误响应

**手动验证步骤**：
1. `yi-agent web` 启动，浏览器访问 `http://127.0.0.1:7292`
2. 修改 model 字段，保存，确认 `.env` 文件写入正确
3. 不修改 API key（显示掩码），保存，确认 key 未被覆盖
4. 修改 API key 为新值，保存，确认 key 更新
5. 运行 `yi-agent`（不带 web 子命令），确认读取了 .env 中的配置

## Existing Code Impact

- `config.rs` 加 `dotenvy::from_path()` — 不破坏现有行为，只补充 .env 来源
- `main.rs` 加子命令 — 原有 `yi-agent` 无子命令直接跑 agent 的行为保持不变（`command: Option<Command>`）

## Dependencies

新增 workspace 依赖：
- `axum = "0.8"`
- `dotenvy = "0.15"`

前端 HTML 通过 `include_str!` 内嵌，无需额外依赖。
