# 看板 daemon socket 契约修复 — 验收记录（2026-11-09）

关联：[设计 spec](../specs/2026-11-09-kanban-daemon-socket-contract-design.md) ·
[实施计划](../plans/2026-11-09-kanban-daemon-socket-contract-impl.md)

分支 `fix/kanban-daemon-socket`，实现 worktree
`.worktrees/fix-kanban-daemon-socket`。

**结论先说：自动矩阵全绿；隔离真机跑完了「独立 bundle → 真 desktop 进程 →
真 sidecar → 真 daemon → 生产同构的 supervisor 清单 → 真插件二进制 → 12 次经宿主
转发的查询」整条链；正式插件仍未安装/重启（人工检查点）。**

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

## 4. 隔离真机（真 daemon + 真桌面 shell + 真插件二进制）

**环境**：macOS（本机）。隔离根 `~/isoapp-kanban`（跑完即删；**不能放在
`/tmp`**，见下方第 3 条踩坑）。全程显式 `HOME` / `YI_AGENT_HOME` /
`YI_AGENT_INSTANCE_HOME` / `TMPDIR` 指向隔离根；模型配置用**占位**
`api_url=http://127.0.0.1:9/v1` 与假 key，从不读生产
`~/.yi-agent/models.json` 的凭据，也没有任何真实模型调用。生产 `~/.yi-agent`
未被写入（`plugins` 目录 mtime 仍是 10-31）。

独立 bundle：按项目 CLAUDE.md 第 1–5 条，`CFBundleIdentifier` 改为
`com.gongyichen.desktop.kbaniso`，`LSEnvironment:YI_AGENT_HOME` 写绝对路径，
前端先 `npm ci && npm run build` 产出 `dist/` 再 `npx tauri build --bundles app`，
最后 sidecar → desktop → app 三层重签，`codesign -v` exit 0。

### 4.1 第一段：宿主 daemon 与插件探到同一地址

| 项 | 命令 / 观测 | 结果 |
|---|---|---|
| 宿主 daemon 在新布局 bind | 隔离实例 `yi-agent daemon serve` | pass：实际生成 `<iso>/home/sockets/8fdb972026b042d5/runtime.sock`，与按 `sha256(realpath(runtime))[:16]` 独立算出的路径**逐字节一致** |
| 真插件探同一地址 | 真插件二进制 `run --runtime-dir … --state-dir …` | pass：日志 `daemon socket at …/8fdb972026b042d5/runtime.sock`，与 daemon 实际 bind 的路径相同 |
| 120 秒同 PID 稳定 | 每 10s 采样共 12 次 | pass：PID 47904 全程存活，`daemon probe failed` 计数 **0** |
| 探活确实成功（反证） | 杀掉 daemon 后观测 | pass：daemon 停止后 **3 秒内**出现 `misses=1/3`、`2/3`、`3/3`，errno `Connection refused (os error 61)`，随后 exit 0。若此前从未探成功，它本该在启动 3 秒后就退出——所以这反证了前 120 秒的探活是真的 |

### 4.2 第二段：生产同构的 supervisor 清单路径（本报告的核心证据）

第一段只证明「函数算出的地址对」。生产实际是 **supervisor 按
`<workdir>/.yi-agent/supervisors/<name>.json` 拉起插件**，所以又补了一段与
生产结构一致的验证：把清单里的 `command` 换成本次 worktree 构建的插件二进制，
其余字段（`args` 占位符、`query_socket`、`switch_key`、`session_driver`）
逐字照抄生产清单；宿主用当轮 release 构建的 `yi-agent daemon serve`。

