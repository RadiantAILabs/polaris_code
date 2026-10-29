//! Private presentation helpers for the conversation panel.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

const MAX_BYTES: usize = 64 * 1024;
/// Rows a preview body shows before it collapses behind `Ctrl-O`.
///
/// Applies to every [`preview_lines`] body, not just diffs: a tool that returns
/// a whole file is exactly as able to bury the conversation as a large
/// replacement is, and a completed tool is meant to read as one small block
/// with its result a keystroke away. The same budget also caps a fenced code
/// block of any language and a fence-free run of rendered prose inside a rich
/// message (see [`code_lines`] and [`markdown_lines`]), so the fold is one
/// mechanism reused everywhere rather than a diff-specific special case.
const DIFF_PREVIEW_ROWS: usize = 24;

/// Remove terminal controls before parsing, including C1 and bidi overrides.
/// Keep newlines and expand tabs so code indentation survives ratatui rendering.
pub(crate) fn safe_text(text: &str) -> String {
    let mut result = String::new();
    let mut column = 0;
    for ch in text.chars() {
        match ch {
            '\n' => {
                result.push(ch);
                column = 0;
            }
            '\t' => {
                let spaces = 4 - column % 4;
                result.extend(std::iter::repeat_n(' ', spaces));
                column += spaces;
            }
            ch if ch.is_control()
                || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') => {}
            ch => {
                result.push(ch);
                column += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            }
        }
    }
    result
}

pub(crate) fn body_lines(text: &str, markdown: bool, expanded: bool) -> Vec<Line<'static>> {
    let end = text.floor_char_boundary(MAX_BYTES.min(text.len()));
    let text_part = safe_text(&text[..end]);
    let mut lines = if markdown {
        markdown_lines(&text_part, expanded)
    } else {
        text_part
            .split('\n')
            .map(|s| Line::raw(s.to_owned()))
            .collect()
    };
    if end < text.len() {
        lines.push(Line::styled(
            "… rendering limited to 64 KiB; original message retained",
            Style::default().fg(Color::Yellow),
        ));
    }
    if lines.is_empty() {
        lines.push(Line::default());
    }
    lines
}

/// Approval and tool bodies are literal except for an identifiable unified diff.
/// In particular, a shell command or a lone `+`/`-` line is not Markdown.
pub(crate) fn preview_lines(text: &str, expanded: bool) -> Vec<Line<'static>> {
    let end = text.floor_char_boundary(MAX_BYTES.min(text.len()));
    let bounded = &text[..end];
    let start = bounded.match_indices("--- ").find_map(|(offset, _)| {
        if offset != 0 && !bounded[..offset].ends_with('\n') {
            return None;
        }
        let tail = &bounded[offset..];
        let next = tail.find('\n')? + 1;
        tail[next..].starts_with("+++ ").then_some(offset)
    });
    let Some(start) = start else {
        return bound_rows(body_lines(text, false, expanded), expanded);
    };
    let mut lines = Vec::new();
    if start > 0 {
        lines.extend(body_lines(
            bounded[..start].trim_end_matches('\n'),
            false,
            expanded,
        ));
    }
    lines.extend(code_lines(&safe_text(&bounded[start..]), "diff", expanded));
    if end < text.len() {
        lines.push(Line::styled(
            "… rendering limited to 64 KiB; original message retained",
            Style::default().fg(Color::Yellow),
        ));
    }
    lines
}

