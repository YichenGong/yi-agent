# 分级测试入口 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 新增 L0/L1/L2 三个分级测试入口和一个并发检测前置警告，让「改了 X 该跑 Y」有明确路由，替代现在「要么全量 13 分钟、要么只跑一个 crate」的二选一。

**Architecture:** 全部落在 `yi-agent-rs/justfile` 与新脚本，不改任何 Rust 代码、不改 `.config/nextest.toml`。复杂逻辑放 `yi-agent-rs/scripts/*.sh`，按本仓惯例配 `test_*.sh` 自测并挂到 `just ci`。L2 的筛选交给 nextest 原生 `rdeps()`，脚本只负责「路径 → crate 名」这一步。

**Tech Stack:** Bash 3.2（macOS 自带）、just、cargo nextest、`cargo metadata --no-deps` + python3 读 JSON。

**设计文档:** `docs/superpowers/specs/2026-11-07-graded-test-tiers-design.md`（下文简称 spec）

## Global Constraints

- 工作必须在 worktree 里：`.worktrees/test-tiers`，分支 `feat/graded-test-tiers`。严禁在 `main` 上改。
- **不改** `.config/nextest.toml` 的任何内容（spec D4：`max-threads` 维持 1）。
- **不改** 任何 Rust 源码、测试断言、超时预算数值。
- **不改** `just ci` 的既有 21 个门禁；只在末尾**追加** `affected-check`。
- **不改** `just test` 的命令与退出码；只在其前面**追加**一段只读预检输出。
- **不碰** `yi-agent-rs/Cargo.toml`、`yi-agent-rs/scripts/check-msrv.sh`、`target/.metadata_never_index`（另一条工作线负责）。
- 所有 L1/L2/L3 recipe 必须带 `--all-features`（spec L1 节已实证：漏掉会让 `yi-agent-store` 直接编译失败）。
- 所有 nextest 调用必须带 `--profile ci`（JUnit 只在该 profile 生成）。
- 脚本用 `#!/usr/bin/env bash` + `set -uo pipefail`（**不要** `set -e`，本仓 `test_reclaim_target.sh` 即如此，因为要收集多个断言失败而不是首个失败就退出）。
- 自测脚本尾部必须 `printf '\n%d passed\n' "$n"`，与本仓既有脚本一致。
- 提交前在 `yi-agent-rs/` 下跑 `cargo fmt --all`（本设计不产 Rust 代码，预期无变更）与 `just ci`。
- Commit message 用 conventional commits，不写 `Co-Authored-By`。
- 完成一组改动后同步更新 `docs/project-management/tooling.md`（本设计属 tooling 模块）。

## 环境前置

`cargo` 不在默认 PATH，每个需要 cargo 的 shell 先执行：

```bash
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:$PATH"
```

本机无 `timeout` 命令、无 `rg`。计时用 `/usr/bin/time -p`，搜索用 `grep -rE`。

## File Structure

| 文件 | 责任 | 动作 |
|---|---|---|
| `yi-agent-rs/scripts/test-tiers.sh` | 全部四个子命令的实现：`check`/`crate`/`affected`/`concurrency` | 新建 |
| `yi-agent-rs/scripts/test_test_tiers.sh` | 上述脚本的自测（纯 bash 断言，不编译） | 新建 |
| `yi-agent-rs/justfile` | 4 个 recipe 入口 + `ci:` 追加 1 项 + `test:` 加预检 | 修改 |
| `docs/project-management/tooling.md` | 记录新入口 | 修改 |
| `docs/superpowers/plans/2026-11-07-graded-test-tiers.md` | 本文件 | 新建 |

**为什么一个脚本而不是四个：** 四个子命令共享「读 workspace 成员」「判定文档/未映射」两段逻辑，拆开就要么复制要么再引一层。本仓 `reclaim-target.sh` 也是单脚本多子命令（`--apply` / `--include-worktrees`），照此办理。

**为什么逻辑在脚本而不直接写在 justfile：** justfile 里写多行 bash 会丢失可测性。本仓 `reclaim` 的教训写在 `scripts/test_reclaim_target.sh` 头部：判据写宽/写窄都不会报错，只能靠自测钉住。

---

### Task 1: 脚本骨架 + 路径到 crate 的映射（纯函数）

**Files:**
- Create: `yi-agent-rs/scripts/test-tiers.sh`
- Create: `yi-agent-rs/scripts/test_test_tiers.sh`

**Interfaces:**
- Consumes: 无
- Produces:
  - `test-tiers.sh map` —— stdin 读路径列表（每行一个），stdout 输出三行：
    `MAPPED: <空格分隔 crate 名>`、`UNMAPPED: <路径>`、`DOCS: <路径>`
  - 环境变量 `WORKSPACE_CRATES` 可覆盖成员列表（自测用）；缺省时从 `cargo metadata` 读

- [ ] **Step 1: 写失败的测试**

新建 `yi-agent-rs/scripts/test_test_tiers.sh`：

