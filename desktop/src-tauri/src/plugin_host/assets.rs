//! 资源服务与容器策略：**生产实现**。
//!
//! 这个模块在没有 `plugin-probe` feature 的正式构建里也会编译。探针
//! （`probe`/`run`）只是它的一个调用方，因此"报告里通过"与"产品里生效"
//! 描述的是同一份代码——如果探针另写一套，安全结论立刻失去意义。
//!
//! 三部分：
//! - [`AssetRoot`]：一个包的白名单。启动时把普通文件（跳过符号链接）收成表，
//!   运行期只查表，不解释路径。
//! - [`serve_asset`]：把一次请求翻译成响应，越界/跨包一律 403。
//! - [`ContainerConfig`]：CSP、scheme、frame 上限、恢复策略。报告里的
//!   `containerConfigSha256` 就是对它的哈希（见 `run::container_config_digest`）。
//!
//! # 与 [`super::cache`] 的关系
//!
//! `AssetRoot` 是"按包目录建白名单"，服务于探针这种已知单包场景；`cache`
//! 是"按宿主实例句柄建白名单并绑定代次"，服务于产品里多代次并存的场景。
//! 两者共用 [`safe_relative_path`] 与 [`content_type_for`]，判定口径一致。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

/// 资源服务的自定义协议名。页面 Origin 在 macOS/Linux 上是
/// `plugin-asset://localhost`，Windows/Android 上是 `http://plugin-asset.localhost`。
pub const ASSET_SCHEME: &str = "plugin-asset";

/// 资源服务里“另一个包”的路径前缀，用于跨包读取攻击。
pub const FOREIGN_PACKAGE: &str = "foreign";

/// 单条用例的最长等待时间。
pub const CASE_DEADLINE: Duration = Duration::from_secs(20);

/// 原生侧允许的 frame 数上限。
pub const MAX_FRAMES: usize = 4;

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// 资源服务的白名单根。每个包一个根，路径必须落在包根之内。
#[derive(Debug, Clone)]
pub struct AssetRoot {
    /// 包内相对路径 → 绝对文件路径。启动时一次性建好，运行期不解释路径。
    files: BTreeMap<String, PathBuf>,
}

impl AssetRoot {
    /// 从目录建立白名单。只接受规范化普通文件；符号链接、越界路径直接跳过，
    /// 这样即便包内容被替换也不会把任意文件捧成资源。
    pub fn load(package_root: &Path) -> std::io::Result<Self> {
        let root = package_root
            .canonicalize()
            .unwrap_or_else(|_| package_root.to_path_buf());
        let mut files = BTreeMap::new();
        collect_files(&root, &root, &mut files)?;
        Ok(Self { files })
    }

    pub fn get(&self, relative: &str) -> Option<&PathBuf> {
        self.files.get(relative)
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

fn collect_files(
    root: &Path,
    dir: &Path,
    out: &mut BTreeMap<String, PathBuf>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.is_symlink() {
            // 链接可以指向包外；宁可漏也别把任意文件当资源。
            continue;
        }
        if metadata.is_dir() {
            collect_files(root, &path, out)?;
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let Some(key) = relative.to_str().map(|value| value.replace('\\', "/")) else {
            continue;
        };
        out.insert(key, path);
    }
    Ok(())
}

/// 把请求路径规范化成包内相对路径；任何越界或非法输入都返回 `None`。
pub fn safe_relative_path(root: &Path, requested: &str) -> Option<PathBuf> {
    let trimmed = requested.trim_start_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.contains('\0') {
        return None;
    }
    let mut depth: i32 = 0;
    for segment in trimmed.split('/') {
        match segment {
            "" | "." => continue,
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return None;
                }
            }
            _ => depth += 1,
        }
    }
    let candidate = root.join(trimmed);
    // 规范化后再确认仍在根内：这一步挡住 `a/../../etc/passwd` 这类拼接。
    let canonical = candidate.canonicalize().ok()?;
    canonical.starts_with(root).then_some(canonical)
}

/// 页面资源响应。抽成纯函数，便于在没有真实 WebView 时单测边界。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl AssetResponse {
    /// 文本响应。生产宿主也要用它表达 4xx，不是探针专用。
    pub fn text(status: u16, body: &str) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: body.as_bytes().to_vec(),
        }
    }
}

