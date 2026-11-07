# yi-agent-lifecycle

## 模块说明

共享的**运行时生命周期控制协议**。它同时被桌面壳（Tauri）和 runtime 侧链接，因此
必须保持极轻：无 Tokio、无 SQLite、无 `yi-agent-core`、无 provider。设计见
[2026-11-02-shared-daemon-lifecycle-and-upgrade-design.md](../superpowers/specs/2026-11-02-shared-daemon-lifecycle-and-upgrade-design.md)，
实施计划见 [2026-11-02-shared-daemon-lifecycle-and-upgrade.md](../superpowers/plans/2026-11-02-shared-daemon-lifecycle-and-upgrade.md)。

## 范围边界

**做什么：**
- `types.rs`：线协议 DTO 与身份类型（`RuntimeScope`、`ArtifactIdentity`、
  `RuntimeIdentity`、`ClientGrant`、`RootAuthorization`、`OperationStatus`、
  `ReclaimResult`、`BundledArtifactManifest`）
- `codec.rs`：4 字节大端长度头 + JSON 体的有界帧，协议版本在帧内
- `deadline.rs`：单次操作的总预算，时间源为可注入的单调 `Clock`
- `client.rs`：阻塞式控制通道客户端（供桌面壳的专用 I/O worker 使用）

**不做什么：**
- 不做租约状态机（Task 4 加入 `leases.rs`）、不做服务端（supervisors 的 guardian）
- 不链接数据库或模型层——这是硬约束，见下方依赖守卫
- 不承担业务 RPC：业务仍走 `yi-agent-store` 原有 socket

## 关键不变量

- **帧上限包含 4 字节头**：`MAX_CONTROL_FRAME = 16384`，最大合法 body 为
  `16384 - 4`。解码在分配 body **之前**校验长度。
- **身份是字符串，不是 PID**：PID 会被复用，不能区分「登记的那次运行」和「继承了
  该号码的后续进程」。
- **凭据只出现在 `ClientGrant`**：`ClientGrant` 与 `RootAuthorization` 的 `Debug`
  是手写的，凭据一律打印为 `<redacted>`；结构化错误不回显凭据。
- **单一总预算**：`Deadline` 贯穿一次操作的所有 I/O，`clamp` 只取「请求值」与
  「剩余量」的较小者，不允许每对象各自计时从而随对象数相乘。
- **未知操作返回 `Unsupported`**，绝不静默成功。

## Features

- [x] 轻量 crate 注册进 workspace，依赖守卫成立 — `yi-agent-rs/Cargo.toml` 的
  `members` 与 `[workspace.dependencies]` 均含 `yi-agent-lifecycle`；依赖只有
  serde／serde_json／getrandom／libc。判据：`cargo tree -p yi-agent-lifecycle`
  输出中不含 `rusqlite`、`yi-agent-core`、`tokio`、任何 provider crate
- [x] 有界帧编解码 — `src/codec.rs::encode`／`decode`／`read_frame`／`write_frame`。
  判据：`cargo test -p yi-agent-lifecycle --test control_codec`（14 passed）覆盖
  超长、截断、短头、未知协议版本、非 UTF-8、恰好到顶与超一字节两种边界
- [x] 恰好到顶的一帧被接受、超一字节被拒 — `control_codec.rs` 的
  `frame_at_the_exact_ceiling_is_accepted` 与
  `frame_one_byte_over_the_ceiling_is_rejected`。判据：把 `codec.rs` 里的上限判断
  改成 `if false && ...` 后这两个用例与 `oversized_frame_is_rejected_before_body_allocation`
  一起失败（实测 RED），恢复后全绿
- [x] 编码侧不会写出撒谎的长度头 — `encode` 对超长 body 返回
  `io::ErrorKind::InvalidInput`。判据：`control_codec.rs` 的
  `encode_refuses_a_body_that_cannot_fit`
- [x] 总预算在注入时钟下可测 — `src/deadline.rs` 的 `Clock` trait 与
  `Deadline::with_clock`。判据：`control_codec.rs` 的
  `deadline_remaining_shrinks_without_wall_clock`、`clamp_never_exceeds_the_remaining_budget`、
  `zero_budget_is_expired_not_immediately_usable`
