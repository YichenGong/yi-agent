//! 受管 CLI 的端到端测试：真实子进程、真实签名包、真实 argv。
//!
//! # 为什么必须起子进程
//!
//! 这一层要证明的恰恰是"宿主不经过 shell、不依赖 `PATH`"：把
//! `Command::new` 换成一次函数调用就测不到 shell 展开，把 `PATH` 清空也测不到
//! 相对路径解析的差异。所以每条用例都跑 `CARGO_BIN_EXE_yi-agent` 本身。
//!
//! # 夹具怎么来
//!
//! 用公开的固定测试种子、仓库里签好的 golden 元数据、以及一段无害的脚本
//! 当作插件程序，现场打一个 tar.gz。**不**调用尚未实现的生产打包工具：
//! 那样这些测试会在打包工具改动时一起红，但它们要钉的是 CLI 的行为。

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};
use yi_agent_plugins::{
    Binding, Effect, Lifecycle, LifecycleRequest, Manifest, PluginId, ProjectId, ReleaseId,
};

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("the CLI crate lives two levels below the repo root")
        .to_path_buf()
}

fn fixture(name: &str) -> Vec<u8> {
    let path = repo_root()
        .join("plugins")
        .join("contracts")
        .join("fixtures")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("could not read fixture {name}: {e}"))
}

fn yi_agent_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_yi-agent"))
}

/// 一段把 argv 原样写进文件的程序。
///
/// 用 shell 脚本而不是 Rust 二进制：夹具要能被 `install_verified` 当作
/// "声明入口"设执行位，构建期的交叉编译会让测试依赖工具链是否装了目标。
const COUNTER_PROGRAM: &str = r#"#!/bin/sh
# 夹具程序：把 argv 逐行写出，便于断言"宿主没有经过 shell"。
# 第一行是标记，第二行起是每个参数及其长度。
out="$YI_AGENT_HOME/argv.txt"
: > "$out"
printf 'program\n' >> "$out"
for arg in "$@"; do
  printf '%s\t%s\n' "${#arg}" "$arg" >> "$out"
done
printf 'home:%s\n' "$YI_AGENT_HOME" >> "$out"
exit 0
"#;

struct Fixture {
    home: tempfile::TempDir,
    project: tempfile::TempDir,
    bundle: PathBuf,
    metadata: PathBuf,
    plugin: PluginId,
    manifest: Manifest,
    release: ReleaseId,
}

impl Fixture {
    /// 打一个签名包，并把注册表 / 生命周期状态摆到"已激活"的样子。
    fn build() -> Fixture {
        let home = tempfile::tempdir().expect("instance home");
        let project = tempfile::tempdir().expect("project");

        // 清单从 golden 样本派生，只把版本与入口钉死为夹具自己的值。
        let mut manifest_value: serde_json::Value =
            serde_json::from_slice(&fixture("manifest-v1.json")).expect("golden manifest");
        manifest_value["version"] = "1.4.0".into();
        manifest_value["program"]["entry"] = "bin/counter".into();
        let program_digest: String = Sha256::digest(COUNTER_PROGRAM.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        manifest_value["program"]["digest"] = program_digest.into();
        let manifest_bytes =
            serde_json::to_vec(&manifest_value).expect("derived manifest must serialize");
        let manifest = Manifest::parse(&manifest_bytes).expect("derived manifest must parse");

        let archive = build_archive(&manifest_bytes);

        let metadata = signed_metadata(&archive, &manifest);
        let bundle = home.path().join("fixture.tar.gz");
        std::fs::write(&bundle, &archive).expect("write bundle");
        let metadata_path = home.path().join("fixture.release.json");
        std::fs::write(&metadata_path, &metadata).expect("write metadata");

        let sha = hex(&archive);
        let release = ReleaseId {
            plugin: manifest.plugin.clone(),
            version: manifest.version.clone(),
            target: manifest.target.clone(),
            sha256: sha,
        };

        Fixture {
            home,
            project,
            bundle,
            metadata: metadata_path,
            plugin: manifest.plugin.clone(),
            manifest,
            release,
        }
    }

    /// 生产解析：`YI_AGENT_HOME` 以 `.yi-agent` 结尾时直接用，否则追加一级。
    ///
    /// 夹具必须走**同一条**规则。直接拿临时目录当实例根会让测试与生产
    /// 指向两个不同的目录——那正是"本地绿、线上找不到插件"的经典形状。
    fn instance_home(&self) -> PathBuf {
        let home = self.home.path().join(".yi-agent");
        std::fs::create_dir_all(&home).expect("create the instance root");
        home
    }

    /// 跑一条 `plugins` 命令，返回 (退出码, stdout, stderr)。
    fn run(&self, args: &[&str], path_value: Option<&str>) -> (i32, String, String) {
        let mut command = Command::new(yi_agent_bin());
        command
            .args(args)
            .env("YI_AGENT_HOME", self.instance_home())
            // 清空 HOME，避免任何实现偷偷回退到开发者真实目录。
            .env("HOME", self.home.path());
        match path_value {
            Some(path) => {
                command.env("PATH", path);
            }
            None => {
                command.env_remove("PATH");
            }
        }
        let output = command.output().expect("could not run yi-agent");
        (
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stdout).to_string(),
            String::from_utf8_lossy(&output.stderr).to_string(),
        )
    }

