//! Tests for the side source, policy and ledger.
//!
//! Kept in one file so the module files stay readable. These cover the four
//! guarantees Task 06 owns: inheritance (model/effort/prompt boundary), the fixed
//! read-only registry, shared provider admission, and the request ledger.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use super::test_support::{CapturingProvider, mock_provider, source_with_history};
use super::{
    EffectiveSideSource, SideError, SideErrorReason, SideSourcePublisher, SideToolPolicy,
    build_side_tool_setup, map_budget_error,
};
use crate::bootstrap::{resolve_side_config, resolve_side_prompt};
use futures::StreamExt;
use yi_agent_core::{
    Agent, AgentConfig, ContentBlock, ProviderTurnGate, ProviderTurnLease, ToolRegistry,
};

/// A gate with a fixed number of permits, recording how many waits happened.
struct TestGate {
    permits: Arc<tokio::sync::Semaphore>,
    waits: AtomicUsize,
}

struct TestLease {
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl TestGate {
    fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            permits: Arc::new(tokio::sync::Semaphore::new(capacity)),
            waits: AtomicUsize::new(0),
        })
    }
}

impl ProviderTurnGate for TestGate {
    fn acquire(
        &self,
    ) -> futures::future::BoxFuture<'static, Result<Box<dyn ProviderTurnLease>, String>> {
        let permits = Arc::clone(&self.permits);
        self.waits.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let permit = permits
                .acquire_owned()
                .await
                .map_err(|_| "gate closed".to_string())?;
            Ok(Box::new(TestLease { _permit: permit }) as Box<dyn ProviderTurnLease>)
        })
    }
}

// ---------------------------------------------------------------------------
// inheritance
// ---------------------------------------------------------------------------

#[tokio::test]
async fn side_turn_inherits_model_effort_and_prompt_boundary() {
    let provider = Arc::new(CapturingProvider::new("旁支回答"));
    let mut source = source_with_history("父问题", "父回答");
    source.provider = provider.clone();
    // The fixed prompt the side will run with: inheritance plus the policy tail.
    source.config.system_prompt = Some(yi_agent_core::side_system_prompt(Some("parent prompt")));

    let mut session = source.session.lock().unwrap().clone();
    session.push(yi_agent_core::Message::user(yi_agent_core::SIDE_BOUNDARY));
    session.push(yi_agent_core::Message::user("旁支新问题"));

    let tools = Arc::new(ToolRegistry::new());
    let mut agent = Agent::new(Arc::clone(&source.provider), tools, source.config.clone())
        .with_session(session);

    let mut events = agent
        .run_blocks(vec![ContentBlock::Text("ignored".into())])
        .await
        .expect("the side must be able to run");
    // The provider request is issued when the stream is polled, so drain it.
    while let Some(event) = events.next().await {
        if matches!(event, yi_agent_core::AgentEvent::Done { .. }) {
            break;
        }
    }

    let requests = provider.requests();
    assert_eq!(requests.len(), 1, "exactly one provider request");
    let request = &requests[0];
    assert_eq!(request.model, "parent-model", "model inheritance");
    assert_eq!(
        request.params.reasoning_effort.as_deref(),
        Some("medium"),
        "effort inheritance"
    );
    let system = request.system.as_deref().unwrap_or_default();
    assert!(
        system.starts_with("parent prompt"),
        "the parent prompt must be preserved: {system}"
    );
    assert!(
        system.contains(yi_agent_core::SIDE_SYSTEM_POLICY),
        "the fixed side policy must be appended last: {system}"
    );
    assert!(
        system.ends_with(yi_agent_core::SIDE_SYSTEM_POLICY),
        "the policy must be the tail, so a later refresh cannot displace it"
    );
    assert!(
        request.tools.is_empty(),
        "a side request must carry zero tools, got {:?}",
        request
            .tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>()
    );
    assert!(
        request
            .messages
            .iter()
            .any(|message| matches!(&message.content[0], ContentBlock::Text(text) if text == yi_agent_core::SIDE_BOUNDARY)),
        "the hidden boundary must reach the model"
    );
}

#[tokio::test]
async fn side_with_empty_history_never_calls_the_provider() {
    let provider = Arc::new(CapturingProvider::new("must not happen"));
    let source = super::test_support::source_with_empty_history();
    let publisher = SideSourcePublisher::new(EffectiveSideSource {
        provider: provider.clone(),
        ..source
    });

    // The snapshot itself refuses: an empty safe history is not a side.
    let error = publisher.snapshot().await.unwrap_err();
    assert_eq!(error.reason, SideErrorReason::ParentNotStarted);
    assert_eq!(
        provider.call_count(),
        0,
        "an empty side must never reach the provider"
    );
}

