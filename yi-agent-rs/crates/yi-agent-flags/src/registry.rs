//! flag 注册表：唯一真源。未在此登记的键一律不受理。

/// 一个功能开关的定义。
pub struct FlagDef {
    /// 唯一键，snake_case，发布后不改名（它就是 flags.json 里的键）。
    pub name: &'static str,
    /// 设置页上显示的一句话说明。
    pub summary: &'static str,
    /// 责任人。
    pub owner: &'static str,
    /// 过期日 `YYYY-MM-DD`，必填。到期后 `no_flag_is_past_its_expiry_date` 失败。
    pub expires_on: &'static str,
    /// 默认值。当前规则：必须全部为 `false`。
    pub default: bool,
}

/// 组长（lead agent）的总开关。名字一旦发布就是 `flags.json` 的键，不能改。
pub const LEAD_AGENT_FLAG: &str = "lead_agent";

/// 远程 SSH 执行的总开关。dev-only：stable 构建下恒关（spec §5.3.1）。
pub const REMOTE_SSH_FLAG: &str = "remote_ssh";

pub const REGISTRY: &[FlagDef] = &[
    FlagDef {
        name: LEAD_AGENT_FLAG,
        summary: "组长：项目级常驻会话，负责分派与回收子任务",
        owner: "yichen",
        expires_on: "2027-01-31",
        default: false,
    },
    FlagDef {
        name: REMOTE_SSH_FLAG,
        summary: "远程执行：经 ssh 在远端项目里跑常驻 app-server，手机可见",
        owner: "yichen",
        expires_on: "2027-06-30",
        default: false,
    },
];

/// 按名字查注册表。
pub fn find(name: &str) -> Option<&'static FlagDef> {
    REGISTRY.iter().find(|def| def.name == name)
}

/// 测试用的注册项。`cfg(test)` 之外的代码看不到它，也进不了 `REGISTRY`。
#[cfg(test)]
pub(crate) mod registry_test_support {
    /// 测试里唯一被承认的 flag 名。
    pub(crate) const KNOWN: &str = "known";
}
/// 测试里承认的名字。生产代码永远返回 `false`（`cfg(test)` 之外不存在）。
#[cfg(test)]
pub(crate) fn is_test_known(name: &str) -> bool {
    name == registry_test_support::KNOWN
}

#[cfg(not(test))]
pub(crate) fn is_test_known(_name: &str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lead_agent_flag_is_registered() {
        let def = find("lead_agent").expect("lead_agent must be in the registry");
        assert!(!def.default, "the lead agent must default to off");
        assert!(
            !def.summary.is_empty(),
            "a summary is what the settings page shows"
        );
        assert!(
            !def.owner.is_empty(),
            "an expired flag needs someone to ask"
        );
    }

    #[test]
    fn the_remote_ssh_flag_is_registered() {
        let def = find(crate::registry::REMOTE_SSH_FLAG).expect("remote_ssh must be registered");
        assert!(
            !def.default,
            "remote ssh must default to off: it is dev-only"
        );
        assert!(!def.summary.is_empty());
        assert!(!def.owner.is_empty());
    }

    #[test]
    fn every_flag_defaults_to_off() {
        for def in REGISTRY {
            assert!(
                !def.default,
                "flag `{}` must default to off: a true default means merging changes \
                 user-visible behavior",
                def.name
            );
        }
    }

    #[test]
    fn no_flag_is_past_its_expiry_date() {
        let today = crate::today();
        for def in REGISTRY {
            assert!(
                def.expires_on > today.as_str(),
                "flag `{}` expired on {} (today is {}) — delete it, or extend the \
                 date if the work is genuinely unfinished",
                def.name,
                def.expires_on,
                today
            );
        }
    }

    #[test]
    fn names_are_unique_and_snake_case() {
        let mut seen = std::collections::HashSet::new();
        for def in REGISTRY {
            assert!(seen.insert(def.name), "duplicate flag name: {}", def.name);
            assert!(
                def.name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "flag `{}` must be snake_case (it becomes a JSON key)",
                def.name
            );
        }
    }

    #[test]
    fn expiry_dates_are_iso_formatted() {
        for def in REGISTRY {
            let bytes = def.expires_on.as_bytes();
            assert_eq!(
                bytes.len(),
                10,
                "flag `{}`: expiry must be YYYY-MM-DD",
                def.name
            );
            assert_eq!(
                bytes[4], b'-',
                "flag `{}`: expiry must be YYYY-MM-DD",
                def.name
            );
            assert_eq!(
                bytes[7], b'-',
                "flag `{}`: expiry must be YYYY-MM-DD",
                def.name
            );
        }
    }

    #[test]
    fn session_search_is_not_a_flag() {
        // 会话内搜索已转为常开功能，前端无条件注册 Cmd+F。这里守住"别再给它
        // 加闸门"：若有人重新登记，前端不会读它，注册表反而会误导用户在实验
        // 页拨一个无效开关。
        assert!(
            find("session_search").is_none(),
            "session search is always on; do not register it as a flag"
        );
    }

    #[test]
    fn a_plugin_switch_is_not_a_host_flag() {
        // 插件自己的开关由插件在清单里声明 `switch_key` 并经受管生命周期推进，
        // 不进宿主 flag 注册表——否则主程序就得替每个插件预先登记一个名字，
        // 也和「加插件不用改主程序」直接冲突。
        assert!(
            find("superpowers_kanban").is_none(),
            "per-plugin switches belong to the plugin manifest, not the host registry"
        );
    }
}
