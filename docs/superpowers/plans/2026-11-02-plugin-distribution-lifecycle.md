# 插件分发与生命周期执行子计划（T07–T09、T12、T13、T16、T24、T26、T27）

日期：2026-11-03

状态：**等待 G2 批准后执行**。G2 未批准前不开始 T07。

对应总计划：[总计划](2026-11-02-kanban-independent-release.md)。
对应子 spec：[分发、信任与生命周期](../specs/2026-11-02-plugin-distribution-lifecycle.md)（G2）、[宿主协议](../specs/2026-11-02-plugin-host-protocol.md)（G1，已批准）。

## 0. 继承与基线

### 0.1 从总计划 §0 继承（逐字不改）

- 仓库根 `R`；每条命令从 `cd "$R"` 开始。
- 用本 worktree，不共用 `CARGO_TARGET_DIR`，不碰真实 `~/.yi-agent`、真实项目、生产 relay。
- Rust 测试用绝对 manifest 路径。
- 每个任务走单行为红绿循环；编译失败**不算**行为红。
- 完成门禁 C 六条逐条执行。

### 0.2 从 G1/G2 子 spec 继承的硬约束

| 约束 | 出处 |
|---|---|
| 验签密钥来自信任集，不来自被验证文档 | G2 §3.2 |
| 一条"该拒绝"的测试不足以证明某机制生效，必须构造**只有该机制能拦**的样本 | G2 §3.2（假绿教训） |
| 签名 payload 字段顺序固定，非字母序 | G1 §8.2/§8.3 |
| 恢复先于准入：除 `done` 外准入一律关闭 | G2 §6.1 |
| 锁顺序：生命周期锁 → registry 短锁，不持锁等待 | G2 §5.1 |
| 不能按 TTL/PID 回收 permit | G1 §6.1 |
| 共享域不可用时**阻止**新自动任务，不静默创建独立预算池 | G2 §8.1 |
| 迁移任一失败则全部不恢复新准入 | G2 §6.3 |

### 0.3 基线核对（2026-11-03）

| 断言 | 实际位置 |
|---|---|
| `instance_home()` | `yi-agent-rs/crates/yi-agent-store/src/paths.rs:20`（非 `.yi-agent` 目录名会追加 `/.yi-agent`，`:22-38`） |
| 现有共享配额域 | `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/lease.rs:42`（`$HOME/.yi-agent/superpowers-kanban/leases`，flock 槽位） |
| 配额域解析回退 | `service.rs:216-224`；`None` 时 `service.rs:490`/`:682` **拒绝启动** |
| `lease.rs:47` 注释与代码不符 | 注释说"fall back to per-project limiting"，代码是拒绝启动。**以代码为准**，T13 修注释 |
| `ed25519-dalek` 未在 Cargo.lock | T07 新增依赖 |
| `RUSTSEC-2022-0093` 影响 `< 2.0` | 约束 `>= 2.0.0`（当前 crates.io 3.0.0） |

### 0.4 已验证的环境陷阱

`just ci` 在 agent shell 里会假失败一条（`YI_AGENT_THREAD_ID` 被注入）。用：

```bash
cd "$R/yi-agent-rs"
env -u YI_AGENT_THREAD_ID -u YI_AGENT_SESSION_ID -u SUPERPOWERS_KANBAN_CARD \
  -u YI_AGENT_PLUGIN_INSTANCE -u YI_AGENT_PLUGIN_BINDING \
  CARGO_BUILD_JOBS=2 just ci
```

**不要**改那个测试或加 `#[ignore]`。

---

## T07：发布信任、有效期与撤销

**前置：** T06。

**Files：** `yi-agent-plugins/src/trust.rs`、`tests/trust.rs`；修改 `src/lib.rs`、crate 依赖、主 lock。

**第一步必须是字节一致性**：Node 参考实现（`scripts/plugins/sign-contracts.mjs`）已冻结 payload 编码。Rust 侧若产出不同字节，签名永远验不过，而两侧各自的自测都会绿。

