//! 7 个环境变量的元数据定义（模型键已迁到 `~/.yi-agent/models.json`）。

/// 字段类型。
#[derive(Debug, Clone, PartialEq)]
pub enum VarType {
    Select,
    Secret,
    Text,
    Number,
    Path,
}

/// 单个环境变量的元数据。
#[derive(Debug, Clone)]
pub struct VarMeta {
    pub key: &'static str,
    pub default: Option<&'static str>,
    pub var_type: VarType,
    pub group: &'static str,
    pub description: &'static str,
    /// 仅 Select 类型使用
    pub options: &'static [&'static str],
}

/// 所有 7 个环境变量的元数据，按分组排列（不含模型键）。
pub static ALL_VARS: &[VarMeta] = &[
    // === Agent ===
    VarMeta {
        key: "YI_AGENT_MAX_TURNS",
        default: Some("20"),
        var_type: VarType::Number,
        group: "Agent",
        description: "Max agent turns per conversation",
        options: &[],
    },
    VarMeta {
        key: "YI_AGENT_WORKDIR",
        default: None,
        var_type: VarType::Path,
        group: "Agent",
        description: "Working directory for file tools",
        options: &[],
    },
    VarMeta {
        key: "YI_AGENT_SYSTEM_PROMPT",
        default: None,
        var_type: VarType::Text,
        group: "Agent",
        description: "Custom system prompt override",
        options: &[],
    },
    VarMeta {
        key: "YI_AGENT_COMPACT_RATIO",
        default: Some("80"),
        var_type: VarType::Number,
        group: "Agent",
        description: "Percentage of context length triggering auto-compact",
        options: &[],
    },
    VarMeta {
        key: "YI_AGENT_COMPACT_USER_BUDGET_TOKENS",
        default: Some("20000"),
        var_type: VarType::Number,
        group: "Agent",
        description: "Real user input tokens retained during compaction",
        options: &[],
    },
    VarMeta {
        key: "YI_AGENT_COMPACT_TOOL_BUDGET_TOKENS",
        default: Some("12000"),
        var_type: VarType::Number,
        group: "Agent",
        description: "Complete tool interaction tokens retained during compaction",
        options: &[],
    },
    // === Tools ===
    VarMeta {
        key: "BOCHA_API_KEY",
        default: None,
        var_type: VarType::Secret,
        group: "Tools",
        description: "Bocha web search API key",
        options: &[],
    },
];

/// 返回所有分组名称（按出现顺序，去重）。
pub fn groups() -> Vec<&'static str> {
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    for var in ALL_VARS {
        if seen.insert(var.group) {
            result.push(var.group);
        }
    }
    result
}

/// 按 key 查找元数据。
pub fn find(key: &str) -> Option<&'static VarMeta> {
    ALL_VARS.iter().find(|v| v.key == key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_vars_count_is_7() {
        assert_eq!(ALL_VARS.len(), 7);
    }

    #[test]
    fn model_keys_are_no_longer_editable() {
        // 9 个模型键必须从这里消失——它们归 models.json，不再有绕过清单的写入口。
        for key in yi_agent_runtime::config::MODEL_ENV_KEYS {
            assert!(find(key).is_none(), "{key} must no longer be editable here");
        }
    }

    #[test]
    fn groups_count_is_2() {
        assert_eq!(groups().len(), 2);
    }

    #[test]
    fn groups_are_ordered() {
        let g = groups();
        assert_eq!(g, vec!["Agent", "Tools"]);
    }

    #[test]
    fn find_returns_meta() {
        let m = find("YI_AGENT_MAX_TURNS").unwrap();
        assert_eq!(m.var_type, VarType::Number);
    }

    #[test]
    fn find_returns_none_for_unknown() {
        assert!(find("UNKNOWN_VAR").is_none());
    }

    #[test]
    fn select_vars_have_options() {
        for var in ALL_VARS {
            if var.var_type == VarType::Select {
                assert!(
                    !var.options.is_empty(),
                    "{} is Select but has no options",
                    var.key
                );
            }
        }
    }

    #[test]
    fn secret_vars_have_no_options() {
        for var in ALL_VARS {
            if var.var_type == VarType::Secret {
                assert!(
                    var.options.is_empty(),
                    "{} is Secret but has options",
                    var.key
                );
            }
        }
    }

    #[test]
    fn all_keys_are_unique() {
        let mut keys: Vec<&str> = ALL_VARS.iter().map(|v| v.key).collect();
        keys.sort();
        let before = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), before, "duplicate keys found");
    }

    #[test]
    fn workdir_has_no_default() {
        let m = find("YI_AGENT_WORKDIR").unwrap();
        assert!(m.default.is_none());
    }

    #[test]
    fn compact_ratio_default_is_80() {
        let m = find("YI_AGENT_COMPACT_RATIO").unwrap();
        assert_eq!(m.default, Some("80"));
    }
}
