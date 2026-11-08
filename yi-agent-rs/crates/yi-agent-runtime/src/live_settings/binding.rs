//! 每会话携带的固定来源 + 共享运行期设置源 + core 窄接口适配器。
//!
//! 一次初始装配得到的 `binding` 可以克隆给多个会话，每个会话在各自的安全
//! 执行点经 [`RuntimeSettingsBinding::agent_source`] 取得**一份**有效快照。
//! 适配器只读、每次调用重新 `resolve`，因此保存后下一次操作自然采用新值。
//!
//! 提示词以 [`LivePromptComposer`] 为唯一组合入口：目录扫描是唯一可能耗时较
//! 长的步骤，扫完后必须重查一次快照；指纹变了就丢弃尚未发出的派生结果并按
//! 新值重组（spec §5.2）。压缩用独立的窄快照，绝不渲染正常提示词。

use std::sync::Arc;

use yi_agent_core::live_settings::{
    AdmissionLimits, AdmissionLimitsSource, AgentSettingsSource, LiveAgentSnapshot,
    LiveCompactSnapshot, LiveSettingsError, SharedSettingsSource,
};

use crate::config::{COMPACT_RATIO_DEFAULT, MAX_TURNS_DEFAULT};
use crate::live_settings::LiveSettings;
use crate::live_settings::prompt::LivePromptComposer;
use crate::live_settings::sources::{FrozenRuntimeSources, ValueSource};

/// 未声明模型上下文时沿用 `config.rs` 的同一兜底长度。
pub const FALLBACK_MODEL_CONTEXT_LENGTH: u32 = 200_000;

/// 目录扫描后重查快照的最大重建次数；超过即按受控错误收口。
///
/// 现实中一次保存不足以触发两次连续变化；给出上限是为了让「持续变化」不会
/// 变成无限循环，而不是为了正常路径。
const MAX_PROMPT_REBUILDS: usize = 3;

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

impl RuntimeSettingsBinding {
    /// 该会话的 core 设置来源。每次操作重新解析，不缓存有效值。
    pub fn agent_source(&self, context: AgentSettingsContext) -> SharedSettingsSource {
        Arc::new(SessionSettingsSource {
            binding: self.clone(),
            context,
        })
    }

    /// 该会话的准入来源，供 worker 工厂在真实准入点读取。
    pub fn admission_source(&self) -> Arc<dyn AdmissionLimitsSource> {
        Arc::new(SessionAdmissionSource {
            binding: self.clone(),
        })
    }

    /// 压缩专用窄来源：不渲染提示词。
    pub fn compact_source(&self, context: AgentSettingsContext) -> SharedSettingsSource {
        self.agent_source(context)
    }
}

/// 一个会话的提示词上下文：真实模型上下文长度 + 统一组合器。
///
/// 上下文长度来自本次装配选定的模型（不是 settings 里的目标值），因此阈值的
/// 分母是真实上下文，而不是某一份文档的默认值。
#[derive(Clone)]
pub struct AgentSettingsContext {
    pub model_context_length: u32,
    pub prompt: Arc<LivePromptComposer>,
}

/// 从一份已解析的有效值里读出一次操作需要的标量。
struct ResolvedOperation {
    user_prompt: Option<String>,
    catalog_budget: usize,
    catalog_source: ValueSource,
    max_turns: Option<u32>,
    compact_ratio: u32,
    user_budget_tokens: usize,
    tool_budget_tokens: usize,
}

impl ResolvedOperation {
    /// 提示词派生只依赖这三项；它们变了就必须重组尚未发出的请求。
    fn prompt_fingerprint(&self) -> (Option<&str>, usize, ValueSource) {
        (
            self.user_prompt.as_deref(),
            self.catalog_budget,
            self.catalog_source,
        )
    }
}

