# 插件线协议契约（G1 / G2 的可执行部分）

这些 JSON 文件是协议的一部分，不是随手写的测试数据。改任何一个字节前先读
[宿主协议子 spec](../../../docs/superpowers/specs/2026-11-02-plugin-host-protocol.md)。

## 目录内容

| 文件 | 性质 | 谁冻结的 |
|---|---|---|
| `manifest-v1.schema.json` | 发布清单结构 | G1 |
| `host-v1.schema.json` | 宿主描述结构 | G1 |
| `bridge-v1.schema.json` | 能力桥方法集与结果联合 | G1 |
| `root-v1.schema.json` | 信任根信封结构 | G2 |
| `release-v1.schema.json` | 已签名发布元数据结构 | G2 |
| `lifecycle-v1.schema.json` | 生命周期事务日志结构 | G2 |
| `fixtures/*.json` | 真实实例样本 | 见下 |
| `package.json` / `package-lock.json` | 校验器依赖（ajv 8.17.1 / ajv-formats 3.0.1，精确版本） | — |

## 黄金样本

`fixtures/` 下每个文件都被 `scripts/plugins/check-contracts.mjs` 当作正样本
校验，被 `check-contracts.test.mjs` 当作篡改基准。

| fixture | 说明 |
|---|---|
| `manifest-v1.json` | example-counter 的完整发布清单 |
| `host-v1.json` | 宿主能力的声明上限 |
| `bridge-v1.json` | 管理 RPC 的闭合方法集 |
| `lifecycle-v1.json` | 一个处于 `draining` 阶段的激活事务 |
| `root-v1.json` | **含真实 Ed25519 签名的**信任根信封 |
| `release-v1.json` | **含真实 Ed25519 签名的**发布元数据 |
| `driver-protocol.json` | 宿主 `session_driver` 与插件 `driver/next` 共用的线协议 |
| `plugin-socket.json` | 宿主转发表与插件服务端共用的 socket 回退契约 |

### 跨端共享夹具（`driver-protocol.json` / `plugin-socket.json`）

这两个文件原本住在看板插件的 crate 目录里，宿主测试反向引用插件源码路径。
那让「编译主程序」与「插件源码是否在检出的树里」绑在一起，也让主程序的测试
文件里出现插件业务字样——独立发布后两者都不可接受。

现在它们是**公共夹具**，宿主与插件各自读同一份：

- 宿主：`session_driver.rs` 用 `include_str!`（编译期钉住文件存在）；
  `supervisors/src/manifest.rs` 运行时读取，目录缺失则跳过该条断言。
- 插件：`superpowers-kanban-ipc/src/socket.rs` 与
  `superpowers-kanban-runner/src/service.rs` 读同一份。

**它们只冻结形状，不冻结业务取值。** 里面的人名/标题/环境变量名都是通用
示例（`item · a.spec`、`PLUGIN_ITEM`），不含任何单个插件的业务语义；插件侧
的断言因此比较**键集合与值类型**，而不是逐字相等——字段名漂移会红，取值
不同不会。

### 关于 `root-v1.json` / `release-v1.json` 的签名

这两个文件里的 `signature` 是**由代码现算并验证过**的字节，不是占位符。

**它们是公开的测试身份。** 密钥对由 `scripts/plugins/sign-contracts.mjs` 里
的固定公开种子（`TEST_SEED = 7`）派生，任何人都能复现私钥。因此：

- **绝不可**用于生产发布。
- 正式构建必须检查「产物里没有嵌入测试根」，这条在 T07 实现时落地。
- 换掉种子会同时让 fixture 与测试失去意义，必须一起改。

`release-v1.json` 的 `payloadB64` 承载**被签名的确切字节**，而不是让验证方
自己从字段重建。这样验证方检查的是签名者签的东西；如果 `payloadB64` 与
`release` 字段不一致，那本身就是完整性失败。

## 校验器

```bash
npm ci --prefix plugins/contracts
node scripts/plugins/check-contracts.mjs        # 正/负样本，打印计数
node --test scripts/plugins/check-contracts.test.mjs
```

期望：`positive: 14, negative: 26, failures: 0` 与 56/56 通过（2026-11-09 实测）。

`driver-protocol.json` 与 `plugin-socket.json` 不是 schema 黄金样本，不参与
上面的正/负样本计数；它们由宿主与插件两侧的 Rust 测试各自断言。

## daemon socket 契约（跨进程，必须真跑）

`daemon-socket.json` 冻结宿主业务 daemon 的地址算法：

```
<canonical instance_home>/sockets/<canonical runtime 的 SHA-256 前 16 hex>/runtime.sock
```

实例家目录的优先级是 `YI_AGENT_INSTANCE_HOME` → `YI_AGENT_HOME`（末级不是
`.yi-agent` 时追加）→ `$HOME/.yi-agent`。**没有 TMPDIR 回退**：回退会让插件与
宿主再次落到不同文件，这正是要消灭的故障。

为什么不能只靠"两侧各自单测"：插件不得依赖任何 `yi-agent-*` crate（它要能独立
安装），所以地址解析是宿主 `lifecycle::scope` 的**独立复刻**。两份各自自洽的实现
完全可能互相不兼容。因此门禁必须让两侧的**真实生产函数**在同一进程对照里比较：

```bash
bash scripts/plugins/check-daemon-socket-contract.sh
```

它按顺序执行 8 个 cargo 阶段：宿主 metadata、构建宿主 example、宿主与插件的
fixture 单测、`--ignored` 的跨进程逐 case 对照、IPC 整包、runner 的 daemon
liveness 进程测试、runner lib。单测试阶段要求"恰好 1 个 test 且真 ok"——0 test
与 ignored 跳过都不算通过。

一个容易踩的坑：run_one 的 `flags` 数组不能改成空数组再 `"${flags[@]}"` 展开。
macOS 自带 Bash 3.2，在 `set -u` 下对空数组展开会报 unbound variable，且某些
路径上还会错误地 exit 0。

接线由静态守卫锁定（recipe 依赖、macOS job、GitLab 的 python3）：

```bash
python3 scripts/plugins/check-daemon-socket-wiring.py
python3 -m unittest discover -s scripts/plugins -p 'test_check_daemon_socket_contract.py'
```

### schema 负责什么、不负责什么

- **负责**：报文结构、必填字段、类型、取值域、包内相对路径、安全上限。
- **不负责**：跨请求行为。重复 request ID、binding 一致性、候选版本不可用、
  生命周期阶段顺序，都由 `check-contracts.mjs` 里的显式校验器实现，并在
  子 spec 里写明理由——不能声称单个 JSON schema 能证明这些。

### 为什么 fixture 里没有 `_comment`

三个信封 schema 都是 `additionalProperties: false`，而未知字段必须被拒绝
（那是 G1 的核心约束之一：宿主看不懂的必填字段意味着插件比宿主新）。所以
说明文字只能放在这里，不能塞进 fixture。
