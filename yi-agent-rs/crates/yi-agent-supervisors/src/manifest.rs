use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use serde::Deserialize;

/// 清单解析失败的原因。
#[derive(Debug)]
pub enum ManifestError {
    Json(serde_json::Error),
    EmptyName,
    EmptyCommand,
    EmptySwitchKey,
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManifestError::Json(error) => write!(f, "invalid manifest json: {error}"),
            ManifestError::EmptyName => write!(f, "manifest needs a non-empty name"),
            ManifestError::EmptyCommand => write!(f, "manifest needs a non-empty command"),
            ManifestError::EmptySwitchKey => write!(f, "manifest needs a non-empty switch_key"),
        }
    }
}

impl std::error::Error for ManifestError {}

/// 一个被托管进程的声明。字段皆通用，不含任何插件语义。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorManifest {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub switch_key: String,
    pub restart_backoff_ms: u64,
    pub restart_backoff_max_ms: u64,
    /// Whether turning the switch off should stop this process. Defaults to
    /// `true`.
    ///
    /// A process that serves a query channel must set this `false`: stopping it
    /// on switch-off removes the only channel that can report the switch and
    /// turn it back on, so "off" would become a one-way door. Such a process
    /// gates its own work on the switch instead of exiting.
    pub stop_when_disabled: bool,
    /// Socket this process can be queried on, if it serves one. Generic: the
    /// field names no plugin and the daemon attaches no meaning to the answers.
    /// `None` means "not queryable", so an older manifest keeps loading.
    pub query_socket: Option<String>,
}

const DEFAULT_BACKOFF_MS: u64 = 1000;
const DEFAULT_BACKOFF_MAX_MS: u64 = 30_000;

/// A manifest that predates `stop_when_disabled` keeps the historical meaning:
/// off means stopped.
fn default_stop_when_disabled() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct RawManifest {
    name: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    switch_key: String,
    restart_backoff_ms: Option<u64>,
    restart_backoff_max_ms: Option<u64>,
    #[serde(default = "default_stop_when_disabled")]
    stop_when_disabled: bool,
    query_socket: Option<String>,
}

impl SupervisorManifest {
    pub fn parse(json: &str) -> Result<Self, ManifestError> {
        let raw: RawManifest = serde_json::from_str(json).map_err(ManifestError::Json)?;
        if raw.name.is_empty() {
            return Err(ManifestError::EmptyName);
        }
        if raw.command.is_empty() {
            return Err(ManifestError::EmptyCommand);
        }
        if raw.switch_key.is_empty() {
            return Err(ManifestError::EmptySwitchKey);
        }
        Ok(Self {
            name: raw.name,
            command: raw.command,
            args: raw.args,
            switch_key: raw.switch_key,
            restart_backoff_ms: raw.restart_backoff_ms.unwrap_or(DEFAULT_BACKOFF_MS),
            restart_backoff_max_ms: raw.restart_backoff_max_ms.unwrap_or(DEFAULT_BACKOFF_MAX_MS),
            stop_when_disabled: raw.stop_when_disabled,
            query_socket: raw.query_socket,
        })
    }

    /// 展开查询 socket 的占位符。未声明 → `None`。
    /// 展开查询 socket 的占位符，并按**插件通道**的规则做长度回退。未声明 → `None`。
    ///
    /// 深路径下直接拼接会超过 `sun_path` 上限，插件根本 bind 不上，而转发表若仍
    /// 指向那条长路径，daemon 就会一直答「插件不可用」。规则必须与插件侧
    /// `superpowers-kanban-ipc::socket::plugin_socket_for` 完全一致：哈希**展开后的
    /// 直接路径**（不是目录），前缀 `plugin-`。两边拿到的是同一个字符串，因此对得上。
    pub fn query_socket_path(
        &self,
        workdir: &Path,
        state_dir: &Path,
        runtime_dir: &Path,
    ) -> Option<Result<PathBuf, SocketPathTooLong>> {
        let raw = self.query_socket.as_ref()?;
        let expanded = PathBuf::from(
            raw.replace("{workdir}", &workdir.to_string_lossy())
                .replace("{state_dir}", &state_dir.to_string_lossy())
                .replace("{runtime_dir}", &runtime_dir.to_string_lossy()),
        );
        Some(resolve_plugin_socket(&expanded))
    }

    /// 展开 `{workdir}` / `{state_dir}` / `{runtime_dir}` 占位。
    pub fn expand_args(&self, workdir: &Path, state_dir: &Path, runtime_dir: &Path) -> Vec<String> {
        let expand = |arg: &str| {
            arg.replace("{workdir}", &workdir.to_string_lossy())
                .replace("{state_dir}", &state_dir.to_string_lossy())
                .replace("{runtime_dir}", &runtime_dir.to_string_lossy())
        };
        self.args.iter().map(|arg| expand(arg)).collect()
    }
}

