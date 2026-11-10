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
#   主仓库实测快照（20:40，未按 inode 去重）：`.rcgu.o` 268,571 个 47.1 GB、
#   `.rlib` 3,517 个 30.7 GB、dSYM 339 个 27.6 GB（陈旧 19.8 GB）、二进制
#   574 个 13.0 GB（陈旧 10.6 GB）。绝对值随构建历史大幅变化，不要当阈值。
#
#   安全性的证据（实测，不是推断）：`find target/debug/deps -name '*.rcgu.o'
#   -delete` 之后 `cargo build -p yi-agent-tools` **1.6 秒完成且不触发重编**。
#   说明 cargo 从不读这些 `.o`——它们只在同一次编译的链接步骤里被用到，
#   编译产物（rlib / 可执行文件）落盘后即可丢。
#
# 注意：这个脚本本身不是必需的修复。`[profile.dev] split-debuginfo = "packed"`
# 让新构建不再产生 `.o`；本脚本负责清理历史上已经堆积的存量。
#
# 四项判据：
#   D1 旧增量会话 —— incremental/ 下每个单元目录只留最新会话
#   D2 孤儿与未被引用 `.o` —— 无同名 rlib 即孤儿；否则按二进制 N_OSO 引用集判定
#   D3 `--prune-units=N` —— 每个 crate 只留最新 N 个单元目录（默认关：
#                           实测 27→1 会让重编从 0.85s 变 3.22s）
#   D4 陈旧二进制与 dSYM —— 同一 (crate, 目标类型) 只留最新一个
#
# 用法：
#   bash scripts/reclaim-target.sh                 # 预演（默认，只报告不删）
#   bash scripts/reclaim-target.sh --apply         # 真的删
#   --include-worktrees     同时清 .worktrees/* 的 target
#   --target DIR            只清指定 target 目录
#   --skip-incremental      跳过 D1
#   --skip-deps             跳过 D2 与 D4
#   --prune-units N         启用 D3，每个 crate 留最新 N 个单元目录
#   --min-age N             跳过 mtime 在 N 秒以内的候选（默认 600）
#   --force                 编译锁被持有时仍执行
#
# 退出码：0 = 正常（预演或清理成功）；1 = 有参数错误或被并发护栏拦下。
set -uo pipefail

MODE="dry-run"
INCLUDE_WORKTREES=0
EXPLICIT_TARGET=""
SKIP_DEPS=0
SKIP_INCREMENTAL=0
PRUNE_UNITS=0
MIN_AGE=600
FORCE=0
while [ $# -gt 0 ]; do
  case "$1" in
    --apply) MODE="apply" ;;
    --include-worktrees) INCLUDE_WORKTREES=1 ;;
    --skip-deps) SKIP_DEPS=1 ;;
    --skip-incremental) SKIP_INCREMENTAL=1 ;;
    --prune-units)
      shift
      PRUNE_UNITS="${1:-}"
      case "$PRUNE_UNITS" in
        ''|*[!0-9]*) echo "reclaim-target: --prune-units 需要一个非负整数（得到 '${PRUNE_UNITS:-}'）" >&2; exit 1 ;;
      esac
      ;;
    --min-age)
      shift
      MIN_AGE="${1:-}"
      case "$MIN_AGE" in
        ''|*[!0-9]*) echo "reclaim-target: --min-age 需要一个非负整数秒数（得到 '${MIN_AGE:-}'）" >&2; exit 1 ;;
      esac
      ;;
    --force) FORCE=1 ;;
    --target) shift; EXPLICIT_TARGET="${1:-}"; [ -n "$EXPLICIT_TARGET" ] || { echo "reclaim-target: --target 需要一个目录参数" >&2; exit 1; } ;;
    -h|--help)
      # 打印文件头注释块（到 `set -uo pipefail` 之前），去掉行首 "# "
      awk 'NR > 1 { if ($0 !~ /^#/) exit; print }' "$0" | sed 's/^# \{0,1\}//'
      exit 0 ;;
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
# D2：deps/ 下未被任何可执行二进制「引用」的 .o。
#
# 判据为什么是 N_OSO 而不是 mtime（spec §4.1 实测）：
#   一次编译会在同一秒写出该 key 的所有 mid，mtime 完全相同，无法排序。实测
#   915 个 OSO 引用里 906 个指向「非最新 mid」——按 mtime 取舍会删掉当前
#   二进制正在用的调试信息。
#
# 引用集来源：对 deps/ 下每个可执行文件跑 `nm -pa`，收集所有 `OSO` 行指向的
# .o basename。deps 里没有可执行文件时无法构造引用集，退化为只清孤儿 key。
#
# 性能：实测 456 个二进制扫一遍约 78 秒。预演也要扫（否则报不出候选）。
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

