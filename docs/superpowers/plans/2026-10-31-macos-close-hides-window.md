# macOS 关闭窗口改为隐藏到后台 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 macOS 上点窗口红叉只隐藏窗口、应用继续驻留 Dock，点 Dock 图标窗口恢复。

**Architecture:** 在 `App::run` 的 `RunEvent` 回调里拦截主窗口的 `CloseRequested`——先 `prevent_close()` 再 `hide()`，窗口对象因此不销毁，前端状态全部保留。点 Dock 图标时系统发 `RunEvent::Reopen { has_visible_windows: false }`，据此 `show()` + `set_focus()`。全部逻辑集中到 `window.rs` 的一个具名函数 `handle_run_event`，`lib.rs` 只做一行转发，测试可以直接驱动生产函数。

**Tech Stack:** Rust 1.98 / Tauri 2.11.6 / `tauri::test::MockRuntime`

## Global Constraints

- 只在 macOS 生效：函数与调用点一律 `#[cfg(target_os = "macos")]`。Windows/Linux 行为必须与改动前逐字节一致。
- 不改前端任何文件（`desktop/src/**` 零改动）。窗口只是隐藏，webview 不销毁。
- 不改 `bridge.rs` 的 sidecar 生命周期逻辑。
- 不引入托盘图标，不新增插件、不新增运行时依赖。唯一新增依赖是已有的 dev-dependency `tauri = { version = "2", features = ["test"] }`（本分支已加）。
- `cargo fmt` 必须通过；提交前在 `desktop/src-tauri` 下跑 `cargo fmt --all`。
- 提交信息用 conventional commits，首行 ≤72 字符，**不写** `Co-Authored-By`。

## 已实测确认的框架行为（不要重新论证，直接用）

这些结论来自本分支上跑过的临时探针（探针已删除），实现时直接依赖：

| `prevent_close()` | `window.close()` 后窗口仍在？ | mock 事件循环 |
|---|---|---|
| 调用 | 是 | 继续跑，需再 `destroy()` 才退出 |
| 不调用 | 否 | 立刻 break |

- 结束 MockRuntime 的无限 loop **只能**用 `window.destroy()`，**不能**用 `app.exit()`——`request_exit` 在 MockRuntime 里是 `unimplemented!()`（`tauri-2.11.6/src/test/mock_runtime.rs:143-145`）。
- MockRuntime 的 `is_visible()` 恒返回 `true`（同文件 `:777-779`），所以「显示/隐藏」这个动作无法用单测区分，只能靠真机验收。
- 测试里记录观测值必须用 `Arc<Mutex<_>>`。用普通 `let mut` 会被 `move` 闭包拿走所有权，外层读到的永远是初始值。
- 取窗口句柄统一用 `Manager::get_webview_window("main")`，与既有 `fit_main_window` 一致。
- **三个函数必须对 `R: tauri::Runtime` 泛型化。** `tauri::AppHandle` 的默认类型参数是
  `Wry` 运行时，写成 `&tauri::AppHandle` 会让 `MockRuntime` 的集成测试无法调用
  （`error[E0308]: expected &AppHandle, found &AppHandle<MockRuntime>`）。这条是照本计划
  实跑时踩到的，不是理论风险。

## File Structure

| 文件 | 职责 | 动作 |
|---|---|---|
| `desktop/src-tauri/src/window.rs` | 主窗口杂务：既有 sizing + 新的关闭策略 | 修改：加 3 个函数与 2 个单测 |
| `desktop/src-tauri/src/lib.rs` | 组装与启动 | 修改：`mod window` 改 `pub mod window`；`builder.run(...)` 拆成 `build` + `run` |
| `desktop/src-tauri/tests/close_hides_window.rs` | 集成测试：关闭后窗口仍在 | 新建 |
| `desktop/src-tauri/Cargo.toml` | dev-dependency | 已改好，本计划不动 |
| `docs/project-management/desktop.md` | 进度条目 | 修改：加一条 `[x]` |
| `docs/project-management/README.md` | 模块索引计数 | 修改：`desktop 103/114` → `104/115` |

