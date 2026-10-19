# 项目组长会话命名 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让每个项目组长会话稳定显示为 `<项目目录 basename> 组长`，且自动标题与通用重命名无法覆盖该标题。

**Architecture:** 在 app-server 的组长模块定义唯一规范标题函数，以 cwd basename 生成标题并处理回退。所有持久化标题写入口通过 thread role 做保护；组长 ensure 路径同步修复存量标题。当前 turn 完成路径先经 `ThreadStore::touch`：新建/规范化组长已有非空标题，因此不会产生首轮 fallback 标题，也不会启动 LLM 标题生成；store 层 CAS role guard 作为纵深保护，防止将来调用路径变化造成覆盖。rename RPC 与驱动会话标题写入保留现有兼容语义但不可改写组长规范标题。普通 thread 的标题流程不变。

**Tech Stack:** Rust、Tokio、Serde、现有 `yi-agent-app-server` 单元及 RPC 测试。

## Global Constraints

- 组长标准标题格式为 `<项目目录 basename> 组长`。
- basename 无法取得或为空时，标题回退为 `组长`。
- 组长标题由 app-server 决定，不依赖模型生成。
- 组长标题不得被 LLM 自动标题或通用重命名改写。
- 普通会话的标题行为保持不变。
- 所有 meta 修改沿用 `ThreadStore` 的读-改-写锁与原子写路径。
- 改动完成后更新 `docs/project-management/yi-agent-app-server.md` 与 `docs/project-management/README.md` 计数。
- 测试后执行 `cargo fmt --all`，并按项目要求在提交前运行 `cargo fmt --all --check`。

---

## File Structure

- Modify `yi-agent-rs/crates/yi-agent-app-server/src/lead.rs`：提供基于 cwd 的规范组长标题函数；`ensure_lead_meta` 在创建及复用时确保标题一致；添加纯逻辑与存储集成测试。
- Modify `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs`：在 `touch`、`set_title_if_unchanged`、`rename` 等通用标题写入口保护 `role == LEAD_ROLE`；增加 store 测试。
- Modify `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`：确认完成路径通过 `ThreadStore::touch` 和 LLM CAS 写标题；新建组长标题非空，因此不触发 fallback/LLM 标题路径，无需改动 server。
- Inspect `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs`：确认 rename 写入经 ThreadStore；现有成功响应兼容语义保持不变。
- Modify `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs`：覆盖组长与普通会话的 rename；既有普通会话首轮 LLM 标题测试作为回归。
- Modify `docs/project-management/yi-agent-app-server.md`：新增已完成 feature 条目，包含代码位置与可执行测试命令。
- Modify `docs/project-management/README.md`：同步更新 app-server 模块完成数与总数。

## Task 1: 规范组长标题并迁移存量 meta

**Files:**
- Modify/Test: `yi-agent-rs/crates/yi-agent-app-server/src/lead.rs`
- Modify/Test: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs`（只在缺少原子 title-only 更新 API 时）

**Interfaces:**
- Produces: `pub(crate) fn lead_title_for_cwd(cwd: &Path) -> String`，返回 `<basename> 组长` 或 basename 不可用时的 `组长`。
在 `thread_store.rs` 新增 `pub(crate) fn set_lead_title(&self, id: &str, title: &str) -> io::Result<bool>`，通过私有 `update_meta` 只更新合法 lead meta 的 title；非 lead 不变更，未知 id 按该 crate 现有 setter 约定返回 `false`。`ensure_lead_meta` 用此 API 做存量规范化。

- [x] **Step 1: 写失败测试，固定标题格式与回退**

在 `lead.rs` 测试中添加：

```rust
#[test]
fn lead_title_uses_the_project_directory_basename() {
    assert_eq!(lead_title_for_cwd(Path::new("/work/my-app")), "my-app 组长");
}

#[test]
fn lead_title_falls_back_when_the_path_has_no_basename() {
    assert_eq!(lead_title_for_cwd(Path::new("/")), "组长");
}
```

在 `ensure_lead_meta` 新建组长测试中断言 meta 标题等于 `lead_title_for_cwd(&project)`。

- [x] **Step 2: 运行目标测试确认失败**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead_title_`
Expected: FAIL，因为规范标题函数尚未定义。

- [x] **Step 3: 实现规范标题函数并用于新组长**

在 `lead.rs` 中添加：

```rust
pub(crate) fn lead_title_for_cwd(cwd: &Path) -> String {
    cwd.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map(|name| format!("{name} 组长"))
        .unwrap_or_else(|| "组长".to_string())
}
```

在新建 `ThreadMeta` 时设 `title: Some(lead_title_for_cwd(cwd))`。

- [x] **Step 4: 写存量组长标题收敛测试**

