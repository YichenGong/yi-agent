//! 远程 SSH 执行：配置、命令行拼装、安装。
//!
//! 依据 `docs/superpowers/specs/2026-10-31-remote-ssh-execution-design.md`。

pub mod arch;
pub mod argv;
pub mod cache;
pub mod config;
pub mod fetch;
pub mod install;
pub mod probe;
pub mod profile;

pub use install::{run_remote, sh_quote};
