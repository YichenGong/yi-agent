# 插件宿主协议 v1（G1 门禁材料）

日期：2026-11-03

状态：**等待 G1 人工审阅**。本文是 G1 的输入，不是 G1 的结论。

对应设计：[设计](2026-11-02-kanban-independent-release-design.md)；对应计划：[总计划](../plans/2026-11-02-kanban-independent-release.md)（T03）；执行子计划：[宿主运行时](../plans/2026-11-02-plugin-host-runtime.md)。

可执行部分（本文不是纯文档）：

| 文件 | 作用 |
|---|---|
| `plugins/contracts/manifest-v1.schema.json` | 发布清单报文结构 |
| `plugins/contracts/host-v1.schema.json` | 宿主描述报文结构 |
| `plugins/contracts/bridge-v1.schema.json` | 能力桥方法集与结果联合 |
| `plugins/contracts/fixtures/*.json` | 黄金样本（公开测试身份，非生产信任根） |
| `scripts/plugins/check-contracts.mjs` | 结构校验 + 跨请求行为校验 |
| `scripts/plugins/check-contracts.test.mjs` | 上述行为的 21 条断言（G2 起增至 42 条，见分发子 spec） |

运行：

```bash
npm ci --prefix plugins/contracts
node scripts/plugins/check-contracts.mjs
node --test scripts/plugins/check-contracts.test.mjs
```

预期（G1 交付时）：`positive: 4, negative: 17, failures: 0` 与 21/21 通过。G2 之后样本集扩大为 `positive: 9, negative: 24` 与 42/42——G1 的 21 条断言没有减少或放宽。

## 1. 本文冻结什么、不冻结什么

**冻结**（改这些必须先改本文、schema、checker 与测试，再改实现）：

- 版本握手规则与不兼容判定；
- 项目句柄的形式与不透明性；
- 两类代次（激活代次 / 项目授权代次）各自的作用域与失效条件；
- 消息顺序、修订号、重连语义；
- 签名原始 payload 的**编码规则**（字节如何拼、如何 base64、哪些字段进 payload）；
- 错误分类与恢复动作；
- 权限矩阵（哪个 effect 允许什么，谁可以签发）；
- 默认安全上限。

**不冻结**（T04 才冻结）：Ed25519 具体库版本、根轮换流程、发布序号与有效期的具体校验表、事务各阶段的崩溃恢复表、锁顺序。本文只声明根信封与发布元数据的**结构**（`fixtures/root-v1.json`、`fixtures/release-v1.json` 是占位，密码学有效字节在 T04 固定），以及签名 payload 的**编码算法**——T04 只需要往里填具体字节。

这条边界是刻意的：如果 G1 就把根轮换和事务恢复表定死，G1 会变成一个无法一次审完的巨大门禁；反过来，如果 G1 不定 payload 编码，T04 与 T06 会各自发明一套，签名就对不上。

## 2. 术语与不变量

| 术语 | 定义 |
|---|---|
| **执行宿主**（host） | 持有注册表、授权、事务日志的 yi-agent 实例，根在 `paths::instance_home()` |
| **宿主身份**（`host_id`） | 执行宿主实例的稳定标识；同一实例跨重启不变，不同实例不同 |
| **发布身份**（`ReleaseId`） | `(plugin, version, target, sha256)` 四元组；任一项变化即是另一个发布 |
| **激活代次**（`activation`） | 实例内某插件每次激活/切换产生的不可复用 `u64`；**重新激活同一版本也递增** |
| **项目授权代次**（`grant`） | 某项目每次启用/停用/变更授权时递增的 `u64`；只失效该项目，不影响其他项目 |
| **运行绑定**（`Binding`） | `(host_id, release, activation, project, grant)`；一切授权判定的最小单位 |

四条不变量，违反任一条即视为协议错误而非实现 bug：

1. **代次只增不减，且永不复用。** 回滚到旧版本也拿新 `activation`，否则旧 binding 会"复活"。
2. **binding 不可跨宿主复用。** 桌面连到另一个执行宿主后，旧 binding 必须 `StaleBinding`，不得悄悄转发。
3. **binding 不可跨项目复用。** 同一插件在两个项目上的授权互不影响；撤销 A 不能让 B 失效，也不能让 A 的记录漂到 B。
4. **`Maintenance` 不是插件可申请的能力。** 它只由生命周期事务签发，`manifest-v1.schema.json` 的 `effect` enum 里刻意没有它。