### 循环 T07-a：payload 字节与参考实现逐字节一致

- **测试**：读 `plugins/contracts/fixtures/release-v1.json` 的 `payloadB64`，用 Rust 的 `encode_release_payload` 重新编码，断言字节相等。
- **为什么这是第一条**：这是唯一能证明"两侧对签什么没有分歧"的测试。没有它，后面所有验签测试都可能在一个自洽但错误的编码上进行。
- **红**：编码器不存在。
- **实现**：严格按 G1 §8.1——`domain || 0x00 || field*`，`field = ascii(name) || 0x00 || u32be(len) || value`，整数用十进制 ASCII。
- **绿**。

### 循环 T07-b：正确签名通过

- **测试**：`TrustStore::bootstrap(&f.trust_root)` 后用 fixture 的 release envelope 验签 → `Ok(VerifiedRelease)`。
- **红**：未实现。

### 循环 T07-c：改一个字节 → 拒绝（但这条**不算**签名机制的证据）

- **测试**（总计划原文）：
  ```rust
  let mut bad = f.envelope.clone();
  let n = bad.len() / 2;
  bad[n] ^= 1;
  assert!(verify_metadata(&bad, &mut trust, 1_800_000_000).is_err());
  ```
- **注意**：这条会被"payload 与字段一致性"拦下。**必须**配 T07-d，否则删掉验签它仍然绿（G2 §3.2 记录的假绿教训）。

### 循环 T07-d：改字段并重算 payload，只有签名能拦

- **测试**：改 `sizeBytes` 并重算 `payloadB64`，保留旧 `signature` → 必须 `Err`，且错误 kind 是 `Integrity`。
- **验收方式**：临时把验签改成恒真，**T07-d 必须变红**。这是证明"签名验证真的在起作用"的唯一手段。

### 循环 T07-e ~ T07-i（每条独立）

| 循环 | 断言 | 错误 kind |
|---|---|---|
| T07-e | `now > expiresAt` | `Expired` |
| T07-f | `now < publishedAt`（时钟回退） | `Expired`（或不新于 `InvalidManifest`，实现时选定并在 spec 记录） |
| T07-g | `serial < acceptedSerial` | `Revoked` 或 `Conflict`（选定后记录） |
| T07-h | `keyId` 不在信任集 | `Untrusted` |
| T07-i | 撤回命中的 `(plugin, version)` | `Revoked` |

### 循环 T07-j：换根必须由旧根授权

- **测试**：`next.authorizedBy != current.keyId` → `Err(Untrusted)`；serial 不递增 → `Err`。

### 循环 T07-k：坏元数据不污染 trust store

- **测试**：验签失败后再验一个合法包 → 仍然成功（接受序号未被推进）。
- **这是最容易写错的一条**：把"更新 trust store"放在验签之前会让一次失败永久破坏后续验证。

### 循环 T07-l：正式构建不得嵌入测试根

- **测试**：检查正式构建路径下不存在测试种子/测试根常量。
- **实现边界**：测试密钥只能在 `#[cfg(test)]` 或 test-only feature 下可达。

**T07 出口**：`cargo test --manifest-path "$R/yi-agent-rs/Cargo.toml" -p yi-agent-plugins --test trust` 全绿；T07-d 的变异验证通过；错误输出不含密钥或原始认证信息。

---

## T08：有界安全解包与不可变安装目录

**前置：** T07。

**Files：** `yi-agent-plugins/src/bundle.rs`、`src/limits.rs`、`tests/bundle.rs`。

### 循环 T08-a ~ T08-h：每种攻击一条

| 循环 | 攻击 | 断言要点 |
|---|---|---|
| T08-a | `../` 路径穿越 | 目标目录不存在，包外无 `escaped` |
| T08-b | 绝对路径 entry | 拒绝 |
| T08-c | 软链接 | 拒绝 |
| T08-d | 硬链接 | 拒绝 |
| T08-e | 设备文件 | 拒绝 |
| T08-f | 重复条目名 | 拒绝 |
| T08-g | 大小写碰撞 | 拒绝 |
| T08-h | 解压总量超限 | 拒绝，且**未写满磁盘**（边解边计） |

