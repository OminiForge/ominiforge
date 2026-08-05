//! `format.toml` configuration: which formatter to run for which file
//! extensions, and whether formatting runs against the whole file or just the
//! edited line range (`doc/format.md` §5).
//!
//! Mirrors [`crate::lsp::config`]: the same layered scheme (built-in registry
//! → global → workspace), the same `enabled` tombstone semantics, and the same
//! "first same-named entry wins" shadowing. The two systems stay decoupled —
//! they only share a *shape*, never code.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// The parsed contents of `config/format.toml` (`doc/format.md` §5).
///
/// `Serialize` exists for the settings UI's write path (`gateway::langconfig`),
/// which rewrites the layer file the user edited; the runtime only ever
/// deserializes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct FormatConfig {
    /// Each `[[formatters]]` table.
    #[serde(default)]
    pub formatters: Vec<FormatterConfig>,
    /// Whether `edit`/`write` auto-format the touched file. `file` (default)
    /// formats the whole file; `edit` formats only the edited line range when
    /// the formatter supports it (and skips otherwise — never silently falls
    /// back to `file`, `doc/format.md` §5); `off` disables formatting.
    ///
    /// `Option` at the parse layer so a higher-priority layer that sets it
    /// shadows a lower one; [`FormatConfig::load`] resolves it to a concrete
    /// mode (`File` when no layer sets it).
    #[serde(default)]
    pub mode: Option<FormatMode>,
}

/// The format mode (`doc/format.md` §5). `file` is the default: most stable,
/// most "project-uniform" — at the cost of touching lines the model didn't.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub enum FormatMode {
    /// Format the whole file after every edit/write.
    #[default]
    File,
    /// Format only the edited line range; a formatter without line-range
    /// support is skipped (logged), never silently run whole-file.
    Edit,
    /// Formatting disabled.
    Off,
}

/// One configured formatter: a stateless stdin→stdout process call
/// (`doc/format.md` §3).
///
/// `Serialize` exists for the settings UI's write path (`gateway::langconfig`);
/// every field is written explicitly (the file the user reads back is the
/// full record they sent, with no skipped defaults to second-guess).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "ts-export", derive(ts_rs::TS), ts(export))]
pub struct FormatterConfig {
    /// Unique name; namespaces the formatter in logs and the UI's
    /// "formatted by \<name\>" annotation.
    pub name: String,

    /// The executable to spawn.
    pub command: String,

    /// Arguments passed to `command`. May contain the `{file}` placeholder,
    /// replaced with the touched file's name (needed by formatters like
    /// `prettier --stdin-filepath <name>` that pick a parser from the name).
    #[serde(default)]
    pub args: Vec<String>,

    /// Extra environment variables for the subprocess.
    #[serde(default)]
    pub env: HashMap<String, String>,

    /// File extensions this formatter handles, without the leading dot
    /// (e.g. `"rs"`). A file is routed to the first enabled formatter whose
    /// list contains its extension.
    pub extensions: Vec<String>,

    /// Whether this formatter is used. A higher-precedence layer sets `false`
    /// under a built-in's `name` to disable that default (tombstone
    /// semantics, mirroring `doc/lsp.md` §3). Defaults to `true`.
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Whether this formatter can format a line range rather than the whole
    /// file (`mode = "edit"`). When `false` and `mode = "edit"`, the
    /// formatter is skipped with a log — never silently run whole-file
    /// (`doc/format.md` §5).
    #[serde(default)]
    pub supports_line_range: bool,

    /// Milliseconds an `edit`/`write` waits for the formatter before giving
    /// up and writing the unformatted text (fail-closed, `doc/format.md`
    /// §4.3). Bounds the formatting overhead on every file op.
    #[serde(default = "default_format_timeout_ms")]
    pub format_timeout_ms: u64,
}

pub const fn default_format_timeout_ms() -> u64 {
    2_000
}

/// `enabled` defaults to `true` (a layer tombstones a built-in with `false`).
/// Local, not borrowed from `lsp::config`: the two systems share a config
/// *shape*, never code (`doc/format.md` §1).
const fn default_enabled() -> bool {
    true
}

impl FormatConfig {
    /// Load and merge `config/format.toml` from each root (highest priority
    /// first; a formatter name defined in a higher root shadows a lower one).
    /// A missing file contributes nothing; absent everywhere yields the
    /// built-in registry alone. Mirrors [`crate::lsp::config::LspConfig::load`].
    ///
    /// # Errors
    /// Returns the offending path and parse error if a present file is
    /// malformed.
    pub fn load(roots: &[std::path::PathBuf]) -> Result<Self, ConfigError> {
        // Roots are highest-priority first: the FIRST same-named formatter
        // wins, so a higher root shadows a lower one. Same for `mode`.
        let mut merged: Vec<FormatterConfig> = Vec::new();
        let mut mode: Option<FormatMode> = None;
        for root in roots {
            let path = root.join("config").join("format.toml");
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => return Err(ConfigError::Io { path, source }),
            };
            let file: Self = toml::from_str(&text).map_err(|source| ConfigError::Parse {
                path: path.clone(),
                source,
            })?;
            if mode.is_none() {
                mode = file.mode;
            }
            for formatter in file.formatters {
                if !merged.iter().any(|f| f.name == formatter.name) {
                    merged.push(formatter);
                }
            }
        }
        // The built-in registry is the lowest-precedence layer. Overlay the
        // user's formatters on it: a same-named user formatter REPLACES the
        // built-in (including an `enabled = false` tombstone that disables
        // it); a new name is appended.
        let mut result: Vec<FormatterConfig> = super::registry::builtin_formatters();
        for formatter in merged {
            if let Some(existing) = result.iter_mut().find(|f| f.name == formatter.name) {
                *existing = formatter;
            } else {
                result.push(formatter);
            }
        }
        // Tombstones applied: drop disabled entries.
        result.retain(|f| f.enabled);
        Ok(Self {
            formatters: result,
            mode: Some(mode.unwrap_or_default()),
        })
    }

    /// The resolved format mode (`File` when no layer set one).
    #[must_use]
    pub fn resolved_mode(&self) -> FormatMode {
        self.mode.unwrap_or_default()
    }
}

