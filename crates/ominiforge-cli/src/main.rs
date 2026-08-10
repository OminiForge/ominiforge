//! Ominiforge binary entry point.
//!
//! Sets up the async runtime and dispatches to the CLI. All command logic lives
//! in `ominiforge_cli`; this file stays thin so the desktop app can reuse the
//! same command surface.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    ominiforge_cli::run().await
}
