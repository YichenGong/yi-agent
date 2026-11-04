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

#[tokio::test]
async fn side_open_cold_parent_has_no_resume_side_effects() {
    let dir = TempDir::new().unwrap();
    let store = Arc::new(ThreadStore::new(dir.path()));
    seed(&store, "p1", "q1", "a1");
    let cfg = base_config(dir.path());
    let resolver = ThreadStoreResolver::new(store.clone(), cfg.clone());

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
    let resolver = ThreadStoreResolver::new(store.clone(), base_config(dir.path()));

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
    let resolver = ThreadStoreResolver::new(store, base_config(dir.path()));

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
    let resolver = ThreadStoreResolver::new(store, base_config(dir.path()));

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
    let resolver = ThreadStoreResolver::new(store, base_config(dir.path()));

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

    let resolver = ThreadStoreResolver::new(store, base_config(dir.path())).with_churn(churn);
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
    let resolver = ThreadStoreResolver::new(store, base_config(dir.path()));
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
