# CLAUDE.md

Guidance for Claude Code working in this repository.

`pinboard-sync` is a Rust CLI that syncs saved/favorited items from multiple services
(Reddit, GitHub, HackerNews) to a Pinboard account, with a `cleanup` subcommand that
normalizes existing bookmarks. See [README.md](README.md) for what it does, authentication,
and usage — don't duplicate that here.

## Commands

Run all tooling through the Nix dev shell so versions match the flake:

```sh
nix develop --command cargo build
nix develop --command cargo test
nix develop --command cargo test <name>      # single test
nix develop --command cargo clippy --all-targets -- -D warnings
nix develop --command cargo fmt --check
nix develop --command cargo mutants --in-diff <diff>  # mutation-tests the lines a diff file changes
nix flake check                              # builds the package (runs tests) + evaluates the NixOS module and config-checks its TOML + lints the Nix files
nix fmt                                      # formats the Nix files (nixfmt, via nixfmt-tree)
```

`nix flake check` runs the flake's `checks` for the host system only: `checks.package`
is the package build, whose `doCheck` runs the hermetic tests, and `checks.nixos-module`
instantiates an x86_64-linux system with the module and all three timers enabled, which
fails on a module assertion or an undefined option without building anything for Linux.
It also runs the host's `pinboard-sync config check` on the module's rendered settings,
re-rendered from the internal `services.pinboard-sync.renderedSettings` option with the
host's `pkgs.formats.toml`, because the module's own config file is an x86_64-linux
derivation. So a key the binary would reject fails the check too. `checks.nix-lint`
runs `nixfmt --check`, statix and deadnix over every `.nix` file. CI runs
`nix flake check` on x86_64-linux, in its `flake` job.

`cargo clippy -D warnings` and `cargo fmt --check` are the gate — keep both clean.
Commit often: each working feature or structural piece, with its tests, keeping the
gate green.

## Architecture

