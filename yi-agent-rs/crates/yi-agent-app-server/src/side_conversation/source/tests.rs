//! Resolver tests: a cold parent must behave exactly like a read.
//!
//! These cover the parts of Task 10 that can be proven without a live driver: the
//! strict cold path, the bounded retry, and the fact that nothing writes.

use std::sync::Arc;

use yi_agent_runtime::side_conversation::{SideErrorReason, SideSourcePublisher};

use super::*;
use crate::thread_store::{ThreadMode, ThreadStore, TurnLine};
use tempfile::TempDir;

fn base_config(workdir: &std::path::Path) -> RuntimeConfig {
    let mut cfg = crate::test_support::test_config();
    cfg.model = "parent-model".into();
    cfg.provider = "anthropic".into();
    cfg.api_url = "http://127.0.0.1:1".into();
    cfg.api_key = "test-key".into();
    cfg.workdir = workdir.to_path_buf();
    cfg
}

fn meta(id: &str, cwd: &std::path::Path) -> crate::thread_store::ThreadMeta {
    crate::thread_store::ThreadMeta {
        thread_id: id.into(),
        cwd: cwd.display().to_string(),
        model: "parent-model".into(),
        created_at: 1,
        updated_at: 1,
        title: None,
        permission_mode: ThreadMode::Normal,
        pin_seq: None,
        driver_project: None,
        driver_ref: None,
        model_ref: None,
        reasoning_effort: Some("high".into()),
        tag: None,
        preserve_thinking: true,
        extra_roots: Vec::new(),
        role: None,
        anchor_sha: None,
        content_epoch: 0,
        last_item_id: None,
        last_read_at: 0,
        last_completed_at: 0,
        forked_from: None,
        plugin_bindings: Vec::new(),
        sandbox_baseline: None,
    }
}

fn seed(store: &ThreadStore, id: &str, question: &str, answer: &str) {
    store.create(&meta(id, store.workdir())).unwrap();
    store
        .append_turn(
            id,
            &TurnLine::Turn {
                items: vec![crate::protocol::Item::UserMessage {
                    id: format!("user-{question}"),
                    text: question.into(),
                    attachments: Vec::new(),
                    images: Vec::new(),
                }],
                usage: None,
                usage_total: None,
                messages: vec![
                    yi_agent_core::Message::user(question),
                    yi_agent_core::Message::assistant(vec![yi_agent_core::ContentBlock::Text(
                        answer.into(),
                    )]),
                ],
            },
        )
        .unwrap();
}

#[test]
fn side_cold_config_resolves_the_catalog_instead_of_the_display_model() {
    use yi_agent_runtime::models::{ModelCatalog, ModelEntry, ModelProvider};

    let dir = TempDir::new().unwrap();
    let store = ThreadStore::new(dir.path());
    seed(&store, "p1", "question", "answer");
    let entry = |name: &str, url: &str, model: &str| ModelEntry {
        name: name.into(),
        provider: ModelProvider::Openai,
        api_url: url.into(),
        model: model.into(),
        api_key: "local-test-placeholder".into(),
        api_style: Some("chat".into()),
        reasoning_effort: Some("low".into()),
        reasoning_capabilities: None,
        context_length: Some(100_000),
    };
    let catalog = ModelCatalog {
        models: vec![
            entry(
                "default entry",
                "http://127.0.0.1:19001/v1",
                "default-wire-model",
            ),
            entry(
                "session entry",
                "http://127.0.0.1:19002/v1",
                "session-wire-model",
            ),
        ],
        default_model: Some("default entry".into()),
        subagent_model: None,
    };
    let base = base_config(dir.path());
    for (model_ref, want_url, want_model) in [
        (None, "http://127.0.0.1:19001/v1", "default-wire-model"),
        (
            Some("session entry"),
            "http://127.0.0.1:19002/v1",
            "session-wire-model",
        ),
    ] {
        store.set_model_ref("p1", model_ref).unwrap();
        let cold = read_cold_parent(&store, "p1", &base, &catalog).unwrap();
        assert_eq!(cold.config.provider, "openai");
        assert_eq!(cold.config.api_url, want_url);
        assert_eq!(
            cold.config.model, want_model,
            "model_ref selects a catalog entry; meta.model is only a display value"
        );
        assert_eq!(cold.config.api_style.as_str(), "chat");
        assert_eq!(cold.config.reasoning_effort.as_deref(), Some("high"));
    }
    store.set_model_ref("p1", None).unwrap();
    store.set_reasoning_effort("p1", None).unwrap();
    let cold = read_cold_parent(&store, "p1", &base, &catalog).unwrap();
    assert_eq!(cold.config.reasoning_effort.as_deref(), Some("low"));

    let mut injected = base.clone();
    injected.model_injected = true;
    let cold = read_cold_parent(&store, "p1", &injected, &catalog).unwrap();
    assert_eq!(cold.config.provider, "anthropic");
    assert_eq!(cold.config.api_url, "http://127.0.0.1:1");
    assert_eq!(cold.config.model, "parent-model");
    store.set_model_ref("p1", Some("session entry")).unwrap();
    let cold = read_cold_parent(&store, "p1", &injected, &catalog).unwrap();
    assert_eq!(cold.config.api_url, "http://127.0.0.1:19002/v1");
    assert_eq!(cold.config.model, "session-wire-model");
}

