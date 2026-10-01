//! Superpowers 看板的 IPC 线格式客户端。
//!
//! 刻意手写协议结构，不依赖任何 `yi-agent-*` crate，因此插件可以独立
//! 安装与卸载。协议版本在信封里校验，不匹配即报错，不做兼容猜测。

pub mod wire;
