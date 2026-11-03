//! 测试夹具：直接读 `plugins/contracts/fixtures/` 的同一份 JSON。
//!
//! **每个测试二进制都会编译这一份**，而各自只用到其中一个子集
//! （`manifest.rs` 不碰签名夹具，`trust.rs` 不碰 `mutated`）。因此这里
//! 必须允许未使用项：否则加一条 trust 夹具就会让 manifest 的测试编译失败，
//! 而那不是真的有代码没人用。
//!
//! 不复制一份到本 crate：契约 fixture 是**唯一真相**，复制品会在某次
//! 改动后悄悄分叉，而两份各自的自测都会绿。路径从 `CARGO_MANIFEST_DIR`
//! 上溯到仓库根，这样在任何 cwd 下跑测试都对。

#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// 仓库根。`yi-agent-rs/crates/yi-agent-plugins` → `yi-agent-rs` → `crates` → `<repo>`。
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("crate lives two levels below the repo root")
        .to_path_buf()
}

pub fn fixture_path(name: &str) -> PathBuf {
    repo_root()
        .join("plugins")
        .join("contracts")
        .join("fixtures")
        .join(name)
}

pub fn fixture_bytes(name: &str) -> Vec<u8> {
    std::fs::read(fixture_path(name))
        .unwrap_or_else(|e| panic!("could not read fixture {name}: {e}"))
}

pub fn fixture_json(name: &str) -> serde_json::Value {
    serde_json::from_slice(&fixture_bytes(name))
        .unwrap_or_else(|e| panic!("fixture {name} is not valid JSON: {e}"))
}

/// 把 fixture 改一处后用 `parse` 试一遍。所有负例都走这条路径，
/// 这样「黄金样本本身合法」与「改动的确被拒」是同一次断言的两半。
pub fn mutated(name: &str, change: impl FnOnce(&mut serde_json::Value)) -> Vec<u8> {
    let mut value = fixture_json(name);
    change(&mut value);
    serde_json::to_vec(&value).expect("mutated fixture must still serialize")
}

// ---------------------------------------------------------------------------
// 签名夹具：从契约 fixture 读 envelope，而不是在这里重算一份。
//
// 重算会让「Rust 认为的签名字节」与「Node 参考实现认为的字节」各自自洽地
// 存在，两边自测都绿，直到真实发布才发现不一致。所以这里是**读**，
// 不是造。
// ---------------------------------------------------------------------------

/// 根信封原样字节。
pub fn root_envelope_bytes() -> Vec<u8> {
    fixture_bytes("root-v1.json")
}

/// 发布信封原样字节。
pub fn release_envelope_bytes() -> Vec<u8> {
    fixture_bytes("release-v1.json")
}

/// 固定测试种子生成的密钥对（与 `scripts/plugins/sign-contracts.mjs` 同一种子）。
/// 只是**公开测试身份**，不是生产信任根。
pub const TEST_SEED: u8 = 7;
pub const ROTATED_SEED: u8 = 11;

// ---------------------------------------------------------------------------
// 真实签名的发布夹具。
//
// 攻击样本也必须走**真实验签**：如果让它们因「摘要不符」失败，把整个解包
// 检查删掉测试仍是绿的——那证明不了任何解包边界。这里用测试种子签一个
// 合法信封，再让 `TrustStore` 验一遍，拿到真正的 `VerifiedRelease`。
// ---------------------------------------------------------------------------

use yi_agent_plugins::{Manifest, TrustStore, VerifiedRelease};

/// fixture 里根证书的有效区间内的一个时刻。
const SIGNING_NOW: u64 = 1_780_000_000;

/// 用给定归档内容签发一个「验证通过」的发布，并附带匹配的清单。
///
/// `program_entry` 决定清单是否声明原生入口——归档里没有该文件时，
/// 调用方自己会看到 `Integrity` 错误，这本身也是一条被覆盖的路径。
pub fn signed_release(archive: &[u8], program_entry: Option<&str>) -> (VerifiedRelease, Manifest) {
    use ed25519_dalek::{Signer, SigningKey};

    let digest = sha256_hex(archive);

    // 清单从契约 golden 样本派生，只动 `program`。
    let mut manifest_value = fixture_json("manifest-v1.json");
    match program_entry {
        Some(entry) => {
            manifest_value["program"]["entry"] = entry.into();
        }
        None => {
            manifest_value["program"] = serde_json::Value::Null;
        }
    }
    let manifest_bytes =
        serde_json::to_vec(&manifest_value).expect("derived manifest must serialize");
    let manifest = Manifest::parse(&manifest_bytes).expect("derived manifest must parse");

    let release = serde_json::json!({
        "plugin": "example-counter",
        "version": "1.4.0",
        "target": "darwin-aarch64",
        "sha256": digest,
        "sizeBytes": archive.len(),
        "hostApi": "v1",
        "serial": 7,
        "publishedAt": 1_767_225_600u64,
        "expiresAt": 1_798_761_600u64,
    });
    let payload = encode_release_payload(&release);
    let signing = SigningKey::from_bytes(&[TEST_SEED; 32]);
    let envelope = serde_json::json!({
        "schemaVersion": 1,
        "keyId": "root-2026-01",
        "release": release,
        "payloadB64": b64(&payload),
        "signature": b64(&signing.sign(&payload).to_bytes()),
    });
    let envelope_bytes = serde_json::to_vec(&envelope).expect("envelope must serialize");

    let mut store = TrustStore::bootstrap(&root_envelope_bytes())
        .expect("the shipped root envelope must bootstrap");
    let verified = store
        .verify_release(&envelope_bytes, SIGNING_NOW, 0)
        .expect("the test release must verify");

    (verified, manifest)
}

