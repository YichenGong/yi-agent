//! 主窗口 CSP 的回归护栏。
//!
//! # 为什么这条测试存在
//!
//! T17 之前 `csp` 是 `null`：没有任何策略，因此"插件页面被挡"无法由主窗口
//! 配置证明。把 `csp` 设上之后，第一类事故不是"没生效"，而是**主窗口自己坏**：
//! 少写一条 `img-src`，工具产出的图片不显示；少写 `ws:`，远程 relay 连不上；
//! 少写 `style-src 'unsafe-inline'`，主题令牌与滚动容器样式失效。
//!
//! 所以这里不是"CSP 非空"这种弱断言，而是**逐条列出主窗口真实需要的来源，
//! 并说明每一条对应哪个功能**。删掉一条，就必须同时删掉对应功能或给出替代
//! 方案，否则测试红。
//!
//! # 为什么不用 `*`
//!
//! 全开能一次修好所有报错，也会让"插件页面不能联网"这条同时失效——同一个
//! `frame-src` 既服务主窗口的 iframe，也是插件视图的边界。

use std::collections::BTreeMap;

/// 解析 `"csp"` 字段的值。只做这一件事，避免为 JSON 引一个解析器。
fn configured_csp() -> String {
    let source = include_str!("../tauri.conf.json");
    let start = source
        .find("\"csp\"")
        .expect("tauri.conf.json must declare csp");
    let rest = &source[start..];
    let open = rest.find('"').unwrap();
    let after_key = &rest[open + 1..];
    let colon = after_key.find(':').unwrap();
    let value = &after_key[colon + 1..];
    let first_quote = value.find('"').unwrap();
    let body = &value[first_quote + 1..];
    let end = body.find('"').unwrap();
    body[..end].to_string()
}

/// 把 CSP 拆成 directive → 来源列表。
fn directives(csp: &str) -> BTreeMap<String, Vec<String>> {
    csp.split(';')
        .filter_map(|part| {
            let mut tokens = part.split_whitespace();
            let name = tokens.next()?;
            Some((name.to_string(), tokens.map(str::to_string).collect()))
        })
        .collect()
}

fn allows(map: &BTreeMap<String, Vec<String>>, directive: &str, source: &str) -> bool {
    map.get(directive)
        .map(|sources| sources.iter().any(|candidate| candidate == source))
        .unwrap_or(false)
}

#[test]
fn the_main_window_has_a_policy_at_all() {
    let csp = configured_csp();
    assert!(
        !csp.trim().is_empty(),
        "`csp` is null/empty again; the plugin view boundary and the main window \
         would both run without a policy"
    );
    assert!(
        !csp.contains('*'),
        "the CSP contains a wildcard source, which would also widen the plugin \
         view boundary: {csp}"
    );
}

#[test]
fn the_main_window_can_still_load_its_own_bundle() {
    let map = directives(&configured_csp());
    // 打包后前端从 `tauri://localhost`（或 Windows 上的 `http://tauri.localhost`）
    // 加载，因此 'self' 就够；不要为了省事写 `http:`/`https:`。
    assert!(allows(&map, "default-src", "'self'"));
    assert!(allows(&map, "script-src", "'self'"));
    // Tailwind 的基线样式与主题切换通过内联 style 属性生效。
    assert!(
        allows(&map, "style-src", "'unsafe-inline'"),
        "the theme tokens and scroll containers rely on inline styles"
    );
}

#[test]
fn the_tool_output_images_still_render() {
    let map = directives(&configured_csp());
    // `useImageData` 把分片读回的字节包成 blob URL 交给 `<img src>`；
    // 图标与部分 SVG 走 data URL。
    assert!(
        allows(&map, "img-src", "blob:"),
        "tool output images are object URLs created from bytes"
    );
    assert!(
        allows(&map, "img-src", "data:"),
        "inline icons and small SVG assets use data URLs"
    );
}

#[test]
fn the_remote_transports_still_connect() {
    let map = directives(&configured_csp());
    // 远程会话走 WebSocket relay（`wss://`），本机直连是 `ws://127.0.0.1`。
    assert!(
        allows(&map, "connect-src", "ws:"),
        "the local sidecar is a ws:// socket"
    );
    assert!(
        allows(&map, "connect-src", "wss:"),
        "remote relays are wss:// endpoints"
    );
    // Tauri 的 IPC 在 macOS 上是自定义 scheme，在 Windows 上走 http://ipc.localhost。
    assert!(allows(&map, "connect-src", "ipc:"));
    assert!(allows(&map, "connect-src", "http://ipc.localhost"));
}

#[test]
fn plugin_views_are_embeddable_but_not_from_elsewhere() {
    let map = directives(&configured_csp());
    assert!(
        allows(&map, "frame-src", "plugin-asset:"),
        "the main window embeds plugin views over the plugin-asset scheme"
    );
    assert!(
        allows(&map, "frame-ancestors", "'none'"),
        "the main window must not be framed by anyone"
    );
}

#[test]
fn the_policy_blocks_the_implicit_exfiltration_routes() {
    let map = directives(&configured_csp());
    // 这几条与 `ContainerConfig::strict_csp` 的意图一致：只写 connect-src
    // 挡不住 `<form>`、`<object>`、`base` 改道与 worker 旁路。
    assert!(allows(&map, "object-src", "'none'"));
    assert!(allows(&map, "form-action", "'none'"));
    assert!(allows(&map, "base-uri", "'self'"));
    assert!(allows(&map, "worker-src", "'none'"));
    // `frame-src` 必须显式给值：缺失时落到 default-src 'self'，
    // 而那会让 `plugin-asset:` 被拒。
    assert!(!map.get("frame-src").map(Vec::is_empty).unwrap_or(true));
}

#[test]
fn inline_scripts_are_not_allowed_in_the_main_window() {
    let map = directives(&configured_csp());
    assert!(
        !allows(&map, "script-src", "'unsafe-inline'"),
        "the main window ships its own bundle; allowing inline scripts would \
         remove the main defence against injected markup"
    );
}

#[test]
fn the_plugin_view_policy_is_stricter_than_the_main_window() {
    let main = directives(&configured_csp());
    let view = directives(&desktop_lib::plugin_host::container::ContainerConfig::default().csp);
    // 插件视图不允许任何网络、也不允许被嵌入别处。
    assert!(allows(&view, "connect-src", "'self'"));
    assert!(!allows(&view, "connect-src", "ws:"));
    assert!(!allows(&view, "connect-src", "wss:"));
    assert!(allows(&view, "frame-src", "'none'"));
    assert!(allows(&view, "frame-ancestors", "'none'"));
    // 主窗口的放行项不能出现在视图策略里。
    for source in ["ws:", "wss:", "blob:", "data:"] {
        assert!(
            !allows(&view, "img-src", source) && !allows(&view, "connect-src", source),
            "the view policy inherited `{source}` from the main window"
        );
    }
    let _ = main;
}
