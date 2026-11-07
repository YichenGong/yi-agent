> **执行状态：已完成（2026-11-07）。** 分支 `feat/reclaim-target-d1d4`，7 个提交。
> 最终自测 **49 passed**（plan 原文预期的 39 是估算，实际实现多覆盖了
> hyphens/underscore 错配、nlink 计数、--help 泄漏等 10 条）。
> `just ci` 除 `test` 里 3 个 **base 提交已存在**的失败外，其余 21 门禁全绿；
> 那 3 个已用纯净 base worktree 复现确认与本次改动无关。

# 编译产物磁盘占用治理 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** 让 `target/` 的增长有界——切断 `.o` 的累积源（profile），并给存量提供可验证的回收手段（脚本）。

**Architecture:** 两条腿，缺一不可。profile 改动阻止**新增**垃圾；脚本回收**已有**垃圾。只动 Cargo profile 配置、一个 shell 脚本与其自测、justfile、文档。不改任何 Rust 业务代码。

**Tech Stack:** Bash 3.2（macOS 自带）、just、cargo、`nm -pa`（LLVM nm，随 Xcode CLT）、python3。

**设计文档:** `docs/superpowers/specs/2026-11-07-build-disk-usage-design.md`（下文简称 spec）

## Global Constraints

- 工作必须在 worktree 里：`.worktrees/spec-build-disk`，分支 `docs/build-disk-usage-spec`。
- **不改**任何 `.rs` 文件、不改测试断言、不改 `.config/nextest.toml`。
- **不改** `just ci` 的既有门禁清单，`reclaim-check` 已在其中，不新增也不删除。
- 脚本用 `#!/usr/bin/env bash` + `set -uo pipefail`（**不要** `set -e`——要收集多个断言而非首个失败退出，与 `test_reclaim_target.sh` 一致）。
- 自测脚本尾部必须 `printf '\n%d passed\n' "$n"`，与本仓既有脚本一致。
- 脚本必须是**幂等**的：连跑两次第二次应为「无候选」。
- 涉及 `rm` 的代码路径默认 `dry-run`，必须显式 `--apply` 才删。
- Commit message 用 conventional commits，不写 `Co-Authored-By`。
- 完成后同步 `docs/project-management/tooling.md` 的条目（该条目现有数字已被本次实测推翻，见 Task 6）。

## 环境前置

`cargo` / `nm` 不在默认 PATH 时，每个需要它们的 shell 先执行：

```bash
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
```

本机无 `timeout`、无 `rg`。计时用 `/usr/bin/time -p`，搜索用 `grep -rE`。

## 关键事实（来自 spec，实现时不要重新推导）

| 事实 | 数值 | spec 位置 |
|---|---|---|
| `packed` 下全新 target 产出 `.o` 数 | **0** | §3 |
| `packed` 全新 target 总量 | 17.8 GiB、**0 个 `.o`**、131 个 dSYM（不累积） | §3 |
| `off` 全新 target 总量（**已否决**） | 9.9 GiB、0 个 `.o`、0 个 dSYM，但 **DWARF 函数数 = 0** | §4.2 |
| `debug=0` 全新 target 总量 | 6.6 GiB；函数符号 337,420 vs A 的 338,203 | §4.2 |
| 主仓库当前 `.o` | 约 26.8 万个、43.8 GiB，其中 nlink==1 的约 8.4 万个占 19.81 GiB | §2.1 |
| 主仓库 `incremental/` | 902 个单元目录、1,046 个会话、32.7 GiB | §2.2 |
| D1（单元内旧会话） | 144 个目录 / 23,922 个文件，零重编代价 | §4.1 |
| D1+D2 合并真实可释放 | **26.17 GiB**（唯一 inode 171,685，按 nlink==1） | §2.3 |
| nlink>1 的两类来源 | deps 内部重复 54,230 组；与 incremental 共享 18,332 个文件 | §2.1 |
| 删多余单元目录的代价 | 3.22 s vs 0.85 s（`yi_agent_core` 27→1） | §7.3 |
| OSO 引用与 mid mtime | 915 个引用里 906 个指向「非最新 mid」 | §4.1 |

## File Structure

| 文件 | 责任 | 动作 |
|---|---|---|
| `yi-agent-rs/Cargo.toml` | 取值**保持 `packed` 不变**；只替换 44 行注释里的错误断言 | 修改（仅注释） |
| `desktop/src-tauri/Cargo.toml` | 新增 `[profile.dev]`（桌面壳是独立 package，不继承主 workspace） | 修改 |
| `plugins/superpowers-kanban/Cargo.toml` | 新增 `[profile.dev]`（独立 workspace） | 修改 |
| `yi-agent-rs/scripts/reclaim-target.sh` | 扩充为 D1/D2/D3 三判据 | 修改 |
| `yi-agent-rs/scripts/test_reclaim_target.sh` | 8 条断言扩充到 39 条 | 修改 |
| `yi-agent-rs/justfile` | `reclaim` 改为变参透传 `*flags`；更新 22 行注释 | 修改 |
| `docs/project-management/tooling.md` | 重写「编译产物磁盘占用治理」条目 | 修改 |
| `CLAUDE.md` | 更新「`target/` 会无限膨胀」小节的实测依据 | 修改 |

**为什么 profile 要写三处：** 仓库有三个互不继承的 Cargo 构建单元（spec §4.3 实测）。只改主 workspace，`desktop/` 与 `plugins/` 的 `target/`（12.6 GiB / 1.16 GiB，各自仍有 7,044 / 9,699 个 `.o`）会继续累积。

**为什么逻辑在脚本而不在 justfile：** 判据写宽会删活跃产物、写窄等于没清理，两种错误都不报错。写在 python 里可以被 `test_reclaim_target.sh` 用假文件精确钉住。

---

### Task 1: 给 `reclaim-target.sh` 加 D1（incremental 旧会话）

**Files:**
- Modify: `yi-agent-rs/scripts/reclaim-target.sh`
- Modify: `yi-agent-rs/scripts/test_reclaim_target.sh`

**Interfaces:**
- Consumes: 既有 `scan_target` / `apply_clean` 的输出格式 `<count>\t<files>\t<bytes>\t<keys>\t<total_files>`
- Produces: 新增 `--skip-deps`（跳过 D2）、`--skip-incremental`（跳过 D1）；扫描输出追加两列
  `<d1_files>\t<d1_bytes>`。默认两者都执行。

- [x] **Step 1: 先写失败的测试**

在 `test_reclaim_target.sh` 的 `# 参数校验` 段**之前**插入：

