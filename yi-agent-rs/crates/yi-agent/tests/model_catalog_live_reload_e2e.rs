//! 真机 e2e：改 `models.json` 后 daemon 必须不重启即跟上。
//!
//! 三步判据（对照 2026-10-17 的现场事故：旧 daemon 在配置修好后仍 0 成功 / 120 次 404）：
//!   1. 清单可用 → 委派成功；
//!   2. 改成不可用 → 委派失败（**证明确实重读了**，不是缓存侥幸）；
//!   3. 改回可用 → 委派再次成功（**证明修好即生效**，无需重启 daemon）。
//!
//! 关键设计点（都不是可选的，缺一项判据即失效）：
//! - **root 侧凭据走 CLI 且带 `/v1`**：root（headless `run`）的 provider 来自
//!   CLI/env，与清单无关（`make headless` 的 `config::load`）。因此第 2 步把清单
//!   改坏后，root 仍能启动并调用 `spawn_agent`——失败的是 **worker**，正是要观测的。
//!   若 root 也依赖清单，第 2 步会把 root 一起打死，就跑不到 worker 失败这一步。
//! - **`run` 必须带 `--subagents`**：否则 headless root 根本不注册委派工具，
//!   一次委派都不会发生（计划初稿漏了它，测试会「永远等不到子任务」）。
//! - **子任务按 `depth == 1` 且非 root 选取**：root 自己也会落终态，若把所有新任务
//!   都当成子任务，读到的会是 root 的 `completed`，第 2 步就永远观测不到 `failed`。
//! - **读状态走 IPC**（`ListTaskSummaries` + `InspectTask`），与
//!   `subagent_real_e2e.rs` 的 `task_snapshot` 同法——本 crate 无 `rusqlite`，也不需要。

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use yi_agent_store::ipc::{IpcRequest, IpcResponse, send_request};

/// 起隔离 fixture 所用的 provider 设置：与现场一致（openai + 网关 + `/v1`）。
const GATEWAY_WITH_V1: &str = "https://llmapi.horizon.auto/v1";
/// 缺 `/v1` 的坏值——这正是 2026-10-17 事故的成因。
const GATEWAY_WITHOUT_V1: &str = "https://llmapi.horizon.auto";
const MODEL: &str = "HORIZON-DeepSeek";

struct Fixture {
    _root: tempfile::TempDir,
    runtime_dir: PathBuf,
    models_path: PathBuf,
    daemon: Child,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

impl Fixture {
    fn home(&self) -> PathBuf {
        self._root.path().join("home")
    }

    fn repository(&self) -> PathBuf {
        self._root.path().join("repository")
    }

    fn socket(&self) -> PathBuf {
        self.runtime_dir.join("runtime.sock")
    }

    /// 写入清单：`api_url` 决定 worker 能否成功（这是唯一的可变量）。
    fn write_catalog(&self, api_url: &str) {
        std::fs::write(
            &self.models_path,
            format!(
                r#"{{"models":[{{"name":"S","provider":"openai","api_url":"{api_url}","model":"{MODEL}","api_key":"{}"}}],"subagent_model":"S"}}"#,
                std::env::var("MODEL_API_KEY").unwrap_or_default()
            ),
        )
        .unwrap();
    }
}

/// 起一个隔离 daemon；无 API key 时返回 `None`（测试据此安全 skip）。
///
/// 隔离要点（照 `subagent_real_e2e.rs` 的 `start_real_runtime_fixture`）：
/// - `YI_AGENT_RUNTIME_DIR` → 隔离 runtime 目录，绝不碰真实项目 runtime；
/// - `HOME` → 隔离目录，于是 `models_path()`（`$HOME/.yi-agent/models.json`）
///   落在这个临时 HOME 下，**不改用户真实清单**。
fn start_fixture() -> Option<Fixture> {
    let api_key = std::env::var("MODEL_API_KEY")
        .ok()
        .filter(|key| !key.trim().is_empty())?;

    let root = tempfile::TempDir::new().expect("fixture tempdir");
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join(".yi-agent")).unwrap();
    let repository = root.path().join("repository");
    std::fs::create_dir(&repository).unwrap();
    // 委派需要一个真正的 git 仓（admission 要解析出一个真实位置）。
    initialize_repository(&repository);