- [x] 控制客户端在真实 Unix socket 上往返且受预算约束 — `src/client.rs` 的
  `ControlClient::connect`／`request`。判据：`cargo test -p yi-agent-lifecycle
  --test control_client`（5 passed），其中 `a_server_that_never_answers_is_bounded_by_the_budget`
  证明对端不回复时客户端靠自己的预算退出
- [x] 凭据不进 Debug — `types.rs` 的手写 `Debug` 实现。判据：
  `control_client.rs` 的 `register_returns_a_grant_without_leaking_the_secret_in_debug`
- [x] 空预算在碰 socket 之前就被拒绝 — `client.rs` 的 `expired()`。判据：
  `control_client.rs` 的 `an_expired_budget_refuses_touching_the_socket`（错误类型为
  `TimedOut`）
- [x] `Unsupported` 不被当成成功 — `ControlResponse::Unsupported` 是独立变体。判据：
  `control_client.rs` 的 `a_rejected_response_is_not_a_success`
- [x] 可移植的 bundle 清单类型 — `types.rs` 的 `BundledArtifactManifest`：build id、
  display version、**相对 bundle** 的 executable 路径、sha256、schema 与
  handover／business 协议能力。判据：`cargo test -p yi-agent-lifecycle
  --test bundled_manifest`（3 passed，含
  `every_wire_field_survives_the_round_trip`——少一个字段桌面壳会在运行期
  拒绝清单，而不是在编译期报错）；字段与
  `desktop/src-tauri/src/runtime_manifest.rs` 的同名结构逐字段一致（桌面壳是独立
  workspace，靠契约测试而不是共享代码保持同步）
- [x] 前端租约状态机（`leases.rs`，纯内存、时间可注入） — `LeaseTable` 回答的是「谁还在保活这个
  runtime」。不执行任何 I/O，时间是从服务启动单调时钟映射来的 `Duration`，由调用方传入——这才让几个难题
  能在不 sleep 的情况下测：迟到的 EOF 撞上已重连的客户端、reclaim 发出之后才到的心跳、睡了一小时的笔记本。
  三条规则承担主要重量：**① EOF 不等于离开**——断连通常就是重连，所以 EOF 既不续租也不回收，租约继续按原
  截止时间走完（`DisconnectOutcome::AwaitReconnect`）；**② epoch 为重连设栅栏**——同一 `client_run_id`
  一生可以有多个连接，每次 `resume` 递增 epoch 并**轮换凭据**，上一个连接的迟到事件因此可被识别为 stale
  而不是歧义；**③ release／expire 是终态**——之后的心跳不能复活客户端（reclaim 已发出，复活等于为一个
  已经不存在的工作保活），`register` 也不能拿同一个 id 复活；tombstone 表按 `MAX_TOMBSTONES = 1024` 有界。
  凭据是 256-bit CSPRNG 十六进制，只存在于 guardian 内存与匿名交接 FD，`Debug` 打码。判据：
  `cargo test -p yi-agent-lifecycle --test client_leases`（21 passed），含
  `late_eof_does_not_remove_resumed_client`、`eof_does_not_renew_the_lease`、
  `a_late_heartbeat_after_release_cannot_revive_the_client`、
  `a_heartbeat_with_the_wrong_credential_is_refused_and_does_not_extend`、
  `a_system_resume_reopens_a_full_renewal_window`、`a_released_client_is_not_revived_by_a_system_resume`、
  `the_released_tombstone_table_stays_bounded`；**变异验证**：让 `disconnected` 不比对 epoch、让
  `heartbeat` 不查 tombstone → 2 个用例红（实测 RED）