/// 重新编码的字节必须与 `encode_release_payload` 一致，这里直接复用实现，
/// 避免测试与实现各自维护一份编码规则。
fn encode_release_payload(release: &serde_json::Value) -> Vec<u8> {
    yi_agent_plugins::trust::encode_release_payload(release)
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ---------------------------------------------------------------------------
// 归档构造：用 tar + gzip 现造攻击样本。
//
// 每种攻击都配一个**有效摘要**，这样测试证明的是解包边界本身会拒绝，
// 而不是"摘要不符所以失败了"。后者是另一种假绿：删掉解包检查，
// 测试仍然会因为摘要错误而红。
// ---------------------------------------------------------------------------

use std::io::Write;

/// tar 条目类型。手写是因为 `tar::Builder` 的便捷方法不允许造出
/// 符号链接/设备文件这类**必须被拒绝**的条目。
pub enum EntryKind {
    /// 普通文件，内容为给定字节。
    File(Vec<u8>),
    /// 符号链接，目标任意。
    Symlink(String),
    /// 硬链接，目标任意。
    Hardlink(String),
    /// 字符设备。
    CharDevice,
    /// 目录。
    Dir,
}

pub struct Entry {
    pub name: String,
    pub kind: EntryKind,
    pub mode: u32,
}

/// 造一个 gzip 压缩的 tar 包。
///
/// **手写 header**，不用 `Builder` 的便捷方法：`append_data` 会替我们拒绝
/// 含 `..` 的路径，于是"路径穿越被拒绝"的测试根本造不出攻击样本——
/// 它会因为构造失败而红在错误的地方。要测解包边界，必须能造出恶意归档。
pub fn build_archive(entries: &[Entry]) -> Vec<u8> {
    use flate2::Compression;
    use flate2::write::GzEncoder;

    let mut tar_bytes = Vec::new();
    for entry in entries {
        let header = build_header(entry);
        tar_bytes.extend_from_slice(&header);
        if let EntryKind::File(content) = &entry.kind {
            tar_bytes.extend_from_slice(content);
            // 补齐到 512 字节块。
            let pad = (512 - content.len() % 512) % 512;
            tar_bytes.extend(std::iter::repeat_n(0u8, pad));
        }
    }
    // 两个全零块表示归档结束。
    tar_bytes.extend(std::iter::repeat_n(0u8, 1024));

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&tar_bytes).expect("gzip write");
    encoder.finish().expect("gzip finish")
}

/// 手写一个 tar header。名字直接写进头部的 100 字节区，不做任何校验。
fn build_header(entry: &Entry) -> [u8; 512] {
    let mut header = [0u8; 512];
    let name_bytes = entry.name.as_bytes();
    let n = name_bytes.len().min(100);
    header[..n].copy_from_slice(&name_bytes[..n]);

    // 权限与 uid/gid 都写成 0，测试用的 mode 由解析后的行为决定。
    write_octal(&mut header[100..108], entry.mode as u64);
    write_octal(&mut header[108..116], 0); // uid
    write_octal(&mut header[116..124], 0); // gid

    let (size, typeflag) = match &entry.kind {
        EntryKind::File(content) => (content.len() as u64, b'0'),
        EntryKind::Symlink(_) | EntryKind::Hardlink(_) => (
            0,
            if matches!(entry.kind, EntryKind::Symlink(_)) {
                b'2'
            } else {
                b'1'
            },
        ),
        EntryKind::CharDevice => (0, b'3'),
        EntryKind::Dir => (0, b'5'),
    };
    write_octal(&mut header[124..136], size);
    write_octal(&mut header[136..148], 0); // mtime
    header[156] = typeflag;

    // 链接目标写进 157..257。
    if let EntryKind::Symlink(target) | EntryKind::Hardlink(target) = &entry.kind {
        let bytes = target.as_bytes();
        let n = bytes.len().min(100);
        header[157..157 + n].copy_from_slice(&bytes[..n]);
    }

    // magic + version
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");

    // 校验和：先当成 8 个空格计算，再写入。
    for byte in &mut header[148..156] {
        *byte = b' ';
    }
    let checksum: u32 = header.iter().map(|b| *b as u32).sum();
    write_checksum(&mut header[148..156], checksum);
    header
}

fn write_octal(field: &mut [u8], value: u64) {
    // 形如 "0000644\0"
    let width = field.len() - 1;
    let text = format!("{value:0width$o}");
    let bytes = text.as_bytes();
    let start = field.len() - 1 - bytes.len();
    let last = field.len() - 1;
    for byte in field.iter_mut() {
        *byte = b'0';
    }
    field[start..last].copy_from_slice(bytes);
    field[last] = 0;
}

fn write_checksum(field: &mut [u8], checksum: u32) {
    // 形如 "012345\0 "
    let text = format!("{checksum:06o}");
    field[..6].copy_from_slice(text.as_bytes());
    field[6] = 0;
    field[7] = b' ';
}

pub fn file(name: &str, content: &str) -> Entry {
    Entry {
        name: name.to_string(),
        kind: EntryKind::File(content.as_bytes().to_vec()),
        mode: 0o644,
    }
}

pub fn file_with_mode(name: &str, content: &str, mode: u32) -> Entry {
    Entry {
        name: name.to_string(),
        kind: EntryKind::File(content.as_bytes().to_vec()),
        mode,
    }
}
