//! The message input line: text plus a cursor, with editing operations.
//!
//! Tracks the cursor as a *character* index (not a byte offset) so multi-byte
//! UTF-8 never splits mid-character, and renders the cursor's *screen column*
//! via `unicode-width` so wide glyphs (CJK, emoji) line the terminal cursor up
//! with the glyph the user is actually editing.

use unicode_width::UnicodeWidthStr;

/// An editable single-line input with a cursor.
#[derive(Default)]
pub struct Input {
    /// The current text.
    chars: Vec<char>,
    /// Cursor position as a character index in `0..=chars.len()`.
    cursor: usize,
}

impl Input {
    /// The current text as a `String`.
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    /// The trimmed text, for "is there anything to send" checks.
    pub fn trimmed(&self) -> String {
        self.text().trim().to_owned()
    }

    /// Clear the text and reset the cursor.
    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
    }

    /// Insert `c` at the cursor and advance past it.
    pub fn insert(&mut self, c: char) {
        self.chars.insert(self.cursor, c);
        self.cursor += 1;
    }

    /// Insert a newline at the cursor (multi-line input, e.g. Shift/Alt+Enter).
    pub fn newline(&mut self) {
        self.insert('\n');
    }

    /// Delete the character before the cursor (Backspace).
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.chars.remove(self.cursor);
        }
    }

    /// Delete the character at the cursor (Delete).
    pub fn delete(&mut self) {
        if self.cursor < self.chars.len() {
            self.chars.remove(self.cursor);
        }
    }

    /// Move the cursor one character left.
    pub const fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Move the cursor one character right.
    pub const fn right(&mut self) {
        if self.cursor < self.chars.len() {
            self.cursor += 1;
        }
    }

    /// The cursor's screen position as `(row, col)` within the (possibly
    /// multi-line) text. `col` is the display width of the text after the last
    /// newline before the cursor, so wide glyphs (CJK, emoji) count as two and
    /// the terminal cursor lands on the glyph being edited.
    pub fn cursor_pos(&self) -> (u16, u16) {
        let before = &self.chars[..self.cursor];
        let row = before.iter().filter(|&&c| c == '\n').count();
        let col_start = before.iter().rposition(|&c| c == '\n').map_or(0, |i| i + 1);
        let line: String = before[col_start..].iter().collect();
        (
            u16::try_from(row).unwrap_or(u16::MAX),
            u16::try_from(line.width()).unwrap_or(u16::MAX),
        )
    }

    /// The number of text rows (1 + newline count), for sizing the input box.
    pub fn line_count(&self) -> u16 {
        let newlines = self.chars.iter().filter(|&&c| c == '\n').count();
        u16::try_from(newlines + 1).unwrap_or(u16::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(s: &str) -> Input {
        let mut i = Input::default();
        for c in s.chars() {
            i.insert(c);
        }
        i
    }

    /// Typing appends and leaves the cursor at the end; the text reads back.
    #[test]
    fn typing_appends_and_advances_cursor() {
        let i = typed("hi");
        assert_eq!(i.text(), "hi");
        assert_eq!(i.cursor, 2);
        assert_eq!(i.cursor_pos(), (0, 2));
    }

    /// Left then insert puts the new character before the cursor position, not
    /// at the end — verifying mid-string editing, not just append.
    #[test]
    fn insert_in_the_middle() {
        let mut i = typed("ac");
        i.left(); // between a and c
        i.insert('b');
        assert_eq!(i.text(), "abc");
        assert_eq!(i.cursor, 2);
    }

    /// Backspace removes the char before the cursor; Delete removes the char at
    /// it. Both clamp at the ends instead of panicking.
    #[test]
    fn backspace_and_delete_clamp_at_edges() {
        let mut i = typed("ab");
        i.left();
        i.left(); // at the start
        i.backspace(); // nothing before the start
        assert_eq!(i.text(), "ab");
        i.delete(); // removes 'a'
        assert_eq!(i.text(), "b");
        i.right();
        i.delete(); // nothing after the end
        assert_eq!(i.text(), "b");
    }

    /// `right()` never runs past the end; `left()` never past the start.
    #[test]
    fn left_right_clamp() {
        let mut i = typed("abc");
        i.left();
        i.left();
        i.left();
        i.left(); // one past the start is ignored
        assert_eq!(i.cursor, 0);
        i.right();
        i.right();
        i.right();
        i.right(); // one past the end is ignored
        assert_eq!(i.cursor, 3);
    }

    /// A wide (CJK) glyph counts as two screen columns, so the cursor column is
    /// the display width, not the character count. This keeps the terminal
    /// cursor aligned with wide glyphs.
    #[test]
    fn wide_glyph_counts_two_columns() {
        let mut i = Input::default();
        i.insert('中'); // width 2
        i.insert('x'); // width 1
        assert_eq!(i.cursor, 2, "two characters");
        assert_eq!(i.cursor_pos(), (0, 3), "but three screen columns");
    }

    /// A newline moves the cursor to the next row, and the column resets to the
    /// width of the text after the newline — so multi-line input positions the
    /// cursor correctly.
    #[test]
    fn newline_advances_row_and_resets_column() {
        let mut i = typed("ab");
        i.newline();
        i.insert('c');
        assert_eq!(i.text(), "ab\nc");
        assert_eq!(i.line_count(), 2);
        assert_eq!(i.cursor_pos(), (1, 1), "second row, one column in");
    }
}