```bash
#!/usr/bin/env bash
# test-tiers.sh 的自测：判据必须只按「路径前缀」分类。
#
# 为什么值得单独测：这个分类决定了 test-affected 跑哪些 crate。分类写宽了
# 会把 desktop/ 当成 Rust crate 去跑（报错或空跑）；写窄了会静默少测——
# 后者是本设计要消除的失败模式，不会报错，只能靠这里钉住。
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPT="$HERE/test-tiers.sh"
[ -x "$SCRIPT" ] || { echo "FAIL: $SCRIPT not executable" >&2; exit 1; }

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf 'ok: %s\n' "$*"; }
n=0

# 固定成员列表，避免测试依赖真实 workspace 内容。
export WORKSPACE_CRATES="yi-agent-core yi-agent-flags yi-agent-web"

map() { printf '%s\n' "$@" | "$SCRIPT" map; }

out="$(map 'yi-agent-rs/crates/yi-agent-core/src/a.rs')"
grep -qx 'MAPPED: yi-agent-core' <<<"$out" || { printf '%s\n' "$out" >&2; fail "core src not mapped"; }
n=$((n + 1)); pass "maps crates/<name>/src to <name>"

out="$(map 'yi-agent-rs/crates/yi-agent-flags/tests/b.rs')"
grep -qx 'MAPPED: yi-agent-flags' <<<"$out" || fail "tests/ path not mapped"
n=$((n + 1)); pass "maps crates/<name>/tests to <name>"

out="$(map 'yi-agent-rs/crates/yi-agent-web/Cargo.toml')"
grep -qx 'MAPPED: yi-agent-web' <<<"$out" || fail "manifest path not mapped"
n=$((n + 1)); pass "maps crates/<name>/Cargo.toml to <name>"

# 去重 + 保持出现顺序
out="$(map 'yi-agent-rs/crates/yi-agent-core/src/a.rs' 'yi-agent-rs/crates/yi-agent-core/src/b.rs' 'yi-agent-rs/crates/yi-agent-flags/src/c.rs')"
grep -qx 'MAPPED: yi-agent-core yi-agent-flags' <<<"$out" \
  || { printf '%s\n' "$out" >&2; fail "did not dedupe/order"; }
n=$((n + 1)); pass "dedupes while preserving first-seen order"

out="$(map 'docs/x.md' 'README.md' 'AGENTS.md' 'CLAUDE.md')"
grep -qx 'DOCS: docs/x.md README.md AGENTS.md CLAUDE.md' <<<"$out" || fail "docs not classified"
grep -qx 'MAPPED: ' <<<"$out" || fail "docs wrongly mapped to a crate"
n=$((n + 1)); pass "classifies docs as DOCS, not as crates"

out="$(map 'desktop/src/app.ts' 'plugins/foo.json' 'integrations/x.rs')"
grep -qx 'UNMAPPED: desktop/src/app.ts plugins/foo.json integrations/x.rs' <<<"$out" \
  || { printf '%s\n' "$out" >&2; fail "non-crate paths not classified as unmapped"; }
n=$((n + 1)); pass "classifies non-crate paths as UNMAPPED"

# 关键反例：crates/ 下但不是宵 workspace 成员的目录（已删除的 crate）
out="$(map 'yi-agent-rs/crates/yi-agent-gone/src/a.rs')"
grep -qx 'UNMAPPED: yi-agent-rs/crates/yi-agent-gone/src/a.rs' <<<"$out" \
  || { printf '%s\n' "$out" >&2; fail "deleted crate should be UNMAPPED, not MAPPED"; }
n=$((n + 1)); pass "treats crates/<removed>/ as UNMAPPED, not MAPPED"

# yi-agent-rs/crates 之外的 yi-agent-rs 文件（justfile 等）也是 UNMAPPED
out="$(map 'yi-agent-rs/justfile')"
grep -qx 'UNMAPPED: yi-agent-rs/justfile' <<<"$out" || fail "justfile should be UNMAPPED"
n=$((n + 1)); pass "treats yi-agent-rs/justfile as UNMAPPED"

# 关键反例：读不到成员列表时必须失败退出，不能把全部路径判成 UNMAPPED 后返回 0。
# 这是本设计最危险的静默失败：cargo 不可用 → 一个 crate 都匹配不上 → 少测所有东西。
unset WORKSPACE_CRATES
err="$(env PATH=/usr/bin:/bin "$SCRIPT" map <<<'yi-agent-rs/crates/yi-agent-core/src/a.rs' 2>&1 >/dev/null)"
rc=$?
[ "$rc" -ne 0 ] || { fail "map must fail (not exit 0) when workspace members cannot be read"; }
grep -q 'cargo' <<<"$err" || { printf '%s\n' "$err" >&2; fail "error must mention cargo"; }
n=$((n + 1)); pass "fails loudly when workspace members cannot be read"

# 同上，但 cargo 存在而 metadata 失败（模拟损坏的 manifest）
fakebin="$(mktemp -d)"
trap 'rm -rf "$fakebin"' EXIT
cat >"$fakebin/cargo" <<'SH'
#!/bin/sh
echo "error: failed to read manifest" >&2
exit 101
SH
chmod +x "$fakebin/cargo"
err="$(env PATH="$fakebin:/usr/bin:/bin" "$SCRIPT" map <<<'yi-agent-rs/crates/yi-agent-core/src/a.rs' 2>&1 >/dev/null)"
rc=$?
[ "$rc" -ne 0 ] || { fail "map must fail when cargo metadata fails"; }
grep -q 'cargo metadata failed' <<<"$err" || { printf '%s\n' "$err" >&2; fail "error must mention cargo metadata failure"; }
n=$((n + 1)); pass "fails loudly when cargo metadata fails"

# 同上，但 metadata 成功却报告零个 package（空 workspace / 输出被截断）
cat >"$fakebin/cargo" <<'SH'
#!/bin/sh
echo '{"packages":[]}'
SH
chmod +x "$fakebin/cargo"
err="$(env PATH="$fakebin:/usr/bin:/bin" "$SCRIPT" map <<<'yi-agent-rs/crates/yi-agent-core/src/a.rs' 2>&1 >/dev/null)"
rc=$?
[ "$rc" -ne 0 ] || { fail "map must fail when workspace has no members"; }
grep -q 'no packages' <<<"$err" || { printf '%s\n' "$err" >&2; fail "error must say no packages"; }
n=$((n + 1)); pass "fails loudly when workspace reports no members"

printf '\n%d passed\n' "$n"
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && chmod +x scripts/test_test_tiers.sh && bash scripts/test_test_tiers.sh
```

