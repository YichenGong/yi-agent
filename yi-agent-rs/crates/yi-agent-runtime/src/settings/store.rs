//! 受管写入：所有 `settings.json` 的写入走同一把锁与同一个提交点。
//!
//! 本模块是唯一的原子写实现。`settings::write` / `settings::write_to` 只是它的
//! 兼容外壳；迁移、RPC、web 表单都经同一入口，因此「读-改-写」的读发生在本
//! 模块的实例写锁之内，而不是调用方在锁外先读一份快照再覆盖回来。
//!
//! 提交语义（与 spec §8「写入」一致）：
//! 1. 先校验 patch 的形状与每个提供的值（未知键、类型、范围、枚举），失败零写入。
//! 2. 取同路径进程内 writer 锁，再取相邻 `.settings.json.lock` 的 `flock`。
//!    两次等待共用一个单调时钟 5 秒 deadline。
//! 3. 持锁后重读原文（至多 1 MiB + 1 字节）；缺文件按空对象，坏 JSON / 不可读 /
//!    超限拒绝覆盖。
//! 4. 应用 patch，对**完整候选**做严格文档校验。既有非法字段若未在本次 patch 中
//!    全部修好，整批拒绝并列出键名（不回显取值）——这样修复 patch 能生效，而未
//!    修复的坏候选绝不会被保存成成功。
//! 5. 有界序列化 → 唯一临时文件排他创建 → 0600 / flush / sync → `rename`。
//!    `rename` 是唯一提交点；它之后的诊断不能把已提交伪装成失败。

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;

use super::document::{ApplyPolicy, apply_policy, validate_document};
use super::{PREVENT_IDLE_SLEEP_KEY, Settings, SettingsError, apply_op, find_spec, validate_value};

/// 完整 `settings.json` 的字节上限（与读取侧同一口径）。
pub const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;

/// 等锁的单调时钟上限。进程内锁与 `flock` 共用这一个 deadline。
pub const LOCK_WAIT_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

/// 等锁重试的间隔。锁一释放下一次重试就能拿到，不需要更密的轮询。
const LOCK_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(2);

/// patch 的合并方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchMode {
    /// patch 里出现的键覆盖文件中的值，其余键原样保留。
    ReplaceProvided,
    /// 只写「持锁重读后仍未被有效设置」的键。已存在（哪怕非法）的值一律不动，
    /// 迁移不得把用户并发保存的值当成缺失覆盖掉。
    FillMissing,
}

/// 一次提交的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitReceipt {
    /// 规范字段的内容标识（[`validate_document`] 的 revision）。
    pub revision: String,
    /// 逻辑值发生变化的已知字段，按 `FIELDS` 顺序。
    ///
    /// 别名规范化、空壳分组清理都不构成逻辑变化，故可能是空表。
    pub changed_keys: Vec<String>,
    /// 只描述 `changed_keys` 的采用策略。
    pub apply_policy: BTreeMap<String, ApplyPolicy>,
    /// 提交后生效的类型化设置。
    pub settings: Settings,
}

/// 提交一次 patch（公共入口）。
///
/// `path` 应是实例目录下的稳定路径；同一路径的所有写入共享进程内锁与相邻的
/// 锁文件。返回的 receipt 描述本次提交的逻辑变化。
///
/// 锁顺序是实例写锁 → 同路径 [`crate::live_settings`] 的 refresh gate。存在
/// 已登记实例时必须先取它的 gate，否则一个正在 gate 内读旧文件的读者会把
/// 过期内容发布回缓存；不存在实例时没有读者，也就没有 gate 可取。
pub fn commit_patch(
    path: &Path,
    patch: &Value,
    mode: PatchMode,
) -> Result<CommitReceipt, SettingsError> {
    let writer = lock_for_commit(path, patch)?;
    let receipt = match crate::live_settings::with_source_gate(path, |()| {
        commit_patch_locked(path, patch, mode)
    }) {
        Some(outcome) => outcome?,
        None => commit_patch_locked(path, patch, mode)?,
    };
    drop(writer);
    // rename 之后唤醒已存在的同路径源；提交结果已经确定，唤醒失败不影响它。
    crate::live_settings::notify_committed(path);
    Ok(receipt)
}

/// 提交前的固定顺序：先校验 patch（零写入可拒绝），再建目录，最后取写锁。
///
/// 目录必须在取锁**之前**建好：`flock` 文件就住在同一目录里，目录不存在时
/// 连锁都打不开。校验仍排在目录创建之前，坏 patch 不会留下任何副作用。
pub(crate) fn lock_for_commit(path: &Path, patch: &Value) -> Result<WriterGuard, SettingsError> {
    validate_patch(patch)?;
    ensure_parent_directory(path)?;
    acquire_writer_lock(path)
}

