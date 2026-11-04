# 远端验收执行器（A16/A17）设计

日期：2026-11-05
状态：已批准，待实现
上游：[完整插件独立发布设计](./2026-11-02-kanban-independent-release-design.md) §13 的 A16/A17

## 1. 要解决的问题

验收矩阵里 A16/A17 要求**跨机器**证据：

| ID | 完成判据 |
|---|---|
| A16 | 远程 UI 精确匹配宿主发布包，两端兼容各自检查，独立签名验证；本地不运行远端程序 |
| A17 | 远端切换、失联、离线缓存、过期客户端及重连不复用旧绑定；headless 更新可用，旧桌面不永久阻塞更新，离线重启已验证版本不误当新安装 |

上一轮全量验收把这两条记成 `unsupported: scripts/plugins/e2e-remote.mjs`（文件不存在）。
现在有一台真实远端机器可用，所以要把它写成可复算的执行器，而不是继续留空。

**这个执行器不是为了把 A16/A17 刷成 pass。** 它要回答的是：在真实远端上，
哪些判据真的成立、哪些还没实现。已有的反例思路（见 `e2e.mjs` 头注释）在这里同样
适用——一个验收执行器的价值不在于它能连上机器，而在于它不会把没验证的东西
算成验证过。

## 2. 已实测的事实（设计依据，非假设）

2026-11-05 在 `agenticlabs@10.103.132.15` 上实测：

| 事实 | 证据 |
|---|---|
| 远端是 macOS 26.5.2 / arm64 / Mac mini | `sw_vers`、`uname -m`、`sysctl hw.model` = Mac16,11 |
| 该架构是受支持目标 | `remote/rpc.rs` 的 `SUPPORTED_ARCHES` 含 `aarch64-apple-darwin` |
| 二进制可投递并运行 | 51,984,240 字节 scp 后在远端 `--version` 返回 `0.1.13-dev+32365036` |
| ssh stdio 可完成 JSON-RPC 握手 | `initialize` 返回 `protocolVersion: 1`、`serverInfo`、`sideHostInstanceId` |
| ssh 断开后远端无残留进程 | 终止 ssh 子进程后远端 `pgrep -f 'yi-agent app-server'` 计数为 0 |
| 宿主实例身份每次启动都变 | `SideHostResources::new` 用 `Uuid::new_v4()`；两次握手 UUID 不同 |
| 代次数字会复用 | 重连后 `generation` 与上次同为 1，**只有完整身份比较才会失效** |
| 桌面绑定按完整身份比较 | `HostBindings::is_current` 比较整个 `HostIdentity`（含 `instance_id`） |

**未实现的部分（本设计不做，但要如实报告）**：`plugin_host/remote_assets.rs` 的
`FetchRequest` 只有定义、零使用点；`plugin/install` 在本机 `plugin_manager` 完成
下载/验签，app-server 的 RPC 不对外提供「把包字节给远端」的通道。所以 A17 的
「离线缓存」跨宿主语义无法在此证明。

## 3. 关键决策

### 3.1 用 ssh + stdio，不引入 SSH 协议库

沿用 `docs/superpowers/specs/2026-10-31-remote-ssh-execution-design.md` §2.1 的既有
形态：把 `ssh` 当字节管道，远端跑 `yi-agent app-server --listen stdio://`。这与产品
真实路径一致；自己实现 SSH 会在验收脚本里引入一条产品不用的通路。

### 3.2 分三层，判定与执行分离

| 单元 | 职责 | 依赖 |
|---|---|---|
| `remote-facts.mjs` | 采集远端 OS/arch/version/hostId；判 loopback | 纯函数 + 可注入命令执行器 |
| `remote-client.mjs` | ssh stdio JSON-RPC 客户端：起停、握手、请求响应配对、超时 | 可注入 spawn |
| `e2e-remote.mjs` | 编排、按用例判定、产出结构化证据 | 上面两层 |

