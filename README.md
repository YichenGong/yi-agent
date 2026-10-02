# yi-agent

用 Rust 写的 AI 编码助手。在终端里用自然语言指挥它读代码、改代码、跑命令。

- **终端优先** —— 全屏 TUI，Markdown 渲染、斜杠命令、token 与花费实时可见
- **轻量** —— 单个原生二进制，无运行时依赖；另有常驻守护进程与桌面 App
- **可控** —— 危险操作先弹窗确认，内置只读 / 工作区写入 / 完全放行三档沙箱

[English](README.en.md)

---

## 它能做什么

- 读懂并修改你的代码库：读文件、写文件、精确替换、按模式全局搜索
- 执行 Shell 命令；长任务可放到后台托管，随时查看输出、随时终止
- 联网查资料（WebFetch / WebSearch）
- 通过 MCP 接入外部工具，把 GitHub、数据库、内部系统接进来
- 加载 Skills，把你自己的操作手册变成它能照着执行的技能
- 派出子 agent 并行干活，实时盯着它的进展，可以给它发消息或叫停
- 对话超出上下文时自动压缩，长会话不会因为爆窗口中断

## 安装

### Homebrew（macOS / Linux）

```bash
brew install YichenGong/yi-agent/yi-agent
```

### npm

```bash
npm install -g @yi-agent/yi-agent
```

npm 包内含 macOS（Intel / Apple Silicon）与 Linux（x64）的预编译二进制，安装后无需编译。

### 直接下载