预期：`FAIL: .../test-tiers.sh not executable`，退出码 1。

- [ ] **Step 3: 写最小实现**

新建 `yi-agent-rs/scripts/test-tiers.sh`：

```bash
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
        *) usage ;;
    esac
}

main "$@"
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && chmod +x scripts/test-tiers.sh && bash scripts/test_test_tiers.sh
```

预期：输出 11 行 `ok: ...`，最后 `11 passed`，退出码 0。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs
git add scripts/test-tiers.sh scripts/test_test_tiers.sh
git commit -m "feat(test-tiers): add path-to-crate mapping with self-test

The three-way classification (crate / docs / unmapped) decides which
crates test-affected runs. A too-loose predicate runs the wrong crates;
a too-tight one silently under-tests, which never errors. Both are the
failure mode this design exists to remove, so pin it with assertions —
same reasoning as scripts/test_reclaim_target.sh."
```

---

### Task 2: `crate` 子命令 —— 校验成员名

**Files:**
- Modify: `yi-agent-rs/scripts/test-tiers.sh`（加 `cmd_crate`）
- Modify: `yi-agent-rs/scripts/test_test_tiers.sh`（加断言）

**Interfaces:**
- Consumes: Task 1 的 `workspace_crates()`
- Produces: `test-tiers.sh crate <name>` —— 成员则 stdout 打印 `OK <name>` 且 exit 0；
  非成员则 stderr 打印可用成员列表且 exit 1

- [ ] **Step 1: 写失败的测试**

在 `test_test_tiers.sh` 的 `printf '\n%d passed\n'` **之前**插入：

```bash
# --- crate 子命令 ---
out="$(WORKSPACE_CRATES="yi-agent-core yi-agent-flags" "$SCRIPT" crate yi-agent-flags 2>&1)"
[ $? -eq 0 ] || fail "valid crate name rejected"
grep -q '^OK yi-agent-flags$' <<<"$out" || { printf '%s\n' "$out" >&2; fail "valid crate not acknowledged"; }
n=$((n + 1)); pass "accepts a real crate name"

WORKSPACE_CRATES="yi-agent-core yi-agent-flags" "$SCRIPT" crate yi-agent-nope >/dev/null 2>&1
[ $? -eq 1 ] || fail "unknown crate name should exit 1"
n=$((n + 1)); pass "rejects an unknown crate name with exit 1"

err="$(WORKSPACE_CRATES="yi-agent-core yi-agent-flags" "$SCRIPT" crate yi-agent-nope 2>&1)"
grep -q 'yi-agent-core' <<<"$err" || { printf '%s\n' "$err" >&2; fail "error did not list available crates"; }
n=$((n + 1)); pass "lists available crate names on rejection"

# 无参数
"$SCRIPT" crate >/dev/null 2>&1
[ $? -eq 2 ] || fail "missing arg should exit 2"
n=$((n + 1)); pass "missing crate arg exits 2"
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && bash scripts/test_test_tiers.sh
```

预期：`FAIL: valid crate name rejected`（`crate` 尚未实现，走 `usage` 返回 2）。

- [ ] **Step 3: 写最小实现**

在 `test-tiers.sh` 的 `usage()` 函数之前插入：

```bash
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
```

并把 `main` 的 `case` 改成：

```bash
    case "$1" in
        map) cmd_map ;;
        crate) shift; cmd_crate "$@" ;;
        *) usage ;;
    esac
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && bash scripts/test_test_tiers.sh
```

预期：`15 passed`，退出码 0。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs
git add scripts/test-tiers.sh scripts/test_test_tiers.sh
git commit -m "feat(test-tiers): validate crate names before running them

Passing a typo like 'yi-agent-flag' to nextest -p silently matches
nothing and exits 4, which reads as a mysterious failure rather than
'a crate name is wrong'. Validate against workspace members first and
list the valid names in the error."
```

