//! `plugin_manager` 的判定测试：真实签名包、真实注册表、真实生命周期。
//!
//! # 为什么不用假的 `Lifecycle`
//!
//! 本模块的全部价值在于"把会话记忆与磁盘事实比出来"。若把 `Lifecycle` 换成
//! 一个返回预设值的替身，测的就变成"我写的比较逻辑"，而不是"会话绑定的版本
//! 在真实升级/回滚之后会怎样"。所以这里和 `managed_launch.rs` 用同一套夹具：
//! 现场打 tar.gz、用公开测试种子签名、`install_verified` 落盘、`Lifecycle`
//! 推进到 `done`。升级就是再走一遍同样的流程，得到一个新的 `activation`。
//!
//! # 每条用例针对一个具体错误
//!
//! - 升级后旧会话若被判 `Current`，就是让新版本无审计地继承旧信任；
//! - 回滚掉插件后若仍报 `Stale`，用户会去"重建会话"，而正确动作是重装；
//! - 代次相同但摘要不同（磁盘被人改过）若判 `Current`，摘要校验等于白做。

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use yi_agent_plugins::{
    Binding, Effect, ExtractLimits, Lifecycle, LifecycleRequest, Manifest, PluginId, ProjectId,
    ReleaseId, RuntimeControl, TrustStore,
};

use super::{BindingState, SessionBindings, binding_state};

// ---------------------------------------------------------------------------
// 夹具（与 `yi-agent-supervisors/tests/managed_launch.rs` 同一套做法）
// ---------------------------------------------------------------------------

const TEST_SEED: [u8; 32] = [7u8; 32];
const NOW: u64 = 1_780_000_000;

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

const MANAGED_PROGRAM: &str = "#!/bin/sh\nexit 0\n";

struct Harness {
    home: PathBuf,
    workdir: PathBuf,
    _temp: tempfile::TempDir,
    plugin: PluginId,
}

impl Harness {
    fn new() -> Harness {
        let temp = tempfile::tempdir().expect("temp dir");
        let home = temp.path().join(".yi-agent");
        let workdir = temp.path().join("project");
        std::fs::create_dir_all(&home).expect("instance root");
        std::fs::create_dir_all(&workdir).expect("project");
        Harness {
            home,
            workdir,
            _temp: temp,
            plugin: PluginId::parse("example-counter").expect("plugin id"),
        }
    }

    fn home(&self) -> &Path {
        &self.home
    }

    fn project_id(&self) -> ProjectId {
        ProjectId::parse(&canonical(&self.workdir)).expect("project id")
    }

    /// 装一个指定版本的包，并把生命周期推进到 `done`。返回该版本的 `ReleaseId`。
    ///
    /// 每次调用产生一个新的 `activation`（代次递增），这正是"升级"的样子。
    fn install_and_activate(&self, version: &str, serial: u64, expected: u64) -> ReleaseId {
        let mut value: serde_json::Value =
            serde_json::from_slice(&fixture("manifest-v1.json")).expect("golden manifest");
        value["version"] = version.into();
        value["program"]["entry"] = "bin/counter".into();
        value["program"]["digest"] = hex(MANAGED_PROGRAM.as_bytes()).into();
        let manifest_bytes = serde_json::to_vec(&value).expect("serialize manifest");
        let manifest = Manifest::parse(&manifest_bytes).expect("parse manifest");
        assert_eq!(manifest.version, version);

        let archive = build_archive(&manifest_bytes);
        let verified = signed_release(&archive, version, serial);

        let relative = format!(
            "plugins/installed/{}/{}/{}",
            manifest.plugin.as_str(),
            manifest.version,
            manifest.target
        );
        let destination = self.home.join(&relative);
        std::fs::create_dir_all(destination.parent().expect("parent")).expect("install dirs");
        yi_agent_plugins::install_verified(
            &verified,
            &manifest,
            &archive,
            &destination,
            &ExtractLimits::default(),
        )
        .expect("installation must succeed");

        yi_agent_plugins::Registry::open(&self.home)
            .expect("registry")
            .record_install(
                &manifest.plugin,
                &manifest.version,
                &verified.release.sha256,
                &relative,
                NOW,
            )
            .expect("record install");

        self.commit(&manifest, expected)
    }

    fn commit(&self, manifest: &Manifest, expected: u64) -> ReleaseId {
        let lifecycle = Lifecycle::open(&self.home).expect("lifecycle");
        let record = yi_agent_plugins::Registry::open(&self.home)
            .expect("registry")
            .list()
            .expect("list")
            .into_iter()
            .find(|r| r.plugin == manifest.plugin)
            .expect("the plugin was just recorded");
        let installed = record
            .installed
            .iter()
            .find(|v| v.version == manifest.version)
            .expect("the version was just installed");
        let release = ReleaseId {
            plugin: manifest.plugin.clone(),
            version: manifest.version.clone(),
            target: manifest.target.clone(),
            sha256: installed.sha256.clone(),
        };

        let transaction = lifecycle
            .start(LifecycleRequest::activate(
                manifest.plugin.clone(),
                release.clone(),
                expected,
            ))
            .expect("start activation");
        for _ in 0..16 {
            let progress = lifecycle
                .advance(&transaction, &NoopRuntime)
                .expect("advance");
            if progress.admission_open {
                return release;
            }
        }
        panic!("activation never reached done");
    }

