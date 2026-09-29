# bash 工具进程组隔离与异常路径整组回收设计

**目标：** 让 `BashTool` spawn 的 shell 自成**独立进程组**，从而：

1. **隔离**：工具内命令再怎么杀进程组，都伤不到 yi-agent 自身。
2. **异常路径整组回收**：**超时**与**被取消**时，回收该调用启动的整个进程组，
   不再留下孤儿空转。
3. **保留跨调用长驻**：**正常结束**时不打扰后台进程，`nohup ... &` 等可跨调用存活。

**状态：** 设计已确认（A 方案），**已实现**（分支 `fix/bash-tool-process-group`，提交 `feat(tools): run bash tool children in their own process group` → `test(tools): cover end-to-end process-group reclamation for bash`）。

实现相对设计的一处补充：**`libc::SIGKILL` 在 Windows 目标上不存在**（libc 的 Windows 后端未定义该常量），若策略层直接写 `libc::SIGKILL` 会让非 unix 构建失败，与 §6「non-unix 为 no-op」冲突。故机制层多导出一个 `pub(crate) const SIGKILL`（unix = `libc::SIGKILL`，其余平台 = 占位值 9），调用点只引用该常量；非 unix 下信号函数整体 no-op，占位值不会被用到。

**范围：** 只改 `yi-agent-tools`（`shell/bash.rs` 行为 + 新增 `process_group.rs`
机制；`process/manager.rs` 仅改为复用共享机制，**行为不变**）。

**不做：** 不改 managed process 工具的生命周期语义；不改 sandbox 策略内容；
不改 Linux bwrap 路径（其 `--new-session` 已提供隔离）；**不在正常退出时回收后台进程**。

---

## 1. 问题（事故复盘）

两条真实故障，同一根因：

1. **高 CPU 泄漏**：探测/压测命令用
   `seq 1 4 | xargs -P4 -I{} sh -c 'while :; do :; done'` 制造空转负载。
   `BashTool` 超时只对**直接子进程**调 `child.kill()`
   （`shell/bash.rs:296`、`:304`），后台子孙不在其内 → 孤儿化继续空转，
   CPU 不回落。

2. **yi-agent 自杀**：某次调查脚本执行 `kill -TERM -"$PGID"`（杀整个进程组），
   而 `BashTool` spawn 的子进程与 yi-agent **同一进程组**（无隔离），
   `$PGID` 即 yi-agent 自身进程组 → yi-agent 被自己杀掉。

根因统一为：**`BashTool` 未对 spawn 的子进程做进程组隔离**。

### 已核实证据

- `shell/bash.rs:136-146` spawn 链只有 `.kill_on_drop(true)`，无 `setpgid`/
  `process_group`；`rg 'process_group|setpgid|pre_exec|killpg|setsid' shell/` 为空。
- 实测：工具 spawn 的 `sh` 及其 `sleep` 子进程 pgid == yi-agent 自身 pid（同组）。
- 实测（阳性/阴性对照）：复刻 `kill -TERM -$PGID` → 调用方以 SIGTERM(-15) 终止；
  仅把 `-$PGID` 改为 `$CHILD` → 调用方存活。唯一变量是负号。
- 实测：`killpg(SIGKILL)` 后组长变 `Z`(zombie)，**`wait()` 回收后整组清空**。
- 实测：只杀子 pid，后台 `while :` 子孙存活（当前 bug 根因）。
- 实测：`sandbox-exec` 下 `setpgid(0,0)` **被允许**（rc=0，组号生效）。
- 实测：后台进程攥管道时，工具由 `idle_limit` **有界返回**
  （`bash.rs:344`），不会挂死 → 跨调用长驻可用。
- 实测：`nohup ... &` **不会脱离进程组**（`nohup` 忽略 SIGHUP，但不调用
  `setsid`/`setpgid`）。因此 `nohup` 起的后台进程仍留在调用进程组内，
  超时/取消时的整组回收**会**把它一起收掉 —— 这正是 §5 边界的由来。

---

## 2. 架构：机制与策略分离

新增 `yi-agent-rs/crates/yi-agent-tools/src/process_group.rs`，**只放机制**：

