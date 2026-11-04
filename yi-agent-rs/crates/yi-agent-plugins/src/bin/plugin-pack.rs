//! `plugin-pack`：把插件目录打成确定性发布包，并签出元数据。
//!
//! 用法（私钥只从文件读，永不进 argv 之外的地方）：
//!
//! ```text
//! plugin-pack pack --dir plugins/superpowers-kanban --out dist/plugins
//! plugin-pack sign --bundle dist/plugins/superpowers-kanban.tar.gz \
//!     --key ~/.yi-agent-keys/plugin-release.json --out dist/plugins \
//!     --serial 7 --published-at 1767225600 --expires-at 1798761600
//! ```
//!
//! 拆成两个动词而不是一条命令，是因为"打包"与"签名"发生在不同的信任边界
//! 两侧：打包可以在任何开发机上做，签名只在持有私钥的发布环境里做。

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::Value;
use yi_agent_plugins::trust::encode_release_payload;

use yi_agent_plugins::pack::pack_directory;

fn usage() -> &'static str {
    "usage:\n\
     \x20 plugin-pack pack --dir <plugin-dir> --out <output-dir>\n\
     \x20 plugin-pack sign --bundle <archive> --key <signing-key.json> --out <dir>\n\
     \x20                --serial <n> --published-at <unix> --expires-at <unix>\n\
     \x20                [--host-api v1] [--plugin <id>] [--version <v>] [--target <t>]"
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first() else {
        return Err(usage().to_string());
    };
    let flags = Flags::parse(&args[1..])?;
    match command.as_str() {
        "pack" => pack(&flags),
        "sign" => sign(&flags),
        "--help" | "-h" | "help" => {
            println!("{}", usage());
            Ok(())
        }
        other => Err(format!("unknown command `{other}`\n{}", usage())),
    }
}

fn pack(flags: &Flags) -> Result<(), String> {
    let dir = flags.required("--dir")?;
    let out = flags.required("--out")?;
    let packed = pack_directory(std::path::Path::new(&dir)).map_err(|e| e.to_string())?;
    let stem = format!("{}-{}-{}", packed.plugin, packed.version, packed.target);
    let bundle = yi_agent_plugins::pack::write_release(&packed, std::path::Path::new(&out), &stem)
        .map_err(|e| e.to_string())?;
    println!("{}", bundle.display());
    println!(
        "{} files, {} bytes, sha256 {}",
        packed.files,
        packed.archive.len(),
        packed.sha256
    );
    Ok(())
}

fn sign(flags: &Flags) -> Result<(), String> {
    use sha2::{Digest, Sha256};

    let bundle = flags.required("--bundle")?;
    let key = flags.required("--key")?;
    let out = flags.required("--out")?;
    let serial: u64 = flags
        .required("--serial")?
        .parse()
        .map_err(|e| format!("--serial must be an unsigned integer: {e}"))?;
    let published_at: u64 = flags
        .required("--published-at")?
        .parse()
        .map_err(|e| format!("--published-at must be a unix timestamp: {e}"))?;
    let expires_at: u64 = flags
        .required("--expires-at")?
        .parse()
        .map_err(|e| format!("--expires-at must be a unix timestamp: {e}"))?;

    let archive = std::fs::read(&bundle).map_err(|e| format!("could not read {bundle}: {e}"))?;

    // 身份字段先从包里读出来，再允许 CLI 覆盖：默认值必须来自包本身，
    // 否则一次手滑的 `--version` 就能签出一个与内容不符的发布。
    let manifest_bytes = yi_agent_plugins::bundle::read_package_file(
        &archive,
        yi_agent_plugins::pack::PACKAGE_MANIFEST,
        1024 * 1024,
    )
    .map_err(|e| e.to_string())?;
    let manifest = yi_agent_plugins::Manifest::parse(&manifest_bytes).map_err(|e| e.to_string())?;

    let fields = ReleaseFields {
        plugin: flags
            .value("--plugin")
            .unwrap_or_else(|| manifest.plugin.as_str().to_string()),
        version: flags
            .value("--version")
            .unwrap_or_else(|| manifest.version.clone()),
        target: flags
            .value("--target")
            .unwrap_or_else(|| manifest.target.clone()),
        sha256: hex(Sha256::digest(&archive)),
        size_bytes: archive.len() as u64,
        host_api: flags
            .value("--host-api")
            .unwrap_or_else(|| "v1".to_string()),
        serial,
        published_at,
        expires_at,
    };
    if fields.plugin != manifest.plugin.as_str()
        || fields.version != manifest.version
        || fields.target != manifest.target
    {
        return Err("the signed identity must match the package manifest; \
             refusing to sign a release whose metadata describes different bytes"
            .to_string());
    }

    let envelope = sign_release(&fields, std::path::Path::new(&key)).map_err(|e| e.to_string())?;
    let stem = format!("{}-{}-{}", fields.plugin, fields.version, fields.target);
    std::fs::create_dir_all(&out).map_err(|e| format!("could not create {out}: {e}"))?;
    let path = PathBuf::from(&out).join(format!("{stem}.json"));
    std::fs::write(&path, &envelope)
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    println!("{}", path.display());
    println!("sha256 {}", fields.sha256);
    Ok(())
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// `--name value` 风格的极简解析。
///
/// 不引 clap：这个工具只跑在发布环境，参数少且固定；多一个依赖就多一份
/// 要审计的代码，而它已经能读到签名私钥了。
struct Flags {
    pairs: Vec<(String, String)>,
}

impl Flags {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut pairs = Vec::new();
        let mut iter = args.iter();
        while let Some(flag) = iter.next() {
            if !flag.starts_with("--") {
                return Err(format!("unexpected argument `{flag}`\n{}", usage()));
            }
            let value = iter
                .next()
                .ok_or_else(|| format!("`{flag}` needs a value"))?;
            pairs.push((flag.clone(), value.clone()));
        }
        Ok(Self { pairs })
    }

    fn value(&self, name: &str) -> Option<String> {
        self.pairs
            .iter()
            .rev()
            .find(|(flag, _)| flag == name)
            .map(|(_, value)| value.clone())
    }

    fn required(&self, name: &str) -> Result<String, String> {
        self.value(name)
            .ok_or_else(|| format!("`{name}` is required\n{}", usage()))
    }
}

