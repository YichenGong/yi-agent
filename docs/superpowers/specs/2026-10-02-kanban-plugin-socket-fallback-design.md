# Superpowers 看板：插件侧 socket 路径回退

日期：2026-10-02
状态：待实施

## 1. 背景

宿主有一套 socket 路径回退：路径超过 `MAX_SOCKET_PATH_BYTES`（103，Unix domain socket 上限）时，
改放 `$TMPDIR/yi-agent-<sha256(runtime_dir) 前 16 位>.sock`（`yi-agent-store::ipc::socket_path_for`）。

**插件没有这套规则**，两处 socket 都是裸拼接：

| 用途 | 拼法 | 本仓库实测长度 | 结果 |
|---|---|---|---|
| 插件查询服务端 | `<state_dir>/superpowers-kanban.sock` | 130 | `AF_UNIX path too long`，bind 失败 |
| 插件→daemon 客户端 | `<runtime_dir>/runtime.sock` | 108 | 文件不存在（宿主实际在回退位置） |

本仓库路径长 96 字符，属于常态。后果：

1. **看板 UI 全瞎**：查询 socket 没 bind 上，daemon 转发表为空，
   TUI 的 `/superpowers-kanban` 与桌面看板面板一律显示「插件未安装/不可用」。
2. **卡片永远不跑**：插件连不上 daemon，无法在队列推进时建立会话；
   卡片停在 `queued`。

实测证据（2026-10-02，本仓库）：插件进程被监督循环正常拉起（PPID = TUI），
但 `<state_dir>/superpowers-kanban.sock` 不存在；`PluginQuery` 返回
`plugin superpowers-kanban is not available`。

这是 Part 4（`query_socket`）引入时的测试盲区：插件侧测试一律用短临时目录，
深路径只有在真实仓库里才暴露。

## 2. 范围

**做：**

- 插件侧新增一个 socket 路径解析函数，规则与宿主的 `socket_path_for` **逐字一致**
  （同一哈希输入、同一前缀长度、同一落点），并同时用于**两处** socket：
  查询服务端与 daemon 客户端。
- 让**清单声明的 `query_socket` 与插件实际 bind 的路径一致**：
  插件在 bind 前按同一规则解析，并在启动时打印实际路径（便于排查）。
- 插件保持**零 `yi-agent-*` 依赖**：规则是手写复刻，不依赖宿主 crate。

**不做（明确非目标）：**

- 不改宿主侧 `socket_path_for`（它已正确）。
- 不改插件→daemon 的线协议（v1 客户端协议不变）。
- 不改清单里 `query_socket` 的**写法**（仍是 `{state_dir}/superpowers-kanban.sock`
  这样的模板；实际落点由插件按回退规则决定）。
- 不解决宿主的 daemon 自身 socket（已正确回退）。

## 3. 设计

### 3.1 前提（已核实）：宿主展开 `query_socket` 后**没有**长度回退

`SupervisorManifest::query_socket_path` 只做占位符替换，不检查长度。所以
宿主的转发表里会放进一条 130 字节的路径，而插件永远 bind 不上它。
**两边都要改**，且必须用同一条规则。

### 3.2 一条规则，两处实现（宿主 + 插件），哈希同一输入

规则（对**直接路径**求哈希，宿主与插件因此得出同一结果）：

```
resolve(direct):
  if len(direct) <= 103: return direct
  fallback = temp_dir() / ("plugin-" + sha256(direct)[..16] + ".sock")
  if len(fallback) > 103: error("socket path too long even after fallback")
  return fallback
```

- **哈希输入是直接路径**，不是目录：宿主拿到的是展开后的清单值，插件拿到的是
  `dir.join(file_name)`；当清单写的是 `{state_dir}/superpowers-kanban.sock` 时两者**逐字相同**，
  于是哈希相同、结果相同。这是两边能对上的关键，也是本 spec 的核心。
- **前缀 `plugin-` 两边共用**：它是这条通道专用的命名空间。宿主原有的
  `yi-agent-<hash>.sock`（daemon 自己的 socket）保持不变，两者输入不同、不会撞名。

### 3.3 两处调用点

| 侧 | 直接路径 | 改哪里 |
|---|---|---|
| 宿主 | 展开后的清单 `query_socket` | `SupervisorManifest::query_socket_path` |
| 插件（服务端） | `<state_dir>/superpowers-kanban.sock` | `superpowers-kanban-ipc::server::socket_path` |
| 插件（客户端） | `<runtime_dir>/runtime.sock` | `superpowers-kanban-ipc::client::socket_path` |

