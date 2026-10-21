# Desktop 首次启动窗口尺寸实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让桌面应用首次打开时窗口以更大的默认尺寸居中显示，并在小屏上自动收缩到工作区以内。

**Architecture:** 窗口尺寸分两层。静态层是 `tauri.conf.json` 里的 `width/height/minWidth/minHeight/center`，负责"正常屏幕"的默认值。动态层是新增的 `desktop/src-tauri/src/window.rs`，在 `setup()` 里读主窗口所在显示器的工作区，把尺寸夹到工作区的 90% 以内、再夹到最小尺寸之上，最后 `set_min_size` + `set_size` + `center`。夹取逻辑是纯函数，单元测试直接覆盖数学边界，不需要真实窗口。

**Tech Stack:** Rust、Tauri 2.11.6（`WebviewWindow::current_monitor()` / `Monitor::work_area()` / `set_size` / `set_min_size` / `center`）、serde_json（测试里解析 `tauri.conf.json`）。

## Global Constraints

- **禁止在 `main` 上直接改代码或提交。** 工作目录是 `.worktrees/feat-window-size`，分支 `feat/window-default-size`。
- **不引入 `tauri-plugin-window-state`**，不记忆上次尺寸（用户已明确选择方案 A）。
- **不改** `desktop/tauri.ios.conf.json`、`desktop/src/App.tsx`、`desktop/src-tauri/capabilities/default.json`。
- 尺寸是**逻辑像素**（Tauri 配置与 `LogicalSize` 语义一致），不要写成物理像素。
- 失败必须**退化而非崩溃**：拿不到显示器或 scale 非法时，用配置里的默认值，不 `unwrap`、不 `expect`。
- 本仓库**没有 `rg`**，用 `grep`。
- commit message 用 conventional commits，不写 `Co-Authored-By`。
- `desktop/` 不是 `yi-agent-rs/` 的 cargo workspace 成员。测试命令在 `desktop/src-tauri` 下跑，不要跑 `cargo test --workspace`。

---

## 文件结构

| 文件 | 职责 |
| --- | --- |
| `desktop/src-tauri/src/window.rs`（新建） | 默认尺寸常量、纯函数 `fit_to_work_area`、副作用入口 `fit_main_window`、单元测试 |
| `desktop/src-tauri/src/lib.rs`（修改） | 声明 `mod window`，在桌面 `setup()` 里调用一次并只打日志 |
| `desktop/src-tauri/tauri.conf.json`（修改） | 主窗口 `width/height/minWidth/minHeight/center` |
| `docs/project-management/desktop.md`（修改） | 登记该 feature 与可执行判据 |

---

## Task 1: 纯函数 `fit_to_work_area` + 单元测试

**Files:**
- Create: `desktop/src-tauri/src/window.rs`

**Interfaces:**
- Consumes: 无。
- Produces:
  - `pub struct Fit { pub width: f64, pub height: f64, pub min_width: f64, pub min_height: f64 }`
  - `pub fn fit_to_work_area(desired: (f64, f64), min_size: (f64, f64), work_area: (f64, f64), scale: f64) -> Fit`
  - 常量 `pub const DESIRED: (f64, f64) = (1280.0, 820.0);`、`pub const MIN: (f64, f64) = (900.0, 600.0);`
  - 参数单位约定：`desired` / `min_size` 是**逻辑像素**，`work_area` 是**物理像素**，`scale` 是物理→逻辑的缩放因子（物理 ÷ scale = 逻辑）。

- [ ] **Step 1: 写失败的测试**

创建 `desktop/src-tauri/src/window.rs`，只写测试与空实现（`unimplemented!()`）：

