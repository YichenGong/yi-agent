//! 生产宿主编排：把已验证的安装变成**能显示的窗口**，并在关闭/升级时收回。
//!
//! # 这一层负责什么
//!
//! 下面的模块各自只做一件纯事：
//! - `assets`：白名单与响应；
//! - `cache`：实例句柄与代次绑定；
//! - `container`：实例身份与拒绝分类；
//! - `recovery`：失败之后的结论。
//!
//! 这里把它们和 Tauri 接起来：解析实例根 → 读注册表与清单 → 校验安装 → 建
//! 窗口 → 注册协议 → 关闭时撤句柄。**没有这一步，四个模块就只是能通过单测
//! 的库**，页面永远打不开。
//!
//! # 三条不可让步的规则
//!
//! 1. **资源请求不认识路径。** 协议处理器只从 URL 里取"实例 id + 相对路径"，
//!    目录由宿主持有。页面无法拼出一个读 `~/.ssh` 的请求。
//! 2. **代次在打开时确定。** 打开时读 `Lifecycle::committed`，句柄绑死那个
//!    代次；之后每次请求再比对。升级之后旧窗口立刻失效，而不是"继续用旧文件
//!    直到用户关掉它"。
//! 3. **窗口 label 由宿主生成。** 不接受页面/调用方指定的 label，否则插件能
//!    把自己叫成 `main`，撞上主窗口的 capability。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::Serialize;
use yi_agent_plugins::{
    InstalledRelease, Lifecycle, Manifest, PluginError, PluginId, Registry, ReleaseId,
};

use crate::plugin_host::assets::{AssetResponse, ASSET_SCHEME};
use crate::plugin_host::cache::{OpenRequest, Rejection, ViewRegistry};
use crate::plugin_host::container::{ContainerConfig, ViewInstance};

/// 受管清单的文件名。与 `yi-agent-supervisors::managed::MANAGED_MANIFEST`
/// 同值；桌面壳不依赖那个 crate（它会带进 supervisor 的运行时假设），
/// 两侧一致性由契约测试钉住。
pub const MANAGED_MANIFEST: &str = "plugin.json";

/// 插件窗口 label 的前缀。capability 明确不覆盖这个前缀，见
/// `tests/app_command_capability.rs`。
pub const WINDOW_PREFIX: &str = "plugin-";

/// 打开的视图对外暴露的信息。**不含安装根**：那是宿主的内部状态，
/// 出现在线协议里就等于把本机目录结构交出去。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenedView {
    pub instance_id: String,
    pub window_label: String,
    pub entry: String,
    pub activation: u64,
}

/// 宿主侧的状态：注册表 + 当前实例根。
pub struct PluginHost {
    home: PathBuf,
    views: Mutex<ViewRegistry>,
    /// 窗口 label → 实例 id。关闭窗口时反向查找。
    labels: Mutex<BTreeMap<String, String>>,
}

impl PluginHost {
    pub fn new(home: PathBuf) -> PluginHost {
        PluginHost {
            home,
            views: Mutex::new(ViewRegistry::new()),
            labels: Mutex::new(BTreeMap::new()),
        }
    }

    /// 用进程实例根构造。拿不到 home 时返回错误而不是退回当前目录：
    /// 退回 `cwd` 会在 Finder 启动时把插件装到 `/` 旁边。
    pub fn from_env() -> Result<PluginHost, PluginError> {
        match crate::bridge::instance_home() {
            Some(home) => Ok(PluginHost::new(home)),
            None => Err(PluginError::Io(
                "the process has no instance home; set YI_AGENT_HOME".to_string(),
            )),
        }
    }

    pub fn home(&self) -> &std::path::Path {
        &self.home
    }

    /// 当前已提交的激活代次。`None` 表示这个插件没有可用版本。
    pub fn committed(
        &self,
        plugin: &PluginId,
    ) -> Result<Option<CommittedActivationInfo>, PluginError> {
        let lifecycle = Lifecycle::open(&self.home)?;
        Ok(lifecycle
            .committed(plugin)?
            .map(|active| CommittedActivationInfo {
                release: active.release,
                activation: active.activation,
            }))
    }