```bash
# ---- D1: incremental 单元内的旧会话 ----
inc="$tmp/debug/incremental"
mkdir -p "$inc/alpha-1111/s-old" "$inc/alpha-1111/s-new" "$inc/beta-2222/s-only"
printf 'x%.0s' {1..3000} >"$inc/alpha-1111/s-old/obj.o"
printf 'x%.0s' {1..1000} >"$inc/alpha-1111/s-new/obj.o"
printf 'x%.0s' {1..500}  >"$inc/beta-2222/s-only/obj.o"
# 会话目录里还有 .lock，删目录时要一并带走且不报错
printf 'lock' >"$inc/alpha-1111/s-old/s-old.lock"
# 人为让 s-new 比 s-old 新（判据用 mtime）
touch -t 202601010000 "$inc/alpha-1111/s-old"
touch -t 202601020000 "$inc/alpha-1111/s-new"

out="$(bash "$SCRIPT" --target "$tmp")" || fail "dry-run exited nonzero"
grep -q "incremental 旧会话" <<<"$out" || { printf '%s\n' "$out" >&2; fail "dry-run does not report incremental"; }

bash "$SCRIPT" --apply --target "$tmp" >/dev/null || fail "--apply exited nonzero"

[ -d "$inc/alpha-1111/s-old" ] && fail "kept the stale session dir"
n=$((n + 1)); pass "D1 removes the stale session inside a unit"
[ -f "$inc/alpha-1111/s-new/obj.o" ] || fail "removed the newest session"
n=$((n + 1)); pass "D1 keeps the newest session in a unit"
[ -f "$inc/beta-2222/s-only/obj.o" ] || fail "removed a single-session unit"
n=$((n + 1)); pass "D1 leaves single-session units alone"

# --skip-incremental 时完全不碰
mkdir -p "$inc/alpha-1111/s-old2"; printf 'y' >"$inc/alpha-1111/s-old2/obj.o"
touch -t 202512010000 "$inc/alpha-1111/s-old2"
bash "$SCRIPT" --apply --target "$tmp" --skip-incremental >/dev/null || fail "--skip-incremental exited nonzero"
[ -d "$inc/alpha-1111/s-old2" ] || fail "--skip-incremental still deleted"
n=$((n + 1)); pass "D1 honours --skip-incremental"
```

- [x] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && bash scripts/test_reclaim_target.sh
```

预期：`FAIL: dry-run does not report incremental`（脚本还没有 D1）。

- [x] **Step 3: 实现 D1**

在 `reclaim-target.sh` 里新增两个函数，并接到主循环。

```bash
# D1：incremental/<crate>-<hash>/s-<id> —— 每个「编译单元目录」内只留 mtime
# 最新的一个会话，其余整目录删。
#
# 注意层级：<crate>-<hash> 是编译单元（hash 覆盖 feature 组合/依赖/profile），
# s-<id> 才是会话。只删会话、不动单元目录本身，因此零重编代价（spec §7.3）。
scan_incremental() { # $1 = target dir -> "files\tbytes"
  python3 - "$1" <<'PY'
import os, sys
root = sys.argv[1]
files = 0
size = 0
for prof in os.listdir(root):
    inc = os.path.join(root, prof, "incremental")
    if not os.path.isdir(inc):
        continue
    for unit in os.scandir(inc):
        if not unit.is_dir():
            continue
        sess = [s for s in os.scandir(unit.path)
                if s.is_dir() and s.name.startswith("s-")]
        if len(sess) < 2:
            continue
        sess.sort(key=lambda s: s.stat().st_mtime)
        for stale in sess[:-1]:
            for r, _ds, fs in os.walk(stale.path):
                for f in fs:
                    try:
                        files += 1
                        size += os.lstat(os.path.join(r, f)).st_size
                    except OSError:
                        pass
print(f"{files}\t{size}")
PY
}

apply_incremental() { # $1 = target dir -> "removed_dirs\tfreed_bytes"
  python3 - "$1" <<'PY'
import os, shutil, sys
root = sys.argv[1]
removed = 0
freed = 0
for prof in os.listdir(root):
    inc = os.path.join(root, prof, "incremental")
    if not os.path.isdir(inc):
        continue
    for unit in os.scandir(inc):
        if not unit.is_dir():
            continue
        sess = [s for s in os.scandir(unit.path)
                if s.is_dir() and s.name.startswith("s-")]
        if len(sess) < 2:
            continue
        sess.sort(key=lambda s: s.stat().st_mtime)
        for stale in sess[:-1]:
            size = 0
            for r, _ds, fs in os.walk(stale.path):
                for f in fs:
                    try:
                        size += os.lstat(os.path.join(r, f)).st_size
                    except OSError:
                        pass
            try:
                shutil.rmtree(stale.path)
                removed += 1
                freed += size
            except OSError:
                pass
print(f"{removed}\t{freed}")
PY
}
```

主循环里，扫描后追加两列；`--apply` 时先跑 D1 再跑 D2（顺序无关，但 D1 先能让 D2 的 nlink 统计更准）。

- [x] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && bash scripts/test_reclaim_target.sh
```

预期：**`12 passed`**（原有 8 条 + Task 1 新增 4 条）。

- [x] **Step 5: 提交**

```bash
git add yi-agent-rs/scripts/reclaim-target.sh yi-agent-rs/scripts/test_reclaim_target.sh
git commit -m "feat(build): reclaim stale incremental sessions inside each build unit"
```

---

### Task 2: 加 D2（N_OSO 引用集）替换「孤儿 key」单判据

**Files:**
- Modify: `yi-agent-rs/scripts/reclaim-target.sh`
- Modify: `yi-agent-rs/scripts/test_reclaim_target.sh`

**Interfaces:**
- Consumes: Task 1 的 `scan_target` / `apply_clean`
- Produces: 新增 `--skip-deps`；`scan_target` 的输出改为
  `<orphan_keys>\t<orphan_files>\t<orphan_bytes>\t<unref_files>\t<unref_bytes>\t<total_keys>\t<total_files>`

- [x] **Step 1: 先写失败的测试**

在 D1 段之后插入。用假 `nm` 桩注入引用集（**不依赖真实 cargo 产物**）：

**关键**：fixture 必须放在 `$tmp/debug/deps` 下。`scan_target` 只认
`<target>/<profile>/deps`（`debug` 优先），放在 `deps2`/`deps3` 里测试会
「通过」但其实什么都没扫到——这种假绿比没有测试更危险。

