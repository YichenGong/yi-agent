//! 自述执行器的行为测试。
//!
//! 分三类：
//!
//! 1. **纯逻辑**（捕获上限、诊断截断、时间判据、回收强度）——不碰进程。
//! 2. **可控时钟**——用冻结时钟把状态机推到目标检查点，再推进到到期；每个测试
//!    同时断言「目标状态确实达到了」，否则冻结时钟会让测试空转成假绿。
//! 3. **真实进程**——fixture 假插件，验证组清理、EOF、背压等真实系统行为。

use std::path::Path;
use std::time::{Duration, Instant};

use super::process::ChildSession;
use super::{Capture, Checkpoint, DescribeError, DescribePolicy, describe_with_clock, drain_once};
use crate::refresh::fixture::{FakePlugin, PluginFixture};

// ---------------------------------------------------------------------------
// 1. 纯逻辑
// ---------------------------------------------------------------------------

#[test]
fn capture_accepts_exactly_the_limit_and_rejects_the_next_byte() {
    let limit = 1024;
    let mut capture = Capture::new(limit, true);
    capture
        .push(&vec![b'a'; limit])
        .expect("exactly the limit is allowed");
    assert_eq!(capture.bytes.len(), limit);
    assert!(!capture.truncated);
    let error = capture.push(b"x").expect_err("one more byte must fail");
    assert!(matches!(error, DescribeError::StdoutLimit(l) if l == limit));
}

#[test]
fn capture_with_a_soft_limit_truncates_and_keeps_reading() {
    let mut capture = Capture::new(64, false);
    capture.push(&[b'x'; 4096]).expect("soft limits never fail");
    assert_eq!(capture.bytes.len(), 64);
    assert!(capture.truncated);
    // 继续 push 不报错——stderr 超限后仍要排空，否则会堵住 stdout。
    capture.push(&[b'y'; 4096]).expect("draining continues");
    assert_eq!(capture.bytes.len(), 64);
}

#[test]
fn capture_text_is_strict_utf8() {
    let mut capture = Capture::new(16, true);
    capture.push(b"\xff").unwrap();
    assert!(matches!(capture.text(), Err(DescribeError::InvalidUtf8)));
}

#[test]
fn diagnostic_keeps_the_first_line_within_the_character_budget() {
    // 中文首行超长：按字符（不是字节）截断，标记计入 512。
    let long = "中".repeat(2000);
    let mut capture = Capture::new(8192, false);
    capture
        .push(format!("{long}\nsecond line\n").as_bytes())
        .unwrap();
    let diagnostic = capture.diagnostic();
    assert_eq!(diagnostic.chars().count(), 512);
    assert!(diagnostic.ends_with(" [truncated]"));
    assert!(diag_core(&diagnostic).chars().all(|c| c == '中'));
}

#[test]
fn diagnostic_of_an_empty_stderr_is_empty() {
    let capture = Capture::new(64, false);
    assert_eq!(capture.diagnostic(), "");
}

#[test]
fn diagnostic_of_a_truncated_capture_is_marked() {
    let mut capture = Capture::new(8, false);
    capture.push(&[b'z'; 64]).unwrap();
    let diagnostic = capture.diagnostic();
    assert!(diagnostic.ends_with(" [truncated]"));
    assert!(diagnostic.chars().count() <= 512);
}

#[test]
fn diagnostic_of_an_empty_first_line_after_truncation_is_just_the_marker() {
    let mut capture = Capture::new(4, false);
    capture.push(b"abcd").unwrap();
    capture.truncated = true;
    // 首行为空、但 capture 已截断：不能返回 " [truncated]" 这种前导空格。
    let mut empty = Capture::new(4, false);
    empty.truncated = true;
    assert_eq!(empty.diagnostic(), "[truncated]");
}

fn diag_core(diagnostic: &str) -> &str {
    diagnostic
        .strip_suffix(" [truncated]")
        .unwrap_or(diagnostic)
}

