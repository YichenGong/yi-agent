# 悬空 daemon 自停 + attach 入口拦截 设计

**日期**: 2026-10-18
**状态**: 待实现
**相关代码**: `yi-agent-store`（`ipc.rs`）、`yi-agent-app-server`（`server.rs`）

## 1. 背景与根因

### 1.1 现象

一次真实事故的取证结果：

- `~/.yi-agent/trace/session-20261018-050932.jsonl` 单个 trace 文件里 `unable to open database file` 出现 **71,651 条**，对应 **71,623 个不同 `request_id`**（平均每个请求只失败一次，说明重试来自**外部调用方**而非 daemon 内部循环）。
- 错误 `target` = `yi_agent_store::ipc`，`request_id` 前缀 `request-<pid>-*`，指向前台 app-server 进程内嵌的 daemon。
- 全部 trace 合计约 113 万条该错误，最早可追溯到 2026-10-03。
- 该现象已被 `.superpowers/review-2026-10-17/SUMMARY.md` 的 P0-11 与 `13b-MAIN-crosscut-supplement.md` §E 现场记录过（PID 32154、~210/min、10.5 小时、30,441 条），根因一致。

### 1.2 触发链（自杀式因果，已取证）

1. kanban 卡片在 `.worktrees/kanban/<slug>/` 各建一个 worktree，各有独立的项目 daemon，数据库在 `<worktree>/.yi-agent/runtime/runtime.sqlite`。
2. 某个 agent（**通过 bash 工具执行裸 `git worktree remove --force`**）删掉了整个 worktree 目录，`.yi-agent/runtime/` 随之消失。
3. 时间证据：`05:24:48.370` 该删除命令开始执行；`05:24:48.760` **第一条** `unable to open database file` 出现——**相隔 0.39 秒**，路径与该 worktree 完全一致。
4. 执行删除的那个 agent 就住在 app-server PID 40987 里，而 40987 同时托管着该项目的 daemon。**它删掉了自己脚下的目录。**

### 1.3 为什么 daemon 会"悬空存活"

`socket_path_for`（`yi-agent-store/src/ipc.rs:1095`）在 `<runtime_dir>/runtime.sock` 超过 `MAX_SOCKET_PATH_BYTES = 103`（`ipc.rs:1046`）时，把 socket 回退到 `$TMPDIR/yi-agent-<sha256[:16]>.sock`。

kanban 的 worktree 路径都超限，于是 **socket 留在 `$TMPDIR`、数据库留在 worktree**：目录一删，daemon 的连接端点还在，进程不死。

实测：`<runtime_dir>` 的 `sha256[:16] = 491ac572b2e1354b` 与 `$TMPDIR/yi-agent-491ac572b2e1354b.sock` 精确匹配；`lsof` 显示该 app-server 同时持有多个 kanban 的 `runtime.sqlite` fd 与这些 tmp sock。

### 1.4 为什么系统不自愈（结构性缺陷）

1. **`ProjectRuntimes` 缓存只插不删**：`server.rs:147` 定义为 `HashMap<PathBuf, Result<Arc<RuntimeBinding>, String>>`，全文件**没有一处 `.remove()`**；注释还明确"失败也缓存"。→ 只有进程重启才清。
2. **`detach_unused_runtimes` 只有一个调用点**：`server.rs:877` 定义，唯一调用在 `rpc/thread.rs:1546`（`thread/delete` 分支）。而 kanban worktree 是被裸 `git worktree remove` 删的，**根本没走 `thread/delete`**，所以从未 detach。
3. **worktree 回收不碰 daemon**：宿主 `worktree_reclaim.rs::remove()` 与插件 `merge.rs:187 cleanup()` 都只删 git worktree，不 detach daemon、不清 `workspaces.json` / `resident-daemons.json` 登记。
4. **现有自愈的触发条件与故障形态错配**（本次事故能隐身 10 小时的关键）：
   - `RuntimeBinding::send_with_generation`（`binding.rs`）对 `is_liveness_error` 会自动 repair；
   - 但 `is_liveness_error`（`binding.rs:267-283`）**只认连接层失败**（`IpcError::Io(kind ∈ {NotFound, ConnectionRefused, ...})`）与 `TruncatedFrame`，其余一律 `false`；
   - 而 `unable to open database file` 是 daemon **正常应答**里的 `IpcResponse::Error { code }`——**连接是好的、socket 还在、那头还有人说话**；
   - 于是 `is_liveness_error == false` → binding 不修复；`probe_runtime` 也因 `Status` 能得到应答而返回 `Healthy` → `attach_cwd_runtime` 继续用缓存的 binding。
   - **结论**：系统里所有"自愈"都建立在"socket 死了"这个信号上，而本故障恰恰让 socket 活着。不是没人写自愈，是自愈的触发条件与故障形态精确错配。

