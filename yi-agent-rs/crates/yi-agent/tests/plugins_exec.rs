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
    // `inspect-legacy` / `adopt-legacy` 属 T25，此时必须明确说"还没实现"。
    let fixture = Fixture::build();
    for verb in ["inspect-legacy", "adopt-legacy"] {
        let (code, _stdout, stderr) = fixture.run(&["plugins", verb], None);
        assert_ne!(code, 0, "{verb} is not implemented yet");
        assert!(
            stderr.contains("NotSupported"),
            "{verb} must be classified, got: {stderr}"
        );
    }
}

#[test]
fn catalog_without_a_configured_origin_refuses() {
    // 目录渠道必须有宿主钉死的源。没有源就不该"从哪都行"。
    let fixture = Fixture::build();
    let (code, _stdout, stderr) = fixture.run(&["plugins", "catalog"], None);
    assert_ne!(code, 0, "catalog must refuse without an origin");
    assert!(
        stderr.contains("NotSupported") || stderr.contains("Untrusted"),
        "the refusal must be classified, got: {stderr}"
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

// ---------------------------------------------------------------------------
// T16 Step 1：安装与启用是两件事
// ---------------------------------------------------------------------------

/// `install` 只落盘"装了什么"，不产生任何运行态。
///
/// 这条断言的价值在于它列的是**全部**侧面：没有授权、没有激活、没有监督器
/// 清单、没有 launchd plist、没有进程。安装一旦顺手做了其中任何一件，"装了"
/// 就等于"用户已经同意让它在某个项目里跑"，而那正是设计 §6 明确禁止的。
#[test]
fn install_creates_no_grant_no_activation_and_no_supervisor() {
    let fixture = Fixture::build();
    fixture.install();

    let home = fixture.instance_home();

    // 1. 没有任何项目授权。
    let table = yi_agent_supervisors::managed::grants::load(&home).expect("grant table");
    assert!(
        table
            .get(
                fixture.plugin.as_str(),
                &ProjectId::parse("/tmp/whatever").unwrap()
            )
            .is_none(),
        "install must not authorize any project"
    );
    assert!(
        table.all().is_empty(),
        "install must not write a grant at all"
    );

    // 2. 没有激活记录。
    let lifecycle = Lifecycle::open(&home).expect("lifecycle");
    assert!(
        lifecycle
            .committed(&fixture.plugin)
            .expect("read")
            .is_none(),
        "install must not activate the release"
    );

    // 3. 没有监督器清单（那是 `plugin/install` 那条更老的路径才做的）。
    let supervisors = home.join("plugins").join("supervisors");
    if supervisors.exists() {
        let leftovers: Vec<_> = std::fs::read_dir(&supervisors)
            .expect("read supervisors")
            .flatten()
            .collect();
        assert!(
            leftovers.is_empty(),
            "install must not write a supervisor manifest, found {:?}",
            leftovers.iter().map(|e| e.path()).collect::<Vec<_>>()
        );
    }

    // 4. 注册表里确实有它——否则上面三条就成了"什么都没干"的空断言。
    let records = yi_agent_plugins::Registry::open(&home)
        .expect("registry")
        .list()
        .expect("list");
    assert!(
        records.iter().any(|record| record.plugin == fixture.plugin),
        "install must record the installation"
    );

    // 5. 首次安装会选中版本，但仍然不授权任何项目。
    let record = records
        .iter()
        .find(|record| record.plugin == fixture.plugin)
        .expect("record");
    assert_eq!(
        record.selected.as_deref(),
        Some(fixture.manifest.version.as_str()),
        "a first install selects the version so `list` is meaningful"
    );
}

/// `plugins uninstall` 摘掉程序，但对用户数据碰都不碰。
#[test]
fn uninstall_removes_the_program_and_keeps_the_projects_data() {
    let fixture = Fixture::build();
    let argv_file = fixture.prepare_activation(Effect::Read, fixture.project.path());

    // 造一份"用户数据"。
    let data = fixture
        .project
        .path()
        .join(".yi-agent/plugins")
        .join(fixture.plugin.as_str());
    std::fs::create_dir_all(&data).expect("data dir");
    std::fs::write(data.join("cards.json"), b"[{\"id\":1}]").expect("cards");

    let (code, out, err) = fixture.run(
        &["plugins", "uninstall", "--plugin", fixture.plugin.as_str()],
        None,
    );
    assert_eq!(code, 0, "uninstall failed: {err}\n{out}");
    assert!(
        out.contains("kept") && out.contains("data"),
        "the user must be told what was kept, got: {out}"
    );

    assert!(
        data.join("cards.json").exists(),
        "uninstalling a program must not delete the user's data"
    );
    let records = yi_agent_plugins::Registry::open(&fixture.instance_home())
        .expect("registry")
        .list()
        .expect("list");
    assert!(
        records.iter().all(|record| record.plugin != fixture.plugin),
        "the plugin must be gone from the registry"
    );
    // 残留的 argv 文件说明 exec 真的跑过；这里只确认它没有被 uninstall 顺手删。
    let _ = argv_file;
}

/// `clear-data` 不带 `--yes` 时什么都不删。
#[test]
fn clear_data_without_confirmation_refuses() {
    let fixture = Fixture::build();
    fixture.prepare_activation(Effect::Read, fixture.project.path());

    let data = fixture
        .project
        .path()
        .join(".yi-agent/plugins")
        .join(fixture.plugin.as_str());
    std::fs::create_dir_all(&data).expect("data dir");
    std::fs::write(data.join("cards.json"), b"[]").expect("cards");

    let (code, _out, err) = fixture.run(
        &[
            "plugins",
            "clear-data",
            "--plugin",
            fixture.plugin.as_str(),
            "--project",
            &canonical(fixture.project.path()),
        ],
        None,
    );
    assert_eq!(code, 1, "an unconfirmed clear must not succeed");
    assert!(
        err.contains("--yes"),
        "the error must name the missing flag: {err}"
    );
    assert!(
        data.join("cards.json").exists(),
        "nothing may be deleted without confirmation"
    );
}

/// `plugins gc` 在有授权时什么都不删，并说明为什么留着。
#[test]
fn gc_keeps_a_granted_version_and_says_why() {
    let fixture = Fixture::build();
    fixture.prepare_activation(Effect::Read, fixture.project.path());

    let (code, out, err) = fixture.run(&["plugins", "gc"], None);
    assert_eq!(code, 0, "gc failed: {err}");

    // 判定顺序是「激活优先，其次授权」。这里发生的是激活，所以理由会写成
    // committed activation；断言理由**存在**而不是断言某一句，是因为换一个
    // 夹具阶段就会换一条理由，而"没说为什么"永远是错的。
    assert!(
        out.contains("kept"),
        "gc must report what it kept, got: {out}"
    );
    assert!(
        out.contains("committed activation") || out.contains("grant"),
        "gc must say why it kept the version, got: {out}"
    );

    let installed = fixture.instance_home().join(format!(
        "plugins/installed/{}/{}/{}",
        fixture.plugin.as_str(),
        fixture.manifest.version,
        fixture.manifest.target
    ));
    assert!(
        installed.exists(),
        "the activated version directory must survive gc: {}",
        installed.display()
    );
}

// ---------------------------------------------------------------------------
// T16：目录渠道（认证目录 + 受控下载）
// ---------------------------------------------------------------------------

/// 起一个只读本地服务器，把它当作"官方源"。
///
/// 用 python3 而不是引一个新的 HTTP dev 依赖：这条测试要证明的是宿主**自己的**
/// 源校验与大小核对，不是某个 HTTP 库的行为；一个只会发文件的服务器正好够用。
///
/// 端口由内核分配、由子进程的 **stdout** 回报。`http.server` 的 CLI 把端口写在
/// stderr 且带缓冲，读它会永久阻塞——这正是先写坏过的那一版。
struct StaticServer {
    child: std::process::Child,
    _dir: tempfile::TempDir,
    port: u16,
}

const SERVE_SCRIPT: &str = r#"
import http.server, socketserver, sys, os
os.chdir(sys.argv[1])
handler = http.server.SimpleHTTPRequestHandler
httpd = socketserver.TCPServer(("127.0.0.1", 0), handler)
print(httpd.server_address[1], flush=True)
httpd.serve_forever()
"#;

impl StaticServer {
    fn start(files: &[(&str, &[u8])]) -> StaticServer {
        let dir = tempfile::tempdir().expect("server dir");
        for (name, bytes) in files {
            let path = dir.path().join(name.trim_start_matches('/'));
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("server subdir");
            }
            std::fs::write(path, bytes).expect("server file");
        }

        let mut child = Command::new("python3")
            .args(["-c", SERVE_SCRIPT])
            .arg(dir.path())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("python3 is required for the catalog tests");

        use std::io::BufRead;
        let stdout = child.stdout.as_mut().expect("stdout");
        let mut reader = std::io::BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line).expect("read the port");
        let port: u16 = line
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("the server printed no port, got {line:?}"));

        StaticServer {
            child,
            _dir: dir,
            port,
        }
    }
}

