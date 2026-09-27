use pulldown_cmark::{Alignment, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::wrap::wrap_by_display_width;

#[cfg(test)]
use std::cell::Cell;

#[cfg(test)]
thread_local! {
    static LIST_CONTEXT_LINE_INSPECTIONS: Cell<usize> = const { Cell::new(0) };
}

/// Render a markdown string into ratatui Lines, wrapped at `width`.
pub fn render_markdown(src: &str, width: u16) -> Vec<Line<'static>> {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_MATH;
    let normalized = normalize_math_delimiters(src);
    let parser = Parser::new_ext(&normalized, opts);
    let mut builder = LineBuilder::new(width);
    for event in parser {
        builder.handle_event(event);
    }
    builder.finish()
}

/// Convert TeX's `\\(...\\)` and `\\[...\\]` delimiters to pulldown-cmark's
/// dollar syntax, without changing Markdown code spans or fenced code blocks.
fn normalize_math_delimiters(src: &str) -> String {
    let mut normalized = String::with_capacity(src.len());
    let mut plain_start = 0;
    let mut index = 0;
    let list_parents = list_parent_contexts(src);

    while index < src.len() {
        if let Some(fence_end) = fenced_code_end(src, index, list_parents[index]) {
            normalized.push_str(&normalize_math_text(&src[plain_start..index]));
            normalized.push_str(&src[index..fence_end]);
            index = fence_end;
            plain_start = index;
        } else if !is_backslash_escaped(src, index) && src[index..].starts_with('`') {
            let ticks = src[index..].bytes().take_while(|ch| *ch == b'`').count();
            if let Some(close) = closing_backticks(src, index + ticks, ticks) {
                normalized.push_str(&normalize_math_text(&src[plain_start..index]));
                normalized.push_str(&src[index..close + ticks]);
                index = close + ticks;
                plain_start = index;
            } else {
                index += ticks;
            }
        } else if let Some(protected_end) = markdown_protected_end(src, index) {
            normalized.push_str(&normalize_math_text(&src[plain_start..index]));
            normalized.push_str(&src[index..protected_end]);
            index = protected_end;
            plain_start = index;
        } else {
            index += src[index..].chars().next().expect("valid UTF-8").len_utf8();
        }
    }

    normalized.push_str(&normalize_math_text(&src[plain_start..]));
    normalized
}

fn markdown_protected_end(src: &str, index: usize) -> Option<usize> {
    if src[index..].starts_with("](") {
        return link_destination_end(src, index + 2);
    }
    if src[index..].starts_with('<') {
        return angle_bracket_end(src, index);
    }
    None
}

fn link_destination_end(src: &str, mut index: usize) -> Option<usize> {
    let mut depth = 1;
    let mut quote = None;

    while index < src.len() {
        let ch = src[index..].chars().next()?;
        if is_backslash_escaped(src, index) {
            index += ch.len_utf8();
            continue;
        }
        if let Some(quote_char) = quote {
            if ch == quote_char {
                quote = None;
            }
        } else {
            match ch {
                '\'' | '"' => quote = Some(ch),
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(index + 1);
                    }
                }
                _ => {}
            }
        }
        index += ch.len_utf8();
    }
    None
}

fn angle_bracket_end(src: &str, index: usize) -> Option<usize> {
    let next = *src.as_bytes().get(index + 1)?;
    if !(next.is_ascii_alphabetic() || matches!(next, b'/' | b'!' | b'?')) {
        return None;
    }

    let mut quote = None;
    let mut cursor = index + 1;
    while cursor < src.len() {
        let ch = src[cursor..].chars().next()?;
        if is_backslash_escaped(src, cursor) {
            cursor += ch.len_utf8();
            continue;
        }
        if let Some(quote_char) = quote {
            if ch == quote_char {
                quote = None;
            }
        } else {
            match ch {
                '\'' | '"' => quote = Some(ch),
                '>' => return Some(cursor + 1),
                '\n' => return None,
                _ => {}
            }
        }
        cursor += ch.len_utf8();
    }
    None
}

fn is_backslash_escaped(src: &str, index: usize) -> bool {
    src[..index]
        .bytes()
        .rev()
        .take_while(|ch| *ch == b'\\')
        .count()
        % 2
        == 1
}

fn normalize_math_text(text: &str) -> String {
    let delimiters = collect_tex_delimiters(text);
    let mut matching_closes = vec![None; text.len()];
    let mut delimiter_starts = vec![false; text.len()];
    let mut next_parenthesis_close = None;
    let mut next_bracket_close = None;

    // A reverse pass records the next valid closer for every opener, so an
    // unmatched opener cannot repeatedly scan the rest of the input.
    for delimiter in delimiters.iter().rev() {
        delimiter_starts[delimiter.index] = true;
        match delimiter.kind {
            TexDelimiterKind::CloseParenthesis => next_parenthesis_close = Some(delimiter.index),
            TexDelimiterKind::CloseBracket => next_bracket_close = Some(delimiter.index),
            TexDelimiterKind::OpenParenthesis => {
                matching_closes[delimiter.index] = next_parenthesis_close;
            }
            TexDelimiterKind::OpenBracket => {
                matching_closes[delimiter.index] = next_bracket_close;
            }
        }
    }

    let mut normalized = String::with_capacity(text.len());
    let mut index = 0;

    while index < text.len() {
        if let Some(close) = matching_closes[index] {
            let display = text[index..].starts_with(r"\[");
            let dollar = if display { "$$" } else { "$" };
            normalized.push_str(dollar);
            normalized.push_str(&text[index + 2..close]);
            normalized.push_str(dollar);
            index = close + 2;
            continue;
        }

        // Markdown consumes the slash of an unmatched TeX delimiter. Double it
        // so malformed or escaped math stays visible to the user.
        if delimiter_starts[index] {
            normalized.push('\\');
        }

        let ch = text[index..].chars().next().expect("valid UTF-8");
        normalized.push(ch);
        index += ch.len_utf8();
    }

    normalized
}

#[derive(Clone, Copy)]
struct TexDelimiter {
    index: usize,
    kind: TexDelimiterKind,
}

#[derive(Clone, Copy)]
enum TexDelimiterKind {
    OpenParenthesis,
    CloseParenthesis,
    OpenBracket,
    CloseBracket,
}

fn collect_tex_delimiters(text: &str) -> Vec<TexDelimiter> {
    let mut delimiters = Vec::new();
    let mut index = 0;
    let mut preceding_backslashes = 0;

    while index < text.len() {
        let ch = text[index..].chars().next().expect("valid UTF-8");
        if ch == '\\' {
            if preceding_backslashes % 2 == 0 {
                let kind = match &text[index..] {
                    rest if rest.starts_with(r"\(") => Some(TexDelimiterKind::OpenParenthesis),
                    rest if rest.starts_with(r"\)") => Some(TexDelimiterKind::CloseParenthesis),
                    rest if rest.starts_with(r"\[") => Some(TexDelimiterKind::OpenBracket),
                    rest if rest.starts_with(r"\]") => Some(TexDelimiterKind::CloseBracket),
                    _ => None,
                };
                if let Some(kind) = kind {
                    delimiters.push(TexDelimiter { index, kind });
                }
            }
            preceding_backslashes += 1;
        } else {
            preceding_backslashes = 0;
        }
        index += ch.len_utf8();
    }

    delimiters
}