The flow is one pass: a source yields drafts → skip those already on Pinboard → write the
rest. `main.rs` loads the config and dispatches to the drivers in
[`src/commands.rs`](src/commands.rs) (`run_sync`, `run_cleanup`, `run_backup`). Clap types
are in [`src/cli.rs`](src/cli.rs), secrets in [`src/secrets.rs`](src/secrets.rs), the
settings ladder (`resolve_setting`, `resolve_dates`, `resolve_job_settings`, `resolve_hook`)
in [`src/settings.rs`](src/settings.rs), job building, `open_pinboard` and `AllRun` in
[`src/jobs.rs`](src/jobs.rs) (`backup`'s in [`backup_jobs.rs`](src/jobs/backup_jobs.rs)),
the hook in [`src/hook.rs`](src/hook.rs), `doctor` in [`src/doctor.rs`](src/doctor.rs), and
the sync write loop in [`src/sync.rs`](src/sync.rs).

**Every `cleanup` source shares one driver**, [`src/cleanup_pass.rs`](src/cleanup_pass.rs),
whose module doc gives the loop. A source implements `CleanupPass::plan` (one bookmark's
end-state as a `Plan`, or `Err` for a per-item failure) and `run_pass` does the rest,
sequencing phases, each its own function: `plan_all`, `group_by_target`, `write_order`,
`residents_of`, `hold_back_visibility_mismatches`, then per group `refuse_group`,
`write_single` or `write_merge`, tallying into a `PassOutcome`.
[`src/cleanup_pass/merge.rs`](src/cleanup_pass/merge.rs) holds the merge rules
(`merge_bookmarks`, `incumbent_of`).

**A failed lookup is not a failed run.** `PassOutcome::into_result`, the single place the
exit code is decided, fails on `write_failed` (Pinboard refusing a write), a halt, or
`plan_failed` (a lookup that failed) in the **majority**: `plan_failed > reached.max(1)`, so
one failed lookup is always survivable, which matters for a pass whose steady-state
population is one or two, like the discussion-link pass that strips its own marker tag. Not
"all", because every permanent per-item condition reaches the driver as a `Plan` (a deleted
or blocked repo is a `Plan::Bookmark` keeping its canonical URL), leaving `plan_failed` to
mean post-retry 5xx, network trouble and unlabelled throttling: a few are a blip, most means
most of the work silently did not happen. `cleanup` runs as a scheduled service, and a
permanently dead URL must not wedge it into a failed state forever.

**A halt stops the pass but still writes the plans already made.** A `SourceError::Halted`
(`Halt::Reauth`, `RateLimited` or `Blocked`) fails every remaining lookup too, which is why
`plan` returns `SourceError` rather than a flattened `anyhow::Error`. `PassOutcome::halted`
records the `Halt`, and `into_result` hands it back as `SourceError::Halted` so the
`cleanup` command fires the auth-failure hook for `Halt::Reauth` only: no credential change
clears a rate limit or an edge block.

**The run has one live view of the account, not a snapshot.** `posts/all` is read once into
an `AccountView` ([`src/bookmark.rs`](src/bookmark.rs)), and `CleanupStore` wraps the real
store so every write updates it (see both types' docs); passes read occupancy
(`AccountState::resident`) and their slice (`snapshot`) from it. Both halves are
load-bearing: **a plan can target a URL its own filter excludes** (HN's item pass rewrites a
story bookmark to its *article* URL), and **`cleanup --all` runs github, then reddit, then
hackernews over one account**, each writing, so a snapshot taken before the first is stale
for the others. A dry run still advances the view, so a preview matches the run.

**No bookmark is replaced by a plan that isn't about it.** Pinboard holds one record per
URL, so plans colliding at a target are field-merged by `merge_bookmarks` into one record,
absorbing the movers' old URLs. At most one record *stays* there, and it leads: the
`resident` (the live view's occupant, never planned by this pass) or the `incumbent` (the
member whose *stored* URL is already the target; its *plan* leads). A bookmark is never a
resident of its own plan, or it would merge its stored record back in and resurrect what the
plan removed. The survivor's `read_later` and `timestamp` are restored after the merge
(`write_merge`'s comments say why), which is also what lets the converged-state check use
`diff`, which ignores `read_later` and `public`. **Resident and incumbent get identical
treatment on purpose**: whether the occupant falls inside the pass's slice is invisible to
the user, and must not decide whether their bookmark is protected. A target whose occupant
is planned to move *away* has no record that stays; that is safe only because the occupant's
own plan moves it first (`write_order` writes its group before this one), the fixpoint
refuses the group heading here if that write is refused, and the write loop marks the URL
occupied if it *fails*. Keep all three. A cycle of moves, such as a swap, has no such
order, so it stays unprotected: if a later write in the cycle fails, the first has already
overwritten a record that never moved.

**A merge never decides visibility.** A plan whose `public` doesn't match the record that
stays drops out of its group, since the merge would either publish a private note or unshare
a shared bookmark. With no record that stays, peers must agree or none writes: the
all()-rule would pick "private" with nothing to justify it, unsharing a shared bookmark and
then deleting it. **Only the mismatched plan is refused, not its group**: blocking the
record that stays would let one duplicate freeze an unrelated bookmark's cleanup for good.
The held-back plan's URL stays occupied, so a rewrite onto it is refused too.

**Refusals propagate, before any write and after a failed one.** A target the pass could not
read (a failed lookup, or one a halt stopped short of) is `untouchable`: nothing is written
there, since what is stored may be stale, and the rewrite counts in `PassOutcome::refused`.
A refused group keeps its members' URLs occupied, so `propagate_refusals` closes the set to
a fixpoint once before any write, making pre-write refusals order-independent, and again
after a write that did not land (`write_single`/`write_merge` return every member's stored
URL to mark), not after a half-applied one, which strands nothing. Skipping a propagation is
worse than not protecting at all: the members of a group refused without it are stranded in
turn. The `Refusal` and `propagate_refusals` docs give the rest.

**`Plan` says whether the source was reached, not just whether to write.** `plan` returns
`Plan::Bookmark` (an end-state), `Plan::Unchanged` (*the source answered*, nothing to do),
or `Plan::Skipped` (not this pass's bookmark — no lookup made). Only the first two count
toward `PassOutcome::reached`, as the last two mean opposite things about the source's
health. Without that split, the every-lookup-failed guard is worthless: `cleanup github`
skips deep links locally (and a deleted gist is `Unchanged`: the lookup answered), and if
those counted as successes a genuinely unreachable GitHub would look fine. HackerNews'
`search_by_url` finding nothing is `Unchanged`: the query ran. When a source runs several
passes, each reaches its **own** verdict rather than pooling tallies, so a pass that can't
fail can't vouch for one that can.

The per-source planners are each source's `cleanup.rs`: [reddit](src/reddit/cleanup.rs),
[github](src/github/cleanup.rs) and [hackernews](src/hackernews/cleanup.rs).

**`Bookmark` is service-agnostic; `PinboardBookmark` is the wire shape.** The domain
`Bookmark` and the `BookmarkStore` port live in [`src/bookmark.rs`](src/bookmark.rs) (its
module doc gives the split); [`src/pinboard.rs`](src/pinboard.rs) is just the client behind
that port, converting `posts/all` via `TryFrom` and mapping a write back to API params in
`post_add`. Because the `url` is a parsed [`Url`], a bookmark or source item whose URL
doesn't parse is skipped with a warning rather than aborting the run (`all()` and each
`fetch()` `filter_map` over the conversion), and consumers never re-parse it.

**The cleanup driver diffs a plan as the store will keep it.** `post_add` cuts a note too
long for its GET URL, so `write_single` and `write_merge` pass each end-state through
`BookmarkStore::fit` and skip it when it would store nothing that writing the stored record
back wouldn't (`stores_nothing_new`: both fitted, `diff` empty). That one test keeps a note
stored cut from being rewritten every run, and a whole note longer than a write allows from
being cut when the plan changes only text past the cut. A merge that absorbs a URL still
writes, and so still cuts such a note. `PinboardClient` implements `fit` with `fitted_note`,
which `post_add` uses too, so the two can't drift. It measures as if a `dt` were sent, so a
note `sync` wrote undated cuts the same once `cleanup` writes it back dated. A test models
the cut with `FakePinboard`'s `trims_notes`, which applies the real `fitted_note` to the
fit and to each write it records.

**Every source is a `Source`, and one sync loop drives them all.** The port lives in
[`src/source.rs`](src/source.rs): `Source::fetch()` returns `BookmarkDraft`s (a `Bookmark`
plus its `dedup_key`), and the `UrlKey::dedup_key()` supertrait method maps an existing
Pinboard URL to that source's key, so `sync::prepare_new_drafts` keeps the drafts not
already synced. **A key must survive cleanup's canonicalization**, or sync re-adds what
cleanup moved: each source keys through a `UrlKey` type in [`src/keys.rs`](src/keys.rs), and
`HackerNewsKeys` keys a Reddit or GitHub article through `RedditKeys` / `GitHubKeys`, so a
change to either, or to what `cleanup reddit` / `cleanup github` rewrite a URL to, has to
keep HN converging. To add a source, implement `Source`, add its account array to
`config.rs`, and implement [`src/jobs.rs`](src/jobs.rs)'s `SourceAccount`, the one trait
`sync`, `backup` and `doctor` reach every source through. Loops are unit-tested against
in-memory fakes ([`src/test_support.rs`](src/test_support.rs)), real clients by `net_tests`
against a wiremock server via test-only `with_base_url(s)` constructors.

**`sync` reads incrementally by default.** `SyncJob::fetch` picks the traversal from the
job's `config::SyncLimit`: `auto` calls `Source::fetch_incremental(existing)`, which every
source overrides to stop paging at the first page that would give `sync` nothing to write
(`source::page_is_caught_up`); `all` and a cap `N` call the full `fetch()`, since a capped
run writes only the newest drafts and an incremental read would never reach the rest.
**`prepare_new_drafts` returns drafts oldest first**, which is what keeps an interrupted
`auto` run recoverable (its doc says why). `backup`, `doctor` and `cleanup github`'s star
dates read every page (the star dates through `fetch_stars`, the stars stream alone). Known
gaps, all recovered by `--limit all`: an older item deleted from Pinboard is not re-added, a
failed write followed by a full page of newer successes is never retried, and a page whose
items all reached Pinboard another way stops the read early.

**Sources:** Reddit ([`reddit.rs`](src/reddit.rs) — cookie-authenticated
`saved.json`/`api/info.json`, plus the `PostInfo` port for cleanup;
[`model.rs`](src/reddit/model.rs) shapes bookmarks and tags), GitHub
([`github.rs`](src/github.rs) — `/user/starred`, `sort=created&direction=desc` pinned
because the incremental read depends on newest-starred-first, then unless `gists = false`
`/gists/starred` as a second stream, shaped in [`gists.rs`](src/github/gists.rs); both go
through `GitHubClient::walk`, and each stops at its own caught-up page), and HackerNews
([`hackernews.rs`](src/hackernews.rs) — scrapes `/favorites` for item IDs in
[`favorites.rs`](src/hackernews/favorites.rs), then batch-reads them from Algolia in
[`algolia.rs`](src/hackernews/algolia.rs)). **HackerNews' caught-up test checks drafts, not
item IDs** (`favorites::is_caught_up`): a favorited story whose article was saved by hand
dedups by URL and never gets an `HN Link:` note, so an ID test would page down to every such
item on every run. `cleanup hackernews --link-discussions` is the reverse, adding the HN
discussion to article bookmarks carrying the `link_tag` marker (default `find-hn`).

**The Reddit cookie is shaped in one place**, [`src/reddit/cookie.rs`](src/reddit/cookie.rs)
(its docs give the rules): `RedditClient::build` turns it into the `SessionCookie` every
entry point sends. `secrets`'s `resolve_reddit_cookie` skips a value that sets no session
(`is_configured_cookie`), as it does a blank one.

**`backup` has one port and one driver, and captures raw bodies in the same traversal.**
[`src/backup.rs`](src/backup.rs) is `cleanup_pass.rs`'s sibling (its module doc gives the
stages): a service implements `BackupSource::dump`, and the driver owns `layout`,
`write_files`, the manifest and the dry-run, so clients never learn where a file goes. Raw
bodies come from a `RawSink` threaded through each client's *existing* pagination:
`sync`/`cleanup` pass `RawSink::disabled()`, `backup` passes `collecting()`. Never re-walk
pagination for a raw pass — the two halves must describe one instant. `PinboardClient`
implements `BackupSource` too. `ExportBookmark` is deliberately *not* a `Serialize` on the
domain `Bookmark`: the snapshot format is a stable output contract, and a domain rename must
not silently change an operator's files.

**All tags are config-driven; there are no tag CLI flags.** Each source's tag config
(`RedditConfig`, `GitHubConfig`, `HackerNewsConfig`) has defaulted fields, rendered by
`push_tag`/`push_prefixed` in [`src/tags.rs`](src/tags.rs) (empty string disables a tag).
The tag tests in [`src/reddit/model/tests.rs`](src/reddit/model/tests.rs) are the spec —
keep them in sync with rule changes. Pinboard tags can't contain whitespace or commas, so
`push_prefixed` slugs them in the value (`lang:jupyter-notebook`) and `Config::parse`
rejects both in config-supplied tags/prefixes.

**Every configured `reddit_domain` counts as Reddit.** An account's domain is its URL host
for both sync `bookmark_url` and cleanup's `normalize_url`; `RedditKeys` stays host-agnostic
so dedup matches across subdomains, and takes the domain too, so a mirror outside reddit.com
still keys. Each Reddit client, and HackerNews keying an article on Reddit, match every
configured account's domain (`Config::reddit_domains`, which is why `SourceAccount::client`
takes the whole `Config`). `cleanup reddit` takes every account's config
(`RedditCleanupOpts::configured`): it leaves a bookmark on whichever configured domain it is
on, so passes run as different accounts never move it back and forth, shapes it with that
domain's account's tags, and collapses a post on several onto the one configured first.

**Config + secret resolution.** [`src/config.rs`](src/config.rs) parses the `--config` TOML:
one `[pinboard]` destination, `[hooks]`, and per-source account arrays
(`[[reddit]]`/`[[github]]`/`[[hackernews]]`). Secrets resolve through one ladder in
[`src/secrets.rs`](src/secrets.rs) (`SecretSpec::resolve`): CLI flag → `$VAR` → `$VAR_FILE`
(a path whose trimmed contents are the value) → config inline → config `*_file`; `--config`
itself resolves flag → `$PINBOARD_SYNC_CONFIG` only, since it is already a file. **The
`_FILE` form is load-bearing**: it's how the NixOS service feeds sops-nix secret *paths*
without putting values in the unit environment, so never add a `$VAR`-only secret.

**A message names the setting a secret came from, never the value.** Each secret is one
`SecretSpec`, whose `resolve` returns a `secrets::ConfiguredSecret` naming the setting for
every Reddit cookie warning, re-auth message and GitHub token error; a list of the settings
a secret can come from is built with `SecretSpec::settings`, never by hand. The env rungs
sit above every account's own value, so `jobs::warn_shared_env_secrets` warns once per
source when one would apply to several accounts, using the resolver's own acceptance check.

**`sync <source> [account]` / `cleanup <source> [account]` select one account** (by name,
else the first, else an implicit CLI/env account). `--all` runs every configured account,
exiting non-zero if any fail (`AllRun`); the two are a `jobs::Selection`, and clap refuses
an account name next to a source's `--all`. `cleanup --all` runs once per source, since it
normalizes the shared bookmark set. `cleanup github` (see its module doc) refreshes only
what the tool wrote: notes in `github_note`'s shape, the base/language/owner tags of
bookmarks carrying any configured account's base tag (with that account's tag config), their
title only while it is still `owner/repo`, and their star date only while the notes are the
tool's too. A repo root is often another source's bookmark (an HN story linking it, or one a
rename merged a star into), so never widen that. Gists follow the same rules in their own
pass ([`src/github/cleanup/gists.rs`](src/github/cleanup/gists.rs)), skipped when the
running account has gists off, keyed on carrying up to two marker tags of an account
(`markers`; the first `gist_tags` entry alone when every entry is also in `tags`): the
default `gist_tags` inherit `github-star`, so no single tag tells a synced gist from a
hand-saved one or one account's gist from another's. The pass plans from one read of
`/gists/starred` and looks up only the gists missing from it; a failed read fails the pass
before anything is planned, so it can't count every gist as reached.