/// 查询 socket 回退后仍超长。
///
/// 与插件侧 `superpowers-kanban-ipc::socket::SocketPathError` 是同一件事的两份
/// 实现——插件零 `yi-agent-*` 依赖，规则只能各自手写，靠跨端一致性测试锁住。
#[derive(Debug)]
pub struct SocketPathTooLong {
    pub path: String,
    pub limit: usize,
}

impl std::fmt::Display for SocketPathTooLong {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "query socket path is {} bytes after fallback, over the {}-byte limit: {}",
            self.path.len(),
            self.limit,
            self.path
        )
    }
}

impl std::error::Error for SocketPathTooLong {}

/// `sun_path` 上限（不含结尾 NUL）。与插件侧同值。
pub const MAX_SOCKET_PATH_BYTES: usize = 103;

/// 插件通道的 socket 规则：直通 → `$TMPDIR/plugin-<sha256(直接路径)[..16]>.sock` → 报错。
///
/// **不要**把它和 `yi-agent-store::ipc::socket_path_for` 合并：后者是 daemon 自己的
/// socket，前缀 `yi-agent-`、哈希 `runtime_dir`，用途不同。
fn resolve_plugin_socket(direct: &Path) -> Result<PathBuf, SocketPathTooLong> {
    if direct.as_os_str().len() <= MAX_SOCKET_PATH_BYTES {
        return Ok(direct.to_path_buf());
    }

    let digest = Sha256::digest(direct.as_os_str().as_encoded_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let fallback = std::env::temp_dir().join(format!("plugin-{}.sock", &hex[..16]));

    if fallback.as_os_str().len() > MAX_SOCKET_PATH_BYTES {
        return Err(SocketPathTooLong {
            path: fallback.display().to_string(),
            limit: MAX_SOCKET_PATH_BYTES,
        });
    }
    Ok(fallback)
}

/// 读清单目录：目录缺失→空；单个文件坏→跳过并记 warning（绝不 panic）。
/// 结果按 `name` 排序，保证调用方看到稳定的顺序。
pub fn load_manifests(dir: &Path) -> Vec<SupervisorManifest> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut manifests = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => match SupervisorManifest::parse(&text) {
                Ok(manifest) => manifests.push(manifest),
                Err(error) => tracing::warn!(
                    path = %path.display(),
                    %error,
                    "skipping malformed supervisor manifest"
                ),
            },
            Err(error) => tracing::warn!(
                path = %path.display(),
                %error,
                "could not read supervisor manifest"
            ),
        }
    }
    manifests.sort_by(|left, right| left.name.cmp(&right.name));
    manifests
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const SAMPLE: &str = r#"{
        "name": "demo",
        "command": "demo-bin",
        "args": ["--state", "{state_dir}", "--root", "{workdir}", "--rt", "{runtime_dir}"],
        "switch_key": "demo_on",
        "restart_backoff_ms": 500,
        "restart_backoff_max_ms": 4000
    }"#;

    #[test]
    fn a_manifest_parses_with_all_fields() {
        let manifest = SupervisorManifest::parse(SAMPLE).unwrap();
        assert_eq!(manifest.name, "demo");
        assert_eq!(manifest.command, "demo-bin");
        assert_eq!(manifest.switch_key, "demo_on");
        assert_eq!(manifest.restart_backoff_ms, 500);
        assert_eq!(manifest.restart_backoff_max_ms, 4000);
        // 缺省保持历史语义：开关关闭即停止。只有显式声明才改。
        assert!(
            manifest.stop_when_disabled,
            "没有该字段的旧清单必须仍按「关即停」处理"
        );
    }

    #[test]
    fn a_manifest_can_declare_that_it_survives_being_disabled() {
        // 声明查询通道的进程要能在开关关闭时继续运行，否则没人能把它打开。
        let json = r#"{"name":"m","command":"c","args":[],"switch_key":"k",
            "stop_when_disabled": false}"#;
        let manifest = SupervisorManifest::parse(json).unwrap();
        assert!(!manifest.stop_when_disabled);
    }

    #[test]
    fn placeholders_expand_to_the_given_directories() {
        let manifest = SupervisorManifest::parse(SAMPLE).unwrap();
        let args = manifest.expand_args(
            Path::new("/proj"),
            Path::new("/proj/.yi-agent/board"),
            Path::new("/proj/.yi-agent/runtime"),
        );
        assert_eq!(
            args,
            vec![
                "--state",
                "/proj/.yi-agent/board",
                "--root",
                "/proj",
                "--rt",
                "/proj/.yi-agent/runtime",
            ]
        );
    }

    #[test]
    fn defaults_apply_when_optional_fields_are_absent() {
        let json = r#"{"name":"m","command":"c","args":[],"switch_key":"k"}"#;
        let manifest = SupervisorManifest::parse(json).unwrap();
        assert_eq!(manifest.restart_backoff_ms, 1000);
        assert_eq!(manifest.restart_backoff_max_ms, 30000);
    }

    #[test]
    fn a_manifest_missing_a_required_field_is_rejected() {
        let json = r#"{"name":"m","args":[],"switch_key":"k"}"#;
        assert!(SupervisorManifest::parse(json).is_err());
    }

    #[test]
    fn load_manifests_skips_broken_files_and_sorts_by_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.json"), SAMPLE.replace("demo", "b")).unwrap();
        std::fs::write(dir.path().join("a.json"), SAMPLE.replace("demo", "a")).unwrap();
        std::fs::write(dir.path().join("broken.json"), "{ not json").unwrap();
        let names: Vec<String> = load_manifests(dir.path())
            .into_iter()
            .map(|m| m.name)
            .collect();
        assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn a_missing_directory_yields_no_manifests() {
        assert!(load_manifests(Path::new("/definitely/not/here")).is_empty());
    }

    #[test]
    fn a_declared_query_socket_expands_its_placeholders() {
        let manifest = SupervisorManifest::parse(
            r#"{"name":"demo","command":"c","args":[],"switch_key":"k",
                "query_socket":"{state_dir}/demo.sock"}"#,
        )
        .unwrap();
        assert_eq!(
            manifest
                .query_socket_path(
                    Path::new("/proj"),
                    Path::new("/proj/.yi-agent/state"),
                    Path::new("/proj/.yi-agent/runtime"),
                )
                .expect("declared")
                .expect("short path stays"),
            std::path::PathBuf::from("/proj/.yi-agent/state/demo.sock")
        );
    }

    /// 读宿主与插件共用的 socket 契约（**与插件 `socket.rs` 是同一份文件**）。
    ///
    /// 运行时读取而非 `include_str!`：插件是可独立安装/卸载的，宿主单独 checkout
    /// 时这个目录可能不存在，编译期依赖会让宿主构建直接失败。这里文件缺失就
    /// 跳过——本测试锁的是「monorepo 内两侧一致」，不是宿主的构建前提。
    fn contract() -> Option<String> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../plugins/superpowers-kanban/crates/superpowers-kanban-ipc/contract/plugin-socket.json");
        std::fs::read_to_string(path).ok()
    }

    fn contract_str(contract: &str, key: &str) -> String {
        let needle = format!("\"{key}\": \"");
        let start = contract.find(&needle).expect("contract key") + needle.len();
        contract[start..].split('"').next().unwrap().to_string()
    }

    fn contract_usize(contract: &str, key: &str) -> usize {
        let needle = format!("\"{key}\": ");
        let start = contract.find(&needle).expect("contract key") + needle.len();
        contract[start..]
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }

    #[test]
    fn the_query_socket_matches_the_shared_contract() {
        let Some(contract) = contract() else {
            return; // 宿主单独 checkout：没有插件目录，无从对照。
        };
        // 契约里的模板与目录，还原出插件会去 bind 的直接路径。
        let template = contract_str(&contract, "query_socket_template");
        let state_dir = contract_str(&contract, "state_dir");
        let expanded = template.replace("{state_dir}", &state_dir);
        assert_eq!(
            expanded,
            contract_str(&contract, "expected_direct_path"),
            "契约自相矛盾：模板展开后应等于 expected_direct_path"
        );

        let manifest = SupervisorManifest::parse(&format!(
            r#"{{"name":"superpowers-kanban","command":"c","args":[],"switch_key":"k",
                "query_socket":"{template}"}}"#
        ))
        .unwrap();

        let socket = manifest
            .query_socket_path(
                Path::new("/proj"),
                Path::new(&state_dir),
                Path::new("/proj/.yi-agent/runtime"),
            )
            .expect("declared")
            .expect("falls back");

        assert!(socket.as_os_str().len() <= contract_usize(&contract, "max_socket_path_bytes"));
        assert_eq!(socket.parent().unwrap(), std::env::temp_dir());
        assert_eq!(
            socket.file_name().unwrap().to_string_lossy(),
            contract_str(&contract, "expected_fallback_file_name"),
            "宿主转发表与插件将用不同的名字：这条路修了等于没修"
        );
    }

    #[test]
    fn a_manifest_without_a_query_socket_still_loads() {
        // Old manifests predate the field; loading them must not start failing.
        let manifest = SupervisorManifest::parse(SAMPLE).unwrap();
        assert_eq!(manifest.query_socket, None);
        assert!(
            manifest
                .query_socket_path(Path::new("/w"), Path::new("/s"), Path::new("/r"))
                .is_none()
        );
    }
}
