//! `plugin-pack` 的 round-trip：打出来的包必须经**生产** verifier 与
//! extractor 装得进去，而不只是"看起来像个 tar"。
//!
//! 这些用例走 `yi_agent_plugins` 的公开 API，与 cmdline 共用同一实现；
//! 命令行只是把它们接到 argparse 上。

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use yi_agent_plugins::limits::ExtractLimits;
use yi_agent_plugins::pack::{PACKAGE_MANIFEST, pack_directory};
use yi_agent_plugins::{Manifest, TrustStore};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("crate lives two levels below the repo root")
        .to_path_buf()
}

fn fixture(name: &str) -> PathBuf {
    repo_root().join("plugins/contracts/fixtures").join(name)
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// 测试身份：与 `tests/common/mod.rs` 同一种子，公开而非生产。
fn signing_key(dir: &Path) -> PathBuf {
    use base64::Engine;
    let path = dir.join("signing-key.json");
    let seed = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "keyId": "root-2026-01",
            "privateKey": seed,
        }))
        .unwrap(),
    )
    .unwrap();
    path
}

/// 一个最小的、清单与内容自洽的插件目录。
fn plugin_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("ui")).unwrap();
    std::fs::write(root.join("ui/index.html"), b"<html>v1</html>").unwrap();
    std::fs::create_dir_all(root.join("skills/demo")).unwrap();
    std::fs::write(root.join("skills/demo/SKILL.md"), b"# demo").unwrap();
    let manifest = serde_json::json!({
        "schemaVersion": 1,
        "packageFormat": 1,
        "plugin": "demo",
        "version": "1.0.0",
        "target": "darwin-aarch64",
        "views": [{
            "id": "main",
            "kind": "main",
            "entry": "ui/index.html",
            "digest": hex(Sha256::digest(b"<html>v1</html>")),
        }],
        "program": null,
        "skills": [{ "name": "demo", "path": "skills/demo" }],
        "methods": [{ "name": "demo/read", "effect": "Read" }],
        "cli": [{ "verb": "demo:read", "effect": "Read" }],
        "capabilities": { "required": ["Read"], "optional": [] },
        "dependencies": { "workflows": [] },
        "data": { "format": 1, "migrations": [] },
        "compatibility": { "hostApi": "v1", "uiApi": "v1" }
    });
    std::fs::write(
        root.join(PACKAGE_MANIFEST),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    dir
}

/// 测试里自己签一个信封。
///
/// 签名器本体在 `src/bin/plugin-pack.rs`（不随宿主发布）；测试目标自己持有
/// 一份最小实现，用公开的 `encode_release_payload` 保证与宿主同一编码规则。
fn sign(packed: &yi_agent_plugins::Packed, key: &Path) -> Vec<u8> {
    use base64::Engine;
    use ed25519_dalek::{Signer, SigningKey};
    let raw = std::fs::read(key).unwrap();
    let key: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    let key_id = key["keyId"].as_str().unwrap();
    let seed = base64::engine::general_purpose::STANDARD
        .decode(key["privateKey"].as_str().unwrap())
        .unwrap();
    let signing = SigningKey::from_bytes(&seed.try_into().unwrap());

    let release = serde_json::json!({
        "plugin": packed.plugin,
        "version": packed.version,
        "target": packed.target,
        "sha256": packed.sha256,
        "sizeBytes": packed.archive.len(),
        "hostApi": "v1",
        "serial": 7,
        "publishedAt": 1_767_225_600u64,
        "expiresAt": 1_798_761_600u64,
    });
    let payload = yi_agent_plugins::trust::encode_release_payload(&release);
    serde_json::to_vec_pretty(&serde_json::json!({
        "schemaVersion": 1,
        "keyId": key_id,
        "release": release,
        "payloadB64": base64::engine::general_purpose::STANDARD.encode(&payload),
        "signature": base64::engine::general_purpose::STANDARD.encode(signing.sign(&payload).to_bytes()),
    }))
    .unwrap()
}

