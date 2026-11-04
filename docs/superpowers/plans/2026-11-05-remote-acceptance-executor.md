# 远端验收执行器实施计划

日期：2026-11-05
设计：[2026-11-05-remote-acceptance-executor-design.md](../specs/2026-11-05-remote-acceptance-executor-design.md)
分支：`feat/plugin-acceptance-runner`

**前置**：`scripts/plugins/e2e.mjs` 与本分支的 `acceptance-commands.json`（已存在）。
`feat/plugin-acceptance-runner` 尚未合入 main，本计划在该分支上继续。

## Task 1：远端事实采集层 `remote-facts.mjs`

**目标**：把「远端是什么」变成可测的纯逻辑。不碰 ssh（由注入的 runner 提供）。

**Files:** Create `scripts/plugins/remote-facts.mjs`、`scripts/plugins/remote-facts.test.mjs`

**Interfaces:**
```js
export function isLoopbackHost(hostId)        // boolean
export function parseVersion(text)            // "yi-agent 0.1.13-dev+3236" -> "0.1.13-dev+3236"
export function normalizeArch(unameM, sysctlModel) // -> "aarch64-apple-darwin" | null
export function collectRemoteFacts({ run })   // -> {hostId, os, arch, version} | throws
export const SUPPORTED_ARCHES                 // 与 desktop/src-tauri/src/remote/rpc.rs 一致
```

**判据**：
- `isLoopbackHost` 认 `localhost`、`127.0.0.1`、`127.x.x.x`、`::1`、`[::1]`，大小写不敏感。
- `normalizeArch` 只返回 `SUPPORTED_ARCHES` 里的值；未知架构返回 `null`（由调用方判 fail）。
- `collectRemoteFacts` 里任一条命令失败即抛错，不返回半份事实。

**测试（每条含反例）**：
1. loopback 各写法都识别（`127.0.0.0/8` 全段）
2. 真实主机名不误判为 loopback
3. `parseVersion` 从 `--version` 输出取值；格式不符返回 `null`
4. 未知架构（如 `i686`）返回 `null` 而不是猜
5. 命令失败时抛错，不返回 `{hostId: ""}`

## Task 2：ssh stdio JSON-RPC 客户端 `remote-client.mjs`

**目标**：可靠地起停一条 ssh 子进程、完成握手、拿到 `sideHostInstanceId`。

**Files:** Create `scripts/plugins/remote-client.mjs`、`scripts/plugins/remote-client.test.mjs`

**Interfaces:**
```js
export function buildSshArgs({host, remotePath, remoteHome})  // -> string[]
export class RemoteSession {
  static connect({host, remotePath, remoteHome, spawn, timeoutMs})  // -> Promise<RemoteSession>
  initialize()   // -> {protocolVersion, serverInfo, sideHostInstanceId}
  close()        // 终止子进程；幂等
}
export function awaitInitialize({readline, send, timeoutMs}) // 可注入的握手等待
```

**判据**：
- `buildSshArgs` 生成的 argv 不含 shell 拼接；`YI_AGENT_HOME` 通过远端命令行赋值。
- 握手超时抛错，**不**返回 null 让调用方「自己看着办」。
- `close()` 幂等：调两次不抛错。
- 断连后能报告「远端是否还有残留进程」需要额外一次 ssh 查询，交给 Task 3 编排。

**测试（注入 fake spawn，不连真机）**：
1. 正常握手返回 `sideHostInstanceId`
2. 超时抛错
3. 子进程提前退出抛错（不挂死）
4. `close()` 两次幂等
5. 响应 id 不匹配时忽略，不错误配对
6. `buildSshArgs`：含空格的远程路径被正确引用

## Task 3：判定与编排 `e2e-remote.mjs`

**目标**：实现 A16/A17 的判定，产出结构化 JSON。

**Files:** Create `scripts/plugins/e2e-remote.mjs`、`scripts/plugins/e2e-remote.test.mjs`