**`sync` builds `SyncJob`s and fetches concurrently.** Each account becomes a
`SyncJob { client: SourceClient, hook, settings }` (`SourceClient` an enum over the three
clients), so `build_jobs` + `run_sync_jobs` handle one account and `--all` uniformly.
Per-account settings resolve CLI flag → account → `[defaults.<source>]` → `[pinboard]`
through `resolve_setting` and `resolve_dates`, each as a `settings::Resolved` carrying its
`Tier`, so a refusal (`ResolvedDates::check_redating`) names where a setting came from,
never its value. `run_sync_jobs` fetches every source concurrently via
`futures::future::join_all` (on one task — the client futures aren't `Send`, so no
`tokio::spawn`), then writes the merged, URL-deduped drafts **sequentially** through one
rate-limited writer (`sync::write_drafts`), reading `posts/all` once for all jobs.
`completions <shell>` and `config example` are utility subcommands the Nix package install
consumes; `config check <file>` runs `Config::parse` without resolving a secret.

**Re-auth vs other errors drive a side effect.** `Halt::Reauth` (Reddit 401/403 that isn't
the "blocked by network security" page, a Reddit redirect to `/login`, GitHub 401) fires the
`--on-auth-failure` hook and exits non-zero; `SourceError::Other` (transient/parse) doesn't.
HackerNews never re-auths. **`cleanup` fires it too**, for every re-auth-capable read:
GitHub's `repo()` mid-pass (via `Halt`), GitHub's `fetch_stars()` for star dates, and
Reddit's `/api/info`, whose expired cookie is by far the most common case. That is why the
three `cleanup` entry points return `Result<(), SourceError>` rather than `anyhow::Result`:
flattening the variant would lose the hook. [`src/hook.rs`](src/hook.rs)'s
`handle_source_err` is the single place the hook is fired; `cleanup` resolves the command
through `PINBOARD_SYNC_ON_AUTH_FAILURE` → account → `[defaults.<source>]` → `[hooks]` (it
has no `--on-auth-failure` flag). **Reading that env var is load-bearing**:
[`nix/module.nix`](nix/module.nix) exports `onAuthFailure` only into the unit environment,
never the generated TOML, and runs the `cleanup --all` timer with it, so resolving from
config alone would leave the hook dead where it matters most.