```bash
# ---- D2: N_OSO 引用集 ----
# 用独立的 target 根目录，避免和前面 D1 的 fixture 互相干扰。
d2="$tmp/t2/debug/deps"
mkdir -p "$d2"
# 活跃单元 foo：有 rlib，3 个 mid；只有 mid0 被二进制引用
for i in 0 1 2; do printf 'x%.0s' {1..1000} >"$d2/foo-aaaaaaaaaaaaaaaa.mid$i.0.rcgu.o"; done
printf 'r%.0s' {1..100} >"$d2/libfoo-aaaaaaaaaaaaaaaa.rlib"
# 孤儿单元 bar：无 rlib，应被整组删掉
printf 'y%.0s' {1..2000} >"$d2/bar-bbbbbbbbbbbbbbbb.mid0.0.rcgu.o"

# 一个可执行文件（名字不带 '.'，cargo 产物即如此）
: >"$d2/foo_bin-1234"; chmod +x "$d2/foo_bin-1234"

# 假 nm：只报告引用了 foo 的 mid0
stub="$tmp/stubbin"; mkdir -p "$stub"
cat >"$stub/nm" <<NMEOF
#!/bin/sh
echo "0000000100000000-0000000100000010 OSO $d2/foo-aaaaaaaaaaaaaaaa.mid0.0.rcgu.o"
NMEOF
chmod +x "$stub/nm"

out="$(PATH="$stub:$PATH" bash "$SCRIPT" --target "$tmp/t2")" || fail "dry-run exited nonzero"
grep -q "未被引用" <<<"$out" || { printf '%s\n' "$out" >&2; fail "dry-run does not report unreferenced .o"; }
n=$((n + 1)); pass "D2 reports unreferenced .o"

# 预演不删
[ "$(find "$d2" -name '*.rcgu.o' | wc -l | tr -d ' ')" = "4" ] || fail "dry-run deleted .o"
n=$((n + 1)); pass "D2 dry-run deletes nothing"

PATH="$stub:$PATH" bash "$SCRIPT" --apply --target "$tmp/t2" >/dev/null || fail "D2 --apply failed"
[ -f "$d2/foo-aaaaaaaaaaaaaaaa.mid0.0.rcgu.o" ] || fail "deleted the OSO-referenced .o"
n=$((n + 1)); pass "D2 keeps OSO-referenced .o"
[ -f "$d2/foo-aaaaaaaaaaaaaaaa.mid1.0.rcgu.o" ] && fail "kept an unreferenced .o in a live unit"
n=$((n + 1)); pass "D2 removes unreferenced .o in a live unit"
[ -f "$d2/bar-bbbbbbbbbbbbbbbb.mid0.0.rcgu.o" ] && fail "kept an orphan-key .o"
n=$((n + 1)); pass "D2 removes orphan-key .o"
[ -f "$d2/libfoo-aaaaaaaaaaaaaaaa.rlib" ] || fail "deleted an rlib"
n=$((n + 1)); pass "D2 leaves rlib alone"

# 幂等
PATH="$stub:$PATH" bash "$SCRIPT" --apply --target "$tmp/t2" >/dev/null || fail "2nd apply failed"
n=$((n + 1)); pass "D2 idempotent"

# 没有任何可执行文件时必须退化为只清孤儿 key，不误删活跃单元
d3="$tmp/t3/debug/deps"; mkdir -p "$d3"
printf 'a' >"$d3/solo-bbbbbbbbbbbbbbbb.mid0.0.rcgu.o"
printf 'b' >"$d3/libsolo-bbbbbbbbbbbbbbbb.rlib"
printf 'c' >"$d3/orphan-dddddddddddddddd.mid0.0.rcgu.o"
bash "$SCRIPT" --apply --target "$tmp/t3" >/dev/null || fail "no-binary case failed"
[ -f "$d3/solo-bbbbbbbbbbbbbbbb.mid0.0.rcgu.o" ] || fail "no-binary fallback deleted a live unit"
n=$((n + 1)); pass "D2 falls back to orphan-key-only when no binaries exist"
[ -f "$d3/orphan-dddddddddddddddd.mid0.0.rcgu.o" ] && fail "fallback did not clean orphan key"
n=$((n + 1)); pass "D2 fallback still cleans orphan keys"

# --skip-deps：完全不碰 deps
d4="$tmp/t4/debug/deps"; mkdir -p "$d4"
printf 'c' >"$d4/orphan-eeeeeeeeeeeeeeee.mid0.0.rcgu.o"
bash "$SCRIPT" --apply --target "$tmp/t4" --skip-deps >/dev/null || fail "--skip-deps failed"
[ -f "$d4/orphan-eeeeeeeeeeeeeeee.mid0.0.rcgu.o" ] || fail "--skip-deps still cleaned deps"
n=$((n + 1)); pass "D2 honours --skip-deps"

# ---- 引用集零交集：必须整体放弃引用判据（这是 47 GB 误删的唯一防线）----
# 真实场景：split-debuginfo 从 unpacked 切到 packed 后，存量的 unpacked .o
# 与新一代二进制的 N_OSO 名字格式完全不同，交集为 0。此时若照用引用判据，
# 会把 100% 的 .o 判成"未被引用"（实测 268,571/268,571）。
t8="$tmp/t8/debug/deps"; mkdir -p "$t8"
printf 'r' >"$t8/livc-1111111111111111.rlib"
printf '%.0sA' {1..100} >"$t8/livc-1111111111111111.mid0.0.rcgu.o"
: >"$t8/livc_bin-8888"; chmod +x "$t8/livc_bin-8888"
# 桩 nm：返回一个完全不存在于磁盘的名字 —— 精确模拟跨代际零交集
mkdir -p "$tmp/nmstub-zero"
cat >"$tmp/nmstub-zero/nm" <<'NM'
#!/bin/sh
echo "0000-1111 OSO /nowhere/ghost-9999999999999999.mid9.9.rcgu.o"
NM
chmod +x "$tmp/nmstub-zero/nm"
PATH="$tmp/nmstub-zero:$PATH" bash "$SCRIPT" --apply --target "$tmp/t8" >/dev/null \
  || fail "zero-overlap case failed"
[ -f "$t8/livc-1111111111111111.mid0.0.rcgu.o" ] \
  || fail "zero-overlap deleted a live .o (this is the 47GB failure mode)"
n=$((n + 1)); pass "D2 abandons N_OSO when reference set does not intersect disk"
outz="$(PATH="$tmp/nmstub-zero:$PATH" bash "$SCRIPT" --target "$tmp/t8")" \
  || fail "zero-overlap dry-run failed"
grep -qiE "跳过|skip|0 命中|no overlap" <<<"$outz" \
  || { printf '%s\n' "$outz" >&2; fail "zero-overlap not explained in output"; }
n=$((n + 1)); pass "D2 explains why N_OSO was skipped"

# 硬链接：文件照删（另半边可能是 incremental 会话），但 nlink>1 时不计入释放量
d5="$tmp/t5/debug/deps"; mkdir -p "$d5"
# 同一 key 的两个名字共享 inode —— 必须让它们落在「活跃单元 + 未被引用」这一
# 分支，否则测的是孤儿分支，`nlink` 的计数逻辑根本没被执行到。
printf 'z%.0s' {1..4000} >"$d5/lnk-ffffffffffffffff.mid0.0.rcgu.o"
printf 'r' >"$d5/liblnk-ffffffffffffffff.rlib"
ln "$d5/lnk-ffffffffffffffff.mid0.0.rcgu.o" "$d5/lnk-ffffffffffffffff.mid0.1.rcgu.o"
ln "$d5/lnk-ffffffffffffffff.mid0.0.rcgu.o" "$tmp/hardlinked_elsewhere"
# 必须有一个可执行文件，否则 bins==0，走不到「未引用」分支
: >"$d5/lnk_bin-9999"; chmod +x "$d5/lnk_bin-9999"
# 预演必须报出来（表观），但「真实可释放」应为 0
out="$(bash "$SCRIPT" --target "$tmp/t5")" || fail "hardlink dry-run failed"
grep -q "lnk-ffffffffffffffff" <<<"$out" || { printf '%s\n' "$out" >&2; fail "hardlink .o not reported at all"; }
n=$((n + 1)); pass "D2 reports hardlinked .o"
# apply 后：名字被删掉（这样才能配合 D1 释放空间）
bash "$SCRIPT" --apply --target "$tmp/t5" >/dev/null || fail "hardlink apply failed"
[ -e "$d5/lnk-ffffffffffffffff.mid0.0.rcgu.o" ] && fail "hardlinked .o was not deleted"
[ -f "$tmp/hardlinked_elsewhere" ] || fail "hardlink outside target was removed"
n=$((n + 1)); pass "D2 deletes a hardlinked .o but spares its external link"
```