**Interfaces:**
```js
export function parseArgs(argv)                    // -> {case, host, remotePath, remoteHome, json}
export function judgeA16({facts, local, handshake, localProcesses})   // -> {verdict, checks, remoteIdentity}
export function judgeA17({first, second, firstBindingStillCurrent, orphanCount, generations}) // -> {...}
export function isBindingCurrent(binding, host)     // 复刻 HostBindings::is_current
```

**判据**：
- `judgeA16` 六条判据（spec §4.1）全部成立才 `pass`，否则 `fail` 且 `checks` 指出哪条不成立。
- `judgeA17` 对已证明部分给 `checks`，整体 `unsupported` 且理由点名未实现部分（spec §4.2 (b)）。
- `--host` 缺失 → `unsupported`，退出码 0（unupported 不是执行错误）。
- `isBindingCurrent` 必须比较完整身份（kind/instanceId/generation），只比 generation 的实现在测试里必须失败。

**测试（全部注入，不连真机）**：
1. A16 全成立 → pass
2. A16 版本不等 → fail，且 checks 指向 version
3. A16 hostId 是 loopback → fail
4. A16 `sideHostInstanceId` 为空 → fail
5. A16 架构不在 SUPPORTED_ARCHES → fail
6. A17 两次身份相同（模拟重连复用）→ 该条 check 失败
7. A17 两次身份不同但 generation 相同 → check 通过（钉死「generation 相同不足以判定复用」）
8. A17 整体 verdict 必为 unsupported，且 reason 提到未实现部分
9. `--host` 缺失 → unsupported + exit 0
10. `--remote-path` 是相对路径 → 退出码 2 并说明原因

## Task 4：让 `e2e.mjs` 传递 `remoteIdentity`

**目标**：修掉 `e2e.mjs:477` 从不传 `remoteIdentity` 的缺陷。

**Files:** Modify `scripts/plugins/e2e.mjs`、`scripts/plugins/e2e.test.mjs`

**判据**：
- 命令声明 `emitsRemoteIdentity: true` 时，解析 stdout 首行 JSON 为 `remoteIdentity`。
- 解析失败 → 该命令 `outcome: "fail"`（不是 missing）。
- 未声明该字段的命令行为完全不变（回归）。

**测试**：
1. 声明 + 合法 JSON → 证据带 remoteIdentity
2. 声明 + 非法 JSON → fail
3. 声明 + JSON 缺字段 → fail（缺 hostId/os/arch/version 任一）
4. 未声明 → 行为不变

## Task 5：接线与文档

**Files:** Modify `scripts/plugins/acceptance-commands.json`（去掉 `needs: e2e-remote.mjs`，改命令带 `--json` 与 `emitsRemoteIdentity`）、`yi-agent-rs/justfile`、`docs/project-management/superpowers-kanban.md`

**判据**：
- `just plugin-acceptance-check` 含 `node --test ../scripts/plugins/e2e-remote.test.mjs` 等新测试。
- 真机跑 A16/A17 并把结果写进报告与进度文档。

## Task 6：真机验证与归档

**判据**：
- `node scripts/plugins/e2e-remote.mjs --case A16 --host agenticlabs@10.103.132.15 --remote-path /Users/agenticlabs/Documents/YichenExperiments/yi-agent-env/test-remote/yi-agent --json` 返回 pass。
- A17 返回 unsupported 且 checks 显示重连不复用旧绑定成立。
- 结果归档到 `docs/superpowers/reports/`。
- `just ci` 全绿。

## 顺序与依赖

Task 1 → 2 → 3（3 依赖 1、2 的接口）；Task 4 独立，可与 1-3 并行；Task 5 依赖 3、4；Task 6 依赖全部。

## 不做的事

- 不实现远端资产拉取（独立 spec）。
- 不把 A17 标 pass。
- 不动远端调试目录之外的路径。