**Every client sends through its `http::RetryingClient`** ([`src/http.rs`](src/http.rs),
whose item docs give the mechanics), with its retry settings fixed where the client is
built. It backs off on network errors / 429 / 5xx and returns other statuses as-is. A
classifier returning an `http::Verdict` decides which responses retry: `send` uses
`backoff_if_transient`, while the HackerNews favorites pages pass
`retry_after_up_to(FAVORITES_MAX_RETRY_AFTER)` to `send_with`, because quick retries of a
429 there only prolong HN's block: it is retried at most once, after a `Retry-After` of at
most 120s, and a 429 that comes back becomes `Halt::RateLimited`. There is deliberately no
overall `timeout`, which would cap a slow but steady `posts/all`.

**A connect failure gets one long wait per host, then the host is given up.** A request that
could not connect (`is_connect`: DNS, TCP or TLS) retries on the exponential
`CONNECT_BACKOFF` (~2.5 minutes), separate from the linear budget, so a resolver restart
during a NixOS switch doesn't fail a timer-started run. Each client's `ConnectBudget` spends
that wait once per host, and gives a host up once it has gone the whole schedule without a
response; every later request to it then fails at once. Otherwise a mid-run outage costs
minutes per remaining item, hours for a big `cleanup`. The give-up is a plain
`SourceError::Other`, deliberately not a `Halt`: in `cleanup` each remaining lookup lands in
`plan_failed` at no cost and the majority rule decides the run, and no hook fires.

