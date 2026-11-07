//! 进程内共享的运行期设置源。
//!
//! 同一绝对路径只对应一个实例（`Weak` 注册表），读取与提交共用同一个
//! refresh gate；写路径共用 [`crate::settings::store`] 的唯一原子提交实现。
//!
//! 三条硬约束（见 spec 与 Task 3 brief）：
//! 1. refresh gate 覆盖 open → 有界读 → 校验 → 发布缓存，不跨长操作持锁。
//! 2. 锁顺序永远是实例写锁 → refresh gate；store 内部不得反过来再请求写锁。
//! 3. 进程内失效序号（[`LiveSettings::subscribe`]）只是唤醒信号，不是持久化
//!    revision，也不用于跨进程排序。

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::settings::document::validate_document;
use crate::settings::store::{CommitReceipt, MAX_DOCUMENT_BYTES, PatchMode, lock_for_commit};
use crate::settings::{Settings, SettingsError};

/// 快照的来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotOrigin {
    /// 本次读取的候选通过严格校验。
    Current,
    /// 候选失败，沿用最后一次成功发布的内容。
    LastKnownGood,
    /// 启动阶段还没有成功快照，坏文件按宽容规则归一化。
    StartupFallback,
}

/// 健康状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    Healthy,
    Degraded,
}

/// 一次快照的健康诊断。只带键名，绝不带取值。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsHealth {
    pub state: HealthState,
    pub code: Option<String>,
    pub invalid_keys: Vec<String>,
}

impl SettingsHealth {
    fn healthy() -> Self {
        Self {
            state: HealthState::Healthy,
            code: None,
            invalid_keys: Vec::new(),
        }
    }

    fn degraded(code: &str, invalid_keys: Vec<String>) -> Self {
        Self {
            state: HealthState::Degraded,
            code: Some(code.to_string()),
            invalid_keys,
        }
    }
}

/// 一份可发布的设置快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsSnapshot {
    pub settings: Settings,
    pub revision: String,
    pub origin: SnapshotOrigin,
    pub health: SettingsHealth,
}

/// 有界读取的结果。缺失是健康默认，不是错误。
enum BoundedSettingsInput {
    Missing,
    Bytes(Vec<u8>),
}

/// 一次失败的候选，连同它已经读进内存的原文（读取/超限错误没有原文）。
struct CandidateFailure {
    error: SettingsError,
    bytes: Option<Vec<u8>>,
    invalid_keys: Vec<String>,
}

impl CandidateFailure {
    fn from_error(error: SettingsError) -> Self {
        let invalid_keys = match &error {
            SettingsError::InvalidDocument { keys } => keys.clone(),
            _ => Vec::new(),
        };
        Self {
            error,
            bytes: None,
            invalid_keys,
        }
    }

    fn code(&self) -> &'static str {
        match self.error {
            SettingsError::TooLarge => "too_large",
            SettingsError::Corrupt(_) => "corrupt",
            SettingsError::InvalidDocument { .. } | SettingsError::InvalidSetting { .. } => {
                "invalid_document"
            }
            SettingsError::Io(_) => "io_error",
            SettingsError::LockTimeout => "lock_timeout",
        }
    }
}

/// refresh gate 内的全部可变状态；只有一个 mutex，天然是「唯一缓存」。
#[derive(Default)]
struct RefreshState {
    /// 最近一次交给调用方的快照（成功或降级）。
    published: Option<Arc<SettingsSnapshot>>,
    /// 最近一次严格校验成功的内容；只有它能当 LKG。
    last_good: Option<Arc<SettingsSnapshot>>,
    /// 成功候选的摘要与结果：相同 bytes 复用校验/健康结果。
    cached_digest: Option<[u8; 32]>,
    cached_result: Option<Arc<SettingsSnapshot>>,
}

