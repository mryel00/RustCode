//! Conversions between LSP positions and rope indices.
//!
//! LSP and Monaco both address text as (line, UTF-16 code unit offset), so every
//! edit that crosses the UI/backend/server boundary uses that encoding. The rope
//! stores `char`s, so all conversions go through ropey's UTF-16 index.

use lsp_types::{Position, TextDocumentContentChangeEvent};
use ropey::{Rope, RopeSlice};

/// Converts an LSP position to a char index in `rope`.
///
/// Follows the LSP spec for out-of-range input: a character offset past the end
/// of the line resolves to the end of the line (before its line break), and a
/// line past the end of the document resolves to the end of the document.
pub fn position_to_char(rope: &Rope, pos: Position) -> usize {
    let line = pos.line as usize;
    if line >= rope.len_lines() {
        return rope.len_chars();
    }
    let line_start = rope.line_to_char(line);
    let content_end = line_start + line_content_len(rope.line(line));
    let start_cu = rope.char_to_utf16_cu(line_start);
    let end_cu = rope.char_to_utf16_cu(content_end);
    let target_cu = (start_cu + pos.character as usize).min(end_cu);
    rope.utf16_cu_to_char(target_cu)
}

/// Converts a char index in `rope` to an LSP position (UTF-16 columns).
pub fn char_to_position(rope: &Rope, char_idx: usize) -> Position {
    let char_idx = char_idx.min(rope.len_chars());
    let line = rope.char_to_line(char_idx);
    let line_start = rope.line_to_char(line);
    let character = rope.char_to_utf16_cu(char_idx) - rope.char_to_utf16_cu(line_start);
    Position::new(line as u32, character as u32)
}

/// Applies one LSP content change to `rope`. A change without a range replaces
/// the whole document.
pub fn apply_change(rope: &mut Rope, change: &TextDocumentContentChangeEvent) {
    match change.range {
        None => *rope = Rope::from_str(&change.text),
        Some(range) => {
            let a = position_to_char(rope, range.start);
            let b = position_to_char(rope, range.end);
            let (start, end) = if a <= b { (a, b) } else { (b, a) };
            rope.remove(start..end);
            rope.insert(start, &change.text);
        }
    }
}

/// Applies changes in order; each change is relative to the result of the previous one.
pub fn apply_changes(rope: &mut Rope, changes: &[TextDocumentContentChangeEvent]) {
    for change in changes {
        apply_change(rope, change);
    }
}

/// Number of chars on a line, excluding its line break (`\n`, `\r\n` or `\r`).
fn line_content_len(line: RopeSlice) -> usize {
    let mut len = line.len_chars();
    if len > 0 && line.char(len - 1) == '\n' {
        len -= 1;
        if len > 0 && line.char(len - 1) == '\r' {
            len -= 1;
        }
    } else if len > 0 && line.char(len - 1) == '\r' {
        len -= 1;
    }
    len
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::Range;

    fn change(sl: u32, sc: u32, el: u32, ec: u32, text: &str) -> TextDocumentContentChangeEvent {
        TextDocumentContentChangeEvent {
            range: Some(Range::new(Position::new(sl, sc), Position::new(el, ec))),
            range_length: None,
            text: text.into(),
        }
    }

    #[test]
    fn utf16_columns_with_surrogate_pairs() {
        // "😀" is one char but two UTF-16 code units.
        let rope = Rope::from_str("a😀b\nxy");
        assert_eq!(position_to_char(&rope, Position::new(0, 0)), 0);
        assert_eq!(position_to_char(&rope, Position::new(0, 1)), 1);
        assert_eq!(position_to_char(&rope, Position::new(0, 3)), 2);
        assert_eq!(position_to_char(&rope, Position::new(0, 4)), 3);
        assert_eq!(char_to_position(&rope, 3), Position::new(0, 4));
        assert_eq!(char_to_position(&rope, 4), Position::new(1, 0));
        assert_eq!(char_to_position(&rope, 6), Position::new(1, 2));
    }

    #[test]
    fn clamps_out_of_range_positions() {
        let rope = Rope::from_str("abc\r\ndef");
        // Past end of line stops before the CRLF.
        assert_eq!(position_to_char(&rope, Position::new(0, 99)), 3);
        // Past end of document.
        assert_eq!(position_to_char(&rope, Position::new(9, 0)), rope.len_chars());
    }

    #[test]
    fn line_breaks_match_lsp() {
        // Only \n, \r\n and \r are line breaks; U+2028 is not.
        let rope = Rope::from_str("a\rb\r\nc\u{2028}d\ne");
        assert_eq!(rope.len_lines(), 4);
        assert_eq!(position_to_char(&rope, Position::new(2, 3)), 8);
    }

    #[test]
    fn applies_incremental_changes_in_order() {
        let mut rope = Rope::from_str("fn main() {\n}\n");
        apply_changes(
            &mut rope,
            &[
                change(0, 11, 0, 11, "\n    let x = 1;"),
                change(1, 8, 1, 9, "answer"),
            ],
        );
        assert_eq!(rope.to_string(), "fn main() {\n    let answer = 1;\n}\n");
    }

    #[test]
    fn full_replacement() {
        let mut rope = Rope::from_str("old");
        apply_change(
            &mut rope,
            &TextDocumentContentChangeEvent { range: None, range_length: None, text: "new".into() },
        );
        assert_eq!(rope.to_string(), "new");
    }

    #[test]
    fn deletes_across_lines_and_emoji() {
        let mut rope = Rope::from_str("x😀y\nzz\n");
        apply_change(&mut rope, &change(0, 1, 1, 1, ""));
        assert_eq!(rope.to_string(), "xz\n");
    }
}