---

### Task 1: 让 `window` 模块对外可见

集成测试（`tests/` 下的独立 crate）只能看到 `pub` 的东西。`window.rs` 现在是私有模块（`desktop/src-tauri/src/lib.rs:4`）。

**Files:**
- Modify: `desktop/src-tauri/src/lib.rs:4`
- Test: `desktop/src-tauri/tests/close_hides_window.rs`（本任务只建一个最小用例证明可见性）

**Interfaces:**
- Consumes: 无
- Produces: `desktop_lib::window::fit_main_window` 等既有函数对集成测试可见

- [ ] **Step 1: 写一个因模块不可见而编译失败的测试**

新建 `desktop/src-tauri/tests/close_hides_window.rs`：

```rust
//! Integration tests for the main-window close policy.
//!
//! These live outside the crate so they exercise the same `RunEvent` path the
//! real app uses, rather than a re-implementation of it.

#[test]
fn the_window_module_is_reachable_from_an_integration_test() {
    // Any public item proves the module is reachable. `fit_main_window` is
    // today's entry point for main-window chores; it is generic-free, so a
    // plain function pointer works (verified by compiling this step).
    let _f: fn(&tauri::AppHandle) -> tauri::Result<()> = desktop_lib::window::fit_main_window;
}
```

- [ ] **Step 2: 跑测试确认它编译失败**

```bash
cd desktop/src-tauri && cargo test --test close_hides_window
```

Expected: 编译错误 `error[E0603]: module 'window' is private`。

- [ ] **Step 3: 把模块改成 pub**

`desktop/src-tauri/src/lib.rs` 第 4 行：

```rust
#[cfg(desktop)]
mod window;
```

改为：

```rust
#[cfg(desktop)]
pub mod window;
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd desktop/src-tauri && cargo test --test close_hides_window
```

Expected: `test the_window_module_is_reachable_from_an_integration_test ... ok`

- [ ] **Step 5: 提交**

```bash
cd desktop/src-tauri && cargo fmt --all
cd .. && git add src-tauri/src/lib.rs src-tauri/tests/close_hides_window.rs && git commit -m "test: expose the window module to integration tests"
```

---

### Task 2: `hide_on_close`

**Files:**
- Modify: `desktop/src-tauri/src/window.rs`（文件末尾，`fit_main_window` 之后）
- Test: `desktop/src-tauri/src/window.rs`（同文件 `#[cfg(test)] mod tests` 里加）

**Interfaces:**
- Consumes: Task 1 的 `pub mod window`
- Produces:
  - `pub fn hide_on_close(app: &tauri::AppHandle, label: &str, api: &tauri::CloseRequestApi)`
  - Task 4 的 `handle_run_event` 会调用它

- [ ] **Step 1: 写失败的单元测试**

在 `desktop/src-tauri/src/window.rs` 的 `mod tests` 里追加。该测试验证「非 main 窗口不受影响」这条可独立判断的规则——main 窗口的真实隐藏行为在 MockRuntime 里无法观测，留给 Task 4 的集成测试。

```rust
    /// Only the main window hides on close. A second window keeps the
    /// framework default (close really closes), so adding one later cannot
    /// silently produce an un-closable window.
    #[test]
    fn hide_on_close_only_applies_to_the_main_window() {
        let app = tauri::test::mock_app();
        let main = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .unwrap();
        let other = tauri::WebviewWindowBuilder::new(&app, "other", Default::default())
            .build()
            .unwrap();

        assert!(hide_on_close_is_our_window("main"));
        assert!(!hide_on_close_is_our_window("other"));
        // Keep the windows alive for the duration of the test.
        let _ = (&main, &other);
    }
```