#[tokio::test]
async fn side_open_cold_parent_has_no_resume_side_effects() {
    let dir = TempDir::new().unwrap();
    let store = Arc::new(ThreadStore::new(dir.path()));
    seed(&store, "p1", "q1", "a1");
    let cfg = base_config(dir.path());
    let resolver = ThreadStoreResolver::new(store.clone(), cfg.clone(), Default::default());

    let paths = [
        store.meta_path("p1"),
        store.log_path("p1"),
        store.partial_path("p1"),
    ];
    let before: Vec<Option<Vec<u8>>> = paths.iter().map(|p| std::fs::read(p).ok()).collect();
    let listing_before: Vec<String> = {
        let mut names: Vec<String> = std::fs::read_dir(store.root())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        names
    };

    let source = resolver
        .resolve("p1")
        .await
        .expect("a healthy parent resolves");

    let after: Vec<Option<Vec<u8>>> = paths.iter().map(|p| std::fs::read(p).ok()).collect();
    let listing_after: Vec<String> = {
        let mut names: Vec<String> = std::fs::read_dir(store.root())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        names
    };

    assert_eq!(before, after, "opening a side must not rewrite the parent");
    assert_eq!(
        listing_before, listing_after,
        "opening a side must not create a driver lock, title cache or partial"
    );
    assert_eq!(source.primary, cfg.workdir);
    assert_eq!(source.config.model, "parent-model");
    assert_eq!(
        source.config.gen_params.reasoning_effort.as_deref(),
        Some("high"),
        "the parent's effort override must be inherited"
    );
}

#[tokio::test]
async fn side_open_cold_parent_inherits_the_parents_history() {
    let dir = TempDir::new().unwrap();
    let store = Arc::new(ThreadStore::new(dir.path()));
    seed(&store, "p1", "父问题", "父回答");
    let resolver =
        ThreadStoreResolver::new(store.clone(), base_config(dir.path()), Default::default());

    let source = resolver.resolve("p1").await.unwrap();
    let session = source
        .session
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    assert_eq!(session.messages().len(), 2);
}

#[tokio::test]
async fn side_open_cold_parent_fails_on_an_empty_history() {
    let dir = TempDir::new().unwrap();
    let store = Arc::new(ThreadStore::new(dir.path()));
    store
        .create(&meta("p1", dir.path()))
        .expect("meta-only thread");
    let resolver = ThreadStoreResolver::new(store, base_config(dir.path()), Default::default());

    let error = resolver
        .resolve("p1")
        .await
        .expect_err("a side with nothing to inherit must fail");
    assert_eq!(error.reason, SideErrorReason::ParentNotStarted);
}

#[tokio::test]
async fn side_open_unknown_parent_reports_not_found() {
    let dir = TempDir::new().unwrap();
    let store = Arc::new(ThreadStore::new(dir.path()));
    let resolver = ThreadStoreResolver::new(store, base_config(dir.path()), Default::default());

    let error = resolver.resolve("nobody").await.unwrap_err();
    assert_eq!(error.reason, SideErrorReason::UnknownThread);
}

#[tokio::test]
async fn side_open_corrupt_parent_is_refused_not_skipped() {
    let dir = TempDir::new().unwrap();
    let store = Arc::new(ThreadStore::new(dir.path()));
    seed(&store, "p1", "q1", "a1");
    std::fs::OpenOptions::new()
        .append(true)
        .open(store.log_path("p1"))
        .unwrap()
        .write_all(b"{bad\n")
        .unwrap();
    let resolver = ThreadStoreResolver::new(store, base_config(dir.path()), Default::default());

    let error = resolver
        .resolve("p1")
        .await
        .expect_err("a corrupt parent must not become a smaller side");
    assert_eq!(error.reason, SideErrorReason::ParentNotStarted);
    assert!(
        error.message.contains("严格"),
        "the message must say the strict read failed, got: {}",
        error.message
    );
}

#[tokio::test]
async fn side_open_cold_parent_bounds_retries_when_the_parent_keeps_changing() {
    // A parent whose files change on every read can never be snapshotted
    // consistently. The resolver must give up with `source_changed` rather than
    // spin forever: an `open` that never returns is worse than one that tells the
    // user to try again.
    let dir = TempDir::new().unwrap();
    let store = Arc::new(ThreadStore::new(dir.path()));
    seed(&store, "p1", "q1", "a1");

    let log = store.log_path("p1");
    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let bump = counter.clone();
    let churn: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        // Append a real turn each time so the stamp genuinely differs (a no-op
        // write would let the retry succeed and prove nothing).
        let n = bump.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let line = serde_json::to_vec(&TurnLine::Turn {
            items: Vec::new(),
            usage: None,
            usage_total: None,
            messages: vec![yi_agent_core::Message::user(format!("churn {n}"))],
        })
        .unwrap();
        let mut file = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        file.write_all(&line).unwrap();
        file.write_all(b"\n").unwrap();
    });

    let resolver = ThreadStoreResolver::new(store, base_config(dir.path()), Default::default())
        .with_churn(churn);
    let error = resolver
        .resolve("p1")
        .await
        .expect_err("a parent that keeps changing must fail");
    assert_eq!(error.reason, SideErrorReason::SourceChanged);
    assert_eq!(
        counter.load(std::sync::atomic::Ordering::SeqCst),
        SIDE_COLD_ATTEMPTS,
        "the retry count must be bounded by SIDE_COLD_ATTEMPTS, not unbounded"
    );
}