补充事实（影响实现方式）：commit `b3c6c6f5` 后 `respond` 改用 `RequestContext` 长连接（`ipc.rs:3158` 起），unix 语义下目录被 unlink 后**已打开的 fd 仍有效**，所以主请求路径在**新代码**下可能不再每请求失败；但订阅 producer 循环（`ipc.rs:1828` / `1931`）与其它 `RuntimeRepository::open` 落点仍会失败，且缓存不摘、回收不 detach 的结构性缺陷不变。**本设计针对的是生命周期缺陷本身，不是某一条 open 路径。**

### 1.5 多周期源

同一坏 runtime 被至少四个周期源反复触达：看板 prefetch（1s）、`watch_children`（500ms）、看板 tick（3s）、supervisor（500ms）。看板 prefetch 虽有 `PREFETCH_FAILURE_LIMIT = 5`（`server.rs:1529`），但它只停止缓存卡片，**不停请求**。

## 2. 目标与非目标

### 2.1 目标

1. daemon 发现**自己在服务的 runtime 目录已不存在**时，主动停止（打真实原因的 WARN，然后退出），把 socket 变成真的死的，从而让**既有**自愈链路自然接管。
2. app-server 在 attach 入口拒绝为**已不存在的项目目录** bring-up，且在唯一入口一次性挡住所有周期源——同时**不复活**已被删除的目录。
3. 让"项目目录已消失"这类不可自愈错误在**默认日志级别**可见（而不是只藏在 trace 里）。

### 2.2 非目标（YAGNI）

- **不做**"回收走生命周期入口"（改 `worktree_reclaim::remove()` 与插件 `merge.rs:cleanup()` 去 detach + 清登记）。理由：裸 `git worktree remove` 是完全合法的 git 命令，宿主无法给它装生命周期钩子；这次的删除者正是裸 bash 命令。它撞上"B6: worktree 回收无卡片生命周期入口"这个独立缺口，应另行立项。
- **不做**启动期对账（`git worktree list` vs `workspaces.json` / `resident-daemons.json`）。已有相邻设计 `2026-10-01-desktop-runtime-self-heal-design.md`，放那里更顺。
- **不做**新增错误码（如 `TargetGone`）暴露给客户端。
- **不推翻** `log_request_failure`（`ipc.rs:2847`）的通用策略。它的注释写明是刻意的安全决策——`error_response` 故意剥掉原因，"让 wire 契约永不泄露数据库、环境或实现细节"；推翻它需要独立论证。
- **不碰** wire 契约、协议文档、前端代码。

## 3. 设计

### 3.1 职责划分

一句话：**app-server 管"要不要 bring-up"，daemon 管"已经在跑的这个还该不该活"。**

判据同源（目录存在性）但职责不重叠，因此**不抽共享抽象**——现在只有两个调用点，抽早了只是多一层间接。

```
worktree 目录被删
   │
   ├─► daemon 侧：自己在服务的 runtime_dir 没了
   │        └─► 打 WARN（真实原因）─► 主动停止 ─► socket 变死
   │                                                │
   └─► 下一次请求 ─► probe 得 Dead ─► binding.repair()
                                        └─► ensure_owned_runtime ─► attach_cwd_runtime 拦截
                                              cwd 不存在 ─► 摘缓存 + 明确报错 + 不建目录
```

分工细节：