    /// 打开一个已安装版本的视图。
    ///
    /// 顺序是刻意的：先确认"注册表里选中/已安装"→ 校验安装完整 → 读清单 →
    /// 确认与已提交激活一致 → 建句柄。任何一步失败都不留下半成品。
    pub fn open(
        &self,
        plugin: &PluginId,
        version: &str,
        view: &str,
        instance_id: String,
    ) -> Result<OpenedView, PluginError> {
        let record = Registry::open(&self.home)?
            .list()?
            .into_iter()
            .find(|record| &record.plugin == plugin)
            .ok_or_else(|| {
                PluginError::NotFound(format!("plugin `{}` is not registered", plugin.as_str()))
            })?;
        let installed = record
            .installed
            .iter()
            .find(|installed| installed.version == version)
            .ok_or_else(|| {
                PluginError::NotFound(format!(
                    "plugin `{}` has no installed version `{version}`",
                    plugin.as_str()
                ))
            })?;

        let destination = self.home.join(&installed.directory);
        // 每次打开都重新核对磁盘字节。安装时的签名只证明"那一刻"，
        // 之后被替换的文件没人再看，而我们要拿它去渲染一个窗口。
        yi_agent_plugins::verify_installed(&destination)?;

        let manifest_bytes = std::fs::read(destination.join(MANAGED_MANIFEST))
            .map_err(|e| PluginError::Integrity(format!("manifest: {e}")))?;
        let manifest = Manifest::parse(&manifest_bytes)?;

        let release = ReleaseId {
            plugin: plugin.clone(),
            version: installed.version.clone(),
            target: manifest.target.clone(),
            sha256: installed.sha256.clone(),
        };

        let lifecycle = Lifecycle::open(&self.home)?;
        let committed = lifecycle.committed(plugin)?;
        let activation = match &committed {
            Some(active) if active.release == release => active.activation,
            Some(active) => {
                return Err(PluginError::Conflict(format!(
                    "plugin `{}` version `{version}` is installed but not the committed \
                     activation (committed: {} @ {})",
                    plugin.as_str(),
                    active.release.version,
                    active.activation
                )))
            }
            None => {
                return Err(PluginError::Conflict(format!(
                    "plugin `{}` has no committed activation; activate it before opening a view",
                    plugin.as_str()
                )))
            }
        };

        let installation = InstalledRelease {
            destination,
            files_written: 0,
            total_bytes: 0,
            program_entry: manifest
                .program
                .as_ref()
                .map(|program| program.entry.clone()),
        };
        let window_label = format!("{WINDOW_PREFIX}{instance_id}");

        let mut views = self.views.lock().unwrap();
        let opened = views.open(OpenRequest {
            instance_id: instance_id.clone(),
            release: &release,
            installation: &installation,
            manifest: &manifest,
            activation,
            view,
        })?;
        let result = OpenedView {
            instance_id: instance_id.clone(),
            window_label: window_label.clone(),
            entry: opened.entry.clone(),
            activation,
        };
        drop(views);
        self.labels
            .lock()
            .unwrap()
            .insert(window_label, instance_id);
        Ok(result)
    }

    /// 处理一次资源请求。`uri` 形如
    /// `plugin-asset://localhost/<instance>/<relative path>`。
    ///
    /// 先把"这个句柄属于哪个插件、绑的是哪个代次"取出来，**释放锁**再去读
    /// journal：journal 是文件 IO，占着全局锁会让一个慢磁盘拖住所有窗口。
    pub fn serve(&self, uri: &str) -> AssetResponse {
        let Some((instance, relative)) = split_asset_uri(uri) else {
            return AssetResponse::text(400, "bad asset request");
        };
        let Some((plugin, bound)) = self
            .views
            .lock()
            .unwrap()
            .get(&instance)
            .map(|view| (view.plugin.clone(), view.activation))
        else {
            return AssetResponse::text(403, "forbidden: unknown view instance");
        };
        let committed = Lifecycle::open(&self.home)
            .and_then(|lifecycle| lifecycle.committed(&plugin))
            .ok()
            .flatten()
            .map(|active| yi_agent_plugins::CommittedActivation {
                release: active.release,
                activation: active.activation,
            });
        let _ = bound;

        let views = self.views.lock().unwrap();
        match views.resolve(&instance, &relative, committed.as_ref()) {
            Ok(body) => AssetResponse {
                status: 200,
                content_type: crate::plugin_host::assets::content_type_for(&relative),
                body,
            },
            Err(Rejection::View(rejection)) => {
                AssetResponse::text(403, &format!("forbidden: {rejection}"))
            }
            Err(Rejection::Path(detail)) => AssetResponse::text(404, &detail),
        }
    }

