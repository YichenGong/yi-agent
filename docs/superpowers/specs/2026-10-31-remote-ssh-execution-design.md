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

依据（调研 codex 源码）：codex 采用完全相同的形态。其 environment 配置结构
（`codex-rs/exec-server/src/environment_toml.rs:41-57` 的 `EnvironmentToml`）包含：

```rust
program: Option<String>,        // 可执行程序，如 "ssh"
args: Option<Vec<String>>,      // ["dev", "codex exec-server --listen stdio"]
env: Option<HashMap<String, String>>,
cwd: Option<PathBuf>,           // 绝对路径，spawn 时直接作为子进程 cwd
connect_timeout_sec: Option<Duration>,
initialize_timeout_sec: Option<Duration>,
```

四件值得直接借鉴的事：

1. **`cwd` 是 spawn 参数**，不是拼进 args 的 `cd`（`environment_toml.rs:190-196`
   把 `cwd` 传给 spawn，`:215-231` 的 `normalize_stdio_cwd` 强制它必须是绝对路径）。
   这比 shell 拼接更安全——无需引号、天然免疫注入。本设计采用这个思路，
   而不是 `cd xxx &&` 的写法（见 §4.4 的修订）。
2. **`env` 可注入环境变量**。本设计用它传 `YI_AGENT_HOME` 做状态隔离（§4.6）。
3. **每个 environment 有独立的连接与初始化超时**（§4.1 的超时表参照此项）。
4. codex 还有一个明确的区分：**recovery（瞬断后恢复同一会话）vs refresh
   （执行器被替换后建新会话）**，分别实现在 `client_recovery.rs` 与
   `client_refresh.rs`。本设计不做会话恢复（§4.3、§7），因此只需要
   recovery 的"重启进程"语义，不需要 refresh。这个区分记在这里是为了避免
   实现时误抄 refresh 的复杂度。

好处：协议零改动、工具语义天然一致（read/grep/glob 都在远端跑，不存在"远程版行为
对不齐"的问题）、不引入 SSH 依赖、不需要维护两套工具实现。

### 2.2 零安装 vs 要求安装：选择"要求安装 + 自动安装"

另一条路是零安装（每条命令包成 `ssh host 'bash -c ...'`），但代价正好落在 yi-agent
最核心的工具上：`read`/`grep`/`glob`/`view_image` 都直接操作本地路径，做成零安装等于
给每个文件工具单独写一份远程版，且永远和本地版对不齐。选自动安装远端二进制。

### 2.3 一项目一个远端进程（并行）

每个远端项目一条独立 ssh 子进程，**可同时存在多条**。项目之间完全隔离：各自的
app-server、各自的工作目录、各自的会话状态、各自的重连退避。

依据：VS Code Remote-SSH 的实现（`~/.vscode/extensions/ms-vscode-remote.remote-ssh-0.128.0/out/install-script/scripts/linux-exec-server-installer.sh:452`）
用 `while true; do sleep 180; done` 让安装脚本进程充当 ssh 会话锚点，server 是其子
进程，ssh 断开即一起退出；codex 也是同一形态。好处是完全复用现有 sidecar 模型
（`desktop/src-tauri/src/bridge.rs`），项目隔离天然成立。

反例是多项目共享一个远端常驻服务（codex 的 daemon 路线）：需要处理多客户端会话隔离、
生命周期与版本升级，而 yi-agent 的 app-server 目前是单 stdio 客户端模型
（`ClientId="local"` 为 Admin），复杂度不成比例。

**注意这里区分两件事**：
- **共享一个远端进程**跑多个项目 → 不采用（要改 app-server 的多客户端模型）
- **每个项目各自一个远端进程、彼此并行** → 采用（只需改桥接层，见 §4.6）

并行不是免费的：现有桥接层是单例，必须重写成多槽位。这是本设计里工作量最大、
也最容易引入隐蔽 bug 的一块，§4.6 给出完整方案。

### 2.4 自动安装：惰性下载 + 本地缓存，App 不内置

远端需要一个 `yi-agent` 二进制。**不把 Linux 二进制打包进 App。**

理由有两条，第二条是决定性的：

1. **体积**：App 现在 51MB（Tauri 壳 5.5MB + macOS 版 yi-agent 44MB）。
   塞入 `x86_64-unknown-linux-gnu` 原图（实测 37.41MB）会涨到约 90MB，**+76%**。
2. **这个功能在 stable 永远关着**（§5.3 的硬闸），所以内置的体积对绝大多数用户
   是纯粹的死重。为默认关闭的功能让所有用户永久多付几十 MB，不划算。

#### 2.4.1 实测压缩率（本机真实产物，非估算）

`target/x86_64-unknown-linux-gnu/release/yi-agent`（已 stripped，37.41MB）：

| 压缩 | 体积 | 相对原图 |
|---|---|---|
| 无 | 37.41 MB | — |
| `gzip -9` | 14.91 MB | -60% |
| `zstd -19` | 9.93 MB | -73% |
| `xz -9e` | **8.95 MB** | **-76%** |

即便压缩到 8.95MB，"内置 xz 包"仍不如"不内置"——省的是用户一次性 15MB 下载，
代价是**所有用户永久多付 9MB**。用的人付一次，比所有人一直付更合理。

#### 2.4.2 三条路径，按优先级

1. **本机下载 → scp → 远端解压**（主路径）
   本机从发布渠道取 `yi-agent-x86_64-unknown-linux-gnu.tar.gz`（14.91MB），
   缓存到 `~/.yi-agent/remote-cache/<version>/`，scp 到远端 `/tmp`，
   再由**远端** `tar -xzf` 解压到 `~/.yi-agent/bin/<version>/`。
   缓存命中则不重复下载；缓存跨 App 重装存活（在 `~/.yi-agent` 里）。

   **解压在远端做**是刻意的：Linux 一定有 `tar`，本机因此不需要引入任何
   xz/gzip 解压依赖，Rust 侧只负责「下载」和「scp」两个动作。

2. **远端直下**（远端有外网时）
   `ssh host 'curl|wget <url>'` 直接拉，省本机带宽。仅当远端确实能连通时启用，
   失败立刻回落到路径 1。

3. **`--from` 手动指定**（气隙 / 特殊环境）
   `yi-agent remote install --from /path/to/binary`：用户自己提供同架构二进制。
   零体积代价，且是显式动作。

#### 2.4.3 为什么这个取舍成立

"远端无外网"这个顾虑比看起来弱：agent 在远端跑、LLM 调用也从远端发出，
**远端连不上 LLM API 时这个功能本就不成立**（除非跑远端本地模型）。所以
"远端必须有一定网络能力"本来就是前提，下载 15MB 不构成新的门槛。

真正需要兜底的是"远端连不了发布渠道但能连别的网络"或完全气隙——由路径 2/3 覆盖。

#### 2.4.4 不要 scp 本机自己的二进制

本机是 `aarch64-apple-darwin`（Mach-O），Linux 服务器是
`x86_64-unknown-linux-gnu`（ELF），跨架构直接不可执行。只在**同架构**场景
（如 macOS 连 macOS）下才允许复用本机二进制。

#### 2.4.5 支持矩阵：只有三个预编译 target，必须显式拒绝其余

`.github/workflows/release.yml:84` 的发布矩阵**只有三个 target**：

| target | 产物 |
|---|---|
| `aarch64-apple-darwin` | macOS Apple Silicon |
| `x86_64-apple-darwin` | macOS Intel |
| `x86_64-unknown-linux-gnu` | Linux x86_64 |

命名规则由 `justfile:202-212` 的 `package-target` 钉死：
`dist/yi-agent-<target>.tar.gz` + `dist/yi-agent-<target>.tar.gz.sha256`。

**因此本设计只支持三类远端**：macOS ARM、macOS x86、Linux x86_64。

必须明确拒绝并给出可读错误的是：

- **`aarch64-unknown-linux-gnu`（ARM Linux）**：发布矩阵里**没有**这个产物。
  很多云主机/Raspberry Pi 是 ARM Linux，会直接撞上。若不显式处理，用户看到的是
  一个 404 或"下载失败"，误以为是网络问题
- `armv7`、`musl`（Alpine）、Windows、FreeBSD 等：同样不在矩阵内

**错误文案必须指出真实原因**：不是"网络不通"，而是"远端架构 `<arch>` 目前没有
预编译产物"，并给出两条出路：改在 x86_64 上运行，或用路径 3（`--from <本机构建>`）
自行交叉编译。这两条恰好是本设计已有的路径，不需要新机制。

**这条约束要写进 `remote_arch` 的校验**（§4.9）：探测到的 arch 不在上表内时，
在**连接前**就失败，而不是先 scp 再发现不对。

### 2.5 按版本分目录，不覆盖单文件

安装路径 `~/.yi-agent/bin/<version>/yi-agent`。多台机器、多版本并存不打架；清理就是
"保留最近 N 个"。VS Code 按 commit hash 分目录并删掉第 6 个以后的旧版本
（`linux-exec-server-installer.sh:174`），同理。

## 3. 架构

```
本机 desktop (Tauri)                                    远端 Linux
  React 前端                                                │
     │ invoke("rpc", {slot, id, method, params})            │
     ▼                                                      │
  bridge.rs  按 slot 查进程表                                │
     ├─ slot=local  ──▶ 本地 sidecar（1 个）                  │
     ├─ slot=A      ──▶ ssh A ──────────────────────────────▶│ app-server (cwd=/home/dev/projA)
     └─ slot=B      ──▶ ssh B ──────────────────────────────▶│ app-server (cwd=/home/dev/projB)
                                                             │
                                                             ├─ bash  → 远端 shell
                                                             ├─ read/grep/glob → 远端 fs
                                                             └─ models.json → 用户自备 key
```

多条 ssh 子进程并行存在，各自独立监督与退避。事件按 slot 分开命名
（`app-server://message/{slot}`），互不串扰。

前端**完全不知道**自己在跟本地还是远端说话——它只依赖 JSON-RPC 线协议
（`desktop/src/lib/protocol.ts`）。这是本设计最重要的性质：所有改动都在桥接层以下。

### 3.1 组件划分

改动集中在 **desktop 的 Tauri 桥接层**（`desktop/src-tauri/`）。
`yi-agent-app-server` **只有一处**改动：新增只读 RPC `remote/relayTargets`
（§4.6，手机经中继够不到 Tauri 层，只能由 app-server 提供）。除此之外不动
app-server，理由见 §3.2。

| 组件 | 职责 | 位置 | 依赖 |
|---|---|---|---|
| `remote/profile.rs`（新增） | 远端配置读写与校验。存 `<本机 instance_home>/remote.json`（schema 见 §4.9） | `desktop/src-tauri/src/` | 无（纯逻辑，好测） |
| `remote/install.rs`（新增） | 探测远端架构、下载与缓存 tar.gz、scp、触发远端解压、校验版本 | 同上 | `ssh`、发布渠道、本机缓存 |
| `remote/rpc.rs`（新增） | `remote/*` 的 Tauri 命令：由前端直接 `invoke`，不经 sidecar | 同上 | 上述两者 |
| `bridge.rs`（改） | **单例改为按 slot 分键的进程表**；按 slot 起本地 sidecar 或 ssh 子进程；事件按 slot 命名 | 同上 | 上述两者 |
| `yi-agent-flags`（改） | 注册 `remote_ssh` | `yi-agent-rs/crates/yi-agent-flags/` | — |
| 前端（改） | 「远程」设置页、进度展示、开关联动 | `desktop/src/` | 只走 Tauri `invoke` |

### 3.2 为什么 `remote/*` 不能放进 app-server（关键）

app-server 是**被 ssh 隧道运送的那一端**。把「怎么连远端」放进 app-server，等于让
远端进程自己决定怎么连自己——职责颠倒。而且开关关闭时远端在跑旧二进制，可能根本
不认识这些方法。

因此 `remote/*` 走 Tauri 的 `invoke` 命令（与已有的 `rpc`、`rpc_respond`、
`restart_sidecar` 同一层，见 `desktop/src-tauri/src/bridge.rs:62`（`rpc`）、`:76`（`rpc_respond`）、`:345`（`restart_sidecar`）），
不经过 sidecar 的 JSON-RPC。前端已有两条通道：`invoke` 给本地桥接命令，
JSON-RPC 给 agent。

**唯一的例外：`remote/relayTargets`**。上面这条原则的依据是"调用方是桌面 UI，
走得到 Tauri"。但手机经"中继 → 本机 app-server"访问，链路上没有 Tauri 桥接层，
因此手机要拿 session 清单**只能**由 app-server 提供。这是本设计中唯一放进
app-server 的 `remote/*` 方法，且它是**只读**的（返回几个字符串），不碰连接逻辑。
详见 §4.6。

`flags` 的读取也分散在两处，需要各自实现，这是既有架构的既成事实，不是本设计引入的：
- **桥接层**（Tauri 命令侧）：需要自己读 `flags.json`，或由前端把 `flags/list`
  的结果一并传下来
- **app-server 侧**：本设计没有新增需求，`flags/*` 维持现状

### 3.3 为什么安装逻辑放 Rust 侧而不是前端

安装涉及 `ssh` 子进程、文件传输、进度反馈，全部是桥接层的活。放前端等于让
webview 直接调 shell，既危险也不好测。前端只发 `remote/install` 请求、收进度事件。

