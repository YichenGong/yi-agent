use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use crate::rpc::{Domain, domain_for};
use crate::server::test_support::{is_terminal_state, serve_scoped_with_loopback};
use crate::test_support::{EVENT_WAIT, isolated_models_path};

fn init_git_repo(dir: &std::path::Path) {
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "test@example.com"],
        vec!["config", "user.name", "Test"],
        vec!["commit", "-q", "--allow-empty", "-m", "init"],
    ] {
        let status = std::process::Command::new("git")
            .args(&args)
            .current_dir(dir)
            .status()
            .expect("git must be available");
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }
}

use async_trait::async_trait;
use futures::StreamExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::*;

struct MockProvider;

#[async_trait]
impl yi_agent_core::Provider for MockProvider {
    async fn call_stream(
        &self,
        _req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        let events = vec![
            yi_agent_core::provider::ProviderEvent::TextDelta("hi".into()),
            yi_agent_core::provider::ProviderEvent::Stop {
                reason: yi_agent_core::provider::StopReason::EndTurn,
            },
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

struct UsageProvider;

#[async_trait]
impl yi_agent_core::Provider for UsageProvider {
    async fn call_stream(
        &self,
        _req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        let events = vec![
            yi_agent_core::provider::ProviderEvent::TextDelta("hi".into()),
            yi_agent_core::provider::ProviderEvent::Usage(yi_agent_core::provider::TokenUsage {
                input_tokens: 1_234,
                output_tokens: 56,
                cache_creation_input_tokens: Some(7),
                cache_read_input_tokens: Some(890),
                reasoning_tokens: None,
            }),
            yi_agent_core::provider::ProviderEvent::Stop {
                reason: yi_agent_core::provider::StopReason::EndTurn,
            },
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

struct RecordingProvider {
    seen: Arc<std::sync::Mutex<Vec<usize>>>,
}

#[async_trait]
impl yi_agent_core::Provider for RecordingProvider {
    async fn call_stream(
        &self,
        req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        if req.system.as_deref() == Some(yi_agent_core::title::TITLE_INSTRUCTIONS) {
            return Ok(Box::pin(futures::stream::iter(vec![
                yi_agent_core::provider::ProviderEvent::TextDelta("t".into()),
                yi_agent_core::provider::ProviderEvent::Stop {
                    reason: yi_agent_core::provider::StopReason::EndTurn,
                },
            ])));
        }
        let n = req.messages.len();
        self.seen.lock().unwrap().push(n);
        let events = vec![
            yi_agent_core::provider::ProviderEvent::TextDelta(format!("n={n}")),
            yi_agent_core::provider::ProviderEvent::Stop {
                reason: yi_agent_core::provider::StopReason::EndTurn,
            },
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

struct CapturingProvider {
    prompts: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait]
impl yi_agent_core::Provider for CapturingProvider {
    async fn call_stream(
        &self,
        req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        use yi_agent_core::message::{ContentBlock, Role};
        if req.system.as_deref() == Some(yi_agent_core::title::TITLE_INSTRUCTIONS) {
            return Ok(Box::pin(futures::stream::iter(vec![
                yi_agent_core::provider::ProviderEvent::TextDelta("t".into()),
                yi_agent_core::provider::ProviderEvent::Stop {
                    reason: yi_agent_core::provider::StopReason::EndTurn,
                },
            ])));
        }
        if let Some(last_user) = req.messages.iter().rev().find(|m| m.role == Role::User) {
            let text = last_user
                .content
                .iter()
                .find_map(|b| match b {
                    ContentBlock::Text(t) => Some(t.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            self.prompts.lock().unwrap().push(text);
        }
        let events = vec![
            yi_agent_core::provider::ProviderEvent::TextDelta("ok".into()),
            yi_agent_core::provider::ProviderEvent::Stop {
                reason: yi_agent_core::provider::StopReason::EndTurn,
            },
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

struct CapturingMessagesProvider {
    messages: Arc<std::sync::Mutex<Vec<Vec<yi_agent_core::Message>>>>,
}

#[async_trait]
impl yi_agent_core::Provider for CapturingMessagesProvider {
    async fn call_stream(
        &self,
        req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        self.messages.lock().unwrap().push(req.messages.clone());
        let events = vec![
            yi_agent_core::provider::ProviderEvent::TextDelta("ok".into()),
            yi_agent_core::provider::ProviderEvent::Stop {
                reason: yi_agent_core::provider::StopReason::EndTurn,
            },
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

struct SlowProvider;

#[async_trait]
impl yi_agent_core::Provider for SlowProvider {
    async fn call_stream(
        &self,
        _req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        let events = futures::stream::unfold(0u64, |i| async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Some((
                yi_agent_core::provider::ProviderEvent::TextDelta("x".into()),
                i + 1,
            ))
        });
        Ok(events.boxed())
    }
}

struct ErroringProvider;

#[async_trait]
impl yi_agent_core::Provider for ErroringProvider {
    async fn call_stream(
        &self,
        _req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        Err(yi_agent_core::provider::ProviderError::Network(
            "boom".into(),
        ))
    }
}

struct TitleProvider;

#[async_trait]
impl yi_agent_core::Provider for TitleProvider {
    async fn call_stream(
        &self,
        req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        // 标题请求(恰好命中 TITLE_INSTRUCTIONS)返回固定标题;其余(主 agent
        // 对话)返回正文 "ok"。
        let text = if req.system.as_deref() == Some(yi_agent_core::title::TITLE_INSTRUCTIONS) {
            "修复登录报错"
        } else {
            "ok"
        };
        let events = vec![
            yi_agent_core::provider::ProviderEvent::TextDelta(text.into()),
            yi_agent_core::provider::ProviderEvent::Stop {
                reason: yi_agent_core::provider::StopReason::EndTurn,
            },
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

struct RejectingTitleProvider {
    title_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl yi_agent_core::Provider for RejectingTitleProvider {
    async fn call_stream(
        &self,
        req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        if req.system.as_deref() == Some(yi_agent_core::title::TITLE_INSTRUCTIONS) {
            self.title_calls.fetch_add(1, Ordering::SeqCst);
            return Err(yi_agent_core::provider::ProviderError::Network(
                "title boom".into(),
            ));
        }
        let events = vec![
            yi_agent_core::provider::ProviderEvent::TextDelta("ok".into()),
            yi_agent_core::provider::ProviderEvent::Stop {
                reason: yi_agent_core::provider::StopReason::EndTurn,
            },
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

struct DelayedProvider;

#[async_trait]
impl yi_agent_core::Provider for DelayedProvider {
    async fn call_stream(
        &self,
        _req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        let events = futures::stream::once(async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            yi_agent_core::provider::ProviderEvent::TextDelta("hi".into())
        })
        .chain(futures::stream::once(async {
            yi_agent_core::provider::ProviderEvent::Stop {
                reason: yi_agent_core::provider::StopReason::EndTurn,
            }
        }));
        Ok(events.boxed())
    }
}

struct CheckpointProvider {
    calls: AtomicUsize,
}

#[async_trait]
impl yi_agent_core::Provider for CheckpointProvider {
    async fn call_stream(
        &self,
        req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        // 标题请求不是对话轮：不参与下方按次数脚本化的计数，否则首轮会把
        // 「第 0 次」让给标题调用，脚本化序列整体错位。
        if req.system.as_deref() == Some(yi_agent_core::title::TITLE_INSTRUCTIONS) {
            return Ok(futures::stream::iter(vec![
                yi_agent_core::provider::ProviderEvent::TextDelta("t".into()),
                yi_agent_core::provider::ProviderEvent::Stop {
                    reason: yi_agent_core::provider::StopReason::EndTurn,
                },
            ])
            .boxed());
        }
        use yi_agent_core::provider::ProviderEvent as E;
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            // 工具入参必须是合法 JSON（`{}`），否则 accumulate 会因解析失败报错。
            let head = futures::stream::iter(vec![
                E::TextDelta("a".into()),
                E::ToolUseStart {
                    id: "t1".into(),
                    name: "noop".into(),
                },
                E::ToolUseDelta {
                    id: "t1".into(),
                    partial_json: "{}".into(),
                },
                E::ToolUseEnd { id: "t1".into() },
                E::Stop {
                    reason: yi_agent_core::provider::StopReason::EndTurn,
                },
            ]);
            Ok(head.boxed())
        } else {
            // 永不产出的尾部：保证 turn 不收尾（否则会被正常 persist）。
            Ok(futures::stream::pending::<E>().boxed())
        }
    }
}

fn test_hub<W>(
    writer: W,
) -> (
    Arc<crate::broadcast::Broadcaster>,
    crate::broadcast::ClientId,
)
where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let hub = Arc::new(crate::broadcast::Broadcaster::new());
    let client = crate::broadcast::ClientId::local();
    // 与生产的 stdio 接线同款:可靠登记,背压不摘除(见 `serve_scoped`)。
    let outbound = hub.register_reliable(client.clone());
    tokio::spawn(pump_stdout(
        outbound,
        writer,
        Arc::clone(&hub),
        client.clone(),
    ));
    (hub, client)
}

pub(crate) fn build_test_agent(
    session: Option<yi_agent_core::Session>,
    _cwd: &std::path::Path,
    _mode: crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent> {
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(MockProvider);
    // 让本 mock 如实汇报「它实际会跑的模型」:与 `crate::test_support::test_config()`
    // 的 `cfg.model` 同源(`TEST_MODEL`)。`thread/start`/`thread/resume` 现在以
    // `agent.config().model` 为上报来源(任务 7 评审 Finding 1),若这里沿用
    // `AgentConfig::default()` 的默认串,就会让夹具与生产语义(会话按 cfg 的模型
    // 起跑)脱节。
    let config = yi_agent_core::AgentConfig {
        model: crate::test_support::TEST_MODEL.to_string(),
        ..yi_agent_core::AgentConfig::default()
    };
    let mut agent = yi_agent_core::Agent::new(
        provider.clone(),
        Arc::new(yi_agent_core::ToolRegistry::new()),
        config.clone(),
    );
    apply_session(&mut agent, session);
    Ok(BuiltAgent {
        agent,
        provider,
        config,
        decision_tx: None,
        decision_rx: None,
        catalog: None,
        yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
        process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        mcp: None,
        sandbox: yi_agent_tools::SandboxController::new(
            yi_agent_core::autonomy::YoloSwitch::new(false),
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            true,
        ),
    })
}

pub(crate) fn build_test_agent_with_provider(
    provider: Arc<dyn yi_agent_core::Provider>,
    session: Option<yi_agent_core::Session>,
    _cwd: &std::path::Path,
    _mode: crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent> {
    let config = yi_agent_core::AgentConfig {
        model: crate::test_support::TEST_MODEL.to_string(),
        ..yi_agent_core::AgentConfig::default()
    };
    let mut agent = yi_agent_core::Agent::new(
        provider.clone(),
        Arc::new(yi_agent_core::ToolRegistry::new()),
        config.clone(),
    );
    apply_session(&mut agent, session);
    Ok(BuiltAgent {
        agent,
        provider,
        config,
        decision_tx: None,
        decision_rx: None,
        catalog: None,
        yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
        process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        mcp: None,
        sandbox: yi_agent_tools::SandboxController::new(
            yi_agent_core::autonomy::YoloSwitch::new(false),
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            true,
        ),
    })
}

/// 供 `session_driver_tests` 使用:带真实用量的工厂。
pub(crate) fn build_usage_agent_for_tests(
    session: Option<yi_agent_core::Session>,
    cwd: &std::path::Path,
    mode: crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent> {
    build_usage_agent(session, cwd, mode)
}

fn build_usage_agent(
    session: Option<yi_agent_core::Session>,
    cwd: &std::path::Path,
    mode: crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent> {
    build_test_agent_with_provider(Arc::new(UsageProvider), session, cwd, mode)
}

pub(crate) fn build_slow_agent(
    session: Option<yi_agent_core::Session>,
    _cwd: &std::path::Path,
    _mode: crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent> {
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(SlowProvider);
    let config = yi_agent_core::AgentConfig::default();
    let mut agent = yi_agent_core::Agent::new(
        provider.clone(),
        Arc::new(yi_agent_core::ToolRegistry::new()),
        config.clone(),
    );
    apply_session(&mut agent, session);
    Ok(BuiltAgent {
        agent,
        provider,
        config,
        decision_tx: None,
        decision_rx: None,
        catalog: None,
        yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
        process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        mcp: None,
        sandbox: yi_agent_tools::SandboxController::new(
            yi_agent_core::autonomy::YoloSwitch::new(false),
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            true,
        ),
    })
}

fn build_delayed_agent(
    session: Option<yi_agent_core::Session>,
    _cwd: &std::path::Path,
    _mode: crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent> {
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(DelayedProvider);
    let config = yi_agent_core::AgentConfig::default();
    let mut agent = yi_agent_core::Agent::new(
        provider.clone(),
        Arc::new(yi_agent_core::ToolRegistry::new()),
        config.clone(),
    );
    apply_session(&mut agent, session);
    Ok(BuiltAgent {
        agent,
        provider,
        config,
        decision_tx: None,
        decision_rx: None,
        catalog: None,
        yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
        process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        mcp: None,
        sandbox: yi_agent_tools::SandboxController::new(
            yi_agent_core::autonomy::YoloSwitch::new(false),
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            true,
        ),
    })
}

fn build_checkpoint_agent(
    session: Option<yi_agent_core::Session>,
    _cwd: &std::path::Path,
    _mode: crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent> {
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(CheckpointProvider {
        calls: AtomicUsize::new(0),
    });
    let config = yi_agent_core::AgentConfig::default();
    let mut agent = yi_agent_core::Agent::new(
        provider.clone(),
        Arc::new(yi_agent_core::ToolRegistry::new()),
        config.clone(),
    );
    apply_session(&mut agent, session);
    Ok(BuiltAgent {
        agent,
        provider,
        config,
        decision_tx: None,
        decision_rx: None,
        catalog: None,
        yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
        process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        mcp: None,
        sandbox: yi_agent_tools::SandboxController::new(
            yi_agent_core::autonomy::YoloSwitch::new(false),
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            true,
        ),
    })
}

fn test_theme() -> crate::theme_tool::ThemeHandle {
    crate::theme_tool::ThemeHandle::new(std::env::temp_dir())
}

pub(crate) fn test_config() -> RuntimeConfig {
    crate::test_support::test_config()
}

pub(crate) fn default_config() -> RuntimeConfig {
    crate::test_support::test_config()
}

pub(crate) struct Harness {
    client_w: tokio::io::DuplexStream,
    client_r: BufReader<tokio::io::DuplexStream>,
    pending: std::collections::VecDeque<serde_json::Value>,
    handle: tokio::task::JoinHandle<anyhow::Result<()>>,
    _index_dir: tempfile::TempDir,
    pub(crate) _models_dir: tempfile::TempDir,
    pub(crate) resident_dir: tempfile::TempDir,
    pairing: Arc<PairingState>,
    pub(crate) hub: Arc<crate::broadcast::Broadcaster>,
    pub(crate) watchman_calls: WatchmanCalls,
    /// 调用方没指定 workdir 时由 Harness 生成的隔离目录。
    ///
    /// 必须持有所有权：`cfg` 只存 `PathBuf`，临时目录一旦在构造函数返回时
    /// 被删除，后续 `thread/start` 就写不进去。`None` 表示调用方自带 workdir。
    pub(crate) _fallback_workdir: Option<tempfile::TempDir>,
    pub(crate) _watchman_home: tempfile::TempDir,
    /// 本次连接的官方目录路径。测试要往它写一份目录再断言 RPC 读到的是这份。
    ///
    /// lib test 里读它的是 `plugin_remote_tests`（同为 lib 模块），但那里的
    /// 用例经本文件编译不到同一份 `dead_code` 视图，故显式标注。
    #[allow(dead_code)]
    pub(crate) plugin_catalog: std::path::PathBuf,
    pub(crate) skills_home_dir: tempfile::TempDir,
    pub(crate) lead_inbox: crate::lead::LeadInbox,
    pub(crate) lead_replies: crate::lead::LeadReplies,
    pub(crate) lead_control: crate::lead_control::LeadControlInbox,
    pub(crate) lead_control_replies: crate::lead_control::LeadControlReplies,
    pub(crate) view_frames: crate::server::ViewFrames,
    /// The shell-brokered authority slot, shared with the server.
    ///
    /// Same technique as `view_frames`: the server writes it, the test observes it. A
    /// second `Arc` here would be a second slot, and every assertion about "the
    /// handshake installed this session's identity" would pass against an empty one.
    pub(crate) managed: crate::server::ManagedSlot,
    pub(crate) plugins_dir: tempfile::TempDir,
    pub(crate) daemon_calls: DaemonCalls,
    /// The host policy this harness installed. Kept (rather than dropped after
    /// wiring) so a test can assert on the exact choice without rebuilding it;
    /// no test reads it today, hence the allow.
    #[allow(dead_code)]
    pub(crate) bringup: Arc<yi_agent_subagent::binding::BringUpChoice>,
    /// 本进程正在驱动的父会话表，与宿主共享同一实例。
    ///
    /// 同一手法（`view_frames` / `managed`）：服务端发布，测试观察。自造一份
    /// 会让「`thread/start` 之后旁问能立刻读到活跃父」这条断言在空表上永远
    /// 失败得莫名其妙。
    pub(crate) live_side_parents: Arc<crate::side_conversation::LiveSideParents>,
}

pub(crate) type WatchmanCalls = Arc<StdMutex<Vec<(bool, PathBuf)>>>;

pub(crate) type DaemonCalls = Arc<StdMutex<Vec<PathBuf>>>;

impl Harness {
    pub(crate) fn new() -> Self {
        Self::with_factory(build_test_agent, PERMISSION_TIMEOUT)
    }

    pub(crate) async fn with_cfg(cfg: RuntimeConfig) -> Self {
        Self::with_config(cfg, build_test_agent, PERMISSION_TIMEOUT)
    }

    fn with_factory<F>(build: F, permission_timeout: Duration) -> Self
    where
        F: Fn(
                Option<yi_agent_core::Session>,
                &std::path::Path,
                crate::thread_store::ThreadMode,
            ) -> anyhow::Result<BuiltAgent>
            + Send
            + Sync
            + 'static,
    {
        Self::with_config(test_config(), build, permission_timeout)
    }

    fn with_config<F>(cfg: RuntimeConfig, build: F, permission_timeout: Duration) -> Self
    where
        F: Fn(
                Option<yi_agent_core::Session>,
                &std::path::Path,
                crate::thread_store::ThreadMode,
            ) -> anyhow::Result<BuiltAgent>
            + Send
            + Sync
            + 'static,
    {
        Self::with_config_and_scope(cfg, build, permission_timeout, Scope::Admin)
    }

    pub(crate) async fn with_scope(scope: Scope) -> Self {
        Self::with_config_and_scope(test_config(), build_test_agent, PERMISSION_TIMEOUT, scope)
    }

    /// 带注入旁支注册表的 harness。scope 可指定,用来验证 `side/*` 的门禁。
    pub(crate) async fn with_scope_and_side_registry(
        scope: Scope,
        registry: Arc<crate::side_conversation::SideRegistry>,
    ) -> Self {
        let calls: WatchmanCalls = Arc::new(StdMutex::new(Vec::new()));
        let install_calls = Arc::clone(&calls);
        let uninstall_calls = Arc::clone(&calls);
        Self::with_config_and_scope_and_watchman(
            test_config(),
            build_test_agent,
            PERMISSION_TIMEOUT,
            scope,
            Arc::new(move |_exe: &Path, home: &Path| {
                install_calls
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((true, home.to_path_buf()));
                Ok(())
            }),
            Arc::new(move |home: &Path| {
                uninstall_calls
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((false, home.to_path_buf()));
                Ok(())
            }),
            calls,
            false,
            Some(registry),
            Arc::new(|| true),
        )
    }

    /// 与 [`Harness::with_scope`] 相同，但实例根由调用方钉死。
    ///
    /// 用来模拟"同一台宿主的第二次连接"：两条 stdio 管道、两个主循环，读写的
    /// 却是同一份 `plugins/registry.json` 与授权表。默认构造每次都给一个新
    /// tempdir，那只能证明"两个宿主互不可见"，证明不了重连后读到权威状态。
    // 只被 `plugin_remote_tests` 的"两次连接"用例使用；同一理由见
    // `plugin_catalog` 的 `allow`。
    #[allow(dead_code)]
    pub(crate) async fn with_scope_and_instance_home(scope: Scope, home: &Path) -> Self {
        let calls: WatchmanCalls = Arc::new(StdMutex::new(Vec::new()));
        let install_calls = Arc::clone(&calls);
        let uninstall_calls = Arc::clone(&calls);
        Self::with_config_and_scope_and_watchman_in(
            test_config(),
            build_test_agent,
            PERMISSION_TIMEOUT,
            scope,
            Arc::new(move |_exe: &Path, home: &Path| {
                install_calls
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((true, home.to_path_buf()));
                Ok(())
            }),
            Arc::new(move |home: &Path| {
                uninstall_calls
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((false, home.to_path_buf()));
                Ok(())
            }),
            calls,
            false,
            None,
            Some(home.to_path_buf()),
            None,
            // 静态装配：只有 `with_live_settings` 注入共享实例。
            None,
            Arc::new(|| true),
            None,
        )
    }

    /// 带一个本地管理面的 harness。
    ///
    /// 管理面是**每连接一份**，所以它从这里进 `serve()`，而不是进程级全局：
    /// 后者会让并行用例共用一条会话，bootstrap 只有第一个能成功。
    pub(crate) async fn with_management(
        handle: Arc<crate::server::local_management::ManagementHandle>,
    ) -> Self {
        let calls: WatchmanCalls = Arc::new(StdMutex::new(Vec::new()));
        let install_calls = Arc::clone(&calls);
        let uninstall_calls = Arc::clone(&calls);
        Self::with_config_and_scope_and_watchman_in(
            test_config(),
            build_test_agent,
            PERMISSION_TIMEOUT,
            Scope::Admin,
            Arc::new(move |_exe: &Path, home: &Path| {
                install_calls
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((true, home.to_path_buf()));
                Ok(())
            }),
            Arc::new(move |home: &Path| {
                uninstall_calls
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((false, home.to_path_buf()));
                Ok(())
            }),
            calls,
            false,
            None,
            None,
            Some(handle),
            // 静态装配：只有 `with_live_settings` 注入共享实例。
            None,
            Arc::new(|| true),
            None,
        )
    }

    pub(crate) async fn with_lead_flag(enabled: bool) -> Self {
        Self::with_config_lead_and_scope(
            test_config(),
            build_test_agent,
            PERMISSION_TIMEOUT,
            Scope::Admin,
            enabled,
        )
    }

    /// 旁问开关按值注入。scope 固定 `Admin`。
    ///
    /// 走注入而不是写 `flags.json`：后者要求 build kind 为 `dev` 且共享进程级
    /// `YI_AGENT_HOME`，并行测试必然互相污染（同 `with_lead_flag` 的理由）。
    ///
    /// 不传注册表（`None`）：`serve()` 会按实例家目录自建一份，与既有
    /// `with_scope` 同一形状。需要注入注册表的用例走下面那个构造器。
    #[allow(dead_code)]
    pub(crate) async fn with_side_flag(enabled: bool) -> Self {
        let calls: WatchmanCalls = Arc::new(StdMutex::new(Vec::new()));
        let install_calls = Arc::clone(&calls);
        let uninstall_calls = Arc::clone(&calls);
        Self::with_config_and_scope_and_watchman_in(
            test_config(),
            build_test_agent,
            PERMISSION_TIMEOUT,
            Scope::Admin,
            Arc::new(move |_exe: &Path, home: &Path| {
                install_calls
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((true, home.to_path_buf()));
                Ok(())
            }),
            Arc::new(move |home: &Path| {
                uninstall_calls
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((false, home.to_path_buf()));
                Ok(())
            }),
            calls,
            false,
            None,
            None,
            None,
            // 静态装配：只有 `with_live_settings` 注入共享实例。
            None,
            Arc::new(move || enabled),
            None,
        )
    }

    /// 三个维度都可指定：scope、注册表、旁问开关。
    ///
    /// `side/*` 的闸门测试要同时钉「权限档不构成回答」与「关掉时方法不存在」，
    /// 必须能同时控制前两者。
    pub(crate) async fn with_scope_side_flag_and_registry(
        scope: Scope,
        registry: Arc<crate::side_conversation::SideRegistry>,
        side_enabled: bool,
    ) -> Self {
        let calls: WatchmanCalls = Arc::new(StdMutex::new(Vec::new()));
        let install_calls = Arc::clone(&calls);
        let uninstall_calls = Arc::clone(&calls);
        Self::with_config_and_scope_and_watchman_in(
            test_config(),
            build_test_agent,
            PERMISSION_TIMEOUT,
            scope,
            Arc::new(move |_exe: &Path, home: &Path| {
                install_calls
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((true, home.to_path_buf()));
                Ok(())
            }),
            Arc::new(move |home: &Path| {
                uninstall_calls
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((false, home.to_path_buf()));
                Ok(())
            }),
            calls,
            false,
            Some(registry),
            None,
            None,
            // 静态装配：只有 `with_live_settings` 注入共享实例。
            None,
            Arc::new(move || side_enabled),
            None,
        )
    }

    /// 垂直集成专用:注入共享活父表、旁支注册表与自定义 agent 工厂。
    ///
    /// 与 [`Harness::with_scope_side_flag_and_registry`] 的差别只有两点,都是
    /// 「真实链路」要求的:
    ///
    /// 1. `live_side_parents` 由调用方持有并注入宿主,于是旁支 resolver 查到的
    ///    就是**宿主自己**在每轮收尾后重发布的那份快照。自造第二份只能证明替身
    ///    会返回答案,证明不了「父未落盘的进度能被旁支看到」。
    /// 2. `build` 可换成记录 provider 的工厂,用来断言父与旁支各自收到的
    ///    `ProviderRequest`(工具集为空、历史逐字节不变)。
    pub(crate) async fn with_live_side_registry<F>(
        scope: Scope,
        registry: Arc<crate::side_conversation::SideRegistry>,
        live_side_parents: Arc<crate::side_conversation::LiveSideParents>,
        build: F,
    ) -> Self
    where
        F: Fn(
                Option<yi_agent_core::Session>,
                &std::path::Path,
                crate::thread_store::ThreadMode,
            ) -> anyhow::Result<BuiltAgent>
            + Send
            + Sync
            + 'static,
    {
        let calls: WatchmanCalls = Arc::new(StdMutex::new(Vec::new()));
        let install_calls = Arc::clone(&calls);
        let uninstall_calls = Arc::clone(&calls);
        Self::with_config_and_scope_and_watchman_in(
            test_config(),
            build,
            PERMISSION_TIMEOUT,
            scope,
            Arc::new(move |_exe: &Path, home: &Path| {
                install_calls
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((true, home.to_path_buf()));
                Ok(())
            }),
            Arc::new(move |home: &Path| {
                uninstall_calls
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((false, home.to_path_buf()));
                Ok(())
            }),
            calls,
            false,
            Some(registry),
            None,
            None,
            None,
            Arc::new(|| true),
            Some(live_side_parents),
        )
    }

    pub(crate) async fn with_cfg_and_lead(cfg: RuntimeConfig, enabled: bool) -> Self {
        Self::with_config_lead_and_scope(
            cfg,
            build_test_agent,
            PERMISSION_TIMEOUT,
            Scope::Admin,
            enabled,
        )
    }

    pub(crate) async fn with_config_and_lead<F>(
        cfg: RuntimeConfig,
        build: F,
        permission_timeout: Duration,
        enabled: bool,
    ) -> Self
    where
        F: Fn(
                Option<yi_agent_core::Session>,
                &std::path::Path,
                crate::thread_store::ThreadMode,
            ) -> anyhow::Result<BuiltAgent>
            + Send
            + Sync
            + 'static,
    {
        Self::with_config_lead_and_scope(cfg, build, permission_timeout, Scope::Admin, enabled)
    }

    fn with_config_lead_and_scope<F>(
        cfg: RuntimeConfig,
        build: F,
        permission_timeout: Duration,
        scope: Scope,
        lead_agent_enabled: bool,
    ) -> Self
    where
        F: Fn(
                Option<yi_agent_core::Session>,
                &std::path::Path,
                crate::thread_store::ThreadMode,
            ) -> anyhow::Result<BuiltAgent>
            + Send
            + Sync
            + 'static,
    {
        let calls: WatchmanCalls = Arc::new(StdMutex::new(Vec::new()));
        let install_calls = Arc::clone(&calls);
        let uninstall_calls = Arc::clone(&calls);
        Self::with_config_and_scope_and_watchman(
            cfg,
            build,
            permission_timeout,
            scope,
            Arc::new(move |_exe: &Path, home: &Path| {
                install_calls
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((true, home.to_path_buf()));
                Ok(())
            }),
            Arc::new(move |home: &Path| {
                uninstall_calls
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((false, home.to_path_buf()));
                Ok(())
            }),
            calls,
            lead_agent_enabled,
            None,
            Arc::new(|| true),
        )
    }

    /// Harness whose server holds a live-settings binding.
    ///
    /// `settings/read` and `settings/write` then go through the shared instance
    /// (the production path) instead of the stateless file facade, which is what
    /// the `settings/changed` notification and the policy/origin fields are
    /// only observable through.
    pub(crate) async fn with_live_settings(
        binding: yi_agent_runtime::live_settings::binding::RuntimeSettingsBinding,
    ) -> Self {
        let calls: WatchmanCalls = Arc::new(StdMutex::new(Vec::new()));
        let install_calls = Arc::clone(&calls);
        let uninstall_calls = Arc::clone(&calls);
        Self::with_config_and_scope_and_watchman_in(
            test_config(),
            build_test_agent,
            PERMISSION_TIMEOUT,
            Scope::Admin,
            Arc::new(move |_exe: &Path, home: &Path| {
                install_calls
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((true, home.to_path_buf()));
                Ok(())
            }),
            Arc::new(move |home: &Path| {
                uninstall_calls
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((false, home.to_path_buf()));
                Ok(())
            }),
            calls,
            false,
            None,
            None,
            None,
            Some(binding),
            // 旁支默认开：这条构造器只关心运行期设置，不该顺带改变旁支行为。
            Arc::new(|| true),
            None,
        )
    }

    pub(crate) async fn with_watchman(
        cfg: RuntimeConfig,
        install: WatchmanInstall,
        uninstall: WatchmanUninstall,
    ) -> Self {
        Self::with_config_and_scope_and_watchman(
            cfg,
            build_test_agent,
            PERMISSION_TIMEOUT,
            Scope::Admin,
            install,
            uninstall,
            Arc::new(StdMutex::new(Vec::new())),
            false,
            None,
            Arc::new(|| true),
        )
    }

    fn with_config_and_scope<F>(
        cfg: RuntimeConfig,
        build: F,
        permission_timeout: Duration,
        scope: Scope,
    ) -> Self
    where
        F: Fn(
                Option<yi_agent_core::Session>,
                &std::path::Path,
                crate::thread_store::ThreadMode,
            ) -> anyhow::Result<BuiltAgent>
            + Send
            + Sync
            + 'static,
    {
        // 默认注入:只记录调用,不碰真实 launchd / home。
        let calls: WatchmanCalls = Arc::new(StdMutex::new(Vec::new()));
        let install_calls = Arc::clone(&calls);
        let uninstall_calls = Arc::clone(&calls);
        Self::with_config_and_scope_and_watchman(
            cfg,
            build,
            permission_timeout,
            scope,
            Arc::new(move |_exe: &Path, home: &Path| {
                install_calls
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((true, home.to_path_buf()));
                Ok(())
            }),
            Arc::new(move |home: &Path| {
                uninstall_calls
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((false, home.to_path_buf()));
                Ok(())
            }),
            calls,
            false,
            None,
            Arc::new(|| true),
        )
    }

    // 9 个参数：cfg + 工厂 + 超时 + scope + 两个 watchman 闭包 + 记录表 + 组长
    // 开关 + 旁支注册表。再拆一个 builder 只会让"哪些组合真的存在"更难读。
    #[allow(clippy::too_many_arguments)]
    fn with_config_and_scope_and_watchman<F>(
        cfg: RuntimeConfig,
        build: F,
        permission_timeout: Duration,
        scope: Scope,
        install: WatchmanInstall,
        uninstall: WatchmanUninstall,
        calls: WatchmanCalls,
        // 组长开关。走这里注入而不是写 `flags.json`：后者要求 build_kind == "dev"
        // 且共享进程级 `YI_AGENT_HOME`，并行测试必然互相污染；本 crate 的 lib
        // 测试从不注入 build info，`build_kind()` 恒为 "release"，写文件也无效。
        lead_agent_enabled: bool,
        // 旁支注册表:注入后 `side/*` 走替身 resolver,不读真实项目目录。
        // `None` 时 `serve()` 按本进程实例家目录自建(生产路径)。
        side_registry: Option<Arc<crate::side_conversation::SideRegistry>>,
        side_gate: crate::side_gate::SideGate,
    ) -> Self
    where
        F: Fn(
                Option<yi_agent_core::Session>,
                &std::path::Path,
                crate::thread_store::ThreadMode,
            ) -> anyhow::Result<BuiltAgent>
            + Send
            + Sync
            + 'static,
    {
        Self::with_bringup_in(
            cfg,
            build,
            permission_timeout,
            scope,
            install,
            uninstall,
            calls,
            lead_agent_enabled,
            side_registry,
            // instance_home_override: 本构造器让 harness 自造 tempdir。
            None,
            // management: 这些用例不承载本地管理句柄。
            None,
            Arc::new(yi_agent_subagent::binding::BringUpChoice::default()),
            // 默认静态装配；只有 `with_live_settings` 才注入共享实例。
            None,
            side_gate,
            // 默认自建活父表；只有垂直集成才注入宿主那一份。
            None,
        )
    }

    /// A harness on a host that declared `policy` instead of the default.
    ///
    /// Needed because the host policy and the connection's shell authority are
    /// different questions: a test has to be able to build "managed host, plain
    /// remote client" without touching process-wide state.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn with_host_policy(
        policy: yi_agent_subagent::attach::BringUpPolicy,
        scope: Scope,
        instance_home: &Path,
    ) -> Self {
        let calls: WatchmanCalls = Arc::new(StdMutex::new(Vec::new()));
        let install_calls = Arc::clone(&calls);
        let uninstall_calls = Arc::clone(&calls);
        Self::with_bringup_in(
            test_config(),
            build_test_agent,
            PERMISSION_TIMEOUT,
            scope,
            Arc::new(move |_exe: &Path, home: &Path| {
                install_calls
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((true, home.to_path_buf()));
                Ok(())
            }),
            Arc::new(move |home: &Path| {
                uninstall_calls
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((false, home.to_path_buf()));
                Ok(())
            }),
            calls,
            false,
            None,
            Some(instance_home.to_path_buf()),
            None,
            Arc::new(yi_agent_subagent::binding::BringUpChoice {
                policy,
                expected: None,
                instance_home: Some(instance_home.to_path_buf()),
            }),
            None,
            Arc::new(|| true),
            None,
        )
    }

    /// 见 [`Harness::with_scope_and_instance_home`]。`instance_home_override`
    /// 为 `Some` 时不再自造 tempdir，而是复用调用方给的根。
    ///
    /// 12 个参数：在 main 的 11 参版上再叠一个 live-settings 注入。刻意不把
    /// `live_settings` 单独拆成第二个构造函数——两个近乎相同的 11 参签名的维护
    /// 成本高于一次多余的可选参数，而调用点全都显式写 `None`。
    #[allow(clippy::too_many_arguments)]
    fn with_config_and_scope_and_watchman_in<F>(
        cfg: RuntimeConfig,
        build: F,
        permission_timeout: Duration,
        scope: Scope,
        install: WatchmanInstall,
        uninstall: WatchmanUninstall,
        calls: WatchmanCalls,
        lead_agent_enabled: bool,
        side_registry: Option<Arc<crate::side_conversation::SideRegistry>>,
        instance_home_override: Option<PathBuf>,
        management: Option<Arc<crate::server::local_management::ManagementHandle>>,
        live_settings: Option<yi_agent_runtime::live_settings::binding::RuntimeSettingsBinding>,
        side_gate: crate::side_gate::SideGate,
        // 活跃父来源表:注入优先。垂直集成必须让 side 的 resolver 查**宿主自己**
        // 发布的那一份;自造第二份,测到的只是替身。
        live_side_parents: Option<Arc<crate::side_conversation::LiveSideParents>>,
    ) -> Self
    where
        F: Fn(
                Option<yi_agent_core::Session>,
                &std::path::Path,
                crate::thread_store::ThreadMode,
            ) -> anyhow::Result<BuiltAgent>
            + Send
            + Sync
            + 'static,
    {
        Self::with_bringup_in(
            cfg,
            build,
            permission_timeout,
            scope,
            install,
            uninstall,
            calls,
            lead_agent_enabled,
            side_registry,
            instance_home_override,
            management,
            Arc::new(yi_agent_subagent::binding::BringUpChoice::default()),
            live_settings,
            side_gate,
            live_side_parents,
        )
    }

    /// 见 [`Harness::with_config_and_scope_and_watchman_in`],外加宿主策略。
    #[allow(clippy::too_many_arguments)]
    fn with_bringup_in<F>(
        cfg: RuntimeConfig,
        build: F,
        permission_timeout: Duration,
        scope: Scope,
        install: WatchmanInstall,
        uninstall: WatchmanUninstall,
        calls: WatchmanCalls,
        lead_agent_enabled: bool,
        side_registry: Option<Arc<crate::side_conversation::SideRegistry>>,
        instance_home_override: Option<PathBuf>,
        management: Option<Arc<crate::server::local_management::ManagementHandle>>,
        bringup: Arc<yi_agent_subagent::binding::BringUpChoice>,
        live_settings: Option<yi_agent_runtime::live_settings::binding::RuntimeSettingsBinding>,
        side_gate: crate::side_gate::SideGate,
        // 活跃父来源表:注入优先。垂直集成必须让 side 的 resolver 查**宿主自己**
        // 发布的那一份;自造第二份,测到的只是替身。
        live_side_parents: Option<Arc<crate::side_conversation::LiveSideParents>>,
    ) -> Self
    where
        F: Fn(
                Option<yi_agent_core::Session>,
                &std::path::Path,
                crate::thread_store::ThreadMode,
            ) -> anyhow::Result<BuiltAgent>
            + Send
            + Sync
            + 'static,
    {
        let (client_w, server_r) = tokio::io::duplex(64 * 1024);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        let index_dir = tempfile::TempDir::new().unwrap();
        // workdir 必须逐个用例隔离。共享的 `/tmp/yi-agent-app-server-test`
        // 会让 `thread/start` 经 attach_delegation 同步拉起/复用**同一个**
        // cwd 的 subagent daemon；全量并发下上百个用例抢同一 socket，实测
        // attach 双峰分布 12~27s vs 70~190ms，前者超过 `EVENT_WAIT`(30s)
        // 即 panic `timed out waiting for a message`。改隔离后同一用例
        // 从 24s 降到 0.6s。
        //
        // 只在调用方**没有**显式指定 workdir 时才替换：很多用例自己建
        // `TempDir` 赋给 `cfg.workdir` 再断言该目录下的落盘文件
        // （如 `title.rs` 读 `.yi-agent/threads/<tid>.meta.json`），
        // 无条件覆盖会让它们一起失败（实测 80 个）。
        let fallback_workdir =
            if cfg.workdir == std::path::Path::new(crate::test_support::DEFAULT_TEST_WORKDIR) {
                Some(tempfile::TempDir::new().unwrap())
            } else {
                None
            };
        let mut cfg = cfg;
        if let Some(dir) = &fallback_workdir {
            cfg.workdir = dir.path().to_path_buf();
        }
        // 常驻登记落在隔离目录里:`plugin/query` 与驱动器测试不得写真的
        // `~/.yi-agent`。
        let resident_dir = tempfile::TempDir::new().unwrap();
        let workspaces = Arc::new(WorkspaceIndex::new(
            index_dir.path().join("workspaces.json"),
        ));
        // 配对状态也落在隔离目录里:`device/list` 从空表起步,且绝不碰用户
        // 真实的 `~/.yi-agent/devices.json`。
        let pairing = Arc::new(PairingState::new(crate::device_store::DeviceStore::new(
            index_dir.path().join("devices.json"),
        )));
        // 与主循环共享同一个扇出中心,便于测试直接断言订阅状态
        // (`thread/subscribe` 的效果)。
        let hub = Arc::new(crate::broadcast::Broadcaster::new());
        // 主题句柄与 `cfg.workdir` 一致：`ui/settings/read` 从 `cfg.workdir`
        // 读、`write` 经句柄落盘，两者不同则会各看各的。
        let theme = crate::theme_tool::ThemeHandle::new(cfg.workdir.clone());
        // 值守的 home 也落在隔离目录里:安装/卸载经调用方注入的闭包,绝不
        // 触碰真实的 `~/Library/LaunchAgents`。
        let watchman_home = tempfile::TempDir::new().unwrap();
        let catalog_path = watchman_home.path().join("plugins").join("catalog.json");
        if let Some(parent) = catalog_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        // 假的插件可执行目录:注入的解析器只在它里面找名字,故既不依赖本机
        // 装了什么插件,也不必改动进程级 `PATH`(那会污染同进程的并行用例)。
        let plugins_dir = tempfile::TempDir::new().unwrap();
        let plugins_root = plugins_dir.path().to_path_buf();
        let resolve_plugin: crate::server::ResolvePlugin = Arc::new(move |name: &str| {
            let candidate = plugins_root.join(name);
            candidate.is_file().then_some(candidate)
        });
        let daemon_calls: DaemonCalls = Arc::new(StdMutex::new(Vec::new()));
        let daemon_log = Arc::clone(&daemon_calls);
        let ensure_daemon: crate::server::EnsureDaemon = Arc::new(move |project: &Path| {
            daemon_log
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(project.to_path_buf());
            Ok(true)
        });
        // skill 根同样落在隔离目录里:`skill/list` 不得枚举开发者真实的
        // `~/.yi-agent/skills`。
        let skills_home = tempfile::TempDir::new().unwrap();
        // 模型清单也落在隔离目录里:`model/*` 不得写开发者真实的
        // `~/.yi-agent/models.json`(里面是明文密钥)。
        let models_dir = tempfile::TempDir::new().unwrap();
        // 组长信箱:先造一份,clone 进 `serve`,原件留在 `Harness` 上。
        let lead_inbox = crate::lead::LeadInbox::new();
        // 回信登记表:同一手法(与主循环共享同一实例)。测试据此断言
        // 「等待方登记的名字被主循环真的回填了」——自造第二份会得到一个
        // 永远等不到回信的登记表。
        let lead_replies = crate::lead::LeadReplies::new();
        // 组长写通道 + 结果登记表:同一手法(各 clone 进 `serve`,原件留在
        // `Harness` 上),供 Task 5 的写指令集成测试直接投递并断言回填。
        let lead_control = crate::lead_control::LeadControlInbox::new();
        let lead_control_replies = crate::lead_control::LeadControlReplies::new();
        // 视图帧表:与主循环共享同一 `Arc`(注入进 `RuntimeAttachments`)。测试据此
        // 直接注入帧,断言 `thread/view/read` 读的是同一张表。
        let view_frames: crate::server::ViewFrames =
            Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        // 与 `view_frames` 同一手法:服务端写入、测试观察。自造一份会让
        // 「握手装上了本会话的身份」这条断言在空槽位上永远成立。
        let managed: crate::server::ManagedSlot = Arc::new(StdMutex::new(None));
        // 与 `view_frames` / `managed` 同一手法：宿主写入、测试观察。
        let live_side_parents = live_side_parents
            .unwrap_or_else(|| Arc::new(crate::side_conversation::LiveSideParents::new()));
        let handle = tokio::spawn(serve_scoped(
            server_r,
            server_w,
            cfg,
            permission_timeout,
            workspaces,
            Arc::clone(&pairing),
            Arc::clone(&hub),
            RuntimeAttachments {
                runtimes: Arc::new(StdMutex::new(HashMap::new())),
                thread_roots: Arc::new(StdMutex::new(HashMap::new())),
                managed: Arc::clone(&managed),
                bringup: Arc::clone(&bringup),
                models_path: models_dir.path().join("models.json"),
                websearch_path: models_dir.path().join("websearch.json"),
                settings_path: models_dir.path().join("settings.json"),
                // 默认静态装配；`with_live_settings` 才注入共享实例。
                live_settings: live_settings.clone(),
                // harness 默认关（= stable 行为）；`with_lead_flag(true)` 打开的
                // 那个构造器会把这个值换成 true。
                lead_agent_enabled,
                // 旁支默认**开**：既有 side 测试全部假定能力可用（spec §6）。
                // 需要关的用例走 `with_scope_side_flag_and_registry`。
                side_gate,
                // 引导的 .env / preferences 也落在隔离目录里:onboarding/*
                // 不得写开发者真实的 `~/.yi-agent`。
                onboarding_preferences_path: index_dir.path().join("preferences.json"),
                resident_dir: resident_dir.path().to_path_buf(),
                skills_home: skills_home.path().to_path_buf(),
                instance_home: instance_home_override
                    .clone()
                    .unwrap_or_else(|| skills_home.path().to_path_buf()),
                remote_dir: index_dir.path().to_path_buf(),
                live_side_parents: Some(Arc::clone(&live_side_parents)),
                side_registry,
                theme,
                git_diff: crate::git_diff_tool::GitDiffHandle::new(),
                // 与 `Harness::view_frames` 是**同一个** `Arc`(见上方 `lead_inbox`
                // 的同一手法):自造一份,测试注入的帧就没人读。
                view_frames: Arc::clone(&view_frames),
                watchman_install: install,
                watchman_uninstall: uninstall,
                watchman_home: watchman_home.path().to_path_buf(),
                // 官方目录也落隔离目录：`plugins/v1/catalog` 与 `assetRelease`
                // 不得读开发者真实的 `~/.yi-agent/plugins/catalog.json`。
                plugin_catalog: watchman_home.path().join("plugins").join("catalog.json"),
                // 假插件目录 + 记账式 daemon 拉起:安装路径可测,且绝不真的
                // `spawn` 一个 detached `daemon serve`。
                resolve_plugin,
                ensure_daemon,
                // 与主循环**共享**同一份信箱(clone 共用同一个 broadcast
                // sender):测试经 `lead_inbox` 直接投递的消息必须被主循环
                // 收到,自造第二份会投进没人听的通道。
                lead_inbox: lead_inbox.clone(),
                lead_replies: lead_replies.clone(),
                lead_control: lead_control.clone(),
                lead_control_replies: lead_control_replies.clone(),
            },
            build,
            scope,
            management,
        ));
        Self {
            client_w,
            client_r: BufReader::new(client_r),
            pending: std::collections::VecDeque::new(),
            handle,
            _index_dir: index_dir,
            _fallback_workdir: fallback_workdir,
            _models_dir: models_dir,
            resident_dir,
            pairing,
            hub,
            watchman_calls: calls,
            _watchman_home: watchman_home,
            plugin_catalog: catalog_path,
            skills_home_dir: skills_home,
            lead_inbox,
            lead_replies,
            lead_control,
            lead_control_replies,
            view_frames,
            managed,
            plugins_dir,
            daemon_calls,
            bringup,
            live_side_parents,
        }
    }

    pub(crate) fn hub(&self) -> Arc<crate::broadcast::Broadcaster> {
        Arc::clone(&self.hub)
    }

    pub(crate) fn skills_home(&self) -> std::path::PathBuf {
        self.skills_home_dir.path().to_path_buf()
    }

    /// 本 harness 的实例根。
    ///
    /// 与 `skills_home` 取同一个隔离目录：`serve` 装配时 `instance_home` 就是这个
    /// 值（见 `tests/mod.rs` 里 `RuntimeAttachments` 的构造），驱动闸门读授权表
    /// 也必须读这里，否则测试会去碰开发者真实的 `~/.yi-agent`。
    pub(crate) fn instance_home(&self) -> std::path::PathBuf {
        self.skills_home_dir.path().to_path_buf()
    }

    pub(crate) fn pairing(&self) -> Arc<PairingState> {
        Arc::clone(&self.pairing)
    }

    pub(crate) async fn send(&mut self, line: &str) {
        self.client_w.write_all(line.as_bytes()).await.unwrap();
        self.client_w.write_all(b"\n").await.unwrap();
        self.client_w.flush().await.unwrap();
    }

    pub(crate) fn deliver_lead_message_for_test(
        h: &Harness,
        workdir: &std::path::Path,
        from_thread_id: &str,
        text: &str,
    ) {
        h.lead_inbox
            .deliver(crate::lead::LeadEnvelope {
                project_key: crate::thread_store::ThreadStore::new(workdir).project_key(),
                from_thread_id: from_thread_id.to_string(),
                text: text.to_string(),
                request_id: None,
            })
            .expect("deliver");
    }

    pub(crate) async fn read_value(&mut self) -> serde_json::Value {
        if let Some(v) = self.pending.pop_front() {
            return v;
        }
        self.read_value_from_wire().await
    }

    async fn read_value_from_wire(&mut self) -> serde_json::Value {
        let mut buf = String::new();
        let n = tokio::time::timeout(EVENT_WAIT, self.client_r.read_line(&mut buf))
            .await
            .expect("timed out waiting for a message")
            .expect("read_line failed");
        assert!(n > 0, "unexpected EOF while waiting for a message");
        serde_json::from_str(buf.trim()).expect("server wrote invalid JSON")
    }

    pub(crate) async fn read_matching(
        &mut self,
        limit: usize,
        mut want: impl FnMut(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        // 先扫已在队列里的：调用方可能已经读过几帧。
        for _ in 0..self.pending.len() {
            let v = self.pending.pop_front().expect("length checked");
            if want(&v) {
                return v;
            }
            self.pending.push_back(v);
        }
        for _ in 0..limit {
            let v = self.read_value_from_wire().await;
            if want(&v) {
                return v;
            }
            if v.get("id").is_some() {
                self.pending.push_back(v);
            }
        }
        panic!("no matching frame in {limit} frames");
    }

    pub(crate) async fn try_read_value(&mut self, wait: Duration) -> Option<serde_json::Value> {
        let mut buf = String::new();
        match tokio::time::timeout(wait, self.client_r.read_line(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => serde_json::from_str(buf.trim()).ok(),
            _ => None,
        }
    }

    pub(crate) async fn read_until_turn_completed(&mut self) -> serde_json::Value {
        for _ in 0..40 {
            let v = self.read_value().await;
            if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
                return v;
            }
        }
        panic!("turn/completed never arrived");
    }

    pub(crate) async fn shutdown(self) {
        let Harness {
            client_w, handle, ..
        } = self;
        drop(client_w);
        let res = tokio::time::timeout(EVENT_WAIT, handle)
            .await
            .expect("server must shut down within the timeout")
            .expect("server task must not panic");
        assert!(res.is_ok(), "serve_stdio should return Ok on EOF: {res:?}");
    }
}

pub(crate) async fn initialize(h: &mut Harness) {
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .await;
    // 用「找 id」而不是「读第一帧」：后台能力探测的广播可以插在中间。
    let v = h
        .read_matching(64, |v| v.get("id") == Some(&serde_json::json!(1)))
        .await;
    assert!(
        v.get("result").is_some() || v.get("error").is_some(),
        "initialize must respond with the request id: {v}"
    );
}

async fn start_thread(h: &mut Harness) -> String {
    initialize(h).await;
    h.send(r#"{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{}}"#)
        .await;
    // 取响应里的 thread_id；中间的 `thread/started` 通知与后台广播一律不丢。
    let v = h
        .read_matching(64, |v| v.get("id") == Some(&serde_json::json!(2)))
        .await;
    v["result"]["thread_id"].as_str().unwrap().to_string()
}

async fn read_response(h: &mut Harness, want: u64) -> serde_json::Value {
    h.read_matching(64, |v| v.get("id") == Some(&serde_json::json!(want)))
        .await
}

fn replayed_items_of(v: &serde_json::Value) -> Vec<crate::protocol::Item> {
    match v.get("method").and_then(|m| m.as_str()) {
        Some("item/completed") => {
            vec![serde_json::from_value(v["params"]["item"].clone()).unwrap()]
        }
        Some("items/completed") => v["params"]["items"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|it| serde_json::from_value(it.clone()).unwrap())
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

async fn collect_replayed_items(h: &mut Harness, want: u64) -> Vec<crate::protocol::Item> {
    let mut out = Vec::new();
    for _ in 0..4096 {
        let v = h.read_value().await;
        out.extend(replayed_items_of(&v));
        if v.get("id") == Some(&serde_json::json!(want)) {
            break;
        }
    }
    out
}

async fn await_notification(
    h: &mut Harness,
    method: &str,
    timeout: Duration,
) -> Option<serde_json::Value> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let slice = remaining.min(Duration::from_millis(1000));
        match tokio::time::timeout(slice, h.read_value()).await {
            Ok(v) => {
                if v.get("method").and_then(|m| m.as_str()) == Some(method) {
                    return Some(v);
                }
            }
            Err(_) => {
                if tokio::time::Instant::now() >= deadline {
                    return None;
                }
            }
        }
    }
}

fn summary(
    task_id: &str,
    state: &str,
    thread_id: Option<&str>,
) -> yi_agent_store::ipc::IpcTaskSummary {
    yi_agent_store::ipc::IpcTaskSummary {
        task_id: task_id.into(),
        state: state.into(),
        is_root: false,
        parent_task_id: Some("root".into()),
        thread_id: thread_id.map(str::to_string),
        usage: yi_agent_store::ipc::IpcTaskUsage::default(),
        role: "execution".to_string(),
        container_activity: "unknown".to_string(),
        historical_execution_state: None,
    }
}

struct GitDiffToolProvider {
    calls: AtomicUsize,
}

#[async_trait]
impl yi_agent_core::Provider for GitDiffToolProvider {
    async fn call_stream(
        &self,
        req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        // 标题请求不是对话轮：不参与下方按次数脚本化的计数，否则首轮会把
        // 「第 0 次」让给标题调用，脚本化序列整体错位。
        if req.system.as_deref() == Some(yi_agent_core::title::TITLE_INSTRUCTIONS) {
            return Ok(futures::stream::iter(vec![
                yi_agent_core::provider::ProviderEvent::TextDelta("t".into()),
                yi_agent_core::provider::ProviderEvent::Stop {
                    reason: yi_agent_core::provider::StopReason::EndTurn,
                },
            ])
            .boxed());
        }
        use yi_agent_core::provider::{ProviderEvent as E, StopReason};
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let events = if n == 0 {
            vec![
                E::ToolUseStart {
                    id: "g1".into(),
                    name: "show_git_diff".into(),
                },
                E::ToolUseDelta {
                    id: "g1".into(),
                    partial_json: "{}".into(),
                },
                E::ToolUseEnd { id: "g1".into() },
                E::Stop {
                    reason: StopReason::EndTurn,
                },
            ]
        } else {
            vec![
                E::TextDelta("done".into()),
                E::Stop {
                    reason: StopReason::EndTurn,
                },
            ]
        };
        Ok(futures::stream::iter(events).boxed())
    }
}

fn build_git_diff_tool_agent(
    session: Option<yi_agent_core::Session>,
    cwd: &std::path::Path,
    mode: crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent> {
    build_test_agent_with_provider(
        Arc::new(GitDiffToolProvider {
            calls: AtomicUsize::new(0),
        }),
        session,
        cwd,
        mode,
    )
}

async fn rpc(
    h: &mut Harness,
    id: u64,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    h.send(
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        })
        .to_string(),
    )
    .await;
    read_response(h, id).await
}

fn write_a_re_encodable_png_into_memory() -> Vec<u8> {
    let img = ::image::RgbImage::from_fn(4096, 64, |x, y| {
        ::image::Rgb([(x % 251) as u8, (y % 253) as u8, ((x ^ y) % 255) as u8])
    });
    let mut buf = Vec::new();
    img.write_to(
        &mut std::io::Cursor::new(&mut buf),
        ::image::ImageFormat::Png,
    )
    .unwrap();
    buf
}

fn write_an_uncompressed_png(path: &std::path::Path) -> Vec<u8> {
    let mut state: u32 = 0x1234_5678;
    let img = ::image::RgbImage::from_fn(1024, 1024, |_, _| {
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        };
        ::image::Rgb([next() as u8, next() as u8, next() as u8])
    });
    img.save_with_format(path, ::image::ImageFormat::Png)
        .unwrap();
    std::fs::read(path).unwrap()
}

struct PermissionMockProvider {
    calls: AtomicUsize,
}

#[async_trait]
impl yi_agent_core::Provider for PermissionMockProvider {
    async fn call_stream(
        &self,
        req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        // 标题请求不是对话轮：不参与下方按次数脚本化的计数，否则首轮会把
        // 「第 0 次」让给标题调用，脚本化序列整体错位。
        if req.system.as_deref() == Some(yi_agent_core::title::TITLE_INSTRUCTIONS) {
            return Ok(futures::stream::iter(vec![
                yi_agent_core::provider::ProviderEvent::TextDelta("t".into()),
                yi_agent_core::provider::ProviderEvent::Stop {
                    reason: yi_agent_core::provider::StopReason::EndTurn,
                },
            ])
            .boxed());
        }
        use yi_agent_core::provider::{ProviderEvent, StopReason};
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let events = if n == 0 {
            vec![
                ProviderEvent::ToolUseStart {
                    id: "c1".into(),
                    name: "bash".into(),
                },
                ProviderEvent::ToolUseDelta {
                    id: "c1".into(),
                    partial_json: r#"{"cmd":"ls"}"#.into(),
                },
                ProviderEvent::ToolUseEnd { id: "c1".into() },
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ]
        } else {
            vec![
                ProviderEvent::TextDelta("done".into()),
                ProviderEvent::Stop {
                    reason: StopReason::EndTurn,
                },
            ]
        };
        Ok(futures::stream::iter(events).boxed())
    }
}

struct FakeBash;

#[async_trait]
impl yi_agent_core::Tool for FakeBash {
    fn name(&self) -> &str {
        "bash"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    fn description(&self) -> &str {
        "fake bash"
    }
    async fn call(&self, _args: serde_json::Value) -> yi_agent_core::ToolResult {
        yi_agent_core::ToolResult::text("ran")
    }
}

struct SlowCallProvider;

static SLOW_CALL_ENTERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[async_trait]
impl yi_agent_core::Provider for SlowCallProvider {
    async fn call_stream(
        &self,
        _req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        SLOW_CALL_ENTERED.store(true, Ordering::SeqCst);
        // 请求发出后、响应头到达前的等待：真实 provider 在这里等 HTTP。
        tokio::time::sleep(Duration::from_secs(30)).await;
        use yi_agent_core::provider::{ProviderEvent, StopReason};
        let events = vec![
            ProviderEvent::TextDelta("late".into()),
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ];
        Ok(futures::stream::iter(events).boxed())
    }
}

fn build_slow_call_agent(
    session: Option<yi_agent_core::Session>,
    _cwd: &std::path::Path,
    _mode: crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent> {
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(SlowCallProvider);
    let config = yi_agent_core::AgentConfig::default();
    let mut agent = yi_agent_core::Agent::new(
        provider.clone(),
        Arc::new(yi_agent_core::ToolRegistry::new()),
        config.clone(),
    );
    apply_session(&mut agent, session);
    Ok(BuiltAgent {
        agent,
        provider,
        config,
        decision_tx: None,
        decision_rx: None,
        catalog: None,
        yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
        process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        mcp: None,
        sandbox: yi_agent_tools::SandboxController::new(
            yi_agent_core::autonomy::YoloSwitch::new(false),
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            true,
        ),
    })
}

struct LongToolProvider;
#[async_trait]
impl yi_agent_core::Provider for LongToolProvider {
    async fn call_stream(
        &self,
        _req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        use yi_agent_core::provider::{ProviderEvent, StopReason};
        let events = vec![
            ProviderEvent::ToolUseStart {
                id: "c1".into(),
                name: "bash".into(),
            },
            ProviderEvent::ToolUseDelta {
                id: "c1".into(),
                partial_json: r#"{"command":"sleep 60"}"#.into(),
            },
            ProviderEvent::ToolUseEnd { id: "c1".into() },
            ProviderEvent::Stop {
                reason: StopReason::EndTurn,
            },
        ];
        Ok(futures::stream::iter(events).boxed())
    }
}

fn build_long_tool_agent(
    session: Option<yi_agent_core::Session>,
    _cwd: &std::path::Path,
    _mode: crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent> {
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(LongToolProvider);
    // 权限层直接放行（yolo 打开），不必走审批，直接进入 ACT。
    let checker = Arc::new(yi_agent_core::permission::PermissionChecker::new(
        yi_agent_core::permission::PermissionsConfig::default(),
        yi_agent_core::autonomy::YoloSwitch::new(true),
        std::path::PathBuf::from("/tmp"),
        Arc::new(|_cmd: &str| None),
    ));
    let registry =
        yi_agent_runtime::bootstrap::build_tool_setup(&crate::test_support::test_config(), false)
            .unwrap();
    let config = yi_agent_core::AgentConfig::default();
    let (decision_tx, decision_rx) = mpsc::channel::<(u64, Decision)>(16);
    let rx_arc = Arc::new(Mutex::new(decision_rx));
    let mut agent = yi_agent_core::Agent::new(
        provider.clone(),
        Arc::clone(&registry.tools),
        config.clone(),
    )
    .with_permission(checker.clone(), rx_arc.clone());
    apply_session(&mut agent, session);
    Ok(BuiltAgent {
        agent,
        provider,
        config,
        decision_tx: Some(decision_tx),
        decision_rx: Some(rx_arc),
        catalog: None,
        yolo: yi_agent_core::autonomy::YoloSwitch::new(true),
        process_manager: Arc::clone(&registry.process_manager),
        mcp: None,
        sandbox: yi_agent_tools::SandboxController::new(
            yi_agent_core::autonomy::YoloSwitch::new(false),
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            true,
        ),
    })
}

fn build_permission_agent(
    session: Option<yi_agent_core::Session>,
    _cwd: &std::path::Path,
    _mode: crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent> {
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(PermissionMockProvider {
        calls: AtomicUsize::new(0),
    });
    let mut registry = yi_agent_core::ToolRegistry::new();
    registry.register(Arc::new(FakeBash));
    let config = yi_agent_core::AgentConfig::default();
    let checker = Arc::new(yi_agent_core::permission::PermissionChecker::new(
        yi_agent_core::permission::PermissionsConfig::default(),
        yi_agent_core::autonomy::YoloSwitch::new(false),
        std::path::PathBuf::from("/tmp/yi-agent-app-server-test"),
        Arc::new(|_cmd: &str| None),
    ));
    let (decision_tx, decision_rx) = mpsc::channel::<(u64, Decision)>(16);
    let rx_arc = Arc::new(Mutex::new(decision_rx));
    let mut agent = yi_agent_core::Agent::new(provider.clone(), Arc::new(registry), config.clone())
        .with_permission(checker.clone(), rx_arc.clone());
    apply_session(&mut agent, session);
    Ok(BuiltAgent {
        agent,
        provider,
        config,
        decision_tx: Some(decision_tx),
        decision_rx: Some(rx_arc),
        catalog: None,
        yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
        process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        mcp: None,
        sandbox: yi_agent_tools::SandboxController::new(
            yi_agent_core::autonomy::YoloSwitch::new(false),
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            true,
        ),
    })
}

pub(crate) async fn serve_ws_with_permission_agent(
    listener: tokio::net::TcpListener,
    cfg: RuntimeConfig,
    workspaces: Arc<WorkspaceIndex>,
    pairing: Arc<PairingState>,
) -> anyhow::Result<()> {
    let theme = crate::theme_tool::ThemeHandle::new(cfg.workdir.clone());
    crate::ws::serve_ws_inner(
        listener,
        cfg,
        workspaces,
        pairing,
        theme,
        // 这条测试接缝没有设置来源：静态装配，与历史行为一致。
        None,
        build_permission_agent,
        Arc::new(yi_agent_subagent::binding::BringUpChoice::default()),
    )
    .await
}

async fn read_until_approval(h: &mut Harness) -> String {
    for _ in 0..20 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("item/toolCall/requestApproval") {
            return v["id"]
                .as_str()
                .expect("approval id must be a string")
                .to_string();
        }
    }
    panic!("expected an item/toolCall/requestApproval notification");
}

async fn read_thread_start_response(h: &mut Harness, want: u64) -> String {
    for _ in 0..8 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(want)) {
            return v["result"]["thread_id"]
                .as_str()
                .expect("thread/start response must carry thread_id")
                .to_string();
        }
    }
    panic!("no thread/start response for id {want}");
}

async fn list_all(h: &mut Harness) -> serde_json::Value {
    h.send(r#"{"jsonrpc":"2.0","id":901,"method":"thread/listAll","params":{}}"#)
        .await;
    for _ in 0..8 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(901)) {
            return v;
        }
    }
    panic!("thread/listAll must respond");
}

fn content_epoch_of(list: &serde_json::Value, thread_id: &str) -> u64 {
    let row = find_listed(list, thread_id);
    row["content_epoch"]
        .as_u64()
        .unwrap_or_else(|| panic!("content_epoch must be present: {row}"))
}

fn last_item_id_of(list: &serde_json::Value, thread_id: &str) -> Option<String> {
    find_listed(list, thread_id)["last_item_id"]
        .as_str()
        .map(str::to_string)
}

fn find_listed<'a>(list: &'a serde_json::Value, thread_id: &str) -> &'a serde_json::Value {
    let in_groups = list["result"]["groups"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|g| g["threads"].as_array().into_iter().flatten());
    let in_pinned = list["result"]["pinned"].as_array().into_iter().flatten();
    in_groups
        .chain(in_pinned)
        .find(|t| t["thread_id"] == serde_json::json!(thread_id))
        .unwrap_or_else(|| panic!("thread {thread_id} must be listed: {list}"))
}

async fn add_workspace(h: &mut Harness, id: u64, path: &str) {
    h.send(
        &serde_json::json!({"jsonrpc":"2.0","id":id,"method":"workspace/add",
        "params":{"path": path}})
        .to_string(),
    )
    .await;
    for _ in 0..8 {
        let v = h.read_value().await;
        if v.get("id") == Some(&serde_json::json!(id)) {
            return;
        }
    }
    panic!("workspace/add must respond");
}

fn write_meta(
    dir: &Path,
    id: &str,
    title: &str,
    driver_project: Option<&str>,
    driver_ref: Option<&str>,
) {
    let meta = crate::thread_store::ThreadMeta {
        plugin_bindings: Vec::new(),
        thread_id: id.into(),
        cwd: dir.to_string_lossy().into(),
        model: "m".into(),
        created_at: 0,
        updated_at: 0,
        title: Some(title.into()),
        permission_mode: crate::thread_store::ThreadMode::Normal,
        pin_seq: None,
        driver_project: driver_project.map(str::to_string),
        driver_ref: driver_ref.map(str::to_string),
        model_ref: None,
        reasoning_effort: None,
        tag: None,
        preserve_thinking: true,
        content_epoch: 0,
        last_item_id: None,
        anchor_sha: None,
        last_read_at: 0,
        last_completed_at: 0,
        sandbox_baseline: None,
        forked_from: None,

        extra_roots: Vec::new(),
        role: None,
    };
    crate::thread_store::ThreadStore::new(dir)
        .create(&meta)
        .unwrap();
}

fn write_lead_meta(dir: &Path, id: &str) {
    let meta = crate::thread_store::ThreadMeta {
        plugin_bindings: Vec::new(),
        thread_id: id.into(),
        cwd: dir.to_string_lossy().into(),
        model: String::new(),
        created_at: 0,
        updated_at: 0,
        title: Some("组长".into()),
        permission_mode: crate::thread_store::ThreadMode::Normal,
        pin_seq: None,
        driver_project: None,
        driver_ref: None,
        model_ref: None,
        reasoning_effort: None,
        tag: None,
        preserve_thinking: true,
        content_epoch: 0,
        last_item_id: None,
        extra_roots: Vec::new(),
        role: Some(crate::thread_store::LEAD_ROLE.to_string()),
        anchor_sha: None,
        last_read_at: 0,
        last_completed_at: 0,
        forked_from: None,
        sandbox_baseline: None,
    };
    crate::thread_store::ThreadStore::new(dir)
        .create(&meta)
        .unwrap();
}

fn git_project(dir: &Path) -> PathBuf {
    let run = |args: &[&str]| {
        assert!(
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .status()
                .unwrap()
                .success(),
            "git {args:?}"
        );
    };
    std::fs::create_dir_all(dir).unwrap();
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.email", "t@t"]);
    run(&["config", "user.name", "t"]);
    // 复刻真实项目的 .gitignore：宿主运行态 `.yi-agent/` 本就被忽略（见仓库根
    // .gitignore）。少了它，`write_meta` 写下的 `<cwd>/.yi-agent/…` 在 worktree
    // 内是 untracked——git 会判"脏"，`worktree remove` 也会被拒。这不是测试造作，
    // 而是生产里同样被忽略：不补它，"干净且已并入 → 静默删"那条根本走不到。
    std::fs::write(dir.join(".gitignore"), ".yi-agent/\n.worktrees/\n").unwrap();
    std::fs::write(dir.join("f.txt"), "hi").unwrap();
    run(&["add", "-A"]);
    run(&["commit", "-q", "-m", "init"]);
    dir.canonicalize().unwrap()
}

fn git_kanban_worktree(project: &Path, card_id: &str) -> PathBuf {
    let slug = crate::worktree_reclaim::slugify(card_id);
    let path = project.join(".worktrees/kanban").join(&slug);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    assert!(
        std::process::Command::new("git")
            .arg("-C")
            .arg(project)
            .args(["worktree", "add", "-q"])
            .arg(&path)
            .args(["-b", &format!("kanban/{slug}")])
            .status()
            .unwrap()
            .success()
    );
    path.canonicalize().unwrap()
}

async fn isolated_harness() -> (Harness, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = crate::test_support::test_config();
    cfg.workdir = dir.path().to_path_buf();
    (Harness::with_cfg(cfg).await, dir)
}

fn tooling_names_for(
    repo: &std::path::Path,
    runtime: &std::path::Path,
    thread_id: &str,
    project_key: String,
    lead_agent_enabled: Option<bool>,
) -> Vec<String> {
    let mut cfg = test_config();
    cfg.workdir = repo.to_path_buf();
    let attached = Arc::new(
        yi_agent_subagent::attach::attach_project_runtime(
            &cfg,
            runtime.to_path_buf(),
            &isolated_models_path(runtime),
        )
        .expect("a clean git repo must attach"),
    );
    let binding = RuntimeBinding::managed(
        &cfg,
        runtime.to_path_buf(),
        isolated_models_path(runtime),
        Arc::clone(&attached),
    );
    let root = ThreadRoot::from_handle(binding, attached.attached_root.clone());
    build_runtime_tooling(
        &cfg,
        &root,
        thread_id,
        yi_agent_core::autonomy::YoloSwitch::new(false),
        test_theme(),
        crate::git_diff_tool::GitDiffHandle::new(),
        Vec::new(),
        project_key,
        crate::lead::LeadInbox::new(),
        crate::lead::LeadReplies::new(),
        crate::lead_control::LeadControlInbox::new(),
        crate::lead_control::LeadControlReplies::new(),
        None,
        lead_agent_enabled,
        Vec::new(),
    )
    .expect("tooling")
    .registry
    .names()
}

async fn isolated_lead_harness() -> (Harness, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().unwrap();
    let mut cfg = crate::test_support::test_config();
    cfg.workdir = dir.path().to_path_buf();
    (Harness::with_cfg_and_lead(cfg, true).await, dir)
}

fn has_lead_meta(project: &std::path::Path) -> bool {
    crate::thread_store::ThreadStore::new(project)
        .list()
        .map(|metas| {
            metas
                .iter()
                .any(|m| m.role.as_deref() == Some(crate::thread_store::LEAD_ROLE))
        })
        .unwrap_or(false)
}

fn tick_test_session(project: &Path, thread_id: &str, status: ThreadStatus) -> ThreadSession {
    let (prompt_tx, _prompt_rx) = mpsc::channel(1);
    let (interrupt_tx, _interrupt_rx) = mpsc::channel(1);
    let (interject_tx, _interject_rx) = mpsc::channel(1);
    let (session_tx, _session_rx) = mpsc::channel(1);
    ThreadSession {
        thread_id: thread_id.to_string(),
        cwd: project.display().to_string(),
        model: "test".to_string(),
        active_turn_id: None,
        turn_permit: None,
        side_source: None,
        yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
        process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        prompt_tx,
        interrupt_tx,
        interject_tx,
        session_tx,
        store: Arc::new(crate::thread_store::ThreadStore::new(project)),
        status: Arc::new(StdMutex::new(status)),
        subagent_tx: tokio::sync::watch::channel(crate::protocol::SubagentTotals::default()).0,
        writer_lock: None,
    }
}

fn tool_text(result: &yi_agent_core::ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            yi_agent_core::ContentBlock::Text(t) => Some(t.clone()),
            _ => None,
        })
        .collect::<String>()
}

const LATE_ASK_WINDOW: Duration = Duration::from_millis(300);

struct LeadEchoProvider {
    conversation_calls: AtomicUsize,
    late_asker: std::sync::Mutex<Option<Arc<crate::lead::AskLeadTool>>>,
    late_reply: Arc<tokio::sync::Mutex<Option<String>>>,
}

impl LeadEchoProvider {
    fn new(late_reply: Arc<tokio::sync::Mutex<Option<String>>>) -> Self {
        Self {
            conversation_calls: AtomicUsize::new(0),
            late_asker: std::sync::Mutex::new(None),
            late_reply,
        }
    }

    fn set_late_asker(&self, tool: Arc<crate::lead::AskLeadTool>) {
        *self.late_asker.lock().unwrap_or_else(|p| p.into_inner()) = Some(tool);
    }
}

#[async_trait]
impl yi_agent_core::Provider for LeadEchoProvider {
    async fn call_stream(
        &self,
        req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        use yi_agent_core::message::{ContentBlock, Role};

        if req.system.as_deref() == Some(yi_agent_core::title::TITLE_INSTRUCTIONS) {
            return Ok(Box::pin(futures::stream::iter(vec![
                yi_agent_core::provider::ProviderEvent::TextDelta("t".into()),
                yi_agent_core::provider::ProviderEvent::Stop {
                    reason: yi_agent_core::provider::StopReason::EndTurn,
                },
            ])));
        }
        if self.conversation_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            let late = self
                .late_asker
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            if let Some(tool) = late {
                let slot = Arc::clone(&self.late_reply);
                tokio::spawn(async move {
                    use yi_agent_core::Tool as _;
                    let result = tool
                        .call(serde_json::json!({
                            "message": "要你裁决 A",
                            "blocking": true,
                        }))
                        .await;
                    *slot.lock().await = Some(tool_text(&result));
                });
                // 等主循环把这条迟到投递叠进**本轮**组长的待回填队列。
                tokio::time::sleep(LATE_ASK_WINDOW).await;
            }
        }
        let text = req
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .and_then(|m| {
                m.content.iter().find_map(|block| match block {
                    ContentBlock::Text(t) => Some(t.clone()),
                    _ => None,
                })
            })
            .unwrap_or_default();
        let events = vec![
            yi_agent_core::provider::ProviderEvent::TextDelta(text),
            yi_agent_core::provider::ProviderEvent::Stop {
                reason: yi_agent_core::provider::StopReason::EndTurn,
            },
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

async fn await_reply_slot(slot: &Arc<tokio::sync::Mutex<Option<String>>>, who: &str) -> String {
    let deadline = std::time::Instant::now() + EVENT_WAIT;
    loop {
        if let Some(text) = slot.lock().await.clone() {
            return text;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{who} never received a reply"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn echo_lead_harness() -> (
    Harness,
    tempfile::TempDir,
    Arc<LeadEchoProvider>,
    Arc<tokio::sync::Mutex<Option<String>>>,
) {
    let dir = tempfile::TempDir::new().unwrap();
    let late_reply = Arc::new(tokio::sync::Mutex::new(None));
    let provider = Arc::new(LeadEchoProvider::new(Arc::clone(&late_reply)));
    let factory_provider: Arc<dyn yi_agent_core::Provider> = provider.clone();
    let mut cfg = test_config();
    cfg.workdir = dir.path().to_path_buf();
    // 组长开关打开：这个 fixture 后面要 `thread/lead`，关着会拿到 -32601。
    let mut h = Harness::with_config_and_lead(
        cfg,
        move |session, cwd, mode| {
            build_test_agent_with_provider(Arc::clone(&factory_provider), session, cwd, mode)
        },
        PERMISSION_TIMEOUT,
        true,
    )
    .await;
    h.send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .await;
    let _ = read_response(&mut h, 1).await;
    (h, dir, provider, late_reply)
}

fn ask_lead_tool(h: &Harness, workdir: &Path, from: &str) -> Arc<crate::lead::AskLeadTool> {
    Arc::new(
        crate::lead::AskLeadTool::new(
            h.lead_inbox.clone(),
            crate::thread_store::ThreadStore::new(workdir).project_key(),
            from.to_string(),
        )
        .with_replies(h.lead_replies.clone(), crate::lead::ASK_LEAD_TIMEOUT),
    )
}

struct GatedToolProvider {
    conversation_calls: AtomicUsize,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    later_messages: Arc<std::sync::Mutex<Vec<Vec<yi_agent_core::Message>>>>,
}

#[async_trait]
impl yi_agent_core::Provider for GatedToolProvider {
    async fn call_stream(
        &self,
        req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        use yi_agent_core::provider::{ProviderEvent as E, StopReason};
        if req.system.as_deref() == Some(yi_agent_core::title::TITLE_INSTRUCTIONS) {
            return Ok(futures::stream::iter(vec![
                E::TextDelta("t".into()),
                E::Stop {
                    reason: StopReason::EndTurn,
                },
            ])
            .boxed());
        }
        if self.conversation_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            // 后台这一轮停在"模型调用在飞"的窗口里,直到测试放行。
            self.entered.notify_one();
            self.release.notified().await;
            return Ok(futures::stream::iter(vec![
                E::ToolUseStart {
                    id: "c1".into(),
                    name: "glob".into(),
                },
                E::ToolUseDelta {
                    id: "c1".into(),
                    partial_json: r#"{"pattern":"*.md"}"#.into(),
                },
                E::ToolUseEnd { id: "c1".into() },
                E::Stop {
                    reason: StopReason::EndTurn,
                },
            ])
            .boxed());
        }
        self.later_messages
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(req.messages.clone());
        Ok(futures::stream::iter(vec![
            E::TextDelta("background done".into()),
            E::Stop {
                reason: StopReason::EndTurn,
            },
        ])
        .boxed())
    }
}

fn gated_lead_factory(
    provider: Arc<GatedToolProvider>,
) -> impl Fn(
    Option<yi_agent_core::Session>,
    &Path,
    crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent>
+ Send
+ Sync
+ 'static {
    let provider: Arc<dyn yi_agent_core::Provider> = provider;
    move |session, cwd, mode| {
        build_test_agent_with_provider(Arc::clone(&provider), session, cwd, mode)
    }
}

fn recording_agent_factory(
    seen: Arc<std::sync::Mutex<Vec<usize>>>,
) -> impl Fn(
    Option<yi_agent_core::Session>,
    &Path,
    crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent>
+ Send
+ Sync
+ 'static {
    move |session, cwd, mode| {
        let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(RecordingProvider {
            seen: Arc::clone(&seen),
        });
        build_test_agent_with_provider(provider, session, cwd, mode)
    }
}

async fn wait_for_lead_turn_to_settle(workdir: &Path, thread_id: &str, texts: &[&str]) {
    let log = workdir
        .join(".yi-agent/threads")
        .join(format!("{thread_id}.jsonl"));
    let partial = workdir
        .join(".yi-agent/threads")
        .join(format!("{thread_id}.partial.json"));
    let deadline = std::time::Instant::now() + EVENT_WAIT;
    loop {
        let appended = std::fs::read_to_string(&log)
            .map(|line| texts.iter().all(|t| line.contains(t)))
            .unwrap_or(false);
        let loaded = crate::thread_store::ThreadStore::new(workdir)
            .load(thread_id)
            .ok()
            .flatten()
            .is_some_and(|loaded| transcript_contains(&loaded.messages, texts));
        if appended && loaded && !partial.exists() {
            return;
        }
        if std::time::Instant::now() > deadline {
            panic!(
                "the lead turn never settled for {texts:?} \
                 (appended={appended}, loaded={loaded}, partial={})",
                partial.exists()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn transcript_contains(messages: &[yi_agent_core::Message], texts: &[&str]) -> bool {
    texts.iter().all(|want| {
        messages.iter().any(|m| {
            m.content.iter().any(|b| match b {
                yi_agent_core::ContentBlock::Text(text) => text.contains(want),
                _ => false,
            })
        })
    })
}

fn item_id_owned(it: &crate::protocol::Item) -> Option<String> {
    item_id(it).map(|s| s.to_string())
}

async fn clear_thread_retrying(h: &mut Harness, tid: &str) -> serde_json::Value {
    for attempt in 0..20 {
        let id = 100 + attempt;
        h.send(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"thread/clear","params":{{"threadId":"{tid}"}}}}"#
        ))
        .await;
        loop {
            let v = h.read_value().await;
            if v.get("id") == Some(&serde_json::json!(id)) {
                if v["error"]["code"] != -32012 {
                    return v;
                }
                break; // 还在 turn 收尾窗口内：等一会儿再试
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }
    panic!("thread/clear kept answering -32012");
}

async fn run_a_turn_and_wait_for_persist(
    h: &mut Harness,
    workdir: &std::path::Path,
    tid: &str,
    text: &str,
) {
    h.send(
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 700, "method": "turn/start",
            "params": { "threadId": tid, "input": [{ "type": "text", "text": text }] },
        })
        .to_string(),
    )
    .await;
    let log = workdir
        .join(".yi-agent/threads")
        .join(format!("{tid}.jsonl"));
    let deadline = tokio::time::Instant::now() + EVENT_WAIT;
    let mut persisted = false;
    while tokio::time::Instant::now() < deadline {
        if std::fs::read_to_string(&log).is_ok_and(|t| !t.trim().is_empty()) {
            persisted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(persisted, "turn for {tid} must reach disk");
    for _ in 0..60 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some("turn/completed") {
            return;
        }
    }
    panic!("turn/completed never arrived for {tid}");
}

fn compactable_factory(
    session: Option<yi_agent_core::Session>,
    _cwd: &std::path::Path,
    _mode: crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent> {
    let provider: Arc<dyn yi_agent_core::Provider> = Arc::new(MockProvider);
    let config = yi_agent_core::AgentConfig::default();
    let session = session.unwrap_or_else(|| {
        let mut s = yi_agent_core::Session::new();
        s.replace_messages(vec![
            yi_agent_core::Message::user("first"),
            yi_agent_core::Message::assistant(vec![yi_agent_core::ContentBlock::Text(
                "reply".into(),
            )]),
            yi_agent_core::Message::user("second"),
        ]);
        s
    });
    Ok(BuiltAgent {
        agent: yi_agent_core::Agent::new(
            provider.clone(),
            Arc::new(yi_agent_core::ToolRegistry::new()),
            config.clone(),
        )
        .with_session(session),
        provider,
        config,
        decision_tx: None,
        decision_rx: None,
        catalog: None,
        yolo: yi_agent_core::autonomy::YoloSwitch::new(false),
        process_manager: yi_agent_tools::ProcessManager::new(std::env::temp_dir()),
        mcp: None,
        sandbox: yi_agent_tools::SandboxController::new(
            yi_agent_core::autonomy::YoloSwitch::new(false),
            yi_agent_tools::SandboxMode::WorkspaceWrite,
            true,
        ),
    })
}

async fn compact_thread(h: &mut Harness, tid: &str) -> serde_json::Value {
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":9,"method":"thread/compact","params":{{"threadId":"{tid}"}}}}"#
    ))
    .await;
    for _ in 0..8 {
        let v = h.read_value().await;
        // 跳过沿线可能出现的通知(thread/status/updated 等),只等响应。
        if v.get("id") == Some(&serde_json::json!(9)) {
            return v;
        }
    }
    panic!("thread/compact must respond");
}

struct GatedByTitleProvider {
    title_seen: Arc<tokio::sync::Notify>,
    title_written: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl yi_agent_core::Provider for GatedByTitleProvider {
    async fn call_stream(
        &self,
        req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        if req.system.as_deref() == Some(yi_agent_core::title::TITLE_INSTRUCTIONS) {
            self.title_seen.notify_one();
            return Ok(Box::pin(futures::stream::iter(vec![
                yi_agent_core::provider::ProviderEvent::TextDelta("修复登录报错".into()),
                yi_agent_core::provider::ProviderEvent::Stop {
                    reason: yi_agent_core::provider::StopReason::EndTurn,
                },
            ])));
        }
        // 等到测试确认 `thread/titleChanged` 已到达才结束本轮。
        self.title_written.notified().await;
        Ok(Box::pin(futures::stream::iter(vec![
            yi_agent_core::provider::ProviderEvent::TextDelta("ok".into()),
            yi_agent_core::provider::ProviderEvent::Stop {
                reason: yi_agent_core::provider::StopReason::EndTurn,
            },
        ])))
    }
}

struct MaterialCapturingProvider {
    materials: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait]
impl yi_agent_core::Provider for MaterialCapturingProvider {
    async fn call_stream(
        &self,
        req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        use yi_agent_core::message::{ContentBlock, Role};
        if req.system.as_deref() == Some(yi_agent_core::title::TITLE_INSTRUCTIONS) {
            if let Some(user) = req.messages.iter().find(|m| m.role == Role::User) {
                let text = user
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                self.materials.lock().unwrap().push(text);
            }
            return Ok(Box::pin(futures::stream::iter(vec![
                yi_agent_core::provider::ProviderEvent::TextDelta("即时标题".into()),
                yi_agent_core::provider::ProviderEvent::Stop {
                    reason: yi_agent_core::provider::StopReason::EndTurn,
                },
            ])));
        }
        Ok(Box::pin(futures::stream::iter(vec![
            yi_agent_core::provider::ProviderEvent::TextDelta("好的，我看下。".into()),
            yi_agent_core::provider::ProviderEvent::Stop {
                reason: yi_agent_core::provider::StopReason::EndTurn,
            },
        ])))
    }
}

struct TitleBlockingProvider {
    title_seen: Arc<tokio::sync::Notify>,
    release_title: Arc<tokio::sync::Notify>,
    title_returned: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl yi_agent_core::Provider for TitleBlockingProvider {
    async fn call_stream(
        &self,
        req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        use yi_agent_core::provider::ProviderEvent as E;
        if req.system.as_deref() == Some(yi_agent_core::title::TITLE_INSTRUCTIONS) {
            self.title_seen.notify_one();
            self.release_title.notified().await;
            self.title_returned.notify_one();
            return Ok(Box::pin(futures::stream::iter(vec![
                E::TextDelta("自动标题".into()),
                E::Stop {
                    reason: yi_agent_core::provider::StopReason::EndTurn,
                },
            ])));
        }
        Ok(Box::pin(futures::stream::iter(vec![
            E::TextDelta("ok".into()),
            E::Stop {
                reason: yi_agent_core::provider::StopReason::EndTurn,
            },
        ])))
    }
}

struct CountingTitleProvider {
    title_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl yi_agent_core::Provider for CountingTitleProvider {
    async fn call_stream(
        &self,
        req: yi_agent_core::provider::ProviderRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, yi_agent_core::provider::ProviderEvent>,
        yi_agent_core::provider::ProviderError,
    > {
        let is_title = req.system.as_deref() == Some(yi_agent_core::title::TITLE_INSTRUCTIONS);
        if is_title {
            self.title_calls.fetch_add(1, Ordering::SeqCst);
        }
        let text = if is_title { "即时标题" } else { "ok" };
        Ok(Box::pin(futures::stream::iter(vec![
            yi_agent_core::provider::ProviderEvent::TextDelta(text.into()),
            yi_agent_core::provider::ProviderEvent::Stop {
                reason: yi_agent_core::provider::StopReason::EndTurn,
            },
        ])))
    }
}

async fn read_until_method(h: &mut Harness, method: &str) -> serde_json::Value {
    for _ in 0..40 {
        let v = h.read_value().await;
        if v.get("method").and_then(|m| m.as_str()) == Some(method) {
            return v;
        }
    }
    panic!("expected a {method} notification");
}

fn pin_test_config(dir: &std::path::Path) -> RuntimeConfig {
    let mut c = test_config();
    c.workdir = dir.to_path_buf();
    c
}

async fn read_notification(h: &mut Harness, method: &str) -> serde_json::Value {
    h.read_matching(64, |v| {
        v.get("method").and_then(|m| m.as_str()) == Some(method)
    })
    .await
}

async fn anthropic_sse_server() -> wiremock::MockServer {
    use wiremock::matchers::{method as wm_method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    let sse = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"m\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":1}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    Mock::given(wm_method("POST"))
        .and(wm_path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse),
        )
        .mount(&server)
        .await;
    server
}

async fn upsert_model(
    h: &mut Harness,
    id: u64,
    name: &str,
    provider: &str,
    model: &str,
) -> serde_json::Value {
    h.send(&format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"model/upsert","params":{{"name":"{name}","provider":"{provider}","api_url":"https://{name}.example","model":"{model}","api_key":"key-{name}"}}}}"#
    ))
    .await;
    read_response(h, id).await
}

fn seed_capabilities(models_path: &std::path::Path, name: &str, supported: &[&str]) {
    use yi_agent_runtime::models::{
        ChannelCapability, ModelEntry, ModelProvider, ReasoningCapabilities, load_catalog_from,
        save_catalog_to,
    };
    let mut catalog = load_catalog_from(models_path);
    catalog.models.push(ModelEntry {
        name: name.to_string(),
        provider: ModelProvider::Openai,
        api_url: "https://seed.example".into(),
        model: "m".into(),
        api_key: "k".into(),
        api_style: Some("chat".into()),
        reasoning_effort: None,
        reasoning_capabilities: Some(ReasoningCapabilities {
            chat: Some(ChannelCapability {
                supported: supported.iter().map(|s| s.to_string()).collect(),
                probed_at: 1,
                error: None,
            }),
            ..Default::default()
        }),
        context_length: None,
    });
    catalog.default_model = Some(name.to_string());
    save_catalog_to(models_path, &catalog).unwrap();
}

fn test_catalog() -> Arc<yi_agent_runtime::models::ModelCatalog> {
    Arc::new(model_catalog_entries(Some("A")))
}

fn fixed_watcher(
    catalog: Arc<yi_agent_runtime::models::ModelCatalog>,
) -> Arc<yi_agent_runtime::models::CatalogWatcher> {
    yi_agent_runtime::models::CatalogWatcher::from_catalog(
        std::path::PathBuf::new(),
        (*catalog).clone(),
    )
}

fn test_catalog_without_default() -> Arc<yi_agent_runtime::models::ModelCatalog> {
    Arc::new(model_catalog_entries(None))
}

fn model_catalog_entries(default_model: Option<&str>) -> yi_agent_runtime::models::ModelCatalog {
    use yi_agent_runtime::models::{ModelCatalog, ModelEntry, ModelProvider};
    ModelCatalog {
        models: vec![
            ModelEntry {
                name: "A".into(),
                provider: ModelProvider::Anthropic,
                api_url: "https://a.example".into(),
                model: "model-a".into(),
                api_key: "key-a".into(),
                api_style: None,
                reasoning_effort: None,
                reasoning_capabilities: None,
                context_length: None,
            },
            ModelEntry {
                name: "B".into(),
                provider: ModelProvider::Openai,
                api_url: "https://b.example".into(),
                model: "model-b".into(),
                api_key: "key-b".into(),
                api_style: None,
                reasoning_effort: None,
                reasoning_capabilities: None,
                context_length: None,
            },
        ],
        default_model: default_model.map(str::to_string),
        subagent_model: None,
    }
}

// ---------- merged stdio + loopback ws (serve_stdio_with_relay) ----------

struct MergedHarness {
    client_w: tokio::io::DuplexStream,
    client_r: tokio::io::BufReader<tokio::io::DuplexStream>,
    addr: std::net::SocketAddr,
    token: String,
    #[allow(dead_code)]
    hub: Arc<crate::broadcast::Broadcaster>,
    handle: tokio::task::JoinHandle<anyhow::Result<()>>,
    _index_dir: tempfile::TempDir,
    /// 隔离的 workdir（理由同 `Harness::_fallback_workdir`）。
    #[allow(dead_code)]
    _fallback_workdir: Option<tempfile::TempDir>,
}

impl MergedHarness {
    async fn new() -> Self {
        // 单测不起真中继。
        Self::new_with_relay(None).await
    }

    async fn new_with_relay(relay_url: Option<String>) -> Self {
        let cfg = test_config();
        let (client_w, server_r) = tokio::io::duplex(64 * 1024);
        let (server_w, client_r) = tokio::io::duplex(64 * 1024);
        let index_dir = tempfile::TempDir::new().unwrap();
        // 与主构造器同样隔离 workdir（理由见 `with_config_and_scope`）。
        let fallback_workdir =
            if cfg.workdir == std::path::Path::new(crate::test_support::DEFAULT_TEST_WORKDIR) {
                Some(tempfile::TempDir::new().unwrap())
            } else {
                None
            };
        let mut cfg = cfg;
        if let Some(dir) = &fallback_workdir {
            cfg.workdir = dir.path().to_path_buf();
        }
        let workspaces = Arc::new(WorkspaceIndex::new(
            index_dir.path().join("workspaces.json"),
        ));
        let pairing = Arc::new(PairingState::new(crate::device_store::DeviceStore::new(
            index_dir.path().join("devices.json"),
        )));
        let hub = Arc::new(crate::broadcast::Broadcaster::new());
        let theme = crate::theme_tool::ThemeHandle::new(cfg.workdir.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // stdin 侧本地客户端仍是生产 stdio 的 Admin。
        let handle = tokio::spawn(serve_scoped_with_loopback(
            server_r,
            server_w,
            cfg,
            PERMISSION_TIMEOUT,
            workspaces,
            Arc::clone(&pairing),
            Arc::clone(&hub),
            RuntimeAttachments {
                runtimes: Arc::new(StdMutex::new(HashMap::new())),
                thread_roots: Arc::new(StdMutex::new(HashMap::new())),
                managed: Arc::new(StdMutex::new(None)),
                bringup: Arc::new(yi_agent_subagent::binding::BringUpChoice::default()),
                models_path: PathBuf::new(),
                websearch_path: PathBuf::new(),
                settings_path: PathBuf::new(),
                // 这些用例不涉运行期设置：静态装配。
                live_settings: None,
                lead_agent_enabled: false,
                onboarding_preferences_path: PathBuf::new(),
                resident_dir: PathBuf::new(),
                // 空路径 = 不读真实 home；这些用例不查 skill。
                skills_home: PathBuf::new(),
                instance_home: PathBuf::new(),
                remote_dir: PathBuf::new(),
                // 与手工装配的 serve_stdio 用例同一个约束:空 remote_dir +
                // `None` 会让 serve() 把旁支宿主目录建到当前工作目录。
                live_side_parents: None,
                side_registry: Some(test_side_registry(index_dir.path(), None)),
                // 第二套 harness（`serve_scoped_with_loopback`）也要能跑 side
                // 测试：默认开，关掉的用例自行构造。
                side_gate: Arc::new(|| true),
                theme,
                git_diff: crate::git_diff_tool::GitDiffHandle::new(),
                view_frames: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
                // 测试注入:只记账,不碰真实 launchd / home。
                watchman_install: Arc::new(|_exe: &Path, _home: &Path| Ok(())),
                watchman_uninstall: Arc::new(|_home: &Path| Ok(())),
                watchman_home: PathBuf::new(),
                plugin_catalog: PathBuf::new(),
                // 测试注入:同上,不拉起真实 daemon。
                resolve_plugin: Arc::new(|_name: &str| -> Option<PathBuf> { None }),
                ensure_daemon: Arc::new(|_project: &Path| -> Result<bool, String> { Ok(true) }),
                lead_inbox: crate::lead::LeadInbox::new(),
                lead_replies: crate::lead::LeadReplies::new(),
                lead_control: crate::lead_control::LeadControlInbox::new(),
                lead_control_replies: crate::lead_control::LeadControlReplies::new(),
            },
            build_test_agent,
            Scope::Admin,
            Some(listener),
            relay_url,
        ));
        // 手机侧凭据:与主循环**共享**的 `pairing` 取一枚(按名字幂等),故 ws 前端认得。
        let token = pairing.seed_local_device("relay-bridge");
        Self {
            client_w,
            client_r: tokio::io::BufReader::new(client_r),
            addr,
            token,
            hub,
            handle,
            _index_dir: index_dir,
            _fallback_workdir: fallback_workdir,
        }
    }

    async fn send(&mut self, line: &str) {
        use tokio::io::AsyncWriteExt;
        self.client_w.write_all(line.as_bytes()).await.unwrap();
        self.client_w.write_all(b"\n").await.unwrap();
        self.client_w.flush().await.unwrap();
    }

    async fn read_value(&mut self) -> serde_json::Value {
        use tokio::io::AsyncBufReadExt;
        let mut buf = String::new();
        let n = tokio::time::timeout(EVENT_WAIT, self.client_r.read_line(&mut buf))
            .await
            .expect("timed out waiting for a message")
            .expect("read_line failed");
        assert!(n > 0, "unexpected EOF while waiting for a message");
        serde_json::from_str(buf.trim()).expect("server wrote invalid JSON")
    }

    async fn shutdown(self) {
        let MergedHarness {
            client_w, handle, ..
        } = self;
        drop(client_w);
        let res = tokio::time::timeout(EVENT_WAIT, handle)
            .await
            .expect("server must shut down within the timeout")
            .expect("server task must not panic");
        assert!(
            res.is_ok(),
            "merged server should return Ok on stdio EOF: {res:?}"
        );
    }

    async fn connect_ws(
        &self,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let uri: axum::http::Uri = format!("ws://{}/ws", self.addr).parse().unwrap();
        let request = tokio_tungstenite::tungstenite::ClientRequestBuilder::new(uri)
            .with_header("Authorization", format!("Bearer {}", self.token))
            .into_client_request()
            .unwrap();
        let (ws, _) = tokio_tungstenite::connect_async(request)
            .await
            .expect("ws client must authenticate with the seeded token");
        ws
    }
}

async fn ws_send_json<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>, line: &str)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use futures::SinkExt as _;
    ws.send(tokio_tungstenite::tungstenite::Message::Text(line.into()))
        .await
        .unwrap();
}

async fn ws_recv_json<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>) -> serde_json::Value
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let msg = tokio::time::timeout(EVENT_WAIT, ws.next())
        .await
        .expect("timed out waiting for a ws frame")
        .expect("ws stream ended before a frame arrived")
        .expect("ws transport error");
    serde_json::from_str(msg.to_text().expect("server sent a non-text frame"))
        .expect("server sent invalid JSON")
}

async fn ws_read_response<S>(
    ws: &mut tokio_tungstenite::WebSocketStream<S>,
    want: u64,
) -> serde_json::Value
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    for _ in 0..16 {
        let v = ws_recv_json(ws).await;
        if v.get("id") == Some(&serde_json::json!(want)) {
            return v;
        }
    }
    panic!("no ws response with id {want}");
}

async fn ws_await_notification<S>(
    ws: &mut tokio_tungstenite::WebSocketStream<S>,
    method: &str,
) -> serde_json::Value
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    for _ in 0..16 {
        let v = ws_recv_json(ws).await;
        if v.get("method").and_then(|m| m.as_str()) == Some(method) {
            return v;
        }
    }
    panic!("no ws notification `{method}`");
}

fn recording_roots_factory(
    seen: Arc<std::sync::Mutex<Vec<Vec<PathBuf>>>>,
) -> impl Fn(
    Option<yi_agent_core::Session>,
    &Path,
    crate::thread_store::ThreadMode,
) -> anyhow::Result<BuiltAgent>
+ Send
+ Sync
+ 'static {
    move |session, cwd, mode| {
        seen.lock().unwrap().push(current_session_extra_roots());
        build_test_agent(session, cwd, mode)
    }
}

fn read_meta(cwd: &Path, tid: &str) -> serde_json::Value {
    let path = cwd
        .join(".yi-agent")
        .join("threads")
        .join(format!("{tid}.meta.json"));
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    serde_json::from_str(&raw).unwrap()
}

// ---------------------------------------------------------------------------
// `plugin/install`:宿主把一个插件装进一个项目（spec §4.4 方案 A 的宿主侧）。
//
// 全新项目没有 `.yi-agent/supervisors/<plugin>.json`，故它的插件不会被拉起，
// `plugin/query` 也就够不着它——这个循环只能由宿主从外面打破。安装 = 让插件
// 自述清单 + 写盘 + 打开开关，缺一都会得到一个「注册了、起来了、却永不推进」
// 的插件。
// ---------------------------------------------------------------------------

fn write_fake_plugin(h: &Harness, name: &str, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = h.plugins_dir.path().join(name);
    std::fs::write(&path, body).unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions).unwrap();
    path
}

async fn send_install(h: &mut Harness, id: u64, project: &std::path::Path, plugin: &str) {
    let params = serde_json::json!({
        "project": project.to_string_lossy(),
        "plugin": plugin,
    });
    h.send(
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "plugin/install",
            "params": params,
        })
        .to_string(),
    )
    .await;
}

fn fake_plugin_printing(h: &Harness, name: &str, manifest: &str) -> std::path::PathBuf {
    // 清单里含引号与换行，故用 `cat <<'EOF'`，避免 shell 再解释一遍。
    write_fake_plugin(
        h,
        name,
        &format!("#!/bin/sh\ncat <<'MANIFEST_EOF'\n{manifest}\nMANIFEST_EOF\n"),
    )
}

// ---------------------------------------------------------------------------
// `plugin/uninstall`:`plugin/install` 的镜像。
//
// 用户右键「移除看板」原本只撤销常驻登记 + 删状态目录:daemon 还在跑、清单还
// 在、开关还开着,于是插件下一拍就把目录重建出来——「移除」成了「重启」。这里
// 钉死真正的卸载语义。
// ---------------------------------------------------------------------------

async fn send_uninstall(h: &mut Harness, id: u64, project: &std::path::Path, plugin: &str) {
    let params = serde_json::json!({
        "project": project.to_string_lossy(),
        "plugin": plugin,
    });
    h.send(
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "plugin/uninstall",
            "params": params,
        })
        .to_string(),
    )
    .await;
}

fn install_and_seed_plugin(h: &Harness, project: &std::path::Path, plugin: &str, switch_key: &str) {
    let manifest = serde_json::json!({
        "name": plugin,
        "command": "/bin/true",
        "switch_key": switch_key,
    });
    fake_plugin_printing(h, plugin, &manifest.to_string());
    let dir = project.join(".yi-agent");
    std::fs::create_dir_all(dir.join(plugin)).unwrap();
    std::fs::write(dir.join(plugin).join("board.json"), "{}").unwrap();
    std::fs::create_dir_all(dir.join("logs")).unwrap();
    std::fs::write(dir.join("logs").join(format!("{plugin}.log")), "old").unwrap();
    // 别人的键:卸载只该删自己那一个。
    std::fs::write(
        dir.join("preferences.json"),
        r#"{"subagent_runtime":"always"}"#,
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// 跨端未读状态同步（设计见 docs/superpowers/specs/2026-10-25-unread-read-state-sync-design.md）
// ---------------------------------------------------------------------------

fn watermark_of(list: &serde_json::Value, thread_id: &str, field: &str) -> u64 {
    find_listed(list, thread_id)[field]
        .as_u64()
        .unwrap_or_else(|| panic!("{field} must be present on {thread_id}: {list}"))
}

#[test]
#[ignore]
fn child_holds_thread_lock() {
    use std::io::Write;
    let path = std::path::PathBuf::from(std::env::var("YI_LOCK_PATH").unwrap());
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .unwrap();
    let rc = unsafe {
        libc::flock(
            std::os::unix::io::AsRawFd::as_raw_fd(&file),
            libc::LOCK_EX | libc::LOCK_NB,
        )
    };
    assert_eq!(rc, 0, "子进程必须能拿到锁");
    println!("LOCK_HELD");
    std::io::stdout().flush().unwrap();
    // 持锁直到被父进程 kill。
    std::thread::sleep(std::time::Duration::from_secs(30));
}

/// 只认 `p1` 的替身解析:测试用它把「打开旁支」从真实项目目录里摘出来。
///
/// `gate` 不是装饰:它让 `resolve` 卡在屏障上,于是「主循环有没有被堵住」变成
/// 可断言的事实,而不是靠计时猜测(见 `tests::side` 的不阻塞用例)。
pub(crate) struct StubSideResolver {
    pub(crate) gate: Option<Arc<tokio::sync::Barrier>>,
}

impl crate::side_conversation::SideParentResolver for StubSideResolver {
    fn resolve<'a>(
        &'a self,
        parent: &'a str,
    ) -> futures::future::BoxFuture<
        'a,
        yi_agent_runtime::side_conversation::SideResult<
            yi_agent_runtime::side_conversation::EffectiveSideSource,
        >,
    > {
        Box::pin(async move {
            if let Some(gate) = &self.gate {
                gate.wait().await;
            }
            Err(yi_agent_runtime::side_conversation::SideError::new(
                yi_agent_runtime::side_conversation::SideErrorReason::UnknownThread,
                format!("stub 不解析 {parent}"),
            ))
        })
    }
}

/// 只认本进程活父表的解析器:垂直集成用。
///
/// 生产路径是 [`crate::side_conversation::WorkspaceResolver`]:先查活父表,查不到
/// 再冷读磁盘。这里去掉冷读,理由是测试里那些父会话只存在于内存(host 的
/// `thread/start` 建的是临时目录里的 thread,断言的是「未落盘的进度也能进快照」),
/// 冷读会把用例变成「磁盘上恰好有什么」而不是「旁支拿到了什么」。
///
/// 活父表**必须**是宿主注入的那一份:自造一份就只是又一个替身,
/// 「父每轮收尾后重发布」这条链路根本没被覆盖。
pub(crate) struct LiveParentResolver {
    pub(crate) live: Arc<crate::side_conversation::LiveSideParents>,
}

impl crate::side_conversation::SideParentResolver for LiveParentResolver {
    fn resolve<'a>(
        &'a self,
        parent: &'a str,
    ) -> futures::future::BoxFuture<
        'a,
        yi_agent_runtime::side_conversation::SideResult<
            yi_agent_runtime::side_conversation::EffectiveSideSource,
        >,
    > {
        Box::pin(async move {
            let publisher = self.live.get(parent).ok_or_else(|| {
                yi_agent_runtime::side_conversation::SideError::new(
                    yi_agent_runtime::side_conversation::SideErrorReason::ParentNotStarted,
                    format!("父会话 {parent} 不在本进程驱动中"),
                )
            })?;
            publisher.snapshot().await
        })
    }
}

/// 与 [`test_side_registry`] 相同,但解析走本进程的活父表。
///
/// 同时把 `SideHostResources` 交出去:验收要断言「退出之后资源槽归零」,
/// 而槽表在 registry 里是私有字段,从别的模块看不到。测试自己持有同一个
/// `Arc` 就能直接读 `slots_in_use()`,不必为断言在生产类型上加口子。
pub(crate) fn live_parent_side_registry(
    home: &Path,
    live: Arc<crate::side_conversation::LiveSideParents>,
) -> (
    Arc<crate::side_conversation::SideRegistry>,
    Arc<yi_agent_runtime::side_conversation::SideHostResources>,
) {
    let pool = Arc::new(yi_agent_tools::SideBlockingPool::new(2, 1, 8, 4));
    let resources = yi_agent_runtime::side_conversation::SideHostResources::new(
        home.to_path_buf(),
        Arc::clone(&pool),
    )
    .expect("test side host resources");
    let registry = Arc::new(crate::side_conversation::SideRegistry::new(
        Arc::new(LiveParentResolver { live }),
        Arc::clone(&resources),
    ));
    (registry, resources)
}

/// 测试用旁支注册表:宿主资源落在 `home` 下,调用方负责保活那个临时目录。
///
/// 手工装配 `RuntimeAttachments` 的用例必须用它,而**不能**留
/// `side_registry: None` + 空 `remote_dir`:那会让 `serve()` 把
/// `side-conversations/` 建到当前工作目录(即源码树里),实测已经发生。
pub(crate) fn test_side_registry(
    home: &Path,
    gate: Option<Arc<tokio::sync::Barrier>>,
) -> Arc<crate::side_conversation::SideRegistry> {
    let pool = Arc::new(yi_agent_tools::SideBlockingPool::new(2, 1, 8, 4));
    let resources = yi_agent_runtime::side_conversation::SideHostResources::new(
        home.to_path_buf(),
        Arc::clone(&pool),
    )
    .expect("test side host resources");
    Arc::new(crate::side_conversation::SideRegistry::new(
        Arc::new(StubSideResolver { gate }),
        resources,
    ))
}

#[cfg(test)]
mod assembly;
mod compact_blocking;
#[cfg(test)]
mod image;
#[cfg(test)]
mod lead;
#[cfg(test)]
mod management;
#[cfg(test)]
mod model_settings;
#[cfg(test)]
mod plugin_admission;
#[cfg(test)]
mod plugin_theme;
#[cfg(test)]
mod process;
#[cfg(test)]
mod runtime_settings;
mod runtime_status;
#[cfg(test)]
mod side;
#[cfg(test)]
mod thread_index;
#[cfg(test)]
mod thread_lifecycle;
#[cfg(test)]
mod title;
#[cfg(test)]
mod transport;
#[cfg(test)]
mod turn;