    /// 关闭一个窗口对应的实例。重复调用是幂等的。
    pub fn close(&self, instance_id: &str) -> bool {
        let mut labels = self.labels.lock().unwrap();
        labels.retain(|_, value| value != instance_id);
        drop(labels);
        self.views.lock().unwrap().close(instance_id)
    }

    /// 按窗口 label 关闭（Tauri 的 `WindowEvent::Destroyed` 只给 label）。
    pub fn close_window(&self, label: &str) -> bool {
        let instance = self.labels.lock().unwrap().remove(label);
        match instance {
            Some(instance) => self.views.lock().unwrap().close(&instance),
            None => false,
        }
    }

    pub fn open_count(&self) -> usize {
        self.views.lock().unwrap().len()
    }

    pub fn instance(&self, instance_id: &str) -> Option<ViewInstance> {
        self.views.lock().unwrap().get(instance_id).cloned()
    }
}

/// 已提交激活的线协议形状。与 `yi_agent_plugins::CommittedActivation` 同义，
/// 但那个类型属于核心 crate，不该直接跑到前端类型里。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommittedActivationInfo {
    pub release: ReleaseId,
    pub activation: u64,
}

/// 把 `plugin-asset://localhost/<instance>/<path>` 拆成 (实例, 相对路径)。
///
/// 只认这一种形状：authority 必须是 `localhost`，第一段是实例 id。**不**做
/// "猜一下"的容错——容错在安全边界上就是扩大攻击面。
pub fn split_asset_uri(uri: &str) -> Option<(String, String)> {
    let (scheme, rest) = uri.split_once("://")?;
    if scheme != ASSET_SCHEME {
        return None;
    }
    let rest = rest.split('?').next().unwrap_or(rest);
    let (authority, path) = rest.split_once('/')?;
    if authority != "localhost" {
        return None;
    }
    let (instance, relative) = path.split_once('/')?;
    if instance.is_empty() || relative.is_empty() {
        return None;
    }
    Some((instance.to_string(), relative.to_string()))
}

/// 一个视图窗口的 URL。实例 id 在 authority 之后的第一段，资源处理器靠它
/// 找回句柄；路径部分是视图入口。
pub fn view_url(instance_id: &str, entry: &str) -> String {
    format!("{ASSET_SCHEME}://localhost/{instance_id}/{entry}")
}

/// 平台窗口大小。与主窗口的尺寸策略一致：插件视图是主窗口里的一块，
/// 但独立窗口用于"放大查看"，因此给一个合理初值而不是全屏。
pub const VIEW_WIDTH: f64 = 1024.0;
pub const VIEW_HEIGHT: f64 = 760.0;

/// 插件视图的 CSP。与 [`ContainerConfig::strict_csp`] 同源，额外把
/// `frame-ancestors` 放开到本协议：视图要能被主窗口以 iframe 嵌入。
pub fn view_csp() -> String {
    ContainerConfig::default().csp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_uris_must_name_an_instance() {
        assert_eq!(
            split_asset_uri("plugin-asset://localhost/view-1/ui/index.html"),
            Some(("view-1".to_string(), "ui/index.html".to_string()))
        );
        assert!(split_asset_uri("plugin-asset://example.com/view-1/ui/index.html").is_none());
        assert!(split_asset_uri("https://localhost/view-1/ui/index.html").is_none());
        assert!(split_asset_uri("plugin-asset://localhost/view-1").is_none());
        assert!(split_asset_uri("plugin-asset://localhost//ui/index.html").is_none());
    }

    #[test]
    fn the_query_string_is_not_part_of_the_path() {
        assert_eq!(
            split_asset_uri("plugin-asset://localhost/v/ui/app.js?v=2"),
            Some(("v".to_string(), "ui/app.js".to_string()))
        );
    }

    #[test]
    fn view_urls_round_trip_through_the_splitter() {
        let url = view_url("v-1", "ui/index.html");
        assert_eq!(
            split_asset_uri(&url),
            Some(("v-1".to_string(), "ui/index.html".to_string()))
        );
    }

    #[test]
    fn the_view_csp_is_the_container_policy() {
        assert_eq!(view_csp(), ContainerConfig::default().csp);
        assert!(view_csp().contains("frame-ancestors 'none'"));
    }
}