#[test]
fn side_config_inherits_model_and_effort_overrides() {
    let base = crate::config::sample_config_for_side_test();
    let cwd = PathBuf::from("/tmp/side-cwd");

    let inherited = resolve_side_config(&base, &cwd, None, None, true).unwrap();
    assert_eq!(inherited.model, base.model);
    assert_eq!(inherited.reasoning_effort, base.reasoning_effort);
    assert_eq!(inherited.workdir, cwd, "the side's cwd is its parent's");

    let overridden = resolve_side_config(
        &base,
        &cwd,
        Some("parent-effective-model"),
        Some("high"),
        true,
    )
    .unwrap();
    assert_eq!(overridden.model, "parent-effective-model");
    assert_eq!(overridden.reasoning_effort.as_deref(), Some("high"));
    // Compaction settings must survive unchanged: the side inherits the policy,
    // it does not get its own.
    assert_eq!(overridden.compact_threshold, base.compact_threshold);
    assert_eq!(
        overridden.compact_user_budget_tokens,
        base.compact_user_budget_tokens
    );
}

#[test]
fn side_config_refuses_an_empty_model_override() {
    let base = crate::config::sample_config_for_side_test();
    let error =
        resolve_side_config(&base, &PathBuf::from("/tmp"), Some("  "), None, true).unwrap_err();
    assert!(
        error.to_string().contains("模型"),
        "an empty override must be an explicit error: {error}"
    );
}

#[test]
fn side_prompt_ends_with_the_fixed_policy_and_is_read_only() {
    let mut cfg = crate::config::sample_config_for_side_test();
    let dir = tempfile::tempdir().unwrap();
    cfg.workdir = dir.path().to_path_buf();
    std::fs::write(dir.path().join("AGENTS.md"), "project rule").unwrap();

    let prompt = resolve_side_prompt(&cfg).unwrap().expect("a prompt");
    assert!(
        prompt.contains("project rule"),
        "root AGENTS.md must be inherited: {prompt}"
    );
    assert!(
        prompt.contains(&chrono::Local::now().format("%Y-%m-%d").to_string()),
        "the date must be present, as in the parent's prompt"
    );
    assert!(
        prompt.ends_with(yi_agent_core::SIDE_SYSTEM_POLICY),
        "the policy must be appended at the very end"
    );
    // Read-only: the side path must not have created a skills installation.
    assert!(
        !dir.path().join(".yi-agent/skills").exists(),
        "resolving a side prompt must never install skills"
    );
}

/// A side keeps the configured catalog budget even when the directory is far
/// larger: it must never inherit another role's "include everything" decision.
#[test]
fn side_prompt_truncates_an_oversized_catalog_without_prompting() {
    let mut cfg = crate::config::sample_config_for_side_test();
    let dir = tempfile::tempdir().unwrap();
    cfg.workdir = dir.path().to_path_buf();
    cfg.skills_catalog_budget = 400;

    let skills_root = dir.path().join("skills");
    for index in 0..20 {
        let skill = skills_root.join(format!("s{index:02}"));
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            format!(
                "---\nname: s{index:02}\ndescription: {}\n---\nbody",
                "x".repeat(600)
            ),
        )
        .unwrap();
    }

    let roots = vec![(skills_root, yi_agent_skills::SkillScope::User)];
    let prompts = crate::live_settings::prompt::LivePromptComposer::new(
        &cfg,
        crate::live_settings::prompt::PromptRole::Side,
        Some(Arc::new(yi_agent_skills::SkillsService::new(roots))),
        crate::live_settings::prompt::InitialCatalogPolicy::Bounded,
    )
    .unwrap()
    .render(
        cfg.system_prompt.as_deref(),
        400,
        crate::live_settings::sources::ValueSource::Settings,
    )
    .unwrap()
    .expect("a prompt");
    assert!(
        !prompts.contains("s19"),
        "an oversized catalog must be truncated, not included whole"
    );
    assert!(prompts.ends_with(yi_agent_core::SIDE_SYSTEM_POLICY));
}

// ---------------------------------------------------------------------------
// the empty registry
// ---------------------------------------------------------------------------

/// A side answers from the snapshot only. The registry must be empty — not a
/// filtered copy of the ordinary one — so the model cannot read files, scan the
/// workspace, search the web or reach any MCP/plugin tool.
#[test]
fn side_registry_is_empty_and_not_a_filtered_ordinary_registry() {
    let registry = build_side_tool_setup();
    assert!(
        registry.names().is_empty(),
        "a side must expose no tools at all, got {:?}",
        registry.names()
    );
    assert!(SideToolPolicy.allowed_names().is_empty());
}

