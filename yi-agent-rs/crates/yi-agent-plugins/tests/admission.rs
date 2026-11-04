//! 活动准入与排空屏障的行为测试。
//!
//! # 为什么 permit 的存活必须用真进程验证
//!
//! 同进程里"释放了没"和"锁还在不在"是两件不同的事：`Arc` 还在不等于锁
//! 还在，锁在也不等于进程还活着。设计明确禁止按 TTL 或 PID 判断，所以这里
//! 用真实子进程：一个正常退出（锁应由内核释放），一个 `SIGSTOP`（锁必须
//! **不**释放）。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use yi_agent_plugins::admission::Phase;
use yi_agent_plugins::{Admission, Binding, Effect, PluginError, PluginId, ProjectId, ReleaseId};

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
        activation: 1,
        project: ProjectId::parse(project).unwrap(),
        grant,
    }
}

// ---------------------------------------------------------------------------
// 排空期间：拒新工作，允许旧 turn 收尾
// ---------------------------------------------------------------------------

#[test]
fn a_draining_plugin_refuses_new_launches() {
    let admission = Admission::new(temp_home());
    admission
        .admit_launch(&binding("/p/a", 1))
        .expect("open admits launches");

    admission.begin_drain().unwrap();
    let error = admission
        .admit_launch(&binding("/p/a", 1))
        .expect_err("a draining plugin must refuse new launches");
    assert_eq!(error.kind(), "Draining", "got: {error}");
}

#[test]
fn a_draining_plugin_refuses_external_writes_but_allows_old_turn_writes() {
    // 这是整个排空设计的核心：拒绝外部写，但必须让旧 turn 收尾。
    // 若连旧 turn 也拒绝，屏障永远等不到零——它既不能写也不能释放。
    let home = temp_home();
    let admission = Admission::new(&home);
    let b = binding("/p/a", 1);

    let permit = admission.admit_turn("turn-1", b.clone()).expect("admit");
    admission.begin_drain().unwrap();

    let error = admission
        .admit_external_write("req-1", &b)
        .expect_err("external writes must be refused while draining");
    assert_eq!(error.kind(), "Draining", "got: {error}");

    admission
        .admit_turn_write("turn-1", &b, Effect::Write)
        .expect("an old turn must be able to wrap up while draining");

    drop(permit);
}

#[test]
fn a_draining_plugin_refuses_new_turns() {
    let admission = Admission::new(temp_home());
    admission.begin_drain().unwrap();
    let error = admission
        .admit_turn("turn-2", binding("/p/a", 1))
        .expect_err("a draining plugin must refuse new turns");
    assert_eq!(error.kind(), "Draining", "got: {error}");
}

#[test]
fn cancelling_a_drain_restores_admission_without_closing_the_plugin() {
    // 取消排空只是回到原状态，不是把插件关掉。
    let admission = Admission::new(temp_home());
    admission.begin_drain().unwrap();
    assert_eq!(admission.phase(), Phase::Draining);

    admission.cancel_drain().unwrap();
    assert_eq!(admission.phase(), Phase::Open);
    admission
        .admit_turn("turn-3", binding("/p/a", 1))
        .expect("admission is back");
    admission
        .admit_launch(&binding("/p/a", 1))
        .expect("launches are back");
}

// ---------------------------------------------------------------------------
// parent_turn 不是授权
// ---------------------------------------------------------------------------

#[test]
fn a_forged_turn_id_is_rejected() {
    // 只填一个长得像别人 turn ID 的字符串，不能收尾别人的回合。
    let admission = Admission::new(temp_home());
    let b = binding("/p/a", 1);
    let error = admission
        .admit_turn_write("turn-someone-else", &b, Effect::Write)
        .expect_err("an unknown turn id is not authorization");
    assert_eq!(error.kind(), "PermissionDenied", "got: {error}");
}

#[test]
fn a_finished_turn_cannot_start_another_round() {
    // turn 结束后旧凭据不能开下一轮：permit 已释放，锁不再被持有。
    let admission = Admission::new(temp_home());
    let b = binding("/p/a", 1);
    let permit = admission.admit_turn("turn-4", b.clone()).expect("admit");
    drop(permit);

    let error = admission
        .admit_turn_write("turn-4", &b, Effect::Write)
        .expect_err("a finished turn may not write again");
    assert_eq!(error.kind(), "PermissionDenied", "got: {error}");
}

