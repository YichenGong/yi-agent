# Managed Runtime Production Integration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 接通默认 managed 生产路径，使项目准备、真实前端租约、逐会话授权、插件需求、存量迁移和退出回收形成可验收闭环。

**Architecture:** 沿用 guardian、FrontendLease、foreground root prepare/commit、进程 registry 和版本切换设计。桌面壳持有 App 的可续租凭据，通过私有本地 stdio 管理面调用 app-server 的 bring-up；业务连接与逐 root 授权分离。迁移独立于普通重启，证据不足一律人工处理，插件仅由真实 live client demand 驱动。

**Tech Stack:** Rust workspace、轻量 yi-agent-lifecycle、Unix socket/FD/flock、Tauri 2、React 19、TypeScript、Vitest、Cargo integration tests。

## Global Constraints

- 依据 [spec](../specs/2026-11-05-managed-runtime-production-integration-design.md)，代码调查基线 `7c32a47c`；执行前重核实际文件及 main 差异，不照搬旧行号。
- macOS 关闭窗口只是隐藏；真正退出 App 才释放 App 的项目租约。
- App 退出回收它名下的工作，不影响同项目仍存活的 TUI。
- 最后一个前端离开后有界回收 runtime；插件安装、看板登记、历史记录不单独保活。
- 保留同一项目数据库、历史、交付与工作目录；不自动重放业务副作用。
- 明确手工启动的 daemon 保留 unmanaged 语义，不全局修改公开 CLI 行为。
- 不将 `default_for_build` 全局改成 Managed。受管桌面、TUI、headless 入口显式选择 `for_managed_frontend`。
- Tauri 继续只依赖轻量 lifecycle crate，不链接 supervisors/store/SQLite。
- `managedReady` 仅指 scope lifecycleReady；每个会话另须 rootReady。插件不虚造 root。
- managed 路径缺租约、心跳、root 授权、driver 或 spawn 通道时拒绝新工作，不静默 embedded/direct spawn。
- 不操作当前正式安装包、daemon、launchd job 或 relay；真实进程仅在隔离实例执行。不调用真实 LLM。
- 变更只在 worktree 新分支；每组需求同步进度文档；每个代码提交前 `cargo fmt --all`、`just ci` 全绿，合并后再跑一次。纯文档提交免 CI/单测。
- 所有测试在自己的 target 串行编译；范围测试按 crate 跑。最终仍须执行 `just ci`，不能用范围测试替代。
- 默认启动总预算沿用 `STARTUP_BUDGET = 30s`；退出沿用壳已有全局 15s，不按项目累加。迁移同一总 deadline 贯穿锁、停止、启动。

---

## 0. 文件职责与执行边界

这是一个有顺序依赖的纵向修复，不是互不依赖的新子系统。先完成底层、再接入口，最后验收；各任务独立测试和评审，中间提交不宣称生产接入完成。

路径缩写仅用于本表，下文 Files 使用完整路径：

| 单元 | 文件职责 |
|---|---|
| `yi-agent-lifecycle/src/production.rs`（新） | 共享非业务管理 DTO、逐 root 授权索引；不依赖 SQLite |
| `yi-agent-supervisors/src/lifecycle/bringup.rs` | 统一带锁/持 guard 启动内核、显式项目身份 |
| `yi-agent-app-server/src/server/local_management.rs`（新） | 本地管理鉴权、prepare、授权快照与意图 broker；不进入业务 RPC 路由 |
| `desktop/src-tauri/src/runtime_management.rs`（新） | 私有 stdio pending/epoch、逐 root 回调、管理响应截获 |
| `desktop/src-tauri/src/runtime_lifecycle.rs` | 实际持租约、项目准备状态、退出屏障 |
| `yi-agent-subagent/src/managed_binding.rs`（新） | 无预先项目 root 的 managed 连接与逐 root 授权 provider |
| `yi-agent-supervisors/src/lifecycle/legacy_evidence.rs`（新） | 只读生产证据、兼容性与人工阻塞原因 |
| `yi-agent-supervisors/src/lifecycle/migration_operation.rs`（新） | instance 锁、write-ahead 日志、确认幂等与阶段对账 |
| existing demand/retirement/plugin modules | 接线而非重造表和进程托管 |
| `docs/testing/managed-runtime-production-integration.md`（新） | A01–A21 实际验收证据，未执行不写通过 |

新增文件须在所属 crate 的 `lib.rs`/`mod.rs` 显式声明。不把秘密类型导出到 ts-rs，不生成面向页面的 capability 绑定。

### 执行前准备

- [ ] 在主仓库只读运行 `git status --short && git worktree list && git log -3 --oneline`，确认没有混入其他任务。加载 using-git-worktrees；由已审阅的文档分支/其提交建立实施 worktree，不在 main 改代码。
- [ ] 用 `git grep -n` 重核本计划 Files 中的符号；若 main 已拆文件，只调整位置并在计划记证据，不改变安全契约。
- [ ] 运行 `(cd yi-agent-rs && cargo test -p yi-agent-supervisors --test managed_bringup) && (cd desktop && npx tsc --noEmit)` 记录基线；失败先定位既有失败，不将其伪装成新用例的 RED。

## 1. 跨任务接口契约

以下是本计划**要新增**的接口，不暗示它们已存在。复用实际 `RuntimeScope`、`RuntimeIdentity`、`ArtifactIdentity`、`RootBindIntent`、`RootAuthorization`、`SecretCapability` 和 `Deadline` 类型。由 Task 1 定稿 DTO；后续任务不得另造拼写不同的镜像。

```rust
// yi-agent-lifecycle/src/production.rs
use crate::{foreground::{RootBindIntent, SecretCapability},
    types::{ArtifactIdentity, RuntimeIdentity, RuntimeScope}};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrepareRequest {
    pub scope: RuntimeScope,
    pub expected: ArtifactIdentity,
    pub attempt_id: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrepareOutcome {
    Connected(RuntimeIdentity),
    VersionChoiceRequired(RuntimeIdentity),
    LegacyChoiceRequired { reason: String },
    Blocked { reason: String },
    Unknown { reason: String },
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RootAuthorityKey {
    pub scope_key: String,
    pub generation: String,
    pub client_run_id: String,
    pub thread_key: String,
    pub root_id: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootAuthorityEntry {
    pub key: RootAuthorityKey,
    pub capability: SecretCapability,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootAuthoritySnapshot {
    pub sidecar_epoch: String,
    pub scope_key: String,
    pub revision: u64,
    pub entries: Vec<RootAuthorityEntry>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootIntentRequest {
    pub sidecar_epoch: String,
    pub scope_key: String,
    pub client_run_id: String,
    pub intent: RootBindIntent,
}
// Deadline 不序列化。接收方钳制到自己配置的剩余预算，不信任对端延长窗口。
```

