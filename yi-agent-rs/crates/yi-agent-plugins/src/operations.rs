//! 幂等操作台账。
//!
//! # 为什么键是三元组
//!
//! 设计 §8：写操作不得因超时、断线或面板重载自动重放。协议层的保证是
//! 「幂等靠 `(requestId, method, params)` 三元组」：
//!
//! - 同 ID + 同 method + 同参数 → 返回原结果，**不重复执行**。
//! - 同 ID + 改了参数 → `Conflict`。静默执行新参数会让调用方以为拿到的是
//!   旧结果，而实际副作用已经发生了第二次。
//!
//! 参数比较与键序无关：`{a:1,b:2}` 与 `{b:2,a:1}` 是同一个请求。JSON 的键序
//! 不是语义。
//!
//! # 为什么读不留日志
//!
//! 读可以显式重试，因此不需要幂等记录；给它留日志会让台账里塞满一次性
//! 条目，磁盘随使用量无界增长。留下永久日志的只有写。
//!
//! # Unknown 不是失败
//!
//! 「已送出但拿不到结果」标 `Unknown`。它**不能**在重连或重启时被重放：
//! 无法区分"没执行"和"执行了但回复丢了"，重放的代价是对同一笔写做两次。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::PluginError;
use crate::locks;
use crate::types::{Binding, Effect, Operation};

/// 台账锁的等待上限。
///
/// 与注册表用同一个量级：持有时间只有一次读 + 一次原子写，正常在毫秒级。
/// 用超时而不是永久阻塞，是为了让"另一个进程卡住"表现为可报的错误，
/// 而不是面板无响应。
const LEDGER_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// 一次写操作的结局。
///
/// 与 G1 §9.2 的恢复动作一一对应：`Unknown` 只能提示用户刷新核对，
/// 不能自行认定失败并重试。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OperationOutcome {
    /// 已提交，携带结果。
    Committed { result: Value },
    /// 已送出但没有拿到结果。
    Unknown { reason: String },
    /// 明确失败，携带错误分类。
    Failed { kind: String, message: String },
}

impl OperationOutcome {
    pub fn kind(&self) -> &'static str {
        match self {
            OperationOutcome::Committed { .. } => "Committed",
            OperationOutcome::Unknown { .. } => "Unknown",
            OperationOutcome::Failed { .. } => "Failed",
        }
    }
}

/// 台账里的一条记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Entry {
    request_id: String,
    method: String,
    /// 规范化后的参数摘要。存摘要而不是原文，避免把业务数据抄一份到台账。
    params_digest: String,
    /// 绑定的项目与授权代次：同一 request ID 在另一个项目里是另一件事。
    project: String,
    grant: u64,
    outcome: OperationOutcome,
}

/// 幂等台账。
#[derive(Debug, Clone)]
pub struct OperationLedger {
    home: PathBuf,
}

impl OperationLedger {
    pub fn open(home: impl Into<PathBuf>) -> OperationLedger {
        OperationLedger { home: home.into() }
    }

    fn path(&self) -> PathBuf {
        self.home.join("plugins").join("operations.json")
    }

    /// 接受一次调用：同 ID 同参数返回原结果，同 ID 改参数返回 `Conflict`。
    ///
    /// 返回 `None` 表示这是首次见到该请求，调用方应当**真正执行**它；返回
    /// `Some(outcome)` 表示重放，直接返回原结果。
    pub fn begin(
        &self,
        operation: &Operation,
        method: &str,
        params: &Value,
    ) -> Result<Option<OperationOutcome>, PluginError> {
        // 读操作不留日志：它可以显式重试，留日志只会让台账无界增长。
        if operation.effect == Effect::Read {
            return Ok(None);
        }

        // 整段读改写必须在同一把跨进程锁内完成。没有这把锁时，两个进程
        // 会各自读到旧表、各自写回，后写者把先写者刚记下的 Unknown 抹掉。
        // 后果不是"少一条日志"，而是崩溃重启后那条请求看起来"从没发生过"，
        // 从而被当作新请求重放——正是 Unknown 机制要防的那笔重复副作用。
        let _guard = locks::acquire(&self.lock_path(), LEDGER_LOCK_TIMEOUT)?;

        let mut entries = self.read()?;
        let digest = params_digest(params);
        let key = (
            operation.binding.project.as_str().to_string(),
            operation.binding.grant,
            operation.request_id.clone(),
        );

        if let Some(existing) = entries
            .iter()
            .find(|e| e.project == key.0 && e.grant == key.1 && e.request_id == key.2)
        {
            if existing.method != method || existing.params_digest != digest {
                return Err(PluginError::Conflict(format!(
                    "request `{}` was already used with a different method or parameters; \
                     replaying with new parameters would apply a second side effect",
                    operation.request_id
                )));
            }
            return Ok(Some(existing.outcome.clone()));
        }

        // 先记 Unknown 再执行：若进程在执行中途崩溃，重启后查到的是
        // "结果未知"而不是"没有这条请求"，从而不会被当作新请求重放。
        let entry = Entry {
            request_id: operation.request_id.clone(),
            method: method.to_string(),
            params_digest: digest,
            project: key.0,
            grant: key.1,
            outcome: OperationOutcome::Unknown {
                reason: "in flight; the host has not recorded a result yet".to_string(),
            },
        };
        entries.push(entry);
        self.write(&entries)?;
        Ok(None)
    }

