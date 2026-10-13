//! `.yi-agent/mcp.json` 配置解析(兼容 Claude Desktop 的 schema)。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use tracing::warn;

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

/// 传输类型。缺省为 `Stdio`(向后兼容没有 `type` 的老配置)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerKind {
    #[default]
    Stdio,
    Http,
}

/// `mcpServers` 中的单个 server 配置。
///
/// 单结构体 + `type` 判别(而非 serde tagged enum):对无 `type`、有 `command`
/// 的老配置零破坏,校验集中在 [`ServerConfig::transport`] 一处。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct ServerConfig {
    /// 传输类型:缺省 stdio。
    #[serde(rename = "type", default)]
    pub kind: ServerKind,
    /// stdio:启动 MCP server 的可执行文件。`kind = Stdio` 时必填(校验期)。
    #[serde(default)]
    pub command: Option<String>,
    /// stdio:传给可执行文件的参数。
    #[serde(default)]
    pub args: Vec<String>,
    /// stdio:追加到子进程的环境变量。
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// http:远端 MCP endpoint 的 URL。`kind = Http` 时必填(校验期)。
    #[serde(default)]
    pub url: Option<String>,
    /// http:每个请求附带的 header。值支持 `${ENV_VAR}` 展开。
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// 单个 server 的开关,缺省为 `true`。
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// 校验后的传输形态:把 [`ServerConfig`] 降维成两个明确变体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
}

fn default_true() -> bool {
    true
}

impl ServerConfig {
    /// 校验并降维成 [`Transport`]。缺必填项时返回带原因的 `Err`。
    pub fn transport(&self) -> Result<Transport> {
        match self.kind {
            ServerKind::Stdio => {
                let command = self
                    .command
                    .clone()
                    .filter(|c| !c.is_empty())
                    .ok_or_else(|| anyhow!("stdio MCP server requires a non-empty `command`"))?;
                Ok(Transport::Stdio {
                    command,
                    args: self.args.clone(),
                    env: self.env.clone(),
                })
            }
            ServerKind::Http => {
                let url = self
                    .url
                    .clone()
                    .filter(|u| !u.is_empty())
                    .ok_or_else(|| anyhow!("http MCP server requires a non-empty `url`"))?;
                Ok(Transport::Http {
                    url,
                    headers: self.headers.clone(),
                })
            }
        }
    }
}

/// 展开字符串中的 `${NAME}` 引用。
///
/// 任一所引用的环境变量未定义时返回 `None`(调用方据此丢弃整个 header,避免
/// 发出空值导致下游 401 难排查)。未闭合的 `${` 按字面量保留。
pub(crate) fn expand_env(value: &str) -> Option<String> {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return Some(out);
        };
        let name = &after[..end];
        let val = std::env::var(name).ok()?;
        out.push_str(&val);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// 对一组 header 做 [`expand_env`];未定义变量的 header 被丢弃并 warn。
pub(crate) fn expand_headers(headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (k, v) in headers {
        match expand_env(v) {
            Some(expanded) => {
                out.insert(k.clone(), expanded);
            }
            None => warn!(header = %k, "dropping MCP http header: referenced env var is unset"),
        }
    }
    out
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
        assert_eq!(fs.command.as_deref(), Some("npx"));
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
    fn stdio_config_without_type_still_parses_as_stdio() {
        let raw = r#"{"mcpServers":{"fs":{"command":"npx","args":["-y","pkg"]}}}"#;
        let cfg: McpConfig = serde_json::from_str(raw).unwrap();
        let fs = cfg.mcp_servers.get("fs").unwrap();
        assert_eq!(fs.kind, ServerKind::Stdio);
        assert_eq!(fs.command.as_deref(), Some("npx"));
        match fs.transport().unwrap() {
            Transport::Stdio { command, args, .. } => {
                assert_eq!(command, "npx");
                assert_eq!(args, vec!["-y", "pkg"]);
            }
            other => panic!("expected Stdio, got {other:?}"),
        }
    }

    #[test]
    fn http_config_parses_with_url_and_headers() {
        let raw = r#"{"mcpServers":{"ws":{"type":"http","url":"https://x/mcp","headers":{"Authorization":"Bearer t"}}}}"#;
        let cfg: McpConfig = serde_json::from_str(raw).unwrap();
        let ws = cfg.mcp_servers.get("ws").unwrap();
        assert_eq!(ws.kind, ServerKind::Http);
        match ws.transport().unwrap() {
            Transport::Http { url, headers } => {
                assert_eq!(url, "https://x/mcp");
                assert_eq!(
                    headers.get("Authorization").map(String::as_str),
                    Some("Bearer t")
                );
            }
            other => panic!("expected Http, got {other:?}"),
        }
    }

    #[test]
    fn http_config_without_url_is_rejected_by_transport() {
        let raw = r#"{"mcpServers":{"ws":{"type":"http"}}}"#;
        let cfg: McpConfig = serde_json::from_str(raw).unwrap();
        assert!(cfg.mcp_servers.get("ws").unwrap().transport().is_err());
    }

    #[test]
    fn stdio_config_without_command_is_rejected_by_transport() {
        // 解析期通过(命令改为 Option),校验期报错。
        let raw = r#"{"mcpServers":{"bad":{"args":[]}}}"#;
        let cfg: McpConfig = serde_json::from_str(raw).unwrap();
        assert!(cfg.mcp_servers.get("bad").unwrap().transport().is_err());
    }

    #[test]
    fn expand_env_replaces_defined_and_rejects_undefined() {
        // PATH 在任何环境都存在;用超长随机名保证未定义。
        assert!(expand_env("${PATH}").is_some());
        assert_eq!(expand_env("plain"), Some("plain".into()));
        assert_eq!(expand_env("${DEFINITELY_UNSET_YI_AGENT_VAR_XYZ}"), None);
        assert_eq!(expand_env("pre/${NOPE_XYZ_ABC}"), None);
    }

    #[test]
    fn expand_headers_drops_only_undefined() {
        let mut h = BTreeMap::new();
        h.insert("A".to_string(), "plain".to_string());
        h.insert(
            "B".to_string(),
            "${DEFINITELY_UNSET_YI_AGENT_VAR_XYZ}".to_string(),
        );
        let out = expand_headers(&h);
        assert_eq!(out.get("A").map(String::as_str), Some("plain"));
        assert!(
            out.get("B").is_none(),
            "undefined env header must be dropped"
        );
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