#[test]
fn a_packed_release_installs_through_the_production_verifier_and_extractor() {
    let dir = plugin_dir();
    let packed = pack_directory(dir.path()).unwrap();
    let key = signing_key(dir.path());
    let envelope = sign(&packed, &key);
    let root = std::fs::read(fixture("root-v1.json")).unwrap();

    let mut store = TrustStore::bootstrap(&root).unwrap();
    let verified = store.verify_release(&envelope, 1_780_000_000, 0).unwrap();
    assert_eq!(verified.release.sha256, packed.sha256);

    let manifest =
        Manifest::parse(&std::fs::read(dir.path().join(PACKAGE_MANIFEST)).unwrap()).unwrap();
    let out = tempfile::tempdir().unwrap();
    let destination = out.path().join("installed");
    let installed = yi_agent_plugins::install_verified(
        &verified,
        &manifest,
        &packed.archive,
        &destination,
        &ExtractLimits::default(),
    )
    .unwrap();

    assert_eq!(installed.files_written, packed.files);
    assert_eq!(
        std::fs::read(destination.join("ui/index.html")).unwrap(),
        b"<html>v1</html>"
    );
    // 装完之后可以按摘要核对：`verify_installed` 是宿主复查的入口。
    yi_agent_plugins::verify_installed(&destination).expect("the installation must self-verify");
}

#[test]
fn the_same_input_produces_the_same_bytes() {
    let dir = plugin_dir();
    let first = pack_directory(dir.path()).unwrap();
    let second = pack_directory(dir.path()).unwrap();
    assert_eq!(first.archive, second.archive);
    assert_eq!(first.sha256, second.sha256);
}

#[test]
fn one_changed_byte_fails_verification_and_installation() {
    let dir = plugin_dir();
    let packed = pack_directory(dir.path()).unwrap();
    let key = signing_key(dir.path());
    let envelope = sign(&packed, &key);
    let root = std::fs::read(fixture("root-v1.json")).unwrap();
    let mut store = TrustStore::bootstrap(&root).unwrap();
    let verified = store.verify_release(&envelope, 1_780_000_000, 0).unwrap();
    let manifest =
        Manifest::parse(&std::fs::read(dir.path().join(PACKAGE_MANIFEST)).unwrap()).unwrap();

    let mut tampered = packed.archive.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 0x01;

    let out = tempfile::tempdir().unwrap();
    let error = yi_agent_plugins::install_verified(
        &verified,
        &manifest,
        &tampered,
        &out.path().join("installed"),
        &ExtractLimits::default(),
    )
    .unwrap_err();
    assert!(
        matches!(error, yi_agent_plugins::PluginError::Integrity(_)),
        "got {error:?}"
    );
}

#[test]
fn the_cli_refuses_to_sign_an_identity_that_disagrees_with_the_package() {
    // 签名只能描述包里的字节。允许 `--version` 覆盖就会签出一个"说的"
    // 与"装的"不同的发布，而验证侧无从发现。这条走**真实二进制**：
    // 在测试里重算一次同样的比较证明不了命令行入口也做了这件事。
    let dir = plugin_dir();
    let out = tempfile::tempdir().unwrap();
    let key = signing_key(dir.path());

    let pack = std::process::Command::new(env!("CARGO_BIN_EXE_plugin-pack"))
        .args([
            "pack",
            "--dir",
            dir.path().to_str().unwrap(),
            "--out",
            out.path().to_str().unwrap(),
        ])
        .output()
        .expect("plugin-pack must run");
    assert!(
        pack.status.success(),
        "pack failed: {}",
        String::from_utf8_lossy(&pack.stderr)
    );
    let bundle = PathBuf::from(
        String::from_utf8(pack.stdout)
            .unwrap()
            .lines()
            .next()
            .expect("pack prints the bundle path")
            .trim(),
    );

    let sign = |version: &str| {
        std::process::Command::new(env!("CARGO_BIN_EXE_plugin-pack"))
            .args([
                "sign",
                "--bundle",
                bundle.to_str().unwrap(),
                "--key",
                key.to_str().unwrap(),
                "--out",
                out.path().to_str().unwrap(),
                "--version",
                version,
                "--serial",
                "7",
                "--published-at",
                "1767225600",
                "--expires-at",
                "1798761600",
            ])
            .output()
            .expect("plugin-pack must run")
    };

    let wrong = sign("9.9.9");
    assert!(
        !wrong.status.success(),
        "signing an identity that disagrees with the package must fail"
    );
    assert!(
        String::from_utf8_lossy(&wrong.stderr).contains("must match the package manifest"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&wrong.stderr)
    );

    // 正确的身份仍然可签，否则上一条可能只是"什么都没签成"。
    let right = sign("1.0.0");
    assert!(
        right.status.success(),
        "sign failed: {}",
        String::from_utf8_lossy(&right.stderr)
    );
}
