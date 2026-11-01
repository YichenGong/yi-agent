# yi-agent 使用指南

README 只保留安装与核心功能。这里放细节：其余安装方式、三种用法、远程连接、
配置与 MCP。

## 其余安装方式

### 直接下载

到 [Releases](https://github.com/YichenGong/yi-agent/releases) 下载对应平台的
`.tar.gz`，解压后把 `yi-agent` 放进 `PATH`。

### macOS 桌面 App（Apple Silicon）

`npm i -g` 会触发 `postinstall`，把 `yi-agent.app` 复制到 `/Applications` 并清掉
quarantine。该步骤只在全局安装时触发，且仅限 Apple Silicon（不支持 Intel Mac）。

若 `/Applications` 不可写（企业机器常见），CLI 仍会装好，并提示你手动执行
`sudo cp` 完成桌面 App 的安装。

从「应用程序」打开 `yi-agent`。首次打开若提示「无法验证开发者」，到
System Settings → Privacy & Security 点 **Open Anyway**。

首次启动会弹出**引导向导**，填「API 密钥」与「模型标识」（可选 API 地址）后点测试
连接，通过即写入 `~/.yi-agent/models.json`。之后再改模型走 **设置 → 模型**（可新增、
编辑、删除条目，也可「导入当前配置」）。App 与 TUI 共用这份清单，配一次即可。

### 内网离线安装（dmg）

内网用户也可到 GitLab 的 **Deploy → Releases** 下载 `yi-agent_<版本>_aarch64.dmg`：

```bash
# 安装（自动清 quarantine；未做签名/公证）
bash install.sh ./yi-agent_<版本>_aarch64.dmg
```

只用命令行、不装 App 的话，Release 页还提供独立 CLI 二进制
`yi-agent-aarch64-apple-darwin.tar.gz`（附 `.sha256`，用法见 Release 说明）。

### 从源码构建

需要 Rust 1.85 或更高版本。

```bash
git clone https://github.com/YichenGong/yi-agent.git
cd yi-agent/yi-agent-rs
cargo build --release
# 产物：target/release/yi-agent
```

装好后确认一下：

```bash
yi-agent --version
```

## 配置一个模型

直接运行 `yi-agent`，首启会引导你填 API 格式、密钥与模型标识；填完先测连接，通过
才写入 `~/.yi-agent/models.json`。

想跳过问答用一次性整包参数也行（`--provider` / `--api-key` / `--model` 三个都要给）：

```bash
yi-agent --provider anthropic --api-key sk-ant-... --model claude-sonnet-4-5
```

> 注意：单独 `export MODEL_API_KEY=...` 不能代替上面的步骤——模型配置只认
> `models.json` 或「整包」参数，缺项会被直接拒绝。

默认走 Anthropic。换 OpenAI 或其它兼容服务见下方「常用配置」。

## 三种用法

### 1. 交互式 TUI（推荐）

```bash
yi-agent
```

| 操作 | 作用 |
| --- | --- |
| 输入 `/` | 唤出命令菜单：`/help` `/model` `/cost` `/clear` `/compact` `/config` `/runtime` `/mcp` `/quit` |
| `Ctrl+P` | 打开运行面板：Bash 任务 / 托管进程 / 子 agent 轨迹 |
| `Esc` | 打断正在跑的这一轮（不会退出程序） |
| `Ctrl+C` 两次 | 退出 |

`/cost` 看这个会话花了多少；`/compact` 手动压缩历史；会话太长时它会自动压缩。

### 2. 非交互模式（脚本 / CI）

给它一句话，跑完就退出：

```bash
yi-agent run "把 src/ 下所有的 TODO 注释汇总成一个清单"

# 输出 JSONL（每行一个事件），方便程序化处理
yi-agent run --json "统计这个仓库有多少行 Rust 代码" | jq -r 'select(.AssistantText) | .AssistantText'

# 从管道拿输入
echo "解释一下 Makefile 里的 deploy 目标" | yi-agent run
```

### 3. 桌面 App（macOS）

原生窗口，对话、工具调用卡片、审批弹窗、子 agent 都在图形界面里。安装与开发方式见
[desktop/README.md](../desktop/README.md)。

## 配置放在哪

| 层 | 位置 | 用途 |
|---|---|---|
| 默认值 | Rust 常量（`yi-agent-runtime/src/config.rs`） | 不需要文件 |
| 模型 | `~/.yi-agent/models.json` | 设置 → 模型 UI 或 `model/*` RPC |
| 运行参数 | `~/.yi-agent/settings.json` | 设置 → 运行参数 UI 或 `settings/*` RPC |
| 临时覆盖 | CLI 参数、真实环境变量 | CI、脚本、一次性试验 |

`YI_AGENT_HOME` 可以把 `~/.yi-agent` 整体换到别处（打包独立实例时用它隔离）。
`.env` 已不再被读取：存量配置在首次启动时自动迁入 `settings.json`，原文件改名为
`.env.migrated` 保留取回。`YI_AGENT_WORKDIR` 已下线——工作目录只由 `--workdir`、
启动目录或桌面端选项目决定。

**一行说明**：`--model` 给的是模型**字符串**；要覆盖它必须同时给出 `--provider`
与 `--api-key`（`--api-url` 可选）——这是「整包注入」，缺项会直接报错。

**图形化入口**：桌面端「设置 → 模型」配置模型（写 `~/.yi-agent/models.json`），
「设置 → 运行参数」配置轮次、压缩、沙箱等（写 `~/.yi-agent/settings.json`）；
`yi-agent web`（默认 <http://127.0.0.1:7292>）改的是同一份 `settings.json`。

## 常用配置

| 想做什么 | 环境变量 | 命令行参数 |
| --- | --- | --- |
| 换模型（整包覆盖，三项都要给） | `YI_AGENT_PROVIDER` + `MODEL_API_KEY` + `YI_AGENT_MODEL` | `--provider` + `--api-key` + `--model`（`--api-url` 可选） |
| API key | `MODEL_API_KEY` | `--api-key` |
| 换服务商 | `YI_AGENT_PROVIDER`（`anthropic` / `openai`） | `--provider` |
| 换模型标识 | —— （用设置 UI / `models.json`） | `--model`（须配 `--provider` + `--api-key`） |
| 自定义 API 地址 | `MODEL_API_URL` | `--api-url` |
| 单轮最大步数 | `YI_AGENT_MAX_TURNS`（默认 200） | `--max-turns` |
| 每个 agent 的直接子任务上限 | `YI_AGENT_MAX_DIRECT_CHILDREN`（默认 8） | —— |
| 子 agent 常驻总容量 | `YI_AGENT_MAX_RESIDENT_SUBAGENTS`（默认 64） | —— |
| 压缩触发阈值 | `YI_AGENT_COMPACT_RATIO`（默认 80，按上下文百分比） | `--compact-ratio` |
| 联网搜索 | `BOCHA_API_KEY` | —— |

**换模型**，比如用 OpenAI（整包给全）：

```bash
yi-agent --provider openai --api-key sk-... --model gpt-4o
```

**关于权限确认。** 默认每个危险操作都要你点一次确认。这是启动时的开关：

- `--yolo`（或 `--dangerously-skip-permissions`）：跳过确认。仅建议在容器或一次性环境里用
- `--sandbox read-only`：整个进程只读，改不了任何文件
- `--sandbox workspace-write`：只能改工作目录，网络受限

黑名单命令在任何模式下都会被拦下。

**关于子 agent 运行时。** 首次在 TUI 里派生子 agent 时它会问你，之后用
`/runtime always` / `/runtime never` / `/runtime ask` 决定是否自动启动本地运行时。

## 接入 MCP server

MCP（Model Context Protocol）让你把外部工具接进来：GitHub、数据库、内部系统。

**最简单的方式是直接说**：

```
帮我加一个 github MCP，用 npx -y @modelcontextprotocol/server-github
```

模型会调用 `mcp_reload` 工具写配置并当场探测。工具从**下一条消息**起可用。
你也可以说「把 fs 那个 MCP 关掉」「删掉刚才加的」——四种动作（增/启用/停用/删）
都会请你确认一次，即使开着 `--yolo` 也一样（它改的是持久配置）。

**手动配置**：写在 `<项目>/.yi-agent/mcp.json`（Claude Desktop 兼容 schema）：

```json
{
  "mcpServers": {
    "fs": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/dir"]
    },
    "ws": {
      "type": "http",
      "url": "https://example.com/mcp",
      "headers": { "Authorization": "Bearer ${MY_TOKEN}" }
    }
  }
}
```

只有两种传输：`stdio`（本地子进程，默认）与 `http`（远端）。**没有 sse**。
`env` / `headers` 里可以用 `${VAR}` 引用环境变量，未定义会在写盘前报错。

改完不用重启：下一轮对话开始时自动检测到变更并重载（只重探变化的 server）。

**位置**：配置是**项目级**的，只对该项目生效。用户级
`~/.yi-agent/mcp.json` 目前不参与加载。

**开关**：TUI 里 `/mcp`（`on|off|reload|enable <server>|disable <server>|status`）；
桌面端「设置 → MCP」有全局与逐 server 的开关。

## 远程连接（实验性）

`app-server` 支持通过 WebSocket 提供服务，供网络客户端连接。**每条连接都要用已配对
设备 token 认证**（`ws://host/ws?token=<t>` 或 `Authorization: Bearer <t>`；无/错
token 以 ws close `4401` 拒绝），因此可多客户端同时连接：

```bash
yi-agent app-server --listen ws://127.0.0.1:8790   # 绑定非回环时会打印认证告警
```

同一批 session 可在多台设备上查看与控制：配对（一次性码 `XXXX-XXXX`、5 分钟有效，
新设备默认 `control` scope）、设备列表/撤销、审批广播到所有已连接设备。

客户端可调 `thread/subscribe {threadIds}` **只收自己关心的会话**（未调用者仍全收）；
已订阅时逐字输出会合并降频（每 100ms 或 4KB 一刷，跨 item 不合并、顺序不变），减少外网
手机的流量与噪音。见 [按会话订阅过滤设计](superpowers/specs/2026-10-02-thread-subscription-filtering-design.md)。

投递按 IM 式分三层：**List 层**（`thread/started`、`thread/status/updated`、`turn/completed`）
与 **Global 层**（`ui/settings/updated`、`error` 等）**永远投给所有客户端**，所以侧栏的会话列表、
运行状态与**未读蓝点**在任何设备上都是实时的；**Content 层**（`item/*`、`turn/started`、`turn/retry`
等正文流）才按 `thread/subscribe` 过滤。`turn/completed` 之所以归 List 层：它是未读蓝点
**唯一**的驱动，而落库的会话摘要（`ThreadSummary`）并不带未读位，`thread/listAll` 无从补齐——
若按内容层过滤，窗口外跑完的会话就永远不亮蓝点，用户会以为「还没跑完」。远程（iOS）客户端因此只保一个最多 8 条会话的 LRU 热窗口随选择滚动订阅，窗口外的
冷会话保持安静；当用户切回一个**正在跑**的冷会话时，客户端用只读的
`thread/readItems {threadId, afterItemId?}` 补齐既有内容，而**不用 `thread/resume`**——
后者会打断进行中的回合。桌面（stdio）端从不订阅，天然全收。

配对支持**扫码**：桌面设置「远程访问」页在填了中继地址时于配对码旁显示二维码，
`yi-agent pair code --relay <url>` 在终端打印二维码；iOS 配对页的「扫码」按钮用相机扫后
自动配对。二维码内容是 `yiagent://pair?v=1&relay=<地址>&code=<码>`，只含中继地址与一次性
配对码，**不含 token**。文本手输路径保留。

> **实验性**：配对链路已**端到端打通**——`pair/create` 铸出的码**落盘**到
> `~/.yi-agent/pairing.json`，因此桌面 stdio 进程铸的码可被 `--relay`/`ws://` 进程
> 兑换；iOS 首启有配对表单（填中继地址 + 码），桌面设置里有「远程访问」页可铸码与
> 管理设备。已知限制：暂**无二维码扫描**（手输码）、**无可安装的 iOS 产物**（受 Xcode
> 运行时/签名阻塞）。详见 [iOS 远程控制与中继部署](relay-deploy.md)。

**iOS App 经自建反向 WSS 中继**接入（两端都只发出站连接，电脑侧不开放入站端口）：

```bash
yi-agent app-server --relay 'wss://relay.example.com/connect?session=<id>'
```

部署中继、域名与 TLS、iOS 构建与配对、故障排查见
[iOS 远程控制与中继部署](relay-deploy.md)。设计出处见
[手机远程访问设计](superpowers/specs/2026-10-02-mobile-remote-access-design.md)（Tier 1）。
