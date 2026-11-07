# 编译产物磁盘占用治理 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

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
| `split-debuginfo="off"` 全新 target 总量 | **9.9 GiB**、0 个 `.o`、0 个 dSYM | §4.2 |
| `debug=0` 全新 target 总量 | 6.6 GiB；函数符号 337,420 vs A 的 338,203 | §4.2 |
| 主仓库当前 `.o` | 267,503 个、43.8 GiB，其中 nlink==1 的 83,854 个占 19.81 GiB | §2.1 |
| 主仓库 `incremental/` | 902 个单元目录、1,046 个会话、32.7 GiB | §2.2 |
| D1（单元内旧会话） | 144 个目录 / 23,922 个文件，零重编代价 | §4.1 |
| D1+D2 合并真实可释放 | **26.17 GiB**（唯一 inode 171,685，按 nlink==1） | §2.3 |
| 删多余单元目录的代价 | 3.22 s vs 0.85 s（`yi_agent_core` 27→1） | §7.3 |
| OSO 引用与 mid mtime | 915 个引用里 906 个指向「非最新 mid」 | §4.1 |

## File Structure

| 文件 | 责任 | 动作 |
|---|---|---|
| `yi-agent-rs/Cargo.toml` | `[profile.dev] split-debuginfo = "off"`；替换 44 行错误注释 | 修改 |
| `desktop/src-tauri/Cargo.toml` | 新增 `[profile.dev]`（桌面壳是独立 package，不继承主 workspace） | 修改 |
| `plugins/superpowers-kanban/Cargo.toml` | 新增 `[profile.dev]`（独立 workspace） | 修改 |
| `yi-agent-rs/scripts/reclaim-target.sh` | 扩充为 D1/D2/D3 三判据 | 修改 |
| `yi-agent-rs/scripts/test_reclaim_target.sh` | 8 条断言扩充到 >= 18 条 | 修改 |
| `yi-agent-rs/justfile` | `reclaim` 新增 `prune-units` / `skip-deps` / `min-age` 参数；更新注释 | 修改 |
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

- [ ] **Step 1: 先写失败的测试**

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

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && bash scripts/test_reclaim_target.sh
```

预期：`FAIL: dry-run does not report incremental`（脚本还没有 D1）。

- [ ] **Step 3: 实现 D1**

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

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && bash scripts/test_reclaim_target.sh
```

预期：`12 passed`（原 8 条 + 新增 4 条）。

- [ ] **Step 5: 提交**

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

- [ ] **Step 1: 先写失败的测试**

在 D1 段之后插入。用假 `nm` 桩注入引用集（**不依赖真实 cargo 产物**）：

