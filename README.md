# yi-agent

用 Rust 写的 AI 编码助手。在终端里用自然语言指挥它读代码、改代码、跑命令。

- **终端优先** —— 全屏 TUI，Markdown 渲染、斜杠命令、token 与花费实时可见
- **轻量** —— 单个原生二进制，无运行时依赖
- **可控** —— 危险操作先弹窗确认，内置只读 / 工作区写入 / 完全放行三档沙箱

[English](README.en.md)

---

## 安装

```bash
npm install -g @yi-agent/yi-agent
```

需要 Node.js 14 或更高。包内含 macOS（Intel / Apple Silicon）与 Linux（x64）的
预编译二进制，装完无需编译。

macOS / Linux 也可以用 Homebrew：

```bash
brew install YichenGong/yi-agent/yi-agent
```

> 国内网络建议显式指定官方源 `--registry=https://registry.npmjs.org`。第三方镜像
> 同步有延迟，缺平台子包时会报 `Platform package ... not installed`。

桌面 App、内网 dmg、从源码构建等其它安装方式见
[使用指南](docs/guide.md#其余安装方式)。

## 快速开始

```bash
cd /path/to/your/project
yi-agent
```

首次运行会引导你填 API 密钥与模型标识，测通后写入 `~/.yi-agent/models.json`。
之后直接用中文说要做什么：

```
> 这个项目的测试怎么跑？先看看 README 和 CI 配置
```

它会自己读文件、找答案，需要执行命令时弹窗问你一次。

## 核心功能

- **读写代码** —— 读文件、写文件、精确替换、按模式全仓搜索
- **执行命令** —— 跑 Shell；长任务转后台托管，随时看输出、随时终止
- **联网查资料** —— WebFetch / WebSearch
- **MCP** —— 把 GitHub、数据库、内部系统接进来。直接说「帮我加一个 github MCP」
  即可，它会写配置并当场探测
- **Skills** —— 把你自己的操作手册变成它能照着执行的技能
- **子 agent** —— 派出去并行干活，实时看进展，可以发消息或叫停
- **自动压缩** —— 长会话超出上下文时自动压缩，不会中途中断

## 配合 superpowers 使用（推荐）

[superpowers](https://github.com/obra/superpowers) 是一套把工程流程写成 skill 的
开源库：先头脑风暴确认需求，再写 spec 和 plan，然后 TDD 实现，最后代码评审与验证。
yi-agent 能直接加载它。

克隆下来，把里面的 skill 目录复制进 skill 根（`~/.yi-agent/skills/`）：

```bash
git clone --depth 1 https://github.com/obra/superpowers.git /tmp/superpowers
mkdir -p ~/.yi-agent/skills
cp -r /tmp/superpowers/skills/* ~/.yi-agent/skills/
```

下一轮对话就能看到这些 skill，不用重启：`brainstorming`、`writing-plans`、
`executing-plans`、`test-driven-development`、`systematic-debugging`、
`requesting-code-review`、`using-git-worktrees`、`verification-before-completion`
等 15 个。

之后不用记命令，直接说需求，它多半会先走 brainstorming；也可以明确点名：

```
> 用 brainstorming 帮我把这个想法理清楚
> 用 systematic-debugging 查一下这个测试为什么挂
```

想只在某个项目里用，把 skill 放进 `<项目>/.yi-agent/skills/`。

## 更多

细节都收在 [使用指南](docs/guide.md)：非交互模式、远程连接、配置项与路径、
MCP 手写配置、桌面 App。模块进度见
[docs/project-management/](docs/project-management/README.md)，开发规范见
[CLAUDE.md](CLAUDE.md)。

## 许可证

MIT
