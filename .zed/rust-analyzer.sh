#!/usr/bin/env bash
# Run rust-analyzer from this repository's Nix flake dev shell.
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
export OMINIFORGE_LSP=1
exec nix develop "$repo_root" --command rust-analyzer "$@"
