//! 写操作账本：同一 `requestId` 不重复执行，结果未知时不猜。
//!
//! # 为什么需要它
//!
//! 协议规定写操作按 `(requestId, method, params)` 幂等（G1 §9.3）。这条规则
//! 只有在**落盘之后**才有意义：一次 `enqueue` 已经把卡片投进 inbox、回包前
//! 进程被杀，调用方重试时若没有记录，就会得到第二张卡。而在内存里记账同样
//! 不够——崩溃重启正是要覆盖的情形。
//!
//! # 三态，不是两态
//!
//! | 状态 | 含义 | 调用方该做什么 |
//! |---|---|---|
//! | `committed` | 已落盘，结果也记下了 | 直接复用结果，不再执行 |
//! | `rejected` | 已判定失败，什么都没写 | 可直接重试 |
//! | `unknown` | **在执行中被中断**，不知道落没落 | 查，不要重发 |
//!
//! `unknown` 是这份账本存在的核心理由。缺失记录与 `rejected` 的区别是"没执行
//! 过"与"执行过且失败"；把二者合并会让一次真正落盘的写入被当成没发生。
//!
//! 反过来，"没有记录"≠`unknown`：前者说明这个 requestId 从未被受理，可以
//! 安全执行；后者说明它被受理过却没收尾。所以 [`OperationLedger::lookup`]
//! 返回 `Option`，`None` 表示"可以执行"。
//!
//! # 同名不同参
//!
//! 同一 `requestId` 配不同的 `(method, params)` 是调用方的 bug，不是可以
//! 覆盖的记录：覆盖会让先落盘的那次写入失去凭据。这里返回
//! [`LedgerError::Conflict`]，与桥的 `Conflict` 语义一致。
//!
//! # 不承诺 exactly-once
//!
//! 账本保证"重试不会二次投递"，不保证"写入与记账原子"。中途崩溃仍可能留下
//! `pending` → 查询得到 `unknown`。这是如实报告，不是失败：真正的判据是插件
//! 业务状态的幂等性（例如 inbox 投递按卡片 id 去重），账本只负责不替调用方
//! 编造一个"成功了"的结论。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 一条记录的结算状态。判别式与宿主桥的 `OperationResult` 逐字一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// 已受理，尚未收尾。崩溃后读到它就是"结果未知"。
    Pending,
    Committed,
    Rejected,
    /// 明确不可知：代次变化、被中断后无法判定。
    Unknown,
}

impl Status {
    /// 线协议里的字符串。与 `Status` 的 serde 表示同源，单独开一个方法是为了
    /// 让调用方不必为一个字符串引入 `serde_json`。
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Committed => "committed",
            Status::Rejected => "rejected",
            Status::Unknown => "unknown",
        }
    }
}

/// 账本失败。都不可重试地"糊过去"：调用方必须知道这次写入没有凭据。
#[derive(Debug, PartialEq, Eq)]
pub enum LedgerError {
    /// 同一 id 配了不同的 method/params。
    Conflict(String),
    /// 磁盘读写出错。
    Io(String),
}

impl std::fmt::Display for LedgerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LedgerError::Conflict(detail) => write!(f, "{detail}"),
            LedgerError::Io(detail) => write!(f, "operation ledger: {detail}"),
        }
    }
}

impl std::error::Error for LedgerError {}

