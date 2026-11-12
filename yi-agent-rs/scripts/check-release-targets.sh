#!/usr/bin/env bash
# 发布前门禁：三个发布目标都必须真的能编译出来。
#
# 为什么需要它：`release.yml` 只在 tag 上跑，而 `just ci` 只编 host 目标。
# 于是「某个 crate 用了只有 macOS 才有的 API、却没有 cfg 隔离」这类缺陷
# 在 PR 与日常门禁里完全不可见，一路绿到 tag，然后在发布时让三个平台的
# 打包一起失败（v0.1.14 实测：两个 darwin 目标各 1m17s 成功，linux 目标
# 因为 `libc::getpeereid` / `proc_bsdinfo` 找不到而编译失败，整条 release
# 在 `Build & package all platforms` 步退 1）。
#
# 为什么不是 `cargo check --target x86_64-unknown-linux-gnu`：本 workspace 有
# `libsqlite3-sys` 这类需要 C 交叉编译器的依赖，裸 `cargo check` 会在
# cc-rs 阶段以 "failed to find tool x86_64-linux-gnu-gcc" 失败——那证明的
# 是环境不齐，不是代码能不能过。`cargo zigbuild` 用 zig 作 C 交叉编译器，
# 与 release.yml 的打包路径**完全一致**，因此它给出的信号就是发布时的信号。
#
# 用法：
#   release-targets-check                 # 校验三个发布目标
#   release-targets-check --host-only     # 只校验 host 目标（本地快速自检）
#
# 退出码：0 = 全部可编译；1 = 有目标编译失败；2 = 环境缺 zig / cargo-zigbuild
# （环境缺失必须显式失败，跳过等于门禁假绿）。
set -uo pipefail

MANIFEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

HOST_ONLY=0
case "${1:-}" in
  "") ;;
  --host-only) HOST_ONLY=1 ;;
  *) echo "usage: $(basename "$0") [--host-only]" >&2; exit 2 ;;
esac

SOURCES_ALL="x86_64-apple-darwin aarch64-apple-darwin x86_64-unknown-linux-gnu"
if [ "$HOST_ONLY" = 1 ]; then
  host="$(rustc -vV | sed -n 's/^host: //p')"
  TARGETS="$host"
else
  TARGETS="$SOURCES_ALL"
fi

fail() { printf 'release-targets-check FAILED: %s\n' "$*" >&2; exit 1; }
# 环境缺失用 2，与「代码编译失败用 1」分开：调用方据此知道该修环境还是修代码。
missing_env() { printf 'release-targets-check ERROR: %s\n' "$*" >&2; exit 2; }

if [ "$HOST_ONLY" = 0 ]; then
  command -v zig >/dev/null 2>&1 \
    || missing_env "zig not found. Install with: brew install zig (the release runner requires it too)"
  command -v cargo-zigbuild >/dev/null 2>&1 \
    || missing_env "cargo-zigbuild not found. Install with: cargo install cargo-zigbuild --locked"
fi

cd "$MANIFEST_DIR" || fail "cannot cd to $MANIFEST_DIR"

status=0
for target in $TARGETS; do
  printf '== %s ==\n' "$target"
  # 编译命令必须与 `just build-target` 逐字一致（规则也一样：只有 linux 走
  # zigbuild），否则连环后果是实测过的：
  #   同样的 darwin 目标，`cargo zigbuild` 与 `cargo build` 的链接器配置不同，
  #   指纹不共享，于是这里会把另两个 darwin 目标完整重编一遍。v0.1.15 的
  #   release 里第 10 步因此从 2m36s 涨到 8m13s，并且污染了下一步要复用的
  #   target 缓存。命令对齐后，紧跟 package-all 跑几乎是白拿。
  #
  # `cargo zigbuild --check` 不存在（该子命令不接受 --check），裸
  # `cargo check --target linux` 又会卡在 cc-rs 找 C 交叉编译器上——那证明的
  # 是环境不齐而非代码能否编译，所以这里只能真编。
  if echo "$target" | grep -q linux; then
    build=(cargo zigbuild --release --target "$target")
  else
    build=(cargo build --release --target "$target")
  fi
  if ! "${build[@]}" 2>&1 | tail -25; then
    printf 'release-targets-check: %s does not compile\n' "$target" >&2
    status=1
  fi
done

if [ "$status" -ne 0 ]; then
  fail "at least one release target does not compile (see the compiler output above)"
fi

printf 'release-targets-check ok: %s\n' "$TARGETS"
