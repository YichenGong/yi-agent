//! 端到端：落盘 → resume → 再 resume。
//!
//! TUI 是全屏应用、无法自动化，`run --resume` 是这条链路唯一的可测载体。
//!
//! 这里用一个最小的 TCP 服务器冒充 Anthropic 的 `/v1/messages`，返回固定的
//! SSE 文本。不引入 wiremock：本 crate 的 dev-dependencies 里没有它，而这条
//! 链路只需要「回一句固定的话」，用手写几十行比加一个依赖更划算。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU16, Ordering};

/// 起一个只会回固定 SSE 的服务器，返回它的 base URL。
///
/// 用固定端口递增而不是让内核分配：`yi-agent` 是子进程，通过环境变量拿地址，
/// 拿不到内核分配的端口号就得再做一次进程间通信。
fn start_fake_anthropic() -> String {
    static NEXT_PORT: AtomicU16 = AtomicU16::new(47_100);
    for _ in 0..50 {
        let port = NEXT_PORT.fetch_add(1, Ordering::SeqCst);
        let Ok(listener) = TcpListener::bind(("127.0.0.1", port)) else {
            continue;
        };
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                std::thread::spawn(move || {
                    // 读完请求头与 body：不回完就读会让客户端看到 reset。
                    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                    let mut content_length = 0usize;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).is_err() || line == "\r\n" || line == "\n" {
                            break;
                        }
                        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                            content_length = v.trim().parse().unwrap_or(0);
                        }
                    }
                    if content_length > 0 {
                        let mut body = vec![0u8; content_length];
                        let _ = reader.read_exact(&mut body);
                    }

                    let body = "event: message_start\ndata: {\"type\":\"message_start\"}\n\n\
                        event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
                        event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"好的\"}}\n\n\
                        event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
                        event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":7,\"output_tokens\":2}}\n\n\
                        event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                });
            }
        });
        return format!("http://127.0.0.1:{port}");
    }
    panic!("找不到可用端口");
}

fn run_in(dir: &Path, api_url: &str, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_yi-agent"))
        .args(args)
        .current_dir(dir)
        // 隔离实例根：否则会读到开发机 ~/.yi-agent 里的真实配置与 relay。
        .env("YI_AGENT_HOME", dir.join("home"))
        .env("YI_AGENT_PROVIDER", "anthropic")
        .env("YI_AGENT_MODEL", "claude-test")
        .env("MODEL_API_URL", api_url)
        .env("MODEL_API_KEY", "sk-test")
        .output()
        .expect("run yi-agent")
}

fn threads_dir(dir: &Path) -> PathBuf {
    dir.join(".yi-agent").join("threads")
}

/// 找出目录里唯一的 thread id。
fn only_thread_id(dir: &Path) -> String {
    let mut ids: Vec<String> = std::fs::read_dir(threads_dir(dir))
        .expect("threads dir")
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.strip_suffix(".meta.json").map(str::to_string)
        })
        .collect();
    ids.sort();
    assert_eq!(ids.len(), 1, "应恰好一个会话，实际 {ids:?}");
    ids.remove(0)
}

fn turn_count(dir: &Path, id: &str) -> usize {
    let log = std::fs::read_to_string(threads_dir(dir).join(format!("{id}.jsonl"))).unwrap();
    log.lines()
        .filter(|l| l.contains("\"type\":\"turn\""))
        .count()
}

