//! 实例注册表与跨进程互斥的行为测试。
//!
//! # 为什么跨进程那条必须真的 fork
//!
//! 进程内 `Mutex` 只能证明"同一进程的两个线程互斥"，这恰恰是**不需要证明**
//! 的那一半。真正的风险是两个宿主进程（桌面壳 + CLI）同时改写同一份
//! registry：它们各自持有自己的 `Mutex`，都不认为自己冲突。所以这里用
//! `std::process::Command` 起真实子进程去争同一把 `flock`。

use std::path::{Path, PathBuf};
use std::time::Duration;

use yi_agent_plugins::locks;
use yi_agent_plugins::registry::Registry;
use yi_agent_plugins::{PluginError, PluginId};

fn plugin(name: &str) -> PluginId {
    PluginId::parse(name).expect("valid plugin id")
}

fn temp_home() -> PathBuf {
    // 目录要在测试结束时仍存在（子进程可能还在用），所以泄漏它。
    tempfile::TempDir::new()
        .expect("temp dir")
        .keep()
        .join("home")
}

const DIGEST_A: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const DIGEST_B: &str = "2222222222222222222222222222222222222222222222222222222222222222";

// ---------------------------------------------------------------------------
// 记录与幂等
// ---------------------------------------------------------------------------

#[test]
fn a_fresh_instance_has_an_empty_registry() {
    let home = temp_home();
    let registry = Registry::open(&home).expect("open");
    assert!(registry.list().expect("list").is_empty());
    // 读不该创建文件：一个从没用过插件的实例不该有插件目录。
    assert!(!home.join("plugins/registry.json").exists());
}

#[test]
fn recording_an_install_persists_it() {
    let home = temp_home();
    let registry = Registry::open(&home).expect("open");
    registry
        .record_install(
            &plugin("example-counter"),
            "1.4.0",
            DIGEST_A,
            "plugins/install/a",
            100,
        )
        .expect("record");

    let reopened = Registry::open(&home).expect("reopen");
    let records = reopened.list().expect("list");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].plugin.as_str(), "example-counter");
    assert_eq!(records[0].installed.len(), 1);
    assert_eq!(records[0].installed[0].sha256, DIGEST_A);
}

#[test]
fn the_first_install_selects_the_version_without_creating_a_grant() {
    // 设计 §6：安装不产生运行授权。顺手建一个 grant 等于把"装了"
    // 等同于"用户已同意它在某个项目里跑"。
    let home = temp_home();
    let registry = Registry::open(&home).expect("open");
    let record = registry
        .record_install(
            &plugin("example-counter"),
            "1.4.0",
            DIGEST_A,
            "plugins/install/a",
            100,
        )
        .expect("record");

    assert_eq!(record.selected.as_deref(), Some("1.4.0"));
    assert!(
        record.grants.is_empty(),
        "installing must not create a project grant"
    );
}

#[test]
fn reinstalling_the_same_digest_is_idempotent() {
    let home = temp_home();
    let registry = Registry::open(&home).expect("open");
    registry
        .record_install(
            &plugin("example-counter"),
            "1.4.0",
            DIGEST_A,
            "plugins/install/a",
            100,
        )
        .expect("first");
    let before = std::fs::read(home.join("plugins/registry.json")).expect("read");

    let record = registry
        .record_install(
            &plugin("example-counter"),
            "1.4.0",
            DIGEST_A,
            "plugins/install/a",
            999,
        )
        .expect("second");
    let after = std::fs::read(home.join("plugins/registry.json")).expect("read");

    assert_eq!(record.installed.len(), 1);
    // 真正的幂等：文件一个字节都没变。只比较"记录条数没变"会漏掉
    // 「把 installed_at 改成 999」这种重写。
    assert_eq!(
        before, after,
        "an idempotent reinstall must not rewrite the file"
    );
}