/// 一条记录。`key` 是 `(method, params)` 的稳定摘要，用来识别"同 id 改参数"。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub request_id: String,
    pub key: String,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 一次查询的结论，形状照宿主桥的 `OperationResult`。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OperationResult {
    pub status: &'static str,
    pub request_id: String,
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 磁盘上的账本。`<state_dir>/operations.json`。
///
/// 放在状态目录而不是共享配额域：账本记录的是**这块看板**的写入，换一个
/// `state_dir` 就是另一块看板，共用一份账本只会让 requestId 在无关项目之间
/// 撞车。
#[derive(Debug, Clone)]
pub struct OperationLedger {
    path: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Contents {
    #[serde(default)]
    entries: Vec<Entry>,
}

impl OperationLedger {
    /// 打开（不创建文件）。写入时才落盘。
    pub fn open(state_dir: &Path) -> Self {
        OperationLedger {
            path: state_dir.join("operations.json"),
        }
    }

    /// `(method, params)` 的稳定摘要。
    ///
    /// 用 `serde_json::to_string` 的字节而不是结构比较：params 是任意 JSON，
    /// 键序在 `serde_json::Value` 里由 `BTreeMap` 决定，所以同一份参数永远得到
    /// 同一串字节。
    ///
    /// `request_id` **不进摘要**：它是传输层标识，不是业务参数。把它算进去会
    /// 让同一份业务参数在"带 id 记一笔"和"不带 id 查一次"两条路径上得到不同的
    /// key，进而把一次正常的查询误判成 `Conflict`。
    fn key(method: &str, params: &Value) -> String {
        let mut business = params.clone();
        if let Some(object) = business.as_object_mut() {
            object.remove("request_id");
        }
        format!("{method}\u{0}{business}")
    }

    /// 查一条记录。`None` = 这个 requestId 从未被受理，可以安全执行。
    pub fn lookup(&self, request_id: &str) -> Result<Option<Entry>, LedgerError> {
        Ok(self
            .load()?
            .entries
            .into_iter()
            .find(|e| e.request_id == request_id))
    }

    /// 按 id 取结论，供"结果未知"的查询路径使用。
    ///
    /// **没有记录时返回 `unknown` 而不是 `rejected`**：调用方问的是"这次写
    /// 到底落没落"，一个它从没见过的 id 无法证明没落（可能是别的实例写的、
    /// 也可能账本被清过），猜成 `rejected` 会让它重发一次真正落盘的写。
    pub fn result(&self, request_id: &str) -> Result<OperationResult, LedgerError> {
        let entry = self.lookup(request_id)?;
        Ok(match entry {
            None => OperationResult {
                status: "unknown",
                request_id: request_id.to_string(),
                result: None,
                error: None,
            },
            Some(entry) => OperationResult {
                // `pending` 对外一律报 `unknown`：还在执行中的写，对"已经
                // 回不来"的查询来说就是不知道结果。把 `pending` 原样漏出去
                // 会诱使调用方以为"稍后就能确定"，而真正能确认它的是发起方。
                status: match entry.status {
                    Status::Pending => "unknown",
                    other => other.as_str(),
                },
                request_id: entry.request_id,
                result: entry.result,
                error: entry.error,
            },
        })
    }

    /// 结算一条记录。返回本次是**新建**还是**复用**。
    ///
    /// 复用时不执行闭包：这是"同一 requestId 不重复投递"的落点。闭包返回
    /// `Err` 时记 `rejected`——"执行过且失败"是可以安全重试的结论，与
    /// 「不知道落没落」不是一回事。
    pub fn settle(
        &self,
        request_id: &str,
        method: &str,
        params: &Value,
        run: impl FnOnce() -> Result<Value, String>,
    ) -> Result<Settled, LedgerError> {
        let key = Self::key(method, params);
        if let Some(existing) = self.lookup(request_id)? {
            if existing.key != key {
                return Err(LedgerError::Conflict(format!(
                    "requestId {request_id:?} was already used with a different method or params — \
                     refusing to overwrite the recorded outcome"
                )));
            }
            return Ok(match existing.status {
                Status::Committed => Settled::Replayed {
                    result: existing.result.unwrap_or(Value::Null),
                },
                Status::Rejected => Settled::PreviouslyRejected {
                    error: existing.error.unwrap_or_default(),
                },
                // `pending` / `unknown`：这一次的结论不可知。**不重跑**，也不
                // 假装成功——调用方拿到的是"结果未知"，该去查而不是重发。
                Status::Pending | Status::Unknown => Settled::Unknown,
            });
        }

        // 先落 `pending` 再执行：顺序反过来就会留下"已写入、无记录"的窗口，
        // 而那正是重试要防的情形。多一次写盘的收益是崩溃后能区分"没开始"
        // 与"做到一半"。
        self.upsert(Entry {
            request_id: request_id.to_string(),
            key: key.clone(),
            status: Status::Pending,
            result: None,
            error: None,
        })?;

        // 执行与结算之间若崩溃，磁盘上留着 `pending`，查询得到 `unknown`。
        // 这正是想要的：不猜。
        match run() {
            Ok(result) => {
                self.upsert(Entry {
                    request_id: request_id.to_string(),
                    key,
                    status: Status::Committed,
                    result: Some(result.clone()),
                    error: None,
                })?;
                Ok(Settled::Executed { result })
            }
            Err(error) => {
                self.upsert(Entry {
                    request_id: request_id.to_string(),
                    key,
                    status: Status::Rejected,
                    result: None,
                    error: Some(error.clone()),
                })?;
                Err(LedgerError::Io(error))
            }
        }
    }

    /// 把一个已知结果记进去。用于"执行在账本之外完成、现在才拿到结论"的
    /// 路径（例如宿主代为执行后回报）。
    pub fn record(
        &self,
        request_id: &str,
        method: &str,
        params: &Value,
        status: Status,
        result: Option<Value>,
        error: Option<String>,
    ) -> Result<(), LedgerError> {
        let key = Self::key(method, params);
        if let Some(existing) = self.lookup(request_id)? {
            if existing.key != key {
                return Err(LedgerError::Conflict(format!(
                    "requestId {request_id:?} was already used with a different method or params"
                )));
            }
        }
        self.upsert(Entry {
            request_id: request_id.to_string(),
            key,
            status,
            result,
            error,
        })
    }

    fn load(&self) -> Result<Contents, LedgerError> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|error| LedgerError::Io(format!("corrupt ledger: {error}"))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Contents::default()),
            Err(error) => Err(LedgerError::Io(error.to_string())),
        }
    }

    /// 读-改-写，全程持 `<state_dir>/operations.lock` 的排他锁。
    ///
    /// 加锁而不是直接 `read → 改 → write`：两个 CLI 进程同时写同一块看板是
    /// 正常用法（一次入队 + 一次合并请求），没有锁会让后写的覆盖先写的。
    fn upsert(&self, entry: Entry) -> Result<(), LedgerError> {
        let _lock = self.lock()?;
        let mut contents = self.load()?;
        match contents
            .entries
            .iter_mut()
            .find(|existing| existing.request_id == entry.request_id)
        {
            Some(existing) => *existing = entry,
            None => contents.entries.push(entry),
        }
        self.save(&contents)
    }

    fn save(&self, contents: &Contents) -> Result<(), LedgerError> {
        let json = serde_json::to_vec_pretty(contents)
            .map_err(|error| LedgerError::Io(error.to_string()))?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| LedgerError::Io(error.to_string()))?;
        }
        let temp = self.path.with_extension("json.tmp");
        {
            let mut file =
                File::create(&temp).map_err(|error| LedgerError::Io(error.to_string()))?;
            file.write_all(&json)
                .map_err(|error| LedgerError::Io(error.to_string()))?;
            file.sync_all()
                .map_err(|error| LedgerError::Io(error.to_string()))?;
        }
        std::fs::rename(&temp, &self.path).map_err(|error| LedgerError::Io(error.to_string()))
    }

    /// 账本自己的锁。与 `board.lock` 分开：两者保护的是不同文件，共用一个
    /// 锁文件会让"写账本"阻塞"读看板"。
    ///
    /// 不在 drop 时 unlink：删文件会让另一个进程在旧 inode 上持锁、在新
    /// inode 上再取到锁，等于没锁（与 `persist.rs` 同理由）。
    fn lock(&self) -> Result<File, LedgerError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| LedgerError::Io(error.to_string()))?;
        }
        let path = self.path.with_extension("lock");
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| LedgerError::Io(error.to_string()))?;
        let outcome = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if outcome != 0 {
            return Err(LedgerError::Io(std::io::Error::last_os_error().to_string()));
        }
        Ok(file)
    }
}

