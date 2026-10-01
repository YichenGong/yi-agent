use std::path::{Path, PathBuf};

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
    /// Socket this process can be queried on, if it serves one. Generic: the
    /// field names no plugin and the daemon attaches no meaning to the answers.
    /// `None` means "not queryable", so an older manifest keeps loading.
    pub query_socket: Option<String>,
}

const DEFAULT_BACKOFF_MS: u64 = 1000;
const DEFAULT_BACKOFF_MAX_MS: u64 = 30_000;

#[derive(Debug, Deserialize)]
struct RawManifest {
    name: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    switch_key: String,
    restart_backoff_ms: Option<u64>,
    restart_backoff_max_ms: Option<u64>,
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
            query_socket: raw.query_socket,
        })
    }

    /// 展开查询 socket 的占位符。未声明 → `None`。
    pub fn query_socket_path(
        &self,
        workdir: &Path,
        state_dir: &Path,
        runtime_dir: &Path,
    ) -> Option<PathBuf> {
        let raw = self.query_socket.as_ref()?;
        Some(PathBuf::from(
            raw.replace("{workdir}", &workdir.to_string_lossy())
                .replace("{state_dir}", &state_dir.to_string_lossy())
                .replace("{runtime_dir}", &runtime_dir.to_string_lossy()),
        ))
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
            manifest.query_socket_path(
                Path::new("/proj"),
                Path::new("/proj/.yi-agent/state"),
                Path::new("/proj/.yi-agent/runtime"),
            ),
            Some(std::path::PathBuf::from("/proj/.yi-agent/state/demo.sock"))
        );
    }

    #[test]
    fn a_manifest_without_a_query_socket_still_loads() {
        // Old manifests predate the field; loading them must not start failing.
        let manifest = SupervisorManifest::parse(SAMPLE).unwrap();
        assert_eq!(manifest.query_socket, None);
        assert_eq!(
            manifest.query_socket_path(Path::new("/w"), Path::new("/s"), Path::new("/r")),
            None
        );
    }
}
