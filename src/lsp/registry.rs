//! The built-in language-server registry: out-of-the-box entries for common
//! languages, so a workspace needs no `lsp.toml` to get diagnostics
//! (`doc/lsp.md` §3). These are the lowest-precedence configuration layer —
//! a same-named server in any `lsp.toml` shadows or disables them.

use super::config::LspServerConfig;

/// The built-in servers, in routing order. Each entry mirrors a `[[servers]]`
/// table with the default timeouts; `enabled` is `true` (a higher layer turns
/// it off via `enabled = false`).
///
/// Only stdio servers whose binary is resolved through `PATH` / the session's
/// direnv env-overlay (`doc/env.md`). An entry whose binary is absent fails to
/// spawn on first touch and is marked `not-installed` — it never blocks a
/// file op.
pub fn builtin_servers() -> Vec<LspServerConfig> {
    let entry = |name: &str, command: &str, args: &[&str], extensions: &[&str]| LspServerConfig {
        name: name.to_owned(),
        command: command.to_owned(),
        args: args.iter().map(|s| (*s).to_owned()).collect(),
        env: std::collections::HashMap::new(),
        extensions: extensions.iter().map(|s| (*s).to_owned()).collect(),
        enabled: true,
        diag_timeout_ms: super::config::default_diag_timeout_ms(),
        init_timeout_ms: super::config::default_init_timeout_ms(),
    };
    vec![
        entry("rust-analyzer", "rust-analyzer", &[], &["rs"]),
        entry(
            "pyright",
            "pyright-langserver",
            &["--stdio"],
            &["py", "pyi"],
        ),
        entry("ruff", "ruff", &["server"], &["py", "pyi"]),
        entry(
            "typescript-language-server",
            "typescript-language-server",
            &["--stdio"],
            &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"],
        ),
        // .svelte files only — svelteserver syntax-parses standalone .ts but
        // does not type-check them, so TS/JS stay with typescript-language-server.
        entry("svelte", "svelteserver", &["--stdio"], &["svelte"]),
        entry("gopls", "gopls", &[], &["go"]),
        entry(
            "clangd",
            "clangd",
            &[],
            &["c", "cc", "cpp", "cxx", "h", "hh", "hpp"],
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
        let servers = builtin_servers();
        assert!(!servers.is_empty());
        let mut names = std::collections::HashSet::new();
        for s in &servers {
            assert!(names.insert(&s.name), "duplicate builtin name {}", s.name);
            assert!(!s.extensions.is_empty(), "{} claims no extension", s.name);
            assert!(s.enabled);
        }
    }

    /// Python's real-world combination — a language server and a linter —
    /// both claim `py`, proving the multi-server-per-language registry shape.
    #[test]
    fn python_has_multiple_servers() {
        let servers = builtin_servers();
        let py = servers
            .iter()
            .filter(|s| s.extensions.iter().any(|e| e == "py"))
            .count();
        assert!(py >= 2, "expected pyright + ruff for py");
    }
}