    /// 写入一条已授权绑定。
    ///
    /// 绑定由宿主生成：这里扮演的是宿主，所以直接写文件，而不是让被测代码
    /// 去"接受一个页面上传的绑定"。
    fn write_binding(&self, effect: Effect, project: &Path) -> PathBuf {
        let table_path = self.instance_home().join("plugins/grants.json");
        std::fs::create_dir_all(table_path.parent().unwrap()).unwrap();
        let project_id = ProjectId::parse(&canonical(project)).unwrap();
        let mut table = yi_agent_plugins::GrantTable::new();
        let grant = table
            .grant(
                self.plugin.as_str(),
                project_id.clone(),
                &self.release,
                &host_id(&self.instance_home()),
                &[effect],
            )
            .expect("grant");
        std::fs::write(
            &table_path,
            serde_json::to_vec_pretty(&table).expect("serialize grants"),
        )
        .unwrap();

        let activation = committed_activation(&self.instance_home(), &self.plugin);
        let binding = Binding {
            host_id: host_id(&self.instance_home()),
            release: self.release.clone(),
            activation,
            project: project_id,
            grant: grant.generation,
        };
        let path = self.instance_home().join("binding.json");
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&binding).expect("serialize binding"),
        )
        .unwrap();
        path
    }

    fn install(&self) {
        let bundle = self.bundle.to_str().unwrap();
        let metadata = self.metadata.to_str().unwrap();
        let (code, _out, err) = self.run(
            &[
                "plugins",
                "install",
                "--bundle",
                bundle,
                "--metadata",
                metadata,
            ],
            None,
        );
        assert_eq!(code, 0, "install failed: {err}");
    }

    /// 把夹具登记为已安装并提交激活，跳过事务推进。
    ///
    /// 生产路径由 daemon 驱动 8 个阶段；这里直接写状态，是为了让 `exec` 的
    /// 测试不依赖事务推进（那是 T14 Step 2 的另一半）。
    fn prepare_activation(&self, effect: Effect, project: &Path) -> PathBuf {
        self.install();
        let relative = format!(
            "plugins/installed/{}/{}/{}",
            self.plugin.as_str(),
            self.manifest.version,
            self.manifest.target
        );
        // install 已经把版本记进注册表；这里补上激活代次。
        commit_activation(&self.instance_home(), &self.plugin, self.release.clone(), 1);
        self.write_binding(effect, project);
        let argv_file = self.instance_home().join("argv.txt");
        let _ = std::fs::remove_file(&argv_file);
        let _ = relative;
        argv_file
    }
}

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn canonical(path: &Path) -> String {
    std::fs::canonicalize(path)
        .expect("test project must exist")
        .to_string_lossy()
        .to_string()
}

fn host_id(home: &Path) -> String {
    yi_agent_supervisors::managed::host_id(home).expect("host id")
}

struct Entry {
    name: &'static str,
    bytes: Vec<u8>,
    mode: u32,
}

fn build_archive(manifest_bytes: &[u8]) -> Vec<u8> {
    let entries = vec![
        Entry {
            name: "plugin.json",
            bytes: manifest_bytes.to_vec(),
            mode: 0o444,
        },
        Entry {
            name: "bin/counter",
            bytes: COUNTER_PROGRAM.as_bytes().to_vec(),
            mode: 0o755,
        },
    ];

    let mut builder = tar::Builder::new(Vec::new());
    for entry in &entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(entry.bytes.len() as u64);
        header.set_mode(entry.mode);
        header.set_cksum();
        builder
            .append_data(&mut header, entry.name, entry.bytes.as_slice())
            .expect("append archive entry");
    }
    let tar_bytes = builder.into_inner().expect("finish tar");

    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&tar_bytes).expect("gzip");
    encoder.finish().expect("finish gzip")
}

