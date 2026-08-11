//! The LSP / format Web configuration views (`doc/lsp.md` §7, `doc/lsp.md`
//! §7): read the layered `lsp.toml` / `format.toml` chain into a
//! source-annotated list for the settings UI, and write an edited list back
//! to the primary config root.
//!
//! ## Scope: two views over the same chain
//!
//! The runtime loads LSP/format config over a chain that puts the session
//! workspace's `.omini` on top of the gateway's config roots
//! (`app::assemble` → `lang_config_roots`). This module mirrors that with two
//! view families:
//!
//! - **Global** (`*_config_view` / `save_*_config`): the gateway's config root
//!   chain alone (explicit `--config-dir` → launch cwd → user home), for the
//!   settings page's global-settings tab. Writes land on the primary root. Install
//!   probes run against the gateway's own PATH.
//! - **Workspace-local** (`*_for` / `save_*_for`): a specific workspace's
//!   `.omini` layered over the gateway chain, for the workspace config dialog.
//!   Writes land on the workspace's `.omini`. Install probes run against the
//!   workspace's **env-overlay PATH** (its direnv/flake environment), so a
//!   tool provided by the project is "installed" here even when it is absent
//!   from the gateway's PATH — the workspace view is the truthful one for
//!   per-project tooling.
//!
//! Within a chain the layers are labelled `builtin` (the compiled-in
//! registry), `workspace` (the top, project root), and `global` (user home).
//!
//! ## Read: layered resolution, registry-driven full list
//!
//! Unlike the permission editor (a user-authored incremental rule list), the
//! LSP/format UI is a **registry-driven fixed checklist**: every built-in
//! entry renders by default — a tombstoned (disabled) one stays visible so the
//! user can re-enable it — with each row annotated by the layer that supplied
//! it and a best-effort `installed` probe (`PATH` lookup of the command).
//!
//! ## Write: full replacement of one layer file + round-trip re-read
//!
//! `PUT` carries the complete desired list. The server:
//!
//! 1. re-reads the current view (after re-probing `installed`, so a request
//!    built minutes ago cannot act on a stale probe);
//! 2. refuses any `command` edit on an entry whose binary is not installed —
//!    the UI shows command inputs for installed rows only, and the server is
//!    the enforcement point (Karpathy §12: never trust the client);
//! 3. re-derives the builtin-set fields (`args` / `env` / `extensions` /
//!    `supports_line_range`) from the *current* view rather than trusting the
//!    wire, so a stale client cannot fork a registry entry it never saw;
//! 4. rewrites `<primary root>/config/{lsp,format}.toml` wholesale with the
//!    user-owned fields (enabled / command / timeouts / mode);
//! 5. reloads through [`LspConfig::load`] / [`FormatConfig::load`] and fails
//!    if the effective config does not reflect the request — a write that
//!    doesn't take is an error, not a silent no-op.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::config::ConfigStore;
use crate::format::config::{FormatConfig, FormatMode, FormatterConfig};
use crate::lsp::config::{LspConfig, LspServerConfig};
use serde::Serialize;

/// Which layer of the config chain supplied an entry (`doc/lsp.md` §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigLayer {
    /// The compiled-in registry (lowest precedence).
    Builtin,
    /// The user-home config root.
    Global,
    /// A non-home config root ahead of home in the chain (launch cwd /
    /// explicit `--config-dir`) — the de-facto project layer of a gateway
    /// started from a project directory.
    Workspace,
}

/// One row of the LSP settings view: the effective server config, the layer
/// that supplied it, and whether its binary is on `PATH`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LspServerView {
    /// Which layer's `lsp.toml` (or the registry) this row came from.
    pub layer: ConfigLayer,
    /// Whether this row is a registry entry (true even when a higher layer
    /// shadowed it — the UI greys registry fields either way).
    pub builtin: bool,
    /// Best-effort install probe: the command resolves on `PATH`.
    pub installed: bool,
    /// The effective server config after the full merge. For a tombstoned
    /// built-in this is the registry entry wearing `enabled = false`, so the
    /// row can render (and be re-enabled) instead of vanishing.
    #[serde(flatten)]
    pub server: LspServerConfig,
}

/// The full LSP settings view (`GET /config/lsp`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LspConfigView {
    /// Every built-in entry plus user-defined ones, in merge order, with
    /// tombstoned built-ins retained (greyed) rather than dropped.
    pub servers: Vec<LspServerView>,
}

/// One row of the format settings view (same shape as [`LspServerView`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FormatterView {
    /// Which layer's `format.toml` (or the registry) this row came from.
    pub layer: ConfigLayer,
    /// Whether this row is a registry entry.
    pub builtin: bool,
    /// Best-effort install probe: the command resolves on `PATH`.
    pub installed: bool,
    /// The effective formatter config after the full merge (tombstoned
    /// built-ins kept with `enabled = false`; see [`LspServerView::server`]).
    #[serde(flatten)]
    pub formatter: FormatterConfig,
}

