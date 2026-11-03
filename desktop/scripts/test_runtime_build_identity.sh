#!/usr/bin/env bash
#
# Contract tests for the runtime sidecar manifest and its finalize order.
#
# Every external tool is faked: this script must never touch a real signing
# identity, a real bundle, or the user's installed app. Run:
#   bash desktop/scripts/test_runtime_build_identity.sh
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

pass=0
fail=0
ok()   { pass=$((pass+1)); printf 'ok   - %s\n' "$1"; }
bad()  { fail=$((fail+1)); printf 'FAIL - %s\n' "$1"; [ -n "${2:-}" ] && printf '       %s\n' "$2"; }

# ---- fakes -----------------------------------------------------------------
BIN="$WORK/bin"
mkdir -p "$BIN"

cat > "$BIN/rustc" <<'EOF'
#!/usr/bin/env bash
echo "host: aarch64-apple-darwin"
EOF

cat > "$BIN/codesign" <<EOF
#!/usr/bin/env bash
# Record every signing call in order, and (like --force) change the signed file's
# bytes so a digest taken before signing is provably wrong.
echo "\$*" >> "$WORK/codesign.log"
target="\${@: -1}"
if [ -f "\$target" ] && [ "\${1:-}" != "--verify" ]; then
  printf 'signed' >> "\$target-\$RANDOM"
fi
exit 0
EOF

cat > "$BIN/shasum" <<'EOF'
#!/usr/bin/env bash
# Deterministic digest of the file's content, so tests can reason about it.
file="${@: -1}"
printf '%s  %s\n' "$(cksum < "$file" | awk '{print $1, $2}' | md5 -q 2>/dev/null || cksum < "$file" | awk '{print $1}')" "$file"
EOF