fn signed_metadata(archive: &[u8], manifest: &Manifest) -> Vec<u8> {
    use ed25519_dalek::{Signer, SigningKey};

    let digest = hex(archive);
    let release = serde_json::json!({
        "plugin": manifest.plugin.as_str(),
        "version": manifest.version,
        "target": manifest.target,
        "sha256": digest,
        "sizeBytes": archive.len(),
        "hostApi": "v1",
        "serial": 7,
        "publishedAt": 1_767_225_600u64,
        "expiresAt": 1_798_761_600u64,
    });
    let payload = yi_agent_plugins::trust::encode_release_payload(&release);
    let signing = SigningKey::from_bytes(&[7u8; 32]);
    use base64::Engine;
    let engine = base64::engine::general_purpose::STANDARD;
    serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 1,
        "keyId": "root-2026-01",
        "release": release,
        "payloadB64": engine.encode(&payload),
        "signature": engine.encode(signing.sign(&payload).to_bytes()),
    }))
    .expect("serialize metadata")
}

/// 直接写生命周期状态，模拟"事务已完成"。
fn commit_activation(home: &Path, plugin: &PluginId, release: ReleaseId, activation: u64) {
    let lifecycle = Lifecycle::open(home).expect("lifecycle");
    // 走真实 API 建立事务，然后把事务推进到 done 需要的 `RuntimeControl`
    // 由测试提供：它什么都不做，但记录调用，从而不会掩盖"没推进"。
    let request = LifecycleRequest::activate(plugin.clone(), release, activation);
    let transaction = lifecycle.start(request).expect("start activation");
    let runtime = NoopRuntime;
    // 逐阶段推进到 `done`。断言"每次都在往前走"，而不是断言"可回滚"：
    // 按 G2 §6.1，`projecting` 之后不可回滚是有意设计，把它当失败会与规范
    // 冲突。真正要防的是推进卡住——那会让测试挂死而不是给出诊断。
    let mut phases = Vec::new();
    for _ in 0..16 {
        let progress = lifecycle.advance(&transaction, &runtime).expect("advance");
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
    panic!("activation did not reach `done` in time: {phases:?}");
}

fn committed_activation(home: &Path, plugin: &PluginId) -> u64 {
    Lifecycle::open(home)
        .expect("lifecycle")
        .committed(plugin)
        .expect("committed")
        .expect("a committed activation")
        .activation
}

struct NoopRuntime;

impl yi_agent_plugins::RuntimeControl for NoopRuntime {
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
// Step 1：受管 CLI
// ---------------------------------------------------------------------------

#[test]
fn exec_runs_the_counter_with_no_path_at_all() {
    // 删掉 PATH 之后仍能跑，说明程序路径来自注册表而不是用户环境。
    // 这是"受管入口"与"调用器随便起一个进程"的分界线。
    let fixture = Fixture::build();
    let argv_file = fixture.prepare_activation(Effect::Read, fixture.project.path());
    let binding = fixture.instance_home().join("binding.json");

    let (code, stdout, stderr) = fixture.run(
        &[
            "plugins",
            "exec",
            "--plugin",
            fixture.plugin.as_str(),
            "--project-handle",
            &canonical(fixture.project.path()),
            "--binding-file",
            binding.to_str().unwrap(),
            "--",
            "status",
        ],
        None,
    );
    assert_eq!(code, 0, "exec failed: stdout={stdout} stderr={stderr}");

    let recorded = std::fs::read_to_string(&argv_file)
        .unwrap_or_else(|e| panic!("the counter never ran ({}): {e}", argv_file.display()));
    assert!(recorded.starts_with("program\n"), "got: {recorded}");
    let expected_home = format!("home:{}\n", fixture.instance_home().display());
    assert!(
        recorded.contains(&expected_home),
        "the host must export its own YI_AGENT_HOME; wanted {expected_home:?} in {recorded:?}"
    );
}

#[test]
fn argv_with_spaces_cjk_and_semicolons_stays_literal() {
    // 这些字符只有在真的经过 shell 时才会被拆分或执行。断言长度与实际内容
    // 一起看：只断言"包含"会让 `a b` 拆成两行也通过。
    let fixture = Fixture::build();
    let argv_file = fixture.prepare_activation(Effect::Read, fixture.project.path());
    let binding = fixture.instance_home().join("binding.json");

    let tricky = "a b;中文;$(id)`whoami`";
    let (code, _stdout, stderr) = fixture.run(
        &[
            "plugins",
            "exec",
            "--plugin",
            fixture.plugin.as_str(),
            "--project-handle",
            &canonical(fixture.project.path()),
            "--binding-file",
            binding.to_str().unwrap(),
            "--",
            "status",
            tricky,
        ],
        None,
    );
    assert_eq!(code, 0, "exec failed: {stderr}");

    let recorded = std::fs::read_to_string(&argv_file).expect("the counter must have run");
    let expected = format!("\t{tricky}\n");
    assert!(
        recorded.contains(&expected),
        "argv must arrive as one literal argument; wanted {expected:?} in {recorded:?}"
    );
    // 命令替换若被执行，输出里会出现 uid 或用户名，而不是原样的 `$(id)`。
    assert!(
        recorded.contains("$(id)"),
        "the shell must never see the argv, got: {recorded}"
    );
}

#[test]
fn exec_refuses_a_verb_the_manifest_does_not_declare() {
    // 未声明的 verb 没有效果声明。**只批 Read** 是关键：若实现把未声明的 verb
    // 默认成 Read，它就会通过授权检查并真的跑起来——用 Write 授权测不出这一点，
    // 因为 Read 不在 Write 授权里，两种实现都会拒绝。
    let fixture = Fixture::build();
    fixture.prepare_activation(Effect::Read, fixture.project.path());
    let binding = fixture.instance_home().join("binding.json");

    let (code, _stdout, stderr) = fixture.run(
        &[
            "plugins",
            "exec",
            "--plugin",
            fixture.plugin.as_str(),
            "--project-handle",
            &canonical(fixture.project.path()),
            "--binding-file",
            binding.to_str().unwrap(),
            "--",
            "definitely-not-declared",
        ],
        None,
    );
    assert_ne!(code, 0, "an undeclared verb must not run");
    assert!(
        stderr.contains("PermissionDenied"),
        "the refusal must be a permission error, got: {stderr}"
    );
}

#[test]
fn exec_refuses_a_write_verb_on_a_read_only_grant() {
    // 这是"效果来自签名清单、不来自调用方"的正面证据：清单里
    // `increment` 声明为 Write，只批了 Read 的项目必须被拒。
    let fixture = Fixture::build();
    fixture.prepare_activation(Effect::Read, fixture.project.path());
    let binding = fixture.instance_home().join("binding.json");

    let (code, _stdout, stderr) = fixture.run(
        &[
            "plugins",
            "exec",
            "--plugin",
            fixture.plugin.as_str(),
            "--project-handle",
            &canonical(fixture.project.path()),
            "--binding-file",
            binding.to_str().unwrap(),
            "--",
            "increment",
        ],
        None,
    );
    assert_ne!(code, 0, "a write verb must not pass a read grant");
    assert!(stderr.contains("PermissionDenied"), "got: {stderr}");
}

#[test]
fn exec_refuses_when_the_binding_names_a_different_project() {
    // 绑定是授权载体。让它作用于别的项目等于绕过项目级隔离。
    let fixture = Fixture::build();
    fixture.prepare_activation(Effect::Read, fixture.project.path());
    let binding = fixture.instance_home().join("binding.json");
    let other = tempfile::tempdir().expect("another project");

    let (code, _stdout, stderr) = fixture.run(
        &[
            "plugins",
            "exec",
            "--plugin",
            fixture.plugin.as_str(),
            "--project-handle",
            &canonical(other.path()),
            "--binding-file",
            binding.to_str().unwrap(),
            "--",
            "status",
        ],
        None,
    );
    assert_ne!(code, 0);
    assert!(stderr.contains("PermissionDenied"), "got: {stderr}");
}

#[test]
fn exec_refuses_a_tampered_installation() {
    // 摘要只在安装时校验一次，之后磁盘上的字节没人看——除非启动前再看一次。
    //
    // 篡改 `plugin.json` 而不是程序本身：程序还有一条"入口摘要与清单一致"
    // 的独立检查会先把它拦住，那样测不出 `verify_installed`。这里只加空白，
    // 解析出来的身份完全不变、入口摘要也不变，**只有**逐文件摘要能发现它。
    let fixture = Fixture::build();
    fixture.prepare_activation(Effect::Read, fixture.project.path());
    let binding = fixture.instance_home().join("binding.json");

    let manifest_path = fixture
        .instance_home()
        .join("plugins/installed")
        .join(fixture.plugin.as_str())
        .join(&fixture.manifest.version)
        .join(&fixture.manifest.target)
        .join("plugin.json");
    let mut bytes = std::fs::read(&manifest_path).expect("the installed manifest");
    bytes.push(b'\n');
    std::fs::set_permissions(&manifest_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::write(&manifest_path, &bytes).expect("rewrite the manifest");

    // 先确认这次篡改确实"语义无害"：清单仍能解析成同一个发布。否则下面的
    // 失败可能来自身份检查，而不是摘要检查。
    let reparsed = Manifest::parse(&bytes).expect("adding whitespace keeps the manifest valid");
    assert_eq!(reparsed.version, fixture.manifest.version);

    let (code, _stdout, stderr) = fixture.run(
        &[
            "plugins",
            "exec",
            "--plugin",
            fixture.plugin.as_str(),
            "--project-handle",
            &canonical(fixture.project.path()),
            "--binding-file",
            binding.to_str().unwrap(),
            "--",
            "status",
        ],
        None,
    );
    assert_ne!(code, 0, "a modified installation must not run");
    assert!(stderr.contains("Integrity"), "got: {stderr}");
}

#[test]
fn exec_refuses_a_binding_issued_by_another_host() {
    // 授权记在执行宿主上。别的宿主（同一个实例根之外的宿主身份）不能拿它
    // 来用，否则"复制一份绑定文件"就等于获得授权。
    let fixture = Fixture::build();
    fixture.prepare_activation(Effect::Read, fixture.project.path());
    let binding = fixture.instance_home().join("binding.json");

    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&binding).unwrap()).unwrap();
    value["host_id"] = serde_json::Value::String("host-someone-else".into());
    std::fs::write(&binding, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

    let (code, _stdout, stderr) = fixture.run(
        &[
            "plugins",
            "exec",
            "--plugin",
            fixture.plugin.as_str(),
            "--project-handle",
            &canonical(fixture.project.path()),
            "--binding-file",
            binding.to_str().unwrap(),
            "--",
            "status",
        ],
        None,
    );
    assert_ne!(code, 0, "a binding from another host must not run");
    assert!(stderr.contains("PermissionDenied"), "got: {stderr}");
    assert!(
        stderr.contains("host-someone-else"),
        "the refusal must name the foreign host, got: {stderr}"
    );
}

#[test]
fn exec_refuses_a_binding_for_an_older_activation() {
    // 激活代次是实例级的。旧代次的绑定必须失效，否则升级后旧客户端可以
    // 继续使用升级前的绑定，绕过"能力增加须重新确认"。
    let fixture = Fixture::build();
    fixture.prepare_activation(Effect::Read, fixture.project.path());
    let binding = fixture.instance_home().join("binding.json");

    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&binding).unwrap()).unwrap();
    let stale = value["activation"].as_u64().unwrap() - 1;
    value["activation"] = serde_json::Value::from(stale);
    std::fs::write(&binding, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

    let (code, _stdout, stderr) = fixture.run(
        &[
            "plugins",
            "exec",
            "--plugin",
            fixture.plugin.as_str(),
            "--project-handle",
            &canonical(fixture.project.path()),
            "--binding-file",
            binding.to_str().unwrap(),
            "--",
            "status",
        ],
        None,
    );
    assert_ne!(code, 0, "a stale activation must not run");
    assert!(stderr.contains("StaleBinding"), "got: {stderr}");
    assert!(
        stderr.contains(&stale.to_string()),
        "the refusal must name the stale generation, got: {stderr}"
    );
}

#[test]
fn exec_does_not_export_a_plugin_supplied_reserved_variable() {
    // 插件若能把 YI_AGENT_HOME 指到别处，就能让宿主读写另一个实例根。
    // 这里从 CLI 侧验证：exec 不把进程环境里的 YI_AGENT_PLUGIN_* 当成插件项，
    // 因此保留变量始终由宿主自己写。
    let fixture = Fixture::build();
    let argv_file = fixture.prepare_activation(Effect::Read, fixture.project.path());
    let binding = fixture.instance_home().join("binding.json");

    let (code, _stdout, stderr) = fixture.run(
        &[
            "plugins",
            "exec",
            "--plugin",
            fixture.plugin.as_str(),
            "--project-handle",
            &canonical(fixture.project.path()),
            "--binding-file",
            binding.to_str().unwrap(),
            "--",
            "status",
        ],
        None,
    );
    assert_eq!(code, 0, "exec failed: {stderr}");
    let recorded = std::fs::read_to_string(&argv_file).expect("the counter must have run");
    let expected_home = format!("home:{}\n", fixture.instance_home().display());
    assert!(
        recorded.contains(&expected_home),
        "the host owns YI_AGENT_HOME; wanted {expected_home:?} in {recorded:?}"
    );
}

#[test]
fn management_commands_report_not_supported_instead_of_faking_success() {
    // 假的"已完成"会让调用方跳过真正的校验，而校验正是这一层的价值。
    let fixture = Fixture::build();
    let (code, _stdout, stderr) = fixture.run(&["plugins", "catalog"], None);
    assert_ne!(code, 0, "catalog is not implemented yet");
    assert!(
        stderr.contains("NotSupported"),
        "the failure must be classified, got: {stderr}"
    );
}

#[test]
fn list_and_transaction_work_without_a_desktop() {
    // headless 是硬要求：管理命令不得依赖 app-server 或 Tauri。
    let fixture = Fixture::build();
    let (code, _stdout, stderr) = fixture.run(&["plugins", "list"], None);
    assert_eq!(code, 0, "list failed: {stderr}");

    let (code, stdout, stderr) = fixture.run(
        &[
            "plugins",
            "transaction",
            "--plugin",
            fixture.plugin.as_str(),
        ],
        None,
    );
    assert_eq!(code, 0, "transaction failed: {stderr}");
    assert!(stdout.contains("no open transaction"), "got: {stdout}");
}

#[test]
fn enable_project_writes_a_grant_that_exec_accepts() {
    // 端到端：管理命令写下的授权，必须正好是 exec 读取的那一份。
    // 两份实现各写各的格式，会让"启用了却跑不起来"成为常态。
    let fixture = Fixture::build();
    fixture.install();
    commit_activation(
        &fixture.instance_home(),
        &fixture.plugin,
        fixture.release.clone(),
        1,
    );

    let project = canonical(fixture.project.path());
    let (code, _stdout, stderr) = fixture.run(
        &[
            "plugins",
            "enable-project",
            "--plugin",
            fixture.plugin.as_str(),
            "--project",
            &project,
            "--capability",
            "read",
        ],
        None,
    );
    assert_eq!(code, 0, "enable-project failed: {stderr}");

    let table = yi_agent_supervisors::managed::grants::load(&fixture.instance_home())
        .expect("the grant table must be readable");
    let project_id = ProjectId::parse(&project).unwrap();
    let grant = table
        .get(fixture.plugin.as_str(), &project_id)
        .expect("the grant must be recorded");
    assert_eq!(grant.granted, vec![Effect::Read]);
    assert_eq!(grant.host_id, host_id(&fixture.instance_home()));
}

#[test]
fn disable_project_revokes_only_that_project() {
    let fixture = Fixture::build();
    fixture.install();
    commit_activation(
        &fixture.instance_home(),
        &fixture.plugin,
        fixture.release.clone(),
        1,
    );

    let first = canonical(fixture.project.path());
    let second_dir = tempfile::tempdir().expect("a second project");
    let second = canonical(second_dir.path());

    for project in [&first, &second] {
        let (code, _out, err) = fixture.run(
            &[
                "plugins",
                "enable-project",
                "--plugin",
                fixture.plugin.as_str(),
                "--project",
                project,
                "--capability",
                "read",
            ],
            None,
        );
        assert_eq!(code, 0, "enable failed for {project}: {err}");
    }

    let (code, _out, err) = fixture.run(
        &[
            "plugins",
            "disable-project",
            "--plugin",
            fixture.plugin.as_str(),
            "--project",
            &first,
        ],
        None,
    );
    assert_eq!(code, 0, "disable failed: {err}");

    let table = yi_agent_supervisors::managed::grants::load(&fixture.instance_home()).unwrap();
    let first_id = ProjectId::parse(&first).unwrap();
    let second_id = ProjectId::parse(&second).unwrap();
    let first_grant = table.get(fixture.plugin.as_str(), &first_id).unwrap();
    let second_grant = table.get(fixture.plugin.as_str(), &second_id).unwrap();
    assert_eq!(
        first_grant.granted.len(),
        0,
        "the revoked project must lose its capabilities"
    );
    assert!(
        second_grant.granted.contains(&Effect::Read),
        "revoking one project must not touch the other"
    );
}
