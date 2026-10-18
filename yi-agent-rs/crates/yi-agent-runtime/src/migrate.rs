//! 一次性迁移：把存量 `.env` 里的活跃模型键收边进清单，并注释掉它们。

use std::path::Path;

use crate::config::MODEL_ENV_KEYS;

#[derive(Debug, PartialEq, Eq)]
pub enum MigrationOutcome {
    NothingToDo,
    Migrated { entry: String },
    AlreadyCategorized,
    EnvNotWritable { reason: String },
    CatalogNotWritable { reason: String },
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
    let existing = crate::models::load_catalog_from(models_path);
    let outcome = if existing.models.is_empty() {
        let get = |key: &str| {
            active
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        let settings = crate::onboarding::ModelSettings {
            provider: get("YI_AGENT_PROVIDER"),
            model: get("YI_AGENT_MODEL"),
            api_url: get("MODEL_API_URL"),
            api_key: get("MODEL_API_KEY"),
        };
        let provider = if settings.provider.trim().is_empty() {
            "anthropic".to_string()
        } else {
            settings.provider.clone()
        };
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
        let Some(parsed) = crate::models::ModelProvider::parse(&provider) else {
            return MigrationOutcome::CatalogNotWritable {
                reason: format!("unknown provider: {provider}"),
            };
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
}
