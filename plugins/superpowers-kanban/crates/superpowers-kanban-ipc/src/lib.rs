//! Superpowers 看板的 IPC 线格式客户端。
//!
//! 刻意手写协议结构，不依赖任何 `yi-agent-*` crate，因此插件可以独立
//! 安装与卸载。协议版本在信封里校验，不匹配即报错，不做兼容猜测。

pub mod client;
/// 宿主业务 daemon 的 socket 地址解析（与本 crate 的查询通道规则分开）。
pub mod daemon_socket;
pub mod server;
pub mod socket;
pub mod wire;
