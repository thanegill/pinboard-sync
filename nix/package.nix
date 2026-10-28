{
  lib,
  rustPlatform,
  pkg-config,
  installShellFiles,
  openssl,
  versionCheckHook,
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
  # native-tls (see Cargo.toml) is OpenSSL on Linux, found through pkg-config; darwin
  # uses the Security framework from the stdenv.
  nativeBuildInputs = [
    pkg-config
    installShellFiles
  ];
  buildInputs = [ openssl ];
  # The `net_tests` modules bind a socket, which the build sandbox forbids; they run
  # under `cargo test` in the dev shell and CI.
  checkFlags = [ "--skip=net_tests" ];
  postInstall = ''
    installShellCompletion --cmd pinboard-sync \
      --bash <($out/bin/pinboard-sync completions bash) \
      --zsh <($out/bin/pinboard-sync completions zsh) \
      --fish <($out/bin/pinboard-sync completions fish)
    # installManPage reads the section from the file name, so no process substitution.
    $out/bin/pinboard-sync man > pinboard-sync.1
    installManPage pinboard-sync.1
    mkdir -p $out/share/pinboard-sync
    $out/bin/pinboard-sync config example > $out/share/pinboard-sync/config.example.toml
  '';
  nativeInstallCheckInputs = [ versionCheckHook ];
  doInstallCheck = true;
  meta = {
    description = "Sync saved/favorited items from multiple services to a Pinboard account";
    homepage = "https://github.com/thanegill/pinboard-sync";
    license = lib.licenses.mit;
    mainProgram = finalAttrs.pname;
  };
})