分层的理由：判定逻辑必须能在没有远端机器时被测试。上一轮已经吃过教训——
`e2e.mjs` 的测试用进程 cwd 解析路径，导致「只在门禁里红」，很容易被当成环境抖动。
所以这里判定层全部注入式，不碰真实 ssh。

### 3.3 证据必须结构化，不能是一行文本

`e2e-report.mjs` 对 `kind: "remote"` 的要求是 `remoteIdentity {hostId, os, arch, version}`，
且 `hostId` 不能是 loopback。当前 `e2e.mjs:477` 写的是
`buildEvidenceEntry({ kind, detail: result.command })`——**从不传 `remoteIdentity`**。
所以本设计必须同时改 `e2e.mjs`：命令可以用一行 JSON 声明自己的远端身份，
执行层解析后交给 `buildEvidenceEntry`。

格式：命令若在 `acceptance-commands.json` 里声明 `"emitsRemoteIdentity": true`，
则要求它的 stdout 是一行 JSON 对象 `{hostId, os, arch, version}`；解析失败即
`fail`（不是 unsupported——脚本跑了却没按约定输出，那是实现缺陷）。

### 3.4 明确不做的三件事

1. **不实现远端资产拉取**。那是新功能（传输协议 + 摘要寻址缓存 + 验签安装），
   需要独立 spec，不能在验收脚本里顺手做。
2. **不把 loopback 当远端**。校验器已经拦这个，执行器也不提供绕过。
3. **不在中继/隧道上模拟跨机器**。本设计只认 ssh 到的真实主机。

## 4. 用例判定

### 4.1 A16：远端身份与版本精确匹配

命令：`node scripts/plugins/e2e-remote.mjs --case A16 --host <h> --remote-path <p> --json`

必须全部成立才 `pass`：

1. 远端 `--version` 与本机 `--version` **完全相同**（不是「兼容」，是相等）。
2. 握手成功，`protocolVersion` 为已知值 1。
3. 返回的 `sideHostInstanceId` 非空。
4. 远端 `hostname` 非 loopback（`127.0.0.0/8`、`::1`、`localhost` 等）。
5. 远端 OS/arch 与声明一致，且 arch 在 `SUPPORTED_ARCHES` 内。
6. 本地不运行远端程序：断言本地进程表里没有从远端路径启动的子进程。

任一条不成立即 `fail` 并给出具体哪条。

### 4.2 A17：重连不复用旧绑定（能证明的那部分）

命令：`node scripts/plugins/e2e-remote.mjs --case A17 --host <h> --remote-path <p> --json`

必须全部成立才 `pass`：

1. 第一次握手得到身份 A。
2. 断开（终止 ssh 子进程）后，远端无残留 `app-server` 进程。
3. 第二次握手得到身份 B。
4. **A ≠ B 的 instance_id**，且用与 `HostBindings::is_current` 相同的比较逻辑，
   挂在 A 上的绑定对 B **不成立**。
5. 显式记录：两次 `generation` 都是 1（若相等，正是「只比代次会误判通过」的证据）。

A17 的其余部分（离线缓存、headless 更新、过期客户端）**不在本例证明范围**。

这里有两种可选写法，必须挑一个并说明理由：

* **(a)** 脚本对自己能证明的那部分返回 `pass`，由报告层把 A17 整体判为
  `unsupported`——但同一份报告里「一条用例有 pass 证据却被判 unsupported」会让
  读的人以为执行器坏了。
* **(b)** 脚本对本例返回 `unsupported`，理由点名「只证明了重连不复用旧绑定；
  离线缓存/headless 更新/过期客户端未实现」，`remoteIdentity` 与 `checks` 仍然
  完整写出，让「已证明什么」可复核。

