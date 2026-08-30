# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- A `backup` subcommand that snapshots **every service** — the Pinboard account and each
  configured Reddit/GitHub/HackerNews account — into one directory: `raw/` holds each
  API response with every field intact (captured as text before parsing, so nothing the typed read path
  drops is lost — Pinboard keeps `meta`/`hash`, GitHub keeps the ~75 repo fields `sync`
  ignores, HackerNews keeps Algolia's `points`/`num_comments` plus the scraped favorites
  HTML), `normalized/` holds the same items in one uniform bookmark shape across all four,
  and `manifest.json` records the run — carrying `complete: false` and the failed targets
  when a target failed, since its files are then left over from an earlier run. Both halves
  come from a single traversal, so they always describe the same instant; Pinboard's
  `posts/all` is fetched **once** and feeds both, because it is rate-limited to one call
  per five minutes.

  `backup <target> [account]` narrows a run and `--all` covers everything; the directory
  comes from `--out DIR` or `[backup].directory`. Backing up a source never contacts
  Pinboard, so those targets take no Pinboard token. Each run replaces the previous
  snapshot in place (no pruning, no run directories — point a real backup tool at it for
  history); every file is written atomically at mode 0600, and a body that isn't a JSON
  array is refused rather than allowed to overwrite a good snapshot. The NixOS module
  gains a `backup` timer (`services.pinboard-sync.backup.{enable,schedule,directory}`)
  running it under the hardened service, writing into the service's `StateDirectory` by
  default.
- `doctor` now checks that a configured `[backup].directory` is writable, probing by
  creating and removing a file rather than reading permission bits (which get
  `DynamicUser`, ACLs and read-only mounts wrong). A misconfigured `StateDirectory` shows
  up here instead of as a quiet failure in the journal at the next timer firing.
- NixOS module: per-credential `*File` options — `pinboardTokenFile`,
  `redditUsernameFile`, `redditCookieFile`, `githubTokenFile`, `hnUsernameFile`. Each
  points at a single secret path (e.g. one sops-nix secret per credential), loaded into
  the unit via systemd `LoadCredential` and read through the binary's `<VAR>_FILE` env
  var — so consumers no longer have to render a combined `environmentFile` template or
  hand-roll a `serviceConfig` override. `environmentFile` is now optional: set it,
  at least one `*File` option, or account-table `*_file` paths (the assertion still
  fires when a source has no credentials configured at all).
- `use_post_date`: date bookmarks by the **source post date** (Pinboard `dt`) instead
  of "now", for both `sync` and `cleanup`. Reddit uses the post's `created_utc`, HN the
  item's `created_at`, GitHub the **star date** (`starred_at`, via the `star+json`
  `/user/starred` response; `cleanup github` fetches the star list to map it, since the
  per-repo lookup omits it). An age cap `post_date_max_age_days` (default 30) bounds
  backdating: older posts use "now" on `sync` and keep their existing date on `cleanup`
  unless `cleanup_stale_to_now` is set. `cleanup reddit` now needs the cookie when
  `use_post_date` is on (the date comes from `/api/info`).
- A per-source default tier via a `[defaults.<source>]` config table: `use_post_date`,
  `toread`, `public`, `limit`, `on_auth_failure`, `post_date_max_age_days`, and
  `cleanup_stale_to_now` now resolve **CLI → account → `[defaults.<source>]` → global**.
  A new global `[pinboard].limit` is the bottom tier for the write cap.
- CLI flags for the per-run setting tier, each topping the ladder
  (**flag → account → `[defaults.<source>]` → `[pinboard]` global → built-in default**):
  `--limit`, `--max-age-days`, `--toread[=BOOL]`, `--public[=BOOL]`,
  `--use-post-date[=BOOL]` on `sync`, and `--max-age-days`, `--use-post-date[=BOOL]`,
  `--stale-to-now[=BOOL]` on `cleanup` (per source and `cleanup --all`). No environment
  variables — these are CLI- and config-only.

### Fixed