# D3：--prune-units=N —— 每个 crate 前缀下只保留 mtime 最新的 N 个单元目录。
#
# 为什么默认关闭（spec §7.3 实测）：把 yi_agent_core 的 27 个单元目录删到只剩
# 1 个后，touch 源码重编要 3.22 s；保留全部 27 个只要 0.85 s。所以这是「用
# 重编时间换磁盘」的选项，不能默认开。
#
# 前缀取法：单元目录名是 <crate_with_underscores>-<hash>，crate 名里的连字符
# 在目标名里已变成下划线（实测 1097 个目录无一带连字符），所以 rsplit('-', 1)
# 是安全的。
prune_units() { # $1 = target dir, $2 = keep N -> "removed_dirs\tfreed_bytes"
  python3 - "$1" "$2" <<'PY'
import os, shutil, sys
root = sys.argv[1]
keep = int(sys.argv[2])
removed = freed = 0
if keep > 0:
    for prof in sorted(os.listdir(root)):
        inc = os.path.join(root, prof, "incremental")
        if not os.path.isdir(inc):
            continue
        groups = {}
        for unit in os.scandir(inc):
            if not unit.is_dir():
                continue
            pre = unit.name.rsplit("-", 1)[0]
            groups.setdefault(pre, []).append(unit)
        for pre, units in groups.items():
            units.sort(key=lambda u: -u.stat().st_mtime)
            for u in units[keep:]:
                size = 0
                for r, _ds, fs in os.walk(u.path):
                    for f in fs:
                        try:
                            size += os.lstat(os.path.join(r, f)).st_size
                        except OSError:
                            pass
                try:
                    shutil.rmtree(u.path)
                    removed += 1
                    freed += size
                except OSError:
                    pass
print(f"{removed}\t{freed}")
PY
}

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

# D4：陈旧二进制 + 对应 dSYM。
#
# 根因是 cargo 对「同一 crate 同一目标」在依赖或 profile 变化时新开一个
# -<hash> 文件名，旧二进制永不删除。实测 target/debug/deps 里 574 个二进制
# 占 12.95 GB，其中 449 个陈旧占 10.6 GB；dSYM 339 个占 27.56 GB，陈旧 19.8 GB。
#
# 分组键必须是 (crate, 目标类型)：只用 crate 前缀会误删。实测 plugin_pack 的
# plugin_pack-28129df3848e2374(3.8MB, bin-plugin-pack) 与
# plugin_pack-f90c9159d28d6eaf(1.0MB, test-bin-plugin-pack) mtime 只差 0.338s，
# 两个都是活的。目标类型从 .fingerprint/<pkg>-<hash>/*.json 的**文件名**读
# （文件名即 "bin-demo" / "test-bin-demo" / "lib"），多个目标按排序拼成键。
#
# 安全阀：没有 fingerprint 证据的二进制**一律保留**。判不了就不动。
_run_stale() { python3 - "$1" "$2" "$3" <<'PY'
import os, shutil, sys, time
root, min_age, mode = sys.argv[1], int(sys.argv[2]), sys.argv[3]
now = time.time()

def dir_size(d):
    tot = 0
    for r, _ds, fs in os.walk(d):
        for f in fs:
            try:
                tot += os.lstat(os.path.join(r, f)).st_size
            except OSError:
                pass
    return tot

sb = sby = sd = sdy = 0
for prof in sorted(os.listdir(root)):
    deps = os.path.join(root, prof, "deps")
    froot = os.path.join(root, prof, ".fingerprint")
    if not os.path.isdir(deps) or not os.path.isdir(froot):
        continue

    # fingerprint 索引：hash -> "target1|target2"
    #
    # 只按 hash 索引，不按 (pkg, hash)。实测真实仓库里两侧的包名形式不同：
    # .fingerprint 用连字符（yi-agent-d89cb4ada82b13a9），deps 里的二进制用
    # 下划线（yi_agent-d89cb4ada82b13a9），哈希完全一致。若按 pkg 拼接做键，
    # 所有真实二进制都会匹配不上、被当成「无证据」而全部保留 —— D4 静默失效。
    # 哈希是 16 位，足够唯一。
    fpmap = {}
    for ent in os.scandir(froot):
        if not ent.is_dir() or "-" not in ent.name:
            continue
        _pkg, _, h = ent.name.rpartition("-")
        if len(h) != 16:
            continue
        kinds = sorted({f[:-5] for f in os.listdir(ent.path) if f.endswith(".json")})
        if kinds:
            fpmap[h] = "|".join(kinds)

    bins = []
    for e in os.scandir(deps):
        if not e.is_file(follow_symlinks=False):
            continue
        if "." in e.name or not os.access(e.path, os.X_OK):
            continue
        pkg, _, h = e.name.rpartition("-")
        if len(h) != 16:
            continue
        kind = fpmap.get(h)
        if kind is None:
            continue                      # 无证据，保留
        try:
            st = e.stat(follow_symlinks=False)
        except OSError:
            continue
        if min_age > 0 and now - st.st_mtime < min_age:
            continue
        bins.append((pkg, kind, e.name, e.path, st.st_mtime, st.st_size))

    groups = {}
    for item in bins:
        groups.setdefault((item[0], item[1]), []).append(item)

    for _key, items in groups.items():
        items.sort(key=lambda x: -x[4])   # 新 -> 旧
        for _pkg, _kind, name, path, _mt, size in items[1:]:
            dsym = path + ".dSYM"
            ds = dir_size(dsym) if os.path.isdir(dsym) else 0
            if mode == "apply":
                try:
                    os.unlink(path)
                    sb += 1
                    sby += size
                    if os.path.isdir(dsym):
                        shutil.rmtree(dsym)
                        sd += 1
                        sdy += ds
                except OSError:
                    pass
            else:
                sb += 1
                sby += size
                if os.path.isdir(dsym):
                    sd += 1
                    sdy += ds

print(f"{sb}\t{sby}\t{sd}\t{sdy}")
PY
}

