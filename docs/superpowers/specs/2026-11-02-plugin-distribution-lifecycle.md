# 插件分发、信任与生命周期 v1（G2 门禁材料）

日期：2026-11-03

状态：**等待 G2 人工审阅**。本文是 G2 的输入，不是 G2 的结论。

前置：[G1 宿主协议](2026-11-02-plugin-host-protocol.md)（已批准 2026-11-03）。

对应设计：[设计](2026-11-02-kanban-independent-release-design.md) §5、§6、§9、§7.1；对应计划：[总计划](../plans/2026-11-02-kanban-independent-release.md)（T04）；执行子计划：[分发与生命周期](../plans/2026-11-02-plugin-distribution-lifecycle.md)。

可执行部分：

| 文件 | 作用 |
|---|---|
| `plugins/contracts/root-v1.schema.json` | 信任根信封结构 |
| `plugins/contracts/release-v1.schema.json` | 已签名发布元数据（含被签名字节） |
| `plugins/contracts/lifecycle-v1.schema.json` | 生命周期事务日志结构 |
| `plugins/contracts/fixtures/{root,release,lifecycle}-v1.json` | 正样本，root/release 含**真实** Ed25519 签名 |
| `scripts/plugins/sign-contracts.mjs` | 规范化 payload 编码 + 验签 + 恢复表 |
| `scripts/plugins/check-contracts.test.mjs` | 42 条断言（G1 21 条 + G2 新增 21 条） |

运行：

```bash
npm ci --prefix plugins/contracts
node scripts/plugins/check-contracts.mjs    # positive: 9, negative: 24, failures: 0
node --test scripts/plugins/check-contracts.test.mjs   # 42/42
```

## 1. 本文冻结什么、不冻结什么

**冻结**：

- 签名原始 payload 的字段顺序与编码（G1 §8 已定，本文落实为可执行代码）；
- 签名覆盖的字段集合（设计 §5 要求的 9 个字段）；
- 有效期、发布序号、回放、时钟异常的四条拒绝规则；
- 信任根引导与换根授权规则；
- 生命周期 8 个持久化阶段的「下一动作 / 可否回滚 / 是否开放准入」；
- 锁顺序：生命周期锁 → 短期 registry 锁，不得持锁等待；
- 默认安全限制与解包边界的判定口径。

**不冻结**（留给实现，T07–T09、T12）：

- `ed25519-dalek` 的**精确补丁版本**（只约束 `>= 2.0.0`，理由见 §3.4）；
- 事务日志的物理文件格式与 fsync 序列（T09/T12 实现时定，但必须满足 §6 的可见性要求）；
- 迁移脚本的具体内容（属于插件包的 `data.migrations[]`，T12 执行）；
- 看板 effect 映射（G3）。

## 2. 签名原始 payload

### 2.1 编码（G1 §8 的执行版）

```text
payload := domain || 0x00 || field(name, value) * N
field(name, value) := ascii(name) || 0x00 || u32be(byte_len(value)) || value
```

实现：`scripts/plugins/sign-contracts.mjs` 的 `encodeRootPayload` / `encodeReleasePayload`。

**为什么不是 JSON**：JSON 的键序、空格、数字表示都允许合法变体，同一语义会产生不同字节，签名侧与验证侧一旦各选一种就永远对不上，且两边自测都会过。

### 2.2 字段顺序（协议的一部分）

根信封（§8.2）：

```text
keyId, publicKey, serial, notBefore, notAfter
```

发布元数据（§8.3）：

```text
plugin, version, target, sha256, sizeBytes, hostApi, serial, publishedAt, expiresAt
```

### 2.3 设计 §5 要求的绑定覆盖情况

设计原文：「签名覆盖明确的原始 payload 字节，绑定 ID、版本、平台、包摘要、大小、兼容范围及发布序号」。

| 设计要求 | payload 字段 | 有独立断言 |
|---|---|---|
| ID | `plugin` | `the signature covers every field the design requires` |
| 版本 | `version` | 同上 |
| 平台 | `target` | 同上 |
| 包摘要 | `sha256` | 同上 |
| 大小 | `sizeBytes` | 同上 |
| 兼容范围 | `hostApi` | 同上 |
| 发布序号 | `serial` | 同上 |
| （有效期） | `publishedAt`, `expiresAt` | 同上 |

**`target` 在 payload 里意味着同一版本的两个平台是两个不同的发布**，各有独立签名与序号。这与 G1 §5.2b 的 `target` 单值设计一致。

### 2.4 信封为什么要携带 `payloadB64`

发布信封同时带 `release` 字段与 `payloadB64`。验证顺序：

