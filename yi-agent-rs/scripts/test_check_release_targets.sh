#!/usr/bin/env bash
# check-release-targets.sh 的行为测试。
#
# 这个门禁的判据必须钉死，因为它们决定了它是不是假绿：
#   1. 环境不齐（缺 zig / 缺 cargo-zigbuild）必须**退出 2 而不是跳过**。
#      「跳过」等于门禁假绿，而假绿会让带缺陷的代码一路走到发布。
#   2. 编译失败必须是退出 1，且要点名是哪个 target。
#   3. 全部可编译是退出 0。
#
# 不跑真实三平台构建：那是 `just release-targets-check` 本身要做的事。
# 这里用假 cargo 驱动，毫秒级。真实仓库的正向验证在 CI 的 macos job 上做
# （那里才有 zig 与 cargo-zigbuild）。
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$HERE/check-release-targets.sh"

# 绝对路径解析一次，后面用受限 PATH 调用时不必再依赖 PATH。
BASH_ABS="$(command -v bash)"
# zig 的绝对路径：`command -v` 在没有导出 PATH 时会落空，而这条断言是
# 「环境缺失必须退 2」的防护，不能因为 shell 环境不同就被跳过。
for candidate in "$(command -v zig 2>/dev/null)" \
                 /opt/homebrew/bin/zig /usr/local/bin/zig /usr/bin/zig; do
  if [ -n "$candidate" ] && [ -x "$candidate" ]; then ZIG_ABS="$candidate"; break; fi
done

FAILED=0
n=0
ok() { n=$((n + 1)); printf 'ok   - %s\n' "$*"; }
bad() { FAILED=$((FAILED + 1)); printf 'FAIL - %s\n' "$*"; }
check() { # $1 描述, $2 期望 rc, $3 实际 rc
  if [ "$2" = "$3" ]; then ok "$1"; else bad "$1 (expected rc=$2, got rc=$3)"; fi
}

[ -x "$SCRIPT" ] || { printf 'FAIL - %s is not executable\n' "$SCRIPT"; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# 用法错误 → 2
"$SCRIPT" --nope >/dev/null 2>&1
check "unknown flag exits 2" 2 "$?"

# 空 PATH：`command -v zig` 一定找不到。用绝对路径的 bash 运行，避免
# macOS 在 PATH 里找不到 shell 时把进程 SIGKILL（实测 137）。
empty="$WORK/empty-path"
mkdir -p "$empty"

out="$(PATH="$empty" "$BASH_ABS" "$SCRIPT" 2>&1)"; rc=$?
check "missing zig exits 2" 2 "$rc"
case "$out" in
  *"brew install zig"*) ok "missing-zig message says how to install it" ;;
  *) bad "missing-zig message must say how to install it; got: $out" ;;
esac

# 有 zig 但缺 cargo-zigbuild → 2
zigdir="$WORK/zig-only"
mkdir -p "$zigdir"
if [ -n "$ZIG_ABS" ]; then
  ln -sf "$ZIG_ABS" "$zigdir/zig"
  out="$(PATH="$zigdir" "$BASH_ABS" "$SCRIPT" 2>&1)"; rc=$?
  check "missing cargo-zigbuild exits 2" 2 "$rc"
  case "$out" in
    *"cargo install cargo-zigbuild"*) ok "missing-zigbuild message says how to install it" ;;
    *) bad "missing-zigbuild message must say how to install it; got: $out" ;;
  esac
else
  # 本机没有 zig 时这两条无法构造（PATH 隔离下 zig 也找不到）。
  printf 'skip - missing-zigbuild case (no zig on this machine)\n'
fi

# 假工具集：zig 通过校验，cargo 对 linux 目标报编译错误。
fake="$WORK/fake"
mkdir -p "$fake/bin"
cat > "$fake/bin/zig" <<'EOF'
#!/bin/sh
echo "zig (fake)"
EOF
cat > "$fake/bin/rustc" <<'EOF'
#!/bin/sh
if [ "$1" = "-vV" ]; then echo "host: aarch64-apple-darwin"; fi
exit 0
EOF
# 脚本调 `cargo zigbuild ... --target <triple>`；cargo-zigbuild 是 cargo 的
# 子命令，但门禁用 `command -v cargo-zigbuild` 做环境校验，所以两者都要有。
cat > "$fake/bin/cargo-zigbuild" <<'EOF'
#!/bin/sh
exit 0
EOF
cat > "$fake/bin/cargo" <<'EOF'
#!/bin/sh
for arg in "$@"; do
  case "$arg" in
    *unknown-linux-gnu*) echo "error[E0425]: cannot find function in crate libc" >&2; exit 101 ;;
  esac
done
echo "Finished release profile"
EOF
chmod +x "$fake/bin/zig" "$fake/bin/rustc" "$fake/bin/cargo" "$fake/bin/cargo-zigbuild"

out="$(PATH="$fake/bin:/usr/bin:/bin" "$BASH_ABS" "$SCRIPT" 2>&1)"; rc=$?
check "a target that does not compile exits 1" 1 "$rc"
case "$out" in
  *"x86_64-unknown-linux-gnu does not compile"*) ok "failure names the offending target" ;;
  *) bad "failure must name the offending target; got: $out" ;;
esac
case "$out" in
  *"at least one release target does not compile"*) ok "failure summary is explicit" ;;
  *) bad "failure summary must be explicit; got: $out" ;;
esac

# 全部成功 → 0，且打印三个 target
cat > "$fake/bin/cargo" <<'EOF'
#!/bin/sh
echo "Finished release profile"
EOF
chmod +x "$fake/bin/cargo"

out="$(PATH="$fake/bin:/usr/bin:/bin" "$BASH_ABS" "$SCRIPT" 2>&1)"; rc=$?
check "all targets compiling exits 0" 0 "$rc"
for t in x86_64-apple-darwin aarch64-apple-darwin x86_64-unknown-linux-gnu; do
  case "$out" in
    *"$t"*) ok "reports target $t" ;;
    *) bad "must report target $t; got: $out" ;;
  esac
done

# --host-only 不要求 zig/cargo-zigbuild（只编 host 目标），但仍会调 cargo
hostfake="$WORK/hostfake"
mkdir -p "$hostfake/bin"
cat > "$hostfake/bin/cargo" <<'EOF'
#!/bin/sh
echo "Finished release profile"
EOF
cat > "$hostfake/bin/rustc" <<'EOF'
#!/bin/sh
if [ "$1" = "-vV" ]; then echo "host: aarch64-apple-darwin"; fi
exit 0
EOF
chmod +x "$hostfake/bin/cargo" "$hostfake/bin/rustc"
out="$(PATH="$hostfake/bin:/usr/bin:/bin" "$BASH_ABS" "$SCRIPT" --host-only 2>&1)"; rc=$?
check "--host-only needs no zig" 0 "$rc"
case "$out" in
  *"aarch64-apple-darwin"*) ok "--host-only checks the host target" ;;
  *) bad "--host-only must check the host target; got: $out" ;;
esac

echo ""
if [ "$FAILED" -ne 0 ]; then
  printf '%d checks, %d failed\n' "$n" "$FAILED" >&2
  exit 1
fi
printf '%d passed\n' "$n"
