# 桌面设置页配置侧车中继（B）

**状态：** 设计已确认（待实现）
**日期：** 2026-10-03
**相关：** `docs/relay-deploy.md`（§3.4）、`docs/superpowers/specs/2026-10-03-shared-session-relay-design.md`、`desktop/src/components/SettingsRemoteTab.tsx`、`desktop/src-tauri/src/bridge.rs`

## 1. 问题

合一模式（桌面 GUI + 手机共用同一 app-server 会话）已实现，但**桌面侧车从哪里取中继地址**只有一个来源：环境变量 `YI_AGENT_RELAY`（`bridge.rs::configured_relay`）。后果：

- 普通用户（双击安装的 App）**没有任何 UI 能设置**它 → 手机永远连不上（这正是本会话现场诊断出的根因）。
- 设置页「远程访问」里**已有**一个「中继地址」输入框，但它只喂**手机侧**的配对二维码（`buildPairUri`），**不接**侧车 → 误导。
- 一个长的 `wss://…/connect?session=…` URL 靠手输环境变量不现实。

## 2. 目标

桌面「远程访问」设置页里的**中继地址**字段**直接配置侧车**：

1. 用户填入中继地址并保存 → 落盘。
2. 侧车启动/重启时读该落盘值，有则带 `--relay <url>` 启动（合一模式）。
3. 用户改动该字段后，**当前侧车重启**以立即生效（无需退出 App）。
4. 环境变量 `YI_AGENT_RELAY` 作为**覆盖/开发用**来源，仍优先生效（不破坏脚本/开发路径）。
5. 字段为空 → 清除配置 → 侧车回到**纯 stdio**（今日行为，逐字节一致）。

## 3. 非目标

- 不做中继服务的常驻安装（那是 C，另议）。
- 不改手机侧配对流程。
- 不做多中继配置/多会话管理。
- 不改动合一模式的服务端语义（已实现）。

## 4. 设计

### 4.1 持久化位置

复用侧车已在读写的偏好文件：**`<workdir>/.yi-agent/preferences.json`**（`settings_store` 的 `theme` / `board_watchman_enabled` 同处一个文件；写必须读-改-写并保留无关键、临时文件 + rename 原子落盘——已有约定）。

新增键：**`relay_url`**（字符串；空/缺省等价于未设置）。

侧车的 `workdir` 在桌面场景下 = 用户 home（`bridge.rs::spawn_once` 以 `home_dir()` 作为 `current_dir`）。因此文件实际是 `~/.yi-agent/preferences.json`。

### 4.2 侧车侧（Rust app-server crate）

`settings_store.rs` 新增：

```rust
pub fn load_relay_url(workdir: &Path) -> Option<String>;   // trim; 空/非法 → None
pub fn save_relay_url(workdir: &Path, url: Option<&str>) -> std::io::Result<()>; // None/空 → 删除该键
```

- 与 `load_watchman_enabled`/`save` 一样走**同一个** `read_write_key` 辅助函数，保证不覆盖 `theme`/`board_watchman_enabled`/`subagent_runtime` 等无关键。
- `ui/settings/read` 的返回体加入 `relay_url`（桌面设置页首屏回填用）；`ui/settings/write` 接受 `relay_url` 字段（`null`/空 → 清除）。

### 4.3 桌面壳侧（`desktop/src-tauri`）

`bridge.rs`：

- `configured_relay()` 改为：**先看 `YI_AGENT_RELAY`**（非空即用）；否则读偏好文件 `~/.yi-agent/preferences.json` 的 `relay_url`。→ 纯函数 `resolve_relay(env: Option<&str>, stored: Option<&str>) -> Option<String>` 便于单测。
- 新增 Tauri 命令 **`set_relay_url(url: Option<String>)`**：
  1. 读写 `~/.yi-agent/preferences.json` 的 `relay_url` 键（读-改-写，保留无关键，原子落盘）；
  2. 杀掉当前侧车子进程（`Sidecar.child`），使监管循环用新设置重启它（既有 `spawn` 循环：进程退出后 `clear_child` + 退避重启）。
- 侧车启动**始终**读当前解析结果，故重启后立即生效。

### 4.4 设置页（React）

`SettingsRemoteTab` 的「中继地址」输入框：

- 语义明确为「**本机侧车要连的中继地址**（电脑侧端点，`…/connect?session=…`）」，与「手机要填的地址」区分说明（手机填的是 `…/ws?session=…`）。
- 「保存」按钮 → 调 `set_relay_url(value)`（经注入的接缝）→ 触发侧车重启 → 提示「已保存，正在重连」。
- 首屏回填：来自 `ui/settings/read` 的 `relay_url`（或宿主传入）。
- 不改变二维码/配对码行为。

## 5. 不变量

1. **零回归**：未设置任何中继（env 与偏好都空）时，侧车 argv 与今日**逐字节一致**（`["app-server","--listen","stdio://"]`）。
2. **env 优先**：`YI_AGENT_RELAY` 非空时忽略偏好文件（显式覆盖）。
3. 写偏好文件必须保留无关键（既有测试口径）。
4. 改动字段后**不要求用户重启 App**。
5. 不泄露 URL 到日志（可记「relay 已配置」而不打印完整 URL）。

## 6. 测试

- `settings_store`：`relay_url` 往返；空 → 清除；写保留无关键；坏文件回退默认（None）。
- `bridge`：`resolve_relay` 的 env-优先/落盘回退/皆空→None 三分支；`sidecar_args` 不变（已有）。
- `server`：`ui/settings/read` 带 `relay_url`；`ui/settings/write` 设/清 `relay_url` 后 read 一致。
- 前端：设置页保存调用 `set_relay_url`；空串 → 传 `null`；首屏回填。
- 手动：设置字段 → 侧车重启 → `ps` 确认 argv 带 `--relay`；手机经中继可见同一批线程。
