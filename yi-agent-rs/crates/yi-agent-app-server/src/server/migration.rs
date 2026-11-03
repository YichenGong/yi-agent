//! 启动期的历史数据迁移（模型键、常驻登记、registry 文件）。
//!
//! 从 `server.rs` 逐字搬出（wave2 C2 batch 4）。

use std::path::Path;

/// 生产启动期的一次性迁移，供所有生产入口共用。
///
/// **两条迁移，固定顺序，一次跑完**：
/// 1. 模型键 `.env → models.json`（既有）；
/// 2. 运行参数 `.env → settings.json`（本 spec 新增）。
///
/// 顺序不能反：两条之间没有数据依赖，但把它们放在同一个函数里，是为了让
/// 「所有入口都迁移」成为结构性保证——四个调用点（`run`、`serve_stdio`、ws、
/// CLI 两处）漏掉任何一个，都会表现为「某个客户端配置丢失」。
///
/// **函数名保留 `model_keys`**：它现在是历史名字，改名要动四个调用点且不增加
/// 正确性。名字与实际职责的偏差写在这里，比散落四处更可控。
///
/// 只在生产入口调用：测试用 `Harness` 注入隔离路径，绝不能在这里产生副作用。
/// `HOME` 缺失时无从解析实例目录，直接跳过（避免在凭空拼出的路径上写文件）。
pub(crate) fn migrate_legacy_model_keys_on_startup() {
    let Some(env_path) = yi_agent_runtime::config::resolve_global_env_path() else {
        return;
    };
    let models_path = yi_agent_runtime::models::models_path();
    let settings_path = yi_agent_runtime::settings::settings_path();
    let websearch_path = yi_agent_tools::websearch_path();

    let outcome = yi_agent_runtime::migrate::migrate_model_keys(&env_path, &models_path);
    yi_agent_runtime::migrate::log_outcome(&outcome, &env_path);

    let outcome =
        yi_agent_runtime::migrate::migrate_env_settings(&env_path, &settings_path, &websearch_path);
    yi_agent_runtime::migrate::log_env_settings_outcome(&outcome, &env_path);
}
/// 旧常驻登记表（看板时代）里登记的项目路径，按 `{"boards":[{"project":"<abs>"}]}` 解析。
///
/// 这里**故意不依赖任何生产代码**：旧格式只有 `boards[].project` 一个字段有意义，
/// 故几十行本地解析比引依赖更稳——它是老环境能否保住自己那些项目的**唯一**路径。
///
/// 读侧宽容，且**故意不**在损坏时改名：一份手改坏的旧登记表若被当成「空表」然后
/// 改名取证，用户就再也拿不回里面的项目了。损坏 → 返回 `None`、文件原地不动，由
/// 调用方记一条 warn；缺文件 → 空列表（没什么可迁的）。
/// 生产启动期的一次性迁移：旧常驻登记表 → 通用常驻登记。
///
/// 只在生产入口（`run` / `serve_stdio_with_relay`）调用一次，与
/// [`migrate_legacy_model_keys_on_startup`] 同一段初始化。**任何失败都只记 warn，
/// 绝不阻断启动**——迁移是便利，不是前置：登记表读不动时宿主照样要能起来。
///
/// 旧文件的**目录来源**由 [`crate::legacy_compat::legacy_board_registry_path`] 给出
/// （与看板时代那个 registry crate 的同名函数逐字同一条规则）。这一步是必须的：
/// 删掉那个 crate 却只删依赖，迁移就会在一个空目录里找文件，老环境的项目静默
/// 全部丢失。`HOME` 缺失时无从查找，直接跳过（与常驻登记自身的策略一致）。
pub(crate) fn migrate_legacy_resident_requests_on_startup(resident_dir: &Path) {
    let Some(legacy_file) = crate::legacy_compat::legacy_board_registry_path() else {
        return;
    };
    migrate_legacy_registry_file(&legacy_file, resident_dir);
}
/// 迁移的**核心**：把一份旧登记表导入通用常驻登记。文件路径由调用方给出，
/// 故生产用解析出的路径、测试用 tempdir——同一个函数，同一条逻辑。
///
/// 请求者名是**磁盘契约**（写进 `resident-daemons.json` 的 `required_by`）：迁移时
/// 写下的名字必须与那个插件此后读写的一致，否则它认不出这是「自己要常驻的项目」，
/// 下一次读取就会把登记当成别人的。
pub(crate) fn migrate_legacy_registry_file(legacy_file: &Path, resident_dir: &Path) {
    let Some(projects) = crate::legacy_compat::parse_legacy_registry(legacy_file) else {
        return;
    };
    if projects.is_empty() {
        return;
    }
    match yi_agent_store::resident::import_legacy_projects(
        &projects,
        legacy_file,
        resident_dir,
        crate::legacy_compat::SUPERPOWERS_KANBAN_REQUESTER,
    ) {
        Ok(imported) => tracing::info!(
            imported,
            path = %legacy_file.display(),
            "imported legacy resident registrations into the generic registry"
        ),
        Err(error) => tracing::warn!(
            %error,
            path = %legacy_file.display(),
            "could not import the legacy registry; it is left in place for the next run"
        ),
    }
}
