# 看板 daemon socket 契约修复 — 验收记录（2026-11-09）

关联：[设计 spec](../specs/2026-11-09-kanban-daemon-socket-contract-design.md) ·
[实施计划](../plans/2026-11-09-kanban-daemon-socket-contract-impl.md)

分支 `fix/kanban-daemon-socket`，实现 worktree
`.worktrees/fix-kanban-daemon-socket`。

**结论先说：自动矩阵全绿；隔离真机做了「真 daemon + 真插件二进制 + 独立实例家
目录」这一段；桌面 app 整包与经真宿主转发的查询转发没有在本轮跑，正式插件仍未
安装/重启。**

状态口径只有三种：`pass` / `fail` / `not_run`。`not_run` 不等于通过，也不等于失败
——它表示本轮没有执行，不能据此宣称已恢复。

---

## 1. 根因（一句话）

宿主 `cac5afe0` 把业务 daemon 的 socket 从「runtime 目录旁的 `runtime.sock`」搬到
`<实例家目录>/sockets/<canonical runtime 的 SHA-256 前 16 hex>/runtime.sock`，
而插件仍按旧规则（含 TMPDIR 回退）解析。两者从此指向不同文件：插件探活必然
`ECONNREFUSED`/`ENOENT`，连续三次失联后主动退出，监督器再拉起、再退出，表现为
「更新之后看板用不了」。

## 2. 自动矩阵

| # | 检查 | 命令 | 结果 |
|---|---|---|---|
| 1 | 插件 IPC 全量 | `cargo test -p superpowers-kanban-ipc` | pass：47 lib + 8 contract（1 ignored）+ 10 `status_budget` |
| 2 | 插件 runner 全量 | `cargo test -p superpowers-kanban-runner` | pass：354 lib + 52 + 8 `daemon_liveness` + 12 |
| 3 | 宿主契约单测 | `cargo test -p yi-agent-lifecycle --test daemon_socket_contract` | pass：3 |
| 4 | 跨进程真实函数对照 | 门禁内 `--ignored --exact` | pass：1 passed / 0 ignored |
| 5 | gate 自测 + wiring 负例 | `python3 -m unittest discover -s scripts/plugins -p 'test_check_daemon_socket_contract.py'` | pass：13（正常 case 断言实际执行 8 个 cargo 阶段） |
| 6 | 跨端门禁 | `cd yi-agent-rs && just plugin-daemon-socket-check` | pass（exit 0） |
| 7 | 插件全门禁 | `cd yi-agent-rs && just plugin-check` | pass |
| 8 | 宿主 22 道门禁 | `cd yi-agent-rs && just ci` | pass（`CI passed`）。audit 因网络无法更新 advisory-db，走本地缓存 `--no-fetch --stale`；脚本自身声明 nightly job 兜底 |
| 9 | 两个 workspace 格式 | `cargo fmt --all -- --check` | pass（插件 + 宿主） |
| 10 | 云端 CI（GitHub Linux/macOS、GitLab） | — | **not_run**：本轮未建 PR/MR，没有运行链接可归档。本机结果不能替代云端 |

## 3. 变异验证（必须红，逐项还原）

| 变异 | 期望 | 实测 |
|---|---|---|
| 插件改回 `<runtime>/runtime.sock` 旧布局 | A10 进程用例红 | pass：`new_socket_keeps_same_pid_and_old_sockets_are_untouched` 失败（新地址 0 次探测），还原后绿 |
| 去掉实例家目录的 canonical 化 | 契约测试红 | pass：`canonical_spelling_and_missing_tail_match` 报 `i-link/...` vs `i/...`；门禁随之 exit 1 |
| 去掉 macOS `SO_NOSIGPIPE` | SIGPIPE 用例红 | pass：worker `died by signal Some(13)`，还原后绿 |
| 拆掉读阶段两层 deadline | 滴灌用例红 | pass：`drip_feed_cannot_renew_budget` 得到 `Truncated` 而非 `Timeout{Read}`，还原后绿 |
| 解开两处帧上限 | 超长流用例红 | pass：`oversized_stream_is_bounded` 失败，还原后绿 |
| 删 `plugin-contracts-check` 的依赖 | wiring 守卫红 | pass：exit 1 并点名 `must depend on ...` |
| 缺宿主 helper | 跨端用例红 | pass：`KANBAN_HOST_SOCKET_PROBE=/nonexistent` → `0 passed; 1 failed` |
| 宿主探针产物指向别的 target | 门禁拒绝 | pass：gate 自测 `test_stale_or_cross_target_probe_is_rejected` |
| 单测试阶段 0 test | 门禁拒绝 | pass：gate 自测 `test_zero_cross_tests_is_rejected` |
| 成功文本 + 非零退出 | 门禁拒绝 | pass：gate 自测 `test_exit_zero_without_real_result_is_rejected` |

## 4. 隔离真机（真 daemon + 真插件二进制）

