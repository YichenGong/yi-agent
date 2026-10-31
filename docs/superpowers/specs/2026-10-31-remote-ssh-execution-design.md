# 远程 SSH 执行（Remote over SSH）

日期：2026-10-31
状态：待实现
开关：`remote_ssh`（实验功能 Tab 可开关，stable 恒关）

## 1. 要解决的问题

yi-agent 现在的工具层全部是**本地执行**：

- `yi-agent-tools/src/shell/bash.rs` 直接 `tokio::process::Command`
- `yi-agent-tools/src/fs/{read,grep,glob,write,edit,view_image}.rs` 直接 `std::fs`
- `yi-agent-tools/src/process/manager.rs` 直接起本地子进程

结果是：用户在 Mac 上打开 desktop，agent 只能读写 Mac 上的文件、跑 Mac 上的命令。
想让它在远程 Linux 服务器上干活，目前**没有任何路径**。

已有的 `--relay`（`docs/relay-deploy.md`）方向相反：它让**手机连到这台电脑**，
干活的仍然是本机。它解决的是"人在外面也能操作自己电脑"，不是"让 agent 去远端干活"。

本设计补上后者：**本机 desktop 作为操作台，agent 在远端服务器上执行**。

## 2. 关键决策与依据

### 2.1 不引入 SSH 协议库，把 `ssh` 当字节管道

远端跑一个 `yi-agent app-server --listen stdio://`，本机 fork
`ssh <host> <remote-yi-agent> app-server --listen stdio://`，把这条 ssh 子进程的
stdin/stdout 当作 JSON-RPC 通道。

依据（调研 codex 源码 `codex-rs/exec-server/src/environment_toml.rs:428-437`）：
codex 采用完全相同的形态，其配置即

```toml
[[environments]]
id = "ssh-dev"
program = "ssh"
args = ["dev", "codex exec-server --listen stdio"]
```

好处：协议零改动、工具语义天然一致（read/grep/glob 都在远端跑，不存在"远程版行为
对不齐"的问题）、不引入 SSH 依赖、不需要维护两套工具实现。

### 2.2 零安装 vs 要求安装：选择"要求安装 + 自动安装"

另一条路是零安装（每条命令包成 `ssh host 'bash -c ...'`），但代价正好落在 yi-agent
最核心的工具上：`read`/`grep`/`glob`/`view_image` 都直接操作本地路径，做成零安装等于
给每个文件工具单独写一份远程版，且永远和本地版对不齐。选自动安装远端二进制。

### 2.3 一项目一个远端进程

一次 desktop 窗口 = 一条 ssh 子进程 = 一个远端 app-server。切换项目即换 ssh 进程。

依据：VS Code Remote-SSH 的实现（`~/.vscode/extensions/ms-vscode-remote.remote-ssh-0.128.0/out/install-script/scripts/linux-exec-server-installer.sh:452`）
用 `while true; do sleep 180; done` 让安装脚本进程充当 ssh 会话锚点，server 是其子
进程，ssh 断开即一起退出；codex 也是同一形态。好处是完全复用现有 sidecar 模型
（`desktop/src-tauri/src/bridge.rs`），项目隔离天然成立。

反例是多项目共享一个远端常驻服务（codex 的 daemon 路线）：需要处理多客户端会话隔离、
生命周期与版本升级，而 yi-agent 的 app-server 目前是单 stdio 客户端模型
（`ClientId="local"` 为 Admin），复杂度不成比例。

### 2.4 自动安装：内置优先，远端下载兜底

远端需要一个 `yi-agent` 二进制。三种来源，按优先级：

1. **App 内置**：发布打包时把 `x86_64-unknown-linux-gnu` 也放进 App bundle，
   首次连接按远端架构挑对应的一份 scp 上去。远端只需 `sshd`，无外网依赖。
2. **远端下载**：内置包缺失或架构不匹配时，`ssh host 'curl|wget'` 从发布渠道拉。
3. **本机代下**：远端无外网时，本机下载后 scp 过去。

第 3 条来自 VS Code 的实现（`linux-exec-server-installer.sh:181-215` 的
`do_client_download()`：发信号给本机下载，脚本轮询等 `.done` 标记文件）。这条在内网
机器上是决定性的——远端没外网、本机能上外网是很常见的组合。

**不要**用 scp 本机自己的二进制：本机是 `aarch64-apple-darwin`（Mach-O），
Linux 服务器是 `x86_64-unknown-linux-gnu`（ELF），跨架构直接不可执行。