// ---------------------------------------------------------------------------
// Tauri 接线
// ---------------------------------------------------------------------------

/// 打开一个插件视图，返回实例 id 与窗口 label。
///
/// 窗口在这里创建而不是交给前端：前端若自己建窗口，就能挑 label，而 label
/// 决定 capability。宿主生成 label 是"插件窗口拿不到主窗口权限"的机械保证。
#[tauri::command]
pub fn open_plugin_view(
    app: tauri::AppHandle,
    host: tauri::State<'_, std::sync::Arc<PluginHost>>,
    plugin: String,
    version: String,
    view: String,
) -> Result<OpenedView, String> {
    let plugin = PluginId::parse(&plugin).map_err(|e| e.to_string())?;
    let instance_id = format!("v{}", uuid::Uuid::new_v4().simple());
    let opened = host
        .open(&plugin, &version, &view, instance_id.clone())
        .map_err(|e| e.to_string())?;

    let url = view_url(&instance_id, &opened.entry)
        .parse()
        .map_err(|e| format!("bad view url: {e}"))?;
    let config = tauri::WebviewWindowBuilder::new(
        &app,
        &opened.window_label,
        tauri::WebviewUrl::External(url),
    )
    .title(format!("{} — {}", plugin.as_str(), view))
    .inner_size(VIEW_WIDTH, VIEW_HEIGHT)
    .on_navigation(|url| {
        // 只允许仍是本实例的文档：顶层跳转由宿主拦，不依赖 WebKit 对
        // CSP `navigate-to` 的不完整支持。
        url.scheme() == ASSET_SCHEME && url.host_str() == Some("localhost")
    });

    if let Err(error) = config.build() {
        // 窗口没建起来就不能留着句柄：否则实例表里会有一个永远没有消费者的
        // entry，它的白名单也一直占着内存。
        host.close(&instance_id);
        return Err(format!("could not create the view window: {error}"));
    }
    Ok(opened)
}

/// 关闭一个视图。窗口销毁时宿主也会自动调用同一路径。
#[tauri::command]
pub fn close_plugin_view(
    app: tauri::AppHandle,
    host: tauri::State<'_, std::sync::Arc<PluginHost>>,
    instance_id: String,
) -> Result<bool, String> {
    use tauri::Manager;

    let window = host
        .instance(&instance_id)
        .map(|_| format!("{WINDOW_PREFIX}{instance_id}"));
    let closed = host.close(&instance_id);
    if let Some(label) = window {
        if let Some(window) = app.get_webview_window(&label) {
            let _ = window.close();
        }
    }
    Ok(closed)
}

/// 查询一个插件当前的已提交激活。前端据此决定"有没有版本可打开"。
#[tauri::command]
pub fn plugin_committed_activation(
    host: tauri::State<'_, std::sync::Arc<PluginHost>>,
    plugin: String,
) -> Result<Option<CommittedActivationInfo>, String> {
    let plugin = PluginId::parse(&plugin).map_err(|e| e.to_string())?;
    host.committed(&plugin).map_err(|e| e.to_string())
}

/// 为插件协议注册处理器。
///
/// 注册在 `Builder` 上而不是某个窗口上：一个进程只有一个资源服务，所有视图
/// 共用它，靠 URL 里的实例 id 分派。
pub fn register_protocol<R: tauri::Runtime>(
    builder: tauri::Builder<R>,
    host: std::sync::Arc<PluginHost>,
) -> tauri::Builder<R> {
    builder.register_uri_scheme_protocol(ASSET_SCHEME, move |_ctx, request| {
        let response = host.serve(&request.uri().to_string());
        tauri::http::Response::builder()
            .status(response.status)
            .header(tauri::http::header::CONTENT_TYPE, response.content_type)
            // 页面 CSP 由宿主写：即使 iframe 的 srcdoc 想放宽也没有机会。
            .header("Content-Security-Policy", view_csp())
            .header("X-Content-Type-Options", "nosniff")
            .header("Referrer-Policy", "no-referrer")
            .header("Cross-Origin-Resource-Policy", "same-origin")
            .body(response.body)
            .unwrap_or_else(|_| tauri::http::Response::new(Vec::new()))
    })
}