插件侧还必须与宿主**行为不同**的一点：客户端那条（插件→daemon）**不需要**与宿主一致，
因为宿主的 daemon socket 用**它自己的**规则（`yi-agent-` 前缀、对 runtime_dir 求哈希）——
所以插件客户端必须**复刻宿主那套**（前缀 `yi-agent-`、哈希 `runtime_dir`），
而不是上面那套。两套规则并存，用途不同：

| 规则 | 前缀 | 哈希输入 | 谁用 |
|---|---|---|---|
| 宿主 daemon socket | `yi-agent-` | `runtime_dir` | 宿主 daemon、宿主所有客户端、**插件客户端** |
| 插件通道 socket | `plugin-` | 直接路径 | 宿主转发表、**插件服务端** |

这条区分必须写进注释与测试，否则下次一定有人把它合并成一套。

### 3.4 校验与可观测

- 插件 bind 前打印实际路径（`eprintln!`），深路径问题下次一眼可见。
- 回退后仍超长 → 显式报错并让查询服务线程退出（现状是静默失败，排查成本高）。

## 4. 验收

1. **单测（规则）**：短路径直通；长路径回退；回退确定（同一输入同一结果）；
   前缀与宿主不同名；回退后仍超长则报错。
2. **跨端一致性测**：对同一深路径，宿主的 `query_socket_path` 与插件的
   `server::socket_path` 得出**同一条**路径。
3. **集成（真实插件 + 深路径）**：在长路径项目里起内嵌 daemon，
   断言插件 socket 被 bind 上、`PluginQuery` 能取到 `switch.read`。
4. **回归**：主工作区、插件、桌面全绿；插件仍零 `yi-agent-*` 依赖。
5. **手工冒烟**：本仓库里看板 UI 能看到队列（这就是认领的冒烟卡）。

## 5. 风险

| 风险 | 处置 |
|---|---|
| 插件与宿主回退命名撞车 | 前缀不同（`superpowers-kanban-` vs `yi-agent-`），测试锁住 |
| 两边哈希输入不一致导致路径不同 | 跨端一致性测试（验收 2）锁住 |
| `$TMPDIR` 本身也很长 | 回退后仍超长则显式报错（现状是静默失败，更糟） |
| `$TMPDIR` 跨重启被清理 | 与宿主同款既有语义，不在本 spec 扩大范围 |

## 6. 实施中发现的第二个断点：协议版本停在 1

真实闭环冒烟时暴露：worktree 预建成功、socket 也通了，但 `create_session` 被 daemon
拒绝——`ipc protocol version mismatch: daemon sent 2, plugin speaks 1`。

插件零 `yi-agent-*` 依赖，协议与版本号只能手写复刻。宿主的 `PROTOCOL_VERSION` 在
`ce82aaf`（加通用插件查询通道）时从 1 升到 2，插件**没有跟着升**。后果与 socket
问题同一量级：卡片能入队、能被消费、能建 worktree，但**永远到不了 running**，
而且因为插件 stderr 被 supervisor 丢弃，表现为静默不动。

宿主的信封字段与命令/回复变体在两版之间没有变（升版本只是为了拒绝旧 daemon 时
给出清晰错误而非解析错误），所以修复是插件侧版本号对齐 + 宿主侧一条防漂移测试。

### 验收（追加）

6. 插件 `PROTOCOL_VERSION` == 宿主 `PROTOCOL_VERSION`，由宿主侧测试锁住
   （插件目录缺失时跳过，保持可独立卸载）。
7. 真实闭环：深路径项目里卡片从 `queued` 推进到 `running`。

## 7. 实施中发现的第三个问题：代码签名缓存（非代码缺陷）

用 `cp` 覆盖 `/opt/homebrew/bin/superpowers-kanban` 后，该路径下的二进制一执行就
被 SIGKILL（137）。同一份字节换到别的路径执行正常；原地 `codesign -f -s -` 重签后
恢复正常。这是 macOS 对"已存在的 ad-hoc 签名文件被同路径替换"的签名缓存拒绝，
**不是代码问题**。

影响：安装步骤若用 `cp` 覆盖旧二进制，插件会起不来，且没有任何错误信息
（监督循环看到子进程秒退，按退避反复重启）。**安装后应重签名**。
