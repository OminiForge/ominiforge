//! Project guidance files (`AGENTS.md`) — per-directory instructions for the
//! agent, discovered from the workspace tree and injected into context.
//!
//! Two tiers (`doc/agents-md.md`):
//! - the **workspace-root** file is loaded once at assembly and appended to the
//!   system prompt (static, prefix-cacheable);
//! - a **nested** file in a sub-directory is loaded lazily the first time the
//!   agent touches a file under it (read/write/edit), injected as a one-shot
//!   context message and deduplicated per session.
//!
//! In any directory `AGENTS.md` is preferred and `CLAUDE.md` is a fallback, so
//! an existing Claude project works unchanged. The content is freeform Markdown:
//! the body passes through verbatim, wrapped only in a delimiter that names its
//! source path so the model can attribute it and a resumed session can recover
//! the dedup key.

use std::path::{Component, Path, PathBuf};

/// Candidate filenames in a directory, in priority order: the dedicated
/// `AGENTS.md` first, then `CLAUDE.md` as a fallback for existing projects.
const GUIDANCE_NAMES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];

/// A discovered guidance file: its workspace-relative path and verbatim body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Guidance {
    /// Workspace-relative path, e.g. `AGENTS.md` or `src/api/AGENTS.md`. Serves
    /// as both the injection `path=` label and the per-session dedup key.
    pub label: String,
    /// The file's verbatim Markdown body.
    pub body: String,
}

/// Read the workspace-root guidance file, if present (`AGENTS.md`, else
/// `CLAUDE.md`). `None` when neither exists or is unreadable.
#[must_use]
pub fn read_root(workspace: &Path) -> Option<Guidance> {
    read_dir_guidance(workspace, workspace)
}

/// Find the nearest guidance file for a touched workspace path.
///
/// Searches the file's own directory and walks up toward — but **excluding** —
/// the workspace root (the root file is already in the system prompt), returning
/// the deepest (nearest) match, or `None`.
///
/// `touched` is the raw `path` a filesystem tool received; any `read` selector
/// suffix (`:1-20`, `:raw`) is stripped first. A path that escapes the
/// workspace yields `None`.
#[must_use]
pub fn discover_nearest(workspace: &Path, touched: &str) -> Option<Guidance> {
    let resolved = resolve_lexical(workspace, strip_selector(touched))?;
    let mut dir = resolved.parent()?;
    while dir != workspace && dir.starts_with(workspace) {
        if let Some(g) = read_dir_guidance(workspace, dir) {
            return Some(g);
        }
        dir = dir.parent()?;
    }
    None
}

/// Wrap a guidance body in a delimiter that names its source path. The `path`
/// attribute lets the model attribute the instructions and lets resume recover
/// the dedup key via [`label_from_wrapped`].
#[must_use]
pub fn wrap(label: &str, body: &str) -> String {
    format!(
        "<project-guidance path=\"{label}\">\n{}\n</project-guidance>",
        body.trim_end()
    )
}

/// Recover the `path` label from a wrapped guidance injection (the inverse of
/// [`wrap`]), used on resume to rebuild the per-session dedup set. `None` if
/// `content` is not a project-guidance injection.
#[must_use]
pub fn label_from_wrapped(content: &str) -> Option<String> {
    let rest = content.strip_prefix("<project-guidance path=\"")?;
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

/// Try the guidance filenames in `dir` (priority order), returning the first
/// readable file with its label taken relative to `workspace`.
fn read_dir_guidance(workspace: &Path, dir: &Path) -> Option<Guidance> {
    for name in GUIDANCE_NAMES {
        let file = dir.join(name);
        if file.is_file()
            && let Ok(body) = std::fs::read_to_string(&file)
        {
            let label = file
                .strip_prefix(workspace)
                .unwrap_or(&file)
                .to_string_lossy()
                .into_owned();
            return Some(Guidance { label, body });
        }
    }
    None
}

/// Lexically resolve `requested` against `workspace` (no filesystem access, like
/// the tool layer): join, fold out `.`/`..`, and reject a path that escapes the
/// workspace.
fn resolve_lexical(workspace: &Path, requested: &str) -> Option<PathBuf> {
    let joined = workspace.join(requested);
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::CurDir => {}
            other => normalized.push(other),
        }
    }
    normalized.starts_with(workspace).then_some(normalized)
}

/// Strip a `read` selector suffix from a path: first `:raw`, then a trailing
/// `:N-M` / `:N+C` range. Mirrors the `read` tool's `parse_arg` so we search
/// from the same path the tool resolved. A `:` that is not a valid selector
/// (part of a filename) is left intact.
fn strip_selector(arg: &str) -> &str {
    let s = arg.strip_suffix(":raw").unwrap_or(arg);
    match s.rsplit_once(':') {
        Some((rest, sel)) if is_range_selector(sel) => rest,
        _ => s,
    }
}