到 [Releases](https://github.com/YichenGong/yi-agent/releases) 下载对应平台的
`.tar.gz`，解压后把 `yi-agent` 放进 `PATH`。

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

## 快速开始

**第一步，给它一个模型 API key。**

```bash
export MODEL_API_KEY=sk-ant-...
```

默认走 Anthropic。要用 OpenAI 或其他兼容服务，见下方的「换模型」。

**第二步，进入你的项目目录，直接运行。**

```bash
cd /path/to/your/project
yi-agent
```

**第三步，用中文说话就行。**

```
> 这个项目的测试怎么跑？先看看 README 和 CI 配置
```

它会自己读文件、找答案，需要执行命令时弹窗问你一次。

> 每次开新终端都要重新 `export` 比较麻烦。想持久化，见「配置放在哪」。

## 三种用法

### 1. 交互式 TUI（推荐）

```bash
yi-agent
```

就是上面的用法。几个顺手的操作：

| 操作 | 作用 |
| --- | --- |
| 输入 `/` | 唤出命令菜单：`/help` `/model` `/cost` `/clear` `/compact` `/config` `/runtime` `/mcp` `/quit` |
| `Ctrl+P` | 打开运行面板：Bash 任务 / 托管进程 / 子 agent 轨迹 |
| `Esc` | 打断正在跑的这一轮（不会退出程序） |
| `Ctrl+C` 两次 | 退出 |

`/cost` 看这个会话花了多少；`/compact` 手动压缩历史；会话太长时它会自动压缩。

### 2. 非交互模式（脚本 / CI）

给它一句话，跑完就退出，适合放进脚本或流水线：

```bash
yi-agent run "把 src/ 下所有的 TODO 注释汇总成一个清单"

# 输出 JSONL（每行一个事件），方便程序化处理
yi-agent run --json "统计这个仓库有多少行 Rust 代码" | jq -r 'select(.AssistantText) | .AssistantText'

# 从管道拿输入
echo "解释一下 Makefile 里的 deploy 目标" | yi-agent run
```

### 3. 桌面 App（macOS）

原生窗口，对话、工具调用卡片、审批弹窗、子 agent 都在图形界面里。安装与开发方式见
[desktop/README.md](desktop/README.md)。

## 远程连接（实验性）

`app-server` 支持通过 WebSocket 提供服务，供网络客户端连接。**每条连接都要用已配对
设备 token 认证**（`ws://host/ws?token=<t>` 或 `Authorization: Bearer <t>`；无/错
token 以 ws close `4401` 拒绝），因此可多客户端同时连接：

```bash
yi-agent app-server --listen ws://127.0.0.1:8790   # 绑定非回环时会打印认证告警
```

同一批 session 可在多台设备上查看与控制：配对（一次性码 `XXXX-XXXX`、5 分钟有效，
新设备默认 `control` scope）、设备列表/撤销、审批广播到所有已连接设备。

> **实验性**：配对链路已**端到端打通**——`pair/create` 铸出的码**落盘**到
> `~/.yi-agent/pairing.json`，因此桌面 stdio 进程铸的码可被 `--relay`/`ws://` 进程
> 兑换；iOS 首启有配对表单（填中继地址 + 码），桌面设置里有「远程访问」页可铸码与
> 管理设备。已知限制：暂**无二维码扫描**（手输码）、**无可安装的 iOS 产物**（受 Xcode
> 运行时/签名阻塞）。详见 [iOS 远程控制与中继部署](docs/relay-deploy.md)。

**iOS App 经自建反向 WSS 中继**接入（两端都只发出站连接，电脑侧不开放入站端口）：

```bash
yi-agent app-server --relay 'wss://relay.example.com/connect?session=<id>'
```

部署中继、域名与 TLS、iOS 构建与配对、故障排查见
[iOS 远程控制与中继部署](docs/relay-deploy.md)。设计出处见
[手机远程访问设计](docs/superpowers/specs/2026-10-02-mobile-remote-access-design.md)（Tier 1）。

## 配置放在哪

按优先级从低到高：

1. 默认值
2. 全局配置 `~/.yi-agent/.env`
3. 项目配置 `<项目目录>/.yi-agent/.env`（覆盖全局）
4. 命令行参数 / 环境变量（优先级最高）

配置文件不会写进项目根目录，而是放在 `.yi-agent/.env`，不会污染你的仓库。

**懒人做法**：跑 `yi-agent web`，在浏览器里（默认 <http://127.0.0.1:7292>）图形化填写，
它会把配置写进上面的文件。

## 常用配置

| 想做什么 | 环境变量 | 命令行参数 |
| --- | --- | --- |
| API key | `MODEL_API_KEY` | `--api-key` |
| 换服务商 | `YI_AGENT_PROVIDER`（`anthropic` / `openai`） | `--provider` |
| 换模型 | `YI_AGENT_MODEL` | `--model` |
| 自定义 API 地址 | `MODEL_API_URL` | `--api-url` |
| 指定工作目录 | `YI_AGENT_WORKDIR` | `--workdir` |
| 单轮最大步数 | `YI_AGENT_MAX_TURNS`（默认 200） | `--max-turns` |
| 压缩触发阈值 | `YI_AGENT_COMPACT_RATIO`（默认 80，按上下文百分比） | `--compact-ratio` |
| 联网搜索 | `BOCHA_API_KEY` | —— |

**换模型**，比如用 OpenAI：

```bash
yi-agent --provider openai --model gpt-4o
```

**关于权限确认。** 默认每个危险操作都要你点一次确认。这是启动时的开关：

- `--yolo`（或 `--dangerously-skip-permissions`）：跳过确认。仅建议在容器或一次性环境里用
- `--sandbox read-only`：整个进程只读，改不了任何文件
- `--sandbox workspace-write`：只能改工作目录，网络受限

黑名单命令在任何模式下都会被拦下。

**关于子 agent 运行时。** 首次在 TUI 里派生子 agent 时它会问你，之后用
`/runtime always` / `/runtime never` / `/runtime ask` 决定是否自动启动本地运行时。

## 项目结构与进展

代码在 `yi-agent-rs/`（Rust workspace，11 个 crate），桌面端在 `desktop/`。
每个模块的完成度、验证命令和维护规则见
[docs/project-management/](docs/project-management/README.md)。

<details>
<summary>模块索引（开发参考）</summary>

| 模块 | 完成 / 总计 | 详情 |
| --- | --- | --- |
| yi-agent-core | 20 / 21 | [详情](docs/project-management/yi-agent-core.md) |
| yi-agent-llm | 6 / 9 | [详情](docs/project-management/yi-agent-llm.md) |
| yi-agent-tools | 14 / 14 | [详情](docs/project-management/yi-agent-tools.md) |
| yi-agent-skills | 8 / 8 | [详情](docs/project-management/yi-agent-skills.md) |
| yi-agent-tui | 30 / 32 | [详情](docs/project-management/yi-agent-tui.md) |
| yi-agent-run | 10 / 10 | [详情](docs/project-management/yi-agent-run.md) |
| yi-agent-cli | 1 / 1 | [详情](docs/project-management/yi-agent-cli.md) |
| yi-agent-web | 6 / 6 | [详情](docs/project-management/yi-agent-web.md) |
| permission | 9 / 9 | [详情](docs/project-management/permission.md) |
| ci-cd | 11 / 13 | [详情](docs/project-management/ci-cd.md) |
| tooling | 3 / 3 | [详情](docs/project-management/tooling.md) |
| yi-agent-mcp | 1 / 1 | [详情](docs/project-management/yi-agent-mcp.md) |
| yi-agent-store | 4 / 5 | [详情](docs/project-management/yi-agent-store.md) |
| yi-agent-runtime | 11 / 11 | [详情](docs/project-management/yi-agent-runtime.md) |
| yi-agent-subagent | 5 / 5 | [详情](docs/project-management/yi-agent-subagent.md) |
| yi-agent-app-server | 24 / 24 | [详情](docs/project-management/yi-agent-app-server.md) |
| subagent-runtime | 42 / 49 | [详情](docs/project-management/subagent-runtime.md) |
| desktop | 30 / 43 | [详情](docs/project-management/desktop.md) |

</details>

## 更多文档

- 已知问题：[docs/bug-list.md](docs/bug-list.md)
- 设计文档：[docs/plans/](docs/plans/)
- 参与开发：[CLAUDE.md](CLAUDE.md)（分支、commit、测试规范）

## 许可证

MIT
