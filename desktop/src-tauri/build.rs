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

use std::path::{Path, PathBuf};
use std::process::Command;

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
/// Every command `invoke_handler` exposes must be listed here: declaring an app ACL
/// manifest makes every custom command denied by default, so a missing entry is not
/// a cosmetic omission -- the window gets `not allowed by ACL` at the first call.
///
/// Two groups arrived later than the rest and were missing until now: the runtime
/// lifecycle surface (the shell owns the lease) and confirmed foreground-root
/// recovery (destructive by design, so a local window action only). Keep this list
/// a flat array of string literals: the capability test parses it naively and a
/// comment between entries would silently drop the entry that follows.
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
    "runtime_acquire_project",
    "runtime_release_project",
    "runtime_registered_projects",
    "runtime_quit_outcome",
    "runtime_status",
    "runtime_preview",
    "runtime_confirm",
    "runtime_operation",
    "runtime_active_operation",
    "runtime_known_scopes",
    "runtime_scope_for_project",
    "runtime_plugin_demand_acquire",
    "runtime_plugin_demand_release",
    "runtime_migration_preview",
    "runtime_migration_confirm",
    "runtime_migration_operation",
    "runtime_active_migration",
    "open_plugin_view",
    "close_plugin_view",
    "plugin_committed_activation",
    "plugin_host_identity",
    "plugin_bind_host",
    "foreground_root_recovery_inspect",
    "foreground_root_recovery_prepare",
    "foreground_root_recovery_confirm",
    "foreground_root_recovery_operation",
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

    let expected = expected_runtime_build_id(kind);
    println!("cargo:rustc-env=YI_AGENT_EXPECTED_RUNTIME_BUILD_ID={expected}");
}

/// Ask the sidecar that this app will actually ship for its build id, and embed
/// the answer.
///
/// Why embed it at all: the manifest inside the bundle carries a build id too, but
/// it is written by the same script that copied the sidecar, so comparing the two
/// at runtime would only prove the script is self-consistent. Embedding the id in
/// the *wrapper* makes "the app was rebuilt but the sidecar was not" a detectable
/// mismatch instead of a silent pairing of new UI with an old daemon.
///
/// Resolution order is the one the bundle actually ships with:
///   1. `src-tauri/binaries/yi-agent-<triple>` (Tauri's externalBin input)
///   2. `<target>/<profile>/yi-agent` (a plain `cargo build` of the sidecar)
///
/// A release build with no sidecar is a hard error: shipping an unverifiable
/// daemon identity is worse than failing the build. Dev builds embed an empty id
/// and report `RuntimeArtifact::Unknown` at runtime.
fn expected_runtime_build_id(kind: &str) -> String {
    let Some(binary) = find_sidecar() else {
        if kind == "release" {
            panic!(
                "no yi-agent sidecar found under binaries/ or the cargo target dir; \
                 run desktop/scripts/build-sidecar.sh before packaging"
            );
        }
        return String::new();
    };
    // Rebuild the shell whenever the sidecar is rebuilt, otherwise a stale
    // embedded id would reject a perfectly good bundle.
    println!("cargo:rerun-if-changed={}", binary.display());

    let output = Command::new(&binary)
        .args(["internal-runtime", "identity", "--json"])
        .output()
        .unwrap_or_else(|error| panic!("failed to run {}: {error}", binary.display()));
    if !output.status.success() {
        panic!(
            "{} internal-runtime identity failed: {}",
            binary.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // serde_json rather than string slicing: the CLI pretty-prints, and a hand
    // parser that assumes a particular spacing silently breaks on format changes.
    let identity: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("unparseable sidecar identity output: {error}\n{text}"));
    identity["build_id"]
        .as_str()
        .unwrap_or_else(|| panic!("no build_id in sidecar identity output: {text}"))
        .to_string()
}

/// Locate the sidecar this build would ship.
fn find_sidecar() -> Option<PathBuf> {
    let triple = std::env::var("TAURI_ENV_TARGET_TRIPLE")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_default();
    let binaries = Path::new("binaries");
    if !triple.is_empty() {
        let candidate = binaries.join(format!("yi-agent-{triple}"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    // Fall back to whatever single `yi-agent-*` is present, so a cross target
    // that sets a different triple string still resolves.
    let entries = std::fs::read_dir(binaries).ok()?;
    let mut found: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("yi-agent-"))
        })
        .collect();
    found.sort();
    if found.len() > 1 {
        panic!("multiple sidecars in binaries/: {found:?}; remove the stale one");
    }
    found.pop()
}