## 3. 版本握手

### 3.1 三个独立版本

设计 §8 要求"前两者稳定版本化，后者由完整发布包协调"。因此有三个**互不替代**的版本轴：

| 轴 | 出现在 | 比较方式 | 不匹配时 |
|---|---|---|---|
| 宿主 API 版本 | `host-v1.json` 的 `api`；`manifest-v1.json` 的 `compatibility.hostApi` | 主版本相等（`v1` vs `v1`） | `Incompatible`，拒绝启用 |
| 契约版本 | 三个 schema 的 `schemaVersion`（均 `const 1`） | 精确相等 | `InvalidManifest`，拒绝解析 |
| 插件业务协议版本 | 插件包内部，宿主不解释 | 由插件自己协调 | 与宿主无关 |

**不允许"向后兼容地忽略未知字段"。** 三个 schema 全层 `additionalProperties: false`。理由是设计 §3.3 把发布清单定为"权威来源"：宿主看不懂的必填字段意味着插件比宿主新，此时猜一个默认值是把它当成了可选，而签名覆盖的是它真的必填。

### 3.2 界面 API 与执行 API 分开握手

同一插件可能连到多个宿主（设计 §10）。握手报文里两个版本号分别上报：

```json
{
  "kind": "handshake",
  "contract": 1,
  "hostApi": "v1",
  "clientUiApi": "v1",
  "binding": { "...": "..." },
  "capabilities": ["events", "threads:open", "notify", "confirm"]
}
```

- **未知必需能力** → 明确拒绝（`NotSupported`），不降级。`capabilities` 数组里未知的项一律视为**必需**；要表达"可选"，用 `optionalCapabilities`。
- 桌面不兼容但执行宿主兼容时：该**视图**不可加载并提示升级，远端已有合法任务继续运行（设计 §10）。不得回退到另一个 UI 版本的缓存。
- 无桌面的 CLI 安装/更新只检查执行宿主。

## 4. 项目句柄

设计 §8：「跨项目看板选择使用宿主发放的项目句柄，不接受任意文件路径。」

### 4.1 形式

```text
ph1_<32 bytes base64url, no padding>
```

- 前缀 `ph1_` 标明版本，便于将来换编码而不误解析。
- 内容由宿主从 `(host_id, project_id, grant)` 用宿主私钥派生的不可伪造映射生成；**不是**项目路径的编码，也不是项目 ID 的明文。
- 页面拿到句柄只能用来指定"我要对哪个已授权项目操作"，不能反推出路径——否则句柄就退化成了文件路径，`read_file(path)` 类后门会重新出现。

### 4.2 校验

宿主在每次业务转发前：

1. 句柄解析失败 → `PermissionDenied`（不区分"格式错"与"不存在"，避免成为探测口）。
2. 句柄存在但 `grant` 已过期 → `StaleBinding`。
3. 句柄属于另一个 `host_id` → `StaleBinding`。
4. 句柄对应项目未在该 binding 的授权范围内 → `PermissionDenied`。

**项目句柄不是凭据。** 它只用于选择，不用于认证；认证来自 binding 与 permits（§6）。

## 5. 权限矩阵

### 5.1 effect 语义

| effect | 含义 | 谁能签发 | 可否由插件清单声明 |
|---|---|---|---|
| `Read` | 只读业务查询 | `Registry::grant` | 是 |
| `Write` | 改业务状态 | `Registry::grant` | 是 |
| `Launch` | 启动会话/自动任务/后台 driver | `Registry::grant` | 是 |
| `QuotaWrite` | 修改整机共享配额配置 | 用户显式授权，单独一项 | 是（但授权不与 Write 合并） |
| `Resident` | 登记常驻服务/后台常驻引用 | 用户显式授权，单独一项 | 是 |
| `Maintenance` | 生命周期事务内的迁移/暂停/健康检查 | **仅生命周期事务** | **否** |

