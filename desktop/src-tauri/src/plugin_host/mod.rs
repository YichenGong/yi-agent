//! 插件宿主容器（`plugin_host`）的入口。
//!
//! 正式容器在 T17 落地；这里目前只有 `plugin-probe` feature 下的技术探针。
//! 模块整体由 feature 门控，正式构建里不存在这段代码，也就不可能被误用成
//! “测试模式绕过安全配置”的后门。

#[cfg(feature = "plugin-probe")]
pub mod probe;

#[cfg(feature = "plugin-probe")]
pub mod run;
