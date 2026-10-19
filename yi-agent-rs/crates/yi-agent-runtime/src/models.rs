//! 机器级模型清单：`~/.yi-agent/models.json`。
//!
//! 每条模型自带 provider 格式 / api_url / api key，密钥跟随 url 走。该文件是
//! **机器级全局**的（与 `~/.yi-agent/.env` 同目录），跨项目、跨重启共享。
//! 未配置时为空清单，调用方回退 `.env`/cfg，行为与今天一致。
//!
//! 落盘约定与 `yi-agent-app-server/src/settings_store.rs` 一致：读-改-写、
//! 临时名逐次唯一、`rename` 原子替换；文件含密钥，权限收紧为 `0600`。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use yi_agent_core::ApiStyle;

use crate::config::RuntimeConfig;

/// 旧版 openai 默认值（不含版本段）。仅这一精确值会被迁移，自定义地址不动。
const LEGACY_OPENAI_BASE_URL: &str = "https://api.openai.com";
/// 新版 openai 默认值：含版本段的 API 根（对齐 OpenAI 官方 SDK 约定）。
const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

/// 加载期精确迁移：把「恰为旧默认值的 openai 条目」改写为含 `/v1` 的新默认值。
///
/// 只认一对一映射（旧默认值 → 新默认值），不做任何启发式猜测；自定义地址
/// （如网关根 `https://llmapi.horizon.auto`）一律不动，由设置页提示用户自查。
fn migrate_openai_api_url(entry: &mut ModelEntry) {
    if entry.provider == ModelProvider::Openai
        && entry.api_url.trim_end_matches('/') == LEGACY_OPENAI_BASE_URL
    {
        entry.api_url = OPENAI_BASE_URL.to_string();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelProvider {
    Anthropic,
    Openai,
}

impl ModelProvider {
    pub fn as_str(&self) -> &'static str {
        match self {
            ModelProvider::Anthropic => "anthropic",
            ModelProvider::Openai => "openai",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "anthropic" => Some(ModelProvider::Anthropic),
            "openai" => Some(ModelProvider::Openai),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelEntry {
    pub name: String,
    #[serde(deserialize_with = "de_provider")]
    pub provider: ModelProvider,
    pub api_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key: String,
    /// `auto`（默认）| `responses` | `chat`；未知值按 `auto` 处理。
    /// 非字符串值按「未设置」处理，不让整条被跳过（见 `de_optional_string`）。
    #[serde(default, deserialize_with = "de_optional_string")]
    pub api_style: Option<String>,
    /// 七档之一；`None` = 不发该字段。
    /// 非字符串值按「未设置」处理，不让整条被跳过（见 `de_optional_string`）。
    #[serde(default, deserialize_with = "de_optional_string")]
    pub reasoning_effort: Option<String>,
    /// 各通道的 effort 探测结果；`None` = 从未探测。坏值按「未探测」处理。
    #[serde(default, deserialize_with = "de_optional_capabilities")]
    pub reasoning_capabilities: Option<ReasoningCapabilities>,
    /// 该条目的上下文窗口（tokens）；`None` = 沿用 cfg 的既有阈值。
    #[serde(default)]
    pub context_length: Option<u32>,
}

/// 七档白名单，顺序即 UI 顺序（与 `desktop/src/lib/modelPicker.ts` 的
/// `REASONING_EFFORTS` 逐字一致）。
pub const REASONING_TIERS: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// 某条目在某条通道上的 effort 探测结果。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelCapability {
    /// 探测为支持的档位，按 [`REASONING_TIERS`] 顺序、已去重。
    #[serde(default, deserialize_with = "de_supported_tiers")]
    pub supported: Vec<String>,
    /// 探测完成时间（Unix 秒）。坏值读作 0。
    #[serde(default, deserialize_with = "de_u64_lossy")]
    pub probed_at: u64,
    /// 非空 = 该通道探测未能完成（鉴权/网络/网关异常），`supported` 不可信。
    #[serde(default, deserialize_with = "de_optional_string")]
    pub error: Option<String>,
}

/// 三条通道各一份结果。未探测的通道为 `None`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningCapabilities {
    #[serde(default, deserialize_with = "de_optional_capability")]
    pub chat: Option<ChannelCapability>,
    #[serde(default, deserialize_with = "de_optional_capability")]
    pub responses: Option<ChannelCapability>,
    #[serde(default, deserialize_with = "de_optional_capability")]
    pub anthropic: Option<ChannelCapability>,
}

impl ReasoningCapabilities {
    /// 通道键（`chat` / `responses` / `anthropic`）对应的槽位。
    pub fn channel_mut(&mut self, channel: &str) -> Option<&mut Option<ChannelCapability>> {
        match channel {
            "chat" => Some(&mut self.chat),
            "responses" => Some(&mut self.responses),
            "anthropic" => Some(&mut self.anthropic),
            _ => None,
        }
    }

    pub fn get(&self, channel: &str) -> Option<&ChannelCapability> {
        match channel {
            "chat" => self.chat.as_ref(),
            "responses" => self.responses.as_ref(),
            "anthropic" => self.anthropic.as_ref(),
            _ => None,
        }
    }

    /// 是否存在任一「可信」结果（有记录且 `error` 为空）。
    pub fn has_trustworthy(&self) -> bool {
        [&self.chat, &self.responses, &self.anthropic]
            .into_iter()
            .flatten()
            .any(|c| c.error.is_none())
    }
}

/// 条目实际会走的通道键：`chat` / `responses` / `anthropic`。
///
/// `auto` 必须选一条：`OpenaiProvider` 运行期先试 `/responses`、缺失才回退
/// chat，故能力也按同一优先级取——两条都有可信记录时以 `responses` 为准，只有
/// chat 可信时才落到 chat。这个函数与 [`supported_efforts`] 必须同源，否则
/// 「UI 展示的档位」和「运行期校验的档位」会来自不同通道。
pub fn effective_channel(entry: &ModelEntry) -> &'static str {
    match entry.provider {
        ModelProvider::Anthropic => "anthropic",
        ModelProvider::Openai => match entry.api_style.as_deref() {
            Some("chat") => "chat",
            Some("responses") => "responses",
            _ => {
                let caps = entry.reasoning_capabilities.as_ref();
                let trustworthy =
                    |c: &Option<ChannelCapability>| c.as_ref().is_some_and(|c| c.error.is_none());
                match caps {
                    Some(c) if trustworthy(&c.chat) && !trustworthy(&c.responses) => "chat",
                    _ => "responses",
                }
            }
        },
    }
}

/// 该条目在**生效通道**上被证实可用的档位；`None` = 能力未知。
///
/// 「未知」与「空集」是两件事：前者表示没探测过（或探测出错），调用方应保持
/// 宽松（平铺七档、运行期靠剥离重投兜底）；后者表示探测明确得出结论——七档
/// 全被拒。把两者混为一谈会让「探测失败的模型」在 UI 上一个档位都没有。
pub fn supported_efforts(entry: &ModelEntry) -> Option<Vec<String>> {
    let caps = entry.reasoning_capabilities.as_ref()?;
    let cap = caps.get(effective_channel(entry))?;
    if cap.error.is_some() {
        // 这一轮没结论（鉴权失败、超时、基线被拒）：按未知处理，不要谎报
        // 「零个档位可用」。
        return None;
    }
    Some(cap.supported.clone())
}

/// 某档位是否可以发给该模型。能力未知时恒为 `true`（保持探测功能上线前的
/// 行为：靠「被拒剥离重投」兜底）。
pub fn effort_is_allowed(entry: &ModelEntry, effort: &str) -> bool {
    supported_efforts(entry)
        .map(|list| list.iter().any(|s| s == effort))
        .unwrap_or(true)
}

/// 只保留白名单内的档位，并按白名单顺序排序去重。坏值读作空表。
fn de_supported_tiers<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    let Some(items) = value.and_then(|v| v.as_array().cloned()) else {
        return Ok(Vec::new());
    };
    let mut kept: Vec<String> = items
        .iter()
        .filter_map(|v| v.as_str())
        .filter(|s| REASONING_TIERS.contains(s))
        .map(str::to_string)
        .collect();
    kept.sort_by_key(|s| {
        REASONING_TIERS
            .iter()
            .position(|t| t == s)
            .unwrap_or(usize::MAX)
    });
    kept.dedup();
    Ok(kept)
}