私有管理方法固定为 `_shell/bootstrap`、`_shell/prepare`、`_shell/rootIntent`、`_shell/replaceRoots`、`_shell/migrationPreview`、`_shell/migrationConfirm`、`_shell/migrationOperation`。request id 为带 epoch 的字符串，业务页面仍使用数字 id。bootstrap 第一帧由原生端直接写 stdin，携带仅该 session 使用的随机 `SecretCapability`；后续私有帧校验它。`_shell/` 全命名空间在 generic `rpc`/`rpc_respond` 拒绝，不仅上述七个方法。bootstrap 未完成时不开放 generic 写入。

页面只得到脱敏 `ProjectAcquireView { scope_key, attempt_id, state, reason, retry }`；Rust DTO 必须声明 `#[serde(rename_all = "camelCase")]`，页面字段为 `scopeKey/attemptId`，由同一 DTO 生成绑定并做 JSON round-trip 测试，不手写另一份镜像。`state` 为 `starting | managedReady | legacyChoiceRequired | blocked | unknown`。已识别 managed 构建不同时仍进入既有版本预览/切换 UI，不混称 legacy；准备先返回 blocked 并保留对应 VersionChoiceRequired 详情，不自动切换。仅控制协议已验证兼容时，Task 3 登记实际 Business 租约供现有 owner-bound preview/confirm 使用，持有不等于 ready；root 授权、业务和插件 demand 仍关闭。不采用不存在的 mode 升级：同代际凭 grant resume，不重复 register；成功切换后核实新代际并在新 guardian 注册，取消/关闭/退出按实际 held 集合处理。未知/不兼容控制协议仅 blocked/unknown，不为显示 UI 强行登记。

### Task 1: 固定启动 scope、lifeline 与共享 DTO

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-lifecycle/src/production.rs`
- Modify: `yi-agent-rs/crates/yi-agent-lifecycle/src/lib.rs`
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/src/lifecycle/bringup.rs`
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/src/lifecycle/guardian.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/managed_runtime.rs`
- Test: `yi-agent-rs/crates/yi-agent-supervisors/tests/runtime_scope.rs`
- Test: `yi-agent-rs/crates/yi-agent/tests/managed_roles.rs`

**Interfaces:** Consumes existing RuntimeScope/claim/role bootstrap. Produces §1 DTO and explicit canonical project propagation. Add `validate_managed_hello(expected_scope: &str, observed_scope: &str) -> std::io::Result<()>` in managed_runtime.rs, tested from its module tests; managed role never accepts absent hello.

- [ ] **Step 1: RED tests.** Add an actual launcher fixture to `runtime_scope.rs`: original project cwd != runtime_dir, symlink maps to canonical project, child reports project/instance/runtime-dir through an isolated pipe. In managed_runtime.rs add:
  ```rust
  #[test]
  fn mismatching_nonempty_scope_is_refused() {
      assert!(validate_managed_hello("scope-a", "scope-b").is_err());
      assert!(validate_managed_hello("scope-a", "").is_err());
      assert!(validate_managed_hello("scope-a", "scope-a").is_ok());
  }
  ```
  Extend managed_roles process tests: bootstrap EOF/timeout yields nonzero and no business socket. Serialize RootAuthorityEntry with a real test secret and assert `format!("{entry:?}")` does not contain its plaintext.
- [ ] **Step 2:** Run `(cd yi-agent-rs && cargo test -p yi-agent-supervisors --test runtime_scope && cargo test -p yi-agent --bin yi-agent mismatching_nonempty_scope_is_refused && cargo test -p yi-agent --test managed_roles)`; require RED attributable to wrong cwd/hello/new contract, not unrelated compilation failures.
- [ ] **Step 3:** Add DTO above; pass canonical project explicitly in the guardian launch env/claim and validate it against scope. Replace the permissive hello expression with the helper:
  ```rust
  pub(crate) fn validate_managed_hello(expected: &str, observed: &str)
      -> std::io::Result<()> {
      if observed.is_empty() || expected != observed {
          return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied,
              "the guardian authorized a different runtime scope"));
      }
      Ok(())
  }
  ```
  The expected value comes from resolved RuntimeScope.key, not an independently guessed workdir path. Treat `read_lifeline_hello -> None` as startup refusal for the managed role. Preserve instance home, model config, runtime-dir overrides and daemon lifeline death monitoring.
- [ ] **Step 4:** Re-run Step 2 and `cargo test -p yi-agent-lifecycle`; confirm no socket after refused launch and no legacy change for manual `daemon serve`.
- [ ] **Step 5:** Update desktop progress with partial evidence, run `cargo fmt --all && just ci` in yi-agent-rs, stage only Task 1 files plus progress docs, commit `fix: validate managed runtime launch identity`.

### Task 2: 本地 stdio 管理面与去重 prepare

**Files:**
- Create: `desktop/src-tauri/src/runtime_management.rs`
- Modify: `desktop/src-tauri/src/bridge.rs`
- Modify: `desktop/src-tauri/src/lib.rs`
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/server/local_management.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/transport/mod.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/transport/stdio.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/attach.rs`

**Interfaces:** Consumes PrepareRequest/Outcome. Produces native `ManagementSession::request(method: &str, params: serde_json::Value, deadline: Deadline) -> Result<serde_json::Value, String>`, `intercept(frame: &serde_json::Value) -> bool`, `close()`. Server `prepare_project(request: PrepareRequest, deadline: Deadline) -> PrepareOutcome` uses validated local artifact and common bring-up, never attaches a root. Both sides track immutable sidecar epoch. Native ManagementSession owns its pending map and bootstrap secret; it is not an exported Tauri command.

