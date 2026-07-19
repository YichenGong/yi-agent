#!/usr/bin/env bash
set -euo pipefail

cd "${CLAUDE_PROJECT_DIR}"

STATE_FILE="${CLAUDE_PROJECT_DIR}/.claude/.last-checked-commit"

current_head=$(git rev-parse HEAD 2>/dev/null || echo "")
if [ -z "$current_head" ]; then
    exit 0
fi

last_commit=""
if [ -f "$STATE_FILE" ]; then
    last_commit=$(tr -d '[:space:]' < "$STATE_FILE" 2>/dev/null || echo "")
fi

changes=""

# 1. 已提交的变化:上次记录的 commit → HEAD
if [ -n "$last_commit" ] && [ "$last_commit" != "$current_head" ]; then
    if git rev-parse --verify "${last_commit}^{commit}" >/dev/null 2>&1; then
        changes=$(git diff --name-only "${last_commit}..HEAD" 2>/dev/null || true)
    else
        # 记录的 commit 不存在了(rebase / filter-branch 等),退回 HEAD~1..HEAD
        if git rev-parse --verify HEAD~1 >/dev/null 2>&1; then
            changes=$(git diff --name-only "HEAD~1..HEAD" 2>/dev/null || true)
        fi
    fi
elif [ -z "$last_commit" ]; then
    # 首次运行:看 HEAD~1..HEAD(若仓库只有一个 commit 则跳过)
    if git rev-parse --verify HEAD~1 >/dev/null 2>&1; then
        changes=$(git diff --name-only "HEAD~1..HEAD" 2>/dev/null || true)
    fi
fi

# 2. 未提交的改动(工作树)
uncommitted=$(git status --porcelain 2>/dev/null | cut -c4- || true)
if [ -n "$uncommitted" ]; then
    if [ -n "$changes" ]; then
        changes="${changes}"$'\n'"${uncommitted}"
    else
        changes="$uncommitted"
    fi
fi

# 无论是否提示,都更新状态文件为当前 HEAD
echo "$current_head" > "$STATE_FILE"

if [ -z "$changes" ]; then
    exit 0
fi

# 3. 过滤出项目代码文件(.rs / .toml / .yaml / .yml / justfile)
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