/// The full format settings view (`GET /config/format`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FormatConfigView {
    /// The resolved format mode (highest layer that sets one wins; `file`
    /// when none does).
    pub mode: FormatMode,
    /// Every built-in formatter plus user-defined ones, tombstoned built-ins
    /// retained (greyed) rather than dropped.
    pub formatters: Vec<FormatterView>,
}

/// The user-editable fields of an LSP server row: everything the Web editor
/// lets the user change. Builtin-set fields (`args` / `env` / `extensions`)
/// are absent — the server re-derives them from the current view so a stale
/// or forged client cannot fork a registry entry.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct LspServerEdit {
    /// Row identity (shadowing key).
    pub name: String,
    /// Enable / tombstone.
    pub enabled: bool,
    /// The executable. Editable only for installed rows (enforced in
    /// [`save_lsp_config`]).
    pub command: String,
    /// Diagnostics wait bound (`doc/lsp.md` §4).
    pub diag_timeout_ms: u64,
    /// Initialize handshake bound (`doc/lsp.md` §4).
    pub init_timeout_ms: u64,
}

/// The full LSP `PUT` body.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct LspConfigEdit {
    /// The complete desired list (user rows plus registry rows, edited or
    /// not). Names not present are untouched… unless they were user-defined,
    /// in which case absence means deletion (full-replacement semantics).
    pub servers: Vec<LspServerEdit>,
}

/// The user-editable fields of a formatter row (see [`LspServerEdit`]).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct FormatterEdit {
    /// Row identity (shadowing key).
    pub name: String,
    /// Enable / tombstone.
    pub enabled: bool,
    /// The executable. Editable only for installed rows (enforced in
    /// [`save_format_config`]).
    pub command: String,
    /// Whole-call timeout (fail-closed, `doc/lsp.md` §4.3).
    pub format_timeout_ms: u64,
}

/// The full format `PUT` body.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct FormatConfigEdit {
    /// The desired mode; `None` leaves the current one untouched.
    pub mode: Option<FormatMode>,
    /// The complete desired list (see [`LspConfigEdit::servers`]).
    pub formatters: Vec<FormatterEdit>,
}

/// Is `path` an executable file? (On non-unix, existence is the test.)
fn is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    if !cfg!(unix) {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Resolve a command against a PATH list (bare name) or the filesystem (path
/// containing a separator). `path_dirs` is the search path the probe runs
/// against — for a workspace-local view this is the workspace's env-overlay
/// PATH (what the runtime spawn actually sees), NOT the gateway's own PATH.
/// The probe is UI-only; it never blocks a file op and is refreshed on every
/// view build.
fn command_installed_with(command: &str, path_dirs: &[PathBuf]) -> bool {
    if command.contains(std::path::MAIN_SEPARATOR) || command.contains('/') {
        return is_executable(Path::new(command));
    }
    path_dirs
        .iter()
        .any(|dir| is_executable(&dir.join(command)))
}

/// The gateway-process PATH as a dir list (the default probe context, used by
/// the global settings view).
fn gateway_path() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default()
}

/// Read one layer's config file. A missing file is `None`; a malformed one
/// fails loud (the view must not silently fall through to a lower layer's
/// entries — same posture as the runtime loader).
fn read_layer<T: serde::de::DeserializeOwned>(
    root: &Path,
    file: &str,
) -> Result<Option<(PathBuf, T)>> {
    let path = root.join("config").join(file);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(crate::config::ConfigError::Io { path, source })
                .context("failed to read config layer");
        }
    };
    let parsed = toml::from_str(&text).map_err(|source| crate::config::ConfigError::Parse {
        path: path.clone(),
        source,
    })?;
    Ok(Some((path, parsed)))
}

/// The per-root view of the chain: its label and parsed file, if present.
struct Chain<T> {
    layers: Vec<(ConfigLayer, Option<(PathBuf, T)>)>,
}

/// Load every root's layer file (highest priority first). `label_of` maps a
/// root to its `ConfigLayer` badge. Layer files are parsed independently here
/// — NOT via the runtime `load`, which drops tombstones — because the view
/// must show disabled built-ins as greyed rows.
fn load_chain<T: serde::de::DeserializeOwned>(
    roots: &[PathBuf],
    file: &str,
    label_of: impl Fn(&Path) -> ConfigLayer,
) -> Result<Chain<T>> {
    let mut layers = Vec::new();
    for root in roots {
        layers.push((label_of(root), read_layer(root, file)?));
    }
    Ok(Chain { layers })
}

/// The label for a root in the GATEWAY's config chain: `global` for the
/// user-home root, `workspace` for anything ahead of it (launch cwd /
/// explicit `--config-dir` — the de-facto project layer).
fn gateway_layer(root: &Path) -> ConfigLayer {
    let home = crate::config::home_dir().map(|h| h.join(".omini"));
    if Some(root.to_path_buf()) == home {
        ConfigLayer::Global
    } else {
        ConfigLayer::Workspace
    }
}

