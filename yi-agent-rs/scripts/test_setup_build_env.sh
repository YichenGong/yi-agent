#!/usr/bin/env bash
# 自测 scripts/setup-build-env.sh。
#
# 背景（真机实测，见 docs/bug-list.md）：
#   Mac mini runner 的 /opt/homebrew/bin 里装着 Homebrew 的 rust 1.98.1，
#   而 release workflow 的 PATH 把 /opt/homebrew/bin 排在 $HOME/.cargo/bin 前。
#   于是 cargo 解析成 Homebrew 那份，交叉编译立刻报
#   `error[E0463]: can't find crate for 'core'`，0.5~3 秒即失败——
#   正是 v0.1.4~v0.1.7 每次 release 的失败形态。
#   另一个坑是 launchd 的 maxfiles=256，并行链接 Linux 目标时报
#   ProcessFdQuotaExceeded。
#
# 这个测试用桩目录复刻两种布局，确认脚本：
#   - 把 rustup 的 bin 排到最前（不被 Homebrew 抢占）
#   - Homebrew 抢占时校验失败（而不是静默用错工具链）
#   - 缺 std 时失败、缺 zig 时失败
#   - --print 只输出可 eval 的语句
set -uo pipefail

SCRIPT="$(cd "$(dirname "$0")" && pwd)/setup-build-env.sh"
[ -f "$SCRIPT" ] || { echo "FAIL: $SCRIPT not found"; exit 1; }

pass=0
failn=0
ok() { echo "ok   - $1"; pass=$((pass + 1)); }
bad() { echo "FAIL - $1"; failn=$((failn + 1)); }

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# 造一个假的 HOME：rustup shim + 工具链 std + zig/zigbuild
make_home() {
  local home="$1" with_std="${2:-yes}" with_zig="${3:-yes}"
  mkdir -p "$home/.cargo/bin" "$home/.local/bin"
  # rustup 本体与 shim（shim 是指向 rustup 的符号链接）。
  # 桩要能回答 `--print sysroot` 与 `--version`，否则脚本取不到 sysroot。
  local tc="$home/.rustup/toolchains/stable-aarch64-apple-darwin"
  cat > "$home/.cargo/bin/rustup" <<SH
#!/bin/sh
case "\$1" in
  --print)
    case "\$2" in
      sysroot) echo "$tc" ;;
      target-libdir) echo "$tc/lib/rustlib/\${4:-aarch64-apple-darwin}/lib" ;;
      *) exit 1 ;;
    esac
    ;;
  --version|-V) echo "rustc 1.99.1 (stub)" ;;
  *) exit 0 ;;
esac
SH
  chmod +x "$home/.cargo/bin/rustup"
  ln -sf rustup "$home/.cargo/bin/cargo"
  ln -sf rustup "$home/.cargo/bin/rustc"
  ln -sf rustup "$home/.cargo/bin/cargo-zigbuild"
  # 假的 sysroot 与三平台 std
  mkdir -p "$tc/bin"
  printf '#!/bin/sh\n' > "$tc/bin/rustc"
  chmod +x "$tc/bin/rustc"
  if [ "$with_std" = "yes" ]; then
    for t in aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu; do
      mkdir -p "$tc/lib/rustlib/$t/lib"
      : > "$tc/lib/rustlib/$t/lib/libcore-abc.rlib"
    done
  else
    # 只装 host 目标，模拟「Homebrew Rust 只认自己那份 std」的后果
    mkdir -p "$tc/lib/rustlib/aarch64-apple-darwin/lib"
    : > "$tc/lib/rustlib/aarch64-apple-darwin/lib/libcore-abc.rlib"
  fi
  if [ "$with_zig" = "yes" ]; then
    mkdir -p "$home/brew/bin"
    printf '#!/bin/sh\necho "0.16.0"\n' > "$home/brew/bin/zig"
    chmod +x "$home/brew/bin/zig"
  fi
}

# 造一个「Homebrew 的 Rust」：普通文件，不是 rustup shim
make_brew_rust() {
  local dir="$1"
  mkdir -p "$dir"
  printf '#!/bin/sh\necho "cargo 1.98.1 (Homebrew)"\n' > "$dir/cargo"
  printf '#!/bin/sh\necho "rustc 1.98.1 (Homebrew)"\n' > "$dir/rustc"
  chmod +x "$dir/cargo" "$dir/rustc"
}

