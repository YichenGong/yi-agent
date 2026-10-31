# macOS 关闭窗口改为隐藏到后台

日期：2026-10-31
状态：已批准（方案 A：拦截关闭请求 + 隐藏窗口）

## 问题

桌面 App 点左上角红叉就直接退出了。macOS 的惯例不是这样：红叉应只关掉窗口，
应用留在 Dock 上，点 Dock 图标再把窗口拿回来。用户当前的体验是「误关一下就
整个 App 没了」，正在跑的 agent 任务、连带 sidecar 一起被终止。

根因在框架默认行为，不难定位：

- `tauri.conf.json` 没有声明任何窗口事件处理，`desktop/src-tauri/src/lib.rs`
  直接 `builder.run(tauri::generate_context!())`，没有任何 `RunEvent` 分支。
- Tauri 在**窗口全部被销毁**时会发出 `ExitRequested { code: None }`
  （`tauri-runtime-wry-2.11.4/src/lib.rs:4311-4325`）。窗口默认「点了关闭就销毁」，
  于是唯一那个窗口一销毁，事件循环就 `ControlFlow::Exit`，进程退出。
- 前端也没有 `onCloseRequested` 之类逻辑可以拦截。

## 目标与非目标

目标：macOS 上点红叉（含 Cmd+W）时窗口隐藏、应用继续驻留 Dock；点 Dock 图标
窗口重新出现并聚焦。

非目标（本次明确不做）：

- 不做托盘 / 菜单栏图标（用户选择方案 A，用 Cmd+Q 退出即可）。
- 不改 Windows/Linux 行为。两个平台没有 Dock，也没有 `RunEvent::Reopen`；
  红叉在那里本来就该是退出。强行隐藏会得到一个既关不掉又不显示的 App。
- 不改前端任何代码。窗口只是隐藏，webview 不销毁，React 状态原样保留。
- 不改 sidecar 生命周期管理（`bridge.rs` 的 supervisor 不感知窗口可见性）。

## 设计

### 行为

| 操作 | 结果 |
|---|---|
| 点红叉 / Cmd+W | 窗口隐藏；App 留在 Dock；sidecar 与在跑的 turn 不受影响 |
| 点 Dock 图标（窗口已隐藏） | 窗口显示并聚焦到前台 |
| 点 Dock 图标（窗口可见，被其他应用遮住） | 系统自己激活 App；我们不做额外动作 |
| Cmd+Q / 菜单「退出 yi-agent」/ Dock 右键退出 | 真正退出（不变） |

「点 Dock 图标时窗口可见」这一支由 `RunEvent::Reopen { has_visible_windows }`
的 `has_visible_windows == true` 表达——此时只需把 App 提到前台，那是系统对
Dock 点击的默认处理，重复调用 `show()` 反而可能抢掉用户正在切换的焦点。

### 为什么拦关闭请求而不是别的做法

- `prevent_close()` 之后窗口对象仍在（不销毁），前端状态（滚动位置、正在
  流式输出的转录、输入框草稿）全部保留，恢复是瞬时的。
- 若改成「允许销毁 + 阻止退出 + 点 Dock 时重建窗口」，内存释放更彻底，但会
  丢掉全部前端状态、恢复慢，还要处理「重建失败」分支，收益不值这个复杂度。

### 为什么不会误伤 Cmd+Q

Cmd+Q 走的是 macOS 原生 `terminate:` 菜单动作
（`muda-0.19.3/src/platform_impl/macos/mod.rs:994`），不经过窗口的
`CloseRequested`，因此 `prevent_close()` 拦不到它。Dock 图标被点击时系统调用
`applicationShouldHandleReopen`，tao 把它转成 `Event::Reopen`
（`tao-0.35.3/src/platform_impl/macos/app_delegate.rs:207-215`），Tauri 再转成
`RunEvent::Reopen`（`tauri-2.11.6/src/app.rs:2632-2635`）——这条事件在
Windows/Linux 上根本不存在，这也是实现必须按平台条件编译的原因。

### 改动点

