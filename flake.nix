{
  description = "Sync saved/favorited items from multiple services to a Pinboard account";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };
  };

  outputs =
    inputs@{
      self,
      nixpkgs,
      flake-parts,
    }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];

      perSystem =
        {
          lib,
          pkgs,
          self',
          ...
        }:
        {
          packages.default = pkgs.callPackage ./nix/package.nix { };

          formatter = pkgs.nixfmt-tree;

          devShells.default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              rustc
              rustfmt
              clippy
              rust-analyzer
              cargo-mutants
              typos
              actionlint
              zizmor
            ];
            # native-tls (reqwest) needs OpenSSL on Linux; pkg-config locates it.
            # No-op on darwin, which links the Security framework instead.
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = [ pkgs.openssl ];
          };

          checks = {
            package = self'.packages.default;

            nix-lint =
              pkgs.runCommand "pinboard-sync-nix-lint"
                {
                  nativeBuildInputs = [
                    pkgs.nixfmt
                    pkgs.statix
                    pkgs.deadnix
                  ];
                  src = lib.fileset.toSource {
                    root = ./.;
                    fileset = lib.fileset.fileFilter (file: file.hasExt "nix") ./.;
                  };
                }
                ''
                  cd "$src"
                  find . -name '*.nix' -exec nixfmt --check {} +
                  statix check .
                  deadnix --fail .
                  touch "$out"
                '';

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
                        backup.directory = "/var/lib/pinboard-sync/100%";
                        onAuthFailure = "printf %s-alert";
                        environmentFile = "/run/secrets/pinboard%-env";
                        pinboardTokenFile = "/run/secrets/pinboard%-token";
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
                unitFile =
                  name:
                  pkgs.writeText name (builtins.unsafeDiscardStringContext nixos.config.systemd.units.${name}.text);
              in
              # Discarding the context keeps the x86_64-linux system out of this check's
              # inputs, which would otherwise have to be built here.
              pkgs.runCommand "pinboard-sync-nixos-module" { nativeBuildInputs = [ self'.packages.default ]; } ''
                summary=$(pinboard-sync config check ${renderedConfig})
                echo "$summary"
                [[ $summary == *" 1 hackernews account(s)"* ]]
                if grep -F on_auth_failure ${renderedConfig}; then
                  echo "onAuthFailure leaked into the rendered config" >&2
                  exit 1
                fi
                grep -Fx 'Environment="PINBOARD_SYNC_ON_AUTH_FAILURE=printf %%s-alert"' \
                  ${unitFile "pinboard-sync.service"}
                grep -Fx 'Environment="PINBOARD_SYNC_ON_AUTH_FAILURE=printf %%s-alert"' \
                  ${unitFile "pinboard-sync-cleanup.service"}
                grep -E '^ExecStart=".*/bin/pinboard-sync" "cleanup" "--all"$' \
                  ${unitFile "pinboard-sync-cleanup.service"}
                grep -F '"--out" "/var/lib/pinboard-sync/100%%"' \
                  ${unitFile "pinboard-sync-backup.service"}
                grep -Fx 'EnvironmentFile=/run/secrets/pinboard%%-env' ${unitFile "pinboard-sync.service"}
                grep -Fx 'LoadCredential=pinboard-token:/run/secrets/pinboard%%-token' \
                  ${unitFile "pinboard-sync.service"}
                echo ${builtins.unsafeDiscardStringContext nixos.config.system.build.toplevel.drvPath} > $out
              '';
          };
        };

      flake = {
        nixosModules.pinboard-sync = import ./nix/module.nix self;
        nixosModules.default = self.nixosModules.pinboard-sync;
      };
    };
}
