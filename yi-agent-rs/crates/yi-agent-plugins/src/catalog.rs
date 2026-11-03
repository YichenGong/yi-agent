//! 认证目录与受控下载（设计 §5，协议 §11 的 `catalog` / `install`）。
//!
//! # 为什么把 fetcher 做成 trait
//!
//! 这里要保证的性质全是**失败时**的性质：重定向离开允许源要拒、无进展超时
//! 要拒、大小与签名声明的不符要拒、总时长超限要拒。这些性质没法用"跑一个
//! 真网络请求再等"来测——那种测试要么慢、要么在 CI 上不可复现。注入
//! fetcher 与时钟，就能把每种失败精确地摆出来。
//!
//! # 三条边界
//!
//! 1. **URL 不由页面给。** 调用方只能提交一个 [`ReleaseId`]；目录把它解析成
//!    一个 `https://` 地址。iframe 传任意 URL 或本地路径没有任何入口。
//! 2. **重定向不能离开允许源。** 跟随跳转是下载器的默认行为，而默认行为会把
//!    一次"官方目录"请求悄悄领到别处。每一次跳转都要重新过源校验。
//! 3. **两种超时分开。** 无进展超时抓"连接活着但不动了"，总超时抓"慢慢挤
//!    牙膏"。只有前者会让一个每秒一字节的服务器永远占着下载位。

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::PluginError;
use crate::trust::VerifiedRelease;
use crate::types::ReleaseId;

/// 官方目录允许的协议与主机。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedOrigin {
    pub scheme: String,
    pub host: String,
}

impl AllowedOrigin {
    /// 官方目录：只允许 https + 一个固定主机。
    pub fn official(host: impl Into<String>) -> AllowedOrigin {
        AllowedOrigin {
            scheme: "https".to_string(),
            host: host.into(),
        }
    }

    /// 一个 URL 是否落在这个源上。
    fn allows(&self, url: &Url) -> bool {
        url.scheme == self.scheme && url.host.eq_ignore_ascii_case(&self.host)
    }
}

/// 拆出来的最小 URL 结构。
///
/// 不引入完整 URL 解析库是有意的：这里只需要 scheme / host / path 三件事，
/// 而一个完整的解析器会把"它认为合法"的边角形状带进来。少一点表面积，
/// 少一处和"什么算同一个源"的分歧。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub scheme: String,
    pub host: String,
    pub path: String,
}

impl Url {
    pub fn parse(raw: &str) -> Result<Url, PluginError> {
        let (scheme, rest) = raw
            .split_once("://")
            .ok_or_else(|| PluginError::Untrusted(format!("`{raw}` is not an absolute URL")))?;
        if scheme.is_empty() {
            return Err(PluginError::Untrusted(
                "URL has an empty scheme".to_string(),
            ));
        }
        // 用户信息段（`https://evil@official.example/...`）会让主机看起来是对的
        // 而实际连的是别处。直接拒绝，不做归一化——归一化本身就是猜。
        if rest.contains('@') {
            return Err(PluginError::Untrusted(format!(
                "`{raw}` carries user info, which makes the host ambiguous"
            )));
        }
        let (authority, path) = match rest.find('/') {
            Some(index) => (&rest[..index], &rest[index..]),
            None => (rest, "/"),
        };
        // 端口不参与"同一个源"的判断是错的（`host:8443` 是另一个源），
        // 所以带端口的地址一律不接受：官方目录没有这种形状。
        if authority.contains(':') {
            return Err(PluginError::Untrusted(format!(
                "`{raw}` pins a port; official downloads never do"
            )));
        }
        if authority.is_empty() {
            return Err(PluginError::Untrusted("URL has an empty host".to_string()));
        }
        Ok(Url {
            scheme: scheme.to_ascii_lowercase(),
            host: authority.to_ascii_lowercase(),
            path: path.to_string(),
        })
    }
}

/// 一次下载请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadRequest {
    pub url: Url,
    /// 签名元数据声明的大小。下载超过它就是攻击或损坏，不等它传完。
    pub expected_bytes: u64,
}

/// 一次下载的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadOutcome {
    pub bytes: Vec<u8>,
    /// 跳转链，按发生顺序。用于审计"它是不是绕了一圈才到"。
    pub redirects: Vec<Url>,
}

/// 供下载器调用的外部世界。
///
/// 每个方法都返回"这一步发生了什么"，而不是让下载器去猜。
pub trait Fetcher {
    /// 发起一次请求，得到一个响应头。
    fn head(&mut self, url: &Url) -> Result<ResponseHead, FetchError>;
    /// 读取一段响应体。`Ok(None)` = 流结束。
    ///
    /// 每次调用**必须**在 `per_progress_timeout` 内返回（或者返回
    /// `FetchError::NoProgress`）。由实现方负责这个截止时间，因为只有它知道
    /// 一次读有多长。
    fn read(&mut self, max_bytes: usize) -> Result<Option<Vec<u8>>, FetchError>;
}

