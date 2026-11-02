use super::*;

/// 旧格式只有 `boards[].project` 有意义;读出来的顺序即文件顺序。
#[test]
fn the_legacy_registry_yields_its_project_paths() {
    let dir = tempfile::tempdir().unwrap();
    let legacy = dir.path().join("boards.json");
    std::fs::write(
        &legacy,
        r#"{"boards":[{"project":"/p/a","created_at":"x"},{"project":"/p/b"}]}"#,
    )
    .unwrap();

    assert_eq!(
        crate::legacy_compat::parse_legacy_registry(&legacy),
        Some(vec![PathBuf::from("/p/a"), PathBuf::from("/p/b")])
    );
}

/// 缺文件不是错误:没什么可迁的(而不是「读不动」)。
#[test]
fn an_absent_legacy_registry_is_nothing_to_migrate() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        crate::legacy_compat::parse_legacy_registry(&dir.path().join("boards.json")),
        Some(Vec::new())
    );
}

/// 手改坏的登记表**不得**被当成空表然后改名:那会把里面的项目永久丢掉。
/// 故解析失败返回 `None` 且文件原文不动。
#[test]
fn a_corrupt_legacy_registry_is_left_in_place() {
    let dir = tempfile::tempdir().unwrap();
    let legacy = dir.path().join("boards.json");
    std::fs::write(&legacy, "{ not json").unwrap();

    assert_eq!(crate::legacy_compat::parse_legacy_registry(&legacy), None);
    assert_eq!(
        std::fs::read_to_string(&legacy).unwrap(),
        "{ not json",
        "the damaged file must be left for a human to recover"
    );
}

/// 启动期整套动作:旧表 → 登记(请求者名字正确) → 旧表改名。
#[test]
fn the_startup_migration_imports_the_projects_and_renames_the_old_file() {
    let dir = tempfile::tempdir().unwrap();
    let board_dir = dir.path().join("boards");
    let resident_dir = dir.path().join("resident");
    std::fs::create_dir_all(&board_dir).unwrap();
    let legacy = board_dir.join("boards.json");
    std::fs::write(&legacy, r#"{"boards":[{"project":"/p/a"}]}"#).unwrap();

    migrate_legacy_registry_file(&legacy, &resident_dir);

    assert_eq!(
        yi_agent_store::resident::list(&resident_dir),
        vec![PathBuf::from("/p/a")]
    );
    assert_eq!(
        yi_agent_store::resident::entries(&resident_dir)[0].required_by,
        vec!["superpowers-kanban"],
        "the requester name is the disk contract the plugin reads back"
    );
    assert!(!legacy.exists(), "the old file is renamed");
    assert!(board_dir.join("boards.json.migrated").exists());
}

/// 幂等:第二次启动时旧表已改名 → 什么都没发生,登记内容一字不变。
#[test]
fn the_startup_migration_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let board_dir = dir.path().join("boards");
    let resident_dir = dir.path().join("resident");
    std::fs::create_dir_all(&board_dir).unwrap();
    std::fs::write(
        board_dir.join("boards.json"),
        r#"{"boards":[{"project":"/p/a"}]}"#,
    )
    .unwrap();

    migrate_legacy_registry_file(&board_dir.join("boards.json"), &resident_dir);
    let after_first = yi_agent_store::resident::list(&resident_dir);

    migrate_legacy_registry_file(&board_dir.join("boards.json"), &resident_dir);

    assert_eq!(yi_agent_store::resident::list(&resident_dir), after_first);
    assert!(!board_dir.join("boards.json").exists());
}

/// 坏表 / 无表都不能让启动失败:迁移是便利,不是前置。
#[test]
fn the_startup_migration_never_blocks_startup() {
    let dir = tempfile::tempdir().unwrap();
    let board_dir = dir.path().join("boards");
    let resident_dir = dir.path().join("resident");
    std::fs::create_dir_all(&board_dir).unwrap();
    std::fs::write(board_dir.join("boards.json"), "{ not json").unwrap();

    // 返回 `()` 本身就是断言:没有 `Result` 可传播,也就没有失败路径。
    migrate_legacy_registry_file(&board_dir.join("boards.json"), &resident_dir);

    assert!(!board_dir.join("boards.json.migrated").exists());
}