/// Collapse a preview to [`DIFF_PREVIEW_ROWS`], saying how much is hidden.
///
/// Nothing is dropped — `expanded` returns the full body — so this is a view
/// budget, not a truncation of the record.
fn bound_rows(mut lines: Vec<Line<'static>>, expanded: bool) -> Vec<Line<'static>> {
    if expanded || lines.len() <= DIFF_PREVIEW_ROWS {
        return lines;
    }
    let hidden = lines.len() - DIFF_PREVIEW_ROWS;
    lines.truncate(DIFF_PREVIEW_ROWS);
    lines.push(Line::styled(
        format!("… {hidden} more lines · Ctrl-O to expand"),
        Style::default().fg(Color::Yellow),
    ));
    lines
}

fn flush(lines: &mut Vec<Line<'static>>, spans: &mut Vec<Span<'static>>) {
    if !spans.is_empty() {
        lines.push(Line::from(std::mem::take(spans)));
    }
}

fn markdown_lines(text: &str, expanded: bool) -> Vec<Line<'static>> {
    // Unified diffs also arrive as bare approval previews.
    if text.starts_with("diff --git ") || text.starts_with("--- ") && text.contains("\n+++ ") {
        return code_lines(text, "diff", expanded);
    }
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    let mut styles = vec![Style::default()];
    let mut lists: Vec<Option<u64>> = Vec::new();
    let mut code: Option<(String, String)> = None;
    // A fenced block already folds itself (see `code_lines`); folding the
    // message on top would cut into its header/footer rather than composing
    // with it. So the message-level fold below only applies to a message that
    // never opened a code fence at all.
    let mut has_code_block = false;
    let mut table: Option<TableBuf> = None;
    let mut link_urls: Vec<String> = Vec::new();
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(text, options) {
        let style = *styles.last().unwrap_or(&Style::default());
        match event {
            Event::Start(Tag::CodeBlock(kind)) => {
                flush(&mut lines, &mut spans);
                has_code_block = true;
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        info.split_whitespace().next().unwrap_or("").to_owned()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                code = Some((lang, String::new()));
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some((lang, body)) = code.take() {
                    lines.extend(code_lines(&body, &lang, expanded));
                }
            }
            Event::Text(body) if code.is_some() => {
                if let Some((_, buffer)) = &mut code {
                    buffer.push_str(&body);
                }
            }
            Event::Start(Tag::Heading { .. }) => {
                flush(&mut lines, &mut spans);
                styles.push(style.fg(Color::Cyan).add_modifier(Modifier::BOLD));
            }
            Event::Start(Tag::Strong) => styles.push(style.add_modifier(Modifier::BOLD)),
            Event::Start(Tag::Emphasis) => styles.push(style.add_modifier(Modifier::ITALIC)),
            Event::Start(Tag::Strikethrough) => {
                styles.push(style.add_modifier(Modifier::CROSSED_OUT))
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                link_urls.push(dest_url.into_string());
                styles.push(style.fg(Color::Blue).add_modifier(Modifier::UNDERLINED));
            }
            Event::End(TagEnd::Link) => {
                styles.pop();
                if let Some(url) = link_urls.pop() {
                    push_span(
                        &mut table,
                        &mut spans,
                        Span::styled(format!(" ({url})"), Style::default().fg(Color::Cyan)),
                    );
                }
            }
            Event::End(TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough) => {
                styles.pop();
            }
            Event::End(TagEnd::Heading(_)) => {
                styles.pop();
                flush(&mut lines, &mut spans);
            }
            Event::Start(Tag::List(start)) => {
                flush(&mut lines, &mut spans);
                lists.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                flush(&mut lines, &mut spans);
                lists.pop();
            }
            Event::Start(Tag::Item) => {
                flush(&mut lines, &mut spans);
                let indent = "  ".repeat(lists.len().saturating_sub(1));
                let marker = match lists.last_mut() {
                    Some(Some(number)) => {
                        let marker = format!("{number}. ");
                        *number = number.saturating_add(1);
                        marker
                    }
                    _ => "• ".to_string(),
                };
                spans.push(Span::raw(format!("{indent}{marker}")));
            }
            Event::End(TagEnd::Item | TagEnd::Paragraph) => flush(&mut lines, &mut spans),
            Event::Start(Tag::Table(_)) => {
                flush(&mut lines, &mut spans);
                table = Some(TableBuf::default());
            }
            Event::End(TagEnd::TableHead) => {
                styles.pop();
                if let Some(t) = table.as_mut() {
                    t.head = std::mem::take(&mut t.row);
                }
            }
            Event::End(TagEnd::TableRow) => {
                if let Some(t) = table.as_mut() {
                    let row = std::mem::take(&mut t.row);
                    t.rows.push(row);
                }
            }
            Event::End(TagEnd::TableCell) => {
                if let Some(t) = table.as_mut() {
                    let cell = std::mem::take(&mut t.cell);
                    t.row.push(cell);
                }
            }
            Event::Start(Tag::TableHead) => styles.push(style.add_modifier(Modifier::BOLD)),
            Event::End(TagEnd::Table) => {
                if let Some(t) = table.take() {
                    lines.extend(render_table(t));
                }
            }
            Event::Start(Tag::BlockQuote(_)) => {
                spans.push(Span::styled("│ ", Style::default().fg(Color::Cyan)))
            }
            Event::Text(body) | Event::Html(body) | Event::InlineHtml(body) => {
                for (index, segment) in body.split('\n').enumerate() {
                    if index > 0 {
                        flush(&mut lines, &mut spans);
                    }
                    push_span(
                        &mut table,
                        &mut spans,
                        Span::styled(segment.to_owned(), style),
                    );
                }
            }
            Event::Code(body) => push_span(
                &mut table,
                &mut spans,
                Span::styled(body.into_string(), style.fg(Color::Yellow)),
            ),
            Event::SoftBreak | Event::HardBreak => {
                flush(&mut lines, &mut spans);
            }
            Event::Rule => {
                flush(&mut lines, &mut spans);
                lines.push(Line::raw("────────"));
            }
            Event::TaskListMarker(checked) => {
                spans.push(Span::raw(if checked { "[x] " } else { "[ ] " }))
            }
            _ => {}
        }
    }
    flush(&mut lines, &mut spans);
    if has_code_block {
        lines
    } else {
        bound_rows(lines, expanded)
    }
}