设计 §4 的"把配额修改、自动任务、常驻分别授权"对应上表后四行：它们不因为用户批了 `Write` 就自动获得。计划 T10 Step 3 的"用户未批的新权限不得因升级获得"由此落地——升级只带来新的声明，授权由用户在变更时重新确认。

### 5.2 声明与授权的区别

- `manifest.methods` = 插件**拥有**的方法，每个带自己的 effect。
- `manifest.capabilities.required` / `.optional` = 插件**申请**的能力，分必需与可选。缺必需能力阻塞该项目；缺可选能力只降级功能。
  - `Read` / `Write` 必须有方法兑现（checker 强制），否则授权清单是空的。
  - `Launch` / `QuotaWrite` / `Resident` **不**要求 backing method：它们是宿主签发的能力，设计 §6 把「发起自动任务」「修改整机配额」「后台常驻」列为独立确认项。
  - 同一 effect 不得同时出现在 required 与 optional（checker 强制）。
- `Binding.grant` = 宿主**实际授予**的代次。授权记录在 `Registry`，不来自清单。

`Registry::grant` 是宿主内部原语：插件不能调用，它也不单独代表"启用完成"。首次启用必须先在维护事务中建立初次激活代次，再启动/健康检查并提交授权；失败不留下可用 binding。

### 5.2b 清单必须声明的其余字段

设计 §5 要求清单声明的不止能力，本 schema 逐项对应：

| 字段 | 设计出处 | 冻结的规则 |
|---|---|---|
| `packageFormat` | §5 包格式版本 | `const 1`；与 `schemaVersion` 分开，归档格式可独立演进 |
| `target` | §5 目标 OS/架构 | 单个值，不是数组。同一版本两个平台是**两个发布**，各有签名与序号（见 §8.3） |
| `views[]` | §5 主视图与设置入口 | `kind: main \| settings`；**恰好一个 `main`**；`id` 全局唯一；`entry` 是包内相对路径 |
| `program` | §5 程序入口 | 可为 `null`（纯视图插件）；只有这里声明的入口由宿主设置执行位，其余静态文件只读 |
| `skills[]` | §5 skill 入口 | 名字唯一；`path` 是包内目录；**不得**复制到用户全局目录 |
| `dependencies.workflows[]` | §5 必需工作流依赖 | `range` 可选；普通文件依赖用能力/命令契约表达，**不得**编造 semver |
| `data.format` / `data.migrations[]` | §5 数据读写版本与迁移描述 | 迁移必须**逐版本相邻**（`to == from + 1`），跳过中间格式会被拒 |
| `compatibility.hostApi` / `.uiApi` | §5 两端分别检查 | 两个独立的主版本；远端兼容不蕴含本地兼容 |

`views` 里「恰好一个 `main`」是 checker 的显式规则而不是 schema 能表达的约束（JSON Schema 在 draft 2020-12 里表达"数组恰好一个元素满足条件"需要 `contains` + `minContains/maxContains` 组合，表达力够但错误信息很差）。实现选显式规则，理由与 §5.4 相同：**错误信息必须能指向具体问题**。

### 5.3 只读授权的边界（必须写进 UI 文案）

`Read` 只限制**通过宿主桥**的调用。它**不阻止**当前 OS 用户权限下的原生文件访问。UI 必须保留原生信任提示，不能把"只读授权"说成沙箱。

### 5.4 声明为 Read 却命名破坏性动词 → 拒绝

计划 T03 的测试要求：`{name:"cards.deleteEverything", effect:"Read"}` 必须被拒绝。这条**不是** schema 能表达的（名字符合 `namespace/verb` 正则，effect 也合法），所以它是 checker 里的显式语义规则：

```
若 effect == "Read" 且 name/verb（小写）包含任一破坏性动词
则拒绝，错误信息指明命中的动词
```

当前动词表（`scripts/plugins/check-contracts.mjs` 的 `DESTRUCTIVE_VERBS`）：

```text
delete remove write set update create merge install uninstall drain kill
restart reset clear purge drop truncate replace move rename enable disable
activate commit rollback
```

三条约束防止这条规则变成摆设：

