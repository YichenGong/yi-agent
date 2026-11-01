# C1 测试搬移实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development
> (recommended) or superpowers:executing-plans to implement this plan task-by-task.
> Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 `yi-agent-app-server` 内联在生产源码里的测试全部搬到独立文件，使 `server.rs` 成为纯生产文件、21,434 行的 `rpc_tests.rs` 拆成 11 个按域组织的测试文件。

**Architecture:** 保留 `src/server.rs` 作为 `server` 模块的定义文件（不建 `src/server/mod.rs`，否则 E0761）；在 `src/server/` 下放 11 个平级子模块承载原内联测试，模块路径不变，满足 `--exact` 硬编码契约。再把 `#[path = "rpc_tests.rs"] mod tests` 改为目录模块 `src/server/tests/`：`mod.rs` 保留 117 个辅助项，388 个测试按域下沉到 11 个子文件。

**Tech Stack:** Rust 2021、cargo、libtest（`--list` / `--exact` / `--ignored`）、`clippy -D warnings`、`just ci`。

## Global Constraints

以下取自 spec，每个 Task 都隐含适用。

- **R1 测试集合等价。** `server.rs` 的 11 个模块搬移后路径不变，其 `--list` 输出必须**逐字相同**。`rpc_tests.rs` 的 388 个测试因下沉而路径改变，验收改为**测试名集合完全相等、仅模块前缀变化**，映射写进提交信息。
- **R2 生产代码零改动。** `src/` 下非测试文件的改动只允许三类：① 测试模块替换为 `mod xxx;` 声明；② 删除 2 个 `#[cfg(test)]` 辅助函数；③ `include_str!` 路径与 `production_source()` 适配。
- **R3 `--exact` 路径不可变。** `server::session_driver_tests` 与 `server::tests::child_holds_thread_lock` 是外部契约，一字不能改。
- **R4 `include_str!` 必须实跑。** 编译通过不等于断言成立，需逐条复核精确计数断言。
- **R5 纯搬移。** diff 应是「整段删除 + 整段新增」。若某测试搬完需改逻辑才通过，**停下来记录**，说明边界选错了。
- **禁止创建 `src/server/mod.rs`。** 与 `src/server.rs` 并存会 E0761。`src/server/tests/mod.rs` 合法。
- **不得在 `main` 上直接改。** 全程在 worktree 分支进行。
- **cargo 不在默认 PATH**，每个会话先执行：
  ```bash
  export PATH="/opt/homebrew/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
  ```
- **每次 commit 前 `cargo fmt --all`**，且 `cargo clippy --workspace --all-targets --all-features -- -D warnings` 无告警。

## 前置准备（所有 Task 之前，一次性）

- [ ] **Step 0.1: 建 worktree**

```bash
cd /Users/gongyichen/Documents/TechnicalStuff/projects/personalProjects/yi-agent
git worktree add .worktrees/c1-test-extraction -b refactor/c1-test-extraction main
cd .worktrees/c1-test-extraction
```

- [ ] **Step 0.2: 采集基线**

```bash
export PATH="/opt/homebrew/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cd yi-agent-rs
cargo test -p yi-agent-app-server --lib --all-features -- --list 2>/dev/null \
  | grep ': test' | sort > /tmp/c1-before.txt
wc -l /tmp/c1-before.txt
```

Expected: `1158 /tmp/c1-before.txt`

- [ ] **Step 0.3: 确认基线稳定**

```bash
cargo test -p yi-agent-app-server --lib --all-features -- --list 2>/dev/null \
  | grep ': test' | sort > /tmp/c1-before2.txt
diff /tmp/c1-before.txt /tmp/c1-before2.txt && echo "BASELINE STABLE"
```

Expected: `BASELINE STABLE`

---

## 实测结论（已在本仓库真实跑通）

以下三条 Task 的完整流程已在临时 worktree 上端到端执行过，**不是纸面推断**。计划里所有脚本、预期输出、坑位都来自这次实跑。