/// 校验 patch 的形状与每个提供的值；任何一项失败都零写入。
pub(crate) fn validate_patch(patch: &Value) -> Result<(), SettingsError> {
    let object = patch
        .as_object()
        .ok_or_else(|| SettingsError::InvalidSetting {
            key: "<root>".to_string(),
            reason: "patch must be a JSON object".to_string(),
        })?;
    for (key, value) in object {
        let spec = find_spec(key).ok_or_else(|| SettingsError::InvalidSetting {
            key: key.clone(),
            reason: "unknown setting".to_string(),
        })?;
        validate_value(spec, value)?;
    }
    Ok(())
}

/// 已持实例写锁（与可选 gate）的内部提交。
///
/// [`commit_patch`] 与 [`crate::live_settings::LiveSettings::commit`] 共用本函数：
/// 整个 crate 只有这一份原子写实现，不得再写第二个。
pub(crate) fn commit_patch_locked(
    path: &Path,
    patch: &Value,
    mode: PatchMode,
) -> Result<CommitReceipt, SettingsError> {
    // 调用方必须已持锁（见 [`lock_for_commit`]）；这里只做合并与提交。
    let patch = patch
        .as_object()
        .ok_or_else(|| SettingsError::InvalidSetting {
            key: "<root>".to_string(),
            reason: "patch must be a JSON object".to_string(),
        })?;

    ensure_parent_directory(path)?;

    let old = read_bounded_document(path)?.unwrap_or_else(empty_document);
    let mut candidate = old.clone();

    for (key, value) in patch {
        if mode == PatchMode::FillMissing && canonical_key_is_present(&old, key) {
            // 用户并发保存的值（或已存在的非法值）优先；迁移不得覆盖。
            continue;
        }
        apply_known_patch(&mut candidate, key, value)?;
    }

    // 完整候选的严格校验：修复 patch 能通过，未修复的坏字段整批拒绝。
    let candidate = validate_document(candidate)?;
    let changed_keys = changed_keys(&old, &candidate.raw);
    let receipt_apply_policy = changed_keys
        .iter()
        .filter_map(|key| apply_policy(key).map(|policy| (key.clone(), policy)))
        .collect();

    write_prepared_temp_then_rename(path, &candidate.raw)?;

    Ok(CommitReceipt {
        revision: candidate.revision,
        changed_keys,
        apply_policy: receipt_apply_policy,
        settings: candidate.settings,
    })
}

/// 规范字段（含有效旧别名）在原始文档里是否存在。
///
/// **存在但非法**也返回 `true`：迁移不能把用户写坏的值当成缺失静默覆盖。
fn canonical_key_is_present(raw: &Value, key: &str) -> bool {
    if raw_field(raw, key).is_some() {
        return true;
    }
    known_aliases().iter().any(|(canonical, group, field)| {
        *canonical == key && raw.get(*group).and_then(|g| g.get(*field)).is_some()
    })
}

/// 把 patch 的一个键应用到候选原文；删除 `null` / 空串以及已知旧别名。
///
/// 未知键原样保留（它们不在 patch 的键集合里）。删除别名是必须的：否则清空
/// `power.prevent_idle_sleep` 后，`agent.prevent_idle_sleep` 会在下次加载时被
/// 搬回新键，旧值「复活」。
fn apply_known_patch(raw: &mut Value, key: &str, value: &Value) -> Result<(), SettingsError> {
    let spec = find_spec(key).ok_or_else(|| SettingsError::InvalidSetting {
        key: key.to_string(),
        reason: "unknown setting".to_string(),
    })?;
    let normalized = validate_value(spec, value)?;
    let object = raw
        .as_object_mut()
        .ok_or_else(|| SettingsError::Corrupt("settings.json root is not an object".to_string()))?;
    apply_op(object, key, normalized);

    for (canonical, group, field) in known_aliases() {
        if *canonical != key {
            continue;
        }
        if let Some(inner) = object
            .get_mut(*group)
            .and_then(|value| value.as_object_mut())
        {
            inner.remove(*field);
            if inner.is_empty() {
                object.remove(*group);
            }
        }
    }
    Ok(())
}

/// 旧键 → 规范键。字面量，不引用常量：常量指向新键，引用它会让迁移随常量漂移。
fn known_aliases() -> &'static [(&'static str, &'static str, &'static str)] {
    &[(PREVENT_IDLE_SLEEP_KEY, "agent", "prevent_idle_sleep")]
}

/// 一个已知字段在原始文档中的逻辑值，用于计算 `changed_keys`。
#[derive(Debug, Clone, PartialEq, Eq)]
enum LogicalValue {
    /// 未设置（键缺失、`null`、空白串）。
    Unset,
    /// 存在但无法被接受；修复它算逻辑变化。
    Invalid,
    /// 规范化后的有效值。
    Set(Value),
}