| 项 | 命令 / 观测 | 结果 |
|---|---|---|
| 二进制身份 | `strings` 两个生产/新二进制 | pass：新二进制含 `daemon probe loop starting`、**不含**旧文案 `daemon is gone`；生产 `/opt/homebrew/bin/superpowers-kanban`（11-09 00:53）恰好相反——这就是用户遇到的故障版本 |
| 地址一致 | 插件日志 vs 独立 sha256 计算 | pass：`…/sockets/58b7a23530f25dc4/runtime.sock` 与 `sha256(realpath(<iso>/prod/.yi-agent/runtime))[:16]` 逐字节一致 |
| 120 秒同 PID | 每 10s 采样共 12 次 | pass：PID 57616 全程存活，`daemon probe failed` 计数 0 |
| 经真宿主转发 12 次 `board/readSwitch` | 向真 daemon socket 发 12 个 `PluginQuery`（`protocol_version: 3`），由 `forward_plugin_query` 转到真插件 | pass：**12/12 ok**，每次 `{"on":true,"source":"project"}`，`request_id` q12-1…q12-12 全部对上 |
| 三次失联退出 | 停 daemon 后观测 | pass：`misses=1/3`、`2/3`、`3/3` + `Connection refused (os error 61)`，随后 `exiting to release instance lock` |
| 实例锁释放 | `lsof plugin.lock` | pass：退出后无持有者 |
| 卡片状态未被探测改动 | 退出后读 `board.json` | pass：`{"cards":[],"next_order":0}`，探测过程没有写卡片 |
| 清理 | `pkill -f isoapp-kanban` | pass：残留 0；隔离根删除 |

### 4.3 这一段暴露的两件实事

1. **查询 IPC 的 `os error 35` 在本轮复现了**：插件日志出现过一次
   `query connection failed: Resource temporarily unavailable (os error 35)`。
   这正是仓库里既有的独立问题（`[ ] 查询 IPC 在非阻塞读写下保持完整帧`），
   计划里也写明遇它要 `fail` + 独立阻塞、不要去修那个模块。本轮按此处理：
   记录、不改、不宣称已解决。但要注意——12/12 的转发全部成功，说明它是
   间歇性的，不是常态失败。
2. **它没有影响 daemon socket 修复的结论**：那 12 次查询与探活是两条独立
   通道，探活全程 0 失败。

### 4.4 踩到的两个坑（写给别人省时间）

1. **隔离根不能放 `/tmp`。** `desktop` 拒绝在含符号链接的路径上定位 sidecar，
   直接报 `StartingBinary found current_exe() that contains a symlink on a
   non-allowed platform: /tmp`，且只在重复刷屏、看起来像别的问题。换到
   `~/isoapp-kanban` 才正常。
2. **`LSEnvironment` 对直接执行 `Contents/MacOS/desktop` 不生效。** 第一次这样
   启动时它读到了**生产**配置，sidecar 带上了生产的 `--relay …?session=`。
   发现后立即终止、改用显式环境变量重启，确认 sidecar 命令行**没有** `--relay`
   才继续。这条与 CLAUDE.md §6「不要只用 `open -a` 验收」是同一类陷阱的
   镜像版本：`Info.plist` 里写的环境不等于进程实际拿到的环境，两边都要查。
3. **受管激活（`plugins install` + `plugins activate`）在本轮走不通。**
   `PluginManager::activate` 自己写着「只建立，不推进」，而推进需要的
   `RuntimeControl` 在非测试代码里**没有实现者**（`grep "impl RuntimeControl for"`
   只命中测试）。事务因此停在 `draining`。生产用的也是 legacy supervisor 清单
   而不是受管安装，所以这一段改用 4.2 的清单路径，与生产的真实结构一致。
   这是既有缺口，不属本轮范围，没有顺手改。

### 4.5 仍然 `not_run` 的部分

| 项 | 原因 | 状态 |
|---|---|---|
| 桌面 GUI 点击路径（真正点开看板界面、拖动卡片） | 本轮验证到「desktop 进程起、sidecar 环境正确、daemon 地址一致、宿主转发通」为止，没有在 GUI 里做人工点击 | not_run |
| 正式插件安装/重启与生产复核 | **人工检查点**，未获用户许可 | not_run（代码完成、正式待确认） |
| 云端 CI（GitHub macOS/Linux、GitLab） | 本轮未建 PR/MR | not_run |

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
5. **跨端契约测试的 5 秒子进程预算在负载下会假红。** 在 `just plugin-check`
   （同时跑 `cargo test --workspace` 等 5 个子门禁）里出现过一次
   `contract child timed out after 5s`，而同一门禁单独跑稳定通过。这不是被测
   逻辑的问题，是预算按空闲机器定的。已改为统一的 `CONTRACT_CHILD_TIMEOUT`
   （60 秒），并在超时信息里带上是**哪个**子进程（原来只说"timed out"，看不出
   卡在哪一侧）。仍然是**有界**失败：真挂死的子进程一样会被杀并让测试红。
   改后连跑两轮 `just plugin-check` 全绿。
6. **Task1 还包含一处计划外但必要的修复**：runner 的 `driver_observation.rs` 引用
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
