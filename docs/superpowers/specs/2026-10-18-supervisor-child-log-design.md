# 被托管子进程的日志落盘设计

**日期**: 2026-10-18
**状态**: 待实现
**相关代码**: `yi-agent-supervisors`（`supervisor.rs`）、`plugins/superpowers-kanban/INSTALL.md`

## 1. 背景与问题

### 1.1 现象

插件（`superpowers-kanban`）**会**报告它的投递结果，但那些报告永远到不了任何人眼里。

`plugins/superpowers-kanban/crates/superpowers-kanban-runner/src/main.rs:973-977`：

```rust
for outcome in service.consume_inbox(chrono::Local::now()) {
    match outcome.result {
        Ok(()) => eprintln!("superpowers-kanban: enqueued {}", outcome.id),
        Err(reason) => eprintln!("superpowers-kanban: rejected {} ({reason})", outcome.id),
    }
}
```

`inbox.rs` 的策略注释也写明意图：「校验失败或损坏 → 移到 `inbox/rejected/` 并记录原因（**绝不静默丢弃、绝不 panic**）」。设计意图是好的。

**但 supervisor 起它时是这样**（`yi-agent-supervisors/src/supervisor.rs:227-233`）：

```rust
let child = Command::new(&manifest.command)
    .args(args)
    .current_dir(&self.layout.workdir)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())   // ← eprintln 全进 /dev/null
    .spawn()?;
```

那条 `rejected <id> (<原因>)` **写进了虚空**。插件以为自己在报告，声音到不了任何人耳朵里。唯一残留是磁盘上的 `inbox/rejected/<id>.json`——而用户不会去翻那个目录。

### 1.2 实测经历（本设计的第一手动因）

一次真实投递中：`superpowers-kanban add-prequeued` **返回成功**并打印了 `card-id`，但卡片没有出现在看板上，**没有任何错误信息**。事后才发现原因（旧进程用旧代码校验新格式），以及磁盘上躺着一个 `inbox/rejected/*.json`。

也就是说：**系统在报告，报告被丢弃；用户在等待，等待无回音。**

### 1.3 为什么这是"运维盲点"而非一次性故障

三层叠加才致命：

1. **升级不重启子进程**：`install` 覆盖二进制会创建新 inode，而 unix 语义下已运行进程继续持有旧 inode 的 fd。`supervisor.rs:177` `ensure_running` 的判据只有「`query_socket` 是否有人应答」——它不问跑的是不是新二进制。全仓验证：`yi-agent-supervisors/` 与 `yi-agent-boards/` 内搜 `mtime`/`inode`/`modified` 做二进制新鲜度比对，**零结果**。
2. **旧代码拒绝新格式**：旧进程的 `RawDelivery` 没有新字段（如 `title`），于是把预排队卡当实现卡校验 → 拒收。
3. **拒收报告被丢进 `/dev/null`**（本设计要修的那一层）。

单独看第 3 层只是"日志没地方去"；与 1、2 叠起来，就成了"升级后功能静默失效"。

### 1.4 范围声明

本设计**只修第 3 层**：让报告可见。第 1 层（二进制新鲜度检测）是独立需求，不在本范围——理由：最致命的不是"版本漂移"（那只是触发条件），而是"**系统在报告却没人听得见**"。修可见性优先于修触发概率，且改动更小、更独立。

## 2. 目标与非目标

### 2.1 目标

1. 被托管子进程的 **stderr** 落盘到项目日志文件，而不是丢弃。
2. supervisor 自己的**生命周期事件**（起、停、失败、退避重启）写进同一文件。
3. 日志基础故障（目录不存在、文件不可写）**绝不阻断**子进程启动。
4. 在 INSTALL.md 补上升级注意事项与日志位置，让下次升级的人不必重踩。

### 2.2 非目标（YAGNI）

- **不做**二进制新鲜度检测 / 自动重启已升级的进程（那是"方案 1"）。
- **不做**日志轮转或大小上限。项目内无轮转范式；实测 `~/.yi-agent/logs/board-watchman.log` 15 天 0 字节——这类常驻进程平时几乎不输出，只在故障时说话。真成问题那是独立需求。
- **不改** stdout 行为（保持 `Stdio::null()`）。核实依据：`run` 长驻路径全程用 `eprintln!`；唯一的 `println!`（`main.rs:820`）是**一次性命令**（`add`/`list`/`on`/`off` 等）的结果输出通道，不是长驻通道。
- **不改** wire 契约、协议文档、桌面前端。
- **不做** rejected 事件的桌面通知（那是"方案 3"）。
- **不给** `SupervisorManifest` 加日志字段：路径由 `manifest.name` 机械推导。

## 3. 设计

### 3.1 职责边界