#[test]
fn limits_treat_the_exact_deadline_as_expired_and_cancel_as_priority() {
    let base = Instant::now();
    let timeout = Duration::from_millis(50);
    let deadline = base + timeout;
    // 恰好到达就是到期：不允许"刚好看了一眼"越过预算。
    assert!(matches!(
        super::check_limits(deadline, deadline, timeout, false),
        Err(DescribeError::Timeout(t)) if t == timeout
    ));
    assert!(super::check_limits(deadline, base, timeout, false).is_ok());
    // 同一检查点两者同时成立：取消优先。
    assert!(matches!(
        super::check_limits(deadline, deadline, timeout, true),
        Err(DescribeError::Cancelled)
    ));
}

#[test]
fn observed_exit_only_accepts_a_zero_code() {
    use super::ObservedExit;
    assert!(
        ObservedExit {
            code: Some(0),
            signal: None
        }
        .success()
    );
    assert!(
        !ObservedExit {
            code: Some(1),
            signal: None
        }
        .success()
    );
    assert!(
        !ObservedExit {
            code: None,
            signal: Some(9)
        }
        .success()
    );
    assert!(
        !ObservedExit {
            code: Some(0),
            signal: Some(9)
        }
        .success()
    );
}

#[test]
fn render_covers_every_error_variant_without_leaking_stdout() {
    let error = DescribeError::Timeout(Duration::from_secs(15));
    assert_eq!(
        error.render("plug"),
        "`plug manifest` did not answer within 15s"
    );
    let error = DescribeError::StdoutLimit(1024 * 1024);
    assert!(error.render("plug").contains("1048576"));
    let error = DescribeError::NonZero {
        exit: super::ObservedExit {
            code: Some(3),
            signal: None,
        },
        stderr: "boom".into(),
    };
    assert_eq!(error.render("plug"), "`plug manifest` failed: boom");
    let error = DescribeError::NonZero {
        exit: super::ObservedExit {
            code: None,
            signal: Some(9),
        },
        stderr: String::new(),
    };
    assert_eq!(
        error.render("plug"),
        "`plug manifest` failed: was killed by signal 9"
    );
    let error = DescribeError::WithCleanup {
        primary: Box::new(DescribeError::Timeout(Duration::from_secs(1))),
        cleanup: "could not signal".into(),
    };
    let rendered = error.render("plug");
    assert!(rendered.starts_with("`plug manifest` did not answer"));
    assert!(rendered.contains("cleanup failed: could not signal"));
    assert!(
        DescribeError::OwnershipLost
            .render("plug")
            .contains("ownership")
    );
    assert!(
        DescribeError::Cancelled
            .render("plug")
            .contains("cancelled")
    );
    assert!(DescribeError::InvalidUtf8.render("plug").contains("UTF-8"));
}

/// 回收强度表：决定「能不能阻塞 wait」。
#[test]
fn reap_mode_table() {
    use super::process::{ReapMode, reap_mode};
    assert_eq!(reap_mode(false, false, false), ReapMode::None);
    assert_eq!(reap_mode(false, true, true), ReapMode::None);
    assert_eq!(reap_mode(true, true, false), ReapMode::Blocking);
    assert_eq!(reap_mode(true, false, true), ReapMode::Blocking);
    assert_eq!(reap_mode(true, false, false), ReapMode::NonBlocking);
}

#[test]
fn drain_stops_at_the_round_budget() {
    let policy = DescribePolicy::shortened(Duration::from_secs(5));
    let mut source = std::io::Cursor::new(vec![b'x'; 1024 * 1024]);
    let mut capture = Capture::new(2 * 1024 * 1024, true);
    let drained = drain_once(&mut source, &mut capture, &policy).unwrap();
    assert_eq!(
        drained.bytes, policy.drain_budget_per_stream,
        "one round must not read past its budget"
    );
    assert!(!drained.eof);
}

// ---------------------------------------------------------------------------
// 2. 可控时钟（目标状态断言 + 真实 backstop）
// ---------------------------------------------------------------------------

