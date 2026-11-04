//! 受管启动的装配测试：真实签名包、真实授权表、真实对账循环。
//!
//! # 这一层要证明什么
//!
//! `managed.rs` 的单元测试钉的是**每一步守卫各自会拒绝**；这里钉的是
//! **监督器真的走了这些守卫**。两者不能互相代替：守卫全对而装配点忘了调用，
//! 线上照样会按清单里的 `command` 启动。
//!
//! 所以每条用例都从磁盘开始（签好的包 + 授权表 + 监督器清单），只驱动
//! `Supervisor::reconcile()`，然后看**实际起了什么进程**：
//!
//! - 清单里的 `command` 指向一个"被篡改"的脚本，它会在项目里留下标记文件。
//!   标记文件出现 = 宿主按清单启动了，也就是绕过了摘要与授权。
//! - 受管入口指向包内的程序，它留下另一个标记文件。
//!
//! 只看"进程起来了"分不清这两者，标记文件才能分得清。
//!
//! # 夹具怎么来
//!
//! 现场打 tar.gz 并用公开测试种子签名，和 `yi-agent/tests/plugins_exec.rs`
//! 同一套做法。**不**调用尚未实现的生产打包工具——那样这些测试会随打包
//! 工具一起变，而不是钉住启动行为。

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use yi_agent_plugins::{
    Effect, ExtractLimits, Lifecycle, LifecycleRequest, Manifest, PluginId, ProjectId, ReleaseId,
    RuntimeControl, TrustStore, install_verified,
};
use yi_agent_supervisors::managed::{JournalGate, ManagedGuard};
use yi_agent_supervisors::supervisor::{Layout, Supervisor};

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("the crate lives two levels below the repo root")
        .to_path_buf()
}

fn fixture(name: &str) -> Vec<u8> {
    let path = repo_root().join("plugins/contracts/fixtures").join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("could not read fixture {name}: {e}"))
}

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// 受管入口程序：只留一个标记文件。
///
/// 用 `/bin/sh` 而不是编译型二进制：夹具要能在这里被构建出来，而交叉编译
/// 依赖会让测试绑上工具链。脚本本身不读任何输入，行为完全由"是否被执行"
/// 决定——这正是这些用例唯一要看的东西。
const MANAGED_PROGRAM: &str = "#!/bin/sh\ntouch \"$YI_AGENT_HOME/managed.marker\"\nexit 0\n";

/// 清单里那条"被篡改"的命令：留下另一个标记。
///
/// 标记路径写死而不是读 `YI_AGENT_HOME`：未装配的普通监督器**不导出**那个
/// 变量（它由受管入口设置），夹具要能在这两种情形下都真的留下痕迹。
const MANIFEST_PROGRAM: &str = "#!/bin/sh\ntouch \"{marker}\"\nexit 0\n";

struct Harness {
    /// 实例根（`.yi-agent`）。不是上层临时目录：`resolve` 按实例根拼路径。
    home: PathBuf,
    workdir: PathBuf,
    _temp: tempfile::TempDir,
    plugin: PluginId,
    manifest: Manifest,
    release: ReleaseId,
}

