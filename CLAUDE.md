# CLAUDE.md

Guidance for Claude Code working in this repository.

`pinboard-sync` is a Rust CLI that syncs saved/favorited items from multiple
services (Reddit, GitHub, HackerNews) to a Pinboard account, with a `cleanup`
subcommand that normalizes existing bookmarks. See [README.md](README.md) for what
it does, authentication, and usage — don't duplicate that here.

## Commands

Run all tooling through the Nix dev shell so versions match the flake:

```sh
nix develop --command cargo build
nix develop --command cargo test
nix develop --command cargo test <name>      # single test
nix develop --command cargo clippy --all-targets -- -D warnings
nix develop --command cargo fmt --check
nix flake check                              # builds the package (runs tests) + evaluates the NixOS module
```

`cargo clippy -D warnings` and `cargo fmt --check` are the gate — keep both clean.
Commit often: each working feature or structural piece, with its tests, keeping the
gate green.

## Architecture

The flow is one pass: a source yields drafts → skip those already on Pinboard →
write the rest. `main.rs` is CLI parsing + config/secret resolution + dispatch; the
sync write loop is in [`src/sync.rs`](src/sync.rs). **Every `cleanup` source shares one
driver** ([`src/cleanup_pass.rs`](src/cleanup_pass.rs)): a source implements
`CleanupPass::plan` (the desired end-state for one bookmark, as a `Bookmark` — `None` to
skip, `Err` for a per-item failure), and `run_pass` owns the loop common to all of them:
diff the planned `Bookmark` against the stored one, skip unchanged, render the dry-run
lines, write via `apply_update` (deleting the old URL on a rewrite), and tally. The
per-source planners live in [`src/cleanup.rs`](src/cleanup.rs) (reddit),
[`src/github.rs`](src/github.rs), and [`src/hackernews.rs`](src/hackernews.rs).

**`Bookmark` is service-agnostic; `PinboardBookmark` is the wire shape.** The domain
[`Bookmark`](src/bookmark.rs) (`url: Url`, `title`, `note`, `tags: Vec<String>`,
`timestamp: Option<OffsetDateTime>`, and `public`/`read_later` `bool`s — the domain's
names and *parsed* types, not the API's string `href`/`description`/`extended` and
`"yes"/"no"` `shared`/`toread`) lives in [`src/bookmark.rs`](src/bookmark.rs) along with
the `BookmarkStore` port — everything not specific to the Pinboard client.
[`src/pinboard.rs`](src/pinboard.rs) is just the client behind that port: it deserializes
`posts/all` into `PinboardBookmark` (space-joined tags, ISO-8601 `time`, `"yes"/"no"`
`shared`/`toread`) and converts each via `TryFrom` into the domain `Bookmark`. Because the
`url` is a parsed [`Url`], a bookmark whose `href` (or a source item's URL) doesn't parse
is skipped with a warning rather than aborting the run — `all()` and each source's
`fetch()` `filter_map` over the fallible conversion. Consumers then never re-parse the URL
(`bookmark.url.host_is(…)`, `HackerNewsItemId::try_from(&bookmark.url)`, etc.). The cleanup driver reads stored bookmarks and plans their end-state in
that one type; `Bookmark::diff` returns the written fields that changed (timestamps
compared by instant, not formatted string), which the driver uses both to skip unchanged
bookmarks and to render the dry-run. A write takes a whole `Bookmark`; `post_add` maps it
to the API params (`public`/`read_later` → `shared`/`toread`, `timestamp` → `dt`) at the
boundary.

**Every source is a `Source`; the sync loop is generic over it.** The port lives in
[`src/source.rs`](src/source.rs): `Source::fetch()` returns `Vec<BookmarkDraft>` (a
`Bookmark` plus the `dedup_key`), and the `UrlKey::dedup_key()` supertrait method
maps an existing Pinboard URL to that source's dedup key. `sync::run` fetches drafts,
builds the set of existing keys by mapping `pinboard.all()` through
`source.dedup_key`, and writes the drafts whose `dedup_key` isn't present. To add
a source, implement `Source` and wire it into `main.rs` + `config.rs`. The clients
also sit behind the `BookmarkStore` port (Pinboard; `add`/`update` both take a
`&Bookmark`) so the loops are unit-tested with
in-memory fakes ([`src/test_support.rs`](src/test_support.rs)); real clients are
covered by `net_tests` against a wiremock server via test-only `with_base_url(s)`
constructors.

**Sources:** Reddit ([`reddit.rs`](src/reddit.rs) — cookie-authenticated
`saved.json`/`api/info.json`, plus the `PostInfo` port for cleanup), GitHub
([`github.rs`](src/github.rs) — `/user/starred` with Link-header pagination),
HackerNews ([`hackernews.rs`](src/hackernews.rs) — scrapes `/favorites` for item IDs
then batch-reads item details from the Algolia HN search API — `objectID:… OR …`,
chunked, so hundreds of favorites cost a couple of queries, not one per item).
`cleanup hackernews --link-discussions` (default off) is the reverse: for article
bookmarks carrying the `link_tag` marker (default `find-hn`), it looks each up on HN
by URL (one Algolia query each) and adds the discussion link. Reddit's bookmark/tag
shaping lives in
[`src/model.rs`](src/model.rs).

**All tags are config-driven; there are no tag CLI flags.** Each source has a tag
config struct (`RedditConfig`, `GitHubConfig`, `HackernewsConfig`) of overridable
fields with built-in defaults; the `tags` list (default e.g. `["reddit"]`) is the
base tag plus any extras, and `push_tag`/`push_prefixed` in `source.rs` render them
(empty string disables a tag). `model.rs`'s tag tests are the spec — keep them in
sync with rule changes. Pinboard tags can't contain spaces (the API splits the tag
string on them), so `push_prefixed` slugs internal whitespace in the value (e.g. the
GitHub language `Jupyter Notebook` → `lang:jupyter-notebook`) and `Config::parse`
rejects whitespace in config-supplied tags/prefixes. Reddit's URL host is the configurable `reddit_domain` (used
by both sync `bookmark_url` and cleanup's `normalize_url`); `reddit_key` stays
host-agnostic so dedup matches across subdomains.

