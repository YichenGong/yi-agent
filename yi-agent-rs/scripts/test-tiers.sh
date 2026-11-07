#!/usr/bin/env bash
# 分级测试入口的实现。四个子命令：
#   map          路径列表(stdin) → MAPPED/UNMAPPED/DOCS 三行
#   crate <名>   校验 crate 名并输出 nextest 参数
#   affected [ref]  git diff → crate 名 → rdeps 表达式
#   concurrency  检测本机其它 cargo 进程（只警告，不阻断）
#
# 为什么这些逻辑不写在 justfile 里：justfile 里的多行 bash 无法单测，
# 而「路径 → crate」这一步判据写宽/写窄都不报错，只能靠自测钉住。
set -uo pipefail

# 读 workspace 成员：优先用环境变量（自测注入），否则问 cargo。
# 必须处理 cwd：从仓库根调用时要指到 yi-agent-rs/Cargo.toml，否则 cargo
# 找不到 manifest，输出为空 → 所有 crate 都被判成 unmapped（静默全错）。
#
# 失败必须显式非零：读不到成员列表时，任何路径都不可能匹配成 crate，
# 于是全部落进 UNMAPPED 而退出码仍是 0。这是本设计要消除的「静默少测」，
# 所以这里宁可报错停下，也不能返回空列表。
workspace_crates() {
    if [ -n "${WORKSPACE_CRATES:-}" ]; then
        printf '%s\n' $WORKSPACE_CRATES
        return 0
    fi
    local manifest="Cargo.toml"
    if [ ! -f "$manifest" ] && [ -f "yi-agent-rs/Cargo.toml" ]; then
        manifest="yi-agent-rs/Cargo.toml"
    fi
    if ! command -v cargo >/dev/null 2>&1; then
        printf 'error: cargo not found in PATH; cannot read workspace members\n' >&2
        printf '       (set WORKSPACE_CRATES="a b c" to override)\n' >&2
        return 1
    fi
    if ! command -v python3 >/dev/null 2>&1; then
        printf 'error: python3 not found in PATH; cannot parse cargo metadata\n' >&2
        return 1
    fi
    local meta
    if ! meta="$(cargo metadata --no-deps --format-version 1 --manifest-path "$manifest" 2>/dev/null)"; then
        printf 'error: cargo metadata failed for %s; cannot read workspace members\n' "$manifest" >&2
        return 1
    fi
    local names
    if ! names="$(printf '%s' "$meta" | python3 -c '
import json, sys
try:
    doc = json.load(sys.stdin)
except Exception:
    sys.exit(3)
for pkg in doc["packages"]:
    print(pkg["name"])
' 2>/dev/null)"; then
        printf 'error: could not parse cargo metadata output\n' >&2
        return 1
    fi
    # 解析成功但一个成员都没有：同样是「空成员列表 → 全部判 unmapped」。
    # 正常 workspace 不会这样，但把这条路堵死才谈得上「读不到就报错」。
    if [ -z "$names" ]; then
        printf 'error: cargo metadata reported no packages for %s\n' "$manifest" >&2
        return 1
    fi
    printf '%s\n' "$names"
}

