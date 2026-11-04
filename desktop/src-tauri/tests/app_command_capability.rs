//! 自定义命令与 ACL 清单的一致性护栏。
//!
//! 背景（真实事故，别再犯）：声明 app ACL manifest 之前，宿主自定义命令
//! **完全绕过 ACL**。证据链是一连串的“看起来通过”：探针没注册命令，于是
//! `rpc` 返回 `Command rpc not found`，被当成“ACL 拒绝了它”，报告 12/12 全绿，
//! 而实际上插件页面能直接调用 `set_relay_url`。
//!
//! 这类缺陷的共同点是**三份清单各自漂移而没有任何东西把它们绑在一起**：
//! 1. `lib.rs` 的 `invoke_handler`（真正暴露的命令）；
//! 2. `build.rs` 的 `APP_COMMANDS`（生成 ACL 用的清单）；
//! 3. `capabilities/default.json`（主窗口被允许调用的命令）。
//!
//! 任一份漏一条，要么主窗口调不动命令（3 漏），要么命令悄悄不可达且 ACL 未覆盖
//! （1 有、2 漏）。所以这里把三份都解析出来逐条比对，而不是靠人记住。

use std::collections::BTreeSet;

/// 从 `generate_handler![...]` 里取出命令名。
///
/// 只认 `a::b::name` 或裸 `name` 两种写法，够用且不会误吞宏参数。
fn handler_commands(source: &str) -> BTreeSet<String> {
    let start = source
        .find("generate_handler![")
        .expect("lib.rs must contain generate_handler![");
    let rest = &source[start..];
    let end = rest.find(']').expect("unterminated generate_handler![");
    let body = &rest["generate_handler![".len()..end];

    body.split(',')
        .filter_map(|entry| {
            let name = entry.trim();
            if name.is_empty() {
                return None;
            }
            // `crate::bridge::rpc` -> `rpc`
            Some(name.rsplit("::").next().unwrap_or(name).trim().to_string())
        })
        .collect()
}

/// 从 `const APP_COMMANDS: &[&str] = &[ ... ];` 里取出命令名。
fn manifest_commands(source: &str) -> BTreeSet<String> {
    let start = source
        .find("const APP_COMMANDS")
        .expect("build.rs must declare APP_COMMANDS");
    let rest = &source[start..];
    let end = rest.find("];").expect("unterminated APP_COMMANDS array");
    let body = &rest[rest.find("= &[").expect("APP_COMMANDS shape") + 4..end];

    body.split(',')
        .filter_map(|entry| {
            let value = entry.trim().trim_matches('"').trim();
            (!value.is_empty()).then(|| value.to_string())
        })
        .collect()
}

/// `set_relay_url` -> `allow-set-relay-url`（tauri-build 的命名规则）。
fn allow_identifier(command: &str) -> String {
    format!("allow-{}", command.replace('_', "-"))
}

/// 极简取值：只认 `"permissions": [ ... ]` 里的字符串字面量，够这份文件用。
fn capability_permissions(source: &str) -> BTreeSet<String> {
    let start = source.find("\"permissions\"").expect("permissions key");
    let rest = &source[start..];
    let end = rest.find(']').expect("unterminated permissions array");
    rest[..end]
        .split(',')
        .filter_map(|entry| {
            let value = entry.trim().trim_matches('"').trim();
            (!value.is_empty() && value != "\"permissions\":[" && !value.contains(':'))
                .then(|| value.to_string())
        })
        .collect()
}

fn lib_source() -> &'static str {
    include_str!("../src/lib.rs")
}
fn build_source() -> &'static str {
    include_str!("../build.rs")
}
fn capability_source() -> &'static str {
    include_str!("../capabilities/default.json")
}

#[test]
fn every_exposed_command_is_in_the_acl_manifest() {
    let exposed = handler_commands(lib_source());
    let manifested = manifest_commands(build_source());

    assert!(
        !exposed.is_empty(),
        "parsed no commands out of generate_handler![], the parser is broken"
    );

    let missing: Vec<_> = exposed.difference(&manifested).collect();
    assert!(
        missing.is_empty(),
        "these commands are exposed by invoke_handler but missing from build.rs \
         APP_COMMANDS, so no ACL permission is generated for them: {missing:?}"
    );

    let extra: Vec<_> = manifested.difference(&exposed).collect();
    assert!(
        extra.is_empty(),
        "APP_COMMANDS lists commands that do not exist in invoke_handler: {extra:?}"
    );
}

#[test]
fn the_main_window_is_allowed_every_app_command() {
    // 声明 app manifest 后自定义命令默认被拒。若主窗口没有逐条 opt in，
    // 应用会静默失去这些调用——比安全漏洞更难发现，所以要钉住。
    let exposed = handler_commands(lib_source());
    let declared = capability_permissions(capability_source());

    let missing: Vec<String> = exposed
        .iter()
        .map(|command| allow_identifier(command))
        .filter(|identifier| !declared.contains(identifier))
        .collect();

    assert!(
        missing.is_empty(),
        "the main window capability is missing these app permissions, so the UI \
         would get `not allowed by ACL` at runtime: {missing:?}"
    );
}

#[test]
fn the_plugin_page_is_not_covered_by_any_capability() {
    // 插件 webview 的 label 形如 `probe-1` / `plugin-<id>`；capability 只声明
    // `main`，所以它们拿不到任何放行。这条断言是“插件窗口不被任何 capability
    // 覆盖”的机械保证——只要有人给 capability 加了通配窗口，测试就红。
    let source = capability_source();
    // 只看 `windows` 数组。权限名里出现 `plugin-`（例如
    // `allow-open-plugin-view`）是正常的；关键是被覆盖的窗口标签。
    let start = source.find("\"windows\"").expect("windows key");
    let rest = &source[start..];
    let end = rest.find(']').expect("unterminated windows array");
    let windows = &rest[..end];
    for forbidden in ["\"*\"", "plugin-", "probe-"] {
        assert!(
            !windows.contains(forbidden),
            "the capability targets {forbidden}; the plugin page would inherit host \
             commands. Capabilities must stay scoped to `main`. Got: {windows}"
        );
    }
}

#[test]
fn the_parser_helpers_reject_surprising_input() {
    // 解析器坏了会让上面三条全都“通过”，这是最坏的情况：护栏失效而无声。
    assert_eq!(
        handler_commands("generate_handler![crate::bridge::rpc, remote_list]"),
        ["remote_list", "rpc"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    );
    assert_eq!(
        manifest_commands("const APP_COMMANDS: &[&str] = &[\n  \"rpc\",\n  \"remote_list\",\n];"),
        ["remote_list", "rpc"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    );
    assert_eq!(allow_identifier("set_relay_url"), "allow-set-relay-url");
}