- **`cleanup hackernews` no longer destroys a separately saved article bookmark.** A
  favorited HN *story* is rewritten to the article it links to — but if that article was
  also bookmarked on its own, the rewrite landed on it with `replace=yes` and replaced the
  user's title, notes and tags with the generated ones. The guard that prevents exactly
  this for other collisions only knew about bookmarks in the pass's own slice, and an
  article URL is not an HN item URL, so it never saw them.
  `cleanup` now checks the target against **every** bookmark in the account. Since
  Pinboard holds one record per URL, a bookmark already sitting at the target joins the
  same field-merge that colliding rewrites already use: tags are unioned, the resident's
  note is kept **byte for byte** and extended only with what the incoming bookmarks add,
  and its title, date and to-read state survive — it is the record that stays at that URL.
  (Its title survives if it has one, and its date if that date parsed; an absorbed
  bookmark's `to-read` flag is discarded rather than OR'd in, so a merge can never set
  to-read on a record the user had already cleared — that last one only when a record
  *is* staying; bookmarks colliding onto a fresh URL still OR it.) So the article keeps the user's title, notes and tags, gains the generated
  `HN Link:` line and HN tags, and the now-redundant HN item bookmark is absorbed —
  one record, nothing lost, and the pass converges. Nothing changes when the article is
  not separately bookmarked: the usual rewrite still happens.

  The same protection covers a collision between two bookmarks the pass *did* plan — a
  renamed GitHub repo starred under both its old and new names, say. The one already
  stored at the target is the record that stays there, so it leads the merge and keeps its
  own date and to-read state, exactly as an unplanned bookmark at that URL does. It
  previously kept neither: the surviving record was silently backdated to the absorbed
  bookmark's date even with dating off, and picked up its to-read flag.

  Two cases are **refused** rather than merged, leaving both records exactly as they are
  and reporting the rewrite as left in place (now shown in `--dry-run` too, not only
  logged). A target whose record this pass *could not read* — its lookup failed, or a dead
  credential stopped the pass before reaching it — is left strictly alone, since what is
  stored there may be stale. And a bookmark whose public/private state differs from the
  record at the URL it is moving to is left where it is, in either direction: merging fuses
  their notes into one record, so it would have to either publish a private annotation or
  unshare a bookmark the user chose to share, and neither is this tool's call to make.
  Only the disagreeing bookmark is held back — the record it was heading for still gets its
  own cleanup, and any other bookmarks merging in still merge. When *nothing* is stored at
  the target there is no record to defer to, so the bookmarks moving there must agree with
  each other instead, and if they don't, none of them is written. Previously a public
  bookmark colliding with a private one onto a fresh URL was merged into a private record
  and then deleted — silently unshared, with the run reporting success. A refusal protects the held-
  back bookmark's own URL as well, so a second rewrite heading there can't overwrite the
  record the refusal just preserved. The same now applies when a rewrite *fails*: the
  record stranded at its old URL is marked occupied and the refusal is re-propagated, where
  before it was left open to being overwritten by a later rewrite — a silent loss of a
  bookmark that had merely failed to move. That protection only covers rewrites not yet
  written when the failure happens, which is why a failed write is still reported as a
  failure and not as a refusal.

  `cleanup --all` runs the three sources over one account in turn, and each of them
  writes. All three now share a **live** view of the account rather than one snapshot
  taken before the first ran, so a later source sees what an earlier one wrote instead of
  planning against state that no longer exists. `--dry-run` advances that view too — the
  preview shows the same set of changes the real run would make.
- **`cleanup` now fires the `--on-auth-failure` hook**, which previously only `sync` did.
  An expired credential during `cleanup` exited non-zero but ran no hook, so anyone using
  it to be told "re-copy your reddit_session" was silently not told when the expiry
  happened on the cleanup timer rather than the sync one. All three re-auth-capable reads
  are covered — Reddit's `/api/info` (the most common, since `reddit_session` cookies
  expire far more often than GitHub tokens), GitHub's per-repo lookup, and GitHub's star
  list under `use_post_date`. A rate limit still does **not** fire it: waiting, not a new
  credential, is what clears a quota. The hook resolves
  `PINBOARD_SYNC_ON_AUTH_FAILURE` → account → `[defaults.<source>]` → `[hooks]` — the
  same tiers `sync` uses, less its `--on-auth-failure` flag, which `cleanup` does not
  take. The env var matters most: the NixOS module passes `onAuthFailure` to the unit
  that way and never writes it into the generated config. Note that `cleanup --all` keeps
  going after a failure, so two dead credentials mean the hook runs twice — once per
  source, each with its own `PINBOARD_SYNC_AUTH_ERROR`. `sync --all` has always behaved
  this way; a hook that notifies should expect to be called more than once per run.