scan_stale_bins() { _run_stale "$1" "${2:-0}" dry; }

apply_stale_bins() { _run_stale "$1" "${2:-0}" apply; }

scan_deps() { # $1 = target root, $2 = min_age -> "orphan_keys\torphan_files\torphan_bytes\tunref_files\tunref_bytes\ttotal_keys\ttotal_files\tskipped"
  python3 - "$1" "${2:-0}" <<'PY'
import os, subprocess, sys, time
root = sys.argv[1]
min_age = int(sys.argv[2])
now = time.time()
deps_dirs = []
for prof in sorted(os.listdir(root)):
    d = os.path.join(root, prof, "deps")
    if os.path.isdir(d):
        deps_dirs.append(d)
aggregate = [0] * 8
skipped_flag = 0
for d in deps_dirs:
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
                    if min_age > 0 and now - st.st_mtime < min_age:
                        continue          # 太新，可能是正在进行的编译
                    objs[e.name] = st.st_size
                except OSError:
                    pass

    bins = 0
    with os.scandir(d) as it:
        for e in it:
            if e.is_file(follow_symlinks=False) and "." not in e.name \
               and os.access(e.path, os.X_OK):
                bins += 1

    # collect referenced basenames from every executable in deps/
    referenced = set()
    if bins:
        print(f"   正在扫描 {bins} 个二进制（约 {bins * 0.17:.0f} 秒）…", file=sys.stderr)
        with os.scandir(d) as it:
            for e in it:
                if not e.is_file(follow_symlinks=False):
                    continue
                if "." in e.name:      # cargo 产物名不含 '.'，带点的都不是二进制
                    continue
                if not os.access(e.path, os.X_OK):
                    continue
                try:
                    out = subprocess.run(["nm", "-pa", e.path],
                                         capture_output=True, text=True,
                                         errors="replace").stdout
                except (OSError, subprocess.SubprocessError):
                    continue
                for line in out.splitlines():
                    if "OSO" in line:
                        referenced.add(os.path.basename(line.split()[-1]))

    # ---- 代际自检：引用集与磁盘 .o 零交集时必须放弃引用判据 ----
    # split-debuginfo 从 unpacked 切到 packed/off 之后，新一代二进制记录的
    # N_OSO 名字与磁盘上存量 .o 的命名格式对不上。实测真实仓库取样 30 个最新
    # 二进制，13,912 条 OSO 名字与 268,571 个磁盘 .o 的交集 = 0。若照用引用
    # 判据，会把 100% 的 .o 判成"未被引用"，一次 reclaim 删掉 47 GB。
    _overlap = len(referenced & set(objs))
    use_ref = bool(referenced) and (_overlap > 0 or not objs)
    if referenced and objs and _overlap == 0:
        skipped_flag = 1

    by_key = {}
    for name, size in objs.items():
        by_key.setdefault(name.split(".")[0], []).append((name, size))

    ok = of = ob = 0
    for key, items in by_key.items():
        if key not in rlibs:
            ok += 1
            of += len(items)
            ob += sum(s for _n, s in items)

    # unreferenced only makes sense for keys that still have an rlib,
    # AND only when the reference set is trustworthy (use_ref)
    uf = ub = 0
    if bins and use_ref:
        for key, items in by_key.items():
            if key not in rlibs:
                continue          # already counted as orphan
            for name, size in items:
                if name not in referenced:
                    uf += 1
                    ub += size

    aggregate[0] += ok
    aggregate[1] += of
    aggregate[2] += ob
    aggregate[3] += uf
    aggregate[4] += ub
    aggregate[5] += len(by_key)
    aggregate[6] += len(objs)

aggregate[7] = skipped_flag
print("\t".join(str(x) for x in aggregate))
PY
}

