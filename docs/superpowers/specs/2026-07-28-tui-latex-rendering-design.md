# TUI LaTeX Rendering Design

## Goal

Render common LaTeX mathematics in the ratatui conversation history as
terminal-native Unicode mathematics instead of displaying raw TeX delimiters
and commands.

## Scope

The Markdown renderer accepts all of these delimiters outside Markdown code:

- Inline: `$...$` and `\\(...\\)`
- Display: `$$...$$` and `\\[...\\]`

The first implementation renders common mathematical syntax without a browser,
image protocol, external binary, or terminal-specific graphics feature:

- Greek letters and common named operators, such as `\\alpha`, `\\sum`,
  `\\leq`, and `\\times`
- Superscripts and subscripts, such as `x^2` and `a_{i+1}`
- Fractions and square roots, such as `\\frac{a}{b}` and `\\sqrt{x}`
- Braced groups and ordinary operators

The renderer is deliberately a readable terminal representation, not a full
TeX layout engine. For example, `\\frac{a}{b}` becomes `a⁄b`; the formula's
meaning remains visible in a one-dimensional terminal cell.

Unsupported commands retain their readable command content rather than being
dropped or causing a rendering error.

## Architecture

`tui/markdown.rs` remains the single Markdown-to-ratatui conversion boundary.
It will enable pulldown-cmark's existing `ENABLE_MATH` option so `$` and `$$`
produce `InlineMath` and `DisplayMath` events. A small normalization pass will
translate `\\(...\\)` and `\\[...\\]` to their dollar-delimited equivalents
only in normal Markdown text, never inside inline code or fenced code blocks.

A focused TeX-to-Unicode helper converts each math event into a styled string.
Inline math becomes a cyan span in the current line. Display math flushes the
current line, emits a cyan formula line, and starts a new line so it remains a
block even when Markdown shares its source line with surrounding text.

Existing `flush_line` logic continues to perform Unicode-width-aware wrapping.
This preserves streaming assistant-message re-rendering, history line counting,
and scroll behavior without changing their APIs.

## Error Handling and Compatibility

Malformed or unmatched delimiters retain the parser's normal literal-text
behavior. The normalizer only replaces matched delimiter pairs and copies every
other byte unchanged. A malformed TeX command produces readable output rather
than panic or data loss.

Inline and fenced code remain literal: formulas in `` `$x$` `` or a fenced code
block are not interpreted. Normal Markdown content that does not contain math
continues through the current renderer unchanged.

## Verification

Unit tests in `tui/markdown.rs` will prove:

- all four delimiters render their contents without delimiters;
- supported symbols, scripts, fractions, and roots produce expected Unicode;
- display math is isolated from surrounding prose;
- inline and fenced code preserve literal delimiter text;
- formula output observes the existing narrow-width wrapping behavior.

The targeted `yi-agent` TUI Markdown tests and formatter will be run before
commit. `docs/project-management/yi-agent-tui.md` and its README index count
will record the completed capability in the same implementation commit.
