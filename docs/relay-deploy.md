# iOS 远程控制与中继部署

本文覆盖 Tier 1 的 iOS 远程控制：自建反向 WSS 中继的部署、电脑侧连接、iOS App
构建与配对、故障排查与安全须知。

> **状态：实验性（Tier 1）。** 协议、配对、多客户端 ws、撤销、审批广播、中继与
> 两端桥接都已落地并通过各自的单测；但**跨中继的扫码配对链路尚未打通**，iOS 的
> **首次配对界面也尚未实现**，且本机产不出 iOS 构建产物。见
> [范围与已知限制](#六范围与已知限制)，不要按"开箱即用"理解本文。

---

## 一、拓扑

```
  iPhone (App)                 VPS                    你的电脑
  ─────────────           ┌──────────────┐        ─────────────────
      │  wss (手机出站)      │ yi-agent-relay │   wss (电脑出站)
      └───────────────────▶│  :8080        │◀────────────────┐
                           └──────────────┘                 │
                                        yi-agent app-server --relay
                                                  │ ws (仅回环)
                                                  ▼ 127.0.0.1:<随机端口>
                                        yi-agent app-server 主循环
                                        (与桌面同一套会话/工具)
```

两端**都是出站连接**：电脑主动连中继的 `/connect`，手机主动连中继的 `/ws`；中继
不主动连任何一方，也**不解析 JSON-RPC**，只按 `session` 转发 ws 帧。电脑侧不开放
任何入站端口（本地 app-server 只绑 `127.0.0.1` 的随机端口）。

中继的两个端点（`crates/yi-agent-relay/src/server.rs`）：

| 端点 | 谁连 | 说明 |
| --- | --- | --- |
| `/connect?session=<id>` | 电脑（agent） | 每个 session 只保留一条最新的电脑连接 |
| `/ws?session=<id>` | 手机（app） | 同一 session 可多条，扇出同一份电脑帧 |

`session` 两端必须一致；缺省为 `"default"`（电脑侧**必须显式给出**，见下文）。
非 ws 的普通 HTTP 请求会得到一行文字说明。

---

## 二、部署中继（VPS）

### 2.1 准备

- 一台有公网 IP 的 VPS（Linux，systemd）。
- 一个指向它的域名，例如 `relay.example.com`。
- 安装 Rust 工具链（构建中继）或直接分发已编译的 `yi-agent-relay` 二进制。

中继启动后只跑一个极薄的 axum 服务，**不带 TLS**（见 §七 安全须知）。

### 2.2 构建

中继是工作区里的独立 crate，可执行文件名为 `yi-agent-relay`：

```bash
cd yi-agent-rs
cargo build --release -p yi-agent-relay
# 产物：yi-agent-rs/target/release/yi-agent-relay
```

### 2.3 域名与 TLS（Let's Encrypt）

中继自身不做 TLS，**请把它放在 TLS 反向代理之后**，手机/电脑都用 `wss://`。用
Caddy 最省事（自动申请 Let's Encrypt 证书，并透明转发 WebSocket 升级）：

```caddyfile
# /etc/caddy/Caddyfile
relay.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

```bash
sudo systemctl reload caddy
```

nginx 等价配置（需自行配好证书与 `Upgrade`/`Connection` 头）：

```nginx
location / {
    proxy_pass http://127.0.0.1:8080;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade";
    proxy_set_header Host $host;
}
```

中继默认监听 `127.0.0.1:8080`，**只回环**即可——对外由反向代理终止 TLS，公网无需
直接暴露 8080。

### 2.4 运行

```bash
# 默认 127.0.0.1:8080
yi-agent-relay

# 或显式指定监听地址
yi-agent-relay --listen 127.0.0.1:8080
```

systemd 单元示例：

```ini
# /etc/systemd/system/yi-agent-relay.service
[Unit]
Description=yi-agent relay
After=network.target

[Service]
ExecStart=/usr/local/bin/yi-agent-relay --listen 127.0.0.1:8080
Restart=always
RestartSec=2
Environment=RUST_LOG=yi_agent_relay=info

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload && sudo systemctl enable --now yi-agent-relay
```

### 2.5 选定 session id

`session` 是电脑与手机约定的一个共享标识，等价于"这条隧道"。生成一个随机值即可：

```bash
openssl rand -hex 16     # 例如 3f7a...c1
```

后面电脑侧、手机侧都用同一个值。

---

## 三、电脑侧：经中继连接

在跑 app-server 的电脑上：

```bash
yi-agent app-server --relay 'wss://relay.example.com/connect?session=<你的 session id>'
```

`--relay <url>` 等价于 `--listen 'relay://<同一个 url>'`；两者都给时 `--relay` 优先
（`crates/yi-agent/src/main.rs` 的 `run_app_server`）。URL 必须以 `ws://` 或 `wss://`
开头，且**必须带 `?session=<id>`**，否则直接报错（电脑与手机必须落在同一 session）。

它的行为（`run_relay_mode`）：

1. 在本机 `127.0.0.1:0` 起一个**普通的** ws app-server（只回环，网络路径上无新增
   暴露面）；
2. 铸一枚**本机**设备 token（scope `control`）供中继桥连接本地 ws；
3. 作为 ws 客户端**出站**连中继的 `/connect?session=<id>`，双向桥接本地 ws 与中继；
4. 断线后按指数退避重连（1s → 2s → … 上限 60s），并在中继连接上每 30s 发一次
   ws Ping 保活（应对 NAT 超时、电脑睡眠唤醒）。

> **注意：中继模式下本地 app-server 恒以那一枚 `control` token 的身份出现。** 经同
> 一条中继 session 进来的多台手机，在本地这一层**不区分** per-device scope；因此
> admin 类 RPC（`pair/create`、`device/revoke`、`thread/delete`、`process/kill`、
> `thread/setPermissionMode`）会被本地 app-server 正常拒绝——与手机直连时的行为一致
> （经中继无法执行 admin 操作是 v1 的已知限制）。

---

## 四、iOS App：构建与配对

### 4.1 工程位置与构建命令

iOS 工程由 Tauri 生成并已提交在 `desktop/src-tauri/gen/apple/`：

```bash
cd desktop
npx tauri ios init      # 生成/刷新 iOS 工程（已提交；重复执行不会覆盖业务代码）
npx tauri ios build     # 产出 iOS 构建（需 Xcode + iOS 平台组件 + 签名）
```

### 4.2 构建状态（如实说明）

`tauri ios init` 已成功并提交；但**本机目前产不出 iOS 构建**：

- Xcode 缺少 **iOS 26.5 Simulator 运行时**组件。`npx tauri ios build` 会报
  `iOS platform not installed`；直接 `xcodebuild` 也失败：`CoreSimulator is out of
  date. Current version (1051.54.0) is older than build version (1051.55.0)`。
- 解决路径：**Xcode → Settings → Components** 安装 iOS 平台（或
  `xcodebuild -downloadPlatform iOS`，约 8.5 GB），并把 macOS 的 CoreSimulator
  更新到 `1051.55.0` 以上。
- 装到真机另需 Apple 开发者签名配置。

一句话：**Tier 1 未产出 `.ipa`/`.app`**，只落地了工程与前端接缝。

### 4.3 App 如何选择远端传输

App 启动时由 `desktop/src/transportFactory.ts` 决定：若 `localStorage` 里存在键
`yi-agent.remote`（形如 `{"url":"wss://.../ws?session=...","token":"yia_..."}`），
就用 `wsTransport` 连中继；否则退回桌面 Tauri 桥。该键只在配对成功后由
`saveRemoteConfig`（`desktop/src/lib/remoteConfig.ts`）写入，因此"有它 = 是远端
客户端"。

### 4.4 配对流程（协议已就绪，界面待建）

配对协议本身已落地（`crates/yi-agent-app-server/src/ws.rs`）：

1. **电脑/桌面**调 RPC `pair/create`（admin-only）→
   `{"code":"XXXX-XXXX","expires_in":300}`，一次性、5 分钟过期。
2. **手机**连 `ws(s)://host/ws?pair=<code>&device_name=<name>`；
3. 服务端回**一帧** Text：

   ```json
   {"jsonrpc":"2.0","method":"pair/redeemed",
    "params":{"device_id":"dev-…","token":"yia_…","scope":"control"}}
   ```

   随后以 ws close **4403**（"已配对，请带 token 重连"）关闭这条一次性连接。
4. 手机持久化 `token`，以后用 `ws(s)://host/ws?token=<token>` 走正常连接。
5. 码不存在/已用/已过期/未提供 → 一律 close **4401**（与"无 token"不可区分，不泄露
   码是否存在）。

两端的前端接缝已实现并带单测：`desktop/src/pairing.ts` 的
`redeemPairCode(url, code, name)`；`desktop/src/wsTransport.ts` 的 `withToken`
把 token 放到 `?token=`（浏览器 WebSocket / iOS WKWebView 无法设置握手请求头，故
只能用查询串形式；服务端也接受 `Authorization: Bearer <token>`）。

> **两个必须知道的限制：**
>
> 1. **首次配对界面尚未实现。** `pairing.ts`/`remoteConfig.ts` 是接缝，但**调用它们
>    的那个屏幕还没建**。当前远端客户端若没有持久化配置会退回 `tauriTransport()`。
>    在此之前，需要手工把 `{url, token}` 写进 App 的
>    `localStorage["yi-agent.remote"]`，或等后续任务补齐配对界面。
> 2. **经中继目前无法完成配对兑换。** 兑现发生在 app-server 的 ws **升级请求**上
>    （`?pair=<code>`），而中继只转发 ws **帧**、不改写升级查询串，其手机侧端点是
>    `/ws?session=<id>`（无 `pair`）。因此手机要拿 token，需要一次能直接到达
>    app-server `/ws` 的连接（例如 `ws://<电脑>:<port>/ws?pair=<code>`）。跨中继的
>    配对是后续工作，不在 Tier 1。
>
> 另：`pair/create` 铸出的码**只存在该进程的内存里**（未持久化到 `devices.json`），
> 而桌面 GUI 的 sidecar 是 `yi-agent app-server --listen stdio://`、`--relay` 模式是
> **另一个进程**。要配对，须让**同一个** app-server 实例既铸码又接受兑现——两个进程
> 不共享待兑现的码。

---

## 五、故障排查

### 连不上中继

- 手机：确认 URL 是 `wss://` 且 host 正确；用 `curl -v https://relay.example.com/`
  确认反向代理在跑（应返回中继的一行说明文字）。
- 电脑：确认 `--relay` URL **带了 `?session=`** 且与手机一致；不一致时两端各自连上
  中继也不会互相看见。
- 看中继日志（`RUST_LOG=yi_agent_relay=info`）：`agent connected` / `app connected`
  分别对应电脑/手机侧升级成功。
- 电脑侧反向代理需支持 WebSocket 升级（Caddy 开箱即用；nginx 要 `Upgrade`/
  `Connection` 头，见 §2.3）。

### 手机发消息没有电脑响应

- 中继在"该 session 没有电脑"时，会把手机发来的帧回一个最小错误帧：
  `{"jsonrpc":"2.0","error":{"code":-32000,"message":"no computer connected for this
  session"}}`。看到它说明电脑侧没连上（或已断开，正在退避重连）。
- 检查电脑侧 `--relay` 进程是否在跑、本地 loopback app-server 是否已起（电脑侧
  stderr 会打印 `relaying via …; local app-server on 127.0.0.1:<port>`）。

### Token 无效 / 连接被 4401 关闭

- 4401 表示**未提供或无效 token**（包括绑定在回环时）。token 是配对战利品，须由
  `pair/redeemed` 取得后持久化，不能手造。
- 若设备已被 `device/revoke` 撤销：中继（或直连）上该设备的**活连接会被关掉**，其
  token 立即失效（fail-closed）。重新配对即可。
- 若曾改过 `~/.yi-agent/devices.json`，确认 token 与表中记录匹配（表中只存哈希）。

### 审批弹不出 / 审批在别的设备上消失

- 审批（`item/toolCall/requestApproval`）会**广播给所有已连接客户端**，谁先答谁生效；
  随后 `item/toolCall/approvalResolved {perm_id, by, decision}` 也会广播，其它设备据此
  关闭自己的弹窗。所以"我还没点，弹窗就没了"通常表示**另一台设备已作答**，属预期。
- 若完全没有弹窗：确认手机在运行时持有的是**控制端**连接（已握手 `initialize`），
  并且电脑侧确实进入了 `awaiting_approval`。

### 手机偶尔掉线 / 收不到后续事件

- 中继与 app-server 都对"跟不上的慢消费者"采取**摘除 + 重连**策略（出站队列写满即
  踢），而不是在 JSON-RPC 流中间静默丢帧。恢复路径是**重连后 `thread/resume` 增量
  回放**。若频繁触发，多为网络抖动或手机长时间后台。

---

## 六、范围与已知限制

**Tier 1 已落地（协议与单测）：** 多客户端 ws + 设备 token 认证（4401）；配对码协议
（`pair/create` → `pair/redeemed` → 4403，一次性、5 分钟）；scope 体系（`observe <
control < admin`，新设备默认 `control`，admin 类 RPC 返回 `-32014`）；设备列表/撤销
（`device/list` 任意已握手客户端可读、`device/revoke` 需 admin，撤销即断连并废 token）；
审批广播 + `approvalResolved`；反向 WSS 中继；电脑侧 `--relay` 出站桥接；iOS target 与
前端传输接缝。

**尚未端到端打通（生产不可组合）：**

- **扫码配对**：配对码只存于**创建它的进程内存**，从不落盘；`pair/create` 由桌面
  stdio 边车进程服务，而跨中继/跨进程的 `?pair=` 兑换发生在**另一个进程**（`--relay`/
  `ws://`），二者 `PairingState` 不共享内存，故生产环境无法用桌面生成的码完成兑换。
  单测通过是因为在**同一进程内** create + serve_ws（见 §4.4）。

**不在 Tier 1（后续）：**

- **APNs 推送**——电脑/中继主动通知手机（后台唤醒）。
- **端到端加密**——v1 只靠 TLS，中继能看到明文帧；v1.1 再叠 E2E。
- **按 thread 订阅过滤**——目前所有客户端收同一份扇出。
- **Android**——本期只做 iOS。
- **iOS 首启配对界面**与**可安装的 iOS 产物**（见 §4.2、§4.4）。
- **跨中继配对兑换**（见 §4.4）。

---

## 七、安全须知

- **中继自身不终止 TLS。** 必须放在 TLS 反向代理（Caddy/nginx）之后，两端一律用
  `wss://`。
- **准入即认证。** app-server 的 ws 传输对每条连接都要求设备 token；绑定非回环地址
  时启动会打印认证告警。不要把无 token 的内部端口暴露到公网。
- **admin 操作只在桌面可发。** `pair/create`、`device/revoke` 等是 admin-only；桌面
  GUI 经 stdio 边车（`serve_stdio` → `Scope::Admin`）可发，网络客户端（直连 `ws://`
  或经中继）一律 `control`，发 admin 类 RPC 会得到 `-32014`。`device/revoke` 对**直连
  的**被撤销设备会即时断开活连接并作废其 token；**经中继的手机不适用**——中继只是透传，
  app-server 侧只看到桥接的单一身份（见 §4.4、§六）。
- **`~/.yi-agent/devices.json` 只存 token 哈希，但请按敏感文件保护**（撤销即删记录）。
- **背压是 fail-safe 的**：慢消费者被摘除、被撤销设备的帧被丢弃，不静默丢单帧。
