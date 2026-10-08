# 中继 VPS 部署（一键）

把 `yi-agent-relay` 部署到一台有公网 IP 的 Linux VPS，让**手机在任何网络**（4G/5G/异地）
都能连上你电脑上的会话。两端都出站，VPS 只做按 `session` 配对的转发。

> **安全提醒**：中继按 `session` 配对转发，**不鉴权**——`session_id` 本身就是凭据。
> 请把地址与 `session_id` **按秘密保管**（见 `docs/relay-deploy.md` §2.6），
> 不要提交进公开仓库。本机真实值记在 `deploy/relay/.relay-local.md`（已 gitignore）。

## 本机当前部署（真实值）

本机在跑的这套部署的真实域名/IP/session 记在 **`deploy/relay/.relay-local.md`**
（该文件已进 `.gitignore`，不会随仓库公开）。丢了就照下方步骤重新部署。

## 快速开始

## 0. 你需要准备

| 项 | 说明 |
|---|---|
| **VPS** | 任意 Linux x86_64（Ubuntu 22.04+ 推荐）。1核1G 足够——中继是极薄转发器 |
| **公网 IPv4** | 必须。手机/电脑都连它 |
| **域名** | 一条 A 记录指向 VPS 公网 IP（`wss://` 必需，Caddy 靠它签免费证书） |
| **安全组** | 放行 **80**（ACME 校验）与 **443**（wss）；**8080 不要对公网开** |

> 为什么必须域名：中继自身不做 TLS（设计如此），必须放在 TLS 反代后，两端用 `wss://`。

## 1. 上传

在**本机**（Mac）执行，把部署目录传到 VPS：

```bash
cd <本仓库>/deploy/relay
scp -r ./* root@<VPS_IP>:/root/relay-deploy/
```

或只传三个文件：

```bash
scp yi-agent-relay-linux-amd64 install.sh README.md root@<VPS_IP>:/root/
```

> 二进制是**静态链接**的 musl 构建（`ELF 64-bit, statically linked`），
> 任何 x86_64 Linux 直接跑，不需要装任何库。

## 2. 部署（在 VPS 上）

```bash
ssh root@<VPS_IP>
cd /root/relay-deploy        # 或 /root
chmod +x install.sh
./install.sh <你的域名> <session_id>
```

`session_id` 是电脑与手机约定的共享标识，等于"这条隧道"。生成一个随机值：

```bash
openssl rand -hex 16
```

脚本会：装中继到 `/usr/local/bin/`、装 Caddy、写 Caddyfile（自动 HTTPS）、
装 systemd 单元（开机自启 + 崩溃重启），并做自检。

## 3. 接入地址

两端用**同一个 session**，注意路径不同：

```
电脑侧:  wss://<域名>/connect?session=<session_id>
手机侧:  wss://<域名>/ws?session=<session_id>
```

## 4. 配置电脑侧

把 Mac 侧车指向公网中继，二选一：

- **改偏好文件**：`~/.yi-agent/preferences.json` 里加/改
  `"relay_url": "wss://<域名>/connect?session=<session_id>"`，然后重启 Mac app。
- **环境变量**（优先级更高）：`YI_AGENT_RELAY="wss://<域名>/connect?session=<session_id>"`。
- **桌面设置页**：「设置 → 远程访问」填入中继地址保存。

## 5. 手机侧配对

iOS app → 配对页 → 地址填 `wss://<域名>/ws?session=<session_id>`，
配对码由电脑侧生成（桌面「远程访问」页，或 CLI `yi-agent pair code`）。

## 6. 验证与排查

```bash
# 中继状态
systemctl status yi-agent-relay
journalctl -u yi-agent-relay -n 50 --no-pager

# 看两端是否连上（应有 agent connected / app connected）
journalctl -u yi-agent-relay -f

# Caddy 证书/反代
systemctl status caddy
journalctl -u caddy -n 50 --no-pager
```

手机连不上时按序查：
1. `wss://<域名>/` 在浏览器能否打开（证书是否签发成功）；
2. 中继日志有无 `agent connected`（电脑侧是否连上）；无则查电脑侧 `relay_url` 与重启；
3. 有无 `app connected`（手机侧是否连上）；无则查手机地址/网络；
4. 两端 session 是否**完全一致**（大小写/空格）。

## 7. 多对多的扩展点（当前架构已预留）

中继现在的模型是 `session → { 唯一电脑, 多台手机 }`（见 `yi-agent-relay/src/lib.rs`）：
- **同一 session，多台手机天然共存**（电脑的帧扇出给全部手机）。
- 要扩展成多对多，增量**全在中继侧**（独立的 `yi-agent-relay` crate），
  不碰桌面 app 与 iOS app：
  1. 加一张"会话目录"（`account → sessions`），手机先拉列表再选；
  2. `agents` 由 `HashMap<session, Slot>` 改成 `HashMap<session, Vec<Slot>>`（与手机侧对称）；
  3. 真正的 M:N 时，把路由表外置到 DB/配置，保持中继进程"可无状态重启"。

公网 VPS 是这一扩展的基座——它是所有设备唯一能共同到达的稳定地址。
