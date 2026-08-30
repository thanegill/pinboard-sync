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
`CleanupPass::plan` (the desired end-state for one bookmark, as a `Plan` — see below —
with `Err` for a per-item failure), and `run_pass` owns the loop common to all of them:
plan every bookmark, group the plans by target URL, then diff each plan against the stored
one, skip unchanged, render the dry-run lines, write via `apply_update` (deleting the old
URL on a rewrite), and tally into a `PassOutcome`. Colliding rewrites — several bookmarks
normalizing to one URL — are field-merged into a single record via `apply_merge` (which
deletes the absorbed URLs) rather than clobbering each other.

**A failed lookup is not a failed run.** `PassOutcome` counts `plan_failed` (a link we
couldn't read from the source) separately from `write_failed` (Pinboard refusing us, on
either `apply_update` or `apply_merge`), and `PassOutcome::into_result` — the single place
the exit code is decided — fails only on the latter, plus a dead credential and lookups
failing in the **majority** (`plan_failed > reached.max(1)` — one failed lookup is always
survivable, which matters for a pass whose steady-state population is one or two, like the
discussion-link pass that strips its own marker tag). The majority test rather than "all
of them" because a `plan` failure can no longer be one dead link: every permanent per-item
condition now reaches the driver as a `Plan` (a deleted or blocked repo is a
`Plan::Bookmark` keeping its canonical URL; a URL the pass doesn't handle is
`Plan::Skipped`), leaving `plan_failed` to mean rate limits, post-retry 5xx, and network
trouble. A handful of those is a blip worth riding out; most of them failing means most of
the work silently did not happen. This is deliberate: `cleanup`
runs as a scheduled service, and a permanently dead URL must not wedge it into a failed
state forever. `SourceError::ReauthRequired` and `SourceError::RateLimited` stop the pass
(a dead credential and an exhausted quota both fail every remaining lookup too) but the
plans already made are still written — which is why `plan`
returns `SourceError` rather than a flattened `anyhow::Error`. `PassOutcome::halted`
records *which* of the two it was as a `Halt`, and `into_result` maps them to different
`SourceError`s so `main` fires the auth-failure hook for `Halt::Reauth` only — no
credential change clears a rate limit.

**No bookmark is ever replaced by a plan that isn't about it.** `run_pass` takes the
account's *whole* bookmark set (`residents`) alongside the slice it plans, because **a plan
can target a URL its own filter excludes**: HN's item pass rewrites a story bookmark to its
*article* URL, which is not an HN item URL and so was never in the slice. Checking
residency against the slice alone silently destroyed a separately-saved article's notes and
tags.

Bookmarks the pass produces no plan for are split by whether their stored record is the
final word on them. **`mergeable`** — outside the slice, or `Plan::Skipped`/`Unchanged` —
means we deliberately didn't plan it, so what is stored is current: a plan landing on that
URL takes the resident in as a participant in the same `merge_bookmarks` collision merge
(resident first, so its title wins), and the mover's old URL is absorbed. Pinboard holds one
record per URL, so the end state there *must* be a single bookmark; merging is what makes
that lossless and convergent. The resident's `read_later` and `timestamp` are restored after
the merge — it is the record that stays, and `merge_bookmarks`'s any()/earliest rules are
for two *plans*, so without that a merge would re-date a bookmark even with dating off.
`public` is deliberately **not** restored: never-widen outranks it, because the mover's
notes land on this record and a private member's annotation must not be republished. **`untouchable`** — the lookup failed, or a dead credential
halted the pass first — means we couldn't establish its state, so nothing is written there
at all; the rewrite is refused and counted in `PassOutcome::refused`. That refusal
propagates: a refused group doesn't move, so its own URL stays occupied and becomes
untouchable too, iterated to a fixpoint before any write so the result is order-independent.