---

### Task 3: `affected` 子命令 —— git diff 到 rdeps 表达式

**Files:**
- Modify: `yi-agent-rs/scripts/test-tiers.sh`（加 `cmd_affected`）
- Modify: `yi-agent-rs/scripts/test_test_tiers.sh`（加断言）

**Interfaces:**
- Consumes: Task 1 的 `cmd_map`、Task 2 的成员校验思路
- Produces: `test-tiers.sh affected [ref]`，输出**恰好一行** `rdeps(a) | rdeps(b)` 到 stdout，
  供 justfile 拼进 `-E`。退出码：
  - 0 = 有映射，已输出表达式
  - 1 = 无映射（全 docs / 全 unmapped / diff 为空），stdout 为空，stderr 说明原因

- [ ] **Step 1: 写失败的测试**

在 `test_test_tiers.sh` 的 `printf '\n%d passed\n'` **之前**插入：

```bash
# --- affected 子命令 ---
# 用假 git 输出来驱动：通过 FAKE_DIFF 环境变量注入改动文件列表。
export WORKSPACE_CRATES="yi-agent-core yi-agent-flags yi-agent-web"

out="$(FAKE_DIFF='yi-agent-rs/crates/yi-agent-core/src/a.rs' "$SCRIPT" affected ref 2>&1)"
[ $? -eq 0 ] || fail "affected with mapped crate should exit 0"
grep -qx 'rdeps(yi-agent-core)' <<<"$out" || { printf '%s\n' "$out" >&2; fail "wrong expression"; }
n=$((n + 1)); pass "emits rdeps() for a single mapped crate"

out="$(FAKE_DIFF='yi-agent-rs/crates/yi-agent-core/src/a.rs
yi-agent-rs/crates/yi-agent-flags/src/b.rs' "$SCRIPT" affected ref 2>&1)"
grep -qx 'rdeps(yi-agent-core) | rdeps(yi-agent-flags)' <<<"$out" \
  || { printf '%s\n' "$out" >&2; fail "multi-crate expression wrong"; }
n=$((n + 1)); pass "joins multiple crates with ' | '"

# 全 docs → exit 0 且说「无需测试」（spec: 文档优先于错误分支）
FAKE_DIFF='docs/x.md' "$SCRIPT" affected ref >/dev/null 2>&1
[ $? -eq 0 ] || fail "docs-only diff should exit 0"
n=$((n + 1)); pass "docs-only diff exits 0"

msg="$(FAKE_DIFF='docs/x.md' "$SCRIPT" affected ref 2>&1)"
grep -q 'no tests needed' <<<"$msg" || { printf '%s\n' "$msg" >&2; fail "docs case did not explain itself"; }
n=$((n + 1)); pass "docs-only diff prints 'no tests needed'"

# 全 unmapped → exit 1
FAKE_DIFF='desktop/src/app.ts' "$SCRIPT" affected ref >/dev/null 2>&1
[ $? -eq 1 ] || fail "all-unmapped diff should exit 1"
n=$((n + 1)); pass "all-unmapped diff exits 1"

# 部分映射 → exit 0，表达式只含映射项，且 stderr 报未映射路径
out="$(FAKE_DIFF='yi-agent-rs/crates/yi-agent-core/src/a.rs
desktop/src/app.ts' "$SCRIPT" affected ref 2>/tmp/tt_err.$$)"
grep -qx 'rdeps(yi-agent-core)' <<<"$out" || fail "partial: expression wrong"
grep -q 'desktop/src/app.ts' /tmp/tt_err.$$ || fail "partial: unmapped path not reported"
n=$((n + 1)); pass "partial mapping runs crates and reports the rest"
rm -f /tmp/tt_err.$$

# 空 diff → exit 1
FAKE_DIFF='' "$SCRIPT" affected ref >/dev/null 2>&1
[ $? -eq 1 ] || fail "empty diff should exit 1"
n=$((n + 1)); pass "empty diff exits 1"
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && bash scripts/test_test_tiers.sh
```

预期：`FAIL: affected with mapped crate should exit 0`。

- [ ] **Step 3: 写最小实现**

在 `test-tiers.sh` 的 `usage()` 之前插入：

