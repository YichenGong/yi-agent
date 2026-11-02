# 重构第二波：app-server 分层 实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 `server.rs`（7,590 行、125 个函数、**197 个顶层项**）按职责拆成 `server/` 子模块，并把 `rpc/thread.rs` 里 2,347 行的 `handle()` 拆成 4 个语义组、10 个文件——全程不改任何运行时行为、线上协议与公共 API。

**Architecture:** 两块相互独立、可并行。C2 把 `server.rs` 的 163 个顶层项搬进 `src/server/**`（12 组），**34 个项留在 `server.rs`**（18 个有外部引用的类型 + 3 个 `impl` + 13 个 `mod *_tests;` 声明），靠 `pub(crate) use` 把搬到子模块的项 re-export 回 `crate::server::X`，使 `rpc/*`（18 个文件）与 `server/tests/*`（3 个文件）的引用零改动。C3 把 `rpc/thread.rs` 的 23 个 `match` 臂（26 个 method）按语义分 4 组搬到 `src/rpc/thread/**`，`handle()` 外壳改成 4 个 `owns()` 的顺序分派。

**Tech Stack:** Rust 2021/2024、tokio、serde_json。无新增依赖。仅用 `cargo` / `just` / `python3`（测量与搬移脚本）。

**Spec:** `docs/superpowers/specs/2026-11-03-refactor-wave2-app-server-design.md`

---

## Global Constraints

以下每条都来自 spec 或实测，**每个任务都隐含包含本节**，不重复列出。

- **工作目录**：`/Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent/.worktrees/refactor-wave2-app-server`，分支 `refactor/wave2-app-server`。**严禁**在 `main` 上改文件或提交。
- **只搬不改语义**：允许的改动**仅限**三样——可见性（`fn`/`struct`/`const`/`static`/`impl` → 加 `pub(crate)`）、`use` 语句、路径前缀（`crate::server::` ↔ `super::`）。任何函数体内部逻辑改动都是违规。
- **`server.rs` 必须保持文件形态**：**不要**把 `src/server.rs` 改成 `src/server/mod.rs`。现有 13 个 `mod` 声明（`mod tests;` 是**目录模块**，指向 `src/server/tests/mod.rs`）都靠 `src/server.rs` + `src/server/` 目录并存的布局解析；改成 `mod.rs` 会让它们全部断裂。
- **搬移单元是「顶层项」，不是「行区间」**：一个顶层项 = 紧贴其上的 doc 注释 + 声明行 + 函数/类型体（到列 0 的 `}` 或行尾的 `;`）。**禁止**用「本函数首行到下一函数首行」切分——实测那样会把 32 个类型、14 个 const、8 个 `impl` 块、4 个 `thread_local!` 块、13 个 `mod` 声明一起切碎。必须用附录 A 的 `extract.py`。
- **`serve()` 的域分派块不许抽成函数**：`app_state!()` 是定义在 `serve()` 体内的宏，按名捕获 41 个局部量；16 个样板臂内联 `continue`。只能用**体内局部宏**收敛（Task 8C）。
- **`Plugin` 臂一行都不改**：它 `tokio::spawn` 一个 owned 的 `PluginChannelState`，语义与样板臂不同。
- **`driver_tick` 的赋值顺序原样保留**：它含 4 处借用规避（先快照 `idle`、先算全机运行数再构造 launcher）。这些顺序是借用检查器逼出来的，改动会编译失败或改变并发语义。
- **协议与公共 API 零变化**：`protocol.rs` 不动；`lib.rs` 的 `pub use server::{run, serve_stdio_with_relay}` 不动（这两个必须保持 `pub`）；`bindings-check` 必须无 diff。
- **不动这三块**：`server/tests/`（12 个文件 / 21,366 行）、`translate.rs`、`thread_store.rs`。
- **每任务结束必须跑**：`cargo test -p yi-agent-app-server` 绿、`cargo clippy -p yi-agent-app-server --all-targets -- -D warnings` 绿、`cargo fmt --all`。
- **测试数硬基线 = 1,166**。任何任务结束后 `cargo test -p yi-agent-app-server -- --list | grep -c ': test$'` 必须仍为 **1,166**（C3a 新增 1 个 `owns()` 测试后为 **1,167**，之后保持不变）。
- **提交信息**：conventional commits，首行 ≤72 字符，**不要**写 `Co-Authored-By`。
- **`server.rs` 顶层函数数必须单调下降到 0**；`rpc/thread/mod.rs` 最终 ≤ 200 行。

### 全局账目（实测，用于每步核对）

| 量 | 值 |
|---|---|
| `server.rs` 总行数 | 7,590 |
| 顶层项总数 | 197（125 fn / 18 struct / 14 const / 10 type / 4 enum / 4 `thread_local!` / 1 static / 8 impl / 13 mod） |
| 搬到子模块 | 163 项 / 7,067 行 |
| 留在 `server.rs` | 34 项 / 286 行 |
| 搬走后剩余（含 use 块、空行、doc） | 约 **355 行** |
| 需改可见性（`private` → `pub(crate)`） | **77 项** |
| 其中 private 但有外部引用（不改就编译不过） | **19 项** |
| `rpc/thread.rs` | 2,809 行，`handle()` 2,347 行，23 臂 / 26 method |

---

## Task 0: 建 worktree 与基线快照

先在隔离环境里跑通基线，并录制 C3 复用的 method 清单。没有基线就无法证明「测试数不变」。

**Files:**
- Create: `.worktrees/refactor-wave2-app-server/`（git worktree）
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread_methods_baseline.txt`
- Create: `/tmp/wave2/extract.py`（搬移脚本，附录 A）

**Interfaces:**
- Consumes: 无
- Produces: 分支 `refactor/wave2-app-server`；`thread_methods_baseline.txt`（26 行）；`/tmp/wave2/` 下的搬移与测量脚本

- [ ] **Step 1: 建 worktree**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
git worktree add .worktrees/refactor-wave2-app-server -b refactor/wave2-app-server
cd .worktrees/refactor-wave2-app-server
git branch --show-current   # 期望 refactor/wave2-app-server
```

- [ ] **Step 2: 跑基线测试，确认 1,166**

```bash
cd yi-agent-rs
cargo test -p yi-agent-app-server -- --list 2>/dev/null | grep -c ': test$'
```

期望 `1166`。若不同，**停止**并报告实际数字——基线已漂移，后续判据需重算。

- [ ] **Step 3: 记录基线结构指标**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
mkdir -p /tmp/wave2
{
  echo "server_lines=$(wc -l < src/server.rs)"
  echo "server_fns=$(grep -cE '^(pub(\(crate\))?\s+)?(async\s+)?fn ' src/server.rs)"
  echo "thread_lines=$(wc -l < src/rpc/thread.rs)"
  echo "tests=$(cd ../../.. && cargo test -p yi-agent-app-server -- --list 2>/dev/null | grep -c ': test$')"
} | tee /tmp/wave2/baseline.txt
```

期望：`server_lines=7590`、`server_fns=125`、`thread_lines=2809`、`tests=1166`。

- [ ] **Step 4: 安装搬移脚本（附录 A 全文写入 `/tmp/wave2/extract.py`）**

附录 A 是本计划的**核心工具**，它按「顶层项」切分而非行区间。先装好并自检：

```bash
mkdir -p /tmp/wave2
# 把附录 A 的代码块完整写入 /tmp/wave2/extract.py
cd yi-agent-rs/crates/yi-agent-app-server
python3 /tmp/wave2/extract.py --list src/server.rs
```

期望输出（精确分类）：

```
197 items  (const 14 / enum 4 / fn 125 / impl 8 / mod 13 / static 1 / struct 18 / tl 4 / type 10)
```

**若数字不符，停止**——脚本与文件不匹配。

顺便看一眼明细（`--verbose`），确认 `ServeLauncher` 与它的两个 impl、`SESSION_*` 的 4 个 `thread_local!` 都在列表里：

```bash
python3 /tmp/wave2/extract.py --list src/server.rs --verbose |
  grep -E 'ServeLauncher|SESSION_|PluginQueryError|DeltaCoalescer'
```

期望看到 8 行：4 个 `thread_local!`（`SESSION_MODEL_REF` / `SESSION_REASONING_EFFORT` /
`SESSION_EXTRA_ROOTS` / `SESSION_PRESERVE_THINKING`）、`ServeLauncher` 与它的 2 个
`impl`、`PluginQueryError`、`DeltaCoalescer` 与它的 1 个 `impl`。

- [ ] **Step 5: 录制 C3 的 method 基线（26 个）**

注意 `HEAD:` 后必须用 `./` 相对路径，否则 git 按仓库根解析会报 path does not exist。

```bash
cd yi-agent-rs
git show HEAD:./crates/yi-agent-app-server/src/rpc/thread.rs \
  | grep -oE '"thread/[a-zA-Z/]+"' | tr -d '"' | sort -u \
  > crates/yi-agent-app-server/src/rpc/thread_methods_baseline.txt
wc -l < crates/yi-agent-app-server/src/rpc/thread_methods_baseline.txt
```

期望 `26`。文件内容应恰好是这 26 行（字母序）：

```
thread/clear
thread/compact
thread/delete
thread/diff/read
thread/fork
thread/group/create
thread/group/delete
thread/group/setMembers
thread/group/update
thread/lead
thread/list
thread/listAll
thread/markRead
thread/readItems
thread/rename
thread/reorderPinned
thread/resume
thread/setModel
thread/setPermissionMode
thread/setPinned
thread/setPreserveThinking
thread/setReasoningEffort
thread/setTag
thread/start
thread/subscribe
thread/view/read
```

- [ ] **Step 6: 确认基线文件不影响编译，然后提交**

```bash
cd yi-agent-rs
cargo build -p yi-agent-app-server 2>&1 | tail -3
cargo fmt --all
git add -A
git commit -m "chore(app-server): wave2 基线快照与 thread method 清单"
```

---

## Task 0.5: 准备 `use` 头搬运机制（**必做，漏了会大面积编译失败**）

`server.rs` 顶部有 23 条 `use`（其中 2 条跨行）。搬出去的每个项都依赖这些名字——实测 `turn/command.rs` 需要 `Arc`/`Path`/`Translator`、`launcher.rs` 需要 `Mutex`/`Duration` 等。**只搬项、不搬 `use`，每个新文件都会冒出几十个 `E0433: failed to resolve`。**

正确做法不是手工挑 `use`（必然漏或多），而是**粘贴完整 `use` 块 + `cargo fix` 自动裁剪**。已实测：一个 `use` 4 条的文件，`cargo fix` 精确删掉 3 条未用的、保留 1 条，输出 `Fixed src/sub.rs (3 fixes)`。

**Files:**
- Create: `/tmp/wave2/usehdr.py`（提取完整 `use` 块）
- Create: `/tmp/wave2/generic_use.rs`（完整 `use` 块，供后续所有 Task 粘贴）

**Interfaces:**
- Consumes: `src/server.rs`
- Produces: `/tmp/wave2/generic_use.rs`

- [ ] **Step 1: 提取 `server.rs` 的完整 `use` 块**

把下面这段写进 `/tmp/wave2/usehdr.py`（注意 heredoc 用 `SCRIPT_EOF`，不要用 `PYEOF`，避免与其它脚本冲突）：

```python
#!/usr/bin/env python3
"""提取 .rs 文件顶部的全部 use 语句（支持跨行），输出到 stdout。"""
import sys, pathlib

lines = pathlib.Path(sys.argv[1]).read_text().splitlines()
out, i = [], 0
while i < len(lines):
    l = lines[i]
    if not l.startswith("use "):
        i += 1
        continue
    buf = [l]
    while not buf[-1].rstrip().endswith(";"):
        i += 1
        buf.append(lines[i])
    out.append("\n".join(buf))
    i += 1