    fn binding(&self, release: &ReleaseId, activation: u64, grant: u64) -> Binding {
        Binding {
            host_id: yi_agent_supervisors::managed::host_id(&self.home).expect("host id"),
            release: release.clone(),
            activation,
            project: self.project_id(),
            grant,
        }
    }

    fn authorize(&self, release: &ReleaseId, effects: &[Effect]) {
        let path = self.home.join("plugins/grants.json");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("grants dir");
        let mut table = yi_agent_plugins::GrantTable::new();
        table
            .grant(
                self.plugin.as_str(),
                self.project_id(),
                release,
                &yi_agent_supervisors::managed::host_id(&self.home).expect("host id"),
                effects,
            )
            .expect("grant");
        std::fs::write(&path, serde_json::to_vec_pretty(&table).expect("serialize"))
            .expect("write grants");
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

fn signed_release(archive: &[u8], version: &str, serial: u64) -> yi_agent_plugins::VerifiedRelease {
    use ed25519_dalek::{Signer, SigningKey};

    let release = serde_json::json!({
        "plugin": "example-counter",
        "version": version,
        "target": "darwin-aarch64",
        "sha256": hex(archive),
        "sizeBytes": archive.len(),
        "hostApi": "v1",
        "serial": serial,
        "publishedAt": 1_767_225_600u64,
        "expiresAt": 1_798_761_600u64,
    });
    let payload = yi_agent_plugins::trust::encode_release_payload(&release);
    let signing = SigningKey::from_bytes(&TEST_SEED);
    let envelope = serde_json::json!({
        "schemaVersion": 1,
        "keyId": "root-2026-01",
        "release": release,
        "payloadB64": b64(&payload),
        "signature": b64(&signing.sign(&payload).to_bytes()),
    });
    let envelope_bytes = serde_json::to_vec(&envelope).expect("serialize envelope");

    let mut store = TrustStore::bootstrap(&fixture("root-v1.json")).expect("root envelope");
    store
        .verify_release(&envelope_bytes, NOW, 0)
        .expect("the test release must verify")
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
// 用例
// ---------------------------------------------------------------------------

/// 刚建会话、插件也没变过：绑定应当是当前。
#[test]
fn a_fresh_binding_points_at_the_committed_activation() {
    let h = Harness::new();
    let release = h.install_and_activate("1.4.0", 7, 1);
    h.authorize(&release, &[Effect::Read, Effect::Write]);

    let state = binding_state(h.home(), &h.binding(&release, 1, 1)).expect("state");
    assert_eq!(state, BindingState::Current);
}

/// 插件升到新版本后，旧会话必须判陈旧，并说出两边的版本号。
///
/// 这是本模块存在的主要理由。若判成 `Current`，一个新版本会继承旧会话的
/// 上下文与授权，而它从未在那个上下文里被验证过。
#[test]
fn an_upgrade_makes_the_old_binding_stale_and_names_both_versions() {
    let h = Harness::new();
    let old = h.install_and_activate("1.4.0", 7, 1);
    let binding = h.binding(&old, 1, 1);

    let new = h.install_and_activate("1.5.0", 8, 2);
    assert_ne!(new.version, old.version);

    let state = binding_state(h.home(), &binding).expect("state");
    match state {
        BindingState::Stale {
            bound_activation,
            bound_version,
            committed_activation,
            committed_version,
        } => {
            assert_eq!(bound_activation, 1);
            assert_eq!(bound_version, "1.4.0");
            assert_eq!(committed_activation, 2);
            assert_eq!(committed_version, "1.5.0");
        }
        other => panic!("an upgrade must make the old binding stale, got {other:?}"),
    }
}

/// 陈旧绑定的调用必须被拒，且错误里同时有两个版本号。
///
/// 只说 "stale binding" 不够：用户要据此决定"重建会话"，而重建的理由正是
/// 他知道自己被绑在哪个旧版本上。
#[test]
fn a_stale_binding_is_refused_with_both_versions_in_the_message() {
    let h = Harness::new();
    let old = h.install_and_activate("1.4.0", 7, 1);
    let bindings = SessionBindings::new(vec![h.binding(&old, 1, 1)]);
    h.authorize(&old, &[Effect::Read]);

    h.install_and_activate("1.5.0", 8, 2);

    let error = bindings
        .check(h.home(), "example-counter", Effect::Read)
        .expect_err("a stale binding must be refused");
    assert_eq!(error.kind(), "StaleBinding", "got: {error}");
    let message = error.to_string();
    assert!(
        message.contains("1.4.0"),
        "must name the bound version: {message}"
    );
    assert!(
        message.contains("1.5.0"),
        "must name the committed version: {message}"
    );
}

/// 完全没绑这个插件时，不是"陈旧"而是"没绑"。
///
/// 两者都拒，但错误要能分开：一个普通会话调插件是被误用（提示去建插件会话），
/// 一个旧会话调插件是升级导致（提示重建会话）。混成一句话会让用户去查错方向。
#[test]
fn an_unbound_session_is_refused_as_unbound_not_as_stale() {
    let h = Harness::new();
    let release = h.install_and_activate("1.4.0", 7, 1);
    h.authorize(&release, &[Effect::Read]);

    let bindings = SessionBindings::default();
    assert!(bindings.is_empty());
    let error = bindings
        .check(h.home(), "example-counter", Effect::Read)
        .expect_err("an unbound session must be refused");
    assert_eq!(error.kind(), "StaleBinding");
    assert!(
        error.to_string().contains("not bound"),
        "the message must say the session is not bound: {error}"
    );
}

/// 绑定的插件被整体撤掉（没有已提交激活）时，报 `Gone` 而不是 `Stale`。
///
/// 用户下一步动作不同：`Stale` 要重建会话，`Gone` 要重装并激活。
#[test]
fn a_binding_whose_plugin_was_never_committed_is_gone() {
    let h = Harness::new();
    // 一个从未提交过任何激活的插件。
    let phantom = ReleaseId {
        plugin: PluginId::parse("never-installed").expect("id"),
        version: "9.9.9".into(),
        target: "darwin-aarch64".into(),
        sha256: "00".repeat(32),
    };
    let binding = Binding {
        host_id: "host".into(),
        release: phantom,
        activation: 1,
        project: h.project_id(),
        grant: 1,
    };
    assert_eq!(
        binding_state(h.home(), &binding).expect("state"),
        BindingState::Gone
    );
}

/// 代次相同但摘要被改过，必须判陈旧。
///
/// 这条防的是"只看 activation 就放行"：摘要校验是整套发布信任的基础，
/// 若这里判 `Current`，摘要就只在安装时被检查过一次，之后谁都能改。
#[test]
fn a_binding_whose_digest_does_not_match_the_committed_one_is_stale() {
    let h = Harness::new();
    let release = h.install_and_activate("1.4.0", 7, 1);
    let mut binding = h.binding(&release, 1, 1);
    binding.release.sha256 = "11".repeat(32);

    match binding_state(h.home(), &binding).expect("state") {
        BindingState::Stale { bound_version, .. } => assert_eq!(bound_version, "1.4.0"),
        other => panic!("a mismatched digest must be stale, got {other:?}"),
    }
}

/// 能力检查发生在陈旧检查**之后**，且陈旧优先。
///
/// 一个陈旧绑定上问"有没有权限"没有意义。若先答权限，用户会看到
/// `PermissionDenied` 去改授权，而真正要做的是重建会话。
#[test]
fn staleness_is_reported_before_a_missing_capability() {
    let h = Harness::new();
    let old = h.install_and_activate("1.4.0", 7, 1);
    // 只授 Read。
    h.authorize(&old, &[Effect::Read]);
    let bindings = SessionBindings::new(vec![h.binding(&old, 1, 1)]);

    h.install_and_activate("1.5.0", 8, 2);

    let error = bindings
        .check(h.home(), "example-counter", Effect::Write)
        .expect_err("must be refused");
    assert_eq!(
        error.kind(),
        "StaleBinding",
        "staleness must win over the missing Write capability, got: {error}"
    );
}

/// 当前绑定 + 缺少能力 = `PermissionDenied`，不能混成陈旧。
#[test]
fn a_current_binding_without_the_capability_is_a_permission_denial() {
    let h = Harness::new();
    let release = h.install_and_activate("1.4.0", 7, 1);
    h.authorize(&release, &[Effect::Read]);
    let bindings = SessionBindings::new(vec![h.binding(&release, 1, 1)]);

    let error = bindings
        .check(h.home(), "example-counter", Effect::Write)
        .expect_err("Write was never granted");
    assert_eq!(error.kind(), "PermissionDenied", "got: {error}");
}

/// 当前绑定 + 有权限 = 放行，并返回那条绑定。
#[test]
fn a_current_binding_with_the_capability_is_allowed() {
    let h = Harness::new();
    let release = h.install_and_activate("1.4.0", 7, 1);
    h.authorize(&release, &[Effect::Read, Effect::Write]);
    let binding = h.binding(&release, 1, 1);
    let bindings = SessionBindings::new(vec![binding.clone()]);

    let allowed = bindings
        .check(h.home(), "example-counter", Effect::Write)
        .expect("Write was granted");
    assert_eq!(allowed, &binding);
}
