#!/usr/bin/env bash
# 独立发布验收：在**同一个 .app** 上安装插件 v1.0、再装 v1.1，确认
#   (a) 插件 UI 内容确实变了（标记从 board-v1 变成 board-v2）；
#   (b) 主程序发布物摘要一个字节都没变。
#
# 用法: verify-plugin-update.sh <app-path> <repo-dir>
#
# 为什么必须用同一个 .app：这正是"插件独立发布"的定义。如果每次改看板都要
# 重新打包主程序，(b) 就无从谈起；如果有人偷偷把看板资源塞回主程序包，
# (b) 依然成立但 `find "$APP" -iname '*kanban*'` 会空——所以这里同时检查
# 主程序包里不含看板资源。
#
# 这个脚本不做网络与签名以外的假设：包由 `plugin-pack` 现场生成，签名用
# 调用方给的测试身份，安装走真实的 `yi-agent plugins install`。
set -euo pipefail

if [ "$#" -ne 2 ]; then
  printf 'usage: %s <app-path> <repo-dir>\n' "$0" >&2
  exit 2
fi

APP="$1"
REPO="$2"

fail() { printf 'plugin update gate FAILED: %s\n' "$*" >&2; exit 1; }
step() { printf '\n== %s\n' "$*"; }

[ -d "$APP/Contents/MacOS" ] || fail "$APP is not a .app bundle"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

BIN="$REPO/yi-agent-rs/target/debug/yi-agent"
PACK="$REPO/yi-agent-rs/target/debug/plugin-pack"
[ -x "$BIN" ] || fail "$BIN is missing; run 'cargo build -p yi-agent -p yi-agent-plugins' first"
[ -x "$PACK" ] || fail "$PACK is missing; run 'cargo build -p yi-agent-plugins --bin plugin-pack' first"

PLUGIN_SRC="$REPO/plugins/superpowers-kanban"
[ -f "$PLUGIN_SRC/plugin.json" ] || fail "$PLUGIN_SRC has no manifest"

step "record the app digest before any plugin install"
before_binary="$(shasum -a 256 "$APP/Contents/MacOS/desktop" | awk '{print $1}')"
before_bundle="$(find "$APP" -type f | sort | xargs shasum -a 256 | shasum -a 256 | awk '{print $1}')"
printf 'desktop=%s\nbundle=%s\n' "$before_binary" "$before_bundle"

step "the app must not ship kanban resources"
if find "$APP" -iname '*kanban*' | grep -q .; then
  fail "the app bundle contains kanban resources; the plugin is not independent"
fi

step "build two plugin versions that differ only in their UI marker"
python3 - "$PLUGIN_SRC" "$WORK" <<'PY'
import hashlib, json, shutil, sys
from pathlib import Path

src = Path(sys.argv[1])
work = Path(sys.argv[2])
ignore = shutil.ignore_patterns("target", "node_modules", ".git", "crates")

for version, marker in (("1.0.0", "board-v1"), ("1.1.0", "board-v2")):
    dst = work / f"v{version}"
    shutil.copytree(src, dst, ignore=ignore)
    index = dst / "ui/dist/index.html"
    html = index.read_text()
    html = html.replace("</body>", f'<script>window.__PLUGIN_MARK="{marker}"</script></body>')
    index.write_text(html)
    manifest = json.loads((dst / "plugin.json").read_text())
    manifest["version"] = version
    digest = hashlib.sha256(index.read_bytes()).hexdigest()
    for view in manifest["views"]:
        view["digest"] = digest
    (dst / "plugin.json").write_text(json.dumps(manifest, indent=2) + "\n")
PY

step "pack and sign both versions"
python3 - "$WORK" <<'PY'
import base64, json, sys
from pathlib import Path
seed = base64.b64encode(bytes([7] * 32)).decode()
Path(sys.argv[1], "signing-key.json").write_text(
    json.dumps({"keyId": "root-2026-01", "privateKey": seed})
)
PY
for v in 1.0.0 1.1.0; do
  "$PACK" pack --dir "$WORK/v$v" --out "$WORK/pkgs" >/dev/null
  "$PACK" sign \
    --bundle "$WORK/pkgs/superpowers-kanban-$v-darwin-aarch64.tar.gz" \
    --key "$WORK/signing-key.json" \
    --out "$WORK/pkgs" \
    --serial 7 --published-at 1767225600 --expires-at 1798761600 >/dev/null
done