print("\n".join(out))
```

```bash
cd yi-agent-rs/crates/yi-agent-app-server
python3 /tmp/wave2/usehdr.py src/server.rs > /tmp/wave2/generic_use.rs
wc -l /tmp/wave2/generic_use.rs
```

期望 `25`（23 条 `use`，其中 2 条跨行占 2 行）。

- [ ] **Step 2: 确认 `cargo fix` 可用**

```bash
cd yi-agent-rs
cargo fix -p yi-agent-app-server --allow-dirty --allow-staged --offline 2>&1 | tail -5
```

期望 `Finished`（无待修项时）或 `Fixed ... (N fixes)`；**不允许出现 `error[E`**。若报 `no VCS found`，说明不在 git 目录内，回到 worktree 根重试。

- [ ] **Step 3: 粘贴 + `cargo fix` 闭环，并加守卫**

每个从 `server.rs` 搬出的新文件，按此顺序：写 `//!` 文档头 → 粘贴 `/tmp/wave2/generic_use.rs` 全文 → 追加搬入的项 → 跑 `cargo fix` → 用守卫脚本确认**只有 import 被改动**。

先装好守卫脚本 `/tmp/wave2/usecheck.py`（**已实测 9 个场景**：删单行 `use`、删跨行 `use` 续行、删整条跨行 `use`、重排 `use`、新增 `mod` 声明（配 `--allow-mod`）均应通过；去 `mut`、删 `let`、改函数签名、无 `--allow-mod` 时增删 `mod` 应拦截）：

```python
#!/usr/bin/env python3
"""守卫: 证明 cargo fix 只动了 import。

原理比解析 diff 更可靠: 把「基线版」和「当前版」的**所有 use 语句整体剥掉**,
再逐行比较剩下的代码。若完全相同, 则改动必然只发生在 use 语句里。

用法: usecheck.py --ref HEAD --root <repo-root> <file> [<file>...]
"""
import argparse, pathlib, subprocess, sys

def strip_uses(text, allow_mod=False):
    """删掉所有 use 语句(支持跨行), 并压缩连续空行。

    allow_mod=True 时也一并删掉 `mod xxx;` 声明 —— 拆分模块时 mod.rs 必须新增
    声明, 那是预期改动, 不该被守卫当成「cargo fix 动了别的东西」。
    """
    import re
    out, i, lines = [], 0, text.splitlines()
    while i < len(lines):
        if lines[i].startswith('use '):
            while i < len(lines) and not lines[i].rstrip().endswith(';'):
                i += 1
            i += 1
            continue
        if allow_mod and re.match(r'^\s*(pub(\([a-z]+\))?\s+)?mod\s+\w+\s*;$', lines[i]):
            i += 1
            continue
        out.append(lines[i].rstrip())
        i += 1
    # 压缩连续空行, 避免删掉 use 后空行数量差异误报
    squeezed, prev_blank = [], False
    for l in out:
        if l == '':
            if not prev_blank:
                squeezed.append(l)
            prev_blank = True
        else:
            squeezed.append(l)
            prev_blank = False
    return squeezed

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--ref', default='HEAD')
    ap.add_argument('--root', required=True)
    ap.add_argument('--allow-mod', action='store_true',
                    help='同时忽略 mod 声明的增删(拆分模块时用)')
    ap.add_argument('files', nargs='+')
    a = ap.parse_args()
    root = pathlib.Path(a.root)
    bad = 0
    for f in a.files:
        rel = str(pathlib.Path(f).resolve().relative_to(root.resolve()))
        old = subprocess.run(['git', '-C', str(root), 'show', f'{a.ref}:{rel}'],
                             capture_output=True, text=True)
        if old.returncode != 0:
            print(f'– {rel}: 基线不存在(新文件), 跳过比较'); continue
        cur = (root / rel).read_text()
        o, c = strip_uses(old.stdout, a.allow_mod), strip_uses(cur, a.allow_mod)
        if o == c:
            print(f'✓ {rel}: 除 import 外零改动')
            continue
        bad += 1
        import difflib
        d = list(difflib.unified_diff(o, c, 'baseline(no-use)', 'current(no-use)', lineterm='', n=1))
        print(f'✗ {rel}: import 之外还有 {sum(1 for l in d if l[:1] in "+-" and not l.startswith(("+++","---")))} 行改动',
              file=sys.stderr)
        for l in d[:14]:
            print('    ' + l, file=sys.stderr)
    return 1 if bad else 0

if __name__ == '__main__':
    sys.exit(main())
```

用法（`--allow-mod` 只在拆分出新的 `mod xxx;` 声明时才加）：

```bash
cd yi-agent-rs
cargo fix -p yi-agent-app-server --allow-dirty --allow-staged --offline 2>&1 | tail -5

python3 /tmp/wave2/usecheck.py --root . \
  --allow-mod \
  crates/yi-agent-app-server/src/server/runtime_binding.rs
```

期望输出 `OK ...: 除 import 外零改动`。

⚠️ **为什么必须加守卫**：`cargo fix` 会应用**所有** MachineApplicable 建议，不限于删除多余 import（例如还会去掉多余的 `mut`、多余的 `let`）。这些改动本身无害，但重构提交里混入无关 diff 会让 review 和二分定位变难。守卫脚本会把它们抓出来。


- [ ] **Step 4: `use` 头模板（后续 Task 照抄）**

```rust
//! <一行说明>

<粘贴 /tmp/wave2/generic_use.rs 全文，随后 cargo fix 裁剪>

<搬入的项>
```

对 `server/**/mod.rs`：同样粘贴 `use` 块后裁剪，再追加 `mod xxx;` 声明。

**禁止 `use super::*`**：文件从 `server.rs` 搬到 `server/transport/mod.rs` 后，`super::` 指向的父模块从 `crate`（或 `server`）变成了另一个模块，静默改变含义。显式 `use` 才能让编译器帮你抓出搬错的引用。同理，新文件里**不要**用 `super::` 指向原 `server.rs` 的项——那些项已经搬走了，要么同组搬进同一文件，要么走 `crate::server::X` 的 re-export。

---

## Task 1: 验证搬移机制（`runtime_binding` 组，9 项 / 153 行）

最小一组，用最小代价验证「搬顶层项 + `pub(crate) use` re-export + `crate::server::X` 对 `rpc/*` 透明」这条链路。

**Files:**
- Create: `yi-agent-app-server/src/server/runtime_binding.rs`
- Modify: `yi-agent-app-server/src/server.rs`

**Interfaces:**
- Consumes: Task 0 的 worktree、基线、`/tmp/wave2/extract.py`
- Produces: 机制模板（后续 11 组照抄）：`src/server/<组名>.rs` 承载顶层项，`server.rs` 里 `pub(crate) mod <组名>;` + `pub(crate) use <组名>::{...};`

本组 **9 项 / 153 行**（`extract.py --list` 的精确输出）：

| 声明行 | 类型 | 名称 | 行数 | 当前可见性 | 需改 |
|---|---|---|---|---|---|
| L521 | fn | `home_dir` | 5 | `pub(crate)` | — |
| L530 | fn | `instance_home_dir` | 7 | `pub(crate)` | — |
| L539 | fn | `skills_home_dir` | 5 | `pub(crate)` | — |
| L626 | fn | `project_key` | 35 | private | **`pub(crate)`** ⚠️ 外部引用 |
| L2187 | `thread_local!` | `SESSION_PRESERVE_THINKING` | 11 | private | **`pub(crate)`** |
| L2205 | fn | `with_session_preserve_thinking` | 14 | `pub(crate)` | — |
| L2215 | fn | `current_session_preserve_thinking` | 4 | private | **`pub(crate)`** |
| L2225 | fn | `production_factory` | 59 | `pub(crate)` | — |
| L7552 | fn | `extract_prompt` | 13 | `pub(crate)` | — |

⚠️ **`SESSION_PRESERVE_THINKING` 是本组最需要注意的项**：它是 `thread_local!` 块（11 行），不是函数。`with_session_preserve_thinking` 与 `current_session_preserve_thinking` 都读写它，三者必须搬进**同一个文件**（否则读写函数要跨模块戳 `LocalKey`，虽可行但没必要）。

**已用最小工程实测确认**（4 个 `thread_local!` 场景全覆盖）：`thread_local!` 生成的 `LocalKey` **可以**被 `pub(crate) use` re-export，跨模块访问、经 re-export 路径访问、直接子模块路径访问三者都正常工作：

```rust
// sub.rs
thread_local! {
    pub(crate) static SESSION_X: std::cell::RefCell<Option<i32>> =
        const { std::cell::RefCell::new(None) };
}

// main.rs
pub(crate) mod sub;
pub(crate) use sub::SESSION_X;          // 编译通过
SESSION_X.with(|c| *c.borrow())        // 经 re-export 访问正常
```

（早先版本误写为"不能简单 re-export"，已实测推翻。）唯一要注意的是**提权语法**：`pub(crate)` 必须写在 `thread_local!` 块内的 `static` 之前，`promote.py` 因缩进而匹配不到，需手工改——见附录 B 末尾。

- [ ] **Step 1: 用 `extract.py` 建模块并搬 9 项**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
python3 /tmp/wave2/extract.py --src src/server.rs \
  --dst src/server/runtime_binding.rs \
  --names home_dir,instance_home_dir,skills_home_dir,project_key,SESSION_PRESERVE_THINKING,with_session_preserve_thinking,current_session_preserve_thinking,production_factory,extract_prompt \
  --header /tmp/wave2/hdr_runtime_binding.txt \
  --move
```

`/tmp/wave2/hdr_runtime_binding.txt` 内容：

```
//! 运行期绑定与进程级路径解析。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 1）。函数体未改，只把可见性
//! 收紧到 `pub(crate)`，并由 `server.rs` 统一 re-export 回 `crate::server::X`，
//! 使 `rpc/*` 与 `server/tests/*` 的既有路径继续解析。
```

期望输出：`moved 9 items / 153 lines from src/server.rs -> src/server/runtime_binding.rs`。

- [ ] **Step 2: 断言 `server.rs` 行数与剩余项**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
wc -l < src/server.rs       # 期望约 7428
python3 /tmp/wave2/extract.py --list src/server.rs | head -1   # 期望 188 items
```

188 = 197 − 9。**若不等，说明 `extract.py` 把相邻的 `mod`/类型一起吃了，立即回滚**：

```bash
cd yi-agent-rs && git checkout -- crates/yi-agent-app-server/src/
```

- [ ] **Step 3: 在 `server.rs` 里加 mod 声明与 re-export**

用 `edit` 工具，锚定 `const PERMISSION_TIMEOUT` 之前插入：

```rust
pub(crate) mod runtime_binding;

pub(crate) use runtime_binding::{
    extract_prompt, home_dir, instance_home_dir, production_factory, project_key, skills_home_dir,
    SESSION_PRESERVE_THINKING, with_session_preserve_thinking, current_session_preserve_thinking,
};
```

⚠️ `thread_local!` 生成的 `SESSION_PRESERVE_THINKING` 是 `LocalKey`。`pub(crate) use` 可以 re-export 它（它是 `static` 项，不是宏），但**必须确认搬移后它仍是 `pub(crate)`**：

```bash
cd yi-agent-rs/crates/yi-agent-app-server
grep -n "thread_local\|static SESSION_PRESERVE_THINKING" src/server/runtime_binding.rs
```

期望看到 `pub(crate) static SESSION_PRESERVE_THINKING` 或 `thread_local! { pub(crate) static ... }` 形式。若还是 private，见 Step 4。

- [ ] **Step 4: 编译，逐个修 `use` 与可见性**

```bash
cd yi-agent-rs
cargo build -p yi-agent-app-server 2>&1 | grep -E "^(error|warning: unused)" | head -30
```

常见错误与修法：
- `cannot find function X in module crate::server` → `pub(crate) use` 列表少了名字，补上。
- `function X is private` / `static X is private` → 该 private 项还没提权。**注意 `thread_local!` 的提权语法是把 `pub(crate)` 写在 `static` 之前**：

  ```rust
  thread_local! {
      pub(crate) static SESSION_PRESERVE_THINKING: std::cell::RefCell<Option<bool>> =
          const { std::cell::RefCell::new(None) };
  }
  ```
- `unused import` → `server.rs` 顶部有因此不再需要的 `use`，删掉那一行。

反复跑到 `cargo build` 无 error。

- [ ] **Step 5: 跑测试与 clippy，确认 1,166 不变**

```bash
cd yi-agent-rs
cargo test -p yi-agent-app-server 2>&1 | tail -5
cargo test -p yi-agent-app-server -- --list 2>/dev/null | grep -c ': test$'
cargo clippy -p yi-agent-app-server --all-targets -- -D warnings 2>&1 | tail -3
```

期望：测试全绿；计数 `1166`；clippy 无 warning 无 error。

- [ ] **Step 6: 校验没有可见性泄露**

```bash
cd yi-agent-rs
git diff -U0 -- 'crates/yi-agent-app-server/src/server*' \
  | grep -E '^\+.*\bpub(\(crate\))? (async )?(fn|struct|const|static|type|enum)\b' | grep -v 'pub(crate)'
```

期望**无输出**（没有新增 `pub`）。注意 regex 必须覆盖 `const`/`static`/`struct`/`type`/`enum`，只查 `fn` 会漏检（实测这条陷阱让早先版本漏掉了 4 个 const 提权）。

- [ ] **Step 7: 确认协议与公共 API 未动**

```bash
cd yi-agent-rs
cargo test -p yi-agent-app-server export_bindings 2>&1 | tail -3
git diff --exit-code -- ../desktop/src/lib/generated && echo "OK: 协议未动"
git diff --exit-code -- crates/yi-agent-app-server/src/lib.rs && echo "OK: lib.rs 未动"
```

- [ ] **Step 8: 提交**

```bash
cd yi-agent-rs
cargo fmt --all
git add -A
git commit -m "refactor(app-server): 抽出 server/runtime_binding（9 项）"
```

---

## Task 2: `rpc_io` + `watchers` + `task_bridge` 三小组（22 项 / 433 行）

三个纯工具组，无结构风险，一次做完以省提交开销。

**Files:**
- Create: `yi-agent-app-server/src/server/rpc_io.rs`
- Create: `yi-agent-app-server/src/server/watchers.rs`
- Create: `yi-agent-app-server/src/server/task_bridge.rs`
- Modify: `yi-agent-app-server/src/server.rs`

**Interfaces:**
- Consumes: Task 1 的机制
- Produces: `crate::server::{ok_response, err_response, write_response, write_notification, write_notification_await, IMAGE_READ_CHUNK_BYTES, IMAGE_READ_MAX_BYTES, ws_client_for_device, forget_ws_device, device_registry, install_device_registry}`；`crate::server::{watch_children, watch_processes, stream_trace, children_for_thread, subagent_totals, trace_rows}`；`crate::server::{cancel_thread_children, list_task_summaries, read_task_trace, forward_to_daemon}`

⚠️ **`rpc_io` 是本块被引用最多的组**。实测 `rpc/*` 的 18 个文件与 `server/tests/` 的 3 个文件都靠这 5 个名字应答：`ok_response`（17 文件）、`err_response`（16 文件）、`write_response`（18 文件）、`write_notification`（5 文件）、`write_notification_await`（1 文件）。**漏一个就是大面积编译失败。**

| 组 | 项数 / 行数 | 需改可见性的项 |
|---|---|---|
| `rpc_io` | 12 项 / 139 行 | `DEVICE_REGISTRY`(static, private)、`device_registry`(fn, private) |
| `watchers` | 6 项 / 189 行 | `trace_rows`(fn, private) |
| `task_bridge` | 4 项 / 105 行 | 无（全 `pub(crate)`） |

精确清单（`extract.py --list` 输出）：

**`rpc_io`（12 项）**
| 声明行 | 类型 | 名称 | 行数 | 需改 |
|---|---|---|---|---|
| L313 | const | `IMAGE_READ_CHUNK_BYTES` | 3 | — (⚠️ 外部 `rpc/image.rs` 引用) |
| L317 | const | `IMAGE_READ_MAX_BYTES` | 3 | — (⚠️ 外部引用) |
| L3798 | fn | `ws_client_for_device` | 13 | — |
| L3810 | fn | `forget_ws_device` | 11 | — |
| L3822 | static | `DEVICE_REGISTRY` | 7 | **`pub(crate)`** |
| L3826 | fn | `device_registry` | 3 | **`pub(crate)`** |
| L3832 | fn | `install_device_registry` | 6 | — |
| L3837 | fn | `ok_response` | 8 | — |
| L3846 | fn | `err_response` | 8 | — |
| L3855 | fn | `write_response` | 29 | — |
| L3885 | fn | `write_notification` | 30 | — |
| L3928 | fn | `write_notification_await` | 18 | — |

**`watchers`（6 项）**
| 声明行 | 类型 | 名称 | 行数 | 需改 |
|---|---|---|---|---|
| L981 | fn | `watch_children` | 50 | — |
| L1032 | fn | `watch_processes` | 38 | — |
| L1071 | fn | `stream_trace` | 52 | — |
| L1124 | fn | `children_for_thread` | 32 | — |
| L1157 | fn | `subagent_totals` | 14 | — |
| L943 | fn | `trace_rows` | 3 | **`pub(crate)`** |

**`task_bridge`（4 项）**
| 声明行 | 类型 | 名称 | 行数 | 需改 |
|---|---|---|---|---|
| L849 | fn | `cancel_thread_children` | 47 | — |
| L897 | fn | `list_task_summaries` | 21 | — |
| L919 | fn | `read_task_trace` | 13 | — |
| L956 | fn | `forward_to_daemon` | 24 | — |

- [ ] **Step 1: 搬 `rpc_io`（12 项）**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
python3 /tmp/wave2/extract.py --src src/server.rs --dst src/server/rpc_io.rs \
  --names IMAGE_READ_CHUNK_BYTES,IMAGE_READ_MAX_BYTES,ws_client_for_device,forget_ws_device,DEVICE_REGISTRY,device_registry,install_device_registry,ok_response,err_response,write_response,write_notification,write_notification_await \
  --header /tmp/wave2/hdr_rpc_io.txt --move
```

`/tmp/wave2/hdr_rpc_io.txt`：

```
//! JSON-RPC 响应/通知的写出，以及 WS 设备登记表。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 2）。被 `rpc/*` 的 18 个文件与
//! `server/tests/` 的 3 个文件引用，故 `server.rs` 必须完整 re-export。
```

⚠️ `DEVICE_REGISTRY` 是 `static`，`device_registry()` 是读它的访问器。两者必须同文件（实测相邻 L3822/L3826）。

- [ ] **Step 2: 搬 `watchers`（6 项）与 `task_bridge`（4 项）**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
python3 /tmp/wave2/extract.py --src src/server.rs --dst src/server/watchers.rs \
  --names watch_children,watch_processes,stream_trace,children_for_thread,subagent_totals,trace_rows \
  --header /tmp/wave2/hdr_watchers.txt --move

python3 /tmp/wave2/extract.py --src src/server.rs --dst src/server/task_bridge.rs \
  --names cancel_thread_children,list_task_summaries,read_task_trace,forward_to_daemon \
  --header /tmp/wave2/hdr_task_bridge.txt --move
```

两个 header：

```
//! 子 agent / 进程 / trace 三类观察流的轮询与汇总。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 2）。
```

```
//! 与 daemon 的 IPC 转发，以及子 agent 任务摘要/轨迹的读取。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 2）。
```

- [ ] **Step 3: 断言总数**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
python3 /tmp/wave2/extract.py --list src/server.rs | head -1
wc -l < src/server.rs
```

期望 `166 items`（197 − 9 − 22）与约 `6,973` 行。

- [ ] **Step 4: 改 3 个 private 为 `pub(crate)`**

`rpc_io.rs` 里的 `DEVICE_REGISTRY`、`device_registry`，`watchers.rs` 里的 `trace_rows`。用附录 B 的 `promote.py`，或手工把 `static DEVICE_REGISTRY` → `pub(crate) static DEVICE_REGISTRY`、`fn device_registry` → `pub(crate) fn device_registry`、`fn trace_rows` → `pub(crate) fn trace_rows`。

- [ ] **Step 5: 加三个 mod 与 re-export**

在 Task 1 写的 `};` 之后追加：

```rust
pub(crate) mod rpc_io;
pub(crate) mod task_bridge;
pub(crate) mod watchers;

pub(crate) use rpc_io::{
    device_registry, err_response, forget_ws_device, install_device_registry, ok_response,
    write_notification, write_notification_await, write_response, ws_client_for_device, DEVICE_REGISTRY,
    IMAGE_READ_CHUNK_BYTES, IMAGE_READ_MAX_BYTES,
};
pub(crate) use task_bridge::{
    cancel_thread_children, forward_to_daemon, list_task_summaries, read_task_trace,
};
pub(crate) use watchers::{
    children_for_thread, stream_trace, subagent_totals, trace_rows, watch_children, watch_processes,
};
```

- [ ] **Step 6: 编译并修错**

```bash
cd yi-agent-rs
cargo build -p yi-agent-app-server 2>&1 | grep -E "^error" -A3 | head -40
```

本组被引用最多。若报 `cannot find function ok_response`，说明 Step 5 的 `use` 列表漏名。

- [ ] **Step 7: 测试 + clippy + 计数**

```bash
cd yi-agent-rs
cargo test -p yi-agent-app-server 2>&1 | tail -5
cargo test -p yi-agent-app-server -- --list 2>/dev/null | grep -c ': test$'   # 期望 1166
cargo clippy -p yi-agent-app-server --all-targets -- -D warnings 2>&1 | tail -3
```

- [ ] **Step 8: 提交**

```bash
cd yi-agent-rs
cargo fmt --all
git add -A
git commit -m "refactor(app-server): 抽出 rpc_io / watchers / task_bridge（22 项）"
```

---

## Task 3: `thread_index` 组（29 项 / 580 行，C3 依赖最重）

这是 C3 的最大依赖方：C3 需要的 28 个 `crate::server::` 函数里，有 **11 个**在这一组（`attach_delegation` / `collect_pinned` / `item_id` / `require_known_thread` / `require_thread_id` / `resolve_thread_cwd` / `store_lookup` / `thread_cwd` / `thread_summary_json` / `thread_summary_json_with_status` / `worktree_json`）。

**Files:**
- Create: `yi-agent-app-server/src/server/thread_index.rs`
- Modify: `yi-agent-app-server/src/server.rs`

**Interfaces:**
- Consumes: Task 1/2 的机制
- Produces: 上述 11 个名字 + 组内其余 18 个，全部从 `crate::server::` 可达

**29 项 / 580 行**，含 3 个组内类型（`Activation` / `SyncHint` / `SyncAction`）与 1 个 const（`TAG_NAME_MAX_LEN`）：

| 声明行 | 类型 | 名称 | 行数 | 需改 |
|---|---|---|---|---|
| L668 | struct | `Activation` | 5 | **`pub(crate)`** |
| L235 | const | `TAG_NAME_MAX_LEN` | 6 | — (⚠️ 外部引用) |
| L4335 | struct | `SyncHint` | 10 | **`pub(crate)`** |
| L4347 | enum | `SyncAction` | 11 | **`pub(crate)`**（⚠️ 外部引用） |
| L5743 | fn | `find_thread_dir` | 8 | **`pub(crate)`** |
| L5752 | fn | `store_for` | 9 | **`pub(crate)`** |
| L5762 | fn | `store_lookup` | 13 | — |
| L5776 | fn | `thread_cwd` | 28 | — |
| L5805 | fn | `image_media_type` | 14 | — |
| L5820 | fn | `item_id` | 10 | — |
| L656 | fn | `attach_delegation` | 126 | — |
| L793 | fn | `require_known_thread` | 22 | — |
| L816 | fn | `socket_for_thread` | 22 | — |
| L4065 | fn | `collect_pinned` | 22 | — |
| L4088 | fn | `worktree_json` | 27 | — |
| L4139 | fn | `plan_sync` | 28 | — |
| L4168 | fn | `parse_resume_hints` | 33 | — |
| L4202 | fn | `record_last_item_id` | 8 | **`pub(crate)`** |
| L4211 | fn | `thread_summary_json` | 7 | — |
| L4219 | fn | `thread_summary_json_with_status` | 40 | — |
| L931 | fn | `snapshot_for_watch` | 9 | — |
| L4431 | fn | `saturate_u32` | 7 | — (保持 `pub`) |
| L4035 | fn | `update_status` | 21 | **`pub(crate)`** |
| L4057 | fn | `thread_status` | 7 | **`pub(crate)`** |
| L4283 | fn | `decision_label` | 13 | **`pub(crate)`** |
| L4297 | fn | `parse_client_decision` | 15 | **`pub(crate)`** |
| L3916 | fn | `is_load_bearing` | 11 | **`pub(crate)`** |
| L7507 | fn | `resolve_thread_cwd` | 28 | — |
| L7536 | fn | `require_thread_id` | 20 | — |

⚠️ **`Activation`、`SyncHint`、`SyncAction` 是组内私有类型**（实测无外部引用），必须随函数一起搬。如果只搬函数不搬类型，会报 `cannot find type SyncAction`。附录 A 按名字搬，名字列表里必须包含它们。

- [ ] **Step 1: 搬 29 项**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
python3 /tmp/wave2/extract.py --src src/server.rs --dst src/server/thread_index.rs \
  --names Activation,TAG_NAME_MAX_LEN,SyncHint,SyncAction,find_thread_dir,store_for,store_lookup,thread_cwd,image_media_type,item_id,attach_delegation,require_known_thread,socket_for_thread,collect_pinned,worktree_json,plan_sync,parse_resume_hints,record_last_item_id,thread_summary_json,thread_summary_json_with_status,snapshot_for_watch,saturate_u32,update_status,thread_status,decision_label,parse_client_decision,is_load_bearing,resolve_thread_cwd,require_thread_id \
  --header /tmp/wave2/hdr_thread_index.txt --move
```