type MdCell = Vec<Span<'static>>;
type MdRow = Vec<MdCell>;

/// Table cell content, buffered by row so column widths can be measured
/// across the whole table before any row is emitted as a [`Line`].
#[derive(Default)]
struct TableBuf {
    head: MdRow,
    rows: Vec<MdRow>,
    row: MdRow,
    cell: MdCell,
}

/// Route inline content into the open table cell instead of the top-level
/// line buffer, so a link or inline code inside a cell still lines up.
fn push_span(table: &mut Option<TableBuf>, spans: &mut Vec<Span<'static>>, span: Span<'static>) {
    match table {
        Some(t) => t.cell.push(span),
        None => spans.push(span),
    }
}

fn cell_width(cell: &[Span<'static>]) -> usize {
    cell.iter()
        .map(|span| unicode_width::UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

/// Pad every cell out to its column's widest content so `│` separators line
/// up across rows. A row that ends up wider than the viewport is left to wrap
/// like any other transcript line — [`crate::widgets::ScrollableTranscript`]
/// already word-wraps every rendered row, so an overlong table degrades the
/// same way an overlong paragraph does, rather than needing its own
/// truncation or ellipsis rule.
fn render_table(table: TableBuf) -> Vec<Line<'static>> {
    let mut all_rows = Vec::with_capacity(table.rows.len() + 1);
    all_rows.push(table.head);
    all_rows.extend(table.rows);
    let columns = all_rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![0usize; columns];
    for row in &all_rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(cell_width(cell));
        }
    }
    all_rows
        .into_iter()
        .map(|row| {
            let mut spans = Vec::new();
            for (index, cell) in row.into_iter().enumerate() {
                if index > 0 {
                    spans.push(Span::styled(" │ ", Style::default().fg(Color::Cyan)));
                }
                let width = cell_width(&cell);
                spans.extend(cell);
                let pad = widths[index].saturating_sub(width);
                if pad > 0 && index + 1 < columns {
                    spans.push(Span::raw(" ".repeat(pad)));
                }
            }
            Line::from(spans)
        })
        .collect()
}

fn code_lines(body: &str, language: &str, expanded: bool) -> Vec<Line<'static>> {
    let diff = matches!(language, "diff" | "patch");
    let mut lines = vec![Line::styled(
        format!(
            "┌ {}",
            if language.is_empty() {
                "code"
            } else {
                language
            }
        ),
        Style::default().fg(Color::Cyan),
    )];
    let rows: Vec<_> = body
        .strip_suffix('\n')
        .unwrap_or(body)
        .split('\n')
        .collect();
    let limit = if expanded {
        rows.len()
    } else {
        DIFF_PREVIEW_ROWS
    };
    for row in rows.iter().take(limit) {
        let spans = if diff {
            let color = if row.starts_with('+') {
                Color::Green
            } else if row.starts_with('-') {
                Color::Red
            } else if row.starts_with("@@") || row.starts_with("diff ") {
                Color::Cyan
            } else {
                Color::Reset
            };
            vec![Span::styled((*row).to_owned(), Style::default().fg(color))]
        } else {
            highlight(row, language)
        };
        lines.push(Line::from(spans));
    }
    if rows.len() > limit {
        let label = if diff { "diff lines" } else { "lines" };
        lines.push(Line::styled(
            format!("… {} more {label} · Ctrl-O to expand", rows.len() - limit),
            Style::default().fg(Color::Yellow),
        ));
    }
    lines.push(Line::styled("└", Style::default().fg(Color::Cyan)));
    lines
}