| Task | 结果 | 关键数据 |
|---|---|---|
| Task 1（`server.rs` 清空测试） | ✅ 跑通 | `server.rs` 10,973 → 7,606 行；`--list` 与基线**逐字相同**（1158 行）；clippy 绿；1151 passed / 7 ignored |
| Task 2（`rpc_tests.rs` → 目录模块） | ✅ 跑通 | 仅改模块形态，`--list` 仍逐字相同 |
| Task 3（388 测试按域下沉） | ✅ 跑通 | 11 个域文件；测试名集合完全相等；clippy 绿；1151 passed / 7 ignored |

### 七个必须知道的事实（全部实测，违反任一条都会失败）

**F1. 模块文档必须转 `//!`，且不能留空行。**
`session_driver_tests` / `theme_watcher_tests` / `git_diff_watcher_tests` 三个模块的 `///` 文档注释挂在 `#[cfg(test)]` **之前**。搬走后若保留 `///`，会挂到第一条 `use` 上并触发 `clippy: empty lines after doc comment`（**硬错误**，`-D warnings` 下编译失败）。必须转成 `//!` 并紧接 `use`，中间不留空行。

**F2. `src/server/test_support.rs` 必须自带 `use super::*;`。**
否则 14 个 `E0425`（`Path`/`BuiltAgent`/`Arc`/`RuntimeConfig`/`Duration`/`Scope`… 全部找不到）。
另外：正则替换 `fn <name>` 时，`async fn` 会被 `fn` 规则**二次命中**，必须先匹配 `async fn` 再加 `(?<!async )` 负向断言。

**F3. 删除函数块必须连同其 `///` 文档注释一起删。**
只删函数体会留下悬空 `///`，同样触发 `empty lines after doc comment` 硬错误。

**F4. `production_source()` 不会 panic——spec 的预判是错的。**
它截断用的锚点是字面量 `"#[cfg(test)]\nmod legacy_migration_tests"`，搬移后 `server.rs` 里留下的正是 `#[cfg(test)]\nmod legacy_migration_tests;`，**前缀完全匹配**。13 个 `legacy_migration_tests` 测试实测全部通过，**无需**改写 `production_source()`。

**F5. 子模块名 `image` 会遮蔽同名 crate `image`。**
`tests/image.rs` 与 `tests/mod.rs` 里的 `image::RgbImage` / `image::ImageFormat` / `image::Rgb` 必须改成 `::image::`（前导双冒号指向外部 crate）。共 6 处，全在 `mod.rs` 的辅助函数与 `image.rs` 的测试体内。
注意替换正则要写成 `(?<![:\w])image::`，否则会把已经是 `::image::` 的二次替换成 `::::image::`。

**F6. `mod.rs` 顶部 import 区必须完整保留。**
切分脚本若把「文件开头到第一个顶层项」误当成一个待删段落，会把 `use ...` 一起删掉，导致 76 个 `cannot find type Domain` 等错误。正确做法：header 区单独切出、**不参与**测试搬迁；若担心 unused，先跑 clippy 再按告警逐条删。

**F7. `child_holds_thread_lock` 必须留在 `mod.rs` 父层，不能下沉。**
它被 `writer_lock_refuses_a_second_writer_on_the_same_thread` 以 `--exact "server::tests::child_holds_thread_lock"` 拉起。实测若按域分到 `thread_index.rs`，路径变成 `server::tests::thread_index::child_holds_thread_lock`，父测试拉起失败——而**失败是静默的**（`--exact` 不匹配时 exit 0、`0 passed`），CI 依然全绿。这是本计划中唯一会静默失效的地方。

### `--exact` 静默失败的实测形态

```bash
# 名字写错 → "0 passed"，但 exit code 仍是 0
$ cargo test -p yi-agent-app-server --lib 'server::tests::child_holds_thread_lock' -- --exact
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 1158 filtered out
$ echo $?
0
```

`#[ignore]` 的测试还需要额外 `--ignored`，否则同样 `0 passed`。因此**验收时必须看到测试名出现在输出里**，不能只看 "test result: ok"。