### Changed

- **A GitHub rate limit is now recognised as one, and stops the pass instead of being
  spent one bookmark at a time.** GitHub answers both its primary and secondary rate
  limits with a `403` or `429` — the same statuses a permission denial uses — so this
  previously surfaced as `github repo returned 403 Forbidden: {…}` once per bookmark,
  reading like a token problem. It is now identified by its headers
  (`x-ratelimit-remaining: 0`, or a `retry-after`) and reported with the reset instant:
  `GitHub rate limit exhausted; it resets at 2026-08-30T15:00:00Z`. Since every remaining
  lookup would fail identically, `cleanup` stops there rather than spending a doomed
  request per bookmark — still writing the bookmarks it had already planned, and still
  exiting non-zero. No `--on-auth-failure` hook fires: waiting, not a new credential, is
  what clears a quota.
- **`cleanup` no longer fails the whole run because one bookmark could not be looked
  up.** A per-item lookup failure is logged and skipped, every other bookmark is still
  cleaned up, and the run exits zero. Previously any single failure made the run exit
  non-zero, so one permanently dead URL (a repo blocked under DMCA answers `451` forever)
  left a scheduled `pinboard-sync-cleanup.service` failed on every run, with no way to
  clear it. The run still exits non-zero when it genuinely could not sync: a bookmark that
  failed to **write** to Pinboard; an expired credential, which now stops the pass instead
  of burning one doomed request per bookmark, while still writing the plans it had already
  made; or lookups failing in the **majority**, which means an outage or a rate limit
  rather than one bad link. A single failed lookup never fails the run whatever the ratio,
  and bookmarks the source was never asked about count on neither side of it — so passes
  that skip most of what they are handed (`cleanup github` ignores deep links and gists)
  can't pad it into looking healthy.
- `cleanup github` treats a repo **blocked under the DMCA** (`451`) the same as a deleted
  one: the bookmark keeps its URL canonicalization and the block is reported as a warning
  rather than as a lookup error.
- A malformed response body now reports where it failed. The GitHub starred, HackerNews
  Algolia, and Pinboard `posts/all` reads decode the body text explicitly instead of via
  `reqwest`'s `json()`, so a proxy or interstitial page returned with a 200 surfaces a
  line and column rather than an opaque decode error.
- `--public` is now a value-taking flag: `--public` (= `true`) or `--public=false`.
  Previously it was a bare force-on switch that could not override a config-set `true`
  back to `false`; the new form can. `--limit` is likewise now an optional value (unset,
  rather than the old `0` sentinel, means "no per-run override").
