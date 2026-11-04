//! The side's bounded, shared transcript.
//!
//! Both hosts (app-server and TUI) render the same items, so the slicing and the
//! budgets live here once. Three rules keep it honest:
//!
//! 1. **The budget is measured on the serialized item**, not on `str::len()`.
//!    JSON escaping can inflate a string several times over (`"` and `\` become two
//!    bytes, control characters six), so a 100 KiB string can exceed a 128 KiB frame
//!    once encoded. Every candidate is serialized before it is accepted, and splits
//!    land on UTF-8 boundaries.
//! 2. **Nothing unbounded or secret enters.** No thinking credentials, no binary
//!    image data, no private paths: an image becomes a description, and the tool
//!    card shows what was read, never where the copy lives.
//! 3. **A growing message grows one part.** Streaming deltas extend the last part
//!    only, a new part is started only when the budget is full, and parts that are
//!    full never change again. Items are published as `Arc` versions, so a reader
//!    holding a snapshot never sees text mutate underneath it.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use yi_agent_core::{AgentEvent, ContentBlock};

use super::{SideError, SideErrorReason, SideResult};

/// Total serialized bytes a transcript may retain.
pub const SIDE_TRANSCRIPT_BYTES: usize = 16 * 1024 * 1024;

/// Serialized bytes one visible item may occupy.
pub const SIDE_TRANSCRIPT_MAX_ITEM_BYTES: usize = 128 * 1024;

/// Reserve kept free so a terminal notice always fits.
pub const SIDE_TRANSCRIPT_CONTROL_RESERVE: usize = 4 * 1024;

/// Display budget for a tool call's input or result.
///
/// Deliberately far below the item cap: a tool card is a summary, and the core
/// session — separately budgeted — is where the full content lives.
pub const SIDE_TOOL_DISPLAY_BYTES: usize = 8 * 1024;

/// How a tool call ended, as far as the side can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideToolStatus {
    Running,
    Completed,
    Failed,
}

/// One visible transcript item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SideItem {
    /// Stable across updates: `<turn_id>/<kind>/<logical>/<part>`.
    pub id: String,
    pub turn_id: String,
    /// Monotonic position in the transcript.
    pub ordinal: u64,
    pub content: SideItemContent,
}

/// What an item shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SideItemContent {
    UserMessage {
        text: String,
    },
    AgentMessage {
        text: String,
    },
    Thinking {
        text: String,
        /// True when the provider only offered a redacted/summary form.
        redacted: bool,
    },
    ToolCall {
        call_id: String,
        name: String,
        input: Value,
        result: Option<String>,
        status: SideToolStatus,
        /// True when `input` or `result` was shortened for display. The note is
        /// already inside the text, so an adapter must not append more.
        truncated: bool,
    },
    Notice {
        text: String,
    },
}

/// The kind segment of an item id, and what a streaming delta extends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    User,
    Agent,
    Thinking,
    Notice,
}

impl Kind {
    fn slug(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
            Self::Thinking => "thinking",
            Self::Notice => "notice",
        }
    }

    fn empty_content(self) -> SideItemContent {
        match self {
            Self::User => SideItemContent::UserMessage {
                text: String::new(),
            },
            Self::Agent => SideItemContent::AgentMessage {
                text: String::new(),
            },
            Self::Thinking => SideItemContent::Thinking {
                text: String::new(),
                redacted: false,
            },
            Self::Notice => SideItemContent::Notice {
                text: String::new(),
            },
        }
    }
}

/// One entry in transcript order.
///
/// A streaming message owns several parts (its split items); a tool call owns
/// exactly one. Entries — not items — define order, so replacing a part never
/// reorders anything.
struct Entry {
    ordinal: u64,
    parts: Vec<Arc<SideItem>>,
}

/// One side's transcript.
pub struct SideTranscript {
    limit_bytes: usize,
    max_item_bytes: usize,
    entries: Vec<Entry>,
    retained_bytes: usize,
    /// Index into `entries` of each streaming message, by key.
    streaming: HashMap<(String, Kind), usize>,
    /// How many streaming messages of each kind a turn has started, so the next
    /// delta of the same kind starts a fresh message.
    started: HashMap<(String, Kind), u64>,
    /// Entry index of each tool call.
    tool_calls: HashMap<String, usize>,
    next_ordinal: u64,
}