- [ ] **Step 1: RED.** In bridge unit tests reject all `_shell/` methods and string management IDs from generic responses. In local_management.rs test unbootstrapped, wrong secret, websocket/relay/SSH and expired epoch yield no call to an injected prepare counter. Tests must inspect emitted frontend events and show zero private frames/secrets. Two identical concurrent prepares must increment the launcher counter once; different scopes must not serialize behind one global IO lock.
  ```rust
  #[test]
  fn reserved_methods_cannot_be_forwarded() {
      assert!(reject_reserved_method("_shell/prepare").is_err());
      assert!(reject_reserved_method("_shell/newFutureMethod").is_err());
      assert!(reject_reserved_method("thread/resume").is_ok());
  }
  ```
- [ ] **Step 2:** Run `cargo test --manifest-path desktop/src-tauri/Cargo.toml --lib bridge && cargo test --manifest-path yi-agent-rs/Cargo.toml -p yi-agent-app-server --lib local_management`; verify targeted RED.
- [ ] **Step 3:** Implement native bootstrap before publishing the local slot as writable; existing tauri-plugin-shell child stdin is the inherited private FD, no extra env secret or replacement process framework needed. Gate generic writes until bootstrap ACK; remote slots retain existing launch behavior without bootstrap. Add:
  ```rust
  fn reject_reserved_method(method: &str) -> Result<(), String> {
      if method.starts_with("_shell/") {
          Err("reserved native management method".into())
      } else { Ok(()) }
  }
  ```
  Invoke it before write_line; generic rpc_respond only accepts registered business numeric requests. Intercept management responses and rootIntent before `app.emit`; correlate epoch/string id and secret, bound payload size to existing transport frame limit, clear pending on timeout/termination/shutdown. Local server dispatches private methods before business routing, requires authenticated local stdio; never registers these in model/relay RPC lists. Prepare calls `for_managed_frontend` explicitly and caches in-flight attempts only, not transient failures. Artifact path/expected identity come from verified native launch context, not mutable page fields.
- [ ] **Step 4:** Re-run Step 2; add stalled prepare + session close and late response tests. Native pending count must return to zero, no queued management resend after reconnect.
- [ ] **Step 5:** Update progress; `(cd yi-agent-rs && cargo fmt --all && just ci)` and Tauri lib suite; commit `feat: add private local runtime preparation channel`.

### Task 3: 壳租约与项目 lifecycleReady

**Files:**
- Modify: `desktop/src-tauri/src/runtime_lifecycle.rs`
- Modify: `desktop/src-tauri/src/runtime_management.rs`
- Modify: `desktop/src-tauri/tests/quit_drains_runtime.rs`
- Modify: `desktop/src-tauri/src/bridge.rs`
- Modify: `desktop/src-tauri/src/lib.rs`
- Modify: `desktop/src-tauri/build.rs`
- Modify: `desktop/src-tauri/capabilities/default.json`

**Interfaces:** Consumes Task 2 prepare. Produces `ProjectAcquireView` from §1 and `DesktopRuntimeManager::ensure_project(&self, scope: &RuntimeScope, budget: Duration) -> Result<ProjectAcquireView, String>` coordinated by existing acquire command, plus a native `authorize_intent(RootIntentRequest, Deadline)` used in Task 4. No grant crosses stdio. Native held leases separate from ready views; ClientKind::App lease mode Business. Compatible version-choice may hold a lease while blocked; every business authorization also requires lifecycleReady, never held alone.

- [ ] **Step 1: RED.** Extend injected ControlBackend tests: held=false never managedReady/owned; prepare→register→heartbeat ordering; heartbeat thread failure leaves no usable ready view; scope A blocked does not block B; root failure after lease success leaves lease in quit barrier. Paused-clock tests: Cmd+Q while prepare or register is in flight, late acquired lease released without root execution; single 15s quit budget.
  ```rust
  // Add beside existing runtime_lifecycle tests using their fake backend.
  // The fake records these exact operation labels.
  assert_eq!(trace, ["prepare", "register_business", "start_heartbeat"]);
  assert_eq!(view.state, "managedReady");
  assert!(manager.registered_projects().contains(&scope_key));
  ```
- [ ] **Step 2:** Run `cargo test --manifest-path desktop/src-tauri/Cargo.toml runtime_lifecycle && cargo test --manifest-path desktop/src-tauri/Cargo.toml --test quit_drains_runtime` to capture RED.
- [ ] **Step 3:** Split per-scope state into actual held lease and acquire view, use attempt fencing. Freeze admission/respawn before draining all held resources. Implement transition ordering:
  ```text
  observed Connected + valid scope/build -> register Business
  register success -> record held immediately -> heartbeat
  heartbeat success + identity still current -> managedReady
  any failure -> blocked/unknown; release held on abandoned attempt
  shutdown -> reject prepares and root intents; revoke late results
  ```
  Map VersionChoiceRequired to the existing version switch flow; no legacy flag for a managed mismatch. Add tests against the real lease table: compatible mismatch→Business registration→preview/confirm allowed while rootIntent/demand/turn refused; same-generation reconnect uses resume; cancelled choice stays blocked and quit releases held; new generation registers once; WaitingNextGeneration never ready. Do not invent LeaseOnly promotion or re-register a live/released run id. Native authorize_intent validates lifecycleReady in addition to live held scope/client, app-server epoch and server-origin thread/workspace intent. Commands accept registered scope keys only; update lib/build/capability command manifests together if adding query commands. Never expose authorize_intent itself as a page invoke.
- [ ] **Step 4:** Re-run Step 2, `cargo test --manifest-path desktop/src-tauri/Cargo.toml --test app_command_capability`; `cargo tree --manifest-path desktop/src-tauri/Cargo.toml` must still contain no rusqlite/store/supervisors dependency.
- [ ] **Step 5:** Update progress, workspace fmt/CI plus `cargo fmt --manifest-path desktop/src-tauri/Cargo.toml --all`; commit `fix: require real shell leases before project readiness`.