/// 原始文档里字段的逻辑值；旧别名在规范键缺失时参与判断，与加载侧同向。
fn logical_field(raw: &Value, key: &str) -> LogicalValue {
    let Some(spec) = find_spec(key) else {
        return LogicalValue::Unset;
    };
    let mut value = raw_field(raw, key);
    if value.is_none() {
        for (canonical, group, field) in known_aliases() {
            if *canonical == key {
                value = raw.get(*group).and_then(|g| g.get(*field));
            }
        }
    }
    let Some(value) = value else {
        return LogicalValue::Unset;
    };
    match validate_value(spec, value) {
        Ok(Some(normalized)) => LogicalValue::Set(normalized),
        Ok(None) => LogicalValue::Unset,
        Err(_) => LogicalValue::Invalid,
    }
}

/// 逻辑值变化的已知字段，按 `FIELDS` 顺序。别名规范化不算逻辑变化。
fn changed_keys(old: &Value, new: &Value) -> Vec<String> {
    super::FIELDS
        .iter()
        .filter(|field| logical_field(old, field.key) != logical_field(new, field.key))
        .map(|field| field.key.to_string())
        .collect()
}

/// `group.field` 的原始值；分组缺失或不是对象都表示未设置。
fn raw_field<'a>(raw: &'a Value, key: &str) -> Option<&'a Value> {
    let (group, field) = key.split_once('.')?;
    raw.get(group)?.get(field)
}

fn empty_document() -> Value {
    Value::Object(serde_json::Map::new())
}

/// 新父目录 0700；**已存在的目录不 chmod**（那是用户已有的安全决策）。
fn ensure_parent_directory(path: &Path) -> Result<(), SettingsError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    if parent.as_os_str().is_empty() || parent.exists() {
        return Ok(());
    }
    let io_error = |error: std::io::Error| SettingsError::Io(error.to_string());
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(io_error)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(parent).map_err(io_error)
    }
}

/// 进程内 writer 锁的登记表：同一规范化绝对路径共用同一把锁。
///
/// 条目按实例路径登记且永不删除——注册表本身就是所有权持有者，故可以安全地
/// 交出 `&'static` 引用，让 `MutexGuard<'static, ()>` 能独立存在。
fn writer_registry() -> &'static Mutex<std::collections::HashMap<PathBuf, &'static Mutex<()>>> {
    static REGISTRY: std::sync::OnceLock<
        Mutex<std::collections::HashMap<PathBuf, &'static Mutex<()>>>,
    > = std::sync::OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// 规范化绝对路径：同一文件的不同写法必须映射到同一把锁、同一个实例。
pub(crate) fn lock_key(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// 本进程内该路径的 writer 锁。
fn writer_mutex(path: &Path) -> &'static Mutex<()> {
    let mut registry = writer_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry
        .entry(lock_key(path))
        .or_insert_with(|| Box::leak(Box::new(Mutex::new(()))))
}

/// 写锁守卫：进程内 mutex + 文件锁 fd，析构即释放（锁文件不 unlink）。
pub(crate) struct WriterGuard {
    _process_guard: std::sync::MutexGuard<'static, ()>,
    /// 持有文件锁的 fd；drop 即关闭，`flock` 随之释放。
    _file: std::fs::File,
}

/// 先取进程内 writer 锁，再取 `flock`；两次等待共用一个 deadline。
pub(crate) fn acquire_writer_lock(path: &Path) -> Result<WriterGuard, SettingsError> {
    let deadline = std::time::Instant::now() + lock_wait_limit();
    let process = writer_mutex(path);

    let process_guard = loop {
        match process.try_lock() {
            Ok(guard) => break guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => break poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                if std::time::Instant::now() >= deadline {
                    record_timeout_stage("process");
                    return Err(SettingsError::LockTimeout);
                }
                std::thread::sleep(LOCK_RETRY_INTERVAL);
            }
        }
    };

    let lock_path = lock_file_path(path);
    let file = open_lock_file(&lock_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let fd = file.as_raw_fd();
        loop {
            // SAFETY: `fd` 由 `file` 持有，调用期间不会关闭。
            let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
            if rc == 0 {
                break;
            }
            let error = std::io::Error::last_os_error();
            match error.raw_os_error() {
                // EWOULDBLOCK 与 EAGAIN 在 Linux/macOS 上同值，但分别列出以免依赖。
                Some(code)
                    if code == libc::EWOULDBLOCK || code == libc::EAGAIN || code == libc::EINTR =>
                {
                    if code == libc::EINTR {
                        continue;
                    }
                    if std::time::Instant::now() >= deadline {
                        record_timeout_stage("flock");
                        return Err(SettingsError::LockTimeout);
                    }
                    std::thread::sleep(LOCK_RETRY_INTERVAL);
                }
                _ => return Err(SettingsError::Io(error.to_string())),
            }
        }
    }

    Ok(WriterGuard {
        _process_guard: process_guard,
        _file: file,
    })
}

/// 相邻的锁文件路径。文件名固定，保证 inode 稳定，永不 unlink/替换。
fn lock_file_path(path: &Path) -> PathBuf {
    path.with_file_name(".settings.json.lock")
}

