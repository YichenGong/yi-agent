//! watchman 值守的安装/卸载与就绪探测，以及项目级插件清单判定。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 4）。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

// 这些项仍由 `server.rs`（或其子模块经其 re-export）提供。
use crate::server::{IdleFacts, WatchmanInstall, WatchmanUninstall, plugin_query_socket_ready};

/// 纯决策：现在能不能回收这个项目的 runtime。
pub(crate) fn should_evict(facts: &IdleFacts, now: Instant, ttl: Duration) -> bool {
    if facts.has_running_turn || facts.has_plugin_manifest || facts.is_resident {
        return false;
    }
    now.duration_since(facts.last_active) >= ttl
}
/// 项目里是否有插件清单。
///
/// 只看「目录里有没有 .json」，不解析内容：损坏的清单仍然意味着有人往这个项目
/// 装了插件，回收 daemon 一样会打断它。宁可多留一个项目，也不误停插件。
pub(crate) fn project_has_plugin_manifest(project: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(yi_agent_supervisors::install::manifests_dir(project))
    else {
        return false;
    };
    entries.flatten().any(|entry| {
        entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "json")
    })
}
/// 轮询间隔。50ms 足够细，不会让就绪多等一整个监督周期。
pub(crate) const PLUGIN_READY_POLL: Duration = Duration::from_millis(50);
/// 带显式期限的版本：生产常量走上一层，测试用它把「永不就绪」压到毫秒级。
pub(crate) fn wait_for_plugin_query_socket_within(
    project: &Path,
    manifest: &yi_agent_supervisors::manifest::SupervisorManifest,
    timeout: Duration,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if plugin_query_socket_ready(project, manifest, &|socket| {
            std::os::unix::net::UnixStream::connect(socket).is_ok()
        }) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(PLUGIN_READY_POLL);
    }
}
pub(crate) fn watchman_home_from(home: Option<&std::ffi::OsStr>) -> PathBuf {
    home.map(PathBuf::from).unwrap_or_default()
}
/// 注入版：把 `YI_AGENT_HOME` 归一化成实例状态根。
pub(crate) fn skills_home_from(
    override_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> PathBuf {
    if let Some(dir) = override_home.filter(|v| !v.is_empty()) {
        let dir = PathBuf::from(dir);
        return if dir.file_name() == Some(std::ffi::OsStr::new(".yi-agent")) {
            dir
        } else {
            dir.join(".yi-agent")
        };
    }
    home.map(|h| PathBuf::from(h).join(".yi-agent"))
        .unwrap_or_default()
}
/// 生产的安装闭包：设置 `YI_AGENT_DISABLE_WATCHMAN` 时退化成 no-op。
///
/// 这个开关是为那些**驱动真实二进制**的 e2e 而设：它们跑的是生产接线，于是
/// `ui/settings/write` 的「打开开关时装值守」会走到这里。不拦的话它会在开发机
/// 真实的 launchd 域里执行 `launchctl bootstrap gui/<uid>`，并往真实的
/// `~/Library/LaunchAgents` 写 plist——测试绝不能碰宿主 launchd。为此 e2e 置该
/// 环境变量即可让安装/卸载变成空操作。
///
/// 只影响生产接线：测试用的 harness 各自注入记账闭包（见 `Harness`），不经过这里。
pub(crate) fn production_watchman_install() -> WatchmanInstall {
    if std::env::var_os("YI_AGENT_DISABLE_WATCHMAN").is_some() {
        return Arc::new(|_exe: &Path, _home: &Path| Ok(()));
    }
    Arc::new(yi_agent_supervisors::watchman::install)
}
/// 生产的卸载闭包；`YI_AGENT_DISABLE_WATCHMAN` 开关同
/// [`production_watchman_install`]。
pub(crate) fn production_watchman_uninstall() -> WatchmanUninstall {
    if std::env::var_os("YI_AGENT_DISABLE_WATCHMAN").is_some() {
        return Arc::new(|_home: &Path| Ok(()));
    }
    Arc::new(yi_agent_supervisors::watchman::uninstall)
}
/// 确保「后台值守」的登录项在装：开关为开、且 plist 未指向**当前**可执行文件时
/// 装一次。
///
/// 这是 `board/create` 时代那段逻辑的通用化落点：它只读通用偏好键、只比较 plist
/// 里的 exe 路径，不解释任何插件语义，故任何「装了一个需要常驻的插件」的路径都能
/// 调它。当前调用点是 `plugin/install` 成功之后。
///
/// best-effort：装不上值守不该让一次已经成功的安装回滚成失败，故只记 warn。
pub(crate) fn ensure_watchman_installed(workdir: &Path, home: &Path, install: &WatchmanInstall) {
    if !crate::settings_store::load_watchman_enabled(workdir) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    if yi_agent_supervisors::watchman::is_installed_for(home, &exe) {
        return;
    }
    if let Err(error) = install(&exe, home) {
        tracing::warn!(%error, "failed to install the resident daemon watchman");
    }
}
pub(crate) fn agent_trace_row(
    row: yi_agent_store::ipc::IpcTraceRow,
) -> crate::protocol::AgentTraceRow {
    crate::protocol::AgentTraceRow {
        event_id: row.event_id,
        task_id: row.task_id,
        kind: row.kind,
        payload_json: row.payload_json,
    }
}
