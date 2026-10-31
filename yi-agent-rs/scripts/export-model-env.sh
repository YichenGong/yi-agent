#!/usr/bin/env bash
# 从 `<instance_home>/models.json` 的默认条目导出 real-test 所需的环境变量。
#
# 用法（在 justfile 里）：
#     eval "$(bash scripts/export-model-env.sh)"
#
# 三条硬要求，都有测试钉着（scripts/test_export_model_env.sh）：
#   1. 首选 python3、回退 jq。前者在本仓库的开发前提（Rust 工具链 + CLT）下必然
#      存在，后者常需 brew install；而且 python3 能一次读出整份 JSON 再统一处理
#      null/缺字段，不必为四个变量写四份表达式。
#   2. 两者都缺时打一行提示并 exit 0——缺工具是环境问题，不该伪装成测试失败。
#      与「无 key 时 real-test 自动跳过、exit 0 不算失败」同一约定。
#   3. 只导出非空值。某字段为空时不导出空串，否则会把本来能用的真实环境变量
#      覆盖成空。
set -uo pipefail

instance_home() {
  if [ -n "${YI_AGENT_HOME:-}" ]; then
    case "$YI_AGENT_HOME" in
      */.yi-agent) printf '%s\n' "$YI_AGENT_HOME" ;;
      *) printf '%s/.yi-agent\n' "$YI_AGENT_HOME" ;;
    esac
  else
    printf '%s/.yi-agent\n' "${HOME:-}"
  fi
}

models_path="$(instance_home)/models.json"

if [ ! -f "$models_path" ]; then
  printf 'export-model-env: 没有找到 %s；真实 LLM 测试将跳过\n' "$models_path" >&2
  exit 0
fi

# 输出四行 `KEY=value`（不带 export），空值行省略。由调用方加 export。
read_fields() {
  # `command -v` 只证明文件在 PATH 上，不证明它能跑（残缺的解释器、被禁用的
  # 二进制、架构不匹配都算）。真跑一次空脚本才算可用，否则会「静默输出空」——
  # 既没回退 jq 也没报错，看起来就像「配置里没有 key」。
  if command -v python3 >/dev/null 2>&1 && python3 -c '' >/dev/null 2>&1; then
    python3 - "$1" <<'PY'
import json, sys
path = sys.argv[1]
try:
    with open(path, encoding="utf-8") as handle:
        data = json.load(handle)
except Exception:
    # 文件读不动或不是合法 JSON：这是配置问题而非工具缺失，提示后当作「无可用
    # 条目」处理（exit 0），不伪装成测试失败。
    print(f"export-model-env: 无法解析 {path}", file=sys.stderr)
    sys.exit(0)
models = data.get("models") or []
default = data.get("default_model")
entry = None
for model in models:
    if model.get("name") == default:
        entry = model
        break
if entry is None and models:
    entry = models[0]
if entry is None:
    sys.exit(0)
for env_key, field in (
    ("YI_AGENT_PROVIDER", "provider"),
    ("YI_AGENT_MODEL", "model"),
    ("MODEL_API_URL", "api_url"),
    ("MODEL_API_KEY", "api_key"),
):
    value = entry.get(field)
    if isinstance(value, str) and value.strip():
        print(f"{env_key}={value}")
PY
    return 0
  fi
  if command -v jq >/dev/null 2>&1 && jq --version >/dev/null 2>&1; then
    jq -r '
      (.models // []) as $models
      | ((.default_model // null) as $d
         | ($models | map(select(.name == $d)) | first) // $models[0]) as $e
      | select($e != null)
      | [
          ["YI_AGENT_PROVIDER", $e.provider],
          ["YI_AGENT_MODEL", $e.model],
          ["MODEL_API_URL", $e.api_url],
          ["MODEL_API_KEY", $e.api_key]
        ]
      | .[]
      | select(.[1] != null and (.[1] | tostring | length) > 0)
      | "\(.[0])=\(.[1])"
    ' "$1"
    return 0
  fi
  return 1
}

if ! fields="$(read_fields "$models_path")"; then
  printf 'export-model-env: 需要 python3 或 jq 之一来读取 models.json；真实 LLM 测试将跳过\n' >&2
  exit 0
fi

# 转义单引号：'  →  '\''
#
# 这个写法已在 bash 3.2 上实测过回环（含单引号、反斜杠、$ 的取值）：
#   v="it's a 'test' with \\ backslash and \$dollar"
#   eval "out='$(escape "$v")'"  →  out 与原值逐字节相同
# 双引号里的 `\\\\` 先变成 `\\`，sed 替换文本把 `\\` 解释成字面反斜杠，
# 故最终输出正是 `'\''`。不要「顺手简化」成 `s/'/\\''/g`——那会少一层转义，
# 生成的脚本在 `source` 时把值截断。
escape() {
  printf '%s' "$1" | sed "s/'/'\\\\''/g"
}

printf '%s\n' "$fields" | while IFS='=' read -r key value; do
  [ -n "$key" ] || continue
  printf "export %s='%s'\n" "$key" "$(escape "$value")"
done