fn resolve_operation(
    binding: &RuntimeSettingsBinding,
    snapshot: &crate::live_settings::SettingsSnapshot,
) -> ResolvedOperation {
    let effective = binding.sources.resolve(&snapshot.settings);
    let value = |key: &str| effective.values.get(key).cloned().unwrap_or_default();
    let source = |key: &str| {
        effective
            .sources
            .get(key)
            .copied()
            .unwrap_or(ValueSource::Default)
    };
    ResolvedOperation {
        user_prompt: value("agent.system_prompt").as_str().map(str::to_string),
        catalog_budget: value("agent.skills_catalog_budget")
            .as_u64()
            .unwrap_or_default() as usize,
        catalog_source: source("agent.skills_catalog_budget"),
        max_turns: value("agent.max_turns")
            .as_u64()
            .map(|value| value as u32)
            .or(Some(MAX_TURNS_DEFAULT)),
        compact_ratio: value("compact.ratio")
            .as_u64()
            .unwrap_or(u64::from(COMPACT_RATIO_DEFAULT)) as u32,
        user_budget_tokens: value("compact.user_budget_tokens")
            .as_u64()
            .unwrap_or_default() as usize,
        tool_budget_tokens: value("compact.tool_budget_tokens")
            .as_u64()
            .unwrap_or_default() as usize,
    }
}

/// ``model_context_length * ratio / 100``，整数舍入，饱和到 `u32`。
///
/// 与 `config.rs` 的启动期计算同一口径；这里只是分母换成该会话真实上下文。
fn compact_threshold(model_context_length: u32, ratio: u32) -> u32 {
    if ratio == 0 {
        return 0;
    }
    (u64::from(model_context_length) * u64::from(ratio) / 100).min(u64::from(u32::MAX)) as u32
}

/// 每会话适配器：读取共享来源，形成 core 需要的窄快照。
pub struct SessionSettingsSource {
    binding: RuntimeSettingsBinding,
    context: AgentSettingsContext,
}

impl std::fmt::Debug for SessionSettingsSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionSettingsSource")
            .field("context_length", &self.context.model_context_length)
            .finish_non_exhaustive()
    }
}

impl SessionSettingsSource {
    /// 一次操作的完整快照。
    ///
    /// 目录扫描之后重查一次：扫描可能很慢，等待期间保存的用户段/预算必须
    /// 进入**尚未发出**的请求。返回时轮次与压缩参数也取自同一份最终快照，
    /// 因此不会出现「提示词是新值、阈值是旧值」的混合。
    fn snapshot_inner(&self) -> Result<LiveAgentSnapshot, LiveSettingsError> {
        for _ in 0..MAX_PROMPT_REBUILDS {
            let snapshot = self.binding.live.refresh();
            let operation = resolve_operation(&self.binding, &snapshot);
            let (prompt, scanned) = self.context.prompt.render_tracked(
                operation.user_prompt.as_deref(),
                operation.catalog_budget,
                operation.catalog_source,
            )?;
            if !scanned {
                return Ok(self.finish(snapshot.revision.clone(), &operation, prompt));
            }

            // The scan is the only long step: re-read and compare the inputs the
            // prompt actually derives from.
            let recheck = self.binding.live.refresh();
            let rechecked = resolve_operation(&self.binding, &recheck);
            if rechecked.prompt_fingerprint() == operation.prompt_fingerprint() {
                return Ok(self.finish(recheck.revision.clone(), &rechecked, prompt));
            }
            // Settings moved while the catalog was being rendered: drop the
            // derived result and rebuild from the new values.
            tracing::debug!("prompt inputs changed during the catalog scan, rebuilding");
        }
        Err(LiveSettingsError::new("settings_unstable"))
    }

    fn finish(
        &self,
        revision: String,
        operation: &ResolvedOperation,
        prompt: Option<String>,
    ) -> LiveAgentSnapshot {
        let threshold =
            compact_threshold(self.context.model_context_length, operation.compact_ratio);
        LiveAgentSnapshot {
            revision,
            max_turns: operation.max_turns,
            compact_threshold: Some(threshold),
            compact_user_budget_tokens: operation.user_budget_tokens,
            compact_tool_budget_tokens: operation.tool_budget_tokens,
            system_prompt: prompt,
        }
    }
}

impl AgentSettingsSource for SessionSettingsSource {
    fn snapshot(&self) -> Result<LiveAgentSnapshot, LiveSettingsError> {
        self.snapshot_inner()
    }

    /// 压缩只解析四个参数：不渲染提示词，也不扫描目录。
    fn compact_snapshot(&self) -> Result<LiveCompactSnapshot, LiveSettingsError> {
        let snapshot = self.binding.live.refresh();
        let operation = resolve_operation(&self.binding, &snapshot);
        Ok(LiveCompactSnapshot {
            revision: snapshot.revision.clone(),
            compact_threshold: Some(compact_threshold(
                self.context.model_context_length,
                operation.compact_ratio,
            )),
            user_budget_tokens: operation.user_budget_tokens,
            tool_budget_tokens: operation.tool_budget_tokens,
        })
    }
}

