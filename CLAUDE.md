# CLAUDE.md

`pinboard-sync` is a Rust CLI that syncs saved and favorited items from Reddit, GitHub and
HackerNews to Pinboard; its `cleanup` subcommand normalizes existing bookmarks. Don't
duplicate [README.md](README.md), which covers what it does, authentication and usage.

The rules below are the ones no test or lint catches for a new change. Each names the hazard
and where the full reason lives; the module docs (`//!`) describe everything else.

## Commands

Run all tooling through the Nix dev shell so versions match the flake:

```sh
nix develop --command cargo build
nix develop --command cargo test
nix develop --command cargo test <name>      # single test
PROPTEST_CASES=10000 nix develop --command cargo test  # property tests past test_support::proptest_config's 64 cases
nix develop --command cargo clippy --all-targets -- -D warnings
nix develop --command cargo fmt --check
nix develop --command cargo machete          # fails on an unused dependency
nix develop --command cargo deny --locked check  # advisories, licenses, duplicates, sources (deny.toml)
nix develop --command cargo mutants --in-diff <diff>  # mutation-tests the lines a diff file changes
nix flake check                              # builds the package (runs tests) + evaluates the NixOS module and config-checks its TOML + lints the Nix files
nix fmt                                      # formats the Nix files (nixfmt, via nixfmt-tree)
RUSTDOCFLAGS='-D warnings' nix develop --command cargo doc --document-private-items --no-deps  # fails on a rustdoc warning
nix develop --command typos                  # spell-checks the tree; false positives go in _typos.toml
nix develop --command actionlint             # lints the workflows, with shellcheck on their scripts
nix develop --command zizmor --offline .github/  # audits the workflows and dependabot.yml for security problems
```

**CI's `lint` job is part of the gate**, beside clippy and fmt: run typos, actionlint,
zizmor, cargo-machete, `cargo deny --locked check` and the rustdoc build before pushing. An
exception goes where the tool reads it, as narrowly as it can be and with its reason: a word
in `_typos.toml`, a dependency cargo-machete can't see used in `ignored` under
`[package.metadata.cargo-machete]`, an advisory `ignore` in `deny.toml`.

**Prefer a lint to a rule here.** A rule that can be checked mechanically goes in
`Cargo.toml`'s `[lints.clippy]`, `clippy.toml` (`disallowed-methods`, `-types`, `-macros`),
a test or a CI check, with `#[expect(..., reason = "...")]` at a legitimate exception;
CLAUDE.md keeps only what none of those can catch; these house rules are checked already:

- Command output goes through [`src/stdout.rs`](src/stdout.rs) and messages through `log`:
  `print_stdout`, `print_stderr`, and `disallowed-methods` on `io::stdout` and `io::stderr`.
- The command line is parsed by `cli::parse_with`: `disallowed-methods`.
- Clients start from `http::client_builder()` (timeouts, h2 pings): `disallowed-methods`,
  which can't see `Client::default()` or `ClientBuilder::default()`, so call neither.
- A test's directory comes from `test_support::scratch_dir`: `disallowed-methods`.
- An environment variable is read through `secrets::env_var`, which a test fakes with
  `FakeSecretEnv`: `disallowed-methods`, which can't see clap's own `env` reads, so a test
  parses with `test_support::parse_cli`, which drops them.