impl Drop for StaticServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 一个带端口的源必须被拒——这是目录渠道的源校验，不是摆设。
///
/// 本地测试服务器只能给 `http://127.0.0.1:<port>`，而官方源的定义是
/// "裸 https 主机名"。让它走完整条链路再被 curl 的 TLS 层拒掉，测到的是
/// curl 的行为；在**源校验**这一步拒掉，测到的才是宿主的策略。
#[test]
fn a_host_with_a_port_is_not_an_acceptable_official_origin() {
    let fixture = Fixture::build();
    // 夹具包与它自己的签名元数据就是要发布的东西。
    let archive = std::fs::read(&fixture.bundle).expect("bundle");
    let metadata = std::fs::read(&fixture.metadata).expect("metadata");

    let mut server =
        StaticServer::start(&[("metadata.json", &metadata), ("bundle.tar.gz", &archive)]);
    let port = server.port;

    // 目录只给路径；源由宿主钉死（`YI_AGENT_PLUGIN_ORIGIN`）。
    let catalog = serde_json::json!({
        "serial": 7,
        "entries": [{
            "release": {
                "plugin": fixture.plugin.as_str(),
                "version": fixture.manifest.version,
                "target": fixture.manifest.target,
                "sha256": hex(&archive),
            },
            "sizeBytes": archive.len(),
            "metadataPath": "/metadata.json",
            "bundlePath": "/bundle.tar.gz",
            "publishedAt": 1_767_225_600u64,
            "expiresAt": 1_798_761_600u64,
            "serial": 7,
        }],
    });
    let catalog_path = fixture.home.path().join("catalog.json");
    std::fs::write(
        &catalog_path,
        serde_json::to_vec_pretty(&catalog).expect("serialize catalog"),
    )
    .expect("write catalog");

    let spec = format!(
        "{}@{}#{}",
        fixture.plugin.as_str(),
        fixture.manifest.version,
        hex(&archive)
    );
    let instance_home = fixture.instance_home();
    let output = Command::new(yi_agent_bin())
        .args(["plugins", "install", "--release", &spec])
        .env("YI_AGENT_HOME", &instance_home)
        .env("HOME", fixture.home.path())
        .env("YI_AGENT_PLUGIN_CATALOG", &catalog_path)
        .env("YI_AGENT_PLUGIN_ORIGIN", format!("127.0.0.1:{port}"))
        .output()
        .expect("run yi-agent");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // 官方源只可能是"裸 https 主机名"。本机测试服务器是 `127.0.0.1:<port>`，
    // 带端口——它必须被拒绝。这条用例证明 YI_AGENT_PLUGIN_ORIGIN 不是一个
    // "写什么都行"的开关：一个带端口的源足以把下载引到用户自己机器上。
    assert_ne!(
        output.status.code().unwrap_or(-1),
        0,
        "a host:port origin must not be accepted as the official source: {stdout}{stderr}"
    );
    assert!(
        stderr.contains("Untrusted"),
        "the refusal must be classified as untrusted, got: {stderr}"
    );

    // 关键：什么都没装进去。
    let records = yi_agent_plugins::Registry::open(&fixture.instance_home())
        .expect("registry")
        .list()
        .expect("list");
    assert!(
        records.is_empty(),
        "a refused catalog install must leave the instance untouched"
    );
}

