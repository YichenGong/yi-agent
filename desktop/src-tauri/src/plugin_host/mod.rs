//! 插件宿主容器（`plugin_host`）的入口。
//!
//! 两条线：
//! - 正式容器（本文件下的 `assets`/`cache`/`container`/`recovery`/`host`）：始终编译，
//!   由宿主在打开插件视图时使用。
//! - 技术探针（`probe`/`run`）：只在 `plugin-probe` feature 下存在，用来证明
//!   WebView 层面的隔离真的生效。正式构建里没有这段代码。
//!
//! 资源读取的唯一入口是 [`cache::ViewRegistry::resolve`]：它只接受宿主自己
//! 发出的实例句柄，不接受路径参数。

pub mod assets;
pub mod cache;
pub mod container;
pub mod host;
pub mod recovery;
pub mod remote_assets;

#[cfg(feature = "plugin-probe")]
pub mod probe;

#[cfg(feature = "plugin-probe")]
pub mod run;

#[cfg(test)]
mod tests;

/// 进程级的宿主状态。
///
/// 拿不到实例根时**不 panic**：桌面壳在没有 home 的环境里也应该能启动并让
/// 主窗口可用，只是插件视图会返回可读的错误。启动期 panic 会把"插件不可用"
/// 变成"应用打不开"，那是更糟的失败模式。
pub fn host_state() -> std::sync::Arc<host::PluginHost> {
    match host::PluginHost::from_env() {
        Ok(host) => std::sync::Arc::new(host),
        Err(error) => {
            eprintln!("[plugin-host] {error}");
            // 用一个占位的、必然拒绝打开的根：所有操作都返回 NotFound，
            // 而不是悄悄退回当前目录。
            std::sync::Arc::new(host::PluginHost::new(std::path::PathBuf::from(
                "\u{0}no-instance-home",
            )))
        }
    }
}