## 4. 数据流

### 4.1 首次连接

1. 用户在设置里填 host（如 `dev@10.0.0.5`）与远端项目路径
2. 点「连接」，前端发 `remote/connect`
3. Rust：`ssh <host> 'uname -sm'` 探测架构，**并立刻对照支持矩阵**（§2.4.5）。
   只有 `x86_64-unknown-linux-gnu`、`aarch64-apple-darwin`、
   `x86_64-apple-darwin` 三类可继续；其余（尤其 `aarch64-unknown-linux-gnu`）
   在此**停下并给出可读原因**，不要进入下载。这一步与路径校验同为"失败要趁早"
4. **探测远端 home，派生 `instance_root`**：`ssh <host> 'printf %s "$HOME"'`。
   `instance_root = <远端 home>/.yi-agent-projects/<profile.id>/.yi-agent`（§4.9）。

   **为什么要单独探测而不是猜**：`instance_home()` 靠 `os_home()`
   （环境变量 `HOME`/`USERPROFILE`，`store/src/paths.rs:20-23`），本机猜不到远端的值。
   特别注意**不要**用 `remote_path` 反推——项目可能在 `/srv/...`，与 home 毫无关系。

   探测结果为**空**（极少见，如 `HOME` 未设）时必须失败并说明，不要退化成相对路径：
   那会让 `instance_root` 落到 ssh 会话的 cwd 下，状态随项目目录漂移。

5. **校验远端项目路径存在**：`ssh <host> 'test -d <远端项目路径>'`。
   不存在就**立即失败并说明**，不要等到起 app-server 时由
   `resolve_workdir` 报 `working directory does not exist`（§4.4 已说明这是必做步骤，
   因为 app-server 启动即会校验 `is_dir()`）
6. 查 `~/.yi-agent/bin/<version>/` 是否已存在且 `--version` 匹配
7. 不存在则安装（§2.4 的三级优先），进度以 `remote/install/progress` 事件回推
8. 安装完起 ssh 会话，完整命令形如：

   ```bash
   ssh <host> "cd '<远端项目路径>' && exec env YI_AGENT_HOME='<instance_root>' \
     '<bin>' app-server --listen stdio://"
   ```

   两件事分开看，**不要混为一谈**：

   - `cd '<远端项目路径>'` → 决定 app-server 的 **cwd**，即项目目录
     （ssh 没有"指定远端 cwd"的参数，只能靠 `cd`，见 §4.4）
   - `env YI_AGENT_HOME=...` → 决定 **状态目录**。这是**必须显式给**的：
     `instance_home()` 只认 `YI_AGENT_HOME` 与 `$HOME`，**不看 cwd**
     （§4.6 已核实）。只 `cd` 不设 env，所有远端项目会共用
     `$HOME/.yi-agent`，静默互相覆盖状态

   用 `exec` 让 app-server 替换 shell 进程：ssh 断线时信号直达 app-server，
   不留一层 `sh` 挡在中间
9. 前端拿到 `remote/status` 的 `connected`，正常开始对话

**每一步都要有超时，不能无限等**。参考 codex 的做法
（`exec-server/src/environment_toml.rs:53-56` 的 `connect_timeout_sec` /
`initialize_timeout_sec` 是每个 environment 的可配置项）：

| 阶段 | 超时建议 | 超时后的行为 |
|---|---|---|
| ssh 建连（第 3 步） | 20s | 报「无法连接 <host>」，附 ssh stderr |
| home 探测（第 4 步） | 10s | 报「无法确定远端 home」 |
| 路径校验（第 5 步） | 10s | 报「远端路径不可用」 |
| 安装（第 7 步） | 不设总超时，但**每阶段有进度** | 用户可取消 |
| app-server 握手（第 9 步） | 30s | 杀掉子进程，报「远端 app-server 未就绪」 |

没有超时的后果是 UI 永远停在"连接中"，用户既不知道在等什么，也没法重试——
这是这类功能最常见的坏体验。

**并发安装必须互斥**：用户在两台窗口里同时连同一个 `(host, version)` 时，两个流程会
同时下载、同时 scp、同时解压到同一个远端目录，产生半截文件。做法：本机按
`(host, version)` 加一把进程内锁（`OnceCell<Mutex<..>>` 的 map 即可，不需要跨进程）；
远端解压用"先解到临时目录再 rename"的原子替换（与 `workspace_index.rs` 的
temp+rename 同法），避免读到半截二进制。

### 4.2 日常连接

从第 5 步开始。**不重复探测架构**——profile 里缓存上次探测结果，架构不会变。
路径校验（第 4 步）仍然**每次都做**：目录可能被删除或改名，这是廉价的一次 `test -d`。

### 4.3 断线

ssh 子进程退出 → `bridge.rs` 现有的重启循环接管（`bridge.rs:96-99` 的
`next_restart_delay`：200ms 起、按 2 的幂递增，但被 `CAP_MS = 5_000` 截断，
所以**实际上限是 5s**。注意源码注释里的"6.4s"是误导——`200ms << 5 = 6.4s`
随后立刻 `.min(5_000)` 被截回 5s，注释没跟着调整）。

**但远端场景不能无条件沿用本地语义**，两者失败原因不同：

- 本地 sidecar 崩溃通常是瞬时故障，快速重试是对的
- 远端 ssh 失败常见原因是**网络断了 / 服务器关机 / VPN 掉线**，这些会持续数分钟到
  数小时。用 5s 上限无限重试，等于每隔 5s 发起一次 ssh 连接，既刷日志也给服务器
  制造无意义的认证请求

**决策：远端模式给重试加约束**：

1. **指数退避上限抬高**到 60s（本地保持 5s 不变）
2. **连续失败 5 次后停止自动重试**（常量 `REMOTE_MAX_RETRIES = 5`，配合 60s 上限
   约 4 分钟），转为「断线」状态，UI 显示明确原因与「重连」按钮，由用户手动触发
3. 退避与停止逻辑只作用于远端 slot，不改本地 sidecar 行为
4. **每个 slot 的退避与失败计数相互独立**：远端 A 连不上不能让远端 B 停止重试，
   更不能让本地 sidecar 被连带重启（§4.6 的多槽位结构保证了这一点）

**注意**：远端 app-server 进程随 ssh 会话一起死（§2.3），所以重连等于重新起进程，
**会话状态不保留**。这与本地 sidecar 崩溃的现状一致，不在本次范围内改进。

### 4.4 路径语义：本机路径 vs 远端路径（最大技术风险）

这一节是本设计里**最容易踩、且最容易被忽略**的地方，必须在实现前定死。

**问题**：`thread/start` 的 `cwd` 由前端传入
（`desktop/src/App.tsx:956` 的 `threadStartParams(cwd)`），随后在 **app-server 进程内**
被校验：

```rust
// yi-agent-app-server/src/server.rs:7761-7769
let Some(s) = params.get("cwd") else { return Ok(Some(cfg.workdir.display()...)) };
match std::fs::canonicalize(s) {
    Ok(p) if p.is_dir() => Ok(Some(p.to_string_lossy().to_string())),
    _ => Err(RpcError::invalid_params("cwd is not a valid directory")),
}
```

`canonicalize` 与 `is_dir()` 都在**运行 app-server 的那台机器**上执行。连上远端后，
app-server 在远端跑，而前端手上的 cwd 是**本机路径**（如
`/Users/yichen/proj/foo`）。结果：`canonicalize` 失败 → 返回
`cwd is not a valid directory` → 用户看到"新建会话失败"，且看不出原因。

而且 `resolve_thread_cwd` 在**远端** canonicalize 后，会把结果写进线程元数据并
回传前端。于是同一个 thread 的 cwd 在 UI 上会显示成远端路径——这本身是对的，但
意味着**前端不能把 cwd 当成"本机可以用它打开文件"的路径**。

**一个更隐蔽的同源问题：`project_key` 会静默兜底，不报错。**

`resolve_thread_cwd` 对不存在的路径是**报错**（上面那段），这没问题。但
`project_key`（`server.rs:644-668`）是另一种行为：

- `canonicalize` 失败时，它**不报错**，而是"canonicalize 最深的已存在祖先，
  再把剩余段原样拼回去"
- 这个值被当作**项目身份**，用在 8 处：会话列表归属（`server.rs:144`）、
  resident 分组（`:149`）、runtime attach（`:835`、`:876`）、
  `cfg.workdir`（`:1242`）、绑定校验（`:1298`）等

后果：如果远端进程的 `cfg.workdir` 与后来 `thread/start` 传的 cwd 拼写不同
（一个有尾斜杠、一个是符号链接展开后的形式、或只是大小写差异），两者会得到
**两个不同的 `project_key`**，表现为"同一个项目下看不到刚建的会话"或
"agent 运行时 attach 不上"。源码注释自己写着这正是要避免的
（`server.rs:640-643`："two keys for one project, which makes delegation
silently disappear"）。

对本设计的含义：
- 远端模式下**更**不能传 cwd（§4.4 方案 B）——`cfg.workdir` 来自 ssh 会话的
  cwd，是唯一的真相，不应再被第二个来源覆盖
- 验收时要专门看"同一远端项目内，新建会话后立刻能在列表里看到"（已有判据 15、
  16 覆盖 id 不串台，但**没有**专门覆盖"会话归属正确"）。因此本设计补一条判据

**根因提醒**：`project_key` 的兜底逻辑是为"目录还没创建"的场景服务的，本身没错。
错的是**让它有机会看到两种拼写**。远端模式必须保证 `cfg.workdir` 是该项目
cwd 的唯一来源。

**决策：引入显式的路径映射，不做隐式猜测。**

`remote.json` 需要存"本机项目 → 远端项目"的映射。**完整 schema 定义在
§4.9，这里只讲它与路径语义的关系**：

- key：本机项目路径（绝对、canonical）
- value：远端路径（绝对）**以及该项目的实例隔离根**（§4.6）

即每个远端项目是一个独立 profile，而不是"一个 host 下挂多个项目目录"。理由同
§4.6：每个项目要独立进程、独立状态目录，把它建成 profile 正好一一对应。

前端目前 `thread/start` 会传本机路径。**远端模式下必须停止这样做**——本机路径
送到远端 `canonicalize` 必然失败。两种处理方式，**选 B**：

- **A. 桥接层改写 `cwd` 参数**：在 `bridge.rs` 转发 `thread/start` 时替换成远端路径。
  缺点：桥接层要解析并改写 JSON-RPC 参数，破坏"桥接层只转发不解析"的现有性质
  （见 `bridge.rs:29-46` 的 `classify`，它只看有没有 `method`/`id`，不看 content），
  而且每新增一个带路径的 RPC 都要记得改，漏一个就回到本节的 bug。
- **B. 远端 cwd 由 ssh 会话的工作目录决定**：前端在远端模式下**不传 `cwd`**，
  此时 `resolve_thread_cwd` 走 `None => cfg.workdir` 分支，直接用进程的 workdir。
  优点：桥接层保持纯转发；路径映射只影响"起进程时工作目录在哪"这一处。

  **为什么只能用 `cd`（重要，别去抄 codex 的 `cwd`）**：
  codex 的 `cwd` 是**本机侧 spawn 参数**（`environment_toml.rs:190-196`），管的是
  **本机 ssh 子进程**的工作目录，管不到远端。而 ssh 协议**没有**"指定远端工作目录"
  的参数，`ssh -t` 需要终端也不适用。所以"让远端 app-server 的 cwd = 项目目录"
  只能靠远端 shell 的 `cd`。这是协议限制，不是设计选择。

  因此形态固定为（注意 `--listen stdio://` 与 `--relay` 的共存规则，见 §4.6 前提 0）：

  ```bash
  ssh <host> "cd '<远端项目路径>' && exec '<bin>' app-server --listen stdio:// [--relay <url>?session=<id>]"
  ```

  `--listen stdio://` 显式写出是**安全的**（`app_server_mode` 里 stdio + `--relay`
  仍落 `StdioWithRelay`），但它**不能**换成 `ws://` 或 `relay://`——那会变成
  `PureRelay`，桌面连不上。显式写 `stdio://` 而非省略，是为了让命令行自解释：
  读代码的人一眼看到"这是 stdio 通道"，不会误以为 `--relay` 独立出现就是纯中继。

  - `<bin>` 是安装产生的固定路径（`~/.yi-agent/bin/<version>/yi-agent`），
    **不由用户输入**，无需转义
  - `<远端项目路径>` 来自 `remote.json`，**必须经 §6 第 3 条校验**（绝对路径、不含
    shell 元字符），并用单引号包裹、内部单引号按 `'\''` 转义
  - 这正是 §6 第 2、3 条要做输入校验的原因：这是全设计里**唯一**把字符串拼进
    shell 的地方，是命令注入的唯一边界

**B 的代价**：远端模式下「一个会话一个项目」被固化为「一个 app-server 进程一个项目」。
这正好与 §2.3「一项目一个远端进程」一致，不构成额外限制。

**B 需要注意的具体位置**（已核实的清单，实现时逐条处理）：

