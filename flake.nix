{
  description = "celo-kona — Celo extensions to op-reth and Kona";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        # Stable toolchain, pinned to the channel in ./rust-toolchain.toml (1.94).
        # The `default` profile ships rustfmt + clippy; we add rust-src,
        # llvm-tools-preview (proof/coverage) and the bare-metal target used by
        # the no-std client (`just hack`, ./.github/scripts/check_no_std.sh).
        rustToolchain = pkgs.rust-bin.stable."1.94.0".default.override {
          extensions = [ "rust-src" "llvm-tools-preview" ];
          targets = [ "riscv32imac-unknown-none-elf" ];
        };

        # Nightly toolchain for the two CI jobs that need it: `cargo-lint`'s
        # `cargo +nightly fmt` check and `cargo-udeps`. Pinned to the same
        # nightly CI pins for udeps (see rust_ci.yml / CELO_UDEPS_TOOLCHAIN).
        nightlyToolchain = pkgs.rust-bin.nightly."2026-05-17".default.override {
          extensions = [ "rustfmt" ];
        };

        # C/C++ toolchain + libs for the *-sys crates.
        #   - librocksdb-sys builds bundled RocksDB C++ via bindgen (needs a
        #     compiler + libclang) with all compression backends on under
        #     --all-features, so its headers must be reachable: bzip2 (bzlib.h),
        #     snappy, lz4, zstd, zlib.
        #   - openssl-sys / native-tls: openssl (+ pkg-config).
        # Putting the libs in buildInputs makes the wrapped compiler add their
        # include/lib paths automatically.
        systemDeps = with pkgs; [
          pkg-config
          cmake
          clang
          mold
          openssl
          bzip2
          snappy
          lz4
          zstd
          zlib
        ];

        # Env the *-sys builds need.
        buildEnv = {
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          # nixpkgs splits openssl: headers + *.pc in `.dev`, the actual
          # libssl/libcrypto in `.out`. openssl-sys can't use a combined
          # OPENSSL_DIR here (it would look for libs under the dev prefix and
          # fail), so point it at each output explicitly. Keep PKG_CONFIG_PATH
          # for the other *-sys crates.
          OPENSSL_NO_VENDOR = "1";
          OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
          OPENSSL_INCLUDE_DIR = "${pkgs.openssl.dev}/include";
          PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";
        };
      in
      {
        # Primary shell: everything needed for the Rust + e2e CI jobs.
        #   just test                       -> cargo-tests
        #   just build-native               -> cargo-build
        #   just hack                       -> cargo-hack
        #   just test-trybuild              -> trybuild
        #   ./.github/scripts/check_no_std.sh -> check-no-std
        #   cargo deny check                -> deny
        #   e2e_test/run_all_tests.sh       -> e2e-tests
        #   cargo clippy ... -D warnings    -> cargo-lint (clippy step)
        devShells.default = pkgs.mkShell (buildEnv // {
          buildInputs = systemDeps ++ (with pkgs; [
            rustToolchain

            # Justfile / CI tooling
            just
            cargo-nextest
            cargo-hack
            cargo-deny
            cargo-llvm-cov # `proof` job coverage

            # e2e-tests job (e2e_test/run_all_tests.sh)
            foundry # cast, forge, anvil
            nodejs_22 # node, npm (CI uses 20; 22 LTS is fine for the JS e2e tests)
            jq
            curl
          ]);

          shellHook = ''
            echo "celo-kona dev shell — $(rustc --version)"
            echo
            echo "Run the CI checks locally:"
            echo "  just test                          # cargo-tests"
            echo "  just build-native                  # cargo-build"
            echo "  just hack                          # cargo-hack"
            echo "  just test-trybuild                 # trybuild (main-only in CI)"
            echo "  ./.github/scripts/check_no_std.sh  # check-no-std"
            echo "  cargo deny check                   # deny"
            echo "  e2e_test/run_all_tests.sh          # e2e-tests"
            echo "  cargo clippy --workspace --all-features --all-targets -- -D warnings   # cargo-lint (clippy)"
            echo
            echo "The two nightly-gated jobs (fmt check, udeps) use 'cargo +nightly',"
            echo "which rustup resolves but this pure-Nix toolchain can't. Run them via"
            echo "the nightly shell instead:"
            echo "  nix develop .#nightly -c cargo fmt --all -- --check                              # cargo-lint (fmt)"
            echo "  nix develop .#nightly -c cargo udeps --workspace --all-features --all-targets     # cargo-udeps"
          '';
        });

        # Nightly shell for the `cargo +nightly` CI steps. Here `cargo` IS the
        # pinned nightly, so run the underlying commands directly (without the
        # rustup-only `+nightly` selector the Justfile uses).
        devShells.nightly = pkgs.mkShell (buildEnv // {
          buildInputs = systemDeps ++ [
            nightlyToolchain
            pkgs.cargo-udeps
          ];

          shellHook = ''
            echo "celo-kona NIGHTLY shell — $(rustc --version)"
            echo "  cargo fmt --all -- --check                              # cargo-lint (fmt)"
            echo "  cargo udeps --workspace --all-features --all-targets    # cargo-udeps"
          '';
        });
      });
}