| 谁 | 负责 | 不负责 |
|---|---|---|
| `yi-agent-supervisors` | 起停子进程；把子进程 stderr 与自身生命周期事件写进日志文件 | 不认识具体插件；不解析日志内容；不轮转 |
| 子进程（插件） | 用 stderr 报告 | 不决定日志位置 |
| 人 | 读 `<workdir>/.yi-agent/logs/<name>.log` | —— |

`lib.rs` 明确写「本 crate 不认识任何具体插件」，因此日志路径必须由清单里已有的通用字段（`name`）机械推导，不引入插件专属知识，也不新增清单 schema。

### 3.2 日志位置与打开方式

**路径**：`<workdir>/.yi-agent/logs/<manifest.name>.log`

- 由 `Layout`（`supervisor.rs:11`，已有 `workdir`）推导，不新增配置。
- 对齐 `watchman.rs:17-22` 的 `~/.yi-agent/logs/` 惯例（`board-watchman.log`），只是落到**项目级**——因为 supervisor 本就是项目级守护（它按 `<workdir>/.yi-agent/supervisors/*.json` 起进程）。
- `<workdir>/.yi-agent/` 已在 `.gitignore:30` 内，日志不会进 git。

**打开方式**：`OpenOptions::new().create(true).append(true).open(path)`

- **append 而非 truncate**：每次重启追加，保留上一次崩溃的遗言——"上一次为什么死"恰恰最需要。这与 `tracing_init.rs:29-31` 的既有写法一致。

**best-effort 硬约束**：

- 目录不存在 → `create_dir_all`；失败则回落 `Stdio::null()`，**不阻断 spawn**。
- 文件打不开（权限、磁盘满）→ 回落 `Stdio::null()`，**不阻断 spawn**。
- **守护子进程是本职，记日志是附加**。日志基础故障绝不能让插件起不来。

### 3.3 写什么

**子进程侧**：`Stdio::null()` → `Stdio::from(文件)`，**仅 stderr**。stdout 保持 `Stdio::null()`。

**supervisor 侧**（同一文件），覆盖以下事件：

| 事件 | 落点 | 内容 |
|---|---|---|
| spawn 成功 | `spawn_and_track`（`supervisor.rs:206`） | `started <name> (pid <pid>)` |
| spawn 失败 | `spawn_and_track`（`spawn` 返回 `Err`） | `could not start <name>: <io error>` |
| 子进程已退出、退避等待 | `ensure_running`（`:190` 一带） | `exited <name>; retrying in <backoff>`，**同一退出只记一次** |
| 因清单消失而停 | `reconcile` 的 stale 分支（`:100`） | `stopped <name> (manifest removed)` |
| 因开关关闭而停 | `reconcile` 的 `desired_on` 为假分支（`:110`） | `stopped <name> (switch off)` |
| `stop_all`（daemon 退出） | `stop_all`（`:246`） | `stopped <name> (supervisor shutting down)` |

**必须区分「清单消失」与「开关关闭」**：这两条在 `reconcile` 里走不同分支，混成一句会让排障时无法判断"谁停的"。

**时间戳**：每行前缀一个，格式 `YYYY-MM-DDTHH:MM:SS`（项目既有 trace 的时间戳风格）。这次事故的核心难点正是"什么时候开始的"，无时间戳的日志会重演同样的困难。

### 3.4 写入器

一个模块私有的小写入器：

```rust
/// 托管子进程的日志文件。写失败后静默降级为"不写"，避免每 tick 重复报错、
/// 也避免日志故障级联。
struct ChildLog {
    file: Option<File>,
}
```

- 方法 `write_line(&mut self, event: &str)`：加时间戳前缀、写一行、失败则置 `file = None`。
- 由 `RunningChild` 持有，`spawn` 时创建。
- **为什么"写失败后置 None"**：`reconcile` 每 500ms 一轮；若每轮都重试打不开的文件并报错，会把真正的问题淹没在噪声里。

**去重（实现期实证补充）**：`exited <name>` 与"写失败"是同一类问题——`reconcile` 每 500ms 跑一次，
而**同一个退出事实在退避窗口内会被观察到很多次**。退避 30s 时，一条"进程退出了"会被写成 60 行，
把真正的失败埋进它自己的噪声里。故 `RunningChild` 带 `logged_exit: bool`：退出只在**首次**观察时记一行，
新实例经 `spawn_and_track` 构造时自然复位。实证：去掉该守卫后，20 拍（≈1s，退避 2000ms）写出 **16 行**重复的
`exited demo`；带守卫时恰 1 行（`a_childs_exit_is_recorded_once_not_once_per_tick`）。