| 谁 | 判据 | 动作 |
|---|---|---|
| daemon | `runtime_dir`（数据库所在目录）不存在 | WARN + 停止 + 清理 socket/lock |
| app-server | `cfg.workdir`（项目根）不存在 | 摘缓存 + 返回明确错误，**不** `create_dir_all` |

只删 `runtime/` 而项目根还在时：daemon 自停，但 app-server 允许重新 attach → 正常重建 `runtime/`（与首次 attach 行为一致，是对的）。

### 3.2 daemon 自停

**落点**：`yi-agent-store/src/ipc.rs` 的 accept 循环（`thread::spawn` 起始于 `:886`，循环体在 `while !thread_stop.load(...)` 内，清理在 `:988`）。

- 循环已有 ≤10ms 节拍（`DAEMON_ACCEPT_POLL = 10ms`，`ipc.rs:1510`）。新增一个**1 秒节流**的存在性检查：统计 `runtime_dir` 是否仍存在。
  - 为什么 1 秒：事故速率约 3 次/秒，1 秒检测已绰绰有余；每 10ms 一次 `stat` 是无谓开销。
- 检测到缺失 → `tracing::warn!`，消息带**真实原因**（目录路径），落在默认日志级别（不是 trace-only）→ `thread_stop.store(true, Ordering::Release)` → `break`。
- **复用现有清理**：循环尾部已有 `remove_if_exists(&cleanup_socket)` / `remove_if_exists(&cleanup_lock)`（`:988` 附近），`break` 后自然执行 → socket 与 lock 被清 → 下一个请求 `connect` 得到 `ConnectionRefused` → 既有 `is_liveness_error`（`binding.rs:267`）判真 → `repair` → 恢复链路自动接管。**零新增恢复代码。**
- **不加**指数退避。"目录缺失"是二值、明确、不可自愈的条件，给它配退避等于给"这部分代码已经死了"配一个倒计时；恢复由 app-server 侧的 re-attach 自动完成。
- `log_request_failure`（`ipc.rs:2847`）与 `error_response` 的行为**保持不变**。

**socket 身份护栏（防递归竞态）**：循环尾部的清理是按路径无条件 unlink 的。一旦"目录被删 → 又被重建（kanban 重试正是同路径重建）→ 新 daemon 起来"的窗口撞上老 daemon 的清理，老 daemon 会删掉**新 daemon** 的 socket，造出第二个悬空 daemon——同一 bug 的递归。今天这条竞态几乎不可达（要精确到微秒），但本改动把它变成可达（目录删除会主动触发这条清理路径）。

护栏：`bind` 之后记下 socket 节点的 `(dev, ino)`（`std::os::unix::fs::MetadataExt`），清理时先 `symlink_metadata` 再比对，**只删身份仍然相同**的那个节点。对既有 `stop()` 路径是严格更安全。

### 3.3 attach 入口拦截

**落点**：`yi-agent-app-server/src/server.rs:844` `attach_cwd_runtime`。

- 在函数**最前面**（查缓存之前）：若 `cfg.workdir` 不存在 →
  1. 若缓存里已有该 key 的条目，先 `remove` 掉；
  2. 返回明确错误，如 `format!("the project directory {} no longer exists", cfg.workdir.display())`。
- **必须在查缓存之前、且必须摘缓存**：现有语义是"失败也缓存，免得反复重跑注定失败的 bring-up"（`server.rs:144-146` 注释）。若把"cwd 缺失"也缓存进去，目录**被重建后**（kanban 重试）缓存里的 `Err` 会永久挡住恢复——把可恢复状态锁死。先判存在、再摘缓存，则目录回来时自然放行。
- 效果：所有高频周期源在**唯一入口**一次性被挡住，且不会 `create_dir_all` 复活已删目录。

**为什么不用 daemon 的 `Status` 应答判断**：长连接在目录删除后仍有效，`Status` 会正常应答（`ipc.rs:3249-3253` 走 `with_repository`），判据不可靠。存在性可以直接问文件系统。

## 4. 测试策略

三层，全部在**单进程内自足**，不依赖桌面 app 或真实看板。

