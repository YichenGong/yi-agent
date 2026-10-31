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

全部改动集中在 **desktop 的 Tauri 桥接层**（`desktop/src-tauri/`），
`yi-agent-app-server` 一行不改。理由见 §3.2。

| 组件 | 职责 | 位置 | 依赖 |
|---|---|---|---|
| `remote/profile.rs`（新增） | 远端配置读写与校验：host、远端项目路径、二进制版本。存 `$YI_AGENT_HOME/remote.json` | `desktop/src-tauri/src/` | 无（纯逻辑，好测） |
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
4. 查 `~/.yi-agent/bin/<version>/` 是否已存在且 `--version` 匹配
5. 不存在则安装（§2.4 的三级优先），进度以 `remote/install/progress` 事件回推
6. 安装完起 `ssh <host> '<path> app-server --listen stdio://'`，接管 stdio
7. 前端拿到 `remote/status` 的 `connected`，正常开始对话

### 4.2 日常连接

直接跳第 4 步之后。**不重复探测架构**——profile 里缓存上次探测结果，架构不会变。

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
2. **连续失败 N 次后停止自动重试**（建议 N=5，约 2 分钟），转为「断线」状态，
   UI 显示明确原因与「重连」按钮，由用户手动触发
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

`remote.json` 里除了 `host`，还要存一对路径：

```json
{
  "host": "dev@10.0.0.5",
  "projects": {
    "/Users/yichen/proj/foo": "/home/dev/proj/foo"
  },
  "binary_version": "0.1.10"
}
```

- **key**：本机项目路径（绝对、canonical）
- **value**：该项目在远端的对应路径（绝对）

前端目前 `thread/start` 会传本机路径。**远端模式下必须停止这样做**——本机路径
送到远端 `canonicalize` 必然失败。两种处理方式，**选 B**：

- **A. 桥接层改写 `cwd` 参数**：在 `bridge.rs` 转发 `thread/start` 时替换成远端路径。
  缺点：桥接层要解析并改写 JSON-RPC 参数，破坏"桥接层只转发不解析"的现有性质
  （见 `bridge.rs:29-46` 的 `classify`，它只看有没有 `method`/`id`，不看 content），
  而且每新增一个带路径的 RPC 都要记得改，漏一个就回到本节的 bug。
- **B. 远端 cwd 由 ssh 命令的 `cd` + app-server 的 `--workdir` 决定**：
  ssh 命令为 `ssh host 'cd <远端项目路径> && exec <bin> app-server --listen stdio://'`，
  且前端在远端模式下**不传 `cwd`**。此时 `resolve_thread_cwd` 走
  `None => cfg.workdir` 分支，直接用进程的 workdir。
  优点：桥接层保持纯转发；路径映射只影响"起进程时 cd 到哪"这一处。

**B 的代价**：远端模式下「一个会话一个项目」被固化为「一个 app-server 进程一个项目」。
这正好与 §2.3「一项目一个远端进程」一致，不构成额外限制。

**B 需要注意**：前端仍有若干路径会传 `cwd`/绝对路径（如历史会话的"最近目录"、
工作区面板）。这些在远端模式下要么不显示，要么只作展示用（不能用于 `canonicalize`）。
具体清单属于实现期任务，本设计只钉住原则：**远端模式下本机路径不得进入任何会被
远端 `canonicalize` 的字段**。

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

一个仍需注意的点：`view_image` 工具读取图片时是**在远端读远端文件**，返回的
base64 delta 也受同一帧限制。若既有实现已按块返回（`fs/readBlock` 式的
`MAX_HTTP_BODY_DELTA_BYTES` 思路），则无需改动；实现期需确认这条路径同样分块。

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

#### 与 `--relay` 的冲突（并行带来的真实约束，必须处理）

`--relay` 的语义是**一个 session 对应一个电脑侧 agent 连接**：

```
// yi-agent-relay/src/lib.rs:1-9
//! 反向 WSS 中继:两端都出站连接,中继按 `session` 配对并双向转发帧。
//! - `app → agent`:送给该 session 的**唯一**电脑连接;
```

而今天 `sidecar_args`（`bridge.rs:161`）只给**那一个**本地 sidecar 加 `--relay`。
改成多槽位后，问题变成：**多台 ssh 子进程各自带同一个 `--relay <session>` 会怎样？**

答案是会打架：中继认为"该 session 的唯一电脑连接"只能有一条，后连上的会顶掉先连上
的，手机端的帧被路由到**哪一个项目的远端**就是不确定的——这会让用户在看 A 项目时
收到 B 项目的回复。

**决策：`--relay` 只挂在 `ProfileKey::Local` 上，远端 slot 一律不带。**

理由：
- 手机端连中继的意图是"看我这台电脑"，不是"看某个远端项目"
- 远端 slot 加 `--relay` 需要为每个项目分配不同的 relay session，这等于把中继
  也改成多租户，远超本设计范围
- 不带 `--relay` 的远端 slot 完全正常工作（stdio 就够），只是手机看不到它