1. 用 `payloadB64` 的字节验签；
2. 断言 `encodeReleasePayload(release)` 与 `payloadB64` 完全相等。

第 2 步是**纵深防御**：即使签名验证实现被破坏，字段与字节不一致仍会被发现。但第 2 步**不能替代**第 1 步——见 §3.2 的假绿教训。

## 3. 信任验证规则

### 3.1 引导：只接受内置根

```text
bootstrapTrust(rootEnvelope, {pinnedKeyIds})
```

- `pinnedKeyIds` 是宿主内置（编译时固定）的根 ID 集合。
- 首次从网络拿到的 root envelope **不能**成为新锚。否则信任根可被任意替换，整个签名体系失效。
- 断言：`a root envelope cannot bootstrap itself`。

### 3.2 验签：密钥来自信任集，不来自被验证文档

- 根的 `publicKey` 必须等于信任集中该 `keyId` 对应的密钥。
- **不能**直接用被验证文档里声明的 `publicKey` 去验证它自己的签名——那是自证循环。
- 断言：`a valid signature does not authorize an unknown root key`。

**这里有一条必须记录的假绿教训。** 最初只有一条篡改测试（`flipping one payload byte`），它在实现里被"payload 与字段一致性"检查拦下，所以**即使把验签整个换成 `return true`，那条测试仍然绿**。变异测试发现了这一点（M1 变异导致 0 条失败）。补了 `rewriting a field and recomputing the payload still fails the signature` 之后，M1 变异才会打红 3 条。

**结论**：一条"该拒绝"的测试不足以证明某个具体机制在起作用；必须构造一条**只有该机制能拦**的样本。这条教训同样适用于后续所有安全断言。

### 3.3 四条时间/序号拒绝规则

| 条件 | 错误 | 为什么必须显式 |
|---|---|---|
| `now > expiresAt` | `expired` | 过期包可能含已知撤回的版本 |
| `now < publishedAt` | `not valid yet` | 时钟回退会让"尚未发布"的包看起来有效 |
| `serial < acceptedSerial` | `replay or rollback` | 无授权回放旧目录；普通更新不降级 |
| `keyId` 不在信任集 | `unknown signing key` | 下载端不能选择公钥 |

四条各有独立断言。变异测试（逐个把条件改成 `if (false)`）每条都打红 3 条测试。

### 3.4 库版本

- 约束：`ed25519-dalek >= 2.0.0`。当前 crates.io 为 `3.0.0`。
- **理由**：`RUSTSEC-2022-0093` 只影响 `< 2.0`（私钥/公钥可分离建模导致的签名预言机攻击），v2 起公共 API 不再接受分离的密钥对。所以 `>= 2.0.0` 是安全下界，不是随意选的值。
- 精确补丁版本留到 T07 在 `cargo audit` 通过后锁定。**不在 G2 提前钉死**，否则 G2 会变成依赖评审。
- **不自造密码算法**（设计 §5）。

### 3.5 换根

```text
rotateRoot(currentEnvelope, nextEnvelope)
```

- `nextEnvelope.authorizedBy` 必须等于 `currentEnvelope.root.keyId`；
- `nextEnvelope.root.serial` 必须大于当前根的 serial；
- 当前根必须先验证通过。

没有旧根授权的换根一律拒绝——那等于允许任何人替换信任锚。

### 3.6 验签失败时不能污染 trust store

T07 实现要求：**只在整体验证成功后推进接受序号**。坏元数据不得留下任何痕迹（不接受序号、不缓存、不记录为"已见"）。否则一次失败的验证就能把后续合法包变成"回放"。

## 4. 安装与解包

### 4.1 判定顺序

```text
验签 → 有效期/序号 → 包摘要与大小 → 有界解包 → 结构检查 → 原子落盘
```

任一步失败，临时产物必须清理，目标目录**不存在**。断言形态（T08）：

```rust
assert!(!outside.join("escaped").exists());
assert!(!destination.exists());
```

### 4.2 解包拒绝清单

来自设计 §6：链接（软/硬）、设备文件、绝对路径、`..`、大小写碰撞、重复条目。

**默认上限**（G1 §10 已定，此处重申）：包 256 MiB、解压总量 1 GiB、单文件 128 MiB、文件数 20 000。宿主可降低，插件不可提升。

**必须边解边计**，不能先解压再看总量——后者让上限退化为事后统计。

### 4.3 权限

- 归档中的 uid/gid、setuid/setgid、原始权限**不采纳**。
- 只有清单 `program.entry` 声明的入口由宿主设置受控执行位；其他静态文件只读。

### 4.4 幂等