impl RefreshState {
    /// 用一次有界读取的结果更新状态，返回要交给调用方的快照。
    ///
    /// 成功候选替换 LKG；失败复用最后成功内容（没有则走启动期宽容归一化）。
    /// 本函数不比较「是否变化」，由 [`RefreshState::changed_since_last_publish`]
    /// 统一判定，保证健康变化也能产生失效序号。
    fn accept_or_degrade(
        &mut self,
        input: Result<BoundedSettingsInput, SettingsError>,
    ) -> Arc<SettingsSnapshot> {
        match input {
            Ok(BoundedSettingsInput::Missing) => {
                self.cached_digest = None;
                self.cached_result = None;
                self.accept_valid(Settings::default(), empty_document_revision())
            }
            Ok(BoundedSettingsInput::Bytes(bytes)) => match self.accept_bytes(&bytes) {
                Ok(snapshot) => snapshot,
                Err(failure) => self.degrade(failure),
            },
            Err(error) => self.degrade(CandidateFailure::from_error(error)),
        }
    }

    /// 严格校验一份原文；同 bytes 直接复用上次结果。
    fn accept_bytes(&mut self, bytes: &[u8]) -> Result<Arc<SettingsSnapshot>, CandidateFailure> {
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        if self.cached_digest == Some(digest)
            && let Some(cached) = &self.cached_result
        {
            return Ok(Arc::clone(cached));
        }

        let value: Value = serde_json::from_slice(bytes).map_err(|error| CandidateFailure {
            error: SettingsError::Corrupt(format!("settings.json is not valid JSON: {error}")),
            bytes: Some(bytes.to_vec()),
            invalid_keys: Vec::new(),
        })?;
        if !value.is_object() {
            return Err(CandidateFailure {
                error: SettingsError::Corrupt("settings.json is not a JSON object".to_string()),
                bytes: Some(bytes.to_vec()),
                invalid_keys: vec!["<root>".to_string()],
            });
        }

        match validate_document(value) {
            Ok(document) => {
                let snapshot = self.accept_valid(document.settings, document.revision);
                self.cached_digest = Some(digest);
                self.cached_result = Some(Arc::clone(&snapshot));
                Ok(snapshot)
            }
            Err(error) => {
                let invalid_keys = match &error {
                    SettingsError::InvalidDocument { keys } => keys.clone(),
                    _ => Vec::new(),
                };
                Err(CandidateFailure {
                    error,
                    bytes: Some(bytes.to_vec()),
                    invalid_keys,
                })
            }
        }
    }

    /// 严格候选成功：发布并要求下一次刷新以它作为 LKG。
    fn accept_valid(&mut self, settings: Settings, revision: String) -> Arc<SettingsSnapshot> {
        let snapshot = Arc::new(SettingsSnapshot {
            settings,
            revision,
            origin: SnapshotOrigin::Current,
            health: SettingsHealth::healthy(),
        });
        self.last_good = Some(Arc::clone(&snapshot));
        snapshot
    }

    /// 候选失败：优先复用 LKG，否则对已读原文做无日志的宽容归一化。
    ///
    /// 启动期回退**不**成为 LKG：重复的坏 bytes 必须继续报 `StartupFallback`，
    /// 直到第一次合法读取建立 LKG。
    fn degrade(&mut self, failure: CandidateFailure) -> Arc<SettingsSnapshot> {
        if let Some(good) = &self.last_good {
            return Arc::new(SettingsSnapshot {
                settings: good.settings.clone(),
                revision: good.revision.clone(),
                origin: SnapshotOrigin::LastKnownGood,
                health: SettingsHealth::degraded(failure.code(), failure.invalid_keys),
            });
        }

        let code = failure.code();
        let fallback = failure.bytes.as_deref().and_then(|bytes| {
            // 只用已经读进内存的 bytes；绝不回头调无界的 `load_from`。
            crate::settings::lenient_startup_fallback(bytes, failure.invalid_keys.clone())
        });
        let (settings, revision, invalid_keys) = match fallback {
            Some(fallback) => {
                let revision = revision_of_settings(&fallback.settings);
                (fallback.settings, revision, fallback.invalid_keys)
            }
            None => (Settings::default(), empty_document_revision(), Vec::new()),
        };

        Arc::new(SettingsSnapshot {
            settings,
            revision,
            origin: SnapshotOrigin::StartupFallback,
            health: SettingsHealth::degraded(code, invalid_keys),
        })
    }