| 位置 | 传的字段 | 远端模式下的处理 |
|---|---|---|
| `desktop/src/App.tsx:956` `thread/start` | `cwd` | **不传**。传了必然 `canonicalize` 失败 |
| `desktop/src/App.tsx:1692` `thread/lead` | `cwd: g.workspace` | 不传或传远端路径；lead 会话在远端模式下的语义需要单独定义，**本设计不做**（§7） |
| `ThreadSidebar.tsx:205` `onRemoveWorkspace` | `cwd` | 只作展示用；移除远端目录应走 `remote/removeProfile`，不调本地 workdir 索引 |
| `threadStore.ts:25` `info.cwd` | 展示用 | 直接用 `thread/start` 返回的**远端**路径展示，不回写本机索引 |

原则不变：**远端模式下本机路径不得进入任何会被远端 `canonicalize` 的字段**。
上表是已知的全部入口；实现时若发现新的 `cwd` 传递点，必须补进这张表。

**验收上必须有一条**：远端模式下新建会话成功，且 `thread/start` 返回的 `cwd` 是
远端路径（不是本机路径、也不是报错）。

### 4.5 大负载：附件与图片走分片，已天然可用

stdio 传输有硬上限 `MAX_FRAME_BYTES = 1 MiB`
（`yi-agent-app-server/src/protocol.rs:9`），任何单帧超限即报错。而附件上限是
50 MiB（`attachments.rs:11` 的 `MAX_ATTACHMENT_BYTES`）——**直接传会被截断报错**。

好消息：**分片上传机制已经存在，不需要新增**。
`image/upload/begin` → `image/upload/chunk` → `image/upload/commit`
（`rpc/image.rs:270-460`），分片大小 `UPLOAD_CHUNK_BYTES = 512 KiB`
（`image_upload.rs:30`）。512 KiB base64 编码后约 683 KiB，**安全落在 1 MiB 帧限制内**。

因此远端模式下附件/图片**无需特殊处理**，只要走既有的分片路径。本设计只需钉住：
**不得**为远端模式引入任何"一次性发送整个文件"的捷径——那会立刻撞上 1 MiB 上限。

另一个已确认的点：`view_image` 走的是 `image/read`，它**本来就是逐页返回**的
（`rpc/image.rs:31` 的注释明确写着"由客户端逐页…与全量读再切片相对"），因此远端
读图同样不会撞帧上限，无需改动。

结论：附件与图片两条路径都天然可用，本设计在这一点上**不需要任何新增代码**。

### 4.6 并行：多项目同时连，桥接层必须改成多进程

**并行是硬需求**：用户会在两个远端项目之间来回切，甚至在两个项目里各跑一个会话。
现状做不到，有六个结构性障碍（障碍一、二 最容易被漏——
它们不是「结构」，而是几个小函数）。

#### 障碍一：`kill_sidecar` / `set_relay_url` 的"全局"语义必须重新定义

这两个函数现在都是**全局作用**的，多槽位下直接沿用会造成用户可见的事故：

| 函数 | 现状（已核实） | 多槽位下的问题 |
|---|---|---|
| `kill_sidecar`（`bridge.rs:353-363`） | 取 `Sidecar.child`（唯一那个）并 kill | 改成 HashMap 后若遍历全杀，**改个中继配置会把所有远端项目一起断开**——用户正在 A 项目里跑长任务，改了个无关设置就断了 |
| `set_relay_url`（`bridge.rs:326-337`） | 写盘 → `kill_sidecar` → 监督循环重启 | 同上，且更隐蔽：它注释写着"restarting"，用户预期是"重连"，实际是"全断" |
| `configured_relay()` 的用法（`bridge.rs:369`） | 作为 `sidecar_args` 的输入，**全局唯一一份** | 本地槽位应始终用它；远端槽位要用**派生后**的 URL（§4.6）。若照搬同一份，所有远端项目会共用一条 session，互相顶掉（§4.6 前提 3 的撞车） |

正确改法：

- `kill_sidecar(app, slot: &ProfileKey)` 只杀指定槽位；另留一个**显式命名**的
  `kill_all_slots(app)` 只给"关闭 flag"（§5.1）用，不要让它成为默认可调用的
- `set_relay_url` 的语义收窄为"只重启**本地**槽位"。远端的 relay URL 是派生的
  （`configured_session` + `profile.id`），改本地配置时**不应**连带重启远端
- `configured_relay()` 只用于本地槽位；远端走
  `derive_relay(configured, profile.id)`（§4.6）。**这两个调用点必须能被
  grep 区分**，否则最可能出现的 bug 是"远端槽位拿了未派生的 URL"

#### 障碍二：几个"单例写死"的辅助函数，改法必须逐个点名

`Sidecar` 结构本身改成 HashMap 是显然的，但**围着它的几个小函数**容易被漏，
而且漏了以后的症状很具体：

| 函数 | 现状 | 多槽位下照搬的后果 |
|---|---|---|
| `clear_child`（`bridge.rs:398-402`） | `*guard = None`，直接清空 | 任一槽位退出都会**清空整张表**，其余槽位的 RPC 立刻报 "sidecar not running"——表现为"一个远端掉线，本地也崩了" |
| `write_line`（`bridge.rs:52-57`） | 取唯一的 `child`，无槽位参数 | `rpc`/`rpc_respond` 无法指定目标槽位，请求会写进"最后设置的那个"进程 |
| `forward_stdout`（`bridge.rs:404`） | 把帧发到全局事件名 | 所有槽位的帧进同一个事件名（§4.6 障碍六） |

正确改法：`clear_child(app, slot)` 只删**该 slot 的键**，且删之前要核对
"表里这个键是不是我启动的那个进程"（作业 token 式校验）——否则会误删已经重启的
新进程的句柄。这与 `relay/src/lib.rs:112` 里
"只摘除仍是我自己的登记"是同一类问题，可以照那个思路写。

#### 障碍三：写 pipe 会阻塞，锁必须每槽位一把

`write_line`（`bridge.rs:52-58`）是**同步写**：

```rust
let mut guard = state.child.lock()?;          // 持锁
let child = guard.as_mut()...?;
child.write(&buf)                            // tauri-plugin-shell 内部 write_all
```

`CommandChild::write` 是 `self.stdin_writer.write_all(buf)`
（`tauri-plugin-shell-2.3.6/src/process/mod.rs:72-75`），**同步**。而且
`rpc`/`rpc_respond` 都是非 `async` 的 `#[tauri::command]`（`bridge.rs:61`、`:75`），
所以阻塞会占住 Tauri 的 IPC 线程。

为什么这在**远端**场景下更严重：

- 本地 sidecar 是同机进程，pipe 被读得快，写阻塞极罕见
- 远端是 **ssh 子进程**：网络抖动、远端负载高、远端 app-server 卡在一次 LLM 调用上
  时，ssh 的 stdin 管道会积压。一旦写满（默认 64KB），`write_all` 就阻塞
- 阻塞期间**持有那把锁**。如果多槽位共用一个全局 `Mutex`，一个远端卡住会让
  **所有槽位**的 RPC 全部卡住，包括本地——表现是「整个 App 卡死」

因此多槽位改造里，锁的粒度**必须是每槽位一把**，不能是一把全局锁：

```rust
// 正确：每个槽位独立的锁，A 阻塞不影响 B
struct Sidecar {
    slots: Mutex<HashMap<ProfileKey, Arc<Mutex<Option<CommandChild>>>>>,
}
```

注意外层 `Mutex` 只保护「查表」，**不能**在写 pipe 期间持有它——否则退化成全局锁。
取到 `Arc<Mutex<..>>` 后立刻释放外层锁，再写 pipe。

**仍然存在的风险（本设计不改，但要记录）**：即便锁是每槽位的，一个卡住的远端
仍会阻塞**它自己**槽位的后续 RPC，直到 ssh 管道疏通或超时。这是同步 IO 的固有限制。
缓解靠 §4.3 的断线检测（重试上限 + 手动重连），不靠「不阻塞」。

#### 障碍四：`Sidecar` 是单例

```rust
// desktop/src-tauri/src/bridge.rs:38-40
pub struct Sidecar {
    child: Mutex<Option<CommandChild>>,
}
```

`spawn()`（`bridge.rs:112`）是一个无限循环维护**一条**子进程；`kill_sidecar`
取的也是这一槽位；`spawn_once` 直接 `.replace(child)`。多进程必须换成
按 key 分槽的表。

#### 障碍五：`id` 空间其实**不需要**偏移（原判断有误，已修正）

**先前的错误判断**：以为每个 `RpcClient` 都从 1 开始编号
（`rpc.ts:61` 的 `nextId = 1`、`:188` 的 `this.nextId++`），多个 client 会送出
相同的 `id=1`，响应无法区分，因此要"注入 id 偏移"。

**核实后不成立**。`RpcClient` 的构造把响应绑定到**自己的** transport：

```ts
constructor(private transport: Transport, private recover?: ...) {
  this.unsubscribeTransport = transport.onMessage((raw) => this.onMessage(raw));
}
```

响应只从本 client 的 transport 进来，`this.pending.get(id)` 查的也是本 client 的表。
两条连接各有独立 transport（本地是 Tauri 事件、远端是 ssh 子进程），**id 相同也
永远不会互相看到**。这是天然隔离，不是巧合。

因此：

- **不要**给 `RpcClient` 加 id 偏移。那是无用的复杂度，还会让调试时的 id 变得难读
- 实施顺序里**删掉**"`RpcClient` 注入 id 偏移"这一步
- 唯一需要保证的是：**每个 client 拿到真正属于自己槽位的 transport**。
  这件事的真难点在下面「事件名」那一条——Tauri 事件名是全局的，
  若两个 client 订阅了同一个事件名，它们会收到同一份帧，那时才是真的串台

换句话说：**id 隔离是免费的，事件隔离不是。** 把力气放在事件名上。

#### 前端侧的单客户端假设（改造时最容易漏的一类）

桥接层改成多槽位后，前端仍有一批"只考虑了一个 client"的代码。已核实的清单：

| 位置 | 现状 | 多槽位下的问题 |
|---|---|---|
| `desktop/src/tauriTransport.ts:51-54` | 事件名是**硬编码字符串** `app-server://message` / `:request` / `:status` | 槽位改造后事件名带 slot（§4.6），这里必须改成参数化。**这是事件路由的唯一入口**，漏改等于所有远端项目的帧都进同一个 client |
| `desktop/src/App.tsx:1375-1379` `onVisibilityChange` | 只 `reconnectNow()` 当前 client | 手机/iPad 切回前台时，**只有当前项目**重连；其他远端项目的连接已死却不重连，表现为"切回去那个项目的前几个请求失败"。必须遍历所有槽位各自重连 |
| `desktop/src/App.tsx:1564` 附近 | 注释明确写着"`app-server://message` 是全局事件名，陈旧监听照样会收到新进程的帧" | 这条**正是**多槽位下最危险的点被放大 N 倍：任何一个槽位的陈旧监听都会收到所有槽位的帧，导致重复应用。每个槽位必须严格 dispose 自己的订阅 |
| `desktop/src/App.tsx:164` `clientRef` | 单例 ref，**全文 50 处引用** | 改成 `Map<ProjectKey, RpcClient>` 后，这 50 处要逐条判断"该用当前活动的那个，还是该用指定槽位的"。这是前端改造工作量的真实来源，不能低估为"改个 ref" |
| `desktop/src/App.tsx:1597-1613` `build/info` + `flags/list` | 发往**当前活动 client** | 当前活动若是远端槽位，读到的就是**远端**的构建类型与 flag。远端 release 恒关（§5.3.1）会让结果恰好也是"关"，但这是巧合不是设计。**必须钉死这两个请求只发往本地槽位**，与 §5.2 判定口径一致 |

**为什么单列这张表**：§4.6 的障碍一~六 都是**桥接层/Rust 侧**的（除障碍五是不需要的改动），容易被当成
全部工作量。而前端这四处是"看起来能跑、实际只在单项目下正确"的隐性问题——单项目
测试全绿，一开第二个远端项目才暴露。实现顺序里必须把"遍历所有槽位重连"和
"事件名参数化"当成独立步骤，各配一条判据。

#### 障碍六：事件通道是全局的，没有 project 维度

```ts
// desktop/src/tauriTransport.ts:51-54
onMessage: (cb) => track(subscribe<unknown>("app-server://message", cb)),
onRequest: (cb) => track(subscribe<ApprovalRequest>("app-server://request", cb)),
onStatus: (cb) => track(subscribe<{ state: string; code?: number | null }>("app-server://status", cb)),
```

`forward_stdout`（`bridge.rs:404`）把**所有**子进程的帧都 emit 到这三个全局事件名上。
多进程时，A 项目的通知会流进 B 项目的会话视图。

#### 设计

**决策一：进程槽改为按 profile 分键**

```rust
pub struct Sidecar {
    children: Mutex<HashMap<ProfileKey, CommandChild>>,
}
```

`ProfileKey` 为 `Local | Remote(String /* remote.json 的 profile 名 */)`。
每个 key 一条独立监督循环（`spawn(key)`），互不影响：远端 A 掉线不重启本地
sidecar，也不重启远端 B。

**决策二：RPC 命令带 key，且 key 由 Rust 侧校验存在性**

`rpc` / `rpc_respond` 增加 `slot: String` 参数，写入对应 key 的 stdin。
未知 key 直接返回错误，而不是静默写进第一条连接。

**决策三：事件名带 key**

