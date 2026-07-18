#!/usr/bin/env bash
set -euo pipefail

cd "${CLAUDE_PROJECT_DIR}"

changes=$(git status --porcelain)

if [ -z "$changes" ]; then
    exit 0
fi

# 匹配项目代码文件：.rs, .toml, .yaml, .yml, justfile
code_changes=$(echo "$changes" | grep -E '\.(rs|toml|ya?ml)$|(^|/)justfile$' || true)

if [ -z "$code_changes" ]; then
    exit 0
fi

cat <<'EOF'
{
  "additionalContext": "本次会话修改了项目代码文件（.rs/.toml/.yaml/.yml/justfile）。请检查 docs/project-management/ 下的进度表格是否需要更新——这是项目唯一可行的进度表格，必须与代码保持一致。如果有 feature 状态变化（完成、开始、放弃、新增），请同步更新对应的模块文件和 README.md 索引。"
}
EOF
exit 0