```bash
# ---- D2: N_OSO 引用集 ----
deps2="$tmp/debug/deps2"
mkdir -p "$deps2"
# 活跃单元 foo：3 个 .o，只有 1 个被二进制引用
for i in 0 1 2; do printf 'x%.0s' {1..1000} >"$deps2/foo-aaaaaaaaaaaaaaaa.mid$i.0.rcgu.o"; done
printf 'r%.0s' {1..100} >"$deps2/libfoo-aaaaaaaaaaaaaaaa.rlib"
# 一个可执行文件，桩 nm 会说它引用了 foo 的 mid0
printf '#!/bin/sh\necho "0000000100000000-0000000100000010 OSO /somewhere/foo-aaaaaaaaaaaaaaaa.mid0.0.rcgu.o"\n' >"$deps2/foo_bin-1234"
chmod +x "$deps2/foo_bin-1234"

# 假 nm：固定输出上面那一条 OSO
stub="$tmp/bin"; mkdir -p "$stub"
cat >"$stub/nm" <<'NMEOF'
#!/bin/sh
echo "0000000100000000-0000000100000010 OSO ${STUB_OSO:-/nonexistent}"
NMEOF
chmod +x "$stub/nm"

out="$(PATH="$stub:$PATH" bash "$SCRIPT" --target "$tmp")" || fail "dry-run exited nonzero"
grep -q "未被引用" <<<"$out" || { printf '%s\n' "$out" >&2; fail "dry-run does not report unreferenced .o"; }
n=$((n + 1)); pass "D2 reports unreferenced .o"

# 用真实路径的桩：只引用 mid0
cat >"$stub/nm" <<NMEOF
#!/bin/sh
echo "0000000100000000-0000000100000010 OSO $deps2/foo-aaaaaaaaaaaaaaaa.mid0.0.rcgu.o"
NMEOF
chmod +x "$stub/nm"
PATH="$stub:$PATH" bash "$SCRIPT" --apply --target "$tmp" >/dev/null || fail "D2 --apply failed"
[ -f "$deps2/foo-aaaaaaaaaaaaaaaa.mid0.0.rcgu.o" ] || fail "deleted a referenced .o"
n=$((n + 1)); pass "D2 keeps OSO-referenced .o"
[ -f "$deps2/foo-aaaaaaaaaaaaaaaa.mid1.0.rcgu.o" ] && fail "kept an unreferenced .o"
n=$((n + 1)); pass "D2 removes unreferenced .o"

# 没有任何可执行文件时，必须退化为只清孤儿 key，不误删活跃单元
deps3="$tmp/debug/deps3"; mkdir -p "$deps3"
printf 'a' >"$deps3/solo-bbbbbbbbbbbbbbbb.mid0.0.rcgu.o"
printf 'b' >"$deps3/libsolo-bbbbbbbbbbbbbbbb.rlib"
PATH="$stub:$PATH" bash "$SCRIPT" --apply --target "$tmp" >/dev/null || fail "no-binary case failed"
[ -f "$deps3/solo-bbbbbbbbbbbbbbbb.mid0.0.rcgu.o" ] || fail "no-binary fallback deleted a live unit"
n=$((n + 1)); pass "D2 falls back to orphan-key-only when no binaries exist"

# --skip-deps
printf 'c' >"$deps3/orphan-cccccccccccccccc.mid0.0.rcgu.o"
PATH="$stub:$PATH" bash "$SCRIPT" --apply --target "$tmp" --skip-deps >/dev/null || fail "--skip-deps failed"
[ -f "$deps3/orphan-cccccccccccccccc.mid0.0.rcgu.o" ] || fail "--skip-deps still cleaned deps"
n=$((n + 1)); pass "D2 honours --skip-deps"
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd yi-agent-rs && bash scripts/test_reclaim_target.sh
```

预期失败在 `D2 reports unreferenced .o`。

- [ ] **Step 3: 实现 D2**

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

orphan_keys = orphan_files = orphan_bytes = 0
by_key = {}
for name, size in objs.items():
    by_key.setdefault(name.split(".")[0], []).append((name, size))

for key, items in by_key.items():
    if key not in rlibs:
        orphan_keys += 1
        orphan_files += len(items)
        orphan_bytes += sum(s for _n, s in items)

# unreferenced only makes sense for keys that still have an rlib
unref_files = unref_bytes = 0
if bins:
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

`apply_clean` 同步改为只删「孤儿 key 的全部 `.o`」**加上**「活跃 key 里未被引用的 `.o`」，并且**跳过 `st_nlink > 1` 的文件**（硬链接别处还有，删了不释放空间也不该计入收益）。

```bash
# 在 apply_clean 里，收集 victims 时追加：
#   if st.st_nlink > 1: continue    # 硬链接，另处仍占用；不计入释放量
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd yi-agent-rs && bash scripts/test_reclaim_target.sh
```

预期：`17 passed`。

- [ ] **Step 5: 在真实主仓库上试跑（dry-run，必须与 spec 数字同量级）**

```bash
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cd yi-agent-rs && bash scripts/reclaim-target.sh 2>&1 | tail -12
```

预期：`deps 未被引用` 一栏是 **267,533 个文件 / 约 43.8 GiB 表观**；`incremental 旧会话` 是 23,922 个文件 / 约 8.5 GiB。若差一个数量级，说明判据写错了，**停下来查**而不是继续。

- [ ] **Step 6: 提交**

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

- [ ] **Step 1: 先写失败的测试**

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

- [ ] **Step 2: 跑测试确认失败**
- [ ] **Step 3: 实现 D3**（`--prune-units` 吃一个整数参数，校验 `N >= 0`；`N=0` 直接跳过）
- [ ] **Step 4: 跑测试确认通过**（预期 `20 passed`）
- [ ] **Step 5: 提交**

```bash
git commit -m "feat(build): add opt-in --prune-units for surplus build unit dirs"
```

---

### Task 4: 改 `[profile.dev]` 为 `split-debuginfo = "off"`