impl SideTranscript {
    /// A transcript bounded by `limit_bytes` overall and `max_item_bytes` per item.
    pub fn new(limit_bytes: usize, max_item_bytes: usize) -> Self {
        Self {
            limit_bytes,
            max_item_bytes,
            entries: Vec::new(),
            retained_bytes: 0,
            streaming: HashMap::new(),
            started: HashMap::new(),
            tool_calls: HashMap::new(),
            next_ordinal: 0,
        }
    }

    /// The default side transcript.
    pub fn side_defaults() -> Self {
        Self::new(SIDE_TRANSCRIPT_BYTES, SIDE_TRANSCRIPT_MAX_ITEM_BYTES)
    }

    pub fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    /// Every item, in transcript order.
    pub fn items(&self) -> Vec<Arc<SideItem>> {
        self.entries
            .iter()
            .flat_map(|entry| entry.parts.iter().cloned())
            .collect()
    }

    /// Byte budget still available for ordinary content, excluding the reserve.
    pub fn remaining_body_bytes(&self) -> usize {
        self.limit_bytes
            .saturating_sub(SIDE_TRANSCRIPT_CONTROL_RESERVE)
            .saturating_sub(self.retained_bytes)
    }

    /// Record the accepted user question.
    pub fn begin_turn(&mut self, turn_id: &str, text: &str) -> SideResult<Vec<Arc<SideItem>>> {
        self.append_text(turn_id, Kind::User, text, false)
    }

    /// Fold one agent event into the transcript.
    pub fn apply(&mut self, turn_id: &str, event: &AgentEvent) -> SideResult<Vec<Arc<SideItem>>> {
        match event {
            AgentEvent::AssistantText(delta) => {
                self.append_text(turn_id, Kind::Agent, delta, false)
            }
            AgentEvent::ReasoningDelta { text, .. } => {
                self.append_text(turn_id, Kind::Thinking, text, false)
            }
            AgentEvent::ThinkingBlock { text, .. } => self.finish_thinking(turn_id, text),
            AgentEvent::ToolCall { id, name, input } => {
                self.upsert_tool_call(turn_id, id, name, input)
            }
            AgentEvent::ToolResult { id, result } => {
                let text = tool_result_text(result);
                let (display, truncated) = truncate(&text, SIDE_TOOL_DISPLAY_BYTES);
                self.finish_tool_call(id, display, truncated, result.is_error)
            }
            AgentEvent::Notice { text, .. } => self.append_text(turn_id, Kind::Notice, text, true),
            AgentEvent::ToolExit { id, code } => match code.filter(|code| *code != 0) {
                Some(code) => self.finish_tool_call(id, format!("退出码 {code}"), false, false),
                None => Ok(self.items()),
            },
            AgentEvent::ToolTimeout { id } => {
                self.finish_tool_call(id, "工具超时".to_string(), false, true)
            }
            _ => Ok(self.items()),
        }
    }

    /// A system note that may use the control reserve.
    pub fn push_notice(&mut self, turn_id: &str, text: &str) -> SideResult<Vec<Arc<SideItem>>> {
        self.append_text(turn_id, Kind::Notice, text, true)
    }

    /// Start a new streaming message of `kind` on the next delta.
    ///
    /// Used when a provider alternates text and tool calls: each contiguous run
    /// becomes its own message instead of merging across a tool boundary.
    #[cfg(test)]
    fn break_stream(&mut self, turn_id: &str, kind: Kind) {
        self.streaming.remove(&(turn_id.to_string(), kind));
    }

