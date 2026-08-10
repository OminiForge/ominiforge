//! The built-in formatter registry: out-of-the-box stdin→stdout invocations
//! for common languages, so a workspace needs no `format.toml` to get
//! formatting (`doc/lsp.md` §3/§5). These are the lowest-precedence
//! configuration layer — a same-named formatter in any `format.toml` shadows
//! or disables them.

use super::config::FormatterConfig;

/// The built-in formatters, in routing order. Each entry mirrors a
/// `[[formatters]]` table with the default timeout; `enabled` is `true` (a
/// higher layer turns it off via `enabled = false`).
///
/// Every entry uses the **stdin→stdout** convention (`doc/lsp.md` §3):
/// ominiforge feeds the source on stdin and reads the formatted result on
/// stdout, then writes it to disk itself — never the formatter's in-place
/// mode. `supports_line_range` marks the formatters that can format an edited
/// line range instead of the whole file (`doc/lsp.md` §5); a `{file}` arg
/// placeholder is substituted with the touched file's name at spawn time.
///
/// Only binaries resolved through `PATH` / the session's direnv env-overlay
/// (`doc/architecture.md`). A missing binary fails the spawn and the formatter is
/// skipped (fail-closed) — it never blocks a file op.
pub fn builtin_formatters() -> Vec<FormatterConfig> {
    let entry = |name: &str,
                 command: &str,
                 args: &[&str],
                 extensions: &[&str],
                 supports_line_range: bool| {
        FormatterConfig {
            name: name.to_owned(),
            command: command.to_owned(),
            args: args.iter().map(|s| (*s).to_owned()).collect(),
            env: std::collections::HashMap::new(),
            extensions: extensions.iter().map(|s| (*s).to_owned()).collect(),
            enabled: true,
            supports_line_range,
            format_timeout_ms: super::config::default_format_timeout_ms(),
        }
    };
    vec![
        // rustfmt: `--emit stdout` reads stdin, writes stdout. `--file-lines`
        // narrows to edited lines in `edit` mode. No `--edition` flag: the
        // edition is the project's own business — rustfmt finds it in the
        // project's `rustfmt.toml` (we set cwd to the file's directory, so
        // its upward discovery works). A project that declares edition only
        // in `Cargo.toml` is one rustfmt cannot serve from stdin; its files
        // fail-closed-skip until the project adds a `rustfmt.toml`.
        entry("rustfmt", "rustfmt", &["--emit", "stdout"], &["rs"], true),
        // clang-format: stdin→stdout by default. `--lines=start:end` narrows.
        entry(
            "clang-format",
            "clang-format",
            &[],
            &["c", "cc", "cpp", "cxx", "h", "hh", "hpp"],
            true,
        ),
        // prettier: `--stdin-filepath` picks the parser from the file name.
        // No line-range stdin mode → skipped in `edit` mode.
        entry(
            "prettier",
            "prettier",
            &["--stdin-filepath", "{file}"],
            &[
                "js", "jsx", "mjs", "cjs", "ts", "tsx", "mts", "cts", "json", "md", "css", "html",
                "yaml", "yml",
            ],
            false,
        ),
        // gofmt: stdin→stdout by default; no line-range mode.
        entry("gofmt", "gofmt", &[], &["go"], false),
        // shfmt: stdin→stdout by default; no line-range mode.
        entry("shfmt", "shfmt", &[], &["sh", "bash"], false),
        // black: `-` reads stdin; no line-range mode. Listed before ruff so
        // it wins the `py` route (routing is first-match, see mod.rs).
        entry("black", "black", &["-"], &["py", "pyi"], false),
        // ruff format: `-` reads stdin; no line-range mode. A project that
        // prefers ruff over black tombstones `black` in its `format.toml`.
        entry(
            "ruff-format",
            "ruff",
            &["format", "-"],
            &["py", "pyi"],
            false,
        ),
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Built-ins are unique by name (shadowing is keyed on it) and every entry
    /// claims at least one extension.
    #[test]
    fn builtins_are_well_formed() {
        let formatters = builtin_formatters();
        assert!(!formatters.is_empty());
        let mut names = std::collections::HashSet::new();
        for f in &formatters {
            assert!(names.insert(&f.name), "duplicate builtin name {}", f.name);
            assert!(!f.extensions.is_empty(), "{} claims no extension", f.name);
            assert!(f.enabled);
        }
    }

    /// The two line-range-capable formatters from `doc/lsp.md` §5 are the
    /// only ones flagged; everything else is skipped in `edit` mode.
    #[test]
    fn line_range_support_matches_design_table() {
        let formatters = builtin_formatters();
        let supported: std::collections::HashSet<_> = formatters
            .iter()
            .filter(|f| f.supports_line_range)
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(supported, ["rustfmt", "clang-format"].into_iter().collect());
    }

    /// `{file}` appears only in entries whose invocation needs the file name
    /// (prettier), and always exactly once.
    #[test]
    fn file_placeholder_is_used_consistently() {
        for f in builtin_formatters() {
            let count = f.args.iter().filter(|a| a.contains("{file}")).count();
            if f.name == "prettier" {
                assert_eq!(count, 1, "prettier needs --stdin-filepath {{file}}");
            } else {
                assert_eq!(count, 0, "{} unexpectedly uses {{file}}", f.name);
            }
        }
    }
}
