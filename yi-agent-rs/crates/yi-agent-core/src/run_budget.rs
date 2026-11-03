//! Production-side budgets for one managed run.
//!
//! A side conversation inherits a parent transcript and then accumulates its
//! own: provider text, tool arguments, tool results and inline images. Every
//! one of those has a byte budget that must be checked **before** the bytes are
//! appended, so an over-eager stream stops the turn instead of growing memory
//! without bound (spec §5.3, §7.1).
//!
//! Text and binary are separate budgets: an inline image's base64 payload is
//! decoded and charged to `binary_bytes`, never charged again as text, while
//! `text_bytes` covers the serialized text/tool history.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::agent::{Session, forkable_messages};
use crate::managed_run::ManagedRunLimits;
use crate::message::{ContentBlock, ImageSource, Message};

/// The running totals of one budget.
#[derive(Debug, Default)]
struct BudgetState {
    text: AtomicUsize,
    binary: AtomicUsize,
    control: AtomicUsize,
}

/// Why a reservation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error, serde::Serialize)]
pub enum BudgetExceeded {
    #[error("side text budget exhausted: requested {requested} bytes, {available} available")]
    Text { requested: usize, available: usize },
    #[error("side binary budget exhausted: requested {requested} bytes, {available} available")]
    Binary { requested: usize, available: usize },
    #[error("side control reserve exhausted: requested {requested} bytes, {available} available")]
    Control { requested: usize, available: usize },
}

/// What a [`BudgetLease`] holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LeaseKind {
    Text,
    Binary,
    Control,
}

/// A reservation; the bytes return to the budget when this is dropped.
#[derive(Debug)]
pub struct BudgetLease {
    state: Arc<BudgetState>,
    kind: LeaseKind,
    bytes: usize,
}

impl Drop for BudgetLease {
    fn drop(&mut self) {
        let counter = match self.kind {
            LeaseKind::Text => &self.state.text,
            LeaseKind::Binary => &self.state.binary,
            LeaseKind::Control => &self.state.control,
        };
        counter.fetch_sub(self.bytes, Ordering::SeqCst);
    }
}

/// Per-run byte budget.
///
/// Cheap to clone (shared state), so a provider accumulator, a tool-result path
/// and the event sender can all charge the same budget.
#[derive(Debug, Clone)]
pub struct RunBudget {
    limits: ManagedRunLimits,
    state: Arc<BudgetState>,
}

impl RunBudget {
    pub fn new(limits: ManagedRunLimits) -> Self {
        Self {
            limits,
            state: Arc::new(BudgetState::default()),
        }
    }

    /// Bytes available to ordinary text, i.e. the text budget minus the reserve
    /// kept free for a terminal/error message.
    fn text_usable(&self) -> usize {
        self.limits
            .text_bytes
            .saturating_sub(self.limits.control_reserve)
    }

    fn try_take(&self, kind: LeaseKind, bytes: usize) -> Result<BudgetLease, BudgetExceeded> {
        let (counter, limit) = match kind {
            LeaseKind::Text => (&self.state.text, self.text_usable()),
            LeaseKind::Binary => (&self.state.binary, self.limits.binary_bytes),
            LeaseKind::Control => (&self.state.control, self.limits.control_reserve),
        };
        let mut current = counter.load(Ordering::SeqCst);
        loop {
            let available = limit.saturating_sub(current);
            if bytes > available {
                return Err(match kind {
                    LeaseKind::Text => BudgetExceeded::Text {
                        requested: bytes,
                        available,
                    },
                    LeaseKind::Binary => BudgetExceeded::Binary {
                        requested: bytes,
                        available,
                    },
                    LeaseKind::Control => BudgetExceeded::Control {
                        requested: bytes,
                        available,
                    },
                });
            }
            match counter.compare_exchange(
                current,
                current + bytes,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => {
                    return Ok(BudgetLease {
                        state: Arc::clone(&self.state),
                        kind,
                        bytes,
                    });
                }
                Err(actual) => current = actual,
            }
        }
    }

    /// Reserve text/tool-history bytes.
    pub fn reserve_text(&self, bytes: usize) -> Result<BudgetLease, BudgetExceeded> {
        self.try_take(LeaseKind::Text, bytes)
    }

    /// Reserve decoded binary (image) bytes.
    pub fn reserve_binary(&self, bytes: usize) -> Result<BudgetLease, BudgetExceeded> {
        self.try_take(LeaseKind::Binary, bytes)
    }

