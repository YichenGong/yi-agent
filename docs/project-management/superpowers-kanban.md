# superpowers-kanban（看板插件）

## 模块说明

`plugins/superpowers-kanban/` 是独立 workspace 的看板插件：把 spec+plan 排成队列，在
限流窗口内按顺序起会话开发，跑完在原会话里申请合并名额。宿主（daemon / app-server /
桌面）

**不认识看板**：它只按 supervisor 清单守护进程、按 `session_driver` 每 3s 拉一拍
`driver/next`、执行插件给的通用动作（`launch` / `append`）。插件通过
`superpowers-kanban manifest` 自述清单，宿主通用 `plugin/install` 消费。

本模块文档只管**插件本体与它的 skill**；宿主侧的解耦改造记在
`yi-agent-app-server.md`，桌面看板 UI 记在 `desktop.md`。

## 范围边界

**做什么：**
- 卡片状态机与排队/并发窗口（`crates/superpowers-kanban-core`）
- 插件与宿主的 socket 契约（`crates/superpowers-kanban-ipc`）
- `run` 推进循环、一次性 CLI 动词、`driver/next` 决策（`crates/superpowers-kanban-runner`）
- 面向模型的 `superpowers-kanban` skill（`skills/superpowers-kanban/SKILL.md`）

**不做什么：**
- 不写 spec / plan（上游是 `brainstorming` / `writing-plans`）
- 不在宿主里留任何看板专属代码路径
- 不管桌面看板面板的渲染（`desktop/`）

## Features

- [ ] **daemon socket 契约同步修复** — 宿主 `cac5afe0` 已迁到实例目录，插件仍探测旧地址，误判 daemon 消失并反复退出。2026-11-09 已交付[修复 spec rev3](../superpowers/specs/2026-11-09-kanban-daemon-socket-contract-design.md)、[四轮复审记录](../superpowers/reviews/2026-11-09-kanban-daemon-socket-spec-review.md)与[8 任务实施计划 rev2](../superpowers/plans/2026-11-09-kanban-daemon-socket-contract-impl.md)，plan 已经[专项反复复审](../superpowers/reviews/2026-11-09-kanban-daemon-socket-plan-review.md)并修正生命周期/可见性、cwd 身份、SIGPIPE/计时、CI依赖及 Bash 3.2 假绿问题；设计与计划当前未发现未决阻断项，尚未实现/部署；不混同查询 IPC、PATH 或 worktree 授权问题。完成判据：该 spec §7 A01–A14、§8 隔离真机与获准后的正式部署复核全部有证据，含不跨 crate 依赖的真实函数进程对照、实例隔离、活宿主下插件 PID 稳定、单次探活 2 秒 IO 预算、空载三次失联退出（不承诺任意工作负载整进程硬上界）。实施后的验证入口：`cd yi-agent-rs && just plugin-daemon-socket-check && just plugin-check && just ci`。

  - [x] Task1 公共路径契约与插件业务地址解析：`plugins/contracts/fixtures/daemon-socket.json` 冻结算法/优先级/后缀/目录/103 字节上限与 root 向量；`crates/superpowers-kanban-ipc/src/daemon_socket.rs` 复刻宿主 `scope.rs` 的 canonicalize-允许缺失尾部与 16 hex 摘要。判据：`cargo test -p superpowers-kanban-ipc --test daemon_socket_contract`（7 passed，含 `short_and_deep_use_instance_socket_directory`、`instance_priority_and_suffix_are_exact`、`canonical_spelling_and_missing_tail_match`、`non_utf8_and_length_are_raw_bytes`、`root_vector_and_fixture_schema_are_pinned`）；宿主侧 `cargo test -p yi-agent-lifecycle --test daemon_socket_contract`（3 passed，读同一 fixture 调真实 `scope::daemon_socket`）。旧「socket 在 runtime 目录旁 / TMPDIR 回退」daemon 断言已删除。

  - [x] Task2 跨进程真实函数对照：宿主 `yi-agent-lifecycle/examples/daemon_socket_contract_probe.rs` 与插件 `plugin_environment_worker` 各自调用**真实**生产函数，`cross_process_daemon_socket_contract` 逐 case 比较原始 hex/错误类别，覆盖 short/deep/instance_priority/override_suffix/empty/relative/dot_segments/symlink/symlink_dotdot/missing_tail/isolated/tmpdir/non_utf8/deleted_cwd/limit_103/limit_104 共 16 个 case ID。判据：`KANBAN_HOST_SOCKET_PROBE=<target>/debug/examples/daemon_socket_contract_probe cargo test -p superpowers-kanban-ipc --test daemon_socket_contract cross_process_daemon_socket_contract -- --ignored --exact`（1 passed / 0 ignored）；缺 helper 与假 helper 各实测失败；三类临时变异（插件改回旧布局、INSTANCE/override 优先级交换、去 canonicalization）各实测红后恢复绿。

  - [x] Task3 首次 connect 前非阻塞且有界：`status_io::connect_before` 先拒绝 NUL/超长地址，socket 返回负 fd 不构造 `OwnedFd`，非阻塞 + CLOEXEC + macOS `SO_NOSIGPIPE`；pending 只认 EINPROGRESS/EALREADY 并 `getsockopt(SO_ERROR)`，EINTR/EAGAIN 关 fd 按**原 deadline** 重建。判据：`cargo test -p superpowers-kanban-ipc status_io::tests -- --test-threads=1`（9 passed，含 `nul_is_rejected_before_connect`、`connect_backlog_obeys_deadline`、`poll_eintr_keeps_deadline`、`failed_attempts_close_descriptors`、`poll_that_never_gets_ready_times_out`）；变异（不重试 EINTR、不拒 NUL、去掉 poll deadline 检查）各实测红/挂起后恢复绿。

  - [x] Task4 探活 IO 共享一个预算：`status_io::{exchange,exchange_connected,write_all,read_one_frame}` 只在 `client::send_status_with_budget(socket, budget)` 这一条入口下服务 `Status`；连接、写、读共享同一 `Instant` deadline，帧上限按**不含 LF** 计。判据：`cargo test -p superpowers-kanban-ipc`（46 lib + 8 contract + 10 `status_budget` 全绿，含 `silent_peer_times_out_during_read`、`drip_feed_cannot_renew_budget`、`short_writes_and_interrupted_io_complete_one_frame`、`frame_limit_excludes_newline`、`hup_with_complete_frame_is_not_truncated`、`broken_peer_does_not_raise_sigpipe`）；变异（去掉 macOS `SO_NOSIGPIPE` → worker 被 signal 13 杀死、解开两处帧上限 → `oversized_stream_is_bounded` 红、拆掉读阶段两层 deadline → `drip_feed_cannot_renew_budget` 红）各实测红后恢复绿。

