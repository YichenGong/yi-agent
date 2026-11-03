#!/usr/bin/env bash
# 自测 desktop/scripts/sync-ios-version.sh：iOS 版本必须跟随 workspace 版本。
#
# 背景：iOS 的版本号硬编码在 tauri.conf.json / project.yml / Info.plist 三处，
# 与主线版本没有任何联动，装到手机上的包自报 0.1.0 而工作区早已 0.1.13。
# 这个测试把两件事钉死：
#   A. 注入脚本本身的行为（写入正确、幂等、只动目标键、非法版本拒绝）
#   B. 仓库快照必须等于主线版本（bump 忘了同步 iOS 时，just ci 在这里变红）
#
# 不用 `set -e`：被测脚本的「非零退出」是期望结果之一，不能被 -e 当成致命错误。
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
SCRIPT="$HERE/sync-ios-version.sh"
[ -x "$SCRIPT" ] || { echo "FAIL: $SCRIPT not executable"; exit 1; }

pass=0
failn=0
ok()   { echo "ok   - $1"; pass=$((pass+1)); }
bad()  { echo "FAIL - $1"; failn=$((failn+1)); }
check() { # check <name> <expected_exit> <actual_exit>
  if [ "$2" = "$3" ]; then ok "$1"; else bad "$1 (expected exit $2, got $3)"; fi
}
check_eq() { # check_eq <name> <expected> <actual>
  if [ "$2" = "$3" ]; then ok "$1"; else bad "$1 (expected '$2', got '$3')"; fi
}
check_contains() { # check_contains <name> <file> <needle>
  if grep -qF -- "$3" "$2" 2>/dev/null; then ok "$1"; else bad "$1 (missing: $3)"; fi
}
check_absent() { # check_absent <name> <file> <needle>
  if grep -qF -- "$3" "$2" 2>/dev/null; then bad "$1 (still present: $3)"; else ok "$1"; fi
}

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# --- fixture：复刻脚本依赖的最小仓库结构 --------------------------------
# 版本停在 0.1.0（三处全是 Tauri/模板默认值），Cargo 侧给 9.9.9。
fixture() { # fixture <dir> [cargo_version]
  # 用 ${2-DEFAULT} 而不是 ${2:-DEFAULT}：后者的「空串」会回落到默认值，
  # 于是「Cargo 里 version = ""」这个用例根本测不到它想测的东西。
  local dir="$1" ver="${2-9.9.9}"
  mkdir -p "$dir/desktop/scripts" "$dir/desktop/src-tauri/gen/apple/desktop_iOS" "$dir/yi-agent-rs"
  cp "$SCRIPT" "$dir/desktop/scripts/sync-ios-version.sh"
  chmod +x "$dir/desktop/scripts/sync-ios-version.sh"

  cat > "$dir/yi-agent-rs/Cargo.toml" <<TOML
[workspace]
members = []

[workspace.package]
version = "$ver"
edition = "2024"
TOML

  cat > "$dir/desktop/src-tauri/tauri.conf.json" <<'JSON'
{
  "$schema": "https://schema.tauri.app/config/2",
  "productName": "yi-agent",
  "version": "0.1.0",
  "identifier": "com.gongyichen.desktop",
  "bundle": {
    "targets": "all",
    "icon": ["icons/32x32.png", "icons/icon.icns"]
  }
}
JSON

  cat > "$dir/desktop/src-tauri/gen/apple/project.yml" <<'YML'
name: desktop
settingGroups:
  app:
    base:
      PRODUCT_NAME: yi-agent
targets:
  desktop_iOS:
    type: application
    info:
      path: desktop_iOS/Info.plist
      properties:
        LSRequiresIPhoneOS: true
        CFBundleShortVersionString: 0.1.0
        CFBundleVersion: "0.1.0"
        CFBundleDisplayName: Yi-Agent
YML

  cat > "$dir/desktop/src-tauri/gen/apple/desktop_iOS/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleExecutable</key>
	<string>$(EXECUTABLE_NAME)</string>
	<key>CFBundleDisplayName</key>
	<string>Yi-Agent</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>0.1.0</string>
	<key>CFBundleVersion</key>
	<string>0.1.0</string>
	<key>LSRequiresIPhoneOS</key>
	<true/>
	<key>NSCameraUsageDescription</key>
	<string>用于扫描桌面端显示的配对二维码</string>
</dict>
</plist>
PLIST
}

echo "=== A. 注入脚本行为 ==="

f="$work/a"
fixture "$f"
s="$f/desktop/scripts/sync-ios-version.sh"
info="$f/desktop/src-tauri/gen/apple/desktop_iOS/Info.plist"
yml="$f/desktop/src-tauri/gen/apple/project.yml"
conf="$f/desktop/src-tauri/tauri.conf.json"

