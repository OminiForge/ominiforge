//! Incremental extraction of tool-call arguments from a partial JSON stream.
//!
//! A tool call's args arrive as a growing string of JSON fragments (the
//! provider streams them token by token). The streaming presenters
//! (`doc/tool-streaming.md` §4) need answers from that *incomplete* text:
//! which top-level fields are finished, and what is the received prefix of a
//! string field still streaming. This is NOT a general partial-JSON parser —
//! tool arg schemas are flat and known, so a small hand-rolled scanner over
//! the accumulated text answers exactly those two questions and nothing more.
//!
//! Feed each new snapshot of the full accumulated args to [`PartialArgs::new`]
//! (the input is always the complete text so far, never a delta), then query
//! the fields you care about. The scanner is tolerant of truncation at any
//! byte — an unterminated string, an open escape, a half-written number all
//! simply mean "not complete yet".

/// A scanned snapshot of one tool call's accumulated args.
///
/// Construct with [`PartialArgs::new`], then query. Cheap enough to rebuild on
/// every throttled frame: the scan is a single left-to-right pass.
pub struct PartialArgs<'a> {
    text: &'a str,
}

impl<'a> PartialArgs<'a> {
    /// Scan one accumulated-args snapshot.
    #[must_use]
    pub const fn new(text: &'a str) -> Self {
        Self { text }
    }

