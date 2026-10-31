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
3. Rust：`ssh <host> 'uname -sm'` 探测架构
4. **校验远端项目路径存在**：`ssh <host> 'test -d <远端项目路径>'`。
   不存在就**立即失败并说明**，不要等到起 app-server 时由
   `resolve_workdir` 报 `working directory does not exist`（§4.4 已说明这是必做步骤，
   因为 app-server 启动即会校验 `is_dir()`）
5. 查 `~/.yi-agent/bin/<version>/` 是否已存在且 `--version` 匹配
6. 不存在则安装（§2.4 的三级优先），进度以 `remote/install/progress` 事件回推
7. 安装完起 ssh 会话：命令里 `cd '<远端项目路径>'` 让**远端进程**的 cwd 落在
   项目目录（ssh 没有指定远端 cwd 的参数，见 §4.4），并用 `YI_AGENT_HOME` 做状态
   隔离（见 §4.6），接管 stdio
8. 前端拿到 `remote/status` 的 `connected`，正常开始对话

**每一步都要有超时，不能无限等**。参考 codex 的做法
（`exec-server/src/environment_toml.rs:53-56` 的 `connect_timeout_sec` /
`initialize_timeout_sec` 是每个 environment 的可配置项）：

| 阶段 | 超时建议 | 超时后的行为 |
|---|---|---|
| ssh 建连（第 3 步） | 20s | 报「无法连接 <host>」，附 ssh stderr |
| 路径校验（第 4 步） | 10s | 报「远端路径不可用」 |
| 安装（第 6 步） | 不设总超时，但**每阶段有进度** | 用户可取消 |
| app-server 握手（第 8 步） | 30s | 杀掉子进程，报「远端 app-server 未就绪」 |

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
`next_restart_delay`，200ms 起、上限 6.4s）。

**但远端场景不能无条件沿用本地语义**，两者失败原因不同：

- 本地 sidecar 崩溃通常是瞬时故障，快速重试是对的
- 远端 ssh 失败常见原因是**网络断了 / 服务器关机 / VPN 掉线**，这些会持续数分钟到
  数小时。用 6.4s 上限无限重试，等于每隔 6.4s 发起一次 ssh 连接，既刷日志也给服务器
  制造无意义的认证请求

**决策：远端模式给重试加约束**：

1. **指数退避上限抬高**到 60s（本地保持 6.4s 不变）
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

  因此形态固定为：

  ```bash
  ssh <host> "cd '<远端项目路径>' && exec '<bin>' app-server --listen stdio://"
  ```

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
现状做不到，有三个结构性障碍。

#### 障碍一：`Sidecar` 是单例

```rust
// desktop/src-tauri/src/bridge.rs:38-40
pub struct Sidecar {
    child: Mutex<Option<CommandChild>>,
}
```

`spawn()`（`bridge.rs:112`）是一个无限循环维护**一条**子进程；`kill_sidecar`
取的也是这一槽位；`spawn_once` 直接 `.replace(child)`。多进程必须换成
按 key 分槽的表。

#### 障碍二：JSON-RPC `id` 空间会撞车（最隐蔽）

前端 `RpcClient` 的 id 是**每实例从 0 单调递增**
（`desktop/src/lib/rpc.ts:188` 的 `this.nextId++`），而 `App.tsx:1777` 只有一个
`RpcClient`。若为每个项目各建一个 `RpcClient`，两条连接都会送出 `id=1`，两个
app-server 各回一个 `id=1` 的响应，**前端无法判断哪个是哪个**——响应会被错配到
另一个项目的 pending 上，表现为"点 A 项目却拿到 B 项目的结果"。这类 bug 极难定位。

#### 障碍三：事件通道是全局的，没有 project 维度

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

**决策四：id 命名空间按 slot 隔离（纵深防御）**