/// Taking a snapshot must never rewrite the parent. The old implementation
/// replaced the parent's messages with the forkable prefix before creating an
/// independent session — silently trimming the live conversation.
#[tokio::test]
async fn side_publisher_fork_never_replaces_parent_messages() {
    use yi_agent_core::{ContentBlock, Message};

    let source = source_with_history("主问题", "主进展");
    let parent = Arc::clone(&source.session);
    parent
        .lock()
        .unwrap()
        .push(Message::assistant(vec![ContentBlock::ToolUse {
            id: "unfinished".into(),
            name: "read".into(),
            input: serde_json::json!({"path": "src/lib.rs"}),
        }]));
    let before = serde_json::to_vec(parent.lock().unwrap().messages()).unwrap();

    let captured = SideSourcePublisher::new(source).snapshot().await.unwrap();

    assert!(
        !Arc::ptr_eq(&parent, &captured.session),
        "the side must get its own session handle"
    );
    assert_eq!(
        serde_json::to_vec(parent.lock().unwrap().messages()).unwrap(),
        before,
        "the parent transcript must stay byte-identical"
    );
    assert!(
        !captured
            .session
            .lock()
            .unwrap()
            .messages()
            .iter()
            .flat_map(|message| &message.content)
            .any(|block| matches!(block, ContentBlock::ToolUse { .. })),
        "the fork itself still trims the unsafe tail"
    );
}

// ---------------------------------------------------------------------------
// shared provider admission
// ---------------------------------------------------------------------------

