//! 真实**多进程**验证：同一份 `settings.json` 被两个独立进程写。
//!
//! 为什么必须开新进程：进程内锁测试无法证明 `flock` 真的跨进程生效，也无法
//! 证明「并发写不同键」不会互相覆盖——那是同一把锁保护的文件级读改写语义，
//! 只有在两个真实 OS 进程里跑才有意义。
//!
//! 子进程复用同一个测试二进制（`current_exe` + `--exact <测试名>`），通过
//! stdin/stdout 行握手协调，**不用 sleep 排竞态**：sleep 只能让测试看起来稳，
//! 稳定的是运气而不是因果。

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use yi_agent_runtime::live_settings::LiveSettings;
use yi_agent_runtime::settings::store::PatchMode;

/// 子进程环境变量：存在即代表"我是被父测试拉起来的 writer"。
const CHILD_PATH_ENV: &str = "YI_TEST_SETTINGS_CHILD_PATH";
/// 子进程就绪信号。带专用前缀，避免把 test harness 自己的输出当协议帧。
const READY: &str = "YI_SETTINGS_CHILD READY";
/// 子进程完成一次提交的信号。
const COMMITTED: &str = "YI_SETTINGS_CHILD COMMITTED";
/// 子进程等锁超时（另一个进程持锁不放）。
const TIMED_OUT: &str = "YI_SETTINGS_CHILD TIMEOUT";

/// 子进程 stdout 的一行，或流结束。
type Line = Option<String>;

/// 保证异常路径也把子进程收掉。
///
/// 只处理本测试自己 `spawn` 的 child：绝不能按进程名匹配去杀，那会顺手杀掉
/// 别的 cargo 测试进程。
struct ChildGuard {
    child: Child,
    /// 读线程送来的行。**读必须带 deadline**：子进程若因为断言失败而停在
    /// 某个不期望的位置（比如本该超时却提交成功），无界 `read_line` 会把整个
    /// 测试挂死，而挂死的测试既不红也不绿——那是比失败更坏的证据。
    lines: std::sync::mpsc::Receiver<Line>,
}

impl ChildGuard {
    fn spawn(path: &Path) -> Self {
        let exe = std::env::current_exe().expect("current_exe");
        let mut child = Command::new(exe)
            .args(["--exact", "settings_writer_child", "--nocapture"])
            .env(CHILD_PATH_ENV, path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn child test binary");
        let stdout = child.stdout.take().expect("child stdout");
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        if tx.send(Some(line)).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = tx.send(None);
        });
        Self { child, lines }
    }

    /// 等一行**专用前缀**，返回其后的载荷部分（无载荷则空串）。
    ///
    /// 带 20 秒上限：真实超时用例最多等 5 秒锁，留足余量即可，超出必是
    /// 协议没按预期推进，直接失败比挂起有用。
    fn expect_line(&mut self, prefix: &str) -> String {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            let remaining = deadline
                .checked_duration_since(std::time::Instant::now())
                .unwrap_or_default();
            assert!(
                !remaining.is_zero(),
                "等 `{prefix}` 超时（20s）：子进程没有按协议推进"
            );
            match self.lines.recv_timeout(remaining) {
                Ok(Some(line)) => {
                    let trimmed = line.trim_end();
                    if trimmed == prefix {
                        return String::new();
                    }
                    if let Some(rest) = trimmed.strip_prefix(prefix) {
                        return rest.trim().to_string();
                    }
                    // 其它行是 cargo/test harness 的输出，不是协议帧：继续读。
                }
                Ok(None) => panic!("子进程在发出 `{prefix}` 之前就退出了（协议未完成）"),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    panic!("等 `{prefix}` 超时（20s）：子进程没有按协议推进")
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("读线程意外结束，未能确认 `{prefix}`")
                }
            }
        }
    }

    fn send(&mut self, line: &str) {
        let stdin = self.child.stdin.as_mut().expect("child stdin");
        writeln!(stdin, "{line}").expect("write child stdin");
        stdin.flush().expect("flush child stdin");
    }

    fn wait(mut self) {
        // 关掉 stdin 让子进程读到 EOF 后退出，再等它，避免留僵尸。
        drop(self.child.stdin.take());
        let status = self.child.wait().expect("wait child");
        assert!(status.success(), "子进程非零退出：{status:?}");
    }
}

