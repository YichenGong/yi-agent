# C1：把内联测试搬出生产文件（重构第 1 步）

**状态：** 设计（待评审）
**日期：** 2026-11-01
**范围：** 仅 `yi-agent-app-server` crate。`tui/app.rs`、`main.rs`、`desktop/` 均**不在**本次范围。
**上游：** 重构分析中的「Lane C1」。
**下游：** C2（`server.rs` 按职责切模块）。本 spec 完成后 `server.rs` 成为纯生产文件，C2 才具备可审阅的 diff 基础。

---

## 1. 为什么先做这件事

重构分析发现代码库的结构问题不在 lint、不在 crate 分层（依赖图无环、`yi-agent-core` 零内部依赖、`clippy --all-targets --all-features -D warnings` 全绿）。问题在**文件粒度过粗**，而 `yi-agent-app-server` 这个 crate 最严重：

| 文件 | 总行 | 生产行 | 测试行 | 测试占比 |
|---|---|---|---|---|
| `src/rpc_tests.rs` | 21,435 | — | 21,435 | 100% |
| `src/server.rs` | 10,973 | 7,559 | 3,414 | 31% |

`server.rs` 的问题不只是长，而是测试**穿插在生产代码之间**：`#[cfg(test)]` 共 14 处（12 个测试模块 + 2 个仅测试构建的生产辅助函数），把 7,559 行生产代码切成 15 段。任何一次职责切分（C2）都要跨越若干个测试块，diff 无法审阅。

**先搬测试 = 用一次零语义的机械操作，换取 C2 的风险可控。** 本阶段本身不修任何 bug；若只做这一阶段，收益是 `server.rs` 变成纯生产文件、`rpc_tests.rs` 变得可导航。

---

## 2. 目标与非目标

### 目标

1. `server.rs` 的 12 个测试模块与 2 个测试专用辅助函数搬离，**`server.rs` 的 `#[cfg(test)]` 归零**。
2. `rpc_tests.rs`（21,435 行 / 388 测试）按域切成多个文件，父模块保持 `server::tests`。
3. 生产逻辑零改动。

### 非目标

- **不**拆分任何生产函数。`rpc/thread.rs` 的 2,347 行 `handle()`、`core/agent.rs` 的 718 行 `run_loop()` 不在范围。
- **不**按职责重新组织 `server.rs` 的生产代码（那是 C2）。
- **不**动 `tui/app.rs`（12,061 行）、`main.rs`（5,828 行）、`desktop/`。
- **不**删除、合并、"顺便改进"任何测试。
- **不**引入 nextest 或改测试运行方式。

---

## 3. 现状测绘（全部实测）

### 3.1 `server.rs`：14 处 `#[cfg(test)]`

`cargo test -p yi-agent-app-server --lib -- --list` 实测 **1,158** 个测试，其中 `server::` 前缀 **476** 个。

| 行区间 | 行数 | 模块名 | 测试数 |
|---|---|---|---|
| 554–595 | 42 | `path_semantics_tests` | 4 |
| 2,441–2,763 | 323 | `legacy_migration_tests` | 13 |
| 7,956–8,032 | 77 | `resident_rpc_tests` | 3 |
| 8,034–8,398 | 365 | `plugin_query_tests` | 11 |
| 8,400–8,800 | 401 | `plugins_tests` | 15 |
| 8,808–10,132 | 1,325 | `session_driver_tests` | 18 |
| 10,144–10,344 | 201 | `theme_watcher_tests` | 4 |
| 10,351–10,443 | 93 | `git_diff_watcher_tests` | 2 |
| 10,445–10,534 | 90 | `watchman_install_tests` | 4 |
| 10,536–10,645 | 110 | `plugin_ready_tests` | 4 |
| 10,647–10,972 | 326 | `evict_idle_tests` | 10 |
| 10,134–10,137 | 4 | `#[path="rpc_tests.rs"] mod tests` | 388 |
| 1,191–1,197 | 7 | ⚠️ `#[cfg(test)] fn is_terminal_state` | 非测试块 |
| 2,819–2,862 | 44 | ⚠️ `#[cfg(test)] async fn serve_scoped_with_loopback` | 非测试块 |

两个 `⚠️` 是**仅测试构建存在的生产辅助函数**，不是测试模块，不能当测试搬：

- `is_terminal_state`（`server.rs:1192`）：唯一调用者 `rpc_tests.rs:2892`。
- `serve_scoped_with_loopback`（`server.rs:2821`）：唯一调用者 `rpc_tests.rs:17841`。

二者搬到测试夹具（新的 `src/server/test_support.rs`），`server.rs` 里删除。

### 3.2 `rpc_tests.rs`：388 测试 / 18,372 行，与 117 个辅助项穿插

| 项 | 数量 | 行数 |
|---|---|---|
| 测试（`#[test]` + `#[tokio::test]`） | 388 | 18,372 |
| 非测试顶层项（helper / impl / struct / const） | 117 | 3,062 |
| 合计 | 505 | 21,434 |

**关键**：二者不是"前半测试、后半辅助"的两段，而是**交错排列**。例如 `build_test_agent`（L1793）、`Harness`（L2031）、`initialize`（L2524）都在测试堆的中间。这决定了切分方案（§6）。

### 3.3 `rpc_tests.rs` 是跨文件共享的夹具面

被 4 处**测试代码**引用（均已确认位于 `#[cfg(test)]` 内，非测试构建不受影响）：

| 引用点 | 用到的名字 |
|---|---|
| `ws.rs:1242` | `serve_ws_with_permission_agent` |
| `test_support.rs:61` | `Harness` |
| `server.rs:8418` | `Harness`、`initialize` |
| `server.rs:8811` | `build_slow_agent`、`build_test_agent`、`test_config` |
| `server.rs:9722/9726` | `Harness::new`、`initialize` |