/// Build the LSP settings view over a root chain, probing installs against
/// `path_dirs` (the runtime's effective PATH for this view's scope).
fn lsp_view_over(
    roots: &[PathBuf],
    path_dirs: &[PathBuf],
    label_of: impl Fn(&Path) -> ConfigLayer,
) -> Result<LspConfigView> {
    let chain = load_chain::<LspConfig>(roots, "lsp.toml", &label_of)?;
    // Built-ins first (registry order), each tagged with the highest layer
    // that shadows it (or `builtin` when untouched). A shadow REPLACES the
    // entry wholesale — including the `enabled = false` tombstone, which the
    // view keeps (greyed) rather than dropping.
    let mut rows: Vec<LspServerView> = crate::lsp::registry::builtin_servers()
        .into_iter()
        .map(|server| LspServerView {
            layer: ConfigLayer::Builtin,
            builtin: true,
            installed: command_installed_with(&server.command, path_dirs),
            server,
        })
        .collect();
    let mut user_rows: Vec<LspServerView> = Vec::new();
    // Roots are highest-priority first: the FIRST layer to name a server wins
    // it (shadowing, `doc/lsp.md` §3) — a lower layer must NOT re-cover a row
    // a higher layer already claimed. `shadowed` tracks claims across layers.
    let mut shadowed: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (layer, file) in &chain.layers {
        let Some((_path, config)) = file else {
            continue;
        };
        for server in &config.servers {
            if shadowed.contains(server.name.as_str()) {
                continue;
            }
            if let Some(row) = rows.iter_mut().find(|r| r.server.name == server.name) {
                row.layer = *layer;
                row.installed = command_installed_with(&server.command, path_dirs);
                row.server = server.clone();
            } else {
                user_rows.push(LspServerView {
                    layer: *layer,
                    builtin: false,
                    installed: command_installed_with(&server.command, path_dirs),
                    server: server.clone(),
                });
            }
            shadowed.insert(server.name.as_str());
        }
    }
    rows.extend(user_rows);
    Ok(LspConfigView { servers: rows })
}

/// Build the LSP settings view over the GATEWAY's config root chain (the
/// global settings tab), probing installs against the gateway's PATH.
///
/// # Errors
/// A present-but-malformed `lsp.toml` in any root (fail loud).
pub fn lsp_config_view(store: &ConfigStore) -> Result<LspConfigView> {
    lsp_view_over(store.roots(), &gateway_path(), gateway_layer)
}

/// Build the format settings view over a root chain (see [`lsp_view_over`]).
fn format_view_over(
    roots: &[PathBuf],
    path_dirs: &[PathBuf],
    label_of: impl Fn(&Path) -> ConfigLayer,
) -> Result<FormatConfigView> {
    let chain = load_chain::<FormatConfig>(roots, "format.toml", &label_of)?;
    // Highest-priority layer with a `mode` key wins; default `file`.
    let mode = chain
        .layers
        .iter()
        .find_map(|(_, file)| file.as_ref().and_then(|(_, c)| c.mode))
        .unwrap_or_default();
    let mut rows: Vec<FormatterView> = crate::format::registry::builtin_formatters()
        .into_iter()
        .map(|formatter| FormatterView {
            layer: ConfigLayer::Builtin,
            builtin: true,
            installed: command_installed_with(&formatter.command, path_dirs),
            formatter,
        })
        .collect();
    let mut user_rows: Vec<FormatterView> = Vec::new();
    // Same first-layer-wins shadowing as `lsp_view_over` (see its comment).
    let mut shadowed: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (layer, file) in &chain.layers {
        let Some((_path, config)) = file else {
            continue;
        };
        for formatter in &config.formatters {
            if shadowed.contains(formatter.name.as_str()) {
                continue;
            }
            if let Some(row) = rows.iter_mut().find(|r| r.formatter.name == formatter.name) {
                row.layer = *layer;
                row.installed = command_installed_with(&formatter.command, path_dirs);
                row.formatter = formatter.clone();
            } else {
                user_rows.push(FormatterView {
                    layer: *layer,
                    builtin: false,
                    installed: command_installed_with(&formatter.command, path_dirs),
                    formatter: formatter.clone(),
                });
            }
            shadowed.insert(formatter.name.as_str());
        }
    }
    rows.extend(user_rows);
    Ok(FormatConfigView {
        mode,
        formatters: rows,
    })
}

/// Build the format settings view over the GATEWAY's config root chain (the
/// global settings tab).
///
/// # Errors
/// A present-but-malformed `format.toml` in any root (fail loud).
pub fn format_config_view(store: &ConfigStore) -> Result<FormatConfigView> {
    format_view_over(store.roots(), &gateway_path(), gateway_layer)
}

// ---- Workspace-local views ----
//
// A workspace's LSP/format chain is its own `.omini` on top of the gateway's
// chain (the same roots `app::assemble` now loads, via `lang_config_roots`),
// and its install probe runs against the workspace's env-overlay PATH — a
// binary provided by the project's `flake.nix`/`direnv` is "installed" for
// that workspace even when it is absent from the gateway's own PATH.