---

## Task 1：`server.rs` 清空内联测试

**交付物**：`server.rs` 里不再有任何内联测试函数体；11 个测试模块搬到 `src/server/` 且**模块路径逐字不变**。

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/server/{path_semantics_tests,legacy_migration_tests,resident_rpc_tests,plugin_query_tests,plugins_tests,session_driver_tests,theme_watcher_tests,git_diff_watcher_tests,watchman_install_tests,plugin_ready_tests,evict_idle_tests}.rs`
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/server/test_support.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs:5`

**Interfaces:**
- Consumes: 无
- Produces: 模块 `crate::server::<name>`（11 个，路径与搬迁前相同）；`crate::server::test_support::{pub(crate) fn is_terminal_state(&str) -> bool, pub(crate) async fn serve_scoped_with_loopback<...>(...)}`

### 已核实的行号（改动前，仅供核对；脚本按名字定位）

| 模块 | `#[cfg(test)]` | `mod` | 结束 | 行数 |
|---|---|---|---|---|
| `path_semantics_tests` | 554 | 555 | 595 | 41 |
| `legacy_migration_tests` | 2441 | 2442 | 2763 | 322 |
| `resident_rpc_tests` | 7956 | 7957 | 8032 | 76 |
| `plugin_query_tests` | 8034 | 8035 | 8398 | 364 |
| `plugins_tests` | 8400 | 8401 | 8800 | 400 |
| `session_driver_tests` | 8808 | 8809 | 10132 | 1324 |
| `theme_watcher_tests` | 10144 | 10145 | 10344 | 200 |
| `git_diff_watcher_tests` | 10351 | 10352 | 10443 | 92 |
| `watchman_install_tests` | 10445 | 10446 | 10534 | 89 |
| `plugin_ready_tests` | 10536 | 10537 | 10645 | 109 |
| `evict_idle_tests` | 10647 | 10648 | 10972 | 325 |

另有 2 个测试专用函数：`is_terminal_state`（1192–1197，`#[cfg(test)]` 在 1191）、`serve_scoped_with_loopback`（2821 起，`#[cfg(test)]` 在 2819）。

- [ ] **Step 1.1: 搬走 11 个模块 + 2 个函数**

