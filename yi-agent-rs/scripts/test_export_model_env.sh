#!/usr/bin/env bash
# export-model-env.sh 的行为测试。全部在临时目录里跑，不碰真实 ~/.yi-agent。
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
target="$script_dir/export-model-env.sh"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

fail() { printf 'FAIL: %s\n' "$1" >&2; exit 1; }
pass() { printf 'ok - %s\n' "$1"; }

# 1. 默认条目被导出
mkdir -p "$tmp/home/.yi-agent"
cat > "$tmp/home/.yi-agent/models.json" <<'JSON'
{
  "models": [
    {"name":"a","provider":"openai","api_url":"https://a.example/v1","model":"m-a","api_key":"ka"},
    {"name":"b","provider":"anthropic","api_url":"https://b.example","model":"m-b","api_key":"kb"}
  ],
  "default_model": "b"
}
JSON

out="$(HOME="$tmp/home" bash "$target")"
printf '%s\n' "$out" | grep -q "export MODEL_API_KEY='kb'" || fail "default entry key not exported: $out"
printf '%s\n' "$out" | grep -q "export YI_AGENT_PROVIDER='anthropic'" || fail "provider not exported: $out"
printf '%s\n' "$out" | grep -q "export YI_AGENT_MODEL='m-b'" || fail "model not exported: $out"
printf '%s\n' "$out" | grep -q "export MODEL_API_URL='https://b.example'" || fail "url not exported: $out"
pass "default entry is exported"

# 2. 空字段不导出（否则会覆盖真实环境变量）
mkdir -p "$tmp/empty/.yi-agent"
cat > "$tmp/empty/.yi-agent/models.json" <<'JSON'
{"models":[{"name":"a","provider":"openai","api_url":"https://a.example/v1","model":"m-a","api_key":""}],"default_model":"a"}
JSON
out="$(HOME="$tmp/empty" bash "$target")"
printf '%s\n' "$out" | grep -q "MODEL_API_KEY" && fail "an empty key must not be exported: $out"
printf '%s\n' "$out" | grep -q "export YI_AGENT_MODEL='m-a'" || fail "non-empty fields still export: $out"
pass "empty fields are skipped"

# 3. 缺 models.json：静默 exit 0
mkdir -p "$tmp/none"
HOME="$tmp/none" bash "$target" >/dev/null 2>&1 || fail "a missing models.json must exit 0"
pass "missing models.json exits 0"

# 4. 特殊字符经 eval 回环后逐字节不变
#
# 断言方式是「真的 eval 一遍再取值」，而不是用 sed 去剥引号——后者要靠 GNU sed 的
# `\+`，在 macOS 自带的 BSD sed 上不生效，测出来的失败是假的。回环才是 `source`
# 实际要做的事，也更难被错误实现蒙混过关。
mkdir -p "$tmp/quote/.yi-agent"
python3 - "$tmp/quote/.yi-agent/models.json" "$tmp/quote/expected" <<'PY'
import json, sys

# 单引号、`$`、反引号、反斜杠：每一项都可能被错误转义吞掉。
value = "it's $dollar \\back and `tick`"
payload = {
    "models": [{
        "name": "a",
        "provider": "openai",
        "api_url": "https://a.example/v1",
        "model": "m-a",
        "api_key": value,
    }],
    "default_model": "a",
}
with open(sys.argv[1], "w", encoding="utf-8") as handle:
    json.dump(payload, handle)
# 期望值由 python 写出，避免在 bash 里手拼带单引号的字面量。
with open(sys.argv[2], "w", encoding="utf-8") as handle:
    handle.write(value)
PY
value="$(HOME="$tmp/quote" bash -c 'eval "$(bash "$0")"; printf %s "$MODEL_API_KEY"' "$target")"
expected="$(cat "$tmp/quote/expected")"
[ "$value" = "$expected" ] || fail "round-trip changed the value: got [$value] want [$expected]"
pass "quotes and shell metacharacters survive the round-trip"

# 5. 没有 python3 也没有 jq：提示并 exit 0
mkdir -p "$tmp/stub"
cat > "$tmp/stub/python3" <<'SH'
#!/usr/bin/env bash
exit 127
SH
chmod +x "$tmp/stub/python3"
# 只放一个 bash 的符号链接（调用方要用），其余一律没有。
# 不能顺手加上 /usr/bin：macOS 自带 /usr/bin/python3，加上去 stub 就轮不到，
# 这条测试永远测不到「两个工具都缺」的分支。
ln -sf "$(command -v bash)" "$tmp/stub/bash"
out="$(PATH="$tmp/stub" HOME="$tmp/home" "$tmp/stub/bash" "$target" 2>&1 || true)"
printf '%s\n' "$out" | grep -qi "python3\|jq" || fail "a missing tool must be reported: $out"
pass "missing tools are reported without failing"

echo "all export-model-env tests passed"