**环境**：macOS（本机），隔离根 `/tmp/iso-k5-PAtp`（跑完即删）。全程
`env -i`，只设 `HOME` / `YI_AGENT_INSTANCE_HOME` / `TMPDIR` 指向隔离根；
模型配置用**占位** `api_url=http://127.0.0.1:9/v1` 与假 key，从不读生产
`~/.yi-agent/models.json` 的凭据，也没有任何真实模型调用。生产 `~/.yi-agent`
全程未被写入。

| 项 | 命令 / 观测 | 结果 |
|---|---|---|
| 宿主 daemon 在新布局 bind | `yi-agent daemon serve`（隔离实例） | pass：实际生成 `<iso>/home/sockets/8fdb972026b042d5/runtime.sock`，与按 `sha256(realpath(runtime))[:16]` 独立算出的路径**逐字节一致** |
| 插件探到该地址 | `superpowers-kanban run --runtime-dir … --state-dir …` | pass：日志 `daemon socket at …/8fdb972026b042d5/runtime.sock`，与 daemon 实际 bind 的路径相同 |
| 120 秒同 PID 稳定 | 每 10s 采样，12 次 | pass：PID 47904 全程存活，`daemon probe failed` 计数 **0** |
| 探活确实成功（而非空转） | 杀掉 daemon 后观测 | pass：daemon 停止后 **3 秒内**出现 `misses=1/3`、`2/3`、`3/3`，errno 为 `Connection refused (os error 61)`，随后 exit 0。反证此前 120 秒的探活是成功的——若从未成功，它早该在启动后 3 秒退出 |
| 单实例锁释放 | 退出后 `acquire` | pass：进程退出即释放（Task6 进程测试同样覆盖） |
| 清理 | `pkill -f <iso>` | pass：残留进程 0；隔离根删除 |

**明确没有做的部分**（`not_run`，不写"已恢复"）：

| 项 | 原因 | 状态 |
|---|---|---|
| 独立 app bundle（独立 `CFBundleIdentifier` + 绝对 `YI_AGENT_HOME`）构建与签名 | 本轮未按 CLAUDE.md 的独立 app 规程打包；已有 `desktop/src-tauri/target/release/bundle/macos/yi-agent.app` 是本次改动**之前**（11-09 00:53）的产物，用它验收等于用旧代码验证新修复 | not_run |
| 桌面 GUI 启动 + sidecar 环境/参数核对 | 同上，没有当轮构建的 bundle | not_run |
| 经真宿主转发 12 次小 `switch.read` | 需要上面两项；且既有「查询 IPC 完整帧」问题属另一条工作线，本轮不顺手修 | not_run |
| 停隔离 desktop 监督父进程后跟踪旧插件三次失联 | 没有拉起桌面实例 | not_run |
| 正式插件安装/重启与生产复核 | **人工检查点**，未获用户许可 | not_run（代码完成、正式待确认） |

## 5. 与计划的偏差（如实记录）

1. **Task2 顺带补齐了 Task1 的一个测试缺口。** 实例家目录的 canonical 化原先只有
   runtime 的 symlink 用例覆盖，导致「去掉 home canonical 化」是假绿。补 `i-link`
   用例后该变异实测红。
2. **修复了跑整包时才暴露的既存问题**：`failed_attempts_close_descriptors` 用进程级
   `/dev/fd` 计数，被同进程其他测试的 socket 干扰而误红——改为独立子进程
   （`fd_leak_worker`，断言 `after == before`）。
3. **修复了 `-D warnings` 下的 clippy 错误**：宿主探针与宿主契约测试的
   `chunks_exact(2)` 触发 `clippy::chunks_exact_to_as_chunks`，会让 `just ci` 的 lint
   直接失败；改为 `chunks(2)`。
4. **`plugins/contracts/README.md` 的期望计数是既存漂移**（旧值 9/24、42/42），按
   实测校正为 14/26、56/56。
5. **Task1 还包含一处计划外但必要的修复**：runner 的 `driver_observation.rs` 引用
   的是已迁移的 `../contract/driver-protocol.json`，基线本身就编不过；
   `service.rs` 两处 `driver_step` 缺 `&no_caps()`。这两处不修，runner 的任何测试
   都跑不起来。

## 6. 剩余风险

- **正式插件仍未恢复。** 本轮只证明代码在隔离实例上能探到真 daemon；生产路径要等
  获准后安装/重启并复核。
- **桌面整包未验证。** 桌面 → sidecar → daemon 这条链里，sidecar 的环境变量传递
  （`YI_AGENT_INSTANCE_HOME` 是否与插件一致）没有在本轮实测，是 Task8 剩下的主要
  未知。
- **云端 CI 未跑。** 新增的 macOS job 步骤与 GitLab 的 `python3` 依赖只在本机以
  `just` 和 shell 直接验证过接线，真实 runner 上的行为要等 PR/MR。
- **audit 用的是本地缓存库。** 网络恢复后应确认 advisory 数据没有新增相关问题。
