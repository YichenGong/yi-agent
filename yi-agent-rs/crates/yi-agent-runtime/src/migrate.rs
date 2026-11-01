//! 一次性迁移：把存量 `.env` 里的活跃模型键收边进清单，并注释掉它们。
//!
//! # 退役计划：`2027-01-01`
//!
//! 本模块已在 2026 年被收缩为**纯粹的读取侧兼容层**——产品的配置读取路径早已
//! 完全不碰 `.env`（见 [`crate::config`] 的模块文档），这里只负责把「从旧版本
//! 升级上来的用户」的存量配置搬进 `models.json` / `settings.json` / `websearch.json`，
//! 然后把 `.env` 改名成 `.env.migrated`。全新安装的机器上它每次启动只是
//! `read_to_string` 拿到 `NotFound` 就返回（`migrate_env_settings` 的
//! `EnvMigrationOutcome::NothingToDo`），不产生任何行为。
//!
//! **保留的代价**：约 1200 行代码 + [`crate::settings::parse_env_text`]，以及
//! 「迁移必须排在 `config::load` 之前」「先落清单、后改名」这两条只能在测试里
//! 守住的不变量。
//!
//! **到期（`2027-01-01` 之后）整块删除**，理由是这个日期足够长，任何还在跑旧
//! 版本的用户有充分机会安装过一个带迁移的版本。届时按下面这份清单逐条删，
//! 删完本模块即可整体移除：
//!
//! 1. 本文件：`migrate_model_keys`、`migrate_env_settings`、`log_outcome`、
//!    `log_env_settings_outcome`，以及本文件的 30 个测试。
//! 2. [`crate::settings::parse_env_text`]：本模块（`migrate_env_settings`）是它
//!    唯一的生产消费者。
//! 3. [`crate::config::resolve_global_env_path`]：只为定位那个待迁移的文件而存在。
//! 4. 四个生产入口的接线：`yi-agent-rs/crates/yi-agent/src/main.rs` 的
//!    `Command::Web` 分支与 `prepare_environment`，
//!    `yi-agent-rs/crates/yi-agent-app-server/src/server.rs` 的
//!    `migrate_legacy_model_keys_on_startup`（`run` / `serve_stdio_with_relay` 各调一次）。
//! 5. **三个钉住接线的测试**必须同批处理，否则会红：
//!    `server.rs` 的 `the_startup_migration_runs_both_model_and_settings_migrations`
//!    与 `the_ws_entry_runs_the_startup_migration`（都用 `include_str!` + 字符串
//!    匹配断言「迁移挂在启动路径上」），以及
//!    `yi-agent-rs/crates/yi-agent/tests/settings_migration.rs` 的两条端到端用例。
//!
//! 删除时的唯一风险：**如果还有用户从未安装过带迁移的版本**，他们的 `.env` 里
//! 的 key 就再也读不出来（表现为「模型未配置」）。因此删之前应先确认最低支持
//! 版本已经高于本次迁移发布的那一版。跟踪项见
//! `docs/project-management/yi-agent-runtime.md` 的「待删」条目。

use std::path::Path;

use crate::config::MODEL_ENV_KEYS;

#[derive(Debug, PartialEq, Eq)]
pub enum MigrationOutcome {
    NothingToDo,
    Migrated {
        entry: String,
    },
    AlreadyCategorized,
    EnvNotWritable {
        reason: String,
    },
    CatalogNotWritable {
        reason: String,
    },
    /// `.env` 里的 `YI_AGENT_PROVIDER` 认不出来（用户输入错误）。一行都没写：
    /// 既没落清单，也没注释 `.env`，用户可以改正后重跑。
    InvalidProvider {
        reason: String,
    },
    /// 旧 `.env` 里有 provider 与 key，但没有模型标识：无从得知用户想用哪个
    /// 模型。不落清单、不改 `.env`，留给引导去问。
    MissingModel,
}

fn is_model_key(key: &str) -> bool {
    MODEL_ENV_KEYS.contains(&key)
}

/// 扫描 `.env`，返回活跃（未被注释）模型键的 `key -> value`。
fn active_model_keys(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some(eq) = trimmed.find('=') else {
            continue;
        };
        let key = trimmed[..eq].trim();
        if is_model_key(key) {
            out.push((
                key.to_string(),
                trimmed[eq + 1..]
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string(),
            ));
        }
    }
    out
}

/// 把活跃模型键行改写为 `# MIGRATED: <原行>`，其余行逐字节保留。
fn comment_out_model_keys(text: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        let active = !trimmed.is_empty() && !trimmed.starts_with('#');
        let key = trimmed
            .find('=')
            .map(|eq| trimmed[..eq].trim())
            .unwrap_or_default();
        if active && is_model_key(key) {
            let indent_len = line.len() - line.trim_start().len();
            lines.push(format!("{}# MIGRATED: {}", &line[..indent_len], trimmed));
        } else {
            lines.push(line.to_string());
        }
    }
    let mut joined = lines.join("\n");
    if text.ends_with('\n') {
        joined.push('\n');
    }
    joined
}