impl Harness {
    /// 装好一个签名包并把生命周期推进到 `done`（准入开放）。
    fn install() -> Harness {
        let temp = tempfile::tempdir().expect("temp dir");
        let home = temp.path().join(".yi-agent");
        let workdir = temp.path().join("project");
        std::fs::create_dir_all(&home).expect("instance root");
        std::fs::create_dir_all(&workdir).expect("project");

        let mut value: serde_json::Value =
            serde_json::from_slice(&fixture("manifest-v1.json")).expect("golden manifest");
        value["program"]["entry"] = "bin/counter".into();
        value["program"]["digest"] = hex(MANAGED_PROGRAM.as_bytes()).into();
        let manifest_bytes = serde_json::to_vec(&value).expect("serialize manifest");
        let manifest = Manifest::parse(&manifest_bytes).expect("parse manifest");

        let archive = build_archive(&manifest_bytes);
        let (verified, _) = signed_release(&archive);

        let relative = format!(
            "plugins/installed/{}/{}/{}",
            manifest.plugin.as_str(),
            manifest.version,
            manifest.target
        );
        let destination = home.join(&relative);
        std::fs::create_dir_all(destination.parent().expect("parent")).expect("install dirs");
        install_verified(
            &verified,
            &manifest,
            &archive,
            &destination,
            &ExtractLimits::default(),
        )
        .expect("installation must succeed");

        yi_agent_plugins::Registry::open(&home)
            .expect("registry")
            .record_install(
                &manifest.plugin,
                &manifest.version,
                &verified.release.sha256,
                &relative,
                1_780_000_000,
            )
            .expect("record install");

        commit_activation(&home, &manifest);

        Harness {
            home,
            workdir,
            _temp: temp,
            release: verified.release,
            plugin: manifest.plugin.clone(),
            manifest,
        }
    }

    fn project_id(&self) -> ProjectId {
        ProjectId::parse(&canonical(&self.workdir)).expect("project id")
    }

    /// 写一条授权。`effects` 决定这个项目被允许做什么。
    fn authorize(&self, effects: &[Effect]) {
        let path = self.home.join("plugins/grants.json");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("grants dir");
        let mut table = yi_agent_plugins::GrantTable::new();
        table
            .grant(
                self.plugin.as_str(),
                self.project_id(),
                &self.release,
                &yi_agent_supervisors::managed::host_id(&self.home).expect("host id"),
                effects,
            )
            .expect("grant");
        std::fs::write(&path, serde_json::to_vec_pretty(&table).expect("serialize"))
            .expect("write grants");
    }

    /// 写一份监督器清单，它的 `command` 指向"篡改版"程序。
    ///
    /// 清单名与插件名可以不同——本 crate 不认识插件名。这里刻意用同一个名字，
    /// 因为装配点正是按清单名把两者对上。
    fn write_manifest(&self, name: &str) {
        let dir = self.workdir.join(".yi-agent/supervisors");
        std::fs::create_dir_all(&dir).expect("manifests dir");
        let tampered = self.workdir.join("tampered.sh");
        let script =
            MANIFEST_PROGRAM.replace("{marker}", &self.manifest_marker().display().to_string());
        std::fs::write(&tampered, script).expect("tampered program");
        std::fs::set_permissions(&tampered, std::fs::Permissions::from_mode(0o755))
            .expect("exec bit");
        let json = format!(
            r#"{{"name":"{name}","command":"{}","args":[],"switch_key":"{name}_on","restart_backoff_ms":40,"restart_backoff_max_ms":160,"stop_when_disabled":false}}"#,
            tampered.display()
        );
        std::fs::write(dir.join(format!("{name}.json")), &json).expect("write manifest");
    }

    /// 打开这个清单名对应的开关。
    fn enable(&self, name: &str) {
        let dir = self.workdir.join(".yi-agent");
        std::fs::create_dir_all(&dir).expect("state dir");
        std::fs::write(
            dir.join("flags.json"),
            format!(r#"{{"flags":{{"{name}_on":true}}}}"#),
        )
        .expect("write flags");
    }

    fn managed_marker(&self) -> PathBuf {
        self.home.join("managed.marker")
    }

    fn manifest_marker(&self) -> PathBuf {
        self.home.join("manifest.marker")
    }
}

fn canonical(path: &Path) -> String {
    std::fs::canonicalize(path)
        .expect("the test path exists")
        .to_string_lossy()
        .to_string()
}

fn build_archive(manifest_bytes: &[u8]) -> Vec<u8> {
    let entries: Vec<(&str, Vec<u8>, u32)> = vec![
        ("plugin.json", manifest_bytes.to_vec(), 0o444),
        ("bin/counter", MANAGED_PROGRAM.as_bytes().to_vec(), 0o755),
    ];

    let mut builder = tar::Builder::new(Vec::new());
    for (name, bytes, mode) in &entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(*mode);
        header.set_cksum();
        builder
            .append_data(&mut header, name, bytes.as_slice())
            .expect("append archive entry");
    }
    let tar_bytes = builder.into_inner().expect("finish tar");

    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&tar_bytes).expect("gzip");
    encoder.finish().expect("finish gzip")
}