/// The label for a root in a WORKSPACE's chain: the workspace's own `.omini`
/// is the `workspace` layer; every root inherited from the gateway chain is
/// labelled by [`gateway_layer`] (global / the gateway's own project root).
fn workspace_layer(ws_root: &Path) -> impl Fn(&Path) -> ConfigLayer + '_ {
    move |root| {
        if root == ws_root {
            ConfigLayer::Workspace
        } else {
            gateway_layer(root)
        }
    }
}

/// The runtime PATH for a workspace view: the env-overlay's `PATH` when the
/// snapshot provides one (joined with the gateway PATH so host-resolved tools
/// still probe), else the gateway PATH alone. `overlay` is the session-env
/// snapshot (`BTreeMap<String, Option<String>>`; `None` values are removals).
fn workspace_path(overlay: Option<&BTreeMap<String, Option<String>>>) -> Vec<PathBuf> {
    let gateway = gateway_path();
    let Some(overlay) = overlay else {
        return gateway;
    };
    match overlay.get("PATH") {
        // The overlay REPLACES PATH (it already contains the full session PATH).
        Some(Some(path)) => std::env::split_paths(path).collect(),
        // PATH removed or untouched by the overlay: the workspace runs with
        // the gateway's PATH.
        _ => gateway,
    }
}

/// The LSP settings view scoped to a workspace: its `.omini` layered over the
/// gateway chain, installs probed against its env-overlay PATH.
/// `overlay` is the workspace's env snapshot when available.
///
/// # Errors
/// A present-but-malformed `lsp.toml` in any root (fail loud).
pub fn lsp_config_view_for(
    store: &ConfigStore,
    workspace: &Path,
    overlay: Option<&BTreeMap<String, Option<String>>>,
) -> Result<LspConfigView> {
    let ws_root = workspace.join(".omini");
    let roots: Vec<PathBuf> = std::iter::once(ws_root.clone())
        .chain(store.roots().iter().cloned())
        .collect();
    lsp_view_over(&roots, &workspace_path(overlay), workspace_layer(&ws_root))
}

/// The format settings view scoped to a workspace (see [`lsp_config_view_for`]).
///
/// # Errors
/// A present-but-malformed `format.toml` in any root (fail loud).
pub fn format_config_view_for(
    store: &ConfigStore,
    workspace: &Path,
    overlay: Option<&BTreeMap<String, Option<String>>>,
) -> Result<FormatConfigView> {
    let ws_root = workspace.join(".omini");
    let roots: Vec<PathBuf> = std::iter::once(ws_root.clone())
        .chain(store.roots().iter().cloned())
        .collect();
    format_view_over(&roots, &workspace_path(overlay), workspace_layer(&ws_root))
}

/// Persist an edited LSP list to a WORKSPACE's `.omini/config/lsp.toml` and
/// verify the reload over the workspace chain.
///
/// # Errors
/// See [`save_lsp_over`].
pub fn save_lsp_config_for(
    store: &ConfigStore,
    workspace: &Path,
    overlay: Option<&BTreeMap<String, Option<String>>>,
    edit: &LspConfigEdit,
) -> Result<()> {
    let ws_root = workspace.join(".omini");
    let roots: Vec<PathBuf> = std::iter::once(ws_root.clone())
        .chain(store.roots().iter().cloned())
        .collect();
    save_lsp_over(
        &roots,
        &ws_root,
        &workspace_path(overlay),
        workspace_layer(&ws_root),
        true,
        edit,
    )
}

/// Persist an edited format list (+ mode) to a WORKSPACE's
/// `.omini/config/format.toml` and verify the reload.
///
/// # Errors
/// See [`save_format_over`].
pub fn save_format_config_for(
    store: &ConfigStore,
    workspace: &Path,
    overlay: Option<&BTreeMap<String, Option<String>>>,
    edit: &FormatConfigEdit,
) -> Result<()> {
    let ws_root = workspace.join(".omini");
    let roots: Vec<PathBuf> = std::iter::once(ws_root.clone())
        .chain(store.roots().iter().cloned())
        .collect();
    save_format_over(
        &roots,
        &ws_root,
        &workspace_path(overlay),
        workspace_layer(&ws_root),
        true,
        edit,
    )
}

/// Serialize and atomically rewrite one layer file under `write_root`, then
/// confirm the reload sees it (`verify`). A failed verification means the
/// write did not take effect — surfaced as an error rather than a silent
/// no-op.
fn write_layer<T: Serialize>(
    write_root: &Path,
    file: &str,
    value: &T,
    verify: impl FnOnce() -> bool,
) -> Result<()> {
    let path = write_root.join("config").join(file);
    let text =
        toml::to_string_pretty(value).map_err(|source| crate::config::ConfigError::Serialize {
            path: path.clone(),
            source,
        })?;
    crate::config::write_atomic(&path, &text)?;
    if !verify() {
        bail!(
            "wrote {} but the effective config does not reflect it",
            path.display()
        );
    }
    Ok(())
}