- [x] 前端租约的客户端侧（`client.rs::FrontendLease`） — 把状态机接到真实 wire 上：`register` 换 grant，`start_heartbeat` 起一条**独立控制连接**
  的线程每 `HEARTBEAT_INTERVAL = 5s` 续约（50ms 分片睡眠，停起来是毫秒级而不是 5 秒），一次续约被拒即置 `lost` 且**不重试**——重试只会
  反复敲一个已经撤回该客户端的 guardian，而它发起的回收才是答案；`release` 先停心跳再发 `Release`；`await_result` 以 100ms 轮询
  `ReclaimStatus`，超时／guardian 消失／未知 operation 一律 `Ok(None)`（"答案没到"不能算成功）；`register_if_served` 在 socket 不存在或
  `ConnectionRefused` 时返回 `Ok(None)`——嵌入式模式没有 guardian，那不是错误。**`Drop` 只停心跳、不 release、不阻塞**：把离开与 panic
  分开，正是为了让一次崩溃不被当成用户退出。判据：`cargo test -p yi-agent-lifecycle`（`client::tests` 3 例，跑在真实 unix socket 上：
  `a_registered_lease_releases_and_reads_its_result`（注册 → 释放 → 读结果；
  并断言 release 之后不再出现 heartbeat）、`a_refused_heartbeat_marks_the_lease_lost_without_retrying`、
  `no_guardian_means_no_lease_rather_than_an_error`）；`cargo test -p yi-agent --test frontend_runtime_lease`（8 passed）
- [x] canonical scope 解析共享给桌面壳（Task 13） — `scope.rs` 把「一个 runtime 的
  身份就是它的**规范**目录」这件事从 supervisors 搬到本 crate：两个拼写（软链项目、
  macOS 的 `/var` 与 `/private/var`）必须解析成同一个 scope，否则它们各拿一把锁、
  各连一个 socket，却共用一个数据库——正是协调锁要防的损坏。首启时 runtime 目录
  还不存在，故 `canonicalize_allowing_missing` 规范化**最深的已存在祖先**再补回缺失
  尾巴，调用方在创建后 `ScopeExt::recheck`（尾巴本身可能是个稍后才出现的软链）。
  搬过来的理由是可链接性：桌面壳必须算出**同一个**路径才能找到 guardian 的 socket，
  而它刻意不链 supervisors（那里带 store ⇒ SQLite）。`GUARDIAN_SOCKET_FILE` 也由本
  crate 定义、supervisors re-export，避免两处各写一个字面量。判据：
  `cargo test -p yi-agent-lifecycle`（`scope::tests` 覆盖同目录两种拼写、缺失尾巴、
  `/var`↔`/private/var`、`runtime_directory` 的 `YI_AGENT_RUNTIME_DIR` 覆盖、
  `control_path` 落在 runtime 目录内）；`cargo tree --manifest-path
  desktop/src-tauri/Cargo.toml | grep -c rusqlite` 为 0
- [x] 预览／确认 DTO 与一次性确认状态机（Task 14） — `types.rs` 的
  `ImpactCounts`／`ImpactEntry`／`ImpactRevision`／`RestartPreview`／
  `ConfirmationContext`，以及 `preview.rs::PreviewStore`。三条判据把「确认」这件事
  钉死：(1) token 绑定发起者、scope、daemon 代际、**目标产物**与两个 revision 分量，
  任一项变化即 `Stale`，陈旧同意无法被放大成新范围；(2) 一次性消费，同 operation
  重试幂等、新 operation 返回 `AlreadyConsumed`，所以连点不会启动第二次切换；
  (3) 分页返回条目但 token 恒为空，读取权限不能变成授权。未知影响是一等答案：
  `coordinator: None` 使 `counts.complete=false` 并强制二次确认，绝不解释成 0。
  分页按 `serde_json` 实际字节切分（`PAGE_BYTES = 16 KiB`），长 id 不会撑破帧上限。
  判据：`cargo test -p yi-agent-lifecycle --lib preview`（15 passed），含
  `a_changed_revision_in_either_authority_makes_the_token_stale`、
  `a_different_target_artifact_makes_the_token_stale`、`a_token_is_consumed_exactly_once`、
  `a_token_expires_after_its_window`、`paging_returns_entries_but_never_a_token`、
  `a_page_is_bounded_by_serialized_size`、`unknown_impact_without_independent_fence_requires_manual_action`
