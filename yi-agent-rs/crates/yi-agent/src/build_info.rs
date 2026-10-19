//! 构建期注入的版本标识。

pub const VERSION: &str = env!("YI_AGENT_VERSION");
pub const BUILD_KIND: &str = env!("YI_AGENT_BUILD_KIND");

/// clap 需要 `&'static str`；`env!` 正是静态值。
pub const CLAP_VERSION: &str = VERSION;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_is_never_empty() {
        assert!(!VERSION.is_empty());
    }

    #[test]
    fn the_build_kind_is_one_of_three() {
        assert!(
            matches!(BUILD_KIND, "release" | "dev" | "nogit"),
            "unexpected build kind: {BUILD_KIND}"
        );
    }

    #[test]
    fn only_a_release_build_reports_a_bare_version() {
        if BUILD_KIND == "release" {
            assert!(
                !VERSION.contains("-dev") && !VERSION.contains("+nogit"),
                "release build must not carry a dev marker: {VERSION}"
            );
            assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
        }
    }
}
