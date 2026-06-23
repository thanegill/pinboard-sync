{
  description = "Sync saved/favorited items from multiple services to a Pinboard account";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "pinboard-sync";
          version = "0.2.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          # reqwest uses native-tls (see Cargo.toml): on Linux that's OpenSSL via
          # openssl-sys, which needs pkg-config + openssl at build time. On darwin
          # native-tls uses the Security framework (provided by the stdenv).
          # installShellFiles provides `installShellCompletion` for postInstall.
          nativeBuildInputs = [ pkgs.pkg-config pkgs.installShellFiles ];
          buildInputs = [ pkgs.openssl ];
          # The `net_tests` integration tests spin up a mock HTTP server, which
          # can't bind a socket in the build sandbox. They run under `cargo test`
          # in the dev shell / CI; the sandboxed build runs everything else.
          checkFlags = [ "--skip=net_tests" ];
          # Generate shell completions and the example config from the built binary.
          postInstall = ''
            installShellCompletion --cmd pinboard-sync \
              --bash <($out/bin/pinboard-sync completions bash) \
              --zsh <($out/bin/pinboard-sync completions zsh) \
              --fish <($out/bin/pinboard-sync completions fish)
            mkdir -p $out/share/pinboard-sync
            $out/bin/pinboard-sync config example > $out/share/pinboard-sync/config.example.toml
          '';
          meta = {
            description = "Sync saved/favorited items from multiple services to a Pinboard account";
            mainProgram = "pinboard-sync";
          };
        };

        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
            rust-analyzer
            cargo-llvm-cov
          ];
          # native-tls (reqwest) needs OpenSSL on Linux; pkg-config locates it.
          # No-op on darwin, which links the Security framework instead.
          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [ pkgs.openssl ];
          # cargo-llvm-cov needs llvm-tools matching rustc's LLVM. `rustc.llvmPackages.llvm`
          # is exactly that LLVM and tracks the toolchain, so there's no version to keep in sync.
          LLVM_COV = pkgs.lib.getExe' pkgs.rustc.llvmPackages.llvm "llvm-cov";
          LLVM_PROFDATA = pkgs.lib.getExe' pkgs.rustc.llvmPackages.llvm "llvm-profdata";
        };
      }
    )
    // {
      nixosModules.pinboard-sync = import ./nix/module.nix self;
      nixosModules.default = self.nixosModules.pinboard-sync;
    };
}