#[test]
fn a_turn_from_another_project_cannot_wrap_up_here() {
    // binding 的项目不一致：别人项目里的 turn 不能替我这个项目收尾。
    let admission = Admission::new(temp_home());
    let permit = admission
        .admit_turn("turn-5", binding("/p/a", 1))
        .expect("admit");

    let error = admission
        .admit_turn_write("turn-5", &binding("/p/b", 1), Effect::Write)
        .expect_err("a turn belongs to its own project");
    assert_eq!(error.kind(), "PermissionDenied", "got: {error}");

    // 同一项目但授权代次不同，同样不是同一个作用域。
    let error = admission
        .admit_turn_write("turn-5", &binding("/p/a", 2), Effect::Write)
        .expect_err("a different grant generation is a different scope");
    assert_eq!(error.kind(), "PermissionDenied", "got: {error}");

    drop(permit);
}

#[test]
fn turn_write_only_covers_writes() {
    let admission = Admission::new(temp_home());
    let b = binding("/p/a", 1);
    let permit = admission.admit_turn("turn-6", b.clone()).expect("admit");
    let error = admission
        .admit_turn_write("turn-6", &b, Effect::Read)
        .expect_err("this entry point is for writes only");
    assert_eq!(error.kind(), "PermissionDenied", "got: {error}");
    drop(permit);
}

// ---------------------------------------------------------------------------
// 屏障计数
// ---------------------------------------------------------------------------

#[test]
fn the_barrier_wait_counts_only_live_before_barrier_permits() {
    let admission = Admission::new(temp_home());
    let a = admission
        .admit_turn("turn-a", binding("/p/a", 1))
        .expect("a");
    let b = admission
        .admit_turn("turn-b", binding("/p/b", 1))
        .expect("b");
    assert_eq!(admission.live_before_barrier_permits(), 2);

    drop(a);
    assert_eq!(
        admission.live_before_barrier_permits(),
        1,
        "a released permit must not keep the barrier waiting"
    );

    admission.begin_drain().unwrap();
    assert_eq!(admission.live_before_barrier_permits(), 1);

    drop(b);
    assert_eq!(admission.live_before_barrier_permits(), 0);
}

// ---------------------------------------------------------------------------
// 跨进程：崩溃释放、SIGSTOP 不释放
// ---------------------------------------------------------------------------

#[test]
fn a_crashed_process_releases_its_permit() {
    // 进程被 SIGKILL → 内核关闭描述符 → 锁释放。屏障必须能看到 0。
    let home = temp_home();
    let admission = Admission::new(&home);

    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "helper_child_holds_turn",
            "--nocapture",
            "--ignored",
        ])
        .env("YI_PLUGIN_HOME", &home)
        .spawn()
        .expect("spawn");

    // 等子进程真的拿到 permit：父进程用独立描述符试锁，拿不到说明被持有。
    wait_until(Duration::from_secs(10), || {
        !probe_lock_free(&home, "turn-child")
    });

    child.kill().expect("kill");
    let _ = child.wait();

    wait_until(Duration::from_secs(10), || {
        probe_lock_free(&home, "turn-child")
    });
    assert!(
        probe_lock_free(&home, "turn-child"),
        "after the holder dies the lock must be released by the kernel"
    );
    // 父进程自己的屏障计数不受影响（它没有登记这个 turn），但状态一致。
    assert_eq!(admission.live_before_barrier_permits(), 0);
}

#[test]
fn a_stopped_process_keeps_its_permit() {
    // `SIGSTOP` 不释放锁。任何"超过 N 秒没心跳就回收"的实现都会在这里
    // 把仍然活着的 permit 判成已结束——暂停的进程恢复后还会继续写。
    let home = temp_home();
    let mut child = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "helper_child_holds_turn",
            "--nocapture",
            "--ignored",
        ])
        .env("YI_PLUGIN_HOME", &home)
        .spawn()
        .expect("spawn");

    wait_until(Duration::from_secs(10), || {
        !probe_lock_free(&home, "turn-child")
    });

    let pid = child.id() as i32;
    unsafe { libc::kill(pid, libc::SIGSTOP) };

    // 停住之后仍然不能拿到锁。
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !probe_lock_free(&home, "turn-child"),
        "a stopped process must keep its permit; TTL-based reclamation would \
         treat a paused writer as finished"
    );

    unsafe { libc::kill(pid, libc::SIGCONT) };
    child.kill().expect("cleanup");
    let _ = child.wait();
}