/// 子代理准入容量的适配器。
struct SessionAdmissionSource {
    binding: RuntimeSettingsBinding,
}

impl AdmissionLimitsSource for SessionAdmissionSource {
    fn snapshot(&self) -> Result<AdmissionLimits, LiveSettingsError> {
        let snapshot = self.binding.live.refresh();
        let effective = self.binding.sources.resolve(&snapshot.settings);
        let value = |key: &str| {
            effective
                .values
                .get(key)
                .and_then(serde_json::Value::as_u64)
                .map(|value| value as u16)
                .unwrap_or_default()
        };
        Ok(AdmissionLimits {
            direct_children: value("agent.max_direct_children"),
            resident_subagents: value("agent.max_resident_subagents"),
        })
    }

    fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.binding.live.subscribe()
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

#[cfg(test)]
mod adapter_tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;
    use crate::config::sample_config;
    use crate::live_settings::prompt::test_hooks;
    use crate::live_settings::prompt::{InitialCatalogPolicy, LivePromptComposer, PromptRole};
    use crate::settings::Settings;
    use crate::settings::store::PatchMode;
    use serde_json::json;

    fn skill_dir(dir: &Path, name: &str, description: &str) {
        let path = dir.join(name);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(
            path.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\nbody"),
        )
        .unwrap();
    }

    struct Fixture {
        _project: tempfile::TempDir,
        _skills: tempfile::TempDir,
        live: Arc<LiveSettings>,
        binding: RuntimeSettingsBinding,
        context: AgentSettingsContext,
    }

    fn fixture(with_skills: bool, model_context_length: u32) -> Fixture {
        let project = tempfile::TempDir::new().unwrap();
        let skills = tempfile::TempDir::new().unwrap();
        std::fs::write(project.path().join("AGENTS.md"), "PROJECT").unwrap();
        if with_skills {
            skill_dir(skills.path(), "alpha-skill", "first catalog entry");
        }
        let mut cfg = sample_config();
        cfg.workdir = project.path().to_path_buf();
        let service = with_skills.then(|| {
            Arc::new(yi_agent_skills::SkillsService::new(vec![(
                skills.path().to_path_buf(),
                yi_agent_skills::SkillScope::User,
            )]))
        });
        let prompt = Arc::new(
            LivePromptComposer::new(
                &cfg,
                PromptRole::Normal,
                service,
                InitialCatalogPolicy::Bounded,
            )
            .unwrap(),
        );
        let live = LiveSettings::shared(project.path().join("settings.json"));
        Fixture {
            binding: RuntimeSettingsBinding {
                live: Arc::clone(&live),
                sources: Arc::new(FrozenRuntimeSources::default()),
            },
            live,
            context: AgentSettingsContext {
                model_context_length,
                prompt,
            },
            _project: project,
            _skills: skills,
        }
    }

    /// The threshold uses the session's real model context, not a global
    /// default: 100000 @ 50% and 200000 @ 50% must differ by exactly 2x.
    #[test]
    fn same_snapshot_uses_session_model_context() {
        let small = fixture(false, 100_000);
        small
            .live
            .commit(&json!({"compact.ratio": 50}), PatchMode::ReplaceProvided)
            .unwrap();
        let small_snapshot = small
            .binding
            .agent_source(small.context.clone())
            .snapshot()
            .unwrap();

        let large = fixture(false, 200_000);
        large
            .live
            .commit(&json!({"compact.ratio": 50}), PatchMode::ReplaceProvided)
            .unwrap();
        let large_snapshot = large
            .binding
            .agent_source(large.context.clone())
            .snapshot()
            .unwrap();

        assert_eq!(small_snapshot.compact_threshold, Some(50_000));
        assert_eq!(large_snapshot.compact_threshold, Some(100_000));
    }

    /// An undeclared context length falls back to the same 200k the config uses.
    #[test]
    fn undeclared_context_falls_back_to_the_config_length() {
        let fx = fixture(false, FALLBACK_MODEL_CONTEXT_LENGTH);
        fx.live
            .commit(&json!({"compact.ratio": 80}), PatchMode::ReplaceProvided)
            .unwrap();
        let snapshot = fx
            .binding
            .agent_source(fx.context.clone())
            .snapshot()
            .unwrap();
        assert_eq!(snapshot.compact_threshold, Some(160_000));
    }

