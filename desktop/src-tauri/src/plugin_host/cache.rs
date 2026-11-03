//! 视图实例注册表：`instance_id → ViewInstance + AssetCache`。
//!
//! # 这一个类型存在的理由
//!
//! 资源 API 的输入来自 WebView（也就是来自插件自己）。如果它接受的是
//! "插件名 + 路径"，那么一个被攻破的视图只要改一个字符串就能读别的包；
//! 如果它接受的是"绝对路径"，那就等于把整个磁盘交出去。所以线上协议的输入
//! 只能是**宿主自己发出去的实例句柄**，句柄→目录的映射只活在宿主进程里。
//!
//! # 打开时做完全部判断
//!
//! `open` 之后实例的目录、入口、代次都固定下来，请求阶段只剩查表。这样一来
//! 插件在运行期无法通过改清单、换符号链接、重命名目录来扩大自己的可见范围：
//! 白名单是打开那一刻的快照。
//!
//! # 关闭即失效
//!
//! 窗口关闭时 `close` 必须被调用。它就是资源的撤销：句柄不在表里，任何请求
//! 都是 [`ViewRejection::UnknownInstance`]，而不是回退到"再打开一次"。

use std::collections::BTreeMap;

use yi_agent_plugins::{CommittedActivation, InstalledRelease, Manifest, PluginError, ReleaseId};

use crate::plugin_host::assets::AssetCache;
use crate::plugin_host::container::{ViewInstance, ViewRejection};
use crate::plugin_host::remote_assets::HostIdentity;

/// 打开视图时需要的、已经过验证的输入。
pub struct OpenRequest<'a> {
    pub instance_id: String,
    /// 完整的发布身份。由调用方从验签过的 `VerifiedRelease` 带下来，**不**
    /// 从清单或目录名反推：同版本不同摘要的包是另一个发布。
    pub release: &'a ReleaseId,
    pub installation: &'a InstalledRelease,
    pub manifest: &'a Manifest,
    pub activation: u64,
    pub view: &'a str,
    /// 这次打开属于哪个宿主。桌面客户端可能同时连着本机与远端，句柄必须
    /// 记住自己在哪一边，否则切到 B 之后 A 的句柄仍然能读 A 的文件。
    pub host: &'a HostIdentity,
}

/// 所有已打开视图的宿主侧注册表。
#[derive(Default)]
pub struct ViewRegistry {
    views: BTreeMap<String, OpenView>,
}

struct OpenView {
    instance: ViewInstance,
    assets: AssetCache,
    /// 打开时的宿主身份快照。与 `activation` 一起构成完整的绑定。
    host: HostIdentity,
}

impl ViewRegistry {
    pub fn new() -> ViewRegistry {
        ViewRegistry::default()
    }

    /// 打开一个视图。
    ///
    /// 失败时不留半成品：`AssetCache::load` 成功之后才插入注册表，所以一次
    /// 失败的打开不会让句柄变成"存在但不可用"。
    pub fn open(&mut self, request: OpenRequest<'_>) -> Result<&ViewInstance, PluginError> {
        if self.views.contains_key(&request.instance_id) {
            return Err(PluginError::InvalidManifest(format!(
                "view instance `{}` is already open",
                request.instance_id
            )));
        }
        let assets = AssetCache::load(request.installation, request.manifest)?;
        let entry = assets.view_entry(request.view).ok_or_else(|| {
            PluginError::InvalidManifest(format!(
                "the release does not declare view `{}`",
                request.view
            ))
        })?;
        let instance = ViewInstance {
            instance_id: request.instance_id.clone(),
            plugin: request.manifest.plugin.clone(),
            release: request.release.clone(),
            activation: request.activation,
            view: request.view.to_string(),
            entry: entry.to_string(),
            root: assets.root().to_path_buf(),
        };
        let host = request.host.clone();
        let key = request.instance_id;
        self.views.insert(
            key.clone(),
            OpenView {
                instance,
                assets,
                host,
            },
        );
        Ok(&self.views[&key].instance)
    }