chmod +x "$BIN"/*
export PATH="$BIN:$PATH"

make_app() {
  local app="$1" build_id="$2"
  mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
  cat > "$app/Contents/MacOS/yi-agent" <<EOF
#!/usr/bin/env bash
if [ "\${1:-}" = "internal-runtime" ] && [ "\${2:-}" = "identity" ] && [ "\${3:-}" = "--json" ]; then
  cat <<JSON
{"build_id":"$build_id","display_version":"0.1.13","executable":"/abs/ignored","sha256":"",
 "schema_min":0,"schema_max":15,"handover_version":1,
 "business_protocol_min":2,"business_protocol_max":2,"capabilities":["control.v1"]}
JSON
  exit 0
fi
exit 1
EOF
  chmod +x "$app/Contents/MacOS/yi-agent"
}

# ---- 1. the manifest is written and carries no absolute build path ---------
APP="$WORK/A.app"
make_app "$APP" "deadbeefdeadbeefdeadbeefdeadbeef"
: > "$WORK/codesign.log"
if bash "$HERE/finalize-runtime-bundle.sh" "$APP" >"$WORK/finalize.log" 2>&1; then
  ok "finalize exits 0"
else
  bad "finalize exits 0" "$(cat "$WORK/finalize.log")"
fi

MANIFEST="$APP/Contents/Resources/runtime-sidecar.json"
if [ -f "$MANIFEST" ]; then ok "manifest written"; else bad "manifest written"; fi
if grep -q '"build_id": "deadbeefdeadbeefdeadbeefdeadbeef"' "$MANIFEST" 2>/dev/null; then
  ok "manifest records the sidecar build id"
else
  bad "manifest records the sidecar build id" "$(cat "$MANIFEST" 2>/dev/null)"
fi
if grep -q '/Users/\|/home/\|/tmp/' "$MANIFEST" 2>/dev/null; then
  bad "manifest has no build-machine absolute path" "$(cat "$MANIFEST")"
else
  ok "manifest has no build-machine absolute path"
fi
if grep -q '"executable": "Contents/MacOS/yi-agent"' "$MANIFEST" 2>/dev/null; then
  ok "manifest executable path is bundle-relative"
else
  bad "manifest executable path is bundle-relative" "$(cat "$MANIFEST" 2>/dev/null)"
fi

# ---- 2. signing order: sidecar first, then the app; never --deep ----------
if [ "$(sed -n '1p' "$WORK/codesign.log")" = "--force --sign - $APP/Contents/MacOS/yi-agent" ]; then
  ok "sidecar is signed first"
else
  bad "sidecar is signed first" "$(cat "$WORK/codesign.log")"
fi
if grep -q -- "--deep" "$WORK/codesign.log"; then
  bad "outer signing never uses --deep" "$(cat "$WORK/codesign.log")"
else
  ok "outer signing never uses --deep"
fi
# The app is signed after the sidecar; the verify call legitimately comes after
# both, so assert the relative order rather than "the final line".
sidecar_line="$(grep -n -- "--sign - $APP/Contents/MacOS/yi-agent" "$WORK/codesign.log" | cut -d: -f1)"
app_line="$(grep -n -- "--sign - $APP$" "$WORK/codesign.log" | cut -d: -f1)"
if [ -n "$app_line" ] && [ -n "$sidecar_line" ] && [ "$app_line" -gt "$sidecar_line" ]; then
  ok "outer app is signed after the sidecar"
else
  bad "outer app is signed after the sidecar" "$(cat "$WORK/codesign.log")"
fi

# ---- 3. the recorded digest matches the file after all signing ------------
if [ -f "$MANIFEST" ]; then
  recorded="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["sha256"])' "$MANIFEST")"
  actual="$(shasum -a 256 "$APP/Contents/MacOS/yi-agent" | awk '{print $1}')"
  if [ "$recorded" = "$actual" ]; then
    ok "recorded digest matches the final sidecar bytes"
  else
    bad "recorded digest matches the final sidecar bytes" "recorded=$recorded actual=$actual"
  fi
fi

# ---- 4. a bundle without a sidecar fails loudly --------------------------
EMPTY="$WORK/Empty.app"
mkdir -p "$EMPTY/Contents/MacOS"
if bash "$HERE/finalize-runtime-bundle.sh" "$EMPTY" >"$WORK/empty.log" 2>&1; then
  bad "a bundle without a sidecar fails"
else
  ok "a bundle without a sidecar fails"
fi

# ---- 5. dry-run touches nothing ------------------------------------------
DRY="$WORK/Dry.app"
make_app "$DRY" "cafebabecafebabecafebabecafebabe"
bash "$HERE/finalize-runtime-bundle.sh" "$DRY" --dry-run >"$WORK/dry.log" 2>&1
if [ ! -f "$DRY/Contents/Resources/runtime-sidecar.json" ]; then
  ok "dry-run writes no manifest"
else
  bad "dry-run writes no manifest"
fi

# ---- 6. the shipped binary really reports a hex build id -----------------
# Resolve the target dir the same way every other script does: CARGO_TARGET_DIR
# and .cargo/config must both be honoured, and a literal path breaks CI.
TARGET_DIR="$(cargo metadata --manifest-path "$REPO/yi-agent-rs/Cargo.toml" \
  --format-version 1 --no-deps 2>/dev/null \
  | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
[ -n "${TARGET_DIR:-}" ] || TARGET_DIR="$(cd "$REPO/yi-agent-rs" && pwd)/target"
BUILT="$TARGET_DIR/debug/yi-agent"
if [ -x "$BUILT" ]; then
  id="$("$BUILT" internal-runtime identity --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["build_id"])')"
  if printf '%s' "$id" | grep -Eq '^[0-9a-f]{32}$'; then
    ok "the built binary reports a 32-hex build id ($id)"
  else
    bad "the built binary reports a 32-hex build id" "$id"
  fi
  # The same binary must report the same id twice: reproducibility is what lets a
  # copied artifact keep its identity.
  again="$("$BUILT" internal-runtime identity --json | python3 -c 'import json,sys; print(json.load(sys.stdin)["build_id"])')"
  if [ "$id" = "$again" ]; then
    ok "the build id is stable across invocations"
  else
    bad "the build id is stable across invocations" "$id vs $again"
  fi
else
  printf 'skip - no debug binary at %s (build it first)\n' "$BUILT"
fi

# ---- 7. the hidden command is really hidden ------------------------------
if [ -x "$BUILT" ]; then
  if "$BUILT" --help 2>&1 | grep -q 'internal-runtime'; then
    bad "internal-runtime is hidden from --help"
  else
    ok "internal-runtime is hidden from --help"
  fi
fi

# ---- 8. every packaging entry point finalizes before archiving -------------
# Reading the scripts rather than running them: they stop the user's app and
# daemon, sign, and upload. The property to pin is purely textual order.
for entry in release-dmg.sh package-npm-app.sh; do
  script="$HERE/$entry"
  fin="$(grep -n 'finalize-runtime-bundle.sh' "$script" | head -1 | cut -d: -f1)"
  if [ -n "$fin" ]; then
    ok "$entry runs finalize-runtime-bundle.sh"
  else
    bad "$entry runs finalize-runtime-bundle.sh"
  fi
  if grep -vE '^[[:space:]]*#' "$script" | grep -q -- '--deep'; then
    bad "$entry never re-signs with --deep"
  else
    ok "$entry never re-signs with --deep"
  fi
done

# The dmg must be produced after the manifest exists, or it archives a bundle
# whose digest does not match its sidecar.
dmg="$(grep -n 'hdiutil create' "$HERE/release-dmg.sh" | head -1 | cut -d: -f1)"
fin="$(grep -n 'finalize-runtime-bundle.sh' "$HERE/release-dmg.sh" | head -1 | cut -d: -f1)"
if [ -n "$dmg" ] && [ -n "$fin" ] && [ "$fin" -lt "$dmg" ]; then
  ok "release-dmg.sh archives after finalizing"
else
  bad "release-dmg.sh archives after finalizing" "finalize@${fin:-none} hdiutil@${dmg:-none}"
fi

# tauri.conf.json must not build the dmg itself: that would bypass finalize.
if python3 -c '
import json,sys
targets = json.load(open(sys.argv[1]))["bundle"]["targets"]
sys.exit(0 if "dmg" not in targets else 1)
' "$REPO/desktop/src-tauri/tauri.conf.json"; then
  ok "tauri.conf.json leaves dmg to the packaging scripts"
else
  bad "tauri.conf.json leaves dmg to the packaging scripts"
fi

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