apply_clean() { # $1 = target root, $2 = min_age -> "removed\tfreed"
  python3 - "$1" "${2:-0}" <<'PY'
import os, subprocess, sys, time
root = sys.argv[1]
min_age = int(sys.argv[2])
now = time.time()
removed = freed = 0
for prof in sorted(os.listdir(root)):
    d = os.path.join(root, prof, "deps")
    if not os.path.isdir(d):
        continue
    rlibs = set()
    with os.scandir(d) as it:
        for e in it:
            if e.is_file(follow_symlinks=False) and e.name.endswith(".rlib"):
                base = e.name[:-5]
                rlibs.add(base[3:] if base.startswith("lib") else base)

    # bins 必须在这里重算：扫描阶段 unref 在 bins==0 时正确保持 0，但若这里
    # 无条件套用「不在引用集里就删」，没有二进制时引用集为空，活跃单元的所有
    # .o 都会被删掉 —— 一次 reclaim 就让下次构建从零开始。
    bins = 0
    with os.scandir(d) as it:
        for e in it:
            if e.is_file(follow_symlinks=False) and "." not in e.name \
               and os.access(e.path, os.X_OK):
                bins += 1

    referenced = set()
    if bins:
        with os.scandir(d) as it:
            for e in it:
                if not e.is_file(follow_symlinks=False):
                    continue
                if "." in e.name or not os.access(e.path, os.X_OK):
                    continue
                try:
                    out = subprocess.run(["nm", "-pa", e.path],
                                         capture_output=True, text=True,
                                         errors="replace").stdout
                except (OSError, subprocess.SubprocessError):
                    continue
                for line in out.splitlines():
                    if "OSO" in line:
                        referenced.add(os.path.basename(line.split()[-1]))

    objs = set()
    with os.scandir(d) as it:
        for e in it:
            if e.is_file(follow_symlinks=False) and e.name.endswith(".rcgu.o"):
                objs.add(e.name)
    # 代际自检：零交集时放弃引用判据，只清孤儿 key
    _overlap = len(referenced & objs)
    use_ref = bool(referenced) and (_overlap > 0 or not objs)

    victims = []
    with os.scandir(d) as it:
        for e in it:
            if not e.is_file(follow_symlinks=False):
                continue
            if not e.name.endswith(".rcgu.o"):
                continue
            try:
                st = e.stat(follow_symlinks=False)
            except OSError:
                continue
            if min_age > 0 and now - st.st_mtime < min_age:
                continue
            key = e.name.split(".")[0]
            if key not in rlibs:
                victims.append((e.path, st.st_size, st.st_nlink))   # 孤儿：无条件
            elif use_ref and e.name not in referenced:
                victims.append((e.path, st.st_size, st.st_nlink))   # 活跃但未被引用

    for path, size, nlink in victims:
        try:
            os.unlink(path)          # 删除：nlink>1 也照删
        except OSError:
            continue
        removed += 1
        if nlink == 1:               # 计数：只有 nlink==1 才真正释放空间
            freed += size

print(f"{removed}\t{freed}")
PY
}