#[test]
fn the_same_version_with_a_different_digest_is_a_conflict() {
    let home = temp_home();
    let registry = Registry::open(&home).expect("open");
    registry
        .record_install(
            &plugin("example-counter"),
            "1.4.0",
            DIGEST_A,
            "plugins/install/a",
            100,
        )
        .expect("first");

    let error = registry
        .record_install(
            &plugin("example-counter"),
            "1.4.0",
            DIGEST_B,
            "plugins/install/b",
            200,
        )
        .expect_err("a different digest for the same version must be refused");
    assert_eq!(error.kind(), "Conflict", "got: {error}");

    // 拒绝不能顺手改写：原记录必须原样还在。
    let records = registry.list().expect("list");
    assert_eq!(records[0].installed.len(), 1);
    assert_eq!(records[0].installed[0].sha256, DIGEST_A);
}

#[test]
fn a_second_version_does_not_change_the_selected_one() {
    // 升级装了新版本，但用户正在用的版本不该在"装好了"这一刻被切走。
    let home = temp_home();
    let registry = Registry::open(&home).expect("open");
    registry
        .record_install(
            &plugin("example-counter"),
            "1.4.0",
            DIGEST_A,
            "plugins/install/a",
            100,
        )
        .expect("first");
    let record = registry
        .record_install(
            &plugin("example-counter"),
            "1.5.0",
            DIGEST_B,
            "plugins/install/b",
            200,
        )
        .expect("second");

    assert_eq!(record.installed.len(), 2);
    assert_eq!(record.selected.as_deref(), Some("1.4.0"));
}

#[test]
fn selecting_a_version_that_is_not_installed_is_rejected() {
    let home = temp_home();
    let registry = Registry::open(&home).expect("open");
    registry
        .record_install(
            &plugin("example-counter"),
            "1.4.0",
            DIGEST_A,
            "plugins/install/a",
            100,
        )
        .expect("first");

    let error = registry
        .select_version(&plugin("example-counter"), "9.9.9")
        .expect_err("selecting an uninstalled version must be refused");
    assert_eq!(error.kind(), "NotFound", "got: {error}");

    let error = registry
        .select_version(&plugin("other-plugin"), "1.4.0")
        .expect_err("selecting from an unregistered plugin must be refused");
    assert_eq!(error.kind(), "NotFound", "got: {error}");
}

#[test]
fn selecting_an_installed_version_persists() {
    let home = temp_home();
    let registry = Registry::open(&home).expect("open");
    registry
        .record_install(
            &plugin("example-counter"),
            "1.4.0",
            DIGEST_A,
            "plugins/install/a",
            100,
        )
        .expect("first");
    registry
        .record_install(
            &plugin("example-counter"),
            "1.5.0",
            DIGEST_B,
            "plugins/install/b",
            200,
        )
        .expect("second");
    registry
        .select_version(&plugin("example-counter"), "1.5.0")
        .expect("select");

    let records = Registry::open(&home).expect("reopen").list().expect("list");
    assert_eq!(records[0].selected.as_deref(), Some("1.5.0"));
}

// ---------------------------------------------------------------------------
// 坏数据：必须显式报错，不能当空表
// ---------------------------------------------------------------------------

