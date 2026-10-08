#!/usr/bin/env bash
# yi-agent 中继一键部署（Ubuntu/Debian，x86_64）
#
# 用法（在 VPS 上以 root 或 sudo 运行）：
#   ./install.sh <域名> [session_id]
#
# 例：
#   ./install.sh relay.example.com 3f7ac1...
#
# 做三件事：
#   1) 把静态中继装到 /usr/local/bin/yi-agent-relay（零依赖）
#   2) 用 Caddy 反代 127.0.0.1:8080 并自动申请 Let's Encrypt 证书（wss 用 443）
#   3) 装 systemd 单元，开机自启 + 崩溃自动重启
#
# 前置：
#   - 域名 A 记录已指向本机公网 IP（否则 Caddy 签不出证书）
#   - 安全组放行 80 与 443（80 用于 ACME 校验，443 用于 wss）
#   - 中继的 8080 不要对公网开放（只回环）
set -euo pipefail

DOMAIN="${1:-}"
SESSION="${2:-}"

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

echo "==> [1/4] 安装中继二进制到 $BIN_DST"
install -m 0755 "$BIN_SRC" "$BIN_DST"
"$BIN_DST" --help >/dev/null 2>&1 || true
echo "    完成: $("$BIN_DST" --version 2>/dev/null || echo '(无版本子命令)')"

echo "==> [2/4] 安装 Caddy（若缺）"
if ! command -v caddy >/dev/null 2>&1; then
  apt-get update -qq
  apt-get install -y -qq debian-keyring debian-archive-keyring apt-transport-https curl gnupg
  curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' \
    | gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg
  curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' \
    | tee /etc/apt/sources.list.d/caddy-stable.list >/dev/null
  apt-get update -qq
  apt-get install -y -qq caddy
else
  echo "    Caddy 已安装，跳过"
fi

echo "==> [3/4] 写配置（systemd + Caddy）"
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

cat > /etc/caddy/Caddyfile <<CADDY
$DOMAIN {
	reverse_proxy 127.0.0.1:8080
}
CADDY

systemctl daemon-reload
systemctl enable --now yi-agent-relay
systemctl reload caddy 2>/dev/null || systemctl restart caddy

echo "==> [4/4] 自检"
sleep 1
systemctl is-active yi-agent-relay && echo "    中继: active"
curl -fsS "http://127.0.0.1:8080/" >/dev/null && echo "    回环 8080: 可达"
echo
if [[ -n "$SESSION" ]]; then
  cat <<SUMMARY
完成。两端接入地址（请用同一 session）：

  电脑侧: wss://$DOMAIN/connect?session=$SESSION
  手机侧: wss://$DOMAIN/ws?session=$SESSION

下一步：把 Mac 上 ~/.yi-agent/preferences.json 的 relay_url 改成上面「电脑侧」那行，
或在桌面「设置 → 远程访问」填入；重启 Mac app 即可。
SUMMARY
else
  cat <<SUMMARY
完成。请在两端用同一 session：

  电脑侧: wss://$DOMAIN/connect?session=<你的 session id>
  手机侧: wss://$DOMAIN/ws?session=<你的 session id>

（生成 session id: openssl rand -hex 16）
SUMMARY
fi