- [x] 租约表提供影响快照所需的观测（Task 14） — `leases.rs` 的 `impact_revision`／
  `pending_count`／`snapshot`／`admits_or_observes`，以及 `disconnected` 只标记连接
  断开、**不**递增 revision。判据：`cargo test -p yi-agent-lifecycle --lib leases`
  （9 passed），含 `heartbeat_does_not_change_impact_revision`、
  `a_disconnected_client_is_pending_not_gone`、
  `a_stale_epoch_eof_does_not_clear_the_current_connection`；
  变异验证：让 `disconnected` 递增 revision → 对应用例红
- [x] 预览／确认／状态客户端方法与本地桌面壳入口（Task 16） — `client.rs` 的
  `FrontendLease::{preview, confirm, operation}` 与 `types.rs` 的
  `ControlRequest::Status`／`ControlResponse::Status(Box<RuntimeStatusSnapshot>)`。
  `Status` **刻意不要求 grant**：状态轮询不能隐式注册或续租租约，而 app-server 不允许
  持有 App 的可续租凭据；边界由 0700 目录 + 0600 控制 socket 提供，不由该帧携带秘密。
  `Status` 负载装箱是因为一个快照比该通道最小的应答大一个数量级，否则每个应答都要按最大
  的那个分配。`Lease` 记住 `kind`，`client_summaries()` 按 `client_run_id` 排序输出，
  分页只取一页而 `counts` 始终是全量。判据：`cargo test -p yi-agent-lifecycle`
  （33 lib + 21 `client_leases` + 3 `bundled_manifest` passed）
- [x] 桌面壳的只读状态与本地重启控制命令（Task 16） — `desktop/src-tauri/src/runtime_lifecycle.rs`
  的 `runtime_status`／`runtime_preview`／`runtime_confirm`／`runtime_operation`
  （+ `runtime_active_operation` 供 WebView 刷新后恢复轮询）。三条边界都能被验证：
  (1) **target 固定为壳自己的产物**，`preview` 只从 `DesktopRuntimeManager::artifact`
  取，未验证的构建（`tauri dev` 无清单）直接 `Unsupported`，绝不接受 WebView 提交路径；
  (2) **scope 只做查表**，`registered_scope` 从 `registered_projects()` 里找，凭空的 key
  得到「not open in this app」而不是一次指向任意目录的控制请求；(3) **operation id 在
  请求发出前就记下**，传输失败才撤销（失败的请求不是运行时接受的操作，留着会让 UI 轮询
  一个不存在的 id），成功后保留供重载恢复。判据：
  `cargo test --manifest-path desktop/src-tauri/Cargo.toml`
  （205 lib passed，含 `preview_always_targets_the_shells_own_artifact`、
  `an_unverified_shell_cannot_preview_a_restart`、
  `a_lost_confirmation_keeps_its_operation_id_for_recovery`、
  `a_confirmed_operation_is_remembered_for_a_reload`、
  `status_is_read_only_and_does_not_need_a_lease`、
  `a_matching_artifact_is_reported_as_the_same_build`、
  `no_answer_is_an_unknown_view_with_an_explanation`、
  `an_unregistered_scope_is_refused_rather_than_looked_up`）
- [x] 壳把「自己配套的产物身份」经本地握手交给 app-server（Task 16） —
  `shell_authorization.rs` 的 `ExpectedRuntime` 字段 + `DesktopRuntimeManager::artifact`。
  这是 `runtime/status` 能回答「是不是同一个 build」的唯一来源：没有它，配套正确的一对
  也会显示 `Unknown`。它**只是身份**——命名字节、不可续租、不授权任何东西，说谎只会得到
  自己项目的错误横幅；空 build id 在 `accept()` 被拒（一个空期望会把每个运行时都标成
  不匹配）。`expected_runtime` 只随本地 stdio 的 `initialize` 走一次。判据：
  `cargo test -p yi-agent-app-server --lib shell_authorization`（11 passed，含
  `the_expected_artifact_round_trips`、`an_empty_expected_build_id_is_refused`、
  `nothing_secret_is_serialized`）；`cargo test --manifest-path
  desktop/src-tauri/Cargo.toml` 的 `a_local_initialize_carries_the_shell_identity`、
  `an_unverified_shell_sends_no_artifact_expectation`
