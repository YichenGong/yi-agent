# iOS 远程控制 Tier 1.5：配对端到端打通（实现计划）

**设计：** `docs/superpowers/specs/2026-10-02-ios-remote-pairing-e2e-design.md`
**前置：** Tier 1 已合入 main（`49082cd`）。
**纪律：** 每个任务先写失败测试（TDD）→ 实现 → `cargo fmt` → `cargo test` / `vitest`。
提交前 `cd yi-agent-rs && cargo fmt --all`；**不扫入** repo 级既有 rustfmt 漂移。
conventional commits，**不写 `Co-Authored-By`**。

## 门禁（每个任务都要满足）

- `cargo test -p yi-agent-app-server` 全绿（基线 248 项，不得回归）。
- `cd desktop && npx vitest run` 全绿（基线 366 项，不得回归）+ `npx tsc --noEmit`。
- 改动文件 `cargo fmt --all -- --check` 干净。

---

## Task 1 — 配对码持久化（Rust，核心）

**目标：** `pair/create` 铸出的码落盘到 `~/.yi-agent/pairing.json`，使**另一个进程**的
`?pair=` 兑换能读同一份码。

**改动**

- `pairing.rs`：
  - `PairingState` 增加 `codes_path: PathBuf`（与 `store.path()` 同目录的
    `pairing.json`）；`new(store)` 从 `store.path().with_file_name("pairing.json")` 推导，
    另加 `with_codes_path(...)` 供测试注入。
  - `PendingCode { expires_at: i64 }`（epoch 秒）。
  - `create_code`：读文件 → 剪过期 → 插入 → 原子写。
  - `redeem`：读文件 → 剪过期 → 移除并写回 → 命中才铸设备。
  - 私有 `read_codes` / `write_codes`（temp + rename，`create_dir_all` 父目录）。
  - 删除 `insert_expired_code` 的进程内用法，改为写文件注入过期码。
- `server.rs`：无签名变化（`PairingState::new` 仍从 `DeviceStore` 推导路径）。

**测试（先写）**

1. `a_code_survives_across_pairing_instances`：实例 A `create_code`；**新实例 B**
   （同一 `devices.json` 路径）`redeem` 成功；A `authenticate(token)` 命中。
2. `an_expired_code_is_rejected_after_persistence`：写一枚过期码进文件，`redeem` 拒绝。
3. `redeem_removes_the_code_from_disk`：兑换后文件里不再有该码，二次兑换失败。
4. `create_code_omits_expired_codes_when_writing`：旧过期码在 `create_code` 后被剪掉。
5. 既有 4 项 pairing 测试改为经 `with_codes_path` 注入临时路径，继续全绿。

**验收：** 测试 1 是本期关键证明——它精确复现"两进程共享文件"的拓扑。

---

## Task 2 — CLI `pair` 子命令（可选便利入口）

**目标：** `yi-agent pair` 打印一枚配对码（走 stdio app-server 的 `pair/create`）或直接
调用 `PairingState`，方便无桌面 GUI 的服务器场景。

**改动**

- `config.rs`：`Command::Pair { #[arg(long)] device: Option<String> }`（`--revoke <id>` 也
  可，若时间允许）。
- `main.rs`：`run_pair` —— 构造 `PairingState::new(default_path)`，`create_code()`，打印
  `码 + 有效期`；`--revoke <id>` 调 `revoke`。

**测试：** 解析测试（`parse_listen` 同风格）；`run_pair` 的输出断言（capture stdout 或拆
纯函数返回字符串）。

**验收：** 手工 `cargo run -p yi-agent -- pair` 打印形如 `ABCD-EFGH (valid 300s)`。

---

## Task 3 — iOS 首启配对界面（前端）

**目标：** iOS 无 `RemoteConfig` 时渲染 `PairingScreen`，成功后持久化并转入 ws 会话。

**改动**

- 新增 `desktop/src/components/PairingScreen.tsx`：地址 + 配对码 + 设备名三输入；提交调
  `redeemPairCode` → `saveRemoteConfig`（经注入的 storage）→ `onPaired()` 回调。
- `App.tsx`：用 `platform.isIos()` 且 `storedRemoteConfig(storage) === null` 时渲染
  `PairingScreen`；`onPaired` 触发 transport 重算（`useState` 版本号）。
- 错误映射：4401 文案、网络错误文案；输入为空时提交按钮禁用。

**测试（先写）**

- 成功路径：mock `redeemPairCode` 解析出 `{token}` → 断言 `saveRemoteConfig` 被调用、
  `onPaired` 触发。
- 4401：断言渲染"配对码无效或已过期"。
- 空输入：按钮 `disabled`。

**验收：** vitest 全绿 + tsc clean；`App` 现有测试不回归。

---

## Task 4 — 桌面「远程访问」设置页（前端）

**目标：** 桌面设置里生成配对码、展示中继地址、列出/撤销设备。

**改动**

- `SettingsDialog.tsx`：`TABS` 加 `{ id: "remote", label: "远程访问" }`。
- 新增 `SettingsRemoteTab.tsx`：接收 transport/RpcClient；"生成配对码"调 `pair/create`
  → 显示码 + 倒计时；`device/list` 渲染表；每行"撤销"调 `device/revoke`。
- `-32014` → 显示"需要桌面端权限"。

**测试（先写）**

- `pair/create` 返回码 → 渲染码文本。
- `device/list` 两行 → 渲染两行；点撤销 → 调 `device/revoke` 且行消失。
- `-32014` → 权限提示。

**验收：** vitest 全绿 + tsc clean。

---

## Task 5 — 文档与收尾

**改动**

- `README.md` / `README.en.md`：把"实验性告警"更新为"配对链路已端到端打通（文本码）"，
  保留"无可安装 iOS 产物 / 无二维码扫描"的诚实说明。
- `docs/relay-deploy.md` §4.4/§六：更新"跨进程兑换不可组合"为已修复（文本码路径）。
- 新增本 spec/plan 的索引（`docs/project-management/desktop.md` 或 README 设计出处）。

**验收：** 文档与代码一致；无夸大。

---

## 已知限制（随任务如实记录）

- 无二维码扫描（文本输入码）。
- 跨进程同时铸码的极小丢失窗口。
- 仍无可安装 iOS 产物（Xcode iOS 26.5 模拟器运行时缺失 + 签名）。