    /// The raw (still JSON-escaped) value of a top-level string field once its
    /// opening quote is seen, whether or not it has closed yet. `None` if the
    /// field hasn't started. The returned slice excludes the surrounding
    /// quotes; it may be a prefix of the final value if the string is still
    /// streaming.
    fn raw_string_prefix(&self, field: &str) -> Option<&'a str> {
        let key = format!("\"{field}\"");
        let key_at = self.text.find(&key)?;
        let after_key = &self.text[key_at + key.len()..];
        // Skip whitespace and the colon between key and value.
        let after_colon = after_key.trim_start().strip_prefix(':')?;
        let value_start = after_colon.trim_start();
        // The value must be a string (opening quote) for us to read it.
        let body = value_start.strip_prefix('"')?;
        Some(body)
    }

    /// The decoded value of a top-level boolean field once its literal
    /// (`true`/`false`) is seen. `None` if absent, not a bool, or truncated.
    /// Used for `edit`'s `replace_all`.
    #[must_use]
    pub fn complete_bool(&self, field: &str) -> Option<bool> {
        let key = format!("\"{field}\"");
        let key_at = self.text.find(&key)?;
        let after_key = &self.text[key_at + key.len()..];
        let after_colon = after_key.trim_start().strip_prefix(':')?;
        let value = after_colon.trim_start();
        if value.starts_with("true") {
            Some(true)
        } else if value.starts_with("false") {
            Some(false)
        } else {
            None
        }
    }

    /// The decoded value of a top-level string field **once fully closed**.
    /// `None` while the field is absent, unterminated, or not a string. Use
    /// this for fields the presenter must wait on before acting (e.g. `path`).
    #[must_use]
    pub fn complete_string(&self, field: &str) -> Option<String> {
        let body = self.raw_string_prefix(field)?;
        let end = closing_quote(body)?;
        let raw = &body[..end];
        decode_json_string(raw).ok()
    }

    /// The decoded **received prefix** of a top-level string field, including
    /// while it is still streaming (unterminated). `None` only if the field
    /// hasn't started or isn't a string. Use this for the big streamed payload
    /// (e.g. `content`): it grows as tokens arrive, and the presenter renders
    /// the growth.
    ///
    /// Decoding stops at the last *complete* JSON escape; a trailing partial
    /// escape (`\`, `\u12`) is dropped rather than misdecoded.
    #[must_use]
    pub fn streaming_string(&self, field: &str) -> Option<String> {
        let body = self.raw_string_prefix(field)?;
        // Take up to the closing quote if present, else the whole remainder.
        let raw = closing_quote(body).map_or(body, |end| &body[..end]);
        decode_json_string(raw).ok()
    }

    /// Split the `edits` array into its entry substrings: every fully-closed
    /// `{...}` entry (its braces balanced) as a parseable JSON slice, plus the
    /// trailing open entry — if any — as a [`PartialArgs`] for field-by-field
    /// extraction. `None` if the `edits` array hasn't opened. Used by the
    /// `edit` presenter (`doc/tool-streaming.md`): closed entries parse
    /// wholesale with serde; only the one still streaming needs partial reads.
    #[must_use]
    pub fn edit_entries(&self) -> Option<(Vec<&'a str>, Option<Self>)> {
        let array = self.array_body("edits")?;
        let mut closed = Vec::new();
        let mut open = None;
        // Walk the array body, carving out each top-level `{...}` object.
        let bytes = array.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'{' {
                if let Some(len) = balanced_object(&array[i..]) {
                    // A complete object: take it, skip past it.
                    closed.push(&array[i..i + len]);
                    i += len;
                } else {
                    // An unterminated object: the open entry, from here to the
                    // end of the array body.
                    open = Some(PartialArgs::new(&array[i..]));
                    break;
                }
            } else {
                i += 1;
            }
        }
        Some((closed, open))
    }

    /// The body of a top-level array field, between its `[` and the matching
    /// `]` (string-aware, so a `]` inside a string doesn't end it early), or
    /// to end-of-text if the array is still open. `None` if the field is
    /// absent or not an array. Bounding at the real `]` matters: without it a
    /// closed array's scan would run on into the NEXT field's key and elements.
    fn array_body(&self, field: &str) -> Option<&'a str> {
        let key = format!("\"{field}\"");
        let key_at = self.text.find(&key)?;
        let after_key = &self.text[key_at + key.len()..];
        let after_colon = after_key.trim_start().strip_prefix(':')?;
        let body = after_colon.trim_start().strip_prefix('[')?;
        Some(up_to_matching_bracket(body))
    }

    /// The decoded string elements of an array field **once the array is fully
    /// closed** (`edit`'s `old`, when the anchor must be complete before it is
    /// located). `None` while the array is absent or still open. Accepts the
    /// single-string form too (always "closed" once its quote closes).
    #[must_use]
    pub fn complete_lines(&self, field: &str) -> Option<Vec<String>> {
        // Single-string form: complete once the string closes.
        if self.raw_string_prefix(field).is_some() {
            return self.complete_string(field).map(|s| split_lines(&s));
        }
        let body = self.array_body(field)?;
        // The array is closed iff array_body stopped at a real `]` (not
        // end-of-text): re-scan to tell which. If the whole remainder is the
        // body, the array never closed.
        // (`up_to_matching_bracket` returns the full slice exactly when no
        // closing bracket was found.)
        let full = self.raw_array_remainder(field)?;
        if full.len() == body.len() {
            return None; // ran to end-of-text: still open
        }
        self.streaming_lines(field)
    }

    /// The unbounded remainder after a top-level array field's `[` (no bracket
    /// bounding) — used to tell a closed array from one that ran to end-of-text.
    fn raw_array_remainder(&self, field: &str) -> Option<&'a str> {
        let key = format!("\"{field}\"");
        let key_at = self.text.find(&key)?;
        let after_key = &self.text[key_at + key.len()..];
        let after_colon = after_key.trim_start().strip_prefix(':')?;
        after_colon.trim_start().strip_prefix('[')
    }

    /// The decoded string elements of an array field (`edit`'s `old`/`new`):
    /// every COMPLETE element, in order, whether or not the array (or its last
    /// string) has closed. An element still streaming is excluded — only
    /// fully-closed `"..."` items count, so the result is always a clean line
    /// set. `None` if the field is absent. Also accepts the single-string form
    /// (`"old": "l1\nl2"`), split on newlines, mirroring `edit`'s
    /// `string_or_lines` normalization.
    #[must_use]
    pub fn streaming_lines(&self, field: &str) -> Option<Vec<String>> {
        // Single-string form first (a string value, not an array).
        if let Some(s) = self.streaming_string(field) {
            return Some(split_lines(&s));
        }
        let body = self.array_body(field)?;
        let mut lines = Vec::new();
        let bytes = body.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'"' {
                // Candidate element. Find its unescaped closing quote.
                let rest = &body[i + 1..];
                match closing_quote(rest) {
                    Some(end) => {
                        if let Ok(s) = decode_json_string(&rest[..end]) {
                            // An element may embed newlines (a pasted block) —
                            // split like `edit`'s normalization does.
                            lines.extend(split_lines(&s));
                        }
                        i += 1 + end + 1; // past opening quote, body, closing quote
                    }
                    // Unterminated string: the element still streaming — stop;
                    // it isn't a complete line yet.
                    None => break,
                }
            } else {
                i += 1;
            }
        }
        Some(lines)
    }
}