**关键**：每个攻击都用**有效测试签名**，确保测到的是解包边界，而不是"因为摘要不符所以失败了"（总计划 T08 Step 2 明写）。

### 循环 T08-i：失败不留垃圾

```rust
assert!(!outside.join("escaped").exists());
assert!(!destination.exists());
```

### 循环 T08-j：权限不采纳

- **测试**：归档里 `program.entry` 带 `setuid` → 安装后该位不存在；其他文件只读；只有 `program.entry` 有执行位。

### 循环 T08-k：磁盘写失败 / 取消

- **测试**：写入过程中注入错误 → 临时目录被清理，目标目录不存在。

### 循环 T08-l：安装后文件被替换

- **测试**：安装成功 → 修改落盘文件一个字节 → 启动前完整性检查拒绝。

**T08 出口**：`--test bundle` 全绿；每个攻击都有独立测试函数，不合并成一个巨型测试。

---

## T09：实例注册表、安装幂等与跨进程互斥

**前置：** T08。

**Files：** `yi-agent-plugins/src/registry.rs`、`src/locks.rs`、`tests/registry.rs`。

### 循环 T09-a ~ T09-f

| 循环 | 断言 |
|---|---|
| T09-a | 同摘要重复安装幂等（不报错、不改写、不新增记录） |
| T09-b | 同版本不同摘要 → `Conflict` |
| T09-c | 坏 registry 文件 → 显式 `Io`/`InvalidManifest`，**不覆盖** |
| T09-d | 首次安装只设选中版本，**不建 grant、不建启动记录** |
| T09-e | 读者看不到半个 JSON（原子写 + 目录 fsync） |
| T09-f | 两个 `instance_home` 不串数据 |

### 循环 T09-g：真实跨进程争抢（subprocess）

- **测试**：两个真实子进程同时 `record_install` → 串行成功，无损坏。
- **禁止**：只用同进程 `Mutex` 测跨进程安全（总计划 T09 Step 4 明写）。

### 循环 T09-h：锁顺序

- **测试**：持有 registry 短锁时尝试获取生命周期锁 → 必须失败或按固定顺序阻塞；反向路径不存在。
- **实现边界**：短锁范围内不得有网络/等待调用（用注入的"慢操作"断言它会超时或报错，而不是静默持锁等待）。

**T09 出口**：`--test registry` 全绿，含真实 subprocess 争抢。

---

## T12：候选激活、事务日志与失败恢复

**前置：** T11。

**Files：** `yi-agent-plugins/src/lifecycle.rs`、`src/journal.rs`、`src/migration.rs`、`tests/lifecycle.rs`、`tests/recovery.rs`。

**测试替身**：`RecordingRuntime` 逐项记录 maintenance/start/stop/health 动作，并按指定阶段返回错误。**不得**空实现成总成功——那样恢复测试什么都测不到。

### 循环 T12-a ~ T12-c：状态机基本约束

| 循环 | 断言 |
|---|---|
| T12-a | 候选版本不可被普通 binding 使用（与 `checkTrace` 的 `candidateActivation` 同语义） |
| T12-b | 一个项目迁移失败 → **全部**项目不恢复新准入 |
| T12-c | cancel 只在 `draining`..`committing` 有效；`projecting` 之后拒绝取消 |

### 循环 T12-d：表驱动覆盖 8 个阶段

- **测试**：对 G2 §6.1 的 8 个 phase 各注入一次崩溃 → 重开 registry/lifecycle → `recover()` → 断言：
  1. 返回的 `Progress.phase` 是期望的下一阶段；
  2. 除 `done` 外 `admission` 关闭；
  3. **恢复前没有任何 start/launch 发生**（用 `RecordingRuntime` 的动作序列断言）。

### 循环 T12-e：备份内容