/// 非对象 / `null` → `None`；对象 → `Some(ChannelCapability)`。
fn de_optional_capability<'de, D>(deserializer: D) -> Result<Option<ChannelCapability>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        Some(serde_json::Value::Object(_)) => serde_json::from_value(value.unwrap())
            .map(Some)
            .map_err(serde::de::Error::custom),
        _ => Ok(None),
    }
}

/// 非数字 → 0。
fn de_u64_lossy<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|v| v.as_u64()).unwrap_or(0))
}

/// 非对象 / `null` → `None`；对象 → `Some(ReasoningCapabilities)`。
fn de_optional_capabilities<'de, D>(
    deserializer: D,
) -> Result<Option<ReasoningCapabilities>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        Some(serde_json::Value::Object(_)) => serde_json::from_value(value.unwrap())
            .map(Some)
            .map_err(serde::de::Error::custom),
        _ => Ok(None),
    }
}

/// 反序列化时把非字符串值当作「未设置」（None），避免一个手写值让整条被跳过。
/// 缺失 / `null` / 任意非字符串 → `None`；字符串 → `Some(s)`（不在读路径校验档位）。
fn de_optional_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|v| v.as_str().map(str::to_string)))
}

/// 反序列化时把非法 provider 变成错误，让整条被跳过（见 `load_catalog_from` 的逐条过滤）。
fn de_provider<'de, D>(deserializer: D) -> Result<ModelProvider, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    ModelProvider::parse(&s).ok_or_else(|| serde::de::Error::custom("unknown provider"))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCatalog {
    #[serde(default)]
    pub models: Vec<ModelEntry>,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub subagent_model: Option<String>,
}

/// `<HOME>/.yi-agent/models.json`；HOME 缺失回退当前目录，与 `resolve_global_env_path` 同口径。
pub fn models_path() -> PathBuf {
    std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".yi-agent")
        .join("models.json")
}

/// 读清单。缺失 / 不可读 / 不可解析 → 空清单（坏文件绝不阻断启动）。
/// 逐条过滤：字段非法（空显示名、未知 provider、空 url/model）的条目被跳过。
pub fn load_catalog() -> ModelCatalog {
    load_catalog_from(&models_path())
}

pub fn load_catalog_from(path: &Path) -> ModelCatalog {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ModelCatalog::default();
        }
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "could not read models.json; treating as empty");
            return ModelCatalog::default();
        }
    };
    match parse_catalog(&text, path) {
        Ok(catalog) => catalog,
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "invalid models.json; treating as empty");
            ModelCatalog::default()
        }
    }
}

/// 严格读取清单：区分「文件不存在」与「不可读 / 不可解析」。
///
/// `Ok(空清单)` 表示「确定没有配置」（文件缺失是明确信号，调用方据此回退 `cfg`）；
/// `Err(reason)` 表示「有文件但读不出来 / 解析不了」——调用方据此保留上一次可用值，
/// 而不是把一次坏编辑当成「用户清空了配置」。
///
/// 既有的 [`load_catalog_from`] 保持「绝不报错」的容错语义（坏文件不阻断启动），
/// 两者是**互补**而非替代关系。
pub fn try_load_catalog_from(path: &Path) -> Result<ModelCatalog, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ModelCatalog::default());
        }
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    parse_catalog(&text, path).map_err(|error| format!("invalid {}: {error}", path.display()))
}

/// `models.json` 的指纹：`(mtime, size)`；`None` 的 mtime 表示文件不存在。
///
/// 用 stat 而非内容哈希：每次调用只是一次微秒级 syscall，而重建清单解析只在
/// 指纹真的变化后发生（见 `2026-10-17-model-catalog-live-reload-design.md` §6.3）。
#[derive(Clone, PartialEq, Eq)]
struct Fingerprint {
    mtime: Option<SystemTime>,
    size: u64,
}

fn fingerprint_of(path: &Path) -> Fingerprint {
    match std::fs::metadata(path) {
        Ok(meta) => Fingerprint {
            mtime: meta.modified().ok(),
            size: meta.len(),
        },
        Err(_) => Fingerprint {
            mtime: None,
            size: 0,
        },
    }
}

struct WatcherState {
    fingerprint: Fingerprint,
    catalog: Arc<ModelCatalog>,
}

/// 进程内「按指纹自更新的模型清单」句柄。
///
/// 谁使用清单、谁就负责它的新鲜度：daemon 的 worker 工厂与 app-server 的
/// 会话工厂都通过它取清单，于是运行期改 `models.json`（无论手工编辑、CLI/TUI
/// 写入还是设置页 RPC）都会在下一次用到时生效，无需重启进程。
///
/// 语义（与设计文档 §4.2 一致）：
/// - 指纹未变 → 直接返回缓存的 `Arc`（零解析）；
/// - 指纹变化且解析成功 → 换缓存 + 推进指纹；
/// - 指纹变化但解析失败（损坏 / 不可读）→ **保留旧值且不推进指纹**，于是文件
///   修好后下一次调用立即生效；
/// - 文件不存在 → 采纳空清单（「没有配置」是明确信号，不是错误）。
pub struct CatalogWatcher {
    path: PathBuf,
    /// `from_catalog` 造的固定快照：永不重读（见 `current`）。
    pinned: bool,
    state: Mutex<WatcherState>,
}

impl CatalogWatcher {
    /// 构造即读一次并记指纹。
    ///
    /// 首次读取用宽松版：启动期的坏文件不该让 daemon 起不来（与既有
    /// 「坏文件绝不阻断启动」一致）。
    pub fn new(path: PathBuf) -> Arc<Self> {
        let catalog = Arc::new(load_catalog_from(&path));
        Arc::new(Self {
            state: Mutex::new(WatcherState {
                fingerprint: fingerprint_of(&path),
                catalog,
            }),
            path,
            pinned: false,
        })
    }

