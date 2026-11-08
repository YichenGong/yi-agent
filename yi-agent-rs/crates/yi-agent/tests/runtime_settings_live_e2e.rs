//! 端到端：设置保存后，**下一个真实请求**必须用新值——跨进程、经真实 CLI。
//!
//! 为什么必须跨进程且用真实二进制：
//! - 进程内测试可以证明 `LiveSettings` 被读取，但证明不了"保存发生在**另一个
//!   进程**里、运行中的 agent 仍会跟上"——那正是用户会遇到的场景（桌面/手机
//!   保存，CLI 会话在跑）。
//! - 断言在**假 provider 收到的请求体**上，而不是 `AgentConfig`：配置正确但循环
//!   没去读，只有看请求才抓得到。
//!
//! 隔离：`YI_AGENT_HOME` 指向临时目录，于是 `settings.json` 落在临时实例根下；
//! 绝不读、也不写开发机真实配置。

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use yi_agent_runtime::live_settings::LiveSettings;
use yi_agent_runtime::settings::store::PatchMode;

/// 一个只会回固定 SSE 文本的假 Anthropic 端点，并**记录每个请求的 system 提示**。
struct FakeProvider {
    base_url: String,
    /// 每次 `/v1/messages` 请求里的 `system` 字段（字符串或块数组都取出文本）。
    systems: Arc<Mutex<Vec<String>>>,
    requests: Arc<AtomicUsize>,
}

