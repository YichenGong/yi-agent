use std::cell::{Cell, RefCell};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState, StatefulWidget, Widget};

use yi_agent_core::{AgentEvent, DoneReason, RetryCause};

use super::cell::HistoryCell;

/// Whether the boundary after `cell` needs a blank visual spacer.
fn has_spacer_after(cell: &HistoryCell, next_cell: Option<&HistoryCell>) -> bool {
    next_cell.is_some()
        && (matches!(cell, HistoryCell::UserMessage { .. })
            || matches!(
                (cell, next_cell),
                (
                    HistoryCell::ToolResult { .. },
                    Some(HistoryCell::AssistantMessage { .. })
                )
            ))
}

/// Hash of everything that can change a cell's rendered output *in place*.
///
/// The hot path calls this once per cell per frame, so it must not do real work:
/// serializing a `ToolCall`'s JSON here cost as much as re-rendering the cell
/// and dominated the frame. The fields that actually mutate after a cell is
/// pushed are the streamed `AssistantMessage` text, the `ToolCall` state, the
/// permission `resolved` flag and the `expanded` flags, and those are hashed in
/// full. Everything else can only change by the cell being replaced, which the
/// cache already notices because a replacement lands at a new index, so a length
/// proxy is enough to detect it.
///
/// `rows` is the cell's own rendered height, which the cache compares against
/// the cached lines' length: it is a free and authoritative check that the entry
/// matches the cell it was rendered from.
fn cell_fingerprint(cell: &HistoryCell) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::mem::discriminant(cell).hash(&mut hasher);
    match cell {
        HistoryCell::UserMessage { text } | HistoryCell::Markdown { text } => {
            text.len().hash(&mut hasher);
        }
        HistoryCell::AssistantMessage { markdown } => {
            markdown.len().hash(&mut hasher);
        }
        HistoryCell::ToolCall {
            id,
            name,
            input,
            state,
            expanded,
        } => {
            id.hash(&mut hasher);
            name.len().hash(&mut hasher);
            // `input` is never mutated in place, only replaced with the cell.
            input.to_string().len().hash(&mut hasher);
            (*state as u8).hash(&mut hasher);
            expanded.hash(&mut hasher);
        }
        HistoryCell::ToolResult {
            result_text,
            is_error,
            expanded,
            ..
        } => {
            result_text.len().hash(&mut hasher);
            is_error.hash(&mut hasher);
            expanded.hash(&mut hasher);
        }
        HistoryCell::Separator { label } => label.as_ref().map(String::len).hash(&mut hasher),
        HistoryCell::PermissionRequest {
            summary,
            prefix_suggestion,
            resolved,
            expanded,
            ..
        } => {
            summary.len().hash(&mut hasher);
            prefix_suggestion
                .as_ref()
                .map(String::len)
                .hash(&mut hasher);
            resolved.hash(&mut hasher);
            expanded.hash(&mut hasher);
        }
        HistoryCell::PermissionResolved { .. } => {}
    }
    hasher.finish()
}

/// Compact one-line description of a permission request.
///
/// Bash-like tools show only the command, since that is what the user is
/// judging. File tools show the target path plus the size of each payload
/// field rather than inlining the payload, which can be thousands of bytes.
fn permission_summary(tool_name: &str, input: &serde_json::Value) -> String {
    match tool_name {
        "bash" | "process_start" => input
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        "write" | "edit" => {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("<unknown path>");
            let mut parts = vec![format!("path: {path}")];
            for key in ["content", "old_string", "new_string"] {
                if let Some(value) = input.get(key).and_then(|v| v.as_str()) {
                    parts.push(format!("{key}: {} bytes", value.len()));
                }
            }
            parts.join(", ")
        }
        _ => input.to_string(),
    }
}
/// A location in the history content, independent of its current wrapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ViewportAnchor {
    cell_index: usize,
    position: AnchorPosition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnchorPosition {
    ContentLine(usize),
    AfterCellSpacer,
}

/// One history cell's rendered output at a specific width.
///
/// `fingerprint` detects in-place mutation (a streamed `AssistantText` chunk, a
/// fold toggle) without cloning the cell to compare against. `lines` is `None`
/// only until the entry has been rendered for this width, or after a mutation
/// dropped it; a `None` entry is always re-rendered before it is read.
pub(in crate::tui) struct CachedCell {
    fingerprint: u64,
    lines: Option<Vec<ratatui::text::Line<'static>>>,
}

/// Flattened, memoized rendering of the history at one width.
///
/// A frame reads the line count and the visible window many times; without this
/// the cells were re-parsed (markdown) and re-wrapped on every read, so a long
/// session burned CPU proportional to the whole transcript on every frame. The
/// cache turns that into one rebuild per content or width change.
pub(in crate::tui) struct HistoryCache {
    width: u16,
    entries: Vec<CachedCell>,
    /// `cumulative[i]` = lines in entries `0..i`, spacers included.
    cumulative: Vec<usize>,
    /// Absolute line index at which each entry starts, spacer included.
    part_offsets: Vec<usize>,
    /// The `HistoryState::content_generation` these entries were built from.
    ///
    /// Deliberately *not* tied to the width: the draw renders at the area width
    /// while `text_width` probes the area minus the scrollbar column, so keying
    /// the memoized `decision` on the width would throw it away every frame and
    /// the fit case would re-wrap the whole history forever.
    reflected_generation: u64,
    /// Last `text_width` verdict, keyed by the area it was made for.
    ///
    /// Without this the fit case would ping-pong: `text_width` probes the area
    /// minus the scrollbar column and returns the full area, so the draw would
    /// re-wrap every cell at the wider width, and the next frame would probe
    /// back at the narrower one. Remembering the verdict keeps a settled frame
    /// from re-wrapping the whole scrollback even when nothing is overflowing.
    decision: Option<(u16, u16, u16, u64)>,
}

impl HistoryCache {
    fn new(width: u16) -> Self {
        Self {
            width,
            entries: Vec::new(),
            cumulative: vec![0],
            part_offsets: Vec::new(),
            reflected_generation: 0,
            decision: None,
        }
    }

    fn total(&self) -> usize {
        self.cumulative.last().copied().unwrap_or(0)
    }
}

impl std::fmt::Debug for HistoryState {
    /// The memoized render is an implementation detail, and dumping every
    /// rendered line would bury the cells it was derived from.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HistoryState")
            .field("cells", &self.cells)
            .field("scroll_offset", &self.scroll_offset)
            .finish_non_exhaustive()
    }
}

/// State for the scrollable history area.
pub struct HistoryState {
    pub cells: Vec<HistoryCell>,
    /// Index of the currently selected cell (for Ctrl+O folding).
    pub selected: Option<usize>,
    /// Vertical scroll offset in lines (0 = bottom).
    pub scroll_offset: usize,
    /// Memoized rendering of `cells` at the last requested width.
    ///
    /// `RefCell` because the read paths (`text_width`,
    /// `flattened_line_count`, ...) take `&self` but must be able to render and
    /// store a cell on first access.
    cache: RefCell<HistoryCache>,
    /// Bumped by every mutation of `cells`.
    ///
    /// This is what lets an idle frame skip the per-cell fingerprint scan
    /// entirely: if the generation matches what the cache reflected, nothing can
    /// have changed, and the number of cells stops mattering for a frame.
    /// `Cell` because the read paths only hold `&self`.
    content_generation: Cell<u64>,
}

impl HistoryState {
    pub fn new() -> Self {
        Self {
            cells: Vec::new(),
            selected: None,
            scroll_offset: 0,
            cache: RefCell::new(HistoryCache::new(0)),
            content_generation: Cell::new(0),
        }
    }

    /// Build a state from an explicit cell list, for tests that need to start
    /// from pre-rendered history instead of a sequence of events.
    #[cfg(test)]
    pub(crate) fn from_cells(cells: Vec<HistoryCell>, scroll_offset: usize) -> Self {
        Self {
            cells,
            selected: None,
            scroll_offset,
            cache: RefCell::new(HistoryCache::new(0)),
            content_generation: Cell::new(0),
        }
    }

    /// Push a new cell while preserving the visible position when scrolled up.
    pub fn push(&mut self, cell: HistoryCell, width: u16) {
        let was_scrolled = self.scroll_offset != 0;
        let lines_before = self.flattened_line_count(width);
        self.cells.push(cell);
        self.note_content_change();
        self.apply_scroll_delta(was_scrolled, lines_before, width);
    }

    /// Append `more` to the trailing assistant message, if that is what the
    /// tail is. Returns false when it is not, which tells the caller to push a
    /// fresh cell instead. Scroll-locking mirrors `push`.
    pub(crate) fn extend_last_assistant_text(&mut self, more: &str, width: u16) -> bool {
        let was_scrolled = self.scroll_offset != 0;
        let lines_before = self.flattened_line_count(width);
        let Some(HistoryCell::AssistantMessage { markdown }) = self.cells.last_mut() else {
            return false;
        };
        markdown.push_str(more);
        self.note_content_change();
        self.apply_scroll_delta(was_scrolled, lines_before, width);
        true
    }

    /// Clear all cells and reset state.
    pub fn clear(&mut self) {
        self.cells.clear();
        self.selected = None;
        self.scroll_offset = 0;
        let cache = self.cache.get_mut();
        cache.entries.clear();
        cache.part_offsets.clear();
        cache.cumulative.truncate(1);
        cache.decision = None;
        self.content_generation
            .set(self.content_generation.get().wrapping_add(1));
    }

    /// Bring the memoized render in line with `cells` at `width`.
    ///
    /// A frame that changed nothing returns immediately. A resize drops every
    /// rendered cell, because the wrap is width-specific. Otherwise the entries
    /// that are still up to date are kept and only the differing ones are
    /// re-rendered, which during a turn is just the streaming tail.
    fn ensure_cache(&self, width: u16) {
        // Two phases on purpose. Rendering a cell takes `&self`, and holding a
        // `RefCell` borrow across it would panic the moment a renderer touched
        // the cache, so jobs are collected first and rendered with the guard
        // dropped.
        if self.cache_is_current(width) {
            return;
        }

        let (changed, rebuild_tables) = {
            let mut cache = self.cache.borrow_mut();
            // The tables have to be rebuilt only when the entries themselves
            // change; a read of unchanged history must not touch them.
            let mut rebuild_tables = false;
            if cache.width != width {
                // A wrap is width-specific, so a resize invalidates everything.
                cache.entries.clear();
                cache.cumulative.truncate(1);
                cache.part_offsets.clear();
                cache.width = width;
                rebuild_tables = true;
            }

            let mut changed = Vec::new();
            for index in 0..self.cells.len() {
                let fingerprint = cell_fingerprint(&self.cells[index]);
                // `lines.is_some()` matters: invalidation drops the rendered
                // lines without touching the fingerprint, so a matching
                // fingerprint alone does not mean the entry is usable.
                let unchanged = cache
                    .entries
                    .get(index)
                    .map(|entry| entry.fingerprint == fingerprint && entry.lines.is_some())
                    .unwrap_or(false);
                if !unchanged {
                    changed.push((index, fingerprint));
                }
            }
            if !changed.is_empty() || cache.entries.len() > self.cells.len() {
                rebuild_tables = true;
            }
            (changed, rebuild_tables)
        };

        if !changed.is_empty() {
            let rendered: Vec<Option<Vec<ratatui::text::Line<'static>>>> = changed
                .iter()
                .map(|(index, _)| Some(self.cells[*index].lines(width)))
                .collect();

            let mut cache = self.cache.borrow_mut();
            for ((index, fingerprint), lines) in changed.into_iter().zip(rendered) {
                let lines = lines.expect("every collected job was rendered");
                match cache.entries.get_mut(index) {
                    Some(entry) => {
                        entry.fingerprint = fingerprint;
                        entry.lines = Some(lines);
                    }
                    None => cache.entries.push(CachedCell {
                        fingerprint,
                        lines: Some(lines),
                    }),
                }
            }
            cache.entries.truncate(self.cells.len());
        }

        if rebuild_tables {
            self.recompute_cache_accelerators();
        }
        self.cache.borrow_mut().reflected_generation = self.content_generation.get();
    }

    /// Whether the cache already reflects `cells` at `width`.
    fn cache_is_current(&self, width: u16) -> bool {
        let cache = self.cache.borrow();
        cache.width == width
            && cache.reflected_generation == self.content_generation.get()
            && cache.entries.len() == self.cells.len()
    }

    fn recompute_cache_accelerators(&self) {
        #[cfg(test)]
        perf_counters::note_rebuild();
        let mut cache = self.cache.borrow_mut();
        let count = cache.entries.len();

        let mut offsets = Vec::with_capacity(count);
        let mut cumulative = Vec::with_capacity(count + 1);
        cumulative.push(0usize);
        let mut total = 0usize;

        for index in 0..count {
            offsets.push(total);
            total += cache.entries[index]
                .lines
                .as_ref()
                .map(Vec::len)
                .unwrap_or(0);
            if has_spacer_after(&self.cells[index], self.cells.get(index + 1)) {
                total += 1;
            }
            cumulative.push(total);
        }

        cache.cumulative = cumulative;
        cache.part_offsets = offsets;
    }

