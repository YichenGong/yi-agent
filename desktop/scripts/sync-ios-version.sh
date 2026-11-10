#!/usr/bin/env bash
# 让 iOS App 的版本号跟随 yi-agent-rs 的 workspace 版本。
#
# 为什么需要它：iOS 的版本号硬编码在三处，与主线版本没有任何联动——
#   desktop/src-tauri/gen/apple/project.yml        （iOS 工程的生成源）
#   desktop/src-tauri/gen/apple/desktop_iOS/Info.plist （实际打包用）
#   desktop/src-tauri/tauri.conf.json              （Tauri 配置）
# 三者都停在 Tauri 模板默认的 0.1.0，而 workspace 早已 0.1.13，于是装到手机上
# 的包自报一个与主线无关的版本。macOS 的 .app 有 package-npm-app.sh 注入版本，
# iOS 这条链从来没有对应步骤。
#
# 为什么 project.yml 与 Info.plist 两处都写：前者是 `tauri ios init` 的生成源，
# 只改产物的话下一次 init 就被重生成抹掉（CFBundleDisplayName 已踩过这个坑，
# 见 docs/superpowers/specs/2026-10-11-ios-app-display-name-design.md）。
#
# 用法：
#   desktop/scripts/sync-ios-version.sh            # 从 Cargo.toml 读版本并写入三处
#   desktop/scripts/sync-ios-version.sh --check    # 只比对不改；一致 exit 0
#
# 退出码：0 = 已同步/已一致；1 = --check 检出漂移；2 = 用法错误、读不到版本、或
# 版本不是 X.Y.Z（CFBundleShortVersionString 必须恰好三段整数，Tauri 遇到
# prerelease 会**静默**截断，产物与主线就此分叉而无人察觉——宁可显式失败）。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DESKTOP="$(cd "$SCRIPT_DIR/.." && pwd)"
ROOT="$(cd "$DESKTOP/.." && pwd)"

CARGO_TOML="$ROOT/yi-agent-rs/Cargo.toml"
TAURI_CONF="$DESKTOP/src-tauri/tauri.conf.json"
PROJECT_YML="$DESKTOP/src-tauri/gen/apple/project.yml"
INFO_PLIST="$DESKTOP/src-tauri/gen/apple/desktop_iOS/Info.plist"

CHECK=0
case "${1:-}" in
  "") ;;
  --check) CHECK=1 ;;
  -h|--help) sed -n '2,30p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
  *) echo "ERROR: 未知参数 '$1'（可用: --check | --help）" >&2; exit 2 ;;
esac

die() { printf 'ERROR: %s\n' "$*" >&2; exit 2; }

# --- 1. 真源：workspace 版本 ---------------------------------------------
# 与 justfile:169 / package-npm-app.sh:34 / release.yml:62 同一读法，
# 不引入第二套解析。
VERSION="$(grep '^version' "$CARGO_TOML" 2>/dev/null | head -1 | awk -F'"' '{print $2}')"
[ -n "$VERSION" ] || die "读不到 $CARGO_TOML 的 version"

# CFBundleShortVersionString 必须恰好三段整数。带 prerelease 时 Tauri 会静默截断
# （CLI 里的原话："CFBundleShortVersionString cannot have prerelease tag; stripping from"），
# 于是「装上去的版本」与「仓库里的版本」悄悄不同。这里直接拒绝。
printf '%s' "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' \
  || die "版本 '$VERSION' 不是 X.Y.Z 三段整数，iOS 的 CFBundleShortVersionString 不接受 prerelease"

for f in "$TAURI_CONF" "$PROJECT_YML" "$INFO_PLIST"; do
  [ -f "$f" ] || die "缺文件: $f"
done

# --- 2. 读取三处的当前值 --------------------------------------------------
conf_version() { # tauri.conf.json 的 .version
  sed -n 's/^[[:space:]]*"version":[[:space:]]*"\([^"]*\)".*/\1/p' "$TAURI_CONF" | head -1
}

yml_short() { # project.yml 的 CFBundleShortVersionString（容忍带/不带引号）
  sed -n 's/^[[:space:]]*CFBundleShortVersionString:[[:space:]]*"\{0,1\}\([^"]*\)"\{0,1\}[[:space:]]*$/\1/p' "$PROJECT_YML" | head -1
}

yml_build() { # project.yml 的 CFBundleVersion
  sed -n 's/^[[:space:]]*CFBundleVersion:[[:space:]]*"\{0,1\}\([^"]*\)"\{0,1\}[[:space:]]*$/\1/p' "$PROJECT_YML" | head -1
}

plist_both() { # 两键同值，故只取 ShortVersionString（CFBundleVersion 由同一函数保障）
  # PlistBuddy 读不到时返回非零并打印 "Does Not Exist"，用 || true 兜住。
  /usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$INFO_PLIST" 2>/dev/null || true
}

plist_build_version() {
  /usr/libexec/PlistBuddy -c 'Print :CFBundleVersion' "$INFO_PLIST" 2>/dev/null || true
}