- [x] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && bash scripts/test_reclaim_target.sh
```

预期失败在 `D2 reports unreferenced .o`。

- [x] **Step 3: 实现 D2**

**性能事实（实测，写代码前先知道）**：`nm -pa` 扫 deps 下全部 456 个可执行
文件要 **77.6 秒**。这不是 bug，是必然成本（漏扫一个二进制就会误删它的
`.o`）。实现时必须：

1. 扫描前先打印 `正在扫描 N 个二进制（约 N×0.17 秒）…`，否则用户以为卡死；
2. 不要用 `timeout=120` 这种「单个二进制超时」掩盖问题——单个二进制 `nm`
   通常 < 1 秒，超时说明文件损坏，跳过并记数即可；
3. `--dry-run` 也要扫（否则报不出候选），所以预演同样是 ~80 秒。


```bash
# D2：deps/ 下未被任何可执行二进制「引用」的 .o。
#
# 判据为什么是 N_OSO 而不是 mtime（spec §4.1 实测）：
#   一次编译会在同一秒写出该 key 的所有 mid，mtime 完全相同，无法排序。实测
#   915 个 OSO 引用里 906 个指向「非最新 mid」——按 mtime 取舍会删掉当前
#   二进制正在用的调试信息。
#
# 引用集来源：对 deps/ 下每个可执行文件跑 `nm -pa`，收集所有 `OSO` 行指向的
# .o basename。deps 里没有可执行文件时无法构造引用集，退化为只清孤儿 key。
scan_deps() { # $1 = deps dir -> "orphan_keys\torphan_files\torphan_bytes\tunref_files\tunref_bytes\ttotal_keys\ttotal_files"
  python3 - "$1" <<'PY'
import os, subprocess, sys
d = sys.argv[1]
rlibs, objs = set(), {}
with os.scandir(d) as it:
    for e in it:
        if not e.is_file(follow_symlinks=False):
            continue
        if e.name.endswith(".rlib"):
            base = e.name[:-5]
            rlibs.add(base[3:] if base.startswith("lib") else base)
        elif e.name.endswith(".rcgu.o"):
            try:
                st = e.stat(follow_symlinks=False)
                objs[e.name] = st.st_size
            except OSError:
                pass

# collect referenced basenames from every executable in deps/
referenced = set()
bins = 0
with os.scandir(d) as it:
    for e in it:
        if not e.is_file(follow_symlinks=False):
            continue
        if "." in e.name:          # cargo 产物名不含 '.'，带点的都不是二进制
            continue
        if not os.access(e.path, os.X_OK):
            continue
        bins += 1
        try:
            out = subprocess.run(["nm", "-pa", e.path],
                                 capture_output=True, text=True,
                                 errors="replace", timeout=120).stdout
        except (OSError, subprocess.SubprocessError):
            continue
        for line in out.splitlines():
            if "OSO" in line:
                referenced.add(os.path.basename(line.split()[-1]))

# ---- 代际自检：引用集与磁盘 .o 零交集时必须放弃引用判据 ----
# 为什么必须有这一步（spec §4.1 的致命前提）：split-debuginfo 从 unpacked
# 切到 packed/off 之后，新一代二进制记录的 N_OSO 名字是上一代 unpacked 的
# 临时路径，与磁盘上两代 .o 的命名格式都对不上。实测真实仓库：
#   取样 30 个最新二进制 -> OSO 名字 13,912 条
#   与 268,571 个磁盘 .o 的 basename 交集 = 0
# 若照用引用判据，会把 100% 的 .o 判成"未被引用"，一次 reclaim 删掉 47 GB。
_objs_on_disk = {k for k in objs}          # objs: basename -> stat，见上
_overlap = len(referenced & _objs_on_disk)
use_ref = bool(referenced) and (_overlap > 0 or not _objs_on_disk)
if referenced and _objs_on_disk and _overlap == 0:
    print(f"  deps 引用判据 : 已跳过"
          f"（N_OSO 与磁盘 .o 无交集 0/{len(_objs_on_disk)}，"
          f"通常是 split-debuginfo 切换前的存量）", file=sys.stderr)

orphan_keys = orphan_files = orphan_bytes = 0
by_key = {}
for name, size in objs.items():
    by_key.setdefault(name.split(".")[0], []).append((name, size))

for key, items in by_key.items():
    if key not in rlibs:
        orphan_keys += 1
        orphan_files += len(items)
        orphan_bytes += sum(s for _n, s in items)

# unreferenced only makes sense for keys that still have an rlib,
# AND only when the reference set is trustworthy (use_ref)
unref_files = unref_bytes = 0
if bins and use_ref:          # <- use_ref 不能省，见上面的代际自检
    for key, items in by_key.items():
        if key not in rlibs:
            continue          # already counted as orphan
        for name, size in items:
            if name not in referenced:
                unref_files += 1
                unref_bytes += size

print(f"{orphan_keys}\t{orphan_files}\t{orphan_bytes}\t"
      f"{unref_files}\t{unref_bytes}\t{len(by_key)}\t{len(objs)}")
PY
}
```

**`apply_clean` 里必须重算 `bins`（不能只靠扫描结果）**。这是实现原型时踩到
的坑：扫描阶段 `unref_files` 在 `bins == 0` 时正确地保持 0，但如果 `apply_clean`
无条件套用「不在引用集里就删」，那么没有二进制时引用集为空，**活跃单元的所有
`.o` 都会被当成未引用删掉**——一次 `just reclaim` 就能让下一次构建从零开始。

```python
bins = 0
for e in os.scandir(d):
    if e.is_file(follow_symlinks=False) and "." not in e.name \
       and os.access(e.path, os.X_OK):
        bins += 1

for e in os.scandir(d):
    if not e.name.endswith(".rcgu.o"):
        continue
    key = e.name.split(".")[0]
    if bins == 0:
        victim = key not in rlibs          # 只清孤儿，活跃 key 一律保留
    else:
        victim = key not in rlibs or e.name not in referenced