#[test]
fn a_corrupt_registry_is_reported_and_left_untouched() {
    let home = temp_home();
    std::fs::create_dir_all(home.join("plugins")).unwrap();
    let path = home.join("plugins/registry.json");
    std::fs::write(&path, b"{ this is not json").unwrap();

    let error = Registry::open(&home).expect_err("a corrupt registry must be reported");
    assert_eq!(error.kind(), "RecoveryRequired", "got: {error}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "{ this is not json",
        "opening a corrupt registry must not rewrite it"
    );
}

#[test]
fn a_registry_corrupted_after_open_is_not_clobbered_by_a_write() {
    // open 时校验只是一层。真正危险的是「打开之后文件坏了」——比如另一个
    // 版本写了一份本进程读不懂的格式。此时写操作若把损坏当空表，用户
    // 全部安装记录会被一次安装抹掉。
    let home = temp_home();
    let registry = Registry::open(&home).expect("open");
    registry
        .record_install(
            &plugin("example-counter"),
            "1.0.0",
            DIGEST_A,
            "plugins/install/a",
            1,
        )
        .expect("seed");

    let path = home.join("plugins/registry.json");
    std::fs::write(&path, b"{ corrupted after open").unwrap();

    let error = registry
        .record_install(
            &plugin("example-counter"),
            "2.0.0",
            DIGEST_B,
            "plugins/install/b",
            2,
        )
        .expect_err("writing over a corrupt registry must fail");
    assert_eq!(error.kind(), "RecoveryRequired", "got: {error}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "{ corrupted after open",
        "the corrupt registry must be left exactly as found"
    );
}

#[test]
fn a_registry_with_an_unknown_schema_version_is_rejected() {
    let home = temp_home();
    std::fs::create_dir_all(home.join("plugins")).unwrap();
    std::fs::write(
        home.join("plugins/registry.json"),
        br#"{"schemaVersion": 99, "plugins": []}"#,
    )
    .unwrap();

    let error = Registry::open(&home).expect_err("an unknown schema version must be rejected");
    assert_eq!(error.kind(), "Incompatible", "got: {error}");
}

#[test]
fn readers_never_observe_a_partial_file() {
    // 并发读者 + 写者：读者解析失败的次数必须为 0。若写入用
    // "truncate 后直接写"，这里几乎必然抓到一次半截 JSON。
    let home = temp_home();
    let registry = Registry::open(&home).expect("open");
    registry
        .record_install(
            &plugin("example-counter"),
            "1.0.0",
            DIGEST_A,
            "plugins/install/a",
            1,
        )
        .expect("seed");

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader_stop = stop.clone();
    let reader_home = home.clone();
    let reader = std::thread::spawn(move || {
        let registry = Registry::open(&reader_home).expect("open");
        let mut reads = 0u32;
        while !reader_stop.load(std::sync::atomic::Ordering::Relaxed) {
            registry
                .list()
                .expect("a reader must never see a partial registry");
            reads += 1;
        }
        reads
    });

    for i in 0..40u32 {
        let version = format!("2.{i}.0");
        let digest = format!("{i:064x}");
        registry
            .record_install(
                &plugin("example-counter"),
                &version,
                &digest,
                "plugins/install/x",
                i as u64,
            )
            .expect("write");
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let reads = reader.join().expect("reader must not panic");
    assert!(
        reads > 0,
        "the reader thread must have actually read something"
    );
}

// ---------------------------------------------------------------------------
// 跨进程锁
// ---------------------------------------------------------------------------

#[test]
fn a_lifecycle_lock_is_exclusive_across_processes() {
    let home = temp_home();
    let path = locks::lifecycle_lock(&home);

    // 父进程持锁，子进程必须拿不到。
    let _held = locks::try_acquire(&path).expect("the parent must get the lock");

    let child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "helper_child_tries_the_lock",
            "--nocapture",
            "--ignored",
        ])
        .env("YI_PLUGIN_LOCK_PATH", &path)
        .output()
        .expect("spawn the child");

    let stdout = String::from_utf8_lossy(&child.stdout);
    assert!(
        stdout.contains("CHILD: BUSY"),
        "the child must observe BUSY while the parent holds the lock; got stdout={stdout:?} \
         stderr={:?}",
        String::from_utf8_lossy(&child.stderr)
    );
}

#[test]
fn the_lock_becomes_available_after_the_holder_drops_it() {
    let home = temp_home();
    let path = locks::lifecycle_lock(&home);

    {
        let _held = locks::try_acquire(&path).expect("acquire");
    }
    // 同进程重新取：进程内去重表必须已清理，否则这里会拿到一个陈旧的
    // `Arc`，表现为"锁永远被自己占着"。
    let again = locks::try_acquire(&path).expect("the lock must be reacquirable after drop");
    drop(again);
}

#[test]
fn two_acquires_in_one_process_yield_the_same_lock() {
    // `flock` 按**打开文件描述**加锁，不按路径：同进程两次 open 会得到两个
    // 描述符，两次 LOCK_EX 都会成功。若不做事进程内去重，"本进程已持有"
    // 就会被自己判成空闲，第二处代码以为自己独占。
    let home = temp_home();
    let path = locks::lifecycle_lock(&home);
    let first = locks::try_acquire(&path).expect("first");
    let second = locks::try_acquire(&path).expect("second");
    assert!(
        std::sync::Arc::ptr_eq(&first, &second),
        "the same path must yield the same lock within one process, \
         otherwise two call sites each believe they hold it exclusively"
    );
}

#[test]
fn dropped_locks_do_not_accumulate_in_the_process_table() {
    // 这只是内存卫生，不是互斥正确性：`existing_in_process` 对已失效的
    // `Weak` 会返回 None 并顺手清掉。但它必须被钉住——否则每个用过的
    // 路径都留一条，长驻宿主里就是一个只增不减的映射。
    let before = locks::tracked_lock_count();
    for i in 0..200 {
        let home = temp_home();
        let path = home.join("plugins").join(format!("p{i}.lock"));
        let lock = locks::try_acquire(&path).expect("acquire");
        drop(lock);
    }
    let after = locks::tracked_lock_count();
    assert!(
        after <= before + 1,
        "the process lock table must not grow with every dropped lock: \
         before={before} after={after}"
    );
}

#[test]
fn two_processes_do_not_both_hold_the_lock() {
    // 两个子进程同时抢同一把锁，必须恰好一个成功。
    //
    // 起跑线是显式的：若只靠"先后 spawn"，第一个可能已经跑完并释放，
    // 第二个才开始——那样两个都成功，测试却什么也没证明。所以约定一个
    // 未来的启动时刻，两个子进程都等到那一刻再取锁。
    let home = temp_home();
    let path = locks::lifecycle_lock(&home);
    let start_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 400;

    let spawn = |tag: &str| {
        std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "helper_child_holds_the_lock_briefly",
                "--nocapture",
                "--ignored",
            ])
            .env("YI_PLUGIN_LOCK_PATH", &path)
            .env("YI_PLUGIN_CHILD_TAG", tag)
            .env("YI_PLUGIN_START_AT_MS", start_at.to_string())
            .spawn()
            .expect("spawn")
    };

    let mut first = spawn("a");
    let mut second = spawn("b");
    let a = first.wait().expect("wait a");
    let b = second.wait().expect("wait b");

    let statuses = [a, b];
    let successes = statuses.iter().filter(|s| s.success()).count();
    assert_eq!(
        successes, 1,
        "exactly one child must win the lock, got {statuses:?}"
    );
}