**Never put a request URL with a query into a message.** Pinboard's `auth_token` rides in
the query string, so `send` strips the URL from every transport error (`reqwest::Error`
prints it in `Display` and `Debug`), and Pinboard errors name the endpoint instead. An error
quotes a response body only through `http::error_with_body`, and a body worth seeing under
`-v` that no error quotes goes through `http::log_body_at_debug`; each caller passes its
client's secrets, which `http::redact` removes from the whole body before anything is cut or
logged. Pinboard response text in an error goes through `redacted_error`, which also covers
the token's other echoed forms. Tests read the log through `test_support::capturing_logs`.

**`Halt::RateLimited` is the third case, and deliberately not either of those.** GitHub
answers its rate limits with a **403 or 429**, the statuses a real permission denial uses,
so `github::rate_limit_message` matches on the headers instead. It must not become
`Halt::Reauth` (no credential change clears a quota, and the hook would send the operator
after the wrong problem) nor a permanent skip (`github::permanent_refusal`, which defers to
`rate_limit_message` first), and retrying won't help: `send`'s backoff totals ~12s, while a
primary limit resets up to an hour out. Stopping the pass is the response instead. The 429
form is still retried by `backoff_if_transient`, wasting those ~12s harmlessly, which is why
its unit test uses a synthesized `HeaderMap`, not a wiremock 429.