    /// Append a delta, extending the last part or starting a new one.
    fn append_text(
        &mut self,
        turn_id: &str,
        kind: Kind,
        text: &str,
        control: bool,
    ) -> SideResult<Vec<Arc<SideItem>>> {
        if text.is_empty() {
            return Ok(self.items());
        }
        let key = (turn_id.to_string(), kind);
        let index = match self.streaming.get(&key) {
            Some(index) => *index,
            None => {
                let logical = self.started.get(&key).copied().unwrap_or(0);
                self.started.insert(key.clone(), logical + 1);
                let ordinal = self.next_ordinal;
                self.next_ordinal += 1;
                self.entries.push(Entry {
                    ordinal,
                    parts: Vec::new(),
                });
                let index = self.entries.len() - 1;
                self.streaming.insert(key.clone(), index);
                index
            }
        };
        let logical = self.started.get(&key).copied().unwrap_or(1) - 1;

        let mut pending = text.to_string();
        while !pending.is_empty() {
            // Grow the last part when it still has room: a stream should be one item
            // that grows, not one item per delta.
            if let Some(last) = self.entries[index].parts.last().cloned() {
                let existing = text_of(&last.content).unwrap_or("");
                let candidate_text = format!("{existing}{pending}");
                let part = self.entries[index].parts.len() - 1;
                let id = item_id(turn_id, kind.slug(), logical, part as u64);
                let candidate =
                    item_with(&last.content, &id, turn_id, last.ordinal, &candidate_text);
                let old = serialized_len(&last);
                let new = serialized_len(&candidate);
                if new <= self.max_item_bytes && self.fits_delta(new, old, control) {
                    self.retained_bytes = self.retained_bytes.saturating_sub(old) + new;
                    self.entries[index].parts[part] = Arc::new(candidate);
                    return Ok(self.items());
                }
            }

            // The part is full: commit what fits and continue with the rest.
            let part = self.entries[index].parts.len() as u64;
            let id = item_id(turn_id, kind.slug(), logical, part);
            let template = kind.empty_content();
            let Some((chunk, rest)) = split_for_budget(
                &template,
                &id,
                turn_id,
                self.entries[index].ordinal,
                &pending,
                self.max_item_bytes,
            ) else {
                return Err(SideError::new(
                    SideErrorReason::SnapshotTooLarge,
                    "旁支转录不足以放下一个字符",
                ));
            };
            let candidate = item_with(&template, &id, turn_id, self.entries[index].ordinal, &chunk);
            let size = serialized_len(&candidate);
            if !self.fits_delta(size, 0, control) {
                return Err(SideError::new(
                    SideErrorReason::SnapshotTooLarge,
                    "旁支转录已达到 16 MiB 上限，请让主会话压缩或减少附件",
                ));
            }
            self.retained_bytes = self.retained_bytes.saturating_add(size);
            self.entries[index].parts.push(Arc::new(candidate));
            pending = rest;
        }
        Ok(self.items())
    }

    /// A completed thinking block replaces its streaming display.
    fn finish_thinking(&mut self, turn_id: &str, text: &str) -> SideResult<Vec<Arc<SideItem>>> {
        let key = (turn_id.to_string(), Kind::Thinking);
        // Discard the display-only stream: the block is the authority and the token
        // it carries must never reach the transcript.
        if let Some(index) = self.streaming.remove(&key) {
            self.drop_entry(index);
        }
        self.started
            .insert(key.clone(), self.started.get(&key).copied().unwrap_or(0));
        let redacted = text.contains("[redacted]");
        self.append_thinking_text(turn_id, text, redacted)
    }

    fn append_thinking_text(
        &mut self,
        turn_id: &str,
        text: &str,
        redacted: bool,
    ) -> SideResult<Vec<Arc<SideItem>>> {
        let before = self.items();
        let updates = self.append_text(turn_id, Kind::Thinking, text, false)?;
        if !redacted {
            return Ok(updates);
        }
        // Mark every part of the newest thinking message as redacted. Content is
        // replaced wholesale (never mutated in place), so snapshots stay valid.
        let key = (turn_id.to_string(), Kind::Thinking);
        let Some(index) = self.streaming.get(&key).copied() else {
            return Ok(self.items());
        };
        for part in &mut self.entries[index].parts {
            if let SideItemContent::Thinking {
                text,
                redacted: mark,
            } = &part.content
                && !*mark
            {
                let replaced = SideItem {
                    id: part.id.clone(),
                    turn_id: part.turn_id.clone(),
                    ordinal: part.ordinal,
                    content: SideItemContent::Thinking {
                        text: text.clone(),
                        redacted: true,
                    },
                };
                let old = serialized_len(part);
                let new = serialized_len(&replaced);
                self.retained_bytes = self.retained_bytes.saturating_sub(old) + new;
                *part = Arc::new(replaced);
            }
        }
        let _ = before;
        Ok(self.items())
    }

    fn upsert_tool_call(
        &mut self,
        turn_id: &str,
        call_id: &str,
        name: &str,
        input: &Value,
    ) -> SideResult<Vec<Arc<SideItem>>> {
        let (display_input, truncated) = truncate_json(input, SIDE_TOOL_DISPLAY_BYTES);
        let ordinal = self.next_ordinal;
        let id = item_id(turn_id, "tool", ordinal, 0);
        let item = SideItem {
            id,
            turn_id: turn_id.to_string(),
            ordinal,
            content: SideItemContent::ToolCall {
                call_id: call_id.to_string(),
                name: name.to_string(),
                input: display_input,
                result: None,
                status: SideToolStatus::Running,
                truncated,
            },
        };
        let size = serialized_len(&item);
        if !self.fits_delta(size, 0, true) {
            return Err(SideError::new(
                SideErrorReason::SnapshotTooLarge,
                "旁支转录已达到 16 MiB 上限，无法再显示工具调用",
            ));
        }
        self.next_ordinal += 1;
        self.retained_bytes = self.retained_bytes.saturating_add(size);
        self.tool_calls
            .insert(call_id.to_string(), self.entries.len());
        self.entries.push(Entry {
            ordinal,
            parts: vec![Arc::new(item)],
        });
        Ok(self.items())
    }