/// 锁文件 0600 创建（已存在则只打开，不改权限、不截断）。
fn open_lock_file(lock_path: &Path) -> Result<std::fs::File, SettingsError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(lock_path)
        .map_err(|error| SettingsError::Io(error.to_string()))
}

/// 持锁后重读原文：至多读 `MAX_DOCUMENT_BYTES + 1` 字节。
///
/// 返回 `None` 表示文件不存在（按空对象处理）。坏 JSON / 根不是对象 / 超限都
/// 拒绝，绝不覆盖。
fn read_bounded_document(path: &Path) -> Result<Option<Value>, SettingsError> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(SettingsError::Io(error.to_string())),
    };
    let mut bytes = Vec::new();
    file.take(MAX_DOCUMENT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| SettingsError::Io(error.to_string()))?;
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(SettingsError::TooLarge);
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
        SettingsError::Corrupt(format!("settings.json is not valid JSON: {error}"))
    })?;
    if !value.is_object() {
        return Err(SettingsError::Corrupt(
            "settings.json is not a JSON object".to_string(),
        ));
    }
    Ok(Some(value))
}

/// 有上限的写入目标：超过 1 MiB 立即失败，不把整份超大 JSON 先分配出来。
struct BoundedBuffer {
    bytes: Vec<u8>,
}

impl std::io::Write for BoundedBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.bytes.len() + buf.len() > MAX_DOCUMENT_BYTES {
            return Err(std::io::Error::other(
                "settings.json exceeds the 1 MiB limit",
            ));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// 有界序列化：超过上限的候选在写出过程中就被拒绝。
fn serialize_bounded(raw: &Value) -> Result<Vec<u8>, SettingsError> {
    let mut buffer = BoundedBuffer { bytes: Vec::new() };
    serde_json::to_writer_pretty(&mut buffer, raw).map_err(|error| {
        // serde_json 把底层 `Write` 的 io 错误原样上抛；用字符串前缀区分
        // 「超限」与「真的序列化失败」，避免把超限报成内部错误。
        if error.to_string().contains("1 MiB limit") {
            SettingsError::TooLarge
        } else {
            SettingsError::Io(error.to_string())
        }
    })?;
    Ok(buffer.bytes)
}

/// 有界序列化 + 唯一临时文件 + 0600/flush/sync + `rename`。
fn write_prepared_temp_then_rename(path: &Path, raw: &Value) -> Result<(), SettingsError> {
    let bytes = serialize_bounded(raw)?;
    if bytes.len() > MAX_DOCUMENT_BYTES {
        return Err(SettingsError::TooLarge);
    }

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("settings.json");
    let (tmp_path, mut file) = create_exclusive_temp(parent, file_name)?;

    // 前置故障只删本次临时文件，原文件逐字节不变。
    let abort = |file: std::fs::File, error: SettingsError| {
        drop(file);
        let _ = std::fs::remove_file(&tmp_path);
        Err(error)
    };

    if fault_reached("BeforeWrite") {
        return abort(
            file,
            SettingsError::Io("injected fault: before write".into()),
        );
    }
    if let Err(error) = file.write_all(&bytes) {
        return abort(file, SettingsError::Io(error.to_string()));
    }
    if let Err(error) = file.flush() {
        return abort(file, SettingsError::Io(error.to_string()));
    }
    if fault_reached("BeforePermissions") {
        return abort(
            file,
            SettingsError::Io("injected fault: before permissions".into()),
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = file.set_permissions(std::fs::Permissions::from_mode(0o600)) {
            return abort(file, SettingsError::Io(error.to_string()));
        }
    }
    if fault_reached("BeforeSync") {
        return abort(
            file,
            SettingsError::Io("injected fault: before sync".into()),
        );
    }
    if let Err(error) = file.sync_all() {
        return abort(file, SettingsError::Io(error.to_string()));
    }
    if fault_reached("BeforeRename") {
        return abort(
            file,
            SettingsError::Io("injected fault: before rename".into()),
        );
    }
    drop(file);

    std::fs::rename(&tmp_path, path).map_err(|error| {
        let _ = std::fs::remove_file(&tmp_path);
        SettingsError::Io(error.to_string())
    })?;

    // 提交点已过。这里的任何故障都只能是诊断：文件已是真相，不能报成零提交。
    if fault_reached("AfterRename") {
        tracing::warn!(
            path = %path.display(),
            "settings.json was committed but a post-commit step failed; the file is authoritative"
        );
    }
    Ok(())
}

/// 排他创建本次唯一的临时文件；0600 由创建模式给定，umask 只会更严。
fn create_exclusive_temp(
    parent: &Path,
    file_name: &str,
) -> Result<(PathBuf, std::fs::File), SettingsError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    for _ in 0..16 {
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".{file_name}.{}.{seq}.tmp", std::process::id()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(SettingsError::Io(error.to_string())),
        }
    }
    Err(SettingsError::Io(
        "could not create a unique temporary settings file".to_string(),
    ))
}