    /// Reserve from the space kept free for a terminal/error message.
    pub fn reserve_control(&self, bytes: usize) -> Result<BudgetLease, BudgetExceeded> {
        self.try_take(LeaseKind::Control, bytes)
    }

    /// Bytes currently held, for tests and diagnostics.
    pub fn used(&self) -> SideHistorySize {
        SideHistorySize {
            text_bytes: self.state.text.load(Ordering::SeqCst),
            binary_bytes: self.state.binary.load(Ordering::SeqCst),
        }
    }

    pub fn limits(&self) -> ManagedRunLimits {
        self.limits
    }
}

/// Measured size of a transcript under the side rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SideHistorySize {
    pub text_bytes: usize,
    pub binary_bytes: usize,
}

/// Measure the safe prefix of `session` without allocating a serialized copy.
///
/// Text is the serialized size of each message (escape overhead included: a
/// quote is two bytes on the wire, a CJK char is six). Inline base64 images are
/// measured by their **decoded** length and excluded from the text total, so the
/// same bytes are never charged twice.
pub fn measure_side_history(
    session: &Session,
    limits: &ManagedRunLimits,
) -> Result<SideHistorySize, BudgetExceeded> {
    let messages = forkable_messages(session);
    measure_messages(&messages, limits)
}

/// Measure an explicit message list under the same rules.
pub fn measure_messages(
    messages: &[Message],
    limits: &ManagedRunLimits,
) -> Result<SideHistorySize, BudgetExceeded> {
    let mut text_bytes = 0usize;
    let mut binary_bytes = 0usize;
    for message in messages {
        // The inline base64 payload is charged to `binary_bytes` as decoded
        // bytes, so its characters must not be charged again as text. Base64
        // never needs JSON escaping, so each character contributes exactly one
        // byte to the serialized form and subtracting the character count is
        // exact.
        text_bytes += serialized_len(message)?.saturating_sub(base64_payload_chars(message));
        binary_bytes += decoded_binary_len(message)?;
    }
    let text_limit = limits.text_bytes.saturating_sub(limits.control_reserve);
    if text_bytes > text_limit {
        return Err(BudgetExceeded::Text {
            requested: text_bytes,
            available: text_limit,
        });
    }
    if binary_bytes > limits.binary_bytes {
        return Err(BudgetExceeded::Binary {
            requested: binary_bytes,
            available: limits.binary_bytes,
        });
    }
    Ok(SideHistorySize {
        text_bytes,
        binary_bytes,
    })
}

/// Size of one message under the side rules, without any limit check.
///
/// Used at commit time: the caller measures first, then asks the budget for a
/// lease of exactly that many bytes, so the refusal happens before the message
/// enters the session.
pub fn measure_message_size(message: &Message) -> SideHistorySize {
    SideHistorySize {
        text_bytes: serialized_len(message)
            .unwrap_or(0)
            .saturating_sub(base64_payload_chars(message)),
        binary_bytes: decoded_binary_len(message).unwrap_or(0),
    }
}

/// Serialized size of one message, counted without materializing the JSON.
fn serialized_len(message: &Message) -> Result<usize, BudgetExceeded> {
    let mut counter = CountingWriter::default();
    serde_json::to_writer(&mut counter, message).map_err(|error| BudgetExceeded::Text {
        requested: error.to_string().len(),
        available: 0,
    })?;
    Ok(counter.bytes)
}

/// Characters occupied by inline base64 payloads in the serialized form.
fn base64_payload_chars(message: &Message) -> usize {
    message
        .content
        .iter()
        .map(block_base64_payload_chars)
        .sum::<usize>()
}

fn block_base64_payload_chars(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Image {
            source: ImageSource::Base64 { data, .. },
            ..
        } => data.len(),
        ContentBlock::ToolResult { content, .. } => {
            content.iter().map(block_base64_payload_chars).sum()
        }
        _ => 0,
    }
}

/// Decoded length of every inline base64 image in `message`.
fn decoded_binary_len(message: &Message) -> Result<usize, BudgetExceeded> {
    let mut total = 0usize;
    for block in &message.content {
        total += block_binary_len(block)?;
    }
    Ok(total)
}

