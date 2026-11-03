//! `runtime/*` 域的 RPC 处理函数。
//!
//! 本域只有一个只读方法:`runtime/status`。刻意没有 `runtime/confirmRestart` /
//! `runtime/stop`,原因不是"还没写",而是**破坏性运行时控制不属于业务 RPC**:
//!
//! * 业务 RPC 可以经 websocket / relay 抵达,而切换或停止只能在本机做(控制通道是
//!   本地 unix socket)。把它放进 dispatcher 就等于给远端一个本机才有的权力。
//! * 桌面壳自己持有 App 的租约与身份,preview / confirm 由它经控制通道发起;把它
//!   复制成一条业务 RPC,就有了第二处能消费一次性 token 的地方。
//!
//! 所以这里的职责只有一件:把 guardian 的只读状态翻译成前端视图。`remote` 由调用
//! 方给出(本地 stdio 还是 ws/relay),视图据此把 `localRestartAvailable` 置假——
//! 远端能**看到**版本不一致,但不能假装自己能重启。

use crate::protocol::{RequestEnvelope, RequestId};
use crate::rpc::{AppState, Flow};
use crate::runtime_status::{self, StatusSource};
use crate::server::{ok_response, write_response};

/// 一次状态读取的预算。与 `CONTROL_REQUEST_TIMEOUT` 同量级:超过它的守护进程就是
/// 卡住了,让 UI 早一点显示 unknown 比让它等更久更有用。
const STATUS_BUDGET: std::time::Duration = yi_agent_lifecycle::CONTROL_REQUEST_TIMEOUT;

pub(crate) async fn handle(
    state: &mut AppState<'_>,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    match req.method.as_str() {
        "runtime/status" => {
            let remote = state.is_remote;
            let view = read_view(state, remote).await;
            let value = serde_json::to_value(view)
                .map_err(|error| anyhow::anyhow!("could not encode the status view: {error}"))?;
            write_response(state.hub, state.client, ok_response(id, value)).await?;
            Ok(Flow::Continue)
        }
        _ => Ok(Flow::Unknown),
    }
}

/// Reads one page of status for the project this session serves.
///
/// The scope is derived from the session's own workdir, never from a request
/// parameter: a caller that could name a scope could ask about another project's
/// runtime, and a status read is not a way to enumerate somebody else's clients.
async fn read_view(state: &AppState<'_>, remote: bool) -> crate::runtime_status::RuntimeStatusView {
    let project = &state.cfg.workdir;
    let runtime_dir = yi_agent_subagent::attach::project_runtime_directory(project);
    let control_path = yi_agent_lifecycle::scope::control_path(&runtime_dir);
    let scope_key = runtime_dir.to_string_lossy().into_owned();
    let cursor = None;

    // The blocking control client runs off the async runtime: a wedged guardian must
    // not occupy a worker thread that other RPCs need.
    let path = control_path.clone();
    let read = tokio::task::spawn_blocking(move || {
        runtime_status::read_status(&path, cursor, STATUS_BUDGET)
    })
    .await;

    let (snapshot, unreachable) = match read {
        Ok(Ok(snapshot)) => (snapshot, None),
        Ok(Err(error)) => (None, Some(error.to_string())),
        // A panicked blocking task means the read itself is broken; saying so is
        // better than reporting "no runtime", which would look normal.
        Err(error) => (
            None,
            Some(format!("the status read did not complete: {error}")),
        ),
    };

    runtime_status::view(
        StatusSource {
            scope_key,
            // Only the local shell can say which artifact it shipped with, and it
            // says so once on the handshake. A CLI or a remote device has no such
            // expectation, and `None` makes the view report `Unknown` rather than
            // claim a match nobody checked. The wrapper's version number is
            // deliberately not used as a fallback: it names different bytes.
            expected: expected_identity(state.shell_authorization),
            snapshot,
            unreachable,
        },
        remote,
    )
}

/// Turns the shell's expectation into the shape the classifier compares against.
///
/// The two structs differ because one is the shell's narrow statement (no sha256, no
/// executable path) and the other is the full artifact identity the switch path
/// needs. Fields the classification does not read are filled with values that can
/// only mean "not compared here" -- and `classify` never looks at them.
fn expected_identity(
    authorization: Option<&crate::shell_authorization::ShellAuthorization>,
) -> Option<yi_agent_lifecycle::types::ArtifactIdentity> {
    let expected = authorization?.expected_runtime.as_ref()?;
    Some(yi_agent_lifecycle::types::ArtifactIdentity {
        build_id: expected.build_id.clone(),
        display_version: expected.display_version.clone(),
        // Not used by `classify_compatibility`; the shell's statement carries no
        // path because a status read must not become a way to learn one.
        executable: std::path::PathBuf::new(),
        sha256: String::new(),
        schema_min: expected.schema_min,
        schema_max: expected.schema_max,
        handover_version: 0,
        business_protocol_min: expected.business_protocol_min,
        business_protocol_max: expected.business_protocol_max,
        capabilities: expected.capabilities.clone(),
    })
}

/// 门禁:本域没有任何需要 Control/Admin 的方法,故不需要额外检查。
///
/// 写在这里是为了让下一位作者看到"为什么没有门禁"的答案,而不是自己去猜。
#[allow(dead_code)]
const _READONLY_ONLY: () = ();