- **测试**：备份里**不含** socket / PID / 活锁；**含**受影响项目数据、实例配置、共享配置修订。

### 循环 T12-f：共享配置回退冲突

- **测试**：事务记录的 `sharedConfigRevision = 12`；回退前发现磁盘上是 13 → 拒绝回退，进入 `RecoveryRequired`，**不覆盖** 13。

### 循环 T12-g：幂等迁移

- **测试**：迁移中断后从备份重做一次 → 结果与一次成功相同；重复执行不叠加副作用。

**T12 出口**：
```bash
cargo test --manifest-path "$R/yi-agent-rs/Cargo.toml" -p yi-agent-plugins --test lifecycle
cargo test --manifest-path "$R/yi-agent-rs/Cargo.toml" -p yi-agent-plugins --test recovery
```
外加断言：恢复前没有 start/launch；备份不含瞬态资源。

---

## T13：整机配额域与项目维护所有权

**前置：** T12、**G3**。

**Files：** `yi-agent-plugins/src/quota.rs`、`src/ownership.rs`、`tests/quota.rs`、`tests/ownership.rs`；修改 `plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/{lease,service,dispatch,resident}.rs`。

### 循环 T13-a：两实例共享同一峰值

- **测试**：两实例/两项目，同一 `QuotaContext` → 合计不超过峰值；不允许各自拿到全额。
- **红**：各实例自建预算池时，合计翻倍。

### 循环 T13-b：共享域不可用 → 阻止

- **测试**：`QuotaContext` 无法定位共享域 → 新自动任务被拒并报告原因；**只读看板仍可用**。
- **同时修正** `lease.rs:47` 注释：把"fall back to per-project limiting"改为"拒绝启动"，与 `service.rs` 实际行为一致。

### 循环 T13-c：修订冲突

- **测试**：两个 owner 并发改共享配置 → 后者收到修订冲突，不覆盖。

### 循环 T13-d：旧格式共存

- **测试**：另一实例仍用旧格式读配置 → 新格式不能静默迁移；不兼容时阻止激活并列出需协调停用的实例。

### 循环 T13-e：项目所有权

| 断言 |
|---|
| 同项目第二 owner 非阻塞失败 |
| 维护期间后端重启，owner 不丢失 |
| 不能抢占活跃持有者 |
| 不能按文件时间判断他人已退出（只认锁） |

### 循环 T13-f：统计口径

- **测试**：`sessions.running`（宿主本进程上报）**不能**直接当整机值；聚合已登记活 owner 的统计，且不双扣同一条租约。
- **注意**：这条的具体口径属于 G3；本文只测"不能用错值"。

**T13 出口**：
```bash
cargo test --manifest-path "$R/yi-agent-rs/Cargo.toml" -p yi-agent-plugins --test quota
cargo test --manifest-path "$R/yi-agent-rs/Cargo.toml" -p yi-agent-plugins --test ownership
cd "$R/plugins/superpowers-kanban" && cargo test -p superpowers-kanban-runner
```

---

## T16：通用插件管理 RPC 与下载协调

**前置：** T14。

**Files：** 见总计划 T16（`plugin_manager/rpc.rs`、`runtime.rs`、`download.rs` 等）。

### 关键约束

- `plugins/v1/*` 17 个方法（G1 §11 闭合集合）逐个接线；**未实现的返回 `NotSupported`，不伪装成功**。
- `install` 与 `enableProject` 分开：安装不产生授权。
- 下载有超时、取消、磁盘不足处理；下载与安装共用生命周期锁。
- 管理命令**不得**携带页面业务权限。

### 循环（每条方法一组红绿）

建议顺序（先读后写）：

1. `catalog`、`list`、`transaction`、`operationResult`、`assetRelease`、`inspectLegacy`（只读）
2. `install`、`enableProject`、`changeGrant`、`disableProject`
3. `activate`、`rollback`、`cancelTransaction`、`uninstall`、`clearData`、`gc`
4. `adoptLegacy`