**1. `desktop/src-tauri/src/window.rs`** 增加三个函数（与既有的
`fit_main_window` 并列，同属「主窗口杂务」）：

```rust
/// 关闭请求：不关窗，只隐藏。
pub fn hide_on_close(app: &tauri::AppHandle, label: &str, api: &tauri::CloseRequestApi)

/// Dock 图标被点、且当时没有可见窗口：把主窗口拿回来。
pub fn reveal_main_window(app: &tauri::AppHandle)

/// `RunEvent` 的窗口行为策略：把上面两个函数接到框架事件上。
pub fn handle_run_event(app: &tauri::AppHandle, event: &tauri::RunEvent)
```

- `hide_on_close`：`api.prevent_close()`，然后取 `main` 窗口并 `hide()`。
  **只对 `main` 生效**：其他 label 直接返回，保持原来的关闭语义，这样将来加第二个
  窗口时不会意外变成「全都关不掉」。隐藏失败只记日志、不 panic——此时窗口已经被
  prevent，最坏结果是窗口留着但 App 没退出，比崩溃好。
- `reveal_main_window`：取 `main` 窗口，`show()` + `set_focus()`。取不到窗口
  （理论上不会发生）就静默返回。
- `handle_run_event`：上面那个 match 就住在这里，`lib.rs` 只调用它。把它抽成
  具名函数是**为了可测**——测试驱动的是生产代码本身，而不是测试里重写一遍
  match（见下节）。
- 三个函数均 `#[cfg(target_os = "macos")]`。

取窗口一律用 **`Manager::get_webview_window("main")`**，与既有的
`fit_main_window`（`desktop/src-tauri/src/window.rs`）同一访问器。原型测试实测
过它在 `CloseRequested` 时能取到窗口；`Manager::get_window` 是另一条路径，未验证，
不采用。

**2. `desktop/src-tauri/src/lib.rs`**：把
`builder.run(tauri::generate_context!())` 拆成 `builder.build(...)` 再
`app.run(|app, event| ...)`。之所以不用 `Builder::on_window_event`，是因为
`Reopen` 是应用级事件，只有 `run` 的回调能拿到；把两处逻辑分到两个回调里读起来
更绕。回调体只有一行转发。

```rust
let app = builder
    .build(tauri::generate_context!())
    .expect("error while building tauri application");

#[cfg(target_os = "macos")]
app.run(|app, event| window::handle_run_event(app, &event));
#[cfg(not(target_os = "macos"))]
app.run(|_app, _event| {});
```

两个 `cfg` 分支只有一条参与编译，因此非 macOS 上回调为空，行为与今天逐字节
一致，也不会产生未使用变量告警。（`App::run` 取 `self` 所有权，`App::build`
先返回 `App` 再 `run` 是官方推荐的两步写法。）

窗口 event 分支里拿不到 `Window` 对象：`RunEvent::WindowEvent` 只给 `label`
（`tauri-2.11.6/src/app.rs:235-240`），因此 `hide_on_close` 收 `&AppHandle` +
label，按 label 判断是否 `main`，再用 `get_webview_window` 取句柄。

**3. 平台条件编译的落点**：`window.rs` 的三个函数与 `lib.rs` 的转发都加
`#[cfg(target_os = "macos")]`。非 macOS 构建里行为与今天逐字节一致。

**4. 不影响前端**：窗口只是隐藏，没有销毁，webview 的
`visibilitychange` 会从 `visible` 变 `hidden`——已核对现有处理：两处监听
（`desktop/src/App.tsx:635` 的 flush、`:1772` 的重连）都只在
`visibilityState === "visible"` 时动作，隐藏不动任何状态，重新显示时顺手触发一次
重连自愈，是既有设计意图，不需要改。

### 测试

**集成测试（新增）**：`desktop/src-tauri/tests/close_hides_window.rs`，用 Tauri
官方 `tauri::test` 的 MockRuntime。dev-dependencies 里已加
`tauri = { version = "2", features = ["test"] }`（与本 spec 同一分支）。