    /// Rendered prompt follows the live user segment and budget on each call.
    #[test]
    fn snapshot_renders_the_live_user_segment_and_budget() {
        let fx = fixture(true, 200_000);
        fx.live
            .commit(
                &json!({"agent.system_prompt": "USER-A"}),
                PatchMode::ReplaceProvided,
            )
            .unwrap();
        let source = fx.binding.agent_source(fx.context.clone());
        let first = source.snapshot().unwrap();
        assert!(first.system_prompt.as_deref().unwrap().contains("USER-A"));
        assert!(
            first
                .system_prompt
                .as_deref()
                .unwrap()
                .contains("alpha-skill")
        );

        fx.live
            .commit(
                &json!({"agent.system_prompt": "USER-B"}),
                PatchMode::ReplaceProvided,
            )
            .unwrap();
        let second = source.snapshot().unwrap();
        let prompt = second.system_prompt.as_deref().unwrap();
        assert!(prompt.contains("USER-B"));
        assert!(!prompt.contains("USER-A"), "the old segment must be gone");
        assert!(prompt.contains("alpha-skill"), "the catalog stays");
    }

    /// Compaction must never render the normal prompt or scan the catalog.
    #[test]
    fn compact_snapshot_never_renders_the_prompt() {
        let _serial = test_hooks::serial();
        let fx = fixture(true, 200_000);
        fx.live
            .commit(
                &json!({
                    "agent.system_prompt": "USER-A",
                    "compact.ratio": 60,
                    "compact.user_budget_tokens": 111,
                    "compact.tool_budget_tokens": 222,
                }),
                PatchMode::ReplaceProvided,
            )
            .unwrap();

        // Install a barrier that would hang a normal render; compaction must
        // never reach it.
        let target = Arc::as_ptr(&fx.context.prompt) as usize;
        let (entered_tx, entered_rx) = mpsc::channel();
        test_hooks::install(
            target,
            Arc::new(move || {
                let _ = entered_tx.send(());
            }),
        );

        let source = fx.binding.agent_source(fx.context.clone());
        let compact = source.compact_snapshot().unwrap();
        assert_eq!(compact.compact_threshold, Some(120_000));
        assert_eq!(compact.user_budget_tokens, 111);
        assert_eq!(compact.tool_budget_tokens, 222);
        assert!(
            entered_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "compaction must not scan the catalog"
        );
        test_hooks::clear();

        // The normal path does reach it, which proves the barrier is live.
        let (entered_tx, entered_rx) = mpsc::channel();
        test_hooks::install(
            target,
            Arc::new(move || {
                let _ = entered_tx.send(());
            }),
        );
        source.snapshot().unwrap();
        assert!(
            entered_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "the normal path must scan"
        );
        test_hooks::clear();
    }

    /// A save during the catalog scan must be reflected in the returned
    /// snapshot, not just inside the composer.
    #[test]
    fn catalog_wait_rechecks_the_unstarted_snapshot() {
        let _serial = test_hooks::serial();
        let fx = fixture(true, 200_000);
        fx.live
            .commit(
                &json!({"agent.system_prompt": "USER-A", "agent.max_turns": 7}),
                PatchMode::ReplaceProvided,
            )
            .unwrap();
        let live = Arc::clone(&fx.live);
        let source = fx.binding.agent_source(fx.context.clone());

        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
        let target = Arc::as_ptr(&fx.context.prompt) as usize;
        {
            let release_rx = Arc::clone(&release_rx);
            test_hooks::install(
                target,
                Arc::new(move || {
                    let _ = entered_tx.send(());
                    let _ = release_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(10));
                }),
            );
        }

        let handle = std::thread::spawn(move || source.snapshot());
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the scan must reach the barrier");

        // Save while the scan is parked: a new user segment, budget and cap.
        live.commit(
            &json!({
                "agent.system_prompt": "USER-B",
                "agent.max_turns": 9,
                "compact.ratio": 25,
            }),
            PatchMode::ReplaceProvided,
        )
        .unwrap();
        release_tx.send(()).unwrap();