/// `settle` 的结论。
#[derive(Debug, Clone, PartialEq)]
pub enum Settled {
    /// 本次真正执行并成功。
    Executed { result: Value },
    /// 同一 requestId 之前已成功，直接复用。
    Replayed { result: Value },
    /// 同一 requestId 之前已判定失败，不重跑。
    PreviouslyRejected { error: String },
    /// 之前受理过但结论不可知。**不重跑**——调用方应去查。
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger() -> (tempfile::TempDir, OperationLedger) {
        let dir = tempfile::tempdir().unwrap();
        let ledger = OperationLedger::open(dir.path());
        (dir, ledger)
    }

    #[test]
    fn a_first_write_executes_and_is_recorded_as_committed() {
        let (_dir, ledger) = ledger();
        let params = serde_json::json!({ "card": "a" });
        let settled = ledger
            .settle("r1", "board/enqueue", &params, || {
                Ok(serde_json::json!({ "id": "a" }))
            })
            .unwrap();
        assert_eq!(
            settled,
            Settled::Executed {
                result: serde_json::json!({ "id": "a" })
            }
        );
        assert_eq!(ledger.result("r1").unwrap().status, "committed");
    }

    #[test]
    fn the_same_request_id_never_executes_twice() {
        let (_dir, ledger) = ledger();
        let params = serde_json::json!({ "card": "a" });
        let mut runs = 0;
        let first = ledger
            .settle("r1", "board/enqueue", &params, || {
                runs += 1;
                Ok(serde_json::json!({ "id": "a" }))
            })
            .unwrap();
        let second = ledger
            .settle("r1", "board/enqueue", &params, || {
                runs += 1;
                Ok(serde_json::json!({ "id": "b" }))
            })
            .unwrap();

        assert_eq!(runs, 1, "the second call must reuse, not execute");
        assert!(matches!(first, Settled::Executed { .. }));
        assert_eq!(
            second,
            Settled::Replayed {
                result: serde_json::json!({ "id": "a" })
            }
        );
    }

