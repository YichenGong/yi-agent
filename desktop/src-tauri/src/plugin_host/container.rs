//! 视图实例：一个具体的 (插件, 发布, 代次, 视图) 组合的身份。
//!
//! # 为什么需要"实例 id"而不仅仅是"包路径"
//!
//! 一个插件可能同时存在多个代次：已提交的旧版本还在服务，候选的新版本正在
//! 健康检查。两者**文件不同、权限不同、生命周期不同**。如果资源 API 只认
//! "插件名 + 视图名"，那么候选版本的视图能读到已提交版本的文件——这不是
//! 崩溃，是跨代次越权。
//!
//! 所以资源句柄里必须带上代次与发布摘要。任何字段对不上，就是陈旧句柄，
//! 拒绝而不是"尽力而为"。
//!
//! # 它不代表授权
//!
//! 打开一个视图**不**等于授予这个视图任何业务能力。业务能力由 `Binding`
//! 与能力表决定，二者正交：视图句柄只回答"这个窗口该读哪个目录"。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::plugin_host::assets::{sha256_hex, ASSET_SCHEME, CASE_DEADLINE, MAX_FRAMES};

use yi_agent_plugins::{PluginId, ReleaseId};

/// 一个已打开的视图实例。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewInstance {
    /// 窗口/视图的实例 id。每次打开重新生成，关闭即失效。
    pub instance_id: String,
    pub plugin: PluginId,
    /// 完整的发布身份。**不含**只是"版本号"：同版本不同摘要的包是另一个
    /// 东西，用版本号当身份会让它们混为一谈。
    pub release: ReleaseId,
    /// 激活代次。升级/回滚之后旧代次的句柄必须失效。
    pub activation: u64,
    /// 视图 id（清单里的 `views[].id`）。
    pub view: String,
    /// 视图入口的包内相对路径。
    pub entry: String,
    /// 安装根。仅供宿主内部读文件，**不**出现在任何线协议里。
    pub(crate) root: PathBuf,
}

impl ViewInstance {
    /// 这个实例是否仍是"当前"的。
    ///
    /// 判据只有一条：磁盘上已提交的激活是否仍是打开时那一个。会话不知道自己
    /// 被升级过，是宿主必须告诉它的事。
    pub fn is_current(&self, committed: Option<&yi_agent_plugins::CommittedActivation>) -> bool {
        match committed {
            Some(active) => active.activation == self.activation && active.release == self.release,
            None => false,
        }
    }
}

/// 一个被拒绝的视图请求。分类是为了让上层能给出可动作的提示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewRejection {
    /// 实例 id 不存在（已关闭，或从未打开）。
    UnknownInstance,
    /// 句柄指向的代次已不是当前激活。
    StaleGeneration { bound: u64, current: Option<u64> },
    /// 清单里没有这个视图。
    UnknownView,
    /// 安装本身有问题（文件被替换、入口不见了）。
    BrokenInstall(String),
}

impl std::fmt::Display for ViewRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ViewRejection::UnknownInstance => {
                write!(f, "this view instance is not open (it was closed or never opened)")
            }
            ViewRejection::StaleGeneration { bound, current } => match current {
                Some(current) => write!(
                    f,
                    "this view is bound to activation {bound}, but activation {current} is committed; \
                     reopen the view"
                ),
                None => write!(
                    f,
                    "this view is bound to activation {bound}, but the plugin has no committed \
                     activation any more; reinstall it"
                ),
            },
            ViewRejection::UnknownView => write!(f, "the release does not declare this view"),
            ViewRejection::BrokenInstall(detail) => {
                write!(f, "the installation is not usable: {detail}")
            }
        }
    }
}

/// 原生侧容器的创建参数。探针与后续正式容器共用同一份描述，
/// 这样报告里的 `containerConfigSha256` 才有意义。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContainerConfig {
    pub scheme: String,
    /// 页面 CSP。覆盖脚本/样式/图片/帧/表单等资源类型，而不只是 connect-src。
    pub csp: String,
    /// 是否允许页面自定义协议之外的网络请求（探针要求 false）。
    pub allow_remote_network: bool,
    pub max_frames: usize,
    pub case_deadline_secs: u64,
    /// 原生侧恢复策略：超时后关闭并重建容器。
    pub recovery: RecoveryPolicy,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPolicy {
    CloseAndRebuild,
}

impl Default for ContainerConfig {
    fn default() -> Self {
        Self {
            scheme: ASSET_SCHEME.to_string(),
            csp: Self::strict_csp(),
            allow_remote_network: false,
            max_frames: MAX_FRAMES,
            case_deadline_secs: CASE_DEADLINE.as_secs(),
            recovery: RecoveryPolicy::CloseAndRebuild,
        }
    }
}

impl ContainerConfig {
    /// 严格的页面 CSP：默认全部拒绝，只放行本包资源与非内联的必要脚本样式。
    ///
    /// `form-action` / `frame-src` / `img-src` / `style-src` / `font-src` 都必须
    /// 显式设为 `'none'` 或自身来源：只写 `connect-src` 挡不住图片/表单外送
    /// （spec §4.4）。
    pub fn strict_csp() -> String {
        "default-src 'none'; \
         script-src 'self' 'unsafe-inline'; \
         style-src 'self' 'unsafe-inline'; \
         img-src 'self'; \
         font-src 'self'; \
         connect-src 'self'; \
         frame-src 'none'; \
         child-src 'none'; \
         worker-src 'none'; \
         form-action 'none'; \
         base-uri 'none'; \
         object-src 'none'; \
         navigate-to 'none'; \
         manifest-src 'none'; \
         media-src 'none'; \
         frame-ancestors 'none'"
            .to_string()
    }

    pub fn digest(&self) -> String {
        sha256_hex(&serde_json::to_vec(self).unwrap_or_default())
    }
}