- Bookmark **titles and notes are cleaned of raw HTML** before reaching Pinboard. Titles
  run through an HTML-strip + entity-decode pass for all three sources (so `&#x27;`/`&amp;`
  decode and stray markup is removed). Note bodies are wrapped in a literal `<blockquote>`,
  which Pinboard renders: HackerNews' raw Algolia HTML is converted to Markdown first,
  while Reddit text (already Markdown via `raw_json=1`) and GitHub repo descriptions are
  wrapped as-is. `cleanup` retrofits this shape onto existing bookmarks for every source —
  rebuilding titles and notes through the shared builders — since `cleanup` is the only
  path that reshapes already-saved bookmarks. Reddit *comment* bookmarks are left untouched
  (their bodies aren't refetched, so the parent post's text can't be misapplied).
- `cleanup --dry-run` output is now uniform across all three sources: the changed-field
  lines (`url`/`title`/`notes`/`tags`/`date`) print in one consistent order, and
  HackerNews now lists changed notes too. Internally the three per-source `cleanup` loops
  were unified behind a single driver ([`src/cleanup_pass.rs`](src/cleanup_pass.rs)) — no
  change to what gets written.
- `cleanup` compares a bookmark's date by **instant** rather than by its formatted string,
  so it no longer issues a redundant re-write when a stored date and the source date are
  the same moment written differently (e.g. a `+00:00` offset vs a trailing `Z`).
- A saved bookmark or fetched item whose **URL doesn't parse** is now skipped with a
  warning, so one malformed entry can't derail a `sync` or `cleanup` run.

### Fixed

- Text posts no longer carry a **duplicated link in their notes**. A HackerNews text post
  (Ask HN, etc.) and a bodyless Reddit self-post both bookmark their own permalink, yet
  `sync` repeated that same URL in the notes (`HN Link: …` / the post's `url`). `sync` now
  omits it, and `cleanup` removes it from existing bookmarks (HackerNews by reshaping;
  Reddit self-posts whose notes are just a link back to their own permalink). Link posts
  keep their external URL.

## [0.4.0] - 2026-06-24

### Added

- Diagnostic logging via `log`/`env_logger`: the version on startup, configured
  account counts, the existing-bookmark count, per-source fetched/new counts, and a
  per-run summary. Logs go to stderr (stdout stays clean for generated output);
  `--verbose`/`-v` is now repeatable (`-v` debug, `-vv` trace, `-vvv` includes
  dependency logs) and `RUST_LOG` overrides the level filter.

### Fixed

- `414 URI Too Long` could still occur on long Reddit self-posts: the previous
  truncation budget (7000 bytes) was above Pinboard's actual, undocumented request-URL
  limit. The starting budget is now lower (4000 bytes) and, crucially, `posts/add`
  retries with a halved budget whenever Pinboard answers 414 — so it self-calibrates to
  the real limit instead of relying on a guessed constant.

## [0.3.3] - 2026-06-24

### Changed

- `sync` no longer aborts the whole run when a single bookmark fails to write (e.g. a
  URL Pinboard rejects): the failure is logged and skipped, the rest are still added,
  and the run exits non-zero if any failed. Source/account fetch failures already
  behaved this way.
- `cleanup` (all sources) is likewise resilient: a single bookmark that fails to look
  up or update is logged and skipped so the rest of the pass still runs, with a
  non-zero exit if any failed.

## [0.3.2] - 2026-06-24

### Changed

- Release automation now points the rolling `latest` tag and the matching `vX.Y.Z`
  tag at the same stamped release commit, so consumers tracking `latest` resolve to a
  tagged version.

## [0.3.1] - 2026-06-24

### Fixed

- `posts/add` no longer fails with `414 URI Too Long` on long Reddit self-posts: since
  the Pinboard API is GET-only, the `extended` (notes) field is now trimmed to keep the
  request URL within a safe byte budget, appending a truncation marker.

## [0.3.0] - 2026-06-24

### Added

- A `man` subcommand that prints a roff man page; the Nix package installs it.
- `[pinboard].rate_limit_secs` to tune the pause between Pinboard writes (default 3).
- A `toread` option to mark new bookmarks unread — `[pinboard].toread` as the default,
  overridable per account.
- A `doctor` subcommand that checks the Pinboard token and every configured account's
  credentials, exiting non-zero if any fail.

### Changed

- `[pinboard].public` is now overridable per account (like `toread`); the resolved
  value travels on each bookmark rather than the shared client.

## [0.2.1] - 2026-06-23

### Added

- Named NixOS module output `nixosModules.pinboard-sync`; `nixosModules.default` now
  aliases it.

### Changed

- Account selection falls back to an account's `username` when its `name` is unset,
  for Reddit and HackerNews (an explicit `name` still wins). GitHub has no username
  and is unchanged.
- The NixOS service groups its timers under `sync` and `cleanup`, each with `enable`
  and `schedule`. `sync.enable` defaults on and `sync.schedule` to every 30 minutes
  (`*:0/30`); `cleanup.enable` is opt-in and `cleanup.schedule` defaults to `weekly`.
- The NixOS service replaces the `mode`/`source`/`account` options with a per-account
  `enable` flag (default `true`) in `settings`: a disabled account stays in the
  declarative config but is pruned from the rendered TOML, so `sync --all` /
  `cleanup --all` skip it.

### Removed

- The `~/.pinboardrc` fallback for the Pinboard token. Supply the token via
  `--pinboard-token`, `PINBOARD_TOKEN` / `PINBOARD_TOKEN_FILE`, or `[pinboard]` in the
  config instead.
- The `PINBOARD_SYNC_CONFIG_FILE` indirection. The config is resolved from `--config`
  or `PINBOARD_SYNC_CONFIG` as a direct file path only.

### Fixed

- `--dry-run` / `--verbose` placed before the source subcommand (e.g.
  `sync --dry-run reddit`) are now honored instead of silently performing a real run.
- The NixOS timers now set `Persistent`, so a sync or cleanup run missed while the
  machine was off fires on next boot.

## [0.2.0] - 2026-06-23

A multi-source Pinboard sync with per-source cleanup.

### Added

- **Three sources**, each behind a generic `Source` port: **Reddit** (saved
  posts/comments via a `reddit_session` cookie + username), **GitHub** (starred
  repositories), and **HackerNews** (favorited stories/comments by username).
- **`sync <source> [account]`** and **`cleanup <source> [account]`**, selecting one
  account by name (or the first/only one). **`--all`** runs every account of a source,
  or every account of every source at the top level.
- **Concurrent fetches** across sources, with merged, URL-deduped, rate-limited
  sequential writes through a single shared Pinboard client; `posts/all` is fetched
  once per run.
- **TOML config** (`--config`, `$PINBOARD_SYNC_CONFIG`/`_FILE`): one `[pinboard]`
  destination, `[hooks]`, and per-source account arrays. All tags are config-driven
  (`tag_*` keys with built-in defaults); there are no tag CLI flags.
- **Secret resolution ladder**: CLI flag → `$VAR` → `$VAR_FILE` → config inline →
  config `*_file`, with the Pinboard token also falling back to `~/.pinboardrc`.
- **`cleanup github`**: canonicalizes repo URLs and refreshes renamed/moved repos,
  titles, and the language tag via the API.
- **`cleanup hackernews`**: rewrites HN item URLs to the linked article; optional
  `--link-discussions` adds the HN discussion link to article bookmarks carrying the
  configurable marker tag (`--link-tag`, default `find-hn`).
- **HackerNews batching** via the Algolia search API, so hundreds of favorites cost a
  couple of queries instead of one per item.
- **`completions <bash|zsh|fish>`** and **`config example`** utility subcommands,
  installed as artifacts by the Nix package.
- **NixOS module** (`nixosModules.default`): renders the non-secret config to the
  store, reads secrets from a systemd `environmentFile` (sops-nix), runs on a timer
  under a hardened `DynamicUser`.
- Input validation for config-supplied tags/prefixes (no whitespace), account-name
  uniqueness, and `reddit_domain` shape.

### Notes

- Tag bundles remain unimplemented pending the Pinboard API v2 (see the README
  Roadmap); the tool runs entirely on the live v1 API.

## [0.1.0] - 2026-06-22

The original Reddit-only release: sync your saved Reddit posts and comments to
Pinboard, with a cleanup pass over existing bookmarks. Superseded by 0.2.0, which
generalized the tool to multiple sources.

### Added

- **Reddit → Pinboard sync** of saved posts and comments, read from
  `old.reddit.com/user/<you>/saved.json` and `api/info.json` and authenticated by a
  `reddit_session` cookie + username (no OAuth).
- **Dedup against Pinboard** rather than relying on `--limit`, so runs are idempotent.
- **`cleanup` subcommand** that normalizes existing Reddit bookmarks (URLs, tags,
  titles).
- **Rich tagging**: a base `reddit` tag, `subreddit:<sub>` (lowercased except
  multi-word camelCase), `reddit-comment`, `nsfw`, and author / flair / media-type
  tags, plus comment thread links in the notes.
- **HTTP retry** with backoff for transient Reddit and Pinboard failures.
- **Ports/fakes architecture** with hermetic unit tests and wiremock integration
  tests for the Reddit and Pinboard clients.
- **NixOS module** with an `environmentFile` for secrets, and a secret-resolution
  ladder (CLI flag → `$VAR` → `$VAR_FILE` → `~/.pinboardrc`).

[Unreleased]: https://github.com/thanegill/pinboard-sync/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/thanegill/pinboard-sync/releases/tag/v0.4.0
[0.3.3]: https://github.com/thanegill/pinboard-sync/releases/tag/v0.3.3
[0.3.2]: https://github.com/thanegill/pinboard-sync/releases/tag/v0.3.2
[0.3.1]: https://github.com/thanegill/pinboard-sync/releases/tag/v0.3.1
[0.3.0]: https://github.com/thanegill/pinboard-sync/releases/tag/v0.3.0
[0.2.1]: https://github.com/thanegill/pinboard-sync/releases/tag/v0.2.1
[0.2.0]: https://github.com/thanegill/pinboard-sync/releases/tag/v0.2.0
[0.1.0]: https://github.com/thanegill/pinboard-sync/releases/tag/v0.1.0
