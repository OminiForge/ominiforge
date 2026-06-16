//! Ominiforge binary entry point.
//!
//! Dispatches to the front-end subcommands (`run` / `tui` / `serve`). The
//! actual command parsing and dispatch land with the `cli` module in Phase 1;
//! for now this is a placeholder so the binary builds and links the library.

fn main() {
    println!("ominiforge {}", env!("CARGO_PKG_VERSION"));
}