- [ ] **查询 IPC 在非阻塞读写下保持完整帧** — 当前 macOS accepted socket 继承非阻塞标志，请求未到或大回复背压时提前断开，造成看板“无法读取”。本轮仅完成[修复 spec 与文档自检](../superpowers/specs/2026-11-03-kanban-query-io-design.md)，尚未实现。完成判据：该 spec §7 的 A01–A15 与 §8 部署验收全部通过，含延迟请求、256 KiB 背压回复、统一截止时间/停止、帧上限及真实 daemon 转发；验证入口：`cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-ipc`，macOS 回退原连接处理时 A02/A04 必须变红。

- [ ] **会话存活观测误报修复** — 代码已全部交付（Tasks 1–7），**待隔离实例真机验收**；真机未通过前不勾完成。根因：`server/driver/tick.rs` 只刷新进程内 `DriverState` 已登记子集，重启后普通 RPC 恢复的热会话未被重新发现；插件把未上报旧 ID 写成 false；桌面把「未观测到」说成「已不存在」、把轮询时间说成「最后活动」。
  已交付：①宿主 `session_driver.rs` 新增 `LoadedThreadFacts` / `loaded_threads_snapshot`（只读内存会话表、按 ID 排序，`idle` = `ThreadStatus::Idle` 且无活跃 turn，单独成 `snapshot_idle` 以便钉住）/ `loaded_threads_payload`，`driver_step` 每次上送完整快照（空热表也发空数组，不省字段），`driver_tick` 在构造 launcher 前采**一次** owned 快照，旧 `threads[].idle` 由同一读数导出；②插件 `driver_observation.rs` 的严格解析（null/非数组/空白或非字符串 ID/非 boolean `idle`/重复 ID 报错且带 `driver/next loaded_threads[index]`）与 `normalize`（完整快照权威、旧宿主缺 `alive` 不伪造展示证据、无关旧记录剪除、`previous` 只读）；③`observation.rs` 三态 `SessionObservation`（`Loaded`/`NotLoaded`/`Unknown`，`stale` 只在 running + 非空执行 ID + 明确 `not_loaded` 且 `observed_at` 合法时为真）；④`service.rs` 的 `driver_step_with_loaded` 按「读取相关 ID → 归一化 → 旁路先落盘 → tick → 消费回报 → gate → 补账 → 调度」接线（保存失败即不消费，结果留到下一拍重试），`related_thread_ids` 只收看板绑定/有效 attempt/pending 续跑/匹配成功回报的 ID，merge attempt 处于 pending/accepted/uncertain 时保留同卡单份 lease；⑤`list`/CLI 与桌面统一走 `for_card` 同一投影：`[stale] 当前宿主未加载（截至 <RFC3339>；历史可能仍存在）` 与 `[observation unknown] 当前宿主未观测到（历史可能仍存在）`，删除 `aria-label="会话已不在"` 与「最后活动」文案，改为「最后观测到已加载 <日期>」（`desktop/src/lib/kanbanObservation.ts` 严格 RFC3339，拒绝 `2026-02-30` 这类不存在的日期）。
  判据与证据：K1–K17 逐条测试表见 `[plan 的 K1–K17 证据表](../superpowers/plans/2026-11-03-kanban-stale-observation-impl.md#k1k17-证据表2026-11-03)`（每条含测试函数、退出码、commit；真机列为待验收）。门禁实测：`just ci` 通过（nextest 4082 项全绿，含 msrv/audit/secret/bindings）、`cargo test -p superpowers-kanban-runner` 267+48、`-p superpowers-kanban-ipc` 30、`npx tsc --noEmit` + `npx vitest run` 2063/153 全绿、`git diff --check` 干净。变异证据：删宿主上送 → 4 failed；`normalize` 忽略完整快照 → 9 failed；删前置保存 → 6 failed。
  尚未完成（故本条不勾）：Task 8 Step 4/5 的独立实例真机验收——独立 bundle id + 绝对 `YI_AGENT_HOME` 打包、真实宿主重启后原卡未加载语义、手动打开原 thread 后下一 tick 角标消失、无额外 turn / 无新 thread ID、热表移除但保留历史只提示未加载、旧载荷下新 UI 只说未观测到。本会话已在真实插件二进制上验证 CLI 投影（`not_loaded` 带时间、旧 `alive:false` 与非法日期均落 `unknown`），但**未**在真实宿主 + 桌面实例上走完上述矩阵。验证入口：`cargo test -p yi-agent-app-server --lib kanban_observation_`、`cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner`、`cd desktop && npx vitest run src/lib/kanbanObservation.test.ts`。