**`Halt::Blocked` is the fourth: Reddit's edge refusing the request by policy.** It is known
only by its body (its `retry-after: 0` must not be read as a rate limit), so `reddit_body`
checks a 403 with `reddit::edge_page`: "blocked by network security" is `Blocked`, and any
other 401/403 is `Halt::Reauth`, including the "whoa there, pardner" page, which Reddit also
serves for a junk cookie. A block arrives with a valid cookie (#78), so the hook would send
the operator to re-copy a credential that isn't the problem. It shares `RateLimited`'s
handling (no hook; a pass halts via `Halt::Blocked`) but keeps its own variant so the
message names a block, which no reset is known to lift. A 401/403 whose body is cut off
mid-read is `Other`, since without the whole page a block and a refused cookie look alike.

**The Reddit client never follows a redirect.** Reddit answers a request without a session
it accepts with a 302 to `/login`, and following it can land on a login page that answers
403, which the 401/403 arm would misreport as a 403 on the endpoint itself. So
`RedditClient` is built with `redirect::Policy::none()`, and `reddit_body` maps a 3xx to or
under `/login` to `Halt::Reauth` and any other to `Other`. Nothing needs to follow one:
`saved.json` pagination only changes query parameters, and `/api/info` is a single GET.

**Pinboard field names are inverted.** In `posts/add`, `description` is the *title* and
`extended` is the notes (delicious backcompat). Bookmarks are written `replace=yes`
(idempotent) and `shared=no` (private) by default.

## Gotchas

- **native-tls, not rustls.** Reddit's anti-bot edge 403s rustls's TLS fingerprint but
  accepts native-tls's, so `reqwest` is built with `native-tls`. On Linux that's OpenSSL via
  `openssl-sys`, so [`nix/package.nix`](nix/package.nix) carries `pkg-config` + `openssl`
  and the closure includes OpenSSL. Keep it this way.
- **`native-tls-alpn` is load-bearing too.** Reddit's edge also 403s a TLS 1.3 ClientHello
  with no ALPN extension, which plain `native-tls` sends
  ([#78](https://github.com/thanegill/pinboard-sync/issues/78)); header case and order are
  not the cause. With `http2` too, the ClientHello offers ALPN `h2, http/1.1`
  ([#91](https://github.com/thanegill/pinboard-sync/issues/91)). Build every new client from
  `http::client_builder()`. `client_hello_offers_alpn_h2_and_http1` and the `h2c_server`
  tests in `http::net_tests` guard the ClientHello and h2's retry path.
- **The Nix package build runs the tests** (`buildRustPackage` `doCheck`), so a failing test
  blocks `nix build`/`nix flake check`. The `net_tests` modules bind a socket, which the
  sandbox forbids — the package's `checkFlags = [ "--skip=net_tests" ]` excludes them there;
  they still run under `cargo test`. Keep all *other* tests hermetic (no network/sockets). A
  `net_tests` case that points a real client at a dead port sleeps the real ~2.5-minute
  `CONNECT_BACKOFF` unless it runs inside `http::recording_waits`.
- **Large test modules live in their own files**, `src/<module>/tests.rs` (or
  `net_tests.rs`) behind `#[cfg(test)] mod tests;`, so the module path — and the
  `--skip=net_tests` match — is the same as inline. `include_str!` paths in them are
  relative to the test file, not the module.
- **No paused clock under a real socket.** `start_paused` auto-advances to the next timer
  whenever the runtime idles, including on wiremock I/O, so any timer live during a request
  fires early, unless that firing *is* the expected outcome. To skip the clients' pauses,
  run the fetch inside `http::recording_waits`, which records each `http::sleep` instead, so
  a client's new pause must go through `http::sleep`.
- **Tests never read a secret from the real environment.** Under `cfg(test)`,
  `secret_ladder`'s `$VAR`/`$VAR_FILE` rungs read a per-thread fake environment, set with
  the `secrets::FakeSecretEnv` guard, so a test can't pick up the developer's real cookie or
  token. `std::env::set_var` on a secret does nothing in a test; use `FakeSecretEnv` or a
  synthetic ladder (`first_cookie_on`).
- **A test that touches the filesystem takes its directory from
  `test_support::scratch_dir`**, a `tempfile::TempDir` removed on drop, so keep the guard
  bound while the path is in use. A fixed path under `temp_dir()` collides with a concurrent
  `cargo test` (another worktree, the Nix build) and deletes its files.
- **`nix build`/`nix flake check` only see git-tracked files.** A new untracked module (or
  `config.example.toml`, used via `include_str!`) fails the sandbox build with
  `file not found`, even though dev-shell `cargo` passes — that discrepancy is the tell.
  `git add` new files before building with Nix. The package source is also narrowed to
  `Cargo.toml`, `Cargo.lock` and `src/` ([`nix/package.nix`](nix/package.nix)'s fileset), so
  a file the build reads from anywhere else has to be added there too.
- The User-Agent (`http::USER_AGENT`) derives from `CARGO_PKG_NAME`/`CARGO_PKG_VERSION`.
- **Service-specific, API-shaped structs carry a service prefix** (`GitHubRepo`,
  `HackerNewsItem`, `RedditSavedItem`, `AlgoliaSearchResponse`) — not bare `Repo`/`Item`.
  GitHub is spelled `GitHub` and HackerNews `HackerNews` in type names (e.g. `GitHubConfig`,
  `HackerNewsConfig`). The CLI enum variants (`SyncSource::Github`, `::Hackernews`) are the
  exception: clap derives the subcommand names from them.