header：

```
//! 会话索引、store 定位、线程摘要与客户端决策解析。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 3）。本组被 `rpc/thread.rs` 直接
//! 引用 11 个函数，`server.rs` 的 re-export 必须完整。
```

- [ ] **Step 2: 断言总数**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
python3 /tmp/wave2/extract.py --list src/server.rs | head -1   # 期望 137 items
```

- [ ] **Step 3: 改 8 个 private 函数 + 3 个 private 类型为 `pub(crate)`**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
python3 /tmp/wave2/promote.py --file src/server/thread_index.rs \
  --names find_thread_dir,store_for,update_status,thread_status,record_last_item_id,is_load_bearing,parse_client_decision,decision_label,Activation,SyncHint,SyncAction
```

（`promote.py` 见附录 B；它会只加 `pub(crate)`、不改动其它字符，并对每个名字断言「恰好命中 1 次」。）

- [ ] **Step 4: 加 mod 与 re-export（29 个全列）**

```rust
pub(crate) mod thread_index;

pub(crate) use thread_index::{
    attach_delegation, collect_pinned, decision_label, find_thread_dir, image_media_type, is_load_bearing,
    item_id, parse_client_decision, parse_resume_hints, plan_sync, record_last_item_id,
    require_known_thread, require_thread_id, resolve_thread_cwd, saturate_u32, snapshot_for_watch,
    socket_for_thread, store_for, store_lookup, thread_cwd, thread_status, thread_summary_json,
    thread_summary_json_with_status, update_status, worktree_json, Activation, SyncAction, SyncHint,
    TAG_NAME_MAX_LEN,
};
```

- [ ] **Step 5: 编译修错**

```bash
cd yi-agent-rs
cargo build -p yi-agent-app-server 2>&1 | grep -E "^error" -A3 | head -40
```

- [ ] **Step 6: 断言 C3 需要的 11 个名字全部可达**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
for f in attach_delegation collect_pinned item_id require_known_thread require_thread_id \
         resolve_thread_cwd store_lookup thread_cwd thread_summary_json \
         thread_summary_json_with_status worktree_json; do
  sed -n '/pub(crate) use thread_index::/,/};/p' src/server.rs | grep -q "\b$f\b" \
    && echo "OK   $f" || echo "MISS $f"
done
```

期望 11 行全 `OK`。

- [ ] **Step 7: 测试 + clippy + 计数 + 提交**

```bash
cd yi-agent-rs
cargo test -p yi-agent-app-server 2>&1 | tail -5
cargo test -p yi-agent-app-server -- --list 2>/dev/null | grep -c ': test$'   # 期望 1166
cargo clippy -p yi-agent-app-server --all-targets -- -D warnings 2>&1 | tail -3
cargo fmt --all
git add -A
git commit -m "refactor(app-server): 抽出 server/thread_index（29 项）"
```

---

## Task 4: `watchman` + `plugin_bridge` + `migration`（26 项 / 525 行）

**Files:**
- Create: `yi-agent-app-server/src/server/watchman.rs`
- Create: `yi-agent-app-server/src/server/plugin_bridge.rs`
- Create: `yi-agent-app-server/src/server/migration.rs`
- Modify: `yi-agent-app-server/src/server.rs`

**Interfaces:**
- Consumes: Task 1 机制
- Produces: `crate::server::{watchman_home_from, production_watchman_install, production_watchman_uninstall, ensure_watchman_installed, project_has_plugin_manifest, should_evict, wait_for_plugin_query_socket_within, agent_trace_row, skills_home_from}`；`crate::server::{plugin_query, plugin_query_self, list_installed_plugins, project_arg, plugin_scope, spawn_resident_watch_loop, plugin_query_socket_ready, wait_for_plugin_query_socket, PluginQueryError, PluginSummary, PLUGIN_READY_TIMEOUT}`；`crate::server::{migrate_legacy_model_keys_on_startup, migrate_legacy_resident_requests_on_startup, migrate_legacy_registry_file}`

**`watchman`（10 项 / 116 行）**
| 声明行 | 类型 | 名称 | 行数 | 需改 |
|---|---|---|---|---|
| L82 | fn | `should_evict` | 7 | — |
| L90 | fn | `project_has_plugin_manifest` | 16 | — |
| L525 | fn | `watchman_home_from` | 3 | — |
| L543 | fn | `skills_home_from` | 16 | — |
| L566 | fn | `production_watchman_install` | 15 | — |
| L582 | fn | `production_watchman_uninstall` | 8 | — |
| L591 | fn | `ensure_watchman_installed` | 22 | — |
| L252 | const | `PLUGIN_READY_POLL` | 2 | — |
| L291 | fn | `wait_for_plugin_query_socket_within` | 19 | — |
| L935 | fn | `agent_trace_row` | 8 | **`pub(crate)`** |

**`plugin_bridge`（13 项 / 324 行，含 2 个 impl 块）**
| 声明行 | 类型 | 名称 | 行数 | 需改 |
|---|---|---|---|---|
| L242 | const | `PLUGIN_READY_TIMEOUT` | 9 | — |
| L255 | fn | `plugin_query_socket_ready` | 24 | — |
| L280 | fn | `wait_for_plugin_query_socket` | 10 | — |
| L1662 | fn | `plugin_query` | 86 | — |
| L1749 | fn | `plugin_query_self` | 78 | — |
| L1828 | fn | `plugin_scope` | 15 | — |
| L1875 | fn | `project_arg` | 15 | — |
| L1841 | fn | `list_installed_plugins` | 33 | — |
| L1907 | fn | `spawn_resident_watch_loop` | 18 | — |
| L1623 | struct | `PluginQueryError` | 11 | **`pub(crate)`** |
| L1635 | impl | `PluginQueryError`（inherent） | 8 | **`pub(crate)`** |
| L1644 | impl | `PluginQueryError`（`Display`） | 5 | — 无需改（trait impl 的可见性跟随类型） |
| L1816 | struct | `PluginSummary` | 12 | **`pub(crate)`** |

⚠️ **`impl` 块不需要单独写 `pub(crate)`**——`impl` 块本身没有可见性修饰符，其方法的可见性由声明决定。`promote.py` 对 `impl` 项会**跳过并报告**，这是正确的。只有 `struct`/`enum` 需要提权。

**`migration`（3 项 / 85 行）**
| 声明行 | 类型 | 名称 | 行数 | 需改 |
|---|---|---|---|---|
| L2340 | fn | `migrate_legacy_model_keys_on_startup` | 30 | — |
| L2371 | fn | `migrate_legacy_resident_requests_on_startup` | 24 | — |
| L2416 | fn | `migrate_legacy_registry_file` | 31 | **`pub(crate)`** |

⚠️ **`wait_for_plugin_query_socket_within` 只有 19 行**（spec 里写的 229 行是错的——那 229 行实际是「该函数到下一个函数之间」的行区间，把 11 个类型和 const 都算进去了）。这是本次核对最重要的修正之一：**所有「函数行数」都必须以 `extract.py --list` 为准**。

- [ ] **Step 1: 搬 `watchman`（10 项）**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
python3 /tmp/wave2/extract.py --src src/server.rs --dst src/server/watchman.rs \
  --names should_evict,project_has_plugin_manifest,watchman_home_from,skills_home_from,production_watchman_install,production_watchman_uninstall,ensure_watchman_installed,PLUGIN_READY_POLL,wait_for_plugin_query_socket_within,agent_trace_row \
  --header /tmp/wave2/hdr_watchman.txt --move
```

header：

```
//! watchman 值守的安装/卸载与就绪探测，以及项目级插件清单判定。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 4）。
```

- [ ] **Step 2: 搬 `plugin_bridge`（13 项）**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
python3 /tmp/wave2/extract.py --src src/server.rs --dst src/server/plugin_bridge.rs \
  --names PLUGIN_READY_TIMEOUT,plugin_query_socket_ready,wait_for_plugin_query_socket,plugin_query,plugin_query_self,plugin_scope,project_arg,list_installed_plugins,spawn_resident_watch_loop,PluginQueryError,PluginSummary \
  --header /tmp/wave2/hdr_plugin_bridge.txt --move
```

⚠️ `--names` 里 11 个是「名字」，另 2 个 `impl` 块会被 `extract.py` **按 impl 头里出现的被搬类型名自动带上**。脚本会打印 `impl items pulled in: N`——期望 `2`。若为 0，说明 `PluginQueryError` 的 impl 块没跟上，会导致 `impl` 丢失，**必须停下检查**。

header：

```
//! 插件查询通道：socket 就绪探测、查询本身、以及常驻 watch 循环。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 4）。`plugin/*` 的 `handle` 在
//! `rpc/plugin.rs`，本文件只放**宿主侧**的查询原语。
```

- [ ] **Step 3: 搬 `migration`（3 项）**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
python3 /tmp/wave2/extract.py --src src/server.rs --dst src/server/migration.rs \
  --names migrate_legacy_model_keys_on_startup,migrate_legacy_resident_requests_on_startup,migrate_legacy_registry_file \
  --header /tmp/wave2/hdr_migration.txt --move
```

header：

```
//! 启动期的历史数据迁移（模型键、常驻登记、registry 文件）。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 4）。
```

⚠️ `migrate_legacy_registry_file` 后面紧跟 `mod legacy_migration_tests;`（L2418）。`extract.py` 不会吞 `mod`（它以 `mod` 为独立项边界），搬完请确认该 `mod` 仍在 `server.rs`：

```bash
grep -c 'mod legacy_migration_tests;' src/server.rs   # 期望 1
```

- [ ] **Step 4: 断言总数**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
python3 /tmp/wave2/extract.py --list src/server.rs | head -1   # 期望 111 items
```

- [ ] **Step 5: 改 4 项为 `pub(crate)`**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
python3 /tmp/wave2/promote.py --file src/server/watchman.rs --names agent_trace_row
python3 /tmp/wave2/promote.py --file src/server/plugin_bridge.rs --names PluginQueryError,PluginSummary
python3 /tmp/wave2/promote.py --file src/server/migration.rs --names migrate_legacy_registry_file
```

- [ ] **Step 6: 加三个 mod 与 re-export**

```rust
pub(crate) mod migration;
pub(crate) mod plugin_bridge;
pub(crate) mod watchman;

pub(crate) use migration::{
    migrate_legacy_model_keys_on_startup, migrate_legacy_registry_file,
    migrate_legacy_resident_requests_on_startup,
};
pub(crate) use plugin_bridge::{
    list_installed_plugins, plugin_query, plugin_query_self, plugin_query_socket_ready, plugin_scope,
    project_arg, spawn_resident_watch_loop, wait_for_plugin_query_socket, PluginQueryError,
    PluginSummary, PLUGIN_READY_TIMEOUT,
};
pub(crate) use watchman::{
    agent_trace_row, ensure_watchman_installed, production_watchman_install,
    production_watchman_uninstall, project_has_plugin_manifest, should_evict, skills_home_from,
    wait_for_plugin_query_socket_within, watchman_home_from, PLUGIN_READY_POLL,
};
```

- [ ] **Step 7: 编译修错、测试、clippy、计数、提交**

```bash
cd yi-agent-rs
cargo build -p yi-agent-app-server 2>&1 | grep -E "^error" -A3 | head -40
cargo test -p yi-agent-app-server 2>&1 | tail -5
cargo test -p yi-agent-app-server -- --list 2>/dev/null | grep -c ': test$'   # 期望 1166
cargo clippy -p yi-agent-app-server --all-targets -- -D warnings 2>&1 | tail -3
cargo fmt --all
git add -A
git commit -m "refactor(app-server): 抽出 watchman / plugin_bridge / migration（26 项）"
```

---

## Task 5: `assembly` 组（18 项 / 510 行，含 3 个 `thread_local!`；第 4 个在 Task 1）

**Files:**
- Create: `yi-agent-app-server/src/server/assembly.rs`
- Modify: `yi-agent-app-server/src/server.rs`

**Interfaces:**
- Consumes: Task 1 机制
- Produces: `crate::server::{build_runtime_tooling, register_theme_tool, registry_with_theme_tool, rebuild_thread_agent_with_theme, wrap_for_delegation, attach_cwd_runtime, detach_unused_runtimes, attached_runtimes, with_session_model_ref, current_session_model_ref, with_session_reasoning_effort, current_session_reasoning_effort, with_session_extra_roots, current_session_extra_roots, RuntimeTooling}`

**18 项 / 510 行**，其中 **3 个 `thread_local!` 块（28 行）与 1 个 struct `RuntimeTooling`**。

> `server.rs` 总共有 **4 个** `thread_local!` 块。本组含 3 个（`SESSION_MODEL_REF` / `SESSION_REASONING_EFFORT` / `SESSION_EXTRA_ROOTS`）；第 4 个 `SESSION_PRESERVE_THINKING` 在 Task 1 的 `runtime_binding` 组。**别把总数当成 3。**

| 声明行 | 类型 | 名称 | 行数 | 需改 |
|---|---|---|---|---|
| L367 | struct | `RuntimeTooling` | 13 | **`pub(crate)`** |
| L1209 | fn | `attach_cwd_runtime` | 68 | **`pub(crate)`** |
| L1288 | fn | `detach_unused_runtimes` | 17 | — |
| L1306 | fn | `attached_runtimes` | 9 | **`pub(crate)`** |
| L1322 | fn | `build_runtime_tooling` | 191 | **`pub(crate)`** (⚠️ 外部引用) |
| L1508 | fn | `register_theme_tool` | 11 | **`pub(crate)`** |
| L1520 | fn | `registry_with_theme_tool` | 12 | **`pub(crate)`** |
| L1533 | fn | `rebuild_thread_agent_with_theme` | 56 | **`pub(crate)`** |
| L1590 | fn | `wrap_for_delegation` | 42 | **`pub(crate)`** |
| L2087 | `thread_local!` | `SESSION_MODEL_REF` | 7 | **`pub(crate)`** |
| L2095 | fn | `with_session_model_ref` | 12 | — |
| L2108 | fn | `current_session_model_ref` | 4 | **`pub(crate)`** |
| L2113 | `thread_local!` | `SESSION_REASONING_EFFORT` | 10 | **`pub(crate)`** |
| L2124 | fn | `with_session_reasoning_effort` | 14 | — |
| L2139 | fn | `current_session_reasoning_effort` | 5 | **`pub(crate)`** |
| L2145 | `thread_local!` | `SESSION_EXTRA_ROOTS` | 11 | **`pub(crate)`** |
| L2157 | fn | `with_session_extra_roots` | 17 | — |
| L2175 | fn | `current_session_extra_roots` | 11 | **`pub(crate)`** |

⚠️ **`production_factory`（59 行）不在本文件**——它在 `runtime_binding.rs`（Task 1）。C4 的 spec 必须据此定位它。

⚠️ **3 个 `thread_local!` 必须与各自的 `with_*` / `current_*` 配对在同文件**。实测三对都紧密相邻（L2087-2111、L2113-2143、L2145-2185），一次搬走不会留空隙。