# --- 案例 1：Homebrew 在前（真实故障布局）→ --check 必须报错 ---
H1="$work/h1"; make_home "$H1"; make_brew_rust "$work/brew1"
# 真实布局：Homebrew 的 bin 在 rustup 的 bin 之前
SHADOW_PATH="$work/brew1:$H1/.cargo/bin:$H1/.local/bin:$H1/brew/bin:/usr/bin:/bin"
out="$(HOME="$H1" PATH="$SHADOW_PATH" bash "$SCRIPT" --check 2>&1)"; rc=$?
if [ "$rc" -ne 0 ] && printf '%s' "$out" | grep -q "shadowing rustup"; then
  ok "rejects Homebrew's Rust shadowing rustup (the real v0.1.4+ failure)"
else
  bad "should reject Homebrew shadowing rustup (rc=$rc): $out"
fi

# --- 案例 2：同样的故障 PATH，--print 重排后应通过并可 eval ---
out="$(HOME="$H1" PATH="$SHADOW_PATH" bash "$SCRIPT" --print 2>&1)"; rc=$?
if [ "$rc" -eq 0 ] && printf '%s' "$out" | grep -q "^export PATH="; then
  ok "--print reorders and emits an eval-able export"
else
  bad "--print should succeed after reorder (rc=$rc): $out"
fi

reordered="$(HOME="$H1" PATH="$SHADOW_PATH" bash "$SCRIPT" --print 2>/dev/null | sed -n 's/^export PATH="\(.*\)"$/\1/p')"
first="${reordered%%:*}"
if [ "$first" = "$H1/.cargo/bin" ]; then
  ok "rustup bin is placed first"
else
  bad "rustup bin should be first, got '$first'"
fi

# 重排不能丢掉其它目录（zig 在 brew/bin 里，丢了就没法交叉编译 linux）
if printf '%s' "$reordered" | tr ':' '\n' | grep -qx "$H1/brew/bin"; then
  ok "reorder preserves other PATH entries"
else
  bad "reorder dropped the brew dir: $reordered"
fi

# 重排要幂等：把结果再喂回去不应重复叠加
reordered2="$(HOME="$H1" PATH="$reordered" bash "$SCRIPT" --print 2>/dev/null | sed -n 's/^export PATH="\(.*\)"$/\1/p')"
if [ "$reordered" = "$reordered2" ]; then
  ok "reorder is idempotent"
else
  bad "reorder is not idempotent: '$reordered' vs '$reordered2'"
fi

# --- 案例 3：缺跨平台 std → 必须失败并提示 rustup target add ---
H2="$work/h2"; make_home "$H2" no yes
GOOD_PATH="$H2/.cargo/bin:$H2/.local/bin:$H2/brew/bin:/usr/bin:/bin"
out="$(HOME="$H2" PATH="$GOOD_PATH" bash "$SCRIPT" --check 2>&1)"; rc=$?
if [ "$rc" -ne 0 ] && printf '%s' "$out" | grep -q "missing std for x86_64-apple-darwin"; then
  ok "reports missing cross-target std instead of building broken"
else
  bad "should report missing std (rc=$rc): $out"
fi
if printf '%s' "$out" | grep -q "rustup target add"; then
  ok "missing std message tells how to fix it"
else
  bad "missing std message should mention 'rustup target add'"
fi

# --- 案例 4：缺 zig → 必须失败（linux 目标走 zigbuild）---
H3="$work/h3"; make_home "$H3" yes no
GOOD_PATH3="$H3/.cargo/bin:$H3/.local/bin:$H3/brew/bin:/usr/bin:/bin"
out="$(HOME="$H3" PATH="$GOOD_PATH3" bash "$SCRIPT" --check 2>&1)"; rc=$?
if [ "$rc" -ne 0 ] && printf '%s' "$out" | grep -q "zig not found"; then
  ok "reports missing zig for the linux target"
else
  bad "should report missing zig (rc=$rc): $out"
fi

# --- 案例 5：环境完好 → --check 成功 ---
H4="$work/h4"; make_home "$H4"; make_brew_rust "$work/brew4"
GOOD_PATH4="$H4/.cargo/bin:$work/brew4:$H4/.local/bin:$H4/brew/bin:/usr/bin:/bin"
out="$(HOME="$H4" PATH="$GOOD_PATH4" bash "$SCRIPT" --check 2>&1)"; rc=$?
if [ "$rc" -eq 0 ]; then
  ok "accepts a healthy environment"
else
  bad "healthy environment should pass (rc=$rc): $out"
fi

# --- 案例 6：未知参数 → exit 2 ---
bash "$SCRIPT" --bogus >/dev/null 2>&1; rc=$?
if [ "$rc" -eq 2 ]; then
  ok "unknown option exits 2"
else
  bad "unknown option should exit 2, got $rc"
fi

echo
if [ "$failn" -eq 0 ]; then
  echo "$pass passed"
  exit 0
fi
echo "$pass passed, $failn failed"
exit 1
