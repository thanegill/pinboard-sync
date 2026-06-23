# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/thanegill/pinboard-sync/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/thanegill/pinboard-sync/releases/tag/v0.2.0