`server.rs:10134` 的既有注释已冻结这件事：

> 这里的模块名必须与文件名不同：`#[path]` 只换文件来源，模块路径由 `mod tests` 决定，故仍是 `crate::server::tests`，该路径下 227 个测试名一字未变。

---

## 4. 三条会静默失败的硬约束（实测验证）

这一节是整份 spec 最重要的部分。三条都是**搬错会让 CI 全绿但测试没跑**或**编译才报错**的区别，不是"注意一下"。

### 约束 1：4 处测试用 `--exact` 起子进程并硬编码完整路径，不匹配时 exit 0

| 位置 | 硬编码路径 |
|---|---|
| `server.rs:9244/9258/9309/9332`（经 `logs_of_ignored_probe`） | `"server::session_driver_tests::probe_a_registered_project_without_a_driver_plugin"` 等 4 个 |
| `rpc_tests.rs:21163` | `"server::tests::child_holds_thread_lock"` |
| `thread_store.rs:4233` | `"thread_store::tests::child_tries_thread_lock"`（不动） |
| `src/session_store.rs:378` | `"session_store::tests::child_holds_lock"`（不动） |

**实测失败形态**：

```
$ cargo test -p yi-agent-app-server --lib -- --exact server::tests::definitely_not_a_real_test
running 0 tests
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 1158 filtered out
```

**exit code 0。路径对不上 = 测试静默消失 = CI 绿。**

**推论**：`session_driver_tests` 的名字与父路径不可变（`server::session_driver_tests`），因此它**只能平移到 `src/server/session_driver_tests.rs`**，不能下沉到子模块。`server::tests` 这个名字不可变，但其下的测试**可以**下沉（见 §6 方案 B 的处理）。

### 约束 2：7 处 `include_str!` 是源码自检测试

全部在 `server.rs` 的 `legacy_migration_tests` 内（行 2561 / 2585 / 2673 读 `"server.rs"`，2710 读 `"legacy_compat.rs"`，2611 / 2645 / 2696 读 `"ws.rs"`）。

搬到 `src/server/legacy_migration_tests.rs` 后需改成 `"../server.rs"` / `"../ws.rs"` / `"../legacy_compat.rs"`（已实测该写法可解析）。

**但改路径不够**。`production_source()`（`server.rs:2583`）用

```rust
include_str!("server.rs").split("#[cfg(test)]\nmod legacy_migration_tests").next()
```

把文件截断在测试模块之前。**搬移后这个锚点在 `server.rs` 里消失了**，`.expect("this module must exist")` 会 panic。

这是**显式失败**（会被测试发现），可接受。正确做法：搬移后 `server.rs` 是纯生产文件，`production_source()` 退化为"读 `../server.rs` + 去注释行"，**比现在更健壮**——测试自己的字符串字面量不再污染生产文件。

同时 `the_lead_tick_arm_is_gated_by_the_flag`（`server.rs:2556`）刻意不用 `production_source()`，注释说明"它在**本测试模块之前**截断，而 tick 臂在 `serve()` 里、位于本模块之后"。它读完整源码并用 `rsplit` 取最后一处，搬移后语义不变，但**必须实跑确认**。

### 约束 3：`server::tests` 是活动夹具面

见 §3.3。切分后以下名字必须在 `server::tests` 路径下以 `pub(crate)` 可达，否则 `ws.rs`、`test_support.rs`、`server.rs` 编译失败：

`Harness`、`initialize`、`build_test_agent`、`build_slow_agent`、`test_config`、`serve_ws_with_permission_agent`、`child_holds_thread_lock`（及其余子模块的 `use super::*` 依赖的私有 helper）。

---

## 5. 五条护栏

**R1 —— 测试集合等价。**
因为 `server.rs` 的 12 个模块搬移后路径不变，这部分的 `--list` 输出必须**逐字相同**。`rpc_tests.rs` 的 388 个测试因下沉而路径改变，验收改为：**测试名集合完全相等，仅模块前缀变化**，且映射表写进提交信息。

**R2 —— 生产代码零改动。**
`git diff` 中 `src/` 下非测试文件的改动只允许三类：① 测试模块替换为 `mod xxx;` 声明；② 删除 2 个 `#[cfg(test)]` 辅助函数；③ `include_str!` 路径与 `production_source()` 适配。

**R3 —— `--exact` 路径不可变。**
`server::session_driver_tests` 与 `server::tests::child_holds_thread_lock` 是外部契约。

**R4 —— `include_str!` 必须实跑。**
编译通过只证明文件找到了，不代表断言仍成立。特别要复核 `every_production_entry_runs_the_migration_at_startup` 里的 `assert_eq!(count, 2)` 这类**精确计数**断言。

**R5 —— 纯搬移。**
diff 应是"整段删除 + 整段新增"。若某测试搬完需要改逻辑才通过，**停下来记录**，说明边界选错了。

---

## 6. 实施方案

### 阶段 A：`server.rs` 清空测试（必须先做）

**A1** 新建 `src/server/` 目录，把 11 个测试模块平移到 `src/server/<name>.rs`：

```
src/server.rs                        ← 保留，生产文件
src/server/path_semantics_tests.rs
src/server/legacy_migration_tests.rs
src/server/resident_rpc_tests.rs
src/server/plugin_query_tests.rs
src/server/plugins_tests.rs
src/server/session_driver_tests.rs   ← 名字不可变（R3）
src/server/theme_watcher_tests.rs
src/server/git_diff_watcher_tests.rs
src/server/watchman_install_tests.rs
src/server/plugin_ready_tests.rs
src/server/evict_idle_tests.rs
```

`server.rs` 原位置留 `mod <name>;`。已实测：该形态下模块路径保持 `server::<name>`。