/// 首轮落盘 → resume 追加 → 再 resume 追加。
///
/// 这条用例同时覆盖三件事：新会话会建 meta（否则 `--last`/`resume` 找不到）、
/// resume 会把历史种进 session（否则第二轮不会带着第一轮的内容）、每次退出
/// 都追加而不是覆盖。
#[test]
fn run_resume_restores_history_and_appends() {
    let dir = tempfile::TempDir::new().unwrap();
    let api = start_fake_anthropic();

    let first = run_in(dir.path(), &api, &["run", "你好"]);
    assert!(
        first.status.success(),
        "首轮失败: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let id = only_thread_id(dir.path());
    assert_eq!(turn_count(dir.path(), &id), 1, "首轮必须落盘");

    let second = run_in(dir.path(), &api, &["run", "--resume", &id, "继续"]);
    assert!(
        second.status.success(),
        "resume 失败: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(turn_count(dir.path(), &id), 2, "第二轮必须追加进同一份日志");

    // 前缀匹配：uuid 太长，实际使用中几乎都是打前缀。
    let prefix = &id[..9];
    let third = run_in(dir.path(), &api, &["run", "--resume", prefix, "再来"]);
    assert!(
        third.status.success(),
        "前缀 resume 失败: {}",
        String::from_utf8_lossy(&third.stderr)
    );
    assert_eq!(turn_count(dir.path(), &id), 3, "前缀也必须解析到同一个会话");

    // 锁文件在首次建会话时就该存在。
    assert!(threads_dir(dir.path()).join(format!("{id}.lock")).exists());
}

/// resume 后的请求必须带上历史：`messages` 里应该出现前几轮的内容。
#[test]
fn resumed_session_carries_the_previous_turns() {
    let dir = tempfile::TempDir::new().unwrap();
    let api = start_fake_anthropic();

    assert!(
        run_in(dir.path(), &api, &["run", "第一个问题"])
            .status
            .success()
    );
    let id = only_thread_id(dir.path());
    assert!(
        run_in(dir.path(), &api, &["run", "--resume", &id, "第二个问题"])
            .status
            .success()
    );

    let log = std::fs::read_to_string(threads_dir(dir.path()).join(format!("{id}.jsonl"))).unwrap();
    let turns: Vec<&str> = log
        .lines()
        .filter(|l| l.contains("\"type\":\"turn\""))
        .collect();
    assert_eq!(turns.len(), 2);
    // 第二个 turn 的 messages 必须包含第一个 turn 的内容——这就是 resume
    // 真的把历史种进 session 了，而不是开了个新会话。
    assert!(
        turns[1].contains("第一个问题"),
        "第二轮落盘的 messages 里应带着第一轮的历史"
    );
    assert!(turns[1].contains("第二个问题"));
}

/// 不存在的会话必须报错退出，而不是静默开一个新的。
#[test]
fn resume_of_missing_thread_fails_loudly() {
    let dir = tempfile::TempDir::new().unwrap();
    let api = start_fake_anthropic();
    let out = run_in(
        dir.path(),
        &api,
        &["run", "--resume", "thread-doesnotexist", "x"],
    );
    assert!(!out.status.success(), "不存在的会话必须报错退出");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("找不到会话"),
        "错误信息要能指导用户，实际: {stderr}"
    );
}

/// 两个会话存在时，歧义前缀必须报错并列出候选，而不是随便挑一个。
#[test]
fn ambiguous_prefix_is_rejected() {
    let dir = tempfile::TempDir::new().unwrap();
    let api = start_fake_anthropic();
    assert!(run_in(dir.path(), &api, &["run", "a"]).status.success());
    assert!(run_in(dir.path(), &api, &["run", "b"]).status.success());

    // 两个 uuid 共享的前缀长度不确定，直接取两者公共前缀再减去末尾字符，
    // 保证它一定同时命中两个。公共前缀通常一个字符都不共享，此时跳过。
    let ids: Vec<String> = std::fs::read_dir(threads_dir(dir.path()))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.file_name()
                .to_string_lossy()
                .strip_suffix(".meta.json")
                .map(str::to_string)
        })
        .collect();
    assert_eq!(ids.len(), 2);
    let common: String = ids[0]
        .chars()
        .zip(ids[1].chars())
        .take_while(|(a, b)| a == b)
        .map(|(a, _)| a)
        .collect();
    // 至少 `thread-` 这段是共享的（7 个字符）。
    assert!(common.len() >= 7, "两个 id 至少共享 thread- 前缀");
    let ambiguous = &common[..common.len() - 1];

    let out = run_in(dir.path(), &api, &["run", "--resume", ambiguous, "x"]);
    // 减一个字符后仍同时命中两条 → 必须是歧义；若恰好只剩一条就跳过断言。
    if ids.iter().filter(|i| i.starts_with(ambiguous)).count() > 1 {
        assert!(!out.status.success(), "歧义前缀必须失败");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("歧义"), "应提示前缀有歧义，实际: {stderr}");
    }
}