/// 签名要写进发布信封的全部字段。
///
/// 放在**发布工具**里而不是库里：宿主只需要验证，永远不需要签发。把签名
/// 能力留在 `yi-agent-plugins` 会让主程序二进制携带"能签发可安装插件"的
/// 代码，而那条路用户永远不该走到。
pub struct ReleaseFields {
    pub plugin: String,
    pub version: String,
    pub target: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub host_api: String,
    pub serial: u64,
    pub published_at: u64,
    pub expires_at: u64,
}

/// 用私钥签出一个发布信封（与宿主 `TrustStore::verify_release` 同格式）。
///
/// `key_file` 是一份 JSON：`{"keyId": "...", "privateKey": "<base64 32 字节种子>"}`
/// ——与 `plugins/contracts/fixtures/` 的测试身份同构，生产身份只是种子不同。
fn sign_release(fields: &ReleaseFields, key_file: &Path) -> Result<Vec<u8>, String> {
    let raw = std::fs::read(key_file)
        .map_err(|e| format!("could not read {}: {e}", key_file.display()))?;
    let key: Value =
        serde_json::from_slice(&raw).map_err(|e| format!("signing key is not JSON: {e}"))?;
    let key_id = key
        .get("keyId")
        .and_then(Value::as_str)
        .ok_or_else(|| "the signing key file has no `keyId`".to_string())?;
    let seed_b64 = key
        .get("privateKey")
        .and_then(Value::as_str)
        .ok_or_else(|| "the signing key file has no `privateKey`".to_string())?;
    let seed = base64::engine::general_purpose::STANDARD
        .decode(seed_b64)
        .map_err(|e| format!("`privateKey` is not base64: {e}"))?;
    let seed: [u8; 32] = seed
        .try_into()
        .map_err(|_| "`privateKey` must be exactly 32 bytes".to_string())?;
    let signing = SigningKey::from_bytes(&seed);

    let release = serde_json::json!({
        "plugin": fields.plugin,
        "version": fields.version,
        "target": fields.target,
        "sha256": fields.sha256,
        "sizeBytes": fields.size_bytes,
        "hostApi": fields.host_api,
        "serial": fields.serial,
        "publishedAt": fields.published_at,
        "expiresAt": fields.expires_at,
    });
    let payload = encode_release_payload(&release);
    let envelope = serde_json::json!({
        "schemaVersion": 1,
        "keyId": key_id,
        "release": release,
        "payloadB64": base64::engine::general_purpose::STANDARD.encode(&payload),
        "signature": base64::engine::general_purpose::STANDARD.encode(signing.sign(&payload).to_bytes()),
    });
    serde_json::to_vec_pretty(&envelope).map_err(|e| format!("envelope does not serialize: {e}"))
}
