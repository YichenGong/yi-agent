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
}