```

`apply_clean` 同步改为只删「孤儿 key 的全部 `.o`」**加上**「活跃 key 里未被引用的 `.o`」。

**`st_nlink` 的用法必须精确**（很容易写错，写错会让收益凭空少 10 GiB）：

- **删除**：nlink > 1 的文件**照删**。它与 `incremental/` 的会话文件是同一
  inode；D1 已经删掉那些会话，此时删 deps 这半边才真正释放空间。
- **计数**：只有 `st_nlink == 1` 才计入「将释放」字节数。理由：若另一处链接
  仍在（比如 D1 没跑、或该会话属于我们保留的最新会话），删掉这个名字不释放
  任何空间，计进去就是虚报。

实测依据（spec §2.3）：D2 单独删时 nlink==1 只有 19.81 GiB；D1+D2 一起删，
按唯一 inode 统计才有 26.17 GiB。**如果为了「安全」跳过 nlink>1 的文件，
这 6.36 GiB 差额永远拿不到。**

```python
# 收集 victims（use_ref 是上一段代际自检算出来的；零交集时为 False，
# 此时一个活跃 .o 都不收，只收孤儿 key 的全部 .o）
victims = []
for e in os.scandir(d):
    if not e.name.endswith(".rcgu.o"):
        continue
    st = e.stat(follow_symlinks=False)
    key = e.name.split(".")[0]
    if key not in rlibs:
        victims.append((e.path, st.st_size, st.st_nlink))     # 孤儿：无条件
    elif use_ref and e.name not in referenced:
        victims.append((e.path, st.st_size, st.st_nlink))     # 活跃但未被引用
    # bins == 0 或 use_ref == False 时，活跃 key 一律保留

freed = 0
for path, size, nlink in victims:
    try:
        os.unlink(path)          # 删除：无条件删（nlink>1 也删）
    except OSError:
        continue
    if nlink == 1:               # 计数：只有 nlink==1 才真正释放空间
        freed += size
```

**注意执行顺序**：必须先 D1 再 D2。D1 删掉旧会话后，原本 nlink==2 的 deps
`.o` 变成 nlink==1，D2 统计才能体现真实收益。反过来则先删 deps、会话还在，
统计出来是虚的（虽然最终空间一样）。

- [x] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && bash scripts/test_reclaim_target.sh
```

预期：**`26 passed`**（12 + Task 2 新增 14 条）。

- [x] **Step 5: 在真实主仓库上试跑（dry-run，必须与 spec 数字同量级）**

```bash
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cd yi-agent-rs && bash scripts/reclaim-target.sh 2>&1 | tail -12
```

预期：`deps 未被引用` 一栏约 **267,500 个文件 / 约 43.8 GiB 表观**；`incremental 旧会话` 约 23,900 个文件 / 约 8.5 GiB。

**这个数会随每次构建变化**（`.o` 在 `packed` 下其实不再新增，但测试二进制重链
会改写引用的 mid），所以判据是「量级」而不是精确值：

- `deps` 未被引用：20 万 ~ 30 万之间即正常，**差一个数量级**（比如只有 1 万个）
  说明引用集构造错了（多半是 `nm` 没拿到全部二进制），停下来查。
- `incremental 旧会话`：1 万 ~ 3 万文件即正常。

**另一个必须核对的现象**：被引用集合里有 **83,162 个 basename 在 deps 里根本
不存在**（实测 84,200 个引用名，只有 1,038 个能对上文件）。这些是**旧二进制**
留下的 debug map 指向早已被删的 `.o`。它们不影响判据正确性（只会让更多
`.o` 被判为「未引用」），但如果 `referenced` 数目远小于预期，说明 `nm` 扫的
二进制太少了。

- [x] **Step 6: 提交**

```bash
git add yi-agent-rs/scripts/reclaim-target.sh yi-agent-rs/scripts/test_reclaim_target.sh
git commit -m "feat(build): pick deps/.o by N_OSO reference set, not mtime"
```

---

### Task 3: 加 D3（`--prune-units=N`，默认关闭）

**Files:**
- Modify: `yi-agent-rs/scripts/reclaim-target.sh`
- Modify: `yi-agent-rs/scripts/test_reclaim_target.sh`

**Interfaces:**
- Consumes: 无
- Produces: `--prune-units N`（缺省 0 = 不执行）；N>0 时每个 crate 前缀保留最新的 N 个单元目录

- [x] **Step 1: 先写失败的测试**

```bash
# ---- D3: --prune-units=N ----
inc="$tmp/debug/incremental"
mkdir -p "$inc/gamma-1111/s-a" "$inc/gamma-2222/s-b" "$inc/gamma-3333/s-c"
printf 'x' >"$inc/gamma-1111/s-a/o"; printf 'y' >"$inc/gamma-2222/s-b/o"; printf 'z' >"$inc/gamma-3333/s-c/o"
touch -t 202601010000 "$inc/gamma-1111"; touch -t 202601020000 "$inc/gamma-2222"; touch -t 202601030000 "$inc/gamma-3333"

# 默认不执行：三个都还在
bash "$SCRIPT" --apply --target "$tmp" >/dev/null || fail "default run failed"
[ -d "$inc/gamma-1111" ] || fail "pruned units without --prune-units"
n=$((n + 1)); pass "D3 is off by default"

bash "$SCRIPT" --apply --target "$tmp" --prune-units 1 >/dev/null || fail "--prune-units 1 failed"
[ -d "$inc/gamma-3333" ] || fail "pruned the newest unit"
n=$((n + 1)); pass "D3 keeps the newest N units"
[ -d "$inc/gamma-1111" ] && fail "kept an old unit when N=1"
n=$((n + 1)); pass "D3 prunes units beyond N"
```

- [x] **Step 2: 跑测试确认失败**
- [x] **Step 3: 实现 D3**（`--prune-units` 吃一个整数参数，校验 `N >= 0`；`N=0` 直接跳过）
- [x] **Step 4: 跑测试确认通过**（实际 **38 passed**：含 Task 3b 的 6 条护栏断言与 1 条非数字校验）
- [x] **Step 5: 提交**

```bash
git commit -m "feat(build): add opt-in --prune-units for surplus build unit dirs"
```

---

### Task 3b: 并发护栏（`--min-age` 与构建锁检测）

**Files:**
- Modify: `yi-agent-rs/scripts/reclaim-target.sh`
- Modify: `yi-agent-rs/scripts/test_reclaim_target.sh`

**为什么必须有**：删除期间若有另一个 `cargo` 正在编译，它会刚写出 `.o` 还没
链接，被我们删掉就得到 `ld: can't open file` 或玄学重编。这是唯一一种「清理
脚本导致构建失败」的路径。

- [x] **Step 1: 先写失败的测试**