#[test]
fn different_instance_homes_do_not_share_locks() {
    // 两个实例的锁路径必须不同，否则第二个实例会被第一个挡住。
    let a = temp_home();
    let b = temp_home();
    assert_ne!(locks::lifecycle_lock(&a), locks::lifecycle_lock(&b));

    let _held_a = locks::try_acquire(&locks::lifecycle_lock(&a)).expect("a");
    let _held_b = locks::try_acquire(&locks::lifecycle_lock(&b))
        .expect("a different instance home must not contend for the same lock");
}

#[test]
fn the_registry_short_lock_does_not_block_the_lifecycle_lock() {
    // 锁顺序是 lifecycle → registry。两者路径不同，所以持短锁再去取
    // 长锁不会自锁——但这条**顺序本身**由调用方遵守，这里只钉住
    // 「两把锁确实是不同的锁」。
    let home = temp_home();
    let short = locks::registry_lock(&home);
    let long = locks::lifecycle_lock(&home);
    assert_ne!(short, long);
    let _held = locks::try_acquire(&short).expect("short");
    let _other = locks::try_acquire(&long).expect("long");
}

// ---------------------------------------------------------------------------
// 子进程入口。`--ignored` 让它不出现在正常测试列表里。
// ---------------------------------------------------------------------------