### 2.5 按版本分目录，不覆盖单文件

安装路径 `~/.yi-agent/bin/<version>/yi-agent`。多台机器、多版本并存不打架；清理就是
"保留最近 N 个"。VS Code 按 commit hash 分目录并删掉第 6 个以后的旧版本
（`linux-exec-server-installer.sh:174`），同理。

## 3. 架构

```
本机 desktop (Tauri)                              远端 Linux
  React 前端                                          │
     │ invoke("rpc", ...)                             │
     ▼                                               │
  bridge.rs  ──写 stdin──▶ ssh 子进程 ────────────────▶│ yi-agent app-server --listen stdio://
                ◀─读 stdout──                            │   (cwd = 远端项目路径)
                                                       │
                                                       ├─ bash  → 远端 shell
                                                       ├─ read/grep/glob → 远端 fs
                                                       └─ models.json → 用户自备 key
```

前端**完全不知道**自己在跟本地还是远端说话——它只依赖 JSON-RPC 线协议
（`desktop/src/lib/protocol.ts`）。这是本设计最重要的性质：所有改动都在桥接层以下。

### 3.1 组件划分

全部改动集中在 **desktop 的 Tauri 桥接层**（`desktop/src-tauri/`），
`yi-agent-app-server` 一行不改。理由见 §3.3。

| 组件 | 职责 | 位置 | 依赖 |
|---|---|---|---|
| `remote/profile.rs`（新增） | 远端配置读写与校验：host、远端项目路径、二进制版本。存 `$YI_AGENT_HOME/remote.json` | `desktop/src-tauri/src/` | 无（纯逻辑，好测） |
| `remote/install.rs`（新增） | 探测远端架构、选包、安装、校验版本 | 同上 | `ssh`、内置包/发布渠道 |
| `remote/rpc.rs`（新增） | `remote/*` 的 Tauri 命令：由前端直接 `invoke`，不经 sidecar | 同上 | 上述两者 |
| `bridge.rs`（改） | 按当前 profile 决定起本地 sidecar 还是 ssh 子进程 | 同上 | 上述两者 |
| `yi-agent-flags`（改） | 注册 `remote_ssh` | `yi-agent-rs/crates/yi-agent-flags/` | — |
| 前端（改） | 「远程」设置页、进度展示、开关联动 | `desktop/src/` | 只走 Tauri `invoke` |

### 3.3 为什么 `remote/*` 不能放进 app-server（关键）

app-server 是**被 ssh 隧道运送的那一端**。把「怎么连远端」放进 app-server，等于让
远端进程自己决定怎么连自己——职责颠倒。而且开关关闭时远端在跑旧二进制，可能根本
不认识这些方法。

因此 `remote/*` 走 Tauri 的 `invoke` 命令（与已有的 `rpc`、`rpc_respond`、
`restart_sidecar` 同一层，见 `desktop/src-tauri/src/bridge.rs:62`（`rpc`）、`:76`（`rpc_respond`）、`:345`（`restart_sidecar`）），
不经过 sidecar 的 JSON-RPC。前端已有两条通道：`invoke` 给本地桥接命令，
JSON-RPC 给 agent。

`flags` 的读取也分散在两处，需要各自实现，这是既有架构的既成事实，不是本设计引入的：
- **桥接层**（Tauri 命令侧）：需要自己读 `flags.json`，或由前端把 `flags/list`
  的结果一并传下来
- **app-server 侧**：本设计没有新增需求，`flags/*` 维持现状

### 3.2 为什么安装逻辑放 Rust 侧而不是前端

安装涉及 `ssh` 子进程、文件传输、进度反馈，全部是桥接层的活。放前端等于让
webview 直接调 shell，既危险也不好测。前端只发 `remote/install` 请求、收进度事件。

## 4. 数据流

### 4.1 首次连接

1. 用户在设置里填 host（如 `dev@10.0.0.5`）与远端项目路径
2. 点「连接」，前端发 `remote/connect`
3. Rust：`ssh <host> 'uname -sm'` 探测架构
4. 查 `~/.yi-agent/bin/<version>/` 是否已存在且 `--version` 匹配
5. 不存在则安装（§2.4 的三级优先），进度以 `remote/install/progress` 事件回推
6. 安装完起 `ssh <host> '<path> app-server --listen stdio://'`，接管 stdio
7. 前端拿到 `remote/status` 的 `connected`，正常开始对话