# 1. 注入后三处都对
"$s" >/dev/null 2>&1
check "1. inject exits 0" 0 "$?"
check_contains "1a. tauri.conf.json version"            "$conf" '"version": "9.9.9"'
check_contains "1b. project.yml ShortVersionString"     "$yml"  'CFBundleShortVersionString: 9.9.9'
check_contains "1c. project.yml CFBundleVersion"        "$yml"  'CFBundleVersion: "9.9.9"'
check_contains "1d. Info.plist ShortVersionString"      "$info" '<string>9.9.9</string>'

# 1e. Info.plist 两处版本都写成 9.9.9（不是只有第一处）
got="$(grep -c '<string>9.9.9</string>' "$info")"
check_eq "1e. Info.plist has both version strings bumped" "2" "$got"

# 2. 幂等：再跑一次，字节完全不变
before="$(shasum -a 256 "$info" "$yml" "$conf" | awk '{print $1}' | tr '\n' ' ')"
"$s" >/dev/null 2>&1
after="$(shasum -a 256 "$info" "$yml" "$conf" | awk '{print $1}' | tr '\n' ' ')"
check_eq "2. idempotent (second run changes nothing)" "$before" "$after"

# 3. 只动目标键：同文件里其它内容保持原样
check_contains "3a. keeps CFBundleDisplayName"          "$info" '<string>Yi-Agent</string>'
check_contains "3b. keeps NSCameraUsageDescription"     "$info" '用于扫描桌面端显示的配对二维码'
check_contains "3c. keeps tauri bundle.icon"            "$conf" '"icons/icon.icns"'
check_contains "3d. keeps productName"                  "$conf" '"productName": "yi-agent"'
check_contains "3e. keeps PRODUCT_NAME"                 "$yml"  'PRODUCT_NAME: yi-agent'

# 3f. Info.plist 仍是合法 plist
if command -v plutil >/dev/null 2>&1; then
  plutil -lint "$info" >/dev/null 2>&1
  check "3f. Info.plist still lints as a valid plist" 0 "$?"
else
  ok "3f. skipped (no plutil)"
fi

# 4. --check 在一致时 exit 0
"$s" --check >/dev/null 2>&1
check "4. --check exits 0 when in sync" 0 "$?"

# 5. --check 检出漂移：exit 1，且输出里同时有期望值与实际值
printf '%s\n' "$(sed 's/<string>9.9.9<\/string>/<string>0.1.0<\/string>/' "$info")" > "$info"
out="$("$s" --check 2>&1)"; rc=$?
check "5. --check exits 1 when drifted" 1 "$rc"
case "$out" in
  *9.9.9*0.1.0*|*0.1.0*9.9.9*) ok "5a. drift message names expected and actual" ;;
  *) bad "5a. drift message names expected and actual (got: $out)" ;;
esac

# 6. 非法版本（prerelease / 非语义化）exit 2，且不写坏任何文件
for badver in "0.1.13-rc1" "abc" ""; do
  g="$work/bad-$RANDOM"
  fixture "$g" "$badver"
  bs="$g/desktop/scripts/sync-ios-version.sh"
  binfo="$g/desktop/src-tauri/gen/apple/desktop_iOS/Info.plist"
  byml="$g/desktop/src-tauri/gen/apple/project.yml"
  bb="$(shasum -a 256 "$binfo" "$byml" | awk '{print $1}' | tr '\n' ' ')"
  "$bs" >/dev/null 2>&1
  rc=$?
  ba="$(shasum -a 256 "$binfo" "$byml" | awk '{print $1}' | tr '\n' ' ')"
  check "6. invalid version '$badver' exits 2" 2 "$rc"
  check_eq "6a. invalid version '$badver' writes nothing" "$bb" "$ba"
done

# 7. 缺 Cargo.toml 时 exit 2
h="$work/nocargo"
fixture "$h"
rm -f "$h/yi-agent-rs/Cargo.toml"
"$h/desktop/scripts/sync-ios-version.sh" >/dev/null 2>&1
check "7. missing Cargo.toml exits 2" 2 "$?"

# 8. 未知参数 exit 2
"$s" --nope >/dev/null 2>&1
check "8. unknown flag exits 2" 2 "$?"

echo ""
echo "=== B. 仓库快照必须等于主线版本 ==="
# 这条是 CI 的拦网点：bump 了 workspace 版本却忘了同步 iOS 三处时，这里变红。
# 故意跑真实仓库（而不是 fixture）——门禁要守的是提交进仓库的那份值。
cargo_ver="$(grep '^version' "$ROOT/yi-agent-rs/Cargo.toml" | head -1 | awk -F'"' '{print $2}')"
out="$(bash "$SCRIPT" --check 2>&1)"; rc=$?
if [ "$rc" = 0 ]; then
  ok "9. repo snapshot matches workspace version ($cargo_ver)"
else
  bad "9. repo snapshot does not match workspace version ($cargo_ver):"
  printf '%s\n' "$out" | sed 's/^/       /' >&2
fi

echo ""
echo "-----------------------------------------"
if [ "$failn" = 0 ]; then
  echo "PASS: $pass checks"
  exit 0
fi
echo "FAIL: $failn failed, $pass passed"
exit 1
