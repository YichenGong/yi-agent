//! Integration tests for the isolated plugin webview probe.
//!
//! These live outside the crate for the same reason the other desktop tests do:
//! building `src/lib.rs` as a test harness collides on `_EMBED_INFO_PLIST` with
//! the `tauri::generate_context!()` expansion, so the crate's own `--lib` target
//! is not testable. Everything exercised here is the production code path, not a
//! re-implementation.
#![cfg(all(target_os = "macos", feature = "plugin-probe"))]

use std::time::{Duration, Instant};

use desktop_lib::plugin_host::probe::{
    assemble_report, bootstrap_script, is_timed_out, iso8601_now, iso8601_now_of,
    navigation_allowed, replay_attempted, safe_relative_path, serve_asset, sha256_hex, AssetRoot,
    CaseReport, CaseStatus, ContainerConfig, ProbeChannel, ProbeReport, RecoveryPolicy,
    CASE_DEADLINE, EXPECTED_CASES,
};
use desktop_lib::plugin_host::run::{
    handle_request, judge_blocked_recovery, write_resume_marker, ProbeOptions, ProbeState,
    ISOLATION_SUITE, MAX_MESSAGE_BYTES,
};
use serde_json::json;
use std::path::PathBuf;

#[test]
fn the_navigation_gate_only_allows_the_probe_document() {
    assert!(navigation_allowed(
        "plugin-asset://localhost/index.html?gen=2"
    ));
    assert!(!navigation_allowed("https://example.com"));
    assert!(!navigation_allowed("http://localhost:1420"));
    assert!(!navigation_allowed("file:///etc/passwd"));
    assert!(!navigation_allowed("data:text/html,<script>1</script>"));
    assert!(!navigation_allowed("plugin-asset://evil.test/index.html"));
    assert!(!navigation_allowed("not a url"));
}

fn temp_pkg(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, body) in files {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }
    dir
}