> ⚠️ **E0761 陷阱（`src/server.rs` 与 `src/server/mod.rs` 只能二选一）**
>
> E0761 是 rustc 的**模块文件歧义**错误：一个模块同时存在两种合法来源时，编译器拒绝猜。
>
> ```rust
> pub mod server;   // 此时 src/server.rs 与 src/server/mod.rs 都在
> ```
> ```
> error[E0761]: file for module `server` found at both "src/server.rs" and "src/server/mod.rs"
>  --> src/lib.rs:1:1
>   = help: delete or rename one of them to remove the ambiguity
> ```
>
> 关键在于**它只管"同一模块的定义文件不能有两个"，不禁止"文件 + 同名目录"并存**。这两种形态实测都合法：
>
> | 形态 | 结构 | 结果 |
> |---|---|---|
> | ✅ 文件 + 同名目录 | `src/server.rs` + `src/server/child.rs`（`server.rs` 里写 `pub mod child;`） | 编译通过 |
> | ✅ 文件 + 同名目录（带 mod.rs） | `src/server.rs` + `src/server/tests/mod.rs` | 编译通过，路径 `server::tests::...` |
> | ❌ 文件 + 目录根 mod.rs | `src/server.rs` + `src/server/mod.rs` | **E0761** |
>
> 所以本方案的形态是：**保留 `src/server.rs` 作为生产文件**，`src/server/` 目录只放它的子模块。目录里**不要**出现 `mod.rs`，除非那一层模块本来就是由 `src/server/tests/` 这样的子目录承载（那时 `tests/mod.rs` 是 `tests` 模块的定义文件，`server` 模块的定义文件仍是 `server.rs`，不构成歧义）。

**A2** 把 2 个测试专用函数搬到 `src/server/test_support.rs`，改 `pub(crate)`，`server.rs` 里删除。

完成后 `server.rs`：10,973 → **≈ 7,577** 行（10,973 − 3,357 测试块 − 51 辅助函数 + 12 声明），`#[cfg(test)]` = **0**。

**A3** 把 `rpc_tests.rs` 变为模块目录 `src/server/tests/`，父模块名仍为 `tests`：

```rust
// server.rs 中
#[cfg(test)]
pub(crate) mod tests;
```

`src/server/tests/mod.rs` 承载全部 117 个辅助项（3,062 行）+ `pub(crate)` 导出，388 个测试按 §6.2 下沉到子模块。

### 6.1 为什么辅助项全留父模块

辅助项与测试交错排列，若跟着测试一起切，会出现跨子模块的 `use` 网（例如 `Harness` 被几乎所有测试用，`build_test_agent` 被多个域用）。全部留在 `tests/mod.rs` 有两个直接好处：

1. **零 import 改动**：子模块只要 `use super::*;` 就能拿到全部夹具（已实测：子模块可见父模块私有项）。
2. **夹具面天然保持**：`ws.rs` / `test_support.rs` / `server.rs` 继续通过 `server::tests::X` 引用，路径不变（满足约束 3）。

代价：`tests/mod.rs` 是 3,062 行的夹具文件。但这个文件**不含测试**，职责单一（"造测试环境"），比现在 21,435 行的混合文件好得多。若将来需要再拆，可独立处理。

### 6.2 388 个测试的域划分（已定稿）

归类依据是**证据而不是命名直觉**：先对每个测试求"调用传递闭包"（测试 → 它调用的辅助函数 → 辅助函数里出现的 RPC 方法串），取其中出现次数最多的域；少数不含任何 RPC 调用的测试（纯装配层、纯函数）按测试名归类。

行数为**测试段行数**（含文档注释与 `#[test]` 属性行），不含 117 个辅助项（那部分留在 `tests/mod.rs`）。

| 文件 | 测试数 | 测试段行数 |
|---|---|---|
| `tests/thread_index.rs` | 73 | 3,050 |
| `tests/turn.rs` | 62 | 3,589 |
| `tests/model_settings.rs` | 60 | 2,067 |
| `tests/thread_lifecycle.rs` | 40 | 1,920 |
| `tests/image.rs` | 29 | 2,171 |
| `tests/lead.rs` | 29 | 1,696 |
| `tests/process.rs` | 24 | 761 |
| `tests/transport.rs` | 24 | 852 |
| `tests/assembly.rs` | 20 | 1,080 |
| `tests/plugin_theme.rs` | 16 | 621 |
| `tests/title.rs` | 10 | 516 |
| **下沉小计** | **387** | **18,323** |
| `tests/mod.rs` 里的 `child_holds_thread_lock` | 1 | 49 |
| **合计** | **388** | **18,372** |

已校验：**388 个测试全部有归属，无遗漏、无重复**（其中 387 个下沉到域文件，`child_holds_thread_lock` 留在父层）。完整映射见附录 A——它就是 plan 阶段直接执行的清单，不需要再重新归类。

`child_holds_thread_lock` **不在**上表任何文件里：按 §6.3，它留在 `tests/mod.rs` 父模块层级。

### 6.3 约束 1 在 A3 中的具体处理

`child_holds_thread_lock`（`rpc_tests.rs:21258`）被 `:21163` 以 `"server::tests::child_holds_thread_lock"` 精确拉起。

若它下沉到 `tests/thread_index.rs`，路径变成 `server::tests::thread_index::child_holds_thread_lock`，**违反 R3**。

**采用：把它留在 `tests/mod.rs` 的父模块层级。** 路径保持 `server::tests::child_holds_thread_lock`，硬编码字符串一字不改。代价是它与同域的 `writer_lock_refuses_a_second_writer_on_the_same_thread` 分居两处，需在双方加注释互相指路。

---

## 7. 单元顺序与可交付切片