- [ ] **Step 1: 搬 18 项到 `src/server/assembly.rs`**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
python3 /tmp/wave2/extract.py --src src/server.rs --dst src/server/assembly.rs \
  --names RuntimeTooling,attach_cwd_runtime,detach_unused_runtimes,attached_runtimes,build_runtime_tooling,register_theme_tool,registry_with_theme_tool,rebuild_thread_agent_with_theme,wrap_for_delegation,SESSION_MODEL_REF,with_session_model_ref,current_session_model_ref,SESSION_REASONING_EFFORT,with_session_reasoning_effort,current_session_reasoning_effort,SESSION_EXTRA_ROOTS,with_session_extra_roots,current_session_extra_roots \
  --header /tmp/wave2/hdr_assembly.txt --move
```

header：

```
//! agent 的装配：runtime tooling、按主题重建、委派包装，以及四个会话上下文字段。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 5）。三对 `thread_local!` 槽位
//! （model_ref / reasoning_effort / extra_roots）与各自的读写函数同文件，
//! 避免跨模块访问 `LocalKey`。
//!
//! 注意 `production_factory` **不在**本文件——它在 `runtime_binding.rs`。
```

- [ ] **Step 2: 断言总数**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
python3 /tmp/wave2/extract.py --list src/server.rs | head -1   # 期望 93 items
wc -l < src/server.rs                                          # 期望约 5285
```

- [ ] **Step 3: 改 14 项为 `pub(crate)`**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
python3 /tmp/wave2/promote.py --file src/server/assembly.rs \
  --names RuntimeTooling,attach_cwd_runtime,attached_runtimes,build_runtime_tooling,register_theme_tool,registry_with_theme_tool,rebuild_thread_agent_with_theme,wrap_for_delegation,SESSION_MODEL_REF,current_session_model_ref,SESSION_REASONING_EFFORT,current_session_reasoning_effort,SESSION_EXTRA_ROOTS,current_session_extra_roots
```

- [ ] **Step 4: 加 mod 与 re-export（18 个全列）**

```rust
pub(crate) mod assembly;

pub(crate) use assembly::{
    attach_cwd_runtime, attached_runtimes, build_runtime_tooling, current_session_extra_roots,
    current_session_model_ref, current_session_reasoning_effort, detach_unused_runtimes,
    rebuild_thread_agent_with_theme, register_theme_tool, registry_with_theme_tool,
    with_session_extra_roots, with_session_model_ref, with_session_reasoning_effort,
    wrap_for_delegation, RuntimeTooling, SESSION_EXTRA_ROOTS, SESSION_MODEL_REF,
    SESSION_REASONING_EFFORT,
};
```

- [ ] **Step 5: 编译修错**

```bash
cd yi-agent-rs
cargo build -p yi-agent-app-server 2>&1 | grep -E "^error" -A3 | head -40
```

**特别注意**：本组涉及 `BuildAgent<'a>` 这个带生命周期的 trait object 别名。若报生命周期错误，**不要**改签名——`BuildAgent<'a>` 定义在 `rpc/mod.rs`，只是被本文件引用。

- [ ] **Step 6: 检查是否需要拆 `session_context.rs`**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
wc -l src/server/assembly.rs
```

若 **> 600**，把 3 个 `thread_local!` + 6 个 `with_*`/`current_*`（共 9 项，约 74 行）挪到 `src/server/assembly/session_context.rs`，`assembly.rs` 改成 `assembly/mod.rs` + `mod session_context; pub(crate) use session_context::*;`。
若 **≤ 600**，保持单文件，在提交信息里注明行数。

- [ ] **Step 7: 测试 + clippy + 计数 + 提交**

```bash
cd yi-agent-rs
cargo test -p yi-agent-app-server 2>&1 | tail -5
cargo test -p yi-agent-app-server -- --list 2>/dev/null | grep -c ': test$'   # 期望 1166
cargo clippy -p yi-agent-app-server --all-targets -- -D warnings 2>&1 | tail -3
cargo fmt --all
git add -A
git commit -m "refactor(app-server): 抽出 server/assembly（18 项）"
```

---

## Task 6: `turn/` 四片（24 项 / 1,435 行，需二次分片）

**Files:**
- Create: `yi-agent-app-server/src/server/turn/{mod,persist,command,title,replay}.rs`
- Modify: `yi-agent-app-server/src/server.rs`

**Interfaces:**
- Consumes: Task 1 机制
- Produces: `crate::server::{start_turn_core, start_thread_core, prepare_turn_core, persist_and_finish_turn, checkpoint, build_partial, apply_session_command, apply_session, generate_title_for_first_input, chunk_items_for_replay, interrupt_and_wait_for_persist, linked_extra_roots, ...}`

**24 项 / 1,435 行**，分片（实测行数，非估算）：

| 文件 | 项（行数） | 小计 |
|---|---|---|
| `turn/mod.rs` | `start_thread_core`(262) `prepare_turn_core`(225) `start_turn_core`(82) `PreparedTurn`(22) `TurnPrepareError`(14) `turn_prepare_rpc_error`(11) `opening_user_item`(23) `emit_item`(27) | 666 |
| `turn/persist.rs` | `persist_and_finish_turn`(120) `build_partial`(25) `checkpoint`(10) | 155 |
| `turn/command.rs` | `apply_session_command`(295) `apply_session`(19) | 314 |
| `turn/title.rs` | `generate_title_for_first_input`(74) | 74 |
| `turn/replay.rs` | `interrupt_and_wait_for_persist`(60) `settle_pending_lead_replies`(43) `chunk_items_for_replay`(29) `finished_event`(22) `route_client_response`(22) `completed_items_agent_text`(14) `lead_reply_text`(12) `linked_extra_roots`(19) `REPLAY_CHUNK_BYTES`(3) `REPLAY_CHUNK_MAX_ITEMS`(2) | 226 |

`turn/mod.rs` 666 行 < 1,000，达标。若实施后仍超 700，把 `start_thread_core`(262) 单独成 `turn/start.rs`。

需改 `pub(crate)` 的 18 项：`apply_session` `REPLAY_CHUNK_BYTES` `REPLAY_CHUNK_MAX_ITEMS` `route_client_response` `finished_event` `settle_pending_lead_replies` `build_partial` `checkpoint` `persist_and_finish_turn` `generate_title_for_first_input` `completed_items_agent_text` `lead_reply_text` `apply_session_command` `TurnPrepareError` `prepare_turn_core` `turn_prepare_rpc_error` `opening_user_item` `emit_item`

已是 `pub(crate)` 保持（6 项）：`chunk_items_for_replay` `interrupt_and_wait_for_persist` `start_thread_core` `PreparedTurn` `start_turn_core` `linked_extra_roots`

⚠️ **`checkpoint` / `persist_and_finish_turn` / `prepare_turn_core` / `settle_pending_lead_replies` 已被外部引用**（`thread_store.rs` / `session.rs` / `protocol.rs` / `image_upload.rs`）。这 4 个如果不提权，`cargo build` 立刻红。

- [ ] **Step 1: 建目录，按片面搬 5 次**

```bash
mkdir -p yi-agent-rs/crates/yi-agent-app-server/src/server/turn
```

五次调用 `extract.py`，`--dst` 与 `--names` 按下表：

| 片 | `--names` |
|---|---|
| `turn/mod.rs` | `start_thread_core,prepare_turn_core,start_turn_core,PreparedTurn,TurnPrepareError,turn_prepare_rpc_error,opening_user_item,emit_item` |
| `turn/persist.rs` | `persist_and_finish_turn,build_partial,checkpoint` |
| `turn/command.rs` | `apply_session_command,apply_session` |
| `turn/title.rs` | `generate_title_for_first_input` |
| `turn/replay.rs` | `interrupt_and_wait_for_persist,settle_pending_lead_replies,chunk_items_for_replay,finished_event,route_client_response,completed_items_agent_text,lead_reply_text,linked_extra_roots,REPLAY_CHUNK_BYTES,REPLAY_CHUNK_MAX_ITEMS` |

`turn/mod.rs` 的 header 要写成模块级文档并声明四个子片：

```
//! turn 生命周期：起轮、准备、持久化、会话命令、标题生成、重放。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 6）。本组 1,435 行，按职责分五片。

pub(crate) mod command;
pub(crate) mod persist;
pub(crate) mod replay;
pub(crate) mod title;
```

其余四片的 header 各一行模块文档即可。

- [ ] **Step 2: 断言总数**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
python3 /tmp/wave2/extract.py --list src/server.rs | head -1   # 期望 69 items
wc -l < src/server.rs                                          # 期望约 3827
```

- [ ] **Step 3: 改 18 项为 `pub(crate)`（分文件）**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
python3 /tmp/wave2/promote.py --file src/server/turn/mod.rs \
  --names prepare_turn_core,TurnPrepareError,turn_prepare_rpc_error,opening_user_item,emit_item
python3 /tmp/wave2/promote.py --file src/server/turn/persist.rs \
  --names persist_and_finish_turn,build_partial,checkpoint
python3 /tmp/wave2/promote.py --file src/server/turn/command.rs \
  --names apply_session_command,apply_session
python3 /tmp/wave2/promote.py --file src/server/turn/title.rs \
  --names generate_title_for_first_input
python3 /tmp/wave2/promote.py --file src/server/turn/replay.rs \
  --names settle_pending_lead_replies,finished_event,route_client_response,completed_items_agent_text,lead_reply_text,REPLAY_CHUNK_BYTES,REPLAY_CHUNK_MAX_ITEMS
```

- [ ] **Step 4: 加 mod 与 re-export（24 个全列）**

```rust
pub(crate) mod turn;

pub(crate) use turn::{
    apply_session, apply_session_command, build_partial, checkpoint, chunk_items_for_replay,
    completed_items_agent_text, emit_item, finished_event, generate_title_for_first_input,
    interrupt_and_wait_for_persist, lead_reply_text, linked_extra_roots, opening_user_item,
    persist_and_finish_turn, prepare_turn_core, route_client_response, settle_pending_lead_replies,
    start_thread_core, start_turn_core, turn_prepare_rpc_error, PreparedTurn, TurnPrepareError,
    REPLAY_CHUNK_BYTES, REPLAY_CHUNK_MAX_ITEMS,
};
```

并在 `turn/mod.rs` 里把子片的名字提到 `turn` 顶层（否则 `use turn::{...}` 找不到子片函数）：

```rust
pub(crate) use command::{apply_session, apply_session_command};
pub(crate) use persist::{build_partial, checkpoint, persist_and_finish_turn};
pub(crate) use replay::{
    chunk_items_for_replay, completed_items_agent_text, finished_event, lead_reply_text,
    linked_extra_roots, route_client_response, settle_pending_lead_replies, REPLAY_CHUNK_BYTES,
    REPLAY_CHUNK_MAX_ITEMS,
};
pub(crate) use title::generate_title_for_first_input;
```

- [ ] **Step 5: 编译修错**

```bash
cd yi-agent-rs
cargo build -p yi-agent-app-server 2>&1 | grep -E "^error" -A3 | head -40
```

