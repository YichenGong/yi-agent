//! Superpowers 看板在宿主前端里的共享逻辑。
//!
//! 开关读写与视图数据装配放在这里，使 TUI 与 desktop 共用同一份语义
//! （尤其是两层解析顺序与原子写入），而不是各自重写一遍。

pub mod switch;
pub mod view;