#[test]
fn sha256_matches_a_known_value() {
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn traversal_and_absolute_paths_never_resolve() {
    let dir = temp_pkg(&[("ui/index.html", "<html></html>")]);
    let root = dir.path().canonicalize().unwrap();
    assert!(safe_relative_path(&root, "../outside").is_none());
    assert!(safe_relative_path(&root, "ui/../../outside").is_none());
    assert!(safe_relative_path(&root, "/etc/passwd").is_none());
    assert!(safe_relative_path(&root, "").is_none());
    assert!(safe_relative_path(&root, "ui/index.html").is_some());
}

#[test]
fn asset_root_skips_symlinks_that_leave_the_package() {
    let dir = temp_pkg(&[("ui/index.html", "<html></html>")]);
    let root = dir.path().canonicalize().unwrap();
    let outside = dir.path().parent().unwrap().join("outside-secret.txt");
    std::fs::write(&outside, "secret").unwrap();
    let link = root.join("ui/leak.txt");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let assets = AssetRoot::load(&root).unwrap();
    assert!(assets.get("ui/leak.txt").is_none());
    assert!(assets.get("ui/index.html").is_some());
    let _ = std::fs::remove_file(outside);
}

#[test]
fn cross_package_and_foreign_prefix_are_forbidden() {
    let dir = temp_pkg(&[("ui/index.html", "<html></html>")]);
    let root = dir.path().canonicalize().unwrap();
    let assets = AssetRoot::load(&root).unwrap();
    assert_eq!(serve_asset(&assets, "ui/index.html", "demo").status, 200);
    assert_eq!(
        serve_asset(&assets, "foreign/secret.txt", "demo").status,
        403
    );
    assert_eq!(
        serve_asset(&assets, "pkg-other/ui/index.html", "demo").status,
        403
    );
}

#[test]
fn absolute_request_paths_are_normalized_not_escaped() {
    let dir = temp_pkg(&[("ui/index.html", "<html></html>")]);
    let root = dir.path().canonicalize().unwrap();
    let assets = AssetRoot::load(&root).unwrap();
    assert_eq!(
        serve_asset(&assets, "plugin-asset://localhost/ui/index.html", "ui").status,
        200
    );
    assert_eq!(serve_asset(&assets, "/../../etc/passwd", "ui").status, 403);
}

#[test]
fn a_second_initialization_is_a_replay() {
    let channel = ProbeChannel::new();
    channel.register("frame-a", "n-1").unwrap();
    assert!(channel.mark_initialized("frame-a").is_ok());
    assert!(channel.mark_initialized("frame-a").is_err());
    assert_eq!(channel.frame("frame-a").unwrap().initialized, 2);
}

#[test]
fn a_frame_cannot_report_with_another_frames_nonce() {
    let channel = ProbeChannel::new();
    channel.register("frame-a", "n-1").unwrap();
    channel.register("frame-b", "n-2").unwrap();
    let impersonation = CaseReport {
        frame: "frame-b".to_string(),
        nonce: "n-1".to_string(),
        case: "native_invoke_denied".to_string(),
        passed: true,
        evidence: vec![],
    };
    assert!(channel.case_reported(&impersonation).is_err());
    assert!(channel.seen().is_empty());
}

#[test]
fn an_unknown_frame_is_rejected() {
    let channel = ProbeChannel::new();
    channel.register("frame-a", "n-1").unwrap();
    let ghost = CaseReport {
        frame: "frame-z".to_string(),
        nonce: "n-1".to_string(),
        case: "dom_read_denied".to_string(),
        passed: true,
        evidence: vec![],
    };
    assert!(channel.case_reported(&ghost).is_err());
}

#[test]
fn reporting_the_same_case_twice_is_rejected() {
    let channel = ProbeChannel::new();
    channel.register("frame-a", "n-1").unwrap();
    let report = CaseReport {
        frame: "frame-a".to_string(),
        nonce: "n-1".to_string(),
        case: "dom_read_denied".to_string(),
        passed: true,
        evidence: vec![],
    };
    assert!(channel.case_reported(&report).is_ok());
    assert!(channel.case_reported(&report).is_err());
}

#[test]
fn a_reported_failure_stays_a_failure() {
    let channel = ProbeChannel::new();
    channel.register("frame-a", "n-1").unwrap();
    let report = CaseReport {
        frame: "frame-a".to_string(),
        nonce: "n-1".to_string(),
        case: "native_invoke_denied".to_string(),
        passed: false,
        evidence: vec!["invoke resolved".to_string()],
    };
    let (_, status, evidence) = channel.case_reported(&report).unwrap();
    assert_eq!(status, CaseStatus::Fail);
    assert_eq!(evidence, vec!["invoke resolved".to_string()]);
}

#[test]
fn the_report_is_not_a_pass_when_any_case_is_missing() {
    let channel = ProbeChannel::new();
    channel.register("frame-a", "n-1").unwrap();
    let report = assemble_report(&channel, &[], iso8601_now(), "a".repeat(64), "b".repeat(64));
    assert!(!report.is_pass());
    assert_eq!(report.cases.len(), EXPECTED_CASES.len());
    assert!(report
        .cases
        .iter()
        .all(|case| case.status == CaseStatus::Fail));
}

#[test]
fn a_page_that_never_reports_still_yields_a_report() {
    // T02 Step 2 的负例：页面不执行任何 JS 时，原生侧必须自己在截止时间后
    // 把每个用例记成失败并落盘，而不是让进程在“没有窗口了”时静默退出、
    // 连一份报告都不留（那会让整个探针看起来“没跑过”，而不是“跑失败了”）。
    let channel = ProbeChannel::new();
    channel.register("frame-a", "n-1").unwrap();
    let timed_out: Vec<String> = EXPECTED_CASES.iter().map(|id| (*id).to_string()).collect();
    let mut report = assemble_report(
        &channel,
        &timed_out,
        iso8601_now(),
        "a".repeat(64),
        "b".repeat(64),
    );
    assert!(!report.is_pass());
    assert!(report
        .cases
        .iter()
        .all(|case| case.evidence.iter().any(|line| line.contains("deadline"))));

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("probe.json");
    report.write_atomic(&out).unwrap();
    let parsed: ProbeReport =
        serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
    assert!(parsed.atomic);
    assert_eq!(parsed.cases.len(), EXPECTED_CASES.len());
    assert!(!parsed.is_pass());
}

#[test]
fn a_timeout_is_recorded_as_a_failure() {
    let channel = ProbeChannel::new();
    let report = assemble_report(
        &channel,
        &["blocked_script_recovery".to_string()],
        iso8601_now(),
        "a".repeat(64),
        "b".repeat(64),
    );
    let case = report
        .cases
        .iter()
        .find(|case| case.id == "blocked_script_recovery")
        .unwrap();
    assert_eq!(case.status, CaseStatus::Fail);
    assert!(case.evidence.iter().any(|e| e.contains("deadline")));
}

#[test]
fn a_fully_reported_report_passes_and_is_written_atomically() {
    let channel = ProbeChannel::new();
    channel.register("frame-a", "n-1").unwrap();
    for id in EXPECTED_CASES {
        let report = CaseReport {
            frame: "frame-a".to_string(),
            nonce: "n-1".to_string(),
            case: (*id).to_string(),
            passed: true,
            evidence: vec!["ok".to_string()],
        };
        channel.case_reported(&report).unwrap();
    }
    let mut report = assemble_report(&channel, &[], iso8601_now(), "a".repeat(64), "b".repeat(64));
    assert!(report.is_pass(), "{:?}", report.cases);

    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("nested/probe.json");
    report.write_atomic(&out).unwrap();
    let raw = std::fs::read_to_string(&out).unwrap();
    let parsed: ProbeReport = serde_json::from_str(&raw).unwrap();
    assert!(parsed.atomic);
    assert_eq!(parsed.cases.len(), EXPECTED_CASES.len());
    // 写入后目录里只应有报告本身，临时文件已被 rename 掉。
    let names: Vec<_> = std::fs::read_dir(out.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names.len(), 1);
}

#[test]
fn the_container_config_digest_changes_with_the_csp() {
    let strict = ContainerConfig::default();
    let mut relaxed = strict.clone();
    relaxed.csp = "default-src *".to_string();
    assert_ne!(strict.digest(), relaxed.digest());
    assert_eq!(strict.digest().len(), 64);

    for directive in [
        "default-src",
        "script-src",
        "img-src",
        "form-action",
        "frame-src",
        "connect-src",
    ] {
        assert!(strict.csp.contains(directive), "csp lacks {directive}");
    }
    assert!(!strict.allow_remote_network);
    assert_eq!(strict.recovery, RecoveryPolicy::CloseAndRebuild);
}

#[test]
fn the_bootstrap_script_quotes_the_nonce() {
    let script = bootstrap_script("a\"b", "frame-a");
    assert!(script.contains(r#""a\"b""#), "{script}");
    assert!(script.contains("__YI_PLUGIN_PROBE__"));
}

#[test]
fn deadline_and_replay_predicates_are_strict() {
    let start = Instant::now();
    assert!(!is_timed_out(start, start));
    assert!(is_timed_out(
        start,
        start + CASE_DEADLINE + Duration::from_millis(1)
    ));
    assert!(!replay_attempted(1));
    assert!(replay_attempted(2));
}

#[test]
fn a_report_started_earlier_records_an_earlier_stamp() {
    // 报告的时长必须真的反映运行耗时：两枚时间戳若总相同，就无法判断这次
    // 运行是否撞上了截止时间。
    let started = Instant::now() - Duration::from_secs(2);
    let stamp = iso8601_now_of(started);
    let now = iso8601_now();
    assert!(stamp < now, "started {stamp} should precede {now}");
    assert_eq!(stamp.len(), 24, "{stamp}");
}

#[test]
fn iso_timestamps_are_ordered_and_shaped() {
    let stamp = iso8601_now();
    assert_eq!(stamp.len(), 24, "{stamp}");
    assert!(stamp.ends_with('Z'), "{stamp}");
    assert_eq!(&stamp[4..5], "-");
    assert_eq!(&stamp[10..11], "T");
}

fn assets() -> AssetRoot {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("index.html"), "<html></html>").unwrap();
    std::fs::write(dir.path().join("attacks.js"), "// attacks").unwrap();
    let root = dir.path().canonicalize().unwrap();
    let assets = AssetRoot::load(&root).unwrap();
    // tempdir 被 drop 后文件消失，但白名单保存的是绝对路径；测试只用路径形状。
    std::mem::forget(dir);
    assets
}

#[test]
fn probe_flags_are_required_together() {
    let parsed = ProbeOptions::from_args([
        "desktop".to_string(),
        "--plugin-probe-run".to_string(),
        "--output".to_string(),
        "/tmp/r.json".to_string(),
    ]);
    assert!(matches!(parsed, Some(Err(_))));
}

#[test]
fn a_normal_launch_is_not_a_probe() {
    assert!(ProbeOptions::from_args(["desktop".to_string()]).is_none());
}

#[test]
fn unknown_probe_flags_are_rejected() {
    let parsed = ProbeOptions::from_args([
        "desktop".to_string(),
        "--plugin-probe-run".to_string(),
        "--plugin-probe-whatever".to_string(),
    ]);
    assert!(matches!(parsed, Some(Err(_))));
}

#[test]
fn probe_options_parse() {
    let parsed = ProbeOptions::from_args([
        "desktop".to_string(),
        "--plugin-probe-run".to_string(),
        "--instance-root".to_string(),
        "/tmp/inst".to_string(),
        "--fixture-root".to_string(),
        "/tmp/fix".to_string(),
        "--output".to_string(),
        "/tmp/out.json".to_string(),
    ])
    .unwrap()
    .unwrap();
    assert_eq!(parsed.suite, ISOLATION_SUITE);
    assert_eq!(parsed.output, PathBuf::from("/tmp/out.json"));
}

#[test]
fn a_report_needs_the_matching_nonce() {
    let config = ContainerConfig::default();
    let state = ProbeState::new(
        assets(),
        config,
        PathBuf::from("/tmp/inst"),
        "n-1".to_string(),
    );
    state.channel.register("frame-1", "n-1").unwrap();
    let body = serde_json::to_vec(&json!({
        "frame": "frame-1", "nonce": "n-2",
        "case": "dom_read_denied", "passed": true, "evidence": []
    }))
    .unwrap();
    let (status, _, _) = handle_request(
        &state,
        "POST",
        "plugin-asset://localhost/__probe/report",
        &body,
    );
    assert_eq!(status, 403);
}

#[test]
fn an_oversized_report_is_refused() {
    let state = ProbeState::new(
        assets(),
        ContainerConfig::default(),
        PathBuf::from("/tmp/inst"),
        "n-1".to_string(),
    );
    let body = vec![b'x'; MAX_MESSAGE_BYTES + 1];
    let (status, _, _) = handle_request(
        &state,
        "POST",
        "plugin-asset://localhost/__probe/report",
        &body,
    );
    assert_eq!(status, 413);
}

#[test]
fn the_state_endpoint_reports_the_host_marker() {
    let dir = tempfile::tempdir().unwrap();
    write_resume_marker(dir.path(), 7).unwrap();
    let state = ProbeState::new(
        assets(),
        ContainerConfig::default(),
        dir.path().to_path_buf(),
        "n-1".to_string(),
    );
    let (status, _, body) =
        handle_request(&state, "GET", "plugin-asset://localhost/__probe/state", &[]);
    assert_eq!(status, 200);
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(value["marker"], 7);
    assert_eq!(value["generation"], 1);
}

#[test]
fn a_page_claim_of_recovery_is_not_evidence() {
    let (ok, evidence) = judge_blocked_recovery(true, 2, true, true, true);
    assert!(!ok);
    assert!(evidence
        .iter()
        .any(|line| line.contains("claimed recovery")));
    // 失败时也必须保留已经成立的事实，否则审阅者无法判断差在哪一步。
    assert!(evidence.iter().any(|line| line.contains("generation 2")));
}

#[test]
fn recovery_needs_all_four_facts() {
    let (ok, evidence) = judge_blocked_recovery(true, 2, true, true, false);
    assert!(ok);
    // 通过时同样要有可复核的证据，不能只有一句“通过”。
    assert!(evidence.iter().all(|line| !line.starts_with("FAIL")));
    assert!(evidence.iter().any(|line| line.contains("generation 2")));
    assert!(!judge_blocked_recovery(false, 2, true, true, false).0);
    assert!(!judge_blocked_recovery(true, 1, true, true, false).0);
    assert!(!judge_blocked_recovery(true, 2, false, true, false).0);
    assert!(!judge_blocked_recovery(true, 2, true, false, false).0);
}

#[test]
fn static_assets_are_served_from_the_whitelist_only() {
    let state = ProbeState::new(
        assets(),
        ContainerConfig::default(),
        PathBuf::from("/tmp/inst"),
        "n-1".to_string(),
    );
    let (status, content_type, body) =
        handle_request(&state, "GET", "plugin-asset://localhost/index.html", &[]);
    assert_eq!(status, 200);
    assert!(content_type.starts_with("text/html"));
    assert_eq!(body, b"<html></html>".to_vec());
    let (status, _, _) = handle_request(&state, "GET", "plugin-asset://localhost/../secret", &[]);
    assert_ne!(status, 200);
}

#[test]
fn writes_to_the_report_endpoint_are_validated_not_trusted() {
    let state = ProbeState::new(
        assets(),
        ContainerConfig::default(),
        PathBuf::from("/tmp/inst"),
        "n-1".to_string(),
    );
    let (status, _, _) = handle_request(
        &state,
        "POST",
        "plugin-asset://localhost/__probe/report",
        b"not json",
    );
    assert_eq!(status, 400);
    assert!(state.channel.seen().is_empty());
}

#[test]
fn a_blocking_announcement_is_observed_by_the_native_side() {
    let state = ProbeState::new(
        assets(),
        ContainerConfig::default(),
        PathBuf::from("/tmp/inst"),
        "n-1".to_string(),
    );
    state.channel.register("frame-1", "n-1").unwrap();
    let body = serde_json::to_vec(&json!({
        "frame": "frame-1", "nonce": "n-1",
        "case": "blocked_script_recovery", "passed": true,
        "evidence": ["blocking"]
    }))
    .unwrap();
    let (status, _, _) = handle_request(
        &state,
        "POST",
        "plugin-asset://localhost/__probe/report",
        &body,
    );
    assert_eq!(status, 200);
    assert!(*state.blocking_seen.lock().unwrap());
}