#[tokio::test]
async fn side_gate_waits_for_capacity_and_is_cancellable() {
    // Capacity 1: the parent holds the only lease, so the side must wait.
    let gate = TestGate::new(1);
    let parent_lease = gate.acquire().await.unwrap();

    let side = tokio::spawn({
        let gate = Arc::clone(&gate) as Arc<dyn ProviderTurnGate>;
        async move { gate.acquire().await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !side.is_finished(),
        "the side must wait while the parent holds the only lease"
    );

    // The wait is cancellable: aborting the acquisition task must not leave a
    // permit behind.
    side.abort();
    let _ = side.await;
    drop(parent_lease);
    let lease = gate.acquire().await.unwrap();
    drop(lease);
    assert!(gate.waits.load(Ordering::SeqCst) >= 3);
}

#[tokio::test]
async fn side_gate_starts_while_the_parent_still_holds_a_lease_when_capacity_is_two() {
    let gate = TestGate::new(2);
    let _parent_lease = gate.acquire().await.unwrap();
    // Two permits: the side gets its own without waiting for the parent's turn
    // to finish.
    let side_lease = tokio::time::timeout(Duration::from_millis(500), gate.acquire())
        .await
        .expect("the side must not be serialised behind the parent's whole turn")
        .unwrap();
    drop(side_lease);
}

/// A side freezes the configuration the parent actually ran with.
///
/// The parent's assembled `config` is not necessarily what the provider saw: a
/// live-settings save can replace the prompt. Taking the snapshot must therefore
/// read the parent's published cache **once**, and the side must never hold a
/// live source afterwards — otherwise a save mid-conversation would silently
/// change the premise of a conversation the user asked to freeze.
#[tokio::test]
async fn a_side_freezes_the_parents_effective_config_and_keeps_no_live_source() {
    let source = source_with_history("q", "a");
    let mut with_cache = source.clone();
    let mut effective = with_cache.config.clone();
    effective.model = "parent-actual-model".into();
    effective.system_prompt = Some("parent actual prompt".into());
    with_cache.effective_config = Some(Arc::new(std::sync::Mutex::new(effective)));

    let publisher = SideSourcePublisher::new(with_cache.clone());
    let captured = publisher.snapshot().await.unwrap();
    assert_eq!(
        captured.config.model, "parent-actual-model",
        "the side must inherit the configuration the parent really used"
    );
    let prompt = captured.config.system_prompt.clone().unwrap_or_default();
    assert!(
        prompt.starts_with("parent actual prompt"),
        "the prompt must be the rendered one, not the assembly-time guess: {prompt}"
    );
    // ...but it must still end with the side policy. The cache holds the
    // *parent's* config, whose prompt tells the model to drive the parent's task;
    // forwarding it verbatim would make the side resume that work.
    assert!(
        prompt.ends_with(yi_agent_core::SIDE_SYSTEM_POLICY),
        "a cached parent prompt must not drop the side policy: {prompt}"
    );
    assert!(
        captured.effective_config.is_none(),
        "the snapshot must not keep a live source: nothing may update it later"
    );

    // A later parent save changes the parent's cache, never the captured side.
    let mut later = with_cache.config.clone();
    later.model = "changed-after-entry".into();
    *with_cache
        .effective_config
        .as_ref()
        .unwrap()
        .lock()
        .unwrap() = later;
    assert_eq!(captured.config.model, "parent-actual-model");
    assert_eq!(captured.revision, captured.revision);
}

#[tokio::test]
async fn side_source_can_carry_the_parent_gate_without_reusing_its_lease() {
    let gate = TestGate::new(2);
    let mut source = source_with_history("q", "a");
    source.provider_gate = Some(Arc::clone(&gate) as Arc<dyn ProviderTurnGate>);

    let agent = Agent::new(
        mock_provider("reply"),
        Arc::new(ToolRegistry::new()),
        AgentConfig::default(),
    );
    let agent = match source.provider_gate.clone() {
        Some(gate) => agent.with_provider_turn_gate(gate),
        None => agent,
    };
    // Attaching a gate must not acquire anything by itself: the lease is taken
    // per turn, inside `think_phase`.
    assert_eq!(
        gate.waits.load(Ordering::SeqCst),
        0,
        "attaching the gate must not consume a permit"
    );
    drop(agent);
}

// ---------------------------------------------------------------------------
// ledger + budget mapping
// ---------------------------------------------------------------------------

#[test]
fn side_budget_overflow_maps_to_a_non_secret_error() {
    let error = map_budget_error(yi_agent_core::BudgetExceeded::Text {
        requested: 10,
        available: 0,
    });
    assert_eq!(error.reason, SideErrorReason::SnapshotTooLarge);
    assert!(!error.message.contains("api_key"));
}

#[test]
fn side_error_is_std_error_and_keeps_its_reason() {
    let error = SideError::new(SideErrorReason::SideCreating, "仍在创建");
    let as_error: &dyn std::error::Error = &error;
    assert!(as_error.to_string().contains("side_creating"));
}

// ---------------------------------------------------------------------------
// source revision consistency
// ---------------------------------------------------------------------------

#[tokio::test]
async fn side_source_snapshot_is_stable_under_repeated_reads() {
    let publisher = SideSourcePublisher::new(source_with_history("父问题", "父回答"));
    let first = publisher.snapshot().await.unwrap();
    let second = publisher.snapshot().await.unwrap();
    assert_eq!(first.revision, second.revision);
    assert_eq!(
        first.session.lock().unwrap().messages().len(),
        second.session.lock().unwrap().messages().len()
    );
    assert!(
        !Arc::ptr_eq(&first.session, &second.session),
        "every snapshot gets its own handle"
    );
}

// ---------------------------------------------------------------------------
// wire strings
// ---------------------------------------------------------------------------

#[test]
fn side_close_reasons_are_stable_snake_case() {
    use super::CloseReason;
    let cases = [
        (CloseReason::UserClosed, "user_closed"),
        (CloseReason::Expired, "expired"),
        (CloseReason::ParentCleared, "parent_cleared"),
        (CloseReason::ParentDeleted, "parent_deleted"),
        (CloseReason::HostShutdown, "host_shutdown"),
        (CloseReason::ResourceLost, "resource_lost"),
    ];
    for (reason, expected) in cases {
        assert_eq!(reason.as_str(), expected);
        assert_eq!(
            serde_json::to_string(&reason).unwrap(),
            format!("\"{expected}\"")
        );
    }
}

#[test]
fn side_error_reasons_match_the_design_wire_strings() {
    let cases = [
        (SideErrorReason::SideNotFound, "side_not_found"),
        (SideErrorReason::ParentNotStarted, "parent_not_started"),
        (SideErrorReason::SideLimit, "side_limit"),
        (SideErrorReason::SnapshotTooLarge, "snapshot_too_large"),
        (SideErrorReason::SourceChanged, "source_changed"),
        (SideErrorReason::SideClosing, "side_closing"),
        (SideErrorReason::CloseConflict, "close_conflict"),
        (SideErrorReason::SnapshotExpired, "snapshot_expired"),
        (SideErrorReason::SideCreating, "side_creating"),
        (SideErrorReason::RequestLimit, "request_limit"),
        (SideErrorReason::ResourceLimit, "resource_limit"),
    ];
    for (reason, expected) in cases {
        assert_eq!(reason.as_str(), expected, "wire string must be stable");
    }
}

#[test]
fn side_error_displays_reason_and_message_without_secrets() {
    let error = SideError::new(SideErrorReason::SourceChanged, "父会话正在更新");
    assert_eq!(error.to_string(), "source_changed: 父会话正在更新");
}