**时间戳实现**：给 `yi-agent-supervisors` 加 `chrono = { workspace = true }`（workspace 已 pin，`yi-agent-boards/src/registry.rs:62` 已在用）。**不**手抄日历算法——`tracing_init.rs:82-105` 那份手写实现是为了"避免引入额外依赖"，而本 crate 的 workspace 已 pin chrono、邻居已在用，重抄一遍没有收益。

### 3.5 数据流

```
daemon 起 → supervise::serve(workdir) → Supervisor::new(Layout)
                                              │
                              reconcile() 每 500ms
                                              │
                    ensure_running(manifest) ─┴─► spawn(manifest)
                                                     │
                                    日志文件: <workdir>/.yi-agent/logs/<name>.log
                                                     │
                                      ┌──────────────┴──────────────┐
                              子进程 stderr                    supervisor 事件
                            (rejected/enqueued…)
```

## 4. 测试策略

`yi-agent-supervisors/tests/supervisor.rs`，沿用该文件既有基础设施（fake child 脚本、marker 文件、`wait_until` 有界轮询、`SPAWN_LOCK` 串行化——后者是既有教训的产物：`tests/supervisor.rs:11-17` 记录过"8 个并发 spawn 让 marker 延迟超 3s"的 flaky）。

1. **子进程 stderr 真的到文件**：fake child 加 `printf 'child-stderr-marker\n' 1>&2`，断言标记出现在 `<workdir>/.yi-agent/logs/<name>.log`。
2. **supervisor 生命周期事件到文件**：断言同一文件含 `started <name>`。
3. **stop 原因可区分**：清单删除 → 含 `manifest removed`；开关关闭 → 含 `switch off`。
4. **stdout 仍被吞**：fake child 往 stdout 写标记，断言它**不在**日志里（证明没顺手把 stdout 也接进去）。
5. **best-effort 不阻断**：把 `<workdir>/.yi-agent/logs` 造成一个**文件**（而非目录）使日志不可写，断言子进程**仍然起来**（marker 出现）。
6. **同一退出只记一次**：长退避（2000ms）下 fake child 在一个约 1s 的观察窗口里被反复观察到已退出，断言 `exited <name>` 在文件里恰出现 **1 次**——不是每 tick 一次（`a_childs_exit_is_recorded_once_not_once_per_tick`，经负向验证）。
7. **无回归**：既有 6 条 supervisor 测试 + `yi-agent-runtime` 的 `serve` 测试全绿。

**测试纪律**：不用固定 sleep 断言；用 marker + `wait_until`。理由同 §4 开头。

## 5. 文档同步

`plugins/superpowers-kanban/INSTALL.md`，**只加不删**：

- **保留**第 20 行「daemon 每 500ms 对账一次 supervisor 目录，所以装完不需要重启 daemon」——它对"新装"场景是正确的。
- **新增**升级小节：`install` 覆盖二进制后，已在跑的 `run` 进程仍持有旧 inode 继续跑旧代码；需 kill 它，supervisor 会按 `restart_backoff_ms` 拉起新的。
- **新增**日志指引：插件报告（含被拒绝的投递）现在落在 `<项目>/.yi-agent/logs/superpowers-kanban.log`；并说明 `inbox/rejected/` 的含义。

## 6. 落点清单

| 文件 | 位置 | 改动 |
|---|---|---|
| `yi-agent-rs/crates/yi-agent-supervisors/src/supervisor.rs` | `spawn`（`:221`）、`spawn_and_track`（`:206`）、`ensure_running`（`:177`）、`reconcile`（`:91`）、`stop`（`:238`）、`stop_all`（`:246`） | 新增 `ChildLog`；stderr 重定向；生命周期事件写入 |
| `yi-agent-rs/crates/yi-agent-supervisors/Cargo.toml` | `[dependencies]` | 加 `chrono = { workspace = true }` |
| `yi-agent-rs/crates/yi-agent-supervisors/tests/supervisor.rs` | 既有测试区 | 新增 6 条集成用例（§4）+ 4 条 `child_log_tests` 单测 |
| `plugins/superpowers-kanban/INSTALL.md` | 升级/排障相关章节 | 新增升级小节与日志指引 |

## 7. 风险

- **日志写入阻塞**：`write_line` 是同步写。日志在故障时才会说话，量极小（实测同类日志 15 天 0 字节），不引入异步。
- **测试时序**：日志由子进程与 supervisor 两方写入，断言必须用 `wait_until` 轮询文件内容，不能假定"子进程一起来日志就到"。
- **best-effort 路径的测试**：用例 5 需要构造"日志不可写"而不误伤 spawn——用"`logs` 是个文件"而非"改权限"（后者在 root/CI 下可能不生效）。
- **`ChildLog` 与 `Child` 的所有权**：`ChildLog` 随 `RunningChild` 存亡；`stop` 时先写 `stopped` 事件**再** kill，否则子进程的临终输出可能排在被 kill 之后丢失。