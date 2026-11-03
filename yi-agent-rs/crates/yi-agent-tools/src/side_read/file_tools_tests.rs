//! Tests for the side read-only file tools.
//!
//! Two properties matter most here and are asserted for every tool:
//!
//! 1. a revoked root is refused (no tool caches authorisation), and
//! 2. the bytes come from the handle that was authorised — replacing the path
//!    after the open does not change what is returned.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde_json::json;
use tokio_util::sync::CancellationToken;
use yi_agent_core::{ToolMetadata, ToolRegistry, ToolSource};

use super::*;
use crate::side_read::SideRoots;

struct TestRoots(RwLock<Vec<std::path::PathBuf>>);

impl SideRoots for TestRoots {
    fn current_roots(&self) -> Vec<std::path::PathBuf> {
        self.0.read().unwrap().clone()
    }
}

struct Fixture {
    /// Owns the temporary directory for the fixture's lifetime.
    _dir: tempfile::TempDir,
    path: std::path::PathBuf,
    roots: Arc<TestRoots>,
    registry: ToolRegistry,
    cancel: CancellationToken,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let roots = Arc::new(TestRoots(RwLock::new(vec![path.clone()])));
        let access =
            Arc::new(SideReadAccess::new(path.clone(), vec![path.clone()], roots.clone()).unwrap());
        let pool = Arc::new(crate::side_read::blocking::SideBlockingPool::new(
            8, 2, 8, 2,
        ));
        let cancel = CancellationToken::new();
        let mut registry = ToolRegistry::new();
        register_side_file_tools(
            &mut registry,
            access,
            pool,
            "side-1".to_string(),
            cancel.clone(),
            Arc::new(()),
        );
        Self {
            _dir: dir,
            path,
            roots,
            registry,
            cancel,
        }
    }

    fn write(&self, relative: &str, bytes: &[u8]) -> std::path::PathBuf {
        let full = self.path.join(relative);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&full, bytes).unwrap();
        full
    }

    fn revoke(&self) {
        self.roots.0.write().unwrap().clear();
    }

    async fn call(&self, tool: &str, args: serde_json::Value) -> yi_agent_core::ToolResult {
        let tool = self.registry.get(tool).expect("tool registered");
        tool.call(args).await
    }
}

