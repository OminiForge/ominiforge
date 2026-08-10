//! Ominiforge desktop binary (CLI + GUI).
//!
//! This binary is a superset of the pure-CLI `ominiforge`: it exposes the exact same
//! command surface (reused from `ominiforge_cli`) and adds a graphical interface on
//! top. On a machine with a display, launching it opens the GPUI client; the CLI
//! subcommands behave identically to the standalone CLI build.
//!
//! TODO(gui): wire the GPUI entry point. When the user asks for the GUI (no
//! subcommand on a desktop session, or an explicit `gui` flag), launch the GPUI
//! client instead of printing CLI help. Until then this forwards to the CLI.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    ominiforge_cli::run().await
}
