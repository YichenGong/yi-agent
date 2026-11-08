#!/usr/bin/env bash
# 真实 LLM 测试的唯一入口。
#
# 两个职责：
#   1. 统一 key 检查。历史上 test-real-subagent 完全不检查 key，导致无 key 时
#      跑 0 个测试然后 exit 0 —— 报告写「通过」但什么都没验证。这里检查三种
#      key 的并集，任何一个存在就放行。
#   2. 把 suite 名映射到 (package, test binary)。
#
# 无可用 key 时打印 SKIPPED 并 exit 0：缺工具是环境问题，不伪装成测试失败
# （沿用 scripts/export-model-env.sh 的既有约定）。
set -euo pipefail

suite="${1:-}"

# suite -> "package test_binary"
describe_suite() {
    case "$1" in
        llm)      echo "yi-agent-llm real_integration" ;;
        e2e)      echo "yi-agent e2e_real" ;;
        complex)  echo "yi-agent e2e_complex" ;;
        subagent) echo "yi-agent subagent_real_e2e" ;;
        catalog)  echo "yi-agent model_catalog_live_reload_e2e" ;;
        *)        return 1 ;;
    esac
}

ALL_SUITES="llm e2e complex subagent catalog"

if [ -n "$suite" ]; then
    if ! describe_suite "$suite" >/dev/null; then
        echo "error: unknown suite '$suite'" >&2
        echo "valid suites: $ALL_SUITES (omit the argument to run all)" >&2
        exit 1
    fi
    SUITES="$suite"
else
    SUITES="$ALL_SUITES"
fi

# 清除 shell 环境变量污染（如 ~/.zshrc 里的全局 key），让 models.json 成为唯一配置源
unset ANTHROPIC_API_KEY OPENAI_API_KEY MODEL_API_KEY MODEL_API_URL YI_AGENT_PROVIDER YI_AGENT_MODEL
eval "$(bash scripts/export-model-env.sh)"

# 统一 key 检查：三者的并集。任一存在即可跑。
if [ -z "${ANTHROPIC_API_KEY:-}" ] && [ -z "${OPENAI_API_KEY:-}" ] && [ -z "${MODEL_API_KEY:-}" ]; then
    echo "SKIPPED: no provider API key configured (models.json has no usable default entry)"
    exit 0
fi

# 通用键优先：models.json 给的是 MODEL_API_KEY + provider/model 三件套，
# 而 provider 层测试认的是各家专属变量名。
if [ -n "${MODEL_API_KEY:-}" ] && [ -n "${YI_AGENT_PROVIDER:-}" ]; then
    case "$YI_AGENT_PROVIDER" in
        anthropic) export ANTHROPIC_API_KEY="$MODEL_API_KEY" ;;
        openai)    export OPENAI_API_KEY="$MODEL_API_KEY" ;;
    esac
fi

failed=""
for s in $SUITES; do
    read -r pkg bin <<<"$(describe_suite "$s")"
    echo "=== live suite: $s ($pkg::$bin) ==="
    # 一个 suite 失败不能阻止其余 suite 运行：真实使用中某个 provider 层测试
    # 失败（例如 provider 不匹配）不该把 e2e / complex 一起吞掉。失败记为
    # 非零退出码，全部跑完后再汇总。`set -e` 会在这里中断，所以临时关掉。
    if ! cargo nextest run --all-features --profile live --run-ignored only \
        -p "$pkg" --test "$bin"; then
        failed="$failed $s"
    fi
done

if [ -n "$failed" ]; then
    echo "FAILED suites:$failed" >&2
    exit 1
fi