fn block_binary_len(block: &ContentBlock) -> Result<usize, BudgetExceeded> {
    match block {
        ContentBlock::Image {
            source: ImageSource::Base64 { data, .. },
            ..
        } => decoded_base64_len(data).ok_or(BudgetExceeded::Binary {
            requested: data.len(),
            available: 0,
        }),
        ContentBlock::ToolResult { content, .. } => {
            let mut total = 0usize;
            for block in content {
                total += block_binary_len(block)?;
            }
            Ok(total)
        }
        _ => Ok(0),
    }
}

/// Decoded byte length of a standard base64 string, validating it first.
///
/// Validation happens before the length is trusted: a malformed payload is
/// rejected instead of being charged a made-up size. This mirrors what a real
/// decode would compute, without allocating the decoded buffer.
pub fn decoded_base64_len(data: &str) -> Option<usize> {
    let bytes = data.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut padding = 0usize;
    for (index, byte) in bytes.iter().enumerate() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'+' | b'/' => {
                if padding > 0 {
                    // Data after padding is malformed.
                    return None;
                }
            }
            b'=' => {
                // Padding is only legal in the last two positions.
                if index + 2 < bytes.len() {
                    return None;
                }
                padding += 1;
                if padding > 2 {
                    return None;
                }
            }
            b'\r' | b'\n' => return None,
            _ => return None,
        }
    }
    Some(bytes.len() / 4 * 3 - padding)
}

/// A `io::Write` that only counts bytes.
#[derive(Default)]
struct CountingWriter {
    bytes: usize,
}