    /// 与上次发布相比是否有任何可见变化；调用后 `next` 成为新的发布内容。
    fn changed_since_last_publish(&mut self, next: &Arc<SettingsSnapshot>) -> bool {
        let changed = match &self.published {
            Some(previous) => changed_since(previous, next),
            None => true,
        };
        self.published = Some(Arc::clone(next));
        changed
    }
}

/// 是否需要在下一个安全点唤醒消费者：revision、origin 与完整健康都要比。
///
/// 只比 revision 会漏掉「同样内容但刚刚降级/恢复」；只比健康会漏掉内容变化。
fn changed_since(previous: &SettingsSnapshot, next: &SettingsSnapshot) -> bool {
    previous.revision != next.revision
        || previous.origin != next.origin
        || previous.health != next.health
}

/// 同路径共享的运行期设置源。
pub struct LiveSettings {
    /// 已规范化为绝对形式的稳定路径。
    path: PathBuf,
    gate: Mutex<RefreshState>,
    watch: tokio::sync::watch::Sender<u64>,
    sequence: AtomicU64,
}

impl LiveSettings {
    /// 取得该路径的进程内唯一实例。
    ///
    /// 注册表存 `Weak`：实例全部释放后条目自然升级失败，不留住旧实例。
    pub fn shared(path: PathBuf) -> Arc<Self> {
        let path = crate::settings::store::lock_key(&path);
        let mut registry = registry()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = registry.get(&path).and_then(Weak::upgrade) {
            return existing;
        }
        let (watch, _receiver) = tokio::sync::watch::channel(0);
        let instance = Arc::new(Self {
            path: path.clone(),
            gate: Mutex::new(RefreshState::default()),
            watch,
            sequence: AtomicU64::new(0),
        });
        registry.insert(path, Arc::downgrade(&instance));
        // 顺手清掉已经死掉的条目，避免长进程里按历史路径堆积。
        registry.retain(|_, weak| weak.strong_count() > 0);
        instance
    }

    /// 一次完整的刷新事务：gate 覆盖 open → 有界读 → 校验 → 发布。
    ///
    /// 运行期 I/O / 校验错误只产生降级快照，绝不升级成任务取消。
    pub fn refresh(&self) -> Arc<SettingsSnapshot> {
        let mut state = self
            .gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        #[cfg(test)]
        test_hooks::fire(&self.path);
        let input = read_bounded_input(&self.path);
        let snapshot = state.accept_or_degrade(input);
        let changed = state.changed_since_last_publish(&snapshot);
        drop(state);
        if changed {
            self.notify();
        }
        snapshot
    }

    /// 与 [`crate::settings::write_to`] 共用同一把写锁与同一个提交点。
    ///
    /// 锁顺序：实例写锁 → refresh gate。持 gate 期间只做「重读 + 合并 + 校验 +
    /// rename」，不调用任何会再取写锁的函数。
    pub fn commit(&self, patch: &Value, mode: PatchMode) -> Result<CommitReceipt, SettingsError> {
        let writer = lock_for_commit(&self.path, patch)?;
        let outcome = {
            let _gate = self
                .gate
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            crate::settings::store::commit_patch_locked(&self.path, patch, mode)
        };
        drop(writer);
        if outcome.is_ok() {
            // 提交点已过：唤醒失败不能把成功伪装成失败，故直接发信号。
            self.notify();
        }
        outcome
    }

    /// 本进程失效序号（不是持久化 revision，也不跨进程排序）。
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.watch.subscribe()
    }

    fn notify(&self) {
        let next = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        // `send_replace` 即使当前没有订阅者也成功，不是错误。
        self.watch.send_replace(next);
    }
}

