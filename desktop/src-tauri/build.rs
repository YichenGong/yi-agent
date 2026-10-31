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

fn main() {
    tauri_build::build();

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