    /// 按句柄读一个资源。
    ///
    /// 四道闸门，顺序固定：
    /// 1. 句柄存在（关闭/从未打开都拒绝）；
    /// 2. 宿主仍是当前宿主（切到别的连接之后旧句柄失效）；
    /// 3. 代次仍是当前激活（升级/回滚之后旧句柄失效）；
    /// 4. 路径在白名单里（包外、跨包、`..` 一律拒绝）。
    ///
    /// `host` 为 `None` 表示宿主还没握手完成。此时按"当前宿主未知"处理：
    /// 只有本地宿主能读，远端句柄一律拒绝——"不确定"绝不能退化成"放行"，
    /// 否则连上第二个连接的那一瞬间就是一个混淆窗口。
    pub fn resolve(
        &self,
        instance_id: &str,
        relative: &str,
        committed: Option<&CommittedActivation>,
        host: Option<&HostIdentity>,
    ) -> Result<Vec<u8>, Rejection> {
        let open = self
            .views
            .get(instance_id)
            .ok_or(Rejection::View(ViewRejection::UnknownInstance))?;
        let current_host = match host {
            Some(host) => host.clone(),
            None if !open.host.is_remote() => open.host.clone(),
            None => {
                return Err(Rejection::View(ViewRejection::ForeignHost {
                    bound: open.host.binding_key(&open.instance.release),
                    current: "<no host handshake>".to_string(),
                }))
            }
        };
        if open.host != current_host {
            return Err(Rejection::View(ViewRejection::ForeignHost {
                bound: open.host.binding_key(&open.instance.release),
                current: current_host.binding_key(&open.instance.release),
            }));
        }
        if !open.instance.is_current(committed) {
            return Err(Rejection::View(ViewRejection::StaleGeneration {
                bound: open.instance.activation,
                current: committed.map(|active| active.activation),
            }));
        }
        let Some(path) = open.assets.resolve(relative) else {
            return Err(Rejection::Path(format!(
                "`{relative}` is not in this view's release"
            )));
        };
        std::fs::read(path).map_err(|e| Rejection::Path(format!("`{relative}`: {e}")))
    }

    pub fn get(&self, instance_id: &str) -> Option<&ViewInstance> {
        self.views.get(instance_id).map(|open| &open.instance)
    }

    pub fn close(&mut self, instance_id: &str) -> bool {
        self.views.remove(instance_id).is_some()
    }

    /// 撤销某个宿主上的全部视图，返回撤销数量。
    ///
    /// 连接切换、断开、旧客户端不兼容都走这里。返回计数而不是 `()`，是为了
    /// 让调用方能断言"确实撤掉了几个"，而不是靠"没有报错"来判断成功。
    pub fn revoke_host(&mut self, host: &HostIdentity) -> usize {
        let doomed: Vec<String> = self
            .views
            .iter()
            .filter(|(_, open)| open.host == *host)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &doomed {
            self.views.remove(id);
        }
        doomed.len()
    }

    /// 按宿主类别撤销（断开连接时只知道 profile，不知道当时的代次）。
    pub fn revoke_kind(&mut self, kind: &str) -> usize {
        let doomed: Vec<String> = self
            .views
            .iter()
            .filter(|(_, open)| open.host.kind == kind)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &doomed {
            self.views.remove(id);
        }
        doomed.len()
    }

    /// 这个实例当前绑定在哪个宿主上。仅供宿主内部与测试使用。
    pub fn host_of(&self, instance_id: &str) -> Option<&HostIdentity> {
        self.views.get(instance_id).map(|open| &open.host)
    }

    pub fn len(&self) -> usize {
        self.views.len()
    }

    pub fn is_empty(&self) -> bool {
        self.views.is_empty()
    }
}

/// 资源请求被拒的两类原因。分开是因为上层处理不同：视图问题要关窗口，
/// 路径问题是页面自己的 bug。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    View(ViewRejection),
    Path(String),
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Rejection::View(inner) => inner.fmt(f),
            Rejection::Path(detail) => write!(f, "resource request denied: {detail}"),
        }
    }
}