pub fn migrate_model_keys(env_path: &Path, models_path: &Path) -> MigrationOutcome {
    let text = match std::fs::read_to_string(env_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return MigrationOutcome::NothingToDo;
        }
        Err(error) => {
            return MigrationOutcome::EnvNotWritable {
                reason: error.to_string(),
            };
        }
    };
    let active = active_model_keys(&text);
    if active.is_empty() {
        return MigrationOutcome::NothingToDo;
    }

    // 1) 先落清单：只有清单为空时才导入，绝不覆盖用户既有配置。
    //
    // 用严格读取区分「文件不存在」（= 确定没配置，可安全导入）与
    // 「文件在但读不出来 / 解析不了」（= 用户有配置、只是坏了一次）。宽容的
    // `load_catalog_from` 把后者也当成空清单，会让 `save_catalog_to` 的 rename
    // 直接盖掉用户整份清单——数据丢失，且毫无提示。
    let existing = match crate::models::try_load_catalog_from(models_path) {
        Ok(catalog) => catalog,
        Err(reason) => {
            // 清单不可安全写入 → 绝不注释 .env（否则用户的 key 凭空丢失）。
            return MigrationOutcome::CatalogNotWritable { reason };
        }
    };
    let outcome = if existing.models.is_empty() {
        let get = |key: &str| {
            active
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        // provider 必须先定：下面的同名键回退要按「最终 provider」选键。
        let provider = {
            let declared = get("YI_AGENT_PROVIDER");
            if declared.trim().is_empty() {
                "anthropic".to_string()
            } else {
                declared
            }
        };
        // 无法识别的 provider 是**用户 .env 里的输入错误**，不是存储故障：
        // 此时一行都还没写。用 `CatalogNotWritable` 描述它会让 Task 6 的
        // `log_outcome` 报出一个并不存在的「清单不可写」，掩盖真正的原因。
        let Some(parsed) = crate::models::ModelProvider::parse(&provider) else {
            return MigrationOutcome::InvalidProvider {
                reason: format!("unknown provider: {provider}"),
            };
        };
        // provider 专属键：仅按已解析的 provider 选，不按「哪个键存在」推断 provider。
        let (named_key, named_url) = match parsed {
            crate::models::ModelProvider::Openai => ("OPENAI_API_KEY", "OPENAI_BASE_URL"),
            crate::models::ModelProvider::Anthropic => ("ANTHROPIC_API_KEY", "ANTHROPIC_BASE_URL"),
        };
        // 通用键优先；缺失 / 空 → 回退到 provider 专属键（否则这些键会被注释掉却收不进清单）。
        let first_non_empty = |candidates: [&str; 2]| {
            candidates
                .into_iter()
                .map(get)
                .find(|value| !value.trim().is_empty())
                .unwrap_or_default()
        };
        let settings = crate::onboarding::ModelSettings {
            provider,
            model: get("YI_AGENT_MODEL"),
            api_url: first_non_empty(["MODEL_API_URL", named_url]),
            api_key: first_non_empty(["MODEL_API_KEY", named_key]),
        };
        // 没有模型标识就没有可落盘的条目：`parse_catalog` 会丢弃 model 为空的
        // 条目，硬写只会得到"清单看似写了、读回却是空的"。也绝不猜一个名字——
        // 那正是本次要修的问题。
        if settings.model.trim().is_empty() {
            return MigrationOutcome::MissingModel;
        }
        let mut catalog = existing.clone();
        let name = {
            let base = settings.model.trim().to_string();
            let mut candidate = base.clone();
            let mut n = 2u32;
            while catalog.models.iter().any(|m| m.name == candidate) {
                candidate = format!("{base}-{n}");
                n += 1;
            }
            candidate
        };
        let default_url = match parsed {
            crate::models::ModelProvider::Openai => "https://api.openai.com/v1",
            crate::models::ModelProvider::Anthropic => "https://api.anthropic.com",
        };
        let api_url = if settings.api_url.trim().is_empty() {
            default_url.to_string()
        } else {
            settings.api_url.clone()
        };
        // 上面已保证非空：这里只做去空白，不再有"猜一个默认名"的分支。
        let model = settings.model.trim().to_string();
        let context_length = get("YI_AGENT_MODEL_CONTEXT_LENGTH").parse::<u32>().ok();
        catalog.models.push(crate::models::ModelEntry {
            name: name.clone(),
            provider: parsed,
            api_url,
            model,
            api_key: settings.api_key.clone(),
            api_style: None,
            reasoning_effort: None,
            reasoning_capabilities: None,
            context_length,
        });
        catalog.default_model = Some(name.clone());
        if let Err(error) = crate::models::save_catalog_to(models_path, &catalog) {
            // 清单没落成 → 绝不注释 .env（否则用户的 key 凭空丢失）。
            return MigrationOutcome::CatalogNotWritable {
                reason: error.to_string(),
            };
        }
        MigrationOutcome::Migrated { entry: name }
    } else {
        MigrationOutcome::AlreadyCategorized
    };

    // 2) 后注释 .env：原子写，失败只降级为 warning，不改变已落的清单。
    let rewritten = comment_out_model_keys(&text);
    if let Err(error) = atomic_write(env_path, &rewritten) {
        return MigrationOutcome::EnvNotWritable {
            reason: error.to_string(),
        };
    }
    outcome
}

/// 把迁移结果记成日志。三个触发点共用，保证每种结果都有可见、可区分的措辞。
///
/// `NothingToDo` 是常态（没有存量模型键），静默；其余四种都值得一条 warning：
/// 迁移成功要告知用户 `.env` 里的键已被注释，而三种降级（清单已接管 / `.env`
/// 不可写 / 清单不可写）分别对应「键已成死配置」「键被忽略」「键未动、可重试」，
/// 措辞必须能把它们分开。
pub fn log_outcome(outcome: &MigrationOutcome, env_path: &Path) {
    match outcome {
        MigrationOutcome::NothingToDo => {}
        MigrationOutcome::Migrated { entry } => tracing::warn!(
            entry = %entry,
            path = %env_path.display(),
            "migrated model settings from .env into models.json; the .env lines were commented out"
        ),
        MigrationOutcome::AlreadyCategorized => tracing::warn!(
            path = %env_path.display(),
            "the model keys in .env are no longer read; remove them or use the settings UI"
        ),
        MigrationOutcome::EnvNotWritable { reason } => tracing::warn!(
            path = %env_path.display(),
            %reason,
            "could not comment out the legacy model keys in .env; they are ignored regardless"
        ),
        MigrationOutcome::CatalogNotWritable { reason } => tracing::warn!(
            path = %env_path.display(),
            %reason,
            "could not migrate the legacy model keys; .env was left untouched"
        ),
        MigrationOutcome::MissingModel => tracing::warn!(
            path = %env_path.display(),
            "legacy model keys in .env have no YI_AGENT_MODEL; .env was left untouched — \
             finish model setup to create a catalog entry"
        ),
        MigrationOutcome::InvalidProvider { reason } => tracing::warn!(
            path = %env_path.display(),
            %reason,
            "could not migrate the legacy model keys: YI_AGENT_PROVIDER in .env names an \
             unknown provider; .env was left untouched"
        ),
    }
}