    /// Total number of display lines across all cells at given width.
    ///
    /// Counts cell content only, without the semantic blank spacers.
    #[allow(dead_code)]
    pub fn total_lines(&self, width: u16) -> usize {
        self.ensure_cache(width);
        self.cache
            .borrow()
            .entries
            .iter()
            .map(|entry| entry.lines.as_ref().map(Vec::len).unwrap_or(0))
            .sum()
    }

    /// Number of display lines including semantic blank spacers. This matches
    /// the line count used by `HistoryView::flattened_lines` / `render`.
    pub fn flattened_line_count(&self, width: u16) -> usize {
        self.ensure_cache(width);
        self.cache.borrow().total()
    }

    /// Width available to history text after reserving a scrollbar column
    /// when the content overflows the viewport.
    pub fn text_width(&self, area_width: u16, viewport_height: u16) -> u16 {
        let candidate_width = area_width.saturating_sub(1);
        if candidate_width == 0 {
            return area_width;
        }

        // Consult the memo before touching the cache. Re-deriving the width
        // would first have to render at the candidate width and might have to
        // render again at the width we return, so a settled frame must not get
        // here at all.
        let generation = self.content_generation.get();
        {
            let cache = self.cache.borrow();
            if let Some((area, height, width, decided_at)) = cache.decision {
                if area == area_width && height == viewport_height && decided_at == generation {
                    // `width` shadows the narrowing, so bind it out before the
                    // borrow is released and the cache is re-aligned.
                    let decided = width;
                    drop(cache);
                    self.ensure_cache(decided);
                    return decided;
                }
            }
        }

        self.ensure_cache(candidate_width);
        let text_width = {
            let cache = self.cache.borrow();
            if cache.total() > viewport_height as usize {
                candidate_width
            } else {
                area_width
            }
        };
        self.cache.borrow_mut().decision =
            Some((area_width, viewport_height, text_width, generation));
        // Leave the cache at the width the caller is about to render at: the
        // probe above ran at `candidate_width`, and without this the draw would
        // re-wrap every cell at `area_width` and the next probe would flip it
        // back, so a settled history would re-render once per frame.
        self.ensure_cache(text_width);
        text_width
    }

    /// Maximum meaningful `scroll_offset` for the current content at the given
    /// width and viewport height. Scrolling beyond this would leave blank rows
    /// at the bottom of the viewport, so `scroll_up` clamps to this value.
    ///
    /// Returns 0 when the content fits entirely within the viewport.
    pub fn max_scroll_offset(&self, width: u16, visible_height: u16) -> usize {
        let total = self.flattened_line_count(width);
        total.saturating_sub(visible_height as usize)
    }

    /// Clamp the stored offset to the current viewport after a resize.
    pub fn reconcile_scroll_offset(&mut self, width: u16, visible_height: u16) {
        self.scroll_offset = self
            .scroll_offset
            .min(self.max_scroll_offset(width, visible_height));
    }

    /// Capture the top visible content location when the viewport is not
    /// following the bottom. A semantic spacer belongs to its preceding cell.
    pub(super) fn capture_viewport_anchor(
        &self,
        text_width: u16,
        viewport_height: u16,
    ) -> Option<ViewportAnchor> {
        self.ensure_cache(text_width);
        let cache = self.cache.borrow();
        let total = cache.total();
        let effective_offset = self
            .scroll_offset
            .min(total.saturating_sub(viewport_height as usize));
        if effective_offset == 0 {
            return None;
        }

        let anchor_top = total.saturating_sub(viewport_height as usize + effective_offset);
        let mut lines_before = 0;
        for (cell_index, cell) in self.cells.iter().enumerate() {
            let cell_lines = cache.entries[cell_index]
                .lines
                .as_ref()
                .map(Vec::len)
                .unwrap_or(0);
            if anchor_top < lines_before + cell_lines {
                return Some(ViewportAnchor {
                    cell_index,
                    position: AnchorPosition::ContentLine(anchor_top - lines_before),
                });
            }
            lines_before += cell_lines;

            if has_spacer_after(cell, self.cells.get(cell_index + 1)) {
                if anchor_top == lines_before {
                    return Some(ViewportAnchor {
                        cell_index,
                        position: AnchorPosition::AfterCellSpacer,
                    });
                }
                lines_before += 1;
            }
        }

        None
    }

    /// Restore a captured top-of-viewport location after wrapping changes.
    pub(super) fn restore_viewport_anchor(
        &mut self,
        anchor: ViewportAnchor,
        text_width: u16,
        viewport_height: u16,
    ) {
        let Some(_anchor_cell) = self.cells.get(anchor.cell_index) else {
            self.reconcile_scroll_offset(text_width, viewport_height);
            return;
        };

        self.ensure_cache(text_width);
        let cache = self.cache.borrow();
        let cell_lines = |index: usize| -> usize {
            cache.entries[index]
                .lines
                .as_ref()
                .map(Vec::len)
                .unwrap_or(0)
        };
        let anchor_cell_lines = cell_lines(anchor.cell_index);

        let mut anchor_top = 0;
        for (cell_index, cell) in self.cells.iter().enumerate() {
            if cell_index == anchor.cell_index {
                anchor_top += match anchor.position {
                    AnchorPosition::ContentLine(line_in_cell) => {
                        line_in_cell.min(anchor_cell_lines.saturating_sub(1))
                    }
                    AnchorPosition::AfterCellSpacer => anchor_cell_lines,
                };
                break;
            }

            anchor_top += cell_lines(cell_index);
            if has_spacer_after(cell, self.cells.get(cell_index + 1)) {
                anchor_top += 1;
            }
        }

        let total = cache.total();
        // `max_scroll_offset` borrows the cache again, so the guard has to go.
        drop(cache);
        self.scroll_offset = total
            .saturating_sub(viewport_height as usize)
            .saturating_sub(anchor_top)
            .min(self.max_scroll_offset(text_width, viewport_height));
    }

    /// Move selection up by one cell.
    pub fn select_up(&mut self) {
        match self.selected {
            None => self.selected = Some(self.cells.len().saturating_sub(1).saturating_sub(1)),
            Some(0) => {}
            Some(i) => self.selected = Some(i - 1),
        }
    }

    /// Move selection down by one cell.
    pub fn select_down(&mut self) {
        match self.selected {
            None => {}
            Some(i) if i + 1 >= self.cells.len() => self.selected = None,
            Some(i) => self.selected = Some(i + 1),
        }
    }

    /// Toggle fold on selected cell.
    pub fn toggle_fold_selected(&mut self) {
        if let Some(i) = self.selected {
            if let Some(cell) = self.cells.get_mut(i) {
                cell.toggle_fold();
                self.invalidate_cell(i);
            }
        }
    }

    /// Record that `cells` changed, so the memoized `text_width` verdict is
    /// re-derived instead of being served from a stale key.
    fn note_content_change(&self) {
        self.content_generation
            .set(self.content_generation.get().wrapping_add(1));
        self.cache.borrow_mut().decision = None;
    }

    /// Drop the cached render for one cell.
    ///
    /// Callers pass the index they just mutated. The cache also catches this
    /// through the fingerprint, but dropping the lines here keeps a long
    /// collapsed body from being held in memory after it is folded away.
    fn invalidate_cell(&self, index: usize) {
        if let Some(entry) = self.cache.borrow_mut().entries.get_mut(index) {
            entry.lines = None;
        }
        self.note_content_change();
    }

    /// Drop cached renders for any cell an event mutates in place.
    ///
    /// Streamed assistant text mutates the last cell; permission resolution
    /// mutates the matching request cell.
    fn invalidate_mutated_cells(&self, event: &AgentEvent) {
        match event {
            AgentEvent::AssistantText(text) => {
                if !text.is_empty() {
                    if let Some(last) = self.cells.len().checked_sub(1) {
                        self.invalidate_cell(last);
                    }
                }
            }
            AgentEvent::ToolResult { id, .. } => {
                let hit = self
                    .cells
                    .iter()
                    .enumerate()
                    .find_map(|(index, cell)| match cell {
                        HistoryCell::ToolCall { id: cell_id, .. } if cell_id == id => Some(index),
                        _ => None,
                    });
                if let Some(index) = hit {
                    self.invalidate_cell(index);
                }
            }
            AgentEvent::PermissionResolved { request_id, .. } => {
                let hit = self
                    .cells
                    .iter()
                    .enumerate()
                    .find_map(|(index, cell)| match cell {
                        HistoryCell::PermissionRequest {
                            request_id: cell_id,
                            resolved: false,
                            ..
                        } if cell_id == request_id => Some(index),
                        _ => None,
                    });
                if let Some(index) = hit {
                    self.invalidate_cell(index);
                }
            }
            _ => {}
        }
    }

    /// Scroll up by `n` lines, clamped to `max_offset` so the viewport never
    /// scrolls past the top of the content (which would leave blank rows at
    /// the bottom). Callers should pass `max_scroll_offset(width, height)`.
    pub fn scroll_up(&mut self, n: usize, max_offset: usize) {
        self.scroll_offset = self.scroll_offset.saturating_add(n).min(max_offset);
    }

    /// Scroll down by `n` lines.
    pub fn scroll_down(&mut self, n: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(n);
    }

    /// Scroll up by one viewport, treating a zero-height viewport as one line.
    pub fn scroll_page_up(&mut self, viewport_height: u16, max_offset: usize) {
        self.scroll_up(viewport_height.max(1) as usize, max_offset);
    }

    /// Scroll down by one viewport, treating a zero-height viewport as one line.
    pub fn scroll_page_down(&mut self, viewport_height: u16) {
        self.scroll_down(viewport_height.max(1) as usize);
    }

    /// Jump to the greatest valid scroll offset for the current viewport.
    pub fn scroll_to_top(&mut self, width: u16, visible_height: u16) {
        self.scroll_offset = self.max_scroll_offset(width, visible_height);
    }

    /// Jump to the newest history content.
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }

    fn apply_scroll_delta(&mut self, was_scrolled: bool, lines_before: usize, width: u16) {
        if !was_scrolled {
            return;
        }

        let lines_after = self.flattened_line_count(width);
        self.scroll_offset = if lines_after >= lines_before {
            self.scroll_offset
                .saturating_add(lines_after - lines_before)
        } else {
            self.scroll_offset
                .saturating_sub(lines_before - lines_after)
        };
    }

    /// Returns info about the most recent unresolved permission request, if any.
    pub fn pending_permission_info(
        &self,
    ) -> Option<(
        u64,
        &str,
        Option<&str>,
        &yi_agent_core::permission::PermissionKind,
    )> {
        self.cells.iter().rev().find_map(|c| match c {
            HistoryCell::PermissionRequest {
                request_id,
                tool_name,
                prefix_suggestion,
                kind,
                resolved: false,
                ..
            } => Some((
                *request_id,
                tool_name.as_str(),
                prefix_suggestion.as_deref(),
                kind,
            )),
            _ => None,
        })
    }

    /// Toggle the expanded state of the most recent unresolved permission
    /// request. Returns `false` when no request is pending.
    pub fn toggle_pending_permission_expanded(&mut self) -> bool {
        let Some(index) = self.cells.iter().rposition(|cell| {
            matches!(
                cell,
                HistoryCell::PermissionRequest {
                    resolved: false,
                    ..
                }
            )
        }) else {
            return false;
        };
        if let HistoryCell::PermissionRequest { expanded, .. } = &mut self.cells[index] {
            *expanded = !*expanded;
        }
        self.invalidate_cell(index);
        true
    }
}

impl HistoryState {
    /// Replaces the latest manual compact progress separator, if it exists.
    fn replace_pending_compaction(&mut self, label: String) {
        if let Some(HistoryCell::Separator {
            label: pending_label,
        }) = self.cells.iter_mut().rev().find(|cell| {
            matches!(
                cell,
                HistoryCell::Separator {
                    label: Some(label)
                } if label == "正在压缩对话..."
            )
        }) {
            *pending_label = Some(label);
        }
    }