三个事件名改为 `app-server://message/{slot}` 形式（或事件 payload 加 `slot`
字段）。**推荐前者**：Tauri 的事件订阅是字符串精确匹配，按 key 订阅天然把
A 项目的帧隔离在 A 的连接里，前端不需要在回调里再过滤一遍。`App.tsx:1564` 已
有注释说明"全局事件名会让陈旧监听照样收到帧"，per-slot 命名同时修掉这个隐患。

**决策四：id 命名空间按 slot 隔离（纵深防御 —— 可选，不是正确性前提）**

**先明确性质**：`RpcClient` 的响应绑定在**自己的 transport** 上
（`rpc.ts:103-105` 构造里 `transport.onMessage((raw) => this.onMessage(raw))`），
`pending` 表也是每实例的。所以**即使不做任何 id 偏移，两个槽位也不会互串**。
「障碍五」已推翻原先"撞车必然错配"的判断。

那么为什么仍然建议做？因为存在**旁路**——即"帧可能绕过 per-slot 事件名进入
某个 client"的情形：

- 中继路径（`--relay`，`bridge.rs:161` 的 `sidecar_args`）会把会话暴露给手机端，
  帧可能经非 per-slot 的通路进入
- 前端的 recovery 逻辑（`rpc.ts:160-186` 的 `-32010` 自愈）会重发请求，
  重发与原始请求共存时更需要 id 可区分
- 任何"全局监听"式的兜底代码（如 `App.tsx:1564` 注释警告的陈旧监听）

一旦有帧从旁路进来，`id` 相同就会静默错配到另一个项目的 pending 上，且**不报错**——
只是结果错。这类 bug 极难定位。

但要注意**代价与优先级**：偏移本身很便宜（几行），可作为纵深防御；
然而它**不能替代**把事件名按 slot 分开（决策三）。若只做偏移不做事件隔离，
帧依然进错 client，偏移只让"错配后的 id 恰好不冲突"——问题依然在。
**优先级：先做事件隔离（必做），再考虑偏移（可选）。**

做法：给每个 `RpcClient` 注入起始偏移，`nextId = base + counter`。
`RequestId` 线上类型是 `Num(i64) | Str(String)`
（`protocol.rs:27` 的 `#[serde(untagged)]`），所以数值偏移合法。约束：

- 偏移用 `(slotIndex << 32)` 形式，每 slot 预留 2^32 个 id
- **必须守住 JS 安全整数上限 2^53**：`slotIndex` 上限即 2^21 个 slot，
  实际远小于此，但代码里要有显式断言，避免将来无限加 slot 时静默精度丢失
- 本地 slot 固定 `slotIndex = 0`，保持今天 `nextId` 从 1 开始的行为，零回归

**不改 Rust 侧改写 id**：协议语义不应被桥接层动，唯一性由发起方保证。
可离线单测：给两个 client 各发若干请求，断言 id 集合不相交。

**决策五：前端持有多个 `RpcClient`，按项目取用**

`App.tsx:1777` 的单例改为 `Map<ProjectKey, RpcClient>`。当前活动项目决定用哪个
client 发请求；每个 client 只订阅自己 slot 的事件名。

#### 与「同一主机多进程」的冲突：共享状态目录（并行引入的真实 bug）

这是并行带来的**第二个**结构性问题，比 `--relay` 更隐蔽，因为它只在"同一个用户在同一台
远端主机上开两个项目"时触发——而这恰恰是最常见的用法（谁会在两台不同服务器上各开一个
项目来回切？绝大多数是同一台开发机上开两个仓库）。

**问题**：远端 app-server 的所有持久状态都在 `<远端 home>/.yi-agent/` 下，且**没有
跨进程互斥**。已核实的共享状态与各自的风险：

| 路径 | 写入方式 | 多进程风险 |
|---|---|---|
| `workspaces.json` | 进程内 `Mutex<()>` + temp+rename（`workspace_index.rs:4` 注释明确写"进程内"） | **丢失更新**：A 读→B 读→A 写→B 写，A 的新增被 B 覆盖 |
| `resident-daemons.json` | temp+rename（`resident.rs:51`） | 同上；且**每个 app-server 都跑一份 watch loop**（`server.rs:1913-1921` 每 30s 一次），两个 loop 会重复拉起/杀掉同一批 daemon |
| `threads/*.jsonl` | 按 thread id 分文件 | 安全（文件名不冲突） |
| `runtime.sqlite` | WAL（`repository.rs:443`） | 安全（WAL 支持多进程并发） |
| `devices.json` / `pairing.json` | 读-改-写 | 配对状态可能互相覆盖 |

**决策：远端 app-server 用独立的 `YI_AGENT_HOME` 隔离，不做共享。**

ssh 命令改为（`YI_AGENT_HOME` 的写法见下方"路径坑"）：

```
ssh host "cd '<远端项目路径>' && exec env YI_AGENT_HOME='<实例根>' '<bin>' app-server --listen stdio://"
```

其中 `<slug>` 由**本机**的 profile 名派生（稳定、可预测），例如 `proj-foo`。
这样每个远端项目有自己完整的 `threads/`、`workspaces.json`、
`resident-daemons.json`、`runtime.sqlite`，**互不干扰**。

**先纠正一个容易搞错的事实：cwd 不影响 `instance_home`。**

本地侧车的做法是"把 cwd 锚到 instance_home"（`bridge.rs:376-383` 的
`sidecar_cwd_from`），容易让人以为这样就把状态目录定住了。但
`instance_home()`（`store/src/paths.rs:20-23`）只看两样东西：

```rust
pub fn instance_home() -> Option<PathBuf> {
    let override_value = std::env::var_os("YI_AGENT_HOME").filter(|v| !v.is_empty());
    instance_home_from(override_value.as_deref(), os_home().as_deref())
}
// os_home() 同样只看 env：std::env::var_os("HOME")
```

**`cwd` 完全不参与**。所以：

- 本地侧车能够隔离，是因为 `YI_AGENT_HOME` 由**桌面进程的环境**继承下来
  （`spawn_once` 用 `cmd.args(...)` 起子进程，默认继承环境），**不是**因为 cwd
- 远端槽位**必须显式在远端 shell 里设 `YI_AGENT_HOME`**（这就是 §4.1 里
  `cd '<remote_path>' && YI_AGENT_HOME='<实例根>' ...` 的由来）。只 `cd` 不设 env，
  远端会落到 `$HOME/.yi-agent` —— 所有远端项目**共用同一个状态目录**，正是
  §8.5 警告的静默丢数据

**`YI_AGENT_HOME` 的路径写法有个坑（已按 `paths.rs` 逻辑实测确认）**：
`instance_home_from`（`yi-agent-store/src/paths.rs:25-40`）的归一化规则是
"若传入路径的 basename 不是 `.yi-agent`，就再补一层 `.yi-agent`"。实测结果：

| 传入 `YI_AGENT_HOME` | 实际 `instance_home` |
|---|---|
| `/home/dev/.yi-agent-projects/proj-foo` | `/home/dev/.yi-agent-projects/proj-foo/.yi-agent` |
| `/home/dev/.yi-agent-projects/proj-foo/.yi-agent` | `/home/dev/.yi-agent-projects/proj-foo/.yi-agent` |

即 `instance_home` 的 basename **必然是 `.yi-agent`**。所以要么显式传
`<home>/.yi-agent-projects/<slug>/.yi-agent`，要么接受归一化补出来的那一层——但
**必须让 spec、验收命令、实际路径三者一致**，否则排障时会对着路径怀疑人生。
推荐显式写全，并加一条单测钉住（断言 `instance_home_from` 的返回值等于预期）。

**不要**把实例目录放在 `<home>/.yi-agent/` **内部**（如
`<home>/.yi-agent/projects/<slug>/...`）：虽然也能工作，但与全局实例共享上层目录，
语义上不够干净。用**平级**的 `<home>/.yi-agent-projects/` 更清楚。

**代价与取舍**：
- 每个远端项目独占一份状态 → 磁盘占用随项目数线性增长。但每份也就几 MB
  （threads 是 jsonl，runtime.sqlite 很小），可接受。
- **同机两个项目看不到彼此的会话历史**。这是隔离的必然结果，也是正确的：项目 A 的
  会话列表本来就不该出现在项目 B 里。
- `models.json` 在 `<instance_home>/models.json`，隔离后每个项目要单独配一次模型。
  **这是本决策最明显的体验代价**，缓解办法见下。

**缓解 `models.json` 重复配置**：安装时如果检测到同名 `models.json` 不存在，从
`<home>/.yi-agent/models.json`（用户可能已有的全局配置）**复制**一份过去。这是
"复制用户自己的配置到同一台机器的另一个目录"，不涉及跨机器传输密钥，与 §6 第 1 条
"不做凭据转发"不冲突。

**不做跨机共享的理由**：另一种设计是让同一台主机的所有远端项目共用一个 app-server
状态目录（即现在的行为）。那样 `threads/` 会混在一起，且必须给 `workspaces.json`
和 `resident-daemons.json` 加跨进程文件锁（`flock`，参照
`yi-agent-store/src/ipc.rs:1211` 的 `InstanceLock`）。**这需要改远端 app-server，
违背"app-server 基本不改"的前提（§3.2），因此不采用。

**验收上必须有一条**：同一台远端主机上开两个不同项目，各自的会话列表只包含自己的
会话；`ls <远端 home>/.yi-agent-projects/` 下有两个目录。

#### 与 `--relay` 的关系：每项目一条 session，手机多连接

中继的语义是**一个 session 对应一条电脑侧 agent 连接**
（`yi-agent-relay/src/lib.rs:1-9`，`agents: HashMap<String, Slot>`，后连上的顶掉先连上的）。
每个远端项目是独立 app-server 进程，所以**必须各占一条 session**，不能共用。

**决策：每个远端 profile 有一条自己的 relay session，手机端多连接。**

session id 由 `(本机实例, profile.id)` 派生，稳定可预测，例如
`<本地 session>-<profile.id>`。本机用同一个派生函数生成，手机端据此建连。

**两个必须先解决的坑（已核对 `docs/relay-deploy.md` 与 `relay/src/lib.rs`）**：

1. **"本地 session" 不是程序生成的，是用户配置的**。中继 URL（含 `session` 参数）
   存在用户配置里（`bridge.rs:161` 的 `sidecar_args` /
   `resolve_relay`），`docs/relay-deploy.md` 明确要求用
   `openssl rand -hex 16` 生成后**两端手填**。因此"派生"只能建立在
   **用户已配好的那个 session 字符串**之上，不能凭空造一个。
   → 派生函数的真实签名是 `derive(configured_session: &str, profile_id: &str)`,
   而不是从零生成。

2. **`profile.id` 的字符集必须能安全拼进 URL**。`remote.json` 已限定
   `^[a-z0-9][a-z0-9-]{0,31}$`（§4.9），是 URL 安全的，可直接拼。若日后放宽这个
   正则，派生逻辑会立刻产生非法 URL——两者是耦合的，必须一起改。

3. **中继不鉴权，`session` 就是唯一凭据**（`docs/relay-deploy.md` 第 207 行
   明确写着"中继按 session 配对转发，**不鉴权**"；`relay/src/server.rs:64,74`
   也证实 `session` 只是一个查询参数）。后果分两层，都要在设计里认下：

   **a) 本方案放大了凭据的暴露面。** 原本只需要保护 1 个 session，现在要保护
   N+1 个（本地 1 条 + 每个远端项目 1 条）。派生规则 `configured_session-profile.id`
   是**可预测的**——知道本地 session 就能推出所有远端的 session。这不是新漏洞
   （中继本来就不鉴权），但让"session 泄露"的代价从"一个项目被接管"变成"全部
   项目被接管"。

   **b) 派生规则可预测 = 可被顶掉。** 中继的注释里记着真实事故：一条误连
   `/connect` 的探针就能顶掉真正的电脑连接，把它变成"僵尸"——socket 看着健康、
   永不重连，手机从此长期收到 `no computer connected`（`relay/src/lib.rs`
   的 `a_replaced_agent_is_closed_so_it_reconnects` 回归测试）。对多远端方案，
   危险在于：**任何人猜到派生 session 就能顶掉某个远端项目的链路**。

   缓解（必须做，不是可选）：
   - `configured_session` 的熵必须足够。`openssl rand -hex 16`（128 bit）是下限；
     **不得**为了"好记"用短串或语义化命名
   - 派生结果**不能**比原 session 更弱：`configured_session` 是 hex 时，派生值
     应为 `<hex>-<profile.id>`，长度与熵都不降低
   - 中继仍应放在 TLS 反代后并用域名（`docs/relay-deploy.md` §2.3），派生规则
     不改变这一点
   - 在设置页明确提示：中继不鉴权，session 等同于密码，**不要**把 relay URL 分享出去

**撞车的后果是静默的**（这条必须写进代码注释）：中继的规则是"每个 session 只保留
一条最新的电脑连接"（`docs/relay-deploy.md` 端点表、`relay/src/lib.rs` 的
`agents: HashMap<String, Slot>`）。两个远端项目若派生出同一个 session，
**后连上的会顶掉先连上的，且不报错**——表现为"某个项目随机连不上/手机看到的是
另一个项目"。因此派生时必须保证 profile.id 全局唯一（`remote.json` 已约束
全文件唯一，§4.9），并把"相同 configured_session + 相同 profile.id ⇒ 必须得到
相同 session"写成单测。