fn fenced_code_end(src: &str, index: usize, list_parent: bool) -> Option<usize> {
    if index != 0 && src.as_bytes()[index - 1] != b'\n' {
        return None;
    }
    let line_end = src[index..]
        .find('\n')
        .map_or(src.len(), |offset| index + offset);
    let line = &src[index..line_end];
    let (container_prefix, quote_depth) = blockquote_prefix(line);
    let indent = line[container_prefix..]
        .bytes()
        .take_while(|ch| *ch == b' ')
        .count();
    let fence_start = index + container_prefix + indent;
    let fence = *src.as_bytes().get(fence_start)?;
    if fence != b'`' && fence != b'~' {
        return None;
    }
    let fence_len = src[fence_start..]
        .bytes()
        .take_while(|ch| *ch == fence)
        .count();
    if fence_len < 3 {
        return None;
    }
    let list_indented = quote_depth == 0 && indent > 3 && list_parent;
    if indent > 3 && !list_indented {
        return None;
    }

    let mut line_start = src[fence_start..]
        .find('\n')
        .map_or(src.len(), |offset| fence_start + offset + 1);
    while line_start < src.len() {
        let line_end = src[line_start..]
            .find('\n')
            .map_or(src.len(), |offset| line_start + offset);
        let line = &src[line_start..line_end];
        let (container_prefix, line_quote_depth) = blockquote_prefix(line);
        let spaces = line[container_prefix..]
            .bytes()
            .take_while(|ch| *ch == b' ')
            .count();
        let run = line[container_prefix + spaces..]
            .bytes()
            .take_while(|ch| *ch == fence)
            .count();
        let remainder = &line[container_prefix + spaces + run..];
        if !line.trim().is_empty()
            && ((quote_depth > 0 && line_quote_depth < quote_depth)
                || (list_indented && line_quote_depth == 0 && spaces < indent))
        {
            return Some(line_start);
        }
        let valid_indent = if list_indented {
            spaces >= indent
        } else {
            spaces <= 3
        };
        if line_quote_depth == quote_depth
            && valid_indent
            && run >= fence_len
            && remainder.bytes().all(|ch| ch == b' ')
        {
            return Some(if line_end < src.len() {
                line_end + 1
            } else {
                line_end
            });
        }
        line_start = if line_end < src.len() {
            line_end + 1
        } else {
            line_end
        };
    }

    // An unclosed fence remains code until its container ends, or EOF for a
    // top-level fence, so the normalizer must leave that range untouched.
    Some(src.len())
}

fn blockquote_prefix(line: &str) -> (usize, usize) {
    let mut index = 0;
    let mut depth = 0;

    loop {
        let spaces = line[index..].bytes().take_while(|ch| *ch == b' ').count();
        if spaces > 3 || line.as_bytes().get(index + spaces) != Some(&b'>') {
            break;
        }
        index += spaces + 1;
        if line.as_bytes().get(index) == Some(&b' ') {
            index += 1;
        }
        depth += 1;
    }

    (index, depth)
}

fn list_parent_contexts(src: &str) -> Vec<bool> {
    let mut contexts = vec![false; src.len() + 1];
    let mut list_parent = false;
    let mut line_start = 0;

    for raw_line in src.split_inclusive('\n') {
        contexts[line_start] = list_parent;
        #[cfg(test)]
        LIST_CONTEXT_LINE_INSPECTIONS.with(|inspections| inspections.set(inspections.get() + 1));

        let line = raw_line.strip_suffix('\n').unwrap_or(raw_line);
        if !line.trim().is_empty() {
            list_parent = is_list_item_line(line);
        }
        line_start += raw_line.len();
    }

    contexts
}

fn is_list_item_line(line: &str) -> bool {
    let line = line.trim_start_matches(' ');
    if matches!(line.as_bytes().first(), Some(b'-' | b'+' | b'*')) {
        return line.as_bytes().get(1) == Some(&b' ');
    }

    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0
        && matches!(line.as_bytes().get(digits), Some(b'.' | b')'))
        && line.as_bytes().get(digits + 1) == Some(&b' ')
}

fn closing_backticks(src: &str, mut index: usize, ticks: usize) -> Option<usize> {
    while index < src.len() {
        if src[index..].starts_with('`') {
            let run = src[index..].bytes().take_while(|ch| *ch == b'`').count();
            if run == ticks {
                return Some(index);
            }
            index += run;
        } else {
            index += src[index..].chars().next()?.len_utf8();
        }
    }
    None
}

struct LineBuilder {
    width: u16,
    lines: Vec<Line<'static>>,
    current_spans: Vec<PendingSpan>,
    current_style: Style,
    display_math_just_flushed: bool,
    in_code_block: bool,
    code_block_lang: Option<String>,
    code_block_buffer: String,
    // Table rendering state. When inside a table, text events are buffered
    // into `current_cell` instead of `current_spans`; on `TagEnd::Table` the
    // whole table is rendered as Unicode box-drawing Lines.
    in_table: bool,
    table_alignments: Vec<Alignment>,
    table_rows: Vec<Vec<String>>,
    current_row: Vec<String>,
    current_cell: String,
    list_stack: Vec<Option<u64>>,
}

struct PendingSpan {
    span: Span<'static>,
    preserve_as_single_span: bool,
}