先说清楚**决策三已经解决了主问题**：事件名按 slot 分开后，A 的 client 只订阅
`app-server://message/A`，收不到 B 的帧，所以 `id` 撞车在正常情况下**不会**造成
错配。这一点必须讲明白，否则会误以为 id 偏移是必需的正确性保证。

那为什么还要做？因为存在**旁路**：

- 中继路径（`--relay`，`bridge.rs:161` 的 `sidecar_args`）会把会话暴露给手机端，
  帧可能经非 per-slot 的通路进入
- 前端的 recovery 逻辑（`rpc.ts:160-186` 的 `-32010` 自愈）会重发请求，
  重发与原始请求共存时更需要 id 可区分
- 任何"全局监听"式的兜底代码（如 `App.tsx:1564` 注释警告的陈旧监听）

一旦有帧从旁路进来，`id` 相同就会静默错配到另一个项目的 pending 上，且**不报错**——
只是结果错。这类 bug 极难定位，代价远高于加一个偏移。

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

为什么不在本地做多路复用（手机连一条、本地转发到多个远端）：本地 app-server
不认识远端进程，要它按项目路由就得改 app-server，违背 §3.2 的分层。
多连接下服务端只多一个只读 RPC（`remote/relayTargets`），隔离性天然成立。

**手机端成本（明确记录）**：iOS 端现在是单连接，要改成支持多连接 + 按项目分组
的列表。这是展示层工作，不动协议。

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
- 该 RPC 需要 `Control` 及以上 scope；`Observe` 只读设备可以看清单但不能建会话

**为什么这个 RPC 必须放在 app-server**：手机 → 中继 → **本机 app-server**，
链路上没有 Tauri 桥接层。而 `remote/*` 系列（§3.1）是 Tauri 命令，手机够不着。
所以这一个 RPC 是**例外**：它要在 app-server 里实现。

**这打破了 §3.2 "app-server 一行不改"** —— 必须诚实记录。取舍：

- 改动极小：一个只读 RPC，返回几个字符串，无状态
- 替代方案是把清单写进一个手机也能读的地方（如中继），但那要改中继且引入
  配置分发，更重
- 因此接受这**唯一一处** app-server 改动，并在 §7 明确列出

#### 远端带 `--relay` 的三个前提（容易漏，逐条写清）

`--relay` 加在**远端**的 app-server 上，所以中继连接是**从远端机器发起的出站连接**，
不是从本机。这带来三个必须满足的前提：

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
| `instance_root` | 绝对路径，**必须以 `.yi-agent` 结尾** | 由 App 派生为 `<远端 home>/.yi-agent-projects/<id>/.yi-agent`，**不由用户输入**；直接传给 `YI_AGENT_HOME`。末尾 `.yi-agent` 不是装饰——`instance_home_from` 的归一化要求如此，见 §4.6 的实测表 |
| `binary_version` | semver 字符串 | 与 App 版本解耦：远端可以跑旧版 |
| `remote_arch` | `uname -sm` 的归一化结果 | 缓存探测结果，避免每次重连都探测（§4.2） |
| `relay_session` | 可为 `null` | 由 `(本地 session, profile.id)` 派生（§4.6）。`null` 仅在未配置中继时出现 |

**存哪里**：本机 `<本机 instance_home>/remote.json`，即
`~/.yi-agent/remote.json`（或 `YI_AGENT_HOME` 指向的实例根）。**不在远端存**——
配置属于"这台电脑要怎么连"，不属于被连的那台机器。

**写入方式**：读-改-写 + temp+rename 原子替换，与
`workspace_index.rs:4` 的既有约定一致。**必须**加进程内锁（多槽位会并发写）。

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

- **不做远端常驻 daemon**（§2.3 已论证）：注意这与"并行"不冲突——并行是
  **多进程**，daemon 是**一个进程服务多客户端**。本设计明确选前者
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
16. **id 不串台（必须单独测）**：A/B 两槽位各发一个 `id` 相同数值的请求，
    两边响应分别落到各自的 pending 上；用单测断言两个 client 的 id 集合不相交
    （§4.6 决策四）
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
27. **`remote/relayTargets` 契约**：手机 scope 调用返回
    `[{profileId,label,session,available}]`；`Observe` scope 可读、不可建会话；
    未开 flag 时该方法**不存在**（`method_not_found`，与 §5.1 一致）（§4.6）
