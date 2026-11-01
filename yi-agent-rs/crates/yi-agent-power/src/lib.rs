//! 空闲保活的纯逻辑层：策略、时钟、探针、平台断言、10s 拍状态机。
//!
//! 这个 crate **不**包含任何进程装配代码——谁持有断言、多久拍一次、busy 从哪
//! 来，全是调用方（`yi-agent-app-server`）的事。于是这里每一行都能在进程内单测。

pub mod assertion;
pub mod clock;
pub mod policy;
pub mod probe;
pub mod supervisor;

pub use supervisor::Supervisor;
