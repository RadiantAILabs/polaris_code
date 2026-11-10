//! Terminal-safe text shared by dialogs and panels.

/// Remove terminal controls before rendering, including C1 and bidi overrides.
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
                || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' | '\u{200b}' | '\u{2028}' | '\u{2029}' | '\u{feff}') =>
                {}
            ch => {
                result.push(ch);
                column += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_direction_and_invisible_controls_while_preserving_layout() {
        assert_eq!(
            safe_text("a\u{202e}\u{2066}\u{200b}\u{2028}\u{2029}\u{feff}\u{1b}\ttext\n\tline"),
            "a   text\n    line",
            "unsafe controls disappear while tabs and newlines retain layout"
        );
    }
}
