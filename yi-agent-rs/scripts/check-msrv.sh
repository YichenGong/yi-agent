#!/usr/bin/env bash
# MSRV 门禁：用 Cargo.toml 声明的 rust-version 真正编译一遍。
#
# 用法: check-msrv.sh [manifest-dir]
#
# 为什么需要它：`rust-version` 只是一行声明，没有任何东西会去验证它。
# 2026-11 的实况是：workspace 声明 1.85，而真实下限已经是 1.89
# （lopdf 0.45 → aes 0.9.3），声明错了整整四个小版本却一直没红。任何
# 依赖升级只要顶高 MSRV，这行声明就会悄悄变成谎话。
#
# 为什么用工具链目录而不是 `cargo +<ver>`：本机和 CI runner 上 PATH 里
# 都可能排着 Homebrew 的 Rust（见 setup-build-env.sh 的整段教训），
# `cargo` 解析到哪一份不确定。直接指向 rustup 的 toolchain 目录才稳。
#
# 工具链缺失时**报错退出**而不是跳过：跳过等于门禁假绿，比没有门禁更糟。
set -euo pipefail

manifest_dir="${1:-.}"

fail() { printf 'msrv-check FAILED: %s\n' "$*" >&2; exit 1; }

[ -f "$manifest_dir/Cargo.toml" ] || fail "no Cargo.toml in '$manifest_dir'"

# 取 [workspace.package] 里第一处 rust-version。子 crate 用
# `rust-version.workspace = true` 继承，所以这里读到的就是全仓唯一的真源。
msrv="$(sed -n 's/^rust-version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' \
  "$manifest_dir/Cargo.toml" | head -1)"
[ -n "$msrv" ] || fail "no rust-version found in $manifest_dir/Cargo.toml"

# rustup 的目录名带 target triple（1.89.0-aarch64-apple-darwin）。先试
# 当前机器常见的两种命名，再退到 glob；都找不到才报缺工具链。
toolchain="${YI_MSRV_TOOLCHAIN:-}"
if [ -z "$toolchain" ]; then
  for triple in aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-gnu; do
    candidate="$HOME/.rustup/toolchains/${msrv}.0-${triple}"
    if [ -x "$candidate/bin/cargo" ]; then
      toolchain="$candidate"
      break
    fi
  done
fi
if [ -z "$toolchain" ]; then
  toolchain="$(ls -d "$HOME"/.rustup/toolchains/"${msrv}"*-* 2>/dev/null | head -1 || true)"
fi
if [ -z "$toolchain" ] || [ ! -x "$toolchain/bin/cargo" ]; then
  fail "rustc ${msrv} is not installed; run: rustup toolchain install ${msrv}.0"
fi

actual="$("$toolchain/bin/rustc" --version)"
printf 'msrv-check: %s declared, building with: %s\n' "$msrv" "$actual"

cd "$manifest_dir"
PATH="$toolchain/bin:$PATH" cargo check --workspace --all-targets
printf 'msrv-check: ok (%s)\n' "$msrv"
