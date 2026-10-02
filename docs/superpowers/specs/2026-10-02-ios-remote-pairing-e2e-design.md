# iOS 远程控制 Tier 1.5：配对端到端打通（设计）

**状态：** 已设计，待实现。
**前置：** `docs/superpowers/specs/2026-10-02-mobile-remote-access-design.md`（Tier 0 + Tier 1，已合入 main）。
**相关计划：** `docs/superpowers/plans/2026-10-02-ios-remote-app-tier1.md`（Tier 1，已合入）。

## 1. 背景与问题

Tier 1 交付了协议与单测（多客户端 ws、设备 token、配对码、scope、审批广播、反向 WSS
中继），但**端到端扫码配对无法组合**。三个已确认缺口：

1. **配对码只存进程内存。** `PairingState.codes` 是 `Mutex<HashMap>`（`pairing.rs:34`），
   从不落盘。桌面 GUI 的边车是 `yi-agent app-server --listen stdio://`（一个进程，服务
   `pair/create`），而中继模式 `--relay` 是**另一个进程**（服务 `?pair=` 兑换）。两者
   各自 `PairingState::new(...)`，不共享内存，因此桌面铸出的码在被兑换的进程里不存在，
   兑换恒 4401。单测能过只因"同一进程内 create + serve_ws"。
2. **没有 iOS 首启配对界面。** `pairing.ts` / `remoteConfig.ts` / `transportFactory.ts`
   是接缝，但调用它们的屏幕不存在；iOS 首启无持久化配置时 `transportFactory` 退回
   `tauriTransport()`（无 Tauri IPC → 死路）。
3. **没有桌面侧生成配对码的入口。** 无任何 UI/CLI 调用 `pair/create`。

因此"手机扫码即用"实际不可用；文档已如实标注为实验性。

## 2. 目标与非目标

**目标（Tier 1.5）**

- **G1 配对码跨进程可兑换**：`pair/create`（stdio 进程）铸出的码，能被 `--relay`/`ws://`
  进程兑换。做法：把待兑现的码**持久化**，使同一台机器上的任意 app-server 进程都能读。
- **G2 iOS 首启配对界面**：手机首次启动进入配对表单（服务器地址 + 配对码），成功后
  `saveRemoteConfig` 并转入正常 ws 会话；失败给出可读错误。
- **G3 桌面"远程访问"设置页**：一键生成配对码、显示码与中继地址、列出/撤销已配对设备。

**非目标（后续）**

- 二维码扫描（相机权限 + 扫码库）——本期用**文本输入**码与地址；二维码留后续。
- APNs 推送、端到端加密、按 thread 订阅过滤、Android（见 Tier 1.1 / Tier 2+）。
- 可安装的 iOS 产物（受 Xcode 模拟器运行时缺失 + 签名阻塞，属环境项，非代码项）。

## 3. 设计

### 3.1 配对码持久化（G1，核心）

**文件**：`<devices.json 同目录>/pairing.json`（即 `~/.yi-agent/pairing.json`），格式：

```json
{ "codes": [ { "code": "ABCD-EFGH", "expires_at": 1790938000 } ] }
```

`expires_at` 用 **epoch 秒**（跨进程可比；原实现用 `Instant` 仅进程内可比）。

**语义**：

- `create_code`：读文件 → 剪掉过期项 → 追加新码 → 原子写（temp + rename）。
- `redeem`：读文件 → 剪掉过期项 → 查找并**移除**目标码 → 写回 → 若命中则铸设备。
  文件是唯一真相，**不做进程内缓存**，从而天然跨进程（配对是低频操作，性能无关）。
- 写失败（磁盘只读等）：`create_code` 仍返回码但记录告警；`redeem` 视为无效码
  （保守：绝不因 I/O 问题放行）。

**并发**：沿用 `DeviceStore` 的"读-改-写 + 原子 rename"约定。跨进程同时铸码的丢失窗口
极小且只影响一次配对尝试（重试即可），文档记录为已知限制。

### 3.2 iOS 首启配对界面（G2）

新增 `PairingScreen`（`desktop/src/components/`），在 iOS 且无 `RemoteConfig` 时由
`App.tsx` 渲染。表单字段：

- **服务器地址**（relay 的 ws/wss 端点，含 `?session=<id>`）；
- **配对码**（`XXXX-XXXX`，桌面显示）。

提交 → `redeemPairCode(url, code, deviceName)` → `saveRemoteConfig(localStorage, {url,
token})` → 触发一次 transport 重建（`transportFactory` 下次读取即得 ws）。

错误映射：4401 → "配对码无效或已过期，请在桌面重新生成"；网络错误 → "无法连接服务器"。

**设备名**：默认取 `navigator.platform`/UA 推断的机型名，用户可编辑。

### 3.3 桌面"远程访问"设置页（G3）

`SettingsDialog` 新增 Tab「远程访问」，内容：

- 按钮"生成配对码" → 经 transport 发 `pair/create` → 显示码 + 有效期倒计时；
- 输入/展示中继地址（供手机填写）；
- 设备表：`device/list`，每行含名称/scope/创建时间 + "撤销"（`device/revoke`）。

**依赖**：`pair/create` 与 `device/revoke` 需 `Admin`。桌面 stdio 客户端即
`Scope::Admin`（`server.rs:983`），故桌面设置页可直接调用；网络客户端一律 `Control`
会得 `-32014`，UI 需优雅提示。

## 4. 测试策略

**Tier 0（单元）**

- `pairing`：码**落盘**；**跨 `PairingState` 实例**可兑换（模拟两进程共享文件）；过期码
  落盘后仍被拒；码用后从文件移除；写失败路径不 panic。
- 设备表：撤销即失效（已有）。

**Tier 0（集成）**

- 现有 `cargo test -p yi-agent-app-server` 全绿零回归（stdio 与 ws 路径不变）。
- 新增：先 `create_code` 于实例 A，再用**实例 B**（同一文件）`redeem` 成功，且 A 能
  `authenticate` 该 token（证明设备表本就共享、码现在也共享）。

**前端**

- `PairingScreen`：提交成功调用 `saveRemoteConfig`；4401 显示对应文案；地址/码缺失时禁用。
- `SettingsRemoteTab`：`pair/create` 后渲染码；`device/list` 渲染行；撤销调用
  `device/revoke`；`-32014` 显示权限提示。
- 现有 `vitest` 全绿零回归（366 项基线）。

**验证命令**

```
cargo test -p yi-agent-app-server
cd desktop && npx vitest run && npx tsc --noEmit
```

## 5. 兼容与迁移

- `pairing.json` 是**新增文件**，`devices.json` schema 不变；旧版本忽略之。
- 协议**零变更**：复用 `pair/create`、`pair/redeemed`、`device/list`、`device/revoke`。
- 内存→落盘的语义变化对既有单测透明（同进程读写同一临时文件）。

## 6. 已知限制（文档如实记录）

- 本期用文本输入配对码，**无二维码扫描**。
- 跨进程同时铸码存在极小丢失窗口（重试即可）。
- 仍**无可安装 iOS 产物**（环境阻塞：Xcode iOS 26.5 模拟器运行时缺失 + 签名）。

Tier 1.5 交付后，"桌面生成码 → 手机填地址+码 → 配对成功 → 同屏控制"这条链路应能
端到端跑通（受限于可运行构建的环境条件）。