### Task 4: 逐会话 root 授权、attach 与 foreground driver

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-subagent/src/managed_binding.rs`
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/lib.rs`
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/attach.rs`
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/binding.rs`
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/thread_root.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/shell_authorization.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/assembly.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/foreground.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/local_management.rs`
- Modify: `desktop/src-tauri/src/runtime_management.rs`

**Interfaces:** Consumes RootIntentRequest/RootAuthoritySnapshot. Produces `RootAuthorityStore::replace(snapshot) -> Result<(), String>` and `get(&RootAuthorityKey) -> Option<SecretCapability>`, scoped to the app-server session, never process-global. Expected epoch is fixed by authenticated bootstrap, not adopted from incoming snapshots; revisions are per scope, empty entries revoke that scope, every entry matches current scope/generation/client, and generation change revokes old entries before reuse. Tests must reject a higher-revision snapshot from the wrong epoch as well as an empty stale snapshot. New subagent trait:
```rust
pub trait ManagedRootAuthority: Send + Sync {
    fn authorize(&self, intent: &yi_agent_lifecycle::production::RootIntentRequest,
        deadline: yi_agent_lifecycle::deadline::Deadline)
        -> Result<yi_agent_lifecycle::production::RootAuthorityEntry, String>;
}
```
App-server provider brokers through Task 2/3; CLI provider uses its own FrontendLease in Task 5. Business driver binds with the existing lifecycle RootAuthorization returned by guardian. Convert the entry to that type at the consumer, do not add a competing authority system.

- [ ] **Step 1: RED.** In managed_binding.rs module tests create two roots `thread:a`/`thread:b` in one project and assert distinct actual root ids, capabilities, child budgets; workdir-as-root-id lookup must fail. Test scope A snapshot update preserves B, stale epoch/revision rejected, mismatched key/client refused, extra_roots preserved. In server foreground tests, live owner TUI prevents App takeover; absent authorization or bind failure prevents turn/tool start.
  ```rust
  // RootAuthorityStore unit test; keys/entries made with real SecretCapability.
  store.replace(snapshot_a_v2.clone()).unwrap();
  assert!(store.replace(snapshot_a_v1).is_err());
  assert_eq!(store.get(&key_b), Some(cap_b));
  assert!(store.get(&key_from_project_path).is_none());
  ```
  Add recovery test: recovery-only entry cannot bind/run; previous capability proof plus old owner gone follows existing prepare/confirm recovery, no owner theft by thread key.
- [ ] **Step 2:** Run `(cd yi-agent-rs && cargo test -p yi-agent-subagent --lib && cargo test -p yi-agent-app-server --lib foreground && cargo test -p yi-agent-supervisors --test foreground_root_lifecycle)`; ensure new assertions demonstrate the old connection/root coupling fails.
- [ ] **Step 3:** Remove managed project-level pre-attach and global ROOT_CAPABILITIES path map. Introduce connection-only RuntimeBinding variant so it can resolve socket/identity without inventing AttachedRoot. ThreadRoot managed branch is:
  ```text
  resolve current connection generation
  send RootIntentRequest(thread key, workspace, exact extra_roots)
  guardian Prepare -> Challenge -> Authority -> Committed
  install matching scoped snapshot; authenticated attach to actual root
  bind foreground driver -> rootReady
  ```
  现有 managed attach 将 guardian capability 作为 `AttachApplicationRoot.idempotency_key`
  发送（attach.rs::attach_to_answer）；复用此鉴权接线，而非继续发送裸 thread key。
  stable thread key 仅用于 RootBindIntent/daemon root reservation，ThreadRoot 的 managed
  attach 改送精确匹配授权的 capability，daemon 返回的 actual root id 必须一致。
  managed 桌面路径必须绕开当前 `ForegroundDispatch::connect`/`ForegroundHost::connect`
  注册 `app-server:{pid}` 独立前端的分支，只消费壳下发的 root authorization；没有
  grant 的 sidecar 不得自行 register/renew。CLI provider 才可持自己的 FrontendLease。
  driver bind operation id 由 scope/root/sidecar epoch 组合生成，每 root 稳定，不能
  让所有线程共享现有 `app-server-bind:{pid}`。加 guardian 租约表断言：三个 App
  会话仍只有一个 App client，无 app-server PID client；壳退出后孤儿 sidecar 不能续租。
  Reuse authoritative daemon root id returned by the existing exchange, not derive an independent client id. Add authenticated attach wire only through existing managed authorization path; legacy AttachApplicationRoot remains isolated to explicit legacy. Authorization intent must run outside the stdio main-loop dispatcher awaiting it (spawn a worker/future and service incoming response concurrently); do not deadlock request processing. rootReady cached together with generation; repair invalidates only affected root(s). Failure returns a typed managed admission error, not Ok(None) legacy. Keep single-shot business retry policy unchanged.
- [ ] **Step 4:** Repeat Step 2 plus `cargo test -p yi-agent-supervisors --test foreground_stop_proof` and app-server local_management tests. Add a new project after initialize and sidecar reconnect test: no App restart needed; same App run id, no duplicate lease, fresh snapshot epoch; stale capability never accepted.
- [ ] **Step 5:** Update progress; fmt/CI and Tauri targeted suite; commit `feat: bind managed authority to individual conversation roots`.

### Task 5: TUI/headless 与 repair 的 fail-closed 接线

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent/src/main.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/frontend_lease.rs`
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/attach.rs`
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/managed_binding.rs`
- Modify: `yi-agent-rs/crates/yi-agent-subagent/src/binding.rs`
- Test: `yi-agent-rs/crates/yi-agent/tests/frontend_runtime_lease.rs`
- Test: `yi-agent-rs/crates/yi-agent/tests/managed_runtime_lifecycle.rs`

**Interfaces:** Consumes ManagedRootAuthority. Produces explicit managed frontend startup using `for_managed_frontend`, an owned lease provider for CLI, and mandatory managed process spawner. Embedded/manual entry retains its existing separate branch.

- [ ] **Step 1: RED.** Extend frontend_runtime_lease tests with operation trace and injected guardian refusal. Assert no attach before register/heartbeat; missing guardian after managed preparation, heartbeat failure, open_managed_channel refusal all yield no work/direct spawn. Test transient lease loss during execution freezes new admission and follows existing drain semantics without replay.
  ```text
  expected trace: prepare, register_business, heartbeat, authorize_root,
                  attach_root, bind_driver, open_managed_channel, admit_turn
  refused at any stage: subsequent labels absent
  ```
  Existing explicit manual daemon and embedded test branches must retain their prior traces.