- [ ] **Step 6: 断言分片行数达标**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
wc -l src/server/turn/*.rs | sort -rn
```

期望最大片 < 1,000。

- [ ] **Step 7: 测试 + clippy + 计数 + 提交**

```bash
cd yi-agent-rs
cargo test -p yi-agent-app-server 2>&1 | tail -5
cargo test -p yi-agent-app-server -- --list 2>/dev/null | grep -c ': test$'   # 期望 1166
cargo clippy -p yi-agent-app-server --all-targets -- -D warnings 2>&1 | tail -3
cargo fmt --all
git add -A
git commit -m "refactor(app-server): 抽出 server/turn 五片（24 项）"
```

---

## Task 7: `driver/` 四片（24 项 / 1,884 行，含 525 行 impl）

**Files:**
- Create: `yi-agent-app-server/src/server/driver/{mod,turn_driver,tick,lead,launcher}.rs`
- Modify: `yi-agent-app-server/src/server.rs`

**Interfaces:**
- Consumes: Task 1 机制
- Produces: `crate::server::{run_thread_driver, driver_tick, lead_tick_once, resolve_lead_target, take_changed_digests, snapshot_of, project_root_of, evict_idle_runtimes}`

**24 项 / 1,884 行**，分片：

| 文件 | 项（行数） | 小计 |
|---|---|---|
| `driver/turn_driver.rs` | `run_thread_driver`(725) | 725 |
| `driver/tick.rs` | `driver_tick`(213) | 213 |
| `driver/lead.rs` | `lead_tick_once`(76) `take_changed_digests`(27) `resolve_lead_target`(21) `snapshot_of`(18) `project_root_of`(7) `LeadTarget`(10) `LeadTickDelivery`(10) `LeadGroup`(7) | 176 |
| `driver/launcher.rs` | `ServeLauncher`(56) `impl<F> SessionLauncher for ServeLauncher`(64) `impl<F> ServeLauncher`(461) `PartialTurnInput`(10) `DeltaCoalescer`(6) `impl DeltaCoalescer`(34) | 631 |
| `driver/mod.rs` | 7 个 const：`PERMISSION_TIMEOUT`(2) `DRIVER_TICK`(6) `EVICT_IDLE_AFTER`(1) `POWER_TICK`(6) `LEAD_TICK`(8) `LEAD_LOOP_SENDER`(7) `TITLE_TIMEOUT`(5) + `evict_idle_runtimes`(104) | 139 |

⚠️ **`driver/launcher.rs` 631 行是本组最大的片**，因为 `ServeLauncher` 的两个 impl 块合计 525 行。它们**必须与 `ServeLauncher` 结构体同文件**（Rust 的 inherent impl 不能跨模块）。若 631 行不可接受，可把 `DeltaCoalescer` + 其 impl（40 行）挪到 `driver/coalescer.rs`，`PartialTurnInput`(10) 留在 `launcher.rs`。

⚠️ **`DeltaCoalescer` 属于 `driver` 组，且只在 `run_thread_driver`（同组 `turn_driver.rs`）里用**（实测 `src/server.rs:5304`，`let mut coalescer = DeltaCoalescer::default();`）。它**不**被 `chunk_items_for_replay` 使用——后者只读两个 `REPLAY_CHUNK_*` 常量。因此 `DeltaCoalescer` + 其 impl 的归宿是 `driver/launcher.rs`，无需跨组 re-export。

需改 `pub(crate)` 的 21 项：`DRIVER_TICK` `EVICT_IDLE_AFTER` `POWER_TICK` `LEAD_TICK` `LEAD_LOOP_SENDER` `TITLE_TIMEOUT` `DeltaCoalescer` `PartialTurnInput` `driver_tick` `ServeLauncher` `LeadTarget` `LeadTickDelivery` `LeadGroup` `snapshot_of` `project_root_of` `lead_tick_once` `take_changed_digests` `resolve_lead_target`（18 个「有名字的」+ 3 个 impl 块不需修饰，但其中 `impl DeltaCoalescer` / 两个 `ServeLauncher` impl 的方法需要 `pub(crate)` 吗？**不需要**——`impl` 块内方法的可见性由方法声明决定，本组 impl 的方法都是内部调用，保持 private 即可，只要 `struct` 本身是 `pub(crate)`）。

`evict_idle_runtimes`(104) 与 `run_thread_driver`(725) 已是 `pub(crate)`；`PERMISSION_TIMEOUT` 也是。

- [ ] **Step 1: 建目录并按片面搬 5 次**

```bash
mkdir -p yi-agent-rs/crates/yi-agent-app-server/src/server/driver
```

| 片 | `--names` |
|---|---|
| `driver/mod.rs` | `PERMISSION_TIMEOUT,DRIVER_TICK,EVICT_IDLE_AFTER,POWER_TICK,LEAD_TICK,LEAD_LOOP_SENDER,TITLE_TIMEOUT,evict_idle_runtimes` |
| `driver/turn_driver.rs` | `run_thread_driver` |
| `driver/tick.rs` | `driver_tick` |
| `driver/lead.rs` | `lead_tick_once,take_changed_digests,resolve_lead_target,snapshot_of,project_root_of,LeadTarget,LeadTickDelivery,LeadGroup` |
| `driver/launcher.rs` | `ServeLauncher,PartialTurnInput,DeltaCoalescer` |

`driver/mod.rs` 的 header：

```
//! thread driver 的主循环、tick、组长 tick、空闲 runtime 回收与通用 action 发起器。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 7）。本组 1,884 行，按职责分五片。

pub(crate) mod launcher;
pub(crate) mod lead;
pub(crate) mod tick;
pub(crate) mod turn_driver;

pub(crate) use launcher::{DeltaCoalescer, PartialTurnInput, ServeLauncher};
pub(crate) use lead::{
    lead_tick_once, project_root_of, resolve_lead_target, snapshot_of, take_changed_digests,
    LeadGroup, LeadTarget, LeadTickDelivery,
};
pub(crate) use tick::driver_tick;
pub(crate) use turn_driver::run_thread_driver;
```

`driver/launcher.rs` 的 header：

```
//! `ServeLauncher`：把插件的 `DriverAction`（`Launch` / `Append`）变成一次真实的
//! 会话准备与投递。两个 inherent/trait impl 合计 525 行，必须与本结构体同文件。
```

⚠️ `--names` 里给 `ServeLauncher,PartialTurnInput,DeltaCoalescer`，`extract.py` 会按 impl 头里的类型名自动带上 3 个 impl 块（打印 `impl items pulled in: 3`）。**若不是 3，停下检查**。

⚠️ **这里有个必须知道的陷阱**：`ServeLauncher` 结构体（L6044）与它的两个 impl（L6111 / L6168）之间**夹着 `LeadTarget`（L6098）与 `LeadTickDelivery`/`LeadGroup`**，而这两个属于 `lead.rs`。若按「相邻项自动带 impl」的规则，`ServeLauncher` 的 525 行 impl 会被错带进 `lead.rs`。**脚本已改为按 impl 头里的类型名匹配**（实测验证：`launcher.rs` 得到 3 个 impl、`lead.rs` 得到 0 个）。搬完务必核对：

```bash
grep -c '^impl' src/server/driver/launcher.rs   # 期望 3
grep -c '^impl' src/server/driver/lead.rs       # 期望 0
```

- [ ] **Step 2: 断言总数**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
python3 /tmp/wave2/extract.py --list src/server.rs | head -1   # 期望 45 items
wc -l < src/server.rs                                          # 期望约 1919
```

- [ ] **Step 3: 改 18 项为 `pub(crate)`（分文件，`impl` 会被跳过）**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
python3 /tmp/wave2/promote.py --file src/server/driver/mod.rs \
  --names DRIVER_TICK,EVICT_IDLE_AFTER,POWER_TICK,LEAD_TICK,LEAD_LOOP_SENDER,TITLE_TIMEOUT
python3 /tmp/wave2/promote.py --file src/server/driver/tick.rs --names driver_tick
python3 /tmp/wave2/promote.py --file src/server/driver/lead.rs \
  --names lead_tick_once,take_changed_digests,resolve_lead_target,snapshot_of,project_root_of,LeadTarget,LeadTickDelivery,LeadGroup
python3 /tmp/wave2/promote.py --file src/server/driver/launcher.rs \
  --names ServeLauncher,PartialTurnInput,DeltaCoalescer
```

`promote.py` 对 `impl` 行不匹配任何名字（名字是 `ServeLauncher` 等），所以不会误改 impl 块。**这是设计如此**，输出里应看到 `skipped impl: ...` 之类提示。

- [ ] **Step 4: 在 `server.rs` 加 mod 与 re-export**

```rust
pub(crate) mod driver;

pub(crate) use driver::{
    driver_tick, evict_idle_runtimes, lead_tick_once, project_root_of, resolve_lead_target,
    run_thread_driver, snapshot_of, take_changed_digests, DeltaCoalescer, LeadGroup, LeadTarget,
    LeadTickDelivery, PartialTurnInput, ServeLauncher, DRIVER_TICK, EVICT_IDLE_AFTER, LEAD_LOOP_SENDER,
    LEAD_TICK, PERMISSION_TIMEOUT, POWER_TICK, TITLE_TIMEOUT,
};
```

- [ ] **Step 5: 编译并逐行确认 `driver_tick` 未被改动**

```bash
cd yi-agent-rs
cargo build -p yi-agent-app-server 2>&1 | grep -E "^error" -A3 | head -40
git show HEAD:crates/yi-agent-app-server/src/server.rs \
  | sed -n '/^fn driver_tick/,/^}/p' > /tmp/wave2/tick_before.rs
sed -n '/^pub(crate) fn driver_tick/,/^}/p' \
  crates/yi-agent-app-server/src/server/driver/tick.rs > /tmp/wave2/tick_after.rs
diff /tmp/wave2/tick_before.rs /tmp/wave2/tick_after.rs && echo "OK: driver_tick 逐行一致"
```

若 diff 只差首行的 `fn` → `pub(crate) fn`，是预期的；其它任何差异都要逐行查明。

- [ ] **Step 6: 测试 + clippy + 计数 + 提交**

```bash
cd yi-agent-rs
cargo test -p yi-agent-app-server 2>&1 | tail -5
cargo test -p yi-agent-app-server -- --list 2>/dev/null | grep -c ': test$'   # 期望 1166
cargo clippy -p yi-agent-app-server --all-targets -- -D warnings 2>&1 | tail -3
cargo fmt --all
git add -A
git commit -m "refactor(app-server): 抽出 server/driver 五片（24 项）"
```

---

## Task 8: `transport/` 四片 + `serve()` 域分派宏收敛（11 项 / 1,547 行）

**本块唯一有结构变化的任务，最后做。** 分三步：先搬 10 个函数，再搬 `serve`，最后做宏收敛。

**Files:**
- Create: `yi-agent-app-server/src/server/transport/{mod,scoped,stdio,notifications}.rs`
- Modify: `yi-agent-app-server/src/server.rs`

**Interfaces:**
- Consumes: Task 1 机制
- Produces: `crate::server::{serve, serve_scoped, serve_scoped_core, serve_stdio, serve_stdio_with_relay, run, parse_relay_url, read_lines, pump_stdout, pump_theme_notifications, pump_git_diff_notifications}`

**11 项 / 1,547 行**：

| 文件 | 项（行数） | 小计 |
|---|---|---|
| `transport/mod.rs` | `serve`(985) | 985 |
| `transport/scoped.rs` | `serve_scoped_core`(165) `serve_stdio_with_relay`(92) `run`(79) `serve_scoped`(47) `serve_stdio`(46) `parse_relay_url`(3) | 432 |
| `transport/stdio.rs` | `read_lines`(36) `pump_stdout`(30) | 66 |
| `transport/notifications.rs` | `pump_git_diff_notifications`(35) `pump_theme_notifications`(29) | 64 |

需改 `pub(crate)` 的 6 项：`parse_relay_url` `serve_scoped_core` `read_lines` `pump_stdout` `pump_theme_notifications` `pump_git_diff_notifications`

**必须保持 `pub`**（2 项，`lib.rs` 有 `pub use server::{run, serve_stdio_with_relay}`）：`run`、`serve_stdio_with_relay`
**保持 `pub(crate)`**（3 项）：`serve` `serve_scoped` `serve_stdio`

⚠️ `pump_stdout` / `pump_theme_notifications` / `pump_git_diff_notifications` 已被外部引用（`broadcast.rs` / `protocol.rs` / `git_diff_tool.rs`）。这 3 个不提权就编译不过。

### 8A：搬 10 个函数（不含 `serve`）

- [ ] **Step 1: 建目录，搬三片（`scoped` / `stdio` / `notifications`）**

```bash
mkdir -p yi-agent-rs/crates/yi-agent-app-server/src/server/transport
cd yi-agent-rs/crates/yi-agent-app-server
python3 /tmp/wave2/extract.py --src src/server.rs --dst src/server/transport/scoped.rs \
  --names serve_scoped_core,serve_stdio_with_relay,run,serve_scoped,serve_stdio,parse_relay_url \
  --header /tmp/wave2/hdr_scoped.txt --move
python3 /tmp/wave2/extract.py --src src/server.rs --dst src/server/transport/stdio.rs \
  --names read_lines,pump_stdout --header /tmp/wave2/hdr_stdio.txt --move
python3 /tmp/wave2/extract.py --src src/server.rs --dst src/server/transport/notifications.rs \
  --names pump_git_diff_notifications,pump_theme_notifications \
  --header /tmp/wave2/hdr_notifications.txt --move
```

三个 header：

```
//! scoped / relay / stdio 三种 serve 入口。
```

```
//! stdin 行读取与 stdout 泵。
```

```
//! 主题与 git-diff 的后台通知泵。
```

- [ ] **Step 2: 断言总数（此时 `serve` 还在 `server.rs`）**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
python3 /tmp/wave2/extract.py --list src/server.rs | head -1   # 期望 35 items
```

- [ ] **Step 3: 改 5 项为 `pub(crate)`（`run` / `serve_stdio_with_relay` 不动）**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
python3 /tmp/wave2/promote.py --file src/server/transport/scoped.rs \
  --names serve_scoped_core,parse_relay_url
python3 /tmp/wave2/promote.py --file src/server/transport/stdio.rs --names read_lines,pump_stdout
python3 /tmp/wave2/promote.py --file src/server/transport/notifications.rs \
  --names pump_git_diff_notifications,pump_theme_notifications
```

⚠️ `promote.py` 会拒绝改动已经是 `pub` 的项（`run` / `serve_stdio_with_relay` 不在 `--names` 里，所以不会被碰）。

- [ ] **Step 4: 建 `transport/mod.rs`（此时只含 mod 声明与 re-export）**

```rust
//! 传输层：stdio / relay / scoped 的 serve 入口与后台 pump。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 8）。`serve()` 仍在本文件。

pub(crate) mod notifications;
pub(crate) mod scoped;
pub(crate) mod stdio;

pub(crate) use notifications::{pump_git_diff_notifications, pump_theme_notifications};
pub(crate) use scoped::{
    parse_relay_url, run, serve_scoped, serve_scoped_core, serve_stdio, serve_stdio_with_relay,
};
pub(crate) use stdio::{pump_stdout, read_lines};
```

- [ ] **Step 5: 在 `server.rs` 加 mod 与 re-export（10 个，不含 `serve`）**

```rust
pub(crate) mod transport;

pub(crate) use transport::{
    parse_relay_url, pump_git_diff_notifications, pump_stdout, pump_theme_notifications, read_lines,
    run, serve_scoped, serve_scoped_core, serve_stdio, serve_stdio_with_relay,
};
```

- [ ] **Step 6: 编译修错**

```bash
cd yi-agent-rs
cargo build -p yi-agent-app-server 2>&1 | grep -E "^error" -A3 | head -40
```

**常见坑**：若报 `function run is only public within the crate`，说明误改了 `run` 的可见性，改回 `pub`。

- [ ] **Step 7: 测试 + 提交（先单独提交，隔离风险）**

```bash
cd yi-agent-rs
cargo test -p yi-agent-app-server 2>&1 | tail -5
cargo test -p yi-agent-app-server -- --list 2>/dev/null | grep -c ': test$'   # 期望 1166
cargo clippy -p yi-agent-app-server --all-targets -- -D warnings 2>&1 | tail -3
cargo fmt --all
git add -A
git commit -m "refactor(app-server): 抽出 server/transport 三片（10 项）"
```

### 8B：把 `serve()` 搬进 `transport/mod.rs`

- [ ] **Step 8: 搬 `serve`**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
cp src/server/transport/mod.rs /tmp/wave2/mod_header.rs
python3 /tmp/wave2/extract.py --src src/server.rs --dst src/server/transport/mod.rs \
  --names serve --append --move
```

`--append` 会把 `serve` 追加到现有 `mod.rs` 末尾（不覆盖 mod 声明与 re-export）。

期望输出 `moved 1 items / 985 lines`。

- [ ] **Step 9: 断言 `server.rs` 只剩 34 项**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
python3 /tmp/wave2/extract.py --list src/server.rs | head -1   # 期望 34 items
wc -l < src/server.rs                                          # 期望约 361
grep -cE '^(pub(\(crate\))?\s+)?(async\s+)?fn ' src/server.rs  # 期望 0
```

- [ ] **Step 10: 在 `server.rs` 的 transport re-export 里补上 `serve`**

```rust
pub(crate) use transport::{
    parse_relay_url, pump_git_diff_notifications, pump_stdout, pump_theme_notifications, read_lines,
    run, serve, serve_scoped, serve_scoped_core, serve_stdio, serve_stdio_with_relay,
};
```

- [ ] **Step 11: 编译，逐个修路径前缀**

`serve()` 搬进子模块后，函数体里所有裸写的同文件调用现在都靠 `server.rs` 的 re-export 解析，**通常不需改**。但若报 `cannot find function X in this scope`，说明 `X` 在 `server.rs` 的 re-export 里漏了，补进对应组的 `use` 列表。

```bash
cd yi-agent-rs
cargo build -p yi-agent-app-server 2>&1 | grep -E "^error" -A3 | head -40
```

- [ ] **Step 12: 跑测试，确认 `serve` 搬移本身零行为变化**

```bash
cd yi-agent-rs
cargo test -p yi-agent-app-server 2>&1 | tail -8
cargo test -p yi-agent-app-server -- --list 2>/dev/null | grep -c ': test$'   # 期望 1166
```

**必须先全绿再进 8C。** 若这里就红，说明搬移破坏了借用关系，问题在搬移不在宏。

- [ ] **Step 13: 提交**

```bash
cd yi-agent-rs
cargo fmt --all
git add -A
git commit -m "refactor(app-server): serve 搬入 transport/mod.rs"
```

### 8C：宏收敛 16 个臂（唯一结构变化）

`serve()` 里 `match crate::rpc::domain_for(...)` 的分派块共 198 行，18 个臂。**16 个臂**收敛成 1 行；只有 `Plugin`(43 行) 与 `Unknown`(1 行) 原样保留。

**为什么 `Thread` 也收敛**：实测把它与 `Ui` 臂归一化后（去注释、把域模块名替换为占位符）**逐行完全相同**，且 `thread::handle` 与其它 15 个域的签名一模一样（`pub(crate) async fn handle(&mut AppState<'_>, &RequestEnvelope, RequestId) -> anyhow::Result<Flow>`）。早先版本误以为它是"特殊臂"，实际不是——它只是注释更多。

16 个收敛臂与行数（实测）：`Ui`/`Model`/`Onboarding`/`Image`/`Workspace`/`Process`/`Pair`/`Turn`/`Agent`/`Skill`/`Mcp`/`Thread` 各 10 行；`Resident`/`Flags`/`Remote`/`WebSearch` 各 7 行。

- [ ] **Step 14: 在 `app_state!` 宏定义之后插入 `dispatch_domain!` 局部宏**

锚定 `transport/mod.rs` 里 `app_state!` 宏的收尾 `}`，紧跟其后插入：

```rust
                // 16 个臂的差异只有域模块名，故用局部宏收敛成一行。
                // 不能抽成函数：`app_state!` 按名捕获 serve() 的局部量、
                // 且臂体内联 `continue`，两者都无法跨函数。
                macro_rules! dispatch_domain {
                    ($handler:path) => {{
                        let mut state = app_state!();
                        match $handler(&mut state, &req, id.clone()).await? {
                            crate::rpc::Flow::Continue => continue,
                            crate::rpc::Flow::Unknown => {}
                        }
                    }};
                }
```

- [ ] **Step 15: 一次收敛 4 个臂，跑一次测试**

按此顺序分 4 批，**每批 4 个、跑一次测试再继续**。

例如 `Ui` 臂的原文（10 行）：

```rust
                    crate::rpc::Domain::Ui => {
                        let mut state = app_state!();
                        match crate::rpc::ui::handle(&mut state, &req, id.clone()).await? {
                            crate::rpc::Flow::Continue => continue,
                            crate::rpc::Flow::Unknown => {}
                        }
                    }
```

替换为：

```rust
                    crate::rpc::Domain::Ui => dispatch_domain!(crate::rpc::ui::handle),
```

四批的域名与 handler 映射（**逐字照抄，模块名不能猜**）：

| 批 | 臂 | 替换为 |
|---|---|---|
| 1 | `Ui` | `dispatch_domain!(crate::rpc::ui::handle)` |
| 1 | `Model` | `dispatch_domain!(crate::rpc::model::handle)` |
| 1 | `Onboarding` | `dispatch_domain!(crate::rpc::onboarding::handle)` |
| 1 | `Image` | `dispatch_domain!(crate::rpc::image::handle)` |
| 2 | `Workspace` | `dispatch_domain!(crate::rpc::workspace::handle)` |
| 2 | `Process` | `dispatch_domain!(crate::rpc::process::handle)` |
| 2 | `Pair` | `dispatch_domain!(crate::rpc::pair::handle)` |
| 2 | `Turn` | `dispatch_domain!(crate::rpc::turn::handle)` |
| 3 | `Agent` | `dispatch_domain!(crate::rpc::agent::handle)` |
| 3 | `Skill` | `dispatch_domain!(crate::rpc::skill::handle)` |
| 3 | `Mcp` | `dispatch_domain!(crate::rpc::mcp::handle)` |
| 3 | `Resident` | `dispatch_domain!(crate::rpc::resident::handle)` |
| 4 | `Flags` | `dispatch_domain!(crate::rpc::flags::handle)` |
| 4 | `Remote` | `dispatch_domain!(crate::rpc::remote::handle)` |
| 4 | `WebSearch` | `dispatch_domain!(crate::rpc::websearch::handle)` |
| 4 | `Thread` | `dispatch_domain!(crate::rpc::thread::handle)` |

**改之前先核验这 15 个模块都有 `handle` 且签名一致**：

```bash
cd yi-agent-rs/crates/yi-agent-app-server
for m in ui model onboarding image workspace process pair turn agent skill mcp resident flags remote websearch thread; do
  printf '%-12s ' "$m"
  grep -c 'pub(crate) async fn handle' src/rpc/$m.rs 2>/dev/null || echo "MISSING"
done
```

期望 **16 行全为 `1`**。若某个模块不是 `pub(crate) async fn handle`（例如返回类型不同），**该臂不要收敛**，留在手工形式。

每批之后：

```bash
cd yi-agent-rs
cargo test -p yi-agent-app-server 2>&1 | tail -3
```

期望每批全绿。**若某批变红，回退该批**（`git checkout -- crates/yi-agent-app-server/src/server/transport/mod.rs`）并查明该域 handler 是否返回 `Flow::Unknown` 的真实路径与样板不同。

- [ ] **Step 16: 逐臂比对，确认只删了样板**

```bash
cd yi-agent-rs
git diff -U0 -- crates/yi-agent-app-server/src/server/transport/mod.rs \
  | grep -E '^[+-].*Domain::' | head -40
```

逐个确认：`-` 行是原臂的 7–10 行块，`+` 行是单行 `dispatch_domain!`。

```bash
cd yi-agent-rs
git diff -U0 -- crates/yi-agent-app-server/src/server/transport/mod.rs \
  | grep -cE '^\+.*Domain::(Plugin|Unknown)'
```

期望 `0`（`Plugin` 与 `Unknown` 没被改）。

再确认 `Thread` **确实被收敛了**（它应该在 `+` 行里出现一次）：

```bash
cd yi-agent-rs
git diff -U0 -- crates/yi-agent-app-server/src/server/transport/mod.rs \
  | grep -cE '^\+.*Domain::Thread'
```

期望 `1`。

- [ ] **Step 17: 断言分派块降到 ≤ 90 行**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
python3 - <<'PY'
L = open('src/server/transport/mod.rs').read().splitlines()
start = next(i for i, l in enumerate(L) if 'match crate::rpc::domain_for' in l)
indent = len(L[start]) - len(L[start].lstrip())
for j in range(start + 1, len(L)):
    if L[j].strip() == '}' and len(L[j]) - len(L[j].lstrip()) == indent:
        print(f"domain_for 分派块 = {j - start + 1} 行（目标 ≤ 90）")
        break
PY
```

- [ ] **Step 18: 断言 18 个域仍然全覆盖**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
for d in Ui Model Onboarding Resident Plugin Image Workspace Process Pair Turn Agent Skill Flags Remote Mcp WebSearch Thread Unknown; do
  grep -q "Domain::$d" src/server/transport/mod.rs || echo "MISSING Domain::$d"
done; echo "（无输出即 18 个域全在）"
grep -c 'dispatch_domain!' src/server/transport/mod.rs   # 期望 17（1 定义 + 16 调用）
```

- [ ] **Step 19: 最终测试、clippy、指标、提交**

```bash
cd yi-agent-rs
cargo test -p yi-agent-app-server 2>&1 | tail -5
cargo test -p yi-agent-app-server -- --list 2>/dev/null | grep -c ': test$'   # 期望 1166
cargo clippy -p yi-agent-app-server --all-targets -- -D warnings 2>&1 | tail -3
cargo fmt --all
wc -l crates/yi-agent-app-server/src/server/transport/mod.rs   # 目标 ≤ 900
git add -A
git commit -m "refactor(app-server): serve 域分派用局部宏收敛 16 个臂"
```

---

## Task 9: C2 收尾——`server.rs` 只剩类型与 re-export

**Files:**
- Modify: `yi-agent-app-server/src/server.rs`

**Interfaces:**
- Consumes: Task 1–8 全部完成
- Produces: `server.rs` ≤ 400 行、顶层函数 0 个

- [ ] **Step 1: 断言顶层函数为 0、剩余项为 34**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
grep -cE '^(pub(\(crate\))?\s+)?(async\s+)?fn ' src/server.rs   # 期望 0
python3 /tmp/wave2/extract.py --list src/server.rs | head -1    # 期望 34 items
```

34 = 18 个类型 + 3 个 `impl` + 13 个 `mod`。若非 34，列出多出/缺少的项：

```bash
python3 /tmp/wave2/extract.py --list src/server.rs
```

- [ ] **Step 2: 断言 `server.rs` 行数达标**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
wc -l < src/server.rs
```

期望 **361**（预演实测值：34 项 286 行 + use 块 + 空行 + doc）。判据是 ≤ 400。

- [ ] **Step 3: 断言 18 个必留类型 + 3 个 impl 仍在**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
cat > /tmp/wave2/check_keep.py <<'EOF'
import re
S = open('src/server.rs').read()
types = ['IdleFacts', 'TurnEvent', 'BuiltAgent', 'ProjectRuntimes', 'ThreadRoots',
         'ViewFrames', 'RuntimeAttachments', 'WatchmanInstall', 'WatchmanUninstall',
         'ResolvePlugin', 'EnsureDaemon', 'ThreadCancelOutcome', 'ChildrenWatch',
         'ProcessWatch', 'TraceWatch', 'ClientScopes', 'ClientInitialized', 'WsDeviceRegistry']
impls = ['ChildrenWatch', 'ProcessWatch', 'TraceWatch']
bad = 0
for t in types:
    if not re.search(r'^\s*(pub\(crate\)\s+)?(struct|enum|type)\s+' + t + r'\b', S, re.M):
        print('MISSING type:', t); bad += 1
for t in impls:
    if not re.search(r'^impl\s+' + t + r'\s*\{', S, re.M):
        print('MISSING impl:', t); bad += 1
print('OK' if bad == 0 else str(bad) + ' missing')
EOF
python3 /tmp/wave2/check_keep.py
```

期望输出 `OK`。

- [ ] **Step 4: 断言 13 个 `mod` 声明仍在，且 `mod tests;` 是目录模块**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
grep -cE '^(pub\(crate\) )?mod [a-z_]+;' src/server.rs   # 期望 13
test -f src/server/tests/mod.rs && echo "OK: src/server/tests/mod.rs 存在"
```

- [ ] **Step 5: 断言最大子模块 ≤ 1,000**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
find src/server -maxdepth 3 -name '*.rs' | xargs wc -l | sort -rn | head -4
```

期望最大值 ≤ 1,000（`transport/mod.rs` 含 `serve` 约 880 是最大的）。

- [ ] **Step 6: 全量校验并提交**

```bash
cd yi-agent-rs
cargo test -p yi-agent-app-server 2>&1 | tail -5
cargo test -p yi-agent-app-server -- --list 2>/dev/null | grep -c ': test$'   # 期望 1166
cargo clippy -p yi-agent-app-server --all-targets -- -D warnings 2>&1 | tail -3
cargo fmt --all
cargo test -p yi-agent-app-server export_bindings 2>&1 | tail -3
git diff --exit-code -- ../desktop/src/lib/generated && echo "OK: 协议未动"
git diff --exit-code -- crates/yi-agent-app-server/src/lib.rs && echo "OK: lib.rs 未动"
git diff --exit-code main -- crates/yi-agent-app-server/src/server/tests/ && echo "OK: server/tests 未动"
git add -A
git commit -m "refactor(app-server): C2 收尾，server.rs 只剩类型与 re-export"
```

---

## Task 10: C3a——`owns()` 骨架与基线测试（先失败）

C3 与 C2 改的文件集不重叠（C2：`server.rs` + `server/**`；C3：`rpc/thread.rs` + `rpc/thread/**`），**可以在 Task 9 之前就并行开工**。本计划按顺序执行；若并行，从 Task 0 的同一个 worktree 另开分支（附录 C）。

**Files:**
- Create: `yi-agent-app-server/src/rpc/thread/mod.rs`（先只放 `owns()` 与测试）
- Delete: `yi-agent-app-server/src/rpc/thread.rs`（内容迁入 `thread/mod.rs`）
- Test: `yi-agent-app-server/src/rpc/thread/mod.rs` 内的 `#[cfg(test)] mod owns_coverage`

**Interfaces:**
- Consumes: `src/rpc/thread_methods_baseline.txt`（Task 0 Step 5 产出，26 行）
- Produces: `crate::rpc::thread::{lifecycle, settings, content, groups}` 四个子模块各自导出 `pub(crate) fn owns(method: &str) -> bool`；四组的 `handle(state, req, id) -> anyhow::Result<Flow>`

**`thread.rs` 的真实结构**（实测）：9 个顶层项 / 2,809 行。

| 项 | 行范围 | 行数 |
|---|---|---|
| `missing_model_error` | L41–63 | 23 |
| `handle` | L65–2416 | 2,352 |
| `scan_workspace_metas` | L2418–2464 | 47 |
| `scan_workspace_metas_for_test` | L2466–2476 | 11 |
| `diff_read_fallback` | L2478–2504 | 27 |
| `build_list_all_response` | L2506–2597 | 92 |
| `groups_json` | L2599–2623 | 25 |
| `apply_group_mutation` | L2625–2800 | 176 |
| `require_group_id` | L2802–2809 | 8 |

`handle()` 内 23 个 `match` 臂，覆盖 26 个 method（`group/*` 是 1 臂 4 method 的或模式）。

- [ ] **Step 1: 把 `thread.rs` 转成目录模块**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
mkdir -p src/rpc/thread
git mv src/rpc/thread.rs src/rpc/thread/mod.rs
mkdir -p src/rpc/thread/lifecycle src/rpc/thread/settings src/rpc/thread/content src/rpc/thread/groups
cd ../../.. && cargo build -p yi-agent-app-server 2>&1 | tail -3
```

`git mv` 保留历史。此时 `mod.rs` 仍是 2,809 行的完整文件，先确认能编译。

- [ ] **Step 1.5: 准备 `rpc/thread.rs` 的 `use` 头（C3 全组共用；建议在 Step 1 之前做）**

`rpc/thread.rs` 顶部有 10 条 `use`（3 条跨行）。拆出的每个子文件同样需要它们。**与 C2 同一套机制**：

**必须在 Step 1 的 `git mv` 之前抽**（`git mv` 之后 `src/rpc/thread.rs` 就不存在了）：

```bash
cd yi-agent-rs/crates/yi-agent-app-server
python3 /tmp/wave2/usehdr.py src/rpc/thread.rs > /tmp/wave2/thread_use.rs
wc -l /tmp/wave2/thread_use.rs
```

期望 `13`（10 条 `use`，3 条跨行各多 1 行）。

若已经执行了 Step 1，改从新路径抽，结果完全相同：

```bash
python3 /tmp/wave2/usehdr.py src/rpc/thread/mod.rs > /tmp/wave2/thread_use.rs
```

⚠️ 注意 `rpc/thread.rs` 的 `use` 块里有一条**从 `server.rs` 引入 30 个符号**的巨块（`use crate::server::{ ... };`）。这条是 C3 与 C2 的**真实耦合点**：C3 拆出的子文件如果用到 `crate::server::run_thread_driver` 之类，必须保证 Task 1–9 的 re-export 已经把该符号导出到 `crate::server`。**C3 开工前先跑一次 `cargo check -p yi-agent-app-server` 确认基线绿。**（若 C2 尚未合并，`crate::server` 仍是原来的单文件，同样可用。）

每个 C3 子文件的开头模板：

```rust
//! <一行说明>

<粘贴 /tmp/wave2/thread_use.rs 全文，随后 cargo fix 裁剪>

<搬入的臂与 helper>
```

裁剪守卫与 C2 相同：

```bash
cd yi-agent-rs
cargo fix -p yi-agent-app-server --allow-dirty --allow-staged --offline 2>&1 | tail -5
python3 /tmp/wave2/usecheck.py --root . --allow-mod \
  crates/yi-agent-app-server/src/rpc/thread/content/list_all.rs
```

- [ ] **Step 2: 写 4 个 `owns()`，此时故意留空（先失败）**

`src/rpc/thread/lifecycle/mod.rs`：

```rust
//! `thread` 域的 lifecycle 组：start / resume / fork / delete / clear。

pub(crate) fn owns(_method: &str) -> bool {
    false
}
```

另三个文件同形状，只改模块文档：

```rust
//! `thread` 域的 settings 组：rename / setModel / setReasoningEffort /
//! setPreserveThinking / setPermissionMode / setPinned / setTag / markRead /
//! reorderPinned / compact。

pub(crate) fn owns(_method: &str) -> bool {
    false
}
```

```rust
//! `thread` 域的 content 组：list / listAll / diff/read / view/read /
//! subscribe / readItems。

pub(crate) fn owns(_method: &str) -> bool {
    false
}
```

```rust
//! `thread` 域的 groups 组：lead / group/create / group/update /
//! group/setMembers / group/delete。

pub(crate) fn owns(_method: &str) -> bool {
    false
}
```

- [ ] **Step 3: 在 `mod.rs` 顶部声明 4 个子模块并写基线测试**

在 `mod.rs` 的 `use` 块之后插入：

```rust
pub(crate) mod content;
pub(crate) mod groups;
pub(crate) mod lifecycle;
pub(crate) mod settings;
```

在 `mod.rs` 末尾追加：

```rust
#[cfg(test)]
mod owns_coverage {
    use super::{content, groups, lifecycle, settings};

    /// 基线清单由 Task 0 从拆分前的 `thread.rs` 枚举，提交入库。
    /// 它必须是**机器枚举**而非手写：手写清单抓不到「原有 method 被漏掉」。
    #[test]
    fn every_thread_method_is_owned_by_exactly_one_group() {
        let all: Vec<&str> = include_str!("../thread_methods_baseline.txt")
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect();
        assert_eq!(all.len(), 26, "基线文件必须恰好 26 行");
        let mut bad = vec![];
        for m in &all {
            let n = [
                lifecycle::owns(m),
                settings::owns(m),
                content::owns(m),
                groups::owns(m),
            ]
            .iter()
            .filter(|x| **x)
            .count();
            if n != 1 {
                bad.push(format!("{m} 被 {n} 组认领"));
            }
        }
        assert!(bad.is_empty(), "未归属或重复归属: {bad:#?}");
    }
}
```

`include_str!("../thread_methods_baseline.txt")` 的路径：测试在 `src/rpc/thread/mod.rs` 里，`../` 指向 `src/rpc/`，基线文件就在那里。

- [ ] **Step 4: 跑测试，确认它失败**

```bash
cd yi-agent-rs
cargo test -p yi-agent-app-server every_thread_method_is_owned_by_exactly_one_group 2>&1 | tail -20
```

期望 **FAIL**，错误信息里含「被 0 组认领」。若 PASS，说明 `owns()` 写错了（不该有组认领任何 method）。

- [ ] **Step 5: 填入 4 个组的真实 `owns()`**

```rust
// lifecycle/mod.rs
pub(crate) fn owns(method: &str) -> bool {
    matches!(
        method,
        "thread/start" | "thread/resume" | "thread/fork" | "thread/delete" | "thread/clear"
    )
}
```

```rust
// settings/mod.rs
pub(crate) fn owns(method: &str) -> bool {
    matches!(
        method,
        "thread/rename"
            | "thread/setModel"
            | "thread/setReasoningEffort"
            | "thread/setPreserveThinking"
            | "thread/setPermissionMode"
            | "thread/setPinned"
            | "thread/setTag"
            | "thread/markRead"
            | "thread/reorderPinned"
            | "thread/compact"
    )
}
```

```rust
// content/mod.rs
pub(crate) fn owns(method: &str) -> bool {
    matches!(
        method,
        "thread/list"
            | "thread/listAll"
            | "thread/diff/read"
            | "thread/view/read"
            | "thread/subscribe"
            | "thread/readItems"
    )
}
```

```rust
// groups/mod.rs
pub(crate) fn owns(method: &str) -> bool {
    matches!(
        method,
        "thread/lead"
            | "thread/group/create"
            | "thread/group/update"
            | "thread/group/setMembers"
            | "thread/group/delete"
    )
}
```

- [ ] **Step 6: 跑测试，确认转绿**

```bash
cd yi-agent-rs
cargo test -p yi-agent-app-server every_thread_method_is_owned_by_exactly_one_group 2>&1 | tail -8
```

期望 PASS。

- [ ] **Step 7: 验证这个测试真的能抓到错误（关键验收，不可跳过）**

临时把 `thread/delete` 从 `lifecycle` 的 `matches!` 里删掉：

```bash
cd yi-agent-rs/crates/yi-agent-app-server
sed -i '' 's/ | "thread\/delete"//' src/rpc/thread/lifecycle/mod.rs
cd ../../.. && cargo test -p yi-agent-app-server every_thread_method_is_owned_by_exactly_one_group 2>&1 \
  | grep -E "被 0 组认领|test result" | head -3
```

期望 **FAIL** 且错误里出现 `thread/delete 被 0 组认领`。确认后**改回来**：

```bash
cd yi-agent-rs/crates/yi-agent-app-server
sed -i '' 's/"thread\/fork" | "thread\/clear"/"thread\/fork" | "thread\/delete" | "thread\/clear"/' \
  src/rpc/thread/lifecycle/mod.rs
cd ../../.. && cargo test -p yi-agent-app-server every_thread_method_is_owned_by_exactly_one_group 2>&1 | tail -3
```

期望恢复 PASS。**这一步不能跳过**——它是证明「测试有效」的唯一证据。

- [ ] **Step 8: 确认测试数变成 1,167**

```bash
cd yi-agent-rs
cargo test -p yi-agent-app-server -- --list 2>/dev/null | grep -c ': test$'
```

期望 **1167**。此后每个 C3 任务都应保持 1,167。

- [ ] **Step 9: 提交**

```bash
cd yi-agent-rs
cargo fmt --all
git add -A
git commit -m "test(app-server): thread 域 method 归属骨架与基线测试"
```

---

## Task 11: C3b——`content/` 三片（6 臂 / 460 行 + 4 个 helper / 191 行）

先做最简单的一组（只读投影为主的组），验证 C3 的搬移机制。


> **每个新建子文件都要先贴 `use` 头**：`//!` 文档 → `/tmp/wave2/thread_use.rs` 全文 → 搬入的臂/helper → `cargo fix` → `usecheck.py --allow-mod` 守卫。见 Task 10 Step 1.5。

**Files:**
- Modify: `yi-agent-app-server/src/rpc/thread/content/mod.rs`
- Create: `yi-agent-app-server/src/rpc/thread/content/list_all.rs`
- Create: `yi-agent-app-server/src/rpc/thread/content/diff_read.rs`

**Interfaces:**
- Consumes: Task 10 的 `owns()` 骨架
- Produces: `content::handle(state, req, id) -> anyhow::Result<Flow>`；`list_all::handle` / `diff_read::handle` 由 `content/mod.rs` 分派

**分片（臂行数为实测）**：

| 文件 | 臂（行数） | helper（行数） | 小计 |
|---|---|---|---|
| `content/mod.rs` | `list`(29) `subscribe`(45) `view/read`(148) `readItems`(74) | `scan_workspace_metas_for_test`(11, 测试用) | 307 |
| `content/list_all.rs` | `listAll`(51) | `scan_workspace_metas`(47) `build_list_all_response`(92) `groups_json`(25) | 215 |
| `content/diff_read.rs` | `diff/read`(113) | `diff_read_fallback`(27) | 140 |

- [ ] **Step 1: 切 `listAll` 臂到 `list_all.rs`**

先从 `mod.rs` 里定位该臂的精确行范围：

```bash
cd yi-agent-rs/crates/yi-agent-app-server
python3 - <<'PY'
import re
L=open('src/rpc/thread/mod.rs').read().splitlines()
h=next(i for i,l in enumerate(L) if re.match(r'^pub\(crate\) async fn handle',l))
arms=[(i,re.match(r'^        "([^"]+)"',l).group(1)) for i,l in enumerate(L)
      if i>h and re.match(r'^        "thread/',l)]
for k,(i,n) in enumerate(arms):
    e=arms[k+1][0]-1 if k+1<len(arms) else None
    print(f"{n:28} L{i+1:5} -> {e+1 if e else 'end':>5}")
PY
```

期望看到 `thread/readItems` 是**最后一个臂**（L2343–2416，74 行）。**这一步必须做**：早先版本的 plan 把 `readItems` 完全漏掉了。

用 `edit` 把 `listAll` 臂（L105–155，51 行）整块从 `mod.rs` 移除，粘到 `content/list_all.rs`，包成：

```rust
//! `thread/listAll` 与其三个 helper。

use crate::protocol::{RequestEnvelope, RequestId};
use crate::rpc::{AppState, Flow};

pub(crate) async fn handle(
    state: &mut AppState<'_>,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    match req.method.as_str() {
        "thread/listAll" => {
            // 原臂体逐字搬入
        }
        _ => return Ok(Flow::Unknown),
    }
    Ok(Flow::Continue)
}
```

**关键改写规则**（只允许这些）：
- 臂内的 `continue` → `return Ok(Flow::Continue)`
- `method.as_str()` → `req.method.as_str()`
- 裸写的局部量 → `state.<字段名>`（原臂已用 `state.` 的保持不变）

- [ ] **Step 2: 把 3 个 helper 从 `mod.rs` 底部搬到 `list_all.rs`**

用 `extract.py`（`--src src/rpc/thread/mod.rs`，`--dst src/rpc/thread/content/list_all.rs`，`--names scan_workspace_metas,build_list_all_response,groups_json`，`--append --move`）。

⚠️ **不要搬 `scan_workspace_metas_for_test`**（11 行，L2466–2476）。它只被测试用，留在 `mod.rs` 的 `#[cfg(test)]`。

- [ ] **Step 3: 同样处理 `diff/read` 与 `diff_read_fallback` 到 `diff_read.rs`**

形状同 Step 1，`_ => return Ok(Flow::Unknown)` 兜底。

- [ ] **Step 4: `content/mod.rs` 改成三分派（保留 4 个臂）**

```rust
pub(crate) mod diff_read;
pub(crate) mod list_all;

pub(crate) async fn handle(
    state: &mut AppState<'_>,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    match req.method.as_str() {
        "thread/listAll" => return list_all::handle(state, req, id).await,
        "thread/diff/read" => return diff_read::handle(state, req, id).await,
        _ => {}
    }
    match req.method.as_str() {
        "thread/list" => { /* 原臂体 */ }
        "thread/subscribe" => { /* 原臂体 */ }
        "thread/view/read" => { /* 原臂体 */ }
        "thread/readItems" => { /* 原臂体 ← 不要漏 */ }
        _ => return Ok(Flow::Unknown),
    }
    Ok(Flow::Continue)
}
```

- [ ] **Step 5: 编译、测试、校验 method 无遗漏**

```bash
cd yi-agent-rs
cargo build -p yi-agent-app-server 2>&1 | grep -E "^error" -A3 | head -40
cargo test -p yi-agent-app-server 2>&1 | tail -5
cargo test -p yi-agent-app-server -- --list 2>/dev/null | grep -c ': test$'   # 期望 1167
grep -rhoE '"thread/[a-zA-Z/]+"' crates/yi-agent-app-server/src/rpc/thread/ \
  | tr -d '"' | sort -u > /tmp/wave2/after.txt
