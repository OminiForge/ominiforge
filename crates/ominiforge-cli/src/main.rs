//! Ominiforge binary entry point.
//!
//! Dispatches to the CLI. All command logic lives in `ominiforge_cli`; this
//! file stays thin so a facade can reuse the same command surface.

fn main() -> anyhow::Result<()> {
    ominiforge_cli::run()
}