**采用 (b)**。理由：`verdict` 的语义是「这条验收判据整体是否被证明」，A17 是
一条判据而不是四条。给 `pass` 等于宣称整条成立，那正是要避免的粉饰。而
`unsupported` 并不丢弃证据——`checks` 里逐条列出已成立项，报告读起来是
「这条不能算过，但下面这些确实验证过了」。

**因此 A16 与 A17 在本执行器里有不同的期望判词**：A16 期望 `pass`（它能完整
证明自己那条），A17 期望 `unsupported`（它只能证明一部分）。这不是不一致，
而是两条判据的范围本来不同。

## 5. 接口

### 5.1 CLI

```
node scripts/plugins/e2e-remote.mjs \
  --case A16|A17 \
  --host <ssh host> \
  --remote-path <absolute path on the remote> \
  [--remote-home <path>] \
  [--json]
```

- `--host` 必填。缺失 → 退出码 2，`unsupported: no remote host`。
- `--remote-path` 必填，必须是绝对路径（相对路径会在远端被解析成别的目录）。
- `--json` 输出一行结构化结果给 `e2e.mjs` 解析。

### 5.2 结构化输出（`--json`）

```json
{
  "case": "A16",
  "verdict": "pass",
  "remoteIdentity": {
    "hostId": "agenticlabsMac-mini-2.local",
    "os": "macOS 26.5.2",
    "arch": "aarch64-apple-darwin",
    "version": "0.1.13-dev+32365036"
  },
  "checks": [
    {"name": "version-exact", "ok": true, "detail": "local == remote"},
    {"name": "handshake", "ok": true, "detail": "protocolVersion 1"}
  ]
}
```

`verdict` 只有 `pass` / `fail` / `unsupported`。**没有 `skipped`**——与既有
`e2e.mjs` 的三态保持一致。

### 5.3 变更 `e2e.mjs`

新增：命令声明 `"emitsRemoteIdentity": true` 时，解析 stdout 首行 JSON；
解析失败 → `fail`；解析成功 → 证据带上 `remoteIdentity`。

## 6. 错误处理

| 情况 | 判词 | 理由 |
|---|---|---|
| `--host` 缺失 | `unsupported` | 没有机器，证明不了 |
| ssh 连不上 | `fail` | 声明了主机却连不上，是配置/网络问题，不是「缺能力」 |
| 远端二进制不存在 | `fail` | 投递是执行的一部分，缺了就是没跑 |
| 握手超时 | `fail` | 跑起来了但不响应，是实现问题 |
| 版本不相等 | `fail` | 判据就是相等 |
| hostId 是 loopback | `fail` | 声明远端却给了 loopback，是伪造 |
| stdout 不是约定 JSON | `fail` | 跑了但没按契约输出 |

**关键**：只有「没有机器」是 `unsupported`。其余全部 `fail`——否则一份报告会把
「连不上」说成「不需要」。

## 7. 测试策略

* `e2e-remote.test.mjs`：注入了 fake ssh/命令执行器，覆盖上面全部判定分支。
  不连真机，因此能在门禁里跑。
* 每个判定**至少一条反例**：版本不等、hostId 是 loopback、stdout 非 JSON、
  连接失败、握手超时、实例身份未变（模拟「重连复用了旧绑定」）。
* 真机验证单独执行并归档报告；门禁只跑注入式测试。

## 8. 验收判据（本设计自身）

- `node --test scripts/plugins/e2e-remote.test.mjs` 全绿且含上述反例。
- `node --test scripts/plugins/e2e.test.mjs` 仍全绿（`e2e.mjs` 改动的回归）。
- `just plugin-acceptance-check` 全绿。
- 真机跑 `A16` 得 `pass`；`A17` 得 `unsupported` 且理由点名未实现部分，
  同时 `checks` 里能看到「重连不复用旧绑定」已成立。
- `just ci` 全绿。

## 9. 不做的事

- 不把 A17 整体标 pass。
- 不在脚本里内置任何凭据；ssh 复用现有 agent/密钥。
- 不修改远端调试目录之外的任何路径。
