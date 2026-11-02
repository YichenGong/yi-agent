#!/usr/bin/env bash
# audit-deps.sh 的自测。不需要真实网络、不需要真实 cargo-audit。
#
# 为什么要自测：这道门禁的核心职责是「**绝不能把真漏洞当成网络问题降级**」。
# 那是个静默失效——一旦判错，门禁平时全绿、真出漏洞也不报，比没有门禁更危险。
# 靠人读一遍脚本无法保证，必须用假 cargo 把每个分支钉住。
#
# 六个分支：优雅通过 / 网络抖动后重试成功 / 网络全挂但有缓存 / 网络全挂且无缓存 /
# 真漏洞（必须红）/ 未知错误（必须红）/ 网络错误里混进漏洞计数（必须红）。

set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
target="$here/audit-deps.sh"

if [ ! -x "$target" ]; then
  echo "FAIL: $target 不存在或不可执行" >&2
  exit 1
fi

pass=0
fail=0

ok() { printf 'ok   - %s\n' "$1"; pass=$((pass + 1)); }
no() {
  printf 'FAIL - %s\n' "$1" >&2
  fail=$((fail + 1))
}

# 造一个假的 cargo：按 FAKE_MODE 输出不同结果，按调用次数推进行为。
#
# FAKE_MODE:
#   clean              每次都成功
#   net_then_ok        前 2 次网络失败，之后成功（模拟一次性抖动）
#   net_always         始终网络失败
#   vuln               直接报 17 个漏洞
#   unknown            未知错误文本
#   net_then_vuln      第一次网络失败，重试时报漏洞
#   net_and_vuln_text  网络错误，但输出里混有 vulnerabilities found 字样
make_fake_cargo() {
  local bin="$1"
  local mode="$2"
  local counter="$3"
  mkdir -p "$bin"
  cat >"$bin/cargo" <<EOF
#!/usr/bin/env bash
n=0
[ -f "$counter" ] && n=\$(cat "$counter")
n=\$((n + 1))
echo "\$n" > "$counter"

# 记录是否被调用过 --no-fetch（用于确认降级路径真的走了缓存）
case " \$* " in
  *" --no-fetch "*) echo "no-fetch" >> "$counter.args" ;;
esac

mode="$mode"
case "\$mode" in
  clean)
    echo "      Loaded 1402 security advisories"
    echo "warning: 1 allowed warning found"
    exit 0 ;;
  net_then_ok)
    if [ "\$n" -le 1 ]; then
      echo "    Fetching advisory database from \`https://github.com/RustSec/advisory-db.git\`" >&2
      echo "error: couldn't fetch advisory database: git operation failed: failed to prepare fetch" >&2
      echo "Caused by:" >&2
      echo "  -> An IO error occurred when talking to the server" >&2
      exit 1
    fi
    echo "warning: 1 allowed warning found"
    exit 0 ;;
  net_always)
    # 带 --no-fetch 时成功 = 本地缓存可用
    case " \$* " in
      *" --no-fetch "*)
        echo "      Loaded 1402 security advisories (from ~/.cargo/advisory-db)"
        echo "warning: 1 allowed warning found"
        exit 0 ;;
    esac
    echo "    Fetching advisory database" >&2
    echo "error: couldn't fetch advisory database: git operation failed" >&2
    echo "  -> An IO error occurred when talking to the server" >&2
    echo '  -> error sending request for url (https://github.com/RustSec/advisory-db.git/info/refs?service=git-upload-pack)' >&2
    exit 1 ;;
  net_no_cache)
    # 连缓存都没有：--no-fetch 也失败
    echo "error: couldn't fetch advisory database: git operation failed" >&2
    echo "  -> An IO error occurred when talking to the server" >&2
    exit 1 ;;
  vuln)
    echo "error: 17 vulnerabilities found!" >&2
    echo "warning: 11 allowed warnings found" >&2
    exit 1 ;;
  unknown)
    echo "error: cargo-audit fatal error: parse error: TOML parse error at line 14" >&2
    exit 1 ;;
  net_then_vuln)
    if [ "\$n" -le 1 ]; then
      echo "error: couldn't fetch advisory database: git operation failed" >&2
      echo "  -> An IO error occurred when talking to the server" >&2
      exit 1
    fi
    echo "error: 3 vulnerabilities found!" >&2
    exit 1 ;;
  net_and_vuln_text)
    echo "error: couldn't fetch advisory database: git operation failed" >&2
    echo "  -> An IO error occurred when talking to the server" >&2
    echo "  (note: 17 vulnerabilities found in a previous run)" >&2
    exit 1 ;;
esac
exit 0
EOF
  chmod +x "$bin/cargo"
}

# 在隔离 PATH 里跑 audit-deps.sh，返回它的退出码，输出存进 $OUTFILE
run_case() {
  local mode="$1"
  local dir="$2"
  mkdir -p "$dir/bin"
  : >"$dir/counter"
  make_fake_cargo "$dir/bin" "$mode" "$dir/counter"
  OUTFILE="$dir/out.txt"
  # 用极简 PATH，确保不会捡到真实 cargo
  PATH="$dir/bin:/usr/bin:/bin" bash "$target" >"$OUTFILE" 2>&1
  return $?
}

