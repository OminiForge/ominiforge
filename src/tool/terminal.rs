//! A minimal terminal screen model for streaming `shell` output
//! (`doc/tool-streaming.md` §5).
//!
//! Unlike `write`/`edit` (whose streamed args only ever grow), a shell command
//! may REWRITE its output in place: progress bars overwrite the current line
//! with `\r`, spinners redraw a frame, full-screen TUIs move the cursor and
//! clear regions. Appending those bytes verbatim would pile control sequences
//! and stale frames into an unreadable mess. So the streamed `terminal` view
//! renders **the current screen**, not the concatenation of everything seen —
//! fed byte chunks, this model maintains a screen buffer and answers "what
//! does the display look like now", self-contained per snapshot.
//!
//! Scope is deliberately minimal — a line-oriented model handling `\n`, `\r`,
//! and the common ANSI/VT sequences (erase display/line, cursor up/down/
//! forward/back/to-column, CR/LF). Ordinary commands (no control sequences)
//! behave as plain growth; panel-style commands behave as in-place refresh.
//! Full-screen TUIs with complex addressing degrade to "roughly readable",
//! not pixel-faithful — a real VT100 emulator is a separate, larger effort.

/// One screen: a sparse grid of lines plus a cursor. Lines are trimmed of
/// trailing blanks when rendered.
pub struct Terminal {
    lines: Vec<Vec<char>>,
    /// Cursor position: row indexes `lines`, col is a cell offset.
    row: usize,
    col: usize,
    /// Parser state for an in-progress escape sequence (`ESC` seen, params
    /// accumulating). Bytes of a partial sequence are held here, not shown.
    escape: Option<String>,
}

impl Terminal {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            lines: Vec::new(),
            row: 0,
            col: 0,
            escape: None,
        }
    }

    /// Feed one chunk of raw output (may split a UTF-8 char or an escape
    /// sequence across chunks; both are handled — lossy decode is fine for a
    /// display model, and a partial escape is held until it completes).
    pub fn feed(&mut self, bytes: &[u8]) {
        for ch in String::from_utf8_lossy(bytes).chars() {
            self.put(ch);
        }
    }

    /// The current screen as display text: lines joined by `\n`, trailing
    /// blank lines dropped, each line trimmed of trailing spaces. This is the
    /// self-contained snapshot the `terminal` view carries.
    #[must_use]
    pub fn screen(&self) -> String {
        let mut lines: Vec<String> = self
            .lines
            .iter()
            .map(|l| l.iter().collect::<String>().trim_end().to_owned())
            .collect();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines.join("\n")
    }

    fn put(&mut self, ch: char) {
        // Inside an escape sequence: accumulate until the final byte.
        if self.escape.is_some() {
            if ch.is_ascii_alphabetic() {
                // `escape` is Some (checked above), so `take` always yields it.
                if let Some(finished) = self.escape.take() {
                    self.apply_escape(&finished, ch);
                }
            } else if let Some(seq) = &mut self.escape {
                seq.push(ch);
            }
            return;
        }
        match ch {
            '\x1b' => self.escape = Some(String::new()),
            '\n' => {
                self.row += 1;
                self.col = 0;
            }
            '\r' => self.col = 0,
            '\t' => {
                let next = (self.col / 8 + 1) * 8;
                for _ in self.col..next {
                    self.write_cell(' ');
                }
            }
            // Other C0 controls (bell, backspace-as-noop, etc.): ignored.
            c if c.is_control() => {}
            c => {
                self.write_cell(c);
                self.col += 1;
            }
        }
    }

    /// Write one printable char at the cursor, growing the grid as needed.
    fn write_cell(&mut self, c: char) {
        while self.lines.len() <= self.row {
            self.lines.push(Vec::new());
        }
        let line = &mut self.lines[self.row];
        while line.len() <= self.col {
            line.push(' ');
        }
        line[self.col] = c;
    }

    /// Apply a finished `ESC [ params final` sequence. Only the common subset
    /// is honored; anything else is ignored (dropped) so it never shows.
    fn apply_escape(&mut self, seq: &str, final_byte: char) {
        // Strip a leading '[' (CSI) — the only introducer we handle.
        let Some(params) = seq.strip_prefix('[') else {
            return;
        };
        let nums: Vec<usize> = params
            .split(';')
            .map(|p| p.parse().unwrap_or(0))
            .collect();
        // A missing or zero count defaults to 1 (`ESC[A` = up one row).
        let n = |i: usize| nums.get(i).copied().unwrap_or(0).max(1);
        match final_byte {
            // Cursor up / down / forward / back.
            'A' => self.row = self.row.saturating_sub(n(0)),
            'B' => self.row += n(0),
            'C' => self.col += n(0),
            'D' => self.col = self.col.saturating_sub(n(0)),
            // Cursor to column (1-based).
            'G' => self.col = n(0).saturating_sub(1),
            // Cursor to row,col (1-based).
            'H' | 'f' => {
                self.row = n(0).saturating_sub(1);
                self.col = n(1).saturating_sub(1);
            }
            // Erase in display: 2 (or 3) = whole screen.
            'J' if nums.first().copied().unwrap_or(0) >= 2 => {
                self.lines.clear();
                self.row = 0;
                self.col = 0;
            }
            // Erase in line: 2 = whole line, 0 = to end of line.
            'K' if self.row < self.lines.len() => {
                let mode = nums.first().copied().unwrap_or(0);
                let line = &mut self.lines[self.row];
                if mode == 2 {
                    line.clear();
                } else {
                    line.truncate(self.col.min(line.len()));
                }
            }
            _ => {}
        }
    }
}

