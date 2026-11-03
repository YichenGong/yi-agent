//! 插件核心的错误分类。
//!
//! 线协议只暴露稳定的 `kind` 字符串，不把 Rust 的 debug 输出去当协议——
//! 后者会在一次重构后悄悄改变客户端的行为。

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    /// 清单不符合契约：字段非法、未知字段、语义违规。
    #[error("invalid manifest: {0}")]
    InvalidManifest(String),
    /// 摘要/签名与内容不符。
    #[error("integrity failure: {0}")]
    Integrity(String),
    /// 签名或信任根不被接受。
    #[error("untrusted: {0}")]
    Untrusted(String),
    /// 已过期或尚未生效。
    #[error("expired: {0}")]
    Expired(String),
    /// 已被撤回或序号回退。
    #[error("revoked: {0}")]
    Revoked(String),
    /// 平台或 API 版本不兼容。
    #[error("incompatible: {0}")]
    Incompatible(String),
    /// 能力/方法未实现或宿主不支持。
    #[error("not supported: {0}")]
    NotSupported(String),
    /// 授权不足。
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    /// 绑定属于旧版本代次。
    #[error("stale binding: {0}")]
    StaleBinding(String),
    /// 依赖缺失或版本不匹配。
    #[error("dependency: {0}")]
    Dependency(String),
    /// 与已有状态冲突（如同一版本不同摘要）。
    #[error("conflict: {0}")]
    Conflict(String),
    /// 正在排空，拒绝新工作。
    #[error("draining: {0}")]
    Draining(String),
    /// 资源被占用。
    #[error("busy: {0}")]
    Busy(String),
    /// 写操作已发出但结果未知。
    #[error("result unknown: {0}")]
    ResultUnknown(String),
    /// 无法安全回退，需人工恢复。
    #[error("recovery required: {0}")]
    RecoveryRequired(String),
    /// 落盘/读取失败。
    #[error("io: {0}")]
    Io(String),
}

impl PluginError {
    /// 线协议里的稳定分类字符串。客户端按它分支，所以改名等于破坏协议。
    pub fn kind(&self) -> &'static str {
        match self {
            PluginError::InvalidManifest(_) => "InvalidManifest",
            PluginError::Integrity(_) => "Integrity",
            PluginError::Untrusted(_) => "Untrusted",
            PluginError::Expired(_) => "Expired",
            PluginError::Revoked(_) => "Revoked",
            PluginError::Incompatible(_) => "Incompatible",
            PluginError::NotSupported(_) => "NotSupported",
            PluginError::PermissionDenied(_) => "PermissionDenied",
            PluginError::StaleBinding(_) => "StaleBinding",
            PluginError::Dependency(_) => "Dependency",
            PluginError::Conflict(_) => "Conflict",
            PluginError::Draining(_) => "Draining",
            PluginError::Busy(_) => "Busy",
            PluginError::ResultUnknown(_) => "ResultUnknown",
            PluginError::RecoveryRequired(_) => "RecoveryRequired",
            PluginError::Io(_) => "Io",
        }
    }
}

impl From<std::io::Error> for PluginError {
    fn from(error: std::io::Error) -> Self {
        PluginError::Io(error.to_string())
    }
}