/// 原子写：临时文件 + rename，权限 0600（.env 含密钥）。
fn atomic_write(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(".env.{}.migrate.tmp", std::process::id()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(text.as_bytes())?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&tmp, text)?;
    }
    std::fs::rename(&tmp, path)
}

/// A 类键迁移的结果。与 [`MigrationOutcome`] 分开：模型键与运行参数是两条独立
/// 的一次性迁移，混用一个枚举会让日志措辞打架。
#[derive(Debug, PartialEq, Eq)]
pub enum EnvMigrationOutcome {
    /// `.env` 不存在，或里面没有任何 A 类键、也没有已废弃的 `YI_AGENT_WORKDIR` /
    /// `YI_AGENT_YOLO`。
    NothingToDo,
    /// 已落盘并改名。
    Migrated { fields: usize, bocha: bool },
    /// `.env` 里只有已废弃的键（`YI_AGENT_WORKDIR` / `YI_AGENT_YOLO`），没有可迁移的
    /// 值。文件已改名成 `.env.migrated`，`settings.json` 与 `websearch.json` 未创建。
    ///
    /// 为什么仍要改名：这两个键已经没有任何读取方，留着只会让每次启动都重复同一条
    /// 告警，而文件本身永远不会消失。改名即「这件事已经处理完了」。
    DroppedOnly,
    /// `.env` 读不出来。`settings.json` 未动。
    EnvUnreadable { reason: String },
    /// `settings.json`（或 `websearch.json`）写不进去。`.env` 未改名，可重试。
    SettingsNotWritable { reason: String },
}

/// 把 `.env` 里的 A 类键迁进 `settings.json`，`BOCHA_API_KEY` 迁进 `websearch.json`，
/// 成功后把 `.env` 改名为 `.env.migrated`。
///
/// **必须与 [`migrate_model_keys`] 在同一处、且在 `RuntimeConfig::load_lenient`
/// 之前调用。** 顺序错了会造出半迁移：`.env` 已被注入进程环境又被改名，配置看起来
/// 迁了、重启后没了。
///
/// **幂等**：第二次调用时 `.env` 已改名，读不到，返回 `NothingToDo` 且不碰
/// `settings.json`。
pub fn migrate_env_settings(
    env_path: &Path,
    settings_path: &Path,
    websearch_path: &Path,
) -> EnvMigrationOutcome {
    let text = match std::fs::read_to_string(env_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return EnvMigrationOutcome::NothingToDo;
        }
        Err(error) => {
            return EnvMigrationOutcome::EnvUnreadable {
                reason: error.to_string(),
            };
        }
    };

    let values = crate::settings::parse_env_text(&text);

    // (点路径键, .env 键)。顺序即日志里 `fields` 的计数依据。
    const A_CLASS: &[(&str, &str)] = &[
        ("agent.max_turns", "YI_AGENT_MAX_TURNS"),
        ("agent.max_direct_children", "YI_AGENT_MAX_DIRECT_CHILDREN"),
        (
            "agent.max_resident_subagents",
            "YI_AGENT_MAX_RESIDENT_SUBAGENTS",
        ),
        ("agent.system_prompt", "YI_AGENT_SYSTEM_PROMPT"),
        (
            "agent.skills_catalog_budget",
            "YI_AGENT_SKILLS_CATALOG_BUDGET",
        ),
        ("compact.ratio", "YI_AGENT_COMPACT_RATIO"),
        (
            "compact.user_budget_tokens",
            "YI_AGENT_COMPACT_USER_BUDGET_TOKENS",
        ),
        (
            "compact.tool_budget_tokens",
            "YI_AGENT_COMPACT_TOOL_BUDGET_TOKENS",
        ),
        ("startup.sandbox", "YI_AGENT_SANDBOX"),
    ];

    // 只填「尚未设置」的字段：既有 `settings.json` 的值优先。
    let existing = crate::settings::load_from(settings_path);
    let mut patch = serde_json::Map::new();
    for (path_key, env_key) in A_CLASS {
        let Some(raw) = values.get(*env_key) else {
            continue;
        };
        if raw.trim().is_empty() {
            continue;
        }
        if field_is_set(&existing, path_key) {
            continue;
        }
        patch.insert(
            (*path_key).to_string(),
            serde_json::Value::from(raw.clone()),
        );
    }

    let bocha_key = values
        .get("BOCHA_API_KEY")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    let has_workdir = values.contains_key("YI_AGENT_WORKDIR");
    let has_yolo = values.contains_key("YI_AGENT_YOLO");

    if patch.is_empty() && bocha_key.is_none() {
        if !has_workdir && !has_yolo {
            // 既没有 A 类键也没有废弃键：这不是我们的文件，原样留着。
            return EnvMigrationOutcome::NothingToDo;
        }
        // 只有废弃键。它们已无读取方，留着只会让每次启动重复同一条告警，
        // 故把所有废弃键逐条 warn 一遍后改名，让 `.env` 彻底退场。
        if has_workdir {
            tracing::warn!(
                path = %env_path.display(),
                "YI_AGENT_WORKDIR is no longer read; workdir now comes from --workdir, \
                 the launch directory, or the project picked in the desktop app"
            );
        }
        if has_yolo {
            tracing::warn!(
                path = %env_path.display(),
                "YI_AGENT_YOLO is no longer read; pass --yolo on every launch instead"
            );
        }
        return match std::fs::rename(env_path, env_path.with_file_name(".env.migrated")) {
            Ok(()) => EnvMigrationOutcome::DroppedOnly,
            Err(error) => EnvMigrationOutcome::SettingsNotWritable {
                reason: format!("could not rename .env to .env.migrated: {error}"),
            },
        };
    }

    // 1) 先落 settings.json：写不进去就绝不改名 `.env`（否则用户的值凭空丢失）。
    let mut migrated_fields = 0usize;
    if !patch.is_empty() {
        // 逐个字段写入：单个字段的非法值只跳过它本身，不拖垮整次迁移。
        for (key, value) in patch {
            let single = serde_json::Value::Object([(key.clone(), value)].into_iter().collect());
            match crate::settings::write_to(settings_path, &single) {
                Ok(()) => migrated_fields += 1,
                Err(error) => {
                    // 键不认识 / 类型不对 / 越界：都是「这个值迁不了」，不是存储故障。
                    // 存储故障（文件损坏 / IO）则必须中止。
                    match error {
                        crate::settings::SettingsError::InvalidSetting { .. } => {
                            tracing::warn!(
                                key = %key,
                                %error,
                                "skipping a .env value that settings.json cannot accept"
                            );
                        }
                        other => {
                            return EnvMigrationOutcome::SettingsNotWritable {
                                reason: other.to_string(),
                            };
                        }
                    }
                }
            }
        }
    }

    // 2) 再落 websearch.json：只在文件里没有非空 key 时写。
    let mut bocha_migrated = false;
    if let Some(key) = bocha_key {
        let config = yi_agent_tools::load_websearch_config_from(websearch_path);
        let already = config
            .engines
            .get("bocha")
            .map(|engine| !engine.api_key.trim().is_empty())
            .unwrap_or(false);
        if !already {
            let mut config = config;
            config
                .engines
                .entry("bocha".to_string())
                .or_default()
                .api_key = key;
            match save_websearch_config(websearch_path, &config) {
                Ok(()) => bocha_migrated = true,
                Err(error) => {
                    return EnvMigrationOutcome::SettingsNotWritable {
                        reason: format!("websearch.json: {error}"),
                    };
                }
            }
        }
    }

    // 3) 最后改名 `.env`：保留唯一一份明文 key 供用户取回。
    let renamed = env_path.with_file_name(".env.migrated");
    if let Err(error) = std::fs::rename(env_path, &renamed) {
        return EnvMigrationOutcome::EnvUnreadable {
            reason: format!("could not rename .env to .env.migrated: {error}"),
        };
    }
    tracing::info!(
        from = %env_path.display(),
        to = %renamed.display(),
        "migrated .env runtime settings into settings.json; the old file was renamed for recovery"
    );

    if has_workdir || has_yolo {
        tracing::warn!(
            "YI_AGENT_WORKDIR / YI_AGENT_YOLO are no longer read; workdir now comes from \
             --workdir or the launch directory, and yolo must be passed on every launch"
        );
    }

    EnvMigrationOutcome::Migrated {
        fields: migrated_fields,
        bocha: bocha_migrated,
    }
}