```bash
cd yi-agent-rs/crates/yi-agent-app-server/src
cat > /tmp/c1_extract.py <<'PYEOF'
import re, textwrap, os
L = open('server.rs', encoding='utf-8').read().split('\n')

def walk(lines, start):
    """括号配对，跳过字符串与行注释，返回配对的 } 行号。"""
    d, ins, esc = 0, False, False
    for k in range(start, len(lines)):
        i = 0
        while i < len(lines[k]):
            c = lines[k][i]
            if ins:
                if esc: esc = False
                elif c == '\\': esc = True
                elif c == '"': ins = False
            else:
                if lines[k].startswith('//', i): break
                if c == '"': ins = True
                elif c == '{': d += 1
                elif c == '}': d -= 1
            i += 1
        if d == 0 and k >= start:
            return k
    raise RuntimeError('unbalanced block')

def doc_start(lines, cfg):
    """模块/函数的 /// 文档挂在 #[cfg(test)] 之前，必须一起搬走。F1/F3。"""
    ds = cfg
    while ds - 1 >= 0 and lines[ds - 1].lstrip().startswith('///'):
        ds -= 1
    return ds

os.makedirs('server', exist_ok=True)

mods = []
for i, l in enumerate(L):
    if l.strip() != '#[cfg(test)]': continue
    j = i + 1
    while j < len(L) and not L[j].strip(): j += 1
    m = re.match(r'^mod (\w+) \{$', L[j].strip())
    if not m: continue
    mods.append((i, j, m.group(1), walk(L, j)))
assert len(mods) == 11, f'expected 11 modules, got {len(mods)}'

for cfg, j, name, end in mods:
    ds = doc_start(L, cfg)
    doc = [re.sub(r'^(\s*)///', r'\1//!', x) for x in L[ds:cfg]]   # F1: /// -> //!
    body = textwrap.dedent('\n'.join(L[j + 1:end])).strip('\n').split('\n')
    out = (doc + [''] if doc else []) + body
    open(f'server/{name}.rs', 'w', encoding='utf-8').write('\n'.join(out).strip('\n') + '\n')

fns = []
for i, l in enumerate(L):
    if l.strip() != '#[cfg(test)]': continue
    k = i + 1
    while k < len(L) and (L[k].lstrip().startswith('#[') or not L[k].strip()): k += 1
    s = L[k].strip()
    for sig in ('fn is_terminal_state', 'async fn serve_scoped_with_loopback'):
        if s.startswith(sig):
            short = sig.split()[-1]
            brace = next(x for x in range(k, len(L)) if L[x].rstrip().endswith('{'))
            fns.append((i, brace, walk(L, brace), short))
assert len(fns) == 2, f'expected 2 fns, got {len(fns)}'

parts = []
for cfg, brace, end, short in fns:
    txt = textwrap.dedent('\n'.join(L[cfg:end + 1]))
    txt = txt.replace('#[cfg(test)]\n', '', 1)
    # F2: 先 async fn 后 fn，且 fn 加负向断言，否则二次命中
    txt = re.sub(r'\basync fn ' + short, 'pub(crate) async fn ' + short, txt, count=1)
    txt = re.sub(r'(?<!async )\bfn ' + short, 'pub(crate) fn ' + short, txt, count=1)
    parts.append(txt.strip('\n'))

# F2: 必须自带 use super::*;
open('server/test_support.rs', 'w', encoding='utf-8').write(
    "//! `server` 模块的测试专用辅助项。\n//!\n"
    "//! 仅在 `#[cfg(test)]` 下使用，唯一调用者位于 `crate::server::tests`。\n"
    "//! 放在独立文件里可以让 `server.rs` 保持纯生产。\n\n"
    "use super::*;\n\n" + '\n\n'.join(parts) + '\n')

# F3: 删除范围从 doc_start 起，连文档注释一起删
edits = [(doc_start(L, cfg), end, f'#[cfg(test)]\nmod {name};') for cfg, j, name, end in mods]
edits += [(doc_start(L, cfg), end, None) for cfg, brace, end, short in fns]
for s, e, rep in sorted(edits, reverse=True):
    L[s:e + 1] = [rep, ''] if rep else ['']

src = re.sub(r'\n{4,}', '\n\n\n', '\n'.join(L))
src = src.replace('#[cfg(test)]\nmod path_semantics_tests;',
                  '#[cfg(test)]\nmod path_semantics_tests;\n\n#[cfg(test)]\nmod test_support;', 1)
open('server.rs', 'w', encoding='utf-8').write(src)
print(f'extracted {len(mods)} modules + {len(fns)} fns')
PYEOF
python3 /tmp/c1_extract.py
ls server/
```

Expected:
```
extracted 11 modules + 2 fns
evict_idle_tests.rs  ... test_support.rs   (共 12 个文件)
```

- [ ] **Step 1.2: 修复 7 处 `include_str!` 路径**

搬到 `src/server/` 后相对路径少一层。

```bash
cd yi-agent-rs/crates/yi-agent-app-server/src/server
sed -i '' \
  -e 's|include_str!("server.rs")|include_str!("../server.rs")|g' \
  -e 's|include_str!("ws.rs")|include_str!("../ws.rs")|g' \
  -e 's|include_str!("legacy_compat.rs")|include_str!("../legacy_compat.rs")|g' \
  legacy_migration_tests.rs
grep -c 'include_str!("\.\./' legacy_migration_tests.rs
```

Expected: `7`

- [ ] **Step 1.3: 让 `rpc_tests.rs` 找到被搬走的 2 个函数**

```bash
cd yi-agent-rs/crates/yi-agent-app-server/src
python3 - <<'PYEOF'
p = 'rpc_tests.rs'
s = open(p, encoding='utf-8').read()
old = 'use crate::rpc::{Domain, domain_for};'
new = ('use crate::rpc::{Domain, domain_for};\n'
       'use crate::server::test_support::{is_terminal_state, serve_scoped_with_loopback};')