- reqwest uses native-tls with ALPN, since Reddit's edge 403s rustls's TLS fingerprint and
  a ClientHello with no ALPN (#78): `deny.toml` bans the `rustls` crate, and
  `http::net_tests::client_hello_offers_alpn_h2_and_http1` checks the ALPN.

## Cleanup

**A failed lookup is not a failed run**: `cleanup` runs on a timer, and one dead URL must
not wedge it in a failed state. `PassOutcome::into_result` is the one place the exit code is
decided; its doc gives the majority rule and why.

**A target whose occupant moves away is safe only through all three of `write_order`, the
`propagate_refusals` fixpoint, and the write loop marking a failed move's URL occupied and
re-closing that fixpoint.** Each has its own test; drop one and the others no longer protect
the record left behind. A cycle of moves has no safe order (`write_order`).

**`Plan::Skipped` means no lookup was made.** A new pass returns it only for a bookmark it
never asked the source about, never for one the source answered; otherwise an unreachable
source looks healthy (`Plan`'s doc).

**`fit` and `post_add` must share `fitted_note`**, so the cleanup driver diffs a plan
exactly as Pinboard will store it; a note cut differently from how the store cuts it is
rewritten every run (`BookmarkStore::fit`, `PinboardClient::measured`).

**`cleanup github` refreshes only what the tool wrote**: notes in `github_note`'s shape, the
base, language and owner tags of bookmarks with a configured account's base tag (by that
account's tag config), the title while it is still `owner/repo`, and the star date while the
title and notes are both still the tool's (`github::cleanup::run`). Never widen that: a repo
root may be another source's bookmark, like an HN story, or one a rename merged a star into.

## Sources and sync

**A dedup key must survive cleanup's canonicalization**, or sync re-adds what cleanup moved
([`src/keys.rs`](src/keys.rs)). `HackerNewsKeys` keys a Reddit or GitHub article through
`RedditKeys` / `GitHubKeys`, so a change to either, or a new rewrite in `cleanup reddit` or
`cleanup github`, needs a matching key test, HN included.

**To add a source**, implement `Source` ([`src/source.rs`](src/source.rs)), add its account
array and `config::Account` impl to `config.rs`, and implement `SourceAccount`
([`src/jobs.rs`](src/jobs.rs)), the one trait `sync`, `backup` and `doctor` use.

**GitHub's `/user/starred` read pins `sort=created&direction=desc`** (`github::STARS`): the
incremental read stops at the first caught-up page, which is only right
newest-starred-first, so the order must not rest on the API's default.

## Secrets, errors and the hook

**Every secret keeps its `_FILE` form; never add a `$VAR`-only one**
([`nix/module.nix`](nix/module.nix)). The NixOS service feeds sops-nix *paths* through
`$VAR_FILE`, keeping values out of the unit environment.

**A message names the setting a secret came from, never its value**
(`secrets::ConfiguredSecret`). A response body goes into an error only through
`http::error_with_body` and into the log only through `http::log_body_at_debug`, each given
the client's secrets to `http::redact`; Pinboard text goes through
`pinboard::redacted_error`, which also strips the token's percent-encoded form and any
`auth_token=` value, so a new Pinboard error path must use it.

**Never put a request URL with a query into a message**: Pinboard's `auth_token` rides in
it, and `reqwest::Error` prints the URL in `Display` and `Debug`. `RetryingClient::send`
strips it; a new error path must too.

**`cleanup` entry points return `Result<(), SourceError>`, not `anyhow::Result`**:
flattening loses `Halt::Reauth`, so `hook::handle_source_err` never fires the hook.

**`cleanup`, `sync --all` and `backup --all` read `PINBOARD_SYNC_ON_AUTH_FAILURE`
themselves** (`hook::on_auth_failure_from_env`): [`nix/module.nix`](nix/module.nix) sets
`onAuthFailure` only in the unit environment, never the generated TOML, so dropping one of
those lookups leaves that timer's hook dead.

**A Reddit 403 is classified by its body, not its status alone** (`reddit::reddit_body`,
`reddit::edge_page`): the edge's "blocked by network security" page arrives with a valid
cookie (#78), so it must not fire the hook.

## Gotchas

- **The Nix package build skips `net_tests`**, since the sandbox forbids sockets, so a test
  outside a `net_tests` module must stay hermetic or it fails only under `nix flake check`.
  Reproduce a build-only proptest failure with
  `PROPTEST_RNG_SEED=1 nix develop --command cargo test --release`.
- **No paused clock under a real socket**: `start_paused` auto-advances whenever the runtime
  idles on I/O, so a timer live during a request fires early, unless that firing is what the
  test expects. Run the fetch inside `http::recording_waits` and send a client's new pause
  through `http::sleep`; otherwise a `net_tests` case against a dead port sleeps the real
  ~2.5-minute `CONNECT_BACKOFF`.
- **Nix sees only git-tracked files**: when dev-shell `cargo` passes but `nix flake check`
  says `file not found`, `git add` the new file, or add it to
  [`nix/package.nix`](nix/package.nix)'s fileset if it is outside `src/`.
- **`SyncSource`, `CleanupSource` and `BackupTarget` spell their variants `Github` and
  `Hackernews`, not `GitHub`/`HackerNews`**: clap derives the subcommand names from them.
