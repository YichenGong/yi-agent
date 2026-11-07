//! 每会话携带的固定来源 + 共享运行期设置源。
//!
//! Task 3 只定义**数据**：一次初始装配得到的 `binding` 可以克隆给多个会话，
//! 每个会话在各自的安全执行点用 `sources.resolve(&live.refresh().settings)`
//! 取得当次有效值。
//!
//! `agent_source` / `admission_source` 这类适配器属于 Task 6，本任务不实现；
//! 在这里加空实现会让「热更新已接线」看起来成立。

use std::sync::Arc;

use crate::live_settings::LiveSettings;
use crate::live_settings::sources::FrozenRuntimeSources;

/// 一个进程内共享的设置源，加上初始装配时冻结的来源表。
///
/// 两个 `Arc` 都是进程级共享：`live` 按绝对路径全局唯一，`sources` 由初始
/// 装配构造一次后克隆给所有会话。二者都不可变，运行期只读。
#[derive(Clone)]
pub struct RuntimeSettingsBinding {
    pub live: Arc<LiveSettings>,
    pub sources: Arc<FrozenRuntimeSources>,
}

impl std::fmt::Debug for RuntimeSettingsBinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `LiveSettings` 的路径与 `sources` 的键名可以出现，取值绝不出现。
        formatter
            .debug_struct("RuntimeSettingsBinding")
            .field("sources", &self.sources)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_shares_one_live_source_for_the_same_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let live = LiveSettings::shared(path.clone());

        let first = RuntimeSettingsBinding {
            live: Arc::clone(&live),
            sources: Arc::new(FrozenRuntimeSources::default()),
        };
        let second = first.clone();

        assert!(Arc::ptr_eq(&first.live, &second.live));
        assert!(Arc::ptr_eq(&first.sources, &second.sources));
        assert!(
            Arc::ptr_eq(&first.live, &LiveSettings::shared(path)),
            "the binding must use the process-wide shared instance"
        );
    }

    #[test]
    fn binding_resolves_frozen_sources_against_the_live_snapshot() {
        use crate::settings::store::PatchMode;
        use serde_json::json;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let live = LiveSettings::shared(path.clone());
        live.commit(&json!({"agent.max_turns": 41}), PatchMode::ReplaceProvided)
            .unwrap();

        let sources = FrozenRuntimeSources::from_pairs(&[(
            "compact.ratio",
            json!(60),
            crate::live_settings::sources::ValueSource::Override,
        )]);
        let binding = RuntimeSettingsBinding {
            live,
            sources: Arc::new(sources),
        };

        let snapshot = binding.live.refresh();
        let effective = binding.sources.resolve(&snapshot.settings);
        assert_eq!(effective.values["agent.max_turns"], json!(41));
        assert_eq!(
            effective.sources["agent.max_turns"],
            crate::live_settings::sources::ValueSource::Settings
        );
        assert_eq!(effective.values["compact.ratio"], json!(60));
        assert_eq!(
            effective.sources["compact.ratio"],
            crate::live_settings::sources::ValueSource::Override
        );
    }
}
