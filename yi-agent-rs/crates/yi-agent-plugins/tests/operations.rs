//! 幂等操作台账的行为测试。
//!
//! # 这些测试在防什么
//!
//! 写操作不得因超时、断线或面板重载自动重放（设计 §8）。所以「同 ID 同参数
//! 返回原结果」和「同 ID 改参数返回 Conflict」必须**分别**有测试：只测前者，
//! 一个「无脑返回原结果」的实现也能通过，而那会把第二次副作用吞掉而不报错。
//!
//! 另一半是「读不留日志」：给它留日志会让台账随一次性的读无限增长。

use std::path::PathBuf;

use serde_json::json;
use yi_agent_plugins::operations::OperationOutcome;
use yi_agent_plugins::{
    Binding, Effect, Operation, OperationLedger, PluginId, ProjectId, ReleaseId,
};

fn temp_home() -> PathBuf {
    tempfile::TempDir::new().expect("temp").keep().join("home")
}

fn binding(project: &str, grant: u64) -> Binding {
    Binding {
        host_id: "host-1".to_string(),
        release: ReleaseId {
            plugin: PluginId::parse("example-counter").unwrap(),
            version: "1.4.0".to_string(),
            target: "darwin-aarch64".to_string(),
            sha256: "a".repeat(64),
        },
        activation: 7,
        project: ProjectId::parse(project).unwrap(),
        grant,
    }
}

fn operation(request_id: &str, binding: Binding, effect: Effect) -> Operation {
    Operation {
        request_id: request_id.to_string(),
        binding,
        effect,
        parent_turn: None,
    }
}

// ---------------------------------------------------------------------------
// 跨进程串行
// ---------------------------------------------------------------------------

#[test]
fn two_processes_beginning_different_requests_do_not_lose_an_update() {
    // 单进程测试看不到这个：两个进程各自 read → 各自 push → 各自 write，
    // 后写者把先写者的条目整个抹掉。台账"少一条"在这里意味着崩溃重启后
    // 那条请求看起来从没发生过，会被当作新请求重放——正是 Unknown 机制
    // 要防的重复副作用。
    //
    // 两个子进程用**显式共同起跑线**。先后 spawn 会让第一个跑完再放锁，
    // 第二个独自成功，测试什么都没证明。
    let home = temp_home();
    let start_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 400;

    let spawn = |request: &str| {
        std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "helper_child_begins_a_request",
                "--nocapture",
                "--ignored",
            ])
            .env("YI_PLUGIN_HOME", &home)
            .env("YI_PLUGIN_REQUEST_ID", request)
            .env("YI_PLUGIN_START_AT_MS", start_at.to_string())
            .spawn()
            .expect("spawn")
    };

    let mut a = spawn("req-alpha");
    let mut b = spawn("req-beta");
    let status_a = a.wait().expect("wait a");
    let status_b = b.wait().expect("wait b");
    assert!(status_a.success(), "child a failed: {status_a:?}");
    assert!(status_b.success(), "child b failed: {status_b:?}");

    // 两条都要在。丢一条说明 `begin` 的读改写没有跨进程串行。
    let ledger = OperationLedger::open(&home);
    for request in ["req-alpha", "req-beta"] {
        ledger
            .result(request, &binding("/p/a", 1))
            .unwrap_or_else(|error| {
                panic!(
                    "`{request}` was lost; two processes overwrote each other's ledger edits                      because `begin` did not hold the cross-process lock: {error}"
                )
            });
    }
}