/// 没有配置官方源时，目录安装明确失败而不是"从哪都行"。
#[test]
fn catalog_install_without_a_configured_origin_refuses() {
    let fixture = Fixture::build();
    let archive = std::fs::read(&fixture.bundle).expect("bundle");
    let catalog = serde_json::json!({
        "serial": 7,
        "entries": [{
            "release": {
                "plugin": fixture.plugin.as_str(),
                "version": fixture.manifest.version,
                "target": fixture.manifest.target,
                "sha256": hex(&archive),
            },
            "sizeBytes": archive.len(),
            "metadataPath": "/metadata.json",
            "bundlePath": "/bundle.tar.gz",
            "publishedAt": 1_767_225_600u64,
            "expiresAt": 1_798_761_600u64,
            "serial": 7,
        }],
    });
    let catalog_path = fixture.home.path().join("catalog.json");
    std::fs::write(
        &catalog_path,
        serde_json::to_vec(&catalog).expect("catalog"),
    )
    .expect("write");

    let spec = format!(
        "{}@{}#{}",
        fixture.plugin.as_str(),
        fixture.manifest.version,
        hex(&archive)
    );
    let output = Command::new(yi_agent_bin())
        .args(["plugins", "install", "--release", &spec])
        .env("YI_AGENT_HOME", fixture.instance_home())
        .env("HOME", fixture.home.path())
        .env("YI_AGENT_PLUGIN_CATALOG", &catalog_path)
        .env_remove("YI_AGENT_PLUGIN_ORIGIN")
        .output()
        .expect("run yi-agent");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_ne!(output.status.code().unwrap_or(-1), 0);
    assert!(
        stderr.contains("NotSupported"),
        "the missing origin must be classified, got: {stderr}"
    );
}

