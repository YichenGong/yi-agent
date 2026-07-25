#!/usr/bin/env bash
set -euo pipefail

cd "${CLAUDE_PROJECT_DIR}"

STATE_FILE="${CLAUDE_PROJECT_DIR}/.claude/.last-merge-checked"

# 只在 main 分支上运行
current_branch=$(git symbolic-ref --short HEAD 2>/dev/null || echo "")
if [ "$current_branch" != "main" ]; then
    exit 0
fi

current_head=$(git rev-parse HEAD 2>/dev/null || echo "")
if [ -z "$current_head" ]; then
    exit 0
fi

last_commit=""
if [ -f "$STATE_FILE" ]; then
    last_commit=$(tr -d '[:space:]' < "$STATE_FILE" 2>/dev/null || echo "")
fi

# 计算自上次检查以来的新 commit 数量
new_commits=""
if [ -n "$last_commit" ] && [ "$last_commit" != "$current_head" ]; then
    if git rev-parse --verify "${last_commit}^{commit}" >/dev/null 2>&1; then
        new_commits=$(git rev-list --count "${last_commit}..HEAD" 2>/dev/null || echo "0")
        new_merge_commits=$(git log --oneline --merges "${last_commit}..HEAD" 2>/dev/null | wc -l | tr -d ' ' || echo "0")
    else
        # 记录的 commit 不存在了(rebase 等),退回 HEAD~1..HEAD
        if git rev-parse --verify HEAD~1 >/dev/null 2>&1; then
            new_commits=$(git rev-list --count "HEAD~1..HEAD" 2>/dev/null || echo "0")
            new_merge_commits=$(git log --oneline --merges "HEAD~1..HEAD" 2>/dev/null | wc -l | tr -d ' ' || echo "0")
        fi
    fi
elif [ -z "$last_commit" ]; then
    # 首次运行:看 HEAD~1..HEAD
    if git rev-parse --verify HEAD~1 >/dev/null 2>&1; then
        new_commits=$(git rev-list --count "HEAD~1..HEAD" 2>/dev/null || echo "0")
        new_merge_commits=$(git log --oneline --merges "HEAD~1..HEAD" 2>/dev/null | wc -l | tr -d ' ' || echo "0")
    fi
fi

# 更新状态文件为当前 HEAD
echo "$current_head" > "$STATE_FILE"

# 没有新 commit,静默退出
if [ -z "$new_commits" ] || [ "$new_commits" = "0" ]; then
    exit 0
fi

# 收集新 commit 的摘要(commit subject)
commit_summary=""
if [ -n "$last_commit" ] && git rev-parse --verify "${last_commit}^{commit}" >/dev/null 2>&1; then
    commit_summary=$(git log --oneline "${last_commit}..HEAD" 2>/dev/null | tail -20)
else
    commit_summary=$(git log --oneline "HEAD~1..HEAD" 2>/dev/null | tail -20)
fi

# 将 commit_summary 中的换行符转为字面 \n（用于 JSON 字符串）
commit_summary_escaped=$(echo "$commit_summary" | awk '{printf "%s\\n", $0}' | sed 's/\\n$//')

# 输出提示给 Claude Code
cat <<EOF
{
  "additionalContext": "main 分支自上次会话以来新增了 ${new_commits} 个 commit（其中 ${new_merge_commits} 个 merge commit）。新增 commit 列表:\\n${commit_summary_escaped}\\n\\n请检查 docs/project-management/ 下的进度表格是否需要更新——这是项目唯一可行的进度表格，必须与代码保持一致。如果有 feature 状态变化（完成、开始、放弃、新增），请同步更新对应的模块文件和 README.md 索引。"
}
EOF
exit 0
