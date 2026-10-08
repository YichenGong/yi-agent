#!/usr/bin/env bash
# yi-agent 中继一键部署（Linux x86_64 / arm64）
#
# 用法（在 VPS 上以 root 运行）：
#   ./install.sh <域名> [session_id]
#
# 例：
#   ./install.sh relay.example.com 3f7ac1...
#
# 做四件事：
#   1) 把静态中继装到 /usr/local/bin/yi-agent-relay（零依赖）
#   2) 装 Caddy（**下载静态二进制**，不走 apt——apt 拉 cloudsmith 源会卡住）
#   3) 用 Caddy 反代 127.0.0.1:8080 并自动申请 Let's Encrypt 证书（wss 走 443）
#   4) 装 systemd 单元（开机自启 + 崩溃重启），并自检
#
# 前置：
#   - 域名 A 记录已指向本机公网 IP（否则 Caddy 签不出证书）
#   - 安全组放行 80 与 443（80 用于 ACME 校验，443 用于 wss）
#   - 中继的 8080 不要对公网开放（只回环）
set -euo pipefail

DOMAIN="${1:-}"
SESSION="${2:-}"
CADDY_VERSION="${CADDY_VERSION:-2.8.4}"

if [[ -z "$DOMAIN" ]]; then
  echo "用法: $0 <域名> [session_id]" >&2
  echo "例:   $0 relay.example.com 3f7ac1..." >&2
  exit 1
fi

if [[ $EUID -ne 0 ]]; then
  echo "请用 root 运行（或 sudo $0 ...）" >&2
  exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN_SRC="$SCRIPT_DIR/yi-agent-relay-linux-amd64"
BIN_DST="/usr/local/bin/yi-agent-relay"
CADDY_BIN="/usr/local/bin/caddy"

# 架构 → Caddy 发布包的命名片段（本脚本分发的中继二进制固定为 amd64）。
case "$(uname -m)" in
  x86_64 | amd64) CADDY_ARCH="amd64" ;;
  aarch64 | arm64) CADDY_ARCH="arm64" ;;
  *)
    echo "不支持的架构: $(uname -m)（只需 amd64 或 arm64）" >&2
    exit 1
    ;;
esac

need() { command -v "$1" >/dev/null 2>&1 || { echo "缺少命令: $1" >&2; exit 1; }; }
need curl
need tar
need systemctl

echo "==> [1/5] 安装中继二进制到 $BIN_DST"
if [[ ! -f "$BIN_SRC" ]]; then
  echo "找不到 $BIN_SRC。" >&2
  echo "它不进仓库（见 deploy/relay/README.md §1）：请先在 Mac 上构建并放到同目录。" >&2
  exit 1
fi
install -m 0755 "$BIN_SRC" "$BIN_DST"
echo "    已安装（静态链接，零依赖）"

echo "==> [2/5] 安装 Caddy（静态二进制，v$CADDY_VERSION）"
if [[ -x "$CADDY_BIN" ]]; then
  echo "    Caddy 已存在，跳过：$("$CADDY_BIN" version)"
else
  tmp="$(mktemp -d)"
  tarball="caddy_${CADDY_VERSION}_linux_${CADDY_ARCH}.tar.gz"
  url="https://github.com/caddyserver/caddy/releases/download/v${CADDY_VERSION}/${tarball}"
  echo "    下载 $url"
  curl -fsSL -o "$tmp/$tarball" "$url"
  tar -xzf "$tmp/$tarball" -C "$tmp" caddy
  install -m 0755 "$tmp/caddy" "$CADDY_BIN"
  rm -rf "$tmp"
  echo "    已安装：$("$CADDY_BIN" version)"
fi