/// 等锁上限；测试可缩短，避免为「共用 deadline」断言真的等 5 秒。
fn lock_wait_limit() -> std::time::Duration {
    #[cfg(test)]
    {
        if let Some(limit) = lock_limit_override() {
            return limit;
        }
    }
    LOCK_WAIT_LIMIT
}

// 测试可覆盖等锁上限（线程局部，避免并行测试互相影响）。
#[cfg(test)]
thread_local! {
    static LOCK_LIMIT_OVERRIDE: std::cell::Cell<Option<std::time::Duration>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn lock_limit_override() -> Option<std::time::Duration> {
    LOCK_LIMIT_OVERRIDE.with(std::cell::Cell::get)
}

#[cfg(test)]
fn with_lock_limit<T>(limit: std::time::Duration, body: impl FnOnce() -> T) -> T {
    struct Restore(Option<std::time::Duration>);
    impl Drop for Restore {
        fn drop(&mut self) {
            LOCK_LIMIT_OVERRIDE.with(|slot| slot.set(self.0));
        }
    }
    let restore = Restore(lock_limit_override());
    LOCK_LIMIT_OVERRIDE.with(|slot| slot.set(Some(limit)));
    let outcome = body();
    drop(restore);
    outcome
}

// 测试可观察「超时发生在哪个阶段」，证明两次等待共用同一份预算。
#[cfg(test)]
thread_local! {
    static TIMEOUT_STAGE: std::cell::Cell<Option<&'static str>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn record_timeout_stage(stage: &'static str) {
    TIMEOUT_STAGE.with(|slot| slot.set(Some(stage)));
}

#[cfg(test)]
fn timed_out_stage() -> Option<&'static str> {
    TIMEOUT_STAGE.with(std::cell::Cell::get)
}

#[cfg(not(test))]
#[inline(always)]
fn record_timeout_stage(_stage: &'static str) {}

/// 测试注入的提交故障点。仅 `cfg(test)` 存在，生产代码里是编译期常量 `false`。
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitFault {
    BeforeWrite,
    BeforePermissions,
    BeforeSync,
    BeforeRename,
    AfterRename,
}

#[cfg(test)]
impl CommitFault {
    fn stage(self) -> &'static str {
        match self {
            CommitFault::BeforeWrite => "BeforeWrite",
            CommitFault::BeforePermissions => "BeforePermissions",
            CommitFault::BeforeSync => "BeforeSync",
            CommitFault::BeforeRename => "BeforeRename",
            CommitFault::AfterRename => "AfterRename",
        }
    }
}

#[cfg(test)]
thread_local! {
    static INJECTED_FAULT: std::cell::Cell<Option<&'static str>> =
        const { std::cell::Cell::new(None) };
}

/// 注入（或清除）本线程的下一个提交故障，返回之前的注入值。
#[cfg(test)]
pub(crate) fn inject_commit_fault(fault: Option<CommitFault>) -> Option<CommitFault> {
    INJECTED_FAULT.with(|slot| {
        let previous = slot.get();
        slot.set(fault.map(CommitFault::stage));
        previous.and_then(|stage| {
            [
                CommitFault::BeforeWrite,
                CommitFault::BeforePermissions,
                CommitFault::BeforeSync,
                CommitFault::BeforeRename,
                CommitFault::AfterRename,
            ]
            .into_iter()
            .find(|candidate| candidate.stage() == stage)
        })
    })
}

#[cfg(test)]
fn fault_reached(stage: &'static str) -> bool {
    INJECTED_FAULT.with(|slot| {
        if slot.get() == Some(stage) {
            slot.set(None);
            true
        } else {
            false
        }
    })
}

