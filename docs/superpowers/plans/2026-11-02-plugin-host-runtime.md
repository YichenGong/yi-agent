# 插件宿主运行时执行子计划（T06、T10、T11、T14、T15、T17–T20）

日期：2026-11-03

状态：**等待 G1 批准后执行**。本文依赖 [宿主协议子 spec](../specs/2026-11-02-plugin-host-protocol.md) 的 G1 结论；G1 未批准前不开始 T06。

对应总计划：[总计划](2026-11-02-kanban-independent-release.md)（T06、T10、T11、T14、T15、T17–T20）。

## 0. 继承的前置、门禁与基线核对

### 0.1 继承自总计划的硬约束

从总计划 §0 逐条继承，逐字不改：

- 仓库根为 `R`；每条命令从 `cd "$R"` 开始。
- 使用本 worktree，不共用 `CARGO_TARGET_DIR`，不碰真实 `~/.yi-agent`、真实项目或生产 relay。
- Rust 核心测试用**绝对 manifest 路径**：`cargo test --manifest-path "$R/yi-agent-rs/Cargo.toml" -p <crate>`。
- 每个任务走"测试代码 → 行为红 → 最小实现 → 同命令绿 → 提交"的单行为循环；总计划的多个负例**不得**合成一个巨型实现步骤。
- 完成门禁 C 的六条（见总计划）逐条执行，特别是第 1 条：**编译失败不算行为红**，接入后还要做一次真实断言失败验证。
- G2/G3 未批准前不执行 T14/T15/T17–T20（它们消费 G2 的事务/锁顺序与 G3 的看板映射）。

### 0.2 前置链（本文内部）

```text
G1 ──► T06 ──► T10 ──► T11 ──────────────► T15
                │         │                  ▲
                └──► G2 ──┴──► T14 ──────────┘
                             │
                             └──► G0 ──► T17 ──► T18 ──► T19 ──► T20
```

- T06 同时需要 G1、G2、G3、B0。**这是一处真实的串行瓶颈**：类型 crate 虽小，但它的公共类型必须同时满足三份子 spec，否则 T10/T14 会各自改造它。
- T17 需要 T16（管理 RPC）与 G0（已批准）。
- T18→T19→T20 是链式的，不能并行。

### 0.3 基线核对（2026-11-03，本 worktree）

计划要求"重新核对基线行号"。以下行号是本次实际核对结果，**执行时若已漂移，先更新本节再动手**。