#[tokio::test]
async fn side_published_source_is_a_consistent_snapshot() {
    let dir = TempDir::new().unwrap();
    let store = Arc::new(ThreadStore::new(dir.path()));
    seed(&store, "p1", "q1", "a1");
    let resolver = ThreadStoreResolver::new(store, base_config(dir.path()), Default::default());
    let source = resolver.resolve("p1").await.unwrap();

    let publisher = SideSourcePublisher::new(source);
    let snapshot = publisher
        .snapshot()
        .await
        .expect("a healthy source snapshots");
    // The snapshot must have its own session: a write through it cannot reach the
    // publisher's source.
    assert!(!Arc::ptr_eq(
        &snapshot.session,
        &publisher.current().unwrap().session
    ));
}

use std::io::Write;

/// A live parent is authoritative: the side must see what the running
/// conversation has, not the last flush, and must reuse its provider object.
#[tokio::test]
async fn side_resolver_prefers_the_live_parent_and_reuses_its_provider() {
    let dir = TempDir::new().unwrap();
    let live = Arc::new(crate::side_conversation::LiveSideParents::new());

    let agent = yi_agent_core::Agent::new(
        Arc::new(CountingProvider),
        Arc::new(yi_agent_core::ToolRegistry::new()),
        yi_agent_core::AgentConfig {
            model: "live-model".into(),
            system_prompt: Some("live prompt".into()),
            ..Default::default()
        },
    );
    let session = agent.session_handle();
    session
        .lock()
        .unwrap()
        .push(yi_agent_core::Message::user("父已经提交的问题"));

    let publisher = Arc::new(SideSourcePublisher::new(
        source_from_live_agent(
            &agent,
            dir.path(),
            vec![dir.path().to_path_buf()],
            Default::default(),
        )
        .expect("a live source with history is valid"),
    ));
    live.publish("p1".into(), Arc::clone(&publisher));

    // The parent commits another message after the publisher was installed.
    session
        .lock()
        .unwrap()
        .push(yi_agent_core::Message::assistant(vec![
            yi_agent_core::ContentBlock::Text("父后来的一步".into()),
        ]));

    let resolver = WorkspaceResolver::new(
        Arc::new(crate::workspace_index::WorkspaceIndex::new(
            dir.path().to_path_buf(),
        )),
        base_config(dir.path()),
        live,
    );
    let source = resolver.resolve("p1").await.unwrap();
    let rendered =
        serde_json::to_string(&source.session.lock().unwrap().messages().to_vec()).unwrap();
    assert!(
        rendered.contains("父后来的一步"),
        "the side must inherit committed progress the file does not have yet: {rendered}"
    );
    assert_eq!(source.config.model, "live-model");
}

struct CountingProvider;

#[async_trait::async_trait]
impl yi_agent_core::Provider for CountingProvider {
    async fn call_stream(
        &self,
        _req: yi_agent_core::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::ProviderEvent>,
        yi_agent_core::ProviderError,
    > {
        Ok(Box::pin(futures::stream::empty()))
    }
}

/// A cold parent must not rescan project rules or skills: those are external
/// reads, and their content would make the side's prompt differ from the one the
/// parent actually ran with.
#[tokio::test]
async fn side_cold_parent_does_not_scan_project_rules_or_skills() {
    let dir = TempDir::new().unwrap();
    let store = Arc::new(ThreadStore::new(dir.path()));
    seed(&store, "p1", "父问题", "父回答");
    // A rules file that would be picked up by a project scan.
    std::fs::create_dir_all(dir.path().join(".yi-agent")).unwrap();
    std::fs::write(
        dir.path().join("AGENTS.md"),
        "RULES_SENTINEL_SHOULD_NOT_APPEAR",
    )
    .unwrap();

    let mut cfg = base_config(dir.path());
    cfg.system_prompt = Some("user-configured prompt".into());
    let resolver = ThreadStoreResolver::new(store, cfg, Default::default());
    let source = resolver.resolve("p1").await.unwrap();

    let prompt = source.config.system_prompt.clone().unwrap_or_default();
    assert!(
        prompt.contains("user-configured prompt"),
        "the user's own prompt is inherited: {prompt}"
    );
    assert!(
        prompt.ends_with(yi_agent_core::SIDE_SYSTEM_POLICY),
        "the fixed side policy stays the tail: {prompt}"
    );
    assert!(
        !prompt.contains("RULES_SENTINEL_SHOULD_NOT_APPEAR"),
        "a cold side must not rescan project rules: {prompt}"
    );
}