| # | 内容 | 独立可交付 | 前置 |
|---|---|---|---|
| 1 | A1 + A2（`server.rs` 清空测试） | ✅ | — |
| 2 | A3-part1：`tests/mod.rs` 成型（辅助项归位 + 388 测试原样留在 mod.rs 的 `#[cfg(test)]` 内袋） | ✅ | 1 |
| 3 | A3-part2：388 测试按域下沉 | ✅ | 2、映射表定稿 |

**切片的理由**：第 1 步完成后 `server.rs` 已是纯生产文件，**C2 即可开工**。第 2 步完成后 `rpc_tests.rs` 从"21,435 行单文件"变为"3,062 行夹具 + 待下沉测试"，已可导航。第 3 步是纯整理，可以延后甚至暂停。

每一步都能独立合并、独立回滚。

---

## 8. 验收

```bash
# 每步必跑（yi-agent-rs/ 下）
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings

# R1：第 1 步要求逐字相同；第 3 步要求集合相等（前缀可变）
cargo test -p yi-agent-app-server --lib --all-features -- --list | grep ': test' | sort > /tmp/after.txt
diff /tmp/before.txt /tmp/after.txt

# R4：必须实跑
cargo test -p yi-agent-app-server --lib --all-features legacy_migration_tests
cargo test -p yi-agent-app-server --lib --all-features session_driver_tests
cargo test -p yi-agent-app-server --lib --all-features thread_lock
cargo test -p yi-agent-app-server --lib --all-features ws

# 完整
just ci
```

**`--exact` 静默通过的防御**：任何触及 `session_driver_tests` 或 `child_holds_thread_lock` 的改动，改完后必须确认测试**确实执行**（`-- --nocapture` 输出里看到名字，或看到 `LOCK_HELD`），不能只看"测试通过"。

---

## 9. 风险与对策

| 风险 | 影响 | 对策 |
|---|---|---|
| `--exact` 路径对不上，探针静默不跑 | **CI 绿但测试消失**，最高危 | R3；A3 中把该测试留父模块层；改完实跑确认 |
| `production_source()` 锚点消失 | panic（显式） | 改写为读整份生产文件 |
| `include_str!` 路径失效 | 编译失败（显式） | 改 `"../xxx.rs"` |
| `include_str!` 内容变化致计数断言失准 | **测试通过但断言已无意义** | R4 逐条复核精确计数 |
| 夹具面切断 | `ws.rs` / `test_support.rs` 编译失败（显式） | 辅助项全留 `tests/mod.rs`，`use super::*` 透传 |
| 388 测试归类错位 | 后续再挪一次，成本翻倍 | §6.2 要求先人工定稿映射表再动手 |
| 并行开发在 `server.rs` 上改生产代码 | 冲突 | 第 1 步尽快合并 |

---

## 10. 已实测验证的前提

不是推断，都用最小可编译 crate 实跑：

| 主张 | 结果 |
|---|---|
| `src/server.rs` + `src/server/<name>.rs` → 路径仍 `server::<name>` | ✅ |
| `src/server/tests/thread.rs` → `server::tests::thread::<name>` | ✅ |
| 子模块可见父模块**私有**项（`use super::*`） | ✅ |
| `pub(crate)` 夹具在子模块与外文件测试中可达 | ✅ |
| `include_str!("../server.rs")` 从 `src/server/` 出发 | ✅ |
| `include_str!("../../server.rs")` 从 `src/server/tests/` 出发 | ✅ |
| `src/server.rs` 与 `src/server/mod.rs` 并存 | ❌ E0761 |
| `#[cfg(test)]` 模块被非测试代码引用 | ❌ E0433（故只允许测试代码引用） |
| `--exact` 不匹配 | ✅ **exit 0**，静默通过 |
| `--list` 输出可作稳定基线 | ✅ 连续两次 `diff` 为空 |

---

## 11. 工作量

| 步 | 内容 | 估计 |
|---|---|---|
| 1 | A1（11 模块平移）+ A2（2 函数归位） | 1.5–2 小时，机械 |
| 2 | A3-part1（`tests/mod.rs` 成型，117 辅助项归位） | 2–3 小时，机械但需细心 |
| 3 | A3-part2（388 测试下沉，先定稿映射表） | 半天到一天，本 spec 唯一需要判断力的部分 |
| — | 映射表人工校验 | 1–2 小时 |

---

## 12. 待评审确认

1. **A3 的域划分**（§6.2）：11 个文件是机器初稿。是否接受"写 plan 前先人工定稿 388 条映射表"这个额外步骤？我倾向于接受——归类错位的返工成本远高于一次人工过一遍。

2. **`tests/mod.rs` 保留 3,062 行夹具**（§6.1）：我认为这是对的（零 import 改动、夹具面稳定），但它确实留下了一个大文件。是否可接受？

---

## 附录 A：388 个测试的最终归属清单

执行时按此表逐条搬迁。清单已校验（合计 388，无重复）。

> 说明：`child_holds_thread_lock` 不在此表，它留在 `tests/mod.rs`（见 §6.3）。

#### `tests/thread_index.rs`（73 个）