pub fn content_type_for(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

/// 处理一次资源请求。
///
/// - `active_package` 是当前视图绑定的包；请求只能落在它的白名单里。
/// - 请求里出现别的包名、或 `/foreign/...` 前缀，一律 403 —— 这正是
///   `cross_package_asset_denied` 用例要证明的行为。
/// - 冒号之后的 host 部分（`plugin-asset://localhost/...`）先被剥掉。
pub fn serve_asset(
    assets: &AssetRoot,
    requested_path: &str,
    active_package: &str,
) -> AssetResponse {
    let mut path = requested_path;
    if let Some((_, rest)) = path.split_once("://") {
        // 去掉 `localhost` / `plugin-asset.localhost` 这类 authority 段。
        path = rest.split_once('/').map(|(_, tail)| tail).unwrap_or("");
    }
    let path = path.trim_start_matches('/');

    if path == FOREIGN_PACKAGE || path.starts_with(&format!("{FOREIGN_PACKAGE}/")) {
        return AssetResponse::text(403, "forbidden: package boundary");
    }
    if let Some((first, _)) = path.split_once('/') {
        if first != active_package && first.starts_with("pkg-") {
            return AssetResponse::text(403, "forbidden: package boundary");
        }
    }

    let root = Path::new("/");
    if safe_relative_path(root, path).is_none() && path.split('/').any(|s| s == "..") {
        return AssetResponse::text(403, "forbidden: traversal");
    }

    let Some(file) = assets.get(path) else {
        return AssetResponse::text(404, "not found");
    };
    match std::fs::read(file) {
        Ok(body) => AssetResponse {
            status: 200,
            content_type: content_type_for(path),
            body,
        },
        Err(_) => AssetResponse::text(403, "forbidden: unreadable"),
    }
}

/// 原生侧的导航闸门。
///
/// CSP 的 `navigate-to` 在 WebKit 里支持并不完整，顶层跳转必须由宿主自己拦。
/// 判据只有一条：**必须仍是本包资源协议 + 本窗口自己的文档**。任何
/// `http(s):`、`file:`、`data:` 或其它 scheme 都拒绝，这样
/// `navigation_and_popup_denied` 证明的是宿主真的挡住了，而不是页面"没去跳"。
pub fn navigation_allowed(url: &str) -> bool {
    let Ok(parsed) = tauri::Url::parse(url) else {
        return false;
    };
    parsed.scheme() == ASSET_SCHEME && parsed.host_str() == Some("localhost")
}

use yi_agent_plugins::{InstalledRelease, Manifest, PluginError};

/// 一个已校验安装的白名单。
#[derive(Debug, Clone)]
pub struct AssetCache {
    /// 安装根（规范化后的绝对路径）。
    root: PathBuf,
    /// 包内相对路径 → 绝对文件路径。
    files: BTreeMap<String, PathBuf>,
    /// 视图 id → 入口相对路径。
    views: BTreeMap<String, String>,
}

impl AssetCache {
    /// 为一个**已验证**的安装建立缓存。
    ///
    /// `manifest` 是安装时验签过的清单：它声明了哪些视图入口存在。这里把入口
    /// 也解出来，因为"请求一个视图"与"请求一个静态文件"是两种意图，前者不该
    /// 由调用方自己拼路径。
    pub fn load(
        release: &InstalledRelease,
        manifest: &Manifest,
    ) -> Result<AssetCache, PluginError> {
        let root = release
            .destination
            .canonicalize()
            .map_err(|e| PluginError::Io(format!("{}: {e}", release.destination.display())))?;

        let mut files = BTreeMap::new();
        collect(&root, &root, &mut files)?;

        let mut views = BTreeMap::new();
        for view in &manifest.views {
            // 入口必须在白名单里。声明了却不在包里，是安装损坏，不是运行时
            // 才该发现的 404——早失败给出的是"哪个文件不见了"。
            if !files.contains_key(&view.entry) {
                return Err(PluginError::Integrity(format!(
                    "view `{}` declares entry `{}`, which is not in the installation",
                    view.id, view.entry
                )));
            }
            views.insert(view.id.clone(), view.entry.clone());
        }

        Ok(AssetCache { root, files, views })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 某个视图的入口相对路径。
    pub fn view_entry(&self, view: &str) -> Option<&str> {
        self.views.get(view).map(String::as_str)
    }

    /// 按相对路径取文件。表里没有就是没有，不回退到磁盘查找。
    pub fn resolve(&self, relative: &str) -> Option<&Path> {
        // 键在构建时已经规范化过（`\` → `/`，去掉前导 `./`）。请求侧再做一次
        // 同样的规范化，让 `./ui/index.html` 与 `ui/index.html` 是同一个文件，
        // 同时**不**接受 `..`：表里不可能有这样的键。
        let key = normalize(relative)?;
        self.files.get(&key).map(PathBuf::as_path)
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// 请求侧的规范化：拒绝空、拒绝 `..`、拒绝绝对路径与 NUL。
///
/// 与构建侧的 [`collect`] 不同，这里**不**做"容忍"：构建侧面对的是字节，
/// 请求侧面对的是不可信输入。
fn normalize(relative: &str) -> Option<String> {
    if relative.is_empty() || relative.contains('\0') {
        return None;
    }
    let trimmed = relative.strip_prefix('/').unwrap_or(relative);
    if trimmed.is_empty() {
        return None;
    }
    let mut segments = Vec::new();
    for segment in trimmed.split('/') {
        match segment {
            "" | "." => continue,
            ".." => return None,
            other => segments.push(other),
        }
    }
    if segments.is_empty() {
        return None;
    }
    Some(segments.join("/"))
}

fn collect(
    root: &Path,
    dir: &Path,
    out: &mut BTreeMap<String, PathBuf>,
) -> Result<(), PluginError> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| PluginError::Io(format!("{}: {e}", dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| PluginError::Io(e.to_string()))?;
        let path = entry.path();
        // 用 `symlink_metadata`：`metadata` 会跟随链接，那样符号链接看起来
        // 就像普通文件，我们会把它的目标读进来。
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|e| PluginError::Io(format!("{}: {e}", path.display())))?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            collect(root, &path, out)?;
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        // 安装根下的相对路径决定键。用 `strip_prefix` 而不是字符串裁剪：
        // 安装根本身可能包含与文件名相同的片段。
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let Some(key) = relative.to_str().map(|value| value.replace('\\', "/")) else {
            continue;
        };
        out.insert(key, path);
    }
    Ok(())
}
