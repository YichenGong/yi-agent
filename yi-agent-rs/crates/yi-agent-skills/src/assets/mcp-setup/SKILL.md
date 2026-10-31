---
name: mcp-setup
description: Add, disable, remove, or troubleshoot MCP servers for yi-agent. Use when the user says things like "帮我加一个 xxx MCP"、"把这个 MCP 加上"、"关掉/删掉某个 MCP"、"我的 MCP 连不上"、"mcp.json 怎么写", or when an MCP server's tools are missing or a probe fails.
---

# MCP Setup

用 `mcp_reload` 工具改配置。**不要**让用户手动编辑 JSON，也**不要**自己 `write`
到 `.yi-agent/mcp.json` —— 工具会做校验、写盘、探测，并且知道什么时候生效。

## 什么时候用这个 skill

- 「帮我加一个 github MCP」
- 「把 fs 那个 MCP 关掉 / 删掉」
- 「我加了 MCP 但工具没出现」
- 「这个 MCP 报 401 / 超时 / 找不到命令」
- 「mcp.json 该怎么写」

## 用工具，不要手写配置

`mcp_reload(action, name, ...)`：

| action | 作用 | 需要额外参数 |
| --- | --- | --- |
| `add` | 新增一个 server（重名要带 `overwrite: true`） | `kind` + `command`/`url` 等 |
| `enable` | 启用已存在的 server | 无 |
| `disable` | 停用，但保留配置 | 无 |
| `remove` | 从配置里删除 | 无 |

四个 action 都会请用户确认一次。这是刻意的：它改的是持久配置。

## 两种传输方式

**stdio**（本地起子进程，最常见）：

```json
{"action": "add", "name": "github", "kind": "stdio",
 "command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"],
 "env": {"GITHUB_TOKEN": "${GITHUB_TOKEN}"}}
```

**http**（连远端服务）：

```json
{"action": "add", "name": "horizon-websearch", "kind": "http",
 "url": "https://example.com/mcp",
 "headers": {"Authorization": "Bearer ${MY_TOKEN}"}}
```

**没有 sse**。只有 `stdio` 和 `http` 两种；用户如果说是 SSE，多半已经过时，
按 http 处理。

## 命名规则

server 名只能是**小写字母、数字、连字符**，1–64 字节，不能以连字符开头/结尾，
不能有连续连字符。

- 好：`github`、`my-fs`、`horizon-websearch`
- 坏：`GitHub`、`my_fs`、`--x`

工具会拒绝坏名字并给出原因。**不要**自己猜测一个"应该能过"的名字就重试，
按报错改。

## `${VAR}` 引用必须在环境里已定义

`env` / `headers` 里的 `${VAR}` 会在写盘前校验。未定义直接报错——这是好事：
运行时会静默丢弃未展开的 header，用户最后面对一个 401 却查不到原因。

需要 token 时，先确认环境变量存在（`bash` 里 `printenv VAR`），再写进去。

## 生效时机（必须告诉用户）

- 配置**当场**写盘。
- 新工具**从下一条消息起**可用。
- **本轮不要尝试调用刚加的工具**——它们还不在本轮的工具集里。

`disable` / `remove` 更快：不联网、不探测，改完立即返回。

## 探测失败怎么读

`add` / `enable` 会同步探测，最坏等几十秒。失败时结果里会带**原始错误**，别
把它糊成"失败了"，按下面几类对症：

| 错误里出现 | 含义 | 怎么办 |
| --- | --- | --- |
| `timeout` / `timed out` | 首次 `npx` 拉包太慢，或网络不通 | 再试一次（第二次通常命中缓存）；仍失败就查网络 |
| `401` / `403` / `Unauthorized` | 凭据无效或没传 | 检查 `${VAR}` 是否真的在环境里、token 是否过期 |
| `not found` / `No such file` | 命令不存在 | 确认 `command` 拼写；`npx`/`uvx`/`node` 是否已安装 |
| `connection refused` / DNS | http endpoint 不可达 | 核对 `url`；内网服务先确认可达 |

**配置已经写盘了**，即使探测失败。所以修参数后可以直接再 `add`（带
`overwrite: true`）或 `enable`，不用从头来。

## 排查「工具没出现」

1. 全局开关是否被关了？——**帮助用户看，但不要替他们关全局开关**：那会一次
   停掉所有 MCP。
2. 该 server 自己的 `enabled` 是否为 false。
3. 工具名形如 `mcp__{server}__{tool}`。找不到就先确认 server 名。
4. `remove` 之后工具会立刻消失；`add` 之后要等**下一条消息**。

## 当前范围

- 配置是**项目级**的：`<项目>/.yi-agent/mcp.json`。只对这个项目生效，
  不要承诺"所有项目都能用"。
- 用户级（`~/.yi-agent/mcp.json`）目前**不参与**加载。用户想要"每个项目都能用"
  时，如实说明这一点。
- 桌面设置页的开关走的是同一个配置文件；不必让用户同时用两个入口。
