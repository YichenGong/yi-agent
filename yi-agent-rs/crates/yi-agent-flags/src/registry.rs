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

pub const REGISTRY: &[FlagDef] = &[];

/// 按名字查注册表。
pub fn find(name: &str) -> Option<&'static FlagDef> {
    REGISTRY.iter().find(|def| def.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