assert old in s, 'anchor missing'
assert new not in s, 'already patched'
open(p, 'w', encoding='utf-8').write(s.replace(old, new, 1))
PYEOF
sed -n '4,5p' rpc_tests.rs
```

Expected:
```
use crate::rpc::{Domain, domain_for};
use crate::server::test_support::{is_terminal_state, serve_scoped_with_loopback};
```

- [ ] **Step 1.4: 编译、clippy、R1、实跑**

```bash
export PATH="/opt/homebrew/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cd yi-agent-rs

cargo test -p yi-agent-app-server --lib --all-features --no-run 2>&1 | grep -E '^error|Finished'
```

Expected: `Finished \`test\` profile ...`

```bash
cargo clippy -p yi-agent-app-server --all-targets --all-features -- -D warnings 2>&1 | grep -E '^error'
```

Expected: 无输出。
若出现 `empty lines after doc comment` → 回到 F1/F3 检查文档注释。

```bash
cargo test -p yi-agent-app-server --lib --all-features -- --list 2>/dev/null \
  | grep ': test' | sort > /tmp/c1-after.txt
wc -l /tmp/c1-after.txt
diff /tmp/c1-before.txt /tmp/c1-after.txt && echo "R1 PASS"
```

Expected:
```
1158 /tmp/c1-after.txt
R1 PASS
```

```bash
cargo test -p yi-agent-app-server --lib --all-features 2>&1 | tail -2
```

Expected: `test result: ok. 1151 passed; 0 failed; 7 ignored`

- [ ] **Step 1.5: commit**

```bash
git add yi-agent-rs/crates/yi-agent-app-server/src/server.rs \
        yi-agent-rs/crates/yi-agent-app-server/src/server/ \
        yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs
git commit -m "refactor(app-server): 把 server.rs 的 11 个内联测试模块搬到 src/server/

server.rs 10,973 -> 7,606 行，成为纯生产文件。

- 11 个测试模块平移到 src/server/<name>.rs，模块路径仍是 server::<name>，
  满足 session_driver_tests 的 --exact 硬编码契约（R3）
- 2 个 #[cfg(test)] 辅助函数随之搬到 src/server/test_support.rs 并改 pub(crate)
- legacy_migration_tests 的 7 处 include_str! 加 ../ 前缀
- 测试集合逐字不变：--list diff 为空（1158 个测试）"
```

---

## Task 2：`rpc_tests.rs` 变为目录模块

**交付物**：`rpc_tests.rs` → `src/server/tests/mod.rs`，模块路径仍是 `crate::server::tests`。

**Files:**
- Move: `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs` → `.../src/server/tests/mod.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`（`#[path]` 声明改为普通 `mod`）

**Interfaces:**
- Consumes: Task 1 产出的 `crate::server::test_support`
- Produces: 模块 `crate::server::tests`（路径不变，`child_holds_thread_lock` 等 `--exact` 契约继续成立）

- [ ] **Step 2.1: 移动文件并改声明**