- [ ] **完整插件独立发布** — 界面、执行程序与 skill 统一版本，通过通用宿主安装和加载；官方包校验、显式项目启用、安全更新及远端版本匹配均须通过。完成判据：[设计](../superpowers/specs/2026-11-02-kanban-independent-release-design.md) §13 的 A01–A20 全部验收项，尤其在同一 yi-agent 应用包下更新看板界面且主程序发布物摘要不变。

  T01–T24 已实现（宿主通用插件核心、受管安装/激活、桥与 SDK、看板迁移与旧安装接管）。T25 的 Step 1–3 已完成并可验证：`python3 -m unittest discover -s scripts/plugins -p 'test_check_host_boundary.py'`（10 passed）、`python3 scripts/plugins/check-host-boundary.py`（exit 0），宿主不再含看板专属路径。T26 Step 1–2 已完成：`plugin-pack`（确定性 tar.gz + 签名，`cargo test -p yi-agent-plugins --test pack` 4 passed）与插件发布 tag 门禁（`just plugin-release-check` 11 passed，已进 `just ci` 成为第 16 道门禁）。

T25 Step 4 已完成并可复现：`bash yi-agent-rs/scripts/verify-plugin-update.sh <built .app> .` 在同一 `.app` 上装 1.0.0 再装 1.1.0，UI 标记 `board-v1` → `board-v2`，主程序 `Contents/MacOS/desktop` 与整个 bundle 摘要前后一致。

T26 Step 3 已完成：`just plugin-check`（`plugin-workspace-check` / `plugin-ui-check` / `plugin-contracts-check` / `plugin-boundary-check`）本地全绿，GitHub `plugins` job 与 GitLab `ci-plugins` job 均跑在云端 runner、不带 `tags:`，插件门禁不再借用 self-hosted runner 或任何签名密钥。

- Step 3g（A03/A08 判词纠正，2026-11-07）：**A03 从「unsupported: missing capability」变成真正执行并通过；A08 从假 `pass` 变成诚实的 `unsupported`。**两个缺陷各自独立：
  - **A03 的命令指向不存在的路径**。映射表写 `scripts/verify-plugin-update.sh`，脚本实际在 `yi-agent-rs/scripts/`（`git log --all` 证明 `scripts/` 下从未有过该文件）。执行层只看到「bash 找不到文件」，判词落 `fail`，读报告的人会去查插件更新实现。同一个 bug 还有第二层：`parseCommand` 不过 shell，`"$YI_AGENT_ACCEPTANCE_APP"` 被当成**字面量**传给脚本，脚本收到的不是路径。修复：① 路径补成 `yi-agent-rs/scripts/verify-plugin-update.sh`；② `parseCommand` 支持 `$VAR`/`${VAR}` 展开，未定义变量**报错**而不是落成空串（空串会与「没给」混淆）。实跑 `YI_AGENT_ACCEPTANCE_OFFICIAL=1 YI_AGENT_ACCEPTANCE_APP=/Applications/yi-agent.app node scripts/plugins/e2e.mjs --suite A03` 得 `pass=1`。
  - **A07 与 A08 跑的是同一条命令**（`--suite isolation`），A08 的 `pass` 只是 A07 的 12 个安全用例顺带跑出来的。实测覆盖情况：`normal_interaction_works` 真覆盖了焦点/中文输入/主题 token/滚动/尺寸，`blocked_script_recovery` 与 `alloc_burst_is_bounded` 真覆盖了故障恢复，但判据里「滥发消息被限流」在 `plugin_host` 里**没有任何限流实现**（`grep throttle|rate_limit` 零命中），却从未被检查。这与 A17 是同一类错误——把功能缺口说成已满足。修复：① `verdictFor` 遇到声明了 `permanentGap` 的用例不再返回 `pass`（此前 `permanentGap` 只影响文案、不影响判词）；② A08 声明限流缺口并把输出写到独立的 `.artifacts/plugins/probe-a08.json`，避免与 A07 互相覆盖。实跑 `--suite A08` 得 `unsupported` 并点名限流未实现，探针本身 12/12 全绿（证明已实现部分无回归），A07 仍为 `pass`。
  - 新增三条守卫测试：`every shell script named by the command map exists on disk`（映射表里的 `.sh` 必须真实存在，用旧路径反向验证过会红）、`A08 is not a copy of A07 and declares the requirement it cannot prove`、`a case that declares a permanent gap can never be recorded as a pass`。`node --test scripts/plugins/*.test.mjs desktop/scripts/plugins/*.test.mjs` 得 **212 passed**。