/// 旧 API 的背压回归：256 KiB 合法清单必须成功。
#[test]
fn describe_drains_large_stdout_without_a_false_timeout() {
    let fixture = PluginFixture::new();
    let plugin = fixture.plugin("a", "small", 256 * 1024, 0);
    let before = std::fs::read(&plugin.stored).unwrap();
    let result =
        crate::refresh::refresh_from_plugins(&fixture.workdir().join(".yi-agent/supervisors"));
    assert_eq!(
        result,
        vec![crate::refresh::RefreshOutcome::Refreshed("a".into())]
    );
    let after = std::fs::read(&plugin.stored).unwrap();
    assert_ne!(after, before);
    let parsed =
        crate::manifest::SupervisorManifest::parse(std::str::from_utf8(&after).unwrap()).unwrap();
    assert!(parsed.stop_when_disabled);
    assert_eq!(parsed.args.last().unwrap().len(), 256 * 1024);
}

/// 先写 2 MiB stderr、再输出小清单：stderr 超过 capture 上限也不能阻断 stdout。
#[test]
fn describe_drains_stderr_after_its_capture_limit() {
    let fixture = PluginFixture::new();
    let plugin = fixture.plugin("a", "noisy", 0, 0);
    let before = std::fs::read(&plugin.stored).unwrap();
    let result =
        crate::refresh::refresh_from_plugins(&fixture.workdir().join(".yi-agent/supervisors"));
    assert_eq!(
        result,
        vec![crate::refresh::RefreshOutcome::Refreshed("a".into())]
    );
    let after = std::fs::read(&plugin.stored).unwrap();
    assert_ne!(after, before);
    let parsed =
        crate::manifest::SupervisorManifest::parse(std::str::from_utf8(&after).unwrap()).unwrap();
    assert!(parsed.stop_when_disabled);
    assert_eq!(parsed.name, "a");
}

/// 父进程已退出、后代仍持写端：必须按时到期，而不是等后代自己退出。
#[test]
fn describe_deadline_covers_inherited_pipes_after_exit() {
    let fixture = PluginFixture::new();
    let plugin = fixture.plugin("a", "inherited", 0, 0);
    let policy = DescribePolicy::shortened(Duration::from_millis(600));
    let base = Instant::now();
    let wall_start = Instant::now();
    let mut reached = false;
    let mut expired = false;
    let mut clock = |point| {
        if matches!(point, Checkpoint::AfterIo {
            observed_exit: true, stdout_eof, stderr_eof, ..
        } if !stdout_eof || !stderr_eof)
            && plugin.descendant_marker.exists()
        {
            reached = true;
            expired = true;
        }
        if wall_start.elapsed() >= Duration::from_secs(20) {
            expired = true;
        }
        if expired { base + policy.timeout } else { base }
    };
    let result = describe_with_clock(
        plugin.command.to_str().unwrap(),
        policy,
        &mut || false,
        &mut clock,
    );
    assert!(
        reached,
        "never reached an exited parent whose inherited pipes stayed open"
    );
    assert!(matches!(result, Err(DescribeError::Timeout(_))));
    // 真实进程组也被收口了：后代不能活在 fixture 目录里继续跑。
    assert_descendant_gone(&plugin);
}

/// 持续输出也不得饿死截止时间检查。
#[test]
fn describe_times_out_while_output_keeps_arriving() {
    let fixture = PluginFixture::new();
    let plugin = fixture.plugin("a", "continuous", 0, 0);
    let policy = DescribePolicy::shortened(Duration::from_millis(600));
    let base = Instant::now();
    let wall_start = Instant::now();
    let mut reached = false;
    let mut expired = false;
    let mut clock = |point| {
        if let Checkpoint::AfterIo { bytes_read, .. } = point
            && bytes_read > 0
        {
            reached = true;
            expired = true;
        }
        if wall_start.elapsed() >= Duration::from_secs(20) {
            expired = true;
        }
        if expired { base + policy.timeout } else { base }
    };
    let result = describe_with_clock(
        plugin.command.to_str().unwrap(),
        policy,
        &mut || false,
        &mut clock,
    );
    assert!(reached, "never observed arriving output before expiring");
    assert!(matches!(result, Err(DescribeError::Timeout(_))));
}