    /// 固定快照：永不重读。测试与「本就持有清单」的调用方用它。
    pub fn from_catalog(path: PathBuf, catalog: ModelCatalog) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(WatcherState {
                fingerprint: fingerprint_of(&path),
                catalog: Arc::new(catalog),
            }),
            path,
            pinned: true,
        })
    }

    /// 返回当前清单：指纹变了才重读，且只在解析成功时推进指纹。
    pub fn current(&self) -> Arc<ModelCatalog> {
        // 固定快照（`from_catalog`）：无论磁盘怎么变都返回构造函数给的那份。
        if self.pinned {
            return Arc::clone(&self.state.lock().unwrap_or_else(|p| p.into_inner()).catalog);
        }
        let observed = fingerprint_of(&self.path);
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if observed == state.fingerprint {
            return Arc::clone(&state.catalog);
        }
        match try_load_catalog_from(&self.path) {
            Ok(catalog) => {
                state.fingerprint = observed;
                state.catalog = Arc::new(catalog);
                Arc::clone(&state.catalog)
            }
            Err(reason) => {
                // 保留 last-known-good，且**不推进指纹**：文件改好后下一次调用即生效。
                tracing::warn!(
                    path = %self.path.display(),
                    %reason,
                    "model catalog is unreadable; keeping the last known good catalog"
                );
                Arc::clone(&state.catalog)
            }
        }
    }
}

/// 把清单文本解析为 [`ModelCatalog`]。`load_catalog_from`（宽松）与
/// `try_load_catalog_from`（严格）共用它，保证两份读取口径逐字节一致
/// （含 warning 日志字段，故**必须**接收 `path`）。
fn parse_catalog(text: &str, path: &Path) -> Result<ModelCatalog, serde_json::Error> {
    let value: serde_json::Value = serde_json::from_str(text)?;
    // 顶层必须是对象。数组 / 标量 / `null` 都是「合法 JSON，但不是清单文档」：
    // 若当成空清单，调用方会误判「用户没配置」并覆盖掉用户的文件（静默丢失）。
    // `load_catalog_from` 仍是 Err→空清单（坏文件不阻断启动），故此处报错安全。
    let Some(object) = value.as_object() else {
        return Err(<serde_json::Error as serde::de::Error>::custom(format!(
            "{}: expected a JSON object at the top level",
            path.display()
        )));
    };
    let mut catalog = ModelCatalog::default();
    if let Some(models) = object.get("models").and_then(|v| v.as_array()) {
        let mut skipped = 0usize;
        for raw in models {
            match serde_json::from_value::<ModelEntry>(raw.clone()) {
                Ok(mut entry)
                    if !entry.name.trim().is_empty()
                        && !entry.api_url.trim().is_empty()
                        && !entry.model.trim().is_empty() =>
                {
                    migrate_openai_api_url(&mut entry);
                    catalog.models.push(entry);
                }
                Ok(_) => {
                    skipped += 1;
                    tracing::warn!(path = %path.display(), "skipping incomplete model entry")
                }
                Err(error) => {
                    skipped += 1;
                    tracing::warn!(%error, path = %path.display(), "skipping malformed model entry")
                }
            }
        }
        // 写了条目却一条都没能留下 = 用户写坏了，绝不能当成「没有配置」：
        // 否则调用方会导入并整段 rename 掉用户的 `models` 数组。
        if !models.is_empty() && catalog.models.is_empty() {
            return Err(<serde_json::Error as serde::de::Error>::custom(format!(
                "{}: all {skipped} model entries were skipped as invalid",
                path.display()
            )));
        }
    }
    catalog.default_model = object
        .get("default_model")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    catalog.subagent_model = value
        .get("subagent_model")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Ok(catalog)
}

pub fn save_catalog(catalog: &ModelCatalog) -> std::io::Result<()> {
    save_catalog_to(&models_path(), catalog)
}

/// 写清单：读-改-写，保留 `models.json` 里无关的顶层键，最后原子替换。
///
/// 既有文件「读不出来」（损坏 JSON / 非对象）时**报错而不覆盖**：那是用户的配置
/// 出了岔子，不是「没有配置」。静默重建会把整份清单扔掉（含别的条目的 key）。
/// 只有文件**不存在**才从空对象起步（真正的首写）。
pub fn save_catalog_to(path: &Path, catalog: &ModelCatalog) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // 读-改-写：以既有对象为底，仅覆盖本模块拥有的三个键，其余顶层键原样保留。
    let mut object = match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "{} exists but is not a readable JSON object; refusing to overwrite it",
                        path.display()
                    ),
                )
            })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::Map::new(),
        Err(error) => return Err(error),
    };
    let catalog_value = serde_json::to_value(catalog).map_err(std::io::Error::other)?;
    if let serde_json::Value::Object(fields) = catalog_value {
        for (key, value) in fields {
            object.insert(key, value);
        }
    }
    let text = serde_json::to_string_pretty(&serde_json::Value::Object(object))
        .map_err(std::io::Error::other)?;
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!("models.json.{}.{}.tmp", std::process::id(), seq));
    write_owner_only(&tmp, &text)?;
    std::fs::rename(&tmp, path)?;
    // rename 保留 tmp 的权限位；显式再设一次以防既有文件权限更宽。
    set_owner_only(path)?;
    Ok(())
}

fn write_owner_only(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(text.as_bytes())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, text)
    }
}