T27 已落地的部分：

- Step 1（报告校验器）：`scripts/plugins/acceptance-cases.json` 是 A01–A20 唯一真相，`scripts/plugins/e2e-report.mjs` 要求每条**有且仅有一个**最终结果、只认 `pass`，并拒绝三种伪造：probe 构建冒充正式包（A20 必须 `unsupported`）、A16/A17 用 loopback 或 `127.0.0.0/8` 冒充远端、`official` 证据缺签名身份或未公证。`just plugin-acceptance-check` 24 passed；变异验证三项各自被拦。
- Step 2（故障注入矩阵）：`scripts/plugins/fault-matrix.json` 把断线提交、旧 frame 重放、两实例预算、活跃审批、更新收尾、候选不可见、共享配置修订冲突、旧安装接管、待清理卸载、每阶段崩溃十项绑到 41 个既有测试；`just plugin-fault-matrix` 真跑 nextest 得 10/10 项、41/41 测试 ok。改名任一测试即 9/10 并以退出码 1 失败。
- Step 4（同一 `.app` 上的 v1→v2）：`just plugin-update-check /Applications/yi-agent.app` 得 UI `board-v1`→`board-v2`、主程序 desktop 与 bundle 摘要前后一致、`verify-active-release.py` 校验活跃版本/view 摘要/skill 来源、卸载后项目数据保留且托管入口消失。五种变异各自被拦。

- Step 3d（构建链缺陷，2026-11-05 实测修复）：三处会阻断「独立发布」验证的缺陷。① **`desktop/src-tauri/Cargo.lock` 在 main 上就是坏的**：`cargo metadata --locked` 退出码 101，`yi-agent-lifecycle` 的依赖数组缺 `]`、`yi-agent-plugins` 缺 `sha2`；由合并 `7e71f238` 手工解冲突引入，干净检出无法构建桌面 app。已最小修复（仅 +4 行，`--locked` 退出码 0）。② **`App.tsx` 在本分支编译不过**（`tsc` 第 3009 行语法错误）：分支落后 main 197 提交，main 上有 6 个后续修复（含 `fix: restore App JSX test collection`）；合入 main 后 `tsc --noEmit` 零错误。③ **sidecar 二进制陈旧**：`build.rs` 要 `internal-runtime identity`，仓库里的 sidecar（11-03）不认识该子命令；`bash scripts/build-sidecar.sh release` 重建后通过。另修 `isDevBuild` 判据：原用二进制内是否含 `localhost:1420`，但 Tauri 会把整份 `tauri.conf.json` 编进二进制，正式构建同样含该串，会把正式产物误判成 dev 构建；改为「dist 的入口 JS 是否被嵌进二进制」。
- Step 3e（A07/A08 打通，2026-11-05）：**A07/A08 从 fail 转为 pass，12/12 隔离用例全绿。**此前 240 秒超时不是实现缺陷：探针代码挂在非默认 `plugin-probe` feature 后（`Cargo.toml` 要求它永不进正式包），而映射表拿普通 `tauri build` 产物去跑，该二进制里 `--plugin-probe-run` 出现 **0 次**——app 把未知参数当普通启动，挂到被杀。修复：① A07/A08 先构建带 feature 的产物到独立 `desktop/src-tauri/target-probe`；② 探针新增 `--app` 直接指定 bundle（tauri CLI 会把 `CARGO_TARGET_DIR` 相对 `src-tauri/` 二次解析，实测落到 `desktop/src-tauri/desktop/src-tauri/...`）；③ `target-probe/` 入 `.gitignore`。`blocked_script_recovery` 与 `alloc_burst_is_bounded` 首次被执行。
- Step 3f（全量验收终值，2026-11-05）：`node scripts/plugins/e2e.mjs` 得 **16 pass / 0 fail / 4 unsupported**。四条 unsupported 均为外部条件缺席：A03/A20 需官方签名发布授权、A16/A17 需远端主机（A17 的离线缓存与跨宿主资产拉取确实未实现：`FetchRequest` 零调用点）。**（2026-11-07 更正：这个「16 pass」里 A08 是假通过——它与 A07 跑同一条命令，「限流」判据从未被检查。A03 当时也只是因缺 `official` 授权被跳过、命令路径本身还是错的。见下方 Step 3g。）**
- Step 3c（全量验收，2026-11-05）：`node scripts/plugins/e2e.mjs` 得 **14 pass / 0 fail / 6 unsupported**。六条 unsupported 各有具体原因：A03 `missing capability: official`、A07/A08 产物是 dev 构建（二进制内含 `localhost:1420`，`frontendDist` 未打包）、A16/A17 未配置远端主机、A20 无官方签名包。配置远端后 A16 转 **pass**、A17 仍 unsupported（见 Step 3b）。**A07/A08 尚未在正式构建上验证过**——探针只在 dev 构建上跑到过 240 秒超时，因此「WebView 隔离与故障恢复可用」这条目前没有正面证据。
- Step 3b（远端执行器，2026-11-05）：`scripts/plugins/e2e-remote.mjs` 经 ssh stdio 连真实远端 `agenticlabs@10.103.132.15`（macOS 26.5.2 / arm64）。**A16 通过**：两端版本 `0.1.13-dev+32365036` 相等、协议版本 1、宿主身份非 loopback、目标架构受支持、本地不运行远端程序。**A17 仍 unsupported**：只证明了「断开后无残留进程 / 重连得到新身份 / 旧绑定失效 / 代次被复用」四条，离线缓存与跨宿主资产获取未实现（`remote_assets.rs` 的 `FetchRequest` 零调用点）。报告见 [远端验收报告](../superpowers/reports/2026-11-05-remote-acceptance-a16-a17.md)。真跑暴露并修掉五个缺陷，其中最关键的是「执行器判 unsupported 却被记成 pass（退出码为 0）」，已改为 `verdictFor` 信任执行器自己的判词。门禁新增 49 项注入式测试（`remote-facts` 10 / `remote-client` 8 / `e2e-remote` 23 / `e2e` 新增 8），不连真机；`just plugin-acceptance-check` 合计 124 项。
- Step 3（执行器真跑 A01–A20）：`scripts/plugins/e2e.mjs` 把验收目录的每条用例绑到一条可复算命令并真执行，`--suite all` 产出 `.artifacts/plugins/acceptance.json`；`scripts/plugins/acceptance-commands.json` 是映射表，`just plugin-acceptance-check` 现为 40 个 node 测试 + 24 个契约测试 + 11 个边界测试。本轮实测 **14 pass / 6 unsupported / 0 fail**，报告归档于 [acceptance 报告](../superpowers/reports/2026-11-02-kanban-independent-release-acceptance.md)。六条 unsupported 逐条点名缺什么：A03/A20 缺官方签名包，A07/A08 缺已构建 `.app`，A16/A17 缺 `e2e-remote.mjs` 执行器与真实远端主机。跑这一遍修掉四个只有真跑才暴露的缺陷：cargo 命令未声明工作目录（报「找不到 Cargo.toml」被误读成实现失败）、fault-matrix 脚本路径相对根目录（根本找不到）、十几条用例各自重跑整个故障矩阵（一次验收变一小时，已改为同命令一次运行内复用）、A07/A08/A16/A17 把「缺产物/缺执行器」记成 fail。