**`Plan` says whether the source was reached, not just whether to write.** `plan` returns
`Plan::Bookmark` (an end-state), `Plan::Unchanged` (*the source answered*, nothing to do),
or `Plan::Skipped` (not this pass's bookmark — no lookup made). The last two both write
nothing but mean opposite things about the source's health, and only the first two count
toward `PassOutcome::reached`. Without that split, the every-lookup-failed guard is
worthless: `cleanup github` skips deep links and gists locally, and if those counted as
successes a genuinely unreachable GitHub would look fine. Conversely HackerNews'
`search_by_url` finding no discussion is `Unchanged`, not `Skipped` — the query ran. When
a source runs several passes, each reaches its **own** verdict rather than pooling
tallies, so a pass that can't fail can't vouch for one that can.

The per-source planners live in [`src/cleanup.rs`](src/cleanup.rs) (reddit),
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

**`backup` has one port and one driver, and captures raw bodies in the same traversal.**
[`src/backup.rs`](src/backup.rs) is `cleanup_pass.rs`'s sibling: a service implements
`BackupSource::dump` (returning a `BackupDump` — captured `RawPage`s plus normalized
`ExportBookmark`s), and the driver owns everything shared — `layout` (pure: payload →
named files), `write_files` (atomic, mode 0600, per-file sanity check), the manifest and
the dry-run rendering. Clients never learn where a file goes. Raw fidelity comes from a
`RawSink` threaded through each client's *existing* pagination: `sync`/`cleanup` pass
`RawSink::disabled()` (one branch, retains nothing, so their typed `from_str` path is
unchanged), `backup` passes `collecting()`. Never re-walk pagination for a raw pass — the
two halves must describe one instant. `PinboardClient` implements `BackupSource` too, so
the destination is a target like any source. `ExportBookmark` is deliberately *not* a
`Serialize` on the domain `Bookmark`: the snapshot format is a stable output contract, and
a domain rename must not silently change an operator's files.

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
and never re-auths. **`cleanup` fires it too, not just `sync`** — which is why the three
`cleanup` entry points return `Result<(), SourceError>` rather than `anyhow::Result`:
flattening the variant at that boundary is exactly what used to lose the hook. Every
re-auth-capable read in `cleanup` is covered, not only the pass — GitHub's `repo()`
mid-pass (via `Halt`), GitHub's `fetch()` for `use_post_date` star dates, and Reddit's
`/api/info`, whose expired cookie is by far the most common in practice. `main`'s
`handle_source_err` is the single place the hook is fired, shared with `sync`; `cleanup`
resolves the command through `PINBOARD_SYNC_ON_AUTH_FAILURE` → account →
`[defaults.<source>]` → `[hooks]` (no `--on-auth-failure` flag on `cleanup`, unlike
`sync`, whose per-source flag is itself env-backed). **Reading that env var is
load-bearing**: [`nix/module.nix`](nix/module.nix) exports `onAuthFailure` only into the
unit environment — it never reaches the generated TOML — and runs the `cleanup --all`
timer with it, so resolving from config alone would leave the hook dead for the
deployment it exists to serve. Both clients send through
`http::send_retrying`
([`src/http.rs`](src/http.rs)), which backs off on network errors / 429 / 5xx but
returns other statuses as-is.

**`SourceError::RateLimited` is the third case, and deliberately not either of those.**
GitHub answers both its primary and secondary rate limits with a **403 or 429** — the
same statuses a real permission denial uses — so `github::rate_limit_message` matches on
the headers instead: `x-ratelimit-remaining: 0` (with `x-ratelimit-reset` for the
instant) or a `retry-after`. It must not become `ReauthRequired` (no credential change
clears a quota, and firing the hook would send the operator after the wrong problem) nor
a permanent skip like the `451` arm. Retrying is not the answer either: `send_retrying`'s
2s-linear backoff over 4 attempts totals ~12s, while a primary limit resets up to an hour
out and GitHub asks for a minute on secondary limits, so it would only fail more slowly.
The 403 form isn't retried; the **429** form still is, by the shared 429/5xx predicate, and
those ~12s are simply wasted before the response is read — harmless, and the reason a
`rate_limit_message` unit test uses a synthesized `HeaderMap` rather than a 429 over
wiremock. Stopping the pass is the response instead. Reddit's 403, by contrast, only ever
means a dead cookie, so it maps straight to `ReauthRequired`.

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
