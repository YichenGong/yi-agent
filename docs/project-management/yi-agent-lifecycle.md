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
- [ ] 预览／确认／状态 DTO 与客户端方法 — 见 Task 14、16
- [ ] 受管进程 spawn 客户端 — 见 Task 8