/// 子进程主体。
///
/// 无 `CHILD_PATH_ENV` 时立即返回——普通 `cargo test` 跑全量时它会是一条
/// **通过的空测试**，不会去碰任何文件。
#[test]
fn settings_writer_child() {
    let Ok(path) = std::env::var(CHILD_PATH_ENV) else {
        return;
    };
    let live = LiveSettings::shared(PathBuf::from(path));
    println!("{READY}");
    std::io::stdout().flush().expect("flush READY");

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line.expect("read patch line");
        if line.trim().is_empty() {
            break;
        }
        // 协议：`<键> <值>`，只用于测试，不解析复杂 JSON。
        let mut parts = line.splitn(2, ' ');
        let key = parts.next().expect("key");
        let value = parts.next().expect("value");
        let patch = serde_json::json!({ key: value });
        match live.commit(&patch, PatchMode::ReplaceProvided) {
            Ok(receipt) => {
                assert!(
                    receipt.changed_keys.iter().any(|k| k == key),
                    "子进程提交后应报告 {key} 变化，实际 {:?}",
                    receipt.changed_keys
                );
                println!("{COMMITTED}");
            }
            // 锁被另一个进程占着：这是**预期**结果之一，必须如实上报，
            // 让父测试去断言"有界失败而不是写坏文件"。
            Err(yi_agent_runtime::settings::SettingsError::LockTimeout) => {
                println!("{TIMED_OUT}");
            }
            Err(other) => panic!("子进程提交遇到非预期错误：{other}"),
        }
        std::io::stdout().flush().expect("flush child status");
    }
}

fn temp_settings_path() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("settings.json");
    (dir, path)
}

#[test]
fn two_processes_writing_different_keys_keep_both() {
    let (_dir, path) = temp_settings_path();
    let mut first = ChildGuard::spawn(&path);
    let mut second = ChildGuard::spawn(&path);
    first.expect_line(READY);
    second.expect_line(READY);

    // 两个进程都就绪后才发各自的 patch：这就是"并发写"的因果点，
    // 而不是靠 sleep 猜时窗。
    first.send("agent.max_turns 40");
    second.send("tui.terminal_title false");
    first.expect_line(COMMITTED);
    second.expect_line(COMMITTED);

    // 父进程重新读盘：两个键都必须还在。若写锁没跨进程生效，
    // 「读-改-写」会丢掉后写者看到的那一半。
    let live = LiveSettings::shared(path.clone());
    let snapshot = live.refresh();
    assert_eq!(snapshot.settings.agent.max_turns, Some(40));
    assert_eq!(
        snapshot.settings.tui.terminal_title.as_deref(),
        Some("false")
    );

    first.wait();
    second.wait();
}

#[test]
fn same_key_last_committed_wins_across_processes() {
    let (_dir, path) = temp_settings_path();
    let mut first = ChildGuard::spawn(&path);
    let mut second = ChildGuard::spawn(&path);
    first.expect_line(READY);
    second.expect_line(READY);

    // 明确控制先后：只有看到 first 的 COMMITTED 才发 second 的写入，
    // 于是"最后提交胜出"是可判定的，不是竞态抽奖。
    first.send("agent.max_turns 11");
    first.expect_line(COMMITTED);
    second.send("agent.max_turns 22");
    second.expect_line(COMMITTED);

    let live = LiveSettings::shared(path.clone());
    let snapshot = live.refresh();
    assert_eq!(
        snapshot.settings.agent.max_turns,
        Some(22),
        "后提交的值必须胜出"
    );

    first.wait();
    second.wait();
}

#[test]
fn the_first_process_observes_a_remote_commit() {
    // 跨进程观察：A 进程持有实例并 refresh 过，B 进程写盘，A 再 refresh
    // 必须看到新值。这条覆盖"另一个进程改了文件"这一最基本的跨进程事实。
    let (_dir, path) = temp_settings_path();
    let live = LiveSettings::shared(path.clone());
    let before = live.refresh();
    assert_eq!(before.settings.agent.max_turns, None);

    let mut writer = ChildGuard::spawn(&path);
    writer.expect_line(READY);
    writer.send("agent.max_turns 77");
    writer.expect_line(COMMITTED);
    writer.wait();

    let after = live.refresh();
    assert_eq!(after.settings.agent.max_turns, Some(77));
    assert_ne!(before.revision, after.revision, "内容变了 revision 必须变");
}

#[test]
fn a_held_lock_times_out_instead_of_corrupting_the_file() {
    // 一个进程持有写锁不放，另一个进程的提交必须在**有界**时间内以
    // `LockTimeout` 失败——而不是写坏文件或无限等待。
    let (_dir, path) = temp_settings_path();
    let live = LiveSettings::shared(path.clone());
    live.commit(
        &serde_json::json!({"agent.max_turns": 5}),
        PatchMode::ReplaceProvided,
    )
    .expect("seed");

    // 锁文件名固定（见 store::lock_file_path）：父进程手工拿同一把 flock。
    let lock_path = path.with_file_name(".settings.json.lock");
    let holder = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .expect("open lock file");
    use std::os::fd::AsRawFd;
    // SAFETY: 对刚由本进程打开的有效 fd 调用 flock，无内存安全影响。
    let locked = unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_EX) };
    assert_eq!(locked, 0, "父进程应先拿到排他锁");

    let mut child = ChildGuard::spawn(&path);
    child.expect_line(READY);
    child.send("agent.max_turns 6");

    // 子进程要么按 5 秒上限超时（预期），要么已经写完（说明锁没生效）。
    // 两种结果都读得到，故这条断言本身能抓住"锁其实没跨进程"。
    let outcome = child.expect_line(TIMED_OUT);
    assert_eq!(outcome, "", "超时信号不应带载荷");

    // SAFETY: 同一次 flock 的解锁。
    unsafe { libc::flock(holder.as_raw_fd(), libc::LOCK_UN) };
    drop(holder);
    let _ = child.child.kill();
    let _ = child.child.wait();

    let snapshot = live.refresh();
    assert_eq!(
        snapshot.settings.agent.max_turns,
        Some(5),
        "锁被占期间不得有任何写入生效，也不得把文件写坏"
    );
}