/// 用公开测试种子签一个发布，并经真实 `TrustStore` 验签。
///
/// 不在这里重算"应该怎么签"：签名字节由 `trust::encode_release_payload`
/// 定义，测试造一份自己的编码规则就会与实现悄悄分叉。
fn signed_release(archive: &[u8]) -> (yi_agent_plugins::VerifiedRelease, Manifest) {
    use ed25519_dalek::{Signer, SigningKey};

    let release = serde_json::json!({
        "plugin": "example-counter",
        "version": "1.4.0",
        "target": "darwin-aarch64",
        "sha256": hex(archive),
        "sizeBytes": archive.len(),
        "hostApi": "v1",
        "serial": 7,
        "publishedAt": 1_767_225_600u64,
        "expiresAt": 1_798_761_600u64,
    });
    let payload = yi_agent_plugins::trust::encode_release_payload(&release);
    let signing = SigningKey::from_bytes(&[7u8; 32]);
    let envelope = serde_json::json!({
        "schemaVersion": 1,
        "keyId": "root-2026-01",
        "release": release,
        "payloadB64": b64(&payload),
        "signature": b64(&signing.sign(&payload).to_bytes()),
    });
    let envelope_bytes = serde_json::to_vec(&envelope).expect("serialize envelope");

    let mut store = TrustStore::bootstrap(&fixture("root-v1.json")).expect("root envelope");
    let verified = store
        .verify_release(&envelope_bytes, 1_780_000_000, 0)
        .expect("the test release must verify");

    // 清单只用于返回给调用方比对；它已随归档一起打进包里。
    let value: serde_json::Value =
        serde_json::from_slice(&fixture("manifest-v1.json")).expect("golden manifest");
    let manifest =
        Manifest::parse(&serde_json::to_vec(&value).expect("serialize")).expect("parse manifest");
    (verified, manifest)
}

fn commit_activation(home: &Path, manifest: &Manifest) {
    let lifecycle = Lifecycle::open(home).expect("lifecycle");
    let release = ReleaseId {
        plugin: manifest.plugin.clone(),
        version: manifest.version.clone(),
        target: manifest.target.clone(),
        sha256: String::new(),
    };
    // 摘要必须与注册表里那份一致，否则 `resolve` 找不到安装。
    let record = yi_agent_plugins::Registry::open(home)
        .expect("registry")
        .list()
        .expect("list")
        .into_iter()
        .find(|r| r.plugin == manifest.plugin)
        .expect("the plugin was just recorded");
    let installed = record.installed.last().expect("an installed version");
    let release = ReleaseId {
        sha256: installed.sha256.clone(),
        ..release
    };

    let transaction = lifecycle
        .start(LifecycleRequest::activate(
            manifest.plugin.clone(),
            release,
            1,
        ))
        .expect("start activation");
    let mut phases = Vec::new();
    for _ in 0..16 {
        let progress = lifecycle
            .advance(&transaction, &NoopRuntime)
            .expect("advance");
        if progress.admission_open {
            return;
        }
        assert!(
            !phases.contains(&progress.phase),
            "phase {:?} repeated: advancing made no progress ({phases:?})",
            progress.phase
        );
        phases.push(progress.phase);
    }
    panic!("activation never reached done: {phases:?}");
}

struct NoopRuntime;