```bash
# ---- 并发护栏 ----
m="$tmp/t6/debug/deps"; mkdir -p "$m"
printf 'x%.0s' {1..500} >"$m/fresh-aaaaaaaaaaaaaaaa.mid0.0.rcgu.o"
printf 'y%.0s' {1..500} >"$m/old-bbbbbbbbbbbbbbbb.mid0.0.rcgu.o"
# old 设成 2 小时前，fresh 保持 mtime = 现在
touch -t 202601010000 "$m/old-bbbbbbbbbbbbbbbb.mid0.0.rcgu.o"

bash "$SCRIPT" --apply --target "$tmp/t6" --min-age 600 >/dev/null || fail "--min-age failed"
[ -f "$m/fresh-aaaaaaaaaaaaaaaa.mid0.0.rcgu.o" ] || fail "--min-age did not protect a fresh file"
n=$((n + 1)); pass "--min-age protects files newer than the threshold"
[ -f "$m/old-bbbbbbbbbbbbbbbb.mid0.0.rcgu.o" ] && fail "--min-age skipped an old file too"
n=$((n + 1)); pass "--min-age still deletes files older than the threshold"

# --min-age 0 解除保护
bash "$SCRIPT" --apply --target "$tmp/t6" --min-age 0 >/dev/null || fail "--min-age 0 failed"
[ -f "$m/fresh-aaaaaaaaaaaaaaaa.mid0.0.rcgu.o" ] && fail "--min-age 0 still protected"
n=$((n + 1)); pass "--min-age 0 disables the protection"

# 检测到构建锁被持有时默认拒绝
l="$tmp/t7/debug"; mkdir -p "$l/deps"
printf 'x' >"$l/deps/orphan-cccccccccccccccc.mid0.0.rcgu.o"
touch -t 202601010000 "$l/deps/orphan-cccccccccccccccc.mid0.0.rcgu.o"
: >"$l/.cargo-build-lock"
# 用假 lsof 模拟「锁被持有」
stub2="$tmp/stub2"; mkdir -p "$stub2"
printf '#!/bin/sh\necho "COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME"\necho "cargo 123 user 3w REG 1,2 0 1 $1"\n' >"$stub2/lsof"
chmod +x "$stub2/lsof"
PATH="$stub2:$PATH" bash "$SCRIPT" --apply --target "$tmp/t7" --min-age 0 >/dev/null 2>&1 \
  && fail "did not refuse while the build lock was held"
n=$((n + 1)); pass "refuses to run while a cargo build lock is held"
PATH="$stub2:$PATH" bash "$SCRIPT" --apply --target "$tmp/t7" --min-age 0 --force >/dev/null \
  || fail "--force did not override the lock check"
n=$((n + 1)); pass "--force overrides the lock check"

# --dry-run 不受锁检测影响（只读，没必要拦）
PATH="$stub2:$PATH" bash "$SCRIPT" --target "$tmp/t7" --min-age 0 >/dev/null \
  || fail "dry-run was refused by the lock check"
n=$((n + 1)); pass "dry-run ignores the lock check"
```

- [x] **Step 2: 跑测试确认失败**
- [x] **Step 3: 实现**

```bash
# --min-age N：跳过 mtime 在 N 秒以内的候选（默认 600）
MIN_AGE=600
FORCE=0

# 锁检测：<target>/<profile>/.cargo-build-lock 被 lsof 命中即认为有编译在跑。
# 注意文件名：实测存在 .cargo-build-lock 与 .cargo-lock 两个，前者才是编译期
# 持有的那个；后者是 cargo 的目录级 informational lock。
has_active_build() { # $1 = target dir
  local f
  for f in "$1"/*/.cargo-build-lock; do
    [ -e "$f" ] || continue
    if lsof "$f" >/dev/null 2>&1; then return 0; fi
  done
  return 1
}
```

`--apply` 且非 `--force` 且 `has_active_build` 为真时，打印原因并 `exit 1`。
python 侧按 `--min-age` 过滤候选：`if time.time() - st.st_mtime < min_age: skip`。

- [x] **Step 4: 跑测试确认通过**（实际并入 Task 3 一并验证，见上）
- [x] **Step 5: 提交**

```bash
git commit -m "feat(build): guard reclaim against concurrent builds"
```
- [x] **Step 5: 提交**

```bash
git commit -m "feat(build): guard reclaim against concurrent builds"
```

---

### Task 3c: 加 D4（陈旧二进制 + dSYM，**实测 30 GB，此前遗漏**）

**为什么加这个**：前三档只清 `.o` 和 `incremental`，实测 `deps` 里还有两块
更大的陈旧产物（spec §2.3b）：

```
dSYM            339 个  27.56 GB   其中陈旧 211 个 = 19.85 GB
二进制          574 个  12.95 GB   其中陈旧 449 个 = 10.60 GB
```

**判据（按 crate + 目标类型分组，不能只用 crate 前缀）**：

反例（实测，写错就误删）：`plugin_pack` 有两个哈希，mtime 只差 0.338 秒，
但它们分别是 **`bin-plugin-pack`** 和 **`test-bin-plugin-pack`** 两个不同
目标，都必须保留。所以分组键 = `(crate 前缀, fingerprint 声明的目标类型)`。

目标类型从 `target/debug/.fingerprint/<pkg>-<hash>/*.json` 的**文件名**读
（`bin-*` / `test-bin-*` / `test-lib-*` / `lib-*` / `test-integration-test-*`）。
组内保留 **mtime 最新**的一个，其余连同同名 `.dSYM` 一起删。

- [x] **Step 1: 先写失败的测试**

```bash
# ---- D4: 陈旧二进制 + dSYM ----
d9="$tmp/t9/debug/deps"; mkdir -p "$d9"
fpdir="$tmp/t9/debug/.fingerprint"
# 同一 crate、同一目标(bin) 的两个 hash：旧的应删、新的应留
mkdir -p "$fpdir/demo-aaaaaaaaaaaaaaaa" "$fpdir/demo-bbbbbbbbbbbbbbbb"
: >"$fpdir/demo-aaaaaaaaaaaaaaaa/bin-demo.json"
: >"$fpdir/demo-bbbbbbbbbbbbbbbb/bin-demo.json"
printf 'old' >"$d9/demo-aaaaaaaaaaaaaaaa"; chmod +x "$d9/demo-aaaaaaaaaaaaaaaa"
printf 'new' >"$d9/demo-bbbbbbbbbbbbbbbb"; chmod +x "$d9/demo-bbbbbbbbbbbbbbbb"
mkdir -p "$d9/demo-aaaaaaaaaaaaaaaa.dSYM/Contents/Resources/DWARF"
printf 'dsym-old' >"$d9/demo-aaaaaaaaaaaaaaaa.dSYM/Contents/Resources/DWARF/demo"
touch -t 202601010000 "$d9/demo-aaaaaaaaaaaaaaaa" "$d9/demo-aaaaaaaaaaaaaaaa.dSYM"
touch -t 202601020000 "$d9/demo-bbbbbbbbbbbbbbbb"

# 同一 crate、不同目标(bin vs test-bin)：mtime 接近，两个都必须留
mkdir -p "$fpdir/multi-cccccccccccccccc" "$fpdir/multi-dddddddddddddddd"
: >"$fpdir/multi-cccccccccccccccc/bin-multi.json"
: >"$fpdir/multi-dddddddddddddddd/test-bin-multi.json"
printf 'a' >"$d9/multi-cccccccccccccccc"; chmod +x "$d9/multi-cccccccccccccccc"
printf 'b' >"$d9/multi-dddddddddddddddd"; chmod +x "$d9/multi-dddddddddddddddd"
touch -t 202601030000 "$d9/multi-cccccccccccccccc"
touch -t 202601030000 "$d9/multi-dddddddddddddddd"

bash "$SCRIPT" --apply --target "$tmp/t9" >/dev/null || fail "D4 apply failed"
[ -e "$d9/demo-aaaaaaaaaaaaaaaa" ] && fail "D4 kept a stale binary"
n=$((n + 1)); pass "D4 deletes a stale binary (same crate + same target)"
[ -e "$d9/demo-aaaaaaaaaaaaaaaa.dSYM" ] && fail "D4 kept the stale dSYM"
n=$((n + 1)); pass "D4 deletes the stale binary's dSYM"
[ -x "$d9/demo-bbbbbbbbbbbbbbbb" ] || fail "D4 deleted the newest binary"
n=$((n + 1)); pass "D4 keeps the newest binary"
[ -x "$d9/multi-cccccccccccccccc" ] && [ -x "$d9/multi-dddddddddddddddd" ] \
  || fail "D4 confused two different targets of the same crate"
n=$((n + 1)); pass "D4 keeps bin and test-bin of the same crate separately"
```

