//! `image/*` 域的 RPC 处理函数。
//!
//! 从 `server.rs` 的 `serve()` dispatch 逐字搬出(Task 3 batch 2)。含分片上传(begin/chunk/commit/abort)与限根读取;
//!
//! 臂体保持原样,只有两处改名:主循环局部量改走 [`AppState`] 字段(`hub` ->
//! `state.hub` 等;字段本身已是引用,故冗余的 `&` 一并去掉),以及 `continue`
//! 换成 `return Ok(Flow::Continue)`——后者的语义由 `serve()` 翻回 `continue`,
//! 与搬移前逐字一致。

use base64::Engine as _;
use serde_json::json;

use crate::protocol::{RequestEnvelope, RequestId, RpcError};
use crate::rpc::{AppState, Flow};
use crate::server::{
    IMAGE_READ_CHUNK_BYTES, IMAGE_READ_MAX_BYTES, err_response, image_media_type, ok_response,
    thread_cwd, write_response,
};

/// 处理本域的一个 RPC 请求。
///
/// 返回 [`Flow::Continue`] 表示「本域已应答,主循环回到下一轮」(等价于搬移前臂
/// 里的 `continue`);[`Flow::Unknown`] 表示方法不在本域内,交回 `serve()` 的原始
/// dispatch(走既有的 `method_not_found`),响应形状与搬移前一致。
pub(crate) async fn handle(
    state: &mut AppState<'_>,
    req: &RequestEnvelope,
    id: RequestId,
) -> anyhow::Result<Flow> {
    match req.method.as_str() {
        // 分片回传一张图片的**原始字节**:客户端按 `nextOffset` 循环
        // 拉取,base64 解码拼接后即得与磁盘逐字节相同的文件。之所以
        // 不在这里重新编码,是因为 `Item.images[].size` 记的就是存盘
        // 文件的字节长度(Task 5 的交接),两者必须一致。
        //
        // 这是一条**安全边界**:限根(拒绝 `..` 与工作区外绝对路径)+
        // 只送可解码的图(`guess_format`)+ 文件大小上限,三者缺一,
        // 本 RPC 就成了任意文件读取原语。
        "image/read" => {
            let Some(thread_id) = req.params.get("threadId").and_then(|v| v.as_str()) else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("threadId required")),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            let Some(rel) = req.params.get("path").and_then(|v| v.as_str()) else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("path required")),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            // 该 thread 的 cwd:内存优先,其次按 store 定位;都落空即
            // 未知 thread。与既有路径限额同源。
            let Some(root) = thread_cwd(state.threads, state.workspaces, state.cfg, thread_id)
            else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("unknown thread")),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            // 限根:join 后 canonicalize,再要求结果落在 cwd 之内。
            // `canonicalize` 会解掉 `..` 与符号链接,故逃逸一定现形;
            // `Path::join` 遇绝对路径会整体替换,也在此被 starts_with 拦下。
            let canonical = match root.join(rel).canonicalize() {
                Ok(p) => p,
                Err(e) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(
                            id,
                            RpcError::invalid_params(format!("cannot read image: {e}")),
                        ),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };
            let root_canonical = root.canonicalize().unwrap_or_else(|_| root.clone());
            if !canonical.starts_with(&root_canonical) {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("path escapes the workspace")),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            // 先按**元数据**长度设闸:否则一个几 GB 的文件会先被整个读进
            // 内存再被拒,分片回传反成了内存放大原语。
            let declared_len = match std::fs::metadata(&canonical) {
                Ok(m) => {
                    // 同一趟元数据里挡下**非普通文件**:FIFO 的 `len`
                    // 恒为 `0`,能骗过尺寸闸,而 `fs::read` 的 `open()`
                    // 在无写端时会**一直阻塞**——本 RPC 是安全边界,
                    // 一个构造请求就能把整条 serve 循环(所有客户端/
                    // thread)挂住。故 `!is_file()` 直接拒绝,不再多发
                    // 一次 `metadata` 系统调用。
                    if !m.is_file() {
                        write_response(
                            state.hub,
                            state.client,
                            err_response(id, RpcError::invalid_params("not a regular file")),
                        )
                        .await?;
                        return Ok(Flow::Continue);
                    }
                    m.len()
                }
                Err(e) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(
                            id,
                            RpcError::invalid_params(format!("cannot read image: {e}")),
                        ),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };
            if declared_len > IMAGE_READ_MAX_BYTES {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("image too large")),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            let bytes = match std::fs::read(&canonical) {
                Ok(b) => b,
                Err(e) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(
                            id,
                            RpcError::invalid_params(format!("cannot read image: {e}")),
                        ),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };
            // 只送可解码的图片:以魔数判定,不看扩展名。
            let format = match image::guess_format(&bytes) {
                Ok(f) => f,
                Err(_) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::invalid_params("not a supported image")),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };
            // 元数据长度与实际读到的长度理论上可能竞态,按**实际字节**复核。
            if bytes.len() as u64 > IMAGE_READ_MAX_BYTES {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("image too large")),
                )
                .await?;
                return Ok(Flow::Continue);
            }
            let offset = req
                .params
                .get("offset")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            // 客户端可要更小的一块,但不得越过帧预算。`0` 视为未指定:
            // 否则 `end == start`、`nextOffset` 不前进,客户端会陷入死循环。
            let max_bytes = req
                .params
                .get("maxBytes")
                .and_then(|v| v.as_u64())
                .filter(|n| *n > 0)
                .unwrap_or(IMAGE_READ_CHUNK_BYTES)
                .min(IMAGE_READ_CHUNK_BYTES);
            let start = (offset as usize).min(bytes.len());
            let end = start.saturating_add(max_bytes as usize).min(bytes.len());
            let next = if end < bytes.len() {
                Some(end as u64)
            } else {
                None
            };
            let data = base64::engine::general_purpose::STANDARD.encode(&bytes[start..end]);
            write_response(
                state.hub,
                state.client,
                ok_response(
                    id,
                    json!({
                        "data": data,
                        "nextOffset": next,
                        "mediaType": image_media_type(format),
                        "size": bytes.len(),
                    }),
                ),
            )
            .await?;
        }
        // iOS 分片上传（设计 §3(3)）：begin / chunk / commit / abort。
        //
        // 与 `image/read` 恰好反向：那个把工作区里的图分片送出，这个把
        // 客户端的分片收进工作区。base64 只在 `chunk.data` 里出现，由
        // 服务端解码（裁定 R6，与 `image/read` 的编码端对称）。
        //
        // 身份链：`begin` 用 `threadId` 解析 cwd（与 `image/read` 同法，
        // 复用 `thread_cwd`）；`commit` 的参数只有 `{uploadId}`，thread
        // 由记录带出再解析 cwd。
        "image/upload/begin" => {
            let Some(thread_id) = req.params.get("threadId").and_then(|v| v.as_str()) else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("threadId required")),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            let Some(root) = thread_cwd(state.threads, state.workspaces, state.cfg, thread_id)
            else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("unknown thread")),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            let name = req
                .params
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("image");
            let Some(size) = req.params.get("size").and_then(|v| v.as_u64()) else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("size required")),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            // 大小/条数上限都在 `begin` 里设闸：一个已认证客户端不能靠
            // 反复 begin 占满 inode/fd（见 `image_upload` 的常量注释）。
            match state.uploads.begin(&root, thread_id, name, size) {
                Ok(upload_id) => {
                    write_response(
                        state.hub,
                        state.client,
                        ok_response(
                            id,
                            json!({
                                "uploadId": upload_id,
                                "chunkSize": crate::image_upload::UPLOAD_CHUNK_BYTES,
                            }),
                        ),
                    )
                    .await?
                }
                Err(message) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::invalid_params(message)),
                    )
                    .await?
                }
            }
        }
        "image/upload/chunk" => {
            let Some(upload_id) = req.params.get("uploadId").and_then(|v| v.as_str()) else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("uploadId required")),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            let Some(index) = req.params.get("index").and_then(|v| v.as_u64()) else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("index required")),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            let Some(data) = req.params.get("data").and_then(|v| v.as_str()) else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("data required")),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            // 服务端解码（裁定 R6）：解码失败即拒，绝不把半个块写进
            // staging。累计上限由 `append` 按声明大小把关。
            let bytes = match base64::engine::general_purpose::STANDARD.decode(data) {
                Ok(bytes) => bytes,
                Err(e) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(
                            id,
                            RpcError::invalid_params(format!("invalid base64 chunk: {e}")),
                        ),
                    )
                    .await?;
                    return Ok(Flow::Continue);
                }
            };
            match state.uploads.append(upload_id, index, &bytes) {
                Ok(()) => {
                    write_response(state.hub, state.client, ok_response(id, json!({}))).await?
                }
                Err(message) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::invalid_params(message)),
                    )
                    .await?
                }
            }
        }
        "image/upload/commit" => {
            let Some(upload_id) = req.params.get("uploadId").and_then(|v| v.as_str()) else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("uploadId required")),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            // 设计的 `commit` 参数只有 `{uploadId}`：thread 从记录带出，
            // 再解析它该落在哪个工作区。
            let Some(thread_id) = state.uploads.thread_id_of(upload_id) else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(
                        id,
                        RpcError::invalid_params(format!("unknown upload: {upload_id}")),
                    ),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            let Some(root) = thread_cwd(state.threads, state.workspaces, state.cfg, &thread_id)
            else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("unknown thread")),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            // 裁定 R1：返回**解析句柄**（`path` 给客户端渲染，`uploadId`
            // 仍是送模型时的引用），而不是只回一个 `uploadId`。
            match state.uploads.commit(&root, &thread_id, upload_id) {
                Ok(attachment) => {
                    write_response(
                        state.hub,
                        state.client,
                        ok_response(
                            id,
                            json!({
                                "path": attachment.path,
                                "mediaType": attachment
                                    .mime
                                    .unwrap_or_else(|| "application/octet-stream".to_string()),
                                "size": attachment.size,
                            }),
                        ),
                    )
                    .await?
                }
                Err(message) => {
                    write_response(
                        state.hub,
                        state.client,
                        err_response(id, RpcError::invalid_params(message)),
                    )
                    .await?
                }
            }
        }
        "image/upload/abort" => {
            let Some(upload_id) = req.params.get("uploadId").and_then(|v| v.as_str()) else {
                write_response(
                    state.hub,
                    state.client,
                    err_response(id, RpcError::invalid_params("uploadId required")),
                )
                .await?;
                return Ok(Flow::Continue);
            };
            // 幂等：未知/已 abort 的 id 也回 `{}`（客户端重试不必区分）。
            state.uploads.abort(upload_id);
            write_response(state.hub, state.client, ok_response(id, json!({}))).await?;
        }
        // 域内兜底:本模块只实现上面列出的方法。若 `domain_for` 把别的同前缀
        // 方法也划给本域,交回 `serve()` 原始 dispatch,而不是在这里自行编造响应。
        _ => return Ok(Flow::Unknown),
    }
    // 臂体走完而**没有** `continue` 的路径,在搬移前同样落到本轮 dispatch 的
    // 末尾、回到主循环下一轮 —— 即与 `continue` 等价,故统一在此收束。
    Ok(Flow::Continue)
}