/// 两根流都 EOF、直接子进程仍活着：不能判成功。
#[test]
fn describe_does_not_succeed_while_the_child_is_live() {
    let fixture = PluginFixture::new();
    let plugin = fixture.plugin("a", "eof_live", 0, 0);
    let policy = DescribePolicy::shortened(Duration::from_millis(600));
    let base = Instant::now();
    let wall_start = Instant::now();
    let mut reached = false;
    let mut expired = false;
    let mut clock = |point| {
        if matches!(
            point,
            Checkpoint::AfterIo {
                observed_exit: false,
                stdout_eof: true,
                stderr_eof: true,
                ..
            }
        ) {
            reached = true;
            expired = true;
        }
        if wall_start.elapsed() >= Duration::from_secs(20) {
            expired = true;
        }
        if expired { base + policy.timeout } else { base }
    };
    let result = describe_with_clock(
        plugin.command.to_str().unwrap(),
        policy,
        &mut || false,
        &mut clock,
    );
    assert!(reached, "never reached both-EOF-with-live-child");
    assert!(matches!(result, Err(DescribeError::Timeout(_))));
}

/// 成功后清理完成时恰好到期：最终检查是成功的线性化点。
#[test]
fn describe_expiring_at_the_final_check_is_not_a_success() {
    let fixture = PluginFixture::new();
    let plugin = fixture.plugin("a", "small", 0, 0);
    let policy = DescribePolicy::shortened(Duration::from_secs(5));
    let base = Instant::now();
    let mut clock = |point| {
        if point == Checkpoint::Final {
            base + policy.timeout
        } else {
            base
        }
    };
    let result = describe_with_clock(
        plugin.command.to_str().unwrap(),
        policy,
        &mut || false,
        &mut clock,
    );
    assert!(matches!(result, Err(DescribeError::Timeout(_))));
}

/// 取消优先于到期：同一检查点两者都成立时是 Cancelled。
#[test]
fn describe_reports_cancellation_before_the_deadline() {
    let fixture = PluginFixture::new();
    let plugin = fixture.plugin("a", "small", 0, 0);
    let policy = DescribePolicy::shortened(Duration::from_secs(5));
    let base = Instant::now();
    let mut clock = |point| {
        if point == Checkpoint::Loop {
            base + policy.timeout
        } else {
            base
        }
    };
    let result = describe_with_clock(
        plugin.command.to_str().unwrap(),
        policy,
        &mut || true,
        &mut clock,
    );
    assert!(matches!(result, Err(DescribeError::Cancelled)));
}

/// 取消发生在子进程启动之后：资源同样被收口。
///
/// The clock is frozen until the child has actually written its marker, then it
/// expires the budget. Advancing on wall time instead made the test a race
/// against the machine: `spawn python3` under load can take longer than the five
/// second budget, so `Timeout` won and the assertion saw the wrong variant --
/// a red test that said nothing about the cancellation path. The 20 second
/// wall-clock backstop keeps a genuinely stuck child from hanging the suite.
#[test]
fn describe_cancels_and_cleans_up_a_running_child() {
    let fixture = PluginFixture::new();
    let plugin = fixture.plugin("a", "hang", 0, 0);
    let policy = DescribePolicy::shortened(Duration::from_secs(5));
    let base = Instant::now();
    let wall_start = Instant::now();
    let mut ran = false;
    let mut clock = |_| {
        if plugin.manifest_marker.exists() {
            ran = true;
        }
        if ran || wall_start.elapsed() >= Duration::from_secs(20) {
            base + policy.timeout
        } else {
            base
        }
    };
    let result = describe_with_clock(
        plugin.command.to_str().unwrap(),
        policy,
        &mut || plugin.manifest_marker.exists(),
        &mut clock,
    );
    assert!(ran, "the plugin never started before the backstop expired");
    assert!(matches!(result, Err(DescribeError::Cancelled)));
    assert!(plugin.manifest_marker.exists(), "the plugin did run");
    assert_process_gone(&plugin.manifest_marker);
}

