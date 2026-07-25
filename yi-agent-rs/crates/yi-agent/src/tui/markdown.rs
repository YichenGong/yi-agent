use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// Render a markdown string into ratatui Lines, wrapped at `width`.
pub fn render_markdown(src: &str, width: u16) -> Vec<Line<'static>> {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
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
    current_spans: Vec<Span<'static>>,
    current_style: Style,
    in_code_block: bool,
    code_block_lang: Option<String>,
    code_block_buffer: String,
}

impl LineBuilder {
    fn new(width: u16) -> Self {
        Self {
            width,
            lines: Vec::new(),
            current_spans: Vec::new(),
            current_style: Style::new(),
            in_code_block: false,
            code_block_lang: None,
            code_block_buffer: String::new(),
        }
    }

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) => {
                if self.in_code_block {
                    self.code_block_buffer.push_str(&text);
                } else {
                    self.push_text(&text);
                }
            }
            Event::Code(code) => {
                self.push_span(Span::styled(code.to_string(), Style::new().fg(Color::Cyan)));
            }
            Event::SoftBreak | Event::HardBreak => {
                self.flush_line();
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
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Heading(_) | TagEnd::Paragraph => {
                self.flush_line();
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
            _ => {}
        }
    }

    fn push_text(&mut self, text: &str) {
        self.push_span(Span::styled(text.to_string(), self.current_style));
    }

    fn push_span(&mut self, span: Span<'static>) {
        self.current_spans.push(span);
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
            for span in spans {
                let span_style = span.style;
                let span_text = span.content.into_owned();
                // Split span into words preserving spaces
                let mut words: Vec<&str> = span_text.split(' ').collect();
                for (i, word) in words.drain(..).enumerate() {
                    let word_width = UnicodeWidthStr::width(word);
                    let sep = if i == 0 && current.is_empty() { 0 } else { 1 }; // space before word
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

    fn finish(mut self) -> Vec<Line<'static>> {
        if !self.current_spans.is_empty() {
            self.flush_line();
        }
        self.lines
    }
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
}
