//! `thread/diff/read` 与它的 git 兜底 helper。
//!
//! 从 `rpc/thread/mod.rs` 的 `handle()` 逐字搬出（wave2 C3b）。

use crate::protocol::{RequestEnvelope, RequestId, RpcError};
use crate::rpc::{AppState, Flow};
use crate::server::{
    err_response, ok_response, require_known_thread, require_thread_id, write_response,
};
use serde_json::json;
use std::sync::Arc;

pub(crate) async fn handle(
    state: &mut AppState<'_>,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    match req.method.as_str() {
        "thread/diff/read" => {
            let Some(thread_id) =
                require_thread_id(state.hub, state.client, &req.params, id.clone()).await?
            else {
                return Ok(Flow::Continue);
            };
            if !require_known_thread(
                state.hub,
                state.client,
                state.threads,
                &thread_id,
                id.clone(),
            )
            .await?
            {
                return Ok(Flow::Continue);
            }
            // cwd 是 diff 的唯一来源：thread 的 cwd，缺省兜底 cfg.workdir。
            let cwd = state
                .threads
                .get(&thread_id)
                .map(|t| t.cwd.clone())
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| state.cfg.workdir.display().to_string());
            // 计算搬到**独立 task** 里做,而不是在本臂内联等待。理由与
            // `thread/listAll` 逐字相同:这一臂要跑 5+ 次 git 子进程,并读每个
            // 未跟踪文件(判二进制 / 合成 unified diff;每个文件只读一趟、超过
            // `MAX_UNTRACKED_FILE_BYTES` 就不读正文)。盘慢、目录多时这段同步
            // IO 会把主循环整个占住,期间收到的 `turn/interrupt` 只能干等,
            // 用户看到的是「点了 Stop 没反应」。
            //
            // 主 runtime 是多线程的(`crates/yi-agent/src/main.rs` 里用
            // `tokio::runtime::Runtime::new()` 建的就是多线程 runtime;本 crate
            // 没有 `#[tokio::main]` 属性)。把计算挪到别的 worker 后,主循环
            // 立刻回到 `select!`。
            //
            // 代价是计算期间不能借 `AppState`:故先把参数快照成纯值。
            let repo = std::path::PathBuf::from(&cwd);
            let base = req
                .params
                .get("base")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let commit = req
                .params
                .get("commit")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let path = req
                .params
                .get("path")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            // 兜底应答的形状取决于请求种类(`commit` / `path` = 单文件,其余 =
            // 整线程)。必须在参数被移进阻塞闭包之前定下来,否则 panic 分支只剩
            // 一个通用形状可选,而两个调用方的契约并不相同。
            let wants_text = commit.is_some() || path.is_some();
            let fallback_base = base.clone();
            let hub = Arc::clone(state.hub);
            let client = state.client.clone();
            tokio::spawn(async move {
                let computed = tokio::task::spawn_blocking(move || {
                    // 这一位描述的是**本线程这一次**计算：先清，算完再取走。
                    crate::git_diff::reset_git_timeout();
                    let body = if let Some(commit) = commit {
                        let (unified_diff, truncated) =
                            crate::git_diff::commit_diff(&repo, &commit);
                        json!({ "unifiedDiff": unified_diff, "truncated": truncated })
                    } else if let Some(path) = path {
                        // 锚点由 Task 2 从 thread meta 接上；本任务先传 `None`（行为不变）。
                        let d = crate::git_diff::thread_diff(&repo, base.as_deref(), None);
                        let (unified_diff, truncated) =
                            crate::git_diff::file_diff(&repo, d.merge_base.as_deref(), &path);
                        json!({ "unifiedDiff": unified_diff, "truncated": truncated })
                    } else {
                        serde_json::to_value(crate::git_diff::thread_diff(
                            &repo,
                            base.as_deref(),
                            None,
                        ))
                        .unwrap_or(json!(null))
                    };
                    let timed_out = crate::git_diff::take_git_timeout();
                    (body, timed_out)
                })
                .await;
                match computed {
                    // 超时分支没有可信的 body 可用，故不绑定它。
                    Ok((_body, true)) => {
                        // git 超时时 `git_ok` 的 `None` 无法与「真没有改动」区分,
                        // 若照常回 body 就会把挂住的仓库渲染成「没有改动」。
                        // 按 spec §4.3:这种情形应当失败,而不是沉默地报空。
                        let _ = write_response(
                            &hub,
                            &client,
                            err_response(id, RpcError::internal("git timed out")),
                        )
                        .await;
                    }
                    Ok((body, false)) => {
                        let _ = write_response(&hub, &client, ok_response(id, body)).await;
                    }
                    Err(error) => {
                        // 只在 panic 时到这里。回一个合法形状的空结果,而不是
                        // 静默丢响应——后者会让客户端一直等。
                        eprintln!("[app-server] thread/diff/read task failed: {error}");
                        let body = diff_read_fallback(wants_text, fallback_base.as_deref());
                        let _ = write_response(&hub, &client, ok_response(id, body)).await;
                    }
                }
            });
            Ok(Flow::Continue)
        }
        _ => Ok(Flow::Unknown),
    }
}

/// `thread/diff/read` 在阻塞计算 panic 时的兜底应答体。
///
/// 两个调用方的契约不同,不能共用一种形状:
/// - 带 `commit` / `path` 的**单文件**请求 → `DiffTextResult`
///   (`{unifiedDiff, truncated}`,`desktop/src/lib/protocol.ts`)。
/// - 只带 `threadId` 的**整线程**请求(前端 `loadThreadDiff` 发的就是这个)
///   → `ThreadDiffResult`。前端不做形状检查,直接 `setThreadDiff(r)` 后由
///   `GitDiffView` 解引用 `diff.files.length`;少给字段不是「显示无改动」而是
///   渲染期抛错。
///
/// 只在 `spawn_blocking` panic 时走到,属降级路径,故 `baseKind` 取 `"none"`。
pub(crate) fn diff_read_fallback(wants_text: bool, base: Option<&str>) -> serde_json::Value {
    if wants_text {
        return json!({ "unifiedDiff": "", "truncated": false });
    }
    // 字段必须是**齐的**:`ThreadDiff` 带 `#[serde(rename_all = "camelCase")]`,
    // 故这里是 camelCase 而非 snake_case。
    json!({
        "base": base,
        "baseKind": "none",
        "mergeBase": serde_json::Value::Null,
        "commits": [],
        "files": [],
        "unifiedDiff": "",
        "truncated": false,
    })
}