另有一个现成的陷阱：中继 `session` **缺省为 `"default"`**（`docs/relay-deploy.md`
第 90 行）。若用户配置里没写 `session` 参数就直连，本地与所有远端会全部挤进
`"default"` 这一条 session，互相顶掉。→ 合成远端 URL 时必须**显式**带上
`session=<派生值>`，不允许走缺省。这条也要有单测。

为什么不在本地做多路复用（手机连一条、本地转发到多个远端）：本地 app-server
不认识远端进程，要它按项目路由就得改 app-server，违背 §3.2 的分层。
多连接下服务端只多一个只读 RPC（`remote/relayTargets`），隔离性天然成立。

**手机端成本（已核实代码，比原先估计的具体）**：**没有独立的 iOS 原生项目**。
iOS 跑的就是同一份 React bundle，由 `transportFactory`（`desktop/src/transportFactory.ts`）
按"有没有持久化"选择传输：

```ts
export function transportFactory(opts = {}): Transport {
  const config = opts.config ?? storedRemoteConfig(...);
  return config ? wsTransport(config.url, config.token) : tauriTransport();
}
```

关键事实：

- **决策只有一处**：有 `{url, token}` 配置 → `wsTransport`（手机）；
  没有 → `tauriTransport`（桌面）。返回类型是**单个 `Transport`**
- 配置存在 `localStorage["yi-agent.remote"]`，形状是 `{url, token}`
  （`lib/remoteConfig.ts` 的 `RemoteConfig`），**只有一个**，没有"多台"的概念
- `storedRemoteConfig` 解析失败一律降级为 `null`（即当成桌面），不抛错

因此"手机多连接"的真实改动是**改这个共用函数**，不是改某个 iOS 客户端：

1. `RemoteConfig` 从单条扩成列表（或在原有单条之上加一个列表字段），
   且**必须保持向后兼容**：老版本 App 读到新存储要能继续用第一条，
   新版本读到老存储要能当成"只有一条"
2. `transportFactory` 的返回值语义要变：从"给我唯一的 transport"变成
   "给我这台的 transport / 给我某个项目的 transport"。**这是破坏性签名变更**，
   调用点（`App.tsx:1777`）必须一起改
3. 因为桌面与手机共用这份代码，改动**同时影响两端**——`tauriTransport()`
   那条分支不能被新逻辑破坏。这是最容易出事的地方：为手机加多连接，
   把桌面走单 transport 的路径改错

**多连接在鉴权上是否可行（已核实，可行）**：`ws.rs` 第 1 行的模块文档写着
"Tier 1:多客户端 + token 认证 + **每连接 scope**"。这意味着：

- 同一台设备的 token 可以支撑**多条** ws 连接，每条独立登记 scope
  （`ws.rs:282` 用 `pairing.authenticate(&token)` 拿到 device，再按连接处理）
- 手机要连 N 个项目 = 建 N 条 ws 连接到**不同的中继 session**，但**用同一个 token**。
  这是可行的，因为 token 标识的是"哪台设备"，不是"哪条会话"
- 注意：不同项目的连接连的是**不同中继 session**（§4.6），不是同一个 app-server。
  token 校验发生在每个远端 app-server 上——而远端是**另一台机器**，
  它的设备表里**没有**这台手机的 token

  **这是必须写清的一点**：远端 app-server 不认识手机。手机连的是
  `wss://<relay>/ws?session=<远端项目 session>`，中继把帧转发给远端的
  `/connect` 连接——**中继本身不鉴权**（§4.6 前提 3），所以帧能到达远端，
  但远端 app-server 的 ws 侧若要求 token，会 4401 拒绝。

  因此需要明确：远端 app-server 的手机接入路径是怎么认证的。两种可能：
  a) 远端不启用 ws 手机接入（只有中继侧的 `/connect`），手机帧经中继直接进入
     远端的 stdio/relay 通道，不经过远端 ws 的 token 校验
  b) 远端也要有设备表，那就需要把手机 token 同步到远端——**这会引入凭据分发**，
     与 §6 第 1 条冲突

  **本设计的取态是 a**，但这一条**必须在实现第一步就验证**：连一个远端项目，
  手机经中继发起一次 `thread/list`，确认不被 4401 拒绝。若不成立，整个手机-远端
  链路需要重新设计（可能是 b，需要单独评估凭据分发）。**这是本设计剩余风险中
  优先级最高的一条。**

**一个反向的好消息**：`opts.config` 本来就是注入点（注释写着"tests, and by a
future paired-URL deep link"），所以给 `transportFactory` 加参数不需要动
它的测试架构，只需要补用例。

**已具备的能力**（无需新增）：

- 多客户端扇出：`broadcast.rs` 的 `Broadcaster` 已支持 `ClientId`、订阅粒度
  （`Feed::All` / `Feed::Only`）、`register_reliable` 保护 stdio 客户端
- 审批先到先得：`server.rs:5645` 用 `hub.broadcast_for(Some(&thread_id), frame)`，
  其余设备靠 `item/toolCall/approvalResolved` 广播关弹窗
- 权限 id 跨 thread 全局唯一（`perm_seq`），多客户端并发审批不串

**本项目不做**：合并列表（把本地与多个远端的会话混成一个列表）。手机端按项目分组
展示即可。若日后觉得分组太碎再加合并视图，那时只动 UI。

#### 手机如何拿到 session 清单（协议面，必须定义）

多连接的前提是手机知道"连哪些 session"。三种做法，**选 B**：

- **A. 多个二维码**：每个远端项目单独出一个码。不用改任何协议，但用户要扫 N 次，
  且每加一个项目就要重扫。**不采用**。
- **B. 配对后按需查询（采用）**：手机与本机配对（现有流程不变），连上本机后调
  一个 RPC 拿清单，据此为每个项目各建一条连接。
- **C. 编码进配对 URL**：把项目清单塞进 `pair_uri`。缺点：清单会变（加/删项目），
  而二维码是**一次性快照**，扫完就过期了。**不采用**。

B 的具体形态：

- 新增 RPC `remote/relayTargets`（**在本机 app-server 侧**，不是 Tauri 命令——
  手机只能经中继访问 app-server，访问不到 Tauri 桥接层）
- 返回：`[{ profileId, label, session, available }]`，其中 `available` 表示该远端
  当前是否真的连着（中继可能连不上，见上一节）
- 该 RPC 的 scope：**`Control` 及以上才能拿到 `session` 字段**。
  `Observe` 只能拿到 `{profileId, label, available}`，**`session` 必须置空**
  （`protocol.rs:18-22` 定义了 `Observe < Control < Admin` 三档）

  **为什么 `Observe` 不能看 session**：中继不鉴权（§4.6 前提 3），`session` 就是
  凭据。任何拿到它的人/设备都能以"电脑"身份连 `/connect?session=<id>`，把真实
  链路顶掉并接管。只读设备的语义是"能看不能动"，把凭据给它等于给了动的能力，
  与 scope 的初衷矛盾。

**为什么这个 RPC 必须放在 app-server**：手机 → 中继 → **本机 app-server**，
链路上没有 Tauri 桥接层。而 `remote/*` 系列（§3.1）是 Tauri 命令，手机够不着。
所以这一个 RPC 是**例外**：它要在 app-server 里实现。

**这打破了 §3.2 "app-server 一行不改"** —— 必须诚实记录。取舍：

- 改动极小：一个只读 RPC，返回几个字符串，无状态
- 替代方案是把清单写进一个手机也能读的地方（如中继），但那要改中继且引入
  配置分发，更重
- 因此接受这**唯一一处** app-server 改动，并在 §7 明确列出

#### 远端带 `--relay` 的四个前提（容易漏，逐条写清）

`--relay` 加在**远端**的 app-server 上，所以中继连接是**从远端机器发起的出站连接**，
不是从本机。这带来三个必须满足的前提：

0. **`--listen` 只能是 `stdio://`（或缺省），绝不能是 `ws://` / `relay://`**
   （已核实 `yi-agent/src/main.rs:157-190` 的 `app_server_mode`）：
   - `--relay <url>` + `--listen` 缺省或 `stdio://` → `StdioWithRelay`
     **这正是我们要的**：桌面经 ssh 走 stdio、手机经中继接入，两条通道并存
   - `--relay <url>` + `--listen ws://…` 或 `relay://…` → **`PureRelay`**，
     stdio 通道消失，桌面将完全连不上远端

   实现上统一**显式**写 `--listen stdio://`（语义与省略等价，但命令行自解释，
   见 §4.1 的形态）。这一条必须是纯函数 `remote_argv()` 的单元测试用例，
   而不是靠实现时记得（§10 第 5 步）：断言 argv 里没有 `ws://` / `relay://`。

1. **远端必须能出网访问中继**。中继 URL 通常在内网可达、外网不可达——如果用户的
   中继自建在内网，而远端在另一张网络上，这条连接建不起来。此时远端仍可正常用
   （stdio 就行），只是手机看不到它。**必须在 UI 上区分"本地可用"与"手机可见"，
   不要让整个连接因为中继失败而失败。**
2. **中继 URL 要传到远端**。本机从 `preferences.json` 读 `relay_url`
   （`settings_store.rs:135` 的 `RELAY_URL_KEY`），然后拼进远端命令行：
   `--relay '<relay_url>?session=<派生 session>'`。这个 URL 是**用户自己的服务地址，
   不是凭据**，但仍要按 §6 第 3 条校验后再拼进 shell。
3. **远端不参与配对**。配对码由本地铸（`PairingState` 的 `codes_path` 允许同机
   跨进程兑换，但**跨机器不行**）。所以手机配对的永远是"本机"，不是远端——
   这正是"手机多连接"设计成立的基础：手机连的是本机给它的 session 清单。

**失败降级**：中继连不上时，远端 slot 必须**照常工作**（stdio 主通道不受影响），
只是该项目的手机可见性为 false。不要因为中继失败而杀掉整个远端 slot。

这条很重要：中继是**附加通道**，不是主通道。主通道永远是 ssh 的 stdio。

#### 为什么值得做这个改造

不做并行，用户切项目就要等几秒重连，且**不能在一个项目里跑长任务的同时去看另一个
项目**——而"让 agent 在远端干活"的典型用法恰恰是长任务。这个成本是每次交互都付，
而多进程改造是一次性的。

#### 范围上的约束

- 本地 sidecar 仍然是**一个**（`ProfileKey::Local`），不引入本地多实例
- 同时连接的远端项目数**不设硬上限**——第 11 个、第 20 个都能连上，绝不拒绝。
  只在超过 10 个时于设置页给一条软提示（资源占用提醒），且**不阻断任何操作**。
  阈值是常量 `REMOTE_SLOTS_HINT_THRESHOLD = 10`，实现时不得散落魔法数字

  不设硬上限的理由：每个远端 slot 是独立进程、独立退避、独立故障域，一个项目的
  资源问题不会污染另一个。硬上限只会让用户在真需要时被自己的工具挡住，而他比
  我们更清楚那台机器扛不扛得住。
- 每个远端 slot 的监督循环独立退避（§4.3 的 60s/5 次规则按 slot 计算）

### 4.7 ssh 认证：只走用户的 ~/.ssh，不自建凭据

这是实际使用的第一道门槛，必须明确。

**决策：不实现任何 ssh 认证逻辑，完全交给系统的 `ssh` 命令。**

具体而言：
- 认证方式（密钥 / agent / 2FA）由用户的 `~/.ssh/config` 与 `ssh-agent` 决定
- 本设计**不存储**密码、不管理密钥、不集成密码库
- 「太麻烦」的兜底是文档指引（照常 `ssh dev@10.0.0.5` 能通，这里就能通），
  而不是在 App 里发明一套凭据管理

理由：`BatchMode=yes` 只影响**交互提示**，不影响 agent 认证与密钥文件。
只要用户平时能 `ssh` 上去，这里的连接就能成。反过来，如果用户平时登不上去，
在 App 里提供密码输入框只会让问题藏得更深。

**必须显式处理的失败**（否则用户只看到"连接失败"）：

| 现象 | 处理 |
|---|---|
| 密钥不存在 / 未加载 | 提示"请先确认 `ssh <host>` 能直接登录"，并展示 ssh 的 stderr |
| host key 未知 | 走 §8.4 的显式信任流程 |
| 端口非 22 | 允许 host 写 `user@host:port`，或直接依赖 `~/.ssh/config` 的 `Port` |
| 需要 2FA | 明确告知"本功能不支持交互式 2FA，请改用密钥或 agent" |

最后一条是本设计的一个**明确的已知限制**，不隐藏。

### 4.8 远端没有可用模型配置时怎么办

这是**第一次连上远端后必然会撞上**的问题，不处理会让用户以为功能坏了。

远端 app-server 读它自己那台机器上的 `models.json`（`yi-agent-runtime/src/config.rs`
是唯一真相源）。刚装好的远端二进制目录里没有这份配置，于是 `thread/start` 报
"no model configured"。

三个选项：

- **A. 报错让用户自己配**：诚实但有摩擦，用户得 ssh 上去手写 JSON
- **B. 自动转发本机 `models.json`**：方便，但**把本机 API key 复制到了远端**——
  与 §6 第 1 条「不做凭据转发」直接冲突
- **C. 连上后先探测，缺失时给出可操作指引**（本设计采用）