#[cfg(not(test))]
#[inline(always)]
fn fault_reached(_stage: &'static str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::{
        CommitFault, MAX_DOCUMENT_BYTES, PatchMode, commit_patch, inject_commit_fault,
        lock_file_path, timed_out_stage,
    };
    use crate::settings::{SettingsError, load_from};

    fn temp_settings() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        (dir, path)
    }

    #[test]
    fn clearing_new_power_key_cannot_resurrect_legacy_alias() {
        let (_dir, path) = temp_settings();
        std::fs::write(
            &path,
            r#"{"agent":{"prevent_idle_sleep":"false"},"power":{"prevent_idle_sleep":"true"},"future":7}"#,
        )
        .unwrap();
        commit_patch(
            &path,
            &serde_json::json!({"power.prevent_idle_sleep": null}),
            PatchMode::ReplaceProvided,
        )
        .unwrap();
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(raw["power"].get("prevent_idle_sleep").is_none());
        assert!(
            raw["agent"].get("prevent_idle_sleep").is_none(),
            "the legacy alias must not resurrect the cleared value: {raw}"
        );
        assert_eq!(raw["future"], 7);
    }

    #[test]
    fn fill_missing_rechecks_latest_file_under_write_lock() {
        let (_dir, path) = temp_settings();
        let migration_patch = serde_json::json!({"agent.max_turns": 50});
        commit_patch(
            &path,
            &serde_json::json!({"agent.max_turns": 77}),
            PatchMode::ReplaceProvided,
        )
        .unwrap();
        let receipt = commit_patch(&path, &migration_patch, PatchMode::FillMissing).unwrap();
        assert!(receipt.changed_keys.is_empty());
        assert_eq!(load_from(&path).agent.max_turns, Some(77));
    }

    /// 首次保存必须自己建出 0700 的目录和 0600 的锁文件。
    #[cfg(unix)]
    #[test]
    fn first_save_creates_private_parent_and_lock() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instance/nested/settings.json");

        commit_patch(
            &path,
            &serde_json::json!({"agent.max_turns": 12}),
            PatchMode::ReplaceProvided,
        )
        .unwrap();

        for parent in [
            path.parent().unwrap(),
            path.parent().unwrap().parent().unwrap(),
        ] {
            let mode = std::fs::metadata(parent).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "created dirs must be 0700: {parent:?}");
        }
        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(file_mode & 0o777, 0o600);
        let lock = lock_file_path(&path);
        let lock_mode = std::fs::metadata(&lock).unwrap().permissions().mode();
        assert_eq!(lock_mode & 0o777, 0o600, "the lock file must be 0600");
        assert!(lock.exists(), "the lock file is never unlinked");
    }

    /// 序列化后的完整候选超限必须整批拒绝，且不留下临时文件。
    #[test]
    fn candidate_over_limit_is_not_committed() {
        let (dir, path) = temp_settings();
        let oversized = "x".repeat(MAX_DOCUMENT_BYTES + 32);
        let error = commit_patch(
            &path,
            &serde_json::json!({"agent.system_prompt": oversized}),
            PatchMode::ReplaceProvided,
        )
        .unwrap_err();
        assert!(matches!(error, SettingsError::TooLarge), "got {error:?}");
        assert!(!path.exists(), "an oversized candidate must not land");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files leaked: {leftovers:?}");
    }

    /// 修复 patch 必须修好**全部**未修复的非法已知字段，否则整批拒绝。
    #[test]
    fn repair_patch_must_repair_all_invalid_known_fields() {
        let (_dir, path) = temp_settings();
        let original = r#"{"compact":{"ratio":0},"power":{"idle_grace_minutes":500}}"#;
        std::fs::write(&path, original).unwrap();

        let error = commit_patch(
            &path,
            &serde_json::json!({"agent.max_turns": 10}),
            PatchMode::ReplaceProvided,
        )
        .unwrap_err();
        match &error {
            SettingsError::InvalidDocument { keys } => {
                assert!(keys.iter().any(|key| key == "compact.ratio"), "{keys:?}");
                assert!(
                    keys.iter().any(|key| key == "power.idle_grace_minutes"),
                    "{keys:?}"
                );
            }
            other => panic!("expected InvalidDocument, got {other:?}"),
        }
        assert_eq!(
            std::fs::read(&path).unwrap(),
            original.as_bytes(),
            "a rejected repair must leave the file byte-identical"
        );
        assert!(
            !error.to_string().contains("500"),
            "the error must not echo configured values: {error}"
        );

        let receipt = commit_patch(
            &path,
            &serde_json::json!({"compact.ratio": 70, "power.idle_grace_minutes": 60}),
            PatchMode::ReplaceProvided,
        )
        .unwrap();
        // `changed_keys` 按 `FIELDS` 顺序排列（power 组在 compact 组之后定义）。
        assert_eq!(
            receipt.changed_keys,
            vec![
                "power.idle_grace_minutes".to_string(),
                "compact.ratio".to_string()
            ]
        );
        let settings = load_from(&path);
        assert_eq!(settings.compact.ratio, Some(70));
        assert_eq!(settings.power.idle_grace_minutes, Some(60));
    }

    /// 文档级错误只带键名，不回显用户填的取值（可能含密钥）。
    #[test]
    fn invalid_document_error_does_not_leak_values() {
        let (_dir, path) = temp_settings();
        std::fs::write(&path, r#"{"agent":{"max_turns":"TOPSECRET-TURNS"}}"#).unwrap();
        let error = commit_patch(
            &path,
            &serde_json::json!({"compact.ratio": 50}),
            PatchMode::ReplaceProvided,
        )
        .unwrap_err();
        assert!(matches!(error, SettingsError::InvalidDocument { .. }));
        assert!(error.to_string().contains("agent.max_turns"), "got {error}");
        assert!(
            !error.to_string().contains("TOPSECRET"),
            "values must not leak: {error}"
        );
    }

    /// 本任务先用两个独立 fd 验证锁超时/释放，不靠 sleep 安排竞态。
    #[cfg(unix)]
    #[test]
    fn a_held_flock_times_out_and_is_usable_again_after_release() {
        let (_dir, path) = temp_settings();
        commit_patch(
            &path,
            &serde_json::json!({"agent.max_turns": 1}),
            PatchMode::ReplaceProvided,
        )
        .unwrap();

        let lock = lock_file_path(&path);
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock)
            .unwrap();
        {
            use std::os::unix::io::AsRawFd;
            let rc = unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            assert_eq!(rc, 0, "the test must own the lock: {rc}");
        }

        super::with_lock_limit(Duration::from_millis(150), || {
            let started = Instant::now();
            let error = commit_patch(
                &path,
                &serde_json::json!({"agent.max_turns": 2}),
                PatchMode::ReplaceProvided,
            )
            .unwrap_err();
            assert!(matches!(error, SettingsError::LockTimeout), "got {error:?}");
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "the configured deadline must bound the wait"
            );
        });

        drop(held);
        let receipt = commit_patch(
            &path,
            &serde_json::json!({"agent.max_turns": 2}),
            PatchMode::ReplaceProvided,
        )
        .unwrap();
        assert_eq!(receipt.changed_keys, vec!["agent.max_turns".to_string()]);
        assert_eq!(load_from(&path).agent.max_turns, Some(2));
    }

    /// 进程内等待消耗掉的预算不能再补给 `flock`：两次等待共用一个 deadline。
    ///
    /// 抓的破坏：每阶段各拿一份完整上限，锁竞争的实际等待变成两倍。
    #[cfg(unix)]
    #[test]
    fn writer_mutex_and_flock_share_one_deadline() {
        let (_dir, path) = temp_settings();
        commit_patch(
            &path,
            &serde_json::json!({"agent.max_turns": 1}),
            PatchMode::ReplaceProvided,
        )
        .unwrap();

        // flock 由测试自己的 fd 全程持有：commit 一旦走到文件锁阶段就必超时。
        let lock = lock_file_path(&path);
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock)
            .unwrap();
        {
            use std::os::unix::io::AsRawFd;
            let rc = unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            assert_eq!(rc, 0, "the test must own the lock: {rc}");
        }

        // 先让另一个线程占用进程内 writer 锁 100ms，消耗掉共享预算的一部分。
        // 若实现每阶段各拿一份 300ms，总等待会 ≥ 400ms；共用 deadline 约 300ms。
        let worker_lock = super::writer_mutex(&path);
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _guard = worker_lock.lock().unwrap();
            acquired_tx.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(100));
        });
        acquired_rx.recv().unwrap();

        super::TIMEOUT_STAGE.with(|slot| slot.set(None));
        let (error, elapsed) = super::with_lock_limit(Duration::from_millis(300), || {
            let started = Instant::now();
            let error = commit_patch(
                &path,
                &serde_json::json!({"agent.max_turns": 2}),
                PatchMode::ReplaceProvided,
            )
            .unwrap_err();
            (error, started.elapsed())
        });
        holder.join().unwrap();

        assert!(matches!(error, SettingsError::LockTimeout), "got {error:?}");
        assert_eq!(
            timed_out_stage(),
            Some("flock"),
            "the process wait must be consumed before the file-lock wait"
        );
        assert!(
            elapsed >= Duration::from_millis(280),
            "the shared deadline must actually be waited out (waited {elapsed:?})"
        );
        assert!(
            elapsed < Duration::from_millis(380),
            "a per-stage deadline would wait ~400ms (waited {elapsed:?})"
        );
        assert_eq!(
            load_from(&path).agent.max_turns,
            Some(1),
            "a timed-out commit must not land"
        );
        drop(held);
    }

    /// 四个提交前故障点：原文件逐字节不变，临时文件回收。
    #[test]
    fn a_pre_commit_fault_changes_nothing() {
        let (dir, path) = temp_settings();
        commit_patch(
            &path,
            &serde_json::json!({"agent.max_turns": 5}),
            PatchMode::ReplaceProvided,
        )
        .unwrap();
        let before = std::fs::read(&path).unwrap();

        for fault in [
            CommitFault::BeforeWrite,
            CommitFault::BeforePermissions,
            CommitFault::BeforeSync,
            CommitFault::BeforeRename,
        ] {
            inject_commit_fault(Some(fault));
            let error = commit_patch(
                &path,
                &serde_json::json!({"agent.max_turns": 9}),
                PatchMode::ReplaceProvided,
            )
            .unwrap_err();
            inject_commit_fault(None);
            assert!(
                matches!(error, SettingsError::Io(_)),
                "{fault:?} must be a controlled io error, got {error:?}"
            );
            assert_eq!(
                std::fs::read(&path).unwrap(),
                before,
                "{fault:?} must leave the file byte-identical"
            );
            let leftovers: Vec<_> = std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().to_string())
                .filter(|name| name.ends_with(".tmp"))
                .collect();
            assert!(leftovers.is_empty(), "{fault:?} leaked {leftovers:?}");
        }
    }

    /// rename 之后的故障不能伪装成「未提交」：文件已是新内容，提交仍算成功。
    ///
    /// 抓的破坏：把提交后的诊断错误一路返回成 `Err`，调用方据此认为零提交，
    /// 于是重试或回滚，而磁盘其实已经变了。
    #[test]
    fn a_post_commit_fault_does_not_masquerade_as_zero_commit() {
        let (_dir, path) = temp_settings();
        commit_patch(
            &path,
            &serde_json::json!({"agent.max_turns": 5}),
            PatchMode::ReplaceProvided,
        )
        .unwrap();

        inject_commit_fault(Some(CommitFault::AfterRename));
        let receipt = commit_patch(
            &path,
            &serde_json::json!({"agent.max_turns": 9}),
            PatchMode::ReplaceProvided,
        )
        .expect("a post-commit diagnostic must not turn into a failed commit");
        inject_commit_fault(None);

        assert_eq!(receipt.changed_keys, vec!["agent.max_turns".to_string()]);
        assert_eq!(load_from(&path).agent.max_turns, Some(9));
    }

    /// 别名规范化与空壳清理不算逻辑变化；只有真值变了才进 `changed_keys`。
    #[test]
    fn changed_keys_track_logical_values_only() {
        let (_dir, path) = temp_settings();
        std::fs::write(
            &path,
            r#"{"agent":{"prevent_idle_sleep":"true","max_turns":200}}"#,
        )
        .unwrap();

        let receipt = commit_patch(
            &path,
            &serde_json::json!({"power.prevent_idle_sleep": "true", "agent.max_turns": 200}),
            PatchMode::ReplaceProvided,
        )
        .unwrap();
        assert!(
            receipt.changed_keys.is_empty(),
            "normalisation is not a logical change: {:?}",
            receipt.changed_keys
        );

        let receipt = commit_patch(
            &path,
            &serde_json::json!({"agent.max_turns": 201}),
            PatchMode::ReplaceProvided,
        )
        .unwrap();
        assert_eq!(receipt.changed_keys, vec!["agent.max_turns".to_string()]);
        assert_eq!(
            receipt.apply_policy.get("agent.max_turns"),
            Some(&crate::settings::document::ApplyPolicy::NextSafePoint)
        );
        assert_eq!(receipt.settings.agent.max_turns, Some(201));
        assert!(!receipt.revision.is_empty());
    }

    /// 迁移不得把「已存在但非法」的值当成缺失静默覆盖，必须给出键名诊断。
    #[test]
    fn fill_missing_does_not_overwrite_an_existing_invalid_value() {
        let (_dir, path) = temp_settings();
        std::fs::write(&path, r#"{"agent":{"max_turns":"TOPSECRET-BAD"}}"#).unwrap();

        let error = commit_patch(
            &path,
            &serde_json::json!({"agent.max_turns": 50}),
            PatchMode::FillMissing,
        )
        .unwrap_err();
        assert!(
            matches!(&error, SettingsError::InvalidDocument { keys } if keys.iter().any(|k| k == "agent.max_turns")),
            "the invalid existing value must be diagnosed, got {error:?}"
        );
        assert!(!error.to_string().contains("TOPSECRET"), "got {error}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            r#"{"agent":{"max_turns":"TOPSECRET-BAD"}}"#,
            "the file must stay byte-identical"
        );
    }

    /// 坏 JSON / 根不是对象一律拒绝覆盖（持锁重读后仍然如此）。
    #[test]
    fn a_corrupt_document_is_never_overwritten() {
        for garbage in ["{ broken", "[1, 2, 3]"] {
            let (_dir, path) = temp_settings();
            std::fs::write(&path, garbage).unwrap();
            let error = commit_patch(
                &path,
                &serde_json::json!({"agent.max_turns": 5}),
                PatchMode::ReplaceProvided,
            )
            .unwrap_err();
            assert!(matches!(error, SettingsError::Corrupt(_)), "got {error:?}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), garbage);
        }
    }

    /// patch 不是对象、或键不认识，都在取锁前拒绝。
    #[test]
    fn a_bad_patch_shape_is_rejected_before_any_io() {
        let (dir, path) = temp_settings();
        let error = commit_patch(
            &path,
            &serde_json::json!([1, 2]),
            PatchMode::ReplaceProvided,
        )
        .unwrap_err();
        assert!(matches!(error, SettingsError::InvalidSetting { .. }));

        let error = commit_patch(
            &path,
            &serde_json::json!({"agent.not_a_field": 1}),
            PatchMode::ReplaceProvided,
        )
        .unwrap_err();
        assert!(matches!(error, SettingsError::InvalidSetting { .. }));

        assert!(!path.exists());
        assert!(
            !lock_file_path(&path).exists(),
            "no lock file on a rejected patch"
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
