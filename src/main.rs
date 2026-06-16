//! Ominiforge binary entry point.
//!
//! Sets up the async runtime and dispatches to the CLI. All command logic lives
//! in `ominiforge::cli`; this file stays thin.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    ominiforge::cli::run().await
}
