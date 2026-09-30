//! Superpowers 看板（Superpowers Board）插件内核。
//!
//! 纯逻辑：卡片模型、队列状态机、时段并发日历、两层开关解析、提升校验。
//! 本 crate 不做 I/O、不依赖 daemon、不做 IPC，因此可独立编译与测试。

pub mod board;
pub mod calendar;
pub mod card;