    /// Process an AgentEvent and update the cell list accordingly.
    pub fn push_event(&mut self, event: AgentEvent, width: u16) {
        let was_scrolled = self.scroll_offset != 0;
        let lines_before = self.flattened_line_count(width);
        self.invalidate_mutated_cells(&event);

        match event {
            AgentEvent::Start => {}
            AgentEvent::ProviderRetry {
                attempt,
                max,
                idle_secs,
                cause,
            } => {
                // Make the retry visible: the user should know the stream failed
                // and that yi-agent is retrying, rather than watching a frozen
                // screen during the backoff. The partial text that was already
                // streamed stays on screen above this line. The label names the
                // actual cause so a timeout is not misreported as a stall.
                let label = match cause {
                    RetryCause::IdleStall => format!(
                        "Provider stalled (no output for {idle_secs}s) — retrying {attempt}/{max}"
                    ),
                    RetryCause::RequestTimeout => {
                        format!("Provider request timed out — retrying {attempt}/{max}")
                    }
                };
                self.cells
                    .push(HistoryCell::Separator { label: Some(label) });
            }
            AgentEvent::AssistantText(text) => match self.cells.last_mut() {
                Some(HistoryCell::AssistantMessage { .. }) => {
                    self.cells.last_mut().unwrap().append_assistant_text(&text);
                }
                _ => {
                    self.cells.push(HistoryCell::from_assistant_text(&text));
                }
            },
            AgentEvent::ToolCall { id, name, input } => {
                self.cells.push(HistoryCell::ToolCall {
                    id,
                    name,
                    input,
                    state: super::cell::CallState::Running,
                    expanded: false,
                });
            }
            AgentEvent::ToolResult { id, result } => {
                let is_error = result.is_error;
                let result_text = result
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        yi_agent_core::ContentBlock::Text(t) => Some(t.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                for cell in self.cells.iter_mut() {
                    if let HistoryCell::ToolCall { id: cid, state, .. } = cell {
                        if cid == &id {
                            *state = if is_error {
                                super::cell::CallState::Failed
                            } else {
                                super::cell::CallState::Success
                            };
                            break;
                        }
                    }
                }
                self.cells.push(HistoryCell::ToolResult {
                    id,
                    result_text,
                    is_error,
                    expanded: false,
                });
            }
            AgentEvent::Done { reason } => match reason {
                DoneReason::EndTurn => {
                    self.cells.push(HistoryCell::Separator { label: None });
                }
                DoneReason::MaxTurns => {
                    self.cells.push(HistoryCell::Separator {
                        label: Some("Max turns".into()),
                    });
                }
                DoneReason::Interrupted { reason } => {
                    self.cells.push(HistoryCell::Separator {
                        label: Some(format!("Interrupted: {reason}")),
                    });
                }
            },
            AgentEvent::Usage { .. } => {}
            AgentEvent::Cancelled => {
                self.cells.push(HistoryCell::Separator {
                    label: Some("Interrupted".into()),
                });
            }
            // 中途追加：在转录里留一行，让用户看到它确实被折进了本轮。
            // 其文本同时会作为 `UserInterjection` item 出现在协议层（app-server）。
            AgentEvent::InterjectionAccepted { text, .. } => {
                self.cells.push(HistoryCell::Separator {
                    label: Some(format!("追加: {text}")),
                });
            }
            // 回推本身已由 `apply_interjection_event` 负责把文本还给输入框并写提示行；
            // 这里不能再写一行，否则同一条会出现在转录里两次。
            AgentEvent::InterjectionsReturned { .. } => {}
            AgentEvent::Error(err) => {
                self.cells.push(HistoryCell::Separator {
                    label: Some(format!("Error: {err}")),
                });
            }
            // `stage`/`cause` are deliberately dropped: the raw diagnostic is
            // in the trace, and rendering it here buried the remedy behind
            // `Error { code: ... }` noise. See `runtime_restart_notice`.
            AgentEvent::SubagentRuntimeUnavailable { .. } => {
                self.cells
                    .push(crate::tui::subagents::runtime_restart_notice());
            }
            // Reported as a transcript line, never as a raw terminal write: the
            // daemon that reclaims the orphans runs in this same process while
            // the TUI paints, so a stray `eprintln!` smudged the input box.
            AgentEvent::OrphanedTasksReclaimed { count } => {
                self.cells
                    .push(crate::tui::subagents::orphaned_reclaim_notice(count));
            }
            AgentEvent::PermissionRequest {
                request_id,
                tool_name,
                tool_input,
                prefix_suggestion,
                kind,
            } => {
                let summary = permission_summary(&tool_name, &tool_input);
                let full = format!(
                    "{}: {}",
                    tool_name,
                    serde_json::to_string_pretty(&tool_input)
                        .unwrap_or_else(|_| tool_input.to_string())
                );
                self.cells.push(HistoryCell::PermissionRequest {
                    request_id,
                    tool_name,
                    summary,
                    full,
                    prefix_suggestion,
                    kind,
                    resolved: false,
                    expanded: false,
                });
            }
            AgentEvent::PermissionResolved {
                request_id,
                decision,
            } => {
                // Update the corresponding PermissionRequest cell
                for cell in self.cells.iter_mut() {
                    if let HistoryCell::PermissionRequest {
                        request_id: rid,
                        resolved,
                        ..
                    } = cell
                    {
                        if *rid == request_id {
                            *resolved = true;
                            break;
                        }
                    }
                }
                self.cells
                    .push(HistoryCell::PermissionResolved { decision });
            }
            AgentEvent::ManualCompacted {
                old_msg_count,
                new_msg_count,
            } => {
                self.replace_pending_compaction(format!(
                    "压缩完成（{old_msg_count} → {new_msg_count} 条消息）"
                ));
            }
            AgentEvent::ManualCompactFailed { message } => {
                self.replace_pending_compaction(format!("压缩失败：{message}"));
            }
            AgentEvent::AutoCompacting {
                old_msg_count,
                new_msg_count,
            } => {
                self.cells.push(HistoryCell::Separator {
                    label: Some(format!(
                        "已自动压缩（{old_msg_count} → {new_msg_count} 条消息）"
                    )),
                });
            }
            AgentEvent::ToolOutputDelta { .. }
            | AgentEvent::ToolExit { .. }
            | AgentEvent::ToolTimeout { .. }
            | AgentEvent::ToolRetry { .. }
            | AgentEvent::EstimatedPrefill(_)
            | AgentEvent::DecodeDelta(_) => {
                // Not tracked in history
            }
        }

        self.note_content_change();
        self.apply_scroll_delta(was_scrolled, lines_before, width);
    }
}

impl Default for HistoryState {
    fn default() -> Self {
        Self::new()
    }
}

/// Ratatui widget that renders the history area.
pub struct HistoryView<'a> {
    pub state: &'a HistoryState,
    #[allow(dead_code)]
    pub width: u16,
}

impl<'a> HistoryView<'a> {
    /// Flatten all cells into display lines, inserting blank spacers at
    /// semantic boundaries while keeping tool work compact.
    ///
    /// The lines come from the memoized cache, so the markdown parse and the
    /// wrapping happen once per content or width change instead of per frame.
    /// A spacer is stitched in cloned at the boundaries, because it is not part
    /// of any cell's cached lines. Tests assert the flattened structure through
    /// this; `render` reads the cache directly and clones nothing.
    #[cfg(test)]
    pub(crate) fn flattened_lines(
        &self,
        text_width: u16,
    ) -> Vec<(usize, ratatui::text::Line<'static>)> {
        self.state.ensure_cache(text_width);
        let cache = self.state.cache.borrow();
        let mut all_lines: Vec<(usize, ratatui::text::Line<'static>)> =
            Vec::with_capacity(cache.total());
        for (index, cell) in self.state.cells.iter().enumerate() {
            if let Some(lines) = cache.entries[index].lines.as_ref() {
                for line in lines {
                    all_lines.push((index, line.clone()));
                }
            }
            if has_spacer_after(cell, self.state.cells.get(index + 1)) {
                all_lines.push((index, ratatui::text::Line::raw("")));
            }
        }
        all_lines
    }

    /// The half-open range of absolute line indexes visible for `scroll_offset`.
    fn visible_range(&self, total: usize, viewport_height: u16, offset: usize) -> (usize, usize) {
        let visible_height = viewport_height as usize;
        let effective_offset = offset.min(total.saturating_sub(visible_height));
        let start = total.saturating_sub(visible_height + effective_offset);
        (start, (start + visible_height).min(total))
    }
}

/// The blank line rendered at a semantic boundary. Shared so the render loop
/// can borrow it instead of rebuilding one per frame.
static SPACER_LINE: ratatui::text::Line<'static> = ratatui::text::Line {
    spans: Vec::new(),
    style: Style::new(),
    alignment: None,
};

/// Renders one line using either a borrowed or an owned `Line`.
///
/// Borrowing is what keeps the steady-state draw cheap: a long session must not
/// clone every visible line on every frame just to hand them to ratatui.
enum LineRef<'a> {
    Borrowed(&'a ratatui::text::Line<'static>),
    Owned(ratatui::text::Line<'static>),
}

impl LineRef<'_> {
    fn line(&self) -> &ratatui::text::Line<'static> {
        match self {
            Self::Borrowed(line) => line,
            Self::Owned(line) => line,
        }
    }
}

impl<'a> Widget for HistoryView<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let text_width = self.state.text_width(area.width, area.height);
        let show_scrollbar = text_width < area.width;
        self.state.ensure_cache(text_width);
        let cache = self.state.cache.borrow();

        let visible_height = area.height as usize;
        let total = cache.total();
        // Clamp the scroll offset defensively: even if the state's
        // `scroll_offset` is larger than the maximum (e.g. content was
        // removed after scrolling, or the caller didn't clamp), the render
        // must still fill the whole viewport without leaving stale blank
        // rows at the bottom.
        let (start, end) = self.visible_range(total, area.height, self.state.scroll_offset);
        let effective_offset = self
            .state
            .scroll_offset
            .min(total.saturating_sub(visible_height));

        for (row, line_index) in (start..end).enumerate() {
            let Some((cell_index, cell_line, _is_spacer)) =
                locate_line(&cache, &self.state.cells, line_index)
            else {
                continue;
            };
            let y = area.y + row as u16;
            let x = area.x;
            let is_selected = self.state.selected == Some(cell_index);
            let mut owned: Option<ratatui::text::Line<'static>> = None;
            if is_selected {
                owned = Some(
                    cell_line
                        .clone()
                        .style(Style::new().add_modifier(Modifier::REVERSED)),
                );
            }
            let line = match owned {
                Some(line) => LineRef::Owned(line),
                None => LineRef::Borrowed(cell_line),
            };
            // ratatui truncates an over-wide `Line` on the right instead of
            // wrapping it (see `Line::render_with_alignment`), and this rect is
            // one row tall, so any overflow is permanently invisible. Every
            // producer is expected to fold to `text_width`; catch a new one
            // immediately rather than letting the user lose text.
            // A zero-width viewport renders nothing (ratatui ignores an empty
            // rect), so there is no width to satisfy.
            debug_assert!(
                text_width == 0 || line.line().width() <= text_width as usize,
                "history line wider than the viewport: {} > {text_width}: {:?}",
                line.line().width(),
                line.line()
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            );
            line.line().render(
                Rect {
                    x,
                    y,
                    width: text_width,
                    height: 1,
                },
                buf,
            );
        }

        if show_scrollbar {
            let max_offset = total.saturating_sub(visible_height);
            let top_origin_position = max_offset.saturating_sub(effective_offset);
            let mut scrollbar_state = ScrollbarState::new(total)
                .viewport_content_length(visible_height)
                .position(top_origin_position);
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .thumb_symbol("█")
                .track_symbol(Some(" "))
                .render(area, buf, &mut scrollbar_state);
        }
    }
}

/// Resolve an absolute line index to its owning cell and line.
///
/// Returns the cell index, the rendered line, and whether the line is the
/// semantic blank spacer that follows a cell. The spacer is synthesised here
/// rather than stored, so it stays out of the per-cell cache.
fn locate_line<'a>(
    cache: &'a HistoryCache,
    cells: &'a [HistoryCell],
    line_index: usize,
) -> Option<(usize, &'a ratatui::text::Line<'static>, bool)> {
    // `part_offsets` is sorted, so this finds the owning entry in log time
    // instead of walking every cell of a long transcript.
    let entry = cache
        .part_offsets
        .partition_point(|offset| *offset <= line_index)
        .checked_sub(1)?;
    let line_count = cache.entries.get(entry)?.lines.as_ref()?.len();
    let line_in_entry = line_index - cache.part_offsets[entry];
    if line_in_entry < line_count {
        return Some((
            entry,
            &cache.entries[entry].lines.as_ref()?[line_in_entry],
            false,
        ));
    }
    // Past the entry's own lines: the only line left in its slice is the
    // semantic spacer. `part_offsets` distinguishes entries by their start, so
    // a previous entry that rendered zero lines (which would make it share a
    // start with this one) never steals the lookup.
    if has_spacer_after(&cells[entry], cells.get(entry + 1)) {
        Some((entry, &SPACER_LINE, true))
    } else {
        None
    }
}

#[cfg(test)]
mod perf_counters {
    use std::cell::Cell;

    thread_local! {
        pub(super) static REBUILDS: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) fn note_rebuild() {
        REBUILDS.with(|count| count.set(count.get() + 1));
    }
}

/// Number of times the flattened history has been rebuilt from scratch.
///
/// A frame must do this at most once no matter how long the conversation is;
/// the number of cells must not affect it.
#[cfg(test)]
pub(crate) fn reset_rebuild_count() {
    perf_counters::REBUILDS.with(|count| count.set(0));
}