同一个 `mod tests` 里，同时在文件顶部（`use` 区）确认已有 `use super::*;`——原有 6 个 `fit_to_work_area` 测试已依赖它。

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop/src-tauri && cargo test --lib window::tests::hide_on_close_only_applies_to_the_main_window
```

Expected: 编译错误 `cannot find function 'hide_on_close_is_our_window'`。

- [ ] **Step 3: 实现**

在 `window.rs` 里 `fit_main_window` 之后追加：

```rust
/// The label of the one window that hides instead of closing.
pub const MAIN_WINDOW: &str = "main";

/// Whether a `CloseRequested` on `label` should hide the window.
///
/// Split out from [`hide_on_close`] so the rule is testable without a runtime:
/// `CloseRequestApi` cannot be constructed outside the framework.
pub fn hide_on_close_is_our_window(label: &str) -> bool {
    label == MAIN_WINDOW
}

/// Hide the window instead of closing it, so the app keeps running in the Dock.
///
/// The window object survives — nothing is destroyed — which is why the
/// webview keeps its React state (scroll position, streamed transcript, the
/// draft in the composer) across the hide/show cycle.
///
/// Only `main` is handled; any other window keeps the framework default. A
/// failed `hide` is logged rather than propagated: the close was already
/// prevented, and a window that stays visible is better than a crash.
#[cfg(target_os = "macos")]
pub fn hide_on_close<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    label: &str,
    api: &tauri::CloseRequestApi,
) {
    use tauri::Manager;

    if !hide_on_close_is_our_window(label) {
        return;
    }
    api.prevent_close();
    match app.get_webview_window(label) {
        Some(window) => {
            if let Err(err) = window.hide() {
                eprintln!("[window] hide failed: {err}");
            }
        }
        None => eprintln!("[window] close requested for a missing window: {label}"),
    }
}
```

`get_webview_window` 来自 `Manager` trait，而现有 `fit_main_window` 用的是函数内局部
`use tauri::{LogicalSize, Manager};`。新函数沿用同一风格，在函数体第一行写
`use tauri::Manager;`（上面的代码块里已包含）。

- [ ] **Step 4: 跑测试确认通过**

```bash
cd desktop/src-tauri && cargo test --lib window::tests
```

Expected: 7 passed（原 6 个 + 新增 1 个）。

- [ ] **Step 5: 提交**

```bash
cd desktop/src-tauri && cargo fmt --all
cd .. && git add src-tauri/src/window.rs && git commit -m "feat: hide the main window instead of closing it"
```

---

### Task 3: `reveal_main_window`

**Files:**
- Modify: `desktop/src-tauri/src/window.rs`
- Test: 无新增单测（理由见下）

**Interfaces:**
- Consumes: `MAIN_WINDOW`（Task 2）
- Produces: `pub fn reveal_main_window(app: &tauri::AppHandle)`，Task 4 调用

**为什么没有单测：** MockRuntime 的 `is_visible()` 恒返回 `true`，`show()` 是空实现（`tauri-2.11.6/src/test/mock_runtime.rs:693-695`、`:777-779`）。写出来的断言会恒真，是假测试。这条路径由 Task 5 的真机验收覆盖。本任务只要求编译通过。

- [ ] **Step 1: 实现**

在 `window.rs` 里 `hide_on_close` 之后追加：

```rust
/// Bring the main window back after the user clicks the Dock icon.
///
/// Called only when macOS reports no visible windows, so this never fights a
/// window the user already has in front. `show` and `set_focus` failures are
/// logged, not propagated: `App::run`'s callback has no way to surface them and
/// a window that is visible but unfocused is still usable.
#[cfg(target_os = "macos")]
pub fn reveal_main_window<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    use tauri::Manager;

    let Some(window) = app.get_webview_window(MAIN_WINDOW) else {
        eprintln!("[window] reopen: no {MAIN_WINDOW} window to show");
        return;
    };
    if let Err(err) = window.show() {
        eprintln!("[window] show failed: {err}");
    }
    if let Err(err) = window.set_focus() {
        eprintln!("[window] focus failed: {err}");
    }
}
```

- [ ] **Step 2: 确认编译通过且既有测试仍绿**

```bash
cd desktop/src-tauri && cargo test --lib window::tests
```

Expected: 7 passed。

- [ ] **Step 3: 提交**

```bash
cd desktop/src-tauri && cargo fmt --all
cd .. && git add src-tauri/src/window.rs && git commit -m "feat: reveal the main window from the dock icon"
```

---

### Task 4: 把两个函数接到 `RunEvent`，并写集成测试

**Files:**
- Modify: `desktop/src-tauri/src/window.rs`（加 `handle_run_event` + 单测）
- Modify: `desktop/src-tauri/src/lib.rs`（`build` + `run`）
- Modify: `desktop/src-tauri/tests/close_hides_window.rs`（替换 Task 1 的最小用例）

**Interfaces:**
- Consumes: `hide_on_close`（Task 2）、`reveal_main_window`（Task 3）
- Produces: `pub fn handle_run_event(app: &tauri::AppHandle, event: &tauri::RunEvent)`

- [ ] **Step 1: 写失败的集成测试**

用下面内容整体替换 `desktop/src-tauri/tests/close_hides_window.rs`：

```rust
//! Integration tests for the main-window close policy.
//!
//! These live outside the crate so they exercise the same `RunEvent` path the
//! real app uses, rather than a re-implementation of it.

