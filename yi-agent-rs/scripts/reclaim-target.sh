#!/usr/bin/env bash
#
# 回收 cargo target 目录里的**可证明无用**的垃圾，而不是整体 `cargo clean`。
#
# 为什么需要它（2026-11-07 实测，见 docs/superpowers/specs/
# 2026-11-07-build-disk-usage-design.md）：
#
#   macOS 上 dev/test profile 默认 `split-debuginfo = "unpacked"`，rustc 把每个
#   codegen unit 的 DWARF 留在 `deps/<crate>-<hash>.<mid>.<idx>.rcgu.o`。文件名
#   带 hash，hash 随依赖图 / feature 组合变化，于是**旧 hash 的 `.o` 永久残留**：
#   cargo 重编同 hash 的单元会覆盖同名文件，但换 hash 后就再也不碰旧文件。
#
#     主仓库实测：782,793 个 `.o` / 135 GB，对应 rlib 只有 2501 个，
#     其中 1149 个编译单元是孤儿（占 93 GB）。
#
#   安全性的证据（实测，不是推断）：`find target/debug/deps -name '*.rcgu.o'
#   -delete` 之后 `cargo build -p yi-agent-tools` **1.6 秒完成且不触发重编**。
#   说明 cargo 从不读这些 `.o`——它们只在同一次编译的链接步骤里被用到，
#   编译产物（rlib / 可执行文件）落盘后即可丢。
#
# 注意：这个脚本本身不是必需的修复。`[profile.dev] split-debuginfo = "packed"`
# 让新构建不再产生 `.o`；本脚本负责清理历史上已经堆积的存量。
#
# 用法：
#   bash scripts/reclaim-target.sh              # 预演（默认，只报告不删）
#   bash scripts/reclaim-target.sh --apply      # 真的删
#   bash scripts/reclaim-target.sh --apply --include-worktrees
#                                              # 同时清 .worktrees/* 的 target
#   bash scripts/reclaim-target.sh --target DIR # 只清指定 target 目录
#
# 退出码：0 = 正常（预演或清理成功）；1 = 有参数错误。
set -uo pipefail

MODE="dry-run"
INCLUDE_WORKTREES=0
EXPLICIT_TARGET=""
SKIP_DEPS=0
SKIP_INCREMENTAL=0
while [ $# -gt 0 ]; do
  case "$1" in
    --apply) MODE="apply" ;;
    --include-worktrees) INCLUDE_WORKTREES=1 ;;
    --skip-deps) SKIP_DEPS=1 ;;
    --skip-incremental) SKIP_INCREMENTAL=1 ;;
    --target) shift; EXPLICIT_TARGET="${1:-}"; [ -n "$EXPLICIT_TARGET" ] || { echo "reclaim-target: --target 需要一个目录参数" >&2; exit 1; } ;;
    -h|--help) sed -n '2,32p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "reclaim-target: unknown option '$1'（见 --help）" >&2; exit 1 ;;
  esac
  shift
done

RS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_ROOT="$(cd "$RS_DIR/.." && pwd)"

# 候选 target 目录：显式指定 > 主仓库 + （可选）各 worktree。
TARGETS=()
if [ -n "$EXPLICIT_TARGET" ]; then
  TARGETS+=("$EXPLICIT_TARGET")