/// 一个响应头。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResponseHead {
    pub status: u16,
    pub location: Option<String>,
    pub content_length: Option<u64>,
}

/// 抓取失败。分类是为了让上层能给出"重试有意义/没有意义"的判断。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// 在给定时间内没有任何进展。
    NoProgress,
    /// 网络/协议层错误。
    Transport(String),
    /// 服务器明确拒绝（4xx/5xx）。
    Status(u16),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::NoProgress => write!(f, "no progress before the deadline"),
            FetchError::Transport(message) => write!(f, "transport error: {message}"),
            FetchError::Status(code) => write!(f, "the server answered {code}"),
        }
    }
}

impl From<FetchError> for PluginError {
    fn from(error: FetchError) -> PluginError {
        match error {
            FetchError::NoProgress => {
                PluginError::Io("the download made no progress before its deadline".to_string())
            }
            FetchError::Transport(message) => {
                PluginError::Io(format!("download failed: {message}"))
            }
            FetchError::Status(code) => {
                PluginError::Io(format!("the download server answered {code}"))
            }
        }
    }
}

/// 下载限额。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadLimits {
    /// 两次读到数据之间允许的最长间隔。
    pub per_progress_timeout: Duration,
    /// 整个下载允许的最长时长。
    pub total_timeout: Duration,
    /// 最多跟随几次跳转。
    pub max_redirects: usize,
}

impl Default for DownloadLimits {
    fn default() -> DownloadLimits {
        DownloadLimits {
            per_progress_timeout: Duration::from_secs(30),
            total_timeout: Duration::from_secs(600),
            max_redirects: 3,
        }
    }
}

/// 时钟。注入是为了让超时测试不真的等 600 秒。
pub trait Clock {
    /// 自下载开始以来的毫秒数。
    fn elapsed_ms(&self) -> u64;
}

/// 下载一个已签名的发布包。
///
/// 顺序是：源校验 → 跳转（每次重新校验）→ 大小预检 → 流式读取（双超时）
/// → 大小核对。任何一步失败都返回错误，且**不留下部分结果**——调用方拿到
/// `Ok` 时 `bytes` 一定是完整的。
pub fn download(
    request: &DownloadRequest,
    origin: &AllowedOrigin,
    fetcher: &mut dyn Fetcher,
    limits: &DownloadLimits,
    clock: &mut dyn Clock,
) -> Result<DownloadOutcome, PluginError> {
    if !origin.allows(&request.url) {
        return Err(PluginError::Untrusted(format!(
            "`{}://{}` is not an allowed download origin",
            request.url.scheme, request.url.host
        )));
    }

    let mut url = request.url.clone();
    let mut redirects = Vec::new();
    let mut head = fetcher.head(&url)?;

    // 跟随跳转，但每一次都要重新过源校验。一次跳转离开官方源，就等同于
    // 让一个陌生服务器提供"官方包"。
    while matches!(head.status, 301 | 302 | 303 | 307 | 308) {
        let location = head.location.clone().ok_or_else(|| {
            PluginError::Untrusted(format!(
                "`{}` answered {} without a Location",
                url.path, head.status
            ))
        })?;
        if redirects.len() >= limits.max_redirects {
            return Err(PluginError::Untrusted(format!(
                "the download redirected more than {} times",
                limits.max_redirects
            )));
        }
        // 相对跳转：只换 path，源不变。
        let next = if location.contains("://") {
            Url::parse(&location)?
        } else {
            Url {
                scheme: url.scheme.clone(),
                host: url.host.clone(),
                path: if location.starts_with('/') {
                    location
                } else {
                    format!("/{location}")
                },
            }
        };
        if !origin.allows(&next) {
            return Err(PluginError::Untrusted(format!(
                "the download redirected to `{}://{}`, which is outside the allowed origin",
                next.scheme, next.host
            )));
        }
        redirects.push(next.clone());
        url = next;
        head = fetcher.head(&url)?;
    }

    if head.status != 200 {
        return Err(FetchError::Status(head.status).into());
    }
    // 声明的大小与签名绑定的不一致：不下载，直接拒绝。等它传完再比，
    // 等于先花掉一次攻击者想要的带宽。
    if let Some(declared) = head.content_length {
        if declared != request.expected_bytes {
            return Err(PluginError::Integrity(format!(
                "the server offers {declared} bytes but the signed metadata binds {}",
                request.expected_bytes
            )));
        }
    }

    let budget = usize::try_from(request.expected_bytes).map_err(|_| {
        PluginError::Integrity("the declared release size does not fit in memory".to_string())
    })?;
    let mut bytes = Vec::with_capacity(budget.min(1 << 20));
    loop {
        if chrono_elapsed_exceeds(clock, limits.total_timeout) {
            return Err(PluginError::Io(
                "the download exceeded its total deadline".to_string(),
            ));
        }
        let chunk = match fetcher.read(64 * 1024) {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(FetchError::NoProgress) => {
                return Err(PluginError::Io(format!(
                    "the download made no progress within {:?}",
                    limits.per_progress_timeout
                )));
            }
            Err(error) => return Err(error.into()),
        };
        // 边读边卡上限：先收进内存再说，就是一个内存耗尽入口。
        if bytes.len() + chunk.len() > budget {
            return Err(PluginError::Integrity(format!(
                "the download exceeded its signed size of {} bytes",
                request.expected_bytes
            )));
        }
        bytes.extend_from_slice(&chunk);
    }

    if bytes.len() as u64 != request.expected_bytes {
        return Err(PluginError::Integrity(format!(
            "the download is {} bytes but the signed metadata binds {}",
            bytes.len(),
            request.expected_bytes
        )));
    }

    Ok(DownloadOutcome { bytes, redirects })
}