### 4.2 日常连接

直接跳第 4 步之后。**不重复探测架构**——profile 里缓存上次探测结果，架构不会变。

### 4.3 断线

ssh 子进程退出 → `bridge.rs` 现有的重启循环接管（`bridge.rs:96-99` 的
`next_restart_delay`，200ms 起、上限 6.4s）。语义与今天 sidecar 崩溃一致。

**注意**：远端 app-server 进程随 ssh 会话一起死（§2.3），所以重连等于重新起进程，
**会话状态不保留**。这与本地 sidecar 崩溃的现状一致，不在本次范围内改进。

## 5. 实验开关

注册项（`yi-agent-flags/src/registry.rs`）：

```rust
pub const REMOTE_SSH_FLAG: &str = "remote_ssh";

FlagDef {
    name: REMOTE_SSH_FLAG,
    summary: "远程执行：让 agent 在 SSH 可达的服务器上读写文件、运行命令",
    owner: "yichen",
    expires_on: "2027-06-30",
    default: false,
}
```

### 5.1 开关的作用点

**必须作用在"整个功能"上**，不是"部分 UI 可见性"。关闭时必须同时满足：

1. 设置页的「远程」入口不渲染
2. `remote/*` 的 Tauri 命令返回错误（与 `flags_rpc.rs:36` 对 stable 的处理同构：
   **命令不存在**，而不是"存在但拒绝"）
3. 已连接的 ssh 子进程被终止，回落到本地 sidecar

第 3 条容易被漏掉：如果只在 UI 隐藏、不杀进程，用户会处于"看不见但远端还在跑、
还在烧 token"的状态。

### 5.1.1 谁能判定 flag 值（易错点）

`remote_ssh` 是**本机**的开关，管的是"本机要不要去连远端"。因此**不能**靠
`flags/list` RPC 判定——那个 RPC 经 sidecar 转发，一旦已经连上远端，它读到的
是**远端机器**的 `flags.json`，与本机开关无关。更糟的是远端在跑自己的二进制，
本机用户根本没在远端打开过这个开关。

判定必须在**本机桥接层**完成，二选一：

- **A（推荐）**：桥接层直接调 `yi-agent-flags::is_enabled("remote_ssh")`。
  desktop 的 `Cargo.toml` 增加对 `yi-agent-flags` 的依赖即可，它本来就是
  workspace 里的独立 crate，无重依赖。
- **B**：前端单独开一个 Tauri 命令 `remote/enabled`，由桥接层用它自己的判定返回
  布尔值，前端据此渲染。

两条路都必须保证前端**永远不**从 sidecar 的 `flags/list` 里读 `remote_ssh`。

### 5.2 stable 构建的语义

`store.rs:75` 的 `status_at` 第一行 `if build_kind != "dev" { return (false, Source::BuildKind) }`
已经保证 stable 恒关。**不需要额外代码**，但需要一条测试钉住它。

### 5.3 关掉开关后的数据处理

不删已保存的 `remote.json`，也不删远端已安装的二进制。理由与看板 flag 一致：
flag 是闸门，不是数据清除。重新打开即恢复。

## 6. 安全边界

1. **不把本机 key 传到远端**。远端 `models.json` 由用户自己配（与今天"每台机器
   各自配置模型"的现状一致，`yi-agent-runtime/src/config.rs` 的 `models.json` 是
   唯一真相源）。本设计**不做**凭据转发——那会引入一个新的密钥分发通道，
   远超本次范围。
2. **host 白名单校验**：`remote.json` 里的 host 必须匹配
   `^[A-Za-z0-9._-]+(@[A-Za-z0-9._-]+)?$`，拒绝任何含空格、引号、分号的输入。
   防命令注入——host 会被拼进 ssh 命令行。
3. **远端路径同样校验**：必须是绝对路径且不含 shell 元字符。
4. **ssh 参数固定**：不用 `-o ProxyCommand` 之类可由用户配置的项，让 ssh 走用户
   自己的 `~/.ssh/config`。连接类命令一律加 `BatchMode=yes`，防止卡在交互式提示
   （连不上就快速失败，而不是让 UI 无限等待）。
   **例外**：`BatchMode=yes` 与 `StrictHostKeyChecking=accept-new` 组合时，遇到
   完全未知的 host 不会自动接受，而是直接失败。因此**首次信任**必须是一个独立的、
   明确的用户动作：UI 上显示待信任指纹，用户点确认后，用一次**不带 `BatchMode`**
   的 `ssh-keyscan` 式流程完成信任，此后所有命令回到 `BatchMode=yes`。