在 tempdir 子目录 `my-app` 下创建 `role=lead`、`title=Some("组长")` 的 meta，调用 `ensure_lead_meta`，断言：返回的 thread id 与既有 id 一致；返回值和重新 load 的 meta 标题均为 `my-app 组长`；其他 meta 字段（例如 `pin_seq`）保留。

- [x] **Step 5: 实现存量标题原子收敛**

在 `ensure_lead_meta` 命中 `find_lead()` 后，若 title 与 `lead_title_for_cwd(cwd)` 不同，调用 Task 1 新增的 `ThreadStore::set_lead_title(id, title)` 专用方法，由 `thread_store.rs` 内部使用私有 `update_meta` 原子读-改-写，只修改 title。重新 load 已落盘 meta 后再返回。不得丢失 role、pin、tag、model 或内容游标等字段。

- [x] **Step 6: 运行 Task 1 测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead_title_ && cargo test -p yi-agent-app-server --lib ensure_lead_meta`
Expected: 标题格式、回退、存量收敛与既有 worktree 拒绝测试全部 PASS。

## Task 2: ThreadStore 通用标题写入保护

**Files:**
- Modify/Test: `yi-agent-rs/crates/yi-agent-app-server/src/thread_store.rs`

**Interfaces:**
- Consumes: Task 1 的 `lead_title_for_cwd` 和 thread meta 的 `role/cwd`。
- Produces: `role == LEAD_ROLE` 的 `touch`、LLM CAS 与通用 `rename` 不得持久化非规范标题；普通 thread 保持现有语义。

- [x] **Step 1: 写 store 层测试**

构造 cwd basename 为 `my-app`、title 为 `my-app 组长`、role 为 `LEAD_ROLE` 的 meta，然后断言：`touch(id, Some("prompt"))` 返回未写标题；`set_title_if_unchanged(id, "my-app 组长", "LLM title")` 返回 `false`；`rename(id, "manual title")` 对已存在 thread 返回成功，但 reload 后标题仍为 `my-app 组长`。同一测试还把普通会话误设为 lead 标题写入的拒绝行为单独断言。

- [x] **Step 2: 实现基于 role 的标题保护**

在 `touch` 中仅对非 lead meta 执行 title 缺失填充；仍按原规则更新时间戳。在 `set_title_if_unchanged` 中，对 lead 不执行标题替换并返回 `false`。在 `rename` 的 `update_meta` 闭包中，对 lead 保持 title 不变但仍视为 thread 存在并返回成功；普通 thread 继续写入 title 与 `updated_at`。所有逻辑必须在同一 `update_meta` 锁内，不得先锁外 load 再写。

- [x] **Step 3: 运行标题存储回归测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead_title_write && cargo test -p yi-agent-app-server --lib thread_store::tests::rename && cargo test -p yi-agent-app-server --lib thread_store::tests::touch && cargo test -p yi-agent-app-server --lib set_title_if_unchanged`
Expected: lead 保护与普通 thread 已有 rename/touch/CAS 测试全部 PASS。

## Task 3: 组长首轮不生成 LLM 标题

**Files:**
- Modify: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`
- Test: `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs`

**Interfaces:**
- Consumes: Task 2 的 role-aware `touch` 与 CAS。
- Produces: 新建组长已有非空规范标题，`touch` 不会生成 fallback，故不触发首轮自动标题；CAS role guard 提供纵深保护。普通 thread 首轮流程不变。

- [x] **Step 1: 检查完成路径与标题写入行为**

检查 `server.rs` 的 Finished 路径：它调用 `touch(thread_id, Some(prompt))`，并仅在 `touch` 返回首次写标题时请求 LLM provider。`ensure_lead_meta` 创建组长时写入非空 `<basename> 组长`，已有组长在 RPC 返回前被规范化；因此 lead 的 `touch` 返回 false，既不写 fallback，也不会进入 `generate_title`。ThreadStore CAS 仍有基于 role 的保护，以覆盖其他调用者或将来路径调整。

- [x] **Step 2: 验证存储写入入口的 role guard**

`lead_title_write_entry_points_preserve_the_project_title` 构造标题等于 `expected` 的 lead meta，再调用 `set_title_if_unchanged`；这可区分 role guard 与普通 CAS 字符串不匹配。该测试同时覆盖 `touch` 与通用 `rename`。普通 thread 的 `first_turn_title_comes_from_llm` 是既有回归覆盖。

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib lead_title_write && cargo test -p yi-agent-app-server --lib first_turn_title_comes_from_llm`
Expected: lead 标题写入入口均保持规范标题，普通 thread 仍走 LLM 标题生成。

## Task 4: RPC 与其他标题写入入口兼容性