**已知限制（写进 §7）**：远端项目的会话**不能**从手机端查看。若将来需要，
那是"中继多 session"的独立设计。

如果实现时发现确有需求，**最小可行方案**是：为该远端 profile 单独配置一个
`relay_session`（`remote.json` 里的可选字段），与本地 session 区分开。但这是
可选增强，不是本设计的必做项。

#### 为什么值得做这个改造

不做并行，用户切项目就要等几秒重连，且**不能在一个项目里跑长任务的同时去看另一个
项目**——而"让 agent 在远端干活"的典型用法恰恰是长任务。这个成本是每次交互都付，
而多进程改造是一次性的。

#### 范围上的约束

- 本地 sidecar 仍然是**一个**（`ProfileKey::Local`），不引入本地多实例
- 同时连接的远端项目数**不设硬上限**，但应给出软提示：超过 N（建议 4）个时在
  设置页提示资源占用
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
  与 §6.1「不做凭据转发」直接冲突
- **C. 连上后先探测，缺失时给出可操作指引**（本设计采用）

C 的具体行为：
1. 建会话前调 `models/list`（或等价探测）。若为空/报错，**不进入主界面**，
   而是显示一个说明页：「远端尚未配置模型。你可以：① 在远端运行
   `yi-agent models add`；② 手动把模型配置放到 `<远端 home>/.yi-agent/models.json`」
2. 说明页里给出**远端实际解析到的配置路径**（从远端读，不要在本机猜）
3. 明确写出「不会自动复制本机密钥」，让用户知道这是有意的安全选择

这样既不偷偷搬密钥，也不让用户在一句 `no model configured` 面前无从下手。

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

第 3 条容易被漏掉：如果只在 UI 隐藏、不杀进程，用户会处于"看不见但远端还在跑、
还在烧 token"的状态。并行后这个风险被放大 N 倍——漏杀一个 slot 就有 N-1 个进程
在后台继续跑。**验收时必须开两个远端项目再关开关**，确认两个都不在了。

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
3. **远端路径同样校验**：必须是绝对路径且不含 shell 元字符。
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
- **不做远端项目的手机端查看**：`--relay` 只挂本地 slot（§4.6）。远端 slot 不带
  中继，手机端看不到远端会话——这是有意收窄，避免中继多 session 的改造
- **不做凭据转发**（§6.1）
- **不做跨会话状态保留**（§4.3）
- **不做零安装模式**（§2.2）
- **不把二进制内置进 App**（§2.4）：体积对不用的人是死重，改为惰性下载 + 缓存
- **不做 Windows 远端**：暂只支持 Linux/macOS 远端。Windows 的路径语义、shell 差异
  是独立的一大块，本期不碰。
- **不改工具层**：所有工具走远端 app-server 内部执行，`yi-agent-tools` 一行不改。
  这是本设计最省力的地方，也是最容易被破坏的地方——见 §8.3。

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

正确处理已写在 §6.5（严格校验 + 用户可见的显式信任步骤）。这里只强调风险性质：
它失败得早、信息量少，很容易被误判成"ssh 连不上"，从而被错误地"修复"成放宽
`StrictHostKeyChecking`。实施时**不允许**把它修成 `accept-new` 或 `no`。

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
18. **中继只挂本地**：`ps axo command | grep "app-server --listen"` 中，
    `--relay` 只出现在本地 sidecar 那条命令上；所有远端 ssh 命令行里没有 `--relay`
    （§4.6）
19. **跨槽位故障隔离**：杀掉 A 的 ssh 进程，B 的会话与本地 sidecar 均不受影响
    （§4.3 第 4 条）
20. **host key 不静默接受**：连一台 known_hosts 里没有的机器，连接必须**失败**
    并进入信任流程；`~/.ssh/known_hosts` 在用户确认前不新增条目（§6.5）
21. **远端无模型配置有指引**：清空远端 `models.json` 后建会话，出现说明页并给出
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
5. **`bridge.rs` 接入远端 slot** — 按 profile 起 ssh 子进程；每 slot 独立监督循环
6. **路径语义** — 落实 §4.4 方案 B：ssh `cd` 到远端项目路径，前端不传 `cwd`
7. **前端多 client 改造** — `Map<ProjectKey, RpcClient>`，按活动项目取用
8. **前端设置页 + 进度事件 + 断线状态** — 含 §4.3 的手动重连按钮
9. **关开关时杀进程**（必须杀**全部**远端 slot）
10. **端到端真机验证** — 走完 §9 的 20 条判据，其中 15/16/17/18 必须双项目并行测
11. 补齐路径 2（远端直下）与路径 3（`--from`）

第 3 步先只做一条安装路径是刻意的：解析压缩包、探测架构、版本校验这些逻辑在
"路径 1"里已经全部覆盖，路径 2/3 只是换个字节来源。先打通一条，避免三路并行时
分不清是安装逻辑的问题还是渠道的问题。