- [x] 受管进程 spawn 客户端（`managed/{client,fdpass}.rs`） — 工具侧通过 guardian 签发的私有
  owner-bound 连接启动进程：`ManagedSpawnClient::connect(OwnedFd)` 接管一条已有连接（没有可连的路径，
  连接只能由 guardian 交给它），`spawn` 把 stdin／stdout／stderr 三个**独立** FD 用 `SCM_RIGHTS`
  随帧送达，`wait`／`try_wait`／`terminate` 复用同一条连接并各占一帧。**owner 不来自请求**：它由服务端
  在连接建立时确定（`ChannelGrant`），请求里只有 program、args、env、cwd——因此工具无法以别人的身份
  spawn，也无法自己登记或续租。判据：`cargo test -p yi-agent-lifecycle --test managed_fdpass`
  （5 passed），含 `three_standard_descriptors_arrive_as_three_distinguishable_streams`、
  `more_descriptors_than_the_protocol_allows_are_refused_before_sending`、
  `a_spawn_receipt_round_trips_through_the_wire_codec`、
  `a_silent_peer_times_out_instead_of_wedging_the_serve_loop`；**变异验证**：让 `recv_with_fds`
  丢弃辅助数据 → 三流用例红（实测 RED）

- [x] 控制协议可以向前端签发私有 spawn 通道（Task 11／12／18 的接线前提） —
  `ControlRequest::OpenManagedChannel { grant }` 是唯一会应答描述符的控制请求：guardian 建 `socketpair`，
  在自己线程上按共享 registry 服务 guardian 端，回 `ControlResponse::ChannelOpened { owner, generation }`
  并把客户端端作为 `SCM_RIGHTS` 辅助数据随帧发出。**请求不能自报 owner**，owner 取自 grant，
  由服务端用租约表解析（lease-only 观察者、已释放客户端、过期 epoch 都在开通道之前被拒）。
  前端侧 `FrontendLease::open_managed_channel`／`open_channel_with`／`control_path` 是配套的客户端封装；
  `open_channel_with` 复用已有 grant 而不重新 register，因为重复 register 会给同一进程造出第二条租约，
  让「最后一个前端离开」无法判定。判据：`cargo test -p yi-agent-supervisors --test managed_channel_authority`
  （3 passed：业务租约拿到绑定自身 owner 的通道、lease-only 被拒、已释放客户端被拒）；
  `cargo test -p yi-agent-lifecycle`（33 + 3 + 21 + 5 + 14 + 4 passed）
- [x] `Wait` 的预算随请求传递，不再借用通道读超时 — 通道的读超时是为了发现死掉的对端，不是用来限制一次合法等待；
  把两者混用会让跑得比通道预算久的命令被答成 `group_stopped: false`、无退出码，调用方读作 `exit: -1`。
  现在 `Wait { process_id, budget_secs }`（`#[serde(default)]`，0 表示老客户端，服务端退回通道预算=旧行为），
  客户端读超时按自身 deadline 放宽，服务端提前 `REPLY_MARGIN = 100ms` 收尾，避免"服务端刚写完"与"客户端刚好读超时"撞在一起。
  判据：`cargo test -p yi-agent-supervisors --test managed_channel`（7 passed + 1 ignored），
  `a_wait_is_bounded_by_the_requests_own_budget` 用 1s 请求预算对抗 120s 通道预算
