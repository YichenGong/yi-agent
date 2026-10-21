#!/usr/bin/env bash
# 自测 desktop/scripts 下的脚本不得硬编码 target 目录。
#
# 背景：v0.1.10（run 39633766015）在 `Build macOS desktop app for npm`
# 编译成功（1m05s）之后失败于
#   cp: .../yi-agent-rs/target/release/yi-agent: No such file or directory
# workflow 为复用缓存把 CARGO_TARGET_DIR 指到 $HOME/cargo-target/github，
# 而 build-sidecar.sh 写死 $ROOT/yi-agent-rs/target。同一个洞在
# package-npm-app.sh 的 APP_SRC 上也存在（tauri 产物同样跟着
# CARGO_TARGET_DIR 走），只是没被这次失败暴露出来。
#
# 这类失败最难查：编译全绿、报错却长得像「文件丢了」。所以钉死这条
# 不变量：脚本里凡是拼 target 路径的，必须经 CARGO_TARGET_DIR 或
# cargo metadata 解析。
set -uo pipefail

SCRIPTS_DIR="$(cd "$(dirname "$0")" && pwd)"
pass=0
failn=0
ok() { echo "ok   - $1"; pass=$((pass + 1)); }
bad() { echo "FAIL - $1"; failn=$((failn + 1)); }

# 只看非注释行：注释里解释历史写法时会引用旧路径，那不是配置。
scan() {
  local file="$1"
  grep -vE '^[[:space:]]*#' "$SCRIPTS_DIR/$file" 2>/dev/null
}

# 1) build-sidecar.sh 必须用 cargo metadata 或 CARGO_TARGET_DIR 解析路径。
if scan build-sidecar.sh | grep -qE 'cargo metadata|CARGO_TARGET_DIR'; then
  ok "build-sidecar.sh resolves the target dir dynamically"
else
  bad "build-sidecar.sh must resolve target dir via cargo metadata or CARGO_TARGET_DIR"
fi

# 2) package-npm-app.sh 同上。
if scan package-npm-app.sh | grep -qE 'cargo metadata|CARGO_TARGET_DIR'; then
  ok "package-npm-app.sh resolves the target dir dynamically"
else
  bad "package-npm-app.sh must resolve target dir via cargo metadata or CARGO_TARGET_DIR"
fi

# 3) 任何脚本都不得把 target 路径写死成字面量。
hardcoded=""
for f in "$SCRIPTS_DIR"/*.sh; do
  case "$(basename "$f")" in
    test_scripts_target_dir.sh) continue ;;  # 本文件自身含有这些字面量
  esac
  if grep -vE '^[[:space:]]*#' "$f" | grep -qE '"[^"]*yi-agent-rs/target/(debug|release)|src-tauri/target/release'; then
    hardcoded="$hardcoded
       $(basename "$f") hardcodes a target path"
  fi
done
if [ -z "$hardcoded" ]; then
  ok "no script hardcodes a target directory"
else
  bad "scripts must not hardcode target paths (use CARGO_TARGET_DIR / cargo metadata)"
  printf '%s\n' "$hardcoded"
fi

# 4) 兜底逻辑必须在：解析失败时退回默认路径，而不是得到空串拼出 "/release/..."。
if scan build-sidecar.sh | grep -qE 'TARGET_DIR=.*target'; then
  ok "build-sidecar.sh falls back to the default target dir"
else
  bad "build-sidecar.sh must fall back when cargo metadata yields nothing"
fi

echo
if [ "$failn" -eq 0 ]; then
  echo "$pass passed"
  exit 0
fi
echo "$pass passed, $failn failed"
exit 1