**Files:**
- Test/Modify: `yi-agent-rs/crates/yi-agent-app-server/src/rpc_tests.rs`
- Inspect/Modify if required: `yi-agent-rs/crates/yi-agent-app-server/src/rpc/thread.rs`
- Inspect/Modify if required: `yi-agent-rs/crates/yi-agent-app-server/src/server.rs`

**Interfaces:**
- Consumes: Task 1 规范标题、Task 2 store 层保护。
- Produces: 普通 thread rename 仍有效；组长 rename 返回现有成功响应但磁盘标题保持规范值；驱动 launch 标题写入也受相同存储保护。

- [x] **Step 1: 写组长 RPC rename 测试**

在 `rpc_tests.rs` 新增测试：在 tempdir 下建 `my-app` 子目录并以其作为项目 cwd；调用 `thread/lead` 取得组长 id；调用 `thread/rename`，title 使用 `manual override`；断言响应无 error，读取 store 并确认标题为 `my-app 组长`。保留现有 `thread_rename_updates_title`，验证普通 thread 仍能改名。

- [x] **Step 2: 验证 RPC 行为**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_rename_does_not_change_lead_title && cargo test -p yi-agent-app-server --lib thread_rename_updates_title`
Expected: lead rename 保持规范标题，普通会话 rename 正常改名。

- [x] **Step 3: 检查 RPC active/cold 两条路径**

确认 `rpc/thread.rs` 的 rename handler 和活跃 thread driver rename 命令最终都调用 `ThreadStore::rename`，保持空标题校验、未知 thread 错误与成功响应 JSON 不变。实现位于统一的 store guard，无绕过写入。

- [x] **Step 4: 检查 driven thread 命名路径**

检查 `server.rs` 的 driven launch 在建 thread 后调用的 `session.store.rename(&thread_id, title)`。保持该调用和普通 driven thread 名称行为；同一 ThreadStore role guard 保护组长标题不被驱动标题覆盖。

- [x] **Step 5: 运行 RPC 与驱动会话回归测试**

Run: `cd yi-agent-rs && cargo test -p yi-agent-app-server --lib thread_rename_ && cargo test -p yi-agent-app-server --lib a_launch_action_records_the_driver_origin_and_still_starts_a_turn`
Expected: 普通 rename、lead rename 与 driven launch 现有行为均 PASS。

## Task 5: 进度文档和最终验证

**Files:**
- Modify: `docs/project-management/yi-agent-app-server.md`
- Modify: `docs/project-management/README.md`

- [x] **Step 1: 更新 app-server feature 清单**

新增 `[x]` 条目，记录 `<basename> 组长` 命名、存量标题收敛、LLM/通用 rename 保护，并引用实际源码路径及下面运行的测试命令作为完成证据。不得使用 `[~]`。

- [x] **Step 2: 更新 README 模块计数**

将 `docs/project-management/README.md` 中 `yi-agent-app-server` 的完成数和总数都加 1，其他模块计数保持不变。

- [x] **Step 3: 确认没有其他 cargo 测试进程并运行 crate 测试**

Run: `ps aux | grep -v grep | grep cargo`，确认无正在执行的 cargo test 后再运行：

```bash
cd yi-agent-rs && cargo test -p yi-agent-app-server
```

Expected: app-server crate 全部测试通过（847 passed, 0 failed, 4 ignored）。

- [x] **Step 4: 格式与差异检查**

```bash
cd yi-agent-rs && cargo fmt --all && cargo fmt --all --check
cd .. && git diff --check
```

Expected: fmt 与 whitespace 检查通过，且无无关文件更改。

- [x] **Step 5: 对照验收清单复核**

确认 basename 标题、路径 fallback、存量标题迁移、组长首轮不触发 LLM（非空规范标题使 `touch` 返回 false）、组长 rename 保持规范标题、普通 thread rename/LLM 标题无回归、project-management 条目与 README 计数已同步。存储 CAS role guard 的测试用匹配的 expected 标题验证，不依赖 CAS mismatch 偶然通过。

## Plan Self-Review

- Spec coverage：标题格式与 fallback 在 Task 1；存量标题收敛在 Task 1；持久化写入口保护在 Task 2；组长首轮 LLM 排除和通知在 Task 3；RPC 与驱动命名路径在 Task 4；普通会话兼容、项目进度文件和全量验证在 Task 2–5。
- Placeholder scan：无 TBD/TODO/待定标记；所有测试步骤给出具体测试名、输入、断言、命令与预期结果。Task 4 的 RPC 测试明确完整构造步骤。
- Type consistency：标题函数签名统一为 `pub(crate) fn lead_title_for_cwd(cwd: &Path) -> String`；store role 保护以已有 `ThreadMeta.role` 和 `LEAD_ROLE` 为准；没有引入新协议字段。