fn registry() -> &'static Mutex<HashMap<PathBuf, Weak<LiveSettings>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Weak<LiveSettings>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 在已登记实例的 refresh gate 内执行 `body`；没有实例时返回 `None`。
///
/// 调用方必须**已经持有**该路径的实例写锁（锁顺序：写锁 → gate）。
pub(crate) fn with_source_gate<T>(path: &Path, body: impl FnOnce(()) -> T) -> Option<T> {
    let source = registered_source(path)?;
    let _gate = source
        .gate
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    Some(body(()))
}

/// 已提交路径的登记实例收到一次失效唤醒；没有实例时是空操作。
///
/// 不取 gate、不阻塞：旧入口（[`crate::settings::write_to`]）也必须能唤醒
/// 已存在的源，但写入者不能因为读者慢而被拖住。
pub(crate) fn notify_committed(path: &Path) {
    if let Some(source) = registered_source(path) {
        source.notify();
    }
}

fn registered_source(path: &Path) -> Option<Arc<LiveSettings>> {
    let key = crate::settings::store::lock_key(path);
    let mut registry = registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let weak = registry.get(&key)?;
    let source = weak.upgrade();
    if source.is_none() {
        registry.remove(&key);
    }
    source
}

/// 有界读取：至多 `MAX_DOCUMENT_BYTES + 1` 字节，绝不把超限文件读进内存。
fn read_bounded_input(path: &Path) -> Result<BoundedSettingsInput, SettingsError> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(BoundedSettingsInput::Missing);
        }
        Err(error) => return Err(SettingsError::Io(error.to_string())),
    };
    let mut bytes = Vec::new();
    file.take(MAX_DOCUMENT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| SettingsError::Io(error.to_string()))?;
    if bytes.len() > MAX_DOCUMENT_BYTES {
        // 超限不保留原文：后续只回退默认，不做宽容归一化。
        return Err(SettingsError::TooLarge);
    }
    Ok(BoundedSettingsInput::Bytes(bytes))
}

/// 空文档的 revision；缺文件与「读完是默认」共用同一个内容标识。
fn empty_document_revision() -> String {
    revision_of_settings(&Settings::default())
}