#[cfg(test)]
pub(crate) fn rebuild_count() -> usize {
    perf_counters::REBUILDS.with(|count| count.get())
}

/// SHA-free equality check over the rendered lines, used by tests to prove the
/// memoized output is identical to a from-scratch render.
#[cfg(test)]
fn lines_equal(a: &[ratatui::text::Line<'static>], b: &[ratatui::text::Line<'static>]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b.iter()).all(|(x, y)| {
            x.style == y.style
                && x.spans.len() == y.spans.len()
                && x.spans
                    .iter()
                    .zip(y.spans.iter())
                    .all(|(p, q)| p.content == q.content && p.style == q.style)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::cell::HistoryCell;
    use ratatui::{Terminal, backend::TestBackend};

    fn two_multiline_assistant_cells() -> HistoryState {
        HistoryState {
            cells: vec![
                HistoryCell::AssistantMessage {
                    markdown: "alpha bravo charlie delta echo foxtrot golf hotel".into(),
                },
                HistoryCell::AssistantMessage {
                    markdown: "india juliet kilo lima mike november oscar papa".into(),
                },
            ],
            selected: None,
            scroll_offset: 0,
            ..HistoryState::new()
        }
    }

    /// A frame walks the history several times (`text_width`,
    /// `reconcile_scroll_offset`, `capture_viewport_anchor`, then the draw
    /// itself). Once a history that overflows the viewport has settled, none of
    /// those walks may re-render cells: the per-frame cost must not depend on
    /// how many cells exist. This is the long-session case that made the TUI
    /// lag.
    #[test]
    fn settled_frame_does_not_re_render_every_cell() {
        const VIEWPORT_HEIGHT: u16 = 5;

        fn cell_renders_for_one_frame(cell_count: usize) -> (usize, usize) {
            let mut state = HistoryState::new();
            for index in 0..cell_count {
                state.push(
                    HistoryCell::AssistantMessage {
                        markdown: format!("answer {index} with some words in it"),
                    },
                    80,
                );
                state.push(HistoryCell::Separator { label: None }, 80);
            }

            // Settle exactly like a frame does, so the measured frame is the
            // steady-state idle frame the user actually experiences.
            let warm_text_width = state.text_width(80, VIEWPORT_HEIGHT);
            state.reconcile_scroll_offset(warm_text_width, VIEWPORT_HEIGHT);
            state.capture_viewport_anchor(warm_text_width, VIEWPORT_HEIGHT);
            let _ = HistoryView {
                state: &state,
                width: 80,
            }
            .flattened_lines(warm_text_width);

            crate::tui::cell::reset_lines_call_count();
            reset_rebuild_count();

            // --- one idle frame, mirroring app.rs `run_loop` ---
            let text_width = state.text_width(80, VIEWPORT_HEIGHT);
            state.reconcile_scroll_offset(text_width, VIEWPORT_HEIGHT);
            state.capture_viewport_anchor(text_width, VIEWPORT_HEIGHT);
            let _ = HistoryView {
                state: &state,
                width: 80,
            }
            .flattened_lines(text_width);

            (crate::tui::cell::lines_call_count(), rebuild_count())
        }

        let (small_renders, small_rebuilds) = cell_renders_for_one_frame(10);
        let (large_renders, large_rebuilds) = cell_renders_for_one_frame(200);

        assert_eq!(small_renders, 0, "an idle frame must re-render no cells");
        assert_eq!(
            small_rebuilds, 0,
            "a settled frame must not rebuild the cache"
        );
        assert_eq!(
            large_renders, 0,
            "per-frame cost must not scale with history size; 200 cells cost {large_renders} cell renders"
        );
        assert_eq!(
            large_rebuilds, 0,
            "a settled frame must not rebuild the cache"
        );
    }

    /// The cache must not grow the per-frame cost with the number of cells even
    /// when the history fits the viewport (where `text_width` probes a narrower
    /// width than it renders at). The bound has to be a constant, not `N`.
    #[test]
    fn fitting_frame_work_is_bounded_regardless_of_size() {
        const VIEWPORT_HEIGHT: u16 = 60;

        fn cell_renders_for_one_frame(cell_count: usize) -> usize {
            let mut state = HistoryState::new();
            for index in 0..cell_count {
                state.push(
                    HistoryCell::AssistantMessage {
                        markdown: format!("short {index}"),
                    },
                    80,
                );
            }
            let warm_text_width = state.text_width(80, VIEWPORT_HEIGHT);
            state.reconcile_scroll_offset(warm_text_width, VIEWPORT_HEIGHT);
            let _ = HistoryView {
                state: &state,
                width: 80,
            }
            .flattened_lines(warm_text_width);

            crate::tui::cell::reset_lines_call_count();
            let text_width = state.text_width(80, VIEWPORT_HEIGHT);
            state.reconcile_scroll_offset(text_width, VIEWPORT_HEIGHT);
            let _ = HistoryView {
                state: &state,
                width: 80,
            }
            .flattened_lines(text_width);
            crate::tui::cell::lines_call_count()
        }

        let small = cell_renders_for_one_frame(10);
        let large = cell_renders_for_one_frame(50);
        assert!(
            large <= 2 * small + 4,
            "fitting-frame work must not scale with cell count: 10 cells -> {small} renders, 50 cells -> {large} renders"
        );
    }

    /// The memoized flattening must produce exactly the same lines as rendering
    /// every cell from scratch, otherwise the optimization changes what the user
    /// sees.
    #[test]
    fn cached_flatten_matches_naive_render() {
        let mut state = HistoryState::new();
        let width = 46u16;
        state.push_event(
            AgentEvent::AssistantText("first reply\n\nwith a second paragraph".into()),
            width,
        );
        state.push_event(
            AgentEvent::ToolCall {
                id: "1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "echo hello && ls -la"}),
            },
            width,
        );
        state.push_event(
            AgentEvent::ToolResult {
                id: "1".into(),
                result: yi_agent_core::ToolResult::text("output line one\noutput line two"),
            },
            width,
        );
        state.push(
            HistoryCell::UserMessage {
                text: "next question".into(),
            },
            width,
        );
        state.push_event(
            AgentEvent::Done {
                reason: DoneReason::EndTurn,
            },
            width,
        );

        let cached = HistoryView {
            state: &state,
            width,
        }
        .flattened_lines(width);

        crate::tui::cell::reset_lines_call_count();
        let cached_again = HistoryView {
            state: &state,
            width,
        }
        .flattened_lines(width);
        assert_eq!(
            crate::tui::cell::lines_call_count(),
            0,
            "re-flattening an unchanged history must hit the cache, not re-render cells"
        );

        let mut naive: Vec<(usize, ratatui::text::Line<'static>)> = Vec::new();
        for (index, cell) in state.cells.iter().enumerate() {
            for line in cell.lines(width) {
                naive.push((index, line));
            }
            if has_spacer_after(cell, state.cells.get(index + 1)) {
                naive.push((index, ratatui::text::Line::raw("")));
            }
        }

        let cached_lines: Vec<_> = cached.iter().map(|(_, line)| line.clone()).collect();
        let cached_again_lines: Vec<_> =
            cached_again.iter().map(|(_, line)| line.clone()).collect();
        assert!(
            lines_equal(&cached_lines, &cached_again_lines),
            "a cache hit must return the same lines as the first render"
        );
        let naive_lines: Vec<_> = naive.iter().map(|(_, line)| line.clone()).collect();
        assert!(
            lines_equal(&cached_lines, &naive_lines),
            "memoized output must equal the from-scratch render\ncached={cached_lines:?}\nnaive={naive_lines:?}"
        );
        let cached_indexes: Vec<_> = cached.iter().map(|(index, _)| *index).collect();
        let naive_indexes: Vec<_> = naive.iter().map(|(index, _)| *index).collect();
        assert_eq!(cached_indexes, naive_indexes);
    }

    /// A resize must drop the cache and re-wrap at the new width, otherwise the
    /// user sees stale line breaks. This is the failure mode a naive
    /// "cache forever" implementation would have.
    #[test]
    fn cache_re_wraps_after_width_change() {
        let mut state = HistoryState::new();
        let sentence = "alpha bravo charlie delta echo foxtrot golf hotel india juliet";
        state.push(
            HistoryCell::UserMessage {
                text: sentence.into(),
            },
            40,
        );

        let narrow = HistoryView {
            state: &state,
            width: 40,
        }
        .flattened_lines(40);
        let wide = HistoryView {
            state: &state,
            width: 120,
        }
        .flattened_lines(120);

        assert!(
            narrow.len() > wide.len(),
            "narrow width must wrap into more lines: narrow={} wide={}",
            narrow.len(),
            wide.len()
        );

        crate::tui::cell::reset_lines_call_count();
        let wide_again = HistoryView {
            state: &state,
            width: 120,
        }
        .flattened_lines(120);
        assert_eq!(
            crate::tui::cell::lines_call_count(),
            0,
            "re-rendering at the width already cached must not re-wrap the cells again"
        );
        let wide_text: Vec<_> = wide.iter().map(|(_, line)| line.clone()).collect();
        let wide_again_text: Vec<_> = wide_again.iter().map(|(_, line)| line.clone()).collect();
        assert!(
            lines_equal(&wide_text, &wide_again_text),
            "a second render at the same width must return the wrapped lines, not stale narrow ones"
        );
    }

    /// A streamed assistant token mutates a cell in place; the next frame must
    /// reflect the new text instead of a cached copy of the old one.
    #[test]
    fn cache_invalidates_when_a_cell_mutates_in_place() {
        let mut state = HistoryState::new();
        let width = 60u16;
        state.push_event(AgentEvent::AssistantText("hello".into()), width);

        let before = HistoryView {
            state: &state,
            width,
        }
        .flattened_lines(width);

        state.push_event(AgentEvent::AssistantText(" world".into()), width);

        let after = HistoryView {
            state: &state,
            width,
        }
        .flattened_lines(width);

        let before_text: String = before
            .iter()
            .map(|(_, line)| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let after_text: String = after
            .iter()
            .map(|(_, line)| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(before_text.contains("hello"), "before={before_text:?}");
        assert!(
            after_text.contains("hello world"),
            "the appended token must be visible; got {after_text:?}"
        );
    }

    /// Toggling a fold mutates a cell; the next frame must show the expanded
    /// body rather than the cached folded preview.
    #[test]
    fn cache_invalidates_when_a_fold_toggles() {
        let mut state = HistoryState::new();
        let width = 60u16;
        state.push_event(
            AgentEvent::ToolResult {
                id: "1".into(),
                result: yi_agent_core::ToolResult::text("first line\nsecond line\nthird line"),
            },
            width,
        );

        let folded = HistoryView {
            state: &state,
            width,
        }
        .flattened_lines(width);
        state.selected = Some(0);
        state.toggle_fold_selected();
        let expanded = HistoryView {
            state: &state,
            width,
        }
        .flattened_lines(width);

        assert!(
            expanded.len() > folded.len(),
            "expanding a cell must add lines: folded={} expanded={}",
            folded.len(),
            expanded.len()
        );
    }

    /// Expanding a pending permission request mutates the cell in place; the
    /// next render must show the full body, not the cached collapsed preview.
    #[test]
    fn cache_invalidates_when_a_permission_request_expands() {
        let mut state = HistoryState::new();
        let width = 60u16;
        state.push_event(
            AgentEvent::PermissionRequest {
                request_id: 1,
                tool_name: "bash".into(),
                tool_input: serde_json::json!({"command": "ls -la"}),
                prefix_suggestion: None,
                kind: yi_agent_core::permission::PermissionKind::Normal,
            },
            width,
        );

        let collapsed = HistoryView {
            state: &state,
            width,
        }
        .flattened_lines(width);
        assert!(state.toggle_pending_permission_expanded());
        let expanded = HistoryView {
            state: &state,
            width,
        }
        .flattened_lines(width);

        let collapsed_text = collapsed
            .iter()
            .map(|(_, line)| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let expanded_text = expanded
            .iter()
            .map(|(_, line)| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            expanded.len() > collapsed.len(),
            "expanding must reveal more lines:\ncollapsed={collapsed_text}\nexpanded={expanded_text}"
        );
    }

    /// While a reply streams, a token arrives before each frame. That must
    /// re-render only the cell it changed, not the scrollback: measuring a
    /// streamed token is the case users actually sit in.
    #[test]
    fn streamed_token_re_renders_only_the_tail() {
        const VIEWPORT_HEIGHT: u16 = 5;

        // The area is 80 wide; overflowing content reserves one column, so the
        // resolved width is 79. The app applies events at this resolved width
        // (`run_loop`), which is what keeps the cache from flip-flopping.
        const AREA_WIDTH: u16 = 80;
        const TEXT_WIDTH: u16 = 79;

        fn renders_for_one_token(cell_count: usize) -> usize {
            let mut state = HistoryState::new();
            for index in 0..cell_count {
                state.push_event(
                    AgentEvent::AssistantText(format!("answer {index} with some words")),
                    TEXT_WIDTH,
                );
                state.push_event(
                    AgentEvent::Done {
                        reason: DoneReason::EndTurn,
                    },
                    TEXT_WIDTH,
                );
            }
            // Settle so the history overflows the viewport at a stable width.
            assert_eq!(
                state.text_width(AREA_WIDTH, VIEWPORT_HEIGHT),
                TEXT_WIDTH,
                "the fixture must overflow, otherwise no column is reserved"
            );
            state.reconcile_scroll_offset(TEXT_WIDTH, VIEWPORT_HEIGHT);
            let _ = HistoryView {
                state: &state,
                width: AREA_WIDTH,
            }
            .flattened_lines(TEXT_WIDTH);
            let _ = state.capture_viewport_anchor(TEXT_WIDTH, VIEWPORT_HEIGHT);

            // One streamed token, then the frame that would show it.
            crate::tui::cell::reset_lines_call_count();
            state.push_event(AgentEvent::AssistantText(" more".into()), TEXT_WIDTH);
            let width = state.text_width(AREA_WIDTH, VIEWPORT_HEIGHT);
            state.reconcile_scroll_offset(width, VIEWPORT_HEIGHT);
            let _ = state.capture_viewport_anchor(width, VIEWPORT_HEIGHT);
            let _ = HistoryView {
                state: &state,
                width: AREA_WIDTH,
            }
            .flattened_lines(width);
            crate::tui::cell::lines_call_count()
        }

        let small = renders_for_one_token(10);
        let large = renders_for_one_token(200);
        assert!(
            large <= small.max(2),
            "a streamed token must not scale with history size: 10 cells -> {small} renders, 200 cells -> {large} renders"
        );
    }

    /// Differential test: drive a long, mixed sequence of mutations, resizes
    /// and viewport changes, and after every step require the memoized output
    /// to be identical to a from-scratch render of the same cells.
    ///
    /// This is the property the whole cache rests on, and a fixed script would
    /// only cover the cases its author thought of, so the sequence is
    /// pseudo-random (deterministic seed) over the mutation kinds that actually
    /// exist.
    #[test]
    fn memoized_output_matches_naive_render_over_long_sequence() {
        fn naive(state: &HistoryState, width: u16) -> Vec<(usize, ratatui::text::Line<'static>)> {
            let mut all: Vec<(usize, ratatui::text::Line<'static>)> = Vec::new();
            for (index, cell) in state.cells.iter().enumerate() {
                for line in cell.lines(width) {
                    all.push((index, line));
                }
                if has_spacer_after(cell, state.cells.get(index + 1)) {
                    all.push((index, ratatui::text::Line::raw("")));
                }
            }
            all
        }

        fn compare(state: &HistoryState, width: u16, step: usize) {
            let cached = HistoryView { state, width }.flattened_lines(width);
            let expected = naive(state, width);
            assert_eq!(
                cached.len(),
                expected.len(),
                "line count diverged at step {step}, width {width}"
            );
            assert_eq!(
                state.flattened_line_count(width),
                expected.len(),
                "flattened_line_count diverged from the naive count at step {step}, width {width}"
            );
            let cached_lines: Vec<_> = cached.iter().map(|(_, line)| line.clone()).collect();
            let expected_lines: Vec<_> = expected.iter().map(|(_, line)| line.clone()).collect();
            assert!(
                lines_equal(&cached_lines, &expected_lines),
                "rendered content diverged at step {step}, width {width}"
            );
            let cached_indexes: Vec<_> = cached.iter().map(|(index, _)| *index).collect();
            let expected_indexes: Vec<_> = expected.iter().map(|(index, _)| *index).collect();
            assert_eq!(
                cached_indexes, expected_indexes,
                "cell attribution diverged at step {step}, width {width}"
            );
        }

        let widths = [20u16, 33, 61, 120];
        let mut state = HistoryState::new();
        // Deterministic LCG so a failure is reproducible.
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };

        const STEPS: usize = 240;
        for step in 0..STEPS {
            let width = widths[next() % widths.len()];
            match next() % 11 {
                0 => state.push_event(
                    AgentEvent::AssistantText(format!("assistant reply number {step}")),
                    width,
                ),
                1 => state.push_event(
                    AgentEvent::AssistantText(format!("\n\nmore prose for step {step}")),
                    width,
                ),
                2 => state.push_event(
                    AgentEvent::ToolCall {
                        id: format!("call-{step}"),
                        name: "bash".into(),
                        input: serde_json::json!({"command": format!("echo {step}")}),
                    },
                    width,
                ),
                3 => state.push_event(
                    AgentEvent::ToolResult {
                        id: format!("call-{}", step.saturating_sub(1)),
                        result: yi_agent_core::ToolResult::text(format!(
                            "output for the call made before step {step}"
                        )),
                    },
                    width,
                ),
                4 => state.push_event(
                    AgentEvent::Done {
                        reason: DoneReason::EndTurn,
                    },
                    width,
                ),
                5 => state.push(
                    HistoryCell::UserMessage {
                        text: format!("user question {step}"),
                    },
                    width,
                ),
                6 => state.push(HistoryCell::Separator { label: None }, width),
                7 => {
                    state.push_event(
                        AgentEvent::PermissionRequest {
                            request_id: step as u64,
                            tool_name: "bash".into(),
                            tool_input: serde_json::json!({"command": format!("rm -rf /tmp/{step}")}),
                            prefix_suggestion: None,
                            kind: yi_agent_core::permission::PermissionKind::Normal,
                        },
                        width,
                    );
                }
                8 => {
                    let _ = state.toggle_pending_permission_expanded();
                }
                9 => {
                    if !state.cells.is_empty() {
                        state.selected = Some(state.cells.len() - 1);
                        state.toggle_fold_selected();
                    }
                }
                _ => {
                    state.scroll_offset = next() % 25;
                    state.reconcile_scroll_offset(width, 7);
                }
            }

            // Every step is checked at every width, so a stale cache left behind
            // by one width is caught by the next.
            for probe in widths {
                compare(&state, probe, step);
            }
            let _ = state.capture_viewport_anchor(width, 7);
            let anchor = state
                .capture_viewport_anchor(width, 7)
                .unwrap_or(ViewportAnchor {
                    cell_index: 0,
                    position: AnchorPosition::ContentLine(0),
                });
            state.restore_viewport_anchor(anchor, width, 7);
        }
    }

    /// Instrumentation sanity check: without it, the two counter-based tests
    /// above could pass vacuously.
    #[test]
    fn render_counters_are_live() {
        let cell = HistoryCell::AssistantMessage {
            markdown: "counted".into(),
        };
        crate::tui::cell::reset_lines_call_count();
        let _ = cell.lines(40);
        assert_eq!(crate::tui::cell::lines_call_count(), 1);
    }

    /// The render layer asserts every line fits `text_width`, because ratatui
    /// clips (never wraps) an over-wide `Line` inside its one-row rect. Feed it
    /// one cell of every kind, including over-wide ones, and prove nothing is
    /// lost off the right edge.
    #[test]
    fn rendered_history_never_exceeds_terminal_width() {
        use unicode_width::UnicodeWidthStr;

        let cjk = "这是一条很长的中文内容需要按显示宽度折行否则右侧会被截断掉看不见";
        let mut state = HistoryState::new();
        let width = 40u16;
        state.push_event(AgentEvent::AssistantText("reply:\n\n".into()), width);
        state.push_event(
            AgentEvent::AssistantText(format!("```\n{cjk}{cjk}\n```\n")),
            width,
        );
        state.push_event(
            AgentEvent::ToolCall {
                id: "1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": cjk}),
            },
            width,
        );
        state.push_event(tool_result("1", false), width);
        state.push_event(
            AgentEvent::Error(yi_agent_core::AgentError::ProviderTurnAdmission(
                cjk.to_string(),
            )),
            width,
        );
        state.push(
            HistoryCell::Separator {
                label: Some(cjk.into()),
            },
            width,
        );
        state.push(HistoryCell::UserMessage { text: cjk.into() }, width);
        state.push(
            HistoryCell::AssistantMessage {
                markdown: format!("{cjk}\n\n{cjk}"),
            },
            width,
        );
        // Widen an expandable cell so the expanded path is covered too.
        state.push_event(
            AgentEvent::ToolResult {
                id: "2".into(),
                result: yi_agent_core::ToolResult::text(cjk),
            },
            width,
        );

        let (w, _) = (width, ());
        for (cell_index, cell) in state.cells.iter().enumerate() {
            let mut expanded = cell.clone();
            if expanded.is_foldable() {
                expanded.toggle_fold();
            }
            for (variant, cell) in [("folded", cell.clone()), ("toggled", expanded)] {
                for (i, line) in cell.lines(w).iter().enumerate() {
                    let text: String = line.spans.iter().map(|s| s.content.to_string()).collect();
                    assert!(
                        !text.contains('\n'),
                        "cell {cell_index} ({variant}) line {i} holds a raw newline"
                    );
                    let lw = UnicodeWidthStr::width(text.as_str());
                    assert!(
                        lw <= w as usize,
                        "cell {cell_index} ({variant}) line {i} is {lw} cols > {w}: {text:?}"
                    );
                }
            }
        }

        // And render for real: ratatui must never have to clip.
        let backend = TestBackend::new(width, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                frame.render_widget(
                    HistoryView {
                        state: &state,
                        width: area.width,
                    },
                    area,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text_width = state.text_width(width, 24);
        for y in 0..24u16 {
            for x in text_width..width {
                let sym = buffer[(x, y)].symbol();
                assert!(
                    sym == " " || sym == "█" || sym == "▲" || sym == "▼",
                    "column {x} (>= text_width {text_width}) row {y} holds {sym:?}:                      a line was right-clipped into the scrollbar column"
                );
            }
        }
    }

    #[test]
    fn viewport_anchor_keeps_same_cell_line_through_reflow() {
        let mut state = two_multiline_assistant_cells();
        state.scroll_offset = 2;

        let anchor = state
            .capture_viewport_anchor(20, 3)
            .expect("a non-bottom viewport should have an anchor");
        assert_eq!(
            anchor,
            ViewportAnchor {
                cell_index: 0,
                position: AnchorPosition::ContentLine(1),
            }
        );

        state.restore_viewport_anchor(anchor, 10, 4);

        assert_eq!(
            state.capture_viewport_anchor(10, 4),
            Some(ViewportAnchor {
                cell_index: 0,
                position: AnchorPosition::ContentLine(1),
            })
        );
    }

    #[test]
    fn viewport_anchor_is_none_at_bottom() {
        let state = two_multiline_assistant_cells();

        assert_eq!(state.capture_viewport_anchor(20, 3), None);
    }

    #[test]
    fn viewport_anchor_restores_user_spacer_after_reflow() {
        let old_width = 20;
        let new_width = 10;
        let viewport_height = 1;
        let mut state = HistoryState {
            cells: vec![
                HistoryCell::UserMessage {
                    text: "alpha bravo charlie delta echo foxtrot".into(),
                },
                HistoryCell::AssistantMessage {
                    markdown: "india juliet kilo lima mike november oscar papa".into(),
                },
            ],
            selected: None,
            scroll_offset: 0,
            ..HistoryState::new()
        };
        let old_user_lines = state.cells[0].line_count(old_width);
        state.scroll_offset = state
            .flattened_line_count(old_width)
            .saturating_sub(viewport_height as usize + old_user_lines);

        let anchor = state
            .capture_viewport_anchor(old_width, viewport_height)
            .expect("the spacer is above the bottom viewport");
        assert_eq!(
            anchor,
            ViewportAnchor {
                cell_index: 0,
                position: AnchorPosition::AfterCellSpacer,
            }
        );

        state.restore_viewport_anchor(anchor, new_width, viewport_height);

        let new_user_lines = state.cells[0].line_count(new_width);
        assert!(
            new_user_lines > old_user_lines,
            "the user message must wrap differently at the new width"
        );
        assert_eq!(
            state.scroll_offset,
            state
                .flattened_line_count(new_width)
                .saturating_sub(viewport_height as usize + new_user_lines),
            "the spacer, rather than a reflowed user-content line, remains at the top"
        );
    }

    #[test]
    fn viewport_anchor_clamps_shortened_content_to_last_line() {
        let old_width = 10;
        let new_width = 20;
        let viewport_height = 1;
        let mut state = two_multiline_assistant_cells();
        let old_line_count = state.cells[0].line_count(old_width);
        state.scroll_offset = state
            .flattened_line_count(old_width)
            .saturating_sub(viewport_height as usize + old_line_count - 1);

        let anchor = state
            .capture_viewport_anchor(old_width, viewport_height)
            .expect("the final old content line is above the bottom viewport");
        assert_eq!(
            anchor,
            ViewportAnchor {
                cell_index: 0,
                position: AnchorPosition::ContentLine(old_line_count - 1),
            }
        );

        state.restore_viewport_anchor(anchor, new_width, viewport_height);

        let new_line_count = state.cells[0].line_count(new_width);
        assert!(
            new_line_count < old_line_count,
            "the first cell must use fewer lines at the new width"
        );
        assert_eq!(
            state.scroll_offset,
            state
                .flattened_line_count(new_width)
                .saturating_sub(viewport_height as usize + new_line_count - 1),
            "a shortened cell should anchor to its final content line, not its spacer"
        );
    }

    #[test]
    fn push_preserves_position_when_scrolled_up() {
        let mut s = HistoryState::new();
        s.push(
            HistoryCell::UserMessage {
                text: "first".into(),
            },
            80,
        );
        s.scroll_offset = 2;
        s.push(
            HistoryCell::UserMessage {
                text: "second".into(),
            },
            80,
        );
        assert_eq!(s.scroll_offset, 4);
    }

    #[test]
    fn push_keeps_bottom_position_at_zero() {
        let mut s = HistoryState::new();
        s.push(
            HistoryCell::UserMessage {
                text: "first".into(),
            },
            80,
        );
        s.push(
            HistoryCell::UserMessage {
                text: "second".into(),
            },
            80,
        );
        assert_eq!(s.scroll_offset, 0);
    }

    #[test]
    fn push_preserves_position_by_rendered_line_delta() {
        let width = 10;
        let mut s = HistoryState::new();
        s.push(
            HistoryCell::UserMessage {
                text: "first".into(),
            },
            width,
        );
        s.scroll_offset = 2;
        let before = s.flattened_line_count(width);

        s.push(
            HistoryCell::UserMessage {
                text: "a long user message that wraps".into(),
            },
            width,
        );

        let added_lines = s.flattened_line_count(width) - before;
        assert!(
            added_lines > 1,
            "the wrapped message and spacer add multiple lines"
        );
        assert_eq!(s.scroll_offset, 2 + added_lines);
    }

    #[test]
    fn select_up_from_none_selects_second_to_last() {
        let mut s = HistoryState::new();
        s.push(HistoryCell::UserMessage { text: "a".into() }, 80);
        s.push(HistoryCell::UserMessage { text: "b".into() }, 80);
        s.push(HistoryCell::UserMessage { text: "c".into() }, 80);
        s.select_up();
        assert_eq!(s.selected, Some(1));
    }

    #[test]
    fn select_down_past_last_clears_selection() {
        let mut s = HistoryState::new();
        s.push(HistoryCell::UserMessage { text: "a".into() }, 80);
        s.selected = Some(0);
        s.select_down();
        assert_eq!(s.selected, None);
    }

    #[test]
    fn toggle_fold_selected_toggles() {
        let mut s = HistoryState::new();
        s.push(
            HistoryCell::ToolCall {
                id: "1".into(),
                name: "t".into(),
                input: serde_json::json!({}),
                state: crate::tui::cell::CallState::Success,
                expanded: false,
            },
            80,
        );
        s.selected = Some(0);
        s.toggle_fold_selected();
        match &s.cells[0] {
            HistoryCell::ToolCall { expanded, .. } => assert!(*expanded),
            _ => panic!("expected ToolCall"),
        }
    }

    #[test]
    fn total_lines_sums_all_cells() {
        let mut s = HistoryState::new();
        s.push(
            HistoryCell::UserMessage {
                text: "hello".into(),
            },
            80,
        );
        s.push(HistoryCell::Separator { label: None }, 80);
        assert_eq!(s.total_lines(80), 2);
    }

    use yi_agent_core::{AgentEvent, DoneReason, ToolResult};

    fn tool_result(id: &str, is_error: bool) -> AgentEvent {
        AgentEvent::ToolResult {
            id: id.into(),
            result: ToolResult {
                content: vec![yi_agent_core::ContentBlock::Text("result".into())],
                is_error,
            },
        }
    }

    #[test]
    fn push_event_assistant_text_appends_to_existing() {
        let mut s = HistoryState::new();
        s.push_event(AgentEvent::AssistantText("hello".into()), 80);
        s.push_event(AgentEvent::AssistantText(" world".into()), 80);
        assert_eq!(s.cells.len(), 1, "two text chunks should merge into 1 cell");
        match &s.cells[0] {
            HistoryCell::AssistantMessage { markdown, .. } => assert_eq!(*markdown, "hello world"),
            _ => panic!("expected AssistantMessage"),
        }
    }

    #[test]
    fn push_event_assistant_text_preserves_embedded_newlines() {
        let mut s = HistoryState::new();
        s.push_event(
            AgentEvent::AssistantText("first line\nsecond line".into()),
            80,
        );

        let rendered: Vec<String> = s.cells[0]
            .lines(80)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.to_string())
                    .collect()
            })
            .collect();
        assert_eq!(rendered, ["first line", "second line"]);
    }

    #[test]
    fn push_event_new_cell_preserves_non_bottom_reading_position() {
        let mut s = HistoryState::new();
        for _ in 0..5 {
            s.push(HistoryCell::Separator { label: None }, 80);
        }
        s.scroll_offset = 3;

        s.push_event(
            AgentEvent::Done {
                reason: DoneReason::EndTurn,
            },
            80,
        );

        assert_eq!(
            s.scroll_offset, 4,
            "a one-line new cell should leave the previously visible lines in place"
        );
    }

    #[test]
    fn push_event_new_content_at_bottom_keeps_offset_zero() {
        let mut s = HistoryState::new();

        s.push_event(
            AgentEvent::Done {
                reason: DoneReason::EndTurn,
            },
            80,
        );

        assert_eq!(s.scroll_offset, 0);
    }

    #[test]
    fn push_event_streaming_text_preserves_non_bottom_position_by_line_delta() {
        let width = 20;
        let mut s = HistoryState::new();
        s.push_event(AgentEvent::AssistantText("short".into()), width);
        s.scroll_offset = 2;
        let before = s.flattened_line_count(width);

        s.push_event(
            AgentEvent::AssistantText(" text that wraps onto multiple display lines".into()),
            width,
        );

        let added_lines = s.flattened_line_count(width).saturating_sub(before);
        assert!(
            added_lines > 0,
            "streaming text should have added display lines"
        );
        assert_eq!(s.scroll_offset, 2 + added_lines);
    }

    #[test]
    fn push_event_assistant_text_after_resize_uses_current_width_for_delta() {
        let wide_width = 80;
        let narrow_width = 20;
        let initial = "this assistant response wraps across several narrow display lines";
        let mut s = HistoryState::new();
        s.push_event(AgentEvent::AssistantText(initial.into()), wide_width);
        s.scroll_offset = 3;

        let before = s.flattened_line_count(narrow_width);
        let expected_before = HistoryCell::from_assistant_text(initial).line_count(narrow_width);
        assert_eq!(
            before, expected_before,
            "history must reflow after a resize"
        );

        s.push_event(
            AgentEvent::AssistantText(" with an additional narrow-width tail".into()),
            narrow_width,
        );

        let after = s.flattened_line_count(narrow_width);
        assert_eq!(s.scroll_offset, 3 + (after - before));
    }

    #[test]
    fn push_event_permission_resolved_reduces_offset_by_removed_lines() {
        let width = 80;
        let mut s = HistoryState::new();
        s.push_event(
            AgentEvent::PermissionRequest {
                request_id: 1,
                tool_name: "bash".into(),
                tool_input: serde_json::json!({}),
                prefix_suggestion: None,
                kind: yi_agent_core::permission::PermissionKind::Normal,
            },
            width,
        );
        s.scroll_offset = 4;
        let before = s.flattened_line_count(width);

        s.push_event(
            AgentEvent::PermissionResolved {
                request_id: 1,
                decision: yi_agent_core::permission::Decision::AllowOnce,
            },
            width,
        );

        let removed_lines = before - s.flattened_line_count(width);
        assert!(removed_lines > 0, "resolving the request compacts it");
        assert_eq!(s.scroll_offset, 4usize.saturating_sub(removed_lines));
    }

    #[test]
    fn push_event_tool_call_creates_separate_cell() {
        let mut s = HistoryState::new();
        s.push_event(AgentEvent::AssistantText("text".into()), 80);
        s.push_event(
            AgentEvent::ToolCall {
                id: "1".into(),
                name: "read".into(),
                input: serde_json::json!({}),
            },
            80,
        );
        assert_eq!(s.cells.len(), 2);
    }

    #[test]
    fn push_event_done_endturn_adds_separator() {
        let mut s = HistoryState::new();
        s.push_event(
            AgentEvent::Done {
                reason: DoneReason::EndTurn,
            },
            80,
        );
        assert_eq!(s.cells.len(), 1);
        assert!(matches!(s.cells[0], HistoryCell::Separator { .. }));
    }

    #[test]
    fn push_event_tool_result_updates_tool_call_state() {
        let mut s = HistoryState::new();
        s.push_event(
            AgentEvent::ToolCall {
                id: "1".into(),
                name: "read".into(),
                input: serde_json::json!({}),
            },
            80,
        );
        s.push_event(
            AgentEvent::ToolResult {
                id: "1".into(),
                result: ToolResult {
                    content: vec![yi_agent_core::ContentBlock::Text("ok".into())],
                    is_error: false,
                },
            },
            80,
        );
        assert!(matches!(
            &s.cells[0],
            HistoryCell::ToolCall {
                state: crate::tui::cell::CallState::Success,
                ..
            }
        ));
        assert!(matches!(
            s.cells.get(1),
            Some(HistoryCell::ToolResult { .. })
        ));
    }

    #[test]
    fn push_event_permission_request_creates_cell() {
        let mut s = HistoryState::new();
        s.push_event(
            AgentEvent::PermissionRequest {
                request_id: 1,
                tool_name: "bash".into(),
                tool_input: serde_json::json!({"command": "ls"}),
                prefix_suggestion: Some("ls".into()),
                kind: yi_agent_core::permission::PermissionKind::Normal,
            },
            80,
        );
        assert_eq!(s.cells.len(), 1);
        assert!(matches!(s.cells[0], HistoryCell::PermissionRequest { .. }));
    }

    #[test]
    fn push_event_permission_resolved_marks_request() {
        let mut s = HistoryState::new();
        s.push_event(
            AgentEvent::PermissionRequest {
                request_id: 1,
                tool_name: "bash".into(),
                tool_input: serde_json::json!({}),
                prefix_suggestion: None,
                kind: yi_agent_core::permission::PermissionKind::Normal,
            },
            80,
        );
        s.push_event(
            AgentEvent::PermissionResolved {
                request_id: 1,
                decision: yi_agent_core::permission::Decision::AllowOnce,
            },
            80,
        );
        match &s.cells[0] {
            HistoryCell::PermissionRequest { resolved, .. } => assert!(*resolved),
            _ => panic!("expected PermissionRequest"),
        }
    }

    #[test]
    fn pending_permission_info_returns_unresolved() {
        let mut s = HistoryState::new();
        s.push_event(
            AgentEvent::PermissionRequest {
                request_id: 5,
                tool_name: "bash".into(),
                tool_input: serde_json::json!({}),
                prefix_suggestion: Some("git".into()),
                kind: yi_agent_core::permission::PermissionKind::Normal,
            },
            80,
        );
        let info = s.pending_permission_info();
        assert!(info.is_some());
        let (id, name, prefix, _) = info.unwrap();
        assert_eq!(id, 5);
        assert_eq!(name, "bash");
        assert_eq!(prefix, Some("git"));
    }

    #[test]
    fn pending_permission_info_none_when_resolved() {
        let mut s = HistoryState::new();
        s.push_event(
            AgentEvent::PermissionRequest {
                request_id: 1,
                tool_name: "bash".into(),
                tool_input: serde_json::json!({}),
                prefix_suggestion: None,
                kind: yi_agent_core::permission::PermissionKind::Normal,
            },
            80,
        );
        s.push_event(
            AgentEvent::PermissionResolved {
                request_id: 1,
                decision: yi_agent_core::permission::Decision::AllowOnce,
            },
            80,
        );
        assert!(s.pending_permission_info().is_none());
    }

    #[test]
    fn flattened_lines_inserts_spacer_after_user_message() {
        let mut s = HistoryState::new();
        s.push(
            HistoryCell::UserMessage {
                text: "hello".into(),
            },
            80,
        );
        s.push_event(AgentEvent::AssistantText("hi there".into()), 80);

        let view = HistoryView {
            state: &s,
            width: 80,
        };
        let lines = view.flattened_lines(80);

        // UserMessage = 1 line, spacer = 1 line, AssistantMessage >= 1 line
        assert!(
            lines.len() >= 3,
            "expected spacer between user and assistant, got {} lines",
            lines.len()
        );

        // The spacer line should be the second line (index 1), belonging to
        // cell index 0 (the UserMessage).
        assert_eq!(lines[1].0, 0, "spacer should be attributed to user cell");
        assert!(lines[1].1.spans.is_empty(), "spacer line should be empty");
    }

    #[test]
    fn flattened_lines_inserts_spacer_before_assistant_after_tool_result() {
        let mut state = HistoryState::new();
        state.push_event(AgentEvent::AssistantText("checking".into()), 80);
        state.push_event(
            AgentEvent::ToolCall {
                id: "1".into(),
                name: "read".into(),
                input: serde_json::json!({}),
            },
            80,
        );
        state.push_event(tool_result("1", false), 80);
        state.push_event(AgentEvent::AssistantText("done".into()), 80);

        let view = HistoryView {
            state: &state,
            width: 80,
        };
        let lines = view.flattened_lines(80);
        let result_index = state
            .cells
            .iter()
            .position(|cell| matches!(cell, HistoryCell::ToolResult { .. }))
            .expect("tool result cell");
        let tool_call_index = state
            .cells
            .iter()
            .position(|cell| matches!(cell, HistoryCell::ToolCall { .. }))
            .expect("tool call cell");
        let final_response_index = state.cells.len() - 1;
        let tool_call_line = lines
            .iter()
            .position(|(cell_index, _)| *cell_index == tool_call_index)
            .expect("tool call line");
        let result_line = lines
            .iter()
            .position(|(cell_index, _)| *cell_index == result_index)
            .expect("tool result line");

        assert_eq!(lines[tool_call_line + 1].0, result_index);
        assert_eq!(lines[result_line + 1].0, result_index);
        assert!(
            lines[result_line + 1].1.spans.is_empty(),
            "tool result should own one blank spacer before the resumed assistant response"
        );
        assert_eq!(lines[result_line + 2].0, final_response_index);
    }

    #[test]
    fn assistant_chunks_after_tool_result_share_one_spacer() {
        let mut state = HistoryState::new();
        state.push_event(
            AgentEvent::ToolCall {
                id: "1".into(),
                name: "read".into(),
                input: serde_json::json!({}),
            },
            80,
        );
        state.push_event(tool_result("1", false), 80);
        state.push_event(AgentEvent::AssistantText("done".into()), 80);
        state.push_event(AgentEvent::AssistantText(" again".into()), 80);

        let view = HistoryView {
            state: &state,
            width: 80,
        };
        let spacer_count = view
            .flattened_lines(80)
            .iter()
            .filter(|(cell_index, line)| {
                line.spans.is_empty()
                    && matches!(state.cells[*cell_index], HistoryCell::ToolResult { .. })
            })
            .count();

        assert_eq!(spacer_count, 1);
        assert!(matches!(
            state.cells.last(),
            Some(HistoryCell::AssistantMessage { markdown }) if markdown == "done again"
        ));
    }

    #[test]
    fn consecutive_tool_results_create_one_spacer_before_response() {
        let mut state = HistoryState::new();
        for id in ["1", "2"] {
            state.push_event(
                AgentEvent::ToolCall {
                    id: id.into(),
                    name: "read".into(),
                    input: serde_json::json!({}),
                },
                80,
            );
            state.push_event(tool_result(id, false), 80);
        }
        state.push_event(AgentEvent::AssistantText("summary".into()), 80);

        let view = HistoryView {
            state: &state,
            width: 80,
        };
        let spacer_count = view
            .flattened_lines(80)
            .iter()
            .filter(|(cell_index, line)| {
                line.spans.is_empty()
                    && matches!(state.cells[*cell_index], HistoryCell::ToolResult { .. })
            })
            .count();

        assert_eq!(spacer_count, 1);
    }

    #[test]
    fn failed_tool_result_still_separates_follow_up_assistant_text() {
        let mut state = HistoryState::new();
        state.push_event(
            AgentEvent::ToolCall {
                id: "1".into(),
                name: "read".into(),
                input: serde_json::json!({}),
            },
            80,
        );
        state.push_event(tool_result("1", true), 80);
        state.push_event(AgentEvent::AssistantText("fallback".into()), 80);

        let view = HistoryView {
            state: &state,
            width: 80,
        };
        let lines = view.flattened_lines(80);
        let result_index = state
            .cells
            .iter()
            .position(|cell| matches!(cell, HistoryCell::ToolResult { is_error: true, .. }))
            .expect("failed tool result cell");
        let result_line = lines
            .iter()
            .position(|(cell_index, _)| *cell_index == result_index)
            .expect("failed tool result line");

        assert_eq!(lines[result_line + 1].0, result_index);
        assert!(lines[result_line + 1].1.spans.is_empty());
    }

    #[test]
    fn flattened_lines_no_spacer_after_last_cell() {
        let mut s = HistoryState::new();
        s.push(
            HistoryCell::UserMessage {
                text: "orphan message".into(),
            },
            80,
        );

        let view = HistoryView {
            state: &s,
            width: 80,
        };
        let lines = view.flattened_lines(80);

        // Only the user message line, no trailing spacer.
        assert_eq!(
            lines.len(),
            1,
            "no spacer after last cell, got {} lines",
            lines.len()
        );
    }

    #[test]
    fn flattened_lines_no_spacer_between_assistant_cells() {
        let mut s = HistoryState::new();
        s.push_event(AgentEvent::AssistantText("part1".into()), 80);
        // Force a new AssistantMessage cell by inserting a Separator first.
        s.push(HistoryCell::Separator { label: None }, 80);
        s.push_event(AgentEvent::AssistantText("part2".into()), 80);

        let view = HistoryView {
            state: &s,
            width: 80,
        };
        let lines = view.flattened_lines(80);

        // No spacer should be inserted between non-UserMessage cells.
        // Count empty lines attributed to non-user cells — should be zero.
        let spacers = lines
            .iter()
            .filter(|(idx, l)| {
                l.spans.is_empty()
                    && !matches!(s.cells.get(*idx), Some(HistoryCell::UserMessage { .. }))
            })
            .count();
        assert_eq!(spacers, 0, "no spacers between assistant/separator cells");
    }

    #[test]
    fn render_over_scrolled_fills_viewport_without_gaps() {
        // Regression: when scroll_offset exceeds total - visible_height, the
        // render slice shrank below visible_height, leaving stale blank rows
        // at the bottom of the history area.
        let mut s = HistoryState::new();
        // 5 separator lines (no spacers inserted between non-UserMessage
        // cells), viewport height 3 → max useful offset is 2.
        for c in ['a', 'b', 'c', 'd', 'e'] {
            s.push(
                HistoryCell::Separator {
                    label: Some(c.to_string()),
                },
                80,
            );
        }
        // Over-scroll past the maximum.
        s.scroll_offset = 10;

        let view = HistoryView {
            state: &s,
            width: 80,
        };
        let area = Rect::new(0, 0, 80, 3);
        let mut buf = Buffer::empty(area);
        view.render(area, &mut buf);

        // Every row in the viewport should have been written by the render
        // (i.e. not left as the default empty ' ' cell with default style).
        // With over-scroll clamped to 2, rows 0..3 should show the top of
        // the content (separators "a", "b", "c"), not blanks.
        for y in 0..3u16 {
            let cell = &buf[(0, y)];
            let sym = cell.symbol();
            assert!(
                !sym.is_empty() && sym != " ",
                "row {y} should not be blank, got {sym:?}"
            );
        }
        // Sanity: the top row should contain the label 'a'.
        let top: String = (0..80u16).map(|x| buf[(x, 0)].symbol()).collect();
        assert!(top.contains('a'), "top row should show label 'a': {top:?}");
    }

    #[test]
    fn render_overflow_reserves_rightmost_column_for_scrollbar() {
        let mut state = HistoryState::new();
        for row in 0..6 {
            state.push(
                HistoryCell::UserMessage {
                    text: format!("row {row}"),
                },
                20,
            );
        }

        let backend = TestBackend::new(20, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                frame.render_widget(
                    HistoryView {
                        state: &state,
                        width: area.width,
                    },
                    area,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        assert!(
            (0..5).any(|y| buffer[(19, y)].symbol() == "█"),
            "an overflowing history should render a scrollbar thumb: {buffer:?}"
        );
        for y in 0..5 {
            let symbol = buffer[(19, y)].symbol();
            assert!(
                matches!(symbol, " " | "█" | "▲" | "▼"),
                "history text must not use the scrollbar column at row {y}: {symbol:?}"
            );
        }
    }

    #[test]
    fn wide_markdown_table_is_not_clipped_off_the_right_edge() {
        // Regression: a table wider than the terminal used to be rendered as
        // one over-wide Line per row, which ratatui truncates on the right, so
        // the closing border and trailing cells were silently lost.
        let mut state = HistoryState::new();
        state.push(
            HistoryCell::Markdown {
                text: "| Name | Description | Owner |\n| --- | --- | --- |\n\
                       | alpha-service | Handles all inbound user authentication | platform-team |\n"
                    .to_string(),
            },
            40,
        );

        let area_width = 40u16;
        let area_height = 20u16;
        let backend = TestBackend::new(area_width, area_height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                frame.render_widget(
                    HistoryView {
                        state: &state,
                        width: area.width,
                    },
                    area,
                );
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let text_width = state.text_width(area_width, area_height);
        let rows: Vec<String> = (0..area_height)
            .map(|y| (0..text_width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        let rendered = rows.join("\n");

        // The top-left corner proves the table reached the screen at all.
        assert!(rendered.contains('┌'), "table never rendered: {rendered:?}");
        // The closing border must be visible: this is what clipping destroyed.
        assert!(
            rows.iter().any(|row| row.ends_with('┐')),
            "the table's right border was clipped off: {rendered:?}"
        );
        assert!(
            rendered.contains('┘'),
            "the bottom-right corner was clipped off: {rendered:?}"
        );
        // Every rendered row must end with a box border or be blank padding.
        for (y, row) in rows.iter().enumerate() {
            assert!(
                row.trim_end().is_empty() || row.ends_with(['│', '┐', '┤', '┘']),
                "row {y} lost its right border: {row:?}"
            );
        }
        assert!(
            rendered.contains("Description") || rendered.contains("Descriptio"),
            "the wide column content was lost: {rendered:?}"
        );
    }

    #[test]
    fn scrollbar_uses_top_origin_while_history_offset_uses_bottom_origin() {
        let mut state = HistoryState::new();
        for row in 0..10 {
            state.push(
                HistoryCell::UserMessage {
                    text: format!("row {row}"),
                },
                20,
            );
        }

        let thumb_rows = |state: &HistoryState| {
            let backend = TestBackend::new(20, 5);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| {
                    let area = frame.area();
                    frame.render_widget(
                        HistoryView {
                            state,
                            width: area.width,
                        },
                        area,
                    );
                })
                .unwrap();
            (0..5u16)
                .filter(|&y| terminal.backend().buffer()[(19, y)].symbol() == "█")
                .collect::<Vec<_>>()
        };

        let bottom_rows = thumb_rows(&state);
        assert_eq!(
            bottom_rows.last(),
            Some(&3),
            "offset zero puts thumb at bottom"
        );

        let text_width = state.text_width(20, 5);
        state.scroll_offset = state.max_scroll_offset(text_width, 5);
        let top_rows = thumb_rows(&state);
        assert_eq!(
            top_rows.first(),
            Some(&1),
            "larger offsets move thumb upward"
        );
    }

    #[test]
    fn text_width_only_reserves_a_column_when_it_can_show_a_scrollbar() {
        let mut state = HistoryState::new();
        state.push(
            HistoryCell::UserMessage {
                text: "one line".into(),
            },
            20,
        );
        assert_eq!(
            state.text_width(20, 5),
            20,
            "fitting content keeps full width"
        );

        for row in 0..5 {
            state.push(
                HistoryCell::UserMessage {
                    text: format!("row {row}"),
                },
                20,
            );
        }
        assert_eq!(state.text_width(20, 5), 19, "overflow reserves one column");
        assert_eq!(
            state.text_width(1, 5),
            1,
            "one-column areas cannot reserve a scrollbar"
        );
        assert_eq!(
            state.text_width(0, 5),
            0,
            "zero-width areas stay zero-width"
        );
    }

    #[test]
    fn streaming_uses_reserved_scrollbar_width_to_preserve_scrolled_position() {
        let area_width = 20;
        let viewport_height = 3;
        let mut state = HistoryState::new();
        for _ in 0..4 {
            state.push(HistoryCell::Separator { label: None }, area_width);
        }
        let text_width = state.text_width(area_width, viewport_height);
        assert_eq!(text_width, 19, "overflow reserves the scrollbar column");

        state.push_event(
            AgentEvent::AssistantText("123456789012345678".into()),
            text_width,
        );
        state.scroll_offset = 2;
        let before = state.flattened_line_count(text_width);

        state.push_event(
            AgentEvent::AssistantText(" wrapping stream content".into()),
            text_width,
        );

        let after = state.flattened_line_count(text_width);
        assert!(
            after > before,
            "the streamed text should wrap at the reserved width"
        );
        assert_eq!(state.scroll_offset, 2 + (after - before));
    }

    #[test]
    fn render_handles_zero_and_one_column_areas() {
        let state = HistoryState {
            cells: vec![HistoryCell::UserMessage {
                text: "history that would overflow a narrow area".into(),
            }],
            selected: None,
            scroll_offset: 0,
            ..HistoryState::new()
        };

        for area in [Rect::new(0, 0, 0, 5), Rect::new(0, 0, 1, 5)] {
            let mut buffer = Buffer::empty(area);
            HistoryView {
                state: &state,
                width: area.width,
            }
            .render(area, &mut buffer);
        }
    }

    #[test]
    fn scroll_up_clamps_at_max_offset() {
        // 5 content lines (no spacers since these are not UserMessages),
        // viewport height 3 → max offset = 2.
        let mut s = HistoryState::new();
        for c in ['a', 'b', 'c', 'd', 'e'] {
            s.push(
                HistoryCell::Separator {
                    label: Some(c.to_string()),
                },
                80,
            );
        }
        // total = 5, visible_height = 3 → max = 2
        let max = s.max_scroll_offset(80, 3);
        assert_eq!(max, 2, "max should be total - visible_height = 5 - 3");

        // Scrolling up by 100 should clamp to 2, not 100.
        s.scroll_up(100, max);
        assert_eq!(s.scroll_offset, 2, "scroll_up should clamp at max");

        // Further scroll_up stays at max.
        s.scroll_up(10, max);
        assert_eq!(s.scroll_offset, 2, "clamped offset should not grow");
    }

    #[test]
    fn scroll_up_zero_max_keeps_offset_zero() {
        // Content shorter than viewport → max offset = 0, scrolling does nothing.
        let mut s = HistoryState::new();
        s.push(HistoryCell::UserMessage { text: "x".into() }, 80);
        let max = s.max_scroll_offset(80, 10);
        assert_eq!(max, 0, "max should be 0 when content < viewport");
        s.scroll_up(5, max);
        assert_eq!(s.scroll_offset, 0, "offset should stay 0");
    }

    #[test]
    fn page_scrolling_moves_by_viewport_height() {
        let mut s = HistoryState::new();

        s.scroll_up(12, 100);
        s.scroll_page_down(5);
        assert_eq!(s.scroll_offset, 7);

        s.scroll_page_up(5, 100);
        assert_eq!(s.scroll_offset, 12);
    }

    #[test]
    fn scroll_to_top_clamps_to_content_and_bottom_resets_offset() {
        let width = 80;
        let height = 1;
        let mut s = HistoryState::new();
        s.push(
            HistoryCell::UserMessage {
                text: "first".into(),
            },
            width,
        );
        s.push(
            HistoryCell::UserMessage {
                text: "second".into(),
            },
            width,
        );

        let max = s.max_scroll_offset(width, height);
        assert!(max > 0, "two user messages include a spacer line");

        s.scroll_to_top(width, height);
        assert_eq!(s.scroll_offset, max);

        s.scroll_to_bottom();
        assert_eq!(s.scroll_offset, 0);
    }

    #[test]
    fn reconcile_scroll_offset_keeps_bottom_after_resize_then_append() {
        let width = 80;
        let mut s = HistoryState::new();
        s.push(HistoryCell::Separator { label: None }, width);
        s.scroll_offset = 1;

        s.reconcile_scroll_offset(width, 10);
        s.push(HistoryCell::Separator { label: None }, width);

        assert_eq!(s.scroll_offset, 0);
    }

    #[test]
    fn manual_compaction_success_replaces_pending_status() {
        let mut history = HistoryState::new();
        history.push(
            HistoryCell::Separator {
                label: Some("正在压缩对话...".into()),
            },
            80,
        );

        history.push_event(
            AgentEvent::ManualCompacted {
                old_msg_count: 12,
                new_msg_count: 5,
            },
            80,
        );

        assert_eq!(history.cells.len(), 1);
        assert!(matches!(
            &history.cells[0],
            HistoryCell::Separator { label: Some(label) }
                if label == "压缩完成（12 → 5 条消息）"
        ));
    }

    #[test]
    fn push_event_provider_retry_shows_visible_separator() {
        let mut s = HistoryState::new();
        s.push_event(AgentEvent::AssistantText("partial text".into()), 80);

        s.push_event(
            AgentEvent::ProviderRetry {
                attempt: 1,
                max: 3,
                idle_secs: 60,
                cause: RetryCause::IdleStall,
            },
            80,
        );

        // The retry must be visible, not silent.
        assert_eq!(s.cells.len(), 2, "partial + retry separator");
        assert!(matches!(
            &s.cells[1],
            HistoryCell::Separator { label: Some(label) }
                if label.contains("retrying 1/3") && label.contains("60s")
        ));
        // The already-streamed partial is preserved for the user.
        assert!(matches!(
            &s.cells[0],
            HistoryCell::AssistantMessage { markdown } if markdown == "partial text"
        ));
    }

    #[test]
    fn push_event_provider_retry_labels_a_timeout_distinctly() {
        let mut s = HistoryState::new();

        s.push_event(
            AgentEvent::ProviderRetry {
                attempt: 1,
                max: 3,
                idle_secs: 0,
                cause: RetryCause::RequestTimeout,
            },
            80,
        );

        // A timeout must not be described as a stall: the wording tells the
        // user which failure mode they hit.
        assert!(matches!(
            &s.cells[0],
            HistoryCell::Separator { label: Some(label) }
                if label.contains("timed out") && label.contains("retrying 1/3")
        ));
    }

    #[test]
    fn push_event_provider_retry_keeps_history_before_it() {
        let mut s = HistoryState::new();
        s.push_event(AgentEvent::AssistantText("done earlier".into()), 80);
        s.push_event(
            AgentEvent::ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
            },
            80,
        );
        let before = s.cells.len();

        s.push_event(
            AgentEvent::ProviderRetry {
                attempt: 2,
                max: 3,
                idle_secs: 60,
                cause: RetryCause::IdleStall,
            },
            80,
        );

        assert_eq!(s.cells.len(), before + 1, "only the separator is appended");
        assert!(matches!(s.cells[0], HistoryCell::AssistantMessage { .. }));
        assert!(matches!(s.cells[1], HistoryCell::ToolCall { .. }));
    }

    #[test]
    fn manual_compaction_failure_replaces_pending_status() {
        let mut history = HistoryState::new();
        history.push(
            HistoryCell::Separator {
                label: Some("正在压缩对话...".into()),
            },
            80,
        );

        history.push_event(
            AgentEvent::ManualCompactFailed {
                message: "provider unavailable".into(),
            },
            80,
        );

        assert!(matches!(
            &history.cells[0],
            HistoryCell::Separator { label: Some(label) }
                if label == "压缩失败：provider unavailable"
        ));
    }

    /// The reclaimed children belong in the transcript, with the count, and
    /// through the same `push_event` path as every other notice. The daemon used
    /// to announce this itself with a raw write, which smeared the input box
    /// while the TUI was painting; the event is now the only channel.
    #[test]
    fn orphaned_reclaim_is_rendered_as_a_transcript_notice() {
        let mut history = HistoryState::new();

        history.push_event(AgentEvent::OrphanedTasksReclaimed { count: 2 }, 80);

        assert!(matches!(
            &history.cells[0],
            HistoryCell::Separator { label: Some(label) }
                if label == "runtime 已回收 2 个孤儿任务（所属 root 已退出）"
        ));
    }

    #[test]
    fn auto_compaction_appends_completed_status() {
        let mut history = HistoryState::new();

        history.push_event(
            AgentEvent::AutoCompacting {
                old_msg_count: 10,
                new_msg_count: 4,
            },
            80,
        );

        assert!(matches!(
            &history.cells[0],
            HistoryCell::Separator { label: Some(label) }
                if label == "已自动压缩（10 → 4 条消息）"
        ));
    }
    #[test]
    fn permission_summary_for_bash_drops_json_noise() {
        let mut s = HistoryState::new();
        s.push_event(
            AgentEvent::PermissionRequest {
                request_id: 1,
                tool_name: "bash".into(),
                tool_input: serde_json::json!({
                    "command": "cargo test",
                    "expected_timeout_sec": 120
                }),
                prefix_suggestion: Some("cargo".into()),
                kind: yi_agent_core::permission::PermissionKind::Normal,
            },
            80,
        );
        match &s.cells[0] {
            HistoryCell::PermissionRequest { summary, full, .. } => {
                assert_eq!(summary, "cargo test");
                assert!(!summary.contains("expected_timeout_sec"));
                assert!(
                    full.contains("expected_timeout_sec"),
                    "full keeps everything"
                );
            }
            _ => panic!("expected PermissionRequest"),
        }
    }

    #[test]
    fn permission_summary_for_write_hides_content_blob() {
        let mut s = HistoryState::new();
        let content = "x".repeat(500);
        s.push_event(
            AgentEvent::PermissionRequest {
                request_id: 1,
                tool_name: "write".into(),
                tool_input: serde_json::json!({ "path": "src/main.rs", "content": content }),
                prefix_suggestion: None,
                kind: yi_agent_core::permission::PermissionKind::Normal,
            },
            80,
        );
        match &s.cells[0] {
            HistoryCell::PermissionRequest { summary, .. } => {
                assert!(summary.contains("src/main.rs"), "summary: {summary}");
                assert!(summary.contains("500 bytes"), "summary: {summary}");
                assert!(
                    !summary.contains(&"x".repeat(50)),
                    "summary must not inline the blob: {summary}"
                );
            }
            _ => panic!("expected PermissionRequest"),
        }
    }

    #[test]
    fn permission_summary_for_edit_reports_both_field_sizes() {
        let mut s = HistoryState::new();
        s.push_event(
            AgentEvent::PermissionRequest {
                request_id: 1,
                tool_name: "edit".into(),
                tool_input: serde_json::json!({
                    "path": "a.rs", "old_string": "aa", "new_string": "bbbb"
                }),
                prefix_suggestion: None,
                kind: yi_agent_core::permission::PermissionKind::Normal,
            },
            80,
        );
        match &s.cells[0] {
            HistoryCell::PermissionRequest { summary, .. } => {
                assert!(summary.contains("a.rs"), "summary: {summary}");
                assert!(
                    summary.contains("old_string: 2 bytes"),
                    "summary: {summary}"
                );
                assert!(
                    summary.contains("new_string: 4 bytes"),
                    "summary: {summary}"
                );
            }
            _ => panic!("expected PermissionRequest"),
        }
    }

    #[test]
    fn toggle_pending_permission_expanded_toggles_only_pending() {
        let mut s = HistoryState::new();
        s.push_event(
            AgentEvent::PermissionRequest {
                request_id: 1,
                tool_name: "bash".into(),
                tool_input: serde_json::json!({"command": "ls"}),
                prefix_suggestion: None,
                kind: yi_agent_core::permission::PermissionKind::Normal,
            },
            80,
        );
        assert!(s.toggle_pending_permission_expanded());
        match &s.cells[0] {
            HistoryCell::PermissionRequest { expanded, .. } => assert!(*expanded),
            _ => panic!("expected PermissionRequest"),
        }
        assert!(s.toggle_pending_permission_expanded());
        match &s.cells[0] {
            HistoryCell::PermissionRequest { expanded, .. } => assert!(!*expanded),
            _ => panic!("expected PermissionRequest"),
        }
    }

    #[test]
    fn toggle_pending_permission_expanded_returns_false_when_none() {
        let mut s = HistoryState::new();
        assert!(!s.toggle_pending_permission_expanded());
    }

    /// 黑名单拒绝的事件序列必须渲染出一次失败的工具调用,且带拒绝原因。
    #[test]
    fn blacklist_deny_is_visible_as_failed_tool_call() {
        let mut s = HistoryState::new();
        s.push_event(
            AgentEvent::ToolCall {
                id: "deny-1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "blocked-cmd"}),
            },
            80,
        );
        s.push_event(
            AgentEvent::ToolResult {
                id: "deny-1".into(),
                result: ToolResult {
                    content: vec![yi_agent_core::ContentBlock::Text(
                        "blocked by safety filter: test rule".into(),
                    )],
                    is_error: true,
                },
            },
            80,
        );

        // 调用单元存在且被标记为失败。
        let call = s
            .cells
            .iter()
            .find_map(|c| match c {
                HistoryCell::ToolCall { id, state, .. } if id == "deny-1" => Some(state),
                _ => None,
            })
            .expect("ToolCall cell must exist so the refusal is visible");
        assert_eq!(*call, crate::tui::cell::CallState::Failed);

        // 拒绝原因出现在渲染输出中。
        let rendered: Vec<String> = s
            .cells
            .iter()
            .flat_map(|c| c.lines(80))
            .map(|l| l.to_string())
            .collect();
        assert!(
            rendered
                .iter()
                .any(|l| l.contains("blocked by safety filter: test rule")),
            "deny reason must be rendered; got: {rendered:?}"
        );
    }
}