- `configure_process_group(cmd: &mut Command)` —— 把子进程放入新组。
- `signal_process_group(pgid: u32, sig: i32)` —— 向某进程组发信号。

策略留在**各调用点**（这是本设计的关键约束）：

| 调用点 | 信号策略 | 顺序 |
|---|---|---|
| `shell/bash.rs` | **超时 / 取消** → SIGKILL 整组 + reap；**正常结束 → 不动后台** | 先杀组，再 `wait()` |
| `process/manager.rs` | **维持现状**（组 SIGTERM + 子 SIGKILL） | 不改行为 |

> 依据：`manager.rs` 内部两处 kill 顺序本就相反
> （`450` start_kill→`455` kill_process_group；`790` kill_process_group→
> `792` start_kill），策略不可强行统一。

---

## 3. 隔离实现

在 `shell/bash.rs` spawn 链加入进程组隔离。优先用 tokio 原生安全 API：

```rust
Command::new(program)
    .args(command_args)
    .current_dir(&cwd)
    .process_group(0)        // tokio 1.53.0，cfg(unix)
    .kill_on_drop(true)
    ...
```

`process_group(0)` 等价于子进程内 `setpgid(0, 0)`（子进程自任组长）。
已在 `tokio 1.53.0`（workspace 实际锁定版本）确认存在，且为安全 API。

隔离后，子进程组 pgid = 子进程 pid，可由 `child.id()`（`Option<u32>`）取得。
取不到时（非 unix / 已退出）退化为 `child.kill()` 单杀。

---

## 4. 回收实现（仅异常路径）

```
spawn(...).process_group(0)
  let pgid = child.id();
  let mut guard = ProcessGroupGuard::new(pgid);   // Drop 时 signal_process_group(SIGKILL)

select 循环:
  ├─ 超时 (296/304)   -> signal_process_group(pgid, SIGKILL); child.wait().await
  │                      guard.disarm();
  ├─ 正常 wait 返回    -> 【不杀组】guard.disarm();            // 保留后台进程（跨调用长驻）
  └─ 任务被 abort/drop -> guard 在 Drop 中 signal_process_group(pgid, SIGKILL)
末尾 reap: 超时路径沿用已有 child.wait().await（bash.rs:326-328）
```

要点：

- **pgid 必须在 spawn 之后、任何 `wait()` 之前立刻抓取**。
  `Child::id()` 一旦子进程被 poll 完成（`FusedChild::Done`）就返回 `None`
  （tokio 1.53.0 `process/mod.rs:1216-1227`，文档明确 *"Once the child has been
  polled to completion this will return `None`"*）。因此必须写成：

  ```rust
  let mut child = cmd.spawn()?;              // 135-143
  let pgid = child.id();                     // <- 立即取，值 = 子进程 pid(pgid)
  let mut guard = ProcessGroupGuard::new(pgid);
  // ... select 循环 ...
  ```

  若拖到 `child.wait()`（`bash.rs:310`）之后才取，正常退出路径将拿不到 pgid，
  整组回收失效。

- **`kill_on_drop(true)` 只杀直接子进程，不覆盖子孙**
  （tokio `process/mod.rs:1114-1133`：`ChildDropGuard::drop` 仅调 `self.kill()`，
  即对单个 pid 发信号）。所以取消路径必须靠**新增的 guard** 杀整组，否则
  子孙仍会孤儿化泄漏。

- **正常退出路径必须显式 `guard.disarm()`**：否则函数返回时 guard 被 Drop，
  会把本应存活的后台进程杀掉 —— 这会直接破坏跨调用长驻。
- 超时路径：把 `child.kill()` 换成**整组 SIGKILL**，再 `child.wait()` reap，
  然后 `disarm()`（已回收，避免重复）。
- 取消路径（future 被 drop）：guard 兜底杀整组，即对
  `kill_on_drop(true)` 的语义加强（见上）。
- pgid 取不到时（非 unix）：guard 为 no-op，沿用 `kill_on_drop(true)` 既有行为。

---

## 5. 语义保持与时序边界

**正常结束时，调用启动的后台进程继续存活（跨调用长驻）**，例如：