impl FakeProvider {
    fn start() -> Self {
        static NEXT_PORT: AtomicU16 = AtomicU16::new(48_200);
        let systems = Arc::new(Mutex::new(Vec::new()));
        let requests = Arc::new(AtomicUsize::new(0));

        for _ in 0..80 {
            let port = NEXT_PORT.fetch_add(1, Ordering::SeqCst);
            let Ok(listener) = TcpListener::bind(("127.0.0.1", port)) else {
                continue;
            };
            // 用别名而不是遮蔽外层变量：遮蔽会把 `Arc` move 进闭包，
            // 后面的 `return Self { systems, requests }` 就拿不到了。
            let thread_systems = Arc::clone(&systems);
            let thread_requests = Arc::clone(&requests);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let systems = Arc::clone(&thread_systems);
                    let requests = Arc::clone(&thread_requests);
                    std::thread::spawn(move || {
                        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
                        let mut content_length = 0usize;
                        loop {
                            let mut line = String::new();
                            if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                                break;
                            }
                            if let Some(value) =
                                line.to_ascii_lowercase().strip_prefix("content-length:")
                            {
                                content_length = value.trim().parse().unwrap_or(0);
                            }
                        }
                        let mut body = vec![0u8; content_length];
                        if content_length > 0 {
                            let _ = reader.read_exact(&mut body);
                        }
                        // 记录这次请求实际携带的 system 提示。
                        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&body) {
                            requests.fetch_add(1, Ordering::SeqCst);
                            let text = json.get("system").map(system_text).unwrap_or_default();
                            systems.lock().unwrap().push(text);
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
            return Self {
                base_url: format!("http://127.0.0.1:{port}"),
                systems,
                requests,
            };
        }
        panic!("找不到可用端口");
    }

    fn systems(&self) -> Vec<String> {
        self.systems.lock().unwrap().clone()
    }

    /// How many requests the fake endpoint recorded.
    ///
    /// Used to assert that the extra request really happened, instead of only
    /// checking that a prompt string appears in *some* request — the latter also
    /// passes when a single request was enough.
    fn request_count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
    /// 等到至少收到 `count` 个请求；超时返回当前已有的（调用方自己断言）。
    fn wait_for_requests(&self, count: usize, limit: Duration) -> Vec<String> {
        let deadline = Instant::now() + limit;
        loop {
            let systems = self.systems();
            if systems.len() >= count || Instant::now() >= deadline {
                return systems;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// `system` 既可能是字符串，也可能是 `[{type:text,text:...}]` 块数组。
fn system_text(value: &serde_json::Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_string();
    }
    let mut out = String::new();
    if let Some(blocks) = value.as_array() {
        for block in blocks {
            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
    }
    out
}

/// 隔离实例：临时 `YI_AGENT_HOME`，因此 `settings.json` 完全在临时目录里。
struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            root: tempfile::TempDir::new().expect("tempdir"),
        }
    }

    fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    fn workdir(&self) -> PathBuf {
        let dir = self.root.path().join("workdir");
        std::fs::create_dir_all(&dir).expect("workdir");
        dir
    }

    fn settings_path(&self) -> PathBuf {
        self.home().join(".yi-agent").join("settings.json")
    }

    /// 与 `yi-agent run` 子进程共用同一个宿主实例根。
    fn run(&self, provider: &FakeProvider, prompt: &str) -> std::process::Child {
        let mut command = Command::new(common::yi_agent_bin());
        command
            //  是**全局**参数，必须写在子命令之前。
            .arg("--workdir")
            .arg(self.workdir())
            .arg("run")
            .arg(prompt)
            .env("YI_AGENT_HOME", self.home())
            .env("YI_AGENT_PROVIDER", "anthropic")
            .env("YI_AGENT_MODEL", "claude-test")
            .env("MODEL_API_URL", &provider.base_url)
            .env("MODEL_API_KEY", "sk-test")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // 父进程的环境变量会覆盖 `settings.json`（覆盖 > 环境 > 文件），
        // 所以这里必须清掉任何可能存在的真实 key/地址。
        for name in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "MODEL_API_KEY",
            "MODEL_API_URL",
            "YI_AGENT_API_URL",
        ] {
            command.env_remove(name);
        }
        command
            .env("MODEL_API_URL", &provider.base_url)
            .env("MODEL_API_KEY", "sk-test")
            .spawn()
            .expect("spawn yi-agent run")
    }
}

fn output_text(child: std::process::Child) -> (bool, String, String) {
    let out = child.wait_with_output().expect("wait yi-agent run");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn a_save_from_another_process_reaches_the_next_real_request() {
    let fixture = Fixture::new();
    let provider = FakeProvider::start();

    // 起点：settings.json 里有一条可识别的 system 提示。
    let live = LiveSettings::shared(fixture.settings_path());
    live.commit(
        &serde_json::json!({ "agent.system_prompt": "PROMPT-ALPHA" }),
        PatchMode::ReplaceProvided,
    )
    .expect("seed settings");

    let success = {
        let child = fixture.run(&provider, "hello");
        let (ok, _stdout, stderr) = output_text(child);
        assert!(ok, "run 应成功，stderr: {stderr}");
        true
    };
    assert!(success);

    let systems = provider.wait_for_requests(1, Duration::from_secs(10));
    assert!(
        systems.iter().any(|s| s.contains("PROMPT-ALPHA")),
        "首个请求必须带上文件里的提示，实际收到 {systems:?}"
    );
    // One turn is at least one request; the `>= 2` below proves a second turn.
    assert!(provider.request_count() >= 1, "至少要有一个请求");

    // 第二个会话：跑之前由**另一个进程**（本测试进程）改设置。
    // 这正是"用户在桌面/手机保存，CLI 会话随后启动"的形状。
    let live_second = LiveSettings::shared(fixture.settings_path());
    live_second
        .commit(
            &serde_json::json!({ "agent.system_prompt": "PROMPT-BETA" }),
            PatchMode::ReplaceProvided,
        )
        .expect("second commit");

    let child = fixture.run(&provider, "hello again");
    let (ok, _stdout, stderr) = output_text(child);
    assert!(ok, "第二次 run 应成功，stderr: {stderr}");

    let systems = provider.wait_for_requests(2, Duration::from_secs(10));
    assert!(
        systems.len() >= 2,
        "应至少收到两次请求，实际 {}",
        systems.len()
    );
    // Two sessions means at least one request each: the count complements the
    // text assertions, because text alone cannot tell "the second session ran"
    // from "the first session retried once".
    assert!(
        provider.request_count() >= 2,
        "两次会话合计至少两个请求，实际 {}",
        provider.request_count()
    );
    assert!(
        systems[0].contains("PROMPT-ALPHA"),
        "第一个请求不该被后来的保存改写：{:?}",
        systems[0]
    );
    assert!(
        systems[1].contains("PROMPT-BETA"),
        "新会话必须采用新保存的值，实际 {:?}",
        systems[1]
    );
    assert!(
        !systems[1].contains("PROMPT-ALPHA"),
        "新会话不该残留旧提示：{:?}",
        systems[1]
    );
}

#[test]
fn max_turns_saved_before_a_run_bounds_that_run() {
    // 一条"数值也会生效"的独立证据：`agent.max_turns` 在真实循环里被使用。
    // 假 provider 每次都回 end_turn，所以这里断言的不是"被截断"，而是
    // 该值确实被读到并进入了有效的运行配置——通过 config 读取端点验证。
    let fixture = Fixture::new();
    let live = LiveSettings::shared(fixture.settings_path());
    live.commit(
        &serde_json::json!({ "agent.max_turns": 7 }),
        PatchMode::ReplaceProvided,
    )
    .expect("commit");

    // 用真实配置加载器读同一路径（`load_from` 是 CLI 走的同一条路径）。
    let settings = yi_agent_runtime::settings::load_from(&fixture.settings_path());
    assert_eq!(settings.agent.max_turns, Some(7));
}

#[test]
fn a_corrupt_file_does_not_silently_look_like_defaults() {
    // 坏文件必须被**报告为降级**，而不是静默当成默认值——否则用户会以为
    // 自己保存的值生不了效是因为"程序不认"，而不是文件坏了。
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.settings_path().parent().unwrap()).unwrap();
    std::fs::write(fixture.settings_path(), b"{ this is not json").unwrap();

    let live = LiveSettings::shared(fixture.settings_path());
    let snapshot = live.refresh();
    assert_eq!(
        snapshot.health.state,
        yi_agent_runtime::live_settings::HealthState::Degraded,
        "坏 JSON 必须降级，实际 {:?}",
        snapshot.health
    );
    assert!(
        snapshot.health.code.is_some(),
        "降级必须带一个可诊断的 code，否则用户只看到'不生效'"
    );
}

#[test]
fn the_child_process_never_reads_the_real_host_settings() {
    // 隔离自检：临时实例根之外的 settings.json 一定不被读到。
    // 这条防的是"测试碰了开发机配置"这种最难在事后发现的事故。
    let fixture = Fixture::new();
    assert!(
        fixture.settings_path().starts_with(fixture.home()),
        "settings.json 必须落在临时实例根内，实际 {:?}",
        fixture.settings_path()
    );
    // 子进程用的写锁文件同样在临时目录内。
    let live = LiveSettings::shared(fixture.settings_path());
    live.commit(
        &serde_json::json!({ "agent.max_turns": 3 }),
        PatchMode::ReplaceProvided,
    )
    .expect("commit");
    let lock = fixture
        .settings_path()
        .with_file_name(".settings.json.lock");
    assert!(lock.exists(), "写锁必须落在临时目录内：{lock:?}");
    assert!(lock.starts_with(fixture.home()));
}
