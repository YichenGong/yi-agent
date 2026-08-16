# 项目进度同步 Hook 设计文档

**日期**: 2026-07-19
**状态**: 已确认
**范围**: `.claude/settings.json` + `.claude/hooks/check-progress-sync.sh`

---

## 1. 目标

在 yi-agent 项目中添加 Claude Code hook，每次 Claude 完成一轮回复后，如果本次修改了项目代码文件（`.rs`/`.toml`/`.yaml`/`.yml`/`justfile`），就提醒 Claude 检查并更新 `docs/project-management/` 下的进度表格，保持代码与进度表格一致。

## 2. 设计决策

| 决策点 | 选择 | 理由 |
|---|---|---|
| 触发事件 | `Stop` | 每次 Claude 完成一轮回复后触发，不依赖具体工具调用 |
| 判断方式 | `git status --porcelain` | 简单可靠，一行命令列出所有改动文件 |
| 触发条件 | 只在改了项目代码文件时 | 避免 `docs/` 下 markdown 改动也触发（包括改进度表格自己） |
| 行为 | 注入 `additionalContext`，不阻塞 | Claude 自己决定是否更新，但不强制中断流程 |
| Scope | 项目级 `.claude/settings.json` | 入版本控制，换机器自动生效；其他项目不受影响 |

## 3. 文件结构

```
.claude/
  settings.json                      # hook 配置
  hooks/
    check-progress-sync.sh           # hook 脚本（可执行）
```

## 4. 配置文件 `.claude/settings.json`

```json
{
  "hooks": {
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "${CLAUDE_PROJECT_DIR}/.claude/hooks/check-progress-sync.sh"
          }
        ]
      }
    ]
  }
}
```

**说明：**
- `Stop` 事件不支持 matcher，每次 Claude 完成回复都触发
- `${CLAUDE_PROJECT_DIR}` 由 Claude Code 注入，指向项目根
- 脚本忽略 stdin（不需要解析 hook 的 JSON 输入）

## 5. Hook 脚本逻辑

```bash
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
```

**关键点：**
- `set -euo pipefail` 严格模式
- `grep -E` 正则匹配项目代码扩展名 + `justfile`
- `|| true` 防止 grep 无匹配时返回非零导致脚本中断
- 输出 JSON 到 stdout，Claude Code 解析 `additionalContext` 注入到上下文
- exit 0，不阻塞 Claude 停止

## 6. 判断逻辑说明

**哪些改动会触发提醒：**
- `crates/yi-agent-core/src/message.rs` → 触发（`.rs`）
- `yi-agent-rs/justfile` → 触发（`justfile`）
- `.gitlab-ci.yml` → 触发（`.yml`）
- `yi-agent-rs/Cargo.toml` → 触发（`.toml`）

**哪些改动不会触发：**
- `docs/project-management/README.md` → 不触发（`.md`）
- `docs/plans/2026-07-19-xxx.md` → 不触发（`.md`）
- `.gitignore` → 不触发（无扩展名，不在白名单）
- `README.md`（根目录）→ 不触发（`.md`）

## 7. 维护方式

- 修改 hook 脚本后需要 `chmod +x`
- 修改 `.claude/settings.json` 后 Claude Code 自动重新加载
- 调试 hook：在脚本里加 `echo "DEBUG: $changes" >&2` 看 stderr（会显示在 transcript 的 hook error notice 里）