# 判定一条路径属于哪一类。
#   crates/<name>/... 且 <name> 是成员  → crate
#   *.md / docs/ / AGENTS.md / CLAUDE.md → docs
#   其余                                 → unmapped
# 注意：crates/<name>/ 但 <name> 不是成员（crate 被删/改名）要落到 unmapped，
# 不能因为路径长得像 crate 就当成 crate——那是静默少测的来源。
cmd_map() {
    local -a members=() mapped=() unmapped=() docs=()
    local m p rest name is_member
    # 先收集成员列表并判成败，再消费 stdin。否则读失败会变成「空成员列表」，
    # 所有路径静默落进 UNMAPPED——看起来成功，实际什么都没测。
    local member_list
    if ! member_list="$(workspace_crates)"; then
        printf 'error: cannot determine workspace members; refusing to classify paths\n' >&2
        exit 1
    fi
    while IFS= read -r m; do
        [ -n "$m" ] && members+=("$m")
    done <<<"$member_list"

    while IFS= read -r p; do
        [ -z "$p" ] && continue
        case "$p" in
            yi-agent-rs/crates/*)
                rest="${p#yi-agent-rs/crates/}"
                name="${rest%%/*}"
                is_member=0
                for m in ${members[@]+"${members[@]}"}; do
                    [ "$m" = "$name" ] && { is_member=1; break; }
                done
                if [ "$is_member" = "1" ]; then
                    # 去重，保持首次出现顺序
                    local seen=0 e
                    for e in ${mapped[@]+"${mapped[@]}"}; do
                        [ "$e" = "$name" ] && { seen=1; break; }
                    done
                    [ "$seen" = "0" ] && mapped+=("$name")
                else
                    unmapped+=("$p")
                fi
                ;;
            *.md | AGENTS.md | CLAUDE.md | docs/*)
                docs+=("$p")
                ;;
            *)
                unmapped+=("$p")
                ;;
        esac
    done

    printf 'MAPPED: %s\n' "${mapped[*]:-}"
    printf 'UNMAPPED: %s\n' "${unmapped[*]:-}"
    printf 'DOCS: %s\n' "${docs[*]:-}"
}

# 校验 crate 名。nextest -p 对拼错的名字不会报错，而是零匹配 → rc=4，
# 看起来像崩溃。先对齐 workspace 成员，把「名字写错」变成一条明确的错误。
# 成员列表读失败时直接返回 2（区别于「名字不存在」的 1），避免把环境问题
# 误报成「这个 crate 不存在」。
cmd_crate() {
    [ $# -ge 1 ] || usage
    local want="$1" m found=0
    local -a members=() avail=()
    local member_list
    if ! member_list="$(workspace_crates)"; then
        return 2
    fi
    while IFS= read -r m; do
        [ -n "$m" ] && members+=("$m")
    done <<<"$member_list"

    for m in ${members[@]+"${members[@]}"}; do
        [ "$m" = "$want" ] && { found=1; break; }
    done

    if [ "$found" = "1" ]; then
        printf 'OK %s\n' "$want"
        return 0
    fi

    printf 'error: %s is not a workspace crate\n' "$want" >&2
    printf 'available crates:\n' >&2
    for m in ${members[@]+"${members[@]}"}; do
        printf '  %s\n' "$m" >&2
    done
    return 1
}

# 取改动文件列表。FAKE_DIFF 存在时直接用它（自测注入），否则跑 git。
# 默认基准是 merge-base 的两点 diff：三点 diff（main...HEAD）会漏掉未提交改动，
# 而「刚改了点东西想跑相关测试」正是 L2 最常见的用法。
#
# 显式传 ref 时不做 merge-base：调用方要的就是「相对这个 ref 的改动」。
diff_paths() {
    if [ "${FAKE_DIFF+x}" = "x" ]; then
        printf '%s' "$FAKE_DIFF"
        return 0
    fi
    local ref="${1:-}"
    if [ -z "$ref" ]; then
        if git rev-parse --verify main >/dev/null 2>&1; then
            ref="$(git merge-base main HEAD 2>/dev/null)" || ref=""
        fi
    fi
    if [ -z "$ref" ]; then
        git diff --name-only
    else
        git diff --name-only "$ref"
    fi
}

cmd_affected() {
    local ref="${1:-}" paths p
    if ! paths="$(diff_paths "$ref")"; then
        printf 'error: could not read the changed-file list from git\n' >&2
        return 1
    fi
    # 去掉空行，只用于判空
    local nonempty
    nonempty="$(printf '%s\n' "$paths" | sed '/^$/d')"
    if [ -z "$nonempty" ]; then
        printf 'error: no changes to infer from (empty diff)\n' >&2
        printf 'hint: pass a base ref, e.g. `just test-affected origin/main`\n' >&2
        return 1
    fi

    # cmd_map 读不到成员列表时会 exit 1（它是整个脚本的终端命令），这里必须在
    # 子 shell 里调用并把失败原样转成「映射不可用」，不能当成「没有改动」。
    local out
    if ! out="$(printf '%s\n' "$paths" | cmd_map)"; then
        printf 'error: cannot map changed paths to crates\n' >&2
        return 1
    fi
    local mapped unmapped docs
    mapped="$(printf '%s\n' "$out" | sed -n 's/^MAPPED: //p')"
    unmapped="$(printf '%s\n' "$out" | sed -n 's/^UNMAPPED: //p')"
    docs="$(printf '%s\n' "$out" | sed -n 's/^DOCS: //p')"

    if [ -n "$unmapped" ]; then
        printf 'warning: these paths are not covered by test-affected:\n' >&2
        # 逐行读而不是 ${unmapped} 直接展开：git 输出的路径可以含空格。
        while IFS= read -r p; do
            [ -n "$p" ] && printf '  %s\n' "$p" >&2
        done <<<"$unmapped"
        printf 'note: run `just test` if your change is not reachable from a crate.\n' >&2
    fi

    if [ -z "$mapped" ]; then
        if [ -n "$docs" ] && [ -z "$unmapped" ]; then
            printf 'ok: no tests needed (docs-only change)\n' >&2
            return 0
        fi
        printf 'error: no changed path maps to a workspace crate\n' >&2
        return 1
    fi

    local expr="" c
    for c in $mapped; do
        [ -n "$expr" ] && expr="$expr | "
        expr="${expr}rdeps($c)"
    done
    printf '%s\n' "$expr"
    return 0
}

usage() {
    cat >&2 <<'EOF'
usage: test-tiers.sh <command> [args]
  map            路径列表从 stdin 读入，输出 MAPPED/UNMAPPED/DOCS
  crate <name>   校验 crate 名
  affected [ref] 推断受影响的 crate
  concurrency    检测本机其它 cargo 进程
EOF
    exit 2
}

main() {
    [ $# -ge 1 ] || usage
    case "$1" in
        map) cmd_map ;;
        crate) shift; cmd_crate "$@" ;;
        affected) shift; cmd_affected "$@" ;;
        *) usage ;;
    esac
}

main "$@"