- `a_board_thread_folds_into_its_project_group`
- `a_control_client_may_delete_a_thread`
- `a_later_frame_written_during_a_refresh_is_not_clobbered`
- `an_admin_client_may_delete_a_thread`
- `an_observe_client_cannot_delete_a_thread`
- `blank_lines_are_ignored`
- `cold_thread_reports_idle`
- `deleting_a_card_session_also_removes_its_worktree`
- `deleting_a_card_session_outside_the_kanban_dir_keeps_the_directory`
- `deleting_a_card_session_reports_a_failed_worktree_removal`
- `deleting_a_card_session_with_dirty_worktree_needs_confirmation`
- `deleting_a_plain_session_leaves_its_directory_alone`
- `deleting_a_thread_prunes_it_from_groups`
- `diff_read_does_not_block_the_main_loop`
- `diff_read_panic_fallback_matches_the_callers_shape`
- `eof_exits_gracefully`
- `group_create_persists_and_validates_workspace`
- `group_mutations_broadcast_groups_changed`
- `group_update_set_members_and_delete`
- `groups_are_listed_and_writable`
- `initialize_plans_a_drop_for_a_deleted_thread`
- `invalid_preserve_value_is_rejected`
- `linked_roots_do_not_leak_across_threads_in_one_cwd`
- `list_all_always_includes_an_empty_groups_array`
- `list_all_carries_content_epoch_and_last_item_id`
- `list_all_carries_groups_and_hides_stale_members`
- `list_all_reports_the_read_watermarks`
- `list_all_scan_does_not_block_the_main_loop`
- `malformed_json_returns_parse_error`
- `mark_read_broadcasts_a_list_level_read_state_update`
- `mark_read_persists_and_is_idempotent`
- `mark_read_rejects_an_unknown_thread`
- `mark_read_requires_control_scope`
- `mark_read_routes_to_the_thread_domain`
- `oversized_frame_returns_err`
- `plain_workspaces_still_group_by_directory`
- `plan_sync_drops_a_deleted_thread_end_to_end`
- `read_state_reaches_an_unsubscribed_client`
- `refresh_preserves_an_explicit_base`
- `reorder_pinned_rewrites_order_and_rejects_bad_input`
- `set_pinned_false_removes_from_pinned_list`
- `set_pinned_puts_thread_on_top_of_list_all_pinned`
- `set_pinned_rejects_non_boolean_and_unknown_id`
- `set_tag_broadcasts_tag_changed`
- `set_tag_null_clears`
- `set_tag_persists_and_reports_in_list`
- `set_tag_rejects_unknown_color_and_unknown_thread`
- `thread_delete_active_thread_removes_it_from_memory`
- `thread_delete_drops_the_view_frame`
- `thread_delete_removes_files_and_unknown_is_error`
- `thread_diff_read_rejects_unknown_thread`
- `thread_diff_read_reports_base_and_files`
- `thread_list_all_groups_by_workspace`
- `thread_list_all_includes_missing_dir_with_empty_threads`
- `thread_list_empty_returns_empty_array`
- `thread_list_returns_created_threads`
- `thread_rename_unknown_returns_unknown_thread`
- `thread_start_records_linked_roots_and_resume_rebuilds_them`
- `thread_view_read_refresh_produces_a_structured_frame`
- `thread_view_read_refresh_reruns_a_command_frame`
- `thread_view_read_rejects_unknown_thread`
- `thread_view_read_returns_null_before_any_frame`
- `unknown_method_returns_method_not_found`
- `view_read_refresh_does_not_block_the_main_loop`
- `workspace_add_list_remove_round_trip`
- `workspace_add_rejects_non_directory`
- `workspace_link_rejects_self_and_unknown_dir`
- `workspace_list_hides_card_worktrees`
- `workspace_list_marks_missing_directory`
- `workspace_list_reports_linked_to`
- `workspace_remove_accepts_alternate_path`
- `writer_lock_does_not_block_subscriptions`
- `writer_lock_refuses_a_second_writer_on_the_same_thread`


#### `tests/turn.rs`（62 个）

- `a_completed_item_never_trails_its_own_buffered_delta`
- `a_completed_turn_removes_its_checkpoint`
- `a_control_client_may_switch_permission_mode`
- `a_failed_append_still_records_the_completion`
- `a_finished_turn_records_its_completion_time`
- `a_foreground_request_makes_the_background_turn_yield`
- `a_reject_to_an_idle_thread_starts_a_turn_that_carries_the_reason`
- `a_turn_while_off_persists_without_credentials`
- `a_turn_with_default_retention_persists_the_session_verbatim`
- `an_in_flight_turn_leaves_a_checkpoint`
- `an_interrupt_without_an_active_turn_is_an_error`
- `an_observe_client_cannot_set_preserve_thinking`
- `an_observe_client_cannot_switch_permission_mode`
- `approval_timeout_leaves_awaiting_status`
- `clear_does_not_record_a_completion`
- `driver_clears_the_checkpoint_when_the_initiator_disconnects_during_approval`
- `driver_ignores_interrupt_tagged_with_other_turn`
- `driver_uses_unique_item_ids_across_turns`
- `interrupt_on_an_idle_thread_reports_that_it_was_already_idle`
- `list_exposes_permission_mode`
- `merged_loop_fans_out_permission_mode_change_to_ws_client`
- `permission_request_ids_are_unique_across_threads`
- `permission_request_round_trip_allows_tool`
- `permission_timeout_defaults_to_deny`
- `read_items_does_not_interrupt_a_running_turn`
- `resume_flags_a_crashed_partial_turn_as_interrupted`
- `resume_honors_a_persisted_preserve_thinking_off`
- `resume_promotes_the_crashed_turn_so_a_later_turn_cannot_drop_it`
- `set_permission_mode_rejects_bad_mode_and_unknown_thread`
- `set_permission_mode_toggles_and_persists`
- `set_preserve_thinking_false_strips_the_shared_session`
- `set_preserve_thinking_true_leaves_the_shared_session_untouched`
- `stale_interrupt_does_not_cancel_turn_during_approval`
- `switching_permission_mode_broadcasts_the_change`
- `the_persisted_completion_is_not_later_than_the_broadcast_one`
- `the_second_approval_answer_is_a_noop_and_the_first_wins`
- `thread_clear_is_rejected_while_a_turn_is_running`
- `thread_compact_is_rejected_while_a_turn_is_running`
- `thread_resume_response_carries_persisted_permission_mode`
- `thread_set_preserve_thinking_persists_and_broadcasts`
- `thread_start_response_carries_permission_mode`
- `toggling_preserve_off_persists_a_stripped_transcript`
- `turn_completed_carries_its_completion_time`
- `turn_completion_persists_items_and_messages`
- `turn_emits_running_then_idle_status`
- `turn_interject_while_running_keeps_the_same_turn`
- `turn_interject_without_an_active_turn_is_rejected`
- `turn_interrupt_cancels_active_turn`
- `turn_interrupt_cancels_after_yolo_while_approval_is_pending`
- `turn_interrupt_kills_a_running_tool_after_yolo`
- `turn_interrupt_still_cancels_after_switching_to_yolo_mid_turn`
- `turn_interrupt_works_while_provider_call_is_blocking`
- `turn_start_emits_full_notification_sequence`
- `turn_start_empty_input_returns_invalid_params`
- `turn_start_missing_thread_id_returns_invalid_params`
- `turn_start_unknown_thread_returns_unknown_thread`
- `turn_start_while_turn_in_progress_returns_turn_in_progress`
- `turn_start_with_a_same_epoch_but_absent_anchor_is_rejected`
- `turn_start_with_a_stale_epoch_is_rejected`
- `turn_start_with_an_anchor_behind_the_tail_is_accepted`
- `turn_start_without_a_context_anchor_is_not_validated`
- `two_threads_run_turns_concurrently`