use std::sync::{Arc, Mutex};

use tauri::{Manager, RunEvent};

/// What the event loop observed while the test ran.
#[derive(Debug, Default)]
struct Seen {
    close_requests: u32,
    exit_requests: u32,
    /// `Some` once we looked: was the main window still registered?
    alive_after_close: Option<bool>,
}

fn run_close_cycle() -> Seen {
    let app = tauri::test::mock_app();
    let window = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
        .build()
        .unwrap();

    // Shared, not captured by value: a plain `let mut` would be moved into the
    // closure and the outer read would see the initial value forever.
    let seen = Arc::new(Mutex::new(Seen::default()));
    let observer = Arc::clone(&seen);
    let mut close_sent = false;

    app.run(move |app, event| {
        // The production policy under test.
        desktop_lib::window::handle_run_event(app, &event);

        match event {
            RunEvent::WindowEvent {
                event: tauri::WindowEvent::CloseRequested { .. },
                ..
            } => observer.lock().unwrap().close_requests += 1,

            // First pass: ask the window to close, as the red button does.
            RunEvent::MainEventsCleared if !close_sent => {
                close_sent = true;
                let _ = window.close();
            }

            // Later passes: the close has been handled. Look, then stop the
            // mock loop by destroying the window (its only exit path).
            RunEvent::MainEventsCleared => {
                let seen = observer.lock().unwrap();
                if seen.alive_after_close.is_none() {
                    drop(seen);
                    let alive = app.get_webview_window("main").is_some();
                    observer.lock().unwrap().alive_after_close = Some(alive);
                    let _ = window.destroy();
                }
            }

            RunEvent::ExitRequested { .. } => observer.lock().unwrap().exit_requests += 1,
            _ => {}
        }
    });

    let seen = seen.lock().unwrap();
    Seen {
        close_requests: seen.close_requests,
        exit_requests: seen.exit_requests,
        alive_after_close: seen.alive_after_close,
    }
}