**T16 出口**：`cargo test -p yi-agent-app-server --lib plugin_manager`；每个返回 `NotSupported` 的方法有断言（不能只测成功的路径）。

---

## T24：停用、卸载、清理与数据保留

**前置：** T23。

**Files：** `yi-agent-plugins/src/uninstall.rs`、`src/gc.rs`、`tests/uninstall.rs`、`tests/gc.rs`；修改 lifecycle、管理 RPC、`PluginCatalog`。

**复用**：停用/卸载复用 `Admission` + `Lifecycle`，**不另造一套杀进程逻辑**。

### 循环 T24-a ~ T24-g

| 循环 | 断言 |
|---|---|
| T24-a | 活动回合时卸载 → 等待或要求用户显式取消；拒绝时恢复旧状态 |
| T24-b | 停止写准入后，新任务不能穿过 |
| T24-c | 默认卸载**保留**项目数据 |
| T24-d | `clearData` 必须列明项目与范围，需二次确认 |
| T24-e | 项目不可达 → 留 `pending cleanup` 记录 + 可见错误，**不宣称完全卸载** |
| T24-f | 其他插件仍引用的常驻服务 → 保留 |
| T24-g | 共享配额配置**不**被卸载删除 |

### 循环 T24-h：GC 只清引用安全的产物

- **输入**：受管引用清单（旧会话可读 skill、事务、备份、活进程、其他实例共享资源）。
- **测试**：每一项被引用时都不被删；无引用时才删。

### 循环 T24-i：卸载后旧 supervisor 不启动

- **测试**：卸载完成 → supervisor 重启决策为"不启动"。

**T24 出口**：`--test uninstall`、`--test gc` 全绿；UI 回归停用/待清理与错误提示。

---

## T26：打包、独立 CI 与安全发布流水线

**前置：** T25。

**Files：** 见总计划 T26。

### 关键约束

- **不使用生产签名密钥**（用户明确约束）。打包流程用测试密钥，并在产物里标记为测试构建。
- 正式构建检查**禁止嵌入测试根**（T07-l 的延伸）。
- 独立 CI 必须包含：`check-contracts`、`check-host-boundary`、插件 workspace 测试、发布物内容检查（不含业务资源/程序/skill）。
- 发布物检查必须能失败：注入一个业务文件 → 必须有断言红。

**T26 出口**：CI 全绿；发布物检查的负例验证通过。

---

## T27：A01–A20 正式验收

**前置：** T26。

**Files：** 见总计划 T27。

### 要求

- 逐条对应总计划 §5 的 A01–A20，每条给出**可执行命令或 file:line**，不写"已实现"。
- 每条至少有一个人工可复核的证据（报告、测试名、命令行输出）。
- `docs/project-management/` 对应模块同步更新，索引计数一并改。

**T27 出口**：A01–A20 全部有证据；`just ci` 全绿；看板状态可进入 `awaiting_merge`。

---

## 5. 跨任务门禁

每个任务提交前：

1. `cargo fmt --all`（对应 workspace）；
2. `just ci`（用 §0.4 的干净环境命令）；
3. 前端改动另跑 `npm ci && npm run build && npm test`；
4. 契约改动另跑 `node scripts/plugins/check-contracts.mjs && node --test scripts/plugins/check-contracts.test.mjs`；
5. 同一 commit 更新 `docs/project-management/`；
6. 只 `git add` 本任务文件。

## 6. 阻塞点

| 阻塞点 | 谁定 |
|---|---|
| 看板 CLI/method → effect 映射 | G3 |
| `sessions.running` 的整机口径 | G3 |
| 共享配置格式的新旧共存细节 | G3 |
| 熔断默认值（G2 §9.1） | G2 审阅 |
| `projecting` 不可回滚（G2 §9.2） | G2 审阅 |
| 共享配置回退选择 `RecoveryRequired`（G2 §9.3） | G2 审阅 |
| `ed25519-dalek` 版本约束（G2 §9.4） | G2 审阅 |