- 同 `(plugin, version, sha256)` 重复安装是幂等操作，不报错、不重写。
- 同 `(plugin, version)` 不同 `sha256` → `Conflict`，拒绝覆盖。
- 首次安装只设"选中版本"，**不创建 grant、不创建启动记录**（设计 §6：安装不产生运行授权）。

## 5. 锁顺序与跨进程互斥

### 5.1 唯一允许的顺序

```text
生命周期锁（长） → 短期 registry 锁（短）
```

**禁止反向**：持有 registry 短锁去等网络、等进程退出、等项目维护完成。否则一个慢请求会让所有实例的 registry 读取全部阻塞。

### 5.2 具体规则

| 规则 | 理由 |
|---|---|
| 同一实例同一插件同时只能有一个生命周期写事务 | 两个并发激活会各自提交代次 |
| registry 短锁只覆盖"读-改-写"一段 | 跨进程原子性需要，但不能包住 IO |
| 不持 registry 锁等待进程/网络/迁移 | 见上 |
| 项目所有权在 guardian，跨实例争用**非阻塞失败** | 设计 §7.1：不能抢占活跃持有者 |
| 不能仅靠文件时间猜测其他实例已退出 | 设计 §7.1 明写；只认锁状态 |

### 5.3 锁必须真实存活

与 G1 §6.1 一致：permit / 所有权都是 OS 级文件锁。进程崩溃 → 内核释放；进程 `SIGSTOP` → **不释放**，因此不能按 TTL 或 PID 回收。

## 6. 生命周期：8 个持久化阶段

### 6.1 阶段表（本文冻结）

| phase | 崩溃后下一动作 | 可否回滚 | 准入开放 |
|---|---|---|---|
| `draining` | 完成屏障建立，再从日志重读已接纳回合集合 | 是 | **否** |
| `drained` | 确认每个屏障前回合已释放 permit，然后进入备份 | 是 | **否** |
| `backing_up` | 丢弃残缺备份并重做；旧版本仍完整 | 是 | **否** |
| `migrating` | 从备份恢复受影响项目数据，幂等重做迁移 | 是 | **否** |
| `health_checking` | 任一项不健康则停候选服务并回退到上一激活 | 是 | **否** |
| `committing` | 视为尚未持久化：重读日志，或重做原子提交，或回滚 | 是 | **否** |
| `projecting` | 从已提交激活修复项目投影，然后开放准入 | **否** | **否** |
| `done` | 开放准入 | 否 | 是 |

两条不变量：

1. **恢复先于准入**：除 `done` 外任何阶段准入都必须关闭。断言：`every persisted phase declares its recovery action` 里对 `admissionOpen` 的循环检查。
2. **`projecting` 不可回滚**：此时新激活已提交，回退就是第二次提交，风险大于修复投影。

### 6.2 设计 §9.2 的七个步骤对应

| 设计步骤 | 本文 phase |
|---|---|
| 1 校验/取锁/记录旧代次 | （事务建立前，不持久化） |
| 2 原子建立排空屏障 | `draining` |
| 3 等待屏障前操作结束 | `drained` |
| 4 超时/取消 → 保持旧版本 | （回滚路径，见 §6.3） |
| 5 停旧服务、暂停重启、保持所有权 | `drained` 的后半 |
| 6 备份 + 迁移 + 候选健康检查 | `backing_up` → `migrating` → `health_checking` |
| 7 原子提交 + 刷新投影 + 恢复准入 | `committing` → `projecting` → `done` |

### 6.3 取消与回滚

- **只在可安全退回的阶段允许取消**：`draining`、`drained`、`backing_up`、`migrating`、`health_checking`、`committing`。
- 取消后：保持旧版本、恢复旧准入、**不强杀任务**。界面列出阻塞项目/回合。
- `projecting` 之后不可取消（已提交）。
- 多项目迁移**任一失败则全部不恢复新准入**。断言：`a failed migration does not restore admission for any project` 要求 `admittedProjects.length === 0` 且 `outcome === "recovery_required"`。

### 6.4 共享配额配置的回退

设计 §9.3：「共享配额配置回退须确认修订未被其他实例更新，不能覆盖别人的新设置」。

实现口径：

- 事务开始时记录 `sharedConfigRevision`（journal 字段，已在 schema 中冻结）；
- 回退前重新读取当前修订；若已前进，**拒绝回退**并进入 `RecoveryRequired`，保留备份与日志；
- 不能"锁住当前实例就迁移全机共享状态"（设计 §7.1）。

### 6.5 备份内容

**包含**：受影响项目数据、实例配置、共享配置的修订信息。

**不包含**：socket、PID 文件、活锁等瞬态资源（设计 §9.2 明写）。备份里带活锁会导致恢复后出现"幽灵持有者"。

