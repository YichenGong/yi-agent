# 远端 SSH 执行（实验功能）

设计依据：[spec](../superpowers/specs/2026-10-31-remote-ssh-execution-design.md)，
实施计划：[plans](../superpowers/plans/2026-10-31-remote-ssh-00-index.md)。

**状态：实验阶段。** 实验开关 `remote_ssh` 默认关，且 stable 构建恒关
（`yi-agent-flags/src/store.rs` 的 build_kind 硬闸）。在正式版开放前必须先在
dev 构建里验证充分。

## 核心设计

- **每项目一个远端进程**：一条 `ssh <host> ... app-server --listen stdio://`，
  cwd = 远端项目路径，`YI_AGENT_HOME` = `<远端 home>/.yi-agent-projects/<id>/.yi-agent`。
- **每项目一条 relay session**：`<原 session>-<profile.id>`，手机据此为每个
  项目各建一条连接，看到的内容与电脑一致。
- **解压在远端做**：本机只下载与 scp；远端解不了 gzip 时改传单个二进制
  （本机用系统 `tar` 解出），因此本机不引入任何解压 crate。

## 完成情况

### 计划 01：多槽位桥接层与远端骨架

- [x] `remote_ssh` flag 注册，默认关 — `yi-agent-flags/src/registry.rs`
- [x] `ProfileKey` 与 profile id 校验 — `desktop/src-tauri/src/remote/profile.rs`
- [x] ssh argv 构造为纯函数、防注入 — `remote/argv.rs`（11 测试）
- [x] `remote.json` 原子持久化（并发写不丢更新）— `remote/config.rs`（6 测试）
- [x] 桥接层按槽位管理进程与锁 — `bridge.rs`
- [x] 事件名按槽位参数化 `app-server://{kind}/{slot}` — `bridge.rs`
- [x] 远端槽位经 ssh 连接 + 每槽位监督循环 — `bridge.rs`
- [x] 前端每槽位一个 `RpcClient` — `desktop/src/lib/clientRegistry.ts`
- [x] 关开关时清掉所有远端槽位 — `bridge.rs`

### 计划 02：远端二进制安装

- [x] 架构归一化与支持矩阵（3 个 target）— `remote/arch.rs`
- [x] 下载 + sha256 校验后才落盘 — `remote/fetch.rs`
- [x] 一次 ssh 往返探测架构与 tar/gzip 能力 — `remote/probe.rs`
- [x] 首次连接自动安装，三条选路 — `remote/install.rs`（14 测试）
- [x] 缓存与旧版本清理 — `remote/cache.rs`
- [x] 远端解不了 gzip 时只传二进制（不引解压库）— `remote/install.rs:needs_local_extract`

### 计划 03：路径语义与手机

- [x] 远端模式不传 `cwd` — `desktop/src/lib/threadStart.ts`（6 测试）
- [x] 重试策略：本地 5s 封顶、远端 60s、5 次后停 — `bridge.rs`（4 测试）
- [x] `remote-status.json` 旁路，供手机读可用性 — `remote/status.rs`
- [x] `remote/*` Tauri 命令带闸门 — `remote/rpc.rs`；**静态审计**确保没有漏网的
  命令，只有收尾动作豁免（`remote_disconnect` / `kill_remote_slots` /
  `remote_enabled`）— `bridge.rs:every_remote_command_is_gated_or_is_cleanup`
- [x] `instance_root` 由远端 home 派生，不由用户输入 — `remote/profile.rs:derive_instance_root`
- [x] 保存时预检远端项目路径存在 — `bridge.rs:remote_upsert`
- [x] 每条远端命令有 watchdog，超时即 kill — `remote/install.rs:run_remote_within`
- [x] `remote/relayTargets`（Observe 域不给 session）— `yi-agent-app-server/src/rpc/remote.rs`
- [x] 手机侧拿到 `/ws` 端点而非电脑侧 `/connect` — `remote_session.rs:client_endpoint`
- [x] 「远程项目」设置页 — `desktop/src/RemoteSettings.tsx`
- [x] `RemoteConfig` 扩展为多项目绑定 — `desktop/src/lib/remoteConfig.ts`
- [x] 手机每项目一条连接 — `desktop/src/lib/relayTargets.ts` + `App.tsx`
- [x] 非活动槽位的通知不写当前视图 — `desktop/src/lib/slotRouting.ts`（5 测试）
- [x] 安装进度按项目回推，UI 显示阶段文案 — `bridge.rs:ensure_installed` → `app-server://install/progress/<id>`；`RemoteSettings.tsx:stageLabel`（4 测试）
- [x] 路径 3（本地二进制）可从设置页触发，且不被「远端已装」短路 — `remote_connect(from)`、`RemoteSettings.tsx`（2 测试）

### 计划 04：端到端验收

- [x] A 批（12 项单测与静态检查）全部 PASS — 见计划的「实际执行记录」
- [ ] B 批：需要一台**有外网出口且配好模型 key** 的远端机；当前仅两条因
      「远端无模型配置」未通过（`gpu-jump` 无外网，无法验证中继桥接）
- [ ] C 批：核心并行/隔离判据已在本机等价拓扑 PASS；真机双项目仍待验证
- [ ] D 批：发布前回归（release 构建、安装包、真机）

## 已知限制

- 远端那台机器必须能访问中继域名（见 `deploy/relay/.relay-local.md`）（若启用手机侧）。
- 远端需自行配置模型 key；未配置时 `thread/start` 返回
  `-32027 model_not_configured`，属预期而非缺陷。
- 不做会话恢复：远端断线即重启进程，状态不保留。
- `Leader` 与多 agent 协调不在本功能范围内。

## 验证命令

```bash
# 单测（三栈）
cd yi-agent-rs && cargo test -p yi-agent-flags -p yi-agent-app-server --lib
cd desktop/src-tauri && cargo test --lib
cd desktop && npx vitest run && npx tsc --noEmit

# 远端安装真机验证（需先交叉编译出 Linux 包）
cd yi-agent-rs && cargo zigbuild --release --target x86_64-unknown-linux-gnu -p yi-agent
```
