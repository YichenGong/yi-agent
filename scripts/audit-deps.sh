#!/usr/bin/env bash
# 依赖漏洞审计门禁：`just ci` 的第 15 道。
#
# 为什么需要这层包装，而不是 justfile 里直接写 `cargo audit`：
#
# `cargo audit` 对「网络失败」和「真发现漏洞」**返回同一个退出码 1**，无法靠
# 退出码区分。实测（cargo-audit 0.22.2）：
#
#   $ cargo audit --url https://127.0.0.1:9/nope.git    # 拉不到库
#   error: couldn't fetch advisory database: git operation failed ...
#   $? = 1
#
#   $ cargo audit                                       # 真发现漏洞
#   error: 17 vulnerabilities found!
#   $? = 1
#
# 直接写 `cargo audit` 的后果：advisory-db 首次拉取偶发抖动一次，`just ci`
# 就整条变红。变红的信号是假的（代码没问题），而假红会训练人开始忽略这道
# 门禁——比没有门禁更糟。
#
# 处置策略：
#   1. 正常跑一次 `cargo audit`。rc=0 → 通过。
#   2. rc≠0 且输出匹配**网络失败特征** → 重试一次（瞬时抖动通常能过）。
#      重试仍失败 → 再试 `--no-fetch --stale`（用本地已有缓存库）。
#        - 缓存可用且干净 → 打醒目警告、exit 0（降级通过）
#        - 缓存也没有 → 打醒目警告、exit 0（降级通过）
#   3. rc≠0 且**不是**网络失败特征 → 直接 exit 1（真漏洞，必须红）。
#
# 为什么网络失败时选择「降级通过」而不是红：
#   audit 检查的是**依赖清单**，与本次代码改动无关。网络状况不该卡住与它无关
#   的代码正确性判定。兜底在 nightly 的独立 audit job（云端 runner 网络稳定，
#   且那里网络失败会真红）。这与仓库既有约定一致：real-llm 系 recipe 在无
#   key 时 SKIP 并 exit 0，而不是伪装成测试失败。
#
# 反过来也要钉死：**绝不能把真漏洞也降级**。区分靠 stderr 文本特征，且要求
# 网络特征存在的同时「漏洞计数」特征不存在——两道判据，避免网络错误信息里
# 恰好含 vulnerabilities 字样时误判。
#
# 判据见 docs/superpowers/specs/2026-11-02-quality-hardening-wave2-design.md
# 的「实施期修正」节。自测：scripts/test_audit_deps.sh（已进 just ci）。

set -uo pipefail

# cargo 可能不在默认 PATH（本机只在 rustup toolchain 目录里）
if ! command -v cargo >/dev/null 2>&1; then
  for d in "$HOME"/.rustup/toolchains/*/bin; do
    [ -x "$d/cargo" ] && PATH="$d:$PATH" && break
  done
fi

if ! command -v cargo >/dev/null 2>&1; then
  printf 'audit-deps: 找不到 cargo；依赖审计将跳过\n' >&2
  exit 0
fi

# 输出缓冲到变量里，同时原样回显，避免用户看不到详情
run_audit() {
  cargo audit "$@" 2>&1 | tee /tmp/.audit-deps.$$.out >&2
  return "${PIPESTATUS[0]}"
}

# 输出里是否含**网络失败**特征
is_network_failure() {
  grep -qE "couldn't fetch advisory database|An IO error occurred when talking to the server|error sending request for url" "$1"
}

# 输出里是否含**真漏洞**特征（有则绝不降级）
#
# 这里刻意用**宽匹配**：只要求出现「N vulnerabilities found」，不锚定行首的
# `error: ` 前缀，也不要求就是结尾。
#
# 为什么不收紧：这是安全门禁，两类误判的代价完全不对称。
#   - 误判成「有漏洞」（假红）：人来看一眼，确认是网络问题，改回代码即可，
#     代价是一次打扰。
#   - 误判成「网络问题」（假绿）：漏洞被静默跳过，可能带着已知 CVE 发版，
#     代价不可逆。
# 所以只要输出里出现任何漏洞计数字样，一律按真漏洞判红。网络失败的正常输出
# 里不会出现 `N vulnerabilities found`（拿不到库就无从计数），放宽匹配不会
# 造成日常假红。
has_vulnerabilities() {
  grep -qE "[0-9]+ vulnerabilit(y|ies) found" "$1"
}

OUT=/tmp/.audit-deps.$$.out
trap 'rm -f "$OUT"' EXIT

if run_audit; then
  exit 0
fi

# ---- 第一次失败：判定是网络问题还是真漏洞 ----
if has_vulnerabilities "$OUT"; then
  printf '\naudit-deps: 发现真实依赖漏洞（见上），门禁失败。\n' >&2
  printf '  处置：按 .cargo/audit.toml 的三类分类处理，不要为了让门禁变绿而乱加 ignore。\n' >&2
  exit 1
fi

if ! is_network_failure "$OUT"; then
  # 既不是网络问题也不是漏洞计数——可能是配置/解析错误，不能吞掉
  printf '\naudit-deps: cargo audit 失败，且输出不匹配已知的网络失败或漏洞特征。\n' >&2
  printf '  按真实失败处理（不降级）。请人工查看上面的输出。\n' >&2
  exit 1
fi

# ---- 是网络问题：重试一次 ----
printf '\naudit-deps: advisory-db 拉取失败（网络问题），重试一次...\n' >&2
if run_audit; then
  printf 'audit-deps: 重试成功。\n' >&2
  exit 0
fi

if has_vulnerabilities "$OUT"; then
  printf '\naudit-deps: 重试后发现真实依赖漏洞，门禁失败。\n' >&2
  exit 1
fi

# ---- 仍然网络失败：降级到本地缓存库 ----
printf 'audit-deps: 重试仍失败，改用本地缓存库（--no-fetch --stale）...\n' >&2
if run_audit --no-fetch --stale; then
  printf '\n' >&2
  printf '========================================================\n' >&2
  printf '  警告：advisory-db 无法更新，本次用的是本地缓存库。\n' >&2
  printf '  结论基于过期数据，可能漏报新披露的 CVE。\n' >&2
  printf '  兜底：nightly 的 audit job 会在网络正常的 runner 上重跑。\n' >&2
  printf '========================================================\n' >&2
  printf '\n' >&2
  exit 0
fi

if has_vulnerabilities "$OUT"; then
  printf '\naudit-deps: 缓存库中发现真实依赖漏洞，门禁失败。\n' >&2
  exit 1
fi

# ---- 连缓存都没有 ----
printf '\n' >&2
printf '========================================================\n' >&2
printf '  警告：advisory-db 拉取失败，且本地没有可用缓存。\n' >&2
printf '  本次**没有做任何依赖漏洞检查**，门禁降级通过。\n' >&2
printf '  兜底：nightly 的 audit job。若长期如此请检查网络/代理。\n' >&2
printf '========================================================\n' >&2
printf '\n' >&2
exit 0