```rust
//! Main-window sizing for the desktop app.
//!
//! `tauri.conf.json` carries the default size, but a default tuned for a laptop
//! is wrong on a 3440x1440 ultrawide (too small) and a default tuned for a
//! desktop is wrong on a 1280x720 panel (does not fit). This module clamps the
//! configured size to the monitor's work area once, at startup, so the window
//! always opens usable and centred.

/// Default opening size in logical pixels. Mirrors `tauri.conf.json`.
pub const DESIRED: (f64, f64) = (1280.0, 820.0);

/// Minimum size in logical pixels. Mirrors `tauri.conf.json`.
pub const MIN: (f64, f64) = (900.0, 600.0);

/// Largest fraction of the monitor work area the window may occupy.
const MAX_FRACTION: f64 = 0.9;

/// Logical-pixel geometry for the main window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fit {
    pub width: f64,
    pub height: f64,
    pub min_width: f64,
    pub min_height: f64,
}

pub fn fit_to_work_area(
    desired: (f64, f64),
    min_size: (f64, f64),
    work_area: (f64, f64),
    scale: f64,
) -> Fit {
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compare geometry with a tolerance: `work * 0.9` is exact for today's
    /// test inputs, but the tests should not depend on that.
    #[track_caller]
    fn assert_fit(actual: Fit, width: f64, height: f64, min_width: f64, min_height: f64) {
        const EPS: f64 = 1e-6;
        let expected = Fit { width, height, min_width, min_height };
        let close = (actual.width - expected.width).abs() < EPS
            && (actual.height - expected.height).abs() < EPS
            && (actual.min_width - expected.min_width).abs() < EPS
            && (actual.min_height - expected.min_height).abs() < EPS;
        assert!(close, "expected {expected:?}, got {actual:?}");
    }

    #[test]
    fn keeps_the_default_size_on_a_large_display() {
        let fit = fit_to_work_area(DESIRED, MIN, (3440.0, 1410.0), 1.0);
        assert_fit(fit, 1280.0, 820.0, 900.0, 600.0);
    }

    #[test]
    fn converts_physical_work_area_with_the_scale_factor() {
        // 2880x1800 physical at scale 2 is 1440x900 logical: the height cap
        // binds (900 * 0.9 = 810) while the width fits (1280 <= 1296).
        let fit = fit_to_work_area(DESIRED, MIN, (2880.0, 1800.0), 2.0);
        assert_fit(fit, 1280.0, 810.0, 900.0, 600.0);
    }

    #[test]
    fn shrinks_to_ninety_percent_of_a_small_work_area() {
        let fit = fit_to_work_area(DESIRED, MIN, (1280.0, 720.0), 1.0);
        assert_fit(fit, 1152.0, 648.0, 900.0, 600.0);
    }

    #[test]
    fn never_opens_below_the_minimum_even_on_a_tiny_work_area() {
        // A work area smaller than the minimum relaxes the minimum to the work
        // area itself, so the window is never larger than the screen.
        let fit = fit_to_work_area(DESIRED, MIN, (800.0, 600.0), 1.0);
        assert_fit(fit, 800.0, 600.0, 800.0, 600.0);
    }

    #[test]
    fn falls_back_to_the_configured_size_without_a_usable_scale() {
        for scale in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let fit = fit_to_work_area(DESIRED, MIN, (1280.0, 720.0), scale);
            assert_fit(fit, 1280.0, 820.0, 900.0, 600.0);
        }
    }
}
```

同时创建 `desktop/src-tauri/src/lib.rs` 的一行接线，让测试能编译到该模块 —— 在文件顶部的 `#[cfg(desktop)] mod bridge;` 下一行加：

```rust
#[cfg(desktop)]
mod window;
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop/src-tauri && cargo test window:: 2>&1 | tail -20
```

Expected:编译通过（`unimplemented!()` 不报错），5 个测试全部 FAIL，失败信息为 `not implemented`。

- [ ] **Step 3: 实现最小逻辑**

把 `fit_to_work_area` 的 `unimplemented!()` 换成：

```rust
    // An unusable scale factor means we cannot compare physical pixels to the
    // logical ones the window API takes; keep the configured size rather than
    // guessing (a wrong guess would open the window off-screen).
    if !scale.is_finite() || scale <= 0.0 {
        return Fit {
            width: desired.0,
            height: desired.1,
            min_width: min_size.0,
            min_height: min_size.1,
        };
    }

    let work_w = work_area.0 / scale;
    let work_h = work_area.1 / scale;

    // A work area smaller than the minimum relaxes the minimum: opening a
    // window wider than the screen would be worse than a cramped one.
    let min_w = min_size.0.min(work_w);
    let min_h = min_size.1.min(work_h);

    Fit {
        // `clamp` order matters: cap by the work area first, then lift to the
        // minimum, so a tiny screen yields the minimum rather than a negative
        // or inverted range.
        width: desired.0.min(work_w * MAX_FRACTION).max(min_w),
        height: desired.1.min(work_h * MAX_FRACTION).max(min_h),
        min_width: min_w,
        min_height: min_h,
    }
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd desktop/src-tauri && cargo test window:: 2>&1 | tail -20
```

Expected:`test result: ok. 5 passed`。

- [ ] **Step 5: 提交**