# Caddy 以非 root 的 caddy 用户运行（更安全）。绑 443 是特权端口，故 systemd 单元
# 授予 CAP_NET_BIND_SERVICE——不加会报 `bind: permission denied`（曾踩）。
id caddy >/dev/null 2>&1 || useradd --system --home-dir /var/lib/caddy --shell /usr/sbin/nologin caddy
mkdir -p /etc/caddy /var/lib/caddy
chown caddy:caddy /var/lib/caddy

echo "==> [3/5] 写配置（systemd + Caddyfile）"
cat > /etc/systemd/system/yi-agent-relay.service <<'UNIT'
[Unit]
Description=yi-agent relay (reverse WSS pairer)
After=network.target

[Service]
ExecStart=/usr/local/bin/yi-agent-relay --listen 127.0.0.1:8080
Restart=always
RestartSec=2
Environment=RUST_LOG=yi_agent_relay=info
# 中继二进制内置 tracing（EnvFilter 默认 yi_agent_relay=info），RUST_LOG 生效。

[Install]
WantedBy=multi-user.target
UNIT

cat > /etc/systemd/system/caddy.service <<'UNIT'
[Unit]
Description=Caddy
After=network.target

[Service]
User=caddy
ExecStart=/usr/local/bin/caddy run --config /etc/caddy/Caddyfile
ExecReload=/usr/local/bin/caddy reload --config /etc/caddy/Caddyfile
Restart=on-failure
LimitNOFILE=1048576
# 非 root 用户绑 443 需要这个能力，否则 bind: permission denied。
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE

[Install]
WantedBy=multi-user.target
UNIT

cat > /etc/caddy/Caddyfile <<CADDY
$DOMAIN {
	reverse_proxy 127.0.0.1:8080
}
CADDY

echo "==> [4/5] 启动服务"
systemctl daemon-reload
# 8080 曾被旧进程占用导致 relay 报 `Address in use (os error 98)`：先释放。
if systemctl is-active --quiet yi-agent-relay; then
  systemctl restart yi-agent-relay
else
  # 清掉任何非 systemd 管着的旧中继占用者（早先手动测试遗留的常见坑）。
  if ss -ltnp 2>/dev/null | grep -q '127.0.0.1:8080'; then
    echo "    提示：8080 已被占用，尝试停止旧实例"
    pkill -f 'yi-agent-relay --listen' 2>/dev/null || true
    sleep 1
  fi
  systemctl enable --now yi-agent-relay
fi
systemctl enable --now caddy
# Caddyfile 可能已变更：重载（首次 enable 已启动，reload 幂等）。
systemctl reload caddy 2>/dev/null || systemctl restart caddy

echo "==> [5/5] 自检"
sleep 2
systemctl is-active --quiet yi-agent-relay && echo "    中继: active"
systemctl is-active --quiet caddy && echo "    Caddy: active"
ss -ltnp 2>/dev/null | grep -q '127.0.0.1:8080' && echo "    回环 8080: 监听中"
echo "    （证书签发可能需要几秒到几分钟，取决于域名解析是否已生效）"
echo
if [[ -n "$SESSION" ]]; then
  cat <<SUMMARY
完成。两端接入地址（请用同一 session）：

  电脑侧: wss://$DOMAIN/connect?session=$SESSION
  手机侧: wss://$DOMAIN/ws?session=$SESSION

下一步：把 Mac 上 ~/.yi-agent/preferences.json 的 relay_url 改成上面「电脑侧」那行，
或在桌面「设置 → 远程访问」填入；重启 Mac app 即可。

排查：
  systemctl status yi-agent-relay caddy
  journalctl -u yi-agent-relay -f          # 两端接入会打印 agent/app connected
  journalctl -u caddy -n 50 --no-pager     # 证书签发情况
SUMMARY
else
  cat <<SUMMARY
完成。请在两端用同一 session：

  电脑侧: wss://$DOMAIN/connect?session=<你的 session id>
  手机侧: wss://$DOMAIN/ws?session=<你的 session id>

（生成 session id: openssl rand -hex 16）
SUMMARY
fi