```bash
# 取改动文件列表。FAKE_DIFF 存在时直接用它（自测注入），否则跑 git。
# 默认基准是 merge-base 的两点 diff：三点 diff（main...HEAD）会漏掉未提交改动，
# 而「刚改了点东西想跑相关测试」正是 L2 最常见的用法。
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
        # cmd_map 的 UNMAPPED 是空格拼接的单行（spec 定义的三行格式），所以这里
        # 整行输出。含空格的路径在这里无法与分隔符区分，但它只用于提示，不参与
        # 判定，不影响实际跑哪些 crate。
        printf '  %s\n' "$unmapped" >&2
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
```

并把 `main` 的 `case` 补上一行：

```bash
        affected) shift; cmd_affected "$@" ;;
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && bash scripts/test_test_tiers.sh
```

预期：`29 passed`，退出码 0。

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs
git add scripts/test-tiers.sh scripts/test_test_tiers.sh
git commit -m "feat(test-tiers): infer affected crates from a git diff

Emits a nextest filterset expression such as 'rdeps(a) | rdeps(b)' so
the caller does not have to hand-compute the reverse-dependency closure.
Defaults to the merge-base two-dot diff because three-dot omits
uncommitted changes, which is exactly when this is most useful.

Docs-only diffs exit 0; all-unmapped diffs exit 1 rather than silently
running zero tests; partial mappings run what they can and report the
rest on stderr."
```

---

### Task 4: `concurrency` 子命令 + `check` 子命令（L0）

**Files:**
- Modify: `yi-agent-rs/scripts/test-tiers.sh`（加 `cmd_concurrency`、`cmd_check`）
- Modify: `yi-agent-rs/scripts/test_test_tiers.sh`（加断言）

**Interfaces:**
- Consumes: 无
- Produces:
  - `test-tiers.sh concurrency` —— **永远 exit 0**（只警告），命中时 stderr 打印
    `WARN: 本机上另有 N 个 cargo/rustc 进程（PID: ...）` 与一行处置建议
  - `test-tiers.sh check` —— 执行 `cargo check --all-targets --all-features`，
    透传其退出码；不在 0 个 target 时静默

- [ ] **Step 1: 写失败的测试**

在 `test_test_tiers.sh` 的 `printf '\n%d passed\n'` **之前**插入：

```bash
# --- concurrency 子命令 ---
# 注入一个假的 pgrep，避免依赖本机真实进程状态。
fakebin="$(mktemp -d)"
trap 'rm -rf "$fakebin"; rm -f /tmp/tt_err.$$' EXIT
cat >"$fakebin/pgrep" <<'FAKE'
#!/usr/bin/env bash
# 只对 cargo 返回两个 pid
[ "$2" = "cargo" ] && { printf '111\n222\n'; exit 0; }
exit 1
FAKE
chmod +x "$fakebin/pgrep"

out="$(PATH="$fakebin:$PATH" "$SCRIPT" concurrency 2>&1)"
[ $? -eq 0 ] || fail "concurrency must always exit 0"
grep -q 'WARN' <<<"$out" || { printf '%s\n' "$out" >&2; fail "no warning when processes present"; }
grep -q '111' <<<"$out" || fail "warning did not list pids"
n=$((n + 1)); pass "warns and lists pids when cargo processes exist"

grep -q '独占' <<<"$out" || fail "warning lacks the 'rerun alone' guidance"
n=$((n + 1)); pass "warning tells the user to rerun alone before judging"

# --- run 子命令：rc=4 必须透传并给出说明 ---
cat >"$fakebin/cargo" <<'FAKE'
#!/usr/bin/env bash
exit 4
FAKE
chmod +x "$fakebin/cargo"
PATH="$fakebin:$PATH" "$SCRIPT" run 'rdeps(x)' >/dev/null 2>/tmp/tt_run.$$
[ $? -eq 4 ] || fail "run must propagate rc=4, not swallow it"
grep -q 'no tests matched' /tmp/tt_run.$$ || fail "run did not explain rc=4"
n=$((n + 1)); pass "propagates rc=4 with an explanatory note"
rm -f /tmp/tt_run.$$

# 没有进程时静默通过
cat >"$fakebin/pgrep" <<'FAKE'
#!/usr/bin/env bash
exit 1
FAKE
chmod +x "$fakebin/pgrep"
out="$(PATH="$fakebin:$PATH" "$SCRIPT" concurrency 2>&1)"
[ $? -eq 0 ] || fail "concurrency should exit 0 when quiet"
grep -q 'WARN' <<<"$out" && fail "warned with no processes"
n=$((n + 1)); pass "stays quiet and exits 0 when no cargo processes"
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && bash scripts/test_test_tiers.sh
```

预期：`FAIL: concurrency must always exit 0`（未实现 → `usage` 返回 2）。

- [ ] **Step 3: 写最小实现**

在 `test-tiers.sh` 的 `usage()` 之前插入：

```bash
# 只警告，不阻断。跨会话并发是假红的根因（spec D3），但在配置层修不掉，
# 所以让它在发生时可见。阻断会让 CI 失效、并让人养成绕过习惯。
# 注意 pgrep -x 匹配整机进程，无法判断对方是否用同一个 CARGO_TARGET_DIR，
# 所以文案说的是「本机上」，不是「同一个 target」。
cmd_concurrency() {
    local name pid list
    local -a pids=()
    for name in cargo cargo-nextest rustc; do
        list="$(pgrep -x "$name" 2>/dev/null)" || continue
        for pid in $list; do
            pids+=("$pid")
        done
    done
    if [ "${#pids[@]}" -gt 0 ]; then
        printf 'WARN: 本机上另有 %d 个 cargo/rustc 进程（PID: %s）\n' \
            "${#pids[@]}" "${pids[*]}" >&2
        printf '      若本次出现超时类失败，先用独占运行复跑对应 crate 再判断\n' >&2
    fi
    return 0
}