#[test]
fn two_processes_recording_different_plugins_do_not_lose_an_update() {
    // 这才是 registry 短锁存在的理由。没有锁时两个进程各自「读-改-写」，
    // 后写的那个把前一个刚加的插件整条覆盖掉——读者只看到一条。
    // 单进程测试观察不到这个，必须真切两个进程。
    let home = temp_home();
    let start_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 400;

    let spawn = |plugin: &str, digest: &str| {
        std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "helper_child_records_an_install",
                "--nocapture",
                "--ignored",
            ])
            .env("YI_PLUGIN_HOME", &home)
            .env("YI_PLUGIN_NAME", plugin)
            .env("YI_PLUGIN_DIGEST", digest)
            .env("YI_PLUGIN_START_AT_MS", start_at.to_string())
            .spawn()
            .expect("spawn")
    };

    // 名字刻意不同但字典序相近，保证两者都会写进同一个文件。
    let mut a = spawn("example-alpha", DIGEST_A);
    let mut b = spawn("example-beta", DIGEST_B);
    let status_a = a.wait().expect("wait a");
    let status_b = b.wait().expect("wait b");
    assert!(status_a.success(), "child a failed: {status_a:?}");
    assert!(status_b.success(), "child b failed: {status_b:?}");

    let records = Registry::open(&home).expect("open").list().expect("list");
    let names: Vec<&str> = records.iter().map(|r| r.plugin.as_str()).collect();
    assert!(
        names.contains(&"example-alpha") && names.contains(&"example-beta"),
        "both installs must survive; a lost update means the registry short lock \
         is not doing its job. got: {names:?}"
    );
}

#[test]
#[ignore]
fn helper_child_records_an_install() {
    let home = std::env::var("YI_PLUGIN_HOME").expect("home");
    let name = std::env::var("YI_PLUGIN_NAME").expect("name");
    let digest = std::env::var("YI_PLUGIN_DIGEST").expect("digest");

    if let Ok(raw) = std::env::var("YI_PLUGIN_START_AT_MS") {
        let target: u64 = raw.parse().expect("start_at");
        loop {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            if now >= target {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    let registry = Registry::open(&home).expect("open");
    match registry.record_install(&plugin(&name), "1.0.0", &digest, "plugins/install/x", 1) {
        Ok(_) => std::process::exit(0),
        Err(error) => {
            eprintln!("record failed: {error}");
            std::process::exit(5);
        }
    }
}

#[test]
#[ignore]
fn helper_child_tries_the_lock() {
    let raw = std::env::var("YI_PLUGIN_LOCK_PATH").expect("path");
    match locks::try_acquire(Path::new(&raw)) {
        Ok(_) => println!("CHILD: GOT"),
        Err(PluginError::Busy(_)) => println!("CHILD: BUSY"),
        Err(other) => println!("CHILD: ERROR {other}"),
    }
}

#[test]
#[ignore]
fn helper_child_holds_the_lock_briefly() {
    let raw = std::env::var("YI_PLUGIN_LOCK_PATH").expect("path");
    let path = Path::new(&raw);

    // 等到共同的起跑线，让两个子进程在同一毫秒附近开始争抢。
    if let Ok(raw) = std::env::var("YI_PLUGIN_START_AT_MS") {
        let target: u64 = raw.parse().expect("start_at");
        loop {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            if now >= target {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    match locks::try_acquire(path) {
        Ok(lock) => {
            // 持有一小会儿，确保输了的那一方确实撞上冲突。
            std::thread::sleep(Duration::from_millis(150));
            drop(lock);
            std::process::exit(0);
        }
        // 输了就是输了，不再重试：重试会在赢家释放后拿到锁，
        // 于是两个都成功，这条测试就失去了意义。
        Err(PluginError::Busy(_)) => std::process::exit(3),
        Err(other) => {
            eprintln!("unexpected: {other}");
            std::process::exit(4);
        }
    }
}