- [ ] **Step 2:** Run `(cd yi-agent-rs && cargo test -p yi-agent --test frontend_runtime_lease && cargo test -p yi-agent --test managed_runtime_lifecycle)` to capture RED.
- [ ] **Step 3:** Reorder attach_managed_tui_runtime/headless helpers to use connection-only prepare, mandatory Business lease, root provider, attach/driver, then managed spawner. Introduce separate strict registration wrapper for managed callers instead of altering legacy `register_if_served` semantics everywhere:
  ```rust
  // Pattern in managed helper, not a best-effort `.ok().flatten()`.
  let lease = FrontendLeaseHandle::register(runtime_dir, kind, run_id)?
      .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotConnected,
          "managed guardian did not register a frontend lease"))?;
  ```
  Strict wrapper also returns heartbeat/open-channel errors; owner factory failure propagates through tool admission, never falls back to direct spawn. Repair reacquires fresh runtime generation and root authority through the same provider; no old snapshot reuse. Preserve CLI release/await_result uncertainty reporting.
- [ ] **Step 4:** Re-run Step 2 plus `(cd yi-agent-rs && cargo test -p yi-agent-subagent --lib)`; run manual CLI tests to demonstrate no global Managed default change.
- [ ] **Step 5:** Update progress, fmt/CI; commit `fix: enforce lease before managed frontend attachment`.

