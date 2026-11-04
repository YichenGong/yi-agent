# 远端验收报告：A16 / A17

- 日期：2026-11-05
- 执行器：`scripts/plugins/e2e-remote.mjs`
- 设计：[2026-11-05-remote-acceptance-executor-design.md](../specs/2026-11-05-remote-acceptance-executor-design.md)
- 远端主机：`agenticlabs@10.103.132.15`（macOS 26.5.2 / arm64 / Mac16,11）
- 实测产物：`.artifacts/plugins/remote-a16.json`、`.artifacts/plugins/remote-a17.json`
- **结论：A16 通过。A17 未通过，且不应通过——它只被证明了一部分。**

## 两台机器的身份

| | 值 |
|---|---|
| 远端 hostname | `agenticlabsMac-mini-2.local` |
| 远端 OS | macOS 26.5.2 |
| 远端架构 | aarch64-apple-darwin（受支持目标） |
| 本机版本 | `0.1.13-dev+32365036` |
| 远端版本 | `0.1.13-dev+32365036`（**相等**） |
| 协议版本 | 1，两端一致 |

本机与远端是两台不同机器：`hostId` 非 loopback，且 ssh 走的是另一台 Mac mini。

## A16：通过

| 判据 | 结果 | 实测 |
|---|---|---|
| 两端版本精确相等 | ok | 两侧都是 `0.1.13-dev+32365036` |
| 协议版本兼容 | ok | `protocolVersion: 1` |
| 宿主实例身份存在 | ok | 每次握手返回不同 UUID |
| 远端非 loopback | ok | `agenticlabsMac-mini-2.local` |
| 架构有预编译产物 | ok | `aarch64-apple-darwin` |
| 本地不运行远端程序 | ok | 本机进程表里没有执行该二进制的进程 |

最后一条的实现值得说明：最初用「命令行包含该路径」判断，结果每次都匹配到
**执行器自己**（`remotePath` 是它的参数），于是一个永远红的检查。现在比较的是
内核记录的 `ps -o comm=`，只有可执行文件真的是那个路径才算。

## A17：未通过（`unsupported`）

已证明的四条：

| 判据 | 结果 | 实测 |
|---|---|---|
| 断开后远端无残留进程 | ok | 0 个 `app-server` 进程存活 |
| 重连得到新身份 | ok | UUID 每次不同 |
| 旧绑定对新身份不成立 | ok | 完整身份比较为 false |
| 代次数字被复用 | ok | 两次都是 `generation: 1` |

**未证明的部分**（因此整条不能算通过）：

- 离线缓存：`plugin_host/remote_assets.rs` 的 `FetchRequest` 只有定义、零调用点。
- 跨宿主资产获取：`plugin/install` 在本机 `plugin_manager` 完成下载与验签，
  app-server 没有对外提供「把包字节交给远端」的 RPC。
- 过期客户端、headless 更新：不在本执行器覆盖范围。

第三条与第四条放在一起才有意义：远端重连后 `generation` **仍然是 1**。只比代次
的实现会把重连判成同一台机器——这正是 A17 要证明不会发生的事。完整身份比较
（kind + instanceId + generation）才使旧绑定失效。

## 这轮跑出来的五个缺陷

全部只有真连一台机器才会暴露，全部已修并钉了测试：

1. 远端探测被拼在二进制后面（`yi-agent hostname` 被当成未知子命令）。
2. 本地进程检查匹配到执行器自身，导致该判据永远红。
3. `--remote-host` 默认 `""` 覆盖了环境变量，设了也不生效。
4. 身份解析读顶层，而执行器把它嵌套在 `remoteIdentity` 下。
5. 执行器返回 `unsupported` 却被记成 `pass`——因为它的退出码是 0。
   这一条最危险：它正是这份报告存在的理由所要防止的粉饰。

## 复算方法

```bash
export YI_AGENT_ACCEPTANCE_REMOTE_HOST=agenticlabs@10.103.132.15
export YI_AGENT_ACCEPTANCE_REMOTE_PATH=/Users/agenticlabs/Documents/YichenExperiments/yi-agent-env/test-remote/yi-agent
export YI_AGENT_ACCEPTANCE_REMOTE_HOME=/Users/agenticlabs/Documents/YichenExperiments/yi-agent-env/test-remote/home
export YI_AGENT_ACCEPTANCE_LOCAL_VERSION=$(yi-agent --version | awk '{print $2}')

node scripts/plugins/e2e-remote.mjs --case A16 --json
node scripts/plugins/e2e-remote.mjs --case A17 --json
```

A16 应输出 `"verdict":"pass"`；A17 应输出 `"verdict":"unsupported"` 且 `reason`
提到 `FetchRequest has no caller`。若 A17 输出 pass，说明判定逻辑坏了。

## 前置条件

远端需要先有同版本的二进制。本轮用 `scp` 投递 51,984,240 字节到
`/Users/agenticlabs/Documents/YichenExperiments/yi-agent-env/test-remote/yi-agent`，
`--version` 两端一致。没有投递时执行器会判 `fail`（不是 unsupported）：投递是
执行的一部分，缺了就是没跑。

## 对整体验收的影响

A16 从 `unsupported` 变为 **`pass`**。
A17 保持 **`unsupported`**，但理由从「没有远端机器」变成「远端资产拉取路径未
实现」——缺口更具体，也更接近可修。

整体 A01–A20 因此从 14 pass / 6 unsupported 变为 **15 pass / 5 unsupported /
0 fail**（在同一台机器上重跑全量时，A03/A07/A08/A20 的状态取决于是否构建了
`.app` 与官方签名包，与本报告无关）。

完整独立发布这一条仍然**不能**标完成：A17 未通过，且 A03/A20 需要发布授权。