C 的具体行为：
1. 建会话前调 `models/list`（或等价探测）。若为空/报错，**不进入主界面**，
   而是显示一个说明页：「远端尚未配置模型。你可以：① 在远端运行
   `yi-agent models add`；② 手动把模型配置放到 `<远端 home>/.yi-agent/models.json`」
2. 说明页里给出**远端实际解析到的配置路径**（从远端读，不要在本机猜）
3. 明确写出「不会自动复制本机密钥」，让用户知道这是有意的安全选择

这样既不偷偷搬密钥，也不让用户在一句 `no model configured` 面前无从下手。

**顺带一个容易漏的：`settings.json` 也一样是空的。**

`settings.json` 的路径是 `<instance_home>/settings.json`
（`yi-agent-runtime/src/settings.rs:302-306` 的 `settings_path()`）。因为
`YI_AGENT_HOME` 被隔离到项目专属目录（§4.6），远端**同样**没有这份文件。

它和 `models.json` 的区别是**严重性不同**：

- `models.json` 缺失 → 明确报 "no model configured"，是**硬失败**，用户必然注意到
- `settings.json` 缺失 → 逐字段回默认值（`settings.rs:362` 的 `normalized()`，
  文档明写"单字段非法只清掉那一个"），**完全静默**。用户会发现"远端的
  `max_turns`、system_prompt 跟本机不一样了"，但没有任何提示

本设计**不做**配置同步（理由同 §6 第 1 条，且那会引入双向一致性问题）。
但必须：

1. 在说明页里**同时**指出"远端的 `settings.json` 也是独立的、当前为空"
2. 验收时确认这一点**不会**让远端报错或行为异常（回默认值是允许的）
3. 文档/提示里写清"远端配置独立"，避免用户以为改了本机就同步过去了

**不要把 `settings.json` 也做成"自动复制"**：它与 `models.json` 不同，里面可能有
per-project 的 system_prompt；跨机器复制会把本机项目的提示词带到别的项目上。

### 4.9 `remote.json` 的完整 schema（唯一定义处）

前面几节零散提到过字段，这里集中定死，实现时以此为准。

```json
{
  "version": 1,
  "profiles": [
    {
      "id": "proj-foo",
      "label": "foo (devbox)",
      "host": "dev@10.0.0.5",
      "local_path": "/Users/yichen/proj/foo",
      "remote_path": "/home/dev/proj/foo",
      "instance_root": "/home/dev/.yi-agent-projects/proj-foo/.yi-agent",
      "binary_version": "0.1.10",
      "remote_arch": "x86_64-unknown-linux-gnu",
      "relay_session": null
    }
  ]
}
```

字段说明与约束：

| 字段 | 约束 | 说明 |
|---|---|---|
| `version` | 整数，当前唯一合法值 `1` | 未来迁移用。读到更高版本**拒绝加载**并提示升级 App，不要猜 |
| `profiles[].id` | `^[a-z0-9][a-z0-9-]{0,31}$`，全文件唯一 | 用作 `ProfileKey`、`instance_root` 末段、事件名后缀。**必须**满足这个字符集，否则事件名与目录名都会被污染 |
| `host` | `^[A-Za-z0-9._-]+(@[A-Za-z0-9._-]+)?$`（§6 第 2 条） | 拼进 ssh 命令行，故严格白名单 |
| `local_path` | 绝对路径，且必须是已存在的目录 | 本机侧的键。UI 上按它显示 |
| `remote_path` | 绝对路径，不含 shell 元字符（§6 第 3 条） | 拼进远端 `cd` |
| `instance_root` | 绝对路径，**统一以 `.yi-agent` 结尾** | 由 App 派生为 `<远端 home>/.yi-agent-projects/<id>/.yi-agent`，**不由用户输入**；直接传给 `YI_AGENT_HOME`。注意：末尾不写 `.yi-agent` 也**不会**出错——`instance_home_from` 会自动补一层，两种写法隔离效果相同（§4.6 实测表）。这里要求写全，只是为了让"配置值、验收命令、实际路径"三者一致，避免排障时对不上 |
| `binary_version` | semver 字符串 | 与 App 版本解耦：远端可以跑旧版 |
| `remote_arch` | 必须是三个受支持 target 之一（§2.4.5） | `uname -sm` 的归一化结果，缓存以避免每次重连都探测（§4.2）。不在矩阵内时连接前即失败，**不得**先 scp 再报错 |
| `relay_session` | 可为 `null` | 由 `(本地 session, profile.id)` 派生（§4.6）。`null` 仅在未配置中继时出现 |

**存哪里**：本机 `<本机 instance_home>/remote.json`，即
`~/.yi-agent/remote.json`（或 `YI_AGENT_HOME` 指向的实例根）。**不在远端存**——
配置属于"这台电脑要怎么连"，不属于被连的那台机器。

**写入方式**：读-改-写 + temp+rename 原子替换。**但这里不能只照抄
`workspace_index.rs` 的做法**——它只有进程内 `Mutex`（`workspace_index.rs:4`），
而 `remote.json` 会被**两个进程**碰：

- **写**：Tauri 桥接层（用户改配置时）
- **读**：app-server 进程（`remote/relayTargets`，§4.6）

进程内 `Mutex` 保护不了跨进程。必须用 `device_store.rs:104-130` 已有的**双层**范式：

```rust
/// 进程内 Mutex + 跨进程 flock 一起拿，覆盖一次读-改-写。
fn exclusive(&self) -> io::Result<(MutexGuard<'_, ()>, std::fs::File)> {
    let guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
    let file = self.lock_file()?;   // flock(LOCK_EX)
    Ok((guard, file))
}
```

**另一个必须照抄的细节：临时文件名要唯一。** `device_store.rs:120-128` 的注释写明：

> 唯一临时名：并发写者不得共用同一个临时路径，否则一个 `rename` 会 `ENOENT`
> 另一个的，且撕裂的读会看成「一条设备都没有」。pid+uuid 足够。

若用固定的 `remote.json.tmp`，两处并发写（例如用户快速连点两次、或桥接层与
app-server 同时写）会互相踩掉临时文件。**必须**用
`remote.json.<pid>.<uuid>.tmp` 形式。

**只读的场景不需要锁**：`remote/relayTargets` 只读，且 temp+rename 保证了
它读到的要么是旧版本要么是新版本，不会是半截文件。**不要**为了"保险"给读路径
也加 flock——那会让 UI 查询被写操作阻塞。

**不存的东西**（有意为之）：
- 不存密码、私钥路径、passphrase —— 完全交给 `~/.ssh`（§4.7）
- 不存 API key（§6 第 1 条）
- 不存远端 `models.json` 内容（§4.8）

**读坏文件的行为**：JSON 语法错误、字段缺失、`version` 不认识 —— **全部**当作
"没有配置"，记一条 warning，让用户重新配置。绝不允许坏的 `remote.json` 导致
desktop 起不来或设置页白屏。

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
3. **全部**远端 slot 的 ssh 子进程被终止，回落到本地 sidecar。
   并行后这里容易只杀"当前活动的那个"，必须遍历整个进程表逐个杀
   （§4.6 的 `HashMap` 全量清空），且要等每个子进程真正退出后再返回

4. 远端 slot 的中继连接随之断开（进程死了连接自然断），且
   `remote/relayTargets` 返回**空清单**或 `method_not_found`——不能让手机
   继续看到一个已关闭功能的 session 清单

第 3 条容易被漏掉：如果只在 UI 隐藏、不杀进程，用户会处于"看不见但远端还在跑、
还在烧 token"的状态。并行后这个风险被放大 N 倍——漏杀一个 slot 就有 N-1 个进程
在后台继续跑。**验收时必须开两个远端项目再关开关**，确认两个都不在了。

第 4 条同样容易漏：手机端持有的是**缓存的 session 清单**，关掉开关后它仍会尝试
重连那些 session。中继会因"无电脑连接"回 `-32000` 错误帧，手机端应把这种情况
显示为"该项目已不可用"，而不是无限重试。

### 5.2 谁能判定 flag 值（易错点）

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

### 5.3 stable 构建的语义

`store.rs:75` 的 `status_at` 第一行 `if build_kind != "dev" { return (false, Source::BuildKind) }`
已经保证 stable 恒关。**不需要额外代码**，但需要一条测试钉住它。

#### 5.3.1 本期定位：dev-only，不进正式版（已与产品确认）

**这个功能只在 dev 构建里可用，正式版（release 构建）里开关恒为关、入口不渲染。**
这是有意为之，不是缺陷：先在实验版里验证充分，再决定是否进正式版。

由此产生几条**必须遵守**的推论，否则实现会走偏：

1. **不要为了"让正式版用户也能用"去动 `store.rs` 的硬闸。** 动它会影响所有
   flag（`lead_agent`、`superpowers_kanban` 等），是全局安全约定的变更，必须
   单独立项评估，不能夹带在本功能里。
2. **远端二进制是 release 构建，`build_kind == "release"`，它的 flag 恒为
   false。** 因此**远端侧绝不能有任何依赖 flag 的逻辑**——判定只在本机（§5.2）。
   写"远端也检查一下 flag 更稳妥"是错的：那会让功能在远端静默失效。
3. **未来进正式版时，需要的是一个"release 放行白名单"机制**（而不是去掉硬闸）。
   本设计**不实现**它，但要在注释里留一句指引，避免未来实现者直接删掉那道闸。

#### 5.3.2 术语：两个"remote"不是一回事（命名冲突，务必区分）

代码里已存在 `isRemoteClient()`（`desktop/src/lib/platform.ts`），它指的是
**iOS 配对客户端**（就是手机，经中继/ws 连本机的那端）。而本设计的"远程"指的是
**桌面经 ssh 连另一台机器上的项目**。两者完全无关，但都叫 remote。

实现时**不得**复用或改写 `isRemoteClient()` 的语义，新增概念必须另起名字：

| 概念 | 建议命名 | 含义 |
|---|---|---|
| 已有：iOS 配对客户端 | `isRemoteClient()`（**保持原样**） | 手机端 webview，无 sidecar |
| 本设计：ssh 连远端项目 | `isSshProfile` / `profile.kind === "ssh"` | 桌面连另一台机器 |

混淆的后果很具体：`App.tsx:1590` 附近已有
`...(isRemoteClient() ? { resumeHints: buildResumeHints() } : {})` 这类分支，
若把 ssh 远端也塞进 `isRemoteClient()`，会导致桌面端走手机端的代码路径
（跳过 sidecar、走 ws 传输），整条 ssh 链路都不会被用到。

### 5.4 关掉开关后的数据处理

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
3. **远端路径同样校验**："不含 shell 元字符"这种说法无法直接实现，定死规则：
   - 必须是绝对路径（以 `/` 开头）
   - **允许的字符集**：`[A-Za-z0-9._/+@-]`，其余一律拒绝（含空格、`'`、`"`、
     `` ` ``、`$`、`&`、`|`、`;`、`<`、`>`、`(`、`)`、`*`、`?`、`!`、`#`、`~`、
     换行、制表符）
   - 不允许出现 `..` 段
   - 长度上限 4096 字节

   这是一份**白名单**，不是黑名单：黑名单永远会漏（例如 Unicode 全角字符、
   各种引号变体）。即便路径被单引号包裹，白名单也让"转义写错"不致命。

   同一规则适用于 `instance_root` 与 `local_path`。
4. **ssh 参数固定**：不用 `-o ProxyCommand` 之类可由用户配置的项，让 ssh 走用户
   自己的 `~/.ssh/config`。连接类命令一律加 `BatchMode=yes`，防止卡在交互式提示
   （连不上就快速失败，而不是让 UI 无限等待）。

5. **host key 走严格校验 + 显式信任**（易错，务必按下面做）：
   不要用 `StrictHostKeyChecking=accept-new`。按 `man ssh_config`，`accept-new`
   会**自动把未知 host key 写进 known_hosts 并直接连上**，等于默默接受 TOFU，
   把首次连接的中间人窗口留给了后台自动连接——这是本功能绝不能有的性质。

   正确做法：
   - 日常连接：`BatchMode=yes` + `StrictHostKeyChecking=yes`。host key 未知时
     直接失败，**绝不自动接受**。
   - 首次信任：一个**独立的、明确的用户动作**。执行 `ssh-keyscan` 拿到远端指纹，
     在 UI 上同时显示指纹与待连接的 host，用户确认后才写入 known_hosts
     （或用一次用户可见的交互式 `ssh` 让用户自己确认）。此后所有连接走
     `StrictHostKeyChecking=yes`。

## 7. 不做什么（非目标）

- **不做"多项目共享一个远端常驻服务"**（§2.3 已论证）：注意这与"并行"不冲突——
  并行是**多进程**，daemon 是**一个进程服务多客户端**。本设计明确选前者。
  **这条不是"远端不跑任何 daemon"**：每个远端 app-server 启动时都会自己
  `spawn_resident_watch_loop`（`server.rs:1913-1921`，每 30s 扫一次
  `resident_dir` 并 `ensure_daemons`），这是**既有行为**，本设计不改它。
  每个项目的 `instance_root` 隔离，所以各自的 `resident-daemons.json` 独立
  （见下面那条风险）
- **不做本地多实例**：`ProfileKey::Local` 恒为单例，并行只针对远端项目
- **不做合并列表**：手机端按项目分组展示多个连接，不把本地与远端的会话混成
  一个列表（§4.6）。需要时再加视图，只动 UI