/// Split a multi-line string the way `edit`'s `split_lines` normalization
/// does: one item per line, stripping a trailing `\r` (CRLF tolerance) and the
/// phantom empty piece a final newline leaves. Blank lines in the middle are
/// real content and preserved.
fn split_lines(s: &str) -> Vec<String> {
    let mut pieces: Vec<&str> = s.split('\n').collect();
    if pieces.last() == Some(&"") {
        pieces.pop();
    }
    pieces
        .into_iter()
        .map(|p| p.strip_suffix('\r').unwrap_or(p).to_owned())
        .collect()
}

/// The slice of an array body up to (excluding) its matching `]`, or the
/// whole body if the array is unterminated. String-aware: a `]` inside a
/// string literal doesn't close the array. Nested arrays count toward depth.
fn up_to_matching_bracket(body: &str) -> &str {
    let bytes = body.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' => i += 2,
                        b'"' => break,
                        _ => i += 1,
                    }
                }
                i += 1;
            }
            b'[' => {
                depth += 1;
                i += 1;
            }
            b']' => {
                if depth == 0 {
                    return &body[..i];
                }
                depth -= 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    body
}

/// The length in bytes of the balanced `{...}` object starting at `text[0]`
/// (which must be `{`), or `None` if it is unterminated. String-aware: braces
/// inside string literals don't count toward nesting.
fn balanced_object(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                // Skip the whole string literal (opening quote consumed by the
                // +1; scan for its unescaped closing quote).
                i += 1;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' => i += 2,
                        b'"' => break,
                        _ => i += 1,
                    }
                }
                // `i` is at the closing quote (or ran off the end); the +1
                // below moves past it.
                i += 1;
            }
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                depth -= 1;
                i += 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// The byte index just past a string body's closing quote, i.e. the position
