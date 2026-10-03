# iOS 远程控制与中继部署

本文覆盖 Tier 1 的 iOS 远程控制：自建反向 WSS 中继的部署、电脑侧连接、iOS App
构建与配对、故障排查与安全须知。

> **状态：实验性（Tier 1，配对端到端已打通）。** 协议、配对、多客户端 ws、撤销、
> 审批广播、中继与两端桥接都已落地并通过各自的单测；配对码已**落盘**，桌面铸码 →
> 手机输码兑换（含经中继的帧级 `pair/redeem`）→ 同屏控制这条链路（文本码）已可用。
> **桌面 GUI 与手机共用同一 app-server 会话（合一模式，§1.1）** 已实现，回环前端有单测/
> 集成测试覆盖；**真中继**下的端到端验证（§五）是最后一步，本文如实标注。仍有：本机产不出
> iOS 构建产物；配对已支持**扫码**（桌面/CLI 出码，手机相机扫）。
> 见 [范围与已知限制](#七范围与已知限制)，不要按"开箱即用"理解本文。

---

## 一、拓扑

有两种形态，**同一个中继**都支持：

### 1.1 合一模式（推荐；桌面 GUI + 手机共用一份会话）

一个 `app-server` 进程、一个 `serve()` 循环、一份内存会话状态（`threads`、runtimes、
审批队列……），桌面 GUI 与手机**同时**连它——手机看到的就是桌面正在用的那份会话。

```
  iPhone (App)                VPS                    你的电脑（一个 app-server 进程）
  ─────────────          ┌──────────────┐     ────────────────────────────────────
      │  wss (手机出站)     │ yi-agent-relay │          wss (电脑出站)
      └──────────────────▶│  :8080        │◀───────────────────────────┐
                          └──────────────┘                            │
                                                             中继桥（ws 客户端）
                                                                     │
                                                            回环 ws 127.0.0.1:0
                                                                     │
  桌面 GUI ──stdio (Admin)──────────────────────────────────▶┌───────▼─────────┐
                                                             │ 唯一 serve()     │
                                                             │ hub / threads    │
                                                             │ runtimes / 审批  │
                                                             └──────────────────┘
```

即：手机与电脑都**出站**连中继；电脑侧的中继桥再把帧转交给本机回环 ws，与桌面 GUI 的
stdio **汇入同一个 `serve()`**。

- 电脑侧仍是**出站**：app-server 作为 ws 客户端连中继的 `/connect`；本地只为中继桥
  在 `127.0.0.1:0` 起一个回环 ws 前端，**不开放任何入站端口**。
- 桌面 GUI 走 stdio（`ClientId="local"`，Admin），手机经中继接入（`ws-<uuid>`，Control）：
  **同一个 hub 扇出**，因此一侧发起的 turn、审批、状态更新会实时出现在另一侧。
- 生命周期：桌面 GUI 退出（stdio EOF）⇒ 整个进程连同手机连接一起优雅退出——手机控的是
  这台电脑，桌面关了它也就没有可服务的会话。

对应命令（下面 §三 详述）：

```bash
yi-agent app-server --relay 'wss://relay.example.com/connect?session=<你的 session id>'
```

`--relay <url>` 且 `--listen` 缺省（默认 `stdio://`）即此形态。

### 1.2 纯中继模式（无 GUI；headless/deploy）

没有 stdio 客户端，只有一个服务手机的中继桥。这是 `--relay` **旧的**行为，现在改用
长写法 `--listen relay://<url>`（见 §3.1）。

```
  iPhone (App)                 VPS                       你的电脑
  ─────────────           ┌──────────────┐        ─────────────────
      │  wss (手机出站)      │ yi-agent-relay │   wss (电脑出站)
      └───────────────────▶│  :8080        │◀────────────────┐
                           └──────────────┘                 │
                                        yi-agent app-server --listen relay://<url>
                                                  │ ws (仅回环)
                                                  ▼ 127.0.0.1:<随机端口>
                                        yi-agent app-server 主循环
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

中继启动后只跑一个极薄的 axum 服务，**不带 TLS**（见 §八 安全须知）。

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

### 3.1 `--relay` 与 `--listen relay://` 的语义（含一处行为变更）

两种写法的**组合**决定形态（`yi-agent-rs/crates/yi-agent/src/main.rs` 的
`app_server_mode`）：

| 命令行 | 形态 | 谁在连 |
| --- | --- | --- |
| `--relay <url>`（`--listen` 缺省或 `stdio://`） | **合一** | 桌面 stdio **和** 手机（经中继），同一个 `serve()` |
| `--listen relay://<url>`（无 `--relay`） | **纯中继** | 只有手机（经中继），**无** stdio 客户端 |
| `--relay <url>` + `--listen ws://…` | 纯中继 | 同上（沿用改动前"`--relay` 优先"的旧行为） |

> **行为变更（请注意）：** 单独一条 `--relay <url>`（不给 `--listen`）**以前**表示
> "纯中继、无 GUI"，**现在**表示"stdio + 中继合一"。想要旧的纯中继语义，请显式用
> `--listen relay://<url>`（该写法与行为完全不变）。若你的部署脚本/文档以裸 `--relay`
> 起一个 headless 侧车，需要改成 `--listen relay://…`。
>
> 为什么这样改：桌面侧车只要多写一行 `--relay <url>` 就能让手机接到**桌面正在用的**
> 那份会话，语义更直觉；而需要 headless 纯中继的场景本就用得上显式 `--listen`。

两处的 `<url>` 都是中继的**电脑侧端点**（`/connect?session=<id>`），必须以 `ws://` 或
`wss://` 开头，且**必须带 `?session=<id>`**，否则启动即报错（电脑与手机必须落在同一
session）。`--relay` 与 `--listen relay://` 的 URL 校验完全一致。

### 3.2 合一模式明细（`--relay`，推荐）

`yi_agent_app_server::serve_stdio_with_relay` 的行为：

1. 只起**一个** `serve()` 主循环，一份 `hub`、一张 `threads` 表；
2. 桌面 stdout/stdin 注册为 `local`（`Scope::Admin`，与今日 stdio 逐字节一致）；
3. 在本机 `127.0.0.1:0` 起**回环 ws 前端**（只回环，网络路径上无新增暴露面）；
4. 铸一枚**本机**设备 token（`seed_local_device("relay-bridge")`，scope `control`，按
   名字**幂等**：重启复用同一张设备卡）；
5. 作为 ws 客户端**出站**连中继的 `/connect?session=<id>`，双向桥接回环 ws 与中继；
6. 断线后按指数退避重连（1s → 2s → … 上限 60s），并在中继连接上每 30s 发一次
   ws Ping 保活（应对 NAT 超时、电脑睡眠唤醒）。

任一端的 turn 事件、审批请求、`thread/status/updated` 都经这唯一的 hub **扇出到双方**。

### 3.3 纯中继模式明细（`--listen relay://<url>`，headless/deploy）

`run_relay_mode` 的行为（与合一模式共享 3.2 的 3–6 步，但没有 stdio 客户端）：在本机
`127.0.0.1:0` 起一个普通 ws app-server，铸本机 `control` token 交给中继客户端出站连接，
双向桥接、退避重连、30s Ping。

```bash
yi-agent app-server --listen 'relay://wss://relay.example.com/connect?session=<你的 session id>'
```

> **注意：中继模式下本地 app-server 恒以那一枚 `control` token 的身份出现。** 经同
> 一条中继 session 进来的多台手机，在本地这一层**不区分** per-device scope；因此
> admin 类 RPC（`pair/create`、`device/revoke`、`thread/delete`、`process/kill`、
> `thread/setPermissionMode`）会被本地 app-server 正常拒绝——与手机直连时的行为一致
> （经中继无法执行 admin 操作是 v1 的已知限制）。合一模式下这条**同样成立**：只有
> 桌面 stdio（`local`/Admin）能发 admin RPC，手机一律 `control`。

### 3.4 在桌面 App 里启用合一模式（`YI_AGENT_RELAY`）

桌面的侧车启动逻辑（`desktop/src-tauri/src/bridge.rs`）读一个环境变量：

- **`YI_AGENT_RELAY` 已设置且非空白** → 侧车以 `app-server --listen stdio:// --relay
  <该值>` 启动，即**合一模式**；
- **未设置 / 空白** → 侧车仍是 `app-server --listen stdio://`，与今日行为**完全一致**
  （纯本地，不开中继）。

变量值是中继的**电脑侧 URL**，形如：

```
YI_AGENT_RELAY='wss://relay.example.com/connect?session=<你的 session id>'
```

- 名字与 §2.5 选定、手机侧填的 session id **必须一致**。
- 值会去掉首尾空白；空白串视为未设置。
- 改完后**重启桌面 App**（侧车在 App 启动时拉起；值在进程内读取）。
- 目前**没有**设置页字段：这是一个刻意的过渡做法（便于开发/启动脚本接入）。
  **未来计划**在桌面设置「远程访问」页加入中继地址字段（与配对/远程访问配置打通），
  届时无需再手动设环境变量。变量与设置页并存期间，环境变量仍是权威来源。

> GUI 方式验证（合一模式是否生效）：桌面 App 启动后，用 `ps` 或 App 日志确认侧车命令行
> 含 `--relay <你的 url>`；手机按 §4.4 配对后应能看到桌面正在用的 threads。

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

`tauri ios init` 已成功并提交；**iOS SDK 已就绪，但本机仍产不出可安装的 iOS 构建**。
（2026-10-03 实跑 `tauri ios build` 核实；此前记录为"iOS platform not installed"，已过时。）

- **iOS 26.5 SDK 已装**（Xcode 26.6，`iphoneos26.5` + `iphonesimulator26.5` 都在）。
  实跑能走完编译/链接/资源/Rust 构建脚本。
- **当前卡在签名**：构建最后一步报
  `Signing for "desktop_iOS" requires a development team.`——`tauri.conf.json` 未配
  `developmentTeam`。解决：Apple 开发者账号 + 在 `tauri.conf.json` 的 `bundle.iOS.
  developmentTeam`（或 Xcode 的 Signing & Capabilities）选择团队。
- **无模拟器运行时**：`xcrun simctl list runtimes` 为空，故模拟器也跑不起来。解决：
  `xcodebuild -downloadPlatform iOS`（约 8.5 GB）安装运行时。
- **无连接设备**：`xcrun devicectl list devices` = "No devices found"。

一句话：**Tier 1 未产出 `.ipa`/`.app`**。障碍已从"缺 SDK"变为"缺签名/运行时"——即
分发与账号问题，不是代码问题。

### 4.3 App 如何选择远端传输

App 启动时由 `desktop/src/transportFactory.ts` 决定：若 `localStorage` 里存在键
`yi-agent.remote`（形如 `{"url":"wss://.../ws?session=...","token":"yia_..."}`），
就用 `wsTransport` 连中继；否则退回桌面 Tauri 桥。该键只在配对成功后由
`saveRemoteConfig`（`desktop/src/lib/remoteConfig.ts`）写入，因此"有它 = 是远端
客户端"。

### 4.4 配对流程（端到端已打通）

配对协议本身已落地（`crates/yi-agent-app-server/src/ws.rs`）：

1. **电脑/桌面**调 RPC `pair/create`（admin-only）→
   `{"code":"XXXX-XXXX","expires_in":300}`，一次性、5 分钟过期。**码落盘**到
   `~/.yi-agent/pairing.json`（epoch 秒过期），因此**同一台机器上的任意 app-server
   进程**都能读到并兑现它——桌面 stdio 进程铸码、`--relay`/`ws://` 进程兑换这条
   跨进程链路因此成立。
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

入口：**iOS 首启配对表单**（`desktop/src/components/PairingScreen.tsx`，填中继地址 +
配对码 + 设备名）；**桌面设置「远程访问」页**（`SettingsRemoteTab.tsx`，铸码/列设备/
撤销）；无 GUI 时用 CLI `yi-agent pair code | list | revoke <id>`。

> **仍有的限制：**
>
> 1. **二维码已支持。** 桌面「远程访问」页在填了中继地址时显示二维码，`yi-agent pair
>    code --relay <url>` 在终端打印二维码，iOS 配对页「扫码」按钮用相机扫后自动配对；
>    文本手输路径保留。
> 2. 跨进程**同时**铸码存在极小丢失窗口（读-改-写非跨进程加锁），重试即可。

**经中继的兑换**（WAN 主路径）：中继只转发 ws **帧**、不改写升级查询串，所以手机的
`?pair=<code>` 到不了本机 app-server。为此服务端提供**帧级** `pair/redeem`
（`{code, device_name}` → `{device_id, token, scope}`，错误 `-32001`）：手机对中继开一条
ws，`initialize` 后直接发这条 RPC，中继桥把它原样转发到本机 app-server，据此铸设备。
`pair/redeem` **不在 admin 门禁内**——凭证就是那枚一次性码本身（只有桌面能铸）。
前端 `defaultRedeem` 会按 URL 自动选择：中继 URL（带 `session`）走帧级
`pair/redeem`，直连 app-server URL 走 `?pair=` 升级查询（因直连对无 token 连接
先回 4401、帧发不进去）。

---

## 五、手工端到端验证（合一模式）

> **状态：这一步（真中继下的 E2E）是本期最后一项、尚未完成。** 合一模式的**回环前端**
> 已有单测/集成测试（一个 stdio 客户端 + 一个回环 ws 客户端看到同一份 threads、事件互相
> 扇出、scope 门禁、stdio EOF 收尾），但还没有在**真中继 + 真手机**上完整跑过。下面按
> 步骤手验。

前置：§二 起好的中继可达（含 TLS 反代，两端用 `wss://`）；§2.5 选定的 session id 已知，
记为 `<ID>`。

1. **起中继**（若尚未起）。本地自测也可直接跑，但手机要能到达它，故通常用 VPS：
   ```bash
   # VPS 上（已在 Caddy 反代之后）
   yi-agent-relay --listen 127.0.0.1:8080
   # 电脑侧端点的 host 即反代域名：wss://relay.example.com/connect?session=<ID>
   ```
2. **起电脑侧（合一模式）**，二选一：
   - **GUI（推荐）**：在**同一个环境**里设好变量再启动桌面 App：
     ```bash
     export YI_AGENT_RELAY='wss://relay.example.com/connect?session=<ID>'
     # 然后在同一 shell/环境里启动桌面 App
     ```
     确认侧车命令行含 `--relay 'wss://…/connect?session=<ID>'`（`ps | grep app-server`
     或 App 日志）。
   - **CLI（用脚本代替 GUI 充当 stdio 客户端）**：
     ```bash
     yi-agent app-server --listen stdio:// \
       --relay 'wss://relay.example.com/connect?session=<ID>'
     ```
     这条命令同样跑**合一模式**，只是没有 GUI：它的 stdin/stdout 就是那唯一 `serve()` 的
     stdio 客户端，可用任意 JSON-RPC 脚本驱动这一侧。
3. **配对手机**：桌面「远程访问」页（或 `yi-agent pair code`）铸码，得到配对码（及二维码）；
   手机 iOS 配对页填中继地址 `wss://relay.example.com/ws?session=<ID>` + 配对码 + 设备名
   （或「扫码」）。按 §4.4，配对成功后手机会持久化 token 并以 `?token=` 重连。
4. **验证 (a)：手机列出桌面的 threads。** 在桌面上确保存在一个 thread（新建或打开一个），
   手机上应看到**同一份** thread 列表（同一 thread id、同一标题/状态），而不是一份空列表。
5. **验证 (b)：一侧的 turn 实时出现在另一侧。**
   - **桌面 → 手机**：从桌面发一条 prompt；手机应在**不刷新、不重进**的情况下实时看到这轮
     turn（流式文本、工具卡片、状态变更），因为它与桌面共享同一个 hub。
   - **手机 → 桌面**：从手机发一条 prompt；桌面应实时看到这轮 turn。桌面才是会话的主人，
     两边看到的是**同一条** thread 的同一轮。
6. **验证 (c)：审批广播（可选但值得一看）。** 让某轮 turn 触发一次工具审批：两端都应弹出
   审批；在**任一**端作答后，另一端据此关闭自己的弹窗（`approvalResolved`）。
7. **验证 (d)：生命周期。** 退出桌面 App（stdio EOF）⇒ 手机连接断开（合一进程随桌面退出，
   见 §1.1）；手机侧重连会一直失败直到桌面再次启动——这是刻意语义。

**未实跑的是第 4/5/(6) 步在真中继+真手机上的结果**：合一模式的回环前端已有单测/集成测试
覆盖，但真机 E2E 仍受可安装 iOS 产物阻塞（§4.2），待具备签名/运行时后进行。

---

## 六、故障排查

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
- 检查电脑侧 `--relay` 进程是否在跑、本地 loopback app-server 是否已起（**纯中继**
  模式会在 stderr 打印 `relaying via …; local app-server on 127.0.0.1:<port>`；**合一**
  模式（`--relay` + stdio）由桌面侧车持有 stdio，没有这行——改用 §3.4 的侧车命令行确认）。

### 合一模式下手机连上了，却看不到桌面正在用的会话

- **多半是侧车没带上 `--relay`。** 合一模式由环境变量 `YI_AGENT_RELAY` 驱动（§3.4）：
  它必须在**启动桌面 App 的那个进程环境**里就绪；改完要重启 App。
- 确认侧车命令行：`ps | grep app-server` 应能看到 `--relay 'wss://…?session=<ID>'`
  （缺省只有 `--listen stdio://`）。看不到就说明变量没生效（拼写/空值/未重启）。
- **session 必须两端一致**（§3.1）；不一致时两端都"连上了中继"，却各看各的，互不可见。

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

## 七、范围与已知限制

**Tier 1 已落地（协议与单测）：** 多客户端 ws + 设备 token 认证（4401）；配对码协议
（`pair/create` → `pair/redeemed` → 4403，一次性、5 分钟）；scope 体系（`observe <
control < admin`，新设备默认 `control`，admin 类 RPC 返回 `-32014`）；设备列表/撤销
（`device/list` 任意已握手客户端可读、`device/revoke` 需 admin，撤销即断连并废 token）；
审批广播 + `approvalResolved`；反向 WSS 中继；电脑侧 `--relay` 出站桥接；iOS target 与
前端传输接缝；按 thread 订阅过滤（`thread/subscribe`：客户端只收自己关心的会话，
已订阅时逐字流 100ms/4KB 合并降频，未订阅客户端不受影响）。

**桌面 GUI 与手机共用同一 app-server 会话（合一模式）已落地：** `app-server --relay
<url>`（`--listen` 缺省或 `stdio://`）由**一个** `serve()` 同时服务桌面 stdio（Admin）
与经中继接入的手机（Control），共享同一份 `threads`/runtimes/审批队列；官方 `--relay`
的**纯中继**语义改由 `--listen relay://<url>` 承载（**行为变更**，见 §3.1）；桌面侧车在
`YI_AGENT_RELAY` 设置时以合一模式启动（见 §3.4）。回环前端已由单测/集成测试覆盖（同一
thread 集合、事件扇出、scope 门禁、stdio EOF 收尾）；**真中继 + 真手机**的完整 E2E 尚待
跑通（见 §五）。允许 iPhone 与电脑**同时**接入同一会话。

**Tier 1.5 已打通端到端：** 配对码**落盘**到 `~/.yi-agent/pairing.json`（epoch 秒过期），
故桌面 stdio 进程铸的码可被 `--relay`/`ws://` 进程兑换。**经中继的 WAN 路径**用**帧级**
`pair/redeem`（中继只转发帧、不改写升级查询串，故手机对中继开 ws 后直接发这条 RPC，
桥原样转发到本机 app-server）。附带：`yi-agent pair {code,list,revoke}` CLI、iOS 首启配对
表单（`PairingScreen`）、桌面设置「远程访问」页。**扫码配对**：桌面「远程访问」页在填了
中继地址时于配对码旁显示二维码，`yi-agent pair code --relay <url>` 在终端打印二维码；
iOS 配对页「扫码」按钮用相机扫后自动配对（文本手输路径保留）。直连 app-server 时仍走 `?pair=` 升级
查询（直连对无 token 连接先回 4401，帧发不进去），`defaultRedeem` 按 URL 自动选择。

**仍不在 Tier 1.5（后续）：**

- **可安装的 iOS 产物**（见 §4.2）——受 Xcode 运行时/签名阻塞。

**不在 Tier 1（后续）：**

- **APNs 推送**——电脑/中继主动通知手机（后台唤醒）。
- **端到端加密**——v1 只靠 TLS，中继能看到明文帧；v1.1 再叠 E2E。
- **Android**——本期只做 iOS。

**仍缺（合一模式相关）：**

- **真中继 + 真手机的手工 E2E**——回环前端已有单测/集成测试，但§五 的完整步骤尚未实跑；
  另受可安装 iOS 产物阻塞（见 §4.2）。
- **桌面设置页的中继地址字段**——当前只认 `YI_AGENT_RELAY` 环境变量（见 §3.4），设置页
  字段是后续增强；届时不再需要手动设环境变量。

---

## 八、安全须知

- **中继自身不终止 TLS。** 必须放在 TLS 反向代理（Caddy/nginx）之后，两端一律用
  `wss://`。
- **准入即认证。** app-server 的 ws 传输对每条连接都要求设备 token；绑定非回环地址
  时启动会打印认证告警。不要把无 token 的内部端口暴露到公网。
- **admin 操作只在桌面可发。** `pair/create`、`device/revoke` 等是 admin-only；桌面
  GUI 经 stdio 边车（`serve_stdio` / 合一模式的 stdio 前端 → `Scope::Admin`）可发，网络
  客户端（直连 `ws://` 或经中继）一律 `control`，发 admin 类 RPC 会得到 `-32014`。合一
  模式下这条**同样成立**：stdio=`Admin`、中继接入的手机=`Control`，scope 门禁与是否共用
  一个 `serve()` 无关。`device/revoke` 对**直连的**被撤销设备会即时断开活连接并作废其
  token；**经中继的手机不适用**——中继只是透传，app-server 侧只看到桥接的单一身份
  （见 §4.4、§七）。
- **`~/.yi-agent/devices.json` 只存 token 哈希，但请按敏感文件保护**（撤销即删记录）。
- **背压是 fail-safe 的**：慢消费者被摘除、被撤销设备的帧被丢弃，不静默丢单帧。
