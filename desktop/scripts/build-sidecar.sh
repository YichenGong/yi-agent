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
  SRC="$ROOT/yi-agent-rs/target/release/yi-agent"
else
  cargo build --manifest-path "$ROOT/yi-agent-rs/Cargo.toml" -p yi-agent
  SRC="$ROOT/yi-agent-rs/target/debug/yi-agent"
fi

mkdir -p "$HERE/src-tauri/binaries"
cp "$SRC" "$HERE/src-tauri/binaries/yi-agent-$TRIPLE"
echo "sidecar -> src-tauri/binaries/yi-agent-$TRIPLE"