### 6.6 候选版本不可用

候选版本在 `health_checking` 期间只有维护凭据可用，**不是公共激活版本**。断言：`a candidate release is not usable through a normal binding`（`checkTrace` 的 `candidateActivation` 选项，两级 binding 都检查，防止继承路径绕过）。

## 7. 失败窗口与熔断

T14 需要具体数值，本文给出**默认值**（宿主可配置，但不得放宽到无效）：

| 项 | 默认 | 说明 |
|---|---|---|
| 健康检查总超时 | 30 s | 单个项目健康检查 10 s |
| 后端崩溃后的重启退避 | 1 s → 2 s → 4 s → 8 s → 16 s（上限 16 s） | 指数退避 |
| 连续崩溃阈值（触发熔断） | 5 次 / 10 分钟 | 达到后关闭新自动任务 |
| 熔断后恢复 | 手动，或连续 10 分钟无崩溃 | **不自动无限重试** |

熔断期间：

- 关闭**新自动任务**；
- 显示后端故障（**不显示为空数据**，设计 §9.3 末段）；
- 只读查看与普通聊天不受影响。

## 8. 共享配额域（G2 与 G3 的边界）

### 8.1 现状核对（必须记录）

现有实现 `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/lease.rs`：

- 配额域是**已存在的共享 flock 槽位池**，路径 `$HOME/.yi-agent/superpowers-kanban/leases`；
- 用 `flock` 而非 PID 文件（内核在进程退出/`SIGKILL` 时释放锁，PID 不可靠）；
- `service.rs:222` 在 `home` 缺失时回退到 `lease::global_leases_dir()`；
- `service.rs:490`、`:682` 在 `leases_dir` 为 `None` 时**直接返回**（不启动），**不是**退化为按项目限流。

**发现一处注释与代码不符**：`lease.rs:47` 的注释写「A caller that cannot resolve it must fall back to per-project limiting rather than inventing a directory」，但 `service.rs` 的实际行为是**拒绝启动**，与设计 §7.1「共享域不可用时阻止新自动任务」一致。

**处置**：以**代码行为**为准（符合设计），注释是错的，T13 修正注释。**不能**按注释去"修复"代码——那会把设计要求的"阻止"改成设计禁止的"静默创建独立预算池"。

### 8.2 本文只冻结接口，不冻结看板语义

| 项 | 归属 |
|---|---|
| `QuotaContext { config_path, leases_path, revision }` 由宿主注入 | G2（本文） |
| 共享配置的修订冲突处理规则 | G2（§6.4） |
| 看板 CLI/method 如何映射到 effect、配额如何按卡片计量 | **G3** |
| `sessions.running` 的口径 | **G3**（设计 §7.1 明写它是宿主本进程上报，不能直接当整机计数） |

## 9. 需要审阅者决定的事

1. **熔断默认值（§7）是否接受。** 5 次/10 分钟、退避上限 16 s、熔断后需手动或 10 分钟无崩溃才恢复。这些值没有实测依据，是按"不让用户被无限重启打扰、也不让一次瞬时故障就熔断"取的。若你有运维经验值，现在改成本最低。
2. **`projecting` 不可回滚**（§6.1）是否接受。替代方案是允许回滚到上一激活，但那等于第二次提交，且可能丢失已迁移的数据。
3. **共享配置回退遇到修订前进时，选择 `RecoveryRequired`（拒绝回退）**（§6.4）是否接受。替代方案是覆盖并提示，但那会丢掉别人的设置——设计 §9.3 明确禁止。
4. **`ed25519-dalek` 只约束 `>= 2.0.0` 而不钉死 3.0.0**（§3.4）是否接受。钉死的好处是完全可复现；坏处是 G2 变成依赖评审，且 3.0.0 刚发布可能还有补丁。

## 10. 本文未证明的事

- **没有 Rust 实现。** 本文的验签是 **Node 参考实现**（`sign-contracts.mjs`），用来冻结字节编码与判定规则。Rust 侧（T07）必须产出**逐字节一致**的 payload；这是 T07 的第一条测试。
- **没有真实密钥轮换演练。** `rotateRoot` 只验证了授权字段存在且 serial 递增，没有做端到端换根。
- **没有崩溃注入。** 8 个阶段的恢复表是**声明**，T12 才用注入崩溃验证每个阶段的下一动作。
- **锁顺序是声明，不是证明。** `lifecycle 锁 → registry 锁` 的强制需要在 T09/T12 用真实跨进程争抢验证。
- **熔断数值没有实测依据**（见 §9.1）。
- **平台矩阵仍只有 macOS arm64**（G0 结论）。
