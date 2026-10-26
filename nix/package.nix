{
  lib,
  rustPlatform,
  pkg-config,
  installShellFiles,
  openssl,
}:
rustPlatform.buildRustPackage (finalAttrs: {
  pname = "pinboard-sync";
  version = (lib.importTOML ../Cargo.toml).package.version;
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
    ];
  };
  cargoLock.lockFile = ../Cargo.lock;
  # reqwest uses native-tls (see Cargo.toml): on Linux that's OpenSSL via
  # openssl-sys, which needs pkg-config + openssl at build time. On darwin
  # native-tls uses the Security framework (provided by the stdenv).
  # installShellFiles provides `installShellCompletion` for postInstall.
  nativeBuildInputs = [
    pkg-config
    installShellFiles
  ];
  buildInputs = [ openssl ];
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
    mainProgram = finalAttrs.pname;
  };
})
