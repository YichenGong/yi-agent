#!/usr/bin/env bash
# install.sh 的单元测试。不碰 /Applications，不调真实 launchctl。
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALLER="$HERE/../packaging/install.sh"
PASS=0
FAIL=0

ok()   { PASS=$((PASS + 1)); printf 'ok   - %s\n' "$1"; }
fail() { FAIL=$((FAIL + 1)); printf 'FAIL - %s\n' "$1" >&2; }

assert_eq() { # assert_eq <name> <expected> <actual>
  if [ "$2" = "$3" ]; then ok "$1"; else fail "$1 (expected [$2], got [$3])"; fi
}

# shellcheck source=../packaging/install.sh
. "$INSTALLER"

assert_eq "app_path honors override" \
  "/tmp/fake/yi-agent.app" \
  "$(YI_AGENT_APP_PATH=/tmp/fake/yi-agent.app app_path)"

assert_eq "app_path defaults to /Applications" \
  "/Applications/yi-agent.app" \
  "$(app_path)"

TMPROOT="$(mktemp -d)"
trap 'rm -rf "$TMPROOT"' EXIT

mk_app() { # mk_app <dir> <version> [cli-exit-code]
  local dir="$1" version="$2" code="${3:-0}"
  mkdir -p "$dir/Contents/MacOS"
  cat > "$dir/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0"><dict>
<key>CFBundleShortVersionString</key><string>$version</string>
</dict></plist>
PLIST
  cat > "$dir/Contents/MacOS/yi-agent" <<CLI
#!/bin/sh
echo "yi-agent $version"
exit $code
CLI
  chmod +x "$dir/Contents/MacOS/yi-agent"
}

APP="$TMPROOT/Fake.app"
mk_app "$APP" "1.2.3"

assert_eq "app_version reads plist" "1.2.3" "$(app_version "$APP")"
assert_eq "cli_version runs binary" "yi-agent 1.2.3" "$(cli_version "$APP")"
assert_eq "app_version missing plist is empty" "" \
  "$(app_version "$TMPROOT/NoSuch.app" 2>/dev/null)"

assert_eq "version_banner first install" \
  "已安装 yi-agent 1.2.3" \
  "$(version_banner "" "1.2.3")"
assert_eq "version_banner upgrade" \
  "检测到已安装版本 1.2.2 → 升级到 1.2.3" \
  "$(version_banner "1.2.2" "1.2.3")"

# --- pick_bin_dir ---
BIN1="$TMPROOT/bin-a"; BIN2="$TMPROOT/bin-b"
mkdir -p "$BIN1" "$BIN2"
assert_eq "pick_bin_dir prefers first writable" \
  "$BIN1" \
  "$(YI_AGENT_BIN_DIRS=$BIN1:$BIN2 pick_bin_dir)"

NOPERM="$TMPROOT/bin-noperm"
mkdir -p "$NOPERM"; chmod 500 "$NOPERM"
assert_eq "pick_bin_dir skips unwritable" \
  "$BIN2" \
  "$(YI_AGENT_BIN_DIRS=$NOPERM:$BIN2 pick_bin_dir)"

# 注意：这条断言必须在恢复权限之前跑，否则目录又可写了。
assert_eq "pick_bin_dir fails when none writable" \
  "rc=1" \
  "$(YI_AGENT_BIN_DIRS=$NOPERM pick_bin_dir >/dev/null 2>&1; echo "rc=$?")"
chmod 700 "$NOPERM"

# 覆盖为空串表示「一个候选都没有」，不回落默认目录。
if [ "$(id -u)" -ne 0 ]; then
  assert_eq "pick_bin_dir fails on empty list" \
    "rc=1" "$(YI_AGENT_BIN_DIRS="" pick_bin_dir >/dev/null 2>&1; echo "rc=$?")"
fi

# --- link_cli ---
LINKDIR="$TMPROOT/linkbin"; mkdir -p "$LINKDIR"
link_cli "$APP" "$LINKDIR"
assert_eq "link_cli creates symlink to app cli" \
  "$APP/Contents/MacOS/yi-agent" \
  "$(readlink "$LINKDIR/yi-agent")"
# command -v 走的是当前 PATH，必须把测试目录放进去才能命中。
assert_eq "link_status reports ok" \
  "ok" "$(PATH="$LINKDIR:$PATH" link_status "$APP" "$LINKDIR")"

assert_eq "link_cli fails without cli" \
  "rc=1" "$(link_cli "$TMPROOT/NoSuch.app" "$LINKDIR" >/dev/null 2>&1; echo "rc=$?")"

# 模拟 PATH 里更靠前的同名程序
SHADOW="$TMPROOT/shadow"; mkdir -p "$SHADOW"
printf '#!/bin/sh\nexit 0\n' > "$SHADOW/yi-agent"; chmod +x "$SHADOW/yi-agent"
assert_eq "link_status detects shadowing" \
  "shadowed:$SHADOW/yi-agent" \
  "$(PATH="$SHADOW:$LINKDIR:$PATH" link_status "$APP" "$LINKDIR")"

# --- replace_app ---
SRC="$TMPROOT/Src.app"; mk_app "$SRC" "2.0.0"
DST="$TMPROOT/Dst.app";  mk_app "$DST" "1.0.0"

replace_app "$SRC" "$DST"
assert_eq "replace_app installs new version" "2.0.0" "$(app_version "$DST")"
assert_eq "replace_app leaves no staging dir" "absent" \
  "$([ -e "$DST.new" ] && echo present || echo absent)"
assert_eq "replace_app leaves no backup dir" "absent" \
  "$([ -e "$DST.old" ] && echo present || echo absent)"

# 模拟拷贝失败：必须保留旧版本
FAILSRC="$TMPROOT/Fail.app"; mk_app "$FAILSRC" "3.0.0"
assert_eq "replace_app reports failure" \
  "rc=1" "$(YI_AGENT_COPY_CMD=false replace_app "$FAILSRC" "$DST" >/dev/null 2>&1; echo "rc=$?")"
assert_eq "replace_app keeps old version on failure" "2.0.0" "$(app_version "$DST")"
assert_eq "replace_app cleans staging on failure" "absent" \
  "$([ -e "$DST.new" ] && echo present || echo absent)"

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