#### `tests/model_settings.rs`（60 个）

- `a_control_client_may_write_mcp_config`
- `a_session_effort_override_beats_the_entry_default`
- `a_session_model_ref_selects_the_catalog_entry`
- `a_thread_started_after_a_catalog_change_uses_the_new_entry`
- `an_observe_client_cannot_apply_onboarding`
- `an_observe_client_cannot_set_reasoning_effort`
- `an_observe_client_may_list_but_not_write_models`
- `an_observe_client_may_not_write_mcp_config`
- `an_observe_client_may_read_websearch_but_not_write_it`
- `an_unknown_model_ref_falls_back_to_the_catalog_default`
- `an_upsert_that_only_changes_the_key_does_not_reprobe`
- `config_read_follows_the_session_model_override`
- `config_read_returns_redacted_view`
- `config_read_with_thread_id_reports_that_session_config`
- `idle_driver_serves_a_set_model_command_without_a_new_turn`
- `invalid_effort_value_is_rejected`
- `model_list_over_dispatch_masks_the_key`
- `model_probe_is_deduplicated_while_one_is_running`
- `model_probe_on_an_unknown_entry_is_a_structured_error`
- `model_probe_records_capabilities_and_broadcasts_them`
- `no_effort_anywhere_stays_unset`
- `no_override_and_no_default_falls_back_to_the_global_cfg`
- `onboarding_apply_lands_the_catalog_after_a_passing_probe`
- `onboarding_apply_refuses_when_the_probe_fails`
- `onboarding_status_is_readable_and_reports_needed`
- `onboarding_test_probes_the_endpoint`
- `session_extra_roots_slot_scopes_and_restores`
- `set_model_command_keeps_the_agent_when_the_provider_fails`
- `set_model_command_rebuilds_with_the_new_model`
- `set_reasoning_effort_accepts_a_supported_tier`
- `set_reasoning_effort_command_rebuilds_without_a_new_provider`
- `set_reasoning_effort_rejects_a_tier_the_model_does_not_support`
- `set_reasoning_effort_stays_permissive_when_capabilities_are_unknown`
- `settings_read_over_rpc_reports_unset_fields_as_null`
- `settings_write_rejects_an_invalid_value`
- `settings_write_then_read_round_trips`
- `startup_migrates_legacy_model_keys_into_the_catalog`
- `the_session_effort_override_does_not_leak_across_calls`
- `the_session_model_ref_does_not_leak_across_calls`
- `thread_list_exposes_model_ref_override_and_null`
- `thread_set_model_persists_and_reports_the_effective_model`
- `thread_set_model_to_null_clears_the_override`
- `thread_set_model_to_unknown_name_is_rejected`
- `thread_set_reasoning_effort_persists_and_broadcasts`
- `thread_start_accepts_an_explicit_injection_without_a_catalog`
- `thread_start_reports_a_missing_model_configuration`
- `ui_settings_read_and_write_round_trip`
- `ui_settings_read_reports_null_when_no_relay_url_is_stored`
- `ui_settings_read_reports_the_stored_relay_url`
- `ui_settings_read_returns_tag_names_overrides`
- `ui_settings_tag_name_length_counts_characters_not_bytes`
- `ui_settings_tag_name_validation_precedes_the_disk_write`
- `ui_settings_write_broadcasts_tag_names_updated`
- `ui_settings_write_clears_the_relay_url`
- `ui_settings_write_ignores_a_mistyped_relay_url`
- `ui_settings_write_notifies_subscribers`
- `ui_settings_write_rejects_an_unknown_theme`
- `ui_settings_write_rejects_invalid_tag_name_key_and_empty_value`
- `ui_settings_write_then_read_round_trips_the_relay_url`
- `writing_the_relay_url_keeps_the_theme`


#### `tests/thread_lifecycle.rs`（40 个）

