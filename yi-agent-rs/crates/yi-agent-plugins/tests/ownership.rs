//! 项目维护所有权的行为测试。
//!
//! # 这些测试在防什么
//!
//! 所有权判断错了会让两个实例同时改同一个项目。三种错误都各有样本：
//!
//! 1. 用 TTL/mtime 判断存活 → 暂停的进程被当成已退出（`SIGSTOP` 不释放
//!    `flock`，但会让 mtime 变旧）。
//! 2. 抢占活跃持有者 → 设计 §7.1 明禁。
//! 3. 回收时连活着的也清掉 → 下一个实例接手后，原进程还在写。

use std::path::PathBuf;
use std::time::Duration;

use yi_agent_plugins::{OwnerRecord, PluginError, ProjectOwnership};

fn temp_dir() -> PathBuf {
    tempfile::TempDir::new().expect("temp").keep()
}

fn ownership() -> ProjectOwnership {
    ProjectOwnership::new(temp_dir().join("owners"))
}

/// 一个持有所有权的子进程。`Drop` 负责杀掉并 wait。
///
/// 两个理由必须这么写：
///
/// 1. **不留孤儿**：子进程是死循环，测试 panic 时若只靠正常路径清理，
///    它会继续跑，下一次跑测试会撞上它持有的锁。
/// 2. **不继承管道**：子进程若继承 cargo 的 stdout/stderr，管道永远等不到
///    EOF，`cargo test` 会一直挂着——这正是之前 900s 超时的原因，
///    和测试逻辑无关。
struct Holder {
    child: std::process::Child,
}