/// 组长 tick 臂必须受开关控制。
///
/// 这一臂驱动的是"算 digest → 投递 → 起 turn"，全是组长的副作用；关掉开关
/// 后它必须整体空转。它没有可注入的观测点（`lead_ticker` 在 `serve` 内部），
/// 故只能按源码断言——与上面那条"生产入口有没有调迁移"同一手法。
#[test]
fn the_lead_tick_arm_is_gated_by_the_flag() {
    // 不走 `production_source()`：它在**本测试模块之前**截断，而 tick 臂在
    // `serve()` 里、位于本模块之后。改用完整源码，并取**最后一处**——本
    // 测试自己的 `.split("...")` 字面量也是一次命中，只有生产代码那处才在
    // 它后面还有 `if !lead_agent_enabled`。
    let source = include_str!("../server.rs");
    let arm = source
        .rsplit("_ = lead_ticker.tick() => {")
        .next()
        .expect("the ticker arm must still exist");
    assert_ne!(arm.len(), source.len(), "the ticker arm must still exist");
    let head: String = arm
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .take(3)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        head.contains("if !lead_agent_enabled") && head.contains("continue"),
        "the ticker arm must short-circuit when the flag is off, got:\n{head}"
    );
}

/// 生产源码:截到本测试模块之前,并去掉注释行。
///
/// 两个理由:① 这些断言问的是「生产入口有没有调」,测试模块里的调用不算数;
/// ② 迁移函数的**文档注释**里提到了那个即将被删的 crate 名(说明为何用字面量
/// 而不是引用它),那是 prose 而非依赖,不该被当成命中。
fn production_source() -> String {
    // 搬移（wave2 C2 batch 8）后 `run` / `serve_stdio_with_relay` 在
    // `server/transport/scoped.rs`，迁移调用点跟着走了。两段都要读：本文件的断言
    // 问的是「生产入口有没有调」，与代码落在哪个文件无关。
    //
    // 逐文件截断再拼接：`#[cfg(test)]` 标记只在 `server.rs` 里，若先拼接再
    // split，`scoped.rs` 会被并进"测试之后"的那一半而整段丢失。
    let own = include_str!("../server.rs")
        .split("#[cfg(test)]\nmod legacy_migration_tests")
        .next()
        .expect("this module must exist");
    let entries = include_str!("../server/transport/scoped.rs");
    let production = format!("{own}\n{entries}");
    production
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 真实调用点:纯函数测得再细,没有调用者时验收 §6.6 仍不成立。
/// 三个生产入口(`run`、`serve_stdio_with_relay`、`ws::serve_ws`)
/// 都必须调它——ws 是第三条入口，漏了它 `--listen ws://` 的首次运行就不迁移。
#[test]
fn every_production_entry_runs_the_migration_at_startup() {
    let production = production_source();
    let call = "migrate_legacy_resident_requests_on_startup(&resident_dir)".to_string();
    assert_eq!(
        production.matches(&call).count(),
        2,
        "`run` and `serve_stdio_with_relay` must each run the migration once"
    );

    // ws 的调用在另一个文件里：本文件里数不到，必须单独看它。
    let ws = include_str!("../ws.rs");
    let ws_production = ws
        .split("#[cfg(test)]")
        .next()
        .expect("ws.rs must have a production part");
    assert_eq!(
        ws_production.matches(&call).count(),
        1,
        "`ws::serve_ws` is the third production entry and must migrate too"
    );

    // 会话期自愈循环同理：三条入口都要起，否则 ws 模式没有常驻兜底。
    // 按**调用形态**收，别按函数名收——函数定义本身也含那个前缀。
    assert_eq!(
        production
            .matches("spawn_resident_watch_loop(resident_dir.clone())")
            .count(),
        2
    );
    assert_eq!(
        ws_production
            .matches("spawn_resident_watch_loop(resident_dir)")
            .count(),
        1
    );
}

/// 启动工作**不得**落在测试接缝 (`serve_ws_body`) 里。
///
/// 接缝跑在 libtest 二进制内,`ensure_running` 会以 `current_exe() daemon
/// serve` 拉起子进程——那等于让每个 ws 测试把自己当 daemon 递归跑起来
/// (实测每个子进程 200% CPU,还会写用户真实的 `~/.yi-agent`)。
#[test]
fn the_ws_test_seam_does_no_startup_work() {
    let ws = include_str!("../ws.rs");
    let production = ws
        .split("#[cfg(test)]")
        .next()
        .expect("ws.rs must have a production part");
    let seam = production
        .split("pub(crate) async fn serve_ws_body(")
        .nth(1)
        .expect("the seam must exist")
        .split("let resident_dir")
        .nth(1)
        .unwrap_or("");
    for call in [
        "migrate_legacy_model_keys_on_startup",
        "migrate_legacy_resident_requests_on_startup",
        "spawn_resident_watch_loop",
    ] {
        assert!(
            !seam.contains(call),
            "`serve_ws_body` is the test seam; `{call}` belongs in the production entry"
        );
    }
}

/// 两条迁移都必须挂在同一个生产启动函数里。逐条断言，漏一条就会让某个
/// 入口（典型是 ws）静默不迁移。
#[test]
fn the_startup_migration_runs_both_model_and_settings_migrations() {
    let source = include_str!("../server/migration.rs");
    let start = source
        .find("pub(crate) fn migrate_legacy_model_keys_on_startup()")
        .expect("the startup migration function must exist");
    let body = &source[start..];
    let body = body
        .split("\n}\n")
        .next()
        .expect("the function body must be terminated");
    for call in [
        "yi_agent_runtime::migrate::migrate_model_keys",
        "yi_agent_runtime::migrate::migrate_env_settings",
    ] {
        assert!(
            body.contains(call),
            "`migrate_legacy_model_keys_on_startup` must call `{call}`"
        );
    }
}

/// ws 入口必须走同一个启动函数——手机配对的配置迁移全靠这一条。
#[test]
fn the_ws_entry_runs_the_startup_migration() {
    let ws = include_str!("../ws.rs");
    let production = ws.split("#[cfg(test)]").next().expect("production part");
    assert!(
        production.contains("migrate_legacy_model_keys_on_startup"),
        "ws.rs must run the shared startup migration"
    );
}

/// 迁移代码**不得**依赖那个已删的看板 registry crate:否则它会随它
/// crate 一起消失,旧环境的看板就没人来迁了。
#[test]
fn the_migration_does_not_depend_on_the_boards_crate() {
    // 旧格式的解析已经收进 `legacy_compat`（那是**唯一**该知道 `boards.json`
    // 长什么样的地方），故这里检查那个文件，而不是本文件的片段。
    let source = include_str!("../legacy_compat.rs");
    let production = source
        .split("#[cfg(test)]")
        .next()
        .expect("legacy_compat must have a production part");
    let needle = ["yi_agent", "_boards"].concat();
    assert!(
        !production.contains(&needle),
        "the migration path must stay independent of the boards crate"
    );
}

/// **Task 6 的关键回归**：迁移的目录来源换了一个实现，但必须仍指向
/// `$HOME/.yi-agent/<插件目录>/boards.json`。以前那条路径来自
/// 那个看板 registry crate 的 `global_dir()`；只删依赖而不换来源，迁移会在空目录里
/// 找文件，老环境的项目静默全部丢失。
///
/// 这里钉的是**启动路径**用的那个函数（不是测试注入的路径）：
/// `migrate_legacy_resident_requests_on_startup` 自己解析目录，故给定一个假的
/// `HOME` 就应当能迁到其中那份旧表。
#[test]
fn the_startup_path_still_finds_the_legacy_file_under_home() {
    let dir = tempfile::tempdir().unwrap();
    let resident_dir = dir.path().join("resident");
    // 造一个假的 `$HOME`，把旧表放到启动路径会去找的位置。
    let home = dir.path().join("home");
    let legacy_dir = home.join(".yi-agent").join("superpowers-kanban");
    std::fs::create_dir_all(&legacy_dir).unwrap();
    std::fs::write(
        legacy_dir.join("boards.json"),
        r#"{"boards":[{"project":"/p/old"}]}"#,
    )
    .unwrap();

    // 直接钉住「目录来源」这条规则（不进程级改 `HOME`：与并行测试互不干扰）。
    let resolved = crate::legacy_compat::legacy_board_dir_from(Some(std::ffi::OsStr::new(
        home.to_str().unwrap(),
    )))
    .expect("a set HOME must resolve");
    assert_eq!(
        resolved, legacy_dir,
        "the startup path must look in $HOME/.yi-agent/superpowers-kanban, \
         the same place the deleted crate used"
    );

    // 走一遍同一个核心：给定启动路径会解析出的文件，登记表里必须出现它。
    migrate_legacy_registry_file(&resolved.join("boards.json"), &resident_dir);
    assert_eq!(
        yi_agent_store::resident::list(&resident_dir),
        vec![PathBuf::from("/p/old")],
        "a legacy environment must keep its projects across the upgrade"
    );
}