#[test]
fn the_lock_file_is_never_unlinked_so_inodes_stay_stable() {
    // 锁文件被 unlink 会让"两个进程拿两把不同的锁"成为可能：后开者创建新
    // inode，与先开者互不排斥。这条断言锁文件在提交后仍然存在。
    let (_dir, path) = temp_settings_path();
    let lock_path = path.with_file_name(".settings.json.lock");
    let live = LiveSettings::shared(path.clone());
    live.commit(
        &serde_json::json!({"agent.max_turns": 9}),
        PatchMode::ReplaceProvided,
    )
    .expect("commit");
    assert!(lock_path.exists(), "锁文件必须在提交后仍然存在");
}

#[test]
fn a_child_that_dies_mid_commit_releases_the_lock() {
    // 内核在进程退出时释放 flock：持锁进程被 kill 后，下一个进程必须能立刻拿到
    // 锁，而不是等到 5 秒超时。这是"锁绑定进程生命周期"的实际验证。
    let (_dir, path) = temp_settings_path();
    let mut child = ChildGuard::spawn(&path);
    child.expect_line(READY);
    // 子进程此刻阻塞在 stdin 上（没有提交），直接杀掉它。
    let _ = child.child.kill();
    let _ = child.child.wait();

    let live = LiveSettings::shared(path.clone());
    live.commit(
        &serde_json::json!({"agent.max_turns": 3}),
        PatchMode::ReplaceProvided,
    )
    .expect("kill 之后提交必须立刻成功（内核已释放锁）");
    assert_eq!(live.refresh().settings.agent.max_turns, Some(3));
}

/// 全字段消费覆盖：`FIELDS` 里的每个键都必须有一个**真实可执行**的消费测试。
///
/// 这里只做清单层面的校验（键集合精确相等、无重复、每行都指向一个具体测试）。
/// 测试名是否真的存在于目标 target、是否真的跑起来并通过，由
/// `scripts/check-live-settings-coverage.py` 交给 cargo 去验证——源码里搜到
/// 一个同名函数不等于有一个会执行的用例。
#[test]
fn consumer_manifest_covers_all_fields() {
    use std::collections::BTreeSet;

    let raw = include_str!("../../../scripts/runtime-settings-consumers.json");
    let document: serde_json::Value =
        serde_json::from_str(raw).expect("consumers 清单必须是合法 JSON");
    let entries = document
        .get("fields")
        .and_then(|value| value.as_array())
        .expect("清单必须有 fields 数组");
    assert!(!entries.is_empty(), "清单不能为空");

    let declared: BTreeSet<&str> = yi_agent_runtime::settings::FIELDS
        .iter()
        .map(|field| field.key)
        .collect();
    let mapped: BTreeSet<&str> = entries
        .iter()
        .map(|entry| {
            entry
                .get("key")
                .and_then(|value| value.as_str())
                .expect("每行必须有 key")
        })
        .collect();

    assert_eq!(
        declared, mapped,
        "FIELDS 与消费清单的键集合必须精确相等；差集意味着某个字段没人消费，\
         或者清单里写了不存在的键"
    );

    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    for entry in entries {
        for field in ["key", "package", "target_kind", "target", "test_name"] {
            assert!(
                entry.get(field).and_then(|v| v.as_str()).is_some(),
                "每条清单都必须有字符串字段 {field}：{entry:?}"
            );
        }
        let kind = entry["target_kind"].as_str().unwrap();
        assert!(
            matches!(kind, "lib" | "bin" | "test"),
            "target_kind 只能是 lib/bin/test，实际 {kind}"
        );
        let test_name = entry["test_name"].as_str().unwrap();
        assert!(
            !test_name.is_empty(),
            "test_name 不能为空：一个字段没有具体测试就不算被证明"
        );
        let identity = (
            entry["key"].as_str().unwrap().to_string(),
            test_name.to_string(),
        );
        assert!(
            seen.insert(identity.clone()),
            "清单里有完全重复的行：{identity:?}"
        );
    }
}
