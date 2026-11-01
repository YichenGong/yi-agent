# 变异测试基线

每个 `<落点>.txt` 一行一个「已知存活体 / 已知超时」。格式是 `cargo mutants`
输出的 `MISSED` / `TIMEOUT` 行，但**截掉耗时段**（` in 67s build + ...`）：
耗时随机器负载浮动，写进基线等于每次重跑都 diff 失败。`justfile` 里的
`run_one` 用 `sed -E 's/ in [0-9]+s build.*$//'` 做同样的截断。

## 判据

**不是退出码。** cargo-mutants 的退出码 2（有存活体）与 3（有超时）是不同
问题却会互相覆盖：实测 permission 有 12 个 missed + 1 个 timeout 时只报 3。
`just mutants` 因此比对本目录，出现「基线里没有的 MISSED/TIMEOUT」即失败。
基线里的条目**消失**同样算失败——那是好事，但基线该更新了，不能默默放着。

## timeout 必须给 180s，不能给 60s

**这是本轮最重要的实测结论。** 用 `--timeout 60` 跑 path_util 得到
「1 missed + 11 timeout」；把 timeout 提到 180 后变成「5 missed + 0 timeout」。
也就是说 **60s 把 4 个真存活体误报成了 timeout**，其中 2 个补测试后立即被
杀死。用 60s 得到的不是「这些守卫很难测」，而是「测试套件本身要 23s，
`-j 8` 争用下 60s 不够跑完」。

permission 提到 180s 后结论与 60s 完全一致（0 missed / 1 timeout），
说明那条 timeout 是真卡住而不是时间不够，见下。

## 各落点现状（2026-11-02 实测，timeout=180）

| 落点 | 变异体 | caught | missed | unviable | timeout |
|---|---|---|---|---|---|
| `core/src/permission.rs` | 50 | 44 | **0** | 5 | 1 |
| `llm/src/sse.rs` | 11 | 4 | **0** | 7 | 0 |
| `tools/src/fs/path_util.rs` | 27 | 24 | **3**（全为等价变异体） | 0 | 0 |

### permission.rs 的 1 个 timeout：测试卡住，不是时间不够

`permission.rs:133:22` 把 `tool_name == "mcp_reload"` 改成 `!=`。这条变异
**确实被测试覆盖**——单独跑
`cargo test -p yi-agent-core --lib mcp_reload_always_needs_confirmation_even_under_yolo`
在变异后立刻 FAILED（实测）。

但全套跑时它是 timeout：变异后 `mcp_reload` 不再「永远需要确认」，
`agent.rs` 的 `agent_with_permission_allow_executes_tool` 卡在等待确认
（日志最后一行是 `has been running for over 60 seconds`）。180s 也一样，
因为那是逻辑死等，不是慢。留作 TIMEOUT 基线是如实记录：**这条变异的
价值已被证明，只是以「卡住」而非「断言失败」的形式暴露**。

### sse.rs：0 missed，落点价值是防退化

11 变异体 / 4 caught / 7 unviable / 0 missed。这与 Spec 的 D4 补充一致：
该模块现有测试质量已经很好，变异测试在此处的收益是「防止未来退化」，
**没有发现当前缺陷**。不夸大。

### path_util.rs 的 3 个存活体：全部是等价变异体

三条都在 `as_os_str().is_empty()` 守卫上，逐条给出「为什么删除后行为不变」
的可验证理由（都实测过：删除后 21→23 个单测全绿）：

1. **`:47:43`（`lexical_contained_in_any` 内的守卫）**
   循环体内 `cursor` 只在成功 `canonicalize()` 或成功上溯时改变；上溯要求
   `parent` 非空。若把守卫改成恒真，唯一变化是当 `parent` 为空时不再走
   `else` 分支——而 `parent` 为空意味着 `cursor` 是相对路径的根（`""`），
   此时 `cursor.file_name()` 也是 `None`，`match` 会落到 `_` 分支。两个分支
   都执行 `contained_in_any(normalized, roots)`，结果相同。
   **实测**：把该守卫改成 `true`，23 个单测全绿。

2. **`:79:25`（`resolve_and_check_roots` 的 `candidate.parent()` 匹配守卫）**
   `candidate` 要么是 `primary.join(path)`（`primary` 是绝对路径，故必有
   非空父目录），要么是绝对的 `path`。要走到空 `parent` 的情况，`path` 必须
   恰好是文件系统根 `/`；而 `Path::new("/").parent()` 是 `None`（实测
   `rustc` 打印 `parent=None`），根本不进入 `Some(parent) if ...` 这个分支。
   **结论**：`is_empty()` 那半个条件在生产调用路径上不可达。
   根路径 `/` 的安全性由 `_` 分支里的包含检查保证（已有测试
   `resolve_rejects_the_filesystem_root` 钉住）。

3. **`:163:12`（`resolve_for_write_roots` 的守卫）**
   走到这里的前提是 `candidate.canonicalize()` 失败（第 152 行的 `if let Ok`
   没命中）。`candidate` 为 `/` 时 `canonicalize()` 会成功（`/` 一定存在），
   所以第 152 行就 return 了，到不了 163。若守卫被改坏，让空 parent 也进
   `parent.canonicalize()`，`""` 的 canonicalize 返回 cwd，随后
   `contained_in_any(cwd, roots)` 会拒掉（cwd 不是根集成员）——但这条路径
   同样不可达，故行为不变。
   **实测**：把它改成 `if parent.as_os_str().is_empty()`，23 个单测全绿。

**处置**：如实记录在基线里，不补测试、不改生产代码。给不可达分支补测试是
自欺——测试只能证明「这段代码跑了没崩」，证明不了它有存在的必要。

## 本轮补的两个测试（真的杀死了变异体）

`path_util.rs` 的 `mod tests` 里新增：

- `resolve_rejects_the_filesystem_root` — 杀死 `:84:20`（`_` 分支里的
  `!contained_in_any` 取反）。不加这个测试，`/` 会被当成合法路径返回。
- `resolve_propagates_permission_denied_instead_of_not_found` — 杀死
  `:95:19`（`e.kind() == NotFound` 恒真）。不加这个测试，EACCES 会被
  伪装成「文件不存在」，排障方向完全错。

两条都按 TDD 验证过：变异 → 新测试变红 → 还原 → 全绿。

## 怎么加新落点

1. 跑 `cargo mutants --file <path> --timeout 180 -j 6`，输出 tee 到临时文件
2. `grep -E "^MISSED|^TIMEOUT" | sed -E 's/ in [0-9]+s build.*$//'` 存成
   `mutants-baseline/<name>.txt`
3. 在 `justfile` 的 `mutants` recipe 里加一行 `run_one <name> ...`
4. 在本文档的表格里补一行实测数字
