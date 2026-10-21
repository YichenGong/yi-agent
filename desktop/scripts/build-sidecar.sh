#!/usr/bin/env bash
# Build the yi-agent CLI and copy it to src-tauri/binaries with the
# target-triple suffix Tauri's externalBin mechanism requires.
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"     # desktop/
ROOT="$(cd "$HERE/.." && pwd)"               # repo root
TRIPLE="$(rustc -vV | awk '/^host:/ {print $2}')"
PROFILE="${1:-debug}"

if [ "$PROFILE" = "release" ]; then
  cargo build --manifest-path "$ROOT/yi-agent-rs/Cargo.toml" -p yi-agent --release
else
  cargo build --manifest-path "$ROOT/yi-agent-rs/Cargo.toml" -p yi-agent
fi

# target 目录不能假定是 yi-agent-rs/target：CI 为复用缓存把
# CARGO_TARGET_DIR 指到 $HOME/cargo-target/github，写死路径会让 cp 找不到
# 刚编出来的二进制（v0.1.10 实现在 1m05s 编译成功后 `cp: ... No such
# file or directory` 退 1）。cargo metadata 是唯一真源——它同时认
# CARGO_TARGET_DIR 与 .cargo/config 里的 build.target-dir。
TARGET_DIR="$(cargo metadata --manifest-path "$ROOT/yi-agent-rs/Cargo.toml" \
  --format-version 1 --no-deps 2>/dev/null \
  | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
[ -n "$TARGET_DIR" ] || TARGET_DIR="$ROOT/yi-agent-rs/target"
SRC="$TARGET_DIR/$PROFILE/yi-agent"

mkdir -p "$HERE/src-tauri/binaries"
cp "$SRC" "$HERE/src-tauri/binaries/yi-agent-$TRIPLE"
echo "sidecar -> src-tauri/binaries/yi-agent-$TRIPLE"