impl std::io::Write for CountingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.bytes += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ImageDetail, ThinkingToken};
    use serde_json::json;

    fn limits_with(text: usize, control: usize) -> ManagedRunLimits {
        let mut limits = ManagedRunLimits::side_defaults();
        limits.text_bytes = text;
        limits.control_reserve = control;
        limits
    }

    #[test]
    fn side_text_budget_rejects_before_accumulation() {
        let limits = limits_with(8, 0);
        let budget = RunBudget::new(limits);
        let held = budget.reserve_text(8).unwrap();
        assert!(budget.reserve_text(1).is_err());
        drop(held);
        assert!(budget.reserve_text(8).is_ok());
    }

    #[test]
    fn side_control_reserve_is_never_spent_on_body_text() {
        let limits = limits_with(16, 4);
        let budget = RunBudget::new(limits);
        // Body text may use 12 of 16 bytes; the last 4 stay free.
        let body = budget.reserve_text(12).unwrap();
        assert!(budget.reserve_text(1).is_err());
        // The reserve is still reachable for a terminal message.
        let control = budget.reserve_control(4).unwrap();
        assert!(budget.reserve_control(1).is_err());
        drop(body);
        drop(control);
        assert_eq!(budget.used().text_bytes, 0);
    }

    #[test]
    fn side_binary_budget_is_separate_from_text() {
        let mut limits = limits_with(1024, 0);
        limits.binary_bytes = 8;
        let budget = RunBudget::new(limits);
        let binary = budget.reserve_binary(8).unwrap();
        assert!(budget.reserve_binary(1).is_err());
        // Binary pressure must not consume text budget.
        assert!(budget.reserve_text(1024).is_ok());
        drop(binary);
        assert!(budget.reserve_binary(8).is_ok());
    }

    #[test]
    fn side_base64_length_counts_decoded_bytes_once() {
        // "AAAA" is 3 decoded bytes; "AAA=" is 2; "AA==" is 1.
        assert_eq!(decoded_base64_len("AAAA"), Some(3));
        assert_eq!(decoded_base64_len("AAA="), Some(2));
        assert_eq!(decoded_base64_len("AA=="), Some(1));
        // Malformed payloads are rejected rather than charged.
        assert_eq!(decoded_base64_len("A==="), None);
        assert_eq!(decoded_base64_len("AAAA="), None);
        assert_eq!(decoded_base64_len("AA*A"), None);
        assert_eq!(decoded_base64_len(""), None);
        assert_eq!(decoded_base64_len("AAA"), None);
    }

    #[test]
    fn side_history_counts_serialized_escape_overhead_not_raw_chars() {
        let mut limits = limits_with(1024, 0);
        limits.binary_bytes = 1024;
        // A quote escapes to two bytes and a CJK char to six in JSON.
        let message = Message {
            role: crate::message::Role::User,
            content: vec![ContentBlock::Text("\\\"中".into())],
        };
        let size = measure_messages(std::slice::from_ref(&message), &limits).unwrap();
        let raw_chars = "\\\"中".len();
        assert!(
            size.text_bytes > raw_chars,
            "escaped JSON size ({}) must exceed raw byte length ({raw_chars})",
            size.text_bytes
        );
        // The counting writer agrees with a real serialization.
        assert_eq!(size.text_bytes, serde_json::to_vec(&message).unwrap().len());
    }

    #[test]
    fn side_history_charges_inline_images_to_binary_only() {
        let mut limits = limits_with(1024, 0);
        limits.binary_bytes = 1024;
        let message = Message {
            role: crate::message::Role::User,
            content: vec![ContentBlock::Image {
                source: ImageSource::Base64 {
                    media_type: "image/png".into(),
                    // 8 chars -> 6 decoded bytes.
                    data: "AAAAAAAA".into(),
                },
                detail: ImageDetail::High,
                path: None,
            }],
        };
        let size = measure_messages(std::slice::from_ref(&message), &limits).unwrap();
        assert_eq!(size.binary_bytes, 6);
        // The base64 text itself is not charged to the text budget.
        assert!(
            size.text_bytes < 200,
            "base64 payload must not be charged as text, got {}",
            size.text_bytes
        );
    }

    #[test]
    fn side_history_rejects_over_budget_before_returning_content() {
        // Text overflow is refused before any content is handed back.
        let mut limits = limits_with(64, 0);
        limits.binary_bytes = 64;
        let message = Message::user("x".repeat(1000));
        assert!(matches!(
            measure_messages(&[message], &limits),
            Err(BudgetExceeded::Text { .. })
        ));

        // An image must be refused by the *binary* budget: the text envelope is
        // 112 bytes here, so the text side alone must stay satisfiable.
        let mut limits = limits_with(2048, 0);
        limits.binary_bytes = 64;
        let image = Message {
            role: crate::message::Role::User,
            content: vec![ContentBlock::Image {
                source: ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: "A".repeat(400),
                },
                detail: ImageDetail::High,
                path: None,
            }],
        };
        assert!(matches!(
            measure_messages(&[image], &limits),
            Err(BudgetExceeded::Binary { .. })
        ));
    }

    #[test]
    fn side_history_large_inline_image_does_not_consume_text_budget() {
        // 100 KiB of base64 decodes to 75 KiB: over the binary budget, but it
        // must not be charged to the text budget either.
        let mut limits = limits_with(2048, 0);
        limits.binary_bytes = 1024;
        let image = Message {
            role: crate::message::Role::User,
            content: vec![ContentBlock::Image {
                source: ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: "A".repeat(100 * 1024),
                },
                detail: ImageDetail::High,
                path: None,
            }],
        };
        assert!(matches!(
            measure_messages(std::slice::from_ref(&image), &limits),
            Err(BudgetExceeded::Binary { .. })
        ));

        // Raise only the binary budget: the text total stays tiny.
        limits.binary_bytes = 128 * 1024;
        let size = measure_messages(&[image], &limits).unwrap();
        assert_eq!(size.binary_bytes, 100 * 1024 / 4 * 3);
        assert!(
            size.text_bytes < 512,
            "base64 must not consume text budget, got {}",
            size.text_bytes
        );
    }

    #[test]
    fn side_history_uses_only_the_safe_prefix() {
        let mut limits = limits_with(4096, 0);
        limits.binary_bytes = 4096;
        // Ends on an unpaired assistant tool_use: that message is not part of
        // the safe prefix and must not be measured.
        let session = Session::from_messages(vec![
            Message::user("hi"),
            Message {
                role: crate::message::Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t".into(),
                    name: "read".into(),
                    input: json!({"path": "x".repeat(2000)}),
                }],
            },
        ]);
        let size = measure_side_history(&session, &limits).unwrap();
        assert!(
            size.text_bytes < 500,
            "only the safe prefix may be measured, got {}",
            size.text_bytes
        );
    }

    #[test]
    fn side_thinking_credentials_count_toward_text() {
        let mut limits = limits_with(4096, 0);
        limits.binary_bytes = 4096;
        let message = Message {
            role: crate::message::Role::Assistant,
            content: vec![ContentBlock::Thinking {
                text: String::new(),
                token: ThinkingToken::Signature("SIG".repeat(10)),
            }],
        };
        let size = measure_messages(std::slice::from_ref(&message), &limits).unwrap();
        assert!(
            size.text_bytes > 30,
            "the credential must be charged, got {}",
            size.text_bytes
        );
    }
}