#[test]
fn closing_the_window_keeps_it_registered_instead_of_quitting() {
    let seen = run_close_cycle();

    assert_eq!(seen.close_requests, 1, "the red button must arrive as one CloseRequested");
    assert_eq!(
        seen.alive_after_close,
        Some(true),
        "prevent_close must leave the window registered; Some(false) means it was destroyed"
    );
}
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop/src-tauri && cargo test --test close_hides_window
```

Expected: 编译错误 `cannot find function 'handle_run_event' in module 'desktop_lib::window'`。

- [ ] **Step 3: 实现 `handle_run_event`**

在 `window.rs` 里 `reveal_main_window` 之后追加：

```rust
/// The window-visibility policy, wired to the app's event loop.
///
/// Kept as a named function so a test can drive the real policy instead of
/// re-writing the match. `lib.rs` does nothing but forward to it.
#[cfg(target_os = "macos")]
pub fn handle_run_event<R: tauri::Runtime>(app: &tauri::AppHandle<R>, event: &tauri::RunEvent) {
    match event {
        tauri::RunEvent::WindowEvent {
            label,
            event: tauri::WindowEvent::CloseRequested { api, .. },
            ..
        } => hide_on_close(app, label, api),
        // `has_visible_windows == true` means the app is merely behind
        // another one: macOS already brings it forward, and calling `show()`
        // there would steal focus the user is in the middle of moving.
        tauri::RunEvent::Reopen {
            has_visible_windows: false,
            ..
        } => reveal_main_window(app),
        _ => {}
    }
}
```

- [ ] **Step 4: 跑集成测试确认通过**

```bash
cd desktop/src-tauri && cargo test --test close_hides_window
```

Expected: `test closing_the_window_keeps_it_registered_instead_of_quitting ... ok`

Expected 观测值（本计划编写时实跑得到）：

```
OBSERVED: Seen { close_requests: 1, exit_requests: 1, alive_after_close: Some(true) }
```

`exit_requests: 1` 是 `destroy()` 之后的正常退出（mock 只在窗口表清空时发它），
不是 bug。若 `alive_after_close` 得到 `Some(false)`，说明 `handle_run_event` 没有真的
拦到 `CloseRequested`——先确认 match 的 `label` 是 `"main"`，再看
`hide_on_close_is_our_window` 的比较。

**变异验证（写 plan 时已做）**：把 `hide_on_close` 里的 `api.prevent_close()` 删掉后，
该测试立即失败（`left: None, right: Some(true)`）。它测的是真实行为，不是恒真断言。

- [ ] **Step 5: 改 `lib.rs` 接线**

`desktop/src-tauri/src/lib.rs` 现在结尾是：

```rust
    builder
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
```

替换为：

```rust
    let app = builder
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    // Closing the main window hides it instead of quitting, so the app keeps
    // running in the Dock like every other Mac app. `Reopen` (the Dock click)
    // only exists on macOS; elsewhere the framework default is correct.
    #[cfg(target_os = "macos")]
    app.run(|app, event| window::handle_run_event(app, &event));
    #[cfg(not(target_os = "macos"))]
    app.run(|_app, _event| {});
}
```

- [ ] **Step 6: 跑全部 desktop Rust 测试**

```bash
cd desktop/src-tauri && cargo test
```

Expected: 所有测试通过（既有 bridge/window 单测 + 新集成测试）。

- [ ] **Step 7: 提交**

```bash
cd desktop/src-tauri && cargo fmt --all
cd .. && git add src-tauri/src/lib.rs src-tauri/src/window.rs src-tauri/tests/close_hides_window.rs && git commit -m "feat: keep the app running when the window is closed"
```

---

### Task 5: 真机验收

MockRuntime 观测不到 `show()`/`hide()` 的效果，这一步是唯一能证明用户所要求行为的地方。**必须做完并把结果写进 commit message 或 PR 描述。**

**Files:** 无代码改动

**Interfaces:**
- Consumes: Task 4 的完整实现
- Produces: 验收结论

- [ ] **Step 1: 启动 dev 版**

```bash
cd desktop && npm run build && npx tauri dev
```

（`dist/` 必须先生成，否则 dev 版前端连不上；`binaries/yi-agent-aarch64-apple-darwin` 若缺失按 CLAUDE.md 从主仓库软链。）

- [ ] **Step 2: 验证「红叉 → 后台」**

点窗口左上角红叉。预期：窗口消失，Dock 图标仍在，App 未退出。

```bash
ps axo command | grep "app-server --listen" | grep -v grep
```

预期：sidecar 仍在跑（输出不为空）。

- [ ] **Step 3: 验证「点 Dock 图标 → 回到前台」**

点 Dock 上的 yi-agent 图标。预期：窗口以隐藏前的样子回来（滚动位置、输入框草稿都在），并聚焦。

- [ ] **Step 4: 验证隐藏不打断进行中的任务**

在会话里发一条会跑一阵的消息（例如「跑一个 30 秒的 sleep 命令」），运行中点红叉，等 10 秒，再点 Dock 图标。预期：输出在隐藏期间继续推进，回来后能看到新内容。

- [ ] **Step 5: 验证「Cmd+Q → 真正退出」**

按 Cmd+Q。预期：App 真正退出，Dock 图标消失。

```bash
ps axo command | grep "app-server --listen" | grep -v grep
```

预期：输出为空（sidecar 已随之退出）。

- [ ] **Step 6: 若任一步不符预期**

停止并在会话里报告哪一步、实际看到什么，**不要**继续后续任务。常见原因：`lib.rs` 的 `cfg` 分支没生效（`cargo test` 通过但 `tauri dev` 用的是旧二进制时先 `cargo clean -p desktop`）。

---

### Task 6: 进度文档

**Files:**
- Modify: `docs/project-management/desktop.md`
- Modify: `docs/project-management/README.md:31`

**Interfaces:**
- Consumes: Task 1-5 的产出（代码位置与判据）
- Produces: 无

- [ ] **Step 1: 在 `desktop.md` 的 Features 列表末尾追加一条**

```markdown
- [x] 关闭主窗口 = 隐藏到后台（macOS），点 Dock 图标恢复 — 点红叉原先会直接退出应用（Tauri 在最后一个窗口销毁时发 `ExitRequested` 并退出），正在跑的会话与 sidecar 一并被终止。现在 `RunEvent::WindowEvent { CloseRequested }` 被拦下：`api.prevent_close()` + `window.hide()`，窗口对象不销毁，前端状态（滚动位置、流式输出、草稿）完整保留；`RunEvent::Reopen { has_visible_windows: false }`（Dock 图标点击）时 `show()` + `set_focus()`。Cmd+Q 走 macOS 原生 `terminate:`，不经过 `CloseRequested`，仍能真正退出。代码：`desktop/src-tauri/src/window.rs` 的 `hide_on_close` / `reveal_main_window` / `handle_run_event`，接线在 `desktop/src-tauri/src/lib.rs`（`build` + `run`，仅 macOS 分支）。判据：`cd desktop/src-tauri && cargo test --test close_hides_window && cargo test --lib window::tests`（关闭后窗口仍注册）；真机 `cd desktop && npx tauri dev` 四步验收（红叉后 sidecar 仍在跑 / 点 Dock 恢复原样 / 隐藏期间输出继续推进 / Cmd+Q 真正退出）— [设计](../superpowers/specs/2026-10-31-macos-close-hides-window-design.md)、[计划](../superpowers/plans/2026-10-31-macos-close-hides-window.md)
```

- [ ] **Step 2: 更新索引计数**

`docs/project-management/README.md:31`：

```markdown
| desktop | 103 / 114 | [详情](./desktop.md) |
```

改为：

```markdown
| desktop | 104 / 115 | [详情](./desktop.md) |
```

- [ ] **Step 3: 提交**

```bash
git add docs/project-management/desktop.md docs/project-management/README.md
git commit -m "docs: record the close-to-background feature"
```

---

## 完成判据

全部任务完成后，这两条必须都成立：

```bash
cd desktop/src-tauri && cargo test
cd desktop && npx tsc --noEmit
```

第二条保证前端零改动、未被意外破坏（本计划不碰前端，跑它只是防回归）。
