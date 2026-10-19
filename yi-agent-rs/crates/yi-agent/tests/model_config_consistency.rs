//! 同一份清单 + 同一组注入，CLI 与 app-server 必须解析出同一个模型。
//!
//! 这是防「两条腿再次分叉」的回归锁：模型配置的唯一真相源是
//! `~/.yi-agent/models.json`，两个用户面向入口都必须经 `resolve_effective`
//! 得到同一个结果。

use yi_agent_runtime::config::{ConfigOverrides, RuntimeConfig};
use yi_agent_runtime::models::{
    ModelCatalog, ModelEntry, ModelProvider, load_catalog_from, resolve_effective,
};

/// 清掉进程里的模型键与 `HOME`，Drop 时恢复。
///
/// 测试宿主的 shell 可能带着 `MODEL_API_KEY` / `YI_AGENT_MODEL` 等（例如从桌面端
/// 继承）；它们会让 `model_injected` 变真，从而绕过清单——那就测不到「清单是唯一
/// 真相源」这件事了。`HOME` 也必须隔离：`RuntimeConfig::load_lenient` 会读
/// `$HOME/.yi-agent/.env`。
struct IsolatedEnv {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl IsolatedEnv {
    fn new() -> Self {
        // 同一进程内两个测试共享进程环境：必须串行，否则一个测试的 Drop 恢复
        // 会把宿主的 `YI_AGENT_MODEL` 塞回另一个正在跑的测试。
        static MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let lock = MUTEX.lock().unwrap_or_else(|p| p.into_inner());
        let keys = [
            "HOME",
            "MODEL_API_KEY",
            "MODEL_API_URL",
            "YI_AGENT_PROVIDER",
            "YI_AGENT_MODEL",
            "YI_AGENT_MODEL_CONTEXT_LENGTH",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_BASE_URL",
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
        ];
        let saved = keys
            .iter()
            .map(|k| (*k, std::env::var_os(k)))
            .collect::<Vec<_>>();
        for k in keys {
            unsafe {
                std::env::remove_var(k);
            }
        }
        Self { saved, _lock: lock }
    }
}

impl Drop for IsolatedEnv {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            unsafe {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

fn catalog_with_default() -> ModelCatalog {
    ModelCatalog {
        models: vec![ModelEntry {
            name: "shared".into(),
            provider: ModelProvider::Openai,
            api_url: "https://gw/v1".into(),
            model: "shared-model".into(),
            api_key: "sk-shared".into(),
            api_style: None,
            reasoning_effort: None,
            reasoning_capabilities: None,
            context_length: None,
        }],
        default_model: Some("shared".into()),
        subagent_model: None,
    }
}

#[test]
fn cli_and_app_server_resolve_the_same_model() {
    let _env = IsolatedEnv::new();
    let dir = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    unsafe {
        std::env::set_var("HOME", home.path());
    }

    let models_path = home.path().join(".yi-agent").join("models.json");
    yi_agent_runtime::models::save_catalog_to(&models_path, &catalog_with_default()).unwrap();

    let catalog = load_catalog_from(&models_path);
    let cfg = RuntimeConfig::load_lenient(&ConfigOverrides {
        workdir: Some(dir.path().to_path_buf()),
        ..Default::default()
    })
    .unwrap();

    // CLI 与 app-server 的生产路径都汇到这一个函数。
    let resolved = resolve_effective(&cfg, &catalog, None);
    assert_eq!(resolved.provider, "openai");
    assert_eq!(resolved.model, "shared-model");
    assert_eq!(resolved.api_url, "https://gw/v1");
    assert_eq!(resolved.api_key, "sk-shared");
}

#[test]
fn a_context_length_entry_recomputes_the_threshold() {
    // 条目上的 `context_length` 必须参与阈值计算（同样在共享解析链里）。
    let _env = IsolatedEnv::new();
    let dir = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    unsafe {
        std::env::set_var("HOME", home.path());
    }

    let mut catalog = catalog_with_default();
    catalog.models[0].context_length = Some(100_000);
    let cfg = RuntimeConfig::load_lenient(&ConfigOverrides {
        workdir: Some(dir.path().to_path_buf()),
        ..Default::default()
    })
    .unwrap();

    let resolved = resolve_effective(&cfg, &catalog, None);
    assert_eq!(
        resolved.compact_threshold,
        100_000 * resolved.compact_ratio / 100
    );
}
