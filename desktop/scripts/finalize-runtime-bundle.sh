#!/usr/bin/env bash
#
# Write the final runtime sidecar manifest into a built .app, then sign in the only
# order that keeps the recorded digest true.
#
# Why this exists: the manifest records the sidecar's sha256 so the desktop shell
# can prove the daemon it is about to run is the daemon it shipped. That digest is
# only meaningful if nothing re-signs the sidecar afterwards. `codesign --deep`
# re-signs nested binaries, which changes the file bytes after the digest was
# written — so the order has to be strict, and `--deep` is forbidden here:
#
#   1. sign the sidecar            (bytes become final)
#   2. hash it and write the manifest into Resources/
#   3. sign the outer app          (does not touch the already-signed sidecar)
#   4. verify with codesign
#
# A missing or mismatched manifest is a hard failure: shipping an app whose daemon
# identity cannot be verified is worse than failing the build.
#
# Usage: finalize-runtime-bundle.sh <path/to/App.app> [--dry-run]
set -euo pipefail

APP="${1:-}"
DRY_RUN="${2:-}"
if [ -z "$APP" ] || [ ! -d "$APP" ]; then
  echo "usage: finalize-runtime-bundle.sh <path/to/App.app>" >&2
  exit 2
fi

TRIPLE="$(rustc -vV | awk '/^host:/ {print $2}')"
MACOS_DIR="$APP/Contents/MacOS"
RES_DIR="$APP/Contents/Resources"
SIDECAR="$MACOS_DIR/yi-agent"
MANIFEST="$RES_DIR/runtime-sidecar.json"

run() {
  if [ "$DRY_RUN" = "--dry-run" ]; then
    printf 'would run: %s\n' "$*"
  else
    "$@"
  fi
}

if [ ! -x "$SIDECAR" ]; then
  echo "ERROR: sidecar not found in bundle: $SIDECAR" >&2
  exit 1
fi

# 1. The sidecar's bytes must be final before anything hashes them.
run codesign --force --sign - "$SIDECAR"

# 2. Ask the binary itself for its identity, then add the digest of the final bytes.
IDENTITY_JSON="$("$SIDECAR" internal-runtime identity --json)"
DIGEST="$(shasum -a 256 "$SIDECAR" | awk '{print $1}')"

mkdir -p "$RES_DIR"
# Paths in the manifest are bundle-relative and forward-slash separated: an
# absolute build-machine path is wrong as soon as the app moves. The executable
# path is the one Tauri's externalBin placed next to the desktop binary.
if [ "$DRY_RUN" = "--dry-run" ]; then
  printf 'would write manifest to %s\n' "$MANIFEST"
else
  printf '%s\n' "$IDENTITY_JSON" | python3 -c '
import json, sys, pathlib
identity = json.load(sys.stdin)
manifest = {
    "build_id": identity["build_id"],
    "display_version": identity["display_version"],
    "executable": "Contents/MacOS/yi-agent",
    "sha256": sys.argv[2],
    "schema_min": identity["schema_min"],
    "schema_max": identity["schema_max"],
    "handover_version": identity["handover_version"],
    "business_protocol_min": identity["business_protocol_min"],
    "business_protocol_max": identity["business_protocol_max"],
    "capabilities": identity["capabilities"],
}
path = pathlib.Path(sys.argv[1])
tmp = path.with_suffix(".json.tmp")
tmp.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
tmp.replace(path)
' "$MANIFEST" "$DIGEST"
fi

# 3. Sign the outer app. NOT --deep: that would re-sign the sidecar and invalidate
#    the digest written a moment ago.
run codesign --force --sign - "$APP"

# 4. Verify: the signature must hold and the digest must still match.
if [ "$DRY_RUN" != "--dry-run" ]; then
  codesign --verify --strict "$APP"
  ACTUAL="$(shasum -a 256 "$SIDECAR" | awk '{print $1}')"
  RECORDED="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["sha256"])' "$MANIFEST")"
  if [ "$ACTUAL" != "$RECORDED" ]; then
    echo "ERROR: sidecar digest changed after the manifest was written" >&2
    echo "  recorded: $RECORDED" >&2
    echo "  actual:   $ACTUAL" >&2
    exit 1
  fi
  echo "finalized $APP ($(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["build_id"])' "$MANIFEST"))"
fi
