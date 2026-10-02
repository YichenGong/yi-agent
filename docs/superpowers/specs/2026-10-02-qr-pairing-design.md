# 扫码配对（S4）设计

**状态：** 已设计，待实现。
**前置：** S2（按会话订阅接线）已合入 `main`（`902d453`）；iOS/中继链路（Tier 1.5）已端到端可用。
**定位：** mobile-remote-access 后续项之一「二维码扫描」——把配对从「手输中继地址 + 手输码」
换成「扫一下」的自包含增量。

## 1. 背景与问题

iOS 首启配对（`desktop/src/components/PairingScreen.tsx`）目前要用户手输**两样**东西：
中继地址（`wss://relay.example.com/ws?session=<id>`）与一次性配对码（`XXXX-XXXX`）。
两者都长、都易错，而地址尤其难输。配对协议本身已端到端打通（含经中继的帧级兑换，
见 `docs/relay-deploy.md` §4.4），唯一的毛刺就是这条手输路径。

桌面侧目前**完全不出二维码**：`desktop/src/pairing.ts` 的注释写着「code shown as a QR
code by the desktop」，但那是设想，未实现。`yi-agent pair code` CLI 也只打印
`ABCD-EFGH (valid 300s)`。两端都没有二维码可扫。

本设计补上**显示侧**（桌面设置页 + CLI 出二维码）与**扫描侧**（iOS App 内扫码后自动配对）。

## 2. 目标与非目标

**目标**

- **G1 统一载荷**：定义一个二维码文本格式（自定义 URI），两端各有一份**纯函数**编解码
  实现，用**同一组 fixture** 断言逐字节一致，防止格式漂移。
- **G2 桌面出码**：设置「远程访问」页在已有「中继地址」字段非空时，于配对码旁渲染二维码。
- **G3 CLI 出码**：`yi-agent pair code` 新增可选 `--relay <url>`；给了就额外在终端打印
  Unicode 二维码，不给则输出与今天完全一致。
- **G4 App 内扫码**：iOS 配对页加「扫码」按钮，打开相机预览，命中后**解析 → 自动配对**。
- **G5 零回归**：文本手输路径与所有既有行为不变；扫码任何环节失败都不使手输路径变差。

**非目标（后续）**

- 端到端加密与公钥指纹校验（v1.1；二维码因此**不**携带指纹）。
- 用系统相机扫、经 OS deep link（`yiagent://`）唤起 App——本设计是 App 内扫码，
  二维码内容是我们两端约定的编码，不依赖任何 OS 级 URL scheme 注册。
- 可安装的 iOS 产物（受 Xcode 运行时/签名阻塞，见 `docs/relay-deploy.md` §4.2）。
- 记住上次填过的中继地址（可选增强，见 §6）。

## 3. 载荷格式（唯一的跨端契约）

二维码文本是一个 URI：

```
yiagent://pair?v=1&relay=<percent-encoded relay url>&code=<XXXX-XXXX>
```

- `relay` 的值是完整的电脑侧地址。它既可以是**中继地址**
  （`wss://relay.example.com/ws?session=<id>`），也可以是**局域网直连地址**
  （`ws://192.168.x.x:8080/ws`）——同一编码两者都装，由用户在字段里填什么决定。
  因其自身含 `?`，作为 query 值必须 **percent-encode**。
- `code` 是 `pair/create` 铸出的一次性码。
- `v=1` 是版本位，便于将来演进；未知版本一律判为无效。

**纯函数契约**（两端各一份，逐字节一致）

- `buildPairUri(relay, code) -> string`
- `parsePairUri(text) -> { relay, code } | null`

`parsePairUri` 只在**同时**满足以下条件时返回非 `null`：

1. scheme 为 `yiagent`，host 为 `pair`；
2. `v === "1"`；
3. `relay` 与 `code` 两个 query 参数都存在且非空；
4. `relay` 反编码后以 `ws://` 或 `wss://` 开头。

其余一律返回 `null`（调用方给统一文案，不静默）。

**安全边界**：二维码**不含 device token**，只含短时效（5 分钟）的一次性 code，
故其泄露窗口与手输码完全相同，不引入新的暴露面。

## 4. 组件与接口

### 4.1 共享编解码

- **前端**：`desktop/src/lib/pairUri.ts` — 纯函数、无依赖、可单测。
- **Rust**：`yi-agent-rs/crates/yi-agent-app-server/src/pair_uri.rs` — 纯函数、可单测。

两份用**同一组 fixture**（同一批 `(relay, code)` 输入需产出同一字符串；同一批非法输入需同判
`null`）做断言。这是防漂移的关键测试。

### 4.2 桌面「远程访问」页

- 新增依赖 `qrcode`（npm `qrcode@1.5.4`；纯 JS 编码器，输出 SVG/dataURL，不引 WASM）。
- `mint()` 拿到 code 后，若「中继地址」字段非空 → 在配对码旁渲染
  `buildPairUri(relayUrl, code)` 的二维码；字段为空 → 不渲染二维码，只出文本码（降级）。
- 二维码是纯展示，**不新增任何 RPC**。

### 4.3 CLI `yi-agent pair code`

- 新增可选参数 `--relay <url>`。
- 给了：除现有 `ABCD-EFGH (valid 300s)` 外，额外打印 Unicode 块字符二维码（Rust `qrcode` crate，0.14.1）。
- 不给：输出**逐字节不变**（既有测试继续通过）。

