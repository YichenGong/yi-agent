use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct PermissionsConfig {
    #[serde(default)]
    pub tool_level: ToolLevelConfig,
    #[serde(default)]
    pub prefix_level: PrefixLevelConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct ToolLevelConfig {
    #[serde(default)]
    pub bash: bool,
    #[serde(default)]
    pub write: bool,
    #[serde(default)]
    pub edit: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct PrefixLevelConfig {
    #[serde(default)]
    pub bash: BashPrefixConfig,
    #[serde(default)]
    pub write: PathPrefixConfig,
    #[serde(default)]
    pub edit: PathPrefixConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct BashPrefixConfig {
    #[serde(default)]
    pub prefixes: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct PathPrefixConfig {
    #[serde(default)]
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionKind {
    Normal,
    Blacklisted(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    AllowOnce,
    AlwaysAllowTool,
    AlwaysAllowPrefix(String),
    Deny,
}

#[derive(Debug, Clone)]
pub struct PermissionRequest {
    pub request_id: u64,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub prefix_suggestion: Option<String>,
    pub kind: PermissionKind,
}

#[derive(Debug, Clone)]
pub enum CheckResult {
    Allow,
    NeedConfirm(PermissionRequest),
    Blacklisted(PermissionRequest),
    Deny,
}

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub type BlocklistFn = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

pub struct PermissionChecker {
    config: Mutex<PermissionsConfig>,
    yolo: bool,
    workdir: std::path::PathBuf,
    blocklist_fn: BlocklistFn,
    next_request_id: AtomicU64,
}

impl PermissionChecker {
    pub fn new(
        config: PermissionsConfig,
        yolo: bool,
        workdir: std::path::PathBuf,
        blocklist_fn: BlocklistFn,
    ) -> Self {
        Self {
            config: Mutex::new(config),
            yolo,
            workdir,
            blocklist_fn,
            next_request_id: AtomicU64::new(1),
        }
    }

    pub fn check(&self, tool_name: &str, tool_input: &serde_json::Value) -> CheckResult {
        // 只对 bash/write/edit 做权限检查,其他工具直接放行
        if !matches!(tool_name, "bash" | "write" | "edit") {
            return CheckResult::Allow;
        }

        // yolo 模式:工具类型层视为全开,但黑名单仍检查
        if self.yolo {
            return self.check_blacklist_then_allow(tool_name, tool_input);
        }

        let config = self.config.lock().unwrap();
        // 第一层:工具类型
        if Self::tool_level_allows(&config, tool_name) {
            return self.check_blacklist_then_allow(tool_name, tool_input);
        }
        // 第二层:前缀/路径
        if Self::prefix_level_allows(&config, tool_name, tool_input, &self.workdir) {
            return self.check_blacklist_then_allow(tool_name, tool_input);
        }
        // 未命中白名单,需要确认
        let request = self.build_request(tool_name, tool_input, PermissionKind::Normal);
        CheckResult::NeedConfirm(request)
    }

    fn check_blacklist_then_allow(
        &self,
        tool_name: &str,
        tool_input: &serde_json::Value,
    ) -> CheckResult {
        // 黑名单只对 bash 检查
        if tool_name == "bash" {
            if let Some(cmd) = tool_input.get("command").and_then(|v| v.as_str()) {
                if let Some(reason) = (self.blocklist_fn)(cmd) {
                    let request = self.build_request(
                        tool_name,
                        tool_input,
                        PermissionKind::Blacklisted(reason),
                    );
                    return CheckResult::Blacklisted(request);
                }
            }
        }
        CheckResult::Allow
    }

    fn tool_level_allows(config: &PermissionsConfig, tool_name: &str) -> bool {
        match tool_name {
            "bash" => config.tool_level.bash,
            "write" => config.tool_level.write,
            "edit" => config.tool_level.edit,
            _ => false,
        }
    }

    fn prefix_level_allows(
        config: &PermissionsConfig,
        tool_name: &str,
        tool_input: &serde_json::Value,
        workdir: &Path,
    ) -> bool {
        match tool_name {
            "bash" => {
                let Some(cmd) = tool_input.get("command").and_then(|v| v.as_str()) else {
                    return false;
                };
                config.prefix_level.bash.prefixes.iter().any(|p| cmd.starts_with(p))
            }
            "write" | "edit" => {
                let Some(path) = tool_input.get("path").and_then(|v| v.as_str()) else {
                    return false;
                };
                let abs = if Path::new(path).is_absolute() {
                    Path::new(path).to_path_buf()
                } else {
                    workdir.join(path)
                };
                let rel = abs.strip_prefix(workdir).unwrap_or(&abs);
                let rel_str = rel.to_string_lossy();
                let patterns = match tool_name {
                    "write" => &config.prefix_level.write.paths,
                    "edit" => &config.prefix_level.edit.paths,
                    _ => return false,
                };
                patterns.iter().any(|pattern| glob_match(pattern, &rel_str))
            }
            _ => false,
        }
    }

    fn build_request(
        &self,
        tool_name: &str,
        tool_input: &serde_json::Value,
        kind: PermissionKind,
    ) -> PermissionRequest {
        PermissionRequest {
            request_id: self.next_request_id.fetch_add(1, Ordering::SeqCst),
            tool_name: tool_name.to_string(),
            tool_input: tool_input.clone(),
            prefix_suggestion: None, // Task 6 填充
            kind,
        }
    }
}

fn glob_match(pattern: &str, path: &str) -> bool {
    glob::Pattern::new(pattern)
        .map(|p| p.matches(path))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_all_false_and_empty() {
        let config = PermissionsConfig::default();
        assert!(!config.tool_level.bash);
        assert!(!config.tool_level.write);
        assert!(!config.tool_level.edit);
        assert!(config.prefix_level.bash.prefixes.is_empty());
        assert!(config.prefix_level.write.paths.is_empty());
        assert!(config.prefix_level.edit.paths.is_empty());
    }

    #[test]
    fn serialize_roundtrip_full_config() {
        let config = PermissionsConfig {
            tool_level: ToolLevelConfig {
                bash: true,
                write: false,
                edit: true,
            },
            prefix_level: PrefixLevelConfig {
                bash: BashPrefixConfig {
                    prefixes: vec!["git push".to_string(), "cargo run".to_string()],
                },
                write: PathPrefixConfig {
                    paths: vec!["src/**".to_string()],
                },
                edit: PathPrefixConfig::default(),
            },
        };
        let toml_str = toml::to_string(&config).unwrap();
        let parsed: PermissionsConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(config, parsed);
    }

    #[test]
    fn deserialize_empty_string_gives_default() {
        let parsed: PermissionsConfig = toml::from_str("").unwrap();
        assert_eq!(parsed, PermissionsConfig::default());
    }

    #[test]
    fn deserialize_partial_config_only_tool_level() {
        let toml_str = r#"
[tool_level]
bash = true
"#;
        let parsed: PermissionsConfig = toml::from_str(toml_str).unwrap();
        assert!(parsed.tool_level.bash);
        assert!(!parsed.tool_level.write);
        assert!(parsed.prefix_level.bash.prefixes.is_empty());
    }

    #[test]
    fn decision_variants_construct() {
        assert_eq!(Decision::AllowOnce, Decision::AllowOnce);
        assert_eq!(
            Decision::AlwaysAllowPrefix("git push".to_string()),
            Decision::AlwaysAllowPrefix("git push".to_string())
        );
    }

    #[test]
    fn permission_kind_blacklisted_carries_reason() {
        let kind = PermissionKind::Blacklisted("rm -rf /".to_string());
        match kind {
            PermissionKind::Blacklisted(r) => assert_eq!(r, "rm -rf /"),
            _ => panic!("expected Blacklisted"),
        }
    }

    use crate::permission::{
        BlocklistFn, CheckResult, Decision, PermissionChecker, PermissionKind,
    };
    use std::sync::Arc;

    fn checker_with(config: PermissionsConfig, yolo: bool) -> PermissionChecker {
        let blocklist: BlocklistFn = Arc::new(|cmd: &str| {
            if cmd.contains("rm -rf /") {
                Some("rm -rf /".to_string())
            } else {
                None
            }
        });
        PermissionChecker::new(config, yolo, std::path::PathBuf::from("/tmp"), blocklist)
    }

    fn bash_input(cmd: &str) -> serde_json::Value {
        serde_json::json!({"command": cmd})
    }

    #[test]
    fn check_tool_level_bash_allow() {
        let config = PermissionsConfig {
            tool_level: ToolLevelConfig {
                bash: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let checker = checker_with(config, false);
        assert!(matches!(
            checker.check("bash", &bash_input("ls")),
            CheckResult::Allow
        ));
    }

    #[test]
    fn check_prefix_level_bash_allow() {
        let config = PermissionsConfig {
            prefix_level: PrefixLevelConfig {
                bash: BashPrefixConfig {
                    prefixes: vec!["git push".to_string()],
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let checker = checker_with(config, false);
        assert!(matches!(
            checker.check("bash", &bash_input("git push origin main")),
            CheckResult::Allow
        ));
        assert!(matches!(
            checker.check("bash", &bash_input("git status")),
            CheckResult::NeedConfirm(_)
        ));
    }

    #[test]
    fn check_no_whitelist_yolo_allow() {
        let checker = checker_with(PermissionsConfig::default(), true);
        assert!(matches!(
            checker.check("bash", &bash_input("ls")),
            CheckResult::Allow
        ));
    }

    #[test]
    fn check_no_whitelist_no_yolo_need_confirm() {
        let checker = checker_with(PermissionsConfig::default(), false);
        assert!(matches!(
            checker.check("bash", &bash_input("ls")),
            CheckResult::NeedConfirm(_)
        ));
    }

    #[test]
    fn check_blacklist_overrides_whitelist() {
        let config = PermissionsConfig {
            tool_level: ToolLevelConfig {
                bash: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let checker = checker_with(config, false);
        assert!(matches!(
            checker.check("bash", &bash_input("rm -rf /")),
            CheckResult::Blacklisted(_)
        ));
    }

    #[test]
    fn check_blacklist_overrides_yolo() {
        let checker = checker_with(PermissionsConfig::default(), true);
        assert!(matches!(
            checker.check("bash", &bash_input("rm -rf /")),
            CheckResult::Blacklisted(_)
        ));
    }

    #[test]
    fn check_write_path_glob_allow() {
        let config = PermissionsConfig {
            prefix_level: PrefixLevelConfig {
                write: PathPrefixConfig {
                    paths: vec!["src/**".to_string()],
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let checker = checker_with(config, false);
        let input = serde_json::json!({"path": "src/main.rs", "content": "x"});
        assert!(matches!(
            checker.check("write", &input),
            CheckResult::Allow
        ));
        let input2 = serde_json::json!({"path": "tests/foo.rs", "content": "x"});
        assert!(matches!(
            checker.check("write", &input2),
            CheckResult::NeedConfirm(_)
        ));
    }

    #[test]
    fn check_read_tool_allow() {
        let checker = checker_with(PermissionsConfig::default(), false);
        assert!(matches!(
            checker.check("read", &serde_json::json!({})),
            CheckResult::Allow
        ));
    }

    #[test]
    fn check_edit_path_glob_allow() {
        let config = PermissionsConfig {
            prefix_level: PrefixLevelConfig {
                edit: PathPrefixConfig {
                    paths: vec!["src/**".to_string()],
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let checker = checker_with(config, false);
        let input = serde_json::json!({"path": "src/lib.rs", "old_string": "a", "new_string": "b"});
        assert!(matches!(
            checker.check("edit", &input),
            CheckResult::Allow
        ));
    }

    #[test]
    fn check_write_absolute_path() {
        let config = PermissionsConfig {
            prefix_level: PrefixLevelConfig {
                write: PathPrefixConfig {
                    paths: vec!["src/**".to_string()],
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let checker = checker_with(config, false);
        // 绝对路径 /tmp/src/foo.rs,strip_prefix(/tmp) 后 = src/foo.rs,匹配 src/**
        let input = serde_json::json!({"path": "/tmp/src/foo.rs", "content": "x"});
        assert!(matches!(
            checker.check("write", &input),
            CheckResult::Allow
        ));
    }
}