1. **它必须能独立触发。** `check-contracts.test.mjs` 用派生样本断言错误信息里出现 `effect|method|unknown`，而 `node -e` 手工验证时错误列表里**只有**这条语义错误——说明 schema 放行了该样本，规则确实在起作用，不是被 schema 顺手拦下。
2. **表是白名单式的保守集合，不是完备语义分析。** 它只拦"名字自己承认要写"的情况；真正的判据仍是 `Registry::check` 用签名清单里的 effect。这条规则的价值是拦住明显矛盾的声明，不是替代授权。
3. **改表 = 改协议。** 新增动词必须同步本文与测试，不能只在代码里加字符串。

## 6. binding 与 permit

### 6.1 谁持有锁

计划 §3：「`TurnPermit` / `OperationPermit` 持有真实存活锁；进程退出释放，不能仅按 TTL 或 PID 判断已无操作。」

- 锁是**跨进程文件锁**（OS 级），不是心跳。
- 进程崩溃 → OS 释放 → 对端可观察到 `released`。
- 进程被 `SIGSTOP` **不释放**锁，因此**不能**用"超过 N 秒无心跳"回收 permit。这是反例：暂停的进程恢复后仍会继续写。
- 判定"已无操作"只能看锁的实际状态，不能看 TTL、PID 存活、mtime。

### 6.2 `parent_turn` 不是授权

计划 T11 Step 3：「`parent_turn` 字符串不是收尾授权」。外部写要收尾，必须同时满足：

1. 该 turn 在宿主侧登记过会话/调用者绑定；
2. 该 turn 仍存活的**屏障前** permit 确实存在；
3. 请求的 binding 与该 turn 的 binding 在 `project` 与 `grant` 上一致。

仅填一个长得像别人 turn ID 的字符串 → `PermissionDenied`。

### 6.3 结果查询与业务准入分离

- 已认证的管理调用可在**原 binding 作用域**查询已提交结果（`plugins/v1/operationResult`）。
- 旧代次**不能**据此再提交新操作。
- 切换项目后**不能**取另一个项目的结果。

## 7. 消息顺序与事件

### 7.1 序列号

- 每个 `(binding, topic)` 有单调递增的 `revision: u64`。
- `PluginEvent` 必带 `topic`、`revision`、`binding`、`payload`。
- 订阅形如 `subscribe(topic, after, onEvent)`：`after` 表示"我已看到 revision ≤ after，请从 after+1 开始"。

### 7.2 重连：先快照，再续订

设计 §8：「事件带修订/序号，重连先取一致性快照再续订，不能仅靠丢失事件推导真实状态。」

```text
断线
  → 重连握手（§3.2）
  → 取快照 snapshot(binding, topic) → { revision, state }
  → subscribe(topic, snapshot.revision, onEvent)
```

**禁止**用"我丢了哪些事件"来补状态：丢的事件内容未知，推导出的状态是猜的。快照是权威。

### 7.3 旧通道的失效时机

以下任一发生，旧通道立即失效并关闭订阅：卸载 frame、项目切换、宿主切换、授权失效（`grant` 递增）、激活代次切换、配置的陈旧阈值到达。

**已提交的操作仍在原 binding 作用域完成**，绝不落到新项目（设计 §8 末段）。这条与"关闭通道"不矛盾：通道关闭影响的是后续调用，不影响已提交写的去向。

## 8. 签名原始 payload 编码

设计 §5：「签名覆盖明确的原始 payload 字节，绑定 ID、版本、平台、包摘要、大小、兼容范围及发布序号」。

### 8.1 编码算法（本文冻结）

签名对象**不是** JSON 文本，因为 JSON 的键序、空格、数字表示都允许合法变体，同一语义会产生不同字节。改为**逐字段规范化拼接**：

```text
payload := domain_separator || 0x00 ||
           field(name,  value) * N
field(name, value) := ascii(name) || 0x00 || u32be(byte_len(value)) || value
```

- `domain_separator` 为 ASCII 字符串，区分根信封与发布元数据：
  - 根信封：`yi-agent-plugin-root-v1`
  - 发布元数据：`yi-agent-plugin-release-v1`
