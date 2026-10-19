#!/usr/bin/env bash
# 自测 scripts/sync-npm-versions.sh：确认发布时主包版本与平台子包版本一起同步。
#
# 背景：`just npm-pack` 过去只 sed `"version"` 字段，主包的
# `optionalDependencies` 里仍写着 0.1.0，于是 `npm i @yi-agent/yi-agent@0.1.3`
# 拉到的其实是 0.1.0 的占位二进制（实测量 430832 字节、输出
# `yi-agent agent CLI (placeholder)`）。这个测试把「依赖必须跟着版本走」钉死。
#
# 不用 `set -e`：被测脚本的「非零退出」是期望结果之一，不能被 -e 当成致命错误。
set -uo pipefail

SCRIPT="$(cd "$(dirname "$0")" && pwd)/sync-npm-versions.sh"
[ -x "$SCRIPT" ] || { echo "FAIL: $SCRIPT not executable"; exit 1; }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

pass=0
failn=0
check() { # check <name> <expected_exit> <actual_exit>
  if [ "$2" = "$3" ]; then echo "ok   - $1"; pass=$((pass+1));
  else echo "FAIL - $1 (expected exit $2, got $3)"; failn=$((failn+1)); fi
}
check_contains() { # check_contains <name> <file> <expected-line>
  if grep -qF -- "$3" "$2"; then echo "ok   - $1"; pass=$((pass+1));
  else echo "FAIL - $1 (missing: $3 in $2)"; failn=$((failn+1)); fi
}
check_absent() { # check_absent <name> <file> <unexpected>
  if grep -qF -- "$3" "$2"; then echo "FAIL - $1 (still present: $3)"; failn=$((failn+1));
  else echo "ok   - $1"; pass=$((pass+1)); fi
}

# fixture <dir>: 复刻发布会用到的包结构，版本全部停在 0.1.0。
fixture() {
  local dir="$1"
  mkdir -p "$dir"/platforms/{darwin-x64,darwin-arm64,linux-x64}
  cat > "$dir/package.json" <<'JSON'
{
  "name": "@yi-agent/yi-agent",
  "version": "0.1.0",
  "description": "A coding agent CLI",
  "license": "MIT",
  "engines": {
    "node": ">=14"
  },
  "bin": {
    "yi-agent": "entry.js"
  },
  "optionalDependencies": {
    "@yi-agent/yi-agent-darwin-x64": "0.1.0",
    "@yi-agent/yi-agent-darwin-arm64": "0.1.0",
    "@yi-agent/yi-agent-linux-x64": "0.1.0"
  },
  "files": ["entry.js"]
}
JSON
  for d in darwin-x64 darwin-arm64 linux-x64; do
    cat > "$dir/platforms/$d/package.json" <<JSON
{
  "name": "@yi-agent/yi-agent-$d",
  "version": "0.1.0",
  "description": "yi-agent binary for $d",
  "license": "MIT",
  "files": ["binaries/"]
}
JSON
  done
}

# --- case 1: 主包与子包版本一起升到 0.1.7
fixture "$work/case1"
bash "$SCRIPT" 0.1.7 "$work/case1" >/dev/null 2>&1
check "sync succeeds" 0 $?
check_contains "main version bumped" "$work/case1/package.json" '"version": "0.1.7"'
check_contains "darwin-x64 dep bumped" "$work/case1/package.json" '"@yi-agent/yi-agent-darwin-x64": "0.1.7"'
check_contains "darwin-arm64 dep bumped" "$work/case1/package.json" '"@yi-agent/yi-agent-darwin-arm64": "0.1.7"'
check_contains "linux-x64 dep bumped" "$work/case1/package.json" '"@yi-agent/yi-agent-linux-x64": "0.1.7"'
check_absent "old 0.1.0 dep gone" "$work/case1/package.json" '": "0.1.0"'
for d in darwin-x64 darwin-arm64 linux-x64; do
  check_contains "subpackage $d bumped" "$work/case1/platforms/$d/package.json" '"version": "0.1.7"'
done

# --- case 2: 无关字段一个字都不许动（name/os/cpu/bin/files 是分发契约）
fixture "$work/case2"
bash "$SCRIPT" 0.1.7 "$work/case2" >/dev/null 2>&1
check_contains "name preserved" "$work/case2/package.json" '"name": "@yi-agent/yi-agent"'
check_contains "bin preserved" "$work/case2/package.json" '"yi-agent": "entry.js"'
check_contains "files preserved" "$work/case2/package.json" '"files": ["entry.js"]'
check_contains "engines preserved" "$work/case2/package.json" '"node": ">=14"'

# --- case 3: 幂等（同版本再跑一次仍成功且结果不变）
fixture "$work/case3"
bash "$SCRIPT" 0.1.7 "$work/case3" >/dev/null 2>&1
bash "$SCRIPT" 0.1.7 "$work/case3" >/dev/null 2>&1
check "re-sync is idempotent" 0 $?
check_contains "still one version line" "$work/case3/package.json" '"version": "0.1.7"'
check "exactly one version field" 1 "$(grep -c '"version"' "$work/case3/package.json")"

# --- case 4: optionalDependencies 缺了 platforms 目录里存在的平台 → 必须失败
#     这正是本次事故的形态：依赖表与平台子包脱节，发出去的包装不到真二进制。
fixture "$work/case4"
python_missing_ok=1
sed -i.bak '/yi-agent-linux-x64/d' "$work/case4/package.json" && rm "$work/case4/package.json.bak"
bash "$SCRIPT" 0.1.7 "$work/case4" >/dev/null 2>&1
check "missing optional dependency entry fails" 1 $?
if [ "$python_missing_ok" = 1 ] && ! grep -q 'yi-agent-linux-x64' "$work/case4/package.json"; then
  echo "ok   - fixture really lacks linux entry"; pass=$((pass+1))
fi

# --- case 5: 平台子包版本与目录对不上（多出来的依赖项）→ 必须失败
fixture "$work/case5"
mkdir -p "$work/case5/platforms/win32-x64"
cp "$work/case5/platforms/linux-x64/package.json" "$work/case5/platforms/win32-x64/package.json"
sed -i.bak 's/yi-agent-linux-x64/yi-agent-win32-x64/' "$work/case5/platforms/win32-x64/package.json"
rm "$work/case5/platforms/win32-x64/package.json.bak"
bash "$SCRIPT" 0.1.7 "$work/case5" >/dev/null 2>&1
check "platform dir without optional dependency fails" 1 $?

# --- case 6: 非法版本号 → 用法错误（不让坏版本污染包）
fixture "$work/case6"
bash "$SCRIPT" "not-a-version" "$work/case6" >/dev/null 2>&1
check "invalid version rejected" 2 $?
check_contains "package left untouched" "$work/case6/package.json" '"version": "0.1.0"'

# --- case 7: 目标目录不存在 → 必须失败而不是静默跳过
bash "$SCRIPT" 0.1.7 "$work/does-not-exist" >/dev/null 2>&1
check "missing package dir fails" 1 $?

echo
if [ "$failn" -ne 0 ]; then echo "$failn failed, $pass passed"; exit 1; fi
echo "$pass passed"
