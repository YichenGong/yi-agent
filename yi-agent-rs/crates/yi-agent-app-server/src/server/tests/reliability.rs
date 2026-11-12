//! Lifecycle error preservation regressions for the reliability repair.
use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn failed_file_delete_retains_the_original_session_for_retry() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    let mut h = Harness::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT);
    let tid = start_thread(&mut h).await;
    let meta = dir
        .path()
        .join(".yi-agent/threads")
        .join(format!("{tid}.meta.json"));
    let backup = std::fs::read(&meta).unwrap();
    std::fs::remove_file(&meta).unwrap();
    std::fs::create_dir(&meta).unwrap();
    h.send(&serde_json::json!({"id":980,"method":"thread/delete","params":{"threadId":tid,"force":true}}).to_string()).await;
    let response = read_response(&mut h, 980).await;
    assert!(
        response.get("error").is_some(),
        "a failed delete must not report success: {response}"
    );
    std::fs::remove_dir(&meta).unwrap();
    std::fs::write(&meta, backup).unwrap();
    h.send(
        &serde_json::json!({"id":981,"method":"thread/resume","params":{"threadId":tid}})
            .to_string(),
    )
    .await;
    let response = read_response(&mut h, 981).await;
    assert!(
        response.get("error").is_none(),
        "failed deletion must resume the original driver: {response}"
    );
    h.shutdown().await;
}