- **不做凭据转发**（§6 第 1 条）
- **不做跨会话状态保留**（§4.3）
- **不做零安装模式**（§2.2）
- **不把二进制内置进 App**（§2.4）：体积对不用的人是死重，改为惰性下载 + 缓存
- **不做 Windows 远端**：暂只支持 Linux/macOS 远端。Windows 的路径语义、shell 差异
  是独立的一大块，本期不碰。
- **不改工具层**：所有工具走远端 app-server 内部执行，`yi-agent-tools` 一行不改。
  这是本设计最省力的地方，也是最容易被破坏的地方——见 §8.3。
- **不改 app-server 的连接逻辑**：唯一例外是只读的 `remote/relayTargets`
  （§4.6）。**不允许**实现时顺手把连接管理、安装、中继等逻辑塞进 app-server
- **不做 scope 透传到远端**：所有经中继进入远端的帧，在远端都是同一台
  `relay-bridge` 的 Control 设备（§8.9）。`Observe` 手机在远端侧也是 Control。
  这是已接受的副作用，**不修**（要修需引入凭据分发，违背 §6 第 1 条）
- **不做 per-device 审计/按设备撤销**：同上，远端无法区分是哪个手机（§8.9）
- **不做会话恢复**（session resume）：断线即重启进程、状态不保留（§4.3）。
  codex 有 `client_recovery` / `client_refresh` 两层，本设计两者都不做，
  只做最朴素的"进程退出就重启"

## 8. 风险

### 8.1 版本漂移

远端二进制的版本与本机不一致时，RPC 协议可能有差异。缓解：安装后强制
`--version` 校验，不匹配则重装；profile 里记录远端版本，连接时比对。

### 8.2 首次连接要多等一次下载

不内置二进制的直接代价：第一次连某台远端，需要下载 14.91MB（gzip 后）再传到
远端。在慢网下可能要等十几秒。

缓解：
- 缓存按 `<version>/<target>` 存，同版本只下一次；跨 App 重装存活
- 进度通过 `remote/install/progress` 事件回推，UI 有明确反馈而不是卡住
- 多台同架构远端共享同一份缓存

**不要**为了消掉这次等待就退回"内置"。体积是永久成本，下载是一次性成本。

### 8.3 工具层被"顺手"改成远程感知

最容易犯的错：为了"让远程更快"，有人在 `yi-agent-tools` 里加
`if remote { ssh ... }` 分支。那会立刻产生两套执行路径、两套行为。**本设计的
前提是工具层完全无感知**——所有远端差异止于 ssh 子进程的 stdio。实施 review 时
需要守住这条线。

### 8.4 host key 首次确认是最容易做成"玄学 bug"的一处

第一次连某个 host，若直接 `BatchMode=yes` + `StrictHostKeyChecking=yes`，ssh 会以
"Host key verification failed" 失败，**用户只看到一句莫名的错误**，且不知道该干什么。

正确处理已写在 §6 第 5 条（严格校验 + 用户可见的显式信任步骤）。这里只强调风险性质：
它失败得早、信息量少，很容易被误判成"ssh 连不上"，从而被错误地"修复"成放宽
`StrictHostKeyChecking`。实施时**不允许**把它修成 `accept-new` 或 `no`。

### 8.5 状态目录隔离若做错，是"静默丢数据"

`workspaces.json` 和 `resident-daemons.json` 只有**进程内**锁（`workspace_index.rs:4`
明确写了这一点）。如果 §4.6 的 `YI_AGENT_HOME` 隔离漏做、或某个 slot 忘了传这个
环境变量，两个远端项目就会并发写同一个文件，后果是**丢失更新且不报错**：
用户的"最近目录"偶尔少一个，或者某个常驻 daemon 被另一个进程的 watch loop 杀掉。

这类 bug 不会崩、不会报错，只会表现为"有时候怪怪的"。因此：
- §4.6 的隔离**不是可选优化，是并行正确性的前提**
- 实施时必须有一条测试：构造两个 `YI_AGENT_HOME` 不同的实例，断言它们的
  `workspace_index::default_path()` 不同
- code review 时重点看"是不是每个起远端进程的地方都传了这个变量"

### 8.6 ssh 会话的 cwd 是唯一且脆弱的通道

远端 app-server 的项目目录**只能**来自进程 cwd（`resolve_workdir` 用
`env::current_dir()`；`--workdir` 参数不存在，`YI_AGENT_WORKDIR` 已下线）。
这意味着：

- 一旦有人为了"方便"把 ssh 命令改成 `ssh host '<bin> ...'`（去掉 `cd`），
  所有远端项目会静默地跑在**用户 home** 下——工具仍然能读写，只是写错了地方，
  而且不会有任何报错
- `cd` 目标目录被删/改名后，重连会在 `resolve_workdir` 的 `is_dir()` 上失败
  （`config.rs:198-200`），报 `working directory does not exist`

缓解：把"起远端命令"收敛到**一个纯函数**（输入 profile，输出 args + cwd），
单测断言输出里包含 `cd '<remote_path>'`，且 `remote_path` 与 profile 一致。
不要在多处拼这条命令。

### 8.7 中继是新的失败面，且它在远端机器上

远端 slot 带 `--relay` 后，失败模式多了一种：**ssh 通、stdio 正常、但中继连不上**。
这种状态下桌面完全正常，只有手机看不到——用户会以为"手机端坏了"，而不是
"远端出不了网"。

缓解：
- UI 必须能分别显示"连接正常"与"手机可见：是/否"，不要合成一个绿灯
- `remote/relayTargets` 的 `available` 字段就是为此存在（§4.6）
- 日志里明确区分 `stdio ok` / `relay failed`，不要把中继错误混进连接错误

另一个容易忽略的：**中继 URL 在用户自己的服务器上**，App 无法验证它的可达性，
只能由远端实测。所以"配置中继成功"不等于"远端能用它"。

### 8.8 手机端持有的是过期清单

手机缓存 `remote/relayTargets` 的结果（否则每次启动都要拉一遍）。但清单会变：
用户在桌面删了项目、关了开关、改了 session。手机拿旧清单去连，会得到中继的
`-32000`（无电脑连接）。

缓解：手机端必须把 `-32000` 解释为"该项目暂不可用"并提供刷新，**不能**无限重试
（那会变成 §4.3 里同样的问题，只是换到手机侧）。刷新入口至少要有一个。

### 8.9 手机 → 远端项目的鉴权链路（原为最高风险，已实证并可接受）

**结论先说：链路是通的，方案 a 成立，但有一个必须记录的 scope 副作用。**

原先的疑问是：手机连中继用本机发的 device token（`wsTransport.ts`），而每个远端
app-server 是另一台机器、有独立的设备表，它凭什么信任这条经中继来的连接？

已核实的答案（不是假设，是读代码确认的）：

- 远端以 `--relay` 启动时走 `serve_stdio_with_relay`（`server.rs:2019`）
- 它在**远端本机**调 `pairing.seed_local_device("relay-bridge")`
  （`server.rs:2961`）铸一枚 **`Scope::Control`** 设备（`pairing.rs:288`），
  然后用这枚 token 连**自己的环回 ws**（`ws://127.0.0.1:<port>/ws`）
- 中继客户端（`yi_agent_relay::run_client`）带这枚 token 连环回 ws，
  再出站连中继的 `/connect?session=<id>`

也就是说：**远端始终用"自己的本地设备"身份处理所有经中继来的帧**。
手机的真实 token 在中继那一层就结束了，从不被远端校验。因此：

- ✅ 手机经中继访问远端**不需要**把 token 分发到远端，与 §6 第 1 条不冲突
- ✅ 不需要远端的设备表里有这台手机

**但必须记录一个副作用（这是本节的真正价值）**：

所有经中继进入远端的帧，在远端看来都来自**同一台 Control 设备**
（`relay-bridge`）。这意味着：

1. **手机的 scope 被"压平"成 Control**。一个 `Observe` 只读手机，经中继连到远端
   后，在远端侧是 Control —— 它**有能力**执行写操作。当前是否可被利用取决于
   手机端是否自己限制，但**远端不再提供第二道防线**
2. **无法区分"哪个手机发的"**。多条手机连接在远端是同一个人。若未来要做
   per-device 审计或按设备撤销权限，这条链路做不到
3. `remote/relayTargets` 的 `Observe` 不返回 `session`（§4.6）因此**更重要**了：
   它是唯一拦住只读设备拿到凭据的地方，不能省

本设计接受这个副作用，理由是：中继本就不鉴权、session 即凭据（§4.6 前提 3），
安全边界已经在"谁知道 session"上。但要在 §7 非目标里明确写"不做 per-device
audit / 不做 scope 透传到远端"，避免后续误以为它已经存在。

### 8.10 远端会留下"故意不死"的 daemon（既有机制与新场景的冲突）

这是本轮核对中发现的、本设计必须**明确认下**的一个行为，不是 bug，但用户在远端会
观察到它。

事实链（已核实）：

1. 每个 app-server 启动时 `spawn_resident_watch_loop`（`server.rs:1913-1921`），
   每 30s 扫 `resident_dir`，对登记的项目调 `ensure_daemons`
2. `ensure_running`（`supervisors/src/daemon.rs:61`）发现没有 daemon 就
   `spawn_detached`
3. `spawn_detached` 用 **`setsid`** 让子进程脱离会话，注释写明
   "the child deliberately outlives the caller: the desktop app must be able to
   quit without its projects' daemons going down"
4. 这个 daemon 的存在是为了"关掉 app 后看板/常驻任务继续跑"

在**本地**这是正确设计。套到**远端**会产生两个后果：

- **远端进程残留**：ssh 断开、远端 app-server 退出后，它拉起的 daemon **不会**
  跟着退出（`setsid` 就是要它别退）。用户 `ssh` 上去 `ps` 会看到
  `yi-agent daemon serve` 在跑
- **多项目 = 多个 daemon 并存**：每个项目一个 `instance_root`（§4.6），各自有
  自己的 `resident-daemons.json`，所以是 N 个 daemon 各守一个项目目录。这是
  内存占用随项目数线性增长的来源

**本设计的取态：不改这个行为**，理由：

- 改它等于改本地已验证的语义（"关 app 后看板继续跑"是明确的产品行为）
- 远端残留的 daemon 是**用户自己的机器、自己的项目**，不是共享资源泄漏
- 强行在 ssh 断线时杀 daemon，会破坏"远端常驻任务继续跑"这个本来可能有用的能力

但必须做到两点，否则用户会以为功能有 bug：

1. **在设置页/文档里说明**："远端项目会在远端留下 `yi-agent daemon serve`
   进程；这是为了让常驻任务继续跑，与本地行为一致"
2. **提供清理路径**：文档给出 `ssh <host> pkill -f 'yi-agent daemon serve'`
   （或未来的 `remote/cleanup` 命令，本期**不做**）

**验收时要确认的是"没有失控"而不是"没有残留"**：连 3 个远端项目后，
每台机器上的 `daemon serve` 数量应等于项目数，**不应**出现同一项目多个 daemon
（`ensure_running` 的 `is_running` 判据应保证幂等，这条要验）。

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
6. **关开关即断开（全部 slot）**：**同时连两个远端项目**的状态下关掉 flag，
   `ps aux | grep ssh` 中**两个** ssh 子进程都消失（不能只杀当前活动的那个，§5.1）
7. **工具层零改动**：`git diff --stat main -- yi-agent-rs/crates/yi-agent-tools`
   为空
8. **App 体积不增长**：`du -sh /Applications/yi-agent.app` 与改动前同量级
   （约 51MB，**不得**出现 +30MB 级增长）；`find yi-agent.app -name '*linux*'`
   无输出
9. **缓存命中不重下**：同一版本连第二台同架构远端，下载次数为 0
   （`~/.yi-agent/remote-cache/<version>/` 下无新增文件，或者日志无 download 行）
10. **解压在远端做**：本机不出现任何 xz/gzip 解压依赖；
    `cargo tree -p desktop | grep -E 'xz2|flate2'` 为空
11. **远端 cwd 正确**：远端模式下 `thread/start` 成功，返回的 `cwd` 是**远端路径**
    （如 `/home/dev/proj/foo`），不是本机路径、也不是
    `cwd is not a valid directory` 错误（§4.4）
12. **本机路径不泄漏到远端**：远端模式下全流程日志中不出现
    `/Users/<本机用户>/` 前缀被传给远端 `canonicalize`
13. **断线不无限重试**：拔网线后，重试间隔达到 60s 上限且连续 5 次后停止，
    UI 出现「重连」按钮（§4.3）
14. **附件走分片**：远端模式上传一张 3MB 图片成功；日志中单帧不超过
    1 MiB（§4.5）
15. **并行连两个远端项目**：项目 A 与 B 各开一个会话，`ps aux | grep ssh`
    显示**两条** ssh 子进程同时存在；A 的长任务运行期间 B 可正常对话（§4.6）
16. **响应归属正确**：A/B 两槽位各发一个请求，响应分别落到各自的 pending 上。
    **即使两边 id 恰好相同也必须正确**（因为响应绑定在本 client 的 transport 上，
    §4.6 障碍五）；若实现了可选偏移，再加一条断言"两个 client 的 id 集合不相交"
