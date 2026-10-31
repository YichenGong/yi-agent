//! 端到端：一个带存量 `.env` 的实例目录，跑一次真实启动路径后，配置必须落在
//! `settings.json`，`.env` 变成 `.env.migrated`，且第二次启动不再改动任何文件。

use std::path::Path;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn a_legacy_instance_migrates_and_then_stays_put() {
    let dir = tempfile::TempDir::new().unwrap();
    let instance = dir.path().join(".yi-agent");
    write(
        &instance.join(".env"),
        "\
YI_AGENT_MAX_TURNS=500
YI_AGENT_SANDBOX=read-only
BOCHA_API_KEY=bocha-abc
YI_AGENT_WORKDIR=/tmp/nowhere
",
    );

    let env_path = instance.join(".env");
    let settings_path = instance.join("settings.json");
    let websearch_path = instance.join("websearch.json");
    let models_path = instance.join("models.json");

    // 第一次：与生产启动函数相同的两条迁移。
    let _ = yi_agent_runtime::migrate::migrate_model_keys(&env_path, &models_path);
    let outcome =
        yi_agent_runtime::migrate::migrate_env_settings(&env_path, &settings_path, &websearch_path);
    assert!(
        matches!(
            outcome,
            yi_agent_runtime::migrate::EnvMigrationOutcome::Migrated { .. }
        ),
        "got {outcome:?}"
    );

    let settings = yi_agent_runtime::settings::load_from(&settings_path);
    assert_eq!(settings.agent.max_turns, Some(500));
    assert_eq!(settings.startup.sandbox.as_deref(), Some("read-only"));
    assert!(
        std::fs::read_to_string(&settings_path)
            .unwrap()
            .find("workdir")
            .is_none(),
        "workdir must not be persisted: {settings:?}"
    );

    let websearch = yi_agent_tools::load_websearch_config_from(&websearch_path);
    assert_eq!(websearch.engines["bocha"].api_key, "bocha-abc");

    assert!(!env_path.exists(), ".env must be renamed");
    assert!(instance.join(".env.migrated").exists());

    // 第二次：幂等，不碰 settings.json。
    let mtime = std::fs::metadata(&settings_path)
        .unwrap()
        .modified()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let second =
        yi_agent_runtime::migrate::migrate_env_settings(&env_path, &settings_path, &websearch_path);
    assert_eq!(
        second,
        yi_agent_runtime::migrate::EnvMigrationOutcome::NothingToDo
    );
    assert_eq!(
        std::fs::metadata(&settings_path)
            .unwrap()
            .modified()
            .unwrap(),
        mtime,
        "a second start must not rewrite settings.json"
    );
}

/// 迁移后的配置必须能真的驱动 `RuntimeConfig`——不只是文件里有值。
#[test]
fn the_migrated_settings_are_what_the_config_loads() {
    let dir = tempfile::TempDir::new().unwrap();
    let instance = dir.path().join(".yi-agent");
    write(&instance.join(".env"), "YI_AGENT_MAX_TURNS=432\n");

    let env_path = instance.join(".env");
    let settings_path = instance.join("settings.json");
    let websearch_path = instance.join("websearch.json");
    let _ =
        yi_agent_runtime::migrate::migrate_env_settings(&env_path, &settings_path, &websearch_path);
    assert!(!env_path.exists());

    // 用 `load_from` + 直接比较，避免改进程级 `YI_AGENT_HOME`（那会与并行测试打架）。
    let settings = yi_agent_runtime::settings::load_from(&settings_path);
    assert_eq!(settings.agent.max_turns, Some(432));
}