/// 冻结时钟 + 真实墙钟兜底。
///
/// The budget here exists to stop a stuck child, not to grade how fast this
/// machine spawns `python3`. Advancing with `Instant::now()` made the assertion
/// race the machine: under load a spawn can outlast the budget, `Timeout` wins,
/// and the test reports a wrong-variant failure that says nothing about the
/// behaviour under test. Freezing the clock keeps `check_limits` satisfied until
/// the backstop fires, so only a genuinely hung child reaches the deadline.
fn backstop_clock(
    policy: &DescribePolicy,
    backstop: Duration,
) -> impl FnMut(Checkpoint) -> Instant {
    let base = Instant::now();
    let timeout = policy.timeout;
    let wall_start = Instant::now();
    move |_| {
        if wall_start.elapsed() >= backstop {
            base + timeout
        } else {
            base
        }
    }
}

// ---------------------------------------------------------------------------
// 3. 真实进程
// ---------------------------------------------------------------------------

/// stdout 超限拒绝，且原清单不被截断覆盖。
#[test]
fn describe_rejects_stdout_over_limit_without_rewriting() {
    let fixture = PluginFixture::new();
    let plugin = fixture.plugin("a", "small", 0, 0);
    // helper 的 print 会补一个换行：补齐到 limit-1 才是恰好 limit。
    fixture.set_manifest_stdout_bytes(&plugin, 4096);
    let policy = DescribePolicy::with_stdout_limit(Duration::from_secs(10), 4096);
    let mut clock = backstop_clock(&policy, Duration::from_secs(60));
    let exact = describe_with_clock(
        plugin.command.to_str().unwrap(),
        policy,
        &mut || false,
        &mut clock,
    );
    assert!(
        exact.is_ok(),
        "exactly the limit must be accepted: {exact:?}"
    );

    // 超出一字节：拒绝。
    fixture.set_manifest_stdout_bytes(&plugin, 4097);
    let over = describe_with_clock(
        plugin.command.to_str().unwrap(),
        policy,
        &mut || false,
        &mut clock,
    );
    assert!(
        matches!(over, Err(DescribeError::StdoutLimit(4096))),
        "unexpected result: {over:?}"
    );

    // 经由公开入口走生产的 1 MiB 硬上限：超一字节即 Skipped，磁盘字节不变。
    fixture.set_manifest_stdout_bytes(&plugin, 1024 * 1024 + 1);
    let before = std::fs::read(&plugin.stored).unwrap();
    let outcomes =
        crate::refresh::refresh_from_plugins(&fixture.workdir().join(".yi-agent/supervisors"));
    assert!(
        matches!(
            outcomes.as_slice(),
            [crate::refresh::RefreshOutcome::Skipped { name, reason }]
                if name == "a" && reason.contains("1048576")
        ),
        "unexpected outcomes: {outcomes:?}"
    );
    assert_eq!(std::fs::read(&plugin.stored).unwrap(), before);
}

/// 超时清理必须杀掉整个进程组，而不只是直接子进程。
///
/// Same frozen-clock shape as above: expire only once the descendant marker
/// proves the grandchild is running. A wall-clock budget made the assertion
/// depend on how fast `spawn` returns under load, which is not what this test
/// is about -- it is about the group kill.
#[test]
fn describe_cleanup_stops_its_process_group() {
    let fixture = PluginFixture::new();
    let plugin = fixture.plugin("a", "inherited", 0, 0);
    let policy = DescribePolicy::shortened(Duration::from_secs(5));
    let base = Instant::now();
    let wall_start = Instant::now();
    let mut started = false;
    let mut clock = |_| {
        if plugin.descendant_marker.exists() {
            started = true;
        }
        if started || wall_start.elapsed() >= Duration::from_secs(20) {
            base + policy.timeout
        } else {
            base
        }
    };
    let result = describe_with_clock(
        plugin.command.to_str().unwrap(),
        policy,
        &mut || false,
        &mut clock,
    );
    assert!(matches!(result, Err(DescribeError::Timeout(_))));
    assert!(
        plugin.descendant_marker.exists(),
        "the descendant must have started before we can assert it died"
    );
    assert_descendant_gone(&plugin);
}