- `a_new_session_anchors_the_default_diff_to_its_starting_commit`
- `a_reject_to_an_unknown_thread_is_an_error`
- `an_upsert_with_a_new_connection_starts_a_probe`
- `chunk_items_for_replay_preserves_order_and_splits_by_count`
- `chunk_items_for_replay_splits_by_bytes`
- `clearing_a_thread_bumps_its_content_epoch`
- `clearing_the_override_falls_back_to_the_entry_default`
- `compact_does_not_erase_last_item_id`
- `compaction_replaces_messages_without_changing_the_handle`
- `initialize_plans_a_full_resync_for_a_cleared_thread`
- `initialize_plans_a_full_resync_when_the_anchor_is_gone`
- `initialize_plans_nothing_for_a_thread_that_has_no_anchor`
- `parse_resume_hints_is_bounded_and_skips_malformed`
- `plan_sync_treats_a_null_anchor_as_present_when_epochs_match`
- `read_items_returns_persisted_items_and_slices_after_id`
- `rebuilding_a_thread_agent_keeps_one_session_handle`
- `resume_passes_persisted_mode_to_factory`
- `resume_replays_every_item_for_long_threads`
- `session_commands_clear_the_stale_input_tokens_without_changing_the_handle`
- `thread_clear_empties_the_context_and_resume_does_not_revive_it`
- `thread_clear_rejects_an_unknown_thread`
- `thread_compact_reports_compacted_when_history_shrinks`
- `thread_compact_reports_failure_without_breaking_the_connection`
- `thread_compact_reports_not_reduced_for_a_short_history`
- `thread_cwd_prefers_memory_then_the_index`
- `thread_fork_copies_a_settled_thread`
- `thread_fork_refuses_a_running_thread_and_writes_nothing`
- `thread_fork_refuses_when_the_working_directory_is_gone`
- `thread_fork_rejects_an_unknown_thread`
- `thread_ops_target_thread_cwd_not_global_workdir`
- `thread_resume_missing_cwd_errors`
- `thread_resume_replays_history_and_restores_context`
- `thread_resume_unknown_returns_unknown_thread`
- `thread_start_allocates_uuid_id_and_writes_meta`
- `thread_start_and_resume_report_the_built_agents_model`
- `thread_start_default_cwd_indexes_canonical_path`
- `thread_start_emits_thread_started_and_responds`
- `thread_start_factory_receives_thread_cwd`
- `thread_start_with_bad_cwd_returns_invalid_params`
- `thread_start_with_cwd_writes_meta_cwd`


#### `tests/lead.rs`（29 个）

- `a_blocking_ask_lead_receives_the_lead_reply`
- `a_card_session_does_not_get_ask_lead`
- `a_cold_lead_delivery_restores_the_persisted_history`
- `a_delivered_message_reaches_the_lead_thread`
- `a_lead_in_a_repo_subdirectory_sees_the_memory_tools`
- `a_lead_thread_never_calls_the_title_model_even_without_a_title`
- `a_lead_turn_without_a_request_id_answers_nobody`
- `a_merge_round_session_does_not_get_ask_lead`
- `a_missing_flag_value_registers_no_lead_tools`
- `a_registered_request_id_receives_the_leads_reply_text`
- `a_second_asker_on_the_same_lead_gets_its_own_reply`
- `a_stuck_thread_wakes_the_lead_with_a_digest_naming_it`
- `identical_digests_are_not_delivered_twice`
- `lead_reply_text_is_none_without_an_agent_message`
- `list_all_does_not_seed_a_lead_when_the_flag_is_off`
- `list_all_exposes_the_lead_role_only_for_the_lead`
- `list_all_never_treats_a_card_worktree_as_a_project_with_a_lead`
- `only_the_lead_sees_the_control_tools`
- `only_the_lead_sees_the_memory_tools`
- `only_the_lead_sees_the_view_tools`
- `the_background_tick_never_creates_a_lead`
- `the_lead_appears_in_list_all`
- `the_lead_does_not_get_ask_lead`
- `thread_lead_is_idempotent_per_project`
- `thread_lead_is_method_not_found_when_the_flag_is_off`
- `thread_lead_refuses_to_create_a_lead_in_a_card_worktree`
- `thread_lead_reports_not_found_before_validating_params_when_off`
- `thread_lead_still_works_when_the_flag_is_on`
- `thread_rename_does_not_change_lead_title`


#### `tests/image.rs`（29 个）

- `a_re_encoded_image_persists_the_stored_file_size`
- `an_image_reaches_the_model_as_a_content_block`
- `an_uploaded_image_cannot_be_ingested_into_another_thread`
- `an_uploaded_image_reaches_the_model_as_a_content_block`
- `an_uploaded_image_round_trips_through_commit_and_turn_start`
- `clearing_a_thread_keeps_its_attachments`
- `deleting_a_thread_prunes_its_image_upload_sessions`
- `deleting_a_thread_removes_its_attachments`
- `image_only_input_does_not_generate_a_title`
- `image_read_does_not_scale_with_file_size`
- `image_read_refuses_a_non_image_file`
- `image_read_refuses_a_non_regular_file`
- `image_read_refuses_a_path_outside_the_workspace`
- `image_read_refuses_an_oversized_image`
- `image_read_refuses_an_unknown_thread`
- `image_read_reports_the_final_chunk_with_a_null_offset`
- `image_read_round_trips_a_file_in_chunks`
- `image_upload_abort_discards_the_staging_file`
- `image_upload_refuses_out_of_order_oversized_and_too_large_uploads`
- `turn_start_accepts_an_attachment_only_message`
- `turn_start_accepts_an_image_only_message`
- `turn_start_copies_attachments_and_echoes_them_on_the_item`
- `turn_start_drops_an_unknown_upload_id_and_still_runs_the_turn`
- `turn_start_prompt_reaching_the_model_contains_the_attachment_manifest`
- `turn_start_rejects_a_non_image_image_input`
- `turn_start_rejects_an_unreadable_attachment`
- `turn_start_reports_a_structured_code_for_a_bad_image_path`
- `turn_start_reports_a_structured_code_for_a_non_image_file`
- `turn_start_with_an_image_injects_an_image_block_and_an_item_ref`


#### `tests/transport.rs`（24 个）