- [x] guardian 感知启动者退出，且测试不再留下孤儿 guardian — 此前 `main.rs` 用
  `let _bootstrap = take_guardian_bootstrap()?` 只**持有** bootstrap socketpair 却从不读它，而
  socketpair 的 EOF 正是「启动者已退出」的信号；不读就永远收不到。叠加主循环唯一退出条件
  `while !shutdown()`（生产路径传 `|| false`），guardian 一旦起来就不走。结果是每跑一次
  `managed_runtime_lifecycle` 留 3 个 guardian，实测 42 个全部 `ppid=1` 且指向已删除的 `TempDir`。
  修复分三层：**(1)** `LauncherLifeline::watch(fd)` 起 detached 线程读该 fd，EOF（`Ok(0)`）或不可恢复
  读错误即置位 `is_gone()`；`Ok(_)` 继续读——这条 fd 是生命线，不接受任何载荷，否则它会悄悄变成
  启动者从未同意服务的命令通道。**(2)** 主循环新增退出条件 `launcher_gone() && no_business_client()`。
  只看 lifeline 是不够的：spec A04 要求「重启最初启动者的 sidecar，其他客户端、guardian/daemon
  不误停」，所以启动者退出不能拆掉别的 App/TUI 正在用的 runtime。`no_business_client()` 刻意**弱于**
  `drained()`——后者要求 `global_drain_started()`，而那只在有人 release 之后才置位，启动者猝死时
  没人 release，等 `drained()` 会永远等下去；同时也排除 `has_running_operations()`，因为正在进行的回收
  就是「证明停止」的那条线程，退出会把它切断。**(3)** guardian 在 `bind_control` 成功之后写
  `runtime/guardian.pid`（`{"pid":N,"start_micros":M}`，带出生时间防 pid 复用），因为
  `processes.json` 记的是 managed 子进程、第一条是 daemon 而不是 guardian：测试原先读它做兜底清理，
  杀掉的是 daemon，而 guardian 会把 daemon 重新拉起，清理看似成功实则自我撤销。
  判据：`cargo test -p yi-agent --test managed_runtime_lifecycle`（9 passed + 1 ignored，跑完
  `ps -axo command= | grep -c 'internal-runtime guardian'` 由 3 降为 0）；
  `cargo test -p yi-agent-supervisors launcher_lifeline`（1 passed，验证在对端存活时 `is_gone()` 为
  false、关闭后 10s 内变 true）
- [x] guardian 控制 socket 不再因 `sun_path` 超长而静默起不来（深路径项目）— `sockaddr_un.sun_path`
  只有 104 字节（macOS/BSD），而 runtime 目录默认是 `<workdir>/.yi-agent/runtime`。项目路径深于
  约 85 字节时 `<runtime_dir>/guardian-control.sock` 就越界，`bind` 直接失败
  `AF_UNIX path too long`。本仓库自身的 runtime 目录 99 字节，控制 socket 121 字节，**必然触发**。
  致命之处在于失败是静默的：launcher 用 `Stdio::null()` 丢弃 guardian 的 stderr，guardian 立刻退出，
  没人应答控制 socket，调用方于是等满 `STARTUP_BUDGET`（30s）才报
  `the managed runtime did not become ready within 30s`，并顺带关掉本会话的子 agent 委派。
  用户可见症状正是「TUI 里说句话要等几十秒」「提示不能用 subagent」。
  修复：把路径计算收敛到 `yi-agent-lifecycle::scope::control_path` 一处，超长时回退到
  `$TMPDIR/guardian-<sha256(运行目录)[..16]>.sock`；哈希对象是 **canonicalize 后的**目录，
  否则 macOS 的 `/var` 与 `/private/var` 两种拼写会算出两个名字，guardian 绑一个、客户端轮询另一个，
  症状与原 bug 完全相同。`control_socket` 是 panic-free 的严格版，供真正要 `bind` 的一方拿诊断。
  embedded 路径早已有同类兜底（`yi-agent-store::ipc::socket_path_for`），漏的只有 managed 的
  guardian 控制 socket。判据：`cargo test -p yi-agent-lifecycle --lib scope`（14 passed，含
  「深路径解析出的 socket 真能 bind」与「两种拼写同名」）；
  `cargo test -p yi-agent --test managed_runtime_lifecycle`（13 passed + 1 ignored，新增
  `a_guardian_starts_for_a_runtime_directory_too_deep_for_a_direct_socket`，用 90 字符深的
  runtime 目录跑真实 `ensure_managed`）。实测 A/B：未修复二进制 `refused:prepare:...did not become
  ready within 30s`（30.3s），修复后 8 个启动阶段全过（14.4s，`run --subagents` 与真实 TUI pty 两条路径各验一次）