/// Why loading `format.toml` failed.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: std::path::PathBuf,
        source: toml::de::Error,
    },
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn write_toml(root: &std::path::Path, body: &str) {
        let cfg = root.join("config");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("format.toml"), body).unwrap();
    }

    /// A representative `format.toml` parses into a formatter with its
    /// extension routing and default timeout intact.
    #[test]
    fn parses_representative_config() {
        let toml_src = r#"
mode = "edit"

[[formatters]]
name = "rustfmt"
command = "rustfmt"
args = ["--emit", "stdout"]
extensions = ["rs"]
"#;
        let config: FormatConfig = toml::from_str(toml_src).unwrap();
        assert_eq!(config.mode, Some(FormatMode::Edit));
        assert_eq!(config.formatters.len(), 1);
        let f = &config.formatters[0];
        assert_eq!(f.name, "rustfmt");
        assert_eq!(f.command, "rustfmt");
        assert_eq!(f.args, ["--emit", "stdout"]);
        assert_eq!(f.extensions, ["rs"]);
        assert!(f.enabled);
        assert_eq!(f.format_timeout_ms, 2_000);
        // User-defined formatters default to no line-range support.
        assert!(!f.supports_line_range);
    }

    /// No `format.toml` anywhere → the built-in registry alone, `file` mode.
    #[test]
    fn missing_everywhere_yields_builtins_and_file_mode() {
        let dir = tempfile::tempdir().unwrap();
        let config = FormatConfig::load(&[dir.path().to_path_buf()]).unwrap();
        assert_eq!(config.resolved_mode(), FormatMode::File);
        assert_eq!(
            config.formatters.len(),
            super::super::registry::builtin_formatters().len()
        );
        assert!(config.formatters.iter().all(|f| f.enabled));
    }

    /// A higher-priority root shadows a same-named formatter in a lower root,
    /// and its `mode` wins too.
    #[test]
    fn higher_root_shadows_formatter_and_mode() {
        let dir = tempfile::tempdir().unwrap();
        let high = dir.path().join("high");
        let low = dir.path().join("low");
        write_toml(
            &high,
            "mode = \"edit\"\n[[formatters]]\nname = \"shared\"\ncommand = \"high-cmd\"\nextensions = [\"rs\"]\n",
        );
        write_toml(
            &low,
            "mode = \"off\"\n[[formatters]]\nname = \"shared\"\ncommand = \"low-cmd\"\nextensions = [\"rs\"]\n",
        );
        let config = FormatConfig::load(&[high, low]).unwrap();
        assert_eq!(config.resolved_mode(), FormatMode::Edit);
        let shared = config
            .formatters
            .iter()
            .find(|f| f.name == "shared")
            .unwrap();
        assert_eq!(shared.command, "high-cmd");
    }

    /// A lower layer's `mode` applies when the higher layer doesn't set one.
    #[test]
    fn lower_mode_applies_when_higher_unset() {
        let dir = tempfile::tempdir().unwrap();
        let high = dir.path().join("high");
        let low = dir.path().join("low");
        write_toml(
            &high,
            "[[formatters]]\nname = \"x\"\ncommand = \"x\"\nextensions = [\"rs\"]\n",
        );
        write_toml(&low, "mode = \"off\"\n");
        let config = FormatConfig::load(&[high, low]).unwrap();
        assert_eq!(config.resolved_mode(), FormatMode::Off);
    }

    /// A higher layer disables a built-in by name via `enabled = false`
    /// (tombstone): the entry vanishes from the merged result.
    #[test]
    fn enabled_false_disables_builtin() {
        let dir = tempfile::tempdir().unwrap();
        write_toml(
            dir.path(),
            "[[formatters]]\nname = \"rustfmt\"\ncommand = \"rustfmt\"\nextensions = [\"rs\"]\nenabled = false\n",
        );
        let config = FormatConfig::load(&[dir.path().to_path_buf()]).unwrap();
        assert!(
            !config.formatters.iter().any(|f| f.name == "rustfmt"),
            "disabled builtin should be dropped"
        );
        // Other builtins survive.
        assert!(config.formatters.iter().any(|f| f.name == "prettier"));
    }

    /// A higher layer overrides a built-in's fields (here: the command) while
    /// keeping it a single merged entry — including inheriting the registry's
    /// `supports_line_range` unless the layer re-states it.
    #[test]
    fn higher_layer_overrides_builtin_fields() {
        let dir = tempfile::tempdir().unwrap();
        write_toml(
            dir.path(),
            "[[formatters]]\nname = \"rustfmt\"\ncommand = \"rustfmt-wrapper\"\nextensions = [\"rs\"]\nsupports_line_range = true\n",
        );
        let config = FormatConfig::load(&[dir.path().to_path_buf()]).unwrap();
        let rustfmt = config
            .formatters
            .iter()
            .find(|f| f.name == "rustfmt")
            .unwrap();
        assert_eq!(rustfmt.command, "rustfmt-wrapper");
        assert!(rustfmt.supports_line_range);
    }
}