/// Persist an edited LSP list to `write_root`'s `lsp.toml` and verify the
/// reload (reloaded over `roots`, the same chain the view was built from).
/// See the module header for the four enforcement steps.
///
/// # Errors
/// A `command` edit on a not-installed entry; a row the current view has no
/// record of (unknown name); serialize/io failure; or the post-write reload
/// not reflecting the request.
fn save_lsp_over(
    roots: &[PathBuf],
    write_root: &Path,
    path_dirs: &[PathBuf],
    label_of: impl Fn(&Path) -> ConfigLayer,
    enforce_installed: bool,
    edit: &LspConfigEdit,
) -> Result<()> {
    // Re-read the view with FRESH install probes: a request built on a stale
    // probe must not bypass the not-installed command lock.
    let current = lsp_view_over(roots, path_dirs, &label_of)?;
    let mut servers: Vec<LspServerConfig> = Vec::new();
    for row in &edit.servers {
        let cur = current
            .servers
            .iter()
            .find(|r| r.server.name == row.name)
            .ok_or_else(|| anyhow!("unknown server `{}` in edit", row.name))?;
        // The not-installed lock only applies where the probe is truthful — a
        // workspace view (its env-overlay PATH). The global view's PATH says
        // nothing about per-project tools, so it does NOT gate command edits
        // (a user pointing command at a flake-provided absolute path is
        // legitimate; a wrong path just fails-open at spawn, never blocks).
        if enforce_installed && row.command != cur.server.command && !cur.installed {
            bail!(
                "refusing to edit command of `{}`: its binary `{}` is not installed",
                row.name,
                cur.server.command
            );
        }
        // Builtin-set fields come from the current view (never the wire), so
        // a stale client cannot fork a registry entry it never saw. For a
        // user-defined row the view's copy IS the on-disk one.
        servers.push(LspServerConfig {
            name: row.name.clone(),
            command: row.command.clone(),
            args: cur.server.args.clone(),
            env: cur.server.env.clone(),
            extensions: cur.server.extensions.clone(),
            enabled: row.enabled,
            diag_timeout_ms: row.diag_timeout_ms,
            init_timeout_ms: row.init_timeout_ms,
        });
    }
    let expected_enabled: Vec<String> = servers
        .iter()
        .filter(|s| s.enabled)
        .map(|s| s.name.clone())
        .collect();
    write_layer(write_root, "lsp.toml", &LspConfig { servers }, || {
        LspConfig::load(roots).is_ok_and(|cfg| {
            expected_enabled
                .iter()
                .all(|name| cfg.servers.iter().any(|s| s.name == *name && s.enabled))
        })
    })
}

/// Persist an edited LSP list to the GATEWAY's primary root (the global
/// settings tab).
///
/// # Errors
/// See [`save_lsp_over`]; also [`crate::config::ConfigError::NoRoot`] when the
/// store has no config root.
pub fn save_lsp_config(store: &ConfigStore, edit: &LspConfigEdit) -> Result<()> {
    let write_root = store
        .roots()
        .first()
        .ok_or(crate::config::ConfigError::NoRoot)?
        .clone();
    // Global view: the install probe is not per-project truthful, so command
    // edits are not gated on it (`enforce_installed = false`).
    save_lsp_over(
        store.roots(),
        &write_root,
        &gateway_path(),
        gateway_layer,
        false,
        edit,
    )
}

/// Persist an edited format list (+ mode) to `write_root`'s `format.toml`
/// and verify the reload. Mirrors [`save_lsp_over`].
///
/// # Errors
/// A `command` edit on a not-installed entry; an unknown formatter name;
/// serialize/io failure; or the post-write reload not reflecting the request.
fn save_format_over(
    roots: &[PathBuf],
    write_root: &Path,
    path_dirs: &[PathBuf],
    label_of: impl Fn(&Path) -> ConfigLayer,
    enforce_installed: bool,
    edit: &FormatConfigEdit,
) -> Result<()> {
    let current = format_view_over(roots, path_dirs, &label_of)?;
    let mut formatters: Vec<FormatterConfig> = Vec::new();
    for row in &edit.formatters {
        let cur = current
            .formatters
            .iter()
            .find(|r| r.formatter.name == row.name)
            .ok_or_else(|| anyhow!("unknown formatter `{}` in edit", row.name))?;
        // See `save_lsp_over`: the lock applies only where the probe is
        // truthful (a workspace view); the global view does not gate commands.
        if enforce_installed && row.command != cur.formatter.command && !cur.installed {
            bail!(
                "refusing to edit command of `{}`: its binary `{}` is not installed",
                row.name,
                cur.formatter.command
            );
        }
        formatters.push(FormatterConfig {
            name: row.name.clone(),
            command: row.command.clone(),
            args: cur.formatter.args.clone(),
            env: cur.formatter.env.clone(),
            extensions: cur.formatter.extensions.clone(),
            enabled: row.enabled,
            supports_line_range: cur.formatter.supports_line_range,
            format_timeout_ms: row.format_timeout_ms,
        });
    }
    let expected_enabled: Vec<String> = formatters
        .iter()
        .filter(|f| f.enabled)
        .map(|f| f.name.clone())
        .collect();
    let expected_mode = edit.mode.unwrap_or(current.mode);
    write_layer(
        write_root,
        "format.toml",
        &FormatConfig {
            formatters,
            mode: Some(expected_mode),
        },
        || {
            FormatConfig::load(roots).is_ok_and(|cfg| {
                cfg.resolved_mode() == expected_mode
                    && expected_enabled
                        .iter()
                        .all(|name| cfg.formatters.iter().any(|f| f.name == *name && f.enabled))
            })
        },
    )
}