**Files:**
- Modify: `yi-agent-rs/Cargo.toml:85-125`

- [ ] **Step 1: 用实测数字替换那段 44 行注释**

现有注释里至少四处被本次实测推翻，必须整段重写（不是补一句）：

| 现有断言 | 实测事实 |
|---|---|
| 「`packed` 单次体积只省 4%」 | 全量测试路径 `packed` 17.8 G **比默认 10.4 G 更费** |
| 「默认 `cargo build` 462 个 `.o`」 | 统计口径错误；全量是 18,024 个 |
| 「`.o` 最大 53 MB，37 MB 是 `__DWARF`」 | 平均 172 KiB、中位 40 KiB，53 MB 是离群值 |
| 「`packed` 保留完整 DWARF，不降级调试」 | 代价是 `.dSYM` 占 8.2 GiB，总量反超 |

- [ ] **Step 2: 把取值改成 `off`**

```toml
[profile.dev]
split-debuginfo = "off"
```

- [ ] **Step 3: 验证 profile 真的生效（必须做，不能只看配置）**

```bash
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cd yi-agent-rs && rm -rf /tmp/profcheck && CARGO_TARGET_DIR=/tmp/profcheck \
  cargo build --offline -p yi-agent-flags --tests -v 2>&1 \
  | grep -o "split-debuginfo=[a-z]*" | sort -u
```

预期输出唯一一行：`split-debuginfo=off`。再确认不产 `.o`：

```bash
find /tmp/profcheck -name '*.rcgu.o' | wc -l     # 期望 0
ls -d /tmp/profcheck/debug/deps/*.dSYM 2>/dev/null | wc -l   # 期望 0
rm -rf /tmp/profcheck
```

- [ ] **Step 4: 提交**

```bash
git add yi-agent-rs/Cargo.toml
git commit -m "perf(build): stop emitting .o and .dSYM in dev profile"
```

---

### Task 5: 给 `desktop/` 与 `plugins/` 加同样的 profile

**Files:**
- Modify: `desktop/src-tauri/Cargo.toml`
- Modify: `plugins/superpowers-kanban/Cargo.toml`

- [ ] **Step 1: 两个文件各追加一段，附一行说明**

```toml
# 与 yi-agent-rs/Cargo.toml 的 [profile.dev] 保持一致。这是独立构建单元
# （desktop 是独立 package、plugins 是独立 workspace），不继承主 workspace，
# 必须在这里各自声明，否则它的 target/ 继续累积 *.rcgu.o。
[profile.dev]
split-debuginfo = "off"
```

- [ ] **Step 2: 验证两处都生效**

```bash
export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$PATH"
cd plugins/superpowers-kanban && CARGO_TARGET_DIR=/tmp/pcheck \
  cargo build --offline -v 2>&1 | grep -o "split-debuginfo=[a-z]*" | sort -u
rm -rf /tmp/pcheck
```

预期 `split-debuginfo=off`。`desktop/` 同样（用 `cargo metadata` 确认它读到了该 profile 即可，不必整包构建——tauri 构建很慢）。

- [ ] **Step 3: 提交**

```bash
git commit -m "perf(build): apply split-debuginfo=off to desktop and plugin crates"
```

---

### Task 6: justfile 参数 + 文档同步

**Files:**
- Modify: `yi-agent-rs/justfile`（`reclaim` recipe 及其 22 行注释）
- Modify: `docs/project-management/tooling.md`
- Modify: `CLAUDE.md`

- [ ] **Step 1: `reclaim` 增加三个透传参数**

```just
reclaim apply="" worktrees="" skip-deps="" skip-incremental="" prune-units="" min-age="": reclaim-check
    bash scripts/reclaim-target.sh \
      {{ if apply != "" { "--apply" } else { "" } }} \
      {{ if worktrees != "" { "--include-worktrees" } else { "" } }} \
      {{ if skip-deps != "" { "--skip-deps" } else { "" } }} \
      {{ if skip-incremental != "" { "--skip-incremental" } else { "" } }} \
      {{ if prune-units != "" { "--prune-units " + prune-units } else { "" } }} \
      {{ if min-age != "" { "--min-age " + min-age } else { "" } }}
```

- [ ] **Step 2: 重写 recipe 上方注释**