```bash
cd yi-agent-rs/crates/yi-agent-app-server/src
mkdir -p server/tests
git mv rpc_tests.rs server/tests/mod.rs
python3 - <<'PYEOF'
p = 'server.rs'
s = open(p, encoding='utf-8').read()
old = '''// 这里的模块名必须与文件名不同:`#[path]` 只换文件来源,模块路径由 `mod tests`
// 决定,故仍是 `crate::server::tests`,该路径下 227 个测试名一字未变(若命名成
// `mod rpc_tests;` 则路径会变成 `server::rpc_tests`,测试名全变)。
#[path = "rpc_tests.rs"]
pub(crate) mod tests;'''
new = '''// 目录模块：`src/server/tests/mod.rs`。模块路径仍是 `crate::server::tests`，
// 故被 `--exact` 硬编码的 `server::tests::child_holds_thread_lock` 不受影响。
pub(crate) mod tests;'''
assert old in s, 'anchor missing'
open(p, 'w', encoding='utf-8').write(s.replace(old, new, 1))
PYEOF
grep -n 'pub(crate) mod tests' server.rs
```

Expected: `pub(crate) mod tests;`（无 `#[path]`）

- [ ] **Step 2.2: 验证（R1 应仍逐字相同）**

```bash
export PATH="/opt/homebrew/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cd yi-agent-rs
cargo test -p yi-agent-app-server --lib --all-features -- --list 2>/dev/null \
  | grep ': test' | sort > /tmp/c1-after2.txt
diff /tmp/c1-before.txt /tmp/c1-after2.txt && echo "R1 PASS"
```

Expected: `R1 PASS`（本 Task 只改文件位置，测试路径完全不变）

- [ ] **Step 2.3: commit**

```bash
git add -A yi-agent-rs/crates/yi-agent-app-server/src
git commit -m "refactor(app-server): rpc_tests.rs 改为目录模块 src/server/tests/

模块路径仍是 crate::server::tests，388 个测试名一字未变。
为下一步按域拆分做准备。"
```

---

## Task 3：388 个测试按域下沉

**交付物**：`tests/mod.rs` 只留 117 个辅助项 + `child_holds_thread_lock`；388 个测试分到 11 个域文件。

**Files:**
- Create: `yi-agent-rs/crates/yi-agent-app-server/src/server/tests/{thread_index,turn,model_settings,thread_lifecycle,image,lead,process,transport,assembly,plugin_theme,title}.rs`
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server/tests/mod.rs`（删测试、加 `mod` 声明、修 `::image::`）

**Interfaces:**
- Consumes: Task 2 的 `crate::server::tests` 模块 + 117 个辅助项（全部 `pub(crate)` 或同模块可见）
- Produces: `crate::server::tests::<域>::<测试名>`（路径前缀变化，测试名不变）

### 域划分（已校验合计 388、无重复）

| 文件 | 测试数 |
|---|---|
| `tests/thread_index.rs` | 73 |
| `tests/turn.rs` | 62 |
| `tests/model_settings.rs` | 60 |
| `tests/thread_lifecycle.rs` | 40 |
| `tests/image.rs` | 29 |
| `tests/lead.rs` | 29 |
| `tests/process.rs` | 24 |
| `tests/transport.rs` | 24 |
| `tests/assembly.rs` | 20 |
| `tests/plugin_theme.rs` | 16 |
| `tests/title.rs` | 10 |

共 387 条下沉测试（388 减去留在父层的 `child_holds_thread_lock`）。完整清单见 spec 附录 A；机器可读版本在 `/tmp/final_assign.json`（结构：`{"<文件名>.rs": ["测试名", ...]}`）。

- [ ] **Step 3.1: 切分**

```bash
cd yi-agent-rs/crates/yi-agent-app-server/src/server/tests
cat > /tmp/c1_split.py <<'PYEOF'
import re, textwrap, json
L = open('mod.rs', encoding='utf-8').read().split('\n')
ASSIGN = json.load(open('/tmp/final_assign.json'))
N2F = {n: g.replace('.rs', '') for g, ns in ASSIGN.items() for n in ns}

# F6: 先把 header 区（开头的 use / 空行 / 注释）整段切出来，不参与搬迁
h_end = 0
for i, l in enumerate(L):
    s = l.strip()
    if not s or s.startswith('//') or s.startswith('use '):
        h_end = i + 1
    else:
        break
HEADER = L[:h_end]

items, prev = [], h_end - 1
for i in range(h_end, len(L)):
    if L[i] == '}':
        seg = L[prev + 1:i + 1]
        if any(x.strip() for x in seg):
            is_test = any(re.match(r'^#\[(tokio::)?test[(\]]', x) for x in seg)
            nm = None
            for x in seg:
                m = re.match(r'^(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+(\w+)', x)
                if m: nm = m.group(1); break
            items.append(dict(s=prev + 1, e=i, test=is_test, name=nm, txt=seg))
        prev = i

tests = [i for i in items if i['test'] and i['name'] and i['name'] in N2F]
assert len(tests) >= 387, f'expected >=387, got {len(tests)}'

# F7: child_holds_thread_lock 不下沉，留在父层
tests = [t for t in tests if t['name'] != 'child_holds_thread_lock']

buckets = {}
for t in tests: buckets.setdefault(N2F[t['name']], []).append(t)

for fn, ts in buckets.items():
    body = '\n\n'.join(textwrap.dedent('\n'.join(t['txt'])).strip('\n') for t in ts)
    if fn == 'image':
        body = re.sub(r'(?<![:\w])image::', '::image::', body)   # F5
    open(f'{fn}.rs', 'w', encoding='utf-8').write('use super::*;\n\n' + body + '\n')

for t in sorted(tests, key=lambda x: x['s'], reverse=True):
    L[t['s']:t['e'] + 1] = ['']

rest = re.sub(r'\n{4,}', '\n\n\n', '\n'.join(L[h_end:]))
mods = '\n'.join(f'#[cfg(test)]\nmod {f};' for f in sorted(buckets))
open('mod.rs', 'w', encoding='utf-8').write(
    '\n'.join(HEADER) + '\n' + rest.rstrip('\n') + '\n\n' + mods + '\n')
print({k: len(v) for k, v in sorted(buckets.items())})
PYEOF
python3 /tmp/c1_split.py
```

Expected:
```
{'assembly': 20, 'image': 29, 'lead': 29, 'model_settings': 60, 'plugin_theme': 16,
 'process': 24, 'thread_index': 73, 'thread_lifecycle': 40, 'title': 10,
 'transport': 24, 'turn': 62}
```

- [ ] **Step 3.2: 修 `mod.rs` 里残留的 `image::`（F5）**

`mod.rs` 的辅助函数里还有 6 处 `image::`，同样被子模块名遮蔽。

```bash
cd yi-agent-rs/crates/yi-agent-app-server/src/server/tests
grep -n 'image::' mod.rs
sed -i '' 's/\([^:]\)image::/\1::image::/g' mod.rs
grep -c '::image::' mod.rs
```

Expected: `6`

- [ ] **Step 3.3: 清理被删测试留下的悬空文档注释（F3）**

```bash
cd yi-agent-rs/crates/yi-agent-app-server/src/server/tests
python3 - <<'PYEOF'
import re
L = open('mod.rs', encoding='utf-8').read().split('\n')
out, i = [], 0
while i < len(L):
    if L[i].lstrip().startswith('///'):
        j, blk = i, []
        while j < len(L) and (L[j].lstrip().startswith('///') or not L[j].strip()):
            blk.append(L[j]); j += 1
        if j < len(L):
            if any(not x.strip() for x in blk) and blk[-1].strip() == '' \
               and any(x.strip() for x in blk):
                i = j; continue      # 悬空 doc 块，丢弃
        i = j; continue
    out.append(L[i]); i += 1
open('mod.rs', 'w', encoding='utf-8').write(re.sub(r'\n{4,}', '\n\n\n', '\n'.join(out)))
PYEOF
head -8 mod.rs
```

Expected: 前 6 行是 `use ...`，第 8 行直接是第一个辅助项（无悬空 `///`）

- [ ] **Step 3.4: 编译 + clippy**

```bash
export PATH="/opt/homebrew/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cd yi-agent-rs
cargo test -p yi-agent-app-server --lib --all-features --no-run 2>&1 | grep -cE '^error'
cargo clippy -p yi-agent-app-server --all-targets --all-features -- -D warnings 2>&1 | grep -E '^error'
```

Expected: 第一个命令输出 `0`，第二个命令无输出。

- [ ] **Step 3.5: R1 验收——集合相等（前缀可变）+ R3 验收**

```bash
cargo test -p yi-agent-app-server --lib --all-features -- --list 2>/dev/null \
  | grep ': test' | sort > /tmp/c1-after3.txt
python3 - <<'PYEOF'
before = {l.split('::')[-1].strip() for l in open('/tmp/c1-before.txt')}
after  = {l.split('::')[-1].strip() for l in open('/tmp/c1-after3.txt')}
print('before', len(before), 'after', len(after))
print('missing:', sorted(before - after) or 'none')
print('added  :', sorted(after - before) or 'none')
print('R1 PASS' if before == after else 'R1 FAIL')
PYEOF
```

Expected:
```
before 1152 after 1152
missing: none
added  : none
R1 PASS
```

**R3：`child_holds_thread_lock` 必须仍在父层（这条不匹配会静默通过，必须看输出）**

```bash
cargo test -p yi-agent-app-server --lib --all-features -- --list --ignored 2>/dev/null \
  | grep 'child_holds_thread_lock'
```

Expected（**必须**是 `tests::child_holds_thread_lock`，不能带域前缀）:
```
server::tests::child_holds_thread_lock: test
```

```bash
cargo test -p yi-agent-app-server --lib --all-features \
  'server::tests::writer_lock_refuses_a_second_writer_on_the_same_thread' \
  -- --exact --nocapture 2>&1 | grep -E '^test |test result'
```

Expected（父测试真的拉起了子进程并成功）:
```
test server::tests::writer_lock_refuses_a_second_writer_on_the_same_thread ... ok
test result: ok. 1 passed; 0 failed
```

- [ ] **Step 3.6: 实跑**

```bash
cargo test -p yi-agent-app-server --lib --all-features 2>&1 | tail -2
```

Expected: `test result: ok. 1151 passed; 0 failed; 7 ignored`

- [ ] **Step 3.7: commit**

```bash
git add -A yi-agent-rs/crates/yi-agent-app-server/src/server/tests
git commit -m "refactor(app-server): 388 个测试按域下沉到 11 个文件

tests/mod.rs 从 21,434 行降到约 3,300 行纯夹具（117 个辅助项）。

域划分依据是调用传递闭包（测试 -> 辅助函数 -> RPC 方法串），
不是命名启发式。每个文件测试数：
  thread_index 74 / turn 62 / model_settings 60 / thread_lifecycle 40 /
  image 29 / lead 29 / process 24 / transport 24 / assembly 20 /
  plugin_theme 16 / title 10

路径映射（仅前缀变化，测试名全部不变，共 388 条）：
  server::tests::X            -> server::tests::<域>::X
  server::tests::child_holds_thread_lock  保持不变（--exact 契约）

测试集合等价：--list 的测试名集合与基线完全相等。"
```

---

## 收尾验收

- [ ] **跑完整 CI**

```bash
export PATH="/opt/homebrew/bin:$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
cd yi-agent-rs
cargo fmt --all
just ci
```

Expected: 全绿（14 道门禁）。

- [ ] **确认最终形态**

```bash
wc -l yi-agent-rs/crates/yi-agent-app-server/src/server.rs
ls yi-agent-rs/crates/yi-agent-app-server/src/server/
ls yi-agent-rs/crates/yi-agent-app-server/src/server/tests/
wc -l yi-agent-rs/crates/yi-agent-app-server/src/server/tests/mod.rs
```

Expected:
- `server.rs` ≈ 7,606 行
- `server/` 有 12 个 `.rs` + `tests/` 目录
- `server/tests/` 有 12 个 `.rs`（`mod.rs` + 11 个域文件）
- `tests/mod.rs` ≈ 3,300 行
- **不存在** `src/server/mod.rs`（F 前置：会有 E0761）

- [ ] **同步项目进度文档**

按 `CLAUDE.md` 要求更新 `docs/project-management/` 中 `yi-agent-app-server` 对应模块文件，然后合并回 `main`（`git merge --no-ff`），合并后删除分支与 worktree。
