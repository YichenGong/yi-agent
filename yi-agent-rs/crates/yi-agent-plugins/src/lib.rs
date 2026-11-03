//! 插件契约核心。
//!
//! 这一层只做三件事：定义值类型、解析并校验发布清单、给出错误分类。
//! 它**不**读写实例状态、**不**启动进程、**不**依赖任何运行时——
//! 路径与时钟都由调用方注入，这样同一份代码可以被宿主、桌面壳和
//! 测试各自复用，而不会把某个运行时的假设固化进来。

pub mod error;
pub mod manifest;
pub mod types;

pub use error::PluginError;
pub use manifest::{Manifest, MethodDecl, ViewDecl};
pub use types::{Binding, Effect, Operation, PluginId, ProjectId, ReleaseId};
