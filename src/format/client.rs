//! One stdin→stdout formatter invocation, with the fail-closed defences
//! (`doc/format.md` §4). Pure of any tool/LSP coupling: config + text in,
//! formatted text or a skip (carrying the original text) out.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;

use super::config::FormatterConfig;
use crate::process_env::apply_env_overlay;

/// Run `formatter` on `text` for `abs_path`.
///
/// `line_range` is a 1-based inclusive `(start, end)` to narrow the format
/// (only passed for a line-range-capable formatter in `mode = "edit"`).
///
/// Returns `Ok(formatted)` on a clean run. Returns `Err(skipped)` — carrying
/// the **original** text — on any suspicious condition, having already logged
/// the reason once (`tracing::warn!` for a config-error-shaped failure,
/// `debug!` for an absent binary). The caller writes the returned text
/// regardless; the fail-closed invariant lives here so it can't be bypassed.
pub async fn run(
    formatter: &FormatterConfig,
    abs_path: &Path,
    text: &str,
    line_range: Option<(u32, u32)>,
    env_overlay: &BTreeMap<String, Option<String>>,
) -> Result<String, super::FormatOutcome> {
    let skip = |reason: &str, warn: bool| {
        if warn {
            tracing::warn!(formatter = %formatter.name, "format: {reason}; using unformatted text");
        } else {
            tracing::debug!(formatter = %formatter.name, "format: {reason}; using unformatted text");
        }
        Err(super::FormatOutcome::Skipped {
            text: text.to_owned(),
        })
    };

    // Build the argv: substitute the `{file}` placeholder with the touched
    // file's name (prettier picks a parser from it), and append the
    // formatter-specific line-range flags for `mode = "edit"`.
    let file_name = abs_path.to_string_lossy();
    let mut args: Vec<String> = formatter
        .args
        .iter()
        .map(|a| a.replace("{file}", &file_name))
        .collect();
    if let Some((start, end)) = line_range {
        args.extend(line_range_args(&formatter.name, start, end));
    }
    // No language-level discovery on our side: a formatter reads its own
    // config (rustfmt reads `rustfmt.toml` upward from the file's directory —
    // including its `edition` — because we set cwd there). If a project
    // declares its edition only in `Cargo.toml` (which rustfmt does not
    // read), a post-2015 file simply fails to parse and the fail-closed
    // defences skip it — loudly, never silently writing a wrong-edition
    // result. The fix belongs in the project (add a `rustfmt.toml`), not in
    // a second discovery layer here.

    let mut command = tokio::process::Command::new(&formatter.command);
    command
        .args(&args)
        .envs(&formatter.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Reap the child if `wait_with_output` is dropped on timeout below —
        // a timed-out formatter must not linger as a zombie.
        .kill_on_drop(true)
        // Run from the file's directory so the formatter's own upward config
        // discovery (`.clang-format`, `rustfmt.toml`) finds the project config
        // (`doc/format.md` §3: discovery is the formatter's job).
        .current_dir(abs_path.parent().unwrap_or_else(|| Path::new(".")));
    apply_env_overlay(&mut command, env_overlay);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // An auto-enabled built-in whose binary isn't installed is not a
            // warning (mirrors the LSP `not-installed` rule, `doc/lsp.md`
            // §4.6) — debug-level only.
            return skip("binary not found", false);
        }
        Err(e) => return skip(&format!("spawn failed: {e}"), true),
    };

    // Feed the source on stdin, then close it so the formatter sees EOF.
    if let Some(mut stdin) = child.stdin.take()
        && stdin.write_all(text.as_bytes()).await.is_err()
    {
        return skip("failed to write to formatter stdin", true);
    }

    let timeout = Duration::from_millis(formatter.format_timeout_ms);
    // `wait_with_output` consumes `child`; on timeout the future is dropped
    // and `kill_on_drop(true)` reaps the process.
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => return skip(&format!("wait failed: {e}"), true),
        Err(_) => return skip("formatter timed out", true),
    };

    let stderr = String::from_utf8_lossy(&output.stderr);
    // Defence 1 (`doc/format.md` §4.1): non-zero exit OR non-empty stderr is
    // failure. The stderr rule is what turns clang-format's silent-fallback
    // (exit 0, one stderr line, wrong result) into a loud skip.
    if !output.status.success() {
        return skip(
            &format!("exited {}: {}", output.status, stderr.trim()),
            true,
        );
    }
    if !stderr.trim().is_empty() {
        return skip(
            &format!("stderr not empty (exit 0): {}", stderr.trim()),
            true,
        );
    }

    let formatted_text = String::from_utf8_lossy(&output.stdout).into_owned();
    // Defence 2 (`doc/format.md` §4.2): consistency check — formatting must be
    // idempotent and must not drop content.
    if let Err(reason) = consistency_check(text, &formatted_text) {
        return skip(&reason, true);
    }
    Ok(formatted_text)
}

