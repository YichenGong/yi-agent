//! 协议与幂等的验收：这些断言先于桌面 UI 存在，所以"页面能跑"不能掩盖
//! "后端其实没有这条方法"。

use std::io::Write;
use std::process::{Command, Stdio};

use example_counter_backend::{Counter, Ledger, Request, Response};

fn request(id: &str, method: &str, params: serde_json::Value) -> Request {
    Request {
        request_id: id.to_string(),
        method: method.to_string(),
        params,
    }
}

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().expect("temp dir")
}

#[test]
fn health_and_describe_have_no_side_effects() {
    let dir = tempdir();
    let mut counter = Counter::open(dir.path()).expect("open");

    let health = counter.handle(&request("r1", "counters/health", serde_json::json!({})));
    match health {
        Response::Ok { result, .. } => {
            assert_eq!(result["ok"], serde_json::json!(true));
            assert_eq!(result["dataFormat"], serde_json::json!(1));
        }
        other => panic!("health failed: {other:?}"),
    }

    let describe = counter.handle(&request("r2", "counters/describe", serde_json::json!({})));
    match describe {
        Response::Ok { result, .. } => {
            assert_eq!(result["storesUnderManagedData"], serde_json::json!(true));
            let methods = result["methods"].as_array().expect("methods array");
            assert!(methods.iter().any(|m| m == "counters/increment"));
        }
        other => panic!("describe failed: {other:?}"),
    }

    // 读类方法不能创建计数文件。
    assert!(
        !dir.path().join("count.json").exists(),
        "Read methods must not write"
    );
    assert_eq!(counter.total(), 0);
}

#[test]
fn increment_persists_under_the_managed_data_dir() {
    let dir = tempdir();
    {
        let mut counter = Counter::open(dir.path()).expect("open");
        let response = counter.handle(&request(
            "r1",
            "counters/increment",
            serde_json::json!({ "by": 2 }),
        ));
        match response {
            Response::Ok { result, .. } => assert_eq!(result["total"], serde_json::json!(2)),
            other => panic!("increment failed: {other:?}"),
        }
    }

    // 新进程视角：重新打开同一目录能读回。
    let reopened = Counter::open(dir.path()).expect("reopen");
    assert_eq!(reopened.total(), 2);
    assert!(dir.path().join("count.json").exists());
}

#[test]
fn unknown_method_is_named_as_not_supported() {
    let dir = tempdir();
    let mut counter = Counter::open(dir.path()).expect("open");
    match counter.handle(&request(
        "r1",
        "counters/deleteEverything",
        serde_json::json!({}),
    )) {
        Response::Err { kind, detail, .. } => {
            assert_eq!(kind, "NotSupported");
            assert!(detail.contains("counters/deleteEverything"));
        }
        other => panic!("expected NotSupported, got {other:?}"),
    }
}

#[test]
fn the_same_request_id_with_the_same_params_returns_the_original_result() {
    let dir = tempdir();
    let mut counter = Counter::open(dir.path()).expect("open");
    let mut ledger = Ledger::default();

    let first = request(
        "r1",
        "counters/increment",
        serde_json::json!({ "by": 1, "note": "a" }),
    );
    let response = counter.handle(&first);
    let settled = ledger.settle(&first, response);
    assert_eq!(counter.total(), 1);

    // 键序不同、语义相同：必须被识别为重试，不能真的再加一次。
    //
    // 注意判定顺序：先查台账，命中就不再执行。先执行再比对就晚了——写已经
    // 落盘，冲突检查只剩下"报告已发生的事"。
    let retry = request(
        "r1",
        "counters/increment",
        serde_json::json!({ "note": "a", "by": 1 }),
    );
    let replay = match ledger.lookup(&retry) {
        Some(settled) => settled,
        None => ledger.settle(&retry, counter.handle(&retry)),
    };
    assert_eq!(counter.total(), 1, "a retry must not execute twice");
    assert_eq!(replay, settled);
}

#[test]
fn the_same_request_id_with_different_params_is_a_conflict() {
    let dir = tempdir();
    let mut counter = Counter::open(dir.path()).expect("open");
    let mut ledger = Ledger::default();

    let first = request("r1", "counters/increment", serde_json::json!({ "by": 1 }));
    let response = counter.handle(&first);
    ledger.settle(&first, response);

    let changed = request("r1", "counters/increment", serde_json::json!({ "by": 5 }));
    let refusal = match ledger.lookup(&changed) {
        Some(settled) => settled,
        None => ledger.settle(&changed, counter.handle(&changed)),
    };
    match refusal {
        Response::Err { kind, .. } => assert_eq!(kind, "Conflict"),
        other => panic!("expected Conflict, got {other:?}"),
    }
    assert_eq!(counter.total(), 1, "a conflicting request must not execute");
    assert_eq!(ledger.len(), 1);
}

#[test]
fn cli_mode_uses_the_same_counter() {
    let dir = tempdir();
    let bin = env!("CARGO_BIN_EXE_example-counter-backend");

    let status = Command::new(bin)
        .args(["--data-dir", dir.path().to_str().unwrap(), "--", "status"])
        .output()
        .expect("run status");
    assert!(status.status.success(), "status failed: {status:?}");
    let body: serde_json::Value = serde_json::from_slice(&status.stdout).expect("json");
    assert_eq!(body["total"], serde_json::json!(0));

    let increment = Command::new(bin)
        .args([
            "--data-dir",
            dir.path().to_str().unwrap(),
            "--",
            "increment",
            "--by",
            "3",
        ])
        .output()
        .expect("run increment");
    assert!(
        increment.status.success(),
        "increment failed: {increment:?}"
    );
    let body: serde_json::Value = serde_json::from_slice(&increment.stdout).expect("json");
    assert_eq!(body["total"], serde_json::json!(3));
}

#[test]
fn stdio_mode_answers_each_line_and_rejects_a_reused_id() {
    let dir = tempdir();
    let bin = env!("CARGO_BIN_EXE_example-counter-backend");

    let mut child = Command::new(bin)
        .args(["--data-dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");

    let mut stdin = child.stdin.take().expect("stdin");
    writeln!(
        stdin,
        "{}",
        serde_json::json!({ "request_id": "r1", "method": "counters/read", "params": {} })
    )
    .unwrap();
    writeln!(
        stdin,
        "{}",
        serde_json::json!({ "request_id": "r1", "method": "counters/read", "params": { "x": 1 } })
    )
    .unwrap();
    writeln!(stdin, "not json at all").unwrap();
    drop(stdin);

    let output = child.wait_with_output().expect("wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3, "one response per input line, got: {stdout}");

    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(first["status"], "ok");
    let conflict: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(conflict["status"], "err");
    assert_eq!(conflict["kind"], "Conflict");
    let garbage: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
    assert_eq!(garbage["status"], "err");
}
