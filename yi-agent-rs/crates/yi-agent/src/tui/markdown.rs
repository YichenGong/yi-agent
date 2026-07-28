use pulldown_cmark::{Alignment, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// Render a markdown string into ratatui Lines, wrapped at `width`.
pub fn render_markdown(src: &str, width: u16) -> Vec<Line<'static>> {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_MATH;
    let parser = Parser::new_ext(src, opts);
    let mut builder = LineBuilder::new(width);
    for event in parser {
        builder.handle_event(event);
    }
    builder.finish()
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
        let mut col_widths = vec![0usize; num_cols];
        for row in &rows {
            for (i, cell) in row.iter().enumerate() {
                let w = UnicodeWidthStr::width(cell.as_str());
                if w > col_widths[i] {
                    col_widths[i] = w;
                }
            }
        }
        // Cap column widths so total table fits in self.width when possible.
        // Total = sum(col_widths) + 3*num_cols + 1 (borders + padding + final border)
        // We don't hard-wrap cells here; if the table is wider than the terminal,
        // we let it overflow (consistent with how `flush_line` handles long words
        // in non-table text: the wrapping layer above this handles wrapping).
        let _ = self.width;

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

        // Helper: build a data row line with the given alignment per column.
        let data_line = |row: &[String]| -> String {
            let mut s = String::new();
            s.push('│');
            for (i, col_w) in col_widths.iter().enumerate() {
                let cell = row.get(i).map(|s| s.as_str()).unwrap_or("");
                let cw = UnicodeWidthStr::width(cell);
                let pad_total = col_w.saturating_sub(cw);
                let align = alignments.get(i).copied().unwrap_or(Alignment::None);
                let (left_pad, right_pad) = match align {
                    Alignment::Center => {
                        let l = pad_total / 2;
                        let r = pad_total - l;
                        (l, r)
                    }
                    Alignment::Right => (pad_total, 0),
                    _ => (0, pad_total), // None and Left both left-align
                };
                s.push(' ');
                for _ in 0..left_pad {
                    s.push(' ');
                }
                s.push_str(cell);
                for _ in 0..right_pad {
                    s.push(' ');
                }
                s.push(' ');
                s.push('│');
            }
            s
        };

        // Top border: ┌─┬─┐
        self.lines.push(Line::raw(border('┌', '┬', '┐')));
        for (ri, row) in rows.iter().enumerate() {
            self.lines.push(Line::raw(data_line(row)));
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

    #[test]
    fn plain_text_renders_as_single_line() {
        let lines = render_markdown("hello world", 80);
        assert_eq!(lines.len(), 1);
        assert_eq!(spans_text(&lines[0]), "hello world");
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
}