MockRuntime 的 `Message::CloseWindow` 分支会先发 `CloseRequested`、等回调、
再在没被 prevent 时移除窗口，最后在窗口清空时发 `ExitRequested`
（`tauri-2.11.6/src/test/mock_runtime.rs:1335-1370`）——这条路径与真实运行时的
顺序一致（`tauri-runtime-wry-2.11.4/src/lib.rs:4438-4467`）。

**已实测确认**（本分支上用临时探针跑过，探针已删除）：同一段「在
`MainEventsCleared` 里 `window.close()`，下一轮检查窗口是否存在」的驱动代码，

| `prevent_close()` | `close_requests` | `alive_after_close` | 循环是否 break |
|---|---|---|---|
| 调用 | 1 | `Some(true)` | 否（需再 `destroy()` 才退出） |
| 不调用 | 1 | `None` | 是（窗口一移除就 break） |

因此 `alive_after_close == Some(true)` 是能区分两种行为的有效断言，而不是恒真。

测试形态：驱动 `window::handle_run_event`（生产的那个 match），让 `App::run`
的闭包把事件转给它，用 `Arc<Mutex<_>>` 记录观测值（**不能**用普通 `let mut`
捕获——闭包拿走所有权后，外层读到的是初始值，探针第一次就踩了这个坑）。

```rust
let app = tauri::test::mock_app();
let window = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
    .build().unwrap();

app.run(move |app, event| {
    window::handle_run_event(app, &event);        // 生产代码

    match event {
        RunEvent::MainEventsCleared if !close_sent => {
            close_sent = true;
            let _ = window.close();
        }
        RunEvent::MainEventsCleared => {
            // 下一轮：close 已被处理过
            record(app.get_webview_window("main").is_some());
            let _ = window.destroy();            // 结束 mock 的无限 loop
        }
        _ => {}
    }
});

assert_eq!(recorded_alive_after_close, Some(true));
```

结束循环只能走 `window.destroy()`，**不能用 `app.exit()`**——MockRuntime 的
`request_exit` 是 `unimplemented!()`
（`tauri-2.11.6/src/test/mock_runtime.rs:143-145`）。`destroy` 让窗口表变空，
mock 才发未拦截的 `ExitRequested` 并 break。这顺带验证了策略**只**拦
`CloseRequested`，没有把销毁路径也拦死。

`Reopen` 的显示动作测不了：mock 的 `is_visible()` 恒返回 `true`
（`tauri-2.11.6/src/test/mock_runtime.rs:777-779`），无法区分隐藏与显示。这一条
不做单测，靠下面的真机验收。

**纯函数与既有测试**：`window.rs` 现有的 6 个 `fit_to_work_area` 测试不受影响。

**手工验收（必须做，功能性回归）**：

```bash
cd desktop && npm run build && npx tauri dev
```

1. 点红叉：窗口消失，Dock 图标仍在，`ps axo command | grep "app-server --listen"`
   里 sidecar 仍在跑。
2. 点 Dock 图标：窗口以隐藏前的样子回来（滚动位置、输入框草稿都还在）。
3. 在会话里发一条会跑一阵的消息，跑的过程中点红叉，等 10 秒再点 Dock 图标：
   输出继续在推进，说明隐藏没有打断 sidecar 通道。
4. Cmd+Q：App 真正退出，Dock 图标消失，sidecar 一并退出。

### 文档

- `docs/project-management/desktop.md` 增加一条 `[x]` feature，判据写测试文件与
  手工验收命令，并链接本 spec。
- `docs/project-management/README.md` 的 `desktop` 计数同步 `+1`。

## 风险

- **窗口隐藏后 webview 定时器被系统节流**：WKWebView 隐藏时会降频，当前实现里
  唯一依赖定时器的是看板 2 秒刷新（`desktop/src/App.tsx:1774`）与内容落盘防抖，
  都不是用户可感知的正确性问题；重新显示时 `visibilitychange` 会触发重连自愈。
- **用户找不到退出入口**：Cmd+Q 与菜单栏「退出 yi-agent」都在，且这是 macOS
  标准行为，不额外做托盘。
