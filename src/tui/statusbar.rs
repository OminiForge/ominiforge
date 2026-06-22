//! The environment status bar shown above the input box.
//!
//! It mirrors what tools like starship and omp show: where you are (cwd, git
//! branch) and what the project/shell is (language + active virtual env). The
//! two are deliberately separate — a Rust project managed by a Nix flake shows
//! *both* `rust` (the language) and `❄ flake` (the active env), because they
//! answer different questions.
//!
//! Detection is pluggable: a [`Detector`] inspects a [`DetectCtx`] (the cwd plus
//! a snapshot of the relevant env vars) and either claims a [`Segment`] or
//! passes. Adding a language or environment is one `impl` plus one list entry.
//! Detection runs once at startup (versions shell out to `rustc`/`python`/… and
//! are cached), since none of it changes within a session.

use std::path::{Path, PathBuf};
use std::process::Command;

use ratatui::style::Color;
use ratatui::text::{Line, Span};

use super::theme;

/// One rendered piece of the bar: a short symbol, a label, and its color.
pub struct Segment {
    icon: &'static str,
    label: String,
    color: Color,
}

impl Segment {
    fn new(icon: &'static str, label: impl Into<String>, color: Color) -> Self {
        Self {
            icon,
            label: label.into(),
            color,
        }
    }
}

/// The facts a detector inspects: the working directory and a snapshot of the
/// environment variables that signal an active virtual environment.
pub struct DetectCtx {
    cwd: PathBuf,
    virtual_env: Option<String>,
    conda_env: Option<String>,
    in_nix_shell: bool,
}

impl DetectCtx {
    /// Capture the current directory and relevant env vars once.
    fn capture(cwd: PathBuf) -> Self {
        Self {
            cwd,
            virtual_env: std::env::var("VIRTUAL_ENV").ok(),
            conda_env: std::env::var("CONDA_DEFAULT_ENV").ok(),
            in_nix_shell: std::env::var_os("IN_NIX_SHELL").is_some(),
        }
    }

    /// Whether any file in the cwd matches `pred` (non-recursive — project
    /// markers live at the root).
    fn any_file(&self, pred: impl Fn(&str) -> bool) -> bool {
        std::fs::read_dir(&self.cwd).is_ok_and(|entries| {
            entries
                .flatten()
                .any(|e| e.file_name().to_str().is_some_and(&pred))
        })
    }

    /// Whether a named file exists in the cwd.
    fn has_file(&self, name: &str) -> bool {
        self.cwd.join(name).exists()
    }
}

/// A detector claims a segment when its signals are present, else returns `None`.
trait Detector: Sync {
    fn detect(&self, ctx: &DetectCtx) -> Option<Segment>;
}

/// Run the first command in `candidates` that succeeds and return its trimmed
/// stdout's first whitespace-separated version-looking token. Best-effort: any
/// failure yields `None` so a missing toolchain just hides the version.
fn probe_version(candidates: &[(&str, &[&str])]) -> Option<String> {
    for (bin, args) in candidates {
        if let Ok(out) = Command::new(bin).args(*args).output()
            && out.status.success()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            let combined = if text.trim().is_empty() {
                String::from_utf8_lossy(&out.stderr).into_owned()
            } else {
                text.into_owned()
            };
            if let Some(v) = combined
                .split_whitespace()
                .find(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))
            {
                return Some(v.to_owned());
            }
        }
    }
    None
}

// ── Languages ──────────────────────────────────────────────────────────────

struct Rust;
impl Detector for Rust {
    fn detect(&self, ctx: &DetectCtx) -> Option<Segment> {
        if !ctx.has_file("Cargo.toml") {
            return None;
        }
        // Prefer the pinned toolchain; fall back to whatever `rustc` resolves.
        let version =
            read_rust_toolchain(&ctx.cwd).or_else(|| probe_version(&[("rustc", &["--version"])]));
        Some(Segment::new(
            "🦀",
            version_label("rust", version),
            theme::tool(),
        ))
    }
}

struct Python;
impl Detector for Python {
    fn detect(&self, ctx: &DetectCtx) -> Option<Segment> {
        let marked = ctx.has_file("pyproject.toml")
            || ctx.has_file("requirements.txt")
            || ctx.has_file("setup.py")
            || ctx.any_file(|n| {
                std::path::Path::new(n)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("py"))
            });
        if !marked {
            return None;
        }
        let version = probe_version(&[("python3", &["--version"]), ("python", &["--version"])]);
        Some(Segment::new(
            "🐍",
            version_label("py", version),
            theme::warn(),
        ))
    }
}