- [x] **Step 2: 跑测试确认失败**

- [x] **Step 3: 实现**

```python
# D4：陈旧二进制 + 同名 .dSYM
# 分组键必须是 (crate, 目标类型)：只用 crate 前缀会把 bin 和 test-bin 当一个组，
# 实测 plugin_pack 的两个哈希 mtime 只差 0.338s，按最新删就会误删其中一个。
def _target_kind(fp_root, h):
    import glob as _glob
    kinds = set()
    for j in _glob.glob(os.path.join(fp_root, "*" + h, "*.json")):
        kinds.add(os.path.basename(j)[:-len(".json")])
    return "|".join(sorted(kinds)) if kinds else ""

groups = {}
for e in os.scandir(d):
    if not e.is_file(follow_symlinks=False):
        continue
    if "." in e.name or not os.access(e.path, os.X_OK):
        continue
    if "-" not in e.name:
        continue
    pre, h = e.name.rsplit("-", 1)
    if len(h) != 16:
        continue
    key = (pre, _target_kind(os.path.join(target, "debug", ".fingerprint"), h))
    groups.setdefault(key, []).append(e)

for key, items in groups.items():
    if len(items) < 2:
        continue
    items.sort(key=lambda x: -x.stat().st_mtime)
    for e in items[1:]:
        victims.append((e.path, e.stat(follow_symlinks=False).st_size, 1, "bin"))
        ds = e.path + ".dSYM"
        if os.path.isdir(ds):
            victims_tree.append((ds, _tree_size(ds), "dsym"))
```

删除 dSYM 是**整目录递归删**（里面是 `Contents/Resources/DWARF/...`），
与删普通文件走不同分支；计数用递归求和。

- [x] **Step 4: 跑测试确认通过**（预期 **`39 passed`**：35 + Task 3c 新增 4 条）

- [x] **Step 5: 提交**

```bash
git commit -m "feat(build): reclaim stale binaries and their dSYM"
```

---

### Task 4: 修正 `yi-agent-rs/Cargo.toml` 的注释（**取值不动**）

**决策已变**：保留现有的 `split-debuginfo = "packed"`，**不改配置**
（理由见 spec §4.2「为什么推翻 A」）。Task 4 只剩一件事：把那段 44 行注释里
被实测推翻的断言改掉，否则下一个人会照错误结论去改配置。

**Files:**
- Modify: `yi-agent-rs/Cargo.toml:85-125`（只改注释）

- [x] **Step 1: 替换注释里四处被实测推翻的断言**

| 现有断言 | 实测事实 |
|---|---|
| 「`packed` 单次体积只省 4%」 | 单次是 **更费**（17.8 G vs 默认 10.4 G），省的是「不再累积」 |
| 「默认 `cargo build` 462 个 `.o`」 | 统计口径错误；全量是 18,024 个 |
| 「`.o` 最大 53 MB，37 MB 是 `__DWARF`」 | 平均 172 KiB、中位 40 KiB，53 MB 是离群值 |
| 「`packed` 保留完整 DWARF，不降级调试」 | 准确，但要补代价：`.dSYM` 占 27.5 GB，且旧 hash 的 dSYM 会累积 |

- [x] **Step 2: 补一句「为什么选 packed 而不是 off」**

注释末尾加：

```
# 为什么不再考虑 split-debuginfo = "off"（2026-11-07 复核）：
# off 能让单次构建再降到 9.9 GiB，但实测 off 下 DWARF 函数数为 0，
# lldb 只能看地址、看不了变量。而"无限累积"这个真痛点已由 packed 解决
# （全新 target 下 cargo build / cargo test 均产 0 个 .o，反复重编也恒为 0）。
# 用永久的排查能力换一次性 8 GiB，不划算。
```

- [x] **Step 3: 确认没有改到取值**

```bash
cd yi-agent-rs && grep -A1 '^\[profile.dev\]' Cargo.toml
```

预期仍是 `split-debuginfo = "packed"`。**本次提交不改变任何编译行为**，
所以不需要重新构建验证。

- [x] **Step 4: 提交**

```bash
git add yi-agent-rs/Cargo.toml
git commit -m "docs(build): correct the profile comment with measured numbers"
```

---

### Task 5: 给 `desktop/` 与 `plugins/` 补 `packed`（它们现在还是默认 `unpacked`）

**这是 Task 5 的新内容**：原计划是去改成 `off`，现在改为**补上 `packed`**。
实测这两个构建单元**都没有设置** `split-debuginfo`，还在用 macOS 默认的
`unpacked`，因此**仍在累积 `.o`**：

```
desktop/src-tauri/target          : 12 GB, 7,044 个 .o
plugins/superpowers-kanban/target : 1.1 GB, 9,699 个 .o
```

**Files:**
- Modify: `desktop/src-tauri/Cargo.toml`
- Modify: `plugins/superpowers-kanban/Cargo.toml`

- [x] **Step 1: 两个文件各追加一段**

```toml
# 与 yi-agent-rs/Cargo.toml 的 [profile.dev] 保持一致。这是独立构建单元
# （desktop 是独立 package、plugins 是独立 workspace），不继承主 workspace，
# 必须在这里各自声明，否则它的 target/ 继续累积 *.rcgu.o。
# 实测补之前：desktop 7,044 个 .o / 12 GB；plugins 9,699 个 .o / 1.1 GB。
[profile.dev]
split-debuginfo = "packed"
```

- [x] **Step 2: 验证 plugins 生效**

```bash
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cd plugins/superpowers-kanban && rm -rf /tmp/pcheck && CARGO_TARGET_DIR=/tmp/pcheck \
  cargo build --offline -v 2>&1 | grep -o "split-debuginfo=[a-z]*" | sort -u
find /tmp/pcheck -name '*.rcgu.o' | wc -l      # 期望 0
rm -rf /tmp/pcheck
```