diff crates/yi-agent-app-server/src/rpc/thread_methods_baseline.txt /tmp/wave2/after.txt \
  && echo "OK: 26 个 method 一个不差"
```

- [ ] **Step 6: 提交**

```bash
cd yi-agent-rs
cargo fmt --all
git add -A
git commit -m "refactor(app-server): 拆出 rpc/thread/content 三片"
```

---

## Task 12: C3c——`settings/` 三片（10 臂 / 866 行）


> **每个新建子文件都要先贴 `use` 头**：`//!` 文档 → `/tmp/wave2/thread_use.rs` 全文 → 搬入的臂/helper → `cargo fix` → `usecheck.py --allow-mod` 守卫。见 Task 10 Step 1.5。

**Files:**
- Modify: `yi-agent-app-server/src/rpc/thread/settings/mod.rs`
- Create: `yi-agent-app-server/src/rpc/thread/settings/model.rs`
- Create: `yi-agent-app-server/src/rpc/thread/settings/effort.rs`

**Interfaces:**
- Consumes: Task 10 骨架、Task 11 的机制验证
- Produces: `settings::handle`；`model::handle` / `effort::handle`

| 文件 | 臂（行数） | 小计 |
|---|---|---|
| `settings/mod.rs` | `rename`(46) `setPinned`(46) `setTag`(69) `markRead`(61) `reorderPinned`(86) `compact`(51) | 359 |
| `settings/model.rs` | `setModel`(134) `setPermissionMode`(75) | 209 |
| `settings/effort.rs` | `setReasoningEffort`(182) `setPreserveThinking`(116) | 298 |