/// Whether `sel` is a `N-M` or `N+C` range selector (both sides all-digits).
fn is_range_selector(sel: &str) -> bool {
    sel.split_once(['-', '+']).is_some_and(|(a, b)| {
        !a.is_empty()
            && !b.is_empty()
            && a.bytes().all(|c| c.is_ascii_digit())
            && b.bytes().all(|c| c.is_ascii_digit())
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// `AGENTS.md` at the root is found and labeled by its bare name.
    #[test]
    fn root_reads_agents_md() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "build: cargo build").unwrap();
        let g = read_root(dir.path()).unwrap();
        assert_eq!(g.label, "AGENTS.md");
        assert_eq!(g.body, "build: cargo build");
    }

    /// `CLAUDE.md` is the fallback when no `AGENTS.md` exists in a directory.
    #[test]
    fn root_falls_back_to_claude_md() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "legacy guidance").unwrap();
        let g = read_root(dir.path()).unwrap();
        assert_eq!(g.label, "CLAUDE.md");
        assert_eq!(g.body, "legacy guidance");
    }

    /// `AGENTS.md` wins over `CLAUDE.md` when both are present in a directory.
    #[test]
    fn agents_md_preferred_over_claude_md() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "preferred").unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "fallback").unwrap();
        assert_eq!(read_root(dir.path()).unwrap().body, "preferred");
    }

    /// No guidance file anywhere → `None`.
    #[test]
    fn root_absent_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_root(dir.path()).is_none());
    }

    /// The nearest (deepest) ancestor guidance file wins, and the root file is
    /// never returned by the nested search (it lives in the system prompt).
    #[test]
    fn nested_returns_deepest_excluding_root() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        std::fs::create_dir_all(ws.join("a/b")).unwrap();
        std::fs::write(ws.join("AGENTS.md"), "root").unwrap();
        std::fs::write(ws.join("a/AGENTS.md"), "a-level").unwrap();
        std::fs::write(ws.join("a/b/AGENTS.md"), "b-level").unwrap();

        // Touch a file in a/b → nearest is a/b/AGENTS.md.
        let g = discover_nearest(ws, "a/b/file.rs").unwrap();
        assert_eq!(
            g.label,
            format!("a{0}b{0}AGENTS.md", std::path::MAIN_SEPARATOR)
        );
        assert_eq!(g.body, "b-level");
    }

    /// A sub-directory with no guidance file of its own walks up to the nearest
    /// ancestor that has one (still excluding the root).
    #[test]
    fn nested_walks_up_to_nearest_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        std::fs::create_dir_all(ws.join("a/b/c")).unwrap();
        std::fs::write(ws.join("AGENTS.md"), "root").unwrap();
        std::fs::write(ws.join("a/AGENTS.md"), "a-level").unwrap();

        // Touch a/b/c/x.rs: b and c have none, so a/AGENTS.md is nearest.
        let g = discover_nearest(ws, "a/b/c/x.rs").unwrap();
        assert_eq!(g.body, "a-level");
    }

    /// A file directly under the workspace root has no *nested* guidance — the
    /// walk excludes the root, so this is `None` (root is in the system prompt).
    #[test]
    fn nested_for_root_level_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        std::fs::write(ws.join("AGENTS.md"), "root").unwrap();
        assert!(discover_nearest(ws, "main.rs").is_none());
    }

    /// The `read` selector suffix is stripped before discovery, so a sliced read
    /// still resolves the right directory.
    #[test]
    fn nested_strips_read_selector() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        std::fs::create_dir_all(ws.join("a")).unwrap();
        std::fs::write(ws.join("a/AGENTS.md"), "a-level").unwrap();
        assert_eq!(
            discover_nearest(ws, "a/file.rs:10-40").unwrap().body,
            "a-level"
        );
        assert_eq!(
            discover_nearest(ws, "a/file.rs:raw").unwrap().body,
            "a-level"
        );
        assert_eq!(
            discover_nearest(ws, "a/file.rs:1-40:raw").unwrap().body,
            "a-level"
        );
    }

    /// A path escaping the workspace yields no guidance (never reads outside).
    #[test]
    fn nested_escaping_path_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(discover_nearest(dir.path(), "../../etc/passwd").is_none());
    }

    /// `wrap` then `label_from_wrapped` round-trips the path label.
    #[test]
    fn wrap_roundtrips_label() {
        let wrapped = wrap("a/b/AGENTS.md", "do the thing\n");
        assert!(wrapped.contains("<project-guidance path=\"a/b/AGENTS.md\">"));
        assert!(wrapped.contains("do the thing"));
        assert!(wrapped.ends_with("</project-guidance>"));
        assert_eq!(
            label_from_wrapped(&wrapped).as_deref(),
            Some("a/b/AGENTS.md")
        );
    }

    /// A non-guidance string has no recoverable label.
    #[test]
    fn label_from_wrapped_rejects_other_content() {
        assert!(label_from_wrapped("<reminder>finish the plan</reminder>").is_none());
    }

    /// `strip_selector` only removes *valid* selectors; a `:` inside a filename
    /// is preserved.
    #[test]
    fn strip_selector_preserves_non_selector_colon() {
        assert_eq!(strip_selector("dir/file.rs"), "dir/file.rs");
        assert_eq!(strip_selector("dir/file.rs:10-20"), "dir/file.rs");
        assert_eq!(strip_selector("dir/file.rs:50+10"), "dir/file.rs");
        assert_eq!(strip_selector("dir/file.rs:raw"), "dir/file.rs");
        // Not a range selector → left intact.
        assert_eq!(strip_selector("weird:name"), "weird:name");
    }
}
