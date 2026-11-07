//! Request idempotency and idle expiry, shared by both hosts.
//!
//! A client that retries `side/ask` must not get two turns, and an idle side
//! must expire at the same moment in the app-server and the TUI. Both rules are
//! therefore defined once, here, rather than in either host's protocol layer.

use std::collections::BTreeMap;

use sha2::{Digest, Sha256};

use super::{SideError, SideErrorReason};

/// A side's maximum accepted user text, in bytes.
///
/// This is a hard protocol limit, not a budget: a request beyond it is refused
/// with `invalid_params` before any turn is created.
pub const SIDE_MAX_REQUEST_BYTES: usize = 32 * 1024;

/// How many request ids one side remembers.
///
/// Bounded so a client cannot grow the ledger without limit by inventing ids. The
/// cap only ever refuses a *new* id; replaying a known id always works.
pub const SIDE_MAX_ACCEPTED_REQUESTS: usize = 256;

/// How long a side may stay idle before it expires.
pub const SIDE_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// One accepted request, remembered so a retry is idempotent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedRequest {
    pub request_id: String,
    pub turn_id: String,
    /// SHA-256 of the request's original UTF-8 bytes.
    pub digest: [u8; 32],
}

/// The set of requests one side has already accepted.
#[derive(Debug, Default, Clone)]
pub struct SideRequestLedger {
    requests: BTreeMap<String, AcceptedRequest>,
}

impl SideRequestLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Look up a retried request.
    ///
    /// Returns the turn id the request already became, or `None` for a request
    /// that was never accepted. A known id whose text differs is a client bug and
    /// is refused rather than silently re-run.
    ///
    /// Callers must consult this **before** the busy check: a retry after the
    /// first attempt started must return the original turn, not
    /// `turn_in_progress`.
    pub fn lookup(&self, id: &str, text: &str) -> Result<Option<String>, SideError> {
        let Some(accepted) = self.requests.get(id) else {
            return Ok(None);
        };
        if accepted.digest != digest_of(text) {
            return Err(SideError::new(
                SideErrorReason::InvalidParams,
                "该 clientRequestId 已用于另一条不同的请求内容",
            ));
        }
        Ok(Some(accepted.turn_id.clone()))
    }

    /// Record a newly accepted request.
    ///
    /// Validates the id, the text and the cap. A repeated id is refused by
    /// [`Self::lookup`] first, so reaching here with an existing id is a logic
    /// error and is reported rather than overwriting the original turn.
    pub fn accept(&mut self, id: String, text: &str, turn_id: String) -> Result<(), SideError> {
        validate_request_id(&id)?;
        if text.trim().is_empty() {
            return Err(SideError::new(
                SideErrorReason::InvalidParams,
                "旁支问题不能为空",
            ));
        }
        if text.len() > SIDE_MAX_REQUEST_BYTES {
            return Err(SideError::new(
                SideErrorReason::InvalidParams,
                format!(
                    "旁支问题超过 {SIDE_MAX_REQUEST_BYTES} 字节上限（实为 {} 字节）",
                    text.len()
                ),
            ));
        }
        if self.requests.contains_key(&id) {
            return Err(SideError::new(
                SideErrorReason::InvalidParams,
                "该 clientRequestId 已被受理",
            ));
        }
        if self.requests.len() >= SIDE_MAX_ACCEPTED_REQUESTS {
            return Err(SideError::new(
                SideErrorReason::RequestLimit,
                format!("单个旁支最多记住 {SIDE_MAX_ACCEPTED_REQUESTS} 个请求"),
            ));
        }
        self.requests.insert(
            id.clone(),
            AcceptedRequest {
                request_id: id,
                turn_id,
                digest: digest_of(text),
            },
        );
        Ok(())
    }

    /// Every accepted request id mapped to its turn id.
    ///
    /// The host re-publishes this so a client can reconcile a retry across a
    /// reconnect; it never contains message text.
    pub fn accepted_requests(&self) -> BTreeMap<String, String> {
        self.requests
            .iter()
            .map(|(id, accepted)| (id.clone(), accepted.turn_id.clone()))
            .collect()
    }
}

/// Whether `id` is a canonical (lowercase, hyphenated) UUID.
///
/// The design pins the id as 36 ASCII bytes so a client cannot use the ledger as
/// a cache by varying the spelling of the same UUID. `Uuid::parse_str` accepts
/// several loose forms, so the round-trip is what decides: only a string that
/// re-serialises to itself is canonical.
fn validate_request_id(id: &str) -> Result<(), SideError> {
    if id.len() != 36 {
        return Err(SideError::new(
            SideErrorReason::InvalidParams,
            "clientRequestId 必须是 36 字符的规范 UUID",
        ));
    }
    let parsed = uuid::Uuid::parse_str(id).map_err(|_| {
        SideError::new(
            SideErrorReason::InvalidParams,
            "clientRequestId 必须是 36 字符的规范 UUID",
        )
    })?;
    if parsed.hyphenated().to_string() != id {
        return Err(SideError::new(
            SideErrorReason::InvalidParams,
            "clientRequestId 必须是小写带连字符的规范 UUID",
        ));
    }
    Ok(())
}

/// SHA-256 over the request text's original bytes.
fn digest_of(text: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hasher.finalize().into()
}

/// The idle expiry rule for one side.
///
/// Defined once so both hosts expire a side at the same instant. Only an
/// explicit open or an accepted turn touches the deadline; listing, reading and
/// polling deliberately do not.
#[derive(Debug, Clone)]
pub struct SideIdleDeadline {
    expires_at: tokio::time::Instant,
}