step "install v1.0 into an isolated instance"
export YI_AGENT_HOME="$WORK/instance"
"$BIN" plugins install \
  --bundle "$WORK/pkgs/superpowers-kanban-1.0.0-darwin-aarch64.tar.gz" \
  --metadata "$WORK/pkgs/superpowers-kanban-1.0.0-darwin-aarch64.json" >/dev/null
first="$(grep -o 'window.__PLUGIN_MARK="[^"]*"' \
  "$YI_AGENT_HOME/.yi-agent/plugins/installed/superpowers-kanban/1.0.0/darwin-aarch64/ui/dist/index.html")"
[ "$first" = 'window.__PLUGIN_MARK="board-v1"' ] || fail "v1.0 marker is $first"

step "install v1.1 on top of it"
"$BIN" plugins install \
  --bundle "$WORK/pkgs/superpowers-kanban-1.1.0-darwin-aarch64.tar.gz" \
  --metadata "$WORK/pkgs/superpowers-kanban-1.1.0-darwin-aarch64.json" >/dev/null
second="$(grep -o 'window.__PLUGIN_MARK="[^"]*"' \
  "$YI_AGENT_HOME/.yi-agent/plugins/installed/superpowers-kanban/1.1.0/darwin-aarch64/ui/dist/index.html")"
[ "$second" = 'window.__PLUGIN_MARK="board-v2"' ] || fail "v1.1 marker is $second"
[ "$first" != "$second" ] || fail "the UI marker did not change across versions"

step "both versions coexist and the newer one can be selected"
"$BIN" plugins list | grep -q 'installed=\[1.0.0,1.1.0\]' \
  || fail "the instance does not report both installed versions"
"$BIN" plugins activate --plugin superpowers-kanban --version 1.1.0 >/dev/null \
  || fail "activation of 1.1.0 did not start"

step "the activated release matches its own manifest"
# 只比「UI 变了」不够：验收要求版本、UI 摘要与 skill 来源都对得上被激活的那
# 一代。清单里的 digest 可能和磁盘内容不一致，skill 路径也可能指向空目录。
python3 "$REPO/scripts/plugins/verify-active-release.py" \
  "$YI_AGENT_HOME/.yi-agent/plugins/installed/superpowers-kanban/1.1.0/darwin-aarch64" \
  1.1.0 \
  || fail "the activated release does not match its own manifest"

step "the app digest must be unchanged"
after_binary="$(shasum -a 256 "$APP/Contents/MacOS/desktop" | awk '{print $1}')"
after_bundle="$(find "$APP" -type f | sort | xargs shasum -a 256 | shasum -a 256 | awk '{print $1}')"
[ "$before_binary" = "$after_binary" ] || fail "the app binary changed: $before_binary -> $after_binary"
[ "$before_bundle" = "$after_bundle" ] || fail "the app bundle changed: $before_bundle -> $after_bundle"

step "uninstalling keeps the user's project data and removes the managed entry"
# 「卸载」不能等于「删数据」。验收要求卸载后默认数据保留，而插件入口、授权与
# 运行记录消失。这里用真实数据文件验证，不靠日志措辞。
PROJECT="$WORK/project"
mkdir -p "$PROJECT/.yi-agent/superpowers-kanban"
printf '{"card":"keep-me"}\n' > "$PROJECT/.yi-agent/superpowers-kanban/board.json"
"$BIN" plugins enable-project --plugin superpowers-kanban --project "$PROJECT" \
  --capability read --capability write >/dev/null \
  || fail "enable-project refused a fresh project"
"$BIN" plugins list | grep -q 'projects=\[' || fail "the project grant is not visible"

"$BIN" plugins uninstall --plugin superpowers-kanban >/dev/null \
  || fail "uninstall failed"
[ -f "$PROJECT/.yi-agent/superpowers-kanban/board.json" ] \
  || fail "uninstall deleted the user's project data"
[ "$(cat "$PROJECT/.yi-agent/superpowers-kanban/board.json")" = '{"card":"keep-me"}' ] \
  || fail "uninstall changed the user's project data"
[ -z "$("$BIN" plugins list)" ] \
  || fail "uninstall left the plugin listed: $("$BIN" plugins list)"
supervisor_entry="$YI_AGENT_HOME/.yi-agent/plugins/supervisors/superpowers-kanban.json"
[ ! -f "$supervisor_entry" ] || fail "uninstall left the supervisor manifest at $supervisor_entry"

printf '\nUI marker %s -> %s, app digest unchanged (%s)\n' "$first" "$second" "$after_bundle"
printf 'uninstall kept %s and removed the managed entry\n' "$PROJECT/.yi-agent/superpowers-kanban/board.json"