三片合计 866 = 10 个臂的实测总行数（无 helper 归属本组）。每片 < 450，达标。

- [ ] **Step 1: 切 `setModel` + `setPermissionMode` 到 `model.rs`**

用 Task 11 Step 1 的定位脚本找两个臂的起止行：`setModel` L912–1045（134 行）、`setPermissionMode` L1344–1418（75 行）。整块搬入 `model.rs`，包成 `handle` 形状（`_ => return Ok(Flow::Unknown)` 兜底 + 末尾 `Ok(Flow::Continue)`）。

- [ ] **Step 2: 切 `setReasoningEffort` + `setPreserveThinking` 到 `effort.rs`**

`setReasoningEffort` L1046–1227（182 行）、`setPreserveThinking` L1228–1343（116 行）。这两臂在源码里**相邻**，一次搬走。

- [ ] **Step 3: `settings/mod.rs` 改成三分派**

```rust
pub(crate) mod effort;
pub(crate) mod model;

pub(crate) async fn handle(state: &mut AppState<'_>, req: &RequestEnvelope, id: RequestId)
    -> anyhow::Result<Flow> {
    match req.method.as_str() {
        "thread/setModel" | "thread/setPermissionMode" => return model::handle(state, req, id).await,
        "thread/setReasoningEffort" | "thread/setPreserveThinking" => {
            return effort::handle(state, req, id).await
        }
        _ => {}
    }
    match req.method.as_str() {
        "thread/rename" => { /* 原臂体 */ }
        "thread/setPinned" => { /* 原臂体 */ }
        "thread/setTag" => { /* 原臂体 */ }
        "thread/markRead" => { /* 原臂体 */ }
        "thread/reorderPinned" => { /* 原臂体 */ }
        "thread/compact" => { /* 原臂体 */ }
        _ => return Ok(Flow::Unknown),
    }
    Ok(Flow::Continue)
}
```

- [ ] **Step 4: 编译、测试、校验、提交**

```bash
cd yi-agent-rs
cargo build -p yi-agent-app-server 2>&1 | grep -E "^error" -A3 | head -40
cargo test -p yi-agent-app-server 2>&1 | tail -5
cargo test -p yi-agent-app-server -- --list 2>/dev/null | grep -c ': test$'   # 期望 1167
grep -rhoE '"thread/[a-zA-Z/]+"' crates/yi-agent-app-server/src/rpc/thread/ | tr -d '"' | sort -u > /tmp/wave2/after.txt
diff crates/yi-agent-app-server/src/rpc/thread_methods_baseline.txt /tmp/wave2/after.txt && echo "OK: 26 个 method 一个不差"
cargo clippy -p yi-agent-app-server --all-targets -- -D warnings 2>&1 | tail -3
cargo fmt --all
git add -A
git commit -m "refactor(app-server): 拆出 rpc/thread/settings 三片"
```

---

## Task 13: C3d——`lifecycle/` 三片（5 臂 / 930 行 + helper 23 行）

`thread/resume` 单臂 386 行，是全部 23 个臂里最大的。


> **每个新建子文件都要先贴 `use` 头**：`//!` 文档 → `/tmp/wave2/thread_use.rs` 全文 → 搬入的臂/helper → `cargo fix` → `usecheck.py --allow-mod` 守卫。见 Task 10 Step 1.5。

**Files:**
- Modify: `yi-agent-app-server/src/rpc/thread/lifecycle/mod.rs`
- Create: `yi-agent-app-server/src/rpc/thread/lifecycle/resume.rs`
- Create: `yi-agent-app-server/src/rpc/thread/lifecycle/delete.rs`

**Interfaces:**
- Consumes: Task 10 骨架
- Produces: `lifecycle::handle`；`resume::handle` / `delete::handle`

| 文件 | 臂（行数） | 小计 |
|---|---|---|
| `lifecycle/mod.rs` | `start`(160) | 160 + `missing_model_error`(23) |
| `lifecycle/resume.rs` | `resume`(386) | 386 |
| `lifecycle/delete.rs` | `delete`(215) `fork`(107) `clear`(62) | 384 |

- [ ] **Step 1: 切 `resume` 到 `resume.rs`**

`resume` 在 L373–758（386 行）。⚠️ 搬移时**只做三件事**：`continue` → `return Ok(Flow::Continue)`、`method.as_str()` → `req.method.as_str()`、裸局部量 → `state.<字段>`。**不要碰任何逻辑**。

- [ ] **Step 2: 切 `fork` / `delete` / `clear` 到 `delete.rs`**

`fork` L759–865（107 行）、`delete` L1709–1923（215 行）、`clear` L1924–1985（62 行）。注意 `fork` 与 `delete` 之间隔了 6 个臂，需**三次单独搬移**，不能按连续区间切。

- [ ] **Step 3: `missing_model_error` 留在 `lifecycle/mod.rs` 并提权**

它被 `start`（同文件）与 `resume`（`resume.rs`）调用，是 23 行 helper。搬移后 `resume.rs` 用 `super::missing_model_error` 引用，所以它必须 `pub(crate)` 或至少 `pub(super)`：

```bash
cd yi-agent-rs/crates/yi-agent-app-server
python3 - <<'PY'
import re, pathlib
p=pathlib.Path('src/rpc/thread/lifecycle/mod.rs'); s=p.read_text()
s2=re.sub(r'(?m)^fn missing_model_error\b', 'pub(super) fn missing_model_error', s, count=1)
assert s2!=s; p.write_text(s2); print("OK")
PY
```

**不要**复制一份到 `resume.rs`。

- [ ] **Step 4: `lifecycle/mod.rs` 改成三分派**

```rust
pub(crate) mod delete;
pub(crate) mod resume;

pub(crate) async fn handle(state: &mut AppState<'_>, req: &RequestEnvelope, id: RequestId)
    -> anyhow::Result<Flow> {
    match req.method.as_str() {
        "thread/resume" => return resume::handle(state, req, id).await,
        "thread/fork" | "thread/delete" | "thread/clear" => {
            return delete::handle(state, req, id).await
        }
        _ => {}
    }
    match req.method.as_str() {
        "thread/start" => { /* 原臂体 */ }
        _ => return Ok(Flow::Unknown),
    }
    Ok(Flow::Continue)
}
```

- [ ] **Step 5: 编译、测试、校验、提交**

```bash
cd yi-agent-rs
cargo build -p yi-agent-app-server 2>&1 | grep -E "^error" -A3 | head -40
cargo test -p yi-agent-app-server 2>&1 | tail -5
cargo test -p yi-agent-app-server -- --list 2>/dev/null | grep -c ': test$'   # 期望 1167
grep -rhoE '"thread/[a-zA-Z/]+"' crates/yi-agent-app-server/src/rpc/thread/ | tr -d '"' | sort -u > /tmp/wave2/after.txt
diff crates/yi-agent-app-server/src/rpc/thread_methods_baseline.txt /tmp/wave2/after.txt && echo "OK: 26 个 method 一个不差"
cargo clippy -p yi-agent-app-server --all-targets -- -D warnings 2>&1 | tail -3
cargo fmt --all
git add -A
git commit -m "refactor(app-server): 拆出 rpc/thread/lifecycle 三片"
```

---

## Task 14: C3e——`groups/` + `handle()` 外壳改成分组分派


> **每个新建子文件都要先贴 `use` 头**：`//!` 文档 → `/tmp/wave2/thread_use.rs` 全文 → 搬入的臂/helper → `cargo fix` → `usecheck.py --allow-mod` 守卫。见 Task 10 Step 1.5。

**Files:**
- Modify: `yi-agent-app-server/src/rpc/thread/groups/mod.rs`
- Modify: `yi-agent-app-server/src/rpc/thread/mod.rs`（`handle()` 改成 4 个 `owns()` 顺序分派）

**Interfaces:**
- Consumes: Task 11–13 的四组 `handle`
- Produces: `rpc/thread/mod.rs` 的 `handle()` ≤ 60 行

| 文件 | 臂（行数） | helper（行数） | 小计 |
|---|---|---|---|
| `groups/mod.rs` | `lead`(57) + `group/*` 或模式臂(28) | `apply_group_mutation`(176) `require_group_id`(8) | 269 |

- [ ] **Step 1: 切 `lead` 与 `group/*` 或模式臂到 `groups/mod.rs`**

`group/*` 是**一个臂覆盖 4 个 method** 的或模式（L1595–1622，28 行）。搬移时保留或模式形状：

```rust
match req.method.as_str() {
    "thread/group/create" | "thread/group/update" | "thread/group/setMembers"
    | "thread/group/delete" => {
        // 原臂体
    }
    _ => return Ok(Flow::Unknown),
}
```

- [ ] **Step 2: 搬 `apply_group_mutation` 与 `require_group_id` 到 `groups/mod.rs`**

⚠️ `apply_group_mutation`（176 行，L2625–2800）内部也有一句 `match method` 覆盖 `"thread/group/create"` 等 4 个字符串——**它是嵌套 match，不是 `handle` 的臂**。整块搬走，不要拆。

- [ ] **Step 3: `mod.rs` 的 `handle()` 改成四组顺序分派**

把原来的 `match req.method.as_str() { ... }` 整块（此时只剩各组残留）替换为：

```rust
pub(crate) async fn handle(
    state: &mut AppState<'_>,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    let method = req.method.as_str();
    if lifecycle::owns(method) {
        return lifecycle::handle(state, req, id).await;
    }
    if settings::owns(method) {
        return settings::handle(state, req, id).await;
    }
    if content::owns(method) {
        return content::handle(state, req, id).await;
    }
    if groups::owns(method) {
        return groups::handle(state, req, id).await;
    }
    Ok(Flow::Unknown)
}
```

⚠️ **`Flow::Unknown` 语义必须保住**：`serve()` 靠它把不认识的 method 交回原始 dispatch。四个 `owns()` 已由 Task 10 的测试保证穷尽且互斥。

- [ ] **Step 4: 断言 `handle()` 行数 ≤ 60**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
python3 - <<'PY'
import re
L=open('src/rpc/thread/mod.rs').read().splitlines()
h=next(i for i,l in enumerate(L) if re.match(r'^pub\(crate\) async fn handle',l))
depth=0; started=False
for j in range(h,len(L)):
    depth+=L[j].count('{')-L[j].count('}')
    if '{' in L[j]: started=True
    if started and depth==0:
        print(f"handle() = {j-h+1} 行（目标 ≤ 60）"); break
PY
```

- [ ] **Step 5: 断言 `mod.rs` 行数 ≤ 200**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
wc -l < src/rpc/thread/mod.rs
```

若超出，精简 `use` 块与残留 helper。

- [ ] **Step 6: 全量验收**

```bash
cd yi-agent-rs
cargo test -p yi-agent-app-server 2>&1 | tail -8
cargo test -p yi-agent-app-server -- --list 2>/dev/null | grep -c ': test$'   # 期望 1167
cargo clippy -p yi-agent-app-server --all-targets -- -D warnings 2>&1 | tail -3
cargo test -p yi-agent-app-server export_bindings 2>&1 | tail -3
grep -rhoE '"thread/[a-zA-Z/]+"' crates/yi-agent-app-server/src/rpc/thread/ | tr -d '"' | sort -u > /tmp/wave2/after.txt
diff crates/yi-agent-app-server/src/rpc/thread_methods_baseline.txt /tmp/wave2/after.txt && echo "OK: 26 个 method 一个不差"
```

- [ ] **Step 7: 提交**

```bash
cd yi-agent-rs
cargo fmt --all
git add -A
git commit -m "refactor(app-server): 拆出 rpc/thread/groups，handle 改为四组分派"
```