**仍未完成：** T26 Step 4（平台签名/公证与真实下载后的 OS 执行策略验证）需要发布授权与签名密钥；T27 Step 3（真实非 loopback 远端宿主证据）需要可访问的外部机器，A16/A17 因此保持未完成；T27 Step 4 的脱敏报告与截图、以及 Step 5 的最终验收提交同样待办。因此本条保持 `[ ]`。
- [x] **合并续跑原开发会话（方案 B）** — 设计见 [2026-11-03-kanban-merge-original-session-design.md](../superpowers/specs/2026-11-03-kanban-merge-original-session-design.md)，设计复核与[实施计划](../superpowers/plans/2026-11-03-kanban-merge-original-session.md)已完成，尚未实现。完成判据：实现卡获授权后只续跑原 `thread_id`，冷会话由宿主恢复；忙碌时等待，不可恢复时 `NeedsYou`，不降级新建；开发 `workdir` 不被合并目录覆盖，并发租约不重复领取。验证入口：`cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner`（225 lib + 45 bin 通过）。

  实现与证据（提交 `e34757f7`、`c0fa47ae`..`2c7b8211`、`b0a5a9f2`、`741e1253`、`c2605709`、`ed9117a1`）：
  - 决策：`service.rs` `promote_merge_queue` / `merge_binding` / `settle_resume_merge` /
    `can_release_merge`。能力先于预算检查，宿主不声明 `resume_append` 时 `Defer`
    而不是退化成 `Launch`（`merge_resume_never_launches_a_replacement_when_the_host_cannot_resume`）。
  - 绑定不可用即人工：缺 thread/workdir → `NeedsBinding` → `needs_you`
    （`merge_resume_sends_the_card_to_you_when_the_binding_is_missing`）；会话找不到
    → `needs_you`（`merge_resume_sends_the_card_to_you_when_the_session_cannot_be_found`）。
  - 忙时零副作用并保留原顺位：`merge_resume_skips_a_busy_head_and_starts_the_next_card`、
    `merge_resume_keeps_the_original_queue_time_when_the_session_is_busy`。
  - 安全记录先于动作：`merge_attempt.rs` 写入 `Pending` 后才落 `Running`
    （`merge_resume_writes_the_attempt_before_it_reports_running`）；崩溃/损坏/卡被清掉
    /legacy holder 均阻塞（`merge_recovery_blocks_a_graph_that_was_left_in_flight`、
    `merge_recovery_blocks_on_a_corrupt_record_instead_of_dropping_it`、
    `merge_recovery_clears_an_unknown_card_after_confirmation`、`..._old_holder_without_a_record`）。
  - 释放判据：`merge_recovery_release_predicate_needs_every_condition`（pending 不释放、
    结果同拍 idle 不释放、后续 idle 才释放）。
  - 不重复领租约：`merge_resume_reuses_an_existing_lease_instead_of_taking_a_second`。
  - 落盘顺序（spec §8.1「记录在前、`Running` 在后」）：`dispatch_resume` 与
    `dispatch_standalone` 都在 `with_board` 闭包里先 `merge_attempt::save`、
    再让闭包成功提交看板（`with_board` 只在 `Ok` 时写盘），两步是同一个提交点。
    若看板提交失败而记录已写，按 spec 允许做一次 CAS 清理（动作确认未交出）。
    证据：`merge_resume_does_not_report_running_when_the_attempt_cannot_be_written`
    ——把记录路径占成目录强制写失败，断言卡仍是 `merge_queued`、无记录残留。
    已做非空性验证：把顺序改回"先写 `Running`"时该测试变红。
  - 自包含授权：`tick.rs` `queued_merge_objective` 全部参数过 `shell_quote`，
    `merge_authorization_is_explicit_and_location_safe`、
    `shell_quoting_survives_a_real_shell_round_trip`、
    `shell_quoting_does_not_execute_injected_commands`。
  - 离线恢复：`merge_attempt::recover_offline`（`merge_recovery_*` 13 项，含
    `refuses_without_acknowledging_a_stop`、`requires_the_exact_action_and_card`、
    `refuses_while_the_card_is_still_running`、
    `refuses_while_the_daemon_still_holds_the_instance_lock`）。
  - 发布文档：`INSTALL.md` §9（升级顺序、降级排空、离线恢复、三种不可自动恢复情形、
    坏 journal）；`SKILL.md` 不再承诺新开合并会话。`list` 现在打印未结算 attempt 的
    action id（`list_prints_the_unsettled_merge_attempt_and_its_action_id`），
    否则 `merge-finish` 的 waiting 文案无法照做。