fn chrono_elapsed_exceeds(clock: &mut dyn Clock, limit: Duration) -> bool {
    clock.elapsed_ms() > limit.as_millis() as u64
}

/// 目录里的一条可用发布。
///
/// **不是**信任凭据：这里只是"官方目录说有这么个版本"。真正的授权仍然来自
/// `TrustStore::verify_release`——目录条目负责告诉调用方该去下载什么，签名
/// 负责说它确实是那个东西。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CatalogEntry {
    pub release: ReleaseId,
    pub size_bytes: u64,
    /// 签名元数据的地址。必须是官方源上的路径。
    pub metadata_path: String,
    /// 包的地址。必须是官方源上的路径。
    pub bundle_path: String,
    pub published_at: u64,
    pub expires_at: u64,
    /// 目录自身的发布序号。低于已接受值的目录拒绝。
    pub serial: u64,
}

/// 认证过的目录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Catalog {
    /// 目录序列号。比已接受值低 = 回放。
    pub serial: u64,
    pub entries: Vec<CatalogEntry>,
}

impl Catalog {
    /// 按发布查一条。
    pub fn find(&self, release: &ReleaseId) -> Option<&CatalogEntry> {
        self.entries.iter().find(|entry| &entry.release == release)
    }

    /// 解析一次安装该下载哪两样东西。
    ///
    /// 路径拼在**已认证的源**上，而不是接受目录里的完整 URL：一份被篡改的
    /// 目录最多能让你找不到东西，不能把你领到别的服务器。
    pub fn download_request(
        &self,
        release: &ReleaseId,
        origin: &AllowedOrigin,
    ) -> Result<(DownloadRequest, DownloadRequest), PluginError> {
        let entry = self.find(release).ok_or_else(|| {
            PluginError::NotFound(format!(
                "the catalog has no release {} {} for {}",
                release.plugin.as_str(),
                release.version,
                release.target
            ))
        })?;
        let make = |path: &str, expected: u64| -> Result<DownloadRequest, PluginError> {
            if !path.starts_with('/') {
                return Err(PluginError::Untrusted(format!(
                    "catalog path `{path}` is not an absolute path on the official origin"
                )));
            }
            Ok(DownloadRequest {
                url: Url {
                    scheme: origin.scheme.clone(),
                    host: origin.host.clone(),
                    path: path.to_string(),
                },
                expected_bytes: expected,
            })
        };
        // 元数据本身的大小不由它自己声明（那是循环论证），给一个足够小的
        // 上限：一份签名信封不该有几十兆。
        let metadata = make(&entry.metadata_path, 256 * 1024)?;
        let bundle = make(&entry.bundle_path, entry.size_bytes)?;
        Ok((metadata, bundle))
    }

    /// 目录条目与签名元数据是否吻合。
    ///
    /// 两者都要对，因为它们是两份独立来源：目录说"有 1.2.0，包是 X 字节"，
    /// 签名说"1.2.0 的包是 Y 字节"。对不上说明至少有一方被改过。
    pub fn agrees_with(&self, verified: &VerifiedRelease) -> Result<(), PluginError> {
        let entry = self.find(&verified.release).ok_or_else(|| {
            PluginError::NotFound("the catalog does not list this release".to_string())
        })?;
        if entry.size_bytes != verified.size_bytes {
            return Err(PluginError::Integrity(format!(
                "the catalog says {} bytes, the signed metadata says {}",
                entry.size_bytes, verified.size_bytes
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