/// Persist an edited format list (+ mode) to the GATEWAY's primary root (the
/// global settings tab).
///
/// # Errors
/// See [`save_format_over`]; also [`crate::config::ConfigError::NoRoot`] when
/// the store has no config root.
pub fn save_format_config(store: &ConfigStore, edit: &FormatConfigEdit) -> Result<()> {
    let write_root = store
        .roots()
        .first()
        .ok_or(crate::config::ConfigError::NoRoot)?
        .clone();
    save_format_over(
        store.roots(),
        &write_root,
        &gateway_path(),
        gateway_layer,
        false,
        edit,
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn store_with(root: &Path) -> ConfigStore {
        ConfigStore::from_roots(vec![root.join(".omini")])
    }

    /// The view lists every built-in even with no config file anywhere —
    /// the registry-driven full checklist (NOT the permission editor's
    /// empty-tier semantics).
    #[test]
    fn lsp_view_lists_builtins_with_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        let view = lsp_config_view(&store).unwrap();
        assert_eq!(
            view.servers.len(),
            crate::lsp::registry::builtin_servers().len()
        );
        assert!(view.servers.iter().all(|r| r.builtin));
        assert!(view.servers.iter().all(|r| r.layer == ConfigLayer::Builtin));
        assert!(view.servers.iter().all(|r| r.server.enabled));
    }

    /// A layer's tombstone keeps the built-in row VISIBLE (greyed) instead of
    /// dropping it — the whole point of the checklist view.
    #[test]
    fn lsp_view_keeps_tombstoned_builtin_visible() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".omini/config");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("lsp.toml"),
            "[[servers]]\nname = \"pyright\"\ncommand = \"pyright-langserver\"\nextensions = [\"py\"]\nenabled = false\n",
        )
        .unwrap();
        let store = store_with(dir.path());
        let view = lsp_config_view(&store).unwrap();
        let pyright = view
            .servers
            .iter()
            .find(|r| r.server.name == "pyright")
            .expect("tombstoned built-in must stay in the view");
        assert!(!pyright.server.enabled);
        assert!(pyright.builtin);
        assert_eq!(pyright.layer, ConfigLayer::Workspace); // temp root ≠ home
    }

    /// A user-defined server rides after the built-ins, tagged with its layer.
    #[test]
    fn lsp_view_appends_user_defined_servers() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".omini/config");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("lsp.toml"),
            "[[servers]]\nname = \"custom-ls\"\ncommand = \"custom-ls\"\nextensions = [\"xyz\"]\n",
        )
        .unwrap();
        let store = store_with(dir.path());
        let view = lsp_config_view(&store).unwrap();
        let custom = view
            .servers
            .iter()
            .find(|r| r.server.name == "custom-ls")
            .unwrap();
        assert!(!custom.builtin);
        assert_eq!(custom.layer, ConfigLayer::Workspace);
    }

    /// A malformed layer file fails loud rather than silently showing a lower
    /// layer's entries (same posture as the runtime loader).
    #[test]
    fn lsp_view_fails_loud_on_malformed_layer() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join(".omini/config");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("lsp.toml"), "not = [valid").unwrap();
        let store = store_with(dir.path());
        assert!(lsp_config_view(&store).is_err());
    }

    /// The workspace-local view puts the workspace's `.omini` on top AND
    /// labels its entries `workspace`, while inherited gateway-chain entries
    /// keep their own layer. This is the layering `assemble` now loads.
    #[test]
    fn workspace_view_labels_workspace_layer_on_top() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global/.omini");
        let ws = dir.path().join("proj");
        // Gateway root: pyright enabled.
        std::fs::create_dir_all(global.join("config")).unwrap();
        std::fs::write(
            global.join("config/lsp.toml"),
            "[[servers]]\nname = \"pyright\"\ncommand = \"pyright-langserver\"\nextensions = [\"py\"]\n",
        )
        .unwrap();
        // Workspace root: tombstone pyright.
        std::fs::create_dir_all(ws.join(".omini/config")).unwrap();
        std::fs::write(
            ws.join(".omini/config/lsp.toml"),
            "[[servers]]\nname = \"pyright\"\ncommand = \"pyright-langserver\"\nextensions = [\"py\"]\nenabled = false\n",
        )
        .unwrap();
        let store = ConfigStore::from_roots(vec![global]);
        let view = lsp_config_view_for(&store, &ws, None).unwrap();
        let pyright = view
            .servers
            .iter()
            .find(|r| r.server.name == "pyright")
            .unwrap();
        assert!(!pyright.server.enabled);
        assert_eq!(pyright.layer, ConfigLayer::Workspace);
    }

    /// The install probe uses the workspace's env-overlay PATH, so a tool the
    /// project provides (its flake/direnv env) is "installed" here even when
    /// it is absent from the gateway's PATH — the per-project truth.
    #[test]
    fn workspace_view_probes_overlay_path() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global/.omini");
        let ws = dir.path().join("proj");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::create_dir_all(&ws).unwrap();
        // A fake `pyright-langserver` binary in a dir NOT on the gateway PATH.
        let bin = dir.path().join("flake-bin");
        std::fs::create_dir_all(&bin).unwrap();
        let exe = bin.join("pyright-langserver");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // The workspace overlay's PATH points at the flake bin dir.
        let overlay: BTreeMap<String, Option<String>> =
            std::iter::once(("PATH".to_owned(), Some(bin.display().to_string()))).collect();
        let store = ConfigStore::from_roots(vec![global]);
        let view = lsp_config_view_for(&store, &ws, Some(&overlay)).unwrap();
        let pyright = view
            .servers
            .iter()
            .find(|r| r.server.name == "pyright")
            .unwrap();
        assert!(
            pyright.installed,
            "overlay PATH should make pyright probe as installed"
        );
        // A different server (rust-analyzer) is NOT on the overlay PATH → not installed.
        let ra = view
            .servers
            .iter()
            .find(|r| r.server.name == "rust-analyzer")
            .unwrap();
        assert!(!ra.installed);
    }

    /// A tombstone write round-trips: after `save_lsp_config`, the runtime
    /// loader drops the disabled built-in and the view shows it greyed.
    #[test]
    fn lsp_save_tombstone_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        let view = lsp_config_view(&store).unwrap();
        let edit = LspConfigEdit {
            servers: view
                .servers
                .iter()
                .map(|r| LspServerEdit {
                    name: r.server.name.clone(),
                    enabled: r.server.name != "pyright", // disable pyright
                    command: r.server.command.clone(),
                    diag_timeout_ms: r.server.diag_timeout_ms,
                    init_timeout_ms: r.server.init_timeout_ms,
                })
                .collect(),
        };
        save_lsp_config(&store, &edit).unwrap();

        // Runtime view: pyright is gone (tombstone applied).
        let effective = LspConfig::load(store.roots()).unwrap();
        assert!(!effective.servers.iter().any(|s| s.name == "pyright"));
        assert!(effective.servers.iter().any(|s| s.name == "rust-analyzer"));
        // UI view: pyright still renders, greyed.
        let view = lsp_config_view(&store).unwrap();
        let pyright = view
            .servers
            .iter()
            .find(|r| r.server.name == "pyright")
            .unwrap();
        assert!(!pyright.server.enabled);
    }

    /// The not-installed command lock applies only where the probe is
    /// truthful. A WORKSPACE save refuses a command edit on a binary its
    /// env-overlay PATH lacks; the GLOBAL save does not gate (its PATH says
    /// nothing about per-project tools, so pointing command at a
    /// flake-provided absolute path stays editable).
    fn edit_all(view: &LspConfigView) -> LspConfigEdit {
        LspConfigEdit {
            servers: view
                .servers
                .iter()
                .map(|r| LspServerEdit {
                    name: r.server.name.clone(),
                    enabled: r.server.enabled,
                    command: r.server.command.clone(),
                    diag_timeout_ms: r.server.diag_timeout_ms,
                    init_timeout_ms: r.server.init_timeout_ms,
                })
                .collect(),
        }
    }

    fn retarget_command(edit: &mut LspConfigEdit, name: &str, command: &str) {
        edit.servers
            .iter_mut()
            .find(|s| s.name == name)
            .unwrap()
            .command = command.to_owned();
    }

    #[test]
    fn global_save_allows_command_edit_when_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        let view = lsp_config_view(&store).unwrap();
        let target = view
            .servers
            .iter()
            .find(|r| !r.installed)
            .expect("test env must lack at least one registry binary")
            .server
            .name
            .clone();
        let mut edit = edit_all(&view);
        retarget_command(&mut edit, &target, "/nix/store/xyz/bin/some-wrapper");
        // Not gated globally: the write succeeds.
        save_lsp_config(&store, &edit).unwrap();
        let reloaded = lsp_config_view(&store).unwrap();
        let row = reloaded
            .servers
            .iter()
            .find(|r| r.server.name == target)
            .unwrap();
        assert_eq!(row.server.command, "/nix/store/xyz/bin/some-wrapper");
    }

    #[test]
    fn workspace_save_refuses_command_edit_when_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global/.omini");
        let ws = dir.path().join("proj");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::create_dir_all(&ws).unwrap();
        let store = ConfigStore::from_roots(vec![global]);
        let view = lsp_config_view_for(&store, &ws, None).unwrap();
        let target = view
            .servers
            .iter()
            .find(|r| !r.installed)
            .expect("test env must lack at least one registry binary")
            .server
            .name
            .clone();
        let mut edit = edit_all(&view);
        retarget_command(&mut edit, &target, "some-wrapper");
        let err = save_lsp_config_for(&store, &ws, None, &edit).unwrap_err();
        assert!(err.to_string().contains("not installed"), "got: {err}");
    }

    /// Format view: no files → built-ins + `file` mode.
    #[test]
    fn format_view_defaults_to_builtins_and_file_mode() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        let view = format_config_view(&store).unwrap();
        assert_eq!(view.mode, FormatMode::File);
        assert_eq!(
            view.formatters.len(),
            crate::format::registry::builtin_formatters().len()
        );
    }

    /// Format save: mode + a tombstone round-trip through the runtime loader.
    #[test]
    fn format_save_mode_and_tombstone_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        let view = format_config_view(&store).unwrap();
        let edit = FormatConfigEdit {
            mode: Some(FormatMode::Edit),
            formatters: view
                .formatters
                .iter()
                .map(|r| FormatterEdit {
                    name: r.formatter.name.clone(),
                    enabled: r.formatter.name != "black",
                    command: r.formatter.command.clone(),
                    format_timeout_ms: r.formatter.format_timeout_ms,
                })
                .collect(),
        };
        save_format_config(&store, &edit).unwrap();
        let effective = FormatConfig::load(store.roots()).unwrap();
        assert_eq!(effective.resolved_mode(), FormatMode::Edit);
        assert!(!effective.formatters.iter().any(|f| f.name == "black"));
        // The tombstone is still visible in the view, greyed.
        let view = format_config_view(&store).unwrap();
        assert!(
            !view
                .formatters
                .iter()
                .find(|r| r.formatter.name == "black")
                .unwrap()
                .formatter
                .enabled
        );
    }

    /// `mode` untouched when the edit omits it.
    #[test]
    fn format_save_without_mode_keeps_current() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        let view = format_config_view(&store).unwrap();
        let edit = FormatConfigEdit {
            mode: None,
            formatters: view
                .formatters
                .iter()
                .map(|r| FormatterEdit {
                    name: r.formatter.name.clone(),
                    enabled: r.formatter.enabled,
                    command: r.formatter.command.clone(),
                    format_timeout_ms: r.formatter.format_timeout_ms,
                })
                .collect(),
        };
        save_format_config(&store, &edit).unwrap();
        let view = format_config_view(&store).unwrap();
        assert_eq!(view.mode, FormatMode::File);
    }

    /// Format mirrors LSP's command-lock split (see
    /// `global_save_allows_command_edit_when_not_installed`): the global save
    /// does not gate command edits on the gateway-PATH probe; the workspace
    /// save does (its env-overlay PATH is the truthful probe).
    fn fmt_edit_all(view: &FormatConfigView) -> FormatConfigEdit {
        FormatConfigEdit {
            mode: Some(view.mode),
            formatters: view
                .formatters
                .iter()
                .map(|r| FormatterEdit {
                    name: r.formatter.name.clone(),
                    enabled: r.formatter.enabled,
                    command: r.formatter.command.clone(),
                    format_timeout_ms: r.formatter.format_timeout_ms,
                })
                .collect(),
        }
    }

    fn fmt_retarget_command(edit: &mut FormatConfigEdit, name: &str, command: &str) {
        edit.formatters
            .iter_mut()
            .find(|f| f.name == name)
            .unwrap()
            .command = command.to_owned();
    }

    #[test]
    fn format_global_save_allows_command_edit_when_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        let store = store_with(dir.path());
        let view = format_config_view(&store).unwrap();
        let target = view
            .formatters
            .iter()
            .find(|r| !r.installed)
            .expect("test env must lack at least one registry binary")
            .formatter
            .name
            .clone();
        let mut edit = fmt_edit_all(&view);
        fmt_retarget_command(&mut edit, &target, "/nix/store/xyz/bin/fmt-wrapper");
        save_format_config(&store, &edit).unwrap();
        let reloaded = format_config_view(&store).unwrap();
        let row = reloaded
            .formatters
            .iter()
            .find(|r| r.formatter.name == target)
            .unwrap();
        assert_eq!(row.formatter.command, "/nix/store/xyz/bin/fmt-wrapper");
    }

    #[test]
    fn format_workspace_save_refuses_command_edit_when_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global/.omini");
        let ws = dir.path().join("proj");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::create_dir_all(&ws).unwrap();
        let store = ConfigStore::from_roots(vec![global]);
        let view = format_config_view_for(&store, &ws, None).unwrap();
        let target = view
            .formatters
            .iter()
            .find(|r| !r.installed)
            .expect("test env must lack at least one registry binary")
            .formatter
            .name
            .clone();
        let mut edit = fmt_edit_all(&view);
        fmt_retarget_command(&mut edit, &target, "fmt-wrapper");
        let err = save_format_config_for(&store, &ws, None, &edit).unwrap_err();
        assert!(err.to_string().contains("not installed"), "got: {err}");
    }
}