/// 正常成功也要收口组内残存的后代：两流已关、后代仍活着时照样清理。
#[test]
fn describe_success_still_cleans_up_a_closed_survivor() {
    let fixture = PluginFixture::new();
    let plugin = fixture.plugin("a", "inherited_closed", 0, 0);
    let policy = DescribePolicy::shortened(Duration::from_secs(30));
    let mut clock = backstop_clock(&policy, Duration::from_secs(60));
    let result = describe_with_clock(
        plugin.command.to_str().unwrap(),
        policy,
        &mut || false,
        &mut clock,
    );
    assert!(
        result.is_ok(),
        "the parent produces a valid manifest: {result:?}"
    );
    assert!(plugin.descendant_marker.exists());
    assert_descendant_gone(&plugin);
}

/// 失败诊断有界：非零退出的首行被保留，第二行不进入 reason。
#[test]
fn describe_keeps_bounded_failure_diagnostics() {
    let fixture = PluginFixture::new();
    let plugin = fixture.plugin("a", "nonzero", 0, 3);
    let policy = DescribePolicy::shortened(Duration::from_secs(10));
    let mut clock = backstop_clock(&policy, Duration::from_secs(60));
    let error = describe_with_clock(
        plugin.command.to_str().unwrap(),
        policy,
        &mut || false,
        &mut clock,
    )
    .expect_err("a nonzero exit is a failure");
    let rendered = error.render(&plugin.command.display().to_string());
    // stderr 非空时诊断就是它的首行；退出码本身另有分类，不挤进这一行。
    assert!(
        rendered.contains("cancelled is only plugin text"),
        "{rendered}"
    );
    assert!(
        !rendered.contains("second line"),
        "only the first line survives: {rendered}"
    );
    assert!(matches!(error, DescribeError::NonZero { exit, .. } if exit.code == Some(3)));
}

/// 非法 UTF-8 的 stdout 是明确分类，不是解析错误。
#[test]
fn describe_rejects_non_utf8_stdout() {
    let fixture = PluginFixture::new();
    let plugin = fixture.plugin("a", "invalid_utf8", 0, 0);
    let policy = DescribePolicy::shortened(Duration::from_secs(10));
    let mut clock = backstop_clock(&policy, Duration::from_secs(60));
    let error = describe_with_clock(
        plugin.command.to_str().unwrap(),
        policy,
        &mut || false,
        &mut clock,
    )
    .expect_err("invalid UTF-8 must fail");
    assert!(matches!(error, DescribeError::InvalidUtf8));
}

/// 展开时 guard 走同一清理次序，且不二次 panic。
#[test]
fn describe_cleanup_runs_while_unwinding() {
    let fixture = PluginFixture::new();
    let plugin = fixture.plugin("a", "hang", 0, 0);
    let command = plugin.command.to_str().unwrap().to_string();
    let unwind = std::panic::catch_unwind(move || {
        let _session = ChildSession::spawn(&command).expect("spawn");
        assert!(wait_for(&plugin.manifest_marker, MARKER_BUDGET));
        panic!("injected unwind while holding the session");
    });
    assert!(unwind.is_err(), "the injected panic must propagate");
}

/// 成功 finish 之后 Drop 不得再发一次组信号。
#[test]
fn finish_is_idempotent_and_drop_does_not_signal_again() {
    let fixture = PluginFixture::new();
    let plugin = fixture.plugin("a", "hang", 0, 0);
    let mut session = ChildSession::spawn(plugin.command.to_str().unwrap()).expect("spawn");
    assert!(wait_for(&plugin.manifest_marker, MARKER_BUDGET));
    session.finish().expect("cleanup");
    let events_after_finish = super::process::debug_events(&session);
    drop(session);
    assert_eq!(
        events_after_finish,
        vec![
            super::process::Event::SignalGroup,
            super::process::Event::ClosePipes,
            super::process::Event::SignalChild,
            super::process::Event::DisarmGroup,
            super::process::Event::Reap,
        ],
        "group signal must precede closing pipes, reaping must come last"
    );
}

