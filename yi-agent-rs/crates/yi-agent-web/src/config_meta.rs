//! 15 个环境变量的元数据定义。

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

/// 所有 15 个环境变量的元数据，按分组排列。
pub static ALL_VARS: &[VarMeta] = &[
    // === Model Provider ===
    VarMeta {
        key: "YI_AGENT_PROVIDER",
        default: Some("anthropic"),
        var_type: VarType::Select,
        group: "Model Provider",
        description: "LLM provider backend",
        options: &["anthropic", "openai"],
    },
    VarMeta {
        key: "YI_AGENT_MODEL",
        default: None,
        var_type: VarType::Text,
        group: "Model Provider",
        description: "Model identifier string",
        options: &[],
    },
    VarMeta {
        key: "YI_AGENT_MODEL_CONTEXT_LENGTH",
        default: None,
        var_type: VarType::Number,
        group: "Model Provider",
        description: "Model max context length in tokens (fallback: 200000)",
        options: &[],
    },
    VarMeta {
        key: "MODEL_API_KEY",
        default: None,
        var_type: VarType::Secret,
        group: "Model Provider",
        description: "API key (overrides provider-specific key)",
        options: &[],
    },
    VarMeta {
        key: "MODEL_API_URL",
        default: None,
        var_type: VarType::Text,
        group: "Model Provider",
        description: "API endpoint URL (overrides provider-specific URL)",
        options: &[],
    },
    VarMeta {
        key: "ANTHROPIC_API_KEY",
        default: None,
        var_type: VarType::Secret,
        group: "Model Provider",
        description: "Anthropic provider API key",
        options: &[],
    },
    VarMeta {
        key: "ANTHROPIC_BASE_URL",
        default: Some("https://api.anthropic.com"),
        var_type: VarType::Text,
        group: "Model Provider",
        description: "Anthropic API base URL",
        options: &[],
    },
    VarMeta {
        key: "OPENAI_API_KEY",
        default: None,
        var_type: VarType::Secret,
        group: "Model Provider",
        description: "OpenAI provider API key",
        options: &[],
    },
    VarMeta {
        key: "OPENAI_BASE_URL",
        default: Some("https://api.openai.com"),
        var_type: VarType::Text,
        group: "Model Provider",
        description: "OpenAI API base URL",
        options: &[],
    },
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
        key: "YI_AGENT_COMPACT_KEEP_TURNS",
        default: Some("4"),
        var_type: VarType::Number,
        group: "Agent",
        description: "Turns retained during compaction",
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
    fn all_vars_count_is_15() {
        assert_eq!(ALL_VARS.len(), 15);
    }

    #[test]
    fn groups_count_is_3() {
        assert_eq!(groups().len(), 3);
    }

    #[test]
    fn groups_are_ordered() {
        let g = groups();
        assert_eq!(
            g,
            vec!["Model Provider", "Agent", "Tools"]
        );
    }

    #[test]
    fn find_returns_meta() {
        let m = find("YI_AGENT_PROVIDER").unwrap();
        assert_eq!(m.var_type, VarType::Select);
        assert_eq!(m.options, &["anthropic", "openai"]);
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
    fn context_length_has_no_default() {
        let m = find("YI_AGENT_MODEL_CONTEXT_LENGTH").unwrap();
        assert!(m.default.is_none());
    }

    #[test]
    fn compact_ratio_default_is_80() {
        let m = find("YI_AGENT_COMPACT_RATIO").unwrap();
        assert_eq!(m.default, Some("80"));
    }
}
