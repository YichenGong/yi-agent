//! `.yi-agent/mcp.json` 配置解析(兼容 Claude Desktop 的 schema)。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// `.yi-agent/mcp.json` 的根结构。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct McpConfig {
    /// 总开关,缺省为 `true`。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 各 MCP server 的配置,键为 server 名。
    #[serde(rename = "mcpServers", default)]
    pub mcp_servers: BTreeMap<String, ServerConfig>,
}

/// `mcpServers` 中的单个 server 配置。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ServerConfig {
    /// 启动 MCP server 的可执行文件。
    pub command: String,
    /// 传给可执行文件的参数。
    #[serde(default)]
    pub args: Vec<String>,
    /// 追加到子进程的环境变量。
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// 单个 server 的开关,缺省为 `true`。
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// 给定工作目录下的配置文件路径。
pub fn config_path(workdir: &Path) -> PathBuf {
    workdir.join(".yi-agent").join("mcp.json")
}

impl McpConfig {
    /// 加载 `workdir` 下的 `.yi-agent/mcp.json`。
    ///
    /// 文件不存在时返回 `Ok(None)`(即未配置 MCP)。
    pub fn load(workdir: &Path) -> Result<Option<Self>> {
        let path = config_path(workdir);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        let cfg =
            serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        Ok(Some(cfg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_config_with_defaults() {
        let raw = r#"{"mcpServers":{"fs":{"command":"npx","args":["-y","pkg"]}}}"#;
        let cfg: McpConfig = serde_json::from_str(raw).unwrap();
        assert!(cfg.enabled, "top-level enabled defaults to true");
        let fs = cfg.mcp_servers.get("fs").unwrap();
        assert_eq!(fs.command, "npx");
        assert_eq!(fs.args, vec!["-y", "pkg"]);
        assert!(fs.enabled, "per-server enabled defaults to true");
        assert!(fs.env.is_empty());
    }

    #[test]
    fn honors_explicit_false_flags() {
        let raw = r#"{"enabled":false,"mcpServers":{"git":{"command":"uvx","enabled":false}}}"#;
        let cfg: McpConfig = serde_json::from_str(raw).unwrap();
        assert!(!cfg.enabled);
        assert!(!cfg.mcp_servers.get("git").unwrap().enabled);
    }

    #[test]
    fn rejects_missing_command() {
        let raw = r#"{"mcpServers":{"bad":{"args":[]}}}"#;
        assert!(serde_json::from_str::<McpConfig>(raw).is_err());
    }

    #[test]
    fn load_returns_none_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(McpConfig::load(dir.path()).unwrap().is_none());
    }

    #[test]
    fn load_reads_file_from_dot_yi_agent() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join(".yi-agent");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("mcp.json"),
            r#"{"mcpServers":{"fs":{"command":"npx"}}}"#,
        )
        .unwrap();
        let cfg = McpConfig::load(dir.path()).unwrap().expect("some");
        assert!(cfg.mcp_servers.contains_key("fs"));
    }

    #[test]
    fn load_errors_on_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join(".yi-agent");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("mcp.json"), "{ not json").unwrap();
        assert!(McpConfig::load(dir.path()).is_err());
    }
}