```bash
git add desktop/src-tauri/src/window.rs desktop/src-tauri/src/lib.rs
git commit -m "feat(desktop): 把窗口初始尺寸夹到显示器工作区内"
```

---

## Task 2: 常量与 `tauri.conf.json` 一致（静态门禁）

**Files:**
- Modify: `desktop/src-tauri/src/window.rs`（追加测试）

**Interfaces:**
- Consumes: `DESIRED`、`MIN`（Task 1）。
- Produces: 无新接口；此任务只加测试。

**为什么需要：** 尺寸现在有两个真源（配置文件和常量）。配置改了常量不改，窗口会先按配置打开、再被 `setup()` 改成另一个尺寸，肉眼看到一次跳动。

- [ ] **Step 1: 写失败的测试**

在 `mod tests` 里追加（`use` 只需 `serde_json::Value`，已在 dev 依赖中）：

```rust
    /// Guards the one thing two sources of truth can silently disagree on: the
    /// config opens the window, `fit_main_window` then resizes it. A mismatch
    /// shows up as a visible jump at launch.
    #[test]
    fn config_matches_the_constants() {
        let raw = include_str!("../tauri.conf.json");
        let config: serde_json::Value = serde_json::from_str(raw).expect("valid tauri.conf.json");
        let win = &config["app"]["windows"][0];

        assert_eq!(win["width"].as_f64(), Some(DESIRED.0), "width");
        assert_eq!(win["height"].as_f64(), Some(DESIRED.1), "height");
        assert_eq!(win["minWidth"].as_f64(), Some(MIN.0), "minWidth");
        assert_eq!(win["minHeight"].as_f64(), Some(MIN.1), "minHeight");
        assert_eq!(win["center"].as_bool(), Some(true), "center");
    }
```

- [ ] **Step 2: 跑测试确认失败**

```bash
cd desktop/src-tauri && cargo test window::config_matches 2>&1 | tail -20
```

Expected:FAIL —— `width`: 实际 `Some(800.0)`，期望 `Some(1280.0)`。

- [ ] **Step 3: 改配置**

把 `desktop/src-tauri/tauri.conf.json` 的 `app.windows[0]` 改成：

```json
      {
        "title": "Yi-Agent",
        "width": 1280,
        "height": 820,
        "minWidth": 900,
        "minHeight": 600,
        "center": true,
        "titleBarStyle": "Overlay",
        "hiddenTitle": true,
        "backgroundColor": "#171717",
        "theme": "Dark"
      }
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd desktop/src-tauri && cargo test window:: 2>&1 | tail -20
```

Expected:`test result: ok. 6 passed`。

- [ ] **Step 5: 提交**

```bash
git add desktop/src-tauri/tauri.conf.json desktop/src-tauri/src/window.rs
git commit -m "feat(desktop): 默认窗口改为 1280x820 居中并设最小尺寸"
```

---

## Task 3: 接上 `setup()`，启动时按工作区夹取

**Files:**
- Modify: `desktop/src-tauri/src/window.rs`（追加 `fit_main_window`）
- Modify: `desktop/src-tauri/src/lib.rs`（`setup()` 里调用）

**Interfaces:**
- Consumes: `fit_to_work_area`、`DESIRED`、`MIN`（Task 1）。
- Produces: `pub fn fit_main_window(app: &tauri::AppHandle) -> tauri::Result<()>` —— 无窗口 / 无显示器时返回 `Ok(())`，不改动任何东西。

**为什么无法用单元测试覆盖：** 它只做 I/O 编排（取窗口、取显示器、发三个调用），所有分支逻辑已在 Task 1 的纯函数里测过。本任务的验证是编译 + 既存测试不回归 + 手动启动目视。

- [ ] **Step 1: 实现 `fit_main_window`**

在 `desktop/src-tauri/src/window.rs` 的 `fit_to_work_area` 之后追加：

```rust
/// Resize and centre the main window once, at startup.
///
/// Configuration already gives the window its default size; this only corrects
/// it for the monitor it landed on. Call it exactly once — re-running it after
/// the user has dragged the window would fight them.
///
/// Every "cannot measure" path returns `Ok(())` on purpose: the configured
/// size is a fine fallback, and a window that is merely smaller than ideal is
/// not a reason to fail launch.
pub fn fit_main_window(app: &tauri::AppHandle) -> tauri::Result<()> {
    use tauri::{LogicalSize, Manager};

    let Some(window) = app.get_webview_window("main") else {
        return Ok(());
    };
    let Some(monitor) = window.current_monitor()? else {
        return Ok(());
    };

    let work = monitor.work_area().size;
    let fit = fit_to_work_area(
        DESIRED,
        MIN,
        (f64::from(work.width), f64::from(work.height)),
        monitor.scale_factor(),
    );

    window.set_min_size(Some(LogicalSize::new(fit.min_width, fit.min_height)))?;
    window.set_size(LogicalSize::new(fit.width, fit.height))?;
    window.center()?;
    Ok(())
}
```

- [ ] **Step 2: 在 `setup()` 里调用**

`desktop/src-tauri/src/lib.rs` 的桌面分支中，把 `setup` 闭包改成：

```rust
        .setup(|app| {
            bridge::spawn(app.handle())?;
            // Sizing is cosmetic: log and continue rather than fail launch.
            if let Err(err) = window::fit_main_window(app.handle()) {
                eprintln!("[window] fit failed: {err}");
            }
            Ok(())
        });
```

- [ ] **Step 3: 编译 + 跑既有测试**

```bash
cd desktop/src-tauri && cargo test 2>&1 | tail -25
```

Expected:`test result: ok`，`window::` 6 个测试与 `bridge` 既有测试全绿，无 warning 提到未使用导入。

- [ ] **Step 4: 前端回归确认（未改前端，仅确认没连带影响）**

```bash
cd desktop && npx vitest run 2>&1 | tail -8
```

Expected:全部通过。

- [ ] **Step 5: 手动目视验证**

```bash
cd desktop && npm run sidecar && npm run tauri dev
```

Expected:主窗口首次打开明显大于原先的 800x600、位于屏幕正中；拖拽缩小到 900x600 时被挡住；拖大再重启，尺寸回到默认值（本次不做记忆）。

- [ ] **Step 6: 提交**

```bash
git add desktop/src-tauri/src/window.rs desktop/src-tauri/src/lib.rs
git commit -m "feat(desktop): 启动时按显示器工作区夹取窗口尺寸"
```

---

## Task 4: 更新模块文档

**Files:**
- Modify: `docs/project-management/desktop.md`（Features 列表，紧跟在现有窗口相关条目之后）

**Interfaces:**
- Consumes: Task 1–3 的代码位置。
- Produces: 无。

- [ ] **Step 1: 追加条目**

在 `docs/project-management/desktop.md` 的 Features 里追加一行（`[x]`，带可执行判据）：

```markdown
- [x] 首次启动窗口尺寸自适应（默认 1280x820 居中，最小 900x600，按显示器工作区 90% 夹取；不记忆用户改过的尺寸）— 配置 `desktop/src-tauri/tauri.conf.json`（`width` / `height` / `minWidth` / `minHeight` / `center`）；夹取 `desktop/src-tauri/src/window.rs:fit_to_work_area`（纯函数：物理工作区按 scale 换算为逻辑像素，`min(desired, work*0.9)` 再抬到最小尺寸，极小屏时最小尺寸回落到工作区）；接线 `desktop/src-tauri/src/window.rs:fit_main_window` / `desktop/src-tauri/src/lib.rs`（`setup()` 内调用一次，失败只记日志不阻断启动）；判据 `cd desktop/src-tauri && cargo test window::`（6 例：大屏保持默认 / Retina 换算 / 小屏收缩 / 极小屏夹紧 / 非法 scale 回落 / 配置与常量一致）— [设计](../superpowers/specs/2026-10-21-desktop-default-window-size-design.md)
```

- [ ] **Step 2: 校验格式与计数**

```bash
grep -c "^- \[x\]" docs/project-management/desktop.md && grep -n "desktop |" docs/project-management/README.md
```

Expected:条目数比改动前多 1；`README.md` 里 desktop 的「完成 / 总计」两个数字同步 +1（两处都改成新值）。

- [ ] **Step 3: 提交**

```bash
git add docs/project-management/desktop.md docs/project-management/README.md
git commit -m "docs: 登记桌面窗口尺寸自适应"
```

---

## 自查记录

- **Spec 覆盖：** 默认尺寸与居中 → Task 2；工作区 90% 夹取与最小尺寸回落 → Task 1；失败不 panic → Task 1 Step 3 + Task 3 Step 1；不记忆尺寸 → Global Constraints；文档 → Task 4。已全覆盖。
- **占位符扫描：** 无 TBD / TODO；每个代码步骤都给了完整代码。
- **类型一致性：** `Fit` 字段、`fit_to_work_area` 与 `fit_main_window` 的签名在 Task 1/3 中一致；`DESIRED` / `MIN` 两个任务同名同值。