### 4.4 iOS App 内扫码

**分层（接缝可测）**

- `desktop/src/lib/qrScanner.ts` — 逻辑层：注入 `getUserMedia` 与解码器，返回
  `{ start(), stop(), onScan }` 风格的扫描循环。**不碰组件**，故错误分支、停止清理、
  多帧才命中等都能在 vitest 里用假 video 流测。
- `desktop/src/components/QrScanner.tsx` — 薄 UI：全屏相机预览 + 命中回调，只做 DOM/video 绑定。

**解码器选型**：`jsqr`（npm 包名小写；导入为 `jsQR`。纯 JS、无 WASM、社区成熟）；输入是
canvas 的 `ImageData`，逐帧调用，命中即停。

**配对页接线**（`PairingScreen.tsx`，复用既有 `initialUrl` / `onSubmit` / `redeem` 接缝）

- 加「扫码」按钮（仅 iOS/远程构建显示）。
- 命中 → `parsePairUri(text)`：
  - 成功 → 填 `relay`/`code` 并**立刻**走既有 redeem（自动配对）；
  - 失败 → 提示「不是有效的配对二维码」，扫描器保持打开可重试；
  - redeem 失败 → **关闭扫描器**，回表单并显示既有错误文案（可编辑重试）。

**相机权限**：iOS 需 `NSCameraUsageDescription`。`Info.plist` 由 Tauri 生成
（`desktop/src-tauri/gen/apple/desktop_iOS/Info.plist`，`tauri ios init` 会重生成），
故按 Tauri 的方式加权限（如 `tauri.conf.json` 的 iOS 配置或 Tauri 提供的 plist 注入机制），
**不手工改生成物**（会被 `ios init` 覆盖）。确切注入点留实现计划核实。

## 5. 错误处理

| 情形 | 行为 |
| --- | --- |
| 文本不是合法配对 URI | 统一文案「不是有效的配对二维码」，扫描器保持打开可重试 |
| scheme/版本/字段缺失 | 同上一律 `parsePairUri → null` |
| redeem 报 4401（码无效/过期/已用） | 复用 `INVALID_CODE_TEXT`，关闭扫描器回可编辑表单 |
| redeem 其他失败 | 复用 `CONNECT_FAILED_TEXT`，关闭扫描器回表单 |
| 相机权限拒绝 / 无 `getUserMedia` / 无相机 | 「无法访问相机，可手输配对码」，表单照常可用 |
| redeem 超时 | `defaultRedeem` 既有 15s 超时继续生效 |

**硬不变量**：任何扫码环节失败都不得使文本手输路径变差。

## 6. 已知限制（如实记录）

- **真实相机不可自动化测试**：`getUserMedia`、WKWebView 的相机权限生效、真机扫码，
  CI 里都无法覆盖，只能**真机手验**；且 iOS 产物目前仍因 Xcode 运行时/签名产不出。
- **要连上外网的前提不在本设计范围**：需要用户自行部署中继（VPS + TLS）、
  电脑侧带 `--relay` 的 app-server 在跑。二维码只是把「手输地址」换成「扫一下」，
  不改变这些前提。
- **不校验公钥指纹**：端到端加密是 v1.1，二维码因此不含指纹。
- **中继地址来源**：桌面 GUI 的中继地址取自页面上的可编辑字段（用户保证填对）；
  CLI 取自 `--relay` 参数。服务端 `pair/create` **不**返回中继地址——桌面 GUI 铸码走的是
  它自己的 stdio 进程，通常不是跑 `--relay` 的那个进程，跨进程给不出权威值。
  「记住上次地址」是可选增强，不在本设计。

## 7. 测试策略

**纯逻辑（Tier 0，两端）**

- `pairUri` 编解码：round-trip；`relay` 含 `?`/`&` 的 percent-encoding；拒绝错误
  scheme/版本/缺字段/非 `ws(s)://` 前缀；**Rust 与 TS 同一 fixture**。
- `qrScanner`：假视频流 + 假解码器跑通「命中即停」「多帧才命中」「解码器报错继续」
  「`stop()` 清理（不再调解码器）」。

**组件（vitest + jsdom）**

- 桌面 `SettingsRemoteTab`：填了地址 → 二维码出现（断言渲染的是 `buildPairUri` 的结果）；
  地址为空 → 无二维码、有文本码。
- `PairingScreen`：扫码命中 → 自动 redeem；解析失败 → 提示且不 redeem；
  无相机 → 降级文案 + 表单可用。
- CLI：`format_pair_code` 附近新增纯函数测「带 `--relay` 时输出含二维码」；
  无 `--relay` 时输出不变。

**门禁**

```
cargo test -p yi-agent-app-server
cargo test -p yi-agent
cd desktop && npx vitest run && npx tsc --noEmit
```

**验证命令参考**（本仓库环境）：

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
export CARGO_TARGET_DIR=/tmp/yi-s4
```

## 8. 兼容与迁移

- **纯增量**：不扫码的客户端行为不变；文本手输路径逐字节不变。
- 新增依赖：前端 `qrcode@1.5.4`（编码）、`jsqr@1.4.0`（解码）；Rust `qrcode@0.14.1`。
  均为成熟库，无 WASM（版本为撰写时 registry 实查值）。
- 无协议变更（不新增 RPC、不改 `PROTOCOL_VERSION`）。