cmd_check() {
    cargo check --all-targets --all-features
}

# 跑 nextest 并解释退出码。
# nextest 在「0 个用例匹配」时返回 4（实测），不是 0。原样透传会让人以为是
# 崩溃；改写成 0 又会让「其实一个测试都没跑」变成绿灯。所以透传 4，
# 但补一句说明，把注意力引向映射而不是被测代码。
cmd_run() {
    local expr="$1"
    cargo nextest run --all-features --profile ci -E "$expr"
    local rc=$?
    if [ "$rc" -eq 4 ]; then
        printf '\nnote: nextest exited 4 = no tests matched the filter.\n' >&2
        printf '      the path-to-crate mapping may be off, or these crates have no tests.\n' >&2
    fi
    return "$rc"
}
```

并把 `main` 的 `case` 补上三行：

```bash
        concurrency) cmd_concurrency ;;
        check) cmd_check ;;
        run) shift; cmd_run "$@" ;;
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && bash scripts/test_test_tiers.sh
```

预期：`29 passed`，退出码 0。

- [ ] **Step 5: 手动验证 check 覆盖 tests/**

```bash
cd yi-agent-rs
printf 'pub fn zz_probe() -> u32 { "not a number" }\n' > crates/yi-agent-web/tests/zz_tmp_probe.rs
bash scripts/test-tiers.sh check; echo "rc=$?"
```

预期：`rc` 非 0，输出含 `error[E0308]` 与 `(test "zz_tmp_probe")`。

```bash
rm -f crates/yi-agent-web/tests/zz_tmp_probe.rs
git status --short   # 确认临时文件已不在
```

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs
git add scripts/test-tiers.sh scripts/test_test_tiers.sh
git commit -m "feat(test-tiers): add concurrency warning and L0 check command

concurrency only warns, never blocks: cross-session contention is the
real cause of the fake reds, but it cannot be fixed in nextest config,
so the design's goal is to make it visible rather than silent.

check uses --all-targets, proven necessary by a control experiment: a
type error in tests/ is caught only with that flag."
```

---

### Task 5: 接上 justfile 的四个 recipe

**Files:**
- Modify: `yi-agent-rs/justfile`（在 `test:` 之后插入新 recipe；在 `test:` 内加预检）

**Interfaces:**
- Consumes: Task 1–4 的 `scripts/test-tiers.sh`
- Produces: `just check-fast`、`just test-crate <name>`、`just test-affected [ref]`、
  `just test` 的预检输出

- [ ] **Step 1: 加 recipe**

在 `justfile` 的 `test:` recipe 之后（`build:` 之前）插入：

```make
# === 分级测试入口（见 docs/superpowers/plans/2026-11-07-graded-test-tiers.md） ===
#
# L0 类型检查：不产出测试二进制，因此不触发 nextest 的 --list 探测开销。
# 用 --all-targets 是必须的：默认不检查 tests/，而本仓有 108 个集成测试 target。
# 注意 L0 不承诺秒级——它比 L1 快，仅此而已。
check-fast:
    bash scripts/test-tiers.sh check

# L1 单 crate 测试。--all-features 不能省：yi-agent-store 的
# foreground-test-support feature 门控了测试辅助方法，漏掉会直接编译失败。
test-crate crate:
    bash scripts/test-tiers.sh crate {{crate}}
    cargo nextest run --all-features -p {{crate}} --profile ci

# L2 改动相关的 crate：由 git diff 推断，再交给 nextest 的 rdeps() 扩展到反向依赖。
# 默认基准是 merge-base 的两点 diff（含未提交改动）。
# 用法：just test-affected            （对比 merge-base main HEAD）
#       just test-affected origin/main （对比指定 ref）
test-affected since="":
    #!/usr/bin/env bash
    set -uo pipefail
    expr="$(bash scripts/test-tiers.sh affected {{since}})" || exit $?
    if [ -z "$expr" ]; then
        # 例如纯文档改动：affected 已说明「no tests needed」并 exit 0
        exit 0
    fi
    echo "nextest -E '$expr'"
    bash scripts/test-tiers.sh run "$expr"
```

- [ ] **Step 2: 给 `test` 加只读预检**

把现有 `test:` recipe：

```make
test:
    cargo nextest --version
    cargo nextest run --all-features --workspace --profile ci
```

改成：

