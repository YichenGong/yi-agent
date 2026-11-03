//! 生成构建标识（`dev` / `release`），供运行期写进 `yi-agent-flags`。
//!
//! **为什么桌面壳需要自己算一遍**：`yi-agent-flags` 的 `build_kind()` 默认
//! 返回 `"release"`，必须由进程在 `main` 里显式 `set_build_info` 注入。
//! `yi-agent` 二进制由它自己的 build script 注入，而 Tauri 壳是另一个包，
//! 不注入就等于**任何开关都恒为关**——包括在 `tauri dev` 里。
//!
//! 判定规则：
//! - `YI_AGENT_BUILD_KIND` 显式指定时优先（打包实验版
//!   `YI_AGENT_BUILD_KIND=dev npx tauri build`）。
//! - 否则按 cargo profile：`debug` → `dev`，其余 → `release`。
//!   这里刻意不用 git tag 判定：Tauri 壳的版本号来自 `tauri.conf.json`，
//!   与 workspace 的 tag 无强绑定，按 tag 猜只会让结果更难解释。

use std::path::Path;

/// 应用自定义命令的 ACL 清单。
///
/// **为什么必须有它。** Tauri 只在三种情况下对自定义命令做 ACL 检查：命令是
/// `plugin:` 命令、应用声明了 app manifest、或来源不是 local。`plugin-asset://`
/// 是已注册协议，因此插件页面被判定为 local；本仓库原先没有 `permissions/`，
/// `has_app_acl_manifest` 恒为 false。三者全不成立，于是 `rpc`、`set_relay_url`
/// 等命令对插件页面**完全不经过 ACL**（实测可调用 `set_relay_url`）。
///
/// 声明 manifest 后，自定义命令默认不被任何窗口允许，必须在 capability 里逐个
/// 列出——这正是不想让插件窗口拿到的能力。新增 `bridge::*` 命令时必须同步加到
/// 这里和 `capabilities/default.json`，否则主窗口会调不动它。
const APP_COMMANDS: &[&str] = &[
    "rpc",
    "rpc_respond",
    "set_relay_url",
    "restart_sidecar",
    "kill_remote_slots",
    "remote_connect",
    "remote_disconnect",
    "remote_status",
    "remote_enabled",
    "remote_list",
    "remote_upsert",
    "remote_remove",
];

fn main() {
    let mut attributes = tauri_build::Attributes::new();
    attributes = attributes.app_manifest(tauri_build::AppManifest::new().commands(APP_COMMANDS));
    tauri_build::try_build(attributes).expect("failed to run tauri-build");

    println!("cargo:rerun-if-env-changed=YI_AGENT_BUILD_KIND");

    let forced = std::env::var("YI_AGENT_BUILD_KIND")
        .ok()
        .filter(|value| !value.is_empty());
    let kind = match forced.as_deref() {
        Some("dev") => "dev",
        Some("release") => "release",
        Some(other) => panic!("YI_AGENT_BUILD_KIND must be `dev` or `release`, got `{other}`"),
        None if std::env::var("PROFILE").as_deref() == Ok("debug") => "dev",
        None => "release",
    };
    println!("cargo:rustc-env=YI_AGENT_BUILD_KIND={kind}");

    // 版本号与 `tauri.conf.json` 同源：CI 会断言两边相等。
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    println!("cargo:rustc-env=YI_AGENT_WRAPPER_VERSION={version}");
    println!(
        "cargo:rerun-if-changed={}",
        Path::new("tauri.conf.json").display()
    );
}
