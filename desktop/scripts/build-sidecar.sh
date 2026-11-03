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
DEST="$HERE/src-tauri/binaries/yi-agent-$TRIPLE"
cp "$SRC" "$DEST"
echo "sidecar -> src-tauri/binaries/yi-agent-$TRIPLE"

# src-tauri/build.rs 会执行这个二进制取 build id 并嵌进桌面壳；快了半个
# release 才发现 sidecar 不认这个子命令，代价是一次十分钟的重编。在这里
# 直接验一次：能跑、且给出 32 位十六进制。
BUILD_ID="$("$DEST" internal-runtime identity --json 2>/dev/null \
  | sed -n 's/.*"build_id"[[:space:]]*:[[:space:]]*"\([0-9a-f]*\)".*/\1/p')"
if ! printf '%s' "$BUILD_ID" | grep -Eq '^[0-9a-f]{32}$'; then
  echo "ERROR: $DEST does not report a build id (got '${BUILD_ID:-<empty>}')" >&2
  echo "       the desktop build would embed an unverifiable identity" >&2
  exit 1
fi
echo "build id -> $BUILD_ID"