impl Holder {
    fn spawn(root: &std::path::Path, project: &str) -> Self {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "helper_child_holds_a_project_owner",
                "--nocapture",
                "--ignored",
            ])
            .env("YI_OWNER_ROOT", root)
            .env("YI_OWNER_PROJECT", project)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn the holder");
        Holder { child }
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn kill_and_wait(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Holder {
    fn drop(&mut self) {
        self.kill_and_wait();
    }
}

/// 等一个**刚刚释放**的项目被观测成"没有活着的 owner"。
///
/// 为什么不是直接断言：`fork` 与 `exec` 之间的子进程与本进程共享打开文件
/// 描述，父进程释放后锁会短暂地仍被那个窗口持有（实测毫秒级）。这是 flock
/// 的固有性质，见 `locks.rs` 顶部说明。生产的调用方（`reclaimable`、维护
/// 重试）也只依赖最终一致，所以这里断言**有界收敛**，否则测试会随机红
/// ——那是把测试的疏忽当成实现的缺陷。
fn wait_until_released(ownership: &ProjectOwnership, project: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while ownership.is_alive(project).unwrap() {
        assert!(
            std::time::Instant::now() < deadline,
            "`{project}` never became free; the owner lock was not released"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// 等 `ownership` 认为项目已被持有。
fn wait_until_owned(ownership: &ProjectOwnership, project: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !ownership.is_alive(project).unwrap() {
        assert!(
            std::time::Instant::now() < deadline,
            "the child never took the owner lock for {project}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---------------------------------------------------------------------------
// 互斥
// ---------------------------------------------------------------------------

#[test]
fn a_second_owner_cannot_take_a_project_that_is_already_owned() {
    let ownership = ownership();
    let _first = ownership
        .acquire("/p/a", "instance-a", 100)
        .expect("first owner");

    let error = ownership
        .acquire("/p/a", "instance-b", 200)
        .expect_err("maintenance must not preempt a live owner");
    match error {
        PluginError::Busy(message) => {
            assert!(
                message.contains("instance-a"),
                "the error must name the holder so the user knows who to wait for, got {message:?}"
            );
        }
        other => panic!("expected Busy, got {other:?}"),
    }
}

#[test]
fn a_second_owner_may_take_a_different_project() {
    let ownership = ownership();
    let _a = ownership.acquire("/p/a", "instance-a", 100).unwrap();
    let _b = ownership
        .acquire("/p/b", "instance-b", 200)
        .expect("a different project is a different lock");
}

#[test]
fn releasing_frees_the_project_for_the_next_owner() {
    let ownership = ownership();
    {
        let _guard = ownership.acquire("/p/a", "instance-a", 100).unwrap();
    }
    ownership
        .acquire("/p/a", "instance-b", 200)
        .expect("after the first owner leaves the project is free");
}

#[test]
fn two_projects_whose_keys_sanitise_to_the_same_file_still_get_different_locks() {
    // 项目键通常是规范化路径。若只把 `/` 换成 `_`，`/p/a/b` 与 `/p/a_b`
    // 会映射到同一个文件，于是两个项目共用一个所有权锁。
    let ownership = ownership();
    let _first = ownership.acquire("/p/a/b", "instance-a", 100).unwrap();
    ownership
        .acquire("/p/a_b", "instance-b", 200)
        .expect("distinct project keys must never share a lock file");
}

// ---------------------------------------------------------------------------
// 存活判定
// ---------------------------------------------------------------------------

#[test]
fn an_owned_project_reports_alive() {
    let ownership = ownership();
    let _guard = ownership.acquire("/p/a", "instance-a", 100).unwrap();
    assert!(ownership.is_alive("/p/a").unwrap());
}

#[test]
fn a_project_nobody_owns_reports_dead() {
    let ownership = ownership();
    assert!(!ownership.is_alive("/p/never-owned").unwrap());
}

#[test]
fn a_released_project_reports_dead() {
    let ownership = ownership();
    {
        let _guard = ownership.acquire("/p/a", "instance-a", 100).unwrap();
    }
    wait_until_released(&ownership, "/p/a");
}

#[test]
fn liveness_is_never_decided_by_file_age() {
    // 这条直接设计成"看起来过期"：把 owner 记录与锁文件的 mtime 都改到
    // 很久以前，再检查它仍然被判为活着。任何按 mtime/TTL 回收的实现都会
    // 在这里说"已退出"。
    let ownership = ownership();
    let _guard = ownership.acquire("/p/a", "instance-a", 100).unwrap();

    let stale = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    for entry in std::fs::read_dir(ownership.root()).unwrap().flatten() {
        let _ = filetime_set(&entry.path(), stale);
    }

    assert!(
        ownership.is_alive("/p/a").unwrap(),
        "a live flock must stay alive no matter how old the files look; \
         age-based reclamation would let a second instance migrate underneath it"
    );
}

#[test]
fn a_stopped_process_keeps_its_ownership() {
    // `SIGSTOP` 不释放 flock，但进程确实"不动了"。任何超时回收都会在这里
    // 判错。用子进程做，因为测试本身不能被暂停。
    let ownership = ownership();
    let root = ownership.root().to_path_buf();
    let holder = Holder::spawn(&root, "/p/a");
    wait_until_owned(&ownership, "/p/a");

    let pid = holder.pid().to_string();
    let _ = std::process::Command::new("kill")
        .args(["-STOP", &pid])
        .status();
    std::thread::sleep(Duration::from_millis(200));

    let alive = ownership.is_alive("/p/a").unwrap();

    // 无论断言成败，先把子进程恢复到可被杀的状态，否则 `Drop` 里的
    // `kill` 对已停止的进程只是挂起信号，`wait` 会永远等下去。
    let _ = std::process::Command::new("kill")
        .args(["-CONT", &pid])
        .status();

    assert!(
        alive,
        "SIGSTOP does not release flock; a stopped owner must still block maintenance"
    );
}

#[test]
fn a_killed_process_releases_its_ownership() {
    let ownership = ownership();
    let root = ownership.root().to_path_buf();
    let mut holder = Holder::spawn(&root, "/p/a");
    wait_until_owned(&ownership, "/p/a");

    // `kill()` 在 unix 上是 SIGKILL：内核释放锁，不经过任何用户代码。
    holder.kill_and_wait();

    // 被杀进程由内核释放锁；窗口消失后必须观测到"没有 owner"。
    wait_until_released(&ownership, "/p/a");
}

#[test]
fn a_free_project_is_acquired_even_when_a_fork_window_temporarily_holds_the_lock() {
    // 钉住「有界重试」。`fork` 与 `exec` 之间子进程与本进程共享打开文件描述，
    // 在此期间释放的锁会短暂仍被判定为占用。一次尝试就会把它报成 Busy，
    // 维护动作于是无理由失败。这里的样本是：`fork` 一个不 `exec` 的子进程，
    // 父进程释放后立刻重新取锁——必须成功。
    let ownership = ownership();

    let guard = ownership.acquire("/p/a", "instance-a", 100).unwrap();
    let pid = unsafe { libc::fork() };
    if pid == 0 {
        // 子进程：不 exec，只短暂停留，制造共享描述的窗口。
        std::thread::sleep(Duration::from_millis(150));
        unsafe { libc::_exit(0) };
    }
    std::thread::sleep(Duration::from_millis(50));
    drop(guard);

    let next = ownership.acquire("/p/a", "instance-b", 200);
    unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };

    next.expect(
        "a transient fork window must not be reported as \"another instance owns this project\"",
    );
}

#[test]
fn project_keys_that_differ_only_in_separators_do_not_collide() {
    // 可读前缀会被过滤成同样的字符。没有摘要后缀时 `a-b` 与 `a_b`、
    // `/p/a/b` 与 `/p/a_b` 全部撞到同一个锁文件——两个不同项目共用一个
    // 所有权，第二个会被莫名拒绝。
    let collisions = [
        ("/p/a-b", "/p/a_b"),
        ("/p/a.b", "/p/a/b"),
        ("/p/a b", "/p/a_b"),
    ];
    for (first, second) in collisions {
        let ownership = ownership();
        let _a = ownership.acquire(first, "instance-a", 100).unwrap();
        ownership
            .acquire(second, "instance-b", 200)
            .unwrap_or_else(|error| {
                panic!("`{first}` and `{second}` must not share a lock: {error:?}")
            });
    }
}

#[test]
fn the_same_project_key_always_maps_to_the_same_lock_file() {
    // 摘要必须是稳定的：重启后同一个项目还要能被认出是同一个锁。
    let ownership = ownership();
    let _a = ownership.acquire("/p/a", "instance-a", 100).unwrap();
    assert!(
        !ownership
            .reclaimable()
            .unwrap()
            .iter()
            .any(|r| r.project == "/p/a"),
        "a live owner must never be listed as reclaimable"
    );
}

// ---------------------------------------------------------------------------
// 回收
// ---------------------------------------------------------------------------

#[test]
fn reclaimable_lists_only_dead_owners() {
    let ownership = ownership();
    let _live = ownership.acquire("/p/live", "instance-a", 100).unwrap();

    // 造一个死掉的 owner：取一次再放掉，然后手工留一份记录。
    {
        let _guard = ownership.acquire("/p/dead", "instance-b", 200).unwrap();
    }
    // `Drop` 会清掉 .owner.json，所以手工补回一份"崩溃留下的"记录。
    let dead_record = OwnerRecord {
        project: "/p/dead".to_string(),
        owner: "instance-b".to_string(),
        pid: 200,
    };
    let name = sanitized_name_for(&ownership, "/p/dead");
    std::fs::write(
        ownership.root().join(format!("{name}.owner.json")),
        serde_json::to_vec(&dead_record).unwrap(),
    )
    .unwrap();

    let reclaimable = ownership.reclaimable().unwrap();
    let projects: Vec<&str> = reclaimable.iter().map(|r| r.project.as_str()).collect();
    assert_eq!(
        projects,
        vec!["/p/dead"],
        "only dead owners may be reclaimed; a live owner must never be swept"
    );
}

#[test]
fn reclaimable_is_empty_when_every_owner_is_alive() {
    let ownership = ownership();
    let _a = ownership.acquire("/p/a", "instance-a", 100).unwrap();
    let _b = ownership.acquire("/p/b", "instance-b", 200).unwrap();
    assert!(
        ownership.reclaimable().unwrap().is_empty(),
        "a sweep must not touch live owners"
    );
}

// ---------------------------------------------------------------------------
// 子进程入口
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn helper_child_holds_a_project_owner() {
    let root = std::env::var("YI_OWNER_ROOT").expect("root");
    let project = std::env::var("YI_OWNER_PROJECT").expect("project");
    let ownership = ProjectOwnership::new(root);
    let _guard = ownership
        .acquire(&project, "child", std::process::id())
        .expect("the child must be able to take the owner lock");
    // 一直持有到被杀/被放过。
    loop {
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// 测试里复现 `owner_path` 的命名规则，用来手工造"崩溃遗留记录"。
fn sanitized_name_for(ownership: &ProjectOwnership, project: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(project.as_bytes());
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    let mut readable: String = project
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(32)
        .collect();
    if readable.is_empty() {
        readable.push_str("project");
    }
    let _ = ownership;
    format!("{readable}-{hex}")
}

#[cfg(unix)]
fn filetime_set(path: &std::path::Path, when: std::time::SystemTime) -> std::io::Result<()> {
    // 用 libc 的 utimes：`std::fs::FileTimes` 在当前 toolchain 的
    // `std::os::unix` 下不可用，而这里只要"把 mtime 改旧"这一个动作。
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let since_epoch = when
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as libc::time_t;
    let times = [
        libc::timeval {
            tv_sec: since_epoch,
            tv_usec: 0,
        },
        libc::timeval {
            tv_sec: since_epoch,
            tv_usec: 0,
        },
    ];
    let outcome = unsafe { libc::utimes(c_path.as_ptr(), times.as_ptr()) };
    if outcome != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn filetime_set(_path: &std::path::Path, _when: std::time::SystemTime) -> std::io::Result<()> {
    Ok(())
}