/// A bounded lexical highlighter for common snippets. Unknown languages remain
/// plain; no regex engines, grammar loading or ANSI strings enter the draw loop.
fn highlight(line: &str, language: &str) -> Vec<Span<'static>> {
    let keywords: &[&str] = match language {
        "rust" | "rs" => &[
            "fn", "let", "mut", "pub", "use", "impl", "struct", "enum", "match", "if", "else",
            "async", "await", "return", "Self", "self", "true", "false",
        ],
        "python" | "py" => &[
            "def", "class", "import", "from", "if", "else", "elif", "for", "in", "while", "return",
            "async", "await", "True", "False", "None",
        ],
        "javascript" | "js" | "typescript" | "ts" => &[
            "const", "let", "function", "class", "if", "else", "return", "async", "await",
            "export", "import", "true", "false", "null",
        ],
        "json" => &["true", "false", "null"],
        _ => return vec![Span::raw(line.to_owned())],
    };
    let mut spans = Vec::new();
    let mut rest = line;
    while !rest.is_empty() {
        let ch = rest.chars().next().unwrap();
        let (len, color) = if rest.starts_with("//")
            || matches!(language, "python" | "py") && rest.starts_with('#')
        {
            (rest.len(), Color::Gray)
        } else if matches!(ch, '\'' | '"') {
            let mut escaped = false;
            let mut end = rest.len();
            for (offset, c) in rest.char_indices().skip(1) {
                if c == ch && !escaped {
                    end = offset + c.len_utf8();
                    break;
                }
                escaped = c == '\\' && !escaped;
            }
            (end, Color::Green)
        } else if ch.is_alphanumeric() || ch == '_' {
            let len = rest
                .find(|c: char| !c.is_alphanumeric() && c != '_')
                .unwrap_or(rest.len());
            (
                len,
                if keywords.contains(&&rest[..len]) {
                    Color::Magenta
                } else if ch.is_ascii_digit() {
                    Color::Cyan
                } else {
                    Color::Reset
                },
            )
        } else {
            (ch.len_utf8(), Color::Reset)
        };
        spans.push(Span::styled(
            rest[..len].to_owned(),
            Style::default().fg(color),
        ));
        rest = &rest[len..];
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nav::NavIntent;
    use crate::widgets::ScrollableTranscript;
    use ratatui::{Terminal, backend::TestBackend};

    fn text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn screen(lines: Vec<Line<'static>>, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut view = ScrollableTranscript::new();
        terminal
            .draw(|frame| view.render(frame, frame.area(), 1, || lines))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn partial_fences_keep_code_and_whitespace_then_close_without_losing_text() {
        let open = body_lines("```rust\nfn main() {\n    let value = 42;", true, false);
        let closed = body_lines(
            "```rust\nfn main() {\n    let value = 42;\n```",
            true,
            false,
        );
        assert_eq!(text(&open), text(&closed));
        assert!(text(&open).contains("    let value = 42;"));
        assert!(
            open.iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.content == "let" && span.style.fg == Some(Color::Magenta))
        );
        let rendered = screen(open, 50, 8);
        assert!(rendered.contains("    let value = 42;"));
    }

    #[test]
    fn a_growing_partial_fence_crosses_the_fold_budget_without_panicking_or_losing_text() {
        let mut streamed = String::from("```rust\n");
        for i in 0..40 {
            streamed.push_str(&format!("let x{i} = {i};\n"));
            // While still open, the partial fence must render at every
            // length without panicking, whether under or over the budget.
            let partial = body_lines(&streamed, true, false);
            assert!(!text(&partial).is_empty());
        }
        let closed = format!("{streamed}```");
        let full = body_lines(&closed, true, true);
        assert!(text(&full).contains("x39"), "no text lost once it closes");
    }

    #[test]
    fn unknown_language_and_plain_messages_preserve_literal_syntax() {
        let source = "# title\n*literal* `ticks`\n\nlast";
        assert_eq!(text(&body_lines(source, false, false)), source);
        let rendered = body_lines("```unheard-of\n  strange ::= [x]\n```", true, false);
        assert!(text(&rendered).contains("  strange ::= [x]"));
        assert!(screen(rendered, 40, 6).contains("  strange ::= [x]"));
    }

    #[test]
    fn markdown_styles_and_nested_lists_survive_terminal_rendering() {
        let lines = body_lines(
            "# Heading\n\n- outer\n  - inner **bold** and `code`",
            true,
            false,
        );
        let rendered = screen(lines.clone(), 50, 10);
        assert!(rendered.contains("Heading"));
        assert!(rendered.contains("  • inner bold and code"));
        assert!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.content == "bold"
                    && span.style.add_modifier.contains(Modifier::BOLD))
        );
    }

    #[test]
    fn narrow_tables_keep_both_cells_and_do_not_leak_header_style() {
        let lines = body_lines(
            "| Key | Value |\n| --- | --- |\n| a | b |\n\nafter",
            true,
            false,
        );
        assert!(text(&lines).contains("Key │ Value"));
        assert!(text(&lines).contains("a   │ b"));
        assert!(
            !lines.last().unwrap().spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        let rendered = screen(lines, 9, 12);
        for word in ["Key", "Value", "a", "b", "after"] {
            assert!(rendered.contains(word), "{rendered:?}");
        }
    }

    #[test]
    fn table_columns_align_by_widest_cell_in_each_column() {
        let lines = body_lines(
            "| Key | Value |\n| --- | --- |\n| a | wide value |\n| longer key | v |",
            true,
            false,
        );
        let rendered = text(&lines);
        let bar_columns: Vec<usize> = rendered
            .lines()
            .filter(|line| line.contains('│'))
            .map(|line| line.find('│').unwrap())
            .collect();
        assert_eq!(bar_columns.len(), 3, "{rendered:?}");
        assert!(
            bar_columns.windows(2).all(|pair| pair[0] == pair[1]),
            "columns did not line up: {rendered:?}"
        );
        assert!(rendered.contains("wide value"));
        assert!(rendered.contains("longer key"));
    }

    #[test]
    fn markdown_links_render_text_and_target_together() {
        let lines = body_lines(
            "See [the docs](https://example.com/path) for more",
            true,
            false,
        );
        let rendered = text(&lines);
        assert!(
            rendered.contains("the docs (https://example.com/path)"),
            "{rendered:?}"
        );
        assert!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.content == "the docs"
                    && span.style.add_modifier.contains(Modifier::UNDERLINED))
        );
    }

    #[test]
    fn wide_tables_wrap_in_a_narrow_viewport_without_dropping_or_panicking() {
        let lines = body_lines(
            "| Column One | Column Two | Column Three |\n| --- | --- | --- |\n| aaaaaaaaaa | bbbbbbbbbb | cccccccccc |",
            true,
            false,
        );
        let rendered = screen(lines, 12, 30);
        for word in [
            "Column",
            "One",
            "Two",
            "Three",
            "aaaaaaaaaa",
            "bbbbbbbbbb",
            "cccccccccc",
        ] {
            assert!(rendered.contains(word), "{rendered:?}");
        }
    }

    #[test]
    fn diff_preview_names_files_and_expands_all_normal_sized_hunks() {
        let source = format!(
            "--- a/file.rs\n+++ b/file.rs\n@@ -1 +1 @@\n-old\n{}",
            (0..40).map(|i| format!("+new{i}\n")).collect::<String>()
        );
        let preview = body_lines(&source, true, false);
        assert!(text(&preview).contains("a/file.rs"));
        assert!(text(&preview).contains("Ctrl-O to expand"));
        let full = body_lines(&source, true, true);
        assert!(text(&full).contains("+new39"));
        assert!(
            full.iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.content == "-old" && span.style.fg == Some(Color::Red))
        );
        assert!(screen(full, 45, 55).contains("+new39"));
    }

    #[test]
    fn a_long_non_diff_fenced_block_folds_and_expands_like_a_diff() {
        let body = (0..40)
            .map(|i| format!("let x{i} = {i};\n"))
            .collect::<String>();
        let source = format!("```rust\n{body}```");
        let preview = body_lines(&source, true, false);
        assert!(text(&preview).contains("Ctrl-O to expand"));
        assert!(
            !text(&preview).contains("x39"),
            "the tail must stay hidden behind the fold: {}",
            text(&preview)
        );
        let full = body_lines(&source, true, true);
        assert!(text(&full).contains("x39"));
        assert!(
            full.iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.content == "let" && span.style.fg == Some(Color::Magenta)),
            "an expanded non-diff block keeps its highlighting"
        );
    }

    #[test]
    fn a_long_plain_prose_message_folds_and_expands() {
        let source = (0..40)
            .map(|i| format!("Paragraph number {i}.\n\n"))
            .collect::<String>();
        let preview = body_lines(&source, true, false);
        assert!(text(&preview).contains("Ctrl-O to expand"));
        assert!(!text(&preview).contains("Paragraph number 39."));
        let full = body_lines(&source, true, true);
        assert!(text(&full).contains("Paragraph number 39."));
    }

    #[test]
    fn a_message_with_a_short_fenced_block_and_short_prose_is_not_folded() {
        // Neither the block nor the surrounding prose alone crosses the
        // budget, and the two folds are independent rather than stacked, so
        // the whole message renders without a Ctrl-O affordance.
        let source = "Here is the fix:\n\n```rust\nlet x = 1;\n```\n\nDone.";
        let rendered = body_lines(source, true, false);
        assert!(!text(&rendered).contains("Ctrl-O to expand"));
        assert!(text(&rendered).contains("Done."));
    }

    #[test]
    fn pathological_unicode_lines_are_bounded_and_controls_neutralized() {
        let source = format!(
            "```rust\n{}\u{1b}[31m\u{9b}bad\u{202e}",
            "世".repeat(MAX_BYTES)
        );
        let lines = body_lines(&source, true, true);
        assert!(text(&lines).contains("rendering limited"));
        assert!(text(&lines).len() < MAX_BYTES + 200);
        let controls = body_lines("a\u{1b}[2J\u{7}\u{9b}\u{202e}b\tend", false, false);
        assert!(!text(&controls).chars().any(char::is_control));
        assert!(screen(controls, 40, 4).contains("b"));
        // Long tokens wrap on the real backend without dropping their tail.
        assert!(screen(body_lines(&"x".repeat(400), false, false), 12, 4).contains("xxxx"));
    }

    #[test]
    fn streaming_growth_does_not_steal_a_scrolled_viewport() {
        let mut terminal = Terminal::new(TestBackend::new(30, 5)).unwrap();
        let mut view = ScrollableTranscript::new();
        let source = (0..20).map(|i| format!("row {i}\n")).collect::<String>();
        terminal
            .draw(|frame| view.render(frame, frame.area(), 1, || body_lines(&source, true, false)))
            .unwrap();
        view.apply(NavIntent::PageUp);
        let offset = view.offset();
        let following = format!("{source}```rust\nfn more() {{");
        terminal
            .draw(|frame| {
                view.render(frame, frame.area(), 2, || {
                    body_lines(&following, true, false)
                })
            })
            .unwrap();
        assert_eq!(view.offset(), offset);
        assert!(!view.is_following());
    }
    #[test]
    fn approval_commands_stay_literal_and_prefixed_diffs_are_styled() {
        let command = "apply edit to src/main.rs\n+ let x = 1;\nrm *.tmp";
        assert_eq!(text(&preview_lines(command, false)), command);
        let diff =
            "Edited src/main.rs:\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-old\n+new";
        let lines = preview_lines(diff, false);
        assert_eq!(lines[0].to_string(), "Edited src/main.rs:");
        assert!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.content == "+new" && span.style.fg == Some(Color::Green))
        );
        let rendered = screen(lines, 50, 12);
        assert!(rendered.contains("--- a/src/main.rs"));
        assert!(rendered.contains("+new"));
    }
}
