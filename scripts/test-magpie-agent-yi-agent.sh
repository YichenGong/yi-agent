#!/usr/bin/env bash
# Magpie agent 插件的 CI 检查：语法 + 行为 + 配置恢复。
#
# 只针对 `integrations/magpie-agent-yi-agent/`，与 Rust 侧的 `just ci` 分开跑——GitLab MR 的
# `rust:latest` 镜像没有 Node，不能把这个检查塞进 `just ci`（否则整条流水线
# 被 Node 缺失拖垮）。由 `.github/workflows/magpie-agent-yi-agent.yml`（path filter：
# `integrations/magpie-agent-yi-agent/**`）在保证有 Node 的环境里单独触发。
#
# 退出码：0 = 通过；1 = 测试失败；2 = 检查没能真正执行（基础设施故障，如
# 缺 Node）。「没跑成」必须失败而不是假绿。
set -euo pipefail

# Node 检查必须最先：这是「基础设施故障」门禁，只依赖 shell 内建
# （command / echo），不碰 `dirname`/`pwd` 等外部命令，这样在缺 coreutils 的
# 最小环境里也能给出明确的 exit 2 而不是「command not found」的 127。
if ! command -v node >/dev/null 2>&1; then
  echo "::error::magpie-agent-yi-agent-check: node is required but not found on PATH" >&2
  exit 2
fi

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PLUGIN_DIR="$(cd "$SCRIPT_DIR/.." && pwd)/integrations/magpie-agent-yi-agent"

if [ ! -d "$PLUGIN_DIR" ]; then
  echo "::error::magpie-agent-yi-agent-check: $PLUGIN_DIR does not exist" >&2
  exit 2
fi

fail=0

# 1) 语法：入口文件必须能独立解析（它跑在 magpie 的受限运行时里，任何 Node
#    API 或外部 import 都会在 magpie 里直接加载失败，这里先兜一层）。
if ! node --check "$PLUGIN_DIR/yi-agent.agent.js"; then
  echo "::error::magpie-agent-yi-agent-check: syntax check failed for yi-agent.agent.js" >&2
  fail=1
fi

# 2) 行为 + 配置恢复：node --test 直接驱动纯映射与 magpie 写协议的模拟。
#    用明确的文件路径而非 glob：glob 支持从 Node 21 才引入，CI 固定 Node 20。
if ! (cd "$PLUGIN_DIR" && node --test tests/agent.test.mjs); then
  echo "::error::magpie-agent-yi-agent-check: behavior tests failed" >&2
  fail=1
fi

if [ "$fail" -ne 0 ]; then
  echo "magpie-agent-yi-agent-check: failed" >&2
  exit 1
fi

echo "magpie-agent-yi-agent-check: passed"