impl Default for Terminal {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn fed(chunks: &[&str]) -> Terminal {
        let mut t = Terminal::new();
        for c in chunks {
            t.feed(c.as_bytes());
        }
        t
    }

    #[test]
    fn plain_output_grows() {
        let t = fed(&["hello\nworld\n"]);
        assert_eq!(t.screen(), "hello\nworld");
    }

    #[test]
    fn carriage_return_overwrites_the_line() {
        // A progress bar: "10%" then \r then "20%" — only the latest shows.
        let t = fed(&["progress: 10%\rprogress: 20%\n"]);
        assert_eq!(t.screen(), "progress: 20%");
    }

    #[test]
    fn spinner_redraws_in_place() {
        // `-` `\` `|` `/` spinner via \r (same-width frames), then a clear-line
        // + done message. Real spinners pad/erase so no residue shows.
        let t = fed(&["working -\rworking \\\rworking |\rworking /\r\x1b[2Kdone\n"]);
        assert_eq!(t.screen(), "done");
    }

    #[test]
    fn carriage_return_does_not_erase_beyond_the_new_text() {
        // `\r` only returns the cursor; a shorter overwrite leaves the tail.
        // (This is why real redraws pad or send erase-line — see above.)
        let t = fed(&["aaaa\rbb\n"]);
        assert_eq!(t.screen(), "bbaa");
    }

    #[test]
    fn clear_screen_resets() {
        // A panel redraw: old content, ESC[2J ESC[H, new frame.
        let t = fed(&["frame one\nframe two\n\x1b[2J\x1b[Hfresh\n"]);
        assert_eq!(t.screen(), "fresh");
    }

    #[test]
    fn cursor_up_and_rewrite() {
        // Rewrite the previous line: "abc\n", cursor up + erase line, "XYZ".
        let t = fed(&["abc\n\x1b[1A\x1b[2KXYZ\n"]);
        assert_eq!(t.screen(), "XYZ");
    }

    #[test]
    fn escape_split_across_chunks_is_held() {
        // The ESC[2J sequence arrives split across two feeds.
        let mut t = Terminal::new();
        t.feed(b"old\n\x1b[");
        t.feed(b"2Jnew\n");
        assert_eq!(t.screen(), "new");
    }

    #[test]
    fn partial_utf8_across_chunks_decodes() {
        let t = fed(&["caf", "é\n"]);
        assert_eq!(t.screen(), "café");
    }

    #[test]
    fn trailing_blank_lines_trimmed() {
        let t = fed(&["a\n\n\n"]);
        assert_eq!(t.screen(), "a");
    }
}