### Task 6: 旧 runtime 生产证据与只读迁移预览

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-supervisors/src/lifecycle/legacy_evidence.rs`
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/src/lifecycle/mod.rs`
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/src/lifecycle/migration.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/local_management.rs`
- Test: `yi-agent-rs/crates/yi-agent-supervisors/tests/runtime_migration.rs`

**Interfaces:** Produces `LegacyEvidence` (peer/start identity, executable digest, scope/runtime lock, host mode, protocol/schema evidence, admission freeze/drain support, resurrection sources, instance impact revision); each observation is verified/unknown, not default-true bool. `LegacyEvidenceProbe::inspect(&RuntimeScope, &Deadline) -> io::Result<LegacyEvidence>` must be read-only. `MigrationPreview` contains opaque token, evidence digest, affected registrations, blockers and expiration; no secrets to page. `LegacyEvidence::automatic_takeover_allowed() -> bool` centralizes conservative policy.

- [ ] **Step 1: RED.** Add fixtures for Status-only, PID reused, peer wrong uid, wrong executable digest, unreadable lock owner, embedded TUI, foreign loaded job, incomplete resurrection sources and active/unfreezable workload; all block automatic takeover. Snapshot isolated home/project before preview and compare after, including nonexistent runtime directory. Unknown protocol blocks temporary use. Known compatible legacy supports explicit temporary choice only.
  ```rust
  assert!(!status_only.automatic_takeover_allowed());
  assert!(!same_pid_different_start_identity.automatic_takeover_allowed());
  assert!(!idle_but_admission_not_frozen.automatic_takeover_allowed());
  assert!(verified_cooperative_idle.automatic_takeover_allowed());
  ```
- [ ] **Step 2:** Run `(cd yi-agent-rs && cargo test -p yi-agent-supervisors --test runtime_migration)` and local_management module tests. RED must show the production probe currently missing, not pretend injected LegacyProbe booleans already prove OS identity.
- [ ] **Step 3:** Implement read-only platform adapters: Unix peer credentials plus process start/executable observations where available; inspect only known runtime/instance paths and known launchd labels. If platform cannot prove lock/host/work evidence, return manual with named missing facts. Do not patch old runtime by guessing. Validate old protocol/schema via supported identity evidence or known verified artifact manifest, never Status high-water. Build instance-wide registration/watchman impact into digest. Preview token short-lived in memory, bound to App run/epoch/scope/target/evidence; zero disk writes.
  ```text
  auto_allowed = identity_verified && scope_and_lock_verified
      && standalone && all_resurrection_sources_known
      && host_freeze_supported && frozen_and_drained && idle_proven
  ```
  Preview reports freeze capability but does not freeze; confirm establishes frozen_and_drained and revalidates it before stop. Lack of cooperative legacy host is an expected manual path, not reason to relax the condition.
- [ ] **Step 4:** Repeat Step 2; inspect counters for zero stop/bootout/write calls on preview/cancel. Add two projects in same instance and verify preview lists both registration impacts.
- [ ] **Step 5:** Update progress, fmt/CI; commit `feat: inspect legacy runtime migration evidence safely`.

### Task 7: 持久迁移编排、退役 gate 与锁序

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-supervisors/src/lifecycle/migration_operation.rs`
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/src/lifecycle/mod.rs`
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/src/lifecycle/bringup.rs`
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/src/lifecycle/retirement.rs`
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/src/lifecycle/migration.rs`
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/src/watch.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/plugin_bridge.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/local_management.rs`
- Test: `yi-agent-rs/crates/yi-agent-supervisors/tests/runtime_migration.rs`
- Test: `yi-agent-rs/crates/yi-agent-supervisors/tests/managed_bringup.rs`

**Interfaces:** Consumes LegacyEvidence/preview. Produces `MigrationCoordinator::{confirm, operation, reconcile}` with explicit `MigrationPhase` and journal. Phases: Accepted, SelfHealFrozen, WatchmanRetired, LegacyStopped, ManagedConnected, Completed, Failed. Completed only after Task 3/4 required ownership readiness is acknowledged; ManagedConnected alone is not business-ready. Add `ensure_managed_under_guard(guard: &CoordinationGuard, scope: &RuntimeScope, expected: &ArtifactIdentity, intent: BringUpIntent, deadline: Deadline, probe: &dyn ControlProbe, launcher: &dyn GuardianLauncher, required_capabilities: &[String]) -> io::Result<BringUpResult>`; validate guard belongs to scope and do not reacquire its lock. The public wrapper acquires then delegates. Add instance `RetirementGate` with fixed-inode lock shared by every cooperating check→ensure and migration; old noncooperating host remains manual.

- [ ] **Step 1: RED.** Fault-inject before/after each journal write and side effect. Assert accepted record fsynced before any marker/bootout/stop, same op id returns existing result, token reused by another id rejected. Race watch::once_unattended paused after checking marker against migration: gate must drain it before stop. Parallel two-scope migration + bring-up must finish within one deadline, never self-lock. PID start identity change between preview/confirm blocks stopping.
  ```text
  confirm order: instance_gate -> sorted_scope_guards -> revalidate
    -> persist Accepted -> freeze_and_drain -> persist SelfHealFrozen
    -> verified watchman bootout -> persist WatchmanRetired
    -> stop verified idle daemon -> prove lock/execution exit
    -> persist LegacyStopped -> ensure_managed_under_guard
    -> persist ManagedConnected -> ownership readiness -> persist Completed
  ```
- [ ] **Step 2:** Run `(cd yi-agent-rs && cargo test -p yi-agent-supervisors --test runtime_migration && cargo test -p yi-agent-supervisors --test managed_bringup)`; include watchdog assertion against reentrant flock waits.
- [ ] **Step 3:** Implement journal per instance in a dedicated migration-operations directory with 0600 files, temp-write/fsync/rename/parent-fsync. Store operation id, scope, target, impact revision, redacted evidence digest/start identities, phase/error/result; reject unsafe operation-id path input. Persist and consume token under locks; fixed instance→sorted scopes order. Share gate across retirement/write and all old ensure callers. Do not hold coordination locks across shell root/lease callbacks: persist ManagedConnected, release guards, then native ownership readiness acknowledgment revalidates operation/scope/generation before Completed. Normal managed bring-up still cannot start a second daemon while runtime lock is held. A crash in this gap leaves ManagedConnected for read-only reconciliation, never reruns stop.
  ```text
  after retirement/start failure -> Failed, retirement stays active
  response lost -> operation(original_id), never repeat confirm
  restart -> reconcile observed facts only; user must explicitly retry
  retry -> revalidate fresh evidence + completed phases, never PID-only stop
  ```
  Marker existence remains retired. New managed App never starts resident legacy loop even before a marker; external watchman retirement still needs explicit instance-impact confirmation. Preserve registrations/config/database. Do not use daemon::stop success as proof of quiescence.
- [ ] **Step 4:** Repeat Step 2; add partial journal/truncated file refusal and filesystem mode assertions, cancellation before Accepted has zero writes, restart read-only counters. Test stale preview from other project after instance revision changes is rejected.
- [ ] **Step 5:** Update progress, fmt/CI; commit `feat: journal safe legacy runtime migration operations`.

### Task 8: 真实客户端 plugin demand 与监督门控

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-lifecycle/src/types.rs`
- Modify: `yi-agent-rs/crates/yi-agent-lifecycle/src/client.rs`
- Modify: `yi-agent-rs/crates/yi-agent-lifecycle/src/foreground.rs`
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/src/lifecycle/demands.rs`
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/src/lifecycle/guardian.rs`
- Modify: `yi-agent-rs/crates/yi-agent/src/managed_runtime.rs`
- Modify: `yi-agent-rs/crates/yi-agent-runtime/src/supervise.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/plugin.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/plugin_bridge.rs`
- Test: `yi-agent-rs/crates/yi-agent-supervisors/tests/guardian_control.rs`
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/server/plugin_ready_tests.rs`

**Interfaces:** Add authenticated control variants `AcquirePluginDemand { grant, plugin }` / `ReleasePluginDemand { grant, plugin }`; use existing ClientGrant. Add generation/sequence-fenced wanted-plugin snapshot on the private guardian→daemon bridge, distinct from business IPC. Guardian demand table drives snapshots; daemon supervisor's managed mode starts only configured AND wanted plugins. Existing manual/legacy supervise mode unchanged. Native demand operations use shell's held lease, not page-supplied grant; CLI uses its own live lease.

- [ ] **Step 1: RED.** Add control tests: lease-only/expired/forged grant rejected, duplicate acquire idempotent, A+B share one plugin, release/expiry A leaves B, release B stops plugin, old generation snapshots ignored. In plugin_ready_tests separate installed=true from queryable=false when lifecycle not ready/no demand; no daemon::ensure_running or promised resident retry. First-client startup window must start zero plugins despite enabled manifests.
  ```text
  config enabled, wanted={} -> zero plugin children
  wanted={plugin:[A,B]}, both live -> one child
  release(A) -> same child
  expire(B) -> stop child, no automatic config-only restart
  ```
- [ ] **Step 2:** Run `(cd yi-agent-rs && cargo test -p yi-agent-supervisors --test guardian_control && cargo test -p yi-agent-app-server --lib plugin_ready && cargo test -p yi-agent-runtime --lib supervise)`; capture targeted RED.
- [ ] **Step 3:** Wire live grant requests to existing PluginDemandTable; demand is not a new lease and cannot renew one. Send desired set through authenticated inherited bridge after each acquire/release/expiry and daemon replacement; ACK install before reporting queryable. Gate current unconditional managed `supervise::serve` by desired-set mode. Plugin install writes config/switch atomically as before; execution requires Task 3 lifecycleReady plus demand. If preparation blocked, return installed/queryable plus reason, not fake install rollback or hidden retry. Avoid opening a session/root solely for plugin access.
- [ ] **Step 4:** Repeat Step 2; run app-server plugin admission/query/remote test groups. Verify legacy manual supervision still works and remote paths cannot mint local demand.
- [ ] **Step 5:** Update progress; regenerate any affected existing bindings through repository recipe (private secret DTOs excluded), fmt/CI; commit `feat: drive managed plugins from live frontend demand`.

### Task 9: 前端准备状态、看板需求与迁移交互

**Files:**
- Modify: `desktop/src/lib/runtimeLease.ts`
- Modify: `desktop/src/hooks/useRuntimeLease.ts`
- Modify: `desktop/src/lib/runtimeLifecycle.ts`
- Modify: `desktop/src/hooks/useRuntimeLifecycle.ts`
- Modify: `desktop/src/components/RuntimeVersionBanner.tsx`
- Modify: `desktop/src/App.tsx`
- Modify: `desktop/src/hooks/useHandshakeRecovery.ts`
- Modify: `desktop/src-tauri/src/runtime_lifecycle.rs`
- Modify: `desktop/src-tauri/src/runtime_management.rs`
- Modify: `desktop/src-tauri/src/lib.rs`
- Modify: `desktop/src-tauri/build.rs`
- Modify: `desktop/src-tauri/capabilities/default.json`
- Test: `desktop/src/lib/runtimeLease.test.ts`
- Test: `desktop/src/hooks/useRuntimeLease.test.tsx`
- Test: `desktop/src/lib/runtimeLifecycle.test.ts`
- Test: `desktop/src/components/RuntimeVersionBanner.test.tsx`
- Test: `desktop/src/App.test.tsx`

**Interfaces:** Consumes ProjectAcquireView. Extend native RuntimeControl with `migrationPreview`, `migrationConfirm`, `migrationOperation` distinct from managed switch methods. Native stores operation id before confirmation send; unknown reply keeps id for read-only reconciliation. Frontend demand intent is `(scope, plugin, consumer)`; aggregate within App by actual view/work consumers before native per-client/plugin acquire/release so one view leaving cannot drop another's demand.

- [ ] **Step 1: RED.** Add React tests for acquire held=false/starting/unknown never entering owned; late project A result after switching B discarded/released; held lease + root blocked still released on quit. Board-only view with no thread enters wanted scopes before demand is granted. Static sidebar history does not acquire. Migration cancel sends zero confirm, lost confirm sends exactly one and polls same op id; instance impact lists other projects, unknown workload says unverified not idle. Unknown banner must not say old-version-started.
  ```ts
  expect(acquiredScopes).not.toContain(scopeA); // starting is not a lease
  expect(confirmCalls).toHaveLength(1);
  expect(operationQueries.every(id => id === originalOperationId)).toBe(true);
  expect(uiText).not.toContain("this runtime was not started by the version");
  ```
- [ ] **Step 2:** Run `(cd desktop && npx vitest run src/lib/runtimeLease.test.ts src/hooks/useRuntimeLease.test.tsx src/lib/runtimeLifecycle.test.ts src/components/RuntimeVersionBanner.test.tsx src/App.test.tsx)`; ensure targeted RED before editing UI.
- [ ] **Step 3:** Replace optimistic acquire handling with discriminated result; track actual owned and lifecycleReady separately, root errors stay per-thread. Compute wanted scopes as current executable conversation + active work + actual plugin usage intent, not every indexed project. Starting state retains input draft, never auto-submits. Board mount sets intent first→prepare→native demand; unmount removes its consumer only after work drains. Migration preview/confirmation goes through native commands, never RpcClient auto-replay; explicit temporary use scoped to this identity/session and cleared on identity change. Register new Tauri commands in lib/build/default capability together. Keep local-only controls absent for remote sessions.
  ```ts
  switch (result.state) {
    case "managedReady": lifecycleReady.add(result.scopeKey); break;
    case "starting": pending.set(result.scopeKey, result.attemptId); break;
    case "legacyChoiceRequired": showLegacyChoice(result); break;
    case "blocked": case "unknown": showPreparationReason(result); break;
  }
  // actual held set comes from native truth; not inferred from this switch.
  ```
- [ ] **Step 4:** Repeat Step 2; run `(cd desktop && npx tsc --noEmit && npx vitest run && npm run build)` plus Tauri command-capability tests. Sidecar reconnect reissues only safe status/root setup, never turn/start or migration confirm.
- [ ] **Step 5:** Update progress, fmt/CI and Tauri suite; commit `feat: expose truthful managed preparation and migration states`.

### Task 10: 生产入口审计与真实进程端到端验收

**Files:**
- Create: `yi-agent-rs/crates/yi-agent/tests/managed_production_integration.rs`
- Modify: `yi-agent-rs/crates/yi-agent/tests/managed_runtime_lifecycle.rs`
- Modify: `yi-agent-rs/crates/yi-agent-supervisors/tests/support/process_fixture.rs`
- Modify: `desktop/src-tauri/tests/quit_drains_runtime.rs`
- Create: `docs/testing/managed-runtime-production-integration.md`
- Modify: `docs/project-management/desktop.md`
- Modify: `docs/project-management/README.md`

**Interfaces:** Uses real `CARGO_BIN_EXE_yi-agent`, native bridge manager test seams and wiremock provider. Produces acceptance record plus call-site classification. Test helper owns TempDir, explicit project/home/runtime, child handle registry, process start identities, cleanup deadline; never global kill by name.

- [ ] **Step 1: RED.** Write real child tests for A01–A14/A16–A21, not only injected backend assertions. Example test structure:
  ```rust
  #[test]
  fn two_frontends_keep_other_owners_work_alive() {
      // In this test file define IsolatedRuntimeFixture around TempDir + Child.
      let mut fixture = IsolatedRuntimeFixture::start_without_bringup_env();
      let app = fixture.connect_app();
      let tui = fixture.connect_tui();
      let app_work = fixture.start_marker_process(&app);
      let tui_work = fixture.start_marker_process(&tui);
      fixture.release_and_wait(&app);
      assert!(!fixture.same_process_alive(&app_work));
      assert!(fixture.same_process_alive(&tui_work));
      fixture.release_and_wait(&tui);
      fixture.assert_runtime_and_plugins_stopped();
  }
  ```
  Define each fixture method in this file using real private/bootstrap/control channels from earlier tasks; `connect_tui` must spawn the actual default TUI entry (PTY if required), headless tests must spawn the actual run entry, and `connect_app` must exercise the native bridge manager plus real app-server bootstrap. Manually calling FrontendLease::register is permitted only for helper/protocol tests and cannot satisfy production-entry acceptance. `same_process_alive` compares start identity, not PID alone. Use fixed marker child process, not LLM. Add original-launcher sidecar kill while App+TUI leases remain, same App run id after reconnect, last-client no resurrection across at least two configured watch ticks. Cancellation/timeout is failure, not successful cleanup.
- [ ] **Step 2:** Run `(cd yi-agent-rs && cargo test -p yi-agent --test managed_production_integration -- --test-threads=1)` and record each A-id result. Before product edits, failures must identify missing wiring; if all pass, add an intentionally stale-capability negative test and verify it rejects. No implementation claim from mock-only tests.
- [ ] **Step 3:** Audit exact call sites with:
  ```sh
  git grep -n -E 'ensure_running|ensure_daemons|spawn_resident_watch_loop|attach_managed_project_runtime|process_bringup_choice|supervise::serve' -- yi-agent-rs/crates desktop/src-tauri
  ```
  Classify every production caller as managed-gated, explicit manual/legacy, or test in acceptance doc. Replace any managed bypass with the existing coordinator/provider; do not broaden CLI policy. Verify config-only install/watch paths cannot start managed work. Add real stop proof to fixture: observed process tree exit, released runtime lock, absent service response; socket absence alone insufficient. Clean up only child handles/start identities owned by fixture, stop parent before supervised children.
- [ ] **Step 4:** Repeat Step 2; run all affected crate suites sequentially and Tauri tests. Add seeded history/delivery rows before migration and verify readable after; operation response loss and app-server restart never duplicate marker side effects.
- [ ] **Step 5:** Write actual commands/results and unresolved native cases in acceptance doc, update progress without prematurely checking feature complete; fmt/CI; commit `test: verify managed production lifecycle across real processes`.

### Task 11: 最终隔离 App 与发布门禁

**Files:**
- Modify: `docs/testing/managed-runtime-production-integration.md`
- Modify: `docs/project-management/desktop.md`
- Modify: `docs/project-management/README.md`

**Interfaces:** Consumes Tasks 1–10 passing plus verified bundled sidecar. Produces A15 native evidence and release decision. This task does not install over `/Applications/yi-agent.app` or migrate user's runtime.

- [ ] **Step 1:** Before committing final code, run fresh full verification:
  ```sh
  (cd yi-agent-rs && cargo fmt --all && just ci) &&
  cargo fmt --manifest-path desktop/src-tauri/Cargo.toml --all -- --check &&
  cargo test --manifest-path desktop/src-tauri/Cargo.toml &&
  (cd desktop && npx tsc --noEmit && npx vitest run && npm run build)
  ```
  If packaging prerequisite is absent, use project-prescribed sidecar build in this worktree; never overwrite running bundle's binary or symlink a stale sidecar as final artifact.
- [ ] **Step 2:** Build in worktree: `(cd desktop && npm ci && npm run sidecar:release && npm run build && npx tauri build --bundles app)`. Resolve target_directory using cargo metadata; follow existing finalize-runtime-bundle.sh ordering. Copy generated bundle to a project-local `.artifacts/managed-integration/` test location, set distinct bundle id/name and absolute `YI_AGENT_HOME`, then finalize/re-sign using the actual packaging script interface. Any post-sign Info.plist/binary change requires rerunning finalization; verify sidecar digest and strict signature afterward. Never use `codesign --deep`.
- [ ] **Step 3:** Launch the isolated `Contents/MacOS/desktop` directly with no relay and a temporary project. Record real Wry/macOS checks: red close hides without lease release, Dock reopen restores input/output, Cmd+Q drains only App work, coexisting isolated TUI survives, last TUI exit drains daemon/plugin, no resurrection over watch ticks. Reboot sidecar separately while shell lives and verify guardian/other client remain alive. Keep PID/start-time/scope/build/lease evidence redacted. If a native case is not run, mark unexecuted and keep feature `[ ]`.
- [ ] **Step 4:** Match every A01–A21 to evidence in acceptance doc; root auth failures and unknown states must be exercised, not just happy path. Verify no formal home/relay/job changed. Review diff with requesting-code-review skill; fix review findings through TDD. Only all gates passed permit desktop feature `[x]` and index count update.
- [ ] **Step 5:** Commit only honest acceptance documentation after diff check. 本计划正式入队后的实施分支遵循 superpowers-kanban 合并名额：交付完成先 `state --self awaiting_merge`；等待用户确认，再 `merge-request <card-id>`。只有 daemon 排到并明确授予本卡合并回合后，才按该回合提示执行 no-ff 合并并在合并结果上重跑 `just ci`，声明成功/失败；不得在普通开发回合自行 `git merge`，不得自行释放别人的名额。清理本卡 worktree/branch 须在成功且不妨碍看板恢复后按合并回合要求完成。若 native tests 未执行，保持 feature 未完成并报告，不伪装成完成。非看板普通分支才适用 finishing-a-development-branch 的本地合并路径。Inspect disk/processes before optional clean; no cargo clean while builds/other work are using target.

## Spec 覆盖与作者复核记录

| Spec | Task | 必须核实的交付 |
|---|---|---|
| §1–§3 / A16 | 1, 5, 10 | 保留已有语义，显式 managed 入口，手工 CLI 不变 |
| §4.1 / A18–A19 | 1, 2 | canonical 项目/instance，hello fail closed，私有 stdio 无页面泄露 |
| §4.2 / A01–A03, A17 | 3, 4, 5 | prepare→真实租约→逐 root 授权/driver，无项目路径伪授权 |
| §4.3 / A03, A05 | 3, 9 | held/ready 分离，迟到结果、草稿、退出竞态 |
| §4.4 / A04, A06 | 3, 4, 10 | sidecar 重启身份稳定，其他客户端不误杀 |
| §5.1 / A08–A09, A11, A21 | 6, 9 | 只读预览、全实例影响、未知不强停 |
| §5.2–§5.3 / A09–A11, A20–A21 | 6, 7, 10 | 生产证据、idle/cooperation、锁序、日志与无重复停止 |
| §6 / A07, A12 | 5, 7, 8, 9, 10 | 无旧自愈绕路，实际 demand 驱动插件 |
| §7 / A13–A14, A18 | 2, 4, 9 | 后置授权/换代、准确文案、dev 不冒充可验证包 |
| §8 / A01–A21 | 10, 11 | 范围测试、真实进程、独立 App、CI 与发布证据 |

2026-11-05 作者两轮自审（不是独立 agent review；本会话派发子代理被运行时拒绝）：

1. 代码对照审查发现并修订 spec：root 与项目路径混淆、先 attach 后租约、Status 无身份、迁移锁重入、项目 cwd 传播、插件 table 未接监督控制、旧自愈在途竞争。
2. 一致性审查收紧：scope lifecycleReady/逐 rootReady 分离；no-session 看板先登记使用意图；ManagedConnected 与 Completed 分开且不持 scope 锁等待壳；preview 严格只读；未知协议/不合作旧 host 明确 manual；私有 stdin bootstrap 不需要重造进程启动框架。
3. 本轮 plan 复审对照现有 `FrontendLease::preview`、`LeaseTable::register/resume` 与控制协议，修复版本选择态无租约导致无法预览的死路：兼容构建差异可实际持 Business 租约但不 ready，不虚构 mode 升级；补充 WaitingNextGeneration 与取消/退出测试。
4. 交叉复审补齐 DTO camelCase 真源、bootstrap 固定期望 epoch/逐 scope revision/空快照撤销、真实 TUI/headless/native bridge 入口验收，并将实施合并改为看板 awaiting_merge→用户确认→merge-request→获授合并回合。
5. 本计划为新增实现契约，不是测试报告。上述所有代码/测试任务当前均未执行；正式实例未迁移。编写期仅检查文档、路径、覆盖矩阵及格式，不把既有底层测试当最终产品接入证据。