/// 某个点路径字段在既有 `Settings` 里是否已有值。
fn field_is_set(settings: &crate::settings::Settings, key: &str) -> bool {
    match key {
        "agent.max_turns" => settings.agent.max_turns.is_some(),
        "agent.max_direct_children" => settings.agent.max_direct_children.is_some(),
        "agent.max_resident_subagents" => settings.agent.max_resident_subagents.is_some(),
        "agent.system_prompt" => settings.agent.system_prompt.is_some(),
        "agent.skills_catalog_budget" => settings.agent.skills_catalog_budget.is_some(),
        "compact.ratio" => settings.compact.ratio.is_some(),
        "compact.user_budget_tokens" => settings.compact.user_budget_tokens.is_some(),
        "compact.tool_budget_tokens" => settings.compact.tool_budget_tokens.is_some(),
        "startup.sandbox" => settings.startup.sandbox.is_some(),
        "tui.notifications" => settings.tui.notifications.is_some(),
        "tui.notification_method" => settings.tui.notification_method.is_some(),
        "tui.notification_condition" => settings.tui.notification_condition.is_some(),
        _ => false,
    }
}

/// `websearch.json` 的原子落盘。与 `websearch_rpc::save_config` 同一套语义
/// （读-改-写保留未知键），但那一个是私有的且属于 app-server，故此处独立实现。
fn save_websearch_config(
    path: &Path,
    config: &yi_agent_tools::WebSearchConfig,
) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut object = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    if let Some(ours) = serde_json::to_value(config)
        .map_err(std::io::Error::other)?
        .as_object()
    {
        for (key, value) in ours {
            object.insert(key.clone(), value.clone());
        }
    }
    let text = serde_json::to_vec_pretty(&serde_json::Value::Object(object))
        .map_err(std::io::Error::other)?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!("websearch.json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// 把 A 类迁移结果记成日志。`NothingToDo` 是常态，静默。
pub fn log_env_settings_outcome(outcome: &EnvMigrationOutcome, env_path: &Path) {
    match outcome {
        EnvMigrationOutcome::NothingToDo => {}
        EnvMigrationOutcome::DroppedOnly => tracing::warn!(
            path = %env_path.display(),
            "the .env file held only retired keys (YI_AGENT_WORKDIR / YI_AGENT_YOLO); \
             it was renamed to .env.migrated"
        ),
        EnvMigrationOutcome::Migrated { fields, bocha } => tracing::warn!(
            path = %env_path.display(),
            fields = *fields,
            bocha = *bocha,
            "migrated runtime settings from .env into settings.json; .env was renamed to .env.migrated"
        ),
        EnvMigrationOutcome::EnvUnreadable { reason } => tracing::warn!(
            path = %env_path.display(),
            %reason,
            "could not migrate .env runtime settings; nothing was changed"
        ),
        EnvMigrationOutcome::SettingsNotWritable { reason } => tracing::warn!(
            path = %env_path.display(),
            %reason,
            "could not migrate .env runtime settings; .env was left untouched, will retry next start"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_env(dir: &Path, text: &str) -> std::path::PathBuf {
        let path = dir.join(".env");
        std::fs::write(&path, text).unwrap();
        path
    }

    /// 旧 `.env` 有 provider 和 key、却没有模型标识时，绝不能猜一个名字：
    /// 猜出来的条目会让 `subagent` 之类"看起来已配置"的路径拿到一个用户
    /// 从未确认过的模型。这里也不动 `.env`，用户还能补上模型标识后重跑。
    #[test]
    fn a_model_less_env_is_not_turned_into_an_invented_entry() {
        let dir = tempfile::TempDir::new().unwrap();
        let env = write_env(dir.path(), "YI_AGENT_PROVIDER=openai\nMODEL_API_KEY=sk-x\n");
        let models = dir.path().join("models.json");
        assert_eq!(
            migrate_model_keys(&env, &models),
            MigrationOutcome::MissingModel
        );
        assert!(!models.exists(), "nothing may be written without a model");
        let text = std::fs::read_to_string(&env).unwrap();
        assert!(
            text.contains("MODEL_API_KEY=sk-x"),
            "the key must stay put so the user can finish setup: {text}"
        );
    }

    #[test]
    fn an_empty_env_is_nothing_to_do() {
        let dir = tempfile::TempDir::new().unwrap();
        let env = write_env(dir.path(), "YI_AGENT_MAX_TURNS=20\n");
        let models = dir.path().join("models.json");
        assert_eq!(
            migrate_model_keys(&env, &models),
            MigrationOutcome::NothingToDo
        );
        assert!(!models.exists());
    }

    #[test]
    fn an_empty_catalog_imports_and_comments_out_the_env_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        let env = write_env(
            dir.path(),
            "YI_AGENT_PROVIDER=openai\nYI_AGENT_MODEL=gpt-4o\nMODEL_API_KEY=sk-x\nYI_AGENT_MAX_TURNS=20\n",
        );
        let models = dir.path().join("models.json");
        assert_eq!(
            migrate_model_keys(&env, &models),
            MigrationOutcome::Migrated {
                entry: "gpt-4o".into()
            }
        );

        let catalog = crate::models::load_catalog_from(&models);
        assert_eq!(catalog.default_model.as_deref(), Some("gpt-4o"));
        assert_eq!(catalog.models[0].api_key, "sk-x");

        let text = std::fs::read_to_string(&env).unwrap();
        assert!(text.contains("# MIGRATED: YI_AGENT_PROVIDER=openai"));
        assert!(!text.contains("\nYI_AGENT_PROVIDER=openai"));
        assert!(
            text.contains("YI_AGENT_MAX_TURNS=20"),
            "non-model keys must survive"
        );
    }

    #[test]
    fn a_populated_catalog_is_not_overwritten() {
        let dir = tempfile::TempDir::new().unwrap();
        let models = dir.path().join("models.json");
        let mut catalog = crate::models::ModelCatalog::default();
        catalog.models.push(crate::models::ModelEntry {
            name: "keep-me".into(),
            provider: crate::models::ModelProvider::Anthropic,
            api_url: "https://a".into(),
            model: "claude-x".into(),
            api_key: "sk-keep".into(),
            api_style: None,
            reasoning_effort: None,
            reasoning_capabilities: None,
            context_length: None,
        });
        crate::models::save_catalog_to(&models, &catalog).unwrap();

        let env = write_env(dir.path(), "MODEL_API_KEY=sk-stale\n");
        assert_eq!(
            migrate_model_keys(&env, &models),
            MigrationOutcome::AlreadyCategorized
        );
        let after = crate::models::load_catalog_from(&models);
        assert_eq!(after.models.len(), 1);
        assert_eq!(after.models[0].name, "keep-me");
        assert!(
            std::fs::read_to_string(&env)
                .unwrap()
                .contains("# MIGRATED: MODEL_API_KEY=sk-stale")
        );
    }

    #[test]
    fn migration_is_idempotent() {
        let dir = tempfile::TempDir::new().unwrap();
        let env = write_env(
            dir.path(),
            "YI_AGENT_PROVIDER=openai\nYI_AGENT_MODEL=gpt-4o\nMODEL_API_KEY=sk-x\n",
        );
        let models = dir.path().join("models.json");
        migrate_model_keys(&env, &models);
        assert_eq!(
            migrate_model_keys(&env, &models),
            MigrationOutcome::NothingToDo
        );
    }

    #[test]
    fn a_read_only_env_dir_degrades_without_losing_the_catalog() {
        let dir = tempfile::TempDir::new().unwrap();
        // 把 .env 放进只读子目录：原子写的临时文件创建会失败（rename 只需目录可写，
        // 所以只 chmod 文件本身测不出来）。models.json 留在可写的根目录。
        let ro = dir.path().join("ro");
        std::fs::create_dir_all(&ro).unwrap();
        let env = write_env(
            &ro,
            "YI_AGENT_PROVIDER=anthropic\nYI_AGENT_MODEL=m\nMODEL_API_KEY=sk-x\n",
        );
        let models = dir.path().join("models.json");
        let mut perms = std::fs::metadata(&ro).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&ro, perms).unwrap();

        match migrate_model_keys(&env, &models) {
            MigrationOutcome::EnvNotWritable { .. } => {}
            other => panic!("expected EnvNotWritable, got {other:?}"),
        }
        // The catalog write already happened; the key is not lost.
        let catalog = crate::models::load_catalog_from(&models);
        assert_eq!(catalog.models[0].api_key, "sk-x");

        // 恢复可写以便 TempDir 清理。这里用显式 mode 位而非 `set_readonly(false)`：
        // 后者既不改 owner 写位（目录仍不可写），又会触发
        // `clippy::permissions_set_readonly_false`（CI 以 `-D warnings` 跑 clippy）。
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    #[test]
    fn a_corrupt_catalog_is_never_clobbered() {
        let dir = tempfile::TempDir::new().unwrap();
        let models = dir.path().join("models.json");
        let garbage = "{ this is not valid json";
        std::fs::write(&models, garbage).unwrap();
        let env_text = "YI_AGENT_PROVIDER=anthropic\nYI_AGENT_MODEL=m\nMODEL_API_KEY=sk-x\nYI_AGENT_MAX_TURNS=20\n";
        let env = write_env(dir.path(), env_text);

        match migrate_model_keys(&env, &models) {
            MigrationOutcome::CatalogNotWritable { .. } => {}
            other => panic!("expected CatalogNotWritable, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(&models).unwrap(),
            garbage,
            "a corrupt models.json must be left byte-identical (never renamed over)"
        );
        assert_eq!(
            std::fs::read_to_string(&env).unwrap(),
            env_text,
            ".env must be untouched: the key must not be commented out"
        );
    }

    /// 迁移同样不得覆盖「合法 JSON 但不是清单文档」的 `models.json`。
    ///
    /// 与上面的损坏文件是**同一类**丢失（用户文件被 rename 掉），只是入口不同：
    /// 顶层非对象的合法 JSON 曾被当成空清单，于是 `migrate` 往里导入并覆盖。
    #[test]
    fn a_non_object_catalog_is_never_clobbered() {
        let dir = tempfile::TempDir::new().unwrap();
        let models = dir.path().join("models.json");
        let original = r#"["something","the","user","wrote"]"#;
        std::fs::write(&models, original).unwrap();
        let env_text = "MODEL_API_KEY=sk-x\n";
        let env = write_env(dir.path(), env_text);

        match migrate_model_keys(&env, &models) {
            MigrationOutcome::CatalogNotWritable { .. } => {}
            other => panic!("expected CatalogNotWritable, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(&models).unwrap(),
            original,
            "a non-object models.json must be left byte-identical"
        );
        assert_eq!(
            std::fs::read_to_string(&env).unwrap(),
            env_text,
            ".env must be untouched: the key must not be commented out"
        );
    }

    /// 迁移不得覆盖「条目全被跳过」的 `models.json`（同类丢失的第二个入口）。
    #[test]
    fn an_all_entries_skipped_catalog_is_never_clobbered() {
        let dir = tempfile::TempDir::new().unwrap();
        let models = dir.path().join("models.json");
        let original = r#"{"models":[{"name":"kept-typo","provider":"anthropic","model":"m"}]}"#;
        std::fs::write(&models, original).unwrap();
        let env_text = "MODEL_API_KEY=sk-x\n";
        let env = write_env(dir.path(), env_text);

        match migrate_model_keys(&env, &models) {
            MigrationOutcome::CatalogNotWritable { .. } => {}
            other => panic!("expected CatalogNotWritable, got {other:?}"),
        }
        assert_eq!(
            std::fs::read_to_string(&models).unwrap(),
            original,
            "a catalog whose entries were all skipped must be left byte-identical"
        );
    }

    #[test]
    fn a_provider_named_anthropic_key_is_imported_not_stranded() {
        let dir = tempfile::TempDir::new().unwrap();
        let env = write_env(
            dir.path(),
            "YI_AGENT_MODEL=m\nANTHROPIC_API_KEY=sk-ant-x\nANTHROPIC_BASE_URL=https://anthropic-proxy.example\n",
        );
        let models = dir.path().join("models.json");
        assert_eq!(
            migrate_model_keys(&env, &models),
            MigrationOutcome::Migrated { entry: "m".into() }
        );

        let catalog = crate::models::load_catalog_from(&models);
        assert_eq!(catalog.models[0].api_key, "sk-ant-x");
        assert_eq!(catalog.models[0].api_url, "https://anthropic-proxy.example");
        assert!(
            std::fs::read_to_string(&env)
                .unwrap()
                .contains("# MIGRATED: ANTHROPIC_API_KEY=sk-ant-x")
        );
    }

    #[test]
    fn a_provider_named_openai_key_is_imported_not_stranded() {
        let dir = tempfile::TempDir::new().unwrap();
        let env = write_env(
            dir.path(),
            "YI_AGENT_PROVIDER=openai\nYI_AGENT_MODEL=gpt-4o\nOPENAI_API_KEY=sk-oai-x\nOPENAI_BASE_URL=https://openai-proxy.example/v1\n",
        );
        let models = dir.path().join("models.json");
        assert_eq!(
            migrate_model_keys(&env, &models),
            MigrationOutcome::Migrated {
                entry: "gpt-4o".into()
            }
        );

        let catalog = crate::models::load_catalog_from(&models);
        assert_eq!(catalog.models[0].api_key, "sk-oai-x");
        assert_eq!(catalog.models[0].api_url, "https://openai-proxy.example/v1");
    }

    #[test]
    fn an_empty_openai_provider_key_is_not_substituted_by_the_anthropic_one() {
        let dir = tempfile::TempDir::new().unwrap();
        // provider 走 `openai`，但 .env 里只有 anthropic 同名键：按 provider 选键，
        // 不做「哪个键存在就用哪个」的推断。
        let env = write_env(
            dir.path(),
            "YI_AGENT_PROVIDER=openai\nYI_AGENT_MODEL=gpt-4o\nANTHROPIC_API_KEY=sk-ant-x\n",
        );
        let models = dir.path().join("models.json");
        assert_eq!(
            migrate_model_keys(&env, &models),
            MigrationOutcome::Migrated {
                entry: "gpt-4o".into()
            }
        );
        let catalog = crate::models::load_catalog_from(&models);
        assert_eq!(catalog.models[0].api_key, "");
        assert_eq!(catalog.models[0].api_url, "https://api.openai.com/v1");
    }

    #[test]
    fn the_generic_keys_win_over_the_provider_named_ones() {
        let dir = tempfile::TempDir::new().unwrap();
        let env = write_env(
            dir.path(),
            "YI_AGENT_MODEL=m\nMODEL_API_KEY=sk-generic\nANTHROPIC_API_KEY=sk-ant-x\nMODEL_API_URL=https://generic.example\nANTHROPIC_BASE_URL=https://anthropic-proxy.example\n",
        );
        let models = dir.path().join("models.json");
        assert_eq!(
            migrate_model_keys(&env, &models),
            MigrationOutcome::Migrated { entry: "m".into() }
        );
        let catalog = crate::models::load_catalog_from(&models);
        assert_eq!(catalog.models[0].api_key, "sk-generic");
        assert_eq!(catalog.models[0].api_url, "https://generic.example");
    }

    #[test]
    fn an_empty_generic_key_falls_back_to_the_provider_named_one() {
        let dir = tempfile::TempDir::new().unwrap();
        let env = write_env(
            dir.path(),
            "YI_AGENT_MODEL=m\nMODEL_API_KEY=\nANTHROPIC_API_KEY=sk-ant-x\n",
        );
        let models = dir.path().join("models.json");
        assert_eq!(
            migrate_model_keys(&env, &models),
            MigrationOutcome::Migrated { entry: "m".into() }
        );
        let catalog = crate::models::load_catalog_from(&models);
        assert_eq!(catalog.models[0].api_key, "sk-ant-x");
    }

    /// 认不出的 provider 是 `.env` 的输入错误，不是存储故障：结果必须是
    /// `InvalidProvider`（而不是谎报「清单不可写」），且一行都没写。
    #[test]
    fn an_unknown_provider_is_reported_as_such_and_writes_nothing() {
        let dir = tempfile::TempDir::new().unwrap();
        let env_text = "YI_AGENT_PROVIDER=azure\nMODEL_API_KEY=sk-x\n";
        let env = write_env(dir.path(), env_text);
        let models = dir.path().join("models.json");

        match migrate_model_keys(&env, &models) {
            MigrationOutcome::InvalidProvider { reason } => {
                assert!(reason.contains("azure"), "got {reason}");
            }
            other => panic!("expected InvalidProvider, got {other:?}"),
        }
        // 没落清单（否则会造出一个 anthropic 默认值的假条目）……
        assert!(
            !models.exists(),
            "an unparsable provider must not create a catalog"
        );
        // ……也没动 .env：用户可以改正 provider 后重跑。
        assert_eq!(std::fs::read_to_string(&env).unwrap(), env_text);
    }

    /// 设计 §6 点名的丢 key 场景：清单所在目录不可写 → 绝不注释 `.env`。
    /// （已有的 `a_corrupt_catalog_is_never_clobbered` 覆盖「文件坏」，
    /// 这条覆盖「写不下去」。）
    #[test]
    fn a_read_only_catalog_dir_leaves_the_env_untouched() {
        let dir = tempfile::TempDir::new().unwrap();
        let ro = dir.path().join("ro");
        std::fs::create_dir_all(&ro).unwrap();
        let models = ro.join("models.json");
        let env_text = "YI_AGENT_PROVIDER=anthropic\nYI_AGENT_MODEL=m\nMODEL_API_KEY=sk-x\nYI_AGENT_MAX_TURNS=20\n";
        let env = write_env(dir.path(), env_text);
        let mut perms = std::fs::metadata(&ro).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&ro, perms).unwrap();

        match migrate_model_keys(&env, &models) {
            MigrationOutcome::CatalogNotWritable { .. } => {}
            other => panic!("expected CatalogNotWritable, got {other:?}"),
        }
        // 顺序规则的要害：清单没落成 → `.env` 逐字节不动，用户的 key 不丢。
        assert_eq!(std::fs::read_to_string(&env).unwrap(), env_text);
        assert!(!models.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    use crate::settings::{load_from, write_to};

    struct MigrationFixture {
        dir: tempfile::TempDir,
    }

    impl MigrationFixture {
        fn new() -> Self {
            Self {
                dir: tempfile::TempDir::new().unwrap(),
            }
        }
        fn env(&self) -> std::path::PathBuf {
            self.dir.path().join(".env")
        }
        fn settings(&self) -> std::path::PathBuf {
            self.dir.path().join("settings.json")
        }
        fn websearch(&self) -> std::path::PathBuf {
            self.dir.path().join("websearch.json")
        }
        fn write_env(&self, text: &str) {
            std::fs::write(self.env(), text).unwrap();
        }
        fn run(&self) -> EnvMigrationOutcome {
            migrate_env_settings(&self.env(), &self.settings(), &self.websearch())
        }
    }

    #[test]
    fn missing_env_is_nothing_to_do() {
        let fixture = MigrationFixture::new();
        assert_eq!(fixture.run(), EnvMigrationOutcome::NothingToDo);
        assert!(
            !fixture.settings().exists(),
            "no settings.json may be created"
        );
    }

    #[test]
    fn env_without_a_class_keys_is_nothing_to_do_and_leaves_the_file_alone() {
        let fixture = MigrationFixture::new();
        fixture.write_env("SOME_OTHER_TOOL=1\n");

        assert_eq!(fixture.run(), EnvMigrationOutcome::NothingToDo);
        assert!(
            fixture.env().exists(),
            "no A-class key means nothing to migrate"
        );
        assert!(!fixture.settings().exists());
    }

    #[test]
    fn a_class_keys_land_in_settings_json() {
        let fixture = MigrationFixture::new();
        fixture.write_env(
            "\
# MIGRATED: YI_AGENT_PROVIDER=openai
YI_AGENT_MAX_TURNS=500
YI_AGENT_COMPACT_RATIO=75
YI_AGENT_SANDBOX=read-only
",
        );

        let outcome = fixture.run();
        assert_eq!(
            outcome,
            EnvMigrationOutcome::Migrated {
                fields: 3,
                bocha: false
            }
        );

        let settings = load_from(&fixture.settings());
        assert_eq!(settings.agent.max_turns, Some(500));
        assert_eq!(settings.compact.ratio, Some(75));
        assert_eq!(settings.startup.sandbox.as_deref(), Some("read-only"));
    }

    #[test]
    fn existing_settings_values_are_never_overwritten() {
        let fixture = MigrationFixture::new();
        write_to(
            &fixture.settings(),
            &serde_json::json!({"agent.max_turns": 42}),
        )
        .unwrap();
        fixture.write_env("YI_AGENT_MAX_TURNS=500\nYI_AGENT_COMPACT_RATIO=75\n");

        fixture.run();

        let settings = load_from(&fixture.settings());
        assert_eq!(
            settings.agent.max_turns,
            Some(42),
            "existing value must win"
        );
        assert_eq!(
            settings.compact.ratio,
            Some(75),
            "the unset field still gets migrated"
        );
    }

    #[test]
    fn env_is_renamed_to_migrated_after_a_successful_run() {
        let fixture = MigrationFixture::new();
        fixture.write_env("YI_AGENT_MAX_TURNS=500\n");

        fixture.run();

        assert!(!fixture.env().exists(), ".env must be gone");
        let renamed = fixture.dir.path().join(".env.migrated");
        assert!(renamed.exists(), ".env.migrated must exist");
        assert!(
            std::fs::read_to_string(&renamed)
                .unwrap()
                .contains("YI_AGENT_MAX_TURNS=500"),
            "the renamed file keeps its content for recovery"
        );
    }

    #[test]
    fn second_run_is_idempotent_and_does_not_touch_settings() {
        let fixture = MigrationFixture::new();
        fixture.write_env("YI_AGENT_MAX_TURNS=500\n");
        fixture.run();

        let mtime_before = std::fs::metadata(fixture.settings())
            .unwrap()
            .modified()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let second = fixture.run();

        assert_eq!(second, EnvMigrationOutcome::NothingToDo);
        let mtime_after = std::fs::metadata(fixture.settings())
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(
            mtime_before, mtime_after,
            "a second run must not rewrite settings.json"
        );
    }

    #[test]
    fn unusable_settings_json_blocks_the_rename() {
        let fixture = MigrationFixture::new();
        std::fs::write(fixture.settings(), "{ broken").unwrap();
        fixture.write_env("YI_AGENT_MAX_TURNS=500\n");

        let outcome = fixture.run();
        assert!(
            matches!(outcome, EnvMigrationOutcome::SettingsNotWritable { .. }),
            "got {outcome:?}"
        );
        assert!(
            fixture.env().exists(),
            ".env must survive when the target is not writable"
        );
    }

    #[test]
    fn bocha_key_moves_into_websearch_json() {
        let fixture = MigrationFixture::new();
        fixture.write_env("BOCHA_API_KEY=bocha-abc\nYI_AGENT_MAX_TURNS=500\n");

        let outcome = fixture.run();
        assert!(matches!(
            outcome,
            EnvMigrationOutcome::Migrated { bocha: true, .. }
        ));

        let config = yi_agent_tools::load_websearch_config_from(&fixture.websearch());
        assert_eq!(config.engines["bocha"].api_key, "bocha-abc");
    }

    #[test]
    fn an_existing_bocha_key_is_not_overwritten() {
        let fixture = MigrationFixture::new();
        let mut cfg = yi_agent_tools::WebSearchConfig::default();
        cfg.engines.get_mut("bocha").unwrap().api_key = "keep-me".to_string();
        let text = serde_json::to_string_pretty(&cfg).unwrap();
        std::fs::write(fixture.websearch(), text).unwrap();

        fixture.write_env("BOCHA_API_KEY=bocha-abc\n");

        fixture.run();

        let config = yi_agent_tools::load_websearch_config_from(&fixture.websearch());
        assert_eq!(config.engines["bocha"].api_key, "keep-me");
    }

    /// `YI_AGENT_WORKDIR` 没有落点，丢弃即可；`YI_AGENT_YOLO` 是安全姿态，绝不能
    /// 被持久化（否则 `sandbox_promotable` 恒为 false）。
    #[test]
    fn workdir_and_yolo_are_dropped() {
        let fixture = MigrationFixture::new();
        fixture.write_env(
            "\
YI_AGENT_WORKDIR=/tmp/somewhere
YI_AGENT_YOLO=true
YI_AGENT_MAX_TURNS=500
",
        );

        fixture.run();

        let text = std::fs::read_to_string(fixture.settings()).unwrap();
        assert!(!text.contains("workdir"), "workdir must not be persisted");
        assert!(!text.contains("yolo"), "yolo must not be persisted");
        assert!(text.contains("max_turns"));
    }

    /// 迁移必须用 Task 1 的解析口径：注释不能被并进值，`export` 前缀要剥掉。
    #[test]
    fn migration_uses_the_dotenvy_compatible_parser() {
        let fixture = MigrationFixture::new();
        fixture.write_env(
            "\
export YI_AGENT_MAX_TURNS=500 # this is a comment
YI_AGENT_SYSTEM_PROMPT='hello # world'
",
        );

        fixture.run();

        let settings = load_from(&fixture.settings());
        assert_eq!(settings.agent.max_turns, Some(500));
        assert_eq!(
            settings.agent.system_prompt.as_deref(),
            Some("hello # world"),
            "a # inside quotes is literal, not a comment"
        );
    }

    /// 一个非法值不能让整次迁移失败：该字段跳过，其余照常落盘。
    #[test]
    fn an_invalid_value_is_skipped_but_the_rest_still_migrates() {
        let fixture = MigrationFixture::new();
        fixture.write_env("YI_AGENT_MAX_TURNS=not-a-number\nYI_AGENT_COMPACT_RATIO=75\n");

        let outcome = fixture.run();
        assert!(matches!(outcome, EnvMigrationOutcome::Migrated { .. }));

        let settings = load_from(&fixture.settings());
        assert_eq!(settings.agent.max_turns, None);
        assert_eq!(settings.compact.ratio, Some(75));
    }

    /// 只剩已废弃键时也要改名：那两个键已无读取方，留着会让每次启动都重复
    /// 同一条告警，而文件永远不消失。
    #[test]
    fn a_file_with_only_retired_keys_is_renamed_not_left_behind() {
        let fixture = MigrationFixture::new();
        fixture.write_env("YI_AGENT_WORKDIR=/some/project\nYI_AGENT_YOLO=1\n");

        assert_eq!(fixture.run(), EnvMigrationOutcome::DroppedOnly);

        assert!(!fixture.env().exists(), "the .env must be renamed away");
        assert!(
            fixture.dir.path().join(".env.migrated").exists(),
            "the old file is kept under .env.migrated for recovery"
        );
        assert!(
            !fixture.settings().exists(),
            "there was nothing to write, so settings.json must not appear"
        );
        assert!(!fixture.websearch().exists());
    }

    /// 第二次调用已无 `.env`，返回 `NothingToDo`——`DroppedOnly` 也是幂等的。
    #[test]
    fn dropped_only_is_idempotent() {
        let fixture = MigrationFixture::new();
        fixture.write_env("YI_AGENT_WORKDIR=/some/project\n");
        assert_eq!(fixture.run(), EnvMigrationOutcome::DroppedOnly);
        assert_eq!(fixture.run(), EnvMigrationOutcome::NothingToDo);
    }
}