/// The formatter-specific argv that narrows a run to the 1-based inclusive
/// line range `[start, end]` (`doc/format.md` §5). Only called for formatters
/// flagged `supports_line_range`; the registry guarantees those are the two
/// below.
fn line_range_args(name: &str, start: u32, end: u32) -> Vec<String> {
    match name {
        // `--file-lines` is a JSON array of {file, range} objects; with stdin
        // the file key is `<stdin>`.
        "rustfmt" => vec![
            "--file-lines".to_owned(),
            format!(r#"[{{"file":"<stdin>","range":[{start},{end}]}}]"#),
        ],
        "clang-format" => vec![format!("--lines={start}:{end}")],
        _ => Vec::new(),
    }
}

/// Defence 2: reject a formatted result that looks like the formatter mangled
/// the file rather than reflowed it. Returns `Err(reason)` to skip.
///
/// Two checks, both cheap and both aimed at the config-error signature:
/// - **empty output for non-empty input** — the formatter dropped everything.
/// - **non-whitespace token collapse** — a config error that reindents or
///   rewraps keeps the same non-whitespace characters; one that mangles the
///   file (wrong parser, truncated read) usually changes them. We compare the
///   count of non-whitespace characters; a large divergence is rejected.
fn consistency_check(input: &str, output: &str) -> Result<(), String> {
    if !input.is_empty() && output.trim().is_empty() {
        return Err("formatter produced empty output for non-empty input".to_owned());
    }
    let in_tokens = non_ws_chars(input);
    let out_tokens = non_ws_chars(output);
    // Reformatting (indent/line-wrap) never adds or removes non-whitespace
    // characters; a small tolerance covers a formatter that normalizes a
    // trailing newline or a `,`/`;` style tweak. Beyond it, the content itself
    // changed — not a format.
    let drift = in_tokens.abs_diff(out_tokens);
    let tolerance = (in_tokens / 20).max(8); // 5% or 8 chars, whichever is larger
    if drift > tolerance {
        return Err(format!(
            "non-whitespace content changed by {drift} chars ({in_tokens} → {out_tokens}); not a format"
        ));
    }
    Ok(())
}

/// Count of non-whitespace characters — a stable signature of "the actual
/// code" that pure reflow (indent, wrap, spacing) preserves.
fn non_ws_chars(text: &str) -> usize {
    text.chars().filter(|c| !c.is_whitespace()).count()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]

    use super::*;
    use crate::format::FormatOutcome;

    fn formatter(name: &str, command: &str, args: &[&str]) -> FormatterConfig {
        FormatterConfig {
            name: name.to_owned(),
            command: command.to_owned(),
            args: args.iter().map(|s| (*s).to_owned()).collect(),
            env: std::collections::HashMap::new(),
            extensions: vec!["rs".to_owned()],
            enabled: true,
            supports_line_range: false,
            format_timeout_ms: 5_000,
        }
    }

    fn path() -> std::path::PathBuf {
        std::path::PathBuf::from("/tmp/x.rs")
    }

    /// The built-in rustfmt entry must pick up the PROJECT's own formatter
    /// config with zero help from us: a temp project whose `rustfmt.toml`
    /// declares `edition = "2024"` must let rustfmt parse AND reflow a
    /// let-chain read from stdin (cwd = the file's directory, so rustfmt's
    /// own upward config discovery finds it). Skipped when the host has no
    /// rustfmt.
    #[tokio::test]
    async fn builtin_rustfmt_discovers_project_config_itself() {
        if std::process::Command::new("rustfmt")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("rustfmt not installed; skipping");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        // The project's own formatter config — the ONLY source of the edition.
        std::fs::write(dir.path().join("rustfmt.toml"), "edition = \"2024\"\n").unwrap();
        // The formatted file lives under it (as in a real edit/write, where
        // the file exists on disk before formatting runs).
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let rustfmt = crate::format::registry::builtin_formatters()
            .into_iter()
            .find(|f| f.name == "rustfmt")
            .expect("registry has rustfmt");
        // A let-chain (Rust 2024) that also needs reflow: parsing it proves
        // rustfmt read the project's `rustfmt.toml` edition on its own.
        let src = "fn f(x: Option<i32>, y: Result<i32, ()>) { if let Some(a) = x && let Ok(b) = y { g(a, b); } }\n";
        let file = dir.path().join("src/lib.rs");
        let out = run(&rustfmt, &file, src, None, &BTreeMap::new())
            .await
            .expect("rustfmt should succeed via project config, not skip");
        assert!(
            out.contains("&& let Ok(b)"),
            "let-chain survived formatting: {out}"
        );
        assert!(out.len() > src.len(), "rustfmt reflowed the long line");
    }

    /// A formatter that just echoes stdin (`cat`) round-trips the text.
    #[tokio::test]
    async fn echo_formatter_round_trips() {
        let f = formatter("cat", "cat", &[]);
        let out = run(&f, &path(), "fn main() {}\n", None, &BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(out, "fn main() {}\n");
    }

    /// A missing binary skips (fail-closed), keeping the original text.
    #[tokio::test]
    async fn missing_binary_skips() {
        let f = formatter("nope", "definitely-not-a-real-binary-xyz", &[]);
        let out = run(&f, &path(), "original\n", None, &BTreeMap::new()).await;
        assert_eq!(
            out,
            Err(FormatOutcome::Skipped {
                text: "original\n".to_owned()
            })
        );
    }

    /// Defence 1: a formatter that exits 0 but prints to stderr is skipped —
    /// the clang-format silent-fallback case.
    #[tokio::test]
    async fn stderr_on_success_skips() {
        let f = formatter(
            "noisy",
            "sh",
            &["-c", "cat; echo 'config error, using default' >&2"],
        );
        let out = run(&f, &path(), "original\n", None, &BTreeMap::new()).await;
        assert!(matches!(out, Err(FormatOutcome::Skipped { .. })));
    }

    /// Defence 1: a non-zero exit is skipped.
    #[tokio::test]
    async fn nonzero_exit_skips() {
        let f = formatter("fail", "sh", &["-c", "exit 1"]);
        let out = run(&f, &path(), "original\n", None, &BTreeMap::new()).await;
        assert!(matches!(out, Err(FormatOutcome::Skipped { .. })));
    }

    /// Defence 2: empty output for non-empty input is skipped.
    #[tokio::test]
    async fn empty_output_skips() {
        let f = formatter("wipe", "sh", &["-c", "true"]);
        let out = run(&f, &path(), "fn main() {}\n", None, &BTreeMap::new()).await;
        assert!(matches!(out, Err(FormatOutcome::Skipped { .. })));
    }

    /// Defence 2: a wild content change (here: the formatter uppercases and
    /// injects text) is skipped as not-a-format.
    #[tokio::test]
    async fn mangled_output_skips() {
        let f = formatter(
            "mangle",
            "sh",
            &[
                "-c",
                "printf 'COMPLETELY DIFFERENT CONTENT WITH MUCH MORE TEXT'",
            ],
        );
        let out = run(&f, &path(), "fn main() {}\n", None, &BTreeMap::new()).await;
        assert!(matches!(out, Err(FormatOutcome::Skipped { .. })));
    }

    /// A pure reflow (reindent/rewrap — whitespace only) passes the
    /// consistency check.
    #[test]
    fn reflow_passes_consistency() {
        let input = "fn main(){let x=1;let y=2;}\n";
        let output = "fn main() {\n    let x = 1;\n    let y = 2;\n}\n";
        assert!(consistency_check(input, output).is_ok());
    }

    /// `{file}` substitution produces a valid argv and the run still
    /// round-trips stdin (here `{file}` becomes `$0`, which the script
    /// ignores). Asserting the substituted *value* would require emitting it
    /// on stdout/stderr, which the fail-closed defences rightly reject — so
    /// this proves substitution is wired without tripping them.
    #[tokio::test]
    async fn file_placeholder_produces_valid_argv() {
        let f = formatter("with-name", "sh", &["-c", "cat", "{file}"]);
        let out = run(&f, &path(), "fn main() {}\n", None, &BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(out, "fn main() {}\n");
    }
}