- 字段顺序**固定**，不按字母序，按本文 8.2 / 8.3 列出的顺序；顺序本身是协议的一部分。
- `byte_len` 是**字节**长度，不是字符数（非 ASCII 路径/ID 必须按 UTF-8 计）。
- 整数字段用**十进制 ASCII** 编码后当 `value` 处理（避免端序争议），`u32be` 只用于长度前缀。
- 长度前缀上限 `2^32-1`；实现必须在分配前检查，不能先读长度再 `Vec::with_capacity`。

签名值本身 base64（标准字母表，**带** padding），放在信封的 `signature` 字段，**不进** payload。

### 8.2 根信封 payload 字段顺序

```text
keyId, publicKey, serial, notBefore, notAfter
```

`publicKey` 是 32 字节 Ed25519 公钥的 **base64 原文**（不 hex、不 URL-safe），与 `root-v1.json` 里出现的形式一致。

### 8.3 发布元数据 payload 字段顺序

```text
plugin, version, target, sha256, sizeBytes, hostApi, serial, publishedAt, expiresAt
```

`sha256` 是小写 hex（64 字符）。`sizeBytes` 是包字节数，十进制 ASCII。`hostApi` 是 `compatibility.hostApi` 的原文。

**注意**：`target` 是发布身份的一部分，因此同一版本的不同平台包是**不同发布**，各有自己的签名与序号。

### 8.4 为什么冻结编码而不冻结库

- 库（`ed25519-dalek` 版本等）属于依赖选择，T04 才在 `cargo audit` 通过后锁定；提前钉死会把 G1 变成依赖评审。
- 但编码必须现在定死：T04 写签名、T06 写验证，两侧对"签什么字节"必须没有解释空间。若留到实现时定，最可能的结果是签名侧签 JSON、验证侧签规范化字节，然后靠"两边都自测通过"掩盖不一致。

**`fixtures/root-v1.json` / `release-v1.json` 目前是结构占位**，`publicKey` / `signature` 里是 `PLACEHOLDER-...` 字符串。它们不可被当作有效签名验证，T04 才替换为密码学有效的字节。这一点写进 fixture 的 `_comment`，避免有人误以为已经在验签。

## 9. 错误与恢复

### 9.1 稳定 kind

线协议只暴露这些 `kind`，不把 Rust `Debug` 字符串当协议（计划 §3）：

```text
InvalidManifest Integrity Untrusted Expired Revoked Incompatible NotSupported
PermissionDenied StaleBinding Dependency Conflict Draining Busy ResultUnknown
RecoveryRequired Io
```

### 9.2 错误 → 恢复动作

| kind | 页面应做什么 | 宿主应做什么 |
|---|---|---|
| `InvalidManifest` | 显示诊断，不重试 | 拒绝安装/启用；记录插件 ID 与字段路径 |
| `Integrity` | 显示"包已损坏"，提示重新下载 | 删除暂存，不激活 |
| `Untrusted` / `Expired` / `Revoked` | 显示原因 + 安全日志入口 | 不安装/不激活新版本；**已运行的版本继续运行**（网络故障不停止已有版本） |
| `Incompatible` | 显示所需版本与当前版本 | 拒绝接入，不降级 |
| `NotSupported` | 显示"本版本尚未实现" | 未实现的管理方法返回它，**不伪装成功** |
| `PermissionDenied` | 不重试；提示需要授权 | 不改授权，不产生副作用 |
| `StaleBinding` | 提示"需要新会话/重新启用" | 拒绝转发；**不悄悄转到新程序** |
| `Dependency` | 显示缺失的 skill/工作流能力与路径 | 允许安装与查看数据，**不启动自动任务** |
| `Conflict` | 显示冲突对象（重复 request ID 改参数、同名 skill 等） | 拒绝后者，不覆盖 |
| `Draining` | 显示"正在维护，请稍候" | 拒绝新 Launch/外部 Write；**允许旧 turn 收尾** |
| `Busy` | 可短暂重试 | 拒绝并发第二写事务 |
| `ResultUnknown` | 提示"结果未知"并刷新核对，**不重复提交** | 标记 Unknown，重启/重连不重放 |
| `RecoveryRequired` | 显示恢复失败 + 人工恢复说明 | 关闭自动执行，保留数据与备份 |
| `Io` | 显示可定位错误与日志入口 | 不吞掉；不把失败显示成空数据 |