impl RuntimeControl for NoopRuntime {
    fn pause_maintenance(&self, _plugin: &PluginId) -> Result<(), yi_agent_plugins::PluginError> {
        Ok(())
    }
    fn resume_maintenance(&self, _plugin: &PluginId) -> Result<(), yi_agent_plugins::PluginError> {
        Ok(())
    }
    fn stop_active(&self, _plugin: &PluginId) -> Result<(), yi_agent_plugins::PluginError> {
        Ok(())
    }
    fn start_candidate(
        &self,
        _plugin: &PluginId,
        _release: &ReleaseId,
    ) -> Result<(), yi_agent_plugins::PluginError> {
        Ok(())
    }
    fn stop_candidate(&self, _plugin: &PluginId) -> Result<(), yi_agent_plugins::PluginError> {
        Ok(())
    }
    fn health_check(
        &self,
        _plugin: &PluginId,
        _project: &str,
    ) -> Result<(), yi_agent_plugins::PluginError> {
        Ok(())
    }
    fn migrate_project(
        &self,
        _plugin: &PluginId,
        _project: &str,
        _from: Option<&ReleaseId>,
        _to: &ReleaseId,
    ) -> Result<(), yi_agent_plugins::PluginError> {
        Ok(())
    }
    fn repair_projection(
        &self,
        _plugin: &PluginId,
        _release: &ReleaseId,
    ) -> Result<(), yi_agent_plugins::PluginError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 等待工具
// ---------------------------------------------------------------------------

/// 串行化会真的起子进程的用例。
///
/// 与 `supervisor.rs` 同一个理由：同进程内并行 fork 多个 `/bin/sh` 在负载高
/// 时会拖到几秒，固定预算就会假红。
static SPAWN_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SPAWN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

const BUDGET: Duration = Duration::from_secs(20);

fn wait_for(mut condition: impl FnMut() -> bool, what: &str) -> bool {
    let deadline = Instant::now() + BUDGET;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            eprintln!("timed out after {BUDGET:?} waiting for {what}");
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// 给"不该发生"的断言留一点真实时间：进程真的被拉起来需要时间，
/// 立刻断言不存在会永远成立，那是一条假绿。
fn settle() {
    std::thread::sleep(Duration::from_millis(400));
}

/// 造一个把清单名接到受管启动器上的监督器。
fn supervised(harness: &Harness, name: &str) -> Supervisor {
    let layout = Layout::for_workdir(&harness.workdir);
    let guard = ManagedGuard::new(
        &harness.home,
        harness.plugin.clone(),
        Box::new(JournalGate::open(&harness.home).expect("journal gate")),
    );
    Supervisor::new(layout, "dev".to_string())
        .with_flags_path(harness.workdir.join(".yi-agent/flags.json"))
        .with_managed(name, guard, harness.project_id(), Vec::new())
}

// ---------------------------------------------------------------------------
// 用例
// ---------------------------------------------------------------------------

#[test]
fn a_managed_manifest_is_started_through_the_host_entrypoint() {
    let _guard = serial();
    let harness = Harness::install();
    harness.authorize(&[Effect::Read, Effect::Launch]);
    harness.write_manifest("example-counter");
    harness.enable("example-counter");

    let mut supervisor = supervised(&harness, "example-counter");
    supervisor.reconcile();

    assert!(
        wait_for(|| harness.managed_marker().exists(), "the managed program"),
        "the host entrypoint must run the packaged program"
    );
    assert!(
        !harness.manifest_marker().exists(),
        "the manifest's own `command` must never run for a managed plugin"
    );
}

#[test]
fn a_managed_manifest_without_a_grant_is_not_started_at_all() {
    // 关键在"两个标记都不出现"：被拒绝时既不能按清单启动，也不能"因为没授权
    // 所以退回清单命令"。后者是最坏的形状——一次被拒绝的启动变成一次成功的启动。
    let _guard = serial();
    let harness = Harness::install();
    harness.write_manifest("example-counter");
    harness.enable("example-counter");

    let mut supervisor = supervised(&harness, "example-counter");
    for _ in 0..3 {
        supervisor.reconcile();
        std::thread::sleep(Duration::from_millis(120));
    }
    settle();

    assert!(
        !harness.managed_marker().exists(),
        "an unauthorized plugin must not run through the host entrypoint"
    );
    assert!(
        !harness.manifest_marker().exists(),
        "a refused launch must not fall back to the manifest command"
    );
}

#[test]
fn a_legacy_manifest_cannot_override_the_managed_launcher() {
    // 这条与第一条互补：第一条证明"走了受管入口"，这条证明"清单命令确实
    // 被忽略"。若装配点只在受管命令失败时回退，第一条仍会绿。
    //
    // 用**篡改过的清单**推进：把受管装配去掉（`with_managed` 不调用），
    // 标记文件就会出现在 manifest 一侧——那正是这条要防的形状。
    let _guard = serial();
    let harness = Harness::install();
    harness.authorize(&[Effect::Read, Effect::Launch]);
    harness.write_manifest("example-counter");
    harness.enable("example-counter");

    // 未装配：起点是"按清单启动"，用来确认夹具本身是有效的。
    let layout = Layout::for_workdir(&harness.workdir);
    let mut plain = Supervisor::new(layout, "dev".to_string())
        .with_flags_path(harness.workdir.join(".yi-agent/flags.json"));
    plain.reconcile();
    assert!(
        wait_for(
            || harness.manifest_marker().exists(),
            "the unmanaged fixture"
        ),
        "the fixture must actually be capable of running the manifest command"
    );
    plain.stop_all();
    std::fs::remove_file(harness.manifest_marker()).expect("clear the marker");

    // 装配后：同一个清单必须改走受管入口。
    let mut managed = supervised(&harness, "example-counter");
    managed.reconcile();
    assert!(
        wait_for(|| harness.managed_marker().exists(), "the managed program"),
        "once assembled, the manifest's command must be replaced by the host entrypoint"
    );
    assert!(
        !harness.manifest_marker().exists(),
        "the legacy manifest command must not run alongside the managed launcher"
    );
    managed.stop_all();
}

#[test]
fn maintenance_keeps_the_backend_stopped() {
    let _guard = serial();
    let harness = Harness::install();
    harness.authorize(&[Effect::Read, Effect::Launch]);
    harness.write_manifest("example-counter");
    harness.enable("example-counter");

    let mut supervisor = supervised(&harness, "example-counter");
    supervisor.reconcile();
    assert!(
        wait_for(|| harness.managed_marker().exists(), "the first launch"),
        "the plugin must start before the maintenance transaction"
    );

    // 开一个维护事务并停在第一个不可准入的阶段。
    let lifecycle = Lifecycle::open(&harness.home).expect("lifecycle");
    let record = yi_agent_plugins::Registry::open(&harness.home)
        .expect("registry")
        .list()
        .expect("list")
        .into_iter()
        .find(|r| r.plugin == harness.plugin)
        .expect("record");
    let installed = record.installed.last().expect("installed");
    let release = ReleaseId {
        plugin: harness.plugin.clone(),
        version: harness.manifest.version.clone(),
        target: harness.manifest.target.clone(),
        sha256: installed.sha256.clone(),
    };
    let transaction = lifecycle
        .start(LifecycleRequest::activate(
            harness.plugin.clone(),
            release,
            2,
        ))
        .expect("start a maintenance transaction");
    let progress = lifecycle
        .advance(&transaction, &NoopRuntime)
        .expect("advance into maintenance");
    assert!(
        !progress.admission_open,
        "the transaction must still be open at this point"
    );

    // 维护期间反复对账：必须停进程且不再拉起。
    std::fs::remove_file(harness.managed_marker()).expect("clear the marker");
    for _ in 0..4 {
        supervisor.reconcile();
        std::thread::sleep(Duration::from_millis(120));
    }
    settle();
    assert_eq!(
        supervisor.running_count(),
        0,
        "a maintenance transaction must stop the supervised backend"
    );
    assert!(
        !harness.managed_marker().exists(),
        "the backend must stay stopped for the whole transaction"
    );
}

#[test]
fn the_guard_itself_refuses_a_launch_without_a_grant() {
    // 直接问 `ManagedGuard`，不经过监督器：这样这条红例只可能被授权那一层
    // 打红。放在监督器层不行——那里还有"读不到闸门就拒"的兜底，删掉授权检查
    // 测试仍然是绿的，等于没钉住。
    let harness = Harness::install();
    harness.write_manifest("example-counter");
    // **不**写任何授权。

    let guard = ManagedGuard::new(
        &harness.home,
        harness.plugin.clone(),
        Box::new(JournalGate::open(&harness.home).expect("journal gate")),
    );
    let error = match guard.command_in(&harness.project_id(), &[], &harness.workdir) {
        Err(error) => error,
        Ok(_) => panic!("a plugin with no grant must not produce a launchable command"),
    };
    assert_eq!(error.kind(), "PermissionDenied", "got: {error}");
    assert!(
        error.to_string().contains("not authorized"),
        "the refusal must say why, got: {error}"
    );
}

#[test]
fn a_corrupt_journal_stops_a_backend_that_is_already_running() {
    // 与下一条（从未启动过）互补：这条钉的是"已经在跑的后端必须被停掉"。
    // 只看"没启动"会让 `ensure_running` 的错误分支掉到 spawn 的第二道检查
    // 上，那条分支就没人测了。
    let _guard = serial();
    let harness = Harness::install();
    harness.authorize(&[Effect::Read, Effect::Launch]);
    harness.write_manifest("example-counter");
    harness.enable("example-counter");

    let mut supervisor = supervised(&harness, "example-counter");
    supervisor.reconcile();
    assert!(
        wait_for(|| harness.managed_marker().exists(), "the first launch"),
        "the backend must be running before the journal is corrupted"
    );
    assert_eq!(supervisor.running_count(), 1, "one backend is tracked");

    let journal = yi_agent_plugins::JournalStore::open(&harness.home)
        .expect("journal store")
        .path(&harness.plugin);
    std::fs::write(&journal, b"{ this is not json").expect("corrupt the journal");

    supervisor.reconcile();
    assert_eq!(
        supervisor.running_count(),
        0,
        "an unreadable journal must stop the backend, not leave it running"
    );
}

#[test]
fn the_supervisor_starts_nothing_while_the_journal_is_unreadable() {
    // 读不到闸门状态时**不能**当成"没在维护"：那会在迁移期间拉起后端。
    // 这里把 journal 写坏，断言既不启动也不回退清单命令。
    let _guard = serial();
    let harness = Harness::install();
    harness.authorize(&[Effect::Read, Effect::Launch]);
    harness.write_manifest("example-counter");
    harness.enable("example-counter");

    // 路径取自实现本身，不手拼：手拼会写到一个没人读的位置，测试就变成
    // "坏文件根本没被看到"，那是一条假绿。
    let journal = yi_agent_plugins::JournalStore::open(&harness.home)
        .expect("journal store")
        .path(&harness.plugin);
    std::fs::create_dir_all(journal.parent().expect("parent")).expect("journal dir");
    std::fs::write(&journal, b"{ this is not json").expect("write a broken journal");

    let mut supervisor = supervised(&harness, "example-counter");
    for _ in 0..3 {
        supervisor.reconcile();
        std::thread::sleep(Duration::from_millis(120));
    }
    settle();

    assert!(
        !harness.managed_marker().exists(),
        "an unreadable journal must not be read as `not in maintenance`"
    );
    assert!(
        !harness.manifest_marker().exists(),
        "an unreadable journal must not fall back to the manifest command"
    );
}