    /// 记录一次写操作的最终结局。
    pub fn finish(
        &self,
        operation: &Operation,
        outcome: OperationOutcome,
    ) -> Result<(), PluginError> {
        if operation.effect == Effect::Read {
            return Ok(());
        }

        // 同 begin：读改写串行，否则一次"记结果"可能被并发的"记新请求"覆盖。
        let _guard = locks::acquire(&self.lock_path(), LEDGER_LOCK_TIMEOUT)?;

        let mut entries = self.read()?;
        let existing = entries
            .iter_mut()
            .find(|e| {
                e.request_id == operation.request_id
                    && e.project == operation.binding.project.as_str()
                    && e.grant == operation.binding.grant
            })
            .ok_or_else(|| {
                PluginError::NotFound(format!(
                    "request `{}` was never begun; recording a result without a begin would \
                     let a caller overwrite another request's outcome",
                    operation.request_id
                ))
            })?;
        existing.outcome = outcome;
        self.write(&entries)
    }

    /// 查询结果，用于「结果未知 → 刷新核对」。
    ///
    /// 必须用**原 binding** 查：切换项目后不能取另一个项目的结果（G1 §6.3）。
    pub fn result(
        &self,
        request_id: &str,
        binding: &Binding,
    ) -> Result<OperationOutcome, PluginError> {
        let entries = self.read()?;
        entries
            .iter()
            .find(|e| {
                e.request_id == request_id
                    && e.project == binding.project.as_str()
                    && e.grant == binding.grant
            })
            .map(|e| e.outcome.clone())
            .ok_or_else(|| {
                PluginError::NotFound(format!(
                    "no recorded operation `{request_id}` for project `{}` grant {}",
                    binding.project.as_str(),
                    binding.grant
                ))
            })
    }

    fn read(&self) -> Result<Vec<Entry>, PluginError> {
        let path = self.path();
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(PluginError::Io(format!("{}: {error}", path.display()))),
        };
        serde_json::from_slice(&bytes).map_err(|error| {
            PluginError::RecoveryRequired(format!(
                "operation ledger at {} is unreadable ({error}); treating it as empty would \
                 replay writes the user already saw succeed",
                path.display()
            ))
        })
    }

    fn write(&self, entries: &[Entry]) -> Result<(), PluginError> {
        let path = self.path();
        let parent = path.parent().expect("ledger path has a parent");
        std::fs::create_dir_all(parent)?;
        let bytes = serde_json::to_vec(entries)
            .map_err(|e| PluginError::Io(format!("could not serialize the ledger: {e}")))?;

        let temp = parent.join(format!(
            ".operations.{}.{}.tmp",
            std::process::id(),
            unique_suffix()
        ));
        let result = (|| -> Result<(), PluginError> {
            use std::io::Write;
            let mut handle = std::fs::File::create(&temp)?;
            handle.write_all(&bytes)?;
            handle.sync_all()?;
            std::fs::rename(&temp, &path)?;
            std::fs::File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }

    /// 台账锁路径。写入必须串行，否则两个进程各读一份、各写一份，
    /// 后者把前者刚记下的 Unknown 抹掉——于是崩溃恢复时看不到那条请求。
    ///
    /// 由 `begin` 与 `finish` 各取一次短锁包住整段读改写。`result` 只读
    /// 不加锁：它只看到完整的旧文件或完整的 `rename` 后的新文件，不会看到
    /// 半成品。
    pub fn lock_path(&self) -> PathBuf {
        locks::registry_lock(&self.home).with_file_name("operations.lock")
    }
}

/// 参数摘要：键序无关的规范化 JSON。
///
/// 直接 `serde_json::to_string` 在启用 `preserve_order` 时会保留原始键序，
/// 于是 `{a,b}` 与 `{b,a}` 得到两个不同摘要，同一个请求会被判成"改了参数"。
///
/// **今天是冗余的第二道保险**：本 crate 没有启用 `serde_json/preserve_order`，
/// 所以 `Map` 就是 `BTreeMap`，键已经有序。因此「删掉规范化」的变异打不红
/// ——不是因为这里错了，而是因为第一道（BTreeMap）还在。
/// `serde_json_maps_are_sorted_so_key_order_is_already_canonical` 钉住这条
/// 前提：若哪天有人开了 `preserve_order`，那条测试会红，从而提醒他这道
/// 保险现在是唯一的了。
fn params_digest(params: &Value) -> String {
    let canonical = canonical_json(params);
    let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
    Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 递归排序对象键。数组保持顺序——数组顺序是语义的一部分。
fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut sorted = BTreeMap::new();
            for (key, nested) in map {
                sorted.insert(key.clone(), canonical_json(nested));
            }
            let mut out = serde_json::Map::new();
            for (key, nested) in sorted {
                out.insert(key, nested);
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical_json).collect()),
        other => other.clone(),
    }
}

fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos}-{n}")
}