| 断言 | 实际位置 | 结论 |
|---|---|---|
| `instance_home()` 遵循 `YI_AGENT_HOME` | `yi-agent-rs/crates/yi-agent-store/src/paths.rs:20` | 存在；注意它会在目录名非 `.yi-agent` 时追加 `/.yi-agent`（`:22-38`） |
| `ThreadMeta` 定义 | `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs:41`，结束 `:142` | 现有字段含 `driver_project`(`:63`)、`driver_ref`(`:69`)、`last_read_at`(`:125`)、`last_completed_at`(`:133`)、`forked_from`(`:141`）。**`Vec<Binding>` 需新增，且必须 `#[serde(default)]`** |
| workspace members | `yi-agent-rs/Cargo.toml:3-19` | 15 个 crate，`yi-agent-plugins` 需加入 |
| supervisors 模块导出 | `yi-agent-rs/crates/yi-agent-supervisors/src/lib.rs:6-13` | `daemon/install/manifest/refresh/supervisor/switch/watch/watchman`；`managed` 需新增 |
| IPC 类型 | `yi-agent-rs/crates/yi-agent-store/src/ipc/protocol.rs:18`(`RequestEnvelope`)、`:41`(`IpcRequest`)、`:321`(`IpcResponse`)、`:480`(`IpcErrorCode`) | 维护协议要在这四处加变体 |
| IPC 导出 | `yi-agent-rs/crates/yi-agent-store/src/ipc/mod.rs:58-73` | 新模块需在此 `pub use` |
| CLI `Command` enum | `yi-agent-rs/crates/yi-agent/src/config.rs:96` | 无 `Plugins` 变体；需新增 `PluginsAction` 子枚举 |
| 桌面前端命令白名单 | `desktop/src-tauri/capabilities/default.json` | G0 已为 12 条 app 命令逐条放行；新增命令**必须同步**，否则 `app_command_capability.rs` 会红 |

**核对方式**（可重复）：

```bash
cd "$R"
grep -n "pub fn instance_home" yi-agent-rs/crates/yi-agent-store/src/paths.rs
awk 'NR>=41 && /^}/ {print NR; exit}' yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs
grep -n "pub mod" yi-agent-rs/crates/yi-agent-supervisors/src/lib.rs
grep -n "pub enum Command" yi-agent-rs/crates/yi-agent/src/config.rs
```

### 0.4 本文不做的事

- 不重复总计划的文件清单（本文按任务引用，不复制）。
- 不写实现代码。每个循环只给测试的**断言形态**与最小实现边界；具体 Rust 代码在执行时写。
- 不定 G2/G3 的语义（事务恢复表、锁顺序、看板 effect 映射）。遇到必须先定的点，**停下来标 BLOCKED**，不在执行时临时决定。

---

## T06：新增独立核心 crate、类型与测试夹具

**前置：** G1、G2、G3、B0。

**Files：** 见总计划 T06。新增 `yi-agent-rs/crates/yi-agent-plugins/{Cargo.toml,src/lib.rs,src/types.rs,src/error.rs,src/manifest.rs,tests/manifest.rs,tests/common/mod.rs}`，修改 `yi-agent-rs/Cargo.toml`、`Cargo.lock`，新增 `docs/project-management/yi-agent-plugins.md` 并登记到 `docs/project-management/README.md`。

**约束：** 核心路径只由调用方注入；**不得**依赖 `yi-agent-store`、Tauri 或 app-server。新公共类型 derives 必须与 schema 一致（`deny_unknown_fields` + `rename_all` 风格要与 `manifest-v1.schema.json` 的 `additionalProperties: false` 对齐）。

### 循环 T06-a：fixture 读取失败必须 panic，不返回空默认值

- **测试**（`tests/common/mod.rs` + `tests/manifest.rs`）：
  ```rust
  #[test]
  fn fixture_bytes_missing_file_panics() {
      let err = std::panic::catch_unwind(|| common::fixture_bytes("no-such-fixture.json"));
      assert!(err.is_err());
  }
  ```
- **红**：`cargo test --manifest-path "$R/yi-agent-rs/Cargo.toml" -p yi-agent-plugins --test manifest fixture_bytes_missing_file_panics` → 编译失败（crate 未建）。建 crate 骨架后再跑，必须因"返回空 vec 而非 panic"而红。
- **最小实现**：`fixture_bytes` 从 `CARGO_MANIFEST_DIR/../../../plugins/contracts/fixtures` 读，`unwrap_or_else(|e| panic!(...))`。
- **绿**：同一命令通过。
- **提交**：与 T06-b 合并提交。

### 循环 T06-b：绝对路径入口被拒绝

- **测试**：
  ```rust
  #[test]
  fn absolute_ui_entry_is_rejected() {
      let mut v: serde_json::Value = serde_json::from_slice(
          &common::fixture_bytes("manifest-v1.json")).unwrap();
      v["entry"]["view"] = "/etc/passwd".into();
      assert!(Manifest::parse(&serde_json::to_vec(&v).unwrap()).is_err());
  }
  ```
  总计划示例与 G1 冻结的 schema 一致（`views` 是数组，每项有 `entry`）：设计 §5 要求清单声明「主视图与设置入口」，因此是多个视图而不是单个 `entry`。这条曾一度被写成单个 `entry.view`，已按设计修正——**任何后续改动以 `plugins/contracts/manifest-v1.schema.json` 为准**。
- **红**：`cargo test ... --test manifest absolute_ui_entry_is_rejected` 因未校验绝对路径而红。
- **最小实现**：`Manifest::parse` 先做 serde 反序列化，再逐项校验每个 `views[].entry`、`program.entry`、`skills[].path`、`data.migrations[].entry` 不含前导 `/`、不含 `..` 段、不含 `\` 与 NUL。
- **绿**：同命令。
- **提交**：`feat: parse and validate plugin release manifests`（C 门禁：`cargo fmt --all`、`just ci`、更新模块文档）。

### 循环 T06-c ~ T06-g（每条一红一绿，各自一个测试函数）

| 循环 | 断言 | 最小实现边界 |
|---|---|---|
| T06-c | `schemaVersion = 2` → `Err(InvalidManifest)` | 常量比较，不用范围 |
| T06-d | `methods[0].effect = "Maintenance"` → `Err` | `Effect` 枚举无 `Maintenance`；反序列化即失败 |
| T06-e | `capabilities.required` 含 `Read`/`Write` 但无对应 method → `Err`（`Launch`/`QuotaWrite`/`Resident` 不适用，它们是宿主签发的能力） | 集合包含检查 |
| T06-f | `views` 里 `main` 的数量 ≠ 1 → `Err` | 计数检查 |
| T06-g | `data.migrations` 里 `to != from + 1` → `Err` | 相邻性检查 |

**T06 出口条件**：`cargo test --manifest-path "$R/yi-agent-rs/Cargo.toml" -p yi-agent-plugins` 全绿；`just ci` 全绿；`docs/project-management/yi-agent-plugins.md` 每条 feature 带 `file.rs:line` 或可执行命令。

---

## T10：项目授权、双代次与能力分类

**前置：** T09。

**Files：** `yi-agent-plugins/src/grants.rs`、`src/project.rs`、`tests/grants.rs`。

**关键不变量**（来自 G1 子 spec §2）：代次只增不复用；binding 不跨宿主、不跨项目。

### 循环 T10-a：撤销 A 不影响 B

- **测试**：两个**不同** `ProjectId` 的临时项目各建 binding；只撤销 A。
  ```rust
  assert!(registry.check(&old_project_a, Effect::Write).is_err());
  assert!(registry.check(&unchanged_project_b, Effect::Write).is_ok());
  ```
  fixture **不得**用相同 `ProjectId` 冒充两个项目——那样测试会在只撤销一个键的情况下碰巧通过。
- **红**：撤销实现为"清空插件全部授权"时，B 也红。
- **最小实现**：授权按 `(plugin, project)` 独立记录代次；撤销只递增目标项目。
- **绿**。
- **提交**：`feat: authorize plugin projects with scoped bindings`。

### 循环 T10-b：`Launch` 不被 `Read` 覆盖

- **测试**：
  ```rust
  assert!(registry.check(&read_only_binding, Effect::Launch).is_err());
  ```
- **最小实现**：能力集合是精确匹配，不做"读包含写"的层级推导。

### 循环 T10-c：同版本重启后旧 binding 失效

- **测试**：激活→拿到 binding→重新激活同一版本→旧 binding `check` 返回 `StaleBinding`；新 binding 可用。
- **最小实现**：激活代次无条件递增，即使版本相同。

### 循环 T10-d：跨宿主 binding 失效

- **测试**：binding 记录 `host_id` A；注册表以 `host_id` B 打开；`check` → `StaleBinding`。

### 循环 T10-e：项目 ID 规范化

- **测试**：真实 symlink 目录、未登记 worktree、同名目录（不同父路径）三种输入，规范化后得到不同/相同的 `ProjectId` 符合预期。
- **最小实现**：canonicalize + 拒绝未登记路径；**不做**前缀猜测。

### 循环 T10-f：升级不自动获得新权限

- **测试**：manifest 新版新增 `Resident` 声明；旧 binding `check(Resident)` → `PermissionDenied`，直到用户重新确认。
- **实现边界**：升级只更新声明，不改授权记录。

**T10 出口**：`cargo test --manifest-path "$R/yi-agent-rs/Cargo.toml" -p yi-agent-plugins --test grants` 全绿，且覆盖持久化后重开注册表。

---

## T11：活动准入、排空屏障与幂等操作记录

**前置：** T10。

**Files：** `yi-agent-plugins/src/admission.rs`、`src/operations.rs`、`tests/admission.rs`、`tests/operations.rs`。

**新增接口**（总计划 T11 要求）：`OperationLedger::result(request_id, binding) -> Result<OperationOutcome, PluginError>`；`OperationOutcome` 与 TS `pending|committed|unknown|rejected` 一一对应。

### 循环 T11-a：排空期间拒绝新 Launch

- **测试**：`drain(plugin)` 后 `begin(Operation{effect: Launch})` → `Draining`。
- **红**：无屏障时通过。

### 循环 T11-b：排空期间允许旧 turn 收尾

- **测试**：turn 已在屏障前持有 permit；`drain` 后该 turn 的 `Write` 仍成功。
- **实现边界**：判定依据是"permit 建立于屏障之前"，不是"turn ID 看起来旧"。

### 循环 T11-c：turn 结束后旧凭据不能开下一轮

- **测试**：turn 结束 → `begin_turn` 用同一 turn_id 与旧 permit → 拒绝。

### 循环 T11-d：`parent_turn` 不是授权

- **测试**：外部写 `Operation{parent_turn: Some("别人的-turn")}`，宿主未登记该绑定 → `PermissionDenied`。
- **实现边界**：必须同时校验（1）宿主登记的会话/调用者绑定、（2）仍存活的屏障前 permit、（3）`project` 与 `grant` 一致。

### 循环 T11-e：同 ID 同参数不重复提交

- **测试**：两次 `begin` 同 `request_id` 同参数 → 第二次返回已记录结果，副作用只发生一次（用计数器验证）。

### 循环 T11-f：同 ID 改参数 → Conflict

- **测试**：第二次 `begin` 同 `request_id` 不同参数 → `Conflict`。
- **注意**：参数比较必须与键序无关（与 `checkTrace` 的 `stableStringify` 同语义）；这条要有专门断言，否则 HashMap 序会让测试偶发通过。

### 循环 T11-g：写结果未知不重放

- **测试**：标记 `Unknown` 后重启 ledger → `result()` 仍返回 `unknown`，且**没有**重新执行。

### 循环 T11-h：读操作不留永久日志

- **测试**：`Read` 操作后 ledger 条目数不增长（或按 TTL 清理有界）。

### 循环 T11-i：暂停进程不被 TTL 回收（subprocess）

- **测试**：子进程持有 permit → `SIGSTOP` → 父进程等待远超 TTL → permit 仍视为占用；`SIGCONT` 后子进程能继续写。
- **实现边界**：只认 OS 文件锁状态；**不得**用 PID 存活或 mtime 判定。

### 循环 T11-j：取消 drain 恢复旧准入

- **测试**：`DrainTicket::cancel()` 后 `begin(Launch)` 成功；插件未被整体关闭。

### 循环 T11-k：活跃审批阻塞 drain

- **测试**：turn 处于 `AwaitingApproval` → `drain` 不返回"已完成"，blocking 列表含该 turn。

### 循环 T11-l：结果查询与新准入分离

- **测试**：旧代次 binding 调 `result(已提交 id)` 成功；调 `begin` → `StaleBinding`。切项目后查另一项目的结果 → `PermissionDenied`。

**T11 出口**：`--test admission` 与 `--test operations` 全绿；并发建立屏障有专门测试（多线程同时 `drain` 只产生一个 ticket）。

---

## T14：接入 supervisor、watchman 与受管 CLI

**前置：** T13。

**Files：** 见总计划 T14。`managed.rs`、`plugins_cmd.rs`、`tests/plugins_exec.rs`、`store/src/ipc/managed.rs` 为新增；`supervisors` 的 `lib.rs:6-13` 需加 `pub mod managed;`；`store/src/ipc/protocol.rs` 的 `IpcRequest`(`:41`)/`IpcResponse`(`:321`)/`IpcErrorCode`(`:480`) 需加维护协议变体；`config.rs:96` 的 `Command` 需加 `Plugins(PluginsAction)`。

**G2 依赖**：维护请求/确认报文、事务 ID 与代次关联、IPC 兼容策略必须在 G2 冻结后才实现。**若 G2 未批准，本任务停在这里。**

### 循环 T14-a：绝对路径 + 空 PATH 仍能运行

- **测试**（`tests/plugins_exec.rs`，subprocess）：
  - `env_clear` 后只设最小变量，`PATH` 置空；
  - 以 `CARGO_BIN_EXE_yi-agent` 绝对路径运行 `plugins exec`；
  - 断言 counter 正常自增。
- **红**：依赖 PATH 找 `yi-agent` 或插件程序时失败。

### 循环 T14-b：参数只作 argv，不经 shell

- **测试**：参数含空格、中文、`;`、`$(rm -rf /)`、反引号；断言插件收到的 argv 与输入逐字相同，且未生成副作用文件。
- **实现边界**：`Command::arg` 逐个传，**禁止** `sh -c`。

### 循环 T14-c：插件不得覆盖宿主保留变量

- **测试**：插件 manifest/运行描述尝试设置 `YI_AGENT_HOME`；断言子进程看到的仍是宿主值。
- **数据来源**：`host-v1.json` 的 `reservedEnv`。

### 循环 T14-d：无授权不执行

- **测试**：registry 无 binding → `exec` 返回 `PermissionDenied`，插件进程**未启动**（用不存在的可执行路径证明"没走到 spawn"）。

### 循环 T14-e：旧 manifest 自述不得覆盖受管 launcher

- **测试**：插件自报 `command: "/tmp/evil"`；受管启动仍用注册表解析的路径。

### 循环 T14-f：维护期间不重启

- **测试**：维护暂停期 → supervisor 重启决策为"不启动"；退出维护后恢复。

### 循环 T14-g：两实例不能同时守护同项目

- **测试**：两个 registry 实例对同一项目申请所有权 → 第二个非阻塞失败（不等待）。

### 循环 T14-h：维护 IPC 丢包不抢跑（真实 detached daemon）

- **测试**：启动真实 detached daemon，注入维护通知丢失；daemon 在启动/重连/重启决策前读持久化状态，仍不启动。
- **实现边界**：RPC 通知丢失**不能**成为放行理由。

### 循环 T14-i：旧协议 daemon 被拒而非当作已暂停

- **测试**：daemon 回 `unsupported`/旧协议 → 激活被阻止并提示显式升级/停止；**不得**把解析错误当"已暂停"。

### 循环 T14-j：熔断

- **测试**：注入时钟 + 连续崩溃 N 次 → 达到阈值后关闭新自动任务并报告后端故障；不无限重启。

**T14 出口**：`cd yi-agent-rs && cargo test -p yi-agent --test plugins_exec && cargo test -p yi-agent-supervisors`；额外用真实子进程模拟"launch 后管理进程中断"，确认不存在一边 rollback 一边继续启动新服务。

---

## T15：接入会话、driver、skill 与恢复路径

**前置：** T14。

**Files：** 见总计划 T15。`plugin_manager/{mod,sessions,skills}.rs`、`server/plugin_binding_tests.rs` 新增；`ThreadMeta`(`thread_store.rs:41-142`) 加 `#[serde(default)] pub bindings: Vec<Binding>`。

### 循环 T15-a：ThreadMeta 旧数据兼容

- **测试**：读一份不含 `bindings` 的旧 meta → `bindings` 为空，不报错；写回时若为空不产生该键（`skip_serializing_if`）。
- **红**：加字段后旧文件解析失败。

### 循环 T15-b：无 Launch 权限的 driver 被拒

- **测试**：mock driver 以 `Read` binding 发起任务 → `PermissionDenied`。

### 循环 T15-c：屏障后 driver 被拒

- **测试**：`drain` 后 driver 发起 → `Draining`。

### 循环 T15-d：错误项目 workdir 被拒

- **测试**：driver 任务的 workdir 不属于授权项目 → `PermissionDenied`。

### 循环 T15-e：现有审批不被跳过

- **测试**：插件任务触发的工具调用仍需原有审批；合并仍需合并确认。这是回归断言，防止"插件可信"成为绕过口。

### 循环 T15-f：旧会话聊天正常但插件调用 StaleBinding

- **测试**：激活代次递增后，旧会话普通问答成功；调用插件 → `StaleBinding`（提示需要新会话）。

### 循环 T15-g：resume 不丢版本绑定

- **测试**：会话带 binding → resume 后 binding 仍在且被校验；宿主身份变化 → `StaleBinding`。

### 循环 T15-h：同名 skill 副本阻止自动任务

- **测试**：用户目录存在同名 skill → 冲突诊断列出两个路径与来源；自动看板任务被阻止；普通聊天与只读查看可用。

### 循环 T15-i：permit 在所有路径释放

- **测试**：`AwaitingApproval`、cancel、模型失败、会话重建、后台 driver、用户追加回合六条路径，每条的 permit 都在结束时释放（用锁可获取性验证）。
- **已知陷阱**（仓库 AGENTS.md 记录过）：`Agent::run()` 会重置 cancel token；测试必须在 `run()` **之后**捕获 token，否则旧 token cancel 无效，测试会挂住。

**T15 出口**：`cd yi-agent-rs && cargo test -p yi-agent-app-server --lib plugin_binding && cargo test -p yi-agent-skills && cargo test -p yi-agent --test plugins_exec`；**检查筛选结果非零**（`plugin_binding` 过滤器若匹配 0 个测试，命令 exit 0 但什么都没验）。

---

## T17：实现生产资源加载与原生恢复容器

**前置：** T16、G0（已批准）。

**Files：** `desktop/src-tauri/src/plugin_host/{assets,cache,container,recovery,tests}.rs` 新增；修改 `plugin_host/mod.rs`、`src/lib.rs`、`src/bridge.rs`、`tauri.conf.json`、`capabilities/default.json`。

**G0 继承的教训（必须遵守）**：

1. 新增任何 Tauri 命令都要同时进 `build.rs` 的 `APP_COMMANDS` 与 `capabilities/default.json`，否则 `desktop/src-tauri/tests/app_command_capability.rs` 会红。
2. 探针攻击判据分三类：`not found` / `missing ... key` 是 INCONCLUSIVE（判失败），只有 `not allowed by ACL` 才算通过。
3. CSP 不能为 `null`，也不能用 `*` 修报错；只对主窗口已有合法资源需求放行。

### 循环 T17-a：缓存损坏被拒

- **测试**：篡改缓存文件一字节 → 加载返回 `Integrity`。
- **红**：仅比对文件名/大小则通过。

### 循环 T17-b：包外路径被拒

- **测试**：视图请求 `../../../../etc/passwd` → `PermissionDenied`；不泄漏文件内容。

### 循环 T17-c：跨包/版本 token 被拒

- **测试**：用 A 包的 view token 请求 B 包资源 → `StaleBinding`/`PermissionDenied`。

### 循环 T17-d：已撤销代次被拒

- **测试**：`activation` 已递增的旧 token → 拒绝。

### 循环 T17-e：symlink 文件替换被拒

- **测试**：包内正常文件替换为指向包外的 symlink → 拒绝。

### 循环 T17-f：Unicode 安装根可用

- **测试**：安装根含中文/空格/emoji → 资源正常加载。

### 循环 T17-g：探针改用生产模块

- **测试**：`cargo test --manifest-path "$R/desktop/src-tauri/Cargo.toml" --features plugin-probe` → 33 例仍通过；`containerConfigSha256` 与发布配置一致。
- **实现边界**：`probe::ContainerConfig` 必须引用生产 `container.rs` 的描述，不能保留副本。

### 循环 T17-h：正式 probe isolation suite

- **测试**：构建 release bundle 后跑 `node desktop/scripts/plugins/probe.mjs --suite isolation` → 12/12；负例（静态页面）12/12 失败。
- **注意**：**不能只跑 Rust MockRuntime 就声称原生 invoke 被拦截**（总计划 T17 Step 4 明写）。

**T17 出口**：`cargo test --manifest-path "$R/desktop/src-tauri/Cargo.toml" plugin_host`；`cargo test --test app_command_capability`；正式 probe 正/负例；`cd desktop && npx tsc --noEmit && npx vitest run`。

---

## T18：实现宿主桥与独立 SDK

**前置：** T17。

**Files：** `desktop/src/lib/pluginHost/{types,bridge,events}.ts` + 各自 `.test.ts`；`plugins/sdk/{package.json,package-lock.json,tsconfig.json,vitest.config.ts,src/index.ts,src/index.test.ts}`。

**SDK 约束**：包名 `@yi-agent/plugin-sdk`；`exports` 与 `types` 指向 build 产出的 dist；**不带**宿主 RPC client、**不 import** Tauri。

### 循环 T18-a：不信任插件自报身份

- **测试**：
  ```ts
  it('does not trust plugin-supplied identity', async () => {
    const reply = await bridge.receive(frameB, messageForFrameA);
    expect(reply.error.kind).toBe('PermissionDenied');
    expect(forwardToBackend).not.toHaveBeenCalled();
  });
  ```
  **必须用两个真实的、不同的 fake frame/source**，不能把身份校验 mock 掉再去断言它。
- **红**：只校验消息内自报 plugin id 时通过（错误）。

### 循环 T18-b：重放初始化被拒

- **测试**：同一 frame 第二次 `init` → `PermissionDenied`；nonce 复用 → 拒绝。

### 循环 T18-c：旧项目响应被丢弃

- **测试**：`grant` 递增后，仍在途的旧响应到达 → 丢弃，不投递给新页面。

### 循环 T18-d：重载后写不重放

- **测试**：页面重载 → 未确认的写请求不自动重发；`result(requestId)` 返回 `unknown`。

### 循环 T18-e：同 ID 改参数 → Conflict

- **测试**：与 T11-f 同语义，桥层也要拒（两层都要，不能只靠后端）。

### 循环 T18-f：超限/未知方法

- **测试**：超出并发/频率/消息体上限 → 明确错误；未知方法 → `NotSupported`。

### 循环 T18-g：事件缺修订先取快照

- **测试**：订阅时 `after` 不连续 → 先取快照再续订，不靠"丢了多少事件"推导。

### 循环 T18-h：订阅清理

- **测试**：卸载 frame / 项目切换 → 所有订阅被移除（用计数断言，不靠"没有报错"）。

### 循环 T18-i：SDK 构建与宿主源码隔离

- **测试**：SDK 的 `dist` 能独立 `npm run build`；`grep` 断言 dist 内不含 `@tauri-apps` 与宿主内部路径。

### 循环 T18-j：契约 golden 双端一致

- **测试**：checker 的 TS golden 与 `plugins/contracts/fixtures/bridge-v1.json` 对照一致。

**T18 出口**：
```bash
cd "$R" && (cd desktop && npx vitest run src/lib/pluginHost && npm run build) \
  && npm ci --prefix plugins/sdk && npm test --prefix plugins/sdk && npm run build --prefix plugins/sdk
```

---

## T19：用第二插件验证发现、导航、设置与授权 UX

**前置：** T18。

**Files：** 见总计划 T19。counter 后端是**独立 workspace**（`plugins/fixtures/example-counter/backend/`），用于开发/验收编译，不要求用户装 Rust。

### 循环 T19-a：counter 后端协议先绿

- **命令**：`cargo test --manifest-path "$R/plugins/fixtures/example-counter/backend/Cargo.toml"`
- **理由**：先证明协议与幂等，**避免正式 WebView 去请求一个不存在的后端**——那会得到一堆"连接失败"被误判成 UI bug。

### 循环 T19-b ~ T19-g（每个状态一条测试）

| 循环 | 断言 |
|---|---|
| T19-b | 未安装 / 已安装未启用 / 只读启用 / 运行故障 / 更新等待 / 待清理 **各自独立状态**，不用一个 `enabled` 布尔 |
| T19-c | 安装按钮**不**隐式开常驻 |
| T19-d | 清单增加第二设置入口，宿主不改注册代码即显示 |
| T19-e | 安装前只渲染静态介绍，业务 iframe **未创建** |
| T19-f | 分项授权：数据 / 自动任务 / 配额 / 常驻 四项独立勾选 |
| T19-g | 停用后仍有宿主管理入口，但业务后端不可调用 |

### 循环 T19-h：真实 WebView 加载

- **测试**：`cd desktop && npx vitest run src/components/PluginView.test.tsx src/components/PluginCatalog.test.tsx src/hooks/usePlugins.test.ts` 后，正式 WebView 加载 counter 主视图与设置页。
- **注意**：单测不能代替真实 WebView 加载（总计划 T19 Step 4）。

**T19 出口**：
```bash
cd "$R/desktop" && npx vitest run src/components/PluginView.test.tsx \
  src/components/PluginCatalog.test.tsx src/hooks/usePlugins.test.ts \
  src/components/SettingsPluginsTab.test.tsx && npm run build
```

---

## T20：接入远程精确版本资源与多客户端撤销

**前置：** T19。

**Files：** `desktop/src-tauri/src/plugin_host/remote_assets.rs`、`desktop/src/lib/pluginHost/remote.test.ts`、`yi-agent-app-server/src/server/plugin_remote_tests.rs` 新增；修改 `bridge.rs`、`remote/mod.rs`、`plugin_host/cache.rs`、`plugin_manager/{rpc,runtime}.rs`。

### 循环 T20-a：两宿主不串版本

- **测试**：宿主 A/B 同插件不同版本；切到 B **不能**展示 A 的 UI。
- **实现边界**：缓存键含 `(host_id, plugin, version, sha256, activation)`；静态字节可去重，**运行通道不可共享**。

### 循环 T20-b：远端平台二进制绝不在桌面 spawn

- **测试**：远端返回平台二进制描述 → 桌面拒绝 spawn（用 spy 断言未调用）。

### 循环 T20-c：旧客户端不兼容只禁该视图

- **测试**：桌面 UI API 不兼容 → 该视图不可加载并提示升级；执行宿主上已有合法任务继续。

### 循环 T20-d：headless 更新可用

- **测试**：无桌面连接时完成安装/更新。

### 循环 T20-e：离线有缓存可恢复

- **测试**：断网 + 匹配已验证缓存 → 原授权视图可加载。

### 循环 T20-f：离线无缓存明确不可加载

- **测试**：断网 + 无缓存 → 明确错误，**不加载任意旧资源**。

### 循环 T20-g：断线写查结果不重提交

- **测试**：断线时未确认的写 → `result()` 查询；不重新提交。

### 循环 T20-h：两个真实隔离 app-server

- **测试**：`cd yi-agent-rs && cargo test -p yi-agent-app-server --lib plugin_remote`；两个**真实隔离** app-server 连接完成远程验收。
- **禁止**：把两个对象命名成"两宿主"就当作验收（总计划 T20 Step 4 明写）。

**T20 出口**：`cargo test -p yi-agent-app-server --lib plugin_remote` + `cd desktop && npx vitest run src/lib/pluginHost/remote.test.ts` + 真实双 app-server 验收。

---

## 4b. 已验证的环境陷阱

`just ci` 在**当前 agent shell** 里会有一个假失败：

```text
yi-agent-tools shell::bash::tests::bash_without_a_session_env_leaves_the_variable_unset
  no session env means an empty expansion: [thread-c652d41f-...]
```

原因不是代码缺陷：该测试断言 `YI_AGENT_THREAD_ID` 未设置，而 agent 会话把
它注入了 shell。用干净环境复跑即绿：

```bash
cd "$R/yi-agent-rs"
env -u YI_AGENT_THREAD_ID -u YI_AGENT_SESSION_ID -u SUPERPOWERS_KANBAN_CARD \
  -u YI_AGENT_PLUGIN_INSTANCE -u YI_AGENT_PLUGIN_BINDING \
  CARGO_BUILD_JOBS=2 just ci
```

**不要**因此改那个测试或加 `#[ignore]`：它保护的是"无会话时不得凭空注入
线程 ID"，那是真实行为。执行 T06 起的所有 CI 门禁都用上面这条命令。

## 5. 跨任务门禁

每个任务的提交前都必须：

1. `cargo fmt --all`（对应 workspace）；
2. `just ci` 在 `yi-agent-rs` 全绿；
3. 前端改动另跑 `npm ci && npm run build && npm test`；
4. 桌面 Rust 改动另跑 `cargo test`（含 `app_command_capability`）与 `cargo fmt --all`;
5. 同一 commit 更新 `docs/project-management/` 对应模块与索引计数；
6. 只 `git add` 本任务文件与文档。

## 6. 阻塞点（必须先定，否则标 BLOCKED）

| 阻塞点 | 谁定 | 不定会怎样 |
|---|---|---|
| 维护请求/确认报文与 IPC 兼容策略 | G2 | T14-a~T14-j 全部无法开始 |
| 事务阶段、锁顺序、崩溃恢复表 | G2 | T10 的"先维护事务再授权"无法实现 |
| 看板 CLI/method → effect 映射表 | G3 | T15-h 的同名 skill 冲突与 T19 的第二插件对照会缺参照 |
| `namespace/verb` 是否够用（子 spec §14.1） | G1 审阅 | T06 的 method 名解析正则要返工 |
| 破坏性动词表是否入协议（子 spec §14.2） | G1 审阅 | checker 与 T06 的校验规则可能冲突 |

前三项属于 G2/G3，本文只列出不代决。后两项在 G1 审阅时一并确认。