/// 用与严格校验同一套规则算一份已归一化设置的内容标识。
///
/// 只用于启动期宽容回退：那里没有合法文档可 hash，但快照仍需要一个稳定的
/// revision。归一化后的设置必然能通过严格校验（越界/倒挂已被清空）。
fn revision_of_settings(settings: &Settings) -> String {
    let value = serde_json::to_value(settings).unwrap_or(Value::Null);
    match validate_document(value) {
        Ok(document) => document.revision,
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    fn temp_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        (dir, path)
    }

    /// 坏候选不覆盖最后一次成功内容，健康降级但仍给出有效快照；文件删除后恢复。
    #[test]
    fn live_refresh_retains_last_good_and_recovers() {
        let (_dir, path) = temp_path();
        let live = LiveSettings::shared(path.clone());
        live.commit(
            &serde_json::json!({"agent.max_turns": 17}),
            PatchMode::ReplaceProvided,
        )
        .unwrap();
        let good = live.refresh();
        std::fs::write(&path, b"{bad-json").unwrap();
        let failed = live.refresh();
        assert_eq!(failed.revision, good.revision);
        assert!(matches!(failed.origin, SnapshotOrigin::LastKnownGood));
        assert!(matches!(failed.health.state, HealthState::Degraded));
        std::fs::remove_file(&path).unwrap();
        let recovered = live.refresh();
        assert!(matches!(recovered.origin, SnapshotOrigin::Current));
        assert!(matches!(recovered.health.state, HealthState::Healthy));
        assert_eq!(recovered.settings.agent.max_turns, None);
    }

    /// 同一路径共享同一实例；不同路径绝对隔离。
    #[test]
    fn same_path_shares_source_and_other_path_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let shared_path = dir.path().join("shared/settings.json");
        let other_path = dir.path().join("other/settings.json");

        let first = LiveSettings::shared(shared_path.clone());
        let second = LiveSettings::shared(shared_path);
        assert!(
            Arc::ptr_eq(&first, &second),
            "same path must share one source"
        );

        let other = LiveSettings::shared(other_path);
        assert!(
            !Arc::ptr_eq(&first, &other),
            "different paths must be isolated"
        );

        first
            .commit(
                &serde_json::json!({"agent.max_turns": 21}),
                PatchMode::ReplaceProvided,
            )
            .unwrap();
        assert_eq!(second.refresh().settings.agent.max_turns, Some(21));
        assert_eq!(other.refresh().settings.agent.max_turns, None);
    }

    /// 旧读者在 gate 内暂停时，提交必须等 gate；旧读不能覆盖新缓存。
    ///
    /// 抓的破坏：refresh 只在 gate 内写缓存、读与校验在 gate 外，于是提交能在
    /// 旧读窗口内完成，旧读随后把过期内容发布回去。
    #[test]
    fn old_open_cannot_overwrite_new_cache() {
        let _serial = super::test_hooks::serial();
        let (_dir, path) = temp_path();
        std::fs::write(&path, br#"{"agent":{"max_turns":1}}"#).unwrap();
        let live = LiveSettings::shared(path.clone());
        let good = live.refresh();
        assert_eq!(good.settings.agent.max_turns, Some(1));

        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Arc::new(Mutex::new(release_rx));
        {
            let release_rx = Arc::clone(&release_rx);
            super::test_hooks::install(
                &path,
                Arc::new(move || {
                    let _ = entered_tx.send(());
                    let _ = release_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(10));
                }),
            );
        }

        let reader = {
            let live = Arc::clone(&live);
            std::thread::spawn(move || live.refresh())
        };
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the old reader must reach the gated read");

        let (commit_started_tx, commit_started_rx) = mpsc::channel();
        let (commit_done_tx, commit_done_rx) = mpsc::channel();
        let committer = {
            let live = Arc::clone(&live);
            std::thread::spawn(move || {
                let _ = commit_started_tx.send(());
                let result = live.commit(
                    &serde_json::json!({"agent.max_turns": 2}),
                    PatchMode::ReplaceProvided,
                );
                let _ = commit_done_tx.send(result.map(|receipt| receipt.changed_keys));
            })
        };
        commit_started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the committer must start");

        assert!(
            commit_done_rx
                .recv_timeout(Duration::from_millis(150))
                .is_err(),
            "a commit must wait for the refresh gate the old reader still holds"
        );

        release_tx.send(()).unwrap();
        let stale = reader.join().unwrap();
        assert_eq!(
            stale.settings.agent.max_turns,
            Some(1),
            "the old reader saw the old file"
        );

        let outcome = commit_done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the commit must proceed once the old reader leaves the gate");
        assert_eq!(
            outcome.expect("the commit must succeed"),
            vec!["agent.max_turns".to_string()]
        );
        committer.join().unwrap();
        super::test_hooks::clear();

        let latest = live.refresh();
        assert_eq!(latest.settings.agent.max_turns, Some(2));
        assert_eq!(latest.origin, SnapshotOrigin::Current);
        assert_eq!(latest.health.state, HealthState::Healthy);
        assert_ne!(latest.revision, good.revision);
    }

    /// 相同坏字节的重复读取必须继续报告降级，修复后才恢复健康。
    ///
    /// 抓的破坏：把「bytes 未变」直接当成健康缓存命中，坏文件的诊断被抹掉。
    #[test]
    fn same_bytes_keep_degraded_health_until_repaired() {
        let (_dir, path) = temp_path();
        std::fs::write(&path, br#"{"agent":{"max_turns":12}}"#).unwrap();
        let live = LiveSettings::shared(path.clone());
        let good = live.refresh();
        assert_eq!(good.origin, SnapshotOrigin::Current);
        assert_eq!(good.settings.agent.max_turns, Some(12));

        let bad = br#"{"agent":{"max_turns":"nonsense"},"compact":{"ratio":70}}"#;
        std::fs::write(&path, bad).unwrap();
        let first = live.refresh();
        let second = live.refresh();
        for snapshot in [&first, &second] {
            assert_eq!(snapshot.origin, SnapshotOrigin::LastKnownGood);
            assert_eq!(snapshot.health.state, HealthState::Degraded);
            assert_eq!(
                snapshot.revision, good.revision,
                "the failed candidate must not move the revision"
            );
            assert_eq!(
                snapshot.settings.agent.max_turns,
                Some(12),
                "the last good settings must survive"
            );
            assert!(
                snapshot
                    .health
                    .invalid_keys
                    .iter()
                    .any(|key| key == "agent.max_turns"),
                "the bad key must be named: {:?}",
                snapshot.health.invalid_keys
            );
        }
        assert_eq!(first.revision, second.revision);

        std::fs::write(&path, bad).unwrap();
        assert_eq!(live.refresh().health.state, HealthState::Degraded);

        std::fs::write(&path, br#"{"compact":{"ratio":70}}"#).unwrap();
        let repaired = live.refresh();
        assert_eq!(repaired.origin, SnapshotOrigin::Current);
        assert_eq!(repaired.health.state, HealthState::Healthy);
        assert!(repaired.health.invalid_keys.is_empty());
        assert_ne!(repaired.revision, good.revision);
        assert_eq!(repaired.settings.compact.ratio, Some(70));
    }

    /// 启动即超限时绝不转调无界加载器，也不把文件内容读进默认值。
    #[test]
    fn startup_oversize_never_calls_unbounded_loader() {
        let (_dir, path) = temp_path();
        let oversized = format!(
            r#"{{"agent":{{"system_prompt":"{}"}}}}"#,
            "x".repeat(MAX_DOCUMENT_BYTES + 16)
        );
        std::fs::write(&path, oversized.as_bytes()).unwrap();

        crate::settings::reset_load_from_calls();
        let live = LiveSettings::shared(path);
        let snapshot = live.refresh();

        assert_eq!(
            crate::settings::load_from_calls(),
            0,
            "the bounded reader must never delegate to the unbounded loader"
        );
        assert_eq!(snapshot.origin, SnapshotOrigin::StartupFallback);
        assert_eq!(snapshot.health.state, HealthState::Degraded);
        assert_eq!(snapshot.health.code.as_deref(), Some("too_large"));
        assert_eq!(snapshot.settings, Settings::default());
    }

    /// 旧别名 + `power: null` 的启动文件不得 panic，降级但保留其它合法字段。
    #[test]
    fn startup_alias_with_a_null_power_group_does_not_panic() {
        let (_dir, path) = temp_path();
        std::fs::write(
            &path,
            br#"{"agent":{"prevent_idle_sleep":"true","max_direct_children":4},"power":null}"#,
        )
        .unwrap();

        let live = LiveSettings::shared(path);
        let snapshot = live.refresh();

        assert_eq!(snapshot.origin, SnapshotOrigin::StartupFallback);
        assert_eq!(snapshot.health.state, HealthState::Degraded);
        assert!(
            snapshot
                .health
                .invalid_keys
                .iter()
                .any(|key| key == "power"),
            "the bad group must be named: {:?}",
            snapshot.health.invalid_keys
        );
        assert_eq!(
            snapshot.settings.agent.max_direct_children,
            Some(4),
            "valid fields survive the lenient startup fallback"
        );
        assert_eq!(snapshot.settings.power.prevent_idle_sleep, None);

        let again = live.refresh();
        assert_eq!(
            again.origin,
            SnapshotOrigin::StartupFallback,
            "a repeated bad read must not masquerade as a known-good snapshot"
        );
        assert_eq!(again.health.state, HealthState::Degraded);
    }

    /// 公共 facade（旧 `commit_patch` / `write_to` 走的入口）同样要等 gate。
    ///
    /// 抓的破坏：只让 `LiveSettings::commit` 取 gate，旧入口绕过去直接写文件，
    /// 于是设置页保存仍能把过期内容发布回缓存。
    #[test]
    fn legacy_commit_facade_waits_for_the_refresh_gate() {
        let _serial = super::test_hooks::serial();
        let (_dir, path) = temp_path();
        std::fs::write(&path, br#"{"agent":{"max_turns":5}}"#).unwrap();
        let live = LiveSettings::shared(path.clone());
        assert_eq!(live.refresh().settings.agent.max_turns, Some(5));

        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Arc::new(Mutex::new(release_rx));
        {
            let release_rx = Arc::clone(&release_rx);
            super::test_hooks::install(
                &path,
                Arc::new(move || {
                    let _ = entered_tx.send(());
                    let _ = release_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(10));
                }),
            );
        }

        let reader = {
            let live = Arc::clone(&live);
            std::thread::spawn(move || live.refresh())
        };
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the reader must reach the gated read");

        let (done_tx, done_rx) = mpsc::channel();
        let writer = {
            let path = path.clone();
            std::thread::spawn(move || {
                let result =
                    crate::settings::write_to(&path, &serde_json::json!({"agent.max_turns": 6}));
                let _ = done_tx.send(result.is_ok());
            })
        };
        assert!(
            done_rx.recv_timeout(Duration::from_millis(150)).is_err(),
            "the legacy facade must wait for the refresh gate"
        );

        release_tx.send(()).unwrap();
        reader.join().unwrap();
        assert!(
            done_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the legacy write must proceed after the reader leaves"),
            "the legacy write must succeed"
        );
        writer.join().unwrap();
        super::test_hooks::clear();

        assert_eq!(live.refresh().settings.agent.max_turns, Some(6));
    }

    /// 旧的 `write_to` 也必须经注册表唤醒已存在的源。
    #[test]
    fn write_to_wakes_a_registered_live_source() {
        let (_dir, path) = temp_path();
        std::fs::write(&path, br#"{"agent":{"max_turns":3}}"#).unwrap();
        let live = LiveSettings::shared(path.clone());
        assert_eq!(live.refresh().settings.agent.max_turns, Some(3));

        let receiver = live.subscribe();
        assert!(!receiver.has_changed().unwrap());

        crate::settings::write_to(&path, &serde_json::json!({"agent.max_turns": 4})).unwrap();
        assert!(
            receiver.has_changed().unwrap(),
            "a legacy write_to must wake the registered source"
        );

        let snapshot = live.refresh();
        assert_eq!(snapshot.settings.agent.max_turns, Some(4));
        assert_eq!(snapshot.origin, SnapshotOrigin::Current);
    }
}

/// 测试用的可控读取 hook：只在安装路径上触发，用来构造「旧读在 gate 内暂停」。
#[cfg(test)]
mod test_hooks {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

    type Hook = Arc<dyn Fn() + Send + Sync>;

    static HOOK: OnceLock<Mutex<Option<(PathBuf, Hook)>>> = OnceLock::new();
    static SERIAL: Mutex<()> = Mutex::new(());

    fn slot() -> &'static Mutex<Option<(PathBuf, Hook)>> {
        HOOK.get_or_init(|| Mutex::new(None))
    }

    /// 同一时刻只允许一个 hook 测试，避免全局 hook 互相覆盖。
    pub(super) fn serial() -> MutexGuard<'static, ()> {
        SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(super) fn install(path: &Path, hook: Hook) {
        let key = crate::settings::store::lock_key(path);
        *slot()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((key, hook));
    }

    pub(super) fn clear() {
        *slot()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    pub(super) fn fire(path: &Path) {
        let hook = {
            let slot = slot()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let key = crate::settings::store::lock_key(path);
            match slot.as_ref() {
                Some((installed, hook)) if *installed == key => Some(Arc::clone(hook)),
                _ => None,
            }
        };
        if let Some(hook) = hook {
            hook();
        }
    }
}
