#!/usr/bin/env bash
# check-model-setup-docs.sh 的自测：夹具必须能变红，否则门禁是摆设。
#
# 不依赖真实仓库内容：在临时目录里造一个最小 git 仓库，铺上夹具文件，
# 再把待测脚本拷进去跑。真实仓库的那次运行由 justfile 的
# `model-setup-docs-check` 负责。
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CHECKER="$HERE/check-model-setup-docs.sh"

PASS=0
FAIL=0
ok()   { PASS=$((PASS + 1)); printf 'ok   - %s\n' "$1"; }
fail() { FAIL=$((FAIL + 1)); printf 'FAIL - %s\n' "$1" >&2; }

assert_rc() { # assert_rc <name> <expected> <actual>
  if [ "$2" = "$3" ]; then ok "$1"; else fail "$1 (expected rc=$2, got rc=$3)"; fi
}

new_repo() { # new_repo -> prints temp dir
  local dir
  dir="$(mktemp -d)"
  ( cd "$dir" && git init -q . && git config user.email t@t && git config user.name t )
  mkdir -p "$dir/scripts" "$dir/desktop/scripts" "$dir/desktop/packaging" "$dir/docs/project-management"
  cp "$CHECKER" "$dir/scripts/check-model-setup-docs.sh"
  printf '%s\n' "$dir"
}

run_check() { # run_check <dir>
  ( cd "$1" && bash scripts/check-model-setup-docs.sh >/dev/null 2>&1 )
  echo $?
}

# --- 1. 干净仓库：全是正确文案，必须绿 ---
D="$(new_repo)"
cat > "$D/README.md" <<'EOF'
首次启动会弹出引导向导，填 API 密钥与模型标识，写入 ~/.yi-agent/models.json。
EOF
cat > "$D/README.en.md" <<'EOF'
The first launch walks you through the API key and model identifier.
EOF
cat > "$D/desktop/scripts/release-dmg.sh" <<'EOF'
2. **配置模型（必需）**：首次打开会弹引导向导，通过后写入 models.json。
EOF
cat > "$D/desktop/packaging/install.sh" <<'EOF'
配置：App 与 TUI 共用同一份模型清单 ~/.yi-agent/models.json，只需配一次。
EOF
cat > "$D/docs/project-management/desktop.md" <<'EOF'
- [x] 模型设置在设置页配置
EOF
assert_rc "clean repo passes" 0 "$(run_check "$D")"
rm -rf "$D"

# --- 2. README 教用户写 .env 里的 key：必须红 ---
D="$(new_repo)"
cat > "$D/README.md" <<'EOF'
echo 'MODEL_API_KEY=sk-ant-...' >> ~/.yi-agent/.env
EOF
assert_rc "README teaching .env key fails" 1 "$(run_check "$D")"
rm -rf "$D"

# --- 3. Release 说明残留「App 内没有设置入口」：必须红 ---
D="$(new_repo)"
cat > "$D/desktop/scripts/release-dmg.sh" <<'EOF'
2. **配置模型 key（必需，App 内没有设置入口）**
EOF
assert_rc "stale 'no settings entry' claim fails" 1 "$(run_check "$D")"
rm -rf "$D"

# --- 4. 安装器输出声称共用 .env：必须红 ---
D="$(new_repo)"
cat > "$D/desktop/packaging/install.sh" <<'EOF'
配置：App 与 TUI 共用 ~/.yi-agent/.env，只需写一次。
EOF
assert_rc "shared .env claim fails" 1 "$(run_check "$D")"
rm -rf "$D"

# --- 5. 项目管理/设计文档里引用旧文案不算违规（历史记录必须能说明修了什么） ---
D="$(new_repo)"
cat > "$D/README.md" <<'EOF'
首次启动会弹出引导向导。
EOF
cat > "$D/docs/project-management/ci-cd.md" <<'EOF'
原文案：echo 'MODEL_API_KEY=sk-ant-x' >> ~/.yi-agent/.env（已修，不再教用户这么做）
EOF
assert_rc "project-management docs may quote the old wording" 0 "$(run_check "$D")"
rm -rf "$D"

# --- 6. 不在 git 仓库里：必须 exit 2（拒绝假绿） ---
D="$(mktemp -d)"
mkdir -p "$D/scripts" "$D/desktop/scripts" "$D/desktop/packaging" "$D/docs/project-management"
cp "$CHECKER" "$D/scripts/check-model-setup-docs.sh"
( cd "$D" && bash scripts/check-model-setup-docs.sh >/dev/null 2>&1 )
assert_rc "outside a git repo refuses to pass" 2 "$?"
rm -rf "$D"

# --- 7. 设计文档里的历史引用不受影响：docs/superpowers 不参与扫描 ---
D="$(new_repo)"
mkdir -p "$D/docs/superpowers/plans"
cat > "$D/README.md" <<'EOF'
首次启动会弹出引导向导。
EOF
cat > "$D/docs/superpowers/plans/old.md" <<'EOF'
原文：echo 'MODEL_API_KEY=sk-ant-...' >> ~/.yi-agent/.env
EOF
assert_rc "design docs may quote the old wording" 0 "$(run_check "$D")"
rm -rf "$D"

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
