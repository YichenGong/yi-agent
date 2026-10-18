#!/usr/bin/env bash
# 挡住「真实部署凭据进跟踪文件」这类缺陷。
#
# 只扫 git 跟踪文件（`git grep` 默认只看跟踪内容），因此
# deploy/relay/.relay-local.md 这类「已忽略的本机私密记录」天然被排除。
#
# 退出码：0 = 通过；1 = 发现疑似凭据。
# 输出以 `::error::` 开头，GitHub Actions 会渲染成注解。
set -euo pipefail

# 扫描界钉在仓库根，而不是调用者的 cwd：`just` recipe 的 cwd 是
# `yi-agent-rs/`，若以 cwd 为界则根目录的 `docs/` 等被跳过，门禁会假绿。
cd "$(git rev-parse --show-toplevel)"

# 扫描排除项：
# - `scripts/check-secrets.sh`：本门禁自身必须写出这些字面值（否则无从匹配）；
# - `scripts/test_check_secrets.sh`：自测夹具必须内嵌真实值，才谈得上「能变红」；
# - `docs/superpowers/**`：本卡的设计文档与计划必须能指名它们要修的值。
# 排除这三处后，门禁对「真正的凭据落进普通文档」仍然零放过。
EXCLUDES=(
  ":(exclude)scripts/check-secrets.sh"
  ":(exclude)scripts/test_check_secrets.sh"
  ":(exclude)docs/superpowers/**"
)

fail=0

report() { # report <message>
  echo "::error::$1"
  fail=1
}

# 命中行常常是一整段文档；只报「文件:行号」加前 160 字符的片段，
# 否则 ::error:: 注解会长到无法阅读。
report_hits() { # report_hits <label> <git-grep-output>
  local label="$1" hits="$2"
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    local loc snippet
    loc="${line%%:*}"                       # 文件路径
    local rest="${line#*:}"
    loc="$loc:${rest%%:*}"                  # 加上行号 -> 文件:行
    snippet="$(printf '%s' "$rest" | cut -c1-160)"   # rest 不含 "文件:行:" 前缀
    report "$label ($loc): $snippet"
  done <<< "$hits"
}

# 1) 已知泄露值的字面量。轮换后若再出现，即为「有人从历史里捞回来」的回归。
set +e
hits=$(git grep -n -e 'fe42addbfed257dcd396e7fcc45e2725' -- . "${EXCLUDES[@]}" 2>/dev/null)
set -e
if [ -n "$hits" ]; then
  report_hits "known leaked session id present:" "$hits"
fi

# 2) 真实中继域名（示例域名 relay.example.com 不匹配本条）。
set +e
hits=$(git grep -n -E 'relay\.yi-agent\.top' -- . "${EXCLUDES[@]}" 2>/dev/null)
set -e
if [ -n "$hits" ]; then
  report_hits "real relay hostname in tracked file:" "$hits"
fi

# 3) 非保留段的公网 IP，且处于 ssh/relay 语境。
#    RFC 5737 保留段（192.0.2.0/24、198.51.100.0/24、203.0.113.0/24）不匹配。
set +e
hits=$(git grep -n -E '(101\.47\.76\.100)|(ssh[^\n]*root@[0-9]{1,3}(\.[0-9]{1,3}){3})' -- . "${EXCLUDES[@]}" 2>/dev/null)
set -e
if [ -n "$hits" ]; then
  report_hits "public IP in relay/ssh context:" "$hits"
fi

if [ "$fail" -ne 0 ]; then
  echo
  echo "凭据卫生检查失败。示例值请用 relay.example.com 与 RFC 5737 保留段（如 203.0.113.10）；"
  echo "真实值请写在被忽略的文件里（范式见 deploy/relay/.relay-local.md）。"
  exit 1
fi

echo "secret-check passed"