**Config + secret resolution.** [`src/config.rs`](src/config.rs) parses the
`--config` TOML: one `[pinboard]` destination, `[hooks]`, and per-source account
arrays (`[[reddit]]`/`[[github]]`/`[[hackernews]]`), each mapping to its tag config.
Secrets resolve through one ladder in `main.rs` (`resolve_secret`): CLI flag → `$VAR`
→ `$VAR_FILE` (a path whose trimmed contents are the value) → config inline → config
`*_file`. The `--config` path is the exception: it resolves flag → `$PINBOARD_SYNC_CONFIG`
only (a direct file path, no `_FILE` form — it is already a file). **The `_FILE` form is
load-bearing** — it's how the
NixOS service feeds sops-nix secret *paths* without putting values in the unit
environment. Don't add a secret that only reads `$VAR`.

**`sync <source> [account]` / `cleanup <source> [account]` select one account**
(by name, else the first, else an implicit CLI/env account). `--all` (per-source or
top-level) runs every configured account, aggregating failures via `AllRun` and
exiting non-zero if any fail. `cleanup --all` runs once per source (reddit, github,
hackernews), since it normalizes the shared bookmark set. `cleanup github`
canonicalizes repo-root URLs and looks each repo up via the API to rewrite
renamed/moved repos and refresh the title + language tag.

**`sync` builds `SyncJob`s and fetches concurrently.** Each account becomes a
`SyncJob { client: SourceClient, hook, limit }`; `SourceClient` is an enum over the
three clients implementing `Source`, so `build_jobs` + `run_sync_jobs` handle one
account and `--all` uniformly. Per-account settings
(`toread`/`public`/`limit`/`use_post_date`/…) resolve CLI flag → account →
`[defaults.<source>]` → `[pinboard]` through the generic `resolve_setting` helper and
`DateSettings`, reading the shared override fields via the `config::Account` trait. `run_sync_jobs` fetches every job's source
concurrently via `futures::future::join_all` (reads only, on one task — the client
futures aren't `Send`, so no `tokio::spawn`), then writes the merged, URL-deduped
drafts **sequentially** through one rate-limited writer (`sync::write_drafts`).
Reads parallel, writes serial — `posts/all` is fetched once up front and shared.
`completions <shell>` and `config example` are utility subcommands the Nix package
install consumes.

**Re-auth vs other errors drive a side effect.** `SourceError::ReauthRequired`
(Reddit 401/403 cookie expiry, GitHub 401) fires the `--on-auth-failure` hook and
exits non-zero; `SourceError::Other` (transient/parse) doesn't. HackerNews is public
and never re-auths. Both clients send through `http::send_retrying`
([`src/http.rs`](src/http.rs)), which backs off on network errors / 429 / 5xx but
returns other statuses as-is.

**Pinboard field names are inverted.** In `posts/add`, `description` is the *title*
and `extended` is the notes (delicious backcompat). Bookmarks are written
`replace=yes` (idempotent) and `shared=no` (private) by default.

## Gotchas

- **native-tls, not rustls.** Reddit's anti-bot edge 403s rustls's TLS fingerprint
  but accepts native-tls's, so `reqwest` is built with `native-tls`. On Linux that's
  OpenSSL via `openssl-sys`, so the flake carries `pkg-config` + `openssl` and the
  closure includes OpenSSL. Keep it this way.
- **The Nix package build runs the tests** (`buildRustPackage` `doCheck`), so a
  failing test blocks `nix build`/`nix flake check`. The `net_tests` modules bind a
  socket, which the sandbox forbids — the flake's `checkFlags = [ "--skip=net_tests" ]`
  excludes them there; they still run under `cargo test`. Keep all *other* tests
  hermetic (no network/sockets).
- **`nix build`/`nix flake check` only see git-tracked files.** A new untracked
  module (or `config.example.toml`, used via `include_str!`) fails the sandbox build
  with `file not found`, even though dev-shell `cargo` passes — that discrepancy is
  the tell. `git add` new files before building with Nix.
- The User-Agent / client identifier derives from `CARGO_PKG_NAME`/`CARGO_PKG_VERSION`.
- **Service-specific, API-shaped structs carry a service prefix** (`GitHubRepo`,
  `HackerNewsItem`, `RedditSavedItem`, `AlgoliaSearchResponse`) — not bare `Repo`/`Item`.
  GitHub is spelled `GitHub` in type names (e.g. `GitHubConfig`, `GitHubClient`).
