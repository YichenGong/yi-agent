//! 分片上传的会话登记表（iOS 客户端把一张图切成若干块送上来）。
//!
//! 为什么要登记表而不是「一个 RPC 收整张图」：帧上限是 1 MiB，一张 20 MiB 的
//! 图必然要分片。分片意味着服务端得跨多次请求记住「这个 uploadId 已经收到第几
//! 块、共收了多少字节、落到哪个 staging 文件」，否则任何一块乱序/重放都会把
//! 文件拼坏。
//!
//! 分工（见设计 §4.3(b) 与 Task 8 的裁定 R1–R3）：
//! - 本模块只负责「把分片拼成一份 staging 文件，再交给 `attachments::store_attachment`
//!   落盘」——**很便宜**，只碰磁盘。
//! - **编码图片（base64）不在这里做**：那步与桌面端 `{type:"image", path}` 走的是
//!   同一条 `image_prep::prepare_image_file` 管线，在 `prepare_turn_core` 里发生。
//!   登记表因此不必持有 `PreparedImage`/base64，内存始终很小。
//! - `commit` **不删记录**：`turn/start` 之后要用 `uploadId` 反查这份已落盘的
//!   附件（见裁定 R2）。记录留到 TTL 到期由 `sweep_expired` 收走。

use crate::protocol::Attachment;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// 一条上传会话的存活期；到期由 [`UploadRegistry::sweep_expired`] 收走。
pub const UPLOAD_TTL: Duration = Duration::from_secs(10 * 60);

/// 建议客户端使用的分片大小。base64 膨胀 4/3 后约 683 KiB，仍在 1 MiB 帧内。
///
/// 这只是**建议**（`begin` 的响应里回给客户端）：服务端 `append` 不按它设限，
/// 只按累计字节数设限，所以客户端分块大一点也不会被拒。
pub const UPLOAD_CHUNK_BYTES: u64 = 512 * 1024;

/// 单个 thread 同时可存在的**未 commit**会话数上限。
///
/// 一个已认证客户端可以不断 `begin` 来占文件描述符/inode，所以必须有闸。8 条
/// 远高于正常用法（客户端通常一次只上传一张图、串行送块），又能挡住刷量。
/// 只数未 commit 的会话：已 commit 的会话没有 staging 文件，不再占 inode。
pub const MAX_UPLOAD_SESSIONS_PER_THREAD: usize = 8;

/// 全进程上传会话上限（含已 commit、尚在 TTL 内的记录），理由是内存。
pub const MAX_UPLOAD_SESSIONS: usize = 64;

/// 一条上传会话的状态。
#[derive(Debug)]
enum UploadState {
    /// 正在收分片。
    Receiving,
    /// `commit` 已完成，`uploadId` 现在指向这份已落盘的附件。
    Committed(Attachment),
}

#[derive(Debug)]
struct UploadSession {
    /// 会话 id，也是 staging 文件名。
    id: String,
    thread_id: String,
    /// 客户端给的原名（未安全化）；落盘时用于给最终文件取名。
    name: String,
    /// staging 文件路径：`<cwd>/.yi-agent/attachments/<tid>/.tmp/<uploadId>`。
    staging: PathBuf,
    size: u64,
    /// 下一个期待的分片号（从 0 起）。
    next_index: u64,
    /// 已收到的字节数（只按实际写入累加，故永远 <= size）。
    received: u64,
    created_at: Instant,
    state: UploadState,
}

impl UploadSession {
    fn is_receiving(&self) -> bool {
        matches!(self.state, UploadState::Receiving)
    }
}

/// staging 文件所在目录。
fn staging_dir(cwd: &Path, thread_id: &str) -> PathBuf {
    cwd.join(".yi-agent")
        .join("attachments")
        .join(thread_id)
        .join(".tmp")
}

/// 分片上传的会话登记表。所有方法都是同步的（只碰磁盘与内存），由 serve 主循环
/// 的单一任务串行调用，因此不需要内部加锁。
#[derive(Debug, Default)]
pub struct UploadRegistry {
    sessions: HashMap<String, UploadSession>,
}

