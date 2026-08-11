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

      # Frontend toolchain. pnpm-in-nix sandboxed builds
      # are a known hard point; initially we just provide the tools in the
      # devShell and run the frontend build inside the shell (non-sandboxed).
      # `chromium` drives the offline UI screenshot tool (frontend/scripts/shot.mjs)
      # via playwright-core, so no Playwright browser download is needed.
      nodeTools = with pkgs; [
        nodejs_22
        pnpm
        chromium
        # Language servers consumed by ominiforge's own LSP integration
        # (doc/design/lsp.md), which routes by file extension to a specific binary.
        # svelteserver handles .svelte; typescript-language-server handles
        # .ts/.js. They are independent processes — svelteserver cannot cover
        # standalone .ts files (it only syntax-parses them, no type-checking),
        # so both are required for frontend diagnostics.
        svelte-language-server
        typescript-language-server
      ];

      miscTools = with pkgs; [
        openssl
        # protoc for boxlite: boxlite-shared's build.rs compiles its gRPC/proto
        # definitions via tonic-build, which needs protoc >= 3.12 at build time
        # (doc/design/sandbox.md §4). Build-time tool, so it belongs in the devShell,
        # not Cargo.toml.
        protobuf
        python3
        taplo
        mdbook
        # ripgrep drives `just lint-english` (its \p{L} is locale-independent, unlike GNU grep).
        ripgrep
      ];
    in {
      # CI-only shell: installs just the tools the cargo job steps (fmt/clippy/test/audit/
      # deny/machete) actually use. Deliberately omits frontend/LSP/local-dev tools (chromium,
      # node, pnpm, svelte/typescript language server, llvm-cov, bacon, cargo-watch, ...) —
      # those serve local dev or frontend screenshot/LSP diagnostics, CI never runs them, and
      # including them only slows every `nix develop` realize. Keep in sync with ci.yml's
      # cargo job when this list changes.
      devShells.ci = pkgs.mkShell {
        packages = with pkgs; [
          rustToolchain
          cargo-audit
          cargo-deny
          cargo-machete
          cargo-nextest
          just
          pkg-config
          # for fmt-check: alejandra checks flake.nix, taplo checks Cargo.toml/rust-toolchain.toml.
          alejandra
          taplo
          # boxlite-shared's build.rs needs protoc; openssl is for pkg-config discovery.
          openssl
          protobuf
          # ripgrep drives `just lint-english` (its \p{L} is locale-independent, unlike GNU grep).
          ripgrep
        ];

        # Same as the default shell: gpui compiles both Wayland+X11 backends on Linux, so test
        # binaries must resolve the X11 client libs at link time (see the default shell's note).
        RUSTFLAGS = "-L ${pkgs.libxcb}/lib -L ${pkgs.libxkbcommon}/lib";
        PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";
      };

      devShells.default = pkgs.mkShell {
        packages = rustTools ++ nixTools ++ nodeTools ++ miscTools;

        # gpui compiles BOTH its Wayland and X11 backends on Linux (they are
        # default features), so every UI test/app binary must RESOLVE the X11
        # client libs at link time (libxcb, libxkbcommon, libxkbcommon-x11)
        # even though a Wayland session never loads them at runtime. Nix does
        # not put these on the default linker search path; without this,
        # `cargo nextest run -p ominiforge-ui` fails at link with
        # `unable to find library -lxcb -lxkbcommon -lxkbcommon-x11`. This is a
        # link-path pointer only — it does NOT tie the app to X11; the runtime
        # backend is chosen by the session (Wayland when WAYLAND_DISPLAY is set).
        RUSTFLAGS = "-L ${pkgs.libxcb}/lib -L ${pkgs.libxkbcommon}/lib";

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

      # Production build (`doc/design/sandbox.md` §8): the gateway with the
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
      # `boxlite`/`auto` are opt-in (`doc/design/gateway.md` §7).
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
            # filter only matches on the file name; the type argument is unused (underscore-marked).
            filter = path: _type: let
              baseName = builtins.baseNameOf path;
            in
              !(builtins.elem baseName [".direnv" ".git" "target" "result"]);
          };
          cargoLock = {
            lockFile = ./Cargo.lock;
            allowBuiltinFetchGit = true; # vendoring hash for the gpui git dependency
          };

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

        # Nix style lint (the clippy of Nix). statix.toml disables manual_inherit — it misfires
        # on `x = f { }` function calls. Fail-on-warning by default: a hard gate.
        nix-lint =
          pkgs.runCommand "nix-lint-check" {
            nativeBuildInputs = [pkgs.statix];
            # statix reads statix.toml at the repo root; copy flake.nix and the config into the sandbox.
          } ''
            cp ${./flake.nix} flake.nix
            cp ${./statix.toml} statix.toml
            statix check flake.nix
            touch $out
          '';

        # Nix dead-code detection (unused let bindings / lambda args). --no-lambda-pattern-names
        # exempts attrset pattern names (e.g. flake outputs' self) — those are framework-contract
        # arguments that must not be deleted.
        nix-dead = pkgs.runCommand "nix-dead-check" {nativeBuildInputs = [pkgs.deadnix];} ''
          cp ${./flake.nix} flake.nix
          deadnix -f --no-lambda-pattern-names flake.nix
          touch $out
        '';

        # TOML format gate, alongside nix-format (which covers flake.nix).
        toml-format = pkgs.runCommand "toml-format-check" {nativeBuildInputs = [pkgs.taplo];} ''
          cp ${./Cargo.toml} Cargo.toml
          cp ${./rust-toolchain.toml} rust-toolchain.toml
          taplo fmt --check Cargo.toml rust-toolchain.toml
          touch $out
        '';

        # Design constraint (hermetic twin of justfile design-lint): literal color values are
        # allowed only in theme.rs. This rule previously never ran in CI; wiring it into checks
        # is what actually enforces it.
        design-lint =
          pkgs.runCommand "design-lint-check" {
            nativeBuildInputs = [pkgs.findutils pkgs.gnugrep];
          } ''
            cp -r ${./crates/ominiforge-ui/src} src
            chmod -R u+w src
            hits=$(grep -rnE '\b(rgb|rgba|hsla)\s*\(' \
              $(find src -name '*.rs' -not -name 'theme.rs') || true)
            if [ -n "$hits" ]; then
              echo "design-lint: color literal outside theme.rs:" >&2
              echo "$hits" >&2
              exit 1
            fi
            touch $out
          '';

        # Hermetic twin of justfile lint-english (AGENTS.md §14): flag any non-ASCII letter
        # (\p{L} outside a-z/A-Z) in code, comments, config, and CI. Punctuation/symbols are
        # allowed. frontend/ is excluded (slated for removal, going i18n); doc/ prose may be any
        # language. A line ending in `lint-english: allow` is skipped (intentional test data).
        # Uses ripgrep: its Unicode class handling is locale-independent (no LC_ALL workaround
        # needed, unlike GNU grep), and it treats recursed dirs and listed files uniformly.
        lint-english =
          pkgs.runCommand "lint-english-check" {
            nativeBuildInputs = [pkgs.ripgrep];
            src = pkgs.lib.cleanSourceWith {
              src = ./.;
              filter = path: _type: let
                baseName = builtins.baseNameOf path;
              in
                !(builtins.elem baseName [".direnv" ".git" "target" "result" "frontend" "doc"]);
            };
          } ''
            cp -r $src src
            chmod -R u+w src
            cd src
            hits=$(rg --no-config -n '[\p{L}--\x{00}-\x{7F}]' \
              -g '*.rs' -g '*.toml' -g '*.yml' -g '*.yaml' \
              -g 'justfile' -g 'flake.nix' -g 'flake.lock' -g 'deny.toml' \
              -g 'clippy.toml' -g 'rustfmt.toml' -g 'statix.toml' \
              crates .github justfile flake.nix flake.lock deny.toml clippy.toml rustfmt.toml statix.toml \
              2>/dev/null | rg -v 'lint-english: allow' || true)
            if [ -n "$hits" ]; then
              echo "lint-english: non-English letter found (AGENTS.md §14 requires English):" >&2
              echo "$hits" >&2
              exit 1
            fi
            touch $out
          '';

        cargo-check = rustPlatform.buildRustPackage {
          pname = "ominiforge-check";
          version = "0.1.0";
          src = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter = path: _type: let
              baseName = builtins.baseNameOf path;
            in
              !(builtins.elem baseName [".direnv" ".git" "target" "result"]);
          };
          cargoLock = {
            lockFile = ./Cargo.lock;
            # gpui is a git dependency and pulls in zed's git crates (e.g. collections). Use the
            # builtin fetchGit for its hash, else vendoring fails with 'No hash was found'.
            allowBuiltinFetchGit = true;
          };
          # Only verify the offline build. Tests (especially the LSP integration tests, which need
          # real language-server processes) cannot run in the hermetic sandbox — they are covered by
          # CI's cargo test/nextest (the devShell provides the language servers), not here.
          doCheck = false;
        };
      };
    });
}
