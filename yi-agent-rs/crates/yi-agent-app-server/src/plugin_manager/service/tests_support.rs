//! 测试夹具：现场打一个**真实签名**的包，写进临时目录。
//!
//! 用真实签名而不是假的 `VerifiedRelease`：服务层的第一步就是验签，夹具若
//! 绕过它，被测的就不是真实路径。签名种子与 `plugin_manager/tests.rs` 同一份
//! ——两份种子意味着两份"测试根"，而信任根的唯一性正是这一层要保证的事。

use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// 公开的测试种子。正式构建里没有它。
const TEST_SEED: [u8; 32] = [7u8; 32];
const NOW_CEILING: u64 = 1_798_761_600;

/// 一个已打好签名的包，落在临时目录里。
pub(crate) struct Fixture {
    _temp: tempfile::TempDir,
    bundle: PathBuf,
    metadata: PathBuf,
    /// 打出来的包摘要。T20 之前没有读者；现在离线缓存测试要拿它当键，
    /// 故保留而不是删字段（删掉会让 `Fixture::build` 少一次摘要计算，
    /// 而那份计算结果正是"包与该摘要绑定"的测试依据）。
    #[allow(dead_code)]
    pub(crate) sha256: String,
}

impl Fixture {
    pub(crate) fn build() -> Fixture {
        Self::build_version("1.4.0", 7)
    }

    pub(crate) fn build_version(version: &str, serial: u64) -> Fixture {
        let temp = tempfile::tempdir().expect("temp dir");
        let manifest = manifest_bytes(version);
        let archive = build_archive(&manifest);
        let metadata = signed_metadata(&archive, version, serial);

        let bundle = temp.path().join("bundle.tar.gz");
        let metadata_path = temp.path().join("metadata.json");
        std::fs::write(&bundle, &archive).expect("write bundle");
        std::fs::write(&metadata_path, &metadata).expect("write metadata");

        Fixture {
            bundle,
            metadata: metadata_path,
            sha256: hex(&archive),
            _temp: temp,
        }
    }

    pub(crate) fn bundle(&self) -> &Path {
        &self.bundle
    }

    pub(crate) fn metadata(&self) -> &Path {
        &self.metadata
    }
}

/// 一份合法清单（与 golden 夹具同形，只换版本号）。
fn manifest_bytes(version: &str) -> Vec<u8> {
    let golden = repo_root().join("plugins/contracts/fixtures/manifest-v1.json");
    let text = std::fs::read_to_string(&golden).expect("golden manifest");
    let mut value: serde_json::Value = serde_json::from_str(&text).expect("golden json");
    value["version"] = serde_json::Value::String(version.to_string());
    serde_json::to_vec(&value).expect("manifest json")
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("the crate lives two levels below the repo root")
        .to_path_buf()
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

const PROGRAM: &str = "#!/bin/sh\nexit 0\n";
const SKILL: &str = "---\nname: example-counter\ndescription: count things\n---\nbody";

/// 打一个 tar.gz。清单声明的每个文件都真的在里面——归档里缺一个文件，
/// 解包时才会报错，那时测的就不是验签而是解包。
fn build_archive(manifest_bytes: &[u8]) -> Vec<u8> {
    let program_digest = hex(PROGRAM.as_bytes());
    let main_digest = hex(b"<html>main</html>");
    let settings_digest = hex(b"<html>settings</html>");
    // 清单里的摘要必须与真实文件一致，否则解包校验会拒。
    let manifest = String::from_utf8_lossy(manifest_bytes).to_string();
    let manifest = manifest
        .replace(
            "5d41402abc4b2a76b9719d911017c5920e1a2b3c4d5e6f708192a3b4c5d6e7f8",
            &program_digest,
        )
        .replace(
            "3b1f2c9a7d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8",
            &main_digest,
        )
        .replace(
            "9c8b7a69584736251403f2e1d0c9b8a7968574635241302f1e0d9c8b7a695847",
            &settings_digest,
        );

    let entries: Vec<(&str, Vec<u8>, u32)> = vec![
        ("plugin.json", manifest.into_bytes(), 0o444),
        ("bin/example-counter", PROGRAM.as_bytes().to_vec(), 0o755),
        ("ui/index.html", b"<html>main</html>".to_vec(), 0o444),
        ("ui/settings.html", b"<html>settings</html>".to_vec(), 0o444),
        (
            "skills/example-counter/SKILL.md",
            SKILL.as_bytes().to_vec(),
            0o444,
        ),
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

    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&tar_bytes).expect("gzip");
    encoder.finish().expect("finish gzip")
}

/// 用公开测试种子签一份发布元数据。
fn signed_metadata(archive: &[u8], version: &str, serial: u64) -> Vec<u8> {
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
        "expiresAt": NOW_CEILING,
    });
    let payload = yi_agent_plugins::trust::encode_release_payload(&release);
    let signing = SigningKey::from_bytes(&TEST_SEED);
    serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 1,
        "keyId": "root-2026-01",
        "release": release,
        "payloadB64": b64(&payload),
        "signature": b64(&signing.sign(&payload).to_bytes()),
    }))
    .expect("serialize metadata")
}