    #[test]
    fn the_same_id_with_different_params_is_a_conflict_not_an_overwrite() {
        let (_dir, ledger) = ledger();
        ledger
            .settle(
                "r1",
                "board/enqueue",
                &serde_json::json!({ "card": "a" }),
                || Ok(Value::Null),
            )
            .unwrap();

        let error = ledger
            .settle(
                "r1",
                "board/enqueue",
                &serde_json::json!({ "card": "b" }),
                || Ok(Value::Null),
            )
            .unwrap_err();
        assert!(matches!(error, LedgerError::Conflict(_)), "{error:?}");
    }

    #[test]
    fn a_failed_run_is_recorded_as_rejected_and_can_be_retried() {
        let (_dir, ledger) = ledger();
        let params = serde_json::json!({});
        assert!(
            ledger
                .settle("r1", "board/enqueue", &params, || Err("boom".to_string()))
                .is_err()
        );
        assert_eq!(ledger.result("r1").unwrap().status, "rejected");

        // 同一 id 重试不会被执行（记录已存在），但结论是 rejected 而不是
        // "已经成功了"——调用方据此知道可以换一个 id 重试。
        let retry = ledger
            .settle("r1", "board/enqueue", &params, || Ok(Value::Null))
            .unwrap();
        assert_eq!(
            retry,
            Settled::PreviouslyRejected {
                error: "boom".to_string()
            }
        );
    }

    #[test]
    fn an_interrupted_write_reads_back_as_unknown_and_is_never_replayed() {
        let (dir, ledger) = ledger();
        let params = serde_json::json!({ "card": "a" });
        // 模拟"写入前落 pending、执行中被杀"：直接写一条 pending 记录。
        ledger
            .record("r1", "board/enqueue", &params, Status::Pending, None, None)
            .unwrap();

        // 重开账本（换进程），确认判据来自磁盘而不是内存。
        let reopened = OperationLedger::open(dir.path());
        assert_eq!(reopened.result("r1").unwrap().status, "unknown");

        let mut runs = 0;
        let settled = reopened
            .settle("r1", "board/enqueue", &params, || {
                runs += 1;
                Ok(Value::Null)
            })
            .unwrap();
        assert_eq!(settled, Settled::Unknown);
        assert_eq!(runs, 0, "an unknown outcome must never be re-delivered");
    }

    #[test]
    fn an_unseen_request_id_reports_unknown_not_rejected() {
        let (_dir, ledger) = ledger();
        // 没见过 ≠ 没落盘过。猜 rejected 会让调用方重发一次可能已经落盘的写。
        assert_eq!(ledger.result("never-seen").unwrap().status, "unknown");
    }

    #[test]
    fn a_corrupt_ledger_is_an_error_rather_than_silently_empty() {
        let (dir, ledger) = ledger();
        std::fs::write(dir.path().join("operations.json"), "{ not json").unwrap();
        // 静默当空表会让"已提交"的记录凭空消失，进而允许重复执行。
        assert!(matches!(
            ledger.lookup("r1").unwrap_err(),
            LedgerError::Io(_)
        ));
    }

    #[test]
    fn records_survive_a_reopen() {
        let (dir, ledger) = ledger();
        ledger
            .settle(
                "r1",
                "board/enqueue",
                &serde_json::json!({ "card": "a" }),
                || Ok(serde_json::json!({ "id": "a" })),
            )
            .unwrap();

        let reopened = OperationLedger::open(dir.path());
        let entry = reopened.lookup("r1").unwrap().expect("record persisted");
        assert_eq!(entry.status, Status::Committed);
        assert_eq!(entry.result, Some(serde_json::json!({ "id": "a" })));
    }
}