else
  TARGETS+=("$RS_DIR/target")
  if [ "$INCLUDE_WORKTREES" = "1" ]; then
    for wt in "$REPO_ROOT"/.worktrees/*/yi-agent-rs/target; do
      [ -d "$wt" ] && TARGETS+=("$wt")
    done
  fi
fi

say() { printf '%s\n' "$*"; }

human() { # $1 = bytes
  awk -v b="$1" 'BEGIN{split("B KiB MiB GiB TiB",u," ");i=1;while(b>=1024&&i<5){b/=1024;i++}printf "%.1f %s",b,u[i]}'
}

# 统计某个 target 目录下「孤儿 .o」的字节数。
#
# 判据：文件名形如 <crate>-<16hex>.<mid>.<idx>.rcgu.o。取 `<crate>-<hash>`
# 前缀；若 deps/ 里没有同名 `<prefix>.rlib`（去掉 lib 前缀后比较），则该编译
# 单元已被替换，它的 `.o` 是确定的垃圾。
#
# 用 python3 而不是 find + shell 循环：主仓库有 78 万个 `.o`，`find -name '*.o'`
# 加上逐文件 stat 在 shell 里要跑十几分钟；一次 scandir + 一次 dict 比对是秒级。
scan_target() { # $1 = target dir
  python3 - "$1" <<'PY'
import os, sys

root = sys.argv[1]
deps = os.path.join(root, "debug", "deps")
if not os.path.isdir(deps):
    # release 等其它 profile 也可能有 deps，逐个扫
    deps_dirs = [os.path.join(root, p, "deps") for p in os.listdir(root)
                 if os.path.isdir(os.path.join(root, p, "deps"))]
else:
    deps_dirs = [deps]

orphan_bytes = orphan_count = total_o = 0
orphan_files = total_files = 0
for d in deps_dirs:
    rlibs, objs, objs_count = set(), {}, {}
    with os.scandir(d) as it:
        for e in it:
            if not e.is_file(follow_symlinks=False):
                continue
            n = e.name
            if n.endswith(".rlib"):
                base = n[:-5]
                if base.startswith("lib"):
                    base = base[3:]
                rlibs.add(base)
            elif n.endswith(".rcgu.o"):
                key = n.split(".")[0]
                try:
                    objs[key] = objs.get(key, 0) + e.stat(follow_symlinks=False).st_size
                    objs_count[key] = objs_count.get(key, 0) + 1
                except OSError:
                    pass
    for key, size in objs.items():
        total_o += 1
        total_files += objs_count.get(key, 0)
        if key not in rlibs:
            orphan_count += 1
            orphan_bytes += size
            orphan_files += objs_count.get(key, 0)

print(f"{orphan_count}\t{orphan_files}\t{orphan_bytes}\t{total_o}\t{total_files}")
PY
}

# D1：incremental/<crate>-<hash>/s-<id> —— 每个「编译单元目录」内只留 mtime
# 最新的一个会话，其余整目录删。
#
# 注意层级：<crate>-<hash> 是编译单元（hash 覆盖 feature 组合/依赖/profile），
# s-<id> 才是会话。只删会话、不动单元目录本身，因此零重编代价（spec §7.3）。
scan_incremental() { # $1 = target dir -> "dirs\tfiles\tbytes"
  python3 - "$1" <<'PY'
import os, sys
root = sys.argv[1]
dirs = 0
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
            dirs += 1
            for r, _ds, fs in os.walk(stale.path):
                for f in fs:
                    try:
                        files += 1
                        size += os.lstat(os.path.join(r, f)).st_size
                    except OSError:
                        pass
print(f"{dirs}\t{files}\t{size}")
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

apply_clean() { # $1 = target dir, $2 = 保留的编译单元数
  python3 - "$1" <<'PY'
import os, sys

root = sys.argv[1]
removed = freed = 0
for prof in os.listdir(root):
    d = os.path.join(root, prof, "deps")
    if not os.path.isdir(d):
        continue
    rlibs = set()
    with os.scandir(d) as it:
        for e in it:
            if e.is_file(follow_symlinks=False) and e.name.endswith(".rlib"):
                base = e.name[:-5]
                if base.startswith("lib"):
                    base = base[3:]
                rlibs.add(base)
    with os.scandir(d) as it:
        victims = []
        for e in it:
            if not e.is_file(follow_symlinks=False):
                continue
            if not e.name.endswith(".rcgu.o"):
                continue
            if e.name.split(".")[0] in rlibs:
                continue
            try:
                victims.append((e.path, e.stat(follow_symlinks=False).st_size))
            except OSError:
                pass
    for p, s in victims:
        try:
            os.unlink(p)
            removed += 1
            freed += s
        except OSError:
            pass
print(f"{removed}\t{freed}")
PY
}

grand_freed=0
grand_d1_dirs=0
for t in "${TARGETS[@]}"; do
  [ -d "$t" ] || continue
  whole="$(du -sm "$t" 2>/dev/null | cut -f1)"
  say ""
  say "== $t"
  say "   目录总大小      : $(human $((whole * 1024 * 1024)))"

  # ---- D1: incremental 旧会话 ----
  d1_files=0; d1_bytes=0; d1_dirs=0
  if [ "$SKIP_INCREMENTAL" = "0" ]; then
    line="$(scan_incremental "$t")" || { say "!! D1 扫描失败：$t"; line=$'0\t0\t0'; }
    IFS=$'\t' read -r d1_dirs d1_files d1_bytes <<<"$line"
    say "   incremental 旧会话: $d1_dirs 个目录 / $d1_files 个文件，$(human "$d1_bytes")"
  else
    say "   incremental 旧会话: 已跳过（--skip-incremental）"
  fi

  # ---- D2: deps 孤儿 .o ----
  oc=0; of=0; ob=0; to=0; tf=0
  if [ "$SKIP_DEPS" = "0" ]; then
    line="$(scan_target "$t")" || { say "!! 扫描失败：$t"; line=$'0\t0\t0\t0\t0'; }
    IFS=$'\t' read -r oc of ob to tf <<<"$line"
    say "   .rcgu.o 文件数  : $tf（属 $to 个编译单元）"
    say "   孤儿 .o         : $of 个文件 / $oc 个编译单元，$(human "$ob")"
  else
    say "   deps 孤儿 .o    : 已跳过（--skip-deps）"
  fi

  if [ "$MODE" = "apply" ]; then
    if [ "$d1_dirs" -gt 0 ]; then
      res="$(apply_incremental "$t")" || { say "!! D1 清理失败：$t"; res=$'0\t0'; }
      IFS=$'\t' read -r rm1 fr1 <<<"$res"
      say "   D1 已删除       : $rm1 个会话目录，回收 $(human "$fr1")"
      grand_freed=$((grand_freed + fr1))
      grand_d1_dirs=$((grand_d1_dirs + rm1))
    fi
    if [ "$of" -gt 0 ]; then
      res="$(apply_clean "$t")" || { say "!! 清理失败：$t"; res=$'0\t0'; }
      IFS=$'\t' read -r rm fr <<<"$res"
      say "   D2 已删除       : $rm 个，回收 $(human "$fr")"
      grand_freed=$((grand_freed + fr))
    fi
    if [ "$d1_dirs" -eq 0 ] && [ "$of" -eq 0 ]; then
      say "   结论            : 无需清理"
    else
      after="$(du -sm "$t" 2>/dev/null | cut -f1)"
      say "   目录现在        : $(human $((after * 1024 * 1024)))"
    fi
  fi
done

say ""
if [ "$MODE" = "apply" ]; then
  say "合计回收：$(human "$grand_freed")"
  say "（重新编译不受影响：cargo 不读取已落盘产物之外的 .o）"
else
  say "预演模式，未删除任何文件。确认后加 --apply 执行。"
fi