struct Cpp;
impl Detector for Cpp {
    fn detect(&self, ctx: &DetectCtx) -> Option<Segment> {
        let marked = ctx.has_file("CMakeLists.txt")
            || ctx.has_file("Makefile")
            || ctx.any_file(|n| {
                std::path::Path::new(n).extension().is_some_and(|ext| {
                    ext.eq_ignore_ascii_case("cpp")
                        || ext.eq_ignore_ascii_case("cc")
                        || ext.eq_ignore_ascii_case("hpp")
                })
            });
        if !marked {
            return None;
        }
        let version = probe_version(&[
            ("c++", &["--version"]),
            ("g++", &["--version"]),
            ("clang++", &["--version"]),
        ]);
        Some(Segment::new(
            "ⓒ",
            version_label("c++", version),
            theme::tool(),
        ))
    }
}

/// Nix-as-a-language: only when the project is *purely* Nix (a `.nix` file and
/// none of the other languages). A Nix flake merely managing another project's
/// env is reported by [`NixFlake`] under environments instead.
struct NixLang;
impl Detector for NixLang {
    fn detect(&self, ctx: &DetectCtx) -> Option<Segment> {
        let other_lang = ctx.has_file("Cargo.toml")
            || ctx.has_file("pyproject.toml")
            || ctx.has_file("package.json")
            || ctx.has_file("CMakeLists.txt");
        if other_lang {
            return None;
        }
        if ctx.any_file(|n| {
            std::path::Path::new(n)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("nix"))
        }) {
            Some(Segment::new("❄", "nix".to_owned(), theme::tool()))
        } else {
            None
        }
    }
}

// ── Environments (active virtual env, separate from language) ────────────────

/// A Nix flake managing the dev environment. Shown brightly when we are inside
/// the flake shell (`IN_NIX_SHELL`, set by `nix develop`/direnv), dimmer when a
/// `flake.nix` is merely present.
struct NixFlake;
impl Detector for NixFlake {
    fn detect(&self, ctx: &DetectCtx) -> Option<Segment> {
        if !ctx.has_file("flake.nix") {
            return None;
        }
        let color = if ctx.in_nix_shell {
            theme::ok()
        } else {
            theme::dim()
        };
        Some(Segment::new("❄", "flake".to_owned(), color))
    }
}

struct Venv;
impl Detector for Venv {
    fn detect(&self, ctx: &DetectCtx) -> Option<Segment> {
        let name = ctx.virtual_env.as_ref().map(|p| {
            Path::new(p)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("venv")
                .to_owned()
        })?;
        Some(Segment::new("⚲", format!("venv:{name}"), theme::ok()))
    }
}

struct Conda;
impl Detector for Conda {
    fn detect(&self, ctx: &DetectCtx) -> Option<Segment> {
        let name = ctx.conda_env.clone()?;
        Some(Segment::new("⚲", format!("conda:{name}"), theme::ok()))
    }
}

/// Programming-language detectors, tried in order; the first match wins so a
/// concrete language beats the pure-Nix fallback.
const LANGUAGES: &[&dyn Detector] = &[&Rust, &Python, &Cpp, &NixLang];

/// Active-environment detectors. All that match are shown (a flake *and* a venv
/// can both be active).
const ENVIRONMENTS: &[&dyn Detector] = &[&NixFlake, &Venv, &Conda];

/// Read the channel out of `rust-toolchain.toml` (`[toolchain] channel = "…"`),
/// for the pinned version without shelling out.
fn read_rust_toolchain(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("rust-toolchain.toml")).ok()?;
    text.lines()
        .find_map(|l| l.trim().strip_prefix("channel"))
        .and_then(|rest| rest.split('"').nth(1))
        .map(ToOwned::to_owned)
}

/// `"rust 1.96"` when a version is known, else just `"rust"`.
fn version_label(name: &str, version: Option<String>) -> String {
    version.map_or_else(|| name.to_owned(), |v| format!("{name} {v}"))
}

/// A computed, render-ready status bar. Built once (detection is not free) and
/// rendered each frame.
pub struct StatusBar {
    cwd: String,
    git: Option<(String, bool)>, // (branch, dirty)
    languages: Vec<Segment>,
    environments: Vec<Segment>,
}

impl StatusBar {
    /// Detect everything for `cwd` once: language, active env, cwd label, git.
    #[must_use]
    pub fn detect(cwd: &Path) -> Self {
        let ctx = DetectCtx::capture(cwd.to_path_buf());
        let languages = LANGUAGES.iter().filter_map(|d| d.detect(&ctx)).collect();
        let environments = ENVIRONMENTS.iter().filter_map(|d| d.detect(&ctx)).collect();
        Self {
            cwd: abbreviate(cwd),
            git: git_info(cwd),
            languages,
            environments,
        }
    }

    /// Re-detect the git branch/dirty state for `cwd`. The language and env
    /// segments are fixed for a session, but git status changes as the agent
    /// edits files, so this is refreshed after each turn.
    pub fn refresh_git(&mut self, cwd: &Path) {
        self.git = git_info(cwd);
    }