---

## Task 15: 整波收尾——`just ci` 与结构指标

**Files:** 无（只验证与清点）

**Interfaces:**
- Consumes: Task 1–14 全部完成
- Produces: 合并前的最终证据

- [ ] **Step 1: 跑完整的 15 道门禁**

```bash
cd yi-agent-rs
just ci 2>&1 | tail -20
```

期望结尾 `CI passed`。若某项失败，**不要**用 `--no-verify` 绕过；逐项修。

- [ ] **Step 2: 核对结构指标**

```bash
cd yi-agent-rs/crates/yi-agent-app-server
echo "=== server.rs（目标 ≤ 400）==="; wc -l < src/server.rs
echo "=== server.rs 顶层函数（目标 0）==="; grep -cE '^(pub(\(crate\))?\s+)?(async\s+)?fn ' src/server.rs
echo "=== server.rs 顶层项（目标 34）==="; python3 /tmp/wave2/extract.py --list src/server.rs | head -1
echo "=== rpc/thread/mod.rs（目标 ≤ 200）==="; wc -l < src/rpc/thread/mod.rs
echo "=== 最大子模块（目标 ≤ 1000）==="; find src/server src/rpc/thread -maxdepth 3 -name '*.rs' | xargs wc -l | sort -rn | head -3
echo "=== 测试数（目标 1167）==="; cd ../../.. && cargo test -p yi-agent-app-server -- --list 2>/dev/null | grep -c ': test$'
```

五项全部达标才算完成。

- [ ] **Step 3: 核对五项人工检查**

```bash
cd yi-agent-rs
git diff --exit-code main -- ../desktop/src/lib/generated && echo "OK: 协议未动"
git diff --exit-code main -- crates/yi-agent-app-server/src/lib.rs && echo "OK: lib.rs 未动"
git diff --exit-code main -- crates/yi-agent-app-server/src/server/tests/ && echo "OK: server/tests 未动"
git diff --exit-code main -- crates/yi-agent-app-server/src/thread_store.rs && echo "OK: thread_store 未动"
git diff --exit-code main -- crates/yi-agent-app-server/src/translate.rs && echo "OK: translate 未动"
```

五项全部 `OK`。

- [ ] **Step 4: 逐提交确认「一个提交只碰一个模块」**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
git log --oneline main..refactor/wave2-app-server
echo "---"
git log --format='%h %s' --name-only main..refactor/wave2-app-server | head -100
```

每个 `refactor(app-server): 抽出 X` 提交只应改 `server.rs` + `server/X*`（或 `rpc/thread.rs` + `rpc/thread/X*`）。

- [ ] **Step 5: 合并回 main**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
git checkout main
git merge --no-ff refactor/wave2-app-server
cd yi-agent-rs && just ci 2>&1 | tail -5
```

**合并后必须在合并结果上再跑一次 `just ci`**——本地绿不等于合并后绿。

- [ ] **Step 6: 清理 worktree 与 target**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
git branch -d refactor/wave2-app-server
git worktree remove .worktrees/refactor-wave2-app-server
cd yi-agent-rs && cargo clean --dry-run && df -h .
```

按项目规范，确认不再需要增量编译后再 `cargo clean`。

---

## 附录 A：`extract.py`（搬移脚本全文）

这是本计划的**核心工具**。它按「顶层项」切分，不按行区间。必须完整写入 `/tmp/wave2/extract.py`。

```python
#!/usr/bin/env python3
"""按「顶层项」搬移 Rust 源文件里的项。

顶层项 = 紧贴其上的 doc/属性注释 + 声明行 + 主体（到列 0 的 '}' 或行尾 ';'）。

用法:
  extract.py --list SRC
  extract.py --src SRC --dst DST --names a,b,c [--header FILE] [--append] [--move]
"""
import argparse, re, sys, pathlib

ITEM_RE = [
    ('fn',     re.compile(r'^(pub(?:\((?:crate|super)\))?\s+)?(?:async\s+)?fn\s+(\w+)')),
    ('type',   re.compile(r'^(pub(?:\((?:crate|super)\))?\s+)?type\s+(\w+)')),
    ('struct', re.compile(r'^(pub(?:\((?:crate|super)\))?\s+)?(?:struct|enum|union)\s+(\w+)')),
    ('const',  re.compile(r'^(pub(?:\((?:crate|super)\))?\s+)?(?:const|static)\s+(\w+)')),
    ('mod',    re.compile(r'^(pub\(crate\)\s+)?mod\s+(\w+);')),
]
IMPL_RE = re.compile(r'^impl\b')
TL_RE = re.compile(r'^thread_local!\s*\{')

ATTR_PREFIX = ('///', '//!', '//', '#[', '#!')

def find_close_brace(lines, start):
    for e in range(start, len(lines)):
        if lines[e] == '}':
            return e
    raise RuntimeError(f'L{start+1}: 找不到列 0 的 }}')

def find_semicolon(lines, start):
    for e in range(start, len(lines)):
        if lines[e].rstrip().endswith(';'):
            return e
    raise RuntimeError(f'L{start+1}: 找不到行尾 ;')

def strip_doc(lines, start):
    s = start
    while s > 0 and lines[s-1].strip().startswith(ATTR_PREFIX):
        s -= 1
    return s

def scan(lines):
    items = []
    for i, l in enumerate(lines):
        kind = name = None
        for k, rx in ITEM_RE:
            m = rx.match(l)
            if m:
                kind, name = k, m.group(2)
                if k == 'struct':
                    kw = re.match(r'^(?:pub(?:\((?:crate|super)\))?\s+)?(struct|enum|union)\b', l)
                    kind = kw.group(1) if kw else 'struct'
                elif k == 'const':
                    kw = re.match(r'^(?:pub(?:\((?:crate|super)\))?\s+)?(const|static)\b', l)
                    kind = kw.group(1) if kw else 'const'
                break
        if kind:
            start = strip_doc(lines, i)
            end = i if kind == 'mod' else (
                find_semicolon(lines, i) if kind in ('type', 'const', 'static') else find_close_brace(lines, i))
            items.append({'kind': kind, 'name': name, 'start': start, 'end': end, 'decl': i})
        elif IMPL_RE.match(l):
            start = strip_doc(lines, i)
            items.append({'kind': 'impl', 'name': 'impl', 'start': start,
                          'end': find_close_brace(lines, i), 'decl': i})
        elif TL_RE.match(l):
            end = find_close_brace(lines, i)
            m = re.search(r'static\s+([A-Z_][A-Z0-9_]*)', '\n'.join(lines[i:end+1]))
            start = strip_doc(lines, i)
            items.append({'kind': 'tl', 'name': m.group(1) if m else f'tl@{i+1}',
                          'start': start, 'end': end, 'decl': i})
    items.sort(key=lambda x: x['start'])
    for a, b in zip(items, items[1:]):
        if a['end'] >= b['start']:
            raise RuntimeError(f"项重叠: {a['kind']} {a['name']} 与 {b['kind']} {b['name']}")
    return items

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--list', metavar='SRC')
    ap.add_argument('--src'); ap.add_argument('--dst')
    ap.add_argument('--names', default=''); ap.add_argument('--header')
    ap.add_argument('--append', action='store_true'); ap.add_argument('--move', action='store_true')
    ap.add_argument('--verbose', '-v', action='store_true')
    a = ap.parse_args()

    if a.list:
        lines = pathlib.Path(a.list).read_text().splitlines()
        items = scan(lines)
        from collections import Counter
        c = Counter(i['kind'] for i in items)
        print(f"{len(items)} items  ({' / '.join(f'{k} {v}' for k, v in sorted(c.items()))})")
        if a.verbose:
            for it in items:
                print(f"  {it['kind']:6} {it['name']:38} L{it['start']+1:5}-{it['end']+1:5} "
                      f"{it['end']-it['start']+1:4}")
        return

    src = pathlib.Path(a.src); dst = pathlib.Path(a.dst)
    lines = src.read_text().splitlines()
    items = scan(lines)
    want = [n.strip() for n in a.names.split(',') if n.strip()]

    byname = {}
    for it in items:
        byname.setdefault(it['name'], []).append(it)

    picked, missing = [], []
    for n in want:
        if n in byname:
            picked.append(byname[n][0])
        else:
            missing.append(n)
    if missing:
        print(f"ERROR: 未找到 {missing}", file=sys.stderr)
        sys.exit(1)

    # 自动带上 impl 块：按 impl 头里出现的被搬类型名匹配（不看相邻）。
    # 相邻规则在这里是错的：ServeLauncher 的 impl 夹在 LeadTarget 与
    # LeadTickDelivery 之间，两者都属别的文件，相邻规则会把 impl 带走。
    picked_names = {it['name'] for it in picked if it['kind'] in ('struct', 'type', 'enum')}
    picked_ids = {id(p) for p in picked}
    pulled = []
    for it in items:
        if it['kind'] != 'impl' or id(it) in picked_ids:
            continue
        hdr = lines[it['decl']]
        if any(re.search(r'\b' + re.escape(n) + r'\b', hdr) for n in picked_names):
            picked.append(it); picked_ids.add(id(it)); pulled.append(it)

    picked.sort(key=lambda x: x['start'])
    body = '\n'.join(
        '\n'.join(lines[it['start']:it['end']+1]).rstrip() for it in picked)
    nlines = sum(it['end'] - it['start'] + 1 for it in picked)

    if a.append and dst.exists():
        cur = dst.read_text().rstrip()
        dst.write_text(cur + '\n\n' + body + '\n')
    else:
        head = ''
        if a.header:
            head = pathlib.Path(a.header).read_text().rstrip() + '\n\n'
        dst.write_text(head + body + '\n')

    if a.move:
        kill = set()
        for it in picked:
            kill.update(range(it['start'], it['end']+1))
        keep = [l for i, l in enumerate(lines) if i not in kill]
        # 折叠连续空行
        out, prev_blank = [], False
        for l in keep:
            blank = not l.strip()
            if blank and prev_blank:
                continue
            out.append(l); prev_blank = blank
        src.write_text('\n'.join(out) + '\n')

    print(f"moved {len(picked)} items / {nlines} lines from {src} -> {dst}")
    if pulled:
        print(f"impl items pulled in: {len(pulled)}")
    for it in picked:
        print(f"  {it['kind']:6} {it['name']:38} {it['end']-it['start']+1:4} lines")

if __name__ == '__main__':
    main()
```

---

## 附录 B：`promote.py`（提权脚本全文）

```python
#!/usr/bin/env python3
"""把指定顶层项改成 pub(crate)，只加修饰符，不动其它字符。

用法: promote.py --file F --names a,b,c
对 impl 块：跳过（impl 没有可见性修饰符），并打印 skipped。
"""
import argparse, re, pathlib, sys

RX = re.compile(
    r'^(pub(?:\((?:crate|super)\))?\s+)?(?:async\s+)?fn\s+(\w+)'
    r'|^(pub(?:\((?:crate|super)\))?\s+)?type\s+(\w+)'
    r'|^(pub(?:\((?:crate|super)\))?\s+)?(?:struct|enum|union)\s+(\w+)'
    r'|^(pub(?:\((?:crate|super)\))?\s+)?(?:const|static)\s+(\w+)'
)

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--file', required=True); ap.add_argument('--names', required=True)
    a = ap.parse_args()
    p = pathlib.Path(a.file)
    lines = p.read_text().splitlines()
    want = [n.strip() for n in a.names.split(',') if n.strip()]

    hit = {n: 0 for n in want}
    for i, l in enumerate(lines):
        m = RX.match(l)
        if not m:
            continue
        name = next(g for g in (m.group(2), m.group(4), m.group(6), m.group(8)) if g)
        if name not in want:
            continue
        hit[name] += 1
        if m.group(1) or m.group(3) or m.group(5) or m.group(7):
            continue  # 已有可见性修饰符
        # 在 'fn|type|struct|enum|union|const|static' 之前插入 pub(crate)
        lines[i] = re.sub(r'\b(fn|type|struct|enum|union|const|static)\b',
                          r'pub(crate) \1', l, count=1)

    p.write_text('\n'.join(lines) + '\n')
    bad = 0
    for n in want:
        if hit[n] == 0:
            print(f"WARN: 未找到 {n}（可能是 impl 或 thread_local 块，跳过）", file=sys.stderr)
            bad += 1
        elif hit[n] > 1:
            print(f"ERROR: {n} 命中 {hit[n]} 次，不唯一", file=sys.stderr)
            bad += 1
    print(f"promoted {sum(1 for n in want if hit[n]==1)} / {len(want)} names in {p}")
    sys.exit(1 if bad and any(hit[n] > 1 for n in want) else 0)

if __name__ == '__main__':
    main()
```

⚠️ **`promote.py` 对 `thread_local!` 块内的 `static` 有效**：它在 `static NAME` 前插入 `pub(crate) `，得到 `pub(crate) static NAME`。但 `thread_local!` 内的 `static` 行有缩进（4 空格），regex 的 `^(pub...)?` 要求行首——**会匹配失败**。所以对 `thread_local!` 里的名字，`promote.py` 会报 `WARN: 未找到`，必须**手工改**：

```rust
thread_local! {
    pub(crate) static SESSION_MODEL_REF: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}
```

Task 1 Step 4 与 Task 5 Step 3 都涉及这个情况，脚本报 WARN 是预期的，手工补上即可。

---

## 附录 C：C2 与 C3 并行的操作方式

若想并行（spec §4 论证了安全性），从 Task 0 的同一个 worktree 另开一个分支：

```bash
cd .worktrees/refactor-wave2-app-server
git checkout -b refactor/wave2-thread-split main
```

然后在这个分支上只做 Task 10–14（C3），另起一个 worktree 做 Task 1–9（C2）。

**合并顺序无所谓**，但 **C3 合并回来之后必须重跑一次 `cargo test -p yi-agent-app-server`**——因为「C2 的 re-export 表」与「C3 需要的 31 个符号」从未在同一次编译里同时存在过（spec §4 已说明）。

C3 需要的 31 个 `crate::server::` 符号（3 类型 + 28 函数）分布在 C2 的 8 个组，必须被 re-export 覆盖。若合并后编译失败，报的会是 `cannot find function X in module crate::server`——按附录 D 的表查是哪个组漏了。

---

## 附录 D：出问题时的定位顺序

按这个顺序查，不要跳：

1. **`cannot find function X in module crate::server`** → 该函数所在组的 `pub(crate) use` 列表漏了 `X`。
2. **`function X is private` / `static X is private` / `type X is private`** → 搬进子模块后忘了改 `pub(crate)`。用 `promote.py` 补。**注意 `thread_local!` 内的 static 需手工改。**
3. **`use of undeclared type X`** → 你把 18 个必留类型之一搬走了，或漏搬了组内私有类型（`Activation` / `SyncHint` / `SyncAction` / `PluginQueryError` / `PluginSummary` / `RuntimeTooling` / `DeltaCoalescer` / `ServeLauncher` / 5 个 `Lead*` / `TurnPrepareError` / `PreparedTurn` / `PartialTurnInput`）。搬回或补搬。
4. **`missing methods` / 行为变化** → 你把某个 `impl` 块切碎了。用 `extract.py --list` 核对项数，`impl` 应分别是 8 / 3（`ChildrenWatch`/`ProcessWatch`/`TraceWatch` 留在 `server.rs`）等。
5. **`cannot find module 'tests'`** → 你把 `server.rs` 改成了 `server/mod.rs`。改回文件形态。
6. **测试数从 1,166 变了** → 有测试被误删或重复。`git diff main -- crates/yi-agent-app-server/src/server/tests/` 应为空。
7. **`serve()` 编译失败且报借用错误** → 检查是否把域分派块抽成了函数（明令禁止），或改动了 `driver_tick` 的赋值顺序。另：`dispatch_domain!` 必须定义在 `app_state!` **之后**，否则宏展开时 `app_state!` 还不存在。
8. **`owns()` 测试通不过** → 先看报的是「被 0 组认领」还是「被 2 组认领」。前者是漏了 method，后者是两个组都写了它。**不要**改基线文件来迁就测试。
9. **`thread/readItems` 找不到** → 它在 `content` 组（`content/mod.rs` 保留的 4 个臂之一），不在 `list_all` 或 `diff_read`。早先版本的 plan 漏了它。

---

## 附录 E：与 spec 的差异（实测修正）

本计划实测核对后发现 spec 有 6 处数字不准，**以本计划为准**：

| # | spec 写法 | 实测 | 影响 |
|---|---|---|---|
| 1 | `wait_for_plugin_query_socket_within` = 229 行 | **19 行**（229 是到下一函数的行区间，含 11 个类型/const） | `watchman` 组从 365 行降到 116 行 |
| 2 | `driver` 组 = 1,832 行 | **1,884 行**（含 525 行 impl） | 分片必须单独给 `launcher.rs` |
| 3 | `turn` 组 = 1,497 行 | **1,435 行** | 分片微调 |
| 4 | `thread_index` = 639 行 | **580 行** | — |
| 5 | 未提 `impl` 块、`thread_local!`、`const` | **8 个 impl / 4 个 `thread_local!` / 14 个 const** 都必须随组搬走 | 按行区间切分会把它们切碎 |
| 6 | 未提 `thread/readItems` | **`readItems` 74 行在 `content` 组** | 漏了就是 method 丢失 |

另：spec 说「17 个有外部引用的类型」，实测是 **18 个**（`IdleFacts` 也留下了，因为它被 `evict_idle_runtimes` 与 `should_evict` 共用且无外部引用但属 driver/watchman 交叉）。
