#!/usr/bin/env bash
# dev-env.sh 的行为测试。用临时目录，不碰真实 ~/.yi-agent。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$HERE/dev-env.sh"

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf 'ok: %s\n' "$*"; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# 1. id 确定且幂等
id1="$(YI_AGENT_INSTANCES_ROOT="$TMP/instances" "$SCRIPT" id)"
id2="$(YI_AGENT_INSTANCES_ROOT="$TMP/instances" "$SCRIPT" id)"
[ "$id1" = "$id2" ] || fail "id is not stable: $id1 vs $id2"
[ -n "$id1" ] || fail "id is empty"
pass "id is stable: $id1"

# 2. run 导出了正确的环境
out="$(YI_AGENT_INSTANCES_ROOT="$TMP/instances" "$SCRIPT" run sh -c 'printf "%s|%s|%s" "$YI_AGENT_HOME" "$YI_AGENT_RUNTIME_DIR" "$YI_AGENT_DISABLE_WATCHMAN"')"
home="${out%%|*}"
rest="${out#*|}"
runtime="${rest%%|*}"
watchman="${rest#*|}"
[ "$home" = "$TMP/instances/$id1/.yi-agent" ] || fail "YI_AGENT_HOME=$home, want $TMP/instances/$id1/.yi-agent"
[ "$runtime" = "$home/runtime" ] || fail "YI_AGENT_RUNTIME_DIR=$runtime"
[ "$watchman" = "1" ] || fail "YI_AGENT_DISABLE_WATCHMAN=$watchman"
pass "run exports an isolated environment"

# 3. run 会创建实例目录
[ -d "$home" ] || fail "instance dir was not created"
pass "run creates the instance dir"

# 3b. models.json 会被复制到新实例（否则实例开箱不可用）
mkdir -p "$TMP/stable/.yi-agent"
printf '{"version":1,"models":[],"default_model":null}' > "$TMP/stable/.yi-agent/models.json"
rm -rf "$TMP/instances"
YI_AGENT_INSTANCES_ROOT="$TMP/instances" HOME="$TMP/stable" \
  "$SCRIPT" run true >/dev/null 2>&1 || true
copied="$TMP/instances/$id1/.yi-agent/models.json"
[ -f "$copied" ] || fail "models.json was not copied into the instance"
pass "models.json is copied into the instance"

# 4. down --purge 删除目录（连空壳父目录一起）
YI_AGENT_INSTANCES_ROOT="$TMP/instances" "$SCRIPT" down --purge >/dev/null 2>&1
[ -d "$TMP/instances/$id1" ] && fail "down --purge left an empty $id1 dir behind"
pass "down --purge removes the instance dir"

# 5. 已存在的 .env 会被保留，不被父级覆盖
mkdir -p "$TMP/instances/$id1/.yi-agent"
printf 'MODEL_API_KEY=from-parent\n' > "$TMP/instances/$id1/.yi-agent/.env"
YI_AGENT_INSTANCES_ROOT="$TMP/instances" "$SCRIPT" run true >/dev/null 2>&1
grep -q 'from-parent' "$TMP/instances/$id1/.yi-agent/.env" || fail "existing .env was overwritten"
pass "an existing .env is preserved"

# 6. 未知子命令给出用法并返回非零
if YI_AGENT_INSTANCES_ROOT="$TMP/instances" "$SCRIPT" bogus >/dev/null 2>&1; then
  fail "an unknown subcommand must fail"
fi
pass "an unknown subcommand fails"

echo "all dev-env checks passed"