### 9.3 写操作的重放禁止

设计 §8：「读操作允许显式重试；写操作不得因超时、断线或面板重载自动重放。」

协议层保证：

- 幂等靠 `(requestId, method, params)` 三元组。同 ID 同参数重试 → 返回原结果，不重复执行。同 ID 改参数 → `Conflict`。
- 已送出但拿不到结果 → `ResultUnknown`。客户端**不得**自行认定为失败并重试。
- 取消只表示停止等待或在提交前取消，**不承诺撤销已提交的写**。

`check-contracts.mjs` 的 `checkTrace` 是这条规则的第一个执行点：它在 trace 层面断言"同 ID 必须同 method 同 params"，且参数比较与键序无关（`{a:1,b:2}` 与 `{b:2,a:1}` 视为同一请求）。

## 10. 默认安全上限

| 项 | 默认 | 可否由宿主降低 | 可否由插件提升 |
|---|---|---|---|
| 包大小 | 256 MiB（268 435 456 B） | 可 | **不可** |
| 解压总量 | 1 GiB（1 073 741 824 B） | 可 | **不可** |
| 单文件 | 128 MiB（134 217 728 B） | 可 | **不可** |
| 文件数 | 20 000 | 可 | **不可** |

`schema` 把上限写成 `maximum`，因此"插件提升"在结构层就不可能通过。宿主降低不需要改 schema（值更小仍满足 `maximum`）。

解包实现（T08）必须**边解边计**，不能先解压再看总量——否则上限只是事后统计。

## 11. 管理 RPC 清单（冻结）

全部方法带 `plugins/v1/` 前缀，`bridge-v1.schema.json` 里是**闭合 enum**：

| 方法 | effect | 说明 |
|---|---|---|
| `catalog` | Read | 官方发布目录 |
| `list` | Read | 已安装/已授权状态 |
| `install` | Write | 下载/本地导入并安装（与启用分开） |
| `enableProject` | Write | 首次启用某项目 |
| `changeGrant` | Write | 变更某个项目的授权 |
| `disableProject` | Write | 停用（≠ 卸载） |
| `activate` | Write | 激活某已安装发布 |
| `rollback` | Write | 回滚到上一发布 |
| `transaction` | Read | 查询事务进度 |
| `cancelTransaction` | Write | 取消事务（提交前） |
| `uninstall` | Write | 卸载（默认保留数据） |
| `clearData` | Write | 显式清数据（独立于卸载，二次确认） |
| `gc` | Write | 只清引用安全的受管产物 |
| `inspectLegacy` | Read | 探测旧安装 |
| `adoptLegacy` | Write | 接管旧安装 |
| `assetRelease` | Read | 远端资源身份与受控传输句柄 |
| `operationResult` | Read | 按 requestId 查已提交结果 |

**闭合的理由**：开放方法集会允许"新方法在没有 schema、没有 effect、没有审阅的情况下出现"，那正是设计里禁止的 `plugin/install` 成为授权旁路。因此：

- `plugins/v1/rpc` 被拒（任意 RPC 后门）；
- `plugins/v1/evalArbitrary` 被拒；
- 业务桥只能请求自己的已授权后端，不经过这张表。

**未实现的方法返回 `NotSupported`，不伪装成功**（设计 §3.3 末段）。

## 12. 受管 CLI

### 12.1 exec

```bash
yi-agent plugins exec --plugin example-counter --project-handle ph1_... \
  --binding-file session-plugin-binding.json -- increment --by 1
```

- `--` 之后**原样作为 argv 数组**传给插件程序，不拼接 shell 字符串（设计 §7.3）。
- `--binding-file` 由宿主生成并绑定当前会话/宿主；**不接受页面上传的文件作为授权**。
- 宿主用受管解析入口（registry → 激活记录 → 受管根）定位程序，**不依赖用户 PATH**。
- 插件环境里的宿主保留变量（`YI_AGENT_HOME` 等，见 `host-v1.json` 的 `reservedEnv`）由宿主设置，插件 manifest/运行描述**不得覆盖**。