        let snapshot = handle.join().unwrap().unwrap();
        let prompt = snapshot.system_prompt.as_deref().unwrap();
        assert!(prompt.contains("USER-B"), "prompt: {prompt}");
        assert!(!prompt.contains("USER-A"));
        assert_eq!(
            snapshot.max_turns,
            Some(9),
            "the cap must come from the final read"
        );
        assert_eq!(snapshot.compact_threshold, Some(50_000));
        test_hooks::clear();
    }

    /// Without a catalog scan there is no wait, so one read is enough and the
    /// snapshot is consistent.
    #[test]
    fn snapshot_without_skills_reads_once_and_stays_consistent() {
        let fx = fixture(false, 200_000);
        fx.live
            .commit(
                &json!({"agent.system_prompt": "USER-A", "agent.max_turns": 11}),
                PatchMode::ReplaceProvided,
            )
            .unwrap();
        let snapshot = fx
            .binding
            .agent_source(fx.context.clone())
            .snapshot()
            .unwrap();
        assert_eq!(snapshot.max_turns, Some(11));
        assert!(snapshot.system_prompt.unwrap().contains("USER-A"));
    }

    /// Admission limits come from the same source and wake subscribers.
    #[test]
    fn admission_source_resolves_live_capacities() {
        let fx = fixture(false, 200_000);
        fx.live
            .commit(
                &json!({"agent.max_direct_children": 3, "agent.max_resident_subagents": 5}),
                PatchMode::ReplaceProvided,
            )
            .unwrap();
        let source = fx.binding.admission_source();
        let limits = source.snapshot().unwrap();
        assert_eq!(limits.direct_children, 3);
        assert_eq!(limits.resident_subagents, 5);

        let receiver = source.subscribe();
        assert!(!receiver.has_changed().unwrap());
        fx.live
            .commit(
                &json!({"agent.max_direct_children": 2}),
                PatchMode::ReplaceProvided,
            )
            .unwrap();
        assert!(receiver.has_changed().unwrap());
        assert_eq!(source.snapshot().unwrap().direct_children, 2);
    }

    /// A configuration without explicit values still resolves defaults, never
    /// zeroes: a missing document must not read as "no children allowed".
    #[test]
    fn missing_settings_resolve_to_defaults_not_zero() {
        let fx = fixture(false, 200_000);
        let limits = fx.binding.admission_source().snapshot().unwrap();
        assert_eq!(
            limits.direct_children,
            crate::config::DIRECT_CHILDREN_DEFAULT
        );
        assert_eq!(
            limits.resident_subagents,
            crate::config::RESIDENT_SUBAGENTS_DEFAULT
        );
        let _ = Settings::default();
    }
}