```make
test:
    bash scripts/test-tiers.sh concurrency
    cargo nextest --version
    cargo nextest run --all-features --workspace --profile ci
```

**注意**：只加第一行，后两行原样保留；不改命令、不改退出码语义。

- [ ] **Step 3: 验证四个入口的手工行为**

```bash
cd yi-agent-rs
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:$PATH"

# L1 非法名 → 非 0 且列出可用 crate
just test-crate yi-agent-nope; echo "rc=$?"     # 期望 rc=1

# L2 在无改动时 → rc=1
just test-affected; echo "rc=$?"                # 期望 rc=1（若确有改动则正常跑）

# L2 纯文档基准 → rc=0 且报 no tests needed
just test-affected HEAD~1 2>&1 | head -3

# L0 可用
just check-fast; echo "rc=$?"
```

- [ ] **Step 4: 验证并发预检**

```bash
cd yi-agent-rs
# 另开一个 cargo 进程制造并发
(cargo nextest list -p yi-agent-flags >/dev/null 2>&1 &) ; sleep 1
bash scripts/test-tiers.sh concurrency; echo "rc=$?"   # 期望看到 WARN，rc=0
```

- [ ] **Step 5: 提交**

```bash
cd yi-agent-rs
git add justfile
git commit -m "feat(test-tiers): wire L0/L1/L2 recipes into justfile

check-fast / test-crate / test-affected are additive: no existing recipe
changes meaning. test gains exactly one leading line that warns about
other cargo processes and never blocks, so CI behaviour is unchanged."
```

---

### Task 6: 自测进 `just ci`

**Files:**
- Modify: `yi-agent-rs/justfile`（`ci:` 依赖行追加 `affected-check`；新增 `affected-check` recipe）

**Interfaces:**
- Consumes: Task 1–4 的 `scripts/test_test_tiers.sh`
- Produces: `just affected-check`

- [ ] **Step 1: 加 recipe**

在 `justfile` 的 `reclaim-check:` 附近（保持「xxx-check」聚集）插入：

```make
# test-tiers.sh 的判据自测（假 git 输出驱动，不编译、秒级）。
# 进 ci 的理由同 reclaim-check：判据写宽会把 desktop/ 当 crate 跑，
# 写窄会静默少测——两种都不报错，只能靠断言发现。
affected-check:
    bash scripts/test_test_tiers.sh
```

- [ ] **Step 2: 追加到 ci 依赖**

把 `ci:` 那一行末尾的 `audit` 之前或之后追加 `affected-check`。当前行尾是：

```
... reclaim-check audit
```

改成：

```
... reclaim-check affected-check audit
```

**不要**重排或删除任何既有依赖。

- [ ] **Step 3: 验证门禁单独可跑**

```bash
cd yi-agent-rs && just affected-check
```

预期：`23 passed`，退出码 0。

- [ ] **Step 4: 提交**

```bash
cd yi-agent-rs
git add justfile
git commit -m "ci: gate the test-tiers classification self-test

Same rationale as reclaim-check: a wrong path-to-crate predicate never
errors, it either runs the wrong crates or silently under-tests. Pure
bash assertions, about a second, no compilation."
```

---

### Task 7: 全量验收（独占运行）

**Files:**
- 无代码改动；产出验收记录

**Interfaces:**
- Consumes: 全部前置任务
- Produces: 验收结论

**前置条件：必须先独占。** 先确认没有其它 cargo 进程，否则测到的是外部争用：

```bash
ps ax -o pid=,stat=,command= | grep -E "cargo|rustc" | grep -v grep
```

若看到其它会话的 cargo 在跑，**等它结束**，不要在此基础上做验收。

- [ ] **Step 1: L1 基线全绿**

```bash
cd yi-agent-rs
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:$PATH"
/usr/bin/time -p just test-crate yi-agent-supervisors
```

预期：451 全绿，耗时与 spec 记录的 37 秒同量级。

- [ ] **Step 2: L2 闭包条数对得上**

```bash
cd yi-agent-rs
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:$PATH"
printf '\n// probe\n' >> crates/yi-agent-core/src/lib.rs
expr="$(bash scripts/test-tiers.sh affected HEAD)"
echo "$expr"
cargo nextest list --all-features -E "$expr" 2>/dev/null | cut -d: -f1 | cut -d' ' -f1 | sort -u
git checkout crates/yi-agent-core/src/lib.rs
```

预期：表达式含 `rdeps(yi-agent-core)`；列出的 crate 数 **12**。

- [ ] **Step 3: L2 反向验证（小闭包）**

```bash
cd yi-agent-rs
printf '\n// probe\n' >> crates/yi-agent-relay/src/lib.rs
expr="$(bash scripts/test-tiers.sh affected HEAD)"; echo "$expr"
cargo nextest list --all-features -E "$expr" 2>/dev/null | cut -d: -f1 | cut -d' ' -f1 | sort -u
git checkout crates/yi-agent-relay/src/lib.rs
```

预期：列出 **3** 个（`yi-agent-relay`、`yi-agent-app-server`、`yi-agent`）。

