# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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

### Changed

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
  Internally, the stored Pinboard bookmark is now parsed into a service-agnostic domain
  type (`src/bookmark.rs`: tags split out, the time as a real `OffsetDateTime`, and
  `public`/`read_later` flags), separate from the `PinboardBookmark` wire shape; each
  source plans a `cleanup` end-state in that same domain type.

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