28. **中继失败不拖垮主通道**：把 `relay_url` 指向不可达地址，远端项目仍然能正常
    对话（stdio 主通道不受影响），只是 `available=false`（§4.6）
29. **远端无模型配置有指引**：清空远端 `models.json` 后建会话，出现说明页并给出
    远端配置路径，而不是一句 `no model configured`（§4.8）

## 10. 实施顺序

多槽位改造（第 4 步）是最大的一块，且**必须先于任何远端功能**：先在只有本地
sidecar 的情况下把单例改成槽位表、把事件名改成带 slot、把 `RpcClient` 的 id 加
偏移，验证本地功能零回归，再往这个骨架上挂远端。反过来做会陷入"远端 bug 与重构
bug 分不清"的泥潭。

1. **flag 注册 + stable 闸门测试** — 最小可验证单元，先落地
2. **`remote.json` 读写与校验** — 纯逻辑（host 白名单、路径校验、项目映射），无 IO
   依赖，最好测
3. **下载 + 缓存 + scp + 远端解压** — 只做路径 1（本机下载）
4. **桥接层多槽位改造**（无远端逻辑，纯重构）：
   - `Sidecar.child` → `HashMap<ProfileKey, CommandChild>`
   - `rpc`/`rpc_respond` 增加 `slot` 参数
   - 事件名改 `app-server://{kind}/{slot}`
   - `RpcClient` 注入 id 偏移
   - **验收：此时只有 `Local` 一个 slot，全部既有测试通过、行为不变**
5. **把"起远端命令"收敛成一个纯函数**（`fn remote_argv(profile) -> Vec<String>`，
   无 IO，可直接单测）——包含 `cd '<remote_path>'` 与 `YI_AGENT_HOME` 两段。
   先写这个函数和它的测试，再接到 bridge 上（§8.6：这是唯一拼 shell 的地方）
6. **`bridge.rs` 接入远端 slot** — 用上一步的纯函数起 ssh 子进程；每 slot 独立监督
   循环；**每个 slot 必须传独立的 `instance_root`**（§4.6、§8.5）；
   每个远端 slot 的 ssh 命令行加上 `--relay <session>`，session 由
   `(本机 session, profile.id)` 派生
7. **路径语义** — 落实 §4.4 方案 B：远端模式下前端不传 `cwd`
8. **前端多 client 改造** — `Map<ProjectKey, RpcClient>`，按活动项目取用
9. **前端设置页 + 进度事件 + 断线状态** — 含 §4.3 的手动重连按钮
10. **关开关时杀进程**（必须杀**全部**远端 slot）
11. **`remote/relayTargets`** — app-server 侧唯一新增方法（只读）。先写契约测试
    （含 flag 关闭时 `method_not_found`、scope 校验），再接前端（§4.6）
12. **手机端多连接** — iOS 端从单连接改为按 profile 多连接；列表按项目分组；
    消费 `remote/relayTargets` 拿清单。这是本设计唯一的非 Rust 客户端改动（§4.6）
13. **端到端真机验证** — 走完 §9 的 **29 条**判据。并行相关的
    15–19、23 必须**双项目同时开**才测得出来；25 需指向不可达地址；
    28 需把 `relay_url` 指向不可达地址
14. 补齐路径 2（远端直下）与路径 3（`--from`）

第 3 步先只做一条安装路径是刻意的：解析压缩包、探测架构、版本校验这些逻辑在
"路径 1"里已经全部覆盖，路径 2/3 只是换个字节来源。先打通一条，避免三路并行时
分不清是安装逻辑的问题还是渠道的问题。
