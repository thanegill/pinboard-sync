{
  description = "Sync saved/favorited items from multiple services to a Pinboard account";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    let
      systems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
    in
    flake-utils.lib.eachSystem systems (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "pinboard-sync";
          version = "0.6.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          # reqwest uses native-tls (see Cargo.toml): on Linux that's OpenSSL via
          # openssl-sys, which needs pkg-config + openssl at build time. On darwin
          # native-tls uses the Security framework (provided by the stdenv).
          # installShellFiles provides `installShellCompletion` for postInstall.
          nativeBuildInputs = [
            pkgs.pkg-config
            pkgs.installShellFiles
          ];
          buildInputs = [ pkgs.openssl ];
          # The `net_tests` integration tests spin up a mock HTTP server, which
          # can't bind a socket in the build sandbox. They run under `cargo test`
          # in the dev shell / CI; the sandboxed build runs everything else.
          checkFlags = [ "--skip=net_tests" ];
          # Generate shell completions, a man page, and the example config from the
          # built binary.
          postInstall = ''
            installShellCompletion --cmd pinboard-sync \
              --bash <($out/bin/pinboard-sync completions bash) \
              --zsh <($out/bin/pinboard-sync completions zsh) \
              --fish <($out/bin/pinboard-sync completions fish)
            # installManPage reads the section from the filename suffix, so write to
            # a `.1` file rather than a process substitution.
            $out/bin/pinboard-sync man > pinboard-sync.1
            installManPage pinboard-sync.1
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

        checks = {
          package = self.packages.${system}.default;

          nixos-module =
            let
              nixos = nixpkgs.lib.nixosSystem {
                modules = [
                  self.nixosModules.default
                  {
                    nixpkgs.hostPlatform = "x86_64-linux";
                    boot.isContainer = true;
                    system.stateVersion = nixpkgs.lib.trivial.release;
                    services.pinboard-sync = {
                      enable = true;
                      cleanup.enable = true;
                      backup.enable = true;
                      pinboardTokenFile = "/run/secrets/pinboard-token";
                      settings.hackernews = [
                        {
                          enable = true;
                          username = "example";
                        }
                        {
                          enable = false;
                          username = "disabled";
                        }
                      ];
                    };
                  }
                ];
              };
              # The host's `pkgs`, not the system's: the module's own config file is an
              # x86_64-linux derivation, which a darwin check would have to build remotely.
              # Keep the settings above literal for the same reason: a store-path string
              # would carry its x86_64-linux context into this file.
              renderedConfig =
                (pkgs.formats.toml { }).generate "pinboard-sync.toml"
                  nixos.config.services.pinboard-sync.renderedSettings;
            in
            # Discarding the context keeps the x86_64-linux system out of this check's
            # inputs, which would otherwise have to be built here.
            pkgs.runCommand "pinboard-sync-nixos-module"
              { nativeBuildInputs = [ self.packages.${system}.default ]; }
              ''
                summary=$(pinboard-sync config check ${renderedConfig})
                echo "$summary"
                [[ $summary == *" 1 hackernews account(s)"* ]]
                echo ${builtins.unsafeDiscardStringContext nixos.config.system.build.toplevel.drvPath} > $out
              '';
        };
      }
    )
    // {
      nixosModules.pinboard-sync = import ./nix/module.nix self;
      nixosModules.default = self.nixosModules.pinboard-sync;
    };
}