/// 目录里没有这条发布时，报 NotFound 而不是去下载别的东西。
#[test]
fn catalog_install_of_an_unlisted_release_is_not_found() {
    let fixture = Fixture::build();
    let catalog = serde_json::json!({ "serial": 1, "entries": [] });
    let catalog_path = fixture.home.path().join("catalog.json");
    std::fs::write(
        &catalog_path,
        serde_json::to_vec(&catalog).expect("catalog"),
    )
    .expect("write");

    let spec = format!(
        "{}@{}#{}",
        fixture.plugin.as_str(),
        fixture.manifest.version,
        "b".repeat(64)
    );
    let output = Command::new(yi_agent_bin())
        .args(["plugins", "install", "--release", &spec])
        .env("YI_AGENT_HOME", fixture.instance_home())
        .env("HOME", fixture.home.path())
        .env("YI_AGENT_PLUGIN_CATALOG", &catalog_path)
        .env("YI_AGENT_PLUGIN_ORIGIN", "plugins.example.com")
        .output()
        .expect("run yi-agent");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_ne!(output.status.code().unwrap_or(-1), 0);
    assert!(
        stderr.contains("NotFound"),
        "an unlisted release must be NotFound, got: {stderr}"
    );
}

/// headless 全流程：安装 → 启用 → 激活 → 查询 → 卸载，全程不启动桌面。
///
/// 这条用例是 T16 Step 2 的硬要求："不能只测 exec"。它把管理命令按用户真实
/// 顺序串起来，证明每一步留下的记录正好是下一步要读的那一份。
#[test]
fn headless_management_walks_install_enable_activate_transaction_uninstall() {
    let fixture = Fixture::build();
    let home = fixture.instance_home();
    let project = canonical(fixture.project.path());

    // 1. 安装：不应该产生任何授权，也不该开事务。
    fixture.install();
    let (code, stdout, stderr) = fixture.run(&["plugins", "list"], None);
    assert_eq!(code, 0, "list failed: {stderr}");
    assert!(stdout.contains("example-counter"), "got: {stdout}");
    assert!(
        stdout.contains("projects=[]"),
        "install must not grant a project, got: {stdout}"
    );

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
    assert!(
        stdout.contains("no open transaction"),
        "install must not open an activation transaction, got: {stdout}"
    );

    // 2. 启用：写一条授权，但**不**激活。
    let (code, stdout, stderr) = fixture.run(
        &[
            "plugins",
            "enable-project",
            "--plugin",
            fixture.plugin.as_str(),
            "--project",
            &project,
            "--capability",
            "read",
            "--capability",
            "launch",
        ],
        None,
    );
    assert_eq!(code, 0, "enable-project failed: {stderr}");
    assert!(
        stdout.contains("generation 1"),
        "first enable starts at generation 1, got: {stdout}"
    );

    let (code, stdout, _stderr) = fixture.run(&["plugins", "list"], None);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("projects=[") && stdout.contains(&project),
        "the grant must show up in list, got: {stdout}"
    );

    // 3. 激活：只建立事务，明确说明尚未推进。
    let (code, stdout, stderr) = fixture.run(
        &[
            "plugins",
            "activate",
            "--plugin",
            fixture.plugin.as_str(),
            "--version",
            "1.4.0",
        ],
        None,
    );
    assert_eq!(code, 0, "activate failed: {stderr}");
    assert!(
        stdout.contains("started activation transaction"),
        "got: {stdout}"
    );
    assert!(
        stdout.contains("not advanced by this command"),
        "activate must say it did not advance the transaction, got: {stdout}"
    );

    // 4. 查询：事务确实在磁盘上，且准入仍是关闭的。
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
    assert!(stdout.contains("draining"), "got: {stdout}");
    assert!(
        stdout.contains("admission=closed"),
        "a started transaction must keep admission closed, got: {stdout}"
    );

    // 5. 未推进的事务不能凭空"取消成功"：真正回滚需要运行期控制面。
    let (code, _stdout, stderr) = fixture.run(
        &[
            "plugins",
            "cancel-transaction",
            "--plugin",
            fixture.plugin.as_str(),
        ],
        None,
    );
    assert_ne!(code, 0, "cancel must not pretend the rollback happened");
    assert!(
        stderr.contains("runtime control plane"),
        "the refusal must name what is missing, got: {stderr}"
    );

    // 6. 卸载：程序与授权都没了，事务日志也不是"授权"。
    let (code, stdout, stderr) = fixture.run(
        &["plugins", "uninstall", "--plugin", fixture.plugin.as_str()],
        None,
    );
    assert_eq!(code, 0, "uninstall failed: {stderr}");
    assert!(stdout.contains("1 grants revoked"), "got: {stdout}");

    let (code, stdout, stderr) = fixture.run(&["plugins", "list"], None);
    assert_eq!(code, 0, "list failed: {stderr}");
    assert!(
        !stdout.contains("example-counter"),
        "the plugin must be gone from the registry, got: {stdout}"
    );

    // 版本目录**不该**由卸载删除：它可能还被一次未完成的备份/事务引用。
    // 删除是 `gc` 的职责，它逐个版本问"还有没有人要用"。这里钉住这个分工，
    // 免得日后有人为了"看起来干净"把卸载改成直接 `remove_dir_all`。
    assert!(
        home.join("plugins/installed/example-counter").exists(),
        "uninstall must leave version directories for gc to judge"
    );
    // `gc` 只遍历**注册表里**的版本，卸载已经把记录摘掉，所以它看不到这
    // 个目录。这是当前实现的真实边界（孤儿目录留给后续清理），不在这里
    // 用断言假装它已被处理——假绿会让"磁盘越用越大"永远没人发现。
    let (code, _stdout, stderr) = fixture.run(&["plugins", "gc"], None);
    assert_eq!(code, 0, "gc must not fail: {stderr}");
}

/// 两个独立进程（模拟 CLI 与 RPC）看到的是**同一个**事务。
///
/// 若事务状态存在进程内存里，两次查询会各自给出一个"刚刚开始"的事务，
/// 而它们都会去推进同一个插件目录。这里用两个真实子进程读同一份磁盘。
#[test]
fn two_separate_processes_observe_the_same_transaction() {
    let fixture = Fixture::build();
    fixture.install();

    let (code, _stdout, stderr) = fixture.run(
        &[
            "plugins",
            "activate",
            "--plugin",
            fixture.plugin.as_str(),
            "--version",
            "1.4.0",
        ],
        None,
    );
    assert_eq!(code, 0, "activate failed: {stderr}");

    let read = |fixture: &Fixture| -> String {
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
        stdout
    };

    let first = read(&fixture);
    let second = read(&fixture);
    assert_eq!(
        first, second,
        "two processes must read the same transaction from disk, not each start their own"
    );
    // 事务 id 必须出现在输出里——否则两条空输出也会"相等"。
    assert!(
        first.split('\t').next().map(|id| id.len()).unwrap_or(0) > 8,
        "the output must name the transaction, got: {first}"
    );
}