impl SideIdleDeadline {
    pub fn new(now: tokio::time::Instant) -> Self {
        Self {
            expires_at: now + SIDE_IDLE_TIMEOUT,
        }
    }

    /// Record activity (an open or an accepted turn).
    pub fn touch(&mut self, now: tokio::time::Instant) {
        self.expires_at = now + SIDE_IDLE_TIMEOUT;
    }

    pub fn expires_at(&self) -> tokio::time::Instant {
        self.expires_at
    }

    /// Whether the side has been idle past its deadline at `now`.
    pub fn is_expired(&self, now: tokio::time::Instant) -> bool {
        now >= self.expires_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    #[test]
    fn side_ledger_lookup_returns_the_original_turn() {
        let mut ledger = SideRequestLedger::new();
        let request = id();
        assert_eq!(ledger.lookup(&request, "问题").unwrap(), None);
        ledger
            .accept(request.clone(), "问题", "turn-1".into())
            .unwrap();
        assert_eq!(
            ledger.lookup(&request, "问题").unwrap(),
            Some("turn-1".to_string()),
            "a retried request must resolve to the original turn"
        );
    }

    #[test]
    fn side_ledger_rejects_a_reused_id_with_different_text() {
        let mut ledger = SideRequestLedger::new();
        let request = id();
        ledger
            .accept(request.clone(), "原来的问题", "turn-1".into())
            .unwrap();
        let error = ledger.lookup(&request, "不同的内容").unwrap_err();
        assert_eq!(error.reason, SideErrorReason::InvalidParams);
    }

    #[test]
    fn side_ledger_rejects_a_non_canonical_uuid() {
        let mut ledger = SideRequestLedger::new();
        // Uppercase spelling and a brace-free but unhyphenated form are both
        // valid UUIDs but not canonical ones.
        for bad in [
            "AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA".to_string(),
            "aaaaaaaaaaaa4aaa8aaaaaaaaaaaaaaa".to_string(),
            "not-a-uuid".to_string(),
            "".to_string(),
        ] {
            let error = ledger.accept(bad.clone(), "问题", "t".into()).unwrap_err();
            assert_eq!(
                error.reason,
                SideErrorReason::InvalidParams,
                "{bad:?} must be refused"
            );
        }
    }

    #[test]
    fn side_ledger_enforces_the_byte_limit_and_never_empty() {
        let mut ledger = SideRequestLedger::new();
        let too_long = "中".repeat(SIDE_MAX_REQUEST_BYTES);
        let error = ledger.accept(id(), &too_long, "t".into()).unwrap_err();
        assert_eq!(error.reason, SideErrorReason::InvalidParams);

        let error = ledger.accept(id(), "   ", "t".into()).unwrap_err();
        assert_eq!(error.reason, SideErrorReason::InvalidParams);

        // Exactly at the limit is allowed.
        let exact = "a".repeat(SIDE_MAX_REQUEST_BYTES);
        assert!(ledger.accept(id(), &exact, "t".into()).is_ok());
    }

    #[test]
    fn side_ledger_caps_new_ids_but_keeps_replays_working() {
        let mut ledger = SideRequestLedger::new();
        let first = id();
        ledger
            .accept(first.clone(), "第一问", "turn-1".into())
            .unwrap();
        for index in 1..SIDE_MAX_ACCEPTED_REQUESTS {
            ledger
                .accept(id(), &format!("问题 {index}"), format!("turn-{index}"))
                .unwrap();
        }
        let error = ledger.accept(id(), "溢出", "turn-x".into()).unwrap_err();
        assert_eq!(error.reason, SideErrorReason::RequestLimit);
        // The cap must not break idempotency for an already accepted request.
        assert_eq!(
            ledger.lookup(&first, "第一问").unwrap(),
            Some("turn-1".to_string())
        );
    }

    #[test]
    fn side_ledger_digest_is_over_the_original_utf8_bytes() {
        let mut ledger = SideRequestLedger::new();
        let request = id();
        let text = "多字节 \\\" 内容";
        ledger.accept(request.clone(), text, "t".into()).unwrap();
        let mut hasher = Sha256::new();
        hasher.update(text.as_bytes());
        let expected: [u8; 32] = hasher.finalize().into();
        assert_eq!(
            ledger.requests.get(&request).unwrap().digest,
            expected,
            "the digest must be the raw bytes, not a normalised form"
        );
    }

    #[test]
    fn side_ledger_publishes_ids_without_text() {
        let mut ledger = SideRequestLedger::new();
        let request = id();
        ledger
            .accept(request.clone(), "秘密正文", "turn-1".into())
            .unwrap();
        let published = ledger.accepted_requests();
        assert_eq!(published.get(&request), Some(&"turn-1".to_string()));
        let rendered = format!("{published:?}");
        assert!(
            !rendered.contains("秘密正文"),
            "the published ledger must never carry message text"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn side_idle_deadline_is_thirty_minutes_and_touch_resets_it() {
        let start = tokio::time::Instant::now();
        let mut deadline = SideIdleDeadline::new(start);
        assert_eq!(deadline.expires_at(), start + SIDE_IDLE_TIMEOUT);

        tokio::time::advance(std::time::Duration::from_secs(29 * 60)).await;
        let later = tokio::time::Instant::now();
        assert!(!deadline.is_expired(later));
        deadline.touch(later);
        assert_eq!(deadline.expires_at(), later + SIDE_IDLE_TIMEOUT);

        tokio::time::advance(std::time::Duration::from_secs(31 * 60)).await;
        assert!(deadline.is_expired(tokio::time::Instant::now()));
    }
}
