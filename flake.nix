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
        # protoc for boxlite: boxlite-shared's build.rs compiles its gRPC/proto
        # definitions via tonic-build, which needs protoc >= 3.12 at build time
        # (doc/sandbox.md §4). Build-time tool, so it belongs in the devShell,
        # not Cargo.toml.
        protobuf
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

      # Production build (`doc/sandbox.md` §8): the gateway with the
      # `sandbox-boxlite` microVM backend compiled in.
      #
      # boxlite's crates.io build.rs enters "stub" mode (it detects the packaged
      # crate) — it emits FFI declarations only and links `libkrun` dynamically,
      # expecting the runtime library to be supplied by the host. We supply the
      # nixpkgs-maintained libraries rather than letting boxlite download its own
      # prebuilt blobs: `libkrun`/`libkrunfw` for the microVM and `bubblewrap`
      # for the jailer. **Zero dependency versions/URLs live in our tree** —
      # nixpkgs owns them; a boxlite bump is just a Cargo.toml change. This keeps
      # host adaptation out of the Rust code and the flake free of hardcoded
      # download manifests.
      #
      # NOTE: boxlite runtime needs KVM on the host. The default `gateway.toml`
      # selects `passthrough`, so a deploy runs everywhere out of the box;
      # `boxlite`/`auto` are opt-in (`doc/gateway.md` §7).
      packages = let
        # Runtime libraries boxlite links/loads (nixpkgs-owned, not vendored).
        boxliteLibs = with pkgs; [libkrun libkrunfw];
        # Runtime executables the boxlite jailer needs on PATH.
        boxliteRuntimeBins = with pkgs; [bubblewrap];
        ominiforge = rustPlatform.buildRustPackage {
          pname = "ominiforge";
          version = "0.1.0";
          src = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter = path: type: let
              baseName = builtins.baseNameOf path;
            in
              !(builtins.elem baseName [".direnv" ".git" "target" "result"]);
          };
          cargoLock.lockFile = ./Cargo.lock;

          buildFeatures = ["sandbox-boxlite"];
          # protoc (boxlite-shared build.rs) + pkg-config/openssl, mirroring the
          # devShell's build-time tools.
          nativeBuildInputs = [pkgs.protobuf pkgs.pkg-config pkgs.makeWrapper];
          buildInputs = [pkgs.openssl] ++ boxliteLibs;

          # Stub mode: boxlite's -sys build scripts must not try to download or
          # build native deps — we provide them from nixpkgs.
          BOXLITE_DEPS_STUB = "1";
          # Link against the nixpkgs libkrun at build time.
          RUSTFLAGS = "-L ${pkgs.libkrun}/lib -L ${pkgs.libkrunfw}/lib";

          # Tests need KVM / network (image pull); skip in the sandboxed build.
          doCheck = false;

          # Runtime: libkrun/libkrunfw on the loader path, bwrap on PATH.
          postInstall = ''
            wrapProgram $out/bin/ominiforge \
              --prefix PATH : ${pkgs.lib.makeBinPath boxliteRuntimeBins} \
              --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath boxliteLibs}
          '';
        };
      in {
        default = ominiforge;
        ominiforge = ominiforge;
      };

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
