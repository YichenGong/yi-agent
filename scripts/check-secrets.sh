#!/usr/bin/env bash
# 挡住「真实部署凭据进跟踪文件」这类缺陷。
#
# 只扫 git 跟踪文件（`git grep` 默认只看跟踪内容），因此
# deploy/relay/.relay-local.md 这类「已忽略的本机私密记录」天然被排除。
#
# 退出码：0 = 通过；1 = 发现疑似凭据；2 = 扫描没能真正执行（基础设施故障）。
# 「扫不动」必须失败，而不是假绿：非 git 仓库、一个文件都扫不到、
# `git grep` 自身报错（rc>1，如坏 pathspec）都走 exit 2；`git grep` 无匹配时
# rc=1，那才是正常的「没有匹配」。
# 输出以 `::error::` 开头，GitHub Actions 会渲染成注解。
set -euo pipefail

# 扫描界钉在仓库根，而不是调用者的 cwd：`just` recipe 的 cwd 是
# `yi-agent-rs/`，若以 cwd 为界则根目录的 `docs/` 等被跳过，门禁会假绿。
# 非仓库目录下 `git rev-parse` 会失败，但 macOS 上 `cd ""` 返回 0、`set -e`
# 兜不住，脚本会「零文件扫描」却报绿 —— 这里显式失败。
top="$(git rev-parse --show-toplevel 2>/dev/null)" || top=""
if [ -z "$top" ] || [ ! -d "$top" ]; then
  echo "::error::secret-check: not inside a git repository - cannot scan"
  exit 2
fi
cd "$top"

# 同一类假绿：一个文件都扫不到时 `git grep` 会「无匹配地成功」，于是
# 「零文件被扫描」被当成「没有凭据」。unborn HEAD 的空仓库即此形态。
scannable="$(git ls-files 2>/dev/null)" || scannable=""
if [ -z "$scannable" ]; then
  echo "::error::secret-check: nothing to scan (no tracked files) - cannot verify credentials"
  exit 2
fi

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
hits=$(git grep -n -e 'fe42addbfed257dcd396e7fcc45e2725' -- . "${EXCLUDES[@]}" 2>/dev/null); rc=$?
set -e
if [ "$rc" -gt 1 ]; then
  echo "::error::secret-check: git grep failed (rc=$rc) - cannot verify credentials"
  exit 2
fi
if [ -n "$hits" ]; then
  report_hits "known leaked session id present:" "$hits"
fi

# 2) 任何 session= 后紧跟 32 位十六进制的值都是凭据；占位符不匹配。
set +e
hits=$(git grep -n -E 'session=[0-9A-Fa-f]{32}([^0-9A-Fa-f]|$)' -- . "${EXCLUDES[@]}" 2>/dev/null); rc=$?
set -e
if [ "$rc" -gt 1 ]; then
  echo "::error::secret-check: git grep failed (rc=$rc) - cannot verify credentials"
  exit 2
fi
if [ -n "$hits" ]; then
  report_hits "session-shaped credential present:" "$hits"
fi

# 3) relay.* 主机不允许入库；示例主机 relay.example.com（含 URL 编码语境）放行。
set +e
hits=$(git grep -n -E 'relay(\.|%3[aA])[[:alnum:]-]+(\.|%2[eE])[[:alnum:].-]+' -- . "${EXCLUDES[@]}" 2>/dev/null); rc=$?
set -e
if [ "$rc" -gt 1 ]; then
  echo "::error::secret-check: git grep failed (rc=$rc) - cannot verify credentials"
  exit 2
fi
if [ -n "$hits" ]; then
  filtered="$(printf '%s\n' "$hits" | grep -v -E 'relay(\.|%3[aA])example(\.|%2[eE])com([^[:alnum:].-]|$)' || true)"
  if [ -n "$filtered" ]; then
    report_hits "non-example relay hostname in tracked file:" "$filtered"
  fi
fi

# 4) SSH/lsof 排查语境中的公网 IPv4。RFC 5737 示例段不命中。
set +e
hits=$(git grep -n -E 'ssh.*root@[0-9]{1,3}(\.[0-9]{1,3}){3}|lsof.*grep.*[0-9]{1,3}(\.[0-9]{1,3}){3}' -- . "${EXCLUDES[@]}" 2>/dev/null); rc=$?
set -e
if [ "$rc" -gt 1 ]; then
  echo "::error::secret-check: git grep failed (rc=$rc) - cannot verify credentials"
  exit 2
fi
if [ -n "$hits" ]; then
  public_hits=""
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    ips="$(printf '%s\n' "$line" | grep -Eo '[0-9]{1,3}(\.[0-9]{1,3}){3}' || true)"
    while IFS= read -r ip; do
      [ -n "$ip" ] || continue
      case "$ip" in
        192.0.2.*|198.51.100.*|203.0.113.*) ;;
        *) public_hits="${public_hits}${line}\n"; break ;;
      esac
    done <<< "$ips"
  done <<< "$hits"
  if [ -n "$public_hits" ]; then
    public_hits="$(printf '%b' "$public_hits")"
    report_hits "public IP in relay/ssh context:" "$public_hits"
  fi
fi

if [ "$fail" -ne 0 ]; then
  echo
  echo "凭据卫生检查失败。示例值请用 relay.example.com 与 RFC 5737 保留段（如 203.0.113.10）；"
  echo "真实值请写在被忽略的文件里（范式见 deploy/relay/.relay-local.md）。"
  exit 1
fi

echo "secret-check passed"