    /// Render the bar as one styled line: cwd · git · languages · environments,
    /// separated by a dim dot. Empty groups are skipped.
    #[must_use]
    pub fn render(&self) -> Line<'static> {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let sep = || Span::styled("  ·  ", theme::fg_dim(theme::dim()));

        spans.push(Span::styled(
            format!(" {}", self.cwd),
            theme::fg(theme::tool_result()),
        ));

        if let Some((branch, dirty)) = &self.git {
            spans.push(sep());
            let color = if *dirty { theme::warn() } else { theme::ok() };
            let mark = if *dirty { " ✚" } else { "" };
            spans.push(Span::styled(format!(" {branch}{mark}"), theme::fg(color)));
        }

        for seg in self.languages.iter().chain(&self.environments) {
            spans.push(sep());
            spans.push(Span::styled(
                format!("{} {}", seg.icon, seg.label),
                theme::fg(seg.color),
            ));
        }

        Line::from(spans)
    }
}

/// The git branch (or short SHA when detached) and whether the working tree is
/// dirty, via libgit2. `None` when the cwd is not a repo.
fn git_info(cwd: &Path) -> Option<(String, bool)> {
    let repo = git2::Repository::discover(cwd).ok()?;

    let head = repo.head().ok();
    let branch = head.as_ref().map_or_else(
        || "detached".to_owned(),
        |h| {
            h.shorthand()
                .map_or_else(|_| "detached".to_owned(), ToOwned::to_owned)
        },
    );

    // Dirty = any non-ignored change in the working tree or index.
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true).include_ignored(false);
    let dirty = repo.statuses(Some(&mut opts)).is_ok_and(|s| !s.is_empty());

    Some((branch, dirty))
}

/// Abbreviate a path for display: `$HOME` becomes `~`, and a long path keeps its
/// first and last two components with `…` in the middle.
fn abbreviate(path: &Path) -> String {
    let full = match std::env::var_os("HOME") {
        Some(home) if path.starts_with(&home) => {
            let rest = path.strip_prefix(&home).unwrap_or(path);
            if rest.as_os_str().is_empty() {
                "~".to_owned()
            } else {
                format!("~/{}", rest.display())
            }
        }
        _ => path.display().to_string(),
    };

    let parts: Vec<&str> = full.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() <= 4 {
        return full;
    }
    let head = &parts[..1];
    let tail = &parts[parts.len() - 2..];
    let lead = if full.starts_with('~') { "" } else { "/" };
    format!("{lead}{}/…/{}", head.join("/"), tail.join("/"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// A Rust project (Cargo.toml present) is detected as the `rust` language,
    /// and the pinned toolchain channel is read from `rust-toolchain.toml`
    /// without shelling out — so the version reflects the project's pin.
    #[test]
    fn rust_project_detected_with_pinned_toolchain() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]").unwrap();
        std::fs::write(
            tmp.path().join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"1.96.0\"\n",
        )
        .unwrap();

        let ctx = DetectCtx::capture(tmp.path().to_path_buf());
        let seg = Rust.detect(&ctx).expect("rust must be detected");
        assert!(seg.label.contains("rust"), "label: {}", seg.label);
        assert!(
            seg.label.contains("1.96.0"),
            "pinned version: {}",
            seg.label
        );
    }

    /// A flake.nix marks the env as a managed flake, distinct from the language.
    /// Inside a nix shell it is highlighted (ok color); merely present, it is
    /// dim. This is the language-vs-environment separation.
    #[test]
    fn nix_flake_is_an_environment_not_a_language() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("flake.nix"), "{}").unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]").unwrap();

        let ctx = DetectCtx::capture(tmp.path().to_path_buf());
        // NixLang must NOT claim this — there is another language present.
        assert!(
            NixLang.detect(&ctx).is_none(),
            "a Rust+flake project is not a pure-nix language project"
        );
        // NixFlake (environment) must claim it.
        let env = NixFlake.detect(&ctx).expect("flake env must be detected");
        assert_eq!(env.label, "flake");
    }

    /// A directory holding only `.nix` files is a pure-Nix *language* project.
    #[test]
    fn pure_nix_dir_is_a_language() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("default.nix"), "{}").unwrap();
        let ctx = DetectCtx::capture(tmp.path().to_path_buf());
        let seg = NixLang.detect(&ctx).expect("pure nix must be a language");
        assert_eq!(seg.label, "nix");
    }

    /// Path abbreviation collapses the middle of a deep path and uses `~` for
    /// home, so the cwd segment stays short.
    #[test]
    fn abbreviate_collapses_deep_paths() {
        let p = Path::new("/a/b/c/d/e/f");
        assert_eq!(abbreviate(p), "/a/…/e/f");
        let shallow = Path::new("/a/b");
        assert_eq!(abbreviate(shallow), "/a/b");
    }
}