impl UploadRegistry {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
        }
    }

    /// 开一条上传会话，返回 `uploadId`。
    ///
    /// 立刻按声明大小设闸：空文件与超过 20 MiB 的都在这里拒，避免客户端把几十
    /// 个超大上传塞进磁盘。
    pub fn begin(
        &mut self,
        cwd: &Path,
        thread_id: &str,
        name: &str,
        size: u64,
    ) -> Result<String, String> {
        if size == 0 {
            return Err("image upload must not be empty".to_string());
        }
        let max = yi_agent_tools::image_prep::MAX_IMAGE_BYTES;
        if size > max {
            return Err(format!("image is {size} bytes, over the {max} byte limit"));
        }
        let receiving_for_thread = self
            .sessions
            .values()
            .filter(|s| s.thread_id == thread_id && s.is_receiving())
            .count();
        if receiving_for_thread >= MAX_UPLOAD_SESSIONS_PER_THREAD {
            return Err(format!(
                "too many live image uploads for this thread (max {MAX_UPLOAD_SESSIONS_PER_THREAD})"
            ));
        }
        if self.sessions.len() >= MAX_UPLOAD_SESSIONS {
            return Err(format!(
                "too many live image uploads (max {MAX_UPLOAD_SESSIONS})"
            ));
        }

        let id = format!("upload-{}", uuid::Uuid::new_v4());
        let staging = staging_dir(cwd, thread_id).join(&id);
        if let Some(parent) = staging.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("could not create the staging directory: {e}"))?;
        }
        // `create` 会截断同名文件：id 是新铸的 uuid，不存在同名历史文件。
        std::fs::File::create(&staging)
            .map_err(|e| format!("could not create the staging file: {e}"))?;

        self.sessions.insert(
            id.clone(),
            UploadSession {
                id: id.clone(),
                thread_id: thread_id.to_string(),
                name: name.to_string(),
                staging,
                size,
                next_index: 0,
                received: 0,
                created_at: Instant::now(),
                state: UploadState::Receiving,
            },
        );
        Ok(id)
    }

    /// 追加一块。`index` 必须严格等于下一个期待的序号，且累计字节不得超过声明大小。
    pub fn append(&mut self, upload_id: &str, index: u64, bytes: &[u8]) -> Result<(), String> {
        let Some(session) = self.sessions.get_mut(upload_id) else {
            return Err(format!("unknown upload: {upload_id}"));
        };
        if !session.is_receiving() {
            return Err(format!("upload {upload_id} is already committed"));
        }
        if index != session.next_index {
            return Err(format!(
                "out-of-order chunk: expected index {}, got {index}",
                session.next_index
            ));
        }
        let total = session.received + bytes.len() as u64;
        if total > session.size {
            return Err(format!(
                "chunk overruns the declared size: {} + {} > {}",
                session.received,
                bytes.len(),
                session.size
            ));
        }
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&session.staging)
            .map_err(|e| format!("could not open the staging file for {upload_id}: {e}"))?;
        file.write_all(bytes)
            .map_err(|e| format!("could not write the chunk for {upload_id}: {e}"))?;
        session.next_index += 1;
        session.received = total;
        Ok(())
    }

    /// 收尾：校验收满 → 落盘 → 删 staging → 记录改为 `Committed`（**保留记录**）。
    ///
    /// 返回落盘后的 `Attachment`（含工作区相对 `path`、`size`）。重复 `commit`
    /// 同一条会话是幂等的，返回同一份附件。
    ///
    /// **不在这里判「是不是图片」**：`begin` 已按 `MAX_IMAGE_BYTES` 设闸，
    /// `commit` 只收拢字节；是不是可解码的图由 `turn/start` 的
    /// `prepare_image_file` 判定（与桌面 `{type:"image", path}` 同一条管线）。
    /// 那样本模块只干「拼块 + 落盘」，判据集中在一处。
    pub fn commit(
        &mut self,
        cwd: &Path,
        thread_id: &str,
        upload_id: &str,
    ) -> Result<Attachment, String> {
        let Some(session) = self.sessions.get_mut(upload_id) else {
            return Err(format!("unknown upload: {upload_id}"));
        };
        if let UploadState::Committed(attachment) = &session.state {
            return Ok(attachment.clone());
        }
        if session.thread_id != thread_id {
            return Err(format!("upload {upload_id} belongs to another thread"));
        }
        if session.received != session.size {
            return Err(format!(
                "upload {upload_id} is incomplete: {} of {} bytes",
                session.received, session.size
            ));
        }
        let max = yi_agent_tools::image_prep::MAX_IMAGE_BYTES;

        // `store_attachment` 从**源文件名**取最终名，而 staging 的名字是 uploadId。
        // 用一个硬链接临时给它挂上原名（O(1)，同一目录内必然成功），落盘后删掉；
        // 这样 staging 路径自始至终不变，abort 永远找得到它。
        let safe_name = crate::attachments::sanitize_filename(&session.name);
        let named = session
            .staging
            .with_file_name(format!("{}-{safe_name}", session.id));
        std::fs::hard_link(&session.staging, &named)
            .map_err(|e| format!("could not stage the upload {upload_id} for storing: {e}"))?;
        let stored = crate::attachments::store_attachment(cwd, thread_id, &named, max);
        let _ = std::fs::remove_file(&named);
        let attachment =
            stored.map_err(|e| format!("could not store the uploaded image: {e:?}"))?;

        // staging 用完即弃；失败只记不报——文件已经安全落盘，残留一个 .tmp 不该
        // 让 commit 失败。
        if let Err(e) = std::fs::remove_file(&session.staging) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!("[app-server] could not remove staging file {upload_id}: {e}");
            }
        }
        session.state = UploadState::Committed(attachment.clone());
        Ok(attachment)
    }

    /// `turn/start` 的反查入口：已 commit 的返回其附件，否则 `None`。
    ///
    /// **只认 id、不认归属**，故不能用于 `turn/start`：`uploadId` 是不透明句柄，
    /// 仅凭它就能取到附件会让任何知道/猜到该 id 的客户端把别的 thread 的图塞进
    /// 自己的 thread（跨 thread 取数）。生产路径一律走 [`Self::resolve_for`]；
    /// 本方法保留给「与归属无关」的测试断言（如 TTL 清理后记录确实消失）。
    pub fn resolve(&self, upload_id: &str) -> Option<Attachment> {
        match self.sessions.get(upload_id).map(|s| &s.state) {
            Some(UploadState::Committed(attachment)) => Some(attachment.clone()),
            _ => None,
        }
    }

    /// `turn/start` 的反查入口（带归属）：只有当会话属于 `thread_id` 且已 commit
    /// 时返回其附件，否则 `None`。
    ///
    /// 归属检查与 `commit` 同源（`session.thread_id != thread_id` 即拒），是
    /// **跨 thread 取数**的闸：`uploadId` 由客户端持有，若反查不看归属，客户端就能
    /// 把别条 thread 的已上传图片摄进自己起的 turn。
    pub fn resolve_for(&self, upload_id: &str, thread_id: &str) -> Option<Attachment> {
        let session = self.sessions.get(upload_id)?;
        if session.thread_id != thread_id {
            return None;
        }
        match &session.state {
            UploadState::Committed(attachment) => Some(attachment.clone()),
            _ => None,
        }
    }

    /// 该会话属于哪条 thread（含未 commit 的）。`commit` 的 RPC 分支据此解析 cwd：
    /// 设计的 `commit` 参数只有 `{ uploadId }`，thread 由记录带出。
    pub fn thread_id_of(&self, upload_id: &str) -> Option<String> {
        self.sessions.get(upload_id).map(|s| s.thread_id.clone())
    }

    /// 丢弃一条会话（删 staging 与记录）。已落盘的附件**不动**：它已是该 thread
    /// 附件的一部分，删掉会让回放里的气泡指不到文件。
    pub fn abort(&mut self, upload_id: &str) {
        if let Some(session) = self.sessions.remove(upload_id) {
            if let Err(e) = std::fs::remove_file(&session.staging) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    eprintln!(
                        "[app-server] could not remove staging file {}: {e}",
                        session.id
                    );
                }
            }
        }
    }

    /// `thread/delete` 的收尾：摘掉属于该 thread 的**全部**会话（含已 commit、
    /// 尚在 TTL 内的记录），未 commit 的顺带删掉其 staging 文件——与 [`Self::abort`]
    /// 同一条规则，**不动**已落盘的附件（它随 `remove_attachments` 与 thread 同一
    /// 生命周期收走）。
    ///
    /// 不 prune 的话：一条 `Committed` 记录会活到 10 分钟 TTL，让被删 thread 的
    /// `uploadId` 仍能反查到一份已被删掉的工作区路径（随后 `prepare_image_file`
    /// 读一个不存在的文件而失败）；未 commit 的 staging 槽与进程级会话名额也会被
    /// 死 thread 白占。只需内存状态与记录里已存的 staging 路径，故不必拿到 cwd。
    pub fn drop_thread(&mut self, thread_id: &str) {
        let doomed: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, s)| s.thread_id == thread_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in doomed {
            self.abort(&id);
        }
    }

    /// 收走超时的会话（serve 主循环每轮调一次）。
    pub fn sweep_expired(&mut self) {
        self.sweep_expired_at(Instant::now());
    }

    fn sweep_expired_at(&mut self, now: Instant) {
        let expired: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, s)| now.duration_since(s.created_at) >= UPLOAD_TTL)
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            self.abort(&id);
        }
    }

    /// 测试专用：把一条会话的创建时刻前移，用于确定性地驱动 TTL 清理。
    #[cfg(test)]
    fn backdate(&mut self, upload_id: &str, age: Duration) {
        if let Some(session) = self.sessions.get_mut(upload_id) {
            session.created_at = Instant::now() - age;
        }
    }

    /// 测试专用：当前会话条数。
    #[cfg(test)]
    fn len(&self) -> usize {
        self.sessions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([x as u8, y as u8, (x ^ y) as u8])
        });
        let mut buf = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    fn staging_of(cwd: &Path, thread: &str, id: &str) -> PathBuf {
        staging_dir(cwd, thread).join(id)
    }

    #[test]
    fn begin_chunk_commit_produces_a_stored_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut reg = UploadRegistry::new();
        let id = reg.begin(tmp.path(), "t1", "shot.jpg", 6).unwrap();
        reg.append(&id, 0, b"abc").unwrap();
        reg.append(&id, 1, b"def").unwrap();
        let att = reg.commit(tmp.path(), "t1", &id).unwrap();
        assert!(
            att.path.starts_with(".yi-agent/attachments/t1/"),
            "{}",
            att.path
        );
        assert_eq!(
            std::fs::read(tmp.path().join(&att.path)).unwrap(),
            b"abcdef"
        );
    }

    #[test]
    fn a_chunk_out_of_order_is_refused() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut reg = UploadRegistry::new();
        let id = reg.begin(tmp.path(), "t1", "x.jpg", 6).unwrap();
        reg.append(&id, 1, b"def").unwrap_err();
    }

    #[test]
    fn abort_removes_the_staging_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut reg = UploadRegistry::new();
        let id = reg.begin(tmp.path(), "t1", "x.jpg", 6).unwrap();
        reg.append(&id, 0, b"abc").unwrap();
        reg.abort(&id);
        assert!(
            !tmp.path()
                .join(".yi-agent/attachments/t1/.tmp")
                .join(&id)
                .exists()
        );
    }

    /// commit 之后：staging 必须消失、记录必须仍在（`turn/start` 还要反查），
    /// 且最终文件名带着客户端给的原名。
    #[test]
    fn commit_deletes_staging_but_keeps_the_record_resolvable() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut reg = UploadRegistry::new();
        let bytes = png_bytes(8, 8);
        let id = reg
            .begin(tmp.path(), "t1", "shot.png", bytes.len() as u64)
            .unwrap();
        reg.append(&id, 0, &bytes).unwrap();
        let att = reg.commit(tmp.path(), "t1", &id).unwrap();

        assert!(
            !staging_of(tmp.path(), "t1", &id).exists(),
            "staging must be gone after commit"
        );
        assert!(
            !tmp.path()
                .join(".yi-agent/attachments/t1/.tmp")
                .join(format!("{id}-shot.png"))
                .exists(),
            "the ephemeral named link must be gone"
        );
        assert_eq!(reg.resolve(&id).as_ref(), Some(&att));
        assert_eq!(reg.len(), 1, "commit must not drop the record");
        assert!(att.path.ends_with("-shot.png"), "{}", att.path);
        // 落盘文件的内容 == 分片拼接结果，size == 该文件字节长度。
        assert_eq!(std::fs::read(tmp.path().join(&att.path)).unwrap(), bytes);
        assert_eq!(
            att.size,
            std::fs::metadata(tmp.path().join(&att.path)).unwrap().len()
        );
    }

    #[test]
    fn commit_is_idempotent_and_resolve_is_none_before_commit() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut reg = UploadRegistry::new();
        let bytes = png_bytes(8, 8);
        let id = reg
            .begin(tmp.path(), "t1", "shot.png", bytes.len() as u64)
            .unwrap();
        assert!(
            reg.resolve(&id).is_none(),
            "receiving sessions are not resolvable"
        );
        reg.append(&id, 0, &bytes).unwrap();
        let first = reg.commit(tmp.path(), "t1", &id).unwrap();
        let second = reg.commit(tmp.path(), "t1", &id).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn an_incomplete_upload_cannot_commit() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut reg = UploadRegistry::new();
        let id = reg.begin(tmp.path(), "t1", "x.png", 6).unwrap();
        reg.append(&id, 0, b"abc").unwrap();
        assert!(reg.commit(tmp.path(), "t1", &id).is_err());
    }

    /// `commit` 只收拢字节、不判图片：判据在 `turn/start` 的 `prepare_image_file`
    /// （与桌面 `{type:"image", path}` 同一条管线）。这里把这个分工钉住——否则以后
    /// 有人给 `commit` 加一道魔数闸，会在 upload 路径上多出一个桌面路径没有的拒绝点。
    #[test]
    fn commit_stores_the_bytes_without_gating_on_image_format() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut reg = UploadRegistry::new();
        let id = reg.begin(tmp.path(), "t1", "not.png", 13).unwrap();
        reg.append(&id, 0, b"%PDF-1.4 fake").unwrap();
        let att = reg.commit(tmp.path(), "t1", &id).unwrap();
        assert_eq!(
            std::fs::read(tmp.path().join(&att.path)).unwrap(),
            b"%PDF-1.4 fake"
        );
        // 内容不是图，但 `turn/start` 会经由 `prepare_image_file` 拒掉它。
        assert!(
            yi_agent_tools::image_prep::prepare_image_file(
                &tmp.path().join(&att.path),
                yi_agent_core::ImageDetail::High,
                yi_agent_tools::image_prep::resolve_budget(),
            )
            .is_err()
        );
    }

    #[test]
    fn a_chunk_past_the_declared_size_is_refused() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut reg = UploadRegistry::new();
        let id = reg.begin(tmp.path(), "t1", "x.png", 4).unwrap();
        reg.append(&id, 0, b"abc").unwrap();
        let err = reg.append(&id, 1, b"de").unwrap_err();
        assert!(err.contains("overruns"), "{err}");
        // 被拒的这块不得部分写入：文件仍是 3 字节。
        assert_eq!(
            std::fs::metadata(staging_of(tmp.path(), "t1", &id))
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn begin_refuses_an_empty_or_oversized_declaration() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut reg = UploadRegistry::new();
        assert!(reg.begin(tmp.path(), "t1", "x.png", 0).is_err());
        assert!(
            reg.begin(
                tmp.path(),
                "t1",
                "x.png",
                yi_agent_tools::image_prep::MAX_IMAGE_BYTES + 1
            )
            .is_err()
        );
    }

    #[test]
    fn begin_refuses_more_than_the_per_thread_cap() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut reg = UploadRegistry::new();
        for _ in 0..MAX_UPLOAD_SESSIONS_PER_THREAD {
            reg.begin(tmp.path(), "t1", "x.png", 4).unwrap();
        }
        let err = reg.begin(tmp.path(), "t1", "x.png", 4).unwrap_err();
        assert!(err.contains("too many"), "{err}");
        // 另一条 thread 仍能开（上限是**按 thread** 计的）。
        reg.begin(tmp.path(), "t2", "x.png", 4).unwrap();
    }

    #[test]
    fn append_refuses_an_unknown_upload() {
        let mut reg = UploadRegistry::new();
        assert!(reg.append("upload-nope", 0, b"x").is_err());
    }

    #[test]
    fn abort_drops_the_record() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut reg = UploadRegistry::new();
        let id = reg.begin(tmp.path(), "t1", "x.png", 4).unwrap();
        reg.abort(&id);
        assert_eq!(reg.len(), 0);
        assert!(reg.resolve(&id).is_none());
        assert!(reg.append(&id, 0, b"x").is_err());
    }

    /// `resolve_for` 是 `turn/start` 的入口：归属不符一律 `None`（跨 thread 取数的闸），
    /// 归属相符且已 commit 才给附件。
    #[test]
    fn resolve_for_refuses_another_threads_upload() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut reg = UploadRegistry::new();
        let bytes = png_bytes(8, 8);
        let id = reg
            .begin(tmp.path(), "t1", "shot.png", bytes.len() as u64)
            .unwrap();
        // 未 commit：即便归属相符也解析不到。
        assert!(reg.resolve_for(&id, "t1").is_none());
        reg.append(&id, 0, &bytes).unwrap();
        let att = reg.commit(tmp.path(), "t1", &id).unwrap();

        assert_eq!(reg.resolve_for(&id, "t1").as_ref(), Some(&att));
        assert!(
            reg.resolve_for(&id, "t2").is_none(),
            "another thread must not resolve this upload"
        );
        assert!(
            reg.resolve(&id).is_some(),
            "resolve (ownership-blind) is only for tests"
        );
    }

    /// `drop_thread` 摘掉该 thread 的全部会话（未 commit 的连 staging 一起删），
    /// 且**不动**已落盘的附件；别条 thread 的会话原封不动。
    #[test]
    fn drop_thread_prunes_only_that_threads_sessions_and_their_staging() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut reg = UploadRegistry::new();
        // t1：一条收了一半的 + 一条已 commit 的。
        let half = reg.begin(tmp.path(), "t1", "half.png", 6).unwrap();
        reg.append(&half, 0, b"abc").unwrap();
        let bytes = png_bytes(8, 8);
        let done = reg
            .begin(tmp.path(), "t1", "shot.png", bytes.len() as u64)
            .unwrap();
        reg.append(&done, 0, &bytes).unwrap();
        let att = reg.commit(tmp.path(), "t1", &done).unwrap();
        // 另一条 thread 的会话必须在 prune 后存活。
        let other = reg.begin(tmp.path(), "t2", "y.png", 4).unwrap();

        reg.drop_thread("t1");

        assert!(
            reg.resolve(&half).is_none(),
            "receiving record must be gone"
        );
        assert!(
            reg.resolve(&done).is_none(),
            "committed record must be gone"
        );
        assert!(
            !staging_of(tmp.path(), "t1", &half).exists(),
            "the uncommitted session's staging file must be removed"
        );
        assert!(
            tmp.path().join(&att.path).is_file(),
            "the already-stored attachment must survive (same rule as abort)"
        );
        assert_eq!(reg.len(), 1, "only t2's session may remain");
        reg.append(&other, 0, b"abcd").unwrap();
    }

    #[test]
    fn sweep_expired_removes_only_the_expired_sessions() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut reg = UploadRegistry::new();
        let old = reg.begin(tmp.path(), "t1", "old.png", 4).unwrap();
        let fresh = reg.begin(tmp.path(), "t1", "fresh.png", 4).unwrap();
        reg.backdate(&old, UPLOAD_TTL + Duration::from_secs(1));
        reg.sweep_expired();
        assert!(reg.resolve(&old).is_none());
        assert!(
            reg.append(&old, 0, b"x").is_err(),
            "expired record must be gone"
        );
        assert!(!staging_of(tmp.path(), "t1", &old).exists());
        assert_eq!(reg.len(), 1, "a fresh session must survive");
        reg.append(&fresh, 0, b"abcd").unwrap();
    }

    /// 一条已 commit 的会话到期后也被收走（记录与 staging 都不必再留），但**附件
    /// 落盘文件仍在**：它已经是该 thread 的附件。
    #[test]
    fn sweep_keeps_the_stored_attachment_after_the_record_expires() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut reg = UploadRegistry::new();
        let bytes = png_bytes(8, 8);
        let id = reg
            .begin(tmp.path(), "t1", "shot.png", bytes.len() as u64)
            .unwrap();
        reg.append(&id, 0, &bytes).unwrap();
        let att = reg.commit(tmp.path(), "t1", &id).unwrap();
        reg.backdate(&id, UPLOAD_TTL + Duration::from_secs(1));
        reg.sweep_expired();
        assert!(reg.resolve(&id).is_none());
        assert!(
            tmp.path().join(&att.path).is_file(),
            "stored file must survive"
        );
    }
}