现有注释说「实测主仓库曾积到 78 万个、135 GiB」「cargo 从不读取这些 `.o`」——前半句是表观值（去重后 19.81 GiB 可单独释放），后半句漏了「`unpacked` 下删了会丢调试信息」。改成 D1/D2/D3 三档说明，并明确 `prune-units` 的重编代价（3.22 s vs 0.85 s）。

- [ ] **Step 3: 重写 `tooling.md` 的条目**

现有条目里的「`just reclaim` 报告 93.1 GiB 孤儿」「`deps` 仅剩 257 个 `.o`」均已被推翻。新条目必须给出**可复现的判据命令**：

```
判据：
- `bash yi-agent-rs/scripts/test_reclaim_target.sh`（>= 20 passed，已进 just ci 为 reclaim-check）
- `find yi-agent-rs/target/debug/deps -name '*.rcgu.o' | wc -l` 在全新 target 上为 0
- `just reclaim` 预演报告 D1+D2 合并真实可释放 26.17 GiB（按 nlink==1）
```

- [ ] **Step 4: 更新 `CLAUDE.md`**

「磁盘占用：`target/` 会无限膨胀」小节补一条：`packed` 反而更费磁盘（17.8 G vs 默认 10.4 G），选的是 `off`。

- [ ] **Step 5: 提交**

```bash
git commit -m "docs(build): rewrite reclaim guidance around verified numbers"
```

---

### Task 7: 全量验证

- [ ] **Step 1: 自测**

```bash
cd yi-agent-rs && bash scripts/test_reclaim_target.sh   # >= 20 passed
```

- [ ] **Step 2: `just ci` 全绿**

```bash
cd yi-agent-rs && export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" && just ci
```

**注意**：profile 从 `packed` 改成 `off` 会让整个 `target/` 的 fingerprint 失效，
`just ci` 会从零重编，预计 **15~25 分钟**。这是预期的一次性成本，不是卡死。
若超过 40 分钟，按 `CLAUDE.md` 的锁诊断流程查 `ps ax -o pid=,stat=,command= | grep cargo`。

- [ ] **Step 3: 真机回收验证**

```bash
cd yi-agent-rs && export PATH="/opt/homebrew/bin:$HOME/.cargo/bin:$PATH"
du -sm target/debug/* | sort -rn | head -3
bash scripts/reclaim-target.sh --apply 2>&1 | tail -8
du -sm target/debug/* | sort -rn | head -3
```

预期：`deps` + `incremental` 合计下降约 26 GiB。

- [ ] **Step 4: 反复重编不累积（核心验收）**

```bash
cd yi-agent-rs && cargo clean -p yi-agent-core
for i in 1 2 3; do
  touch crates/yi-agent-core/src/lib.rs
  cargo build --all-features 2>/dev/null
  find target/debug/deps -name '*.rcgu.o' | wc -l
done
```

预期三行都是 `0`。**只要有一行非 0，说明 profile 没生效，回去查 Task 4 Step 3。**

- [ ] **Step 5: 合并**

按 `superpowers:finishing-a-development-branch`：在 worktree 里跑完 `cargo fmt --all`、`just ci`，再 `git checkout main && git merge --no-ff docs/build-disk-usage-spec`，合并后删除分支与 worktree。

**清理时注意**：`.worktrees/` 下另外 5 个 worktree 的 target 合计约 114 GiB（28/31/32/11/11 GiB）。它们各自的 `Cargo.toml` 没有本次改动，但下次在那个 worktree 里构建就会用新的 profile。本 plan **不自动删**它们——那是工作流决策（spec §6）。只提示用户可执行：

```bash
just reclaim apply=1 worktrees=1
```

---

## 验收标准（全部可执行）

| 标准 | 命令 | 期望 |
|---|---|---|
| 判据自测 | `bash yi-agent-rs/scripts/test_reclaim_target.sh` | `>= 20 passed` |
| profile 生效 | `cargo build -v` 抓 `split-debuginfo=` | 唯一值 `off` |
| 新产物无 `.o` | `find target/debug/deps -name '*.rcgu.o' \| wc -l`（全新 target） | `0` |
| 无 dSYM | `ls target/debug/deps/*.dSYM \| wc -l` | `0` |
| 不累积 | 三次 `touch` 重编后数 `.o` | `0/0/0` |
| 存量回收 | `just reclaim --apply` 前后 `du -sm` | 降约 26 GiB |
| CI 全绿 | `just ci` | 16 门禁全过 |