- [ ] **Step 4: L2 不滥编**

```bash
cd yi-agent-rs
printf '\n// probe\n' >> crates/yi-agent-flags/src/lib.rs
expr="$(bash scripts/test-tiers.sh affected HEAD)"
cargo nextest run --all-features --profile ci -E "$expr" 2>&1 | grep -E "^\s+Compiling"
git checkout crates/yi-agent-flags/src/lib.rs
```

预期：`Compiling` 只有 8 个闭包成员，**不含** `yi-agent-core` / `yi-agent-llm` / `yi-agent-mcp`。

- [ ] **Step 5: 不改既有配置**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/test-tiers
git diff main -- yi-agent-rs/.config/nextest.toml | head
```

预期：输出为空。

- [ ] **Step 6: 记录结论并提交**

把实际观测的数字写进 `docs/project-management/tooling.md`（Task 8 一并做）。

```bash
git add -A docs/project-management/tooling.md
git commit -m "docs: record graded test tier acceptance results"
```

---

### Task 8: 文档同步

**Files:**
- Modify: `docs/project-management/tooling.md`

**Interfaces:**
- Consumes: Task 7 的实测数字
- Produces: 无

- [ ] **Step 1: 读现有结构**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/test-tiers
cat docs/project-management/tooling.md
```

按文件既有格式（`[x]` / `[ ]` 两态，每条带可验证判据）追加一节。

- [ ] **Step 2: 追加条目**

在 `tooling.md` 末尾追加（如已有「测试」小节则并入其中）：

```markdown
## 分级测试入口（2026-11-07）

设计：`docs/superpowers/specs/2026-11-07-graded-test-tiers-design.md`
计划：`docs/superpowers/plans/2026-11-07-graded-test-tiers.md`

- [x] `just check-fast` —— L0，`cargo check --all-targets --all-features`。
      判据：`bash scripts/test-tiers.sh check`；在 `tests/` 放类型错误会报错。
- [x] `just test-crate <crate>` —— L1，单 crate 测试，带 `--all-features`。
      判据：`cargo nextest run --all-features -p <crate> --profile ci`。
- [x] `just test-affected [ref]` —— L2，按 git diff 推断 crate 再交 `rdeps()` 扩展。
      判据：改 `yi-agent-core` 时 `nextest list` 命中 12 个 crate；改 `yi-agent-relay` 命中 3 个。
- [x] `just test` 前置并发警告 —— 检测本机其它 cargo 进程，只警告不阻断。
      判据：另起一个 cargo 进程后跑 `bash scripts/test-tiers.sh concurrency` 会打印 WARN。
- [x] `just affected-check` —— 上述分类逻辑的自测，进 `just ci`。
      判据：`bash scripts/test_test_tiers.sh` 输出 `N passed` 且退出码 0。
- [ ] L3 全量（`just test`）本身未加速，仍是 13 分钟量级；本设计只增加入口，不改全量路径。
```

- [ ] **Step 3: 提交**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/test-tiers
git add docs/project-management/tooling.md
git commit -m "docs: record graded test tiers in tooling module"
```

---

### Task 9: 合并前自检

- [ ] **Step 1: 跑 fmt（预期无 Rust 变更）**

```bash
cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:$PATH" && cargo fmt --all -- --check
```

- [ ] **Step 2: 跑 ci**

```bash
cd yi-agent-rs && export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$HOME/.cargo/bin:$PATH" && just ci
```

若因并发导致超时类假红，先按 `scripts/test-tiers.sh concurrency` 的提示判断，独占后复跑该 crate。

- [ ] **Step 3: 确认没有碰另一条工作线的文件**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/test-tiers
git diff --name-only main | grep -E "Cargo.toml|check-msrv.sh|metadata_never_index" && echo "越界！" || echo "ok: 未触碰环境治理文件"
```

- [ ] **Step 4: 合并回 main**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
git checkout main && git merge --no-ff feat/graded-test-tiers
```

合并后按 `superpowers:finishing-a-development-branch` 清理 worktree。

---

## 附录：本次未做但已知的改进空间

以下都不在 spec 范围内，列出来避免后来者重复踩：

1. **跨会话假红仍未消除。** 本设计只让它可见，不修它。真要修得重新评估测试里写死的超时预算数值
   （spec 非目标明确排除），或给每个会话独立 `CARGO_TARGET_DIR`（属另一条工作线）。
2. **`pgrep -x` 无法判断对方是否用同一个 target。** 精确判断需要读 `.cargo-lock`，
   但该锁只在编译期持有，测试执行期为冒烟，会漏报（spec 已实测记录）。
3. **L0 在重载机器上仍可能几十分钟。** 期望值应是「比 L1 快」，不是「秒级」。
4. **`yi-agent-rs/justfile` 与另一条工作线的 `gc` recipe 可能冲突。** 两边都改同一个文件，
   后合者 rebase；本设计的改动集中在 `test:` 前后与 `ci:` 一行。
