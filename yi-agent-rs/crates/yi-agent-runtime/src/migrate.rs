//! 一次性迁移：把存量 `.env` 里的活跃模型键收边进清单，并注释掉它们。

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
        let provider = settings.provider.clone();
        let mut catalog = existing.clone();
        let name = {
            let base = if settings.model.trim().is_empty() {
                provider.clone()
            } else {
                settings.model.trim().to_string()
            };
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
        let model = if settings.model.trim().is_empty() {
            match parsed {
                crate::models::ModelProvider::Openai => "gpt-4o".to_string(),
                crate::models::ModelProvider::Anthropic => "claude-sonnet-4-20250514".to_string(),
            }
        } else {
            settings.model.clone()
        };
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

#[cfg(test)]
mod tests {
    use super::*;

    fn write_env(dir: &Path, text: &str) -> std::path::PathBuf {
        let path = dir.join(".env");
        std::fs::write(&path, text).unwrap();
        path
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
        let env = write_env(dir.path(), "MODEL_API_KEY=sk-x\n");
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
        let env = write_env(&ro, "MODEL_API_KEY=sk-x\n");
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
        let env_text = "MODEL_API_KEY=sk-x\nYI_AGENT_MAX_TURNS=20\n";
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
            "ANTHROPIC_API_KEY=sk-ant-x\nANTHROPIC_BASE_URL=https://anthropic-proxy.example\n",
        );
        let models = dir.path().join("models.json");
        assert_eq!(
            migrate_model_keys(&env, &models),
            MigrationOutcome::Migrated {
                entry: "anthropic".into()
            }
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
            "YI_AGENT_PROVIDER=openai\nOPENAI_API_KEY=sk-oai-x\nOPENAI_BASE_URL=https://openai-proxy.example/v1\n",
        );
        let models = dir.path().join("models.json");
        assert_eq!(
            migrate_model_keys(&env, &models),
            MigrationOutcome::Migrated {
                entry: "openai".into()
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
            "YI_AGENT_PROVIDER=openai\nANTHROPIC_API_KEY=sk-ant-x\n",
        );
        let models = dir.path().join("models.json");
        assert_eq!(
            migrate_model_keys(&env, &models),
            MigrationOutcome::Migrated {
                entry: "openai".into()
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
            "MODEL_API_KEY=sk-generic\nANTHROPIC_API_KEY=sk-ant-x\nMODEL_API_URL=https://generic.example\nANTHROPIC_BASE_URL=https://anthropic-proxy.example\n",
        );
        let models = dir.path().join("models.json");
        assert_eq!(
            migrate_model_keys(&env, &models),
            MigrationOutcome::Migrated {
                entry: "anthropic".into()
            }
        );
        let catalog = crate::models::load_catalog_from(&models);
        assert_eq!(catalog.models[0].api_key, "sk-generic");
        assert_eq!(catalog.models[0].api_url, "https://generic.example");
    }

    #[test]
    fn an_empty_generic_key_falls_back_to_the_provider_named_one() {
        let dir = tempfile::TempDir::new().unwrap();
        let env = write_env(dir.path(), "MODEL_API_KEY=\nANTHROPIC_API_KEY=sk-ant-x\n");
        let models = dir.path().join("models.json");
        assert_eq!(
            migrate_model_keys(&env, &models),
            MigrationOutcome::Migrated {
                entry: "anthropic".into()
            }
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
        let env_text = "MODEL_API_KEY=sk-x\nYI_AGENT_MAX_TURNS=20\n";
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
}
