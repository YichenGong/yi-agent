#!/usr/bin/env bash
# 挡住「教用户手写 .env 配模型」这类过时文案回流。
#
# 背景：模型配置早已迁到 `~/.yi-agent/models.json`，`.env` 里的模型键会被
# `config.rs` 的 `prune_dotenv_model_keys` 主动剔除（模型键不再从 `.env` 读）。
# 但 GitLab Release 说明、install.sh 输出、README 里长期留着
#
#     mkdir -p ~/.yi-agent && echo 'MODEL_API_KEY=sk-ant-...' >> ~/.yi-agent/.env
#
# 实测后果：用户照做后 `yi-agent run` 报「尚未配置模型」并 exit 1，`.env`
# 原样保留、`models.json` 根本没生成——因为迁移要求 `.env` 里同时有
# `YI_AGENT_MODEL`，只给 key 会以 `MissingModel` 静默放弃。
#
# 同一批文案还声称「App 内没有设置入口」，而设置 → 模型
# （`desktop/src/components/SettingsModelsTab.tsx`）与首启引导
# （`desktop/src/components/OnboardingWizard.tsx`）早已存在。
#
# 退出码：0 = 通过；1 = 发现过时文案。
set -uo pipefail

top="$(git rev-parse --show-toplevel 2>/dev/null)" || top=""
if [ -z "$top" ] || [ ! -d "$top" ]; then
  echo "::error::model-setup-docs: not inside a git repository - cannot scan"
  exit 2
fi
cd "$top"

fail=0
report() { # report <message>
  echo "::error::$1"
  fail=1
}

# 只扫「面向用户」的文件：发布说明模板、安装器输出、README。
# 设计/计划文档（docs/superpowers/**）与项目管理文档
# （docs/project-management/**、docs/bug-list.md）是历史记录，必须能引用
# 当时错在哪、修了什么，故不参与扫描。
TARGETS=(
  "README.md"
  "README.en.md"
  "desktop/scripts/release-dmg.sh"
  "desktop/packaging/install.sh"
)

existing=()
for t in "${TARGETS[@]}"; do
  [ -e "$t" ] && existing+=("$t")
done
if [ "${#existing[@]}" -eq 0 ]; then
  echo "::error::model-setup-docs: none of the target files exist - cannot verify"
  exit 2
fi

# 1) 禁止教用户往 .env 里写模型 key。
hits="$(grep -rn "MODEL_API_KEY=sk-ant" "${existing[@]}" 2>/dev/null || true)"
if [ -n "$hits" ]; then
  report "model-setup-docs: 不要再教用户把 MODEL_API_KEY 写进 .env（模型配置只认 models.json）:
$hits"
fi

# 2) 禁止「App 内没有设置入口」这类已经过时的断言。
hits="$(grep -rn "没有设置入口\|没有设置界面\|no settings entry" "${existing[@]}" 2>/dev/null || true)"
if [ -n "$hits" ]; then
  report "model-setup-docs: App 内已有「设置 → 模型」与首启引导，不要再说没有设置入口:
$hits"
fi

# 3) 禁止声称 App 与 TUI 共用 `.env` 作为模型配置。
hits="$(grep -rn "共用 ~/.yi-agent/.env\|共用\s*\`~/.yi-agent/.env\`\|shared .env" "${existing[@]}" 2>/dev/null || true)"
if [ -n "$hits" ]; then
  report "model-setup-docs: App 与 TUI 共用的是 models.json，不是 .env:
$hits"
fi

if [ "$fail" -eq 0 ]; then
  echo "model-setup-docs: ok (${#existing[@]} targets)"
fi
exit "$fail"