**测试环境前提（关键，否则假通过）**：事故的结构之一是 **socket 不在 runtime 目录里**。`socket_path_for` 只在 `<runtime_dir>/runtime.sock` 超过 `MAX_SOCKET_PATH_BYTES = 103` 时才回退到 `$TMPDIR`。本机 `tempfile::TempDir` 根只有约 61 字符，`<TempDir>/runtime/runtime.sock` 仅约 81 字符 —— socket 会留在 runtime 目录内，`remove_dir_all` 连带删掉它，于是"目录没了 → socket 也没了"的断言**在 daemon 完全没自停时也会通过**。

因此相关测试的 runtime 目录必须深到强制回退（在 `TempDir` 下用一个约 70 字符的长名子目录，实测使 `<runtime_dir>/runtime.sock` 达 161 字符），并**先断言 socket 确实在 runtime 目录之外**作为 precondition。这既忠实复刻事故结构，也是防"日后路径规则变化导致测试静默变成假通过"的护栏。

1. **daemon 自停**（`yi-agent-store/tests/runtime_ipc.rs`，与既有 `manually_stopped_daemon_releases_its_socket_and_instance_lock:2837` 同区）：深路径起 daemon → 删掉 runtime 目录 → 有界等待 → 断言 socket 节点消失、且**不重建目录**。
2. **socket 身份护栏**：对纯谓词 `remove_socket_if_same_inode(path, expected)` 做确定性单测——身份相同则删、同路径换节点（`dev, ino` 不同）则不删、身份未知则不删、节点已不在则安静通过。**不用行为测试**：该竞态要求"老 daemon 的清理恰好落在新 daemon bind 之后"，无法确定性复现，行为测试只能靠 `sleep` 赌时序、必然 flaky。正常停止路径"删掉自己的 socket"由既有 `manually_stopped_daemon_releases_its_socket_and_instance_lock` 覆盖。
3. **attach 拦截**（`yi-agent-app-server/src/rpc_tests.rs`，与既有 `attach_cwd_runtime` 测试区约 `:895` 起同区）：
   - cwd 缺失 → 返回 `Err`、**未建目录**、缓存无残留；
   - **重建 cwd** → 下一次调用能正常 attach（证明没被缓存锁死）。
4. **事故结构复现**（app-server 侧，因为同时依赖 store + subagent）：深路径下 `runtime_dir == <workdir>/.yi-agent/runtime`（与事故一致）→ bring-up → 删项目根 → 断言 (a) `attach_cwd_runtime` 报错且不复活目录、(b) daemon 自停（socket 消失）、(c) 反复调用持续被拒且**不再无限重复**（钉住"收敛"，而非只钉谓词）。

## 5. 落点清单

| 文件 | 位置 | 改动 |
|---|---|---|
| `yi-agent-rs/crates/yi-agent-store/src/ipc.rs` | `start_with_factory`（`:838` 起）、accept 循环（`:886`–`:988`） | 目录存在性节流检查 + WARN + `stop` + `break`；socket 身份护栏 |
| `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` | `attach_cwd_runtime`（`:844`） | cwd 存在性拦截 + 摘缓存 |
| `yi-agent-rs/crates/yi-agent-store/tests/runtime_ipc.rs` | 与 `:2837` 同区 | 自停测试、身份护栏测试 |
| `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs` | 与 `:895` 同区 | attach 拦截测试、事故结构复现 |
| `docs/bug-list.md` | 未修复条目区 | 补登该 bug（含证据） |

## 6. 风险

- **清理时机**：`break` 后需要 join 已有 client handler（既有代码已做），否则会与"释放 runtime 文件"竞态。本设计不改这段顺序。
- **测试时序**：自停检测是 1 秒节流，测试必须有界等待（参考既有 `daemon_stop_request_releases_the_runtime_for_a_future_manual_start:2859` 的 50×10ms 轮询写法），不要用固定 sleep 断言。
- **不重建目录的断言**：`TempDir` 析构会删临时目录，断言要落在"daemon 停止后"这个时点，且断言的是 daemon **没有**创建它（而不是它不存在）。