# Desktop 首次启动窗口尺寸放大

日期：2026-10-21
状态：已确认

## 问题

`desktop/src-tauri/tauri.conf.json` 里主窗口初始尺寸是 `800x600`，没有最小尺寸也没有居中。在 3440x1440 这类宽屏上首次打开时窗口明显偏小，且落在屏幕左上角，看起来像被挤住的一小块。

## 目标

- 首次启动时窗口默认更大（不再有"窗口太小"的观感），并居中显示。
- 屏幕较小时窗口自适应收缩，绝不超出显示器工作区（不含菜单栏 / Dock / 任务栏）。
- 用户手动拖拽 / 缩放过的尺寸不被覆盖 —— 本次**不**引入 `tauri-plugin-window-state`，只改首次默认值。

非目标：记住上次窗口尺寸与位置（方案 C，已明确不做）、多显示器记忆、移动端（iOS 不涉及窗口尺寸）。

## 方案

### 1. 配置层：`desktop/src-tauri/tauri.conf.json`

主窗口改为：

```json
"width": 1280,
"height": 820,
"minWidth": 900,
"minHeight": 600,
"center": true
```

Tauri 配置中的尺寸是逻辑像素，Retina 上不会被放大成两倍。`minWidth/minHeight` 防止用户拖到不可用的窄条；`center: true` 让窗口出现在屏幕正中而不是左上角。

### 2. 小屏保护：新增 `desktop/src-tauri/src/window.rs`

在 `setup()` 中、侧车 spawn 之后执行一次 `window::fit_main_window(app.handle())`：

1. 取主窗口 `current_monitor()`（WebviewWindow 提供），读其 `work_area()`（物理像素）与 `scale_factor()`。
2. 工作区换算成逻辑像素：`logical = physical / scale_factor`。
3. 最小尺寸 = `min(900, work_logical_width)` × `min(600, work_logical_height)`，避免屏幕本身就比最小尺寸小时窗口被撑出屏幕。
4. 目标尺寸 = 配置尺寸，收缩到工作区 90% 以内，再夹紧到最小尺寸之上：
   - `width = clamp(1280, min_width, work_logical_width * 0.9)`
   - `height = clamp(820, min_height, work_logical_height * 0.9)`

   `clamp(v, lo, hi) = v.min(hi).max(lo)`。当工作区小于最小尺寸对应的 90% 时（极小屏），窗口取最小尺寸而不是比它还窄 —— 否则 `set_size` 的结果会被窗口管理器按 min size 回弹，行为不可预期。
5. `set_min_size(...)`、`set_size(...)`、`center()`。

错误处理：`current_monitor()` 返回 `None`（极少数情况下窗口尚未附着到显示器）或 `scale_factor` 非正时，直接返回，配置里的 `width/height/center` 仍然生效 —— 退化为"没有小屏保护"，不会 panic。

### 3. 可测性

核心是纯函数，不碰真实窗口：

```rust
pub struct Fit {
    pub width: f64,
    pub height: f64,
    pub min_width: f64,
    pub min_height: f64,
}

/// `desired` 与 `min_size` 是逻辑像素，`work_area` 是物理像素，`scale` 是
/// 物理→逻辑的缩放因子。
pub fn fit_to_work_area(
    desired: (f64, f64),
    min_size: (f64, f64),
    work_area: (f64, f64),
    scale: f64,
) -> Fit
```

单元测试（`#[cfg(test)] mod tests`，风格与 `bridge.rs` 的测试一致，纯计算无 I/O）：

| 用例 | 输入（desired 1280x820 / min 900x600，work 为物理像素） | 期望 |
| --- | --- | --- |
| 大屏保持默认尺寸 | work 3440x1410 @ scale 1 | size 1280x820，min 900x600 |
| Retina 等价 | work 2880x1800 @ scale 2（逻辑 1440x900） | size 1280x810，min 900x600 |
| 小屏收缩 | work 1280x720 @ scale 1 | size 1152x648，min 900x600 |
| 极小屏 | work 800x600 @ scale 1 | min 800x600，size 800x600（夹紧到 min 之上） |
| 非法 scale | scale 0 / NaN | 直接返回 desired 1280x820 与 min 900x600，不 panic |

（具体数值以最终实现为准，测试断言直接写期望值。）

### 4. 不做的事

- 不引入 `tauri-plugin-window-state`。
- 不改 `tauri.ios.conf.json`。
- 不改前端 `App.tsx`。
- 不改 `capabilities/default.json`：`set_size` / `set_min_size` / `center` 都在 Rust 侧调用，不走前端 IPC，无需新增权限。

## 验证

1. `cd desktop/src-tauri && cargo test` —— 新单元测试通过，既有 `bridge` 测试不回归。
2. `cd desktop && npm test` —— 前端测试不回归（本次未改前端，作回归确认）。
3. `cd desktop && npm run tauri dev` 目视确认：窗口首次打开明显大于 800x600 且居中；拖到最小尺寸时被 900x600 挡住。
4. 小屏保护逻辑由单元测试覆盖；真机小屏不可用时以测试为准。

## 影响面

- `desktop/src-tauri/tauri.conf.json`（尺寸与最小尺寸）
- `desktop/src-tauri/src/window.rs`（新增）
- `desktop/src-tauri/src/lib.rs`（setup 中调用一次）
- `docs/project-management/desktop.md`（记录该条 feature 与判据）