# 并发护栏：--apply 且未 --force 时，若任一 target 上有编译持锁就拒绝执行。
# 这是唯一一种「清理脚本导致构建失败」的路径（删掉刚写出、还没链接的 .o）。
if [ "$MODE" = "apply" ] && [ "$FORCE" != "1" ]; then
  for t in "${TARGETS[@]}"; do
    [ -d "$t" ] || continue
    if has_active_build "$t"; then
      say "reclaim-target: 检测到 $(basename "$t") 上有 cargo 正在编译（锁被持有）。"
      say "               此时删除可能让正在进行的链接失败。"
      say "               等编译结束，或加 --force 强制执行。"
      exit 1
    fi
  done
fi

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

  # ---- D2: deps 孤儿 + 未被引用的 .o ----
  oc=0; of=0; ob=0; uf=0; ub=0; tk=0; tf=0; skipped=0
  if [ "$SKIP_DEPS" = "0" ]; then
    line="$(scan_deps "$t" "$MIN_AGE" 2>/dev/null)" || { say "!! 扫描失败：$t"; line=$'0\t0\t0\t0\t0\t0\t0\t0'; }
    IFS=$'\t' read -r oc of ob uf ub tk tf skipped <<<"$line"
    say "   .rcgu.o 文件数  : ${tf}（属 ${tk} 个编译单元）"
    say "   孤儿 .o         : $of 个文件 / $oc 个编译单元，$(human "$ob")"
    if [ "${skipped:-0}" = "1" ]; then
      say "   未被引用的 .o   : 已跳过（N_OSO 与磁盘 .o 无交集，可能是"
      say "                     split-debuginfo 切换前的存量；只清孤儿）"
    else
      say "   未被引用的 .o   : $uf 个文件，$(human "$ub")"
    fi
  else
    say "   deps            : 已跳过（--skip-deps）"
  fi

  # ---- D4: 陈旧二进制 + dSYM ----
  sb=0; sby=0; sd=0; sdy=0
  if [ "$SKIP_DEPS" = "0" ]; then
    line="$(scan_stale_bins "$t" "$MIN_AGE")" || { say "!! D4 扫描失败：$t"; line=$'0\t0\t0\t0'; }
    IFS=$'\t' read -r sb sby sd sdy <<<"$line"
    say "   陈旧二进制      : $sb 个（含 dSYM $sd 个），$(human $((sby + sdy)))"
  fi

  if [ "$MODE" = "apply" ]; then
    if [ "$d1_dirs" -gt 0 ]; then
      res="$(apply_incremental "$t")" || { say "!! D1 清理失败：$t"; res=$'0\t0'; }
      IFS=$'\t' read -r rm1 fr1 <<<"$res"
      say "   D1 已删除       : $rm1 个会话目录，回收 $(human "$fr1")"
      grand_freed=$((grand_freed + fr1))
      grand_d1_dirs=$((grand_d1_dirs + rm1))
    fi
    if [ "$of" -gt 0 ] || [ "$uf" -gt 0 ]; then
      res="$(apply_clean "$t" "$MIN_AGE")" || { say "!! 清理失败：$t"; res=$'0\t0'; }
      IFS=$'\t' read -r rm fr <<<"$res"
      say "   D2 已删除       : $rm 个，回收 $(human "$fr")"
      grand_freed=$((grand_freed + fr))
    fi
    if [ "$sb" -gt 0 ]; then
      res="$(apply_stale_bins "$t" "$MIN_AGE")" || { say "!! D4 清理失败：$t"; res=$'0\t0\t0\t0'; }
      IFS=$'\t' read -r rm4 fr4 d4n d4b <<<"$res"
      say "   D4 已删除       : $rm4 个二进制 + $d4n 个 dSYM，回收 $(human $((fr4 + d4b)))"
      grand_freed=$((grand_freed + fr4 + d4b))
    fi
    if [ "$PRUNE_UNITS" -gt 0 ]; then
      res="$(prune_units "$t" "$PRUNE_UNITS")" || { say "!! D3 清理失败：$t"; res=$'0\t0'; }
      IFS=$'\t' read -r rm3 fr3 <<<"$res"
      if [ "$rm3" -gt 0 ]; then
        say "   D3 已删除       : $rm3 个多余单元目录，回收 $(human "$fr3")"
        grand_freed=$((grand_freed + fr3))
      fi
    fi
    if [ "$d1_dirs" -eq 0 ] && [ "$of" -eq 0 ] && [ "$uf" -eq 0 ] && [ "$PRUNE_UNITS" -eq 0 ] && [ "$sb" -eq 0 ]; then
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