## 7. 不做什么（非目标）

- **不做远端常驻 daemon**（§2.3 已论证）
- **不做凭据转发**（§6.1）
- **不做跨会话状态保留**（§4.3）
- **不做零安装模式**（§2.2）
- **不做 Windows 远端**：暂只支持 Linux/macOS 远端。Windows 的路径语义、shell 差异
  是独立的一大块，本期不碰。
- **不改工具层**：所有工具走远端 app-server 内部执行，`yi-agent-tools` 一行不改。
  这是本设计最省力的地方，也是最容易被破坏的地方——见 §8.3。

## 8. 风险

### 8.1 版本漂移

远端二进制的版本与本机不一致时，RPC 协议可能有差异。缓解：安装后强制
`--version` 校验，不匹配则重装；profile 里记录远端版本，连接时比对。

### 8.2 App 体积

内置 Linux 包会让 App 从约 51MB 涨到约 90MB（`x86_64-unknown-linux-gnu` 产物
38.8MB）。这是一个显式的取舍，换来"远端零依赖"。若体积不可接受，可以退化为
"首次使用时下载而不是内置"，但那会把第 1 优先级降级为第 2。

### 8.3 工具层被"顺手"改成远程感知

最容易犯的错：为了"让远程更快"，有人在 `yi-agent-tools` 里加
`if remote { ssh ... }` 分支。那会立刻产生两套执行路径、两套行为。**本设计的
前提是工具层完全无感知**——所有远端差异止于 ssh 子进程的 stdio。实施 review 时
需要守住这条线。

### 8.4 host key 首次确认

第一次连某个 host，ssh 会问 `yes`。若直接用 `BatchMode=yes`，连接会以
"Host key verification failed" 静默失败，用户只看到一句莫名的错误。

这是本设计里最容易做成"玄学 bug"的一处，必须显式设计（见 §6.4 的例外条款）：
把 host key 信任做成**用户可见的一步**，而不是靠参数蒙混过去。具体做法：
先用 `ssh-keyscan <host>` 取指纹并与 UI 上展示的指纹比对（用户可从服务器控制台
自行核实），确认后写入 `~/.ssh/known_hosts`，之后回归 `BatchMode=yes`。

不做这件事的后果是：用户第一次连不上，且不知道原因。

## 9. 验收判据

每条都可用命令验证，不写主观描述。

1. **flag 已注册且默认关**：`cargo test -p yi-agent-flags` 含
   `the_remote_ssh_flag_is_registered`
2. **stable 恒关**：`YI_AGENT_BUILD_KIND=release cargo build -p yi-agent` 后
   `./target/debug/yi-agent flags status remote_ssh` 报 `source: build-kind`
3. **开着时命令存在、关着时不存在**：`cd desktop/src-tauri && cargo test` 含
   `remote_command_is_absent_when_the_flag_is_off`
4. **host 注入被拒**：`cd desktop/src-tauri && cargo test remote::` 含
   `a_host_with_a_semicolon_is_rejected`
5. **端到端**：真机 SSH 到一台 Linux，desktop 里开 flag → 配 host → 连接 →
   agent 执行 `hostname` 返回的是**远端主机名**而非本机
6. **关开关即断开**：连接状态下关掉 flag，`ps aux | grep ssh` 中该 ssh 子进程消失
7. **工具层零改动**：`git diff --stat main -- yi-agent-rs/crates/yi-agent-tools`
   为空

## 10. 实施顺序

1. flag 注册 + stable 闸门测试（最小可验证单元，先落地）
2. `remote.json` 的读写与校验（纯逻辑，无 IO 依赖，好测）
3. 安装逻辑（探测架构 → 选包 → 安装 → 校验版本），先只做"本机代下"一条路径
4. `bridge.rs` 接入：按 profile 起 ssh 子进程
5. 前端设置页 + 进度事件
6. 补齐"内置包优先"与"远端下载"两条路径
7. 关开关时杀进程
8. 端到端真机验证

第 3 步先只做一条路径是刻意的：三条安装路径里，"本机代下"最容易验证（不依赖远端
网络也不依赖打包流程），先用它打通整条链路，再补另外两条。