`desktop/` 用 `cargo metadata` 确认 profile 即可，不必整包构建（tauri 很慢）：

```bash
cd desktop/src-tauri && cargo metadata --format-version 1 --no-deps \
  | python3 -c "import json,sys; d=json.load(sys.stdin); print([p.get('profile') for p in d['packages']])"
```

- [x] **Step 3: 提交**

```bash
git commit -m "perf(build): set split-debuginfo=packed on desktop and plugin crates"
```

---

### Task 6: justfile 参数 + 文档同步

**Files:**
- Modify: `yi-agent-rs/justfile`（`reclaim` recipe 及其 22 行注释）
- Modify: `docs/project-management/tooling.md`
- Modify: `CLAUDE.md`

- [x] **Step 1: `reclaim` 改为变参透传**

```just
# 用变参（`*flags`）而不是具名参数。实测 just 的具名参数**不能带连字符**
# （`prune-units=""` 会让 `--prune-units 1` 被当成位置参数吃掉，参数全盘错位），
# 而这次要新增的两个参数恰恰需要带值的长选项。变参把一切原样透传，最稳。
reclaim *flags: reclaim-check
    bash scripts/reclaim-target.sh {{flags}}
```

脚本侧负责兼容旧写法（`just reclaim apply=1 worktrees=1` 是现有文档里的用法，
不能失效），在参数解析开头加：

```bash
# 兼容旧写法：`just reclaim apply=1 worktrees=1`（just 早期 recipe 用的是
# 「非空即真」的具名参数）。新写法直接传 --apply 等长选项。
norm=()
for a in "$@"; do
  case "$a" in
    apply=1)     norm+=(--apply) ;;
    worktrees=1) norm+=(--include-worktrees) ;;
    *)           norm+=("$a") ;;
  esac
done
set -- "${norm[@]}"
```

实测矩阵（`just --dry-run` 看到的实际参数）：

| 调用 | 传给脚本 |
|---|---|
| `just reclaim` | （空） |
| `just reclaim apply=1` | `--apply` |
| `just reclaim apply=1 worktrees=1` | `--apply --include-worktrees` |
| `just reclaim --apply --prune-units 1` | `--apply --prune-units 1` |
| `just reclaim --apply --min-age 0` | `--apply --min-age 0` |

- [x] **Step 2: 重写 recipe 上方注释**

现有注释说「实测主仓库曾积到 78 万个、135 GiB」「cargo 从不读取这些 `.o`」——前半句是表观值（去重后 19.81 GiB 可单独释放），后半句漏了「`unpacked` 下删了会丢调试信息」。改成 D1/D2/D3 三档说明，并明确 `prune-units` 的重编代价（3.22 s vs 0.85 s）。

- [x] **Step 3: 重写 `tooling.md` 的条目**

现有条目里的「`just reclaim` 报告 93.1 GiB 孤儿」「`deps` 仅剩 257 个 `.o`」均已被推翻。新条目必须给出**可复现的判据命令**：

```
判据：
- `bash yi-agent-rs/scripts/test_reclaim_target.sh`（39 passed，已进 just ci 为 reclaim-check）
- `find yi-agent-rs/target/debug/deps -name '*.rcgu.o' | wc -l` 在全新 target 上为 0
- `just reclaim` 预演报告 D1+D2 合并真实可释放 26.17 GiB（按 nlink==1）
```

- [x] **Step 4: 更新 `CLAUDE.md`**

「磁盘占用：`target/` 会无限膨胀」小节补一条：`packed` 单次更费（17.8 G vs 默认 10.4 G），但换来「重编不再累积」——这是选它而不是 `off` 的理由（`off` 会丢光 DWARF）。

- [x] **Step 5: 提交**

```bash
git commit -m "docs(build): rewrite reclaim guidance around verified numbers"
```

---

### Task 7: 全量验证

- [x] **Step 1: 自测**

```bash
cd yi-agent-rs && bash scripts/test_reclaim_target.sh   # 39 passed
```

- [x] **Step 2: `just ci` 全绿**

```bash
cd yi-agent-rs && export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && just ci
```

**注意**：profile 任何取值变更（含这次**不改**）都会让 `target/` fingerprint 失效，
`just ci` 会从零重编，预计 **15~25 分钟**。这是预期的一次性成本，不是卡死。
若超过 40 分钟，按 `CLAUDE.md` 的锁诊断流程查 `ps ax -o pid=,stat=,command= | grep cargo`。

- [x] **Step 3: 真机回收验证**

```bash
cd yi-agent-rs && export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$PATH"
du -sm target/debug/* | sort -rn | head -3
bash scripts/reclaim-target.sh --apply 2>&1 | tail -8
du -sm target/debug/* | sort -rn | head -3
```

预期：`deps` + `incremental` 合计下降约 26 GiB。

- [x] **Step 4: 反复重编不累积（核心验收）**

```bash
cd yi-agent-rs && cargo clean -p yi-agent-core
for i in 1 2 3; do
  touch crates/yi-agent-core/src/lib.rs
  cargo build --all-features 2>/dev/null
  find target/debug/deps -name '*.rcgu.o' | wc -l
done
```

预期三行都是 `0`。**只要有一行非 0，说明 profile 没生效，回去查 Task 4 Step 3。**

- [x] **Step 5: 合并**

按 `superpowers:finishing-a-development-branch`：在 worktree 里跑完 `cargo fmt --all`、`just ci`，再 `git checkout main && git merge --no-ff docs/build-disk-usage-spec`，合并后删除分支与 worktree。

**清理时注意**：`.worktrees/` 下另外 5 个 worktree 的 target 合计约 114 GiB（28/31/32/11/11 GiB）。它们各自的 `Cargo.toml` 没有本次改动，但下次在那个 worktree 里构建就会用新的 profile。本 plan **不自动删**它们——那是工作流决策（spec §6）。只提示用户可执行：

```bash
just reclaim --apply --include-worktrees
```

---

## 验收标准（全部可执行）

| 标准 | 命令 | 期望 |
|---|---|---|
| 判据自测 | `bash yi-agent-rs/scripts/test_reclaim_target.sh` | **`39 passed`**（8 原有 + 31 新增） |
| profile 生效 | `cargo build -v` 抓 `split-debuginfo=` | 唯一值 **`packed`** |
| 新产物无 `.o` | `find target/debug/deps -name '*.rcgu.o' \| wc -l`（全新 target） | `0` |
| 无 dSYM | `ls target/debug/deps/*.dSYM \| wc -l` | `0` |
| 不累积 | 三次 `touch` 重编后数 `.o` | `0/0/0` |
| 存量回收 | `just reclaim apply=1`（Task 6 改造后：`just reclaim --apply`）前后 `du -sm` | 降约 26 GiB |
| CI 全绿 | `just ci` | **22** 门禁全过（含 `reclaim-check`、`affected-check`） |