    fn finish_tool_call(
        &mut self,
        call_id: &str,
        text: String,
        truncated: bool,
        failed: bool,
    ) -> SideResult<Vec<Arc<SideItem>>> {
        let Some(index) = self.tool_calls.get(call_id).copied() else {
            // A result for a call the side never showed (a stale turn): ignore it
            // rather than inventing an item.
            return Ok(self.items());
        };
        let Some(previous) = self.entries[index].parts.first().cloned() else {
            return Ok(self.items());
        };
        let SideItemContent::ToolCall {
            call_id: held,
            name,
            input,
            truncated: was_truncated,
            ..
        } = &previous.content
        else {
            return Ok(self.items());
        };
        let updated = SideItem {
            id: previous.id.clone(),
            turn_id: previous.turn_id.clone(),
            ordinal: previous.ordinal,
            content: SideItemContent::ToolCall {
                call_id: held.clone(),
                name: name.clone(),
                input: input.clone(),
                result: Some(text),
                status: if failed {
                    SideToolStatus::Failed
                } else {
                    SideToolStatus::Completed
                },
                truncated: *was_truncated || truncated,
            },
        };
        let new = serialized_len(&updated);
        if new > self.max_item_bytes {
            return Err(SideError::new(
                SideErrorReason::SnapshotTooLarge,
                "工具结果超出旁支转录单条上限",
            ));
        }
        let old = serialized_len(&previous);
        self.retained_bytes = self.retained_bytes.saturating_sub(old) + new;
        self.entries[index].parts[0] = Arc::new(updated);
        Ok(self.items())
    }

    /// Remove an entry and anything keyed to it, keeping order intact.
    fn drop_entry(&mut self, index: usize) {
        for part in self.entries[index].parts.drain(..) {
            self.retained_bytes = self.retained_bytes.saturating_sub(serialized_len(&part));
        }
        self.entries.remove(index);
        // Re-point every index that referred to a later entry.
        let key = |_: &(String, Kind)| true;
        let _ = key;
        let stale: Vec<(String, Kind)> = self
            .streaming
            .iter()
            .filter(|(_, at)| **at > index)
            .map(|(key, _)| key.clone())
            .collect();
        for key in stale {
            if let Some(at) = self.streaming.get(&key).copied() {
                self.streaming.insert(key, at - 1);
            }
        }
        let stale_tools: Vec<String> = self
            .tool_calls
            .iter()
            .filter(|(_, at)| **at > index)
            .map(|(call, _)| call.clone())
            .collect();
        for call in stale_tools {
            if let Some(at) = self.tool_calls.get(&call).copied() {
                self.tool_calls.insert(call, at - 1);
            }
        }
    }

    /// Whether replacing `old_size` bytes with `new_size` fits the budget.
    fn fits_delta(&self, new_size: usize, old_size: usize, control: bool) -> bool {
        if new_size > self.max_item_bytes {
            return false;
        }
        let after = self
            .retained_bytes
            .saturating_sub(old_size)
            .saturating_add(new_size);
        if control {
            after <= self.limit_bytes
        } else {
            after
                <= self
                    .limit_bytes
                    .saturating_sub(SIDE_TRANSCRIPT_CONTROL_RESERVE)
        }
    }
}

/// The streaming text of an item, if it has any.
fn text_of(content: &SideItemContent) -> Option<&str> {
    match content {
        SideItemContent::UserMessage { text }
        | SideItemContent::AgentMessage { text }
        | SideItemContent::Thinking { text, .. }
        | SideItemContent::Notice { text } => Some(text),
        SideItemContent::ToolCall { .. } => None,
    }
}

