//! 通用的「按开关托管子进程」能力。
//!
//! 本 crate 不认识任何具体插件：它只读清单里的通用字段（命令、参数、开关键、
//! state-dir 占位）并负责拉起/守护/停止。任何自主场景都可复用它。

pub mod manifest;
pub mod supervisor;
pub mod switch;