```
调用 #1:  nohup ./server.py > /tmp/server.log 2>&1 &   # sh 立即退出 → 正常结束
调用 #2:  curl -s localhost:8000/health                # server 仍在 → 连通
```

这是本方案**有意保留**的既有可用行为。

**边界（澄清主语）：** 被回收的对象是「**这一次 bash 调用所启动的长驻进程**」；
触发条件是「**这一次调用本身超时或被取消**」。也就是说，长驻进程并非自身出了问题，
而是它与那次异常结束的调用同属一个进程组，因此被一并回收。

**边界：异常终止会回收整组。** 若某次调用**超时**或**被取消**，且它同时启动了
长驻进程，则这些进程会随整组一起被回收。实测依据：`nohup ... &` **不脱离进程组**，
所以它挡不住整组回收。理由：

- 超时/取消意味着该调用未按预期完成，留下无人管理的半启动状态更危险；
- 这也是修掉 CPU 泄漏的必要条件（当前只有单杀，子孙必漏）。

若要「即便异常终止也保命」的长驻服务，请使用 managed process 工具
（`ProcessStartTool` / `OnExitPolicy::Keep`）。

**文档同步：** 在 `BashTool` 的 `description()` 中写明：超时或被取消时，
该命令启动的后台进程会被一并回收；需要不受此影响的长驻服务请用 managed
process 工具。

---

## 6. 错误处理

- 对已消失的进程组发信号 → `ESRCH`，属正常，**尽力而为、忽略**。
- 杀组后**必须 `wait()`** reap，否则留 zombie（实测：killpg 后组长为 `Z`）。
- non-unix：隔离与整组回收均为 no-op，行为与今天一致。

---

## 7. 测试

新增（`shell/bash.rs` 内联或 `tests/bash_stream.rs`）：

1. **进程组隔离**：spawn 的子进程 pgid ≠ 本进程 pgid（且 == 自身 pid）。
2. **正常结束保留后台进程（跨调用长驻）**：`sleep 30 & exit 0` 返回后，
   那个后台 `sleep` **仍然存活**（这是 A 方案的核心断言，与 B 相反）。
3. **超时整组回收**：`xargs -P4 -I{} sh -c 'while :; do :; done'` 超时后组内无残留。
4. **取消整组回收**：运行中 abort 后，组内子进程与后台子孙都不残留。
5. **自杀防护（本次事故回归）**：命令内执行 `kill -TERM -$PGID` 后，**调用方仍存活**。

回归（必须继续通过）：
- `bash_timeout_kills`
- `dropping_bash_call_stops_the_child_process`（取消路径，现为整组回收）
- `bash_orphan_subprocess_does_not_hang_call_stream`（`sleep 30 & exit 0`）
  —— 正常结束不杀组，管道仍由 `idle_limit` 兜底，应返回 `exit 0` 成功。

测试清理约定：测试里启动的后台 `sleep` 需在用例结束时显式清理，避免遗留。

---

## 8. 风险与已知边界

- **macOS `sandbox-exec`**：已实测 `setpgid` 被允许，风险已排除。
- **Linux bwrap**：已有 `--new-session` + `--die-with-parent`，无需改动；
  本设计在 Linux 上的 `process_group(0)` 属冗余但无害（未在 Linux 实跑，标注为推断）。
- **Windows/non-unix**：no-op，与现状一致。
- **行为变更面**：仅「超时/取消时会额外回收子孙」；正常结束行为**不变**。

---

## 9. 改动清单（预估）

| 文件 | 改动 |
|---|---|
| `src/process_group.rs` | **新增**：两个机制函数 + `SIGKILL` 常量 + non-unix 空实现（约 35 行） |
| `src/shell/bash.rs` | spawn 加 `.process_group(0)`；超时/取消路径整组 SIGKILL；Drop guard（正常路径 disarm）；更新 description；新增测试 |
| `src/process/manager.rs` | 删除本地两个 helper，改 `use crate::process_group::…`（**行为不变**） |
| `src/lib.rs` | 加 `mod process_group;`（约 1 行） |

`libc = "0.2"` 已在 `yi-agent-tools` 依赖中，无需新增依赖。
