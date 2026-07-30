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
    #![allow(clippy::unwrap_used)]
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
}