impl LineBuilder {
    fn new(width: u16) -> Self {
        Self {
            width,
            lines: Vec::new(),
            current_spans: Vec::new(),
            current_style: Style::new(),
            display_math_just_flushed: false,
            in_code_block: false,
            code_block_lang: None,
            code_block_buffer: String::new(),
            in_table: false,
            table_alignments: Vec::new(),
            table_rows: Vec::new(),
            current_row: Vec::new(),
            current_cell: String::new(),
            list_stack: Vec::new(),
        }
    }

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) => {
                self.display_math_just_flushed = false;
                if self.in_code_block {
                    self.code_block_buffer.push_str(&text);
                } else if self.in_table {
                    // Inside a table: buffer cell text instead of styling it.
                    self.current_cell.push_str(&text);
                } else {
                    self.push_text(&text);
                }
            }
            Event::Code(code) => {
                self.display_math_just_flushed = false;
                if self.in_table {
                    self.current_cell.push_str(code.as_ref());
                } else {
                    self.push_span(Span::styled(code.to_string(), Style::new().fg(Color::Cyan)));
                }
            }
            Event::InlineMath(formula) => {
                self.display_math_just_flushed = false;
                let formula = render_math(&formula);
                if self.in_table {
                    self.current_cell.push_str(&formula);
                } else {
                    self.push_math_span(Span::styled(formula, Style::new().fg(Color::Cyan)));
                }
            }
            Event::DisplayMath(formula) => {
                let formula = render_math(&formula);
                if self.in_table {
                    self.current_cell.push_str(&formula);
                } else {
                    if !self.current_spans.is_empty() {
                        self.flush_line();
                    }
                    self.push_math_span(Span::styled(formula, Style::new().fg(Color::Cyan)));
                    self.flush_line();
                    self.display_math_just_flushed = true;
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if self.in_table {
                    // Treat as space within a cell.
                    self.current_cell.push(' ');
                } else {
                    self.flush_line();
                }
            }
            _ => {}
        }
    }

    fn start_tag(&mut self, tag: Tag) {
        match tag {
            Tag::Heading { level, .. } => {
                self.current_style = match level {
                    pulldown_cmark::HeadingLevel::H1 => {
                        Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                    }
                    pulldown_cmark::HeadingLevel::H2 => Style::new().add_modifier(Modifier::BOLD),
                    _ => Style::new().add_modifier(Modifier::BOLD | Modifier::ITALIC),
                };
            }
            Tag::CodeBlock(kind) => {
                self.in_code_block = true;
                self.code_block_lang = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(lang) => Some(lang.into_string()),
                    _ => None,
                };
                self.code_block_buffer.clear();
            }
            Tag::Emphasis => {
                self.current_style = self.current_style.add_modifier(Modifier::ITALIC);
            }
            Tag::Strong => {
                self.current_style = self.current_style.add_modifier(Modifier::BOLD);
            }
            Tag::Strikethrough => {
                self.current_style = self.current_style.add_modifier(Modifier::CROSSED_OUT);
            }
            Tag::BlockQuote(_) => {
                self.current_style = self.current_style.fg(Color::Green);
            }
            Tag::Link { dest_url, .. } => {
                self.push_span(Span::styled(
                    dest_url.to_string(),
                    Style::new()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::UNDERLINED),
                ));
            }
            Tag::Table(alignments) => {
                self.in_table = true;
                self.table_alignments = alignments;
                self.table_rows.clear();
                self.current_row.clear();
                self.current_cell.clear();
            }
            Tag::TableHead | Tag::TableRow => {
                self.current_row.clear();
            }
            Tag::TableCell => {
                self.current_cell.clear();
            }
            Tag::List(start) => {
                self.list_stack.push(start);
            }
            Tag::Item => {
                let indent = "  ".repeat(self.list_stack.len().saturating_sub(1));
                let marker = match self.list_stack.last_mut() {
                    Some(Some(next)) => {
                        let marker = format!("{next}.");
                        *next += 1;
                        marker
                    }
                    _ => "-".to_string(),
                };
                self.push_span(Span::styled(
                    format!("{indent}{marker}"),
                    self.current_style,
                ));
            }
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Heading(_) | TagEnd::Paragraph => {
                if !self.display_math_just_flushed {
                    self.flush_line();
                }
                self.display_math_just_flushed = false;
                self.current_style = Style::new();
            }
            TagEnd::CodeBlock => {
                self.flush_code_block();
                self.in_code_block = false;
                self.code_block_lang = None;
                self.code_block_buffer.clear();
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::BlockQuote(_) => {
                self.current_style = Style::new();
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.current_cell);
                self.current_row.push(cell);
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                let row = std::mem::take(&mut self.current_row);
                self.table_rows.push(row);
            }
            TagEnd::Table => {
                self.flush_table();
                self.in_table = false;
            }
            TagEnd::Item => {
                if !self.current_spans.is_empty() {
                    self.flush_line();
                }
            }
            TagEnd::List(_) => {
                self.list_stack.pop();
            }
            _ => {}
        }
    }

    fn push_text(&mut self, text: &str) {
        self.push_span(Span::styled(text.to_string(), self.current_style));
    }

    fn push_span(&mut self, span: Span<'static>) {
        self.current_spans.push(PendingSpan {
            span,
            preserve_as_single_span: false,
        });
    }

    fn push_math_span(&mut self, span: Span<'static>) {
        self.current_spans.push(PendingSpan {
            span,
            preserve_as_single_span: true,
        });
    }

    #[allow(dead_code)]
    fn wrap_line(&self, line: Line<'static>) -> Line<'static> {
        // Single Line can't represent wrapping; we handle it at flush_line level
        // by splitting into multiple Lines. This function is kept for API compat
        // but the actual wrapping happens in flush_line.
        line
    }

    fn flush_line(&mut self) {
        if self.current_spans.is_empty() {
            self.lines.push(Line::raw(""));
        } else {
            let spans = std::mem::take(&mut self.current_spans);
            let max_w = self.width as usize;
            // Word-wrap: split spans into words, accumulate until display width exceeded.
            // If a single word exceeds max_w (common for CJK text without spaces),
            // break it character-by-character.
            let mut current: Vec<Span<'static>> = Vec::new();
            let mut current_width: usize = 0;
            let mut previous_was_formula = false;
            for pending in spans {
                let span = pending.span;
                let span_style = span.style;
                let span_text = span.content.into_owned();
                // Keep formulas readable as one semantic span
                // whenever it fits, rather than splitting it at internal spaces.
                if pending.preserve_as_single_span {
                    let formula_width = UnicodeWidthStr::width(span_text.as_str());
                    if formula_width <= max_w {
                        if !current.is_empty() && current_width + formula_width > max_w {
                            self.lines.push(Line::from(std::mem::take(&mut current)));
                            current_width = 0;
                        }
                        current_width += formula_width;
                        current.push(Span::styled(span_text, span_style));
                        previous_was_formula = true;
                        continue;
                    }
                }
                // Split span into words preserving spaces
                let mut words: Vec<&str> = span_text.split(' ').collect();
                for (i, word) in words.drain(..).enumerate() {
                    let word_width = UnicodeWidthStr::width(word);
                    let sep = if i == 0 && (current.is_empty() || previous_was_formula) {
                        0
                    } else {
                        1
                    }; // space before word
                    if current_width + sep + word_width <= max_w {
                        // Fits on current line
                        if sep == 1 && !current.is_empty() {
                            current.push(Span::raw(" "));
                        }
                        current.push(Span::styled(word.to_string(), span_style));
                        current_width += sep + word_width;
                    } else if word_width <= max_w {
                        // Word fits on its own line; start new line
                        self.lines.push(Line::from(std::mem::take(&mut current)));
                        current.push(Span::styled(word.to_string(), span_style));
                        current_width = word_width;
                    } else {
                        // Single word exceeds max_w: break character-by-character.
                        // Flush whatever is on the current line first.
                        if !current.is_empty() {
                            self.lines.push(Line::from(std::mem::take(&mut current)));
                            current_width = 0;
                        }
                        let mut chunk = String::new();
                        let mut chunk_width: usize = 0;
                        for ch in word.chars() {
                            let ch_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                            if ch_w == 0 {
                                continue;
                            }
                            if chunk_width + ch_w > max_w && !chunk.is_empty() {
                                self.lines.push(Line::from(vec![Span::styled(
                                    std::mem::take(&mut chunk),
                                    span_style,
                                )]));
                                chunk_width = 0;
                            }
                            chunk.push(ch);
                            chunk_width += ch_w;
                        }
                        if !chunk.is_empty() {
                            current.push(Span::styled(chunk, span_style));
                            current_width = chunk_width;
                        }
                    }
                }
                previous_was_formula = false;
            }
            if !current.is_empty() {
                self.lines.push(Line::from(current));
            }
        }
    }

    fn flush_code_block(&mut self) {
        let lang = self.code_block_lang.as_deref().unwrap_or("");
        let _ = lang;
        for code_line in self.code_block_buffer.lines() {
            self.lines.push(Line::styled(
                format!("  {code_line}"),
                Style::new().fg(Color::Yellow),
            ));
        }
    }

    /// Render the accumulated table rows as Unicode box-drawing Lines and push
    /// them to `self.lines`. Resets all table state.
    ///
    /// The table is always folded to `self.width`: columns are shrunk until the
    /// box fits, and any cell that no longer fits its column is hard-wrapped by
    /// display width across physical lines. When the width cannot even hold
    /// minimum-width columns (e.g. a six-column table on a 20-col terminal),
    /// we fall back to vertical `label: value` records, which always fit.
    /// Without this the rendered line keeps growing past the terminal and
    /// ratatui truncates its right side permanently (see `Line::render`), so
    /// the user silently loses cells off the right edge.
    fn flush_table(&mut self) {
        let rows = std::mem::take(&mut self.table_rows);
        let alignments = std::mem::take(&mut self.table_alignments);
        if rows.is_empty() {
            return;
        }
        // Determine number of columns and the max display width in each column.
        let num_cols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        if num_cols == 0 {
            return;
        }
        let natural_widths: Vec<usize> = (0..num_cols)
            .map(|i| {
                rows.iter()
                    .filter_map(|row| row.get(i))
                    .map(|cell| UnicodeWidthStr::width(cell.as_str()))
                    .max()
                    .unwrap_or(0)
            })
            .collect();

        let table_width = self.width as usize;
        // Smallest drawable box: 1 content column + 2 padding columns per
        // column, plus one border char per column and one closing border.
        let min_total = 4 * num_cols + 1;
        if min_total > table_width {
            // Cannot draw even a minimal box within the terminal. Vertical
            // records trade the table shape for content that always fits.
            self.push_vertical_records(&rows, &alignments);
            self.in_table = false;
            self.current_row.clear();
            self.current_cell.clear();
            return;
        }
        let col_widths = fit_column_widths(&natural_widths, table_width);

        // Helper: build a horizontal border line.
        // `left`, `mid`, `right` are the corner/junction chars; `fill` is ─.
        let border = |left: char, mid: char, right: char| -> String {
            let mut s = String::new();
            s.push(left);
            for (i, w) in col_widths.iter().enumerate() {
                if i > 0 {
                    s.push(mid);
                }
                let pad = w + 2; // 2 spaces of padding around cell content
                for _ in 0..pad {
                    s.push('─');
                }
            }
            s.push(right);
            s
        };

        // A physical row is one line of every cell's wrapped sub-lines. Cells
        // shorter than the tallest wrap to blank padding so borders stay aligned.
        let row_lines = |row: &[String]| -> Vec<String> {
            let wrapped: Vec<Vec<String>> = (0..num_cols)
                .map(|i| {
                    let cell = row.get(i).map(|s| s.as_str()).unwrap_or("");
                    wrap_cell(cell, col_widths[i])
                })
                .collect();
            let height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
            (0..height)
                .map(|line| {
                    let mut s = String::from("│");
                    for (i, segments) in wrapped.iter().enumerate() {
                        let segment = segments.get(line).map(|s| s.as_str()).unwrap_or("");
                        s.push_str(&pad_cell(
                            segment,
                            col_widths[i],
                            alignments.get(i).copied().unwrap_or(Alignment::None),
                        ));
                        s.push('│');
                    }
                    s
                })
                .collect()
        };

        // Top border: ┌─┬─┐
        self.lines.push(Line::raw(border('┌', '┬', '┐')));
        for (ri, row) in rows.iter().enumerate() {
            for line in row_lines(row) {
                self.lines.push(Line::raw(line));
            }
            if ri == 0 {
                // Header separator after the first (header) row.
                self.lines.push(Line::raw(border('├', '┼', '┤')));
            }
        }
        // Bottom border: └─┴─┘
        self.lines.push(Line::raw(border('└', '┴', '┘')));
        self.in_table = false;
        self.current_row.clear();
        self.current_cell.clear();
    }

    /// Fallback layout for tables too wide for a box: one `label: value` record
    /// per row, every record wrapped to `self.width`. Used when the terminal
    /// cannot hold minimum-width columns, where a box would clip.
    fn push_vertical_records(&mut self, rows: &[Vec<String>], alignments: &[Alignment]) {
        let _ = alignments;
        let width = self.width as usize;
        let Some(header) = rows.first() else {
            return;
        };
        for (ri, row) in rows.iter().enumerate().skip(1) {
            if ri > 1 {
                self.lines.push(Line::raw(""));
            }
            for (i, value) in row.iter().enumerate() {
                let label = header.get(i).map(|s| s.as_str()).unwrap_or("");
                let label = if label.is_empty() {
                    format!("col{i}")
                } else {
                    label.to_string()
                };
                let text = format!("{label}: {value}");
                for chunk in wrap_by_display_width(&text, width.max(1), "", "  ") {
                    self.lines.push(Line::raw(chunk));
                }
            }
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        if !self.current_spans.is_empty() {
            self.flush_line();
        }
        self.lines
    }
}

fn render_math(formula: &str) -> String {
    TexRenderer::new(formula).render()
}

/// Shrink natural column widths until the whole box fits `table_width`.
///
/// A column never drops below 1 content column, and the fixed overhead is 2
/// padding columns per column plus 1 char per border (the leading `│`, each
/// interior `│`, and the trailing `│`, i.e. `num_cols + 1`). Space is taken
/// from the widest column first so narrow columns keep their natural size and
/// the box stays as readable as possible. Assumes the caller already checked
/// that minimum-width columns fit.
fn fit_column_widths(natural: &[usize], table_width: usize) -> Vec<usize> {
    let num_cols = natural.len();
    let overhead = num_cols * 2 + (num_cols + 1);
    let mut widths = natural.to_vec();
    // Space the box may use for content after fixed padding and borders.
    // Padding and borders already fit (checked by the caller), so this is
    // never negative and only reports how much content space is left.
    let budget = table_width.saturating_sub(overhead);
    loop {
        let total: usize = widths.iter().sum();
        if total <= budget {
            return widths;
        }
        // Widest first; ties resolve to the earliest column for determinism.
        let Some(index) = widths
            .iter()
            .enumerate()
            .filter(|(_, w)| **w > 1)
            .max_by_key(|(i, w)| (**w, std::cmp::Reverse(*i)))
            .map(|(i, _)| i)
        else {
            return widths; // Every column is already at the 1-col minimum.
        };
        widths[index] -= 1;
    }
}

/// Wrap one cell's text into sub-lines of at most `width` display columns.
///
/// Words are kept intact when they fit; a word longer than the column is hard
/// split by display width (CJK and emoji count two columns). Returns a
/// non-empty vector so callers can index it safely.
fn wrap_cell(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for word in text.split(' ') {
        let word_width = UnicodeWidthStr::width(word);
        let separator = if current.is_empty() { 0 } else { 1 };
        if current_width + separator + word_width <= width {
            if separator == 1 {
                current.push(' ');
                current_width += 1;
            }
            current.push_str(word);
            current_width += word_width;
            continue;
        }
        if !current.is_empty() {
            out.push(std::mem::take(&mut current));
            current_width = 0;
        }
        if word_width <= width {
            current.push_str(word);
            current_width = word_width;
            continue;
        }
        // A single word wider than the column: split it by display width.
        for ch in word.chars() {
            let ch_width = UnicodeWidthStr::width(ch.to_string().as_str());
            if current_width + ch_width > width && !current.is_empty() {
                out.push(std::mem::take(&mut current));
                current_width = 0;
            }
            current.push(ch);
            current_width += ch_width;
        }
    }
    if !current.is_empty() || out.is_empty() {
        out.push(current);
    }
    out
}

/// Pad a wrapped cell segment to `width` display columns inside its 1-space
/// border padding, applying the column alignment. Wrapped segments are always
/// padded on the right so the closing `│` lines up across sub-lines.
fn pad_cell(segment: &str, width: usize, align: Alignment) -> String {
    let segment_width = UnicodeWidthStr::width(segment);
    let pad_total = width.saturating_sub(segment_width);
    let (left_pad, right_pad) = match align {
        Alignment::Center => {
            let left = pad_total / 2;
            (left, pad_total - left)
        }
        Alignment::Right => (pad_total, 0),
        _ => (0, pad_total), // None and Left both left-align
    };
    let mut out = String::with_capacity(width + 2);
    out.push(' ');
    for _ in 0..left_pad {
        out.push(' ');
    }
    out.push_str(segment);
    for _ in 0..right_pad {
        out.push(' ');
    }
    out.push(' ');
    out
}

const MAX_TEX_NESTING: usize = 32;

struct TexRenderer {
    chars: Vec<char>,
    position: usize,
}

impl TexRenderer {
    fn new(formula: &str) -> Self {
        Self {
            chars: formula.chars().collect(),
            position: 0,
        }
    }

    fn render(mut self) -> String {
        self.render_until(None, 0)
    }

    fn render_until(&mut self, closing: Option<char>, depth: usize) -> String {
        let mut rendered = String::new();

        while let Some(ch) = self.next_char() {
            if Some(ch) == closing {
                break;
            }
            match ch {
                '\\' => rendered.push_str(&self.render_command(depth)),
                '{' => {
                    if depth >= MAX_TEX_NESTING {
                        rendered.push_str(&self.consume_raw_group_after_open());
                    } else {
                        rendered.push_str(&self.render_until(Some('}'), depth + 1));
                    }
                }
                '^' => rendered.push_str(&self.render_script(true, depth)),
                '_' => rendered.push_str(&self.render_script(false, depth)),
                _ => rendered.push(ch),
            }
        }

        rendered
    }

    fn render_command(&mut self, depth: usize) -> String {
        let name = self.consume_command_name();
        match name.as_str() {
            "alpha" => "α".to_string(),
            "beta" => "β".to_string(),
            "gamma" => "γ".to_string(),
            "delta" => "δ".to_string(),
            "epsilon" => "ε".to_string(),
            "theta" => "θ".to_string(),
            "lambda" => "λ".to_string(),
            "mu" => "μ".to_string(),
            "pi" => "π".to_string(),
            "sigma" => "σ".to_string(),
            "phi" => "φ".to_string(),
            "omega" => "ω".to_string(),
            "Gamma" => "Γ".to_string(),
            "Delta" => "Δ".to_string(),
            "Theta" => "Θ".to_string(),
            "Lambda" => "Λ".to_string(),
            "Pi" => "Π".to_string(),
            "Sigma" => "Σ".to_string(),
            "Phi" => "Φ".to_string(),
            "Omega" => "Ω".to_string(),
            "sum" => "∑".to_string(),
            "prod" => "∏".to_string(),
            "int" => "∫".to_string(),
            "infty" => "∞".to_string(),
            "le" | "leq" => "≤".to_string(),
            "ge" | "geq" => "≥".to_string(),
            "neq" => "≠".to_string(),
            "times" => "×".to_string(),
            "cdot" => "·".to_string(),
            "pm" => "±".to_string(),
            "to" | "rightarrow" => "→".to_string(),
            "sin" | "cos" | "tan" => name,
            "left" | "right" => String::new(),
            "frac" => self.render_fraction(depth),
            "sqrt" => format!("√{}", self.render_argument(depth)),
            _ if name.is_empty() => "\\".to_string(),
            _ => name,
        }
    }

    fn consume_command_name(&mut self) -> String {
        let start = self.position;
        while self
            .chars
            .get(self.position)
            .is_some_and(|ch| ch.is_ascii_alphabetic())
        {
            self.position += 1;
        }
        self.chars[start..self.position].iter().collect()
    }

    fn render_fraction(&mut self, depth: usize) -> String {
        let numerator_end = self.braced_group_end(self.position);
        let denominator_end = numerator_end.and_then(|end| self.braced_group_end(end));
        if denominator_end.is_none() {
            return "frac".to_string();
        }

        let numerator = self.render_argument(depth);
        let denominator = self.render_argument(depth);
        format!("{numerator}⁄{denominator}")
    }

    fn render_argument(&mut self, depth: usize) -> String {
        if self.peek_char() == Some('{') {
            if depth >= MAX_TEX_NESTING {
                self.position += 1;
                self.consume_raw_group_after_open()
            } else {
                self.position += 1;
                self.render_until(Some('}'), depth + 1)
            }
        } else if self.peek_char() == Some('\\') {
            self.position += 1;
            self.render_command(depth)
        } else {
            self.next_char()
                .map_or_else(String::new, |ch| ch.to_string())
        }
    }

    fn render_script(&mut self, superscript: bool, depth: usize) -> String {
        let script = self.render_argument(depth);
        script
            .chars()
            .map(|ch| map_script_character(ch, superscript))
            .collect()
    }

    fn braced_group_end(&self, start: usize) -> Option<usize> {
        if self.chars.get(start) != Some(&'{') {
            return None;
        }

        let mut nesting = 0;
        for (index, ch) in self.chars.iter().enumerate().skip(start) {
            match ch {
                '{' => nesting += 1,
                '}' => {
                    nesting -= 1;
                    if nesting == 0 {
                        return Some(index + 1);
                    }
                }
                _ => {}
            }
        }
        None
    }

    fn consume_raw_group_after_open(&mut self) -> String {
        let mut raw = String::from("{");
        let mut nesting = 1;

        while let Some(ch) = self.next_char() {
            raw.push(ch);
            match ch {
                '{' => nesting += 1,
                '}' => {
                    nesting -= 1;
                    if nesting == 0 {
                        break;
                    }
                }
                _ => {}
            }
        }

        raw
    }

    fn peek_char(&self) -> Option<char> {
        self.chars.get(self.position).copied()
    }

    fn next_char(&mut self) -> Option<char> {
        let ch = self.peek_char()?;
        self.position += 1;
        Some(ch)
    }
}

fn map_script_character(ch: char, superscript: bool) -> char {
    let mapped = if superscript {
        match ch {
            '0' => Some('⁰'),
            '1' => Some('¹'),
            '2' => Some('²'),
            '3' => Some('³'),
            '4' => Some('⁴'),
            '5' => Some('⁵'),
            '6' => Some('⁶'),
            '7' => Some('⁷'),
            '8' => Some('⁸'),
            '9' => Some('⁹'),
            '+' => Some('⁺'),
            '-' => Some('⁻'),
            '=' => Some('⁼'),
            '(' => Some('⁽'),
            ')' => Some('⁾'),
            'a' => Some('ᵃ'),
            'b' => Some('ᵇ'),
            'c' => Some('ᶜ'),
            'd' => Some('ᵈ'),
            'e' => Some('ᵉ'),
            'f' => Some('ᶠ'),
            'g' => Some('ᵍ'),
            'h' => Some('ʰ'),
            'i' => Some('ⁱ'),
            'j' => Some('ʲ'),
            'k' => Some('ᵏ'),
            'l' => Some('ˡ'),
            'm' => Some('ᵐ'),
            'n' => Some('ⁿ'),
            'o' => Some('ᵒ'),
            'p' => Some('ᵖ'),
            'r' => Some('ʳ'),
            's' => Some('ˢ'),
            't' => Some('ᵗ'),
            'u' => Some('ᵘ'),
            'v' => Some('ᵛ'),
            'w' => Some('ʷ'),
            'x' => Some('ˣ'),
            'y' => Some('ʸ'),
            'z' => Some('ᶻ'),
            _ => None,
        }
    } else {
        match ch {
            '0' => Some('₀'),
            '1' => Some('₁'),
            '2' => Some('₂'),
            '3' => Some('₃'),
            '4' => Some('₄'),
            '5' => Some('₅'),
            '6' => Some('₆'),
            '7' => Some('₇'),
            '8' => Some('₈'),
            '9' => Some('₉'),
            '+' => Some('₊'),
            '-' => Some('₋'),
            '=' => Some('₌'),
            '(' => Some('₍'),
            ')' => Some('₎'),
            'a' => Some('ₐ'),
            'e' => Some('ₑ'),
            'h' => Some('ₕ'),
            'i' => Some('ᵢ'),
            'j' => Some('ⱼ'),
            'k' => Some('ₖ'),
            'l' => Some('ₗ'),
            'm' => Some('ₘ'),
            'n' => Some('ₙ'),
            'o' => Some('ₒ'),
            'p' => Some('ₚ'),
            'r' => Some('ᵣ'),
            's' => Some('ₛ'),
            't' => Some('ₜ'),
            'x' => Some('ₓ'),
            _ => None,
        }
    };
    mapped.unwrap_or(ch)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.to_string()).collect()
    }

    fn reset_list_context_line_inspections() {
        LIST_CONTEXT_LINE_INSPECTIONS.with(|inspections| inspections.set(0));
    }

    fn list_context_line_inspections() -> usize {
        LIST_CONTEXT_LINE_INSPECTIONS.with(Cell::get)
    }

    #[test]
    fn plain_text_renders_as_single_line() {
        let lines = render_markdown("hello world", 80);
        assert_eq!(lines.len(), 1);
        assert_eq!(spans_text(&lines[0]), "hello world");
    }

    #[test]
    fn many_deep_fence_candidates_use_one_forward_list_context_pass() {
        let src = format!("{}outside \\(x\\)", "    ```text\n".repeat(128));
        reset_list_context_line_inspections();

        let rendered = render_markdown(&src, 80);

        assert!(
            rendered
                .iter()
                .any(|line| spans_text(line).contains("outside x"))
        );
        assert_eq!(list_context_line_inspections(), src.lines().count());
    }

    #[test]
    fn h1_is_bold_underlined() {
        let lines = render_markdown("# Title", 80);
        assert_eq!(lines.len(), 1);
        let style = lines[0].spans[0].style;
        assert!(style.add_modifier.contains(Modifier::BOLD));
        assert!(style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn inline_code_is_cyan() {
        let lines = render_markdown("use `foo` here", 80);
        assert_eq!(lines.len(), 1);
        let code_span = lines[0]
            .spans
            .iter()
            .find(|s| s.content == "foo")
            .expect("should find code span");
        assert_eq!(code_span.style.fg, Some(Color::Cyan));
    }

    #[test]
    fn escaped_backticks_do_not_hide_backslash_math() {
        let rendered = render_markdown(r"\`literal \(\alpha\) `", 80);

        assert!(spans_text(&rendered[0]).contains('α'));
    }

    #[test]
    fn escaped_backtick_closes_an_open_code_span() {
        let rendered = render_markdown(r"`foo \` math \(\alpha\) `", 80);

        assert!(spans_text(&rendered[0]).contains('α'));
    }

    #[test]
    fn math_delimiters_in_link_destinations_remain_literal() {
        let src =
            r"[visible \(\alpha\)](https://example.com/\(path\)) ![alt](https://img.test/\[x\])";
        let normalized = normalize_math_delimiters(src);
        let rendered = render_markdown(src, 120);
        let text: String = rendered.iter().map(spans_text).collect();

        assert!(text.contains('α'));
        assert!(normalized.contains(r"https://example.com/\(path\)"));
        assert!(normalized.contains(r"https://img.test/\[x\]"));
    }

    #[test]
    fn math_delimiters_in_autolinks_and_html_attributes_remain_literal() {
        let src = r#"<https://example.com/\(path\)> <a href="https://example.com/\[path\]">link \(\beta\)</a>"#;
        let normalized = normalize_math_delimiters(src);
        let rendered = render_markdown(src, 120);
        let text: String = rendered.iter().map(spans_text).collect();

        assert!(normalized.contains(r"https://example.com/\(path\)"));
        assert!(normalized.contains(r"https://example.com/\[path\]"));
        assert!(text.contains('β'));
    }

    #[test]
    fn inline_code_with_spaces_keeps_existing_word_wrapping() {
        let lines = render_markdown("`two words`", 80);
        let code_words: Vec<_> = lines[0]
            .spans
            .iter()
            .filter(|span| span.style.fg == Some(Color::Cyan))
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(code_words, ["two", "words"]);
    }

    #[test]
    fn inline_dollar_math_omits_delimiters_and_is_cyan() {
        let lines = render_markdown("area is $\\pi r^2$.", 80);
        let formula = lines[0]
            .spans
            .iter()
            .find(|span| span.content == "π r²")
            .expect("formula span");
        assert_eq!(formula.style.fg, Some(Color::Cyan));
        assert_eq!(spans_text(&lines[0]), "area is π r².");
    }

    #[test]
    fn inline_backslash_parenthesis_math_renders_as_inline_math() {
        let lines = render_markdown(r"mass \(\alpha + \beta\)", 80);

        assert_eq!(spans_text(&lines[0]), "mass α + β");
    }

    #[test]
    fn backslash_bracket_math_renders_on_its_own_line() {
        let rendered: Vec<String> = render_markdown("before\n\n\\[\\sqrt{x}\\]\n\nafter", 80)
            .iter()
            .map(spans_text)
            .collect();

        assert_eq!(rendered, ["before", "√x", "after"]);
    }

    #[test]
    fn backslash_math_delimiters_in_code_are_preserved() {
        let inline = render_markdown(r"`\(\alpha\)`", 80);
        let fenced = render_markdown("```text\n\\[\\sqrt{x}\\]\n```", 80);

        assert_eq!(spans_text(&inline[0]), r"\(\alpha\)");
        assert!(
            fenced
                .iter()
                .any(|line| spans_text(line).contains(r"\[\sqrt{x}\]"))
        );
    }

    #[test]
    fn blockquote_fenced_code_preserves_backslash_math_delimiters() {
        let rendered: Vec<String> = render_markdown("> ```text\n> \\(x\\)", 80)
            .iter()
            .map(spans_text)
            .collect();

        assert!(rendered.iter().any(|line| line.contains(r"\(x\)")));
    }

    #[test]
    fn unclosed_blockquote_fence_stops_protection_at_outer_prose() {
        let rendered: Vec<String> = render_markdown("> ```text\n> code\noutside \\(x\\)", 80)
            .iter()
            .map(spans_text)
            .collect();

        assert!(rendered.iter().any(|line| line.contains("outside x")));
    }

    #[test]
    fn list_indented_fenced_code_preserves_backslash_math_delimiters() {
        let rendered: Vec<String> = render_markdown("- item\n\n    ```text\n    \\(x\\)", 80)
            .iter()
            .map(spans_text)
            .collect();

        assert!(rendered.iter().any(|line| line.contains(r"\(x\)")));
    }

    #[test]
    fn unclosed_list_fence_stops_protection_at_outer_prose() {
        let rendered: Vec<String> =
            render_markdown("- item\n\n    ```text\n    code\noutside \\(x\\)", 80)
                .iter()
                .map(spans_text)
                .collect();

        assert!(rendered.iter().any(|line| line.contains("outside x")));
    }

    #[test]
    fn single_backtick_code_span_ignores_longer_backtick_runs() {
        let rendered = render_markdown(r"`triple ``` then \(\alpha\) end`", 80);

        assert_eq!(spans_text(&rendered[0]), r"triple ``` then \(\alpha\) end");
    }

    #[test]
    fn unclosed_fenced_code_preserves_backslash_math_delimiters_to_eof() {
        let rendered: Vec<String> = render_markdown("```text\n\\(x\\)", 80)
            .iter()
            .map(spans_text)
            .collect();

        assert!(rendered.iter().any(|line| line.contains(r"\(x\)")));
    }

    #[test]
    fn invalid_fence_closer_does_not_end_code_protection() {
        let rendered: Vec<String> = render_markdown("```text\n```not-a-close\n\\(x\\)", 80)
            .iter()
            .map(spans_text)
            .collect();

        assert!(rendered.iter().any(|line| line.contains(r"\(x\)")));
    }

    #[test]
    fn tab_after_fence_run_does_not_end_code_protection() {
        let rendered: Vec<String> = render_markdown("```text\n```\t\n\\(x\\)", 80)
            .iter()
            .map(spans_text)
            .collect();

        assert!(rendered.iter().any(|line| line.contains(r"\(x\)")));
    }

    #[test]
    fn unmatched_or_escaped_backslash_math_delimiters_remain_literal() {
        let unmatched = render_markdown(r"mass \(\alpha", 80);
        let escaped = render_markdown(r"mass \\(\alpha\\)", 80);

        assert!(spans_text(&unmatched[0]).contains(r"\(\alpha"));
        let escaped_text = spans_text(&escaped[0]);
        assert!(escaped_text.contains(r"\("));
        assert!(escaped_text.contains(r"\)"));
    }

    #[test]
    fn inline_math_renders_symbols_scripts_fractions_and_roots() {
        let rendered = render_markdown("$\\alpha + x^2 + a_{i+1} + \\frac{m}{n} + \\sqrt{z}$", 80);
        assert_eq!(spans_text(&rendered[0]), "α + x² + aᵢ₊₁ + m⁄n + √z");
    }

    #[test]
    fn unsupported_math_command_remains_readable() {
        let rendered = render_markdown("$\\unknown{x}$", 80);
        assert_eq!(spans_text(&rendered[0]), "unknownx");
    }

    #[test]
    fn script_command_arguments_remain_readable_without_slashes() {
        let rendered = render_markdown("$x^\\alpha + a_\\infty$", 80);
        assert_eq!(spans_text(&rendered[0]), "xα + a∞");
    }

    #[test]
    fn malformed_fraction_commands_remain_readable() {
        let incomplete = render_markdown("$\\frac{a}$", 80);
        let bare = render_markdown("$\\frac$", 80);

        assert_eq!(spans_text(&incomplete[0]), "fraca");
        assert_eq!(spans_text(&bare[0]), "frac");
        assert_eq!(render_math("\\"), "\\");
    }

    #[test]
    fn nesting_beyond_the_limit_keeps_group_content_readable() {
        let nesting = 33;
        let formula = format!("${}x{}$", "{".repeat(nesting), "}".repeat(nesting));

        let rendered = render_markdown(&formula, 120);

        assert!(spans_text(&rendered[0]).contains("{x}"));
    }

    #[test]
    fn formula_moves_to_a_fresh_line_without_splitting() {
        let lines = render_markdown("word$a b$", 6);
        assert_eq!(lines.len(), 2);
        assert_eq!(spans_text(&lines[0]), "word");
        assert_eq!(spans_text(&lines[1]), "a b");
        assert_eq!(lines[1].spans.len(), 1);
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::Cyan));
    }

    #[test]
    fn display_dollar_math_is_its_own_line() {
        let rendered: Vec<String> = render_markdown("before\n\n$$\\frac{a}{b}$$\n\nafter", 80)
            .iter()
            .map(spans_text)
            .collect();
        assert_eq!(rendered, ["before", "a⁄b", "after"]);
    }

    #[test]
    fn display_math_wraps_at_requested_width() {
        let rendered = render_markdown("$$\\alpha + \\beta + \\gamma + \\delta$$", 8);

        assert!(
            rendered
                .iter()
                .all(|line| { UnicodeWidthStr::width(spans_text(line).as_str()) <= 8 })
        );
    }

    #[test]
    fn code_block_renders_as_separate_lines() {
        let src = "```rust\nfn main() {}\n```\n";
        let lines = render_markdown(src, 80);
        let has_code = lines.iter().any(|l| spans_text(l).contains("fn main()"));
        assert!(has_code, "expected code block content");
    }

    #[test]
    fn bold_and_italic_toggle() {
        let lines = render_markdown("**bold** and *italic*", 80);
        assert_eq!(lines.len(), 1);
        let text = spans_text(&lines[0]);
        assert!(text.contains("bold"));
        assert!(text.contains("italic"));
    }

    #[test]
    fn paragraph_break_creates_new_line() {
        let lines = render_markdown("para one\n\npara two", 80);
        assert_eq!(lines.len(), 2);
        assert_eq!(spans_text(&lines[0]), "para one");
        assert_eq!(spans_text(&lines[1]), "para two");
    }

    #[test]
    fn unordered_list_renders_each_item_on_its_own_line() {
        let lines = render_markdown("- First item\n- Second item\n", 80);
        let rendered: Vec<String> = lines.iter().map(spans_text).collect();

        assert_eq!(rendered, ["- First item", "- Second item"]);

        let ordered = render_markdown("1. first\n2. second", 80);
        assert_eq!(
            ordered.iter().map(spans_text).collect::<Vec<_>>(),
            ["1. first", "2. second"]
        );
    }

    #[test]
    fn empty_string_returns_empty() {
        let lines = render_markdown("", 80);
        assert!(lines.is_empty());
    }

    #[test]
    fn long_text_wraps_at_width() {
        let src = "this is a very long line that should wrap when the terminal is narrow";
        let lines = render_markdown(src, 20);
        assert!(
            lines.len() > 1,
            "expected wrapping, got {} lines",
            lines.len()
        );
        // No single line should exceed the width
        for line in &lines {
            let w: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
            assert!(w <= 20, "line width {w} exceeds 20: {:?}", spans_text(line));
        }
    }

    #[test]
    fn cjk_text_wraps_at_display_width() {
        // Each CJK char is 2 display columns wide. With width=10, we should
        // fit at most 5 CJK chars per line.
        let src = "一二三四五六七八九十";
        let lines = render_markdown(src, 10);
        assert!(
            lines.len() > 1,
            "expected CJK text to wrap at display width 10, got {} lines",
            lines.len()
        );
        // Verify no line exceeds 10 display columns
        for (i, line) in lines.iter().enumerate() {
            let w: usize = line
                .spans
                .iter()
                .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            assert!(
                w <= 10,
                "line {} display width {w} exceeds 10: {:?}",
                i,
                spans_text(line)
            );
        }
    }

    #[test]
    fn cjk_mixed_with_ascii_wraps_correctly() {
        // Mixed CJK + ASCII: "编写或修改代码" is 14 display cols + "hello" is 5 = 19 cols
        // With width=15, this should wrap.
        let src = "编写或修改代码 hello world";
        let lines = render_markdown(src, 15);
        assert!(
            lines.len() > 1,
            "expected mixed CJK/ASCII to wrap at width 15, got {} lines",
            lines.len()
        );
        for (i, line) in lines.iter().enumerate() {
            let w: usize = line
                .spans
                .iter()
                .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            assert!(
                w <= 15,
                "line {} display width {w} exceeds 15: {:?}",
                i,
                spans_text(line)
            );
        }
    }

    #[test]
    fn emoji_width_counts_as_two() {
        // 📝 has display width 2. With width=5, "📝📝📝" (6 cols) should wrap.
        let src = "📝📝📝";
        let lines = render_markdown(src, 5);
        assert!(
            lines.len() > 1,
            "expected emoji to wrap at width 5, got {} lines",
            lines.len()
        );
    }

    #[test]
    fn simple_table_renders_with_box_drawing() {
        // A basic two-column table with a header row and one data row.
        // The renderer should emit Unicode box drawing characters (┌─┬─┐ etc.)
        // and the cell text, NOT raw markdown pipes.
        let src = "| Name | Age |\n| --- | --- |\n| Alice | 30 |\n";
        let lines = render_markdown(src, 40);
        let rendered: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect();
        let joined = rendered.join("\n");

        // Should contain box-drawing top border with column separator ┬
        assert!(
            joined.contains('┌') && joined.contains('┐'),
            "expected top corners ┌/┐, got: {joined:?}"
        );
        assert!(
            joined.contains('┬'),
            "expected column separator ┬ in top border, got: {joined:?}"
        );
        // Should contain both header and data cell text
        assert!(
            joined.contains("Name") && joined.contains("Age"),
            "expected header text, got: {joined:?}"
        );
        assert!(
            joined.contains("Alice") && joined.contains("30"),
            "expected data row text, got: {joined:?}"
        );
        // Should NOT contain raw markdown pipe syntax for table structure
        // (pipes might still appear in cell content, but not as `| --- |` separator)
        assert!(
            !joined.contains("---"),
            "expected no raw markdown separator dashes, got: {joined:?}"
        );
    }

    #[test]
    fn table_cell_keeps_inline_math_without_trailing_formula_line() {
        let rendered: Vec<String> = render_markdown("| Formula |\n| --- |\n| $\\pi$ |\n", 40)
            .iter()
            .map(spans_text)
            .collect();
        let joined = rendered.join("\n");

        assert!(joined.contains('π'), "missing table formula: {rendered:?}");
        assert!(
            !rendered.iter().any(|line| line == "π"),
            "formula escaped the table: {rendered:?}"
        );
    }

    #[test]
    fn table_borders_use_unicode_box_chars_not_pipes() {
        // Regression guard: table should render with Unicode box drawing,
        // not as raw markdown source with `|` column separators.
        let src = "| h1 | h2 |\n| --- | --- |\n| a | b |\n";
        let lines = render_markdown(src, 40);
        let joined: String = lines
            .iter()
            .flat_map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.chars().collect::<Vec<_>>())
            })
            .flatten()
            .collect();
        // Box-drawing vertical bar ─── or │ should appear
        assert!(
            joined.contains('│'),
            "expected vertical box-drawing char │, got: {joined:?}"
        );
        assert!(
            joined.contains('─'),
            "expected horizontal box-drawing char ─, got: {joined:?}"
        );
    }

    #[test]
    fn table_with_cjk_cells_uses_display_width() {
        // CJK characters have display width 2. Column width should be based on
        // display width, not char count, so "姓名" (2 chars, 4 cols) and "Alice"
        // (5 chars, 5 cols) both fit in a column sized to the wider one (5 cols).
        let src = "| 姓名 | 年龄 |\n| --- | --- |\n| Alice | 30 |\n";
        let lines = render_markdown(src, 40);
        let rendered: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect();
        let joined = rendered.join("\n");
        assert!(joined.contains("姓名"), "missing CJK header: {joined:?}");
        assert!(joined.contains("Alice"), "missing ASCII data: {joined:?}");
        // Verify no data row exceeds the rendered width: each row should start
        // with │ and end with │, and contain both cells.
        let data_line = rendered.iter().find(|l| l.contains("Alice"));
        assert!(data_line.is_some(), "missing Alice row: {rendered:?}");
        let data_line = data_line.unwrap();
        assert!(
            data_line.starts_with('│'),
            "row should start with │: {data_line:?}"
        );
        assert!(
            data_line.ends_with('│'),
            "row should end with │: {data_line:?}"
        );
    }

    #[test]
    fn table_preserves_cell_text_without_dropping_words() {
        // Regression guard: all cell content should appear in output. Earlier
        // bug had table tags silently dropped, which concatenated cells as
        // plain text. This test ensures no cell content is lost.
        let src = "| alpha | beta | gamma |\n| --- | --- | --- |\n| 1 | 2 | 3 |\n| x | y | z |\n";
        let lines = render_markdown(src, 60);
        let rendered: Vec<String> = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect();
        let joined = rendered.join("\n");
        for expected in ["alpha", "beta", "gamma", "1", "2", "3", "x", "y", "z"] {
            assert!(
                joined.contains(expected),
                "missing cell content {expected:?} in: {joined:?}"
            );
        }
    }

    /// Render a markdown table and return one string per display line.
    fn table_lines(lines: &[Line<'static>]) -> Vec<String> {
        lines.iter().map(spans_text).collect()
    }

    const BOX_CHARS: [char; 11] = ['─', '│', '┌', '┬', '┐', '├', '┼', '┤', '└', '┴', '┘'];

    /// Character counts of the rendered table, excluding box-drawing borders
    /// and all whitespace (wrapping adds padding, so counting characters that
    /// are not padding is how "no content was dropped" is asserted).
    fn table_chars(rendered: &[String]) -> std::collections::BTreeMap<char, usize> {
        let mut counts = std::collections::BTreeMap::new();
        for ch in rendered.iter().flat_map(|line| line.chars()) {
            if ch.is_whitespace() || BOX_CHARS.contains(&ch) {
                continue;
            }
            *counts.entry(ch).or_insert(0) += 1;
        }
        counts
    }

    /// Character counts of the source cells, ignoring the markdown syntax that
    /// is not part of any cell value.
    fn source_chars(cells: &[&str]) -> std::collections::BTreeMap<char, usize> {
        let mut counts = std::collections::BTreeMap::new();
        for ch in cells
            .iter()
            .flat_map(|cell| cell.chars())
            .filter(|ch| *ch != ' ')
        {
            *counts.entry(ch).or_insert(0) += 1;
        }
        counts
    }

    const WIDE_TABLE_HEADER: [&str; 3] = ["Name", "Description", "Owner"];
    const WIDE_TABLE_ROW: [&str; 3] = [
        "alpha-service",
        "Handles all inbound user authentication and session rotation",
        "platform-team",
    ];

    fn wide_table() -> String {
        format!(
            "| {} | {} | {} |\n| --- | --- | --- |\n| {} | {} | {} |\n",
            WIDE_TABLE_HEADER[0],
            WIDE_TABLE_HEADER[1],
            WIDE_TABLE_HEADER[2],
            WIDE_TABLE_ROW[0],
            WIDE_TABLE_ROW[1],
            WIDE_TABLE_ROW[2],
        )
    }

    #[test]
    fn table_wider_than_terminal_wraps_to_fit_width() {
        // The natural width of this table is ~96 columns. At any narrower
        // terminal every rendered line must fit, instead of being clipped.
        for width in [40u16, 60, 80] {
            for line in table_lines(&render_markdown(&wide_table(), width)) {
                let w = UnicodeWidthStr::width(line.as_str());
                assert!(
                    w <= width as usize,
                    "width {width}: line is {w} cols: {line:?}"
                );
            }
        }
    }

    #[test]
    fn wrapped_table_preserves_every_cell_character() {
        // Wrapping may split a long word across sub-lines, and columns
        // interleave on each physical row, so contiguity is not guaranteed.
        // What must hold is that every source character survives exactly once.
        let rendered = table_lines(&render_markdown(&wide_table(), 40));
        let source = source_chars(
            &WIDE_TABLE_HEADER
                .iter()
                .chain(WIDE_TABLE_ROW.iter())
                .copied()
                .collect::<Vec<_>>(),
        );
        assert_eq!(
            table_chars(&rendered),
            source,
            "table content changed while wrapping: {rendered:?}"
        );
    }

    #[test]
    fn single_column_table_keeps_words_intact_when_they_fit() {
        // With one column there is no interleaving, so a word that fits the
        // column must survive the wrap unbroken.
        let src = "| Description |\n| --- |\n| Handles all inbound user authentication |\n";
        let rendered = table_lines(&render_markdown(src, 40));
        let joined: String = rendered.join("");
        for word in ["Handles", "all", "inbound", "user", "authentication"] {
            assert!(joined.contains(word), "lost word {word:?}: {rendered:?}");
        }
    }

    #[test]
    fn wrapped_table_rows_keep_aligned_left_and_right_borders() {
        let rendered = table_lines(&render_markdown(&wide_table(), 40));
        let widths: Vec<usize> = rendered
            .iter()
            .map(|line| UnicodeWidthStr::width(line.as_str()))
            .collect();
        let table_width = widths[0];
        assert!(table_width <= 40, "table too wide: {table_width}");
        assert!(
            widths.iter().all(|w| *w == table_width),
            "wrapped rows are ragged: {widths:?} in {rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .all(|line| matches!(line.chars().next(), Some('│' | '┌' | '├' | '└'))),
            "row is missing its left border: {rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .all(|line| matches!(line.chars().last(), Some('│' | '┐' | '┤' | '┘'))),
            "row is missing its right border: {rendered:?}"
        );
    }

    #[test]
    fn table_too_narrow_for_columns_renders_vertical_records() {
        // Six columns need 3*6+1 = 19 cols of borders and padding alone, so a
        // 20-col terminal cannot hold a box table. It must not clip.
        let src = "| Model | input | output | cache_create | cache_read | calls |\n\
             | --- | ---: | ---: | ---: | ---: | ---: |\n\
             | claude-sonnet-4-5 | 1,000 | 200 | 30 | 40 | 5 |\n";
        let rendered = table_lines(&render_markdown(src, 20));
        for line in &rendered {
            let w = UnicodeWidthStr::width(line.as_str());
            assert!(w <= 20, "vertical record is {w} cols: {line:?}");
        }
        let joined = rendered.join("\n");
        assert!(
            !joined.contains('│'),
            "no box table at this width: {joined:?}"
        );
        for label in ["Model:", "input:", "output:", "cache_create:", "calls:"] {
            assert!(
                joined.contains(label),
                "expected the {label:?} record label: {joined:?}"
            );
        }
        assert!(joined.contains("1,000"), "lost a value: {joined:?}");
    }

    #[test]
    fn table_that_fits_keeps_box_borders_and_column_alignment() {
        let src = "| Name | Age |\n| --- | ---: |\n| Alice | 30 |\n";
        let rendered = table_lines(&render_markdown(src, 40));
        assert!(
            rendered
                .iter()
                .all(|l| UnicodeWidthStr::width(l.as_str()) <= 40),
            "a fitting table must not wrap: {rendered:?}"
        );
        let data_row = rendered
            .iter()
            .find(|l| l.contains("Alice"))
            .expect("missing data row");
        assert!(data_row.ends_with('│'), "lost right border: {data_row:?}");
        assert!(data_row.contains("30"), "lost value: {data_row:?}");
    }

    #[test]
    fn narrow_table_with_cjk_wraps_at_display_width() {
        let src = "| 姓名 | 描述 |\n| --- | --- |\n\
             | 张三丰 | 这是一个很长的中文描述文本用于测试折行 |\n";
        let expected = source_chars(&[
            "姓名",
            "描述",
            "张三丰",
            "这是一个很长的中文描述文本用于测试折行",
        ]);
        for width in [16u16, 24] {
            let rendered = table_lines(&render_markdown(src, width));
            for line in &rendered {
                let w = UnicodeWidthStr::width(line.as_str());
                assert!(
                    w <= width as usize,
                    "width {width}: line is {w} cols: {line:?}"
                );
            }
            assert_eq!(
                table_chars(&rendered),
                expected,
                "CJK content changed while wrapping at width {width}: {rendered:?}"
            );
        }
    }
}
