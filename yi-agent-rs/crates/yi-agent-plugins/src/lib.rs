//! 插件契约核心。
//!
//! 这一层只做三件事：定义值类型、解析并校验发布清单、给出错误分类。
//! 它**不**读写实例状态、**不**启动进程、**不**依赖任何运行时——
//! 路径与时钟都由调用方注入，这样同一份代码可以被宿主、桌面壳和
//! 测试各自复用，而不会把某个运行时的假设固化进来。

pub mod admission;
pub mod bundle;
pub mod error;
pub mod grants;
pub mod journal;
pub mod lifecycle;
pub mod limits;
pub mod locks;
pub mod manifest;
pub mod migration;
pub mod operations;
pub mod project;
pub mod registry;
pub mod trust;
pub mod types;

pub use admission::{Admission, Phase, TurnPermit};
pub use bundle::{InstalledRelease, install_verified, verify_installed};
pub use error::PluginError;
pub use grants::{Grant, GrantTable};
pub use journal::{
    Blocking, Journal, JournalError, JournalErrorKind, JournalStore, JournalTarget,
    LifecycleAction, LifecyclePhase,
};
pub use lifecycle::{
    BackupSource, CommittedActivation, Lifecycle, LifecycleRequest, PendingTransaction, Progress,
    RuntimeControl,
};
pub use limits::ExtractLimits;
pub use manifest::{Manifest, MethodDecl, ViewDecl};
pub use migration::{Backup, BackupEntry, BackupManifest};
pub use operations::{OperationLedger, OperationOutcome};
pub use project::{ProjectResolver, ProjectRoot};
pub use registry::{GrantSummary, InstalledVersion, PluginRecord, Registry};
pub use trust::{TrustStore, VerifiedRelease};
pub use types::{Binding, Effect, Operation, PluginId, ProjectId, ReleaseId};