- [x] 全局开关 `superpowers_kanban` 决定进程起停 — 清单 `supervisors/superpowers-kanban.json` 设 `stop_when_disabled: true`，注册表默认关。关掉 = 完全停掉 daemon 含调度（不再留进程轮询）；重开走设置页 `flags/set`（app-server 直写 `flags.json`，不经过 daemon），所以不是单向门。宿主侧同时收口：`plugin/query` 返回 `plugin_disabled`(-32025) 而不是 `daemon_unavailable`，`plugins/list` 每项带 `enabled`，桌面隐藏看板入口与全部 `driver_ref` 会话。判据：`cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner --lib`（164 passed）；`cargo test -p yi-agent-app-server --lib a_switched_off_plugin`（含 `..._says_disabled_instead_of_broken`、`..._is_still_listed_but_marked_disabled`）；`cd desktop && vitest run src/App.test.tsx -t 看板开关关着时`
- [x] 插件自述清单 — `manifest` 动词把 supervisor 清单打到 stdout，宿主 `plugin/install` 直接消费；`command` 取当前可执行文件绝对路径。判据：`cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner manifest_`（含"与随包 `supervisors/superpowers-kanban.json` 逐字段相等"）
- [x] **过期清单在启动时自校准** — 清单是安装那一刻的快照，插件升级后没人刷新它，于是新逻辑读旧结论：`stop_when_disabled` 从 `false` 改成 `true` 后，用户关掉开关、进程仍在跑，且界面无任何线索。现在监督线程进入循环前跑一次 `<command> manifest`，用插件自述覆盖过期清单（同目录 temp + rename，截断的清单会被静默跳过，等于插件消失）。只认清单里的 `command`、不认插件名；跑不起来 / 输出不是清单 / 名字对不上都只 warn 并**保留原文件**。判据：`cargo test -p yi-agent-supervisors --lib refresh::`（6 passed）；`cargo test -p yi-agent-supervisors --test supervisor refreshing_a_stale`（变异验证：把刷新替换为空结果后报 "the stale manifest must be replaced by the plugin's own description"）
- [x] 预排队列（`PRE-QUEUED`）与提升 — `add-prequeued` 记一张不占槽的卡，`promote` 补齐 spec/plan 并重排到队尾，id 全程不变。判据：`cargo test -p superpowers-kanban-core` 的 `promote`/`pre_queued` 用例
- [x] 会话内合并名额 — `merge-request` 只取名额（每项目一把 `merge.lock`，`merge.lock.holder` 记 card/pid/since，残留 pid 自动回收）；`merge-finish` **只放锁**，不碰 git、不改状态；准入只挡排队中的实现卡，其余由模型自己负责。判据：`cargo test -p superpowers-kanban-runner --lib merge_request_` 与 `merge_finish_`（`merge_finish_only_releases_the_gate_and_never_touches_git_or_state` 钉住「不放行 git 复核」）
- [x] **skill 与 CLI 不再各自漂移** — `SKILL.md` 随插件发布，模型靠它决定怎么调 CLI。新增三条回归锁（`crates/superpowers-kanban-runner/src/main.rs` 的 `the_skill_*`）：① 动词表**从 `USAGE` 解析**，技能必须给出每个动词的完整命令；② 状态名**从 `CardState` 枚举定义解析**，`list` 会打印的每个状态名技能都必须解释；③ frontmatter `description` 必须覆盖 enqueue / promote / merge / status 四类意图（否则那类说法根本不会加载技能）。判据：`cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner the_skill`（3 passed）；变异验证：删掉技能里的 `purge` 或 `Paused` 后对应测试分别失败
- [x] skill 覆盖**触发面之外的正文** — 补 `superpowers-kanban archive|purge|list --all|manifest|state`、全部 8 个 `CardState` 词表、默认 `--state-dir` 取 **cwd** 的陷阱（卡片会话 cwd 是实现 worktree，裸跑 `add`/`list` 会指向空目录）、`done` 的兼容语义。判据：同上的 `the_skill_*` 三条
- [x] skill 与 `finishing-a-development-branch` 的冲突裁决 — 对方 skill 的 "Option 1: Merge locally" 会直接 `git checkout base && git merge`，绕过多项目唯一的 `merge.lock`。`SKILL.md` §6 显式裁决：卡片 worktree 内一律走 `merge-request` → `merge-finish`；用户要在看板外手工合并则让其自己执行、再走 `done` 登记；普通分支不插手。判据：`SKILL.md` 的 §6 + `the_skill_documents_every_cli_verb`
- [x] **并发配额按「全机运行会话数」判定** — 限流限的是总流量，配额判定从「只看板内 `Running` 卡数」改为「整机峰值 − 宿主上报的全机运行会话数」：宿主每拍在 `driver/next` 里下发 `sessions.running`（口径：`active_turn_id` 有值或状态 `Running`；`AwaitingApproval` 与人闲不计），插件算 `budget = limit − running` 后只判 `budget > 0`，启动侧**不再二次减**本看板 `Running` 卡数（否则同一批会话被扣两遍），并发判定也不再看「卡片是否绑在某个状态槽」——只看会话是否在跑。峰值配置的唯一来源是整机 `~/.yi-agent/plugins/superpowers-kanban/superpowers-kanban.toml`，项目层即便还写 `default_max_tasks` / `window` 也忽略；`settings.read` 返回合并视图，`settings.write` 校验通过后按字段拆两半落盘。旧宿主不带 `sessions` 时退回**全机租约**口径（`budget = 峰值`，租约本就按活会话记、跨项目共享），绝不整拍报错。判据：`cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner`（164 passed，含 `outside_sessions_consume_the_same_budget`、`a_launch_is_allowed_when_the_whole_machine_has_room`、`a_running_card_is_not_subtracted_twice`、`a_running_count_above_the_peak_yields_zero_budget`、`a_host_that_does_not_report_sessions_falls_back_to_the_lease_pool`）；`cargo test -p superpowers-kanban-core`（116 passed）；宿主侧 `cargo test -p yi-agent-app-server --lib session_driver::tests`。变异验证：让 `launch_budget` 忽略宿主上报后，上述整机口径测试立即失败
- [x] **合并队列也吃全机并发额度** — 合并是一次真实的模型回合（在会话里调模型、跑 `git merge`），与实现会话抢同一份额度。`driver_step` 把余量 `budget = 峰值 − 宿主上报的全机运行会话数` 算在合并队列之前，`budget > 0` 才调 `promote_merge_queue`；余量为 0 时队首**留在 `merge_queued`**，不拿 `merge.lock`，下一拍余量回来再开——不设这道闸，峰值 4 的机器会出现「4 个实现会话 + 1 个合并」。次序仍是合并优先于启动，且合并开局即 `return`，同一拍不再起新会话。租约池大小仍按**峰值**开（`acquire_in` 按 `0..limit` 编号复用槽位，拿会变小的 `budget` 开池会让两批会话撞同一个槽）。判据：`cd plugins/superpowers-kanban && cargo test -p superpowers-kanban-runner --lib the_merge_queue_waits_when_the_whole_machine_has_no_room`；变异验证：去掉 `if budget > 0` 后该测试立即红（队首会抢走 `merge.lock` 却跑不起来）
- [x] 升级路径写清"两处都要换" — 覆盖二进制后 `run` 仍持旧 inode（需重启子进程），skill 是独立副本（需重新 `cp`，且新会话才生效）。判据：`plugins/superpowers-kanban/INSTALL.md` 的 §2a 与 §3a（含 `diff … && echo in-sync`）
- [x] **状态流转改由模型驱动（看板不再猜）** — 卡片状态收缩为 8 个（`PreQueued`/`Queued`/`Running`/`NeedsYou`/`AwaitingMerge`/`Done`/`Failed`/`Cancelled`），删除 `Launching`/`Merging`/`Paused`；迁移规则只剩「终态不可复活」。daemon 只写三件事：排队、启动成功、启动失败；`idle`/`git HEAD`/宿主重启**一律不再推断状态**。模型经新增动词 `state <id> <state> [--detail]` / `state --self <state>` 声明状态。判据：`crates/superpowers-kanban-core/src/card.rs` 的 `can_transition_to`；`cargo test -p superpowers-kanban-runner --lib` 里的 `an_idle_session_never_changes_a_running_card`、`a_session_the_host_forgot_never_changes_the_card`、`a_card_moves_only_where_the_model_puts_it`
- [x] **并发按活会话计，与卡片状态解耦** — 宿主在 `driver/next` 的 `threads[]` 里多报 `alive`；插件按观测持有/释放租约，一张卡被标 `done` 但会话还开着仍占名额。删掉 `occupies_slot`/`running_count`/`free_slots`。判据：`a_live_session_keeps_its_slot_even_after_the_card_is_done`；宿主侧 `cargo test -p yi-agent-app-server --lib -- session_driver` 的 `a_thread_that_left_the_host_table_is_reported_dead_on_the_next_tick`
- [x] **观测旁路 `observations.json`（只读，永不写回状态）** — daemon 每拍只记 thread 的 `alive` 与 `last_seen_alive_at`；`list` 派生 `stale`（卡写着 running 但会话没了）与 `last_alive_at`，桌面显示角标与最后活动时间。判据：`crates/superpowers-kanban-runner/src/observation.rs` 的单测 + `list_marks_a_running_card_stale_when_its_session_is_gone`；桌面 `npx vitest run src/lib/superpowersKanbanState.test.ts src/components/SuperpowersKanbanCard.test.tsx`
- [x] **删掉 git 驱动的收敛面** — 移除 `core/converge.rs`、`runner/reconcile.rs`、`runner/runner.rs`、`recover_cards_the_host_forgot`、`has_commits`、`MAX_MERGE_RETRIES`、启动时的 `git merge --abort`、`DriverAction::Append`。daemon 不再碰 git，只剩终态卡超期归档这类纯簿记。判据：`grep -rn "converge\|has_commits\|MAX_MERGE_RETRIES" plugins/superpowers-kanban/crates` 无结果；`cargo test -p superpowers-kanban-core`（108 passed）与 `cargo test -p superpowers-kanban-runner`（181 passed）
- [x] **会话知道「这张卡的状态归我管」** — objective 末尾追加职责说明（含 `state --self` 与 `SUPERPOWERS_KANBAN_CARD`）；`launch` 动作新增通用 `env` 字段，由插件给出键值、宿主只透传，`bash` 子进程因此看得见卡片句柄（宿主不认任何具体键名）。判据：`cargo test -p superpowers-kanban-runner --lib the_objective_tells_the_session_it_owns_the_card_state`；`cargo test -p yi-agent-app-server --lib the_session_env_reaches_the_bash_child`
- [x] **合并要用户确认，队列串行** — `MergeQueued` 是唯一的合并授权，`AwaitingMerge` 只表示「活儿干完了」。用户发话 → 模型 `merge-request <id>` **只入队**（置 `MergeQueued` + `merge_queued_at`），不拿锁；daemon 每拍按入队时刻 FIFO 取队首，**先要会话名额再要 `merge.lock`**（反过来会占着锁等额度、堵住本项目所有合并），拿到才置 `Running` 并为它新开一轮合并会话（文案 `queued_merge_objective` 明说名额已持有、不要再申请）。一拍只推进一个，推完即返回。`merge-cancel <id>` 只在 `MergeQueued` 上生效，退回 `awaiting_merge`——队列由 daemon 每拍推进，若退队走无条件写状态，就会在「用户反悔」与「daemon 跑了下一拍」之间留下竞态。谁在合并由 `merge.lock.holder` 表达（`list` 派生 `merging`，CLI 行尾 `[merging]`，桌面角标），**不设 `Merging` 状态**。独立合并卡（`add-merge`）出生即 `MergeQueued`，否则它会走实现卡会话通路、永远拿不到名额。合并回合一结束，**daemon 自己**在下一拍把名额还回去：锁是它进程里的 `flock`，会话里那句 `merge-finish` 落在另一个进程、只会回 `cleared`；不回收的话一次合并会把本项目此后所有合并永久堵死，而界面上还看不到有人在合。
判据：`cargo test -p superpowers-kanban-runner --lib` 的 `the_daemon_releases_the_gate_once_the_merge_turn_settles`、`merge_finish_clears_a_holder_record_left_by_a_dead_process`、`merge_request_only_queues_the_card`、`merge_cancel_`、`the_queue_head_starts_merging_when_the_gate_is_free`、`only_one_merge_starts_per_tick`、`a_held_gate_leaves_the_queue_untouched`、`a_delivered_merge_card_is_started_by_the_queue`、`list_marks_the_card_that_holds_the_merge_gate`；`cargo test -p superpowers-kanban-core --lib a_delivered_merge_card_is_born_queued_for_merge`；`cd desktop && npx vitest run src/lib/boardIndex.test.ts src/lib/superpowersKanbanBoard.test.ts src/components/SuperpowersKanbanCard.test.tsx`
- [x] 设置面板的时段并发窗口写明时区口径 — 窗口在插件进程内按 `chrono::Local` 判定，UI 原先只给 `09:00` 而不说是哪个时区的 `09:00`，桌面经 relay 连远端宿主时用户无从判断。修复：面板在「全机时段并发窗口」标题下加一行无条件说明（`desktop/src/components/SuperpowersKanbanPluginSettings.tsx`，`data-testid="window-timezone-note"`），`INSTALL.md` 6.5 节补同口径句子；判定逻辑、`superpowers-kanban.toml` 格式与 `plugin/settings/read` 载荷均未改动。判据：`cd desktop && npx vitest run src/components/SuperpowersKanbanPluginSettings.test.tsx`（12 passed，含 `states which clock the window hours follow`）— [设计](../superpowers/specs/2026-10-29-kanban-window-timezone-note-design.md)