    let runtime_dir = root.path().join("runtime");
    let models_path = home.join(".yi-agent/models.json");

    let daemon = Command::new(common::yi_agent_bin())
        .arg("--workdir")
        .arg(&repository)
        .arg("--provider")
        .arg("openai")
        .arg("--api-url")
        .arg(GATEWAY_WITH_V1)
        .arg("--api-key")
        .arg(&api_key)
        .arg("--model")
        .arg(MODEL)
        .arg("daemon")
        .arg("serve")
        .env("YI_AGENT_RUNTIME_DIR", &runtime_dir)
        .env("HOME", &home)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start isolated runtime daemon");

    let fixture = Fixture {
        _root: root,
        runtime_dir,
        models_path,
        daemon,
    };

    // 等 socket 就绪（与基线 e2e 同法）。
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if matches!(
            send_request(&fixture.socket(), IpcRequest::Status),
            Ok(IpcResponse::Status { .. })
        ) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "isolated runtime daemon did not become ready"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // 起点必须是可用清单。
    fixture.write_catalog(GATEWAY_WITH_V1);
    Some(fixture)
}

fn initialize_repository(repository: &Path) {
    let run = |args: &[&str]| {
        let status = Command::new("git")
            .current_dir(repository)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    };
    std::fs::write(repository.join("README.md"), "live reload fixture\n").unwrap();
    std::fs::write(repository.join(".gitignore"), ".worktrees/\n").unwrap();
    run(&["init", "-q"]);
    run(&["config", "user.name", "Yi"]);
    run(&["config", "user.email", "yi@example.test"]);
    run(&["add", "README.md", ".gitignore"]);
    run(&["commit", "-q", "-m", "baseline"]);
}