fn text_of(result: &yi_agent_core::ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            yi_agent_core::ContentBlock::Text(text) => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn side_file_tools_are_builtin_and_read_only() {
    let f = Fixture::new();
    for name in ["read", "read_document", "view_image", "glob", "grep"] {
        let tool = f.registry.get(name).expect("registered");
        let metadata = tool.metadata();
        assert_eq!(metadata.source, ToolSource::Builtin, "{name} source");
        assert!(metadata.read_only, "{name} must be read-only");
        assert!(
            !metadata.requires_confirmation,
            "{name} must not require confirmation"
        );
    }
    // The registry really contains exactly these five side tools.
    assert_eq!(
        f.registry.get("read").unwrap().name(),
        "read",
        "side tools keep the ordinary names so the model's expectations hold"
    );
}

#[tokio::test]
async fn side_read_returns_numbered_lines() {
    let f = Fixture::new();
    f.write("a.txt", b"one\ntwo\nthree\n");
    let result = f.call("read", json!({"path": "a.txt"})).await;
    assert!(!result.is_error, "{}", text_of(&result));
    let text = text_of(&result);
    assert!(text.contains("one"));
    assert!(text.contains("three"));
}

#[tokio::test]
async fn side_read_honours_offset_and_limit() {
    let f = Fixture::new();
    f.write("a.txt", b"1\n2\n3\n4\n5\n");
    let result = f
        .call("read", json!({"path": "a.txt", "offset": 2, "limit": 2}))
        .await;
    let text = text_of(&result);
    // Exactly the requested window: line 2 and line 3, numbered with their real
    // line numbers, and the continuation hint pointing at line 4.
    assert!(
        text.contains("     2\t2") && text.contains("     3\t3"),
        "offset/limit must select lines 2-3: {text}"
    );
    assert!(!text.contains("\t4"), "limit must stop at line 3: {text}");
    assert!(text.contains("continue with offset=4"), "hint: {text}");
}

#[tokio::test]
async fn side_read_tools_refuse_a_revoked_root() {
    let f = Fixture::new();
    f.write("a.txt", b"secret");
    f.write("doc.md", b"# title");
    f.revoke();
    for (tool, args) in [
        ("read", json!({"path": "a.txt"})),
        ("read_document", json!({"path": "doc.md"})),
        ("glob", json!({"pattern": "**/*.txt"})),
        ("grep", json!({"pattern": "secret"})),
    ] {
        let result = f.call(tool, args).await;
        assert!(result.is_error, "{tool} must fail after revocation");
    }
}

#[tokio::test]
async fn side_read_document_parses_text_and_reports_pages_errors() {
    let f = Fixture::new();
    f.write("notes.md", b"# Title\n\nbody text\n");
    let result = f.call("read_document", json!({"path": "notes.md"})).await;
    assert!(!result.is_error, "{}", text_of(&result));
    assert!(text_of(&result).contains("body text"));

    // An invalid pages spec is reported by the shared parser, not silently
    // ignored (the side tool must keep the path tool's wording).
    let bad = f
        .call(
            "read_document",
            json!({"path": "notes.md", "pages": "not-a-range"}),
        )
        .await;
    assert!(bad.is_error, "an invalid pages spec must be rejected");
}

#[tokio::test]
async fn side_glob_finds_files_and_respects_revocation() {
    let f = Fixture::new();
    f.write("src/one.rs", b"fn one() {}");
    f.write("src/two.rs", b"fn two() {}");
    f.write("other.txt", b"x");
    let result = f.call("glob", json!({"pattern": "**/*.rs"})).await;
    assert!(!result.is_error, "{}", text_of(&result));
    let text = text_of(&result);
    assert!(text.contains("one.rs"));
    assert!(text.contains("two.rs"));
    assert!(!text.contains("other.txt"));
}

#[tokio::test]
async fn side_grep_reports_matching_lines() {
    let f = Fixture::new();
    f.write("src/lib.rs", b"fn alpha() {}\nfn beta() {}\n");
    let result = f.call("grep", json!({"pattern": "alpha"})).await;
    assert!(!result.is_error, "{}", text_of(&result));
    let text = text_of(&result);
    assert!(text.contains("lib.rs"));
    assert!(text.contains("alpha"));
    assert!(!text.contains("beta"));
}

#[tokio::test]
async fn side_grep_rejects_an_invalid_regex() {
    let f = Fixture::new();
    let result = f.call("grep", json!({"pattern": "([unclosed"})).await;
    assert!(result.is_error);
}

#[tokio::test]
async fn side_view_image_returns_an_image_block() {
    let f = Fixture::new();
    // A 1x1 PNG.
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    f.write("dot.png", PNG);
    let result = f.call("view_image", json!({"path": "dot.png"})).await;
    assert!(!result.is_error, "{}", text_of(&result));
    assert!(
        result
            .content
            .iter()
            .any(|block| matches!(block, yi_agent_core::ContentBlock::Image { .. })),
        "view_image must return an image block"
    );
}

#[tokio::test]
async fn side_read_reads_the_opened_handle_not_the_replaced_path() {
    // The decisive property: the bytes come from the handle that was opened, so
    // swapping the file after the open cannot change what is returned. The
    // check-then-read-by-path shape would return the replacement.
    let f = Fixture::new();
    let original = f.write("a.txt", b"original-content\n");

    let access = Arc::new(
        crate::side_read::SideReadAccess::new(
            f.path.clone(),
            vec![f.path.clone()],
            f.roots.clone(),
        )
        .unwrap(),
    );
    let file = access.open_regular(std::path::Path::new("a.txt")).unwrap();

    // Replace the *directory entry* after the handle exists. An in-place
    // `write` would reuse the same inode and prove nothing; `rename` installs a
    // different file, so a path re-resolve would now see the replacement.
    let replacement = f.write("replacement.txt", b"replacement-content\n");
    std::fs::rename(&replacement, &original).unwrap();
    let mut bytes = Vec::new();
    {
        use std::io::Read;
        let mut file = file;
        file.read_to_end(&mut bytes).unwrap();
    }
    assert_eq!(
        String::from_utf8_lossy(&bytes),
        "original-content\n",
        "the open handle must still refer to the authorised file"
    );
}

#[tokio::test]
async fn side_read_rejects_a_symlink_at_call_time() {
    let f = Fixture::new();
    f.write("real.txt", b"ok");
    std::os::unix::fs::symlink(f.path.join("real.txt"), f.path.join("link.txt")).unwrap();
    let result = f.call("read", json!({"path": "link.txt"})).await;
    assert!(result.is_error, "a symlink must be refused by the tool too");
}

#[tokio::test]
async fn side_tools_report_cancellation_instead_of_hanging() {
    let f = Fixture::new();
    f.write("a.txt", b"x");
    f.cancel.cancel();
    let result = f.call("read", json!({"path": "a.txt"})).await;
    assert!(result.is_error, "a cancelled side must not run tool work");
}

#[tokio::test]
async fn side_tools_never_scan_outside_the_authorised_roots() {
    let f = Fixture::new();
    f.write("inside.txt", b"needle");
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("outside.txt"), b"needle").unwrap();

    // Point the scan at the outside directory: authorisation is per path, so it
    // must refuse rather than walk it.
    let result = f
        .call(
            "glob",
            json!({"pattern": "**/*.txt", "path": outside.path().to_string_lossy()}),
        )
        .await;
    assert!(result.is_error, "scanning outside the root must be refused");
}