17. **事件不串台**：A 项目的 `turn/*` 通知不出现在 B 项目的会话视图里
    （§4.6 决策三）
18. **不设硬上限**：连满 10 个远端项目后，第 11 个**仍然能连上并正常工作**；
    设置页出现软提示，但没有任何操作被阻断（§4.6 范围约束）
19. **每项目一条 relay session**：连两个远端项目后，中继侧能看到两个不同
    session 各有一条电脑连接；手机端能同时连上两个，各自看到自己的会话（§4.6）
20. **手机能审批远端项目的工具调用**：手机在远端项目会话里收到审批弹窗，
    点同意后该 turn 继续；同时桌面端弹窗自动关闭（`approvalResolved`）（§4.6）
21. **跨槽位故障隔离**：杀掉 A 的 ssh 进程，B 的会话与本地 sidecar 均不受影响
    （§4.3 第 4 条）
22. **host key 不静默接受**：连一台 known_hosts 里没有的机器，连接必须**失败**
    并进入信任流程；`~/.ssh/known_hosts` 在用户确认前不新增条目（§6 第 5 条）
23. **同主机的两个项目状态隔离（必测）**：同一台远端主机上开项目 A 与 B，
    各自的会话列表只含自己的会话；远端
    `ls <home>/.yi-agent-projects/` 下有两个目录，且各自的
    `workspaces.json` 不含对方的项目路径（§4.6）
24. **远端路径不存在时快速失败**：填一个不存在的远端路径，连接在
    `test -d` 阶段就报「远端路径不可用」，不是等到 app-server 启动后报
    `working directory does not exist`（§4.1 第 4 步、§4.4）
25. **每阶段有超时**：把 ssh 指向一个黑洞地址（如 `10.255.255.1`），
    20s 内必须报「无法连接」并给出重试，UI 不能永远停在"连接中"（§4.1）
26. **并发安装不产生半截文件**：同时触发两个 `(host, version)` 相同的安装，
    只有一个真正执行；远端 `<bin>` 可执行且 `--version` 正确（§4.1）
27. **session 派生不撞车（必测，静默故障）**：连两个远端项目后，中继侧
    **同时存在**两条电脑连接，且各自对应的项目没有互顶；额外用单测断言
    "同 configured_session + 同 profile.id ⇒ 同 session"、
    "同 configured_session + 不同 profile.id ⇒ 不同 session"
28. **不落 `default` session**：用户配置的 relay URL 里**不带** `session` 参数时，
    合成出的远端 URL 必须显式注入派生值，中继侧看到的不是 `default`（§4.6）
29. **`remote/relayTargets` 契约**：`Control` 及以上返回
    `[{profileId,label,session,available}]`；**`Observe` 返回的条目里 `session`
    必须为空**（凭据不外泄）；未开 flag 时该方法**不存在**
    （`method_not_found`，与 §5.1 一致）（§4.6）
30. **中继失败不拖垮主通道**：把 `relay_url` 指向不可达地址，远端项目仍然能正常
    对话（stdio 主通道不受影响），只是 `available=false`（§4.6）
31. **切回前台重连全部槽位**：开两个远端项目 + 本地，切到后台再切回，
    三条连接**都**恢复到 `connected`（不能只有当前活动项目恢复）（§4.6）
32. **事件名参数化后不串台**：`tauriTransport.ts` 按 slot 订阅后，B 项目的
    `item/delta` 不出现在 A 项目的消息流里；两个项目的流式文本都完整、无重复
33. **陈旧订阅被摘除**：反复切换项目 20 次后，每个槽位只有**一条**活跃订阅
    （否则文本会逐字重复）（§4.6）
34. **远端 argv 的 `--listen` 只能是 stdio**：对 `remote_argv()` 的单测断言
    argv 含 `--relay`，且 `--listen` 的值（若有）为 `stdio://`、**不含**
    `ws://`/`relay://`；真机验证远端进程的
    `app_server_mode` 落在 `StdioWithRelay` 而非 `PureRelay`
    （否则桌面完全连不上）（§4.6）
35. **不支持的架构快速失败**：对一台 ARM Linux（`aarch64-unknown-linux-gnu`）
    配置远端，连接必须在**下载前**失败，文案点明"该架构无预编译产物"而非"网络
    错误"，并提示路径 3（§2.4.5）
36. **`build/info`/`flags/list` 只问本地**：当前活动项目是远端时，
    这两个请求仍发往本地槽位；把远端槽位断开后，实验开关面板状态**不变**
    （证明没读远端）（§5.3.2）
37. **dev-only 边界**：release 构建的 App 里「远程」入口不渲染、
    `remote/*` 方法不存在；dev 构建里正常可用（§5.3.1）
38. **`clear_child` 只清自己的槽位**：开两个远端 + 本地，杀掉远端 A 的 ssh，
    本地与远端 B 的 RPC **仍然可用**（不能报 "sidecar not running"）（§4.6 障碍二）
39. **陈旧句柄不误删新进程**：远端 A 退出后立刻重启，重启期间旧句柄的清理
    不得把新进程的句柄删掉（否则 A 永久不可用）（§4.6 障碍二）
40. **会话归属不分裂**：在远端项目里新建会话后，**立刻**在会话列表里看到它
    （不是刷新后才出现、也不是出现在别的项目下）；再建第二个会话，两者都归属
    同一项目（验证 `project_key` 没有因拼写差异分裂，§4.4）
41. **`settings.json` 缺失不致病**：隔离后的远端没有 `settings.json` 时，
    连接与对话正常（逐字段回默认），说明页里明确指出"远端配置独立、当前为空"（§4.8）
42. **`transportFactory` 向后兼容（必测，两端共用代码）**：
    ① 老存储（单条 `{url,token}`）+ 新版 App → 仍能用，且威胁不到桌面路径；
    ② 新存储（列表）+ 老版 App → 至少第一条可用，不崩；
    ③ 桌面（无存储）仍走 `tauriTransport`，多连接改动**未**让它走进 ws 分支（§4.6）
43. **手机经中继访问远端可用**：连一个远端项目，手机经
    `wss://<relay>/ws?session=<远端>` 发起 `thread/list`，成功返回（§8.9 已实证
    链路通，此判据防止实现时把 `--relay` 参数接错而回归）
44. **改中继配置不断开远端**：连着两个远端项目时，在设置里改本地 relay URL，
    两个远端项目的会话**保持连接**（只有本地槽位重启）（§4.6 障碍一）
45. **远端不带未派生的 URL**：远端槽位的 argv 里 `session=` 的值**不等于**本地
    的 configured session（必须含 profile.id 后缀）；单测断言两个调用点用的是
    不同函数（§4.6 障碍一）
46. **远端 daemon 不失控**：连 3 个远端项目后，每台远端机器上
    `pgrep -fc 'yi-agent daemon serve'` 等于该项目数（不是 0，也不是 2 倍以上）；
    同一项目重复重连 3 次后数量**不增长**（`ensure_running` 幂等，§8.10）
47. **instance_root 由探测的 home 派生**：远端 `HOME` 指向非常规路径
    （如 `/home/dev` 之外的 `/data/dev`）时，`YI_AGENT_HOME` 落在
    `/data/dev/.yi-agent-projects/<id>/.yi-agent`，而不是项目目录下、也不是本机猜的值；
    另测 `HOME` 为空时连接**失败并说明**（§4.1）
48. **`remote.json` 并发写不丢更新**：桥接层连续写 50 次（模拟连点、含多线程）
    后文件仍能解析、`profiles` 数量正确、无 `remote.json.tmp` 残留
    （验证 flock + 唯一临时名，§4.9）
49. **一个槽位卡住不拖垮其他（必测）**：让远端 A 的 ssh 阻塞（指向黑洞地址，
    或对远端 app-server `kill -STOP`），本地与远端 B 的对话**仍正常**；
    确认锁是每槽位一把，不是外层那把（§4.6）
50. **只 cd 不设 env 会共用状态（负向测试）**：手动把 `remote_argv()` 里的
    `env YI_AGENT_HOME=...` 去掉，连两个远端项目，确认它们**确实**共用
    `$HOME/.yi-agent`（证明这条 env 是隔离的唯一来源，而非 cwd）；恢复后
    两者分开（§4.6、§4.1 第 8 步）
51. **远端无模型配置有指引**：清空远端 `models.json` 后建会话，出现说明页并给出
    远端配置路径，而不是一句 `no model configured`（§4.8）

## 10. 实施顺序

多槽位改造（第 4 步）是最大的一块，且**必须先于任何远端功能**：先在只有本地
sidecar 的情况下把单例改成槽位表、把事件名改成带 slot、把 `RpcClient` 的 id 加
偏移，验证本地功能零回归，再往这个骨架上挂远端。反过来做会陷入"远端 bug 与重构
bug 分不清"的泥潭。

0. **冒烟验证 §8.9 的链路** — 手动起一个远端 app-server 带 `--relay`，
   用现有 iOS 端经中继发一次 `thread/list`。§8.9 已通过读代码确认链路成立
   （`seed_local_device("relay-bridge")`），这一步是防止环境性偏差，成本低
   但不再阻塞设计
1. **flag 注册 + stable 闸门测试** — 最小可验证单元，先落地
2. **`remote.json` 读写与校验** — 纯逻辑（host 白名单、路径校验、项目映射），无 IO
   依赖，最好测
3. **下载 + 缓存 + scp + 远端解压** — 只做路径 1（本机下载）
4. **桥接层多槽位改造**（无远端逻辑，纯重构）：
   - `Sidecar.child` → `HashMap<ProfileKey, CommandChild>`
   - `rpc`/`rpc_respond` 增加 `slot` 参数
   - 事件名改 `app-server://{kind}/{slot}`
   - **`kill_sidecar` 加槽位参数**，另起 `kill_all_slots`（§4.6 障碍一）
   - **`set_relay_url` 收窄为只重启本地槽位**（§4.6 障碍一）
   - **`clear_child` 加槽位 + 作业 token 校验**（§4.6 障碍二）
   - **锁改成每槽位一把**（外层只保护查表，写 pipe 时不持外层锁，§4.6 障碍三）
   - 事件名参数化是**必做**；id 偏移是**可选**（§4.6 决策四）
   - **验收：此时只有 `Local` 一个 slot，全部既有测试通过、行为不变。
     特别跑一遍"改 relay URL"与"重启 sidecar"两条既有路径，确认语义未变**
5. **把"起远端命令"收敛成一个纯函数**（`fn remote_argv(profile) -> Vec<String>`，
   无 IO，可直接单测）——包含 `cd '<remote_path>'` 与 `YI_AGENT_HOME` 两段。
   先写这个函数和它的测试，再接到 bridge 上（§8.6：这是唯一拼 shell 的地方）
6. **`bridge.rs` 接入远端 slot** — 用上一步的纯函数起 ssh 子进程；每 slot 独立监督
   循环；**每个 slot 必须传独立的 `instance_root`**（§4.6、§8.5）；
   每个远端 slot 的 ssh 命令行加上 `--relay <session>`，session 由
   `(本机 session, profile.id)` 派生
7. **路径语义** — 落实 §4.4 方案 B：远端模式下前端不传 `cwd`
8. **前端多 client 改造** — `Map<ProjectKey, RpcClient>`，按活动项目取用。
   这一步必须同时做完 §4.6「前端侧的单客户端假设」表里的五件事，且**各自单独
   验证**，因为它们都只在开第二个项目时才暴露：
   - `tauriTransport.ts:51-54` 事件名参数化（否则所有槽位的帧进同一个 client）
   - `App.tsx:1375-1379` `onVisibilityChange` 改为遍历所有槽位重连
   - 每槽位严格 dispose 自己的订阅（`App.tsx:1564` 附近已记录此风险）
   - `clientRef` 的 50 处引用逐条判断用"当前活动"还是"指定槽位"
   - `build/info` 与 `flags/list` 钉死只发本地槽位（§5.2 口径）
9. **前端设置页 + 进度事件 + 断线状态** — 含 §4.3 的手动重连按钮
10. **关开关时杀进程**（必须杀**全部**远端 slot）
11. **`remote/relayTargets`** — app-server 侧唯一新增方法（只读）。先写契约测试
    （含 flag 关闭时 `method_not_found`、scope 校验），再接前端（§4.6）
12. **手机端多连接** — 改的是共用的 `transportFactory`（无独立 iOS 项目，
    桌面与手机同一份 React 代码，§4.6）。`RemoteConfig` 扩成列表且向后兼容；
    `transportFactory` 签名从"返回单个 Transport"改为可按项目取；
    列表按项目分组；消费 `remote/relayTargets` 拿清单。
    **必须单独验证桌面路径未被破坏**（`tauriTransport()` 分支）
13. **端到端真机验证** — 走完 §9 的 **51 条**判据。并行相关的
    15–19、23、27、31–33 必须**双项目同时开**才测得出来；25 需指向不可达地址；
    28 需把 `relay_url` 指向不可达地址
14. 补齐路径 2（远端直下）与路径 3（`--from`）

第 3 步先只做一条安装路径是刻意的：解析压缩包、探测架构、版本校验这些逻辑在
"路径 1"里已经全部覆盖，路径 2/3 只是换个字节来源。先打通一条，避免三路并行时
分不清是安装逻辑的问题还是渠道的问题。
