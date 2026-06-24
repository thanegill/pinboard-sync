# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/thanegill/pinboard-sync/compare/v0.3.1...HEAD
[0.3.1]: https://github.com/thanegill/pinboard-sync/releases/tag/v0.3.1
[0.3.0]: https://github.com/thanegill/pinboard-sync/releases/tag/v0.3.0
[0.2.1]: https://github.com/thanegill/pinboard-sync/releases/tag/v0.2.1
[0.2.0]: https://github.com/thanegill/pinboard-sync/releases/tag/v0.2.0
[0.1.0]: https://github.com/thanegill/pinboard-sync/releases/tag/v0.1.0