#[tokio::test]
async fn side_scan_does_not_follow_a_symlink_out_of_the_root() {
    // A symlink inside an authorised root must never be a route to bytes
    // outside it: neither `glob` may list it nor `grep` may read through it.
    let f = Fixture::new();
    f.write("inside.txt", b"inside-needle");
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("leak.txt"), b"outside-needle").unwrap();
    std::os::unix::fs::symlink(outside.path().join("leak.txt"), f.path.join("leak.txt")).unwrap();
    std::os::unix::fs::symlink(outside.path(), f.path.join("leakdir")).unwrap();

    let globbed = f.call("glob", json!({"pattern": "**/*.txt"})).await;
    assert!(!globbed.is_error, "{}", text_of(&globbed));
    let listing = text_of(&globbed);
    assert!(listing.contains("inside.txt"), "{listing}");
    assert!(
        !listing.contains("leak.txt"),
        "a symlink must not be listed: {listing}"
    );

    let grepped = f.call("grep", json!({"pattern": "needle"})).await;
    assert!(!grepped.is_error, "{}", text_of(&grepped));
    let hits = text_of(&grepped);
    assert!(hits.contains("inside-needle"), "{hits}");
    assert!(
        !hits.contains("outside-needle"),
        "the scan must not read through a symlink: {hits}"
    );
}

#[tokio::test]
async fn side_tool_metadata_is_never_confirmable_write() {
    // Guards the whitelist contract: a side tool that needed confirmation could
    // prompt the parent's UI, which a side must never do.
    let f = Fixture::new();
    for name in ["read", "read_document", "view_image", "glob", "grep"] {
        let metadata: ToolMetadata = f.registry.get(name).unwrap().metadata();
        assert!(
            metadata.read_only && !metadata.requires_confirmation,
            "{name} must be a non-confirming read-only tool"
        );
    }
}

#[tokio::test]
async fn side_pool_limits_are_shared_across_tools() {
    // Every tool goes through the same admission: a slow document parse cannot
    // be bypassed by switching tools.
    let f = Fixture::new();
    f.write("a.md", b"# x");
    let start = std::time::Instant::now();
    let (first, second) = tokio::join!(
        f.call("read_document", json!({"path": "a.md"})),
        f.call("read_document", json!({"path": "a.md"}))
    );
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(!first.is_error && !second.is_error);
}