### 12.2 headless 管理

```text
yi-agent plugins catalog|list|install|enable-project|change-grant|disable-project|
activate|rollback|transaction|cancel-transaction|uninstall|clear-data|gc|
inspect-legacy|adopt-legacy
```

- `install` 二选一：官方目录 release 身份，或 `--bundle` + `--metadata` 本地官方签名输入；**同一验证流程**。
- 命令选择的实例遵循 `YI_AGENT_HOME`；项目显式选择。
- 管理命令**不得携带页面业务权限**。
- 不带桌面也能完成安装与更新（T16/T24 实现）。

## 13. 与总计划 §3 草案的对应关系

本文**继承**草案名字，不引入第二套：

| 草案位置 | 本文落点 |
|---|---|
| `PluginId` / `ProjectId` / `ReleaseId` / `Binding` / `Effect` / `Operation` | §2 表格 + §5.1；字段顺序与类型见 §8.3 与 §2 |
| `PluginError` 的 16 个 kind | §9.1（逐字一致） |
| `Manifest::parse` / `TrustStore::bootstrap` / `verify_metadata` / `install_verified` / `Registry::*` / `Admission::*` / `Lifecycle::*` | §6（permit 语义）、§5.2（grant）、本文 §13 不改名；实现签名见执行子计划 |
| `PluginHost` TS 桥 7 个方法 | 执行子计划 T18；`context/request/result/subscribe/openThread/notify/confirm` 不改名 |
| `OperationResult<T>` = `pending/committed/unknown/rejected` | `bridge-v1.schema.json` 的 `responseVariants`，是 `const`，少一个就红 |
| `plugins/v1/*` 17 个管理方法 | §11（与草案逐字一致） |
| `yi-agent plugins exec` argv | §12.1 |
| headless 管理子命令 | §12.2 |

**审阅者若改名**，必须同时更新：本文、三个 schema、checker、测试、总计划 §3、执行子计划、下游 T04/T05 子 spec。计划 §2 已明确"不能留两套名字"。

## 14. 需要审阅者决定的事

1. **`namespace/verb` 是否够用。** 当前要求方法名至少含一个 `/` 且两段都是 camelCase。看板现有 CLI 是 `state --self`、`merge-request` 这种词形；迁移到 method 名时需要一个映射表（T05 做）。如果审阅者认为应该允许 `namespace:verb` 或单段名，现在改成本最低。
2. **破坏性动词表是否作为协议的一部分。** 本文已经把它写成协议（§5.4）。若审阅者认为它太脆弱（自然语言启发式），替代方案是要求 `Read` 方法名必须匹配一个更严格的白名单前缀（如仅 `get*/list*/read*/describe*/inspect*`）。两者不能同时用，否则规则互相打架。
3. **`ph1_` 句柄是否需要可撤销列表。** 当前设计是可推导（宿主私钥派生），因此撤销靠 `grant` 递增，不存句柄表。若审阅者要求"单个句柄立即作废"，就需要持久化签发记录，成本与 T09 注册表设计相关。
4. **`capabilities` 未知项一律视为必需**（§3.2）是否过严。好处是不可能悄悄忽略一个能力；代价是新增可选能力必须走 `optionalCapabilities`，实现方容易漏。

## 15. 本文未证明的事

如实列出，避免被当成已完成的证据：

- **没有实现。** 本文是协议 + schema + checker。任何"已验证"都只针对报文结构与该 checker 覆盖的跨请求规则。
- **没有密码学验证。** `root-v1.json` / `release-v1.json` 的签名是占位字符串；直到 T04 之前，没有一行代码验过签名。
- **schema 不证明生命周期顺序。** 事务阶段、锁顺序、崩溃恢复表在 T04/G2。`checkTrace` 只覆盖"同 ID 同参数"与"binding 一致性"两条，不是完整状态机。
- **没有真实 WebView 证据。** 容器与 ACL 证据在 G0 材料里，本文不重复也不延伸。
- **平台矩阵仅 macOS arm64**（G0 已确认）。schema 的 `targets` 允许其他组合，但那些组合没有任何验证。