- `a_client_can_subscribe_to_a_thread_set`
- `a_control_client_can_redeem_a_code_with_pair_redeem`
- `a_control_client_cannot_pair_or_revoke`
- `agent_factory_failure_returns_internal_error`
- `delta_coalescer_concat_within_an_item_and_flush_across_items`
- `driver_reports_finished_when_writer_fails`
- `every_tui_notification_field_is_writable_and_readable`
- `idle_driver_serves_a_session_command_without_a_new_turn`
- `initialize_plans_nothing_when_the_hint_matches`
- `initialize_responds_with_server_info`
- `initialize_without_hints_reports_an_empty_sync_plan`
- `log_outcome_handles_every_variant`
- `merged_loop_control_client_is_denied_admin_rpc`
- `merged_loop_fans_out_stdio_notifications_to_ws_client`
- `merged_loop_returns_ok_on_stdio_eof_with_relay_configured`
- `notification_thread_key_matches_the_wire`
- `notifications_are_routed_by_thread_key`
- `pair_create_returns_a_code_and_device_revoke_invalidates_it`
- `plan_sync_covers_the_four_verdicts`
- `plan_sync_is_empty_for_no_hints`
- `request_before_initialize_returns_not_initialized`
- `without_subscribers_deltas_stay_immediate_and_in_order`
- `write_or_fatal_reports_a_local_write_failure_on_the_fatal_channel`
- `write_response_to_a_gone_ws_client_is_not_fatal`


#### `tests/process.rs`（24 个）

- `a_finished_child_stays_in_the_list`
- `agent_cancel_requires_a_confirmation_token`
- `agent_children_list_is_empty_without_an_attached_runtime`
- `agent_message_requires_a_task_and_text`
- `agent_message_without_a_runtime_is_an_error`
- `agent_methods_reject_an_unknown_thread`
- `agent_trace_read_requires_a_task_id`
- `agent_trace_unwatch_is_idempotent`
- `children_are_scoped_by_the_conversation_marker`
- `children_for_thread_carries_task_usage`
- `clear_resets_persisted_totals`
- `config_read_rejects_an_unknown_thread_instead_of_the_process_snapshot`
- `process_kill_reports_an_unknown_process_as_an_error`
- `process_kill_terminates_a_held_manager_process`
- `process_list_and_read_observe_the_managers_the_factory_handed_over`
- `process_list_is_empty_for_a_fresh_thread_and_for_an_unknown_one`
- `process_read_reports_an_unknown_process_as_an_error`
- `read_items_returns_persisted_totals`
- `resume_of_a_live_thread_rebinds_process_watcher_to_the_effective_manager`
- `subagent_baseline_never_underflows`
- `subagent_baseline_subtracts_cleared_usage`
- `subagent_totals_sums_every_child`
- `the_agent_notifications_use_their_documented_wire_shape`
- `the_root_is_never_listed_as_a_child`


#### `tests/assembly.rs`（20 个）

- `a_dead_runtime_is_repaired_on_the_next_tool_call`
- `a_git_project_gets_the_delegation_tools`
- `a_git_project_gets_the_show_git_diff_tool`
- `a_non_git_cwd_attaches_in_place_with_the_delegation_tools`
- `a_project_directory_keys_the_same_before_and_after_it_exists`
- `a_reclaimed_worktree_converges_instead_of_retrying_forever`
- `a_symlinked_project_keys_by_its_target`
- `a_vanished_project_directory_is_refused_and_not_recreated`
- `delegation_tooling_carries_the_theme_tool`
- `delegation_tooling_uses_the_threads_live_yolo_switch`
- `lead_tools_follow_the_flag`
- `no_lead_tools_exist_when_the_flag_is_off`
- `rebuilding_the_thread_agent_keeps_the_approval_path`
- `the_app_side_loop_ensures_every_registered_project`
- `the_assembly_hands_the_reply_registry_to_ask_lead`
- `the_non_delegation_registry_carries_the_theme_tool`
- `the_session_env_reaches_the_bash_child`
- `the_theme_tool_is_registered_into_a_thread_registry`
- `two_threads_in_one_cwd_share_the_daemon_but_not_the_root`
- `ws_client_for_device_resolves_through_the_installed_registry`


#### `tests/plugin_theme.rs`（16 个）

- `a_failed_reinstall_does_not_destroy_a_working_installation`
- `a_failed_watchman_install_reports_a_warning_without_failing_the_write`
- `plugin_channel_shared_state_is_owned_and_send`
- `plugin_install_rejects_a_manifest_that_describes_another_plugin`
- `plugin_install_rejects_a_manifest_the_supervisor_would_silently_skip`
- `plugin_install_rejects_a_manifest_without_a_switch_key`
- `plugin_install_rejects_non_json_and_requires_a_project`
- `plugin_install_reports_a_missing_plugin_without_writing_anything`
- `plugin_install_reports_a_plugin_that_fails_to_describe_itself`
- `plugin_install_resolves_the_command_and_writes_what_the_plugin_describes`
- `plugin_install_rolls_the_manifest_back_when_the_switch_cannot_be_written`
- `plugin_install_succeeds_but_reports_not_queryable_when_the_socket_never_appears`
- `plugin_uninstall_is_safe_to_repeat_and_on_a_never_installed_plugin`
- `plugin_uninstall_rejects_an_unusable_plugin_name`
- `plugin_uninstall_removes_the_manifest_the_switch_and_the_state`
- `writing_the_watchman_setting_installs_or_uninstalls_it`


#### `tests/title.rs`（10 个）

- `a_manual_rename_during_generation_wins_over_the_generated_title`
- `a_thread_that_already_has_a_title_never_calls_the_title_model`
- `failed_title_generation_broadcasts_no_title_change`
- `first_turn_broadcasts_llm_title_change`
- `first_turn_title_comes_from_llm`
- `first_turn_title_falls_back_when_generation_fails`
- `second_turn_does_not_regenerate_the_title`
- `thread_rename_rejects_empty_title`
- `thread_rename_updates_title`
- `title_material_uses_only_the_user_input`