/// 委派一次并返回**子任务**（`depth == 1`）的终态字符串。
///
/// root 的 provider 明确走 CLI（带 `/v1`），故清单被改坏时 root 仍能起、仍能
/// `spawn_agent`，失败的是 worker——失败正是第 2 步要观测的信号。
fn delegate_once(fixture: &Fixture) -> String {
    let api_key = std::env::var("MODEL_API_KEY").unwrap_or_default();
    // daemon 跨次保留历史任务，故先记下已有 task id，只认本次新建的那些：
    // 否则第 2 步会立刻读到第 1 步遗留的子任务终态，判据形同虚设。
    let before = all_task_ids(&fixture.socket());
    let prompt = format!(
        "Call the spawn_agent tool exactly once with task \"Reply with the single word PONG\" \
         and mode \"read_only\". Do not do anything else yourself. Then call wait_agent with \
         mode \"all\", and finally reply with the child's result (or report it failed). \
         The model to run is {MODEL}."
    );

    let run = Command::new(common::yi_agent_bin())
        .arg("--workdir")
        .arg(fixture.repository())
        .arg("--provider")
        .arg("openai")
        .arg("--api-url")
        .arg(GATEWAY_WITH_V1)
        .arg("--api-key")
        .arg(&api_key)
        .arg("--model")
        .arg(MODEL)
        .args(["run", "--subagents", &prompt])
        .env("YI_AGENT_RUNTIME_DIR", &fixture.runtime_dir)
        .env("HOME", fixture.home())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run delegation");
    assert!(
        run.status.success() || !run.stdout.is_empty(),
        "delegation run must complete; stdout: {}; stderr: {}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );

    // 等一个 `depth == 1` 的子任务并读到它的终态。root 自己也会落终态，故必须
    // 按 depth 过滤，否则读到的是 root 的 completed。
    let socket = fixture.socket();
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if let Some(state) = direct_child_terminal_state(&socket, &before) {
            return state;
        }
        assert!(
            Instant::now() < deadline,
            "no direct child ever reached a terminal state; snapshot:\n{}",
            task_snapshot(&socket)
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// 当前 daemon 的全部 task id。
fn all_task_ids(socket: &Path) -> Vec<String> {
    match send_request(
        socket,
        IpcRequest::ListTaskSummaries {
            session_id: None,
            active_only: false,
        },
    ) {
        Ok(IpcResponse::TaskSummaries { tasks }) => tasks.into_iter().map(|t| t.task_id).collect(),
        _ => Vec::new(),
    }
}

/// 找一个**本次新建**、`depth == 1` 且已终态的子任务状态；没有则 `None`。
fn direct_child_terminal_state(socket: &Path, before: &[String]) -> Option<String> {
    let IpcResponse::TaskSummaries { tasks } = send_request(
        socket,
        IpcRequest::ListTaskSummaries {
            session_id: None,
            active_only: false,
        },
    )
    .ok()?
    else {
        return None;
    };
    for task in tasks
        .into_iter()
        .filter(|task| !task.is_root && !before.contains(&task.task_id))
    {
        let Ok(IpcResponse::TaskDetail(detail)) = send_request(
            socket,
            IpcRequest::InspectTask {
                task_id: task.task_id.clone(),
            },
        ) else {
            continue;
        };
        if detail.depth == 1 && (detail.state == "failed" || detail.state.starts_with("completed"))
        {
            return Some(detail.state);
        }
    }
    None
}

/// 诊断用快照：把每个任务的状态打成一行，失败时能看清发生了什么。
fn task_snapshot(socket: &Path) -> String {
    let Ok(IpcResponse::TaskSummaries { tasks }) = send_request(
        socket,
        IpcRequest::ListTaskSummaries {
            session_id: None,
            active_only: false,
        },
    ) else {
        return "could not list task summaries".into();
    };
    tasks
        .into_iter()
        .map(|task| {
            let detail = match send_request(
                socket,
                IpcRequest::InspectTask {
                    task_id: task.task_id.clone(),
                },
            ) {
                Ok(IpcResponse::TaskDetail(detail)) => detail,
                Ok(other) => return format!("{}: unexpected detail {other:?}", task.task_id),
                Err(error) => return format!("{}: detail failed: {error}", task.task_id),
            };
            let events = match send_request(
                socket,
                IpcRequest::ReadTaskEvents {
                    task_id: task.task_id.clone(),
                    after_event_id: None,
                },
            ) {
                Ok(IpcResponse::TaskEvents { events }) => events
                    .into_iter()
                    .map(|event| format!("{}:{}", event.event_id, event.kind))
                    .collect::<Vec<_>>()
                    .join(" | "),
                Ok(other) => format!("unexpected events {other:?}"),
                Err(error) => format!("events failed: {error}"),
            };
            format!(
                "task={} root={} depth={} state={} terminal={:?} events=[{}]",
                task.task_id,
                task.is_root,
                detail.depth,
                detail.state,
                detail.terminal_json,
                events
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 真机三步判据。`#[ignore]`：默认跳过（避免成本与密钥依赖）；跑它需 `--ignored`。
#[test]
#[ignore]
fn changing_models_json_takes_effect_without_restarting_the_daemon() {
    let Some(fixture) = start_fixture() else {
        eprintln!("skip: MODEL_API_KEY is not set");
        return;
    };

    // 1. 清单可用 → 委派成功。
    let first = delegate_once(&fixture);
    assert!(
        first.contains("completed"),
        "step 1: a usable catalog must let the delegation succeed, got {first}"
    );

    // 2. 改成不可用（同一 daemon 进程，绝不重启）→ 委派失败。
    fixture.write_catalog(GATEWAY_WITHOUT_V1);
    let second = delegate_once(&fixture);
    assert!(
        second.contains("failed"),
        "step 2: a bad catalog must make the next delegation fail, got {second} \
         (this proves the daemon re-read the file rather than serving a stale cache)"
    );

    // 3. 改回可用 → 委派再次成功。本次事故正是卡在这里。
    fixture.write_catalog(GATEWAY_WITH_V1);
    let third = delegate_once(&fixture);
    assert!(
        third.contains("completed"),
        "step 3: fixing the catalog must take effect on the next delegation, got {third}"
    );
}