#[test]
#[ignore]
fn helper_child_begins_a_request() {
    let home = std::env::var("YI_PLUGIN_HOME").expect("home");
    let request = std::env::var("YI_PLUGIN_REQUEST_ID").expect("request id");

    if let Ok(raw) = std::env::var("YI_PLUGIN_START_AT_MS") {
        let target: u64 = raw.parse().expect("start_at");
        loop {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;
            if now >= target {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    let ledger = OperationLedger::open(&home);
    let op = Operation {
        request_id: request,
        binding: Binding {
            host_id: "host-1".to_string(),
            release: ReleaseId {
                plugin: PluginId::parse("example-counter").unwrap(),
                version: "1.4.0".to_string(),
                target: "darwin-aarch64".to_string(),
                sha256: "a".repeat(64),
            },
            activation: 7,
            project: ProjectId::parse("/p/a").unwrap(),
            grant: 1,
        },
        effect: Effect::Write,
        parent_turn: None,
    };
    match ledger.begin(&op, "counters/increment", &json!({"by": 1})) {
        Ok(_) => std::process::exit(0),
        Err(error) => {
            eprintln!("begin failed: {error}");
            std::process::exit(5);
        }
    }
}

// ---------------------------------------------------------------------------
// 幂等
// ---------------------------------------------------------------------------

#[test]
fn the_first_call_is_not_a_replay() {
    let ledger = OperationLedger::open(temp_home());
    let op = operation("req-1", binding("/p/a", 1), Effect::Write);
    let outcome = ledger
        .begin(&op, "counters/increment", &json!({"by": 1}))
        .expect("begin");
    assert!(outcome.is_none(), "the first call must actually execute");
}

#[test]
fn the_same_request_with_the_same_params_replays_the_recorded_result() {
    let ledger = OperationLedger::open(temp_home());
    let op = operation("req-2", binding("/p/a", 1), Effect::Write);

    ledger
        .begin(&op, "counters/increment", &json!({"by": 1}))
        .expect("begin");
    ledger
        .finish(
            &op,
            OperationOutcome::Committed {
                result: json!({"count": 5}),
            },
        )
        .expect("finish");

    let replay = ledger
        .begin(&op, "counters/increment", &json!({"by": 1}))
        .expect("replay");
    assert_eq!(
        replay,
        Some(OperationOutcome::Committed {
            result: json!({"count": 5})
        }),
        "a retry must return the original result, not execute again"
    );
}

#[test]
fn parameter_key_order_does_not_make_a_different_request() {
    // JSON 的键序不是语义。若按原始序列化结果比较摘要，
    // `{a,b}` 与 `{b,a}` 会被判成"改了参数"并返回 Conflict——同一个请求
    // 因为一次无害的重排被永久拒绝。
    let ledger = OperationLedger::open(temp_home());
    let op = operation("req-3", binding("/p/a", 1), Effect::Write);

    ledger
        .begin(&op, "counters/increment", &json!({"by": 1, "label": "x"}))
        .expect("begin");
    let replay = ledger
        .begin(&op, "counters/increment", &json!({"label": "x", "by": 1}))
        .expect("begin again");
    assert!(
        replay.is_some(),
        "key order must not change request identity"
    );
}

#[test]
fn serde_json_maps_are_sorted_so_key_order_is_already_canonical() {
    // 这条不是测我们的代码，是测**我们的前提**。
    //
    // `params_digest` 里有一层显式的键排序。它今天是多余的：本 crate 没有
    // 启用 `serde_json/preserve_order`，`Map` 就是 `BTreeMap`。若哪天有人
    // 为了别的功能开启 `preserve_order`，这个前提就没了，而显式排序会从
    // "第二道保险"变成"唯一一道"。这条测试就是那一刻的报警器。
    let value: serde_json::Value = serde_json::from_str(r#"{"b": 2, "a": 1, "c": 3}"#).unwrap();
    let keys: Vec<&String> = value.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        vec!["a", "b", "c"],
        "serde_json::Map is expected to be sorted; if this fails, `preserve_order` \
         was enabled and the explicit canonicalisation in `params_digest` is now \
         the only thing keeping key order irrelevant"
    );
}

#[test]
fn the_same_request_id_with_different_params_is_a_conflict() {
    let ledger = OperationLedger::open(temp_home());
    let op = operation("req-4", binding("/p/a", 1), Effect::Write);

    ledger
        .begin(&op, "counters/increment", &json!({"by": 1}))
        .expect("begin");
    let error = ledger
        .begin(&op, "counters/increment", &json!({"by": 2}))
        .expect_err("changed params must be refused");
    assert_eq!(error.kind(), "Conflict", "got: {error}");
}

#[test]
fn the_same_request_id_with_a_different_method_is_a_conflict() {
    let ledger = OperationLedger::open(temp_home());
    let op = operation("req-5", binding("/p/a", 1), Effect::Write);

    ledger
        .begin(&op, "counters/increment", &json!({"by": 1}))
        .expect("begin");
    let error = ledger
        .begin(&op, "counters/reset", &json!({"by": 1}))
        .expect_err("a different method under the same id must be refused");
    assert_eq!(error.kind(), "Conflict", "got: {error}");
}

#[test]
fn array_order_still_matters() {
    // 对象键序无关，但数组顺序是语义：`[a,b]` 与 `[b,a]` 是两个不同请求。
    let ledger = OperationLedger::open(temp_home());
    let op = operation("req-6", binding("/p/a", 1), Effect::Write);

    ledger
        .begin(&op, "counters/apply", &json!({"items": ["a", "b"]}))
        .expect("begin");
    let error = ledger
        .begin(&op, "counters/apply", &json!({"items": ["b", "a"]}))
        .expect_err("array order is part of the request");
    assert_eq!(error.kind(), "Conflict", "got: {error}");
}

#[test]
fn a_different_project_is_a_different_request() {
    // 同一 request ID 在两个项目里是两件事：项目作用域不同，副作用不同。
    let ledger = OperationLedger::open(temp_home());
    let a = operation("req-7", binding("/p/a", 1), Effect::Write);
    let b = operation("req-7", binding("/p/b", 1), Effect::Write);

    assert!(
        ledger
            .begin(&a, "counters/increment", &json!({}))
            .expect("a")
            .is_none()
    );
    assert!(
        ledger
            .begin(&b, "counters/increment", &json!({}))
            .expect("b")
            .is_none(),
        "the same request id in another project must be a new request"
    );
}

// ---------------------------------------------------------------------------
// 读不留日志
// ---------------------------------------------------------------------------

#[test]
fn reads_do_not_leave_a_permanent_log() {
    let home = temp_home();
    let ledger = OperationLedger::open(&home);

    for i in 0..50 {
        let op = operation(&format!("read-{i}"), binding("/p/a", 1), Effect::Read);
        assert!(
            ledger
                .begin(&op, "counters/read", &json!({}))
                .unwrap()
                .is_none(),
            "reads always execute; they are safely retryable"
        );
    }
    assert!(
        !home.join("plugins/operations.json").exists(),
        "reads must not create an operation log; it would grow without bound"
    );
}

#[test]
fn finishing_a_read_does_not_create_a_log_either() {
    let home = temp_home();
    let ledger = OperationLedger::open(&home);
    let op = operation("read-1", binding("/p/a", 1), Effect::Read);
    ledger.begin(&op, "counters/read", &json!({})).unwrap();
    ledger
        .finish(&op, OperationOutcome::Committed { result: json!({}) })
        .expect("finishing a read is a no-op");
    assert!(!home.join("plugins/operations.json").exists());
}

// ---------------------------------------------------------------------------
// Unknown：不重放
// ---------------------------------------------------------------------------

#[test]
fn a_begun_but_unfinished_write_is_unknown() {
    // begin 之后进程崩溃：重启不能把它当成"没执行过"而重放。
    let home = temp_home();
    let ledger = OperationLedger::open(&home);
    let op = operation("req-8", binding("/p/a", 1), Effect::Write);
    ledger
        .begin(&op, "counters/increment", &json!({"by": 1}))
        .unwrap();

    let outcome = ledger
        .result("req-8", &op.binding)
        .expect("the record must survive a crash");
    assert!(
        matches!(outcome, OperationOutcome::Unknown { .. }),
        "an in-flight write must read as Unknown, got {outcome:?}"
    );
    assert_eq!(outcome.kind(), "Unknown");
}

#[test]
fn a_replay_of_an_unknown_request_returns_unknown_not_a_fresh_start() {
    // 重连时客户端重试同 ID：必须拿到 Unknown，而不是"这是新请求，开始执行"。
    // 后者会在副作用可能已经发生的情况下再执行一次。
    let home = temp_home();
    let ledger = OperationLedger::open(&home);
    let op = operation("req-9", binding("/p/a", 1), Effect::Write);
    ledger.begin(&op, "counters/increment", &json!({})).unwrap();

    let replay = ledger.begin(&op, "counters/increment", &json!({})).unwrap();
    assert_eq!(replay.map(|o| o.kind()), Some("Unknown"));
}

// ---------------------------------------------------------------------------
// 结果查询与业务准入分离
// ---------------------------------------------------------------------------

#[test]
fn a_result_cannot_be_read_through_another_project_binding() {
    // 切换项目后不能取另一个项目的结果（G1 §6.3）。
    let ledger = OperationLedger::open(temp_home());
    let op = operation("req-10", binding("/p/a", 1), Effect::Write);
    ledger.begin(&op, "counters/increment", &json!({})).unwrap();
    ledger
        .finish(
            &op,
            OperationOutcome::Committed {
                result: json!({"count": 1}),
            },
        )
        .unwrap();

    let error = ledger
        .result("req-10", &binding("/p/b", 1))
        .expect_err("another project must not see this result");
    assert_eq!(error.kind(), "NotFound", "got: {error}");

    let error = ledger
        .result("req-10", &binding("/p/a", 2))
        .expect_err("another grant generation must not see this result either");
    assert_eq!(error.kind(), "NotFound", "got: {error}");
}

#[test]
fn a_failed_outcome_is_recorded_and_replayed() {
    let ledger = OperationLedger::open(temp_home());
    let op = operation("req-11", binding("/p/a", 1), Effect::Write);
    ledger.begin(&op, "counters/increment", &json!({})).unwrap();
    ledger
        .finish(
            &op,
            OperationOutcome::Failed {
                kind: "PermissionDenied".to_string(),
                message: "not allowed".to_string(),
            },
        )
        .unwrap();

    let replay = ledger.begin(&op, "counters/increment", &json!({})).unwrap();
    assert_eq!(replay.map(|o| o.kind()), Some("Failed"));
}

#[test]
fn finishing_a_request_that_was_never_begun_is_rejected() {
    let ledger = OperationLedger::open(temp_home());
    let op = operation("req-never", binding("/p/a", 1), Effect::Write);
    let error = ledger
        .finish(&op, OperationOutcome::Committed { result: json!({}) })
        .expect_err("recording a result without a begin must be refused");
    assert_eq!(error.kind(), "NotFound", "got: {error}");
}

// ---------------------------------------------------------------------------
// 坏台账
// ---------------------------------------------------------------------------

#[test]
fn a_corrupt_ledger_is_reported_rather_than_treated_as_empty() {
    // 把损坏当空台账，等于"所有写都没执行过"，于是崩溃恢复会重放
    // 用户已经看到成功的写。
    let home = temp_home();
    std::fs::create_dir_all(home.join("plugins")).unwrap();
    std::fs::write(home.join("plugins/operations.json"), b"{ broken").unwrap();

    let ledger = OperationLedger::open(&home);
    let op = operation("req-12", binding("/p/a", 1), Effect::Write);
    let error = ledger
        .begin(&op, "counters/increment", &json!({}))
        .expect_err("a corrupt ledger must be reported");
    assert_eq!(error.kind(), "RecoveryRequired", "got: {error}");
    assert_eq!(
        std::fs::read_to_string(home.join("plugins/operations.json")).unwrap(),
        "{ broken",
        "the corrupt ledger must be left as found"
    );
}

#[test]
fn the_ledger_survives_a_round_trip() {
    let home = temp_home();
    {
        let ledger = OperationLedger::open(&home);
        let op = operation("req-13", binding("/p/a", 1), Effect::Write);
        ledger
            .begin(&op, "counters/increment", &json!({"by": 3}))
            .unwrap();
        ledger
            .finish(
                &op,
                OperationOutcome::Committed {
                    result: json!({"count": 3}),
                },
            )
            .unwrap();
    }
    let reopened = OperationLedger::open(&home);
    let outcome = reopened
        .result("req-13", &binding("/p/a", 1))
        .expect("the record must persist");
    assert_eq!(
        outcome,
        OperationOutcome::Committed {
            result: json!({"count": 3})
        }
    );
}