sandbox="$(mktemp -d)"
trap 'rm -rf "$sandbox"' EXIT

# ---- 1. 正常通过 ----
if run_case clean "$sandbox/c1"; then
  ok "clean: 退出码 0"
else
  no "clean: 应退出 0，实际 $?"
fi
if grep -q "警告" "$OUTFILE"; then
  no "clean: 正常通过时不应打降级警告"
else
  ok "clean: 无降级警告"
fi

# ---- 2. 网络抖动一次后重试成功 ----
if run_case net_then_ok "$sandbox/c2"; then
  ok "net_then_ok: 退出码 0（重试后成功）"
else
  no "net_then_ok: 应退出 0，实际 $?"
fi
if grep -q "重试成功" "$OUTFILE"; then
  ok "net_then_ok: 走了重试路径"
else
  no "net_then_ok: 未打印重试成功"
fi

# ---- 3. 网络全挂，但本地缓存可用 → 降级通过 + 醒目警告 ----
if run_case net_always "$sandbox/c3"; then
  ok "net_always: 退出码 0（降级通过）"
else
  no "net_always: 应降级为 0，实际 $?"
fi
if grep -q "用的是本地缓存库" "$OUTFILE"; then
  ok "net_always: 明确告知用了过期缓存"
else
  no "net_always: 未告知使用缓存库"
fi
if grep -q "advisory-db 无法更新" "$OUTFILE"; then
  ok "net_always: 打了降级警告"
else
  no "net_always: 缺少降级警告"
fi

# ---- 3b. 网络全挂且无缓存 → 降级通过，但必须明说「没做任何检查」----
if run_case net_no_cache "$sandbox/c3b"; then
  ok "net_no_cache: 退出码 0（降级通过）"
else
  no "net_no_cache: 应降级为 0，实际 $?"
fi
if grep -q "没有做任何依赖漏洞检查" "$OUTFILE"; then
  ok "net_no_cache: 明确告知本次未做检查"
else
  no "net_no_cache: 未告知本次未做检查（会让人以为查过了）"
fi

# ---- 4. 真漏洞 → 必须红，且不能退化成缓存路径 ----
if run_case vuln "$sandbox/c4"; then
  no "vuln: 真漏洞时必须退出 1，实际退出 0（严重：静默失效）"
else
  ok "vuln: 真漏洞 → 退出码非 0"
fi
if grep -q "发现真实依赖漏洞" "$OUTFILE"; then
  ok "vuln: 明确报出是真实漏洞而非网络问题"
else
  no "vuln: 未区分出真实漏洞"
fi
if [ -f "$sandbox/c4/counter.args" ]; then
  no "vuln: 真漏洞时不应走 --no-fetch 降级路径"
else
  ok "vuln: 未走降级路径（直接判红）"
fi

# ---- 5. 未知错误 → 不吞，判红 ----
if run_case unknown "$sandbox/c5"; then
  no "unknown: 未知错误时应退出 1，实际 0（会吞掉配置错误）"
else
  ok "unknown: 未知错误 → 退出码非 0"
fi
if grep -q "不匹配已知的网络失败或漏洞特征" "$OUTFILE"; then
  ok "unknown: 说明了为何不降级"
else
  no "unknown: 缺少不降级的说明"
fi

# ---- 6. 首次网络失败，重试时报漏洞 → 必须红（禁止沿网络路径降级）----
if run_case net_then_vuln "$sandbox/c6"; then
  no "net_then_vuln: 重试后发现漏洞必须退出 1，实际 0（严重：静默失效）"
else
  ok "net_then_vuln: 重试后发现漏洞 → 退出码非 0"
fi

# ---- 7. 网络错误文本里混进漏洞计数 → 必须红（双判据）----
if run_case net_and_vuln_text "$sandbox/c7"; then
  no "net_and_vuln_text: 输出含漏洞计数时应判红，实际 0"
else
  ok "net_and_vuln_text: 含漏洞计数 → 退出码非 0"
fi

# ---- 8. 找不到 cargo 时跳过而不是红 ----
d="$sandbox/c8"
mkdir -p "$d/bin"
OUTFILE="$d/out.txt"
# HOME 也要隔离：脚本会在 $HOME/.rustup 下回退查找 cargo，
  # 不隔离就会捡到本机真实 toolchain，测不到「完全没有 cargo」这条分支。
  HOME="$d/home" PATH="$d/bin:/bin:/usr/bin" bash "$target" >"$OUTFILE" 2>&1
rc=$?
if [ "$rc" -eq 0 ]; then
  ok "no-cargo: 缺 cargo 时跳过（退出 0）"
else
  no "no-cargo: 缺 cargo 时应退出 0，实际 $rc"
fi

printf '\n%d passed, %d failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
