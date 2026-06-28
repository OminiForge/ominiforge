{
  description = "a multi agent app";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = {
    self,
    nixpkgs,
    rust-overlay,
    flake-utils,
    ...
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      overlays = [rust-overlay.overlays.default];
      pkgs = import nixpkgs {inherit system overlays;};

      rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      rustPlatform = pkgs.makeRustPlatform {
        cargo = rustToolchain;
        rustc = rustToolchain;
      };

      rustTools = with pkgs; [
        rustToolchain
        bacon
        cargo-audit
        cargo-deny
        cargo-edit
        cargo-expand
        cargo-llvm-cov
        cargo-machete
        cargo-nextest
        cargo-outdated
        cargo-sort
        cargo-watch
        just
        pkg-config
      ];

      nixTools = with pkgs; [
        alejandra
        deadnix
        statix
      ];

      # Frontend toolchain (doc/frontend.md §7). pnpm-in-nix sandboxed builds
      # are a known hard point; initially we just provide the tools in the
      # devShell and run the frontend build inside the shell (non-sandboxed).
      # `chromium` drives the offline UI screenshot tool (frontend/scripts/shot.mjs)
      # via playwright-core, so no Playwright browser download is needed.
      nodeTools = with pkgs; [
        nodejs_22
        pnpm
        chromium
      ];

      miscTools = with pkgs; [
        openssl
        python3
        taplo
      ];
    in {
      devShells.default = pkgs.mkShell {
        packages = rustTools ++ nixTools ++ nodeTools ++ miscTools;

        RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
        PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";
        # Point the screenshot tool (and playwright-core) at the nix Chromium and
        # stop Playwright trying to download its own browser.
        CHROMIUM_BIN = "${pkgs.chromium}/bin/chromium";
        PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD = "1";

        shellHook = ''
          export CARGO_HOME="''${CARGO_HOME:-$HOME/.cargo}"
          export RUST_BACKTRACE="''${RUST_BACKTRACE:-1}"
          if [ -z "''${OMINIFORGE_LSP:-}" ]; then
            echo "Rust dev shell ready: $(rustc --version)"
          fi
        '';
      };

      formatter = pkgs.alejandra;

      checks = {
        nix-format = pkgs.runCommand "nix-format-check" {nativeBuildInputs = [pkgs.alejandra];} ''
          alejandra --check ${./flake.nix}
          touch $out
        '';

        cargo-check = rustPlatform.buildRustPackage {
          pname = "ominiforge-check";
          version = "0.1.0";
          src = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter = path: type: let
              baseName = builtins.baseNameOf path;
            in
              !(builtins.elem baseName [".direnv" ".git" "target" "result"]);
          };
          cargoLock.lockFile = ./Cargo.lock;
          doCheck = true;
        };
      };
    });
}