/// Host-level assertions: a real `Agent` against a recording provider.
///
/// These live in the library (not `tests/`) because they drive the catalog
/// barrier, which is an internal test-only injection point. The claim under test
/// is "the request that actually went out used the final settings".
#[cfg(test)]
mod host_tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::time::Duration;

    use futures::StreamExt;
    use serde_json::json;
    use tokio::time::timeout;

    use yi_agent_core::{
        Agent, AgentConfig, AgentEvent, DoneReason, Provider, ProviderEvent, ProviderRequest,
        StopReason, ToolRegistry,
    };

    use super::*;
    use crate::config::sample_config;
    use crate::live_settings::prompt::{
        InitialCatalogPolicy, LivePromptComposer, PromptRole, test_hooks,
    };
    use crate::settings::store::PatchMode;

    struct RecordingProvider {
        requests: Arc<std::sync::Mutex<Vec<ProviderRequest>>>,
    }

    impl RecordingProvider {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                requests: Arc::new(std::sync::Mutex::new(Vec::new())),
            })
        }

        fn recorded(&self) -> Vec<ProviderRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl Provider for RecordingProvider {
        async fn call_stream(
            &self,
            req: ProviderRequest,
        ) -> Result<futures::stream::BoxStream<'static, ProviderEvent>, yi_agent_core::ProviderError>
        {
            self.requests.lock().unwrap().push(req);
            Ok(futures::stream::iter(vec![
                ProviderEvent::TextDelta("done".to_string()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ])
            .boxed())
        }
    }

    fn skill_dir(dir: &Path, name: &str) {
        let path = dir.join(name);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(
            path.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: a skill for the catalog\n---\nbody"),
        )
        .unwrap();
    }

    struct Harness {
        _project: tempfile::TempDir,
        _skills: tempfile::TempDir,
        live: Arc<LiveSettings>,
        binding: RuntimeSettingsBinding,
        context: AgentSettingsContext,
    }

    fn harness(model_context_length: u32) -> Harness {
        let project = tempfile::TempDir::new().unwrap();
        let skills = tempfile::TempDir::new().unwrap();
        skill_dir(skills.path(), "alpha-skill");
        std::fs::write(project.path().join("AGENTS.md"), "PROJECT-RULES").unwrap();

        let mut cfg = sample_config();
        cfg.workdir = project.path().to_path_buf();
        cfg.system_prompt = None;
        cfg.max_turns = 3;

        let service = Arc::new(yi_agent_skills::SkillsService::new(vec![(
            skills.path().to_path_buf(),
            yi_agent_skills::SkillScope::User,
        )]));
        let prompt = Arc::new(
            LivePromptComposer::new(
                &cfg,
                PromptRole::Normal,
                Some(service),
                InitialCatalogPolicy::Bounded,
            )
            .unwrap(),
        );

        let live = LiveSettings::shared(project.path().join("settings.json"));
        Harness {
            binding: RuntimeSettingsBinding {
                live: Arc::clone(&live),
                sources: Arc::new(FrozenRuntimeSources::default()),
            },
            live,
            context: AgentSettingsContext {
                model_context_length,
                prompt,
            },
            _project: project,
            _skills: skills,
        }
    }

    async fn run(agent: &mut Agent, prompt: &str) -> Vec<AgentEvent> {
        let stream = agent
            .run(prompt.to_string())
            .await
            .expect("the run must start");
        timeout(Duration::from_secs(15), async move {
            stream.collect::<Vec<_>>().await
        })
        .await
        .expect("run did not finish")
    }

    fn composer_key(context: &AgentSettingsContext) -> usize {
        Arc::as_ptr(&context.prompt) as usize
    }

    /// A save that lands while the catalog is being scanned must reach the
    /// request that has not been sent yet: user segment, cap and compaction
    /// parameters all come from one final snapshot.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn catalog_wait_rechecks_the_first_provider_request() {
        // This test installs a process-global barrier; serialise it, but never
        // hold the guard across an await (clippy::await_holding_lock).
        let serial = test_hooks::serial();
        let harness = harness(200_000);
        harness
            .live
            .commit(
                &json!({
                    "agent.system_prompt": "USER-A",
                    "agent.max_turns": 5,
                    "compact.ratio": 80,
                }),
                PatchMode::ReplaceProvided,
            )
            .unwrap();

        let source = harness.binding.agent_source(harness.context.clone());
        let live = Arc::clone(&harness.live);
        let target = composer_key(&harness.context);

        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
        {
            let release_rx = Arc::clone(&release_rx);
            test_hooks::install(
                target,
                Arc::new(move || {
                    let _ = entered_tx.send(());
                    let _ = release_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(10));
                }),
            );
        }

        let saver = tokio::task::spawn_blocking(move || {
            entered_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("the scan must park");
            live.commit(
                &json!({
                    "agent.system_prompt": "USER-B",
                    "agent.max_turns": 9,
                    "compact.ratio": 25,
                }),
                PatchMode::ReplaceProvided,
            )
            .unwrap();
            release_tx.send(()).unwrap();
        });

        let provider = RecordingProvider::new();
        let config = AgentConfig {
            max_turns: Some(200),
            system_prompt: Some("static-should-not-be-used".to_string()),
            ..AgentConfig::default()
        };
        let mut agent = Agent::new(provider.clone(), Arc::new(ToolRegistry::new()), config);
        agent.set_live_settings(source);

        // Release the global serialisation before any await: the barrier itself
        // already guarantees the ordering this test needs.
        drop(serial);
        let events = run(&mut agent, "hello").await;
        saver.await.unwrap();
        test_hooks::clear();

        assert!(
            events
                .iter()
                .all(|event| !matches!(event, AgentEvent::Error(_))),
            "unexpected error: {events:?}"
        );
        let requests = provider.recorded();
        let system = requests
            .first()
            .and_then(|request| request.system.clone())
            .expect("the request must carry a system prompt");
        assert!(system.contains("USER-B"), "prompt: {system}");
        assert!(
            !system.contains("USER-A"),
            "the parked scan must be discarded"
        );
        assert!(!system.contains("static-should-not-be-used"));
        assert!(system.contains("alpha-skill"), "catalog: {system}");
        assert!(system.contains("PROJECT-RULES"));
    }

    /// Clearing the user segment between turns keeps every other block.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn clearing_the_user_prompt_between_turns_keeps_the_other_segments() {
        let harness = harness(200_000);
        harness
            .live
            .commit(
                &json!({"agent.system_prompt": "USER-A"}),
                PatchMode::ReplaceProvided,
            )
            .unwrap();

        let provider = RecordingProvider::new();
        let mut agent = Agent::new(
            provider.clone(),
            Arc::new(ToolRegistry::new()),
            AgentConfig::default(),
        );
        agent.set_live_settings(harness.binding.agent_source(harness.context.clone()));

        run(&mut agent, "first").await;
        harness
            .live
            .commit(
                &json!({"agent.system_prompt": ""}),
                PatchMode::ReplaceProvided,
            )
            .unwrap();
        run(&mut agent, "second").await;

        let requests = provider.recorded();
        assert!(requests.len() >= 2, "two turns must reach the provider");
        let first = requests[0].system.clone().unwrap();
        let second = requests[1].system.clone().unwrap();
        assert!(first.contains("USER-A"));
        assert!(!second.contains("USER-A"), "second: {second}");
        assert!(!second.contains("User-provided instructions:"));
        assert!(second.contains("PROJECT-RULES"), "second: {second}");
        assert!(second.contains("alpha-skill"), "second: {second}");
        assert!(second.contains("Current date:"));
    }

    /// A cap reached at the safe point ends the run before the next iteration,
    /// so no second request is built and no tool is replayed.
    ///
    /// The provider asks for one tool call, which is what makes a second
    /// iteration possible; the live snapshot then reports a cap of one, so the
    /// loop must stop with `MaxTurns` after exactly one completed request.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reached_turn_limit_stops_before_the_next_iteration() {
        let harness = harness(200_000);
        harness
            .live
            .commit(&json!({"agent.max_turns": 1}), PatchMode::ReplaceProvided)
            .unwrap();

        struct ToolCallingProvider {
            requests: Arc<std::sync::Mutex<Vec<ProviderRequest>>>,
        }

        #[async_trait::async_trait]
        impl Provider for ToolCallingProvider {
            async fn call_stream(
                &self,
                req: ProviderRequest,
            ) -> Result<
                futures::stream::BoxStream<'static, ProviderEvent>,
                yi_agent_core::ProviderError,
            > {
                let index = {
                    let mut requests = self.requests.lock().unwrap();
                    requests.push(req);
                    requests.len()
                };
                let events = if index == 1 {
                    vec![
                        ProviderEvent::ToolUseStart {
                            id: "call-1".to_string(),
                            name: "bash".to_string(),
                        },
                        ProviderEvent::ToolUseDelta {
                            id: "call-1".to_string(),
                            partial_json: "{\"command\":\"echo hi\"}".to_string(),
                        },
                        ProviderEvent::ToolUseEnd {
                            id: "call-1".to_string(),
                        },
                        // A tool-bearing turn is what makes another iteration
                        // possible; the stop reason itself carries no tool tag.
                        ProviderEvent::Stop {
                            reason: StopReason::EndTurn,
                        },
                    ]
                } else {
                    vec![
                        ProviderEvent::TextDelta("done".to_string()),
                        ProviderEvent::Stop {
                            reason: StopReason::EndTurn,
                        },
                    ]
                };
                Ok(futures::stream::iter(events).boxed())
            }
        }

        let provider = Arc::new(ToolCallingProvider {
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
        });
        let mut agent = Agent::new(
            provider.clone(),
            Arc::new(ToolRegistry::new()),
            AgentConfig::default(),
        );
        agent.set_live_settings(harness.binding.agent_source(harness.context.clone()));

        let events = run(&mut agent, "hello").await;
        let reasons: Vec<DoneReason> = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::Done { reason } => Some(reason.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            reasons,
            vec![DoneReason::MaxTurns],
            "the cap must stop the second iteration: {events:?}"
        );
        assert_eq!(
            provider.requests.lock().unwrap().len(),
            1,
            "no second request may be built"
        );
    }
}