/// Build an item of the same shape as `template`, carrying `text`.
fn item_with(
    template: &SideItemContent,
    id: &str,
    turn_id: &str,
    ordinal: u64,
    text: &str,
) -> SideItem {
    let content = match template {
        SideItemContent::UserMessage { .. } => SideItemContent::UserMessage {
            text: text.to_string(),
        },
        SideItemContent::AgentMessage { .. } => SideItemContent::AgentMessage {
            text: text.to_string(),
        },
        SideItemContent::Thinking { redacted, .. } => SideItemContent::Thinking {
            text: text.to_string(),
            redacted: *redacted,
        },
        SideItemContent::Notice { .. } => SideItemContent::Notice {
            text: text.to_string(),
        },
        SideItemContent::ToolCall { .. } => SideItemContent::AgentMessage {
            text: text.to_string(),
        },
    };
    SideItem {
        id: id.to_string(),
        turn_id: turn_id.to_string(),
        ordinal,
        content,
    }
}

/// Stable id: `<turn_id>/<kind>/<logical>/<part>`.
fn item_id(turn_id: &str, kind: &str, logical: u64, part: u64) -> String {
    format!("{turn_id}/{kind}/{logical}/{part}")
}

/// Serialized size of an item: the number that is actually budgeted.
fn serialized_len(item: &SideItem) -> usize {
    serde_json::to_vec(item)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX)
}

/// The longest prefix of `text` whose item fits `max_item_bytes`, and the rest.
///
/// Binary-searched on *serialized* size: escaping makes encoded length grow
/// non-monotonically with character count, so subtracting an overhead estimate is
/// not safe. Every candidate boundary is a UTF-8 boundary.
fn split_for_budget(
    template: &SideItemContent,
    id: &str,
    turn_id: &str,
    ordinal: u64,
    text: &str,
    max_item_bytes: usize,
) -> Option<(String, String)> {
    let boundaries: Vec<usize> = (0..=text.len())
        .filter(|index| text.is_char_boundary(*index))
        .collect();
    let chars = boundaries.len().saturating_sub(1);
    let size_at = |count: usize| {
        serialized_len(&item_with(
            template,
            id,
            turn_id,
            ordinal,
            &text[..boundaries[count]],
        ))
    };
    if chars == 0 || size_at(1) > max_item_bytes {
        return None;
    }
    let mut low = 1usize;
    let mut high = chars;
    while low < high {
        let mid = (low + high).div_ceil(2);
        if size_at(mid) <= max_item_bytes {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    let end = boundaries[low];
    Some((text[..end].to_string(), text[end..].to_string()))
}

/// Shorten `text` for display, appending a marker *inside* the budget.
fn truncate(text: &str, budget: usize) -> (String, bool) {
    if text.len() <= budget {
        return (text.to_string(), false);
    }
    let note = "…（已截断）";
    let available = budget.saturating_sub(note.len());
    let end = floor_char_boundary(text, available);
    (format!("{}{note}", &text[..end]), true)
}

/// Shorten a tool input for display.
///
/// The shortened form is re-encoded as a JSON string, which adds surrounding quotes
/// and escaping, so the budget must account for that: truncating to exactly
/// `budget` would overflow it by the escape overhead.
fn truncate_json(input: &Value, budget: usize) -> (Value, bool) {
    let encoded = serde_json::to_string(input).unwrap_or_default();
    if encoded.len() <= budget {
        return (input.clone(), false);
    }
    // `budget - 2` leaves room for the two quotes; `truncate` may shrink further
    // when the string needs escaping.
    let inner_budget = budget.saturating_sub(2);
    let mut cut = truncate(&encoded, inner_budget).0;
    // Shrink until the *encoded* form fits, since escaping can only make it longer.
    while serde_json::to_string(&Value::String(cut.clone()))
        .map(|text| text.len())
        .unwrap_or(usize::MAX)
        > budget
    {
        let next = floor_char_boundary(&cut, cut.len().saturating_sub(1));
        if next == 0 {
            return (Value::String(String::new()), true);
        }
        cut.truncate(next);
    }
    (Value::String(cut), true)
}

/// The largest char boundary at or below `index`.
fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut end = index.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// Displayable text for a tool result.
///
/// An image contributes a description, never its bytes: the transcript must not
/// carry binary, and a client cannot read a side's private path anyway.
fn tool_result_text(result: &yi_agent_core::ToolResult) -> String {
    let mut parts = Vec::new();
    for block in &result.content {
        match block {
            ContentBlock::Text(text) => parts.push(text.clone()),
            ContentBlock::Image { .. } => parts.push("（图片，未放入旁支转录）".to_string()),
            ContentBlock::Thinking { .. } => {}
            ContentBlock::ToolUse { name, .. } => parts.push(format!("（调用 {name}）")),
            ContentBlock::ToolResult { .. } => {}
        }
    }
    if result.is_error {
        parts.push("（工具返回错误）".to_string());
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests;