/// of the unescaped `"` that ends it. `None` if the body is unterminated.
fn closing_quote(body: &str) -> Option<usize> {
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2, // skip the escaped char (may run past end on truncation)
            b'"' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// Decode a JSON string body (no surrounding quotes), tolerating a truncated
/// trailing escape by dropping it. Returns `Err` only on a structurally
/// invalid escape we can't recover from.
fn decode_json_string(raw: &str) -> Result<String, ()> {
    // Fast path: no escapes at all.
    if !raw.contains('\\') {
        return Ok(raw.to_owned());
    }
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        // A trailing lone backslash (truncated escape) is dropped, not an
        // error — the escape completes on the next snapshot.
        let Some(esc) = chars.next() else { break };
        match esc {
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            '/' => out.push('/'),
            'b' => out.push('\u{0008}'),
            'f' => out.push('\u{000C}'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'u' => {
                // \uXXXX: read up to 4 hex digits. A TRUNCATED sequence (fewer
                // than 4 hex chars before the text ends) is dropped — it will
                // re-decode correctly once complete on a later snapshot. Only
                // a genuinely invalid (non-hex) digit is an error.
                let mut code = 0u32;
                let mut read = 0;
                while read < 4 {
                    match chars.next() {
                        Some(h) => match h.to_digit(16) {
                            Some(d) => {
                                code = code * 16 + d;
                                read += 1;
                            }
                            None => return Err(()), // non-hex in \u: invalid
                        },
                        None => break, // truncated \u: drop it
                    }
                }
                if read == 4
                    && let Some(ch) = char::from_u32(code)
                {
                    out.push(ch);
                }
            }
            _ => return Err(()), // invalid escape
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn complete_string_waits_for_close() {
        let p = PartialArgs::new(r#"{"path": "src/a.rs""#);
        assert_eq!(p.complete_string("path"), Some("src/a.rs".to_owned()));
        // Unterminated → not complete.
        let p = PartialArgs::new(r#"{"path": "src/a"#);
        assert_eq!(p.complete_string("path"), None);
    }

    #[test]
    fn streaming_string_gives_prefix_while_open() {
        let p = PartialArgs::new(r#"{"content": "line1\nline2"#);
        assert_eq!(p.streaming_string("content"), Some("line1\nline2".to_owned()));
        // Closed → same.
        let p = PartialArgs::new(r#"{"content": "line1\n"}"#);
        assert_eq!(p.streaming_string("content"), Some("line1\n".to_owned()));
    }

    #[test]
    fn decodes_real_newlines_and_escapes() {
        let p = PartialArgs::new(r#"{"content": "a\nb\tc\"d"}"#);
        assert_eq!(p.streaming_string("content"), Some("a\nb\tc\"d".to_owned()));
    }

    #[test]
    fn drops_truncated_trailing_escape() {
        // `\u12` incomplete → dropped, not misdecoded.
        let p = PartialArgs::new(r#"{"content": "ab\u12"#);
        assert_eq!(p.streaming_string("content"), Some("ab".to_owned()));
        // Lone trailing backslash → dropped.
        let p = PartialArgs::new(r#"{"content": "ab\"#);
        assert_eq!(p.streaming_string("content"), Some("ab".to_owned()));
    }

    #[test]
    fn unicode_escape_decoded_when_complete() {
        let p = PartialArgs::new(r#"{"content": "aAé"}"#);
        assert_eq!(p.streaming_string("content"), Some("aAé".to_owned()));
    }

    #[test]
    fn missing_field_is_none() {
        let p = PartialArgs::new(r#"{"path": "x"}"#);
        assert_eq!(p.streaming_string("content"), None);
        assert_eq!(p.complete_string("content"), None);
    }

    #[test]
    fn skips_whitespace_around_colon() {
        let p = PartialArgs::new("{ \"path\" : \"a.rs\" }");
        assert_eq!(p.complete_string("path"), Some("a.rs".to_owned()));
    }

    #[test]
    fn path_then_content_in_generation_order() {
        // The realistic write stream: path completes first, content streams.
        let partial = r#"{"path": "src/main.rs", "content": "fn main() {"#;
        let p = PartialArgs::new(partial);
        assert_eq!(p.complete_string("path"), Some("src/main.rs".to_owned()));
        assert_eq!(
            p.streaming_string("content"),
            Some("fn main() {".to_owned())
        );
    }

    // --- edits array extraction (the `edit` tool) ---------------------------

    #[test]
    fn edit_entries_splits_closed_and_open() {
        // First entry complete, second still streaming its `new`.
        let p = PartialArgs::new(
            r#"{"edits": [{"path":"a.rs","old":["x"],"new":["y"]}, {"path":"b.rs","old":["p"],"new":["q"#,
        );
        let (closed, open) = p.edit_entries().unwrap();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0], r#"{"path":"a.rs","old":["x"],"new":["y"]}"#);
        let open = open.expect("second entry is open");
        assert_eq!(open.complete_string("path"), Some("b.rs".to_owned()));
        assert_eq!(open.streaming_lines("old"), Some(vec!["p".to_owned()]));
        // `new`'s only element is still streaming → not yet a complete line.
        assert_eq!(open.streaming_lines("new"), Some(vec![]));
    }

    #[test]
    fn edit_entries_all_closed() {
        let p = PartialArgs::new(
            r#"{"edits": [{"path":"a","old":["x"],"new":["y"]}]}"#,
        );
        let (closed, open) = p.edit_entries().unwrap();
        assert_eq!(closed.len(), 1);
        assert!(open.is_none());
    }

    #[test]
    fn edit_entries_braces_inside_strings_dont_confuse_the_scan() {
        // A `}` inside a string literal must not close the object early.
        let p = PartialArgs::new(
            r#"{"edits": [{"path":"a","old":["}"],"new":["y"]}, {"path":"b"#,
        );
        let (closed, open) = p.edit_entries().unwrap();
        assert_eq!(closed.len(), 1, "the string `}}` stays inside the entry");
        assert!(open.is_some());
    }

    #[test]
    fn complete_lines_waits_for_the_array_to_close() {
        // Open array → None (anchor not ready).
        let p = PartialArgs::new(r#"{"old":["l1","l2"#);
        assert_eq!(p.complete_lines("old"), None);
        // Closed array → the full line set.
        let p = PartialArgs::new(r#"{"old":["l1","l2"]"#);
        assert_eq!(
            p.complete_lines("old"),
            Some(vec!["l1".to_owned(), "l2".to_owned()])
        );
        // Single-string form: complete once the string closes.
        let p = PartialArgs::new(r#"{"old":"l1\nl2"}
#);
        assert_eq!(
            p.complete_lines("old"),
            Some(vec!["l1".to_owned(), "l2".to_owned()])
        );
    }

    #[test]
    fn streaming_lines_reads_array_and_single_string_forms() {
        // Array form.
        let p = PartialArgs::new(r#"{"old":["l1","l2"]}"#);
        assert_eq!(
            p.streaming_lines("old"),
            Some(vec!["l1".to_owned(), "l2".to_owned()])
        );
        // Single-string form, split on newlines (edit's normalization).
        let p = PartialArgs::new(r#"{"old":"l1\nl2"}"#);
        assert_eq!(
            p.streaming_lines("old"),
            Some(vec!["l1".to_owned(), "l2".to_owned()])
        );
        // An element still streaming is excluded (only complete items).
        let p = PartialArgs::new(r#"{"old":["l1","l2"#);
        assert_eq!(p.streaming_lines("old"), Some(vec!["l1".to_owned()]));
    }
}