#[test]
fn a_turn_waiting_on_approval_keeps_the_barrier_from_completing() {
    // 一个回合在等用户审批时仍然持有 permit：它还在"进行中"，
    // 只是暂时不产生新动作。drain 必须等它，不能因为"最近没有活动"
    // 就认为它可以被跨过去——审批回来后它还要继续写。
    let admission = Admission::new(temp_home());
    let permit = admission
        .admit_turn("turn-waiting", binding("/p/a", 1))
        .expect("admit");

    admission.begin_drain().unwrap();
    assert_eq!(
        admission.live_before_barrier_permits(),
        1,
        "a turn awaiting approval still holds its permit"
    );

    // 模拟很久没有活动。时间流逝本身不改变锁状态——这正是设计要求的：
    // 判定只能看锁，不能看静默时长。
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(admission.live_before_barrier_permits(), 1);

    drop(permit);
    assert_eq!(admission.live_before_barrier_permits(), 0);
}

#[test]
fn a_single_turn_can_wrap_up_several_writes_and_release_once() {
    // 一个回合在排空期常需要多次收尾写（写状态、写锁释放记录）。
    // 每次都必须被允许——要求"只准写一次"会让收尾卡在中间。
    let admission = Admission::new(temp_home());
    let b = binding("/p/a", 1);
    let permit = admission
        .admit_turn("turn-multi", b.clone())
        .expect("admit");
    admission.begin_drain().unwrap();

    for _ in 0..3 {
        admission
            .admit_turn_write("turn-multi", &b, Effect::Write)
            .expect("every wrap-up write must be admitted");
    }
    drop(permit);
}

#[test]
fn concurrent_barrier_establishment_is_idempotent() {
    // 两个管理员同时点"更新"会各建一次屏障。第二次不能把第一次的屏障
    // 状态推翻，也不能把已经在收尾的旧 turn 判成新工作。
    let admission = Admission::new(temp_home());
    let b = binding("/p/a", 1);
    let permit = admission.admit_turn("turn-old", b.clone()).expect("admit");

    admission.begin_drain().unwrap();
    admission.begin_drain().unwrap();
    assert_eq!(admission.phase(), Phase::Draining);
    assert_eq!(admission.live_before_barrier_permits(), 1);
    admission
        .admit_turn_write("turn-old", &b, Effect::Write)
        .expect("the old turn is still a wrap-up, not new work");
    drop(permit);
}

#[test]
fn two_processes_cannot_both_take_the_same_turn_permit() {
    let home = temp_home();
    let admission = Admission::new(&home);
    let _permit = admission
        .admit_turn("turn-shared", binding("/p/a", 1))
        .expect("admit");

    let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "--exact",
            "helper_child_tries_turn",
            "--nocapture",
            "--ignored",
        ])
        .env("YI_PLUGIN_HOME", &home)
        .env("YI_PLUGIN_TURN", "turn-shared")
        .output()
        .expect("spawn");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("CHILD: BUSY"),
        "another process must not take a held permit; got {stdout:?}"
    );
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    condition()
}

/// 用独立描述符试锁：能拿到说明没人持有。
fn probe_lock_free(home: &std::path::Path, turn: &str) -> bool {
    use std::os::unix::io::AsRawFd;
    let path = home
        .join("plugins")
        .join("permits")
        .join(format!("{turn}.permit"));
    let Ok(file) = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
    else {
        return true;
    };
    let outcome = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if outcome == 0 {
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        true
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// 子进程入口
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn helper_child_holds_turn() {
    let home = std::env::var("YI_PLUGIN_HOME").expect("home");
    let admission = Admission::new(&home);
    let _permit = admission
        .admit_turn("turn-child", binding("/p/a", 1))
        .expect("admit");
    println!("CHILD: HOLDING");
    // 一直持有直到被杀。
    loop {
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[ignore]
fn helper_child_tries_turn() {
    let home = std::env::var("YI_PLUGIN_HOME").expect("home");
    let turn = std::env::var("YI_PLUGIN_TURN").expect("turn");
    let admission = Admission::new(&home);
    match admission.admit_turn(&turn, binding("/p/a", 1)) {
        Ok(_) => println!("CHILD: GOT"),
        Err(PluginError::Busy(_)) => println!("CHILD: BUSY"),
        Err(other) => println!("CHILD: ERROR {other}"),
    }
}