/// WNOWAIT：观察退出后 child 仍可 wait，回收后才 ECHILD。
#[test]
fn exit_observation_keeps_the_leader_waitable() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("exit7.sh");
    std::fs::write(&script, "#!/bin/sh\nexit 7\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut session = ChildSession::spawn(script.to_str().unwrap()).expect("spawn");
    // 回收前记下 PID：回收之后这个号在 ≤P_MAXPID 内不会被复用，探测才有意义。
    let pid = session.pid();
    let exit = wait_for_exit(&mut session);
    assert_eq!(exit.code, Some(7));
    assert_eq!(exit.signal, None);
    // 第二次观察返回同一份，而不是「没退出」。
    let again = session.observe().expect("observe").expect("still observed");
    assert_eq!(again, exit);
    // 观察之后、回收之前，原生 WNOWAIT 仍然能看到它——这就是"没有提前回收"。
    assert!(
        raw_waitid_still_waitable(pid),
        "WNOWAIT must keep it waitable"
    );
    session.finish().expect("reap");
    // 已经回收：原生 waitid 得到 ECHILD。
    assert_eq!(
        raw_waitid_errno(pid),
        Some(libc::ECHILD),
        "a reaped child must not remain waitable"
    );
}

#[test]
fn observing_a_running_child_reports_none() {
    let fixture = PluginFixture::new();
    let plugin = fixture.plugin("a", "hang", 0, 0);
    let mut session = ChildSession::spawn(plugin.command.to_str().unwrap()).expect("spawn");
    assert!(wait_for(&plugin.manifest_marker, MARKER_BUDGET));
    assert_eq!(
        session.observe().unwrap(),
        None,
        "a live child has no exit yet"
    );
    session.finish().expect("cleanup");
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// 观测「子进程已进入目标阶段」的预算。**宽 backstop，不是期望值**：全量测试
/// 并发跑时 python3 冷启动实测能超过 10 秒（2026-11-03 本机 nextest 并发下复现），
/// 把「机器有多空」写进断言只会制造假红。
const MARKER_BUDGET: Duration = Duration::from_secs(30);

fn wait_for(path: &Path, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

fn wait_for_exit(session: &mut ChildSession) -> super::ObservedExit {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Some(exit) = session.observe().expect("observe") {
            return exit;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("the child never exited");
}

/// 读 marker 里的 PID 只用于**观察**：用有界的 `ps` 轮询确认它已终止。
///
/// 绝不按 marker PID 盲目 `kill`——PID 复用会误杀无关进程。PID 复用最多让测试
/// 安全地失败（看到一个活着的新进程），不会伤害别人。
fn assert_process_gone(marker: &Path) {
    let pid = std::fs::read_to_string(marker)
        .expect("marker with a pid")
        .trim()
        .to_string();
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if !process_is_alive(&pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("process {pid} from {marker:?} is still alive");
}

fn assert_descendant_gone(plugin: &FakePlugin) {
    assert_process_gone(&plugin.descendant_marker);
}

/// `ps -p <pid> -o stat=`：不存在、或状态为 `Z`（僵尸）都算已终止。
fn process_is_alive(pid: &str) -> bool {
    let output = std::process::Command::new("ps")
        .args(["-p", pid, "-o", "stat="])
        .output()
        .expect("ps runs");
    if !output.status.success() {
        return false;
    }
    let stat = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stat.is_empty() {
        return false;
    }
    !stat.starts_with('Z')
}

/// `waitid(WNOWAIT)` 是否仍能看到这个 child 的退出（即尚未回收）。
fn raw_waitid_still_waitable(pid: libc::pid_t) -> bool {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: `pid` is this session's own, not yet reaped child; `info` is writable.
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    rc == 0 && unsafe { info.si_pid() } == pid
}

fn raw_waitid_errno(pid: libc::pid_t) -> Option<i32> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: probing a PID that no longer belongs to us; `info` is writable.
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if rc == 0 {
        None
    } else {
        std::io::Error::last_os_error().raw_os_error()
    }
}