fn set_owner_only(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 密钥掩码：只留尾 4 位，其余用 `••••`；长度 ≤4 时全部掩掉。
pub fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 4 {
        return "••••".to_string();
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("••••{tail}")
}

/// 会话覆盖 → 全局默认 → 原 cfg 三级解析，返回改写了 provider/api_url/api_key/model
/// 以及 `api_style` / `reasoning_effort` 的 cfg 克隆，其余字段保持原值。
///
/// 条目选择走 [`effective_entry_for`]：会话覆盖 > 整包注入 > 清单默认。
pub fn resolve_effective(
    cfg: &RuntimeConfig,
    catalog: &ModelCatalog,
    session_override: Option<&str>,
) -> RuntimeConfig {
    match effective_entry_for(cfg, catalog, session_override) {
        Some(entry) => {
            let mut out = cfg.clone();
            out.provider = entry.provider.as_str().to_string();
            out.api_url = entry.api_url.clone();
            out.api_key = entry.api_key.clone();
            out.model = entry.model.clone();
            out.api_style = entry
                .api_style
                .as_deref()
                .map(ApiStyle::parse)
                .unwrap_or(RuntimeConfig::default_api_style());
            out.reasoning_effort = entry.reasoning_effort.clone();
            // 能力可信且该档不在支持列表里 → 按「未发送」处理：发出去必然被拒，
            // 只会让 provider 白跑一轮「剥离字段重投」。`None`（能力未知）不受影响。
            if let Some(effort) = out.reasoning_effort.clone() {
                if !effort_is_allowed(entry, &effort) {
                    tracing::info!(
                        model = %entry.name,
                        %effort,
                        "configured reasoning effort is not supported; dropping it"
                    );
                    out.reasoning_effort = None;
                }
            }
            // `context_length` 来自手写 `models.json`，是外部输入：饱和乘法挡住 `length * ratio`
            // 的溢出（debug 会 panic、release 会静默回绕）；`0` 视为"未声明"，否则
            // `compact_threshold == 0` 会让每一轮都触发压缩。静默条目保持 cfg 阈值原样。
            if let Some(length) = entry.context_length {
                if length > 0 {
                    out.compact_threshold = length.saturating_mul(out.compact_ratio) / 100;
                }
            }
            out
        }
        None => cfg.clone(),
    }
}

/// 条目选择的**唯一**规则：会话覆盖 > 整包注入 > 清单默认。
///
/// `model_injected` 由调用方从真实配置取（[`RuntimeConfig::model_injected`]）。这条
/// 规则单独开一个入口，是给**拿不到完整 `RuntimeConfig`** 的调用方用的（TUI `/effort`
/// 手上只有快照里的几个字段）：让它们复用同一条规则，而不是各自再推导一遍——
/// 「上报 == 实际发出」正是靠这一点成立的。`resolve_effective` 与 TUI 都落到这里。
pub fn effective_entry_with_injection<'a>(
    catalog: &'a ModelCatalog,
    session_override: Option<&str>,
    model_injected: bool,
) -> Option<&'a ModelEntry> {
    let by_name = |name: &str| catalog.models.iter().find(|m| m.name == name);
    if let Some(entry) = session_override.and_then(by_name) {
        return Some(entry);
    }
    if model_injected {
        return None;
    }
    catalog.default_model.as_deref().and_then(by_name)
}

/// 解析 `cfg` 与清单得到本次生效条目。优先级：会话覆盖 > 整包注入 > 清单默认。
///
/// 注入存在时**不看**清单默认：用户显式给的一整包压过持久化的默认值，
/// 这正是「改了没反应」那个病的解药。
pub fn effective_entry_for<'a>(
    cfg: &RuntimeConfig,
    catalog: &'a ModelCatalog,
    session_override: Option<&str>,
) -> Option<&'a ModelEntry> {
    effective_entry_with_injection(catalog, session_override, cfg.model_injected)
}

/// 无注入语境下的条目解析（启动期、`onboarding` 评估等）。等价于
/// [`effective_entry_with_injection`] 且注入标记为 `false`。
pub fn effective_entry<'a>(
    catalog: &'a ModelCatalog,
    session_override: Option<&str>,
) -> Option<&'a ModelEntry> {
    effective_entry_with_injection(catalog, session_override, false)
}

