//! 磁盘上的工具 schema 缓存:`.yi-agent/mcp-cache.json`。
//!
//! 用 `ServerConfig`(命令/参数/环境变量)的 SHA-256 指纹做失效判断:配置变了
//! 就丢弃旧缓存,重新拉取远端工具列表。指纹用摘要而非原文,避免 `env` 里的
//! token/密钥明文落盘。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::ServerConfig;

/// 一个被缓存的远端工具。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedTool {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    pub read_only: bool,
}

/// 单个 server 的缓存条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedServer {
    /// `ServerConfig`(command+args+env)的 SHA-256 十六进制指纹。
    fingerprint: String,
    tools: Vec<CachedTool>,
}

/// 整个缓存文件。
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct McpCache {
    #[serde(default)]
    servers: BTreeMap<String, CachedServer>,
}

/// 给定工作目录下的缓存文件路径。
fn cache_path(workdir: &Path) -> PathBuf {
    workdir.join(".yi-agent").join("mcp-cache.json")
}

/// 计算 `ServerConfig` 的确定性指纹(SHA-256 十六进制摘要)。
///
/// 只包含 command+args+env;`enabled` 是运行时开关,不影响 schema,故意排除
/// 以免切换开关时无谓地让缓存失效。摘要而非原文,避免把 `env` 里的
/// token/密钥明文落到 `.yi-agent/mcp-cache.json`。
fn fingerprint(cfg: &ServerConfig) -> String {
    let key = serde_json::json!({
        "command": cfg.command,
        "args": cfg.args,
        "env": cfg.env,
    });
    // 序列化 `serde_json::Value` 不会失败;显式 panic 好过静默回退成空串
    // (空串会让所有配置都判定为命中,从而一直提供过期 schema)。
    let json = serde_json::to_string(&key).expect("serializing a serde_json::Value is infallible");
    let digest = Sha256::digest(json.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

impl McpCache {
    /// 加载缓存;文件缺失或损坏时返回空缓存。
    ///
    /// 失效只依据配置指纹,不感知远端工具列表的变化:像 `npx -y pkg` /
    /// `@latest` 这类未锁定版本的 spec,即使工具集变了指纹也不变,缓存会被
    /// 一直沿用。需要强制刷新时,删除 `.yi-agent/mcp-cache.json` 即可。
    pub fn load(workdir: &Path) -> Self {
        std::fs::read_to_string(cache_path(workdir))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// 当 `cfg` 的指纹匹配时,返回 `name` 对应的缓存工具。
    pub fn get_for(&self, name: &str, cfg: &ServerConfig) -> Option<&[CachedTool]> {
        let entry = self.servers.get(name)?;
        (entry.fingerprint == fingerprint(cfg)).then_some(entry.tools.as_slice())
    }

    /// 写入/覆盖 `name` 的缓存条目。
    pub fn put(&mut self, name: &str, cfg: &ServerConfig, tools: Vec<CachedTool>) {
        self.servers.insert(
            name.to_string(),
            CachedServer {
                fingerprint: fingerprint(cfg),
                tools,
            },
        );
    }

    /// 持久化缓存,必要时创建父目录;失败时返回错误交由调用方记录。
    pub fn save(&self, workdir: &Path) -> Result<()> {
        let path = cache_path(workdir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir {}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn server(cmd: &str) -> ServerConfig {
        ServerConfig {
            command: cmd.into(),
            args: vec!["--x".into()],
            env: BTreeMap::new(),
            enabled: true,
        }
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = McpCache::default();
        cache.put(
            "fs",
            &server("npx"),
            vec![CachedTool {
                name: "read".into(),
                description: Some("reads".into()),
                input_schema: serde_json::json!({"type":"object"}),
                read_only: true,
            }],
        );
        cache.save(dir.path()).unwrap();

        let loaded = McpCache::load(dir.path());
        let hit = loaded.get_for("fs", &server("npx")).expect("cache hit");
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].name, "read");
        assert_eq!(hit[0].description.as_deref(), Some("reads"));
        assert_eq!(hit[0].input_schema, serde_json::json!({"type":"object"}));
        assert!(hit[0].read_only);
    }

    #[test]
    fn fingerprint_mismatch_invalidates() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = McpCache::default();
        cache.put("fs", &server("npx"), vec![]);
        cache.save(dir.path()).unwrap();
        let loaded = McpCache::load(dir.path());
        assert!(loaded.get_for("fs", &server("uvx")).is_none());
    }

    #[test]
    fn missing_file_yields_empty_cache() {
        let dir = tempfile::tempdir().unwrap();
        let c = McpCache::load(dir.path());
        assert!(c.get_for("fs", &server("npx")).is_none());
    }

    /// 损坏的缓存文件(非法 JSON)不应导致 panic,而是退化为空缓存。
    #[test]
    fn corrupt_file_yields_empty_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join(".yi-agent");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(cache_dir.join("mcp-cache.json"), "{ not json").unwrap();

        let c = McpCache::load(dir.path());
        assert!(c.get_for("fs", &server("npx")).is_none());
    }

    /// `enabled` 是运行时开关,切换它不应让 schema 缓存失效。
    #[test]
    fn toggling_enabled_does_not_invalidate() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = McpCache::default();
        cache.put("fs", &server("npx"), vec![]);
        cache.save(dir.path()).unwrap();

        let loaded = McpCache::load(dir.path());
        let mut disabled = server("npx");
        disabled.enabled = false;
        assert!(loaded.get_for("fs", &disabled).is_some());
    }
}
