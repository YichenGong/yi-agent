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

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