/// 子 agent 条目：`subagent_model` → 全局默认 → None（调用方回退 cfg）。
pub fn subagent_entry(catalog: &ModelCatalog) -> Option<&ModelEntry> {
    let by_name = |name: &str| catalog.models.iter().find(|m| m.name == name);
    catalog
        .subagent_model
        .as_deref()
        .and_then(by_name)
        .or_else(|| catalog.default_model.as_deref().and_then(by_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> ModelEntry {
        ModelEntry {
            name: name.to_string(),
            provider: ModelProvider::Anthropic,
            api_url: "https://x".into(),
            model: "claude-x".into(),
            api_key: "sk-secret-1234".into(),
            api_style: None,
            reasoning_effort: None,
            reasoning_capabilities: None,
            context_length: None,
        }
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        let catalog = ModelCatalog {
            models: vec![entry("a")],
            default_model: Some("a".into()),
            subagent_model: None,
        };
        save_catalog_to(&path, &catalog).unwrap();
        assert_eq!(load_catalog_from(&path), catalog);
    }

    #[test]
    fn a_missing_file_is_an_empty_catalog() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        assert_eq!(load_catalog_from(&path), ModelCatalog::default());
    }

    #[test]
    fn a_broken_file_is_an_empty_catalog() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load_catalog_from(&path), ModelCatalog::default());
    }

    #[test]
    fn malformed_entries_are_skipped() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(
            &path,
            r#"{"models":[
                {"name":"ok","provider":"openai","api_url":"u","model":"m","api_key":"k"},
                {"name":"","provider":"openai","api_url":"u","model":"m","api_key":"k"},
                {"name":"bad-provider","provider":"gemini","api_url":"u","model":"m","api_key":"k"}
            ],"default_model":"ok"}"#,
        )
        .unwrap();
        let catalog = load_catalog_from(&path);
        assert_eq!(catalog.models.len(), 1);
        assert_eq!(catalog.models[0].name, "ok");
        assert_eq!(catalog.default_model.as_deref(), Some("ok"));
    }

    #[test]
    fn the_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        save_catalog_to(&path, &ModelCatalog::default()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "models.json carries API keys and must be 0600");
    }

    #[test]
    fn saving_leaves_no_temp_file_behind() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        save_catalog_to(&path, &ModelCatalog::default()).unwrap();
        let stray: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != "models.json")
            .collect();
        assert!(
            stray.is_empty(),
            "no temp file may survive a save: {stray:?}"
        );
    }

    #[test]
    fn saving_preserves_unrelated_top_level_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(&path, r#"{"models":[],"future_key":42}"#).unwrap();
        let catalog = ModelCatalog {
            models: vec![entry("a")],
            default_model: Some("a".into()),
            subagent_model: None,
        };
        save_catalog_to(&path, &catalog).unwrap();

        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw.get("future_key"), Some(&serde_json::json!(42)));
        assert_eq!(load_catalog_from(&path), catalog);
    }

    /// 写入不得把「读不出来的既有文件」当成空对象从头重建——那会静默丢掉用户
    /// 的整个 `models.json`。首写（文件不存在）仍必须成功创建。
    #[test]
    fn saving_refuses_to_overwrite_a_base_it_cannot_read() {
        // (a) 文件不存在：确实没有既有内容 → 正常创建。
        let dir = tempfile::TempDir::new().unwrap();
        let fresh = dir.path().join("models.json");
        save_catalog_to(
            &fresh,
            &ModelCatalog {
                models: vec![entry("a")],
                default_model: Some("a".into()),
                subagent_model: None,
            },
        )
        .unwrap();
        assert_eq!(load_catalog_from(&fresh).models.len(), 1);

        // (b) 既有文件是损坏 JSON / 非对象：必须报错并保持文件原样，不得覆盖。
        for original in ["{ this is not valid json", r#"["not","an","object"]"#] {
            let path = dir.path().join("models.json");
            std::fs::write(&path, original).unwrap();
            let result = save_catalog_to(
                &path,
                &ModelCatalog {
                    models: vec![entry("a")],
                    default_model: Some("a".into()),
                    subagent_model: None,
                },
            );
            assert!(
                result.is_err(),
                "saving over an unreadable base must fail, not silently rebuild it"
            );
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                original,
                "an unreadable base must be left byte-identical"
            );
        }
    }

    #[test]
    fn masks_the_key_leaving_the_tail() {
        assert_eq!(mask_key("sk-secret-1234"), "••••1234");
        assert_eq!(mask_key("ab"), "••••");
        assert_eq!(mask_key(""), "••••");
    }

    #[test]
    fn entries_round_trip_the_two_new_optional_fields() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(
            &path,
            r#"{"models":[{"name":"L","provider":"openai","api_url":"u","model":"m",
            "api_key":"k","api_style":"responses","reasoning_effort":"none"}]}"#,
        )
        .unwrap();
        let catalog = load_catalog_from(&path);
        assert_eq!(catalog.models.len(), 1);
        assert_eq!(catalog.models[0].api_style.as_deref(), Some("responses"));
        assert_eq!(catalog.models[0].reasoning_effort.as_deref(), Some("none"));
    }

    #[test]
    fn legacy_entries_without_the_new_fields_still_load() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(
            &path,
            r#"{"models":[{"name":"A","provider":"openai","api_url":"u","model":"m","api_key":"k"}]}"#,
        )
        .unwrap();
        let catalog = load_catalog_from(&path);
        assert_eq!(
            catalog.models.len(),
            1,
            "missing optional fields must not skip the entry"
        );
        assert_eq!(catalog.models[0].api_style, None);
        assert_eq!(catalog.models[0].reasoning_effort, None);
    }

    /// 非字符串值（例如手写的 `5` / `["x"]`）必须退化成「未设置」，
    /// 而不能让整条 model entry 被跳过（设计 4.1：非法值按「默认 / 不发」处理）。
    #[test]
    fn non_string_new_fields_degrade_to_none_without_skipping_the_entry() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(
            &path,
            r#"{"models":[{"name":"N","provider":"openai","api_url":"u","model":"m",
            "api_key":"k","api_style":5,"reasoning_effort":["x"]}]}"#,
        )
        .unwrap();
        let catalog = load_catalog_from(&path);
        assert_eq!(
            catalog.models.len(),
            1,
            "non-string new fields must not skip the entry"
        );
        assert_eq!(catalog.models[0].api_style, None);
        assert_eq!(catalog.models[0].reasoning_effort, None);
    }

    /// 非字符串值只影响它自己那个字段：同一侧的合法字符串仍照常读出。
    #[test]
    fn non_string_new_field_does_not_clear_a_valid_sibling() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(
            &path,
            r#"{"models":[{"name":"N","provider":"openai","api_url":"u","model":"m",
            "api_key":"k","api_style":"responses","reasoning_effort":{"tier":"low"}}]}"#,
        )
        .unwrap();
        let catalog = load_catalog_from(&path);
        assert_eq!(catalog.models.len(), 1, "entry must still load");
        assert_eq!(catalog.models[0].api_style.as_deref(), Some("responses"));
        assert_eq!(catalog.models[0].reasoning_effort, None);
    }

    /// 加载期精确迁移：只有「openai 且恰为旧默认值」才改写；其余一律不动。
    #[test]
    fn legacy_openai_default_url_is_migrated_to_v1() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(
            &path,
            r#"{"models":[
                {"name":"legacy","provider":"openai","api_url":"https://api.openai.com","model":"m","api_key":"k"},
                {"name":"legacy-slash","provider":"openai","api_url":"https://api.openai.com/","model":"m","api_key":"k"},
                {"name":"custom","provider":"openai","api_url":"https://llmapi.horizon.auto","model":"m","api_key":"k"},
                {"name":"already","provider":"openai","api_url":"https://api.openai.com/v1","model":"m","api_key":"k"},
                {"name":"anth","provider":"anthropic","api_url":"https://api.openai.com","model":"m","api_key":"k"}
            ]}"#,
        )
        .unwrap();

        let catalog = load_catalog_from(&path);
        let url_of = |name: &str| {
            catalog
                .models
                .iter()
                .find(|e| e.name == name)
                .unwrap()
                .api_url
                .clone()
        };
        assert_eq!(url_of("legacy"), "https://api.openai.com/v1");
        assert_eq!(url_of("legacy-slash"), "https://api.openai.com/v1");
        // 自定义地址绝不改写——这正是方案 A 的立场。
        assert_eq!(url_of("custom"), "https://llmapi.horizon.auto");
        // 已是新默认值：幂等，不变。
        assert_eq!(url_of("already"), "https://api.openai.com/v1");
        // anthropic 条目零影响。
        assert_eq!(url_of("anth"), "https://api.openai.com");
    }

    /// 迁移只在内存里发生，不写回文件：读一次不该改用户的数据。
    #[test]
    fn migration_does_not_rewrite_the_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        let original = r#"{"models":[{"name":"legacy","provider":"openai","api_url":"https://api.openai.com","model":"m","api_key":"k"}]}"#;
        std::fs::write(&path, original).unwrap();

        let _ = load_catalog_from(&path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    /// 严格读取必须区分「文件不存在」（可判定的明确信号 → Ok 空清单）与
    /// 「不可读 / 不可解析」（→ Err），否则调用方无法实现「坏文件保留旧值」。
    #[test]
    fn try_load_distinguishes_absent_from_corrupt() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");

        // 不存在：Ok 空清单（与 cfg 回退语义一致，不是错误）。
        let absent = try_load_catalog_from(&path).expect("absent file must be Ok(empty)");
        assert!(absent.models.is_empty());
        assert!(absent.default_model.is_none());

        // 合法内容：Ok，条目就位。
        std::fs::write(
            &path,
            r#"{"models":[{"name":"A","provider":"openai","api_url":"https://a/v1","model":"m","api_key":"k"}],"default_model":"A"}"#,
        )
        .unwrap();
        let good = try_load_catalog_from(&path).expect("valid file must be Ok");
        assert_eq!(good.default_model.as_deref(), Some("A"));

        // 坏 JSON：Err（这是「损坏」，绝不能被当成空清单）。
        std::fs::write(&path, "{ not json").unwrap();
        let corrupt = try_load_catalog_from(&path);
        assert!(
            corrupt.is_err(),
            "corrupt file must be Err, got {:?}",
            corrupt.map(|c| c.models.len())
        );

        // 合法但为空清单：Ok（合法空不等于损坏）。
        std::fs::write(&path, r#"{"models":[]}"#).unwrap();
        assert!(try_load_catalog_from(&path).is_ok());
    }

    /// 严格读取必须拒绝「合法 JSON 但根本不是清单文档」（顶层不是对象）。
    ///
    /// 这类文件过去被当成 `Ok(空清单)`，于是调用方（迁移）误判「用户没配置」，
    /// 直接把它 rename 覆盖掉。`Ok(空清单)` 的契约是「**确定**没有配置」，
    /// 一份合法却非对象的文件显然不满足。
    #[test]
    fn try_load_rejects_a_document_that_is_not_a_json_object() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");

        for document in [
            r#"["a","b","c"]"#,
            r#""just a string""#,
            "42",
            "null",
            "true",
        ] {
            std::fs::write(&path, document).unwrap();
            assert!(
                try_load_catalog_from(&path).is_err(),
                "{document} is not a catalog document and must not report Ok(empty)"
            );
        }

        // 合法的空对象本身仍是「结构上可读」的清单文档 → Ok(空)。
        std::fs::write(&path, "{}").unwrap();
        assert!(try_load_catalog_from(&path).is_ok());
    }

    /// 顶层是对象、但 `models` 里**每条都被跳过**时，也不能谎报 `Ok(空清单)`。
    ///
    /// 这代表「用户写了配置、只是写坏了」（例如漏了 `api_url`）。当成空清单，
    /// 迁移就会导入并整段 rename 掉用户的 `models` 数组——静默数据丢失。
    #[test]
    fn try_load_rejects_a_catalog_whose_entries_were_all_skipped() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        // 唯一一条缺 `api_url` → 被逐条过滤跳过 → 幸存 0 条。
        std::fs::write(
            &path,
            r#"{"models":[{"name":"kept-typo","provider":"anthropic","model":"m"}]}"#,
        )
        .unwrap();
        assert!(
            try_load_catalog_from(&path).is_err(),
            "all entries skipped means the user wrote something malformed, not 'no config'"
        );

        // 反过来：只要幸存下一条，就仍是可读清单 → Ok。
        std::fs::write(
            &path,
            r#"{"models":[{"name":"ok","provider":"anthropic","api_url":"u","model":"m"},"#
                .to_string()
                + r#"{"name":"bad","provider":"anthropic","model":"m"}]}"#,
        )
        .unwrap();
        assert!(try_load_catalog_from(&path).is_ok());
    }

    /// 指纹未变时必须复用同一个 `Arc`——用 `ptr_eq` 证明「零重读」，而不是
    /// 只看值相等（值相等无法区分「重读后恰好相同」与「根本没读」）。
    #[test]
    fn watcher_reuses_the_cached_catalog_when_the_fingerprint_is_unchanged() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(&path, r#"{"models":[],"default_model":"A"}"#).unwrap();

        let watcher = CatalogWatcher::new(path.clone());
        let first = watcher.current();
        let second = watcher.current();
        assert!(
            std::sync::Arc::ptr_eq(&first, &second),
            "unchanged fingerprint must return the cached Arc, not a re-read"
        );
    }

    /// 指纹变化后必须读到新值（这是「配置改了进程要跟上」的最小断言）。
    #[test]
    fn watcher_picks_up_a_changed_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(&path, r#"{"models":[],"default_model":"OLD"}"#).unwrap();

        let watcher = CatalogWatcher::new(path.clone());
        assert_eq!(watcher.current().default_model.as_deref(), Some("OLD"));

        // 改 mtime + size 都变，确保指纹判定必然不同。
        std::fs::write(&path, r#"{"models":[],"default_model":"NEWER-VALUE"}"#).unwrap();
        assert_eq!(
            watcher.current().default_model.as_deref(),
            Some("NEWER-VALUE")
        );
    }

    /// 坏文件必须保留上一次可用值，**且不推进指纹**——这样文件修好后下一次
    /// `current()` 立即生效（本修复的核心行为）。
    #[test]
    fn watcher_keeps_last_known_good_across_a_corrupt_file_and_heals() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(&path, r#"{"models":[],"default_model":"GOOD"}"#).unwrap();

        let watcher = CatalogWatcher::new(path.clone());
        let good = watcher.current();
        assert_eq!(good.default_model.as_deref(), Some("GOOD"));

        // 坏文件：保留旧值（同一个 Arc），不 panic、不变空。
        std::fs::write(&path, "{ broken").unwrap();
        let during = watcher.current();
        assert!(
            std::sync::Arc::ptr_eq(&good, &during),
            "a corrupt file must keep the last known good catalog"
        );
        assert_eq!(during.default_model.as_deref(), Some("GOOD"));

        // 修好：下一次 current() 立即生效（不推进指纹的意义所在）。
        std::fs::write(&path, r#"{"models":[],"default_model":"HEALED"}"#).unwrap();
        assert_eq!(
            watcher.current().default_model.as_deref(),
            Some("HEALED"),
            "a fixed file must take effect on the very next call"
        );
    }

    /// 文件被删除是明确信号（不是损坏）：采纳空清单、回退 cfg。
    #[test]
    fn watcher_adopts_an_absent_file_as_empty() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(&path, r#"{"models":[],"default_model":"X"}"#).unwrap();

        let watcher = CatalogWatcher::new(path.clone());
        assert_eq!(watcher.current().default_model.as_deref(), Some("X"));

        std::fs::remove_file(&path).unwrap();
        assert!(
            watcher.current().default_model.is_none(),
            "a deleted file means 'no configured catalog', not 'keep the old one'"
        );
    }

    #[test]
    fn reasoning_capabilities_roundtrip_and_drop_unknown_tiers() {
        let json = r#"{
            "models":[{
                "name":"A","provider":"openai","api_url":"https://a","model":"m",
                "reasoning_capabilities":{
                    "chat":{"supported":["none","bogus","low"],"probed_at":123},
                    "responses":{"supported":["high"],"probed_at":456,"error":"401"}
                }
            }]
        }"#;
        let catalog = parse_catalog(json, Path::new("x")).unwrap();
        let caps = catalog.models[0].reasoning_capabilities.as_ref().unwrap();
        assert_eq!(caps.chat.as_ref().unwrap().supported, vec!["none", "low"]);
        assert_eq!(caps.chat.as_ref().unwrap().probed_at, 123);
        assert_eq!(
            caps.responses.as_ref().unwrap().error.as_deref(),
            Some("401")
        );
        assert!(caps.anthropic.is_none());
    }

    #[test]
    fn bad_capabilities_values_degrade_to_unknown_without_dropping_the_entry() {
        // 整份能力数据不是对象 → 视作未探测。
        for raw in [r#""nope""#, r#"[]"#, r#"42"#] {
            let json = format!(
                r#"{{"models":[{{"name":"A","provider":"openai","api_url":"https://a","model":"m","reasoning_capabilities":{raw}}}]}}"#
            );
            let catalog = parse_catalog(&json, Path::new("x")).unwrap();
            assert_eq!(catalog.models.len(), 1, "entry must survive: {raw}");
            assert!(
                catalog.models[0].reasoning_capabilities.is_none(),
                "non-object capabilities must read as unknown: {raw}"
            );
        }

        // 单条通道不是对象 → 该通道视作未探测，其它通道不受影响。
        let json = r#"{"models":[{"name":"A","provider":"openai","api_url":"https://a","model":"m",
            "reasoning_capabilities":{"chat":"nope","responses":{"supported":["low"]}}}]}"#;
        let catalog = parse_catalog(json, Path::new("x")).unwrap();
        let caps = catalog.models[0].reasoning_capabilities.as_ref().unwrap();
        assert!(
            caps.chat.is_none(),
            "non-object channel must read as unknown"
        );
        assert_eq!(caps.responses.as_ref().unwrap().supported, vec!["low"]);

        // 通道内字段类型错误 → 该字段降级（空表 / 0），通道本身仍存在。
        let json = r#"{"models":[{"name":"A","provider":"openai","api_url":"https://a","model":"m",
            "reasoning_capabilities":{"chat":{"supported":"nope","probed_at":"x"}}}]}"#;
        let catalog = parse_catalog(json, Path::new("x")).unwrap();
        let chat = catalog.models[0]
            .reasoning_capabilities
            .as_ref()
            .unwrap()
            .chat
            .as_ref()
            .unwrap();
        assert!(
            chat.supported.is_empty(),
            "bad supported must read as empty"
        );
        assert_eq!(chat.probed_at, 0, "bad probed_at must read as 0");
    }

    /// `from_catalog` 是固定快照：即便磁盘变了也不重读（测试与「本就持有清单」的调用方用它）。
    #[test]
    fn from_catalog_is_a_fixed_snapshot() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("models.json");
        std::fs::write(&path, r#"{"models":[],"default_model":"DISK"}"#).unwrap();

        let fixed = ModelCatalog {
            models: Vec::new(),
            default_model: Some("PINNED".into()),
            subagent_model: None,
        };
        let watcher = CatalogWatcher::from_catalog(path, fixed);
        let a = watcher.current();
        let b = watcher.current();
        assert_eq!(a.default_model.as_deref(), Some("PINNED"));
        assert!(std::sync::Arc::ptr_eq(&a, &b));
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::*;
    use crate::config::RuntimeConfig;

    /// `config.rs` exposes no reusable test constructor, so build a complete
    /// literal here. The fields the resolver rewrites are deliberately set
    /// to recognizable "base" values so the tests can tell them apart.
    fn cfg() -> RuntimeConfig {
        RuntimeConfig {
            provider: "anthropic".into(),
            api_url: "https://base".into(),
            api_key: "base-key".into(),
            model: "base-model".into(),
            // Base values for the two fields the resolver also rewrites.
            api_style: yi_agent_core::ApiStyle::Chat,
            reasoning_effort: Some("base-effort".into()),
            max_turns: 200,
            max_resident_subagents: 64,
            max_direct_children: crate::config::DIRECT_CHILDREN_DEFAULT,
            workdir: std::path::PathBuf::from("."),
            system_prompt: None,
            compact_threshold: 1000,
            compact_ratio: 80,
            model_context_length: None,
            compact_user_budget_tokens: 100,
            compact_tool_budget_tokens: 100,
            yolo: false,
            sandbox_promotable: false,
            sandbox: yi_agent_tools::SandboxMode::WorkspaceWrite,
            sandbox_writable_roots: Vec::new(),
            skills_catalog_budget: 0,
            skills_catalog_budget_explicit: false,
            model_injected: false,
        }
    }

    fn openai_entry(api_style: Option<&str>, effort: Option<&str>) -> ModelEntry {
        ModelEntry {
            name: "L".into(),
            provider: ModelProvider::Openai,
            api_url: "https://l".into(),
            model: "m".into(),
            api_key: "k".into(),
            api_style: api_style.map(str::to_string),
            reasoning_effort: effort.map(str::to_string),
            reasoning_capabilities: None,
            context_length: None,
        }
    }

    fn caps(
        chat: Option<(&[&str], Option<&str>)>,
        responses: Option<(&[&str], Option<&str>)>,
    ) -> ReasoningCapabilities {
        let conv = |v: Option<(&[&str], Option<&str>)>| {
            v.map(|(tiers, err)| ChannelCapability {
                supported: tiers.iter().map(|s| s.to_string()).collect(),
                probed_at: 1,
                error: err.map(str::to_string),
            })
        };
        ReasoningCapabilities {
            chat: conv(chat),
            responses: conv(responses),
            anthropic: None,
        }
    }

    #[test]
    fn an_unsupported_default_effort_is_dropped_at_resolve_time() {
        let catalog = ModelCatalog {
            models: vec![ModelEntry {
                reasoning_capabilities: Some(caps(Some((&["low"], None)), None)),
                ..openai_entry(Some("chat"), Some("high"))
            }],
            default_model: Some("L".into()),
            subagent_model: None,
        };
        let out = resolve_effective(&cfg(), &catalog, None);
        assert_eq!(
            out.reasoning_effort, None,
            "an unsupported tier must not be sent at all"
        );
    }

    #[test]
    fn a_supported_default_effort_survives_resolution() {
        let catalog = ModelCatalog {
            models: vec![ModelEntry {
                reasoning_capabilities: Some(caps(Some((&["low", "high"], None)), None)),
                ..openai_entry(Some("chat"), Some("high"))
            }],
            default_model: Some("L".into()),
            subagent_model: None,
        };
        assert_eq!(
            resolve_effective(&cfg(), &catalog, None).reasoning_effort,
            Some("high".to_string())
        );
    }

    #[test]
    fn an_unknown_capability_keeps_the_default_effort() {
        let entry = openai_entry(Some("chat"), Some("high"));
        assert!(effort_is_allowed(&entry, "high"), "unknown must not block");
        assert_eq!(supported_efforts(&entry), None);
    }

    #[test]
    fn a_probe_error_is_treated_as_unknown_not_as_zero_tiers() {
        let entry = ModelEntry {
            reasoning_capabilities: Some(caps(None, Some((&[], Some("HTTP 401"))))),
            ..openai_entry(Some("responses"), Some("high"))
        };
        assert_eq!(
            supported_efforts(&entry),
            None,
            "an errored probe says nothing about which tiers work"
        );
        assert!(effort_is_allowed(&entry, "high"));
    }

    /// 明确的「七档全被拒」是结论而非未知：UI 该显示零个档位，运行期必须拦下。
    #[test]
    fn an_empty_but_trustworthy_list_blocks_every_tier() {
        let entry = ModelEntry {
            reasoning_capabilities: Some(caps(Some((&[], None)), None)),
            ..openai_entry(Some("chat"), Some("high"))
        };
        assert_eq!(supported_efforts(&entry), Some(Vec::new()));
        assert!(!effort_is_allowed(&entry, "high"));
    }

    #[test]
    fn auto_prefers_responses_but_falls_back_to_a_trustworthy_chat() {
        let both = ModelEntry {
            reasoning_capabilities: Some(caps(Some((&["low"], None)), Some((&["high"], None)))),
            ..openai_entry(None, None)
        };
        assert_eq!(effective_channel(&both), "responses");
        assert_eq!(supported_efforts(&both), Some(vec!["high".to_string()]));

        let chat_only = ModelEntry {
            reasoning_capabilities: Some(caps(Some((&["low"], None)), None)),
            ..openai_entry(None, None)
        };
        assert_eq!(effective_channel(&chat_only), "chat");
        assert_eq!(supported_efforts(&chat_only), Some(vec!["low".to_string()]));

        // chat 有记录但出错、responses 没有 → 仍是 responses（未知），
        // 不能把「chat 那轮失败」当成 chat 支持集合。
        let errored_chat = ModelEntry {
            reasoning_capabilities: Some(caps(Some((&[], Some("boom"))), None)),
            ..openai_entry(None, None)
        };
        assert_eq!(effective_channel(&errored_chat), "responses");
        assert_eq!(supported_efforts(&errored_chat), None);
    }

    #[test]
    fn an_explicit_api_style_pins_the_channel() {
        let entry = ModelEntry {
            reasoning_capabilities: Some(caps(Some((&["low"], None)), Some((&["high"], None)))),
            ..openai_entry(Some("chat"), None)
        };
        assert_eq!(effective_channel(&entry), "chat");
        assert_eq!(supported_efforts(&entry), Some(vec!["low".to_string()]));
    }

    #[test]
    fn an_anthropic_entry_only_looks_at_the_anthropic_slot() {
        let entry = ModelEntry {
            reasoning_capabilities: Some(ReasoningCapabilities {
                chat: Some(ChannelCapability {
                    supported: vec!["low".into()],
                    probed_at: 1,
                    error: None,
                }),
                ..Default::default()
            }),
            ..openai_entry(None, None)
        };
        let mut entry = entry;
        entry.provider = ModelProvider::Anthropic;
        assert_eq!(effective_channel(&entry), "anthropic");
        assert_eq!(supported_efforts(&entry), None);
    }

    fn catalog() -> ModelCatalog {
        ModelCatalog {
            models: vec![
                ModelEntry {
                    name: "A".into(),
                    provider: ModelProvider::Anthropic,
                    api_url: "https://a".into(),
                    model: "model-a".into(),
                    api_key: "key-a".into(),
                    api_style: None,
                    reasoning_effort: None,
                    reasoning_capabilities: None,
                    context_length: None,
                },
                ModelEntry {
                    name: "B".into(),
                    provider: ModelProvider::Openai,
                    api_url: "https://b".into(),
                    model: "model-b".into(),
                    api_key: "key-b".into(),
                    api_style: None,
                    reasoning_effort: None,
                    reasoning_capabilities: None,
                    context_length: None,
                },
            ],
            default_model: Some("A".into()),
            subagent_model: None,
        }
    }

    #[test]
    fn a_session_override_wins() {
        let out = resolve_effective(&cfg(), &catalog(), Some("B"));
        assert_eq!(out.provider, "openai");
        assert_eq!(out.api_url, "https://b");
        assert_eq!(out.model, "model-b");
        assert_eq!(out.api_key, "key-b");
    }

    #[test]
    fn a_dangling_override_falls_back_to_the_default() {
        let out = resolve_effective(&cfg(), &catalog(), Some("gone"));
        assert_eq!(out.model, "model-a");
    }

    #[test]
    fn an_injection_beats_the_catalog_default() {
        let mut base = cfg();
        base.model_injected = true;
        base.model = "injected-model".into();
        let out = resolve_effective(&base, &catalog(), None);
        assert_eq!(out.model, "injected-model");
    }

    #[test]
    fn a_session_override_still_beats_an_injection() {
        let mut base = cfg();
        base.model_injected = true;
        base.model = "injected-model".into();
        let out = resolve_effective(&base, &catalog(), Some("B"));
        assert_eq!(out.model, "model-b");
    }

    #[test]
    fn no_default_falls_back_to_cfg() {
        let base = cfg();
        let empty = ModelCatalog::default();
        let out = resolve_effective(&base, &empty, None);
        assert_eq!(out.model, base.model);
        assert_eq!(out.api_url, base.api_url);
    }

    /// 条目改写的是四个身份字段（provider/api_url/api_key/model）与两个通道字段
    /// （api_style/reasoning_effort）；其余字段必须原样带过。
    #[test]
    fn only_the_model_identity_fields_change() {
        let base = cfg();
        let out = resolve_effective(&base, &catalog(), Some("B"));
        assert_eq!(out.workdir, base.workdir);
        assert_eq!(out.sandbox, base.sandbox);
        assert_eq!(out.max_turns, base.max_turns);
    }

    #[test]
    fn subagent_falls_back_to_default_then_cfg() {
        let cat = catalog();
        assert_eq!(subagent_entry(&cat).map(|e| e.name.as_str()), Some("A"));
        let mut cat2 = cat.clone();
        cat2.subagent_model = Some("B".into());
        assert_eq!(subagent_entry(&cat2).map(|e| e.name.as_str()), Some("B"));
    }

    #[test]
    fn resolve_effective_carries_api_style_and_effort_from_the_entry() {
        let mut cat = catalog();
        cat.models[1].api_style = Some("responses".into());
        cat.models[1].reasoning_effort = Some("low".into());
        let out = resolve_effective(&cfg(), &cat, Some("B"));
        assert_eq!(out.reasoning_effort.as_deref(), Some("low"));
        assert_eq!(out.api_style.as_str(), "responses");
    }

    #[test]
    fn resolve_effective_defaults_to_auto_and_no_effort_when_entry_silent() {
        let out = resolve_effective(&cfg(), &catalog(), Some("A"));
        assert_eq!(out.reasoning_effort, None);
        assert_eq!(out.api_style.as_str(), "auto");
    }

    #[test]
    fn an_entrys_context_length_recomputes_the_compact_threshold() {
        let mut cat = catalog();
        cat.models[1].context_length = Some(100_000);
        // cfg() defaults to compact_ratio 80.
        let out = resolve_effective(&cfg(), &cat, Some("B"));
        assert_eq!(out.compact_threshold, 80_000);
    }

    #[test]
    fn a_silent_context_length_leaves_the_threshold_alone() {
        let base = cfg();
        let out = resolve_effective(&base, &catalog(), Some("A"));
        assert_eq!(out.compact_threshold, base.compact_threshold);
    }

    /// `context_length: 0` 是手写 `models.json` 里可达的输入。它必须被当成"未声明"：
    /// 阈值保持 cfg 原值，否则 `compact_threshold == 0` 会让每一轮都触发压缩。
    #[test]
    fn a_zero_context_length_is_treated_as_silent() {
        let base = cfg();
        let mut cat = catalog();
        cat.models[1].context_length = Some(0);
        let out = resolve_effective(&base, &cat, Some("B"));
        assert_eq!(out.compact_threshold, base.compact_threshold);
    }

    /// 一个被误写成原始窗口/字符数的超大值不能在 `length * ratio` 上溢出：
    /// debug 构建会 panic（会话启动路径），release 会静默回绕。饱和乘法必须让它算完。
    #[test]
    fn a_huge_context_length_saturates_instead_of_overflowing() {
        let mut cat = catalog();
        cat.models[1].context_length = Some(1_000_000_000);
        // cfg() 的 compact_ratio 为 80（非零），正是会溢出的倍数。
        let out = resolve_effective(&cfg(), &cat, Some("B"));
        assert_eq!(
            out.compact_threshold,
            1_000_000_000u32.saturating_mul(80) / 100
        );
    }
}