# --- 3. --check：逐处比对 -------------------------------------------------
if [ "$CHECK" = 1 ]; then
  drift=0
  report() { # report <位置> <实际值>
    if [ "$2" = "$VERSION" ]; then
      printf 'ok   - %s = %s\n' "$1" "$2"
    else
      printf 'FAIL - %s: 期望 %s，实际 %s\n' "$1" "$VERSION" "${2:-<缺失>}" >&2
      drift=1
    fi
  }
  report "tauri.conf.json .version"                    "$(conf_version)"
  report "project.yml CFBundleShortVersionString"      "$(yml_short)"
  report "project.yml CFBundleVersion"                 "$(yml_build)"
  report "Info.plist CFBundleShortVersionString"       "$(plist_both)"
  report "Info.plist CFBundleVersion"                  "$(plist_build_version)"

  if [ "$drift" != 0 ]; then
    echo "" >&2
    echo "修复：bash desktop/scripts/sync-ios-version.sh" >&2
    exit 1
  fi
  echo "iOS 版本已跟随 workspace 版本: $VERSION"
  exit 0
fi

# --- 4. 写入 -------------------------------------------------------------
# tauri.conf.json：只替换 .version 的值，其余字节不动。
sed -i.bak -E "s|^([[:space:]]*\"version\":[[:space:]]*)\"[^\"]*\"|\1\"$VERSION\"|" "$TAURI_CONF"
rm -f "$TAURI_CONF.bak"

# project.yml：两个键都写，且**保留原有的引号风格**（本文件里
# CFBundleShortVersionString 不带引号、CFBundleVersion 带引号；统一引号风格会
# 产生与版本无关的 diff）。正则只吃掉值，引号原样留着。
sed -i.bak -E "s|^([[:space:]]*CFBundleShortVersionString:[[:space:]]*\"?)[^\"]*(\"?[[:space:]]*)\$|\1$VERSION\2|" "$PROJECT_YML"
rm -f "$PROJECT_YML.bak"
sed -i.bak -E "s|^([[:space:]]*CFBundleVersion:[[:space:]]*\"?)[^\"]*(\"?[[:space:]]*)\$|\1$VERSION\2|" "$PROJECT_YML"
rm -f "$PROJECT_YML.bak"

# Info.plist：只在目标键之后的第一个 <string> 上定点替换，其余字节不动。
#
# 不能用 PlistBuddy 的 Set：它会重写整个文件——键顺序变了（实测
# CFBundleDisplayName 与 NSCameraUsageDescription 被挪到别处）、文件末尾多出换行。
# 值虽然对了，但产生一整片与版本无关的 diff，review 时看不出真正改了什么。
# 也不能用普通正则跨行匹配：XML 里 <key> 与 <string> 分行，"最短匹配"很容易
# 打滑到下一个键。这里改用 awk 维护「上一行是哪把 key」的状态。
# 记下原文件是否以换行结尾：awk 会对每一行补换行，末尾会凭空多出一个
# "\ No newline at end of file" 的 diff——与版本无关，不该出现在提交里。
ends_with_newline() { [ -s "$1" ] && [ "$(tail -c 1 "$1" | od -An -c | tr -d ' ')" = "\\n" ]; }
had_trailing_newline=0
ends_with_newline "$INFO_PLIST" && had_trailing_newline=1

for key in CFBundleShortVersionString CFBundleVersion; do
  tmp="$(mktemp)"
  awk -v key="$key" -v ver="$VERSION" '
    # 命中目标 <key> 时置标志，下一行的第一个 <string> 就是它的值。
    index($0, "<key>" key "</key>") { armed = 1; print; next }
    armed && /<string>[^<]*<\/string>/ {
      sub(/<string>[^<]*<\/string>/, "<string>" ver "<\/string>")
      armed = 0
      print
      next
    }
    { print }
  ' "$INFO_PLIST" > "$tmp"
  mv "$tmp" "$INFO_PLIST"
done
# 还原末尾换行特征（原文件没有就删掉 awk 补的那个）。
if [ "$had_trailing_newline" = 0 ]; then
  printf '%s' "$(cat "$INFO_PLIST")" > "$INFO_PLIST.tmp" && mv "$INFO_PLIST.tmp" "$INFO_PLIST"
fi
# 产物仍是合法 plist 才算数（静止文件被写坏时，报错要在本地出而不是在 Xcode 里）。
plutil -lint "$INFO_PLIST" >/dev/null || die "Info.plist 被写坏了（plutil -lint 失败）"

# --- 5. 回读校验：写完必须真的是这个值，否则静默失败 ---------------------------------
fail=0
[ "$(conf_version)"        = "$VERSION" ] || { echo "ERROR: tauri.conf.json 未写成 $VERSION" >&2; fail=1; }
[ "$(yml_short)"           = "$VERSION" ] || { echo "ERROR: project.yml CFBundleShortVersionString 未写成 $VERSION" >&2; fail=1; }
[ "$(yml_build)"           = "$VERSION" ] || { echo "ERROR: project.yml CFBundleVersion 未写成 $VERSION" >&2; fail=1; }
[ "$(plist_both)"          = "$VERSION" ] || { echo "ERROR: Info.plist CFBundleShortVersionString 未写成 $VERSION" >&2; fail=1; }
[ "$(plist_build_version)" = "$VERSION" ] || { echo "ERROR: Info.plist CFBundleVersion 未写成 $VERSION" >&2; fail=1; }
if [ "$fail" != 0 ]; then
  die "写入后回读不一致，拒绝以「成功」退出"
fi

echo "iOS 版本已同步到 ${VERSION}：tauri.conf.json / project.yml / Info.plist"
