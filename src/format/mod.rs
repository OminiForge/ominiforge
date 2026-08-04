//! Auto-format after `edit`/`write`: a stateless stdin→stdout formatter call
//! sandwiched between the model's target text and the diff/diagnostics
//! (`doc/format.md`).
//!
//! **This system is deliberately separate from [`crate::lsp`]** — the two
//! share only a config *shape*. Their failure semantics are opposite
//! (`doc/format.md` §1): LSP diagnostics are fail-open (a server that can't
//! answer just yields no diagnostics), formatting is **fail-closed** — any
//! suspicious condition means *skip the format and use the original text*,
//! never write a suspicious result to disk.
//!
//! ## The fail-closed invariant
//!
//! A formatter with a broken config can silently fall back to defaults and
//! reformat the file into something the model never wrote (`doc/format.md` §4
//! clang-format case). Writing that, and feeding the resulting diff back to
//! the model, is worse than not formatting. So three defences, any of which
//! skips the format (keeps the original text, logs once):
//!
//! 1. **stderr non-empty is failure** — even with exit 0 (config parse
//!    errors print to stderr while exiting cleanly).
//! 2. **Consistency check** — empty output for non-empty input, or a wild
//!    divergence in non-whitespace token structure, is rejected.
//! 3. **Bounded + best-effort** — missing binary / timeout / non-zero exit
//!    all skip; an `edit` never fails because `prettier` isn't installed.

mod client;
pub(crate) mod config;
pub(crate) mod registry;

pub use config::{ConfigError, FormatConfig, FormatMode, FormatterConfig};

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

/// The outcome of formatting one file: either the formatter ran and possibly
/// changed the text, or it was skipped (fail-closed) and the text is the
/// original.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatOutcome {
    /// The formatter ran successfully. `text` is the formatted result (which
    /// may equal the input — an already-formatted file). `formatter` is the
    /// config `name`, surfaced in the UI's "formatted by \<name\>" annotation.
    Formatted { text: String, formatter: String },
    /// Formatting was skipped (no formatter, `mode = "off"`, or a fail-closed
    /// defence tripped). `text` is the original, unchanged.
    Skipped { text: String },
}

impl FormatOutcome {
    /// The final text to write to disk, diff, and analyze.
    #[must_use]
    pub fn into_text(self) -> String {
        match self {
            Self::Formatted { text, .. } | Self::Skipped { text } => text,
        }
    }

    /// The formatter name when it ran, for the UI annotation.
    #[must_use]
    pub fn formatter(&self) -> Option<&str> {
        match self {
            Self::Formatted { formatter, .. } => Some(formatter),
            Self::Skipped { .. } => None,
        }
    }
}

/// Routes `edit`/`write`'s touched files to their formatter and runs it
/// fail-closed. Held per session like [`crate::lsp::LspManager`]; stateless
/// beyond the config (each format is a fresh subprocess).
pub struct FormatManager {
    config: FormatConfig,
    env_overlay: BTreeMap<String, Option<String>>,
}

impl std::fmt::Debug for FormatManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FormatManager")
            .field("formatters", &self.config.formatters.len())
            .field("mode", &self.config.resolved_mode())
            .finish_non_exhaustive()
    }
}

impl FormatManager {
    /// Build a manager for `config`. Returns `None` when formatting can never
    /// do anything — `mode = "off"` or no formatters configured — so callers
    /// skip the whole step with zero overhead.
    #[must_use]
    pub fn new(
        config: FormatConfig,
        env_overlay: BTreeMap<String, Option<String>>,
    ) -> Option<Arc<Self>> {
        if config.resolved_mode() == FormatMode::Off || config.formatters.is_empty() {
            return None;
        }
        Some(Arc::new(Self {
            config,
            env_overlay,
        }))
    }

    /// The enabled formatter handling `path`'s extension, if any. Routing is
    /// **first match wins** (config order): formatting *rewrites* the file, so
    /// running two formatters would have them fight — unlike LSP diagnostics,
    /// which aggregate (`doc/format.md` vs `doc/lsp.md` §5). A project that
    /// wants `ruff-format` over `black` tombstones `black`.
    fn formatter_for(&self, path: &Path) -> Option<&FormatterConfig> {
        let ext = path.extension().and_then(|e| e.to_str())?;
        self.config
            .formatters
            .iter()
            .find(|f| f.extensions.iter().any(|e| e == ext))
    }

    /// Format `text` for `abs_path`, fail-closed.
    ///
    /// `edited_lines` is the 1-based inclusive `(start, end)` range the edit
    /// touched, used only in `mode = "edit"`; `write` passes `None` (a write
    /// replaces the whole file, so it always formats whole-file). Returns
    /// [`FormatOutcome::Skipped`] with the original text whenever formatting
    /// must not touch the result.
    pub async fn format(
        &self,
        abs_path: &Path,
        text: &str,
        edited_lines: Option<(u32, u32)>,
    ) -> FormatOutcome {
        let skipped = || FormatOutcome::Skipped {
            text: text.to_owned(),
        };
        let Some(formatter) = self.formatter_for(abs_path) else {
            return skipped();
        };
        let line_range = match self.config.resolved_mode() {
            FormatMode::Off => return skipped(),
            FormatMode::File => None,
            FormatMode::Edit => {
                if !formatter.supports_line_range {
                    // Never silently fall back to whole-file (`doc/format.md`
                    // §5): the user asked for minimal edits, so skip loudly.
                    tracing::debug!(
                        formatter = %formatter.name,
                        "format: mode=edit but formatter has no line-range support; skipping"
                    );
                    return skipped();
                }
                edited_lines
            }
        };
        match client::run(formatter, abs_path, text, line_range, &self.env_overlay).await {
            Ok(formatted_text) => FormatOutcome::Formatted {
                text: formatted_text,
                formatter: formatter.name.clone(),
            },
            Err(skipped_text) => skipped_text,
        }
    }
}
