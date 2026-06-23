# pinboard-sync

A small Rust CLI that syncs the things you save across the web into a single
[Pinboard](https://pinboard.in) account, and tidies up the bookmarks once they're
there. It currently knows three sources:

- **Reddit** — your saved posts and comments.
- **GitHub** — your starred repositories.
- **HackerNews** — your favorited stories and comments.

Each run is a single pass: read what you've saved from a source, skip anything
already on Pinboard, and write the rest. Pinboard *is* the state — there's no local
database — so runs are idempotent and safe to schedule.

## How bookmarks look

Every source maps an item to a Pinboard bookmark (`url`, title, notes, tags) with
sensible, fully-configurable tags:

| Source | URL | Title | Tags (defaults) |
| --- | --- | --- | --- |
| Reddit | the `old.reddit.com` permalink | post/comment title | `reddit`, `subreddit:<sub>`, `reddit-comment`, `nsfw`, `author:reddit:<u>`, `reddit-flair:<f>`, `type:image`/`type:video` |
| GitHub | the repo URL | `owner/repo` | `github-star`, `lang:<language>` |
| HackerNews | the article (or HN permalink) | story title | `hackernews`, `hackernews-comment`, `author:hackernews:<u>`, `hackernews:show-hn` (and `ask-hn`/`tell-hn`/`launch-hn`) |

A favorited HN *story* is bookmarked as the linked article with the discussion in
the notes; a favorited *comment* keeps the HN permalink. Dedup is by URL, so the
same item saved under any subdomain (or already bookmarked from elsewhere) is left
alone. `--limit` is an optional per-run write cap, not correctness — Reddit also
caps any listing at ~1000 items.

## Install

With Nix (provides the binary plus shell completions and an example config):

```sh
nix build            # ./result/bin/pinboard-sync
nix run . -- --help
```

Or build with Cargo (needs OpenSSL + pkg-config on Linux — the HTTP client uses
native-tls; see [Why native-tls](#why-native-tls)):

```sh
cargo build --release
```

## Authentication

The Pinboard API token (`username:TOKEN`, from your
[settings](https://pinboard.in/settings/password)) is shared by every source.

Per source:

- **Reddit** needs your **username** (non-secret — whose saves to read) and a
  **`reddit_session` cookie**. Reddit ended self-serve API access, and its anti-bot
  edge blocks cookieless requests, so the tool reads
  `old.reddit.com/user/<you>/saved.json` with a logged-in cookie. Copy
  `reddit_session` from your browser's DevTools (Application → Cookies →
  reddit.com). It lasts about a year; when it expires the tool exits non-zero and
  runs the optional auth-failure hook so you can re-copy it.
- **GitHub** needs a **personal access token** (`public_repo`/`read` scope is
  plenty — it only reads `/user/starred`).
- **HackerNews** needs only your **username**; favorites are public.

### Environment variables

Each value resolves through a ladder, highest precedence first: a **CLI flag**, the
**environment variable**, that variable's **`<VAR>_FILE`** form, then the inline value
and `*_file` path in the **`--config` file** (below). A blank or missing rung falls
through to the next; the first non-empty value wins.

`<VAR>_FILE` names a **path whose trimmed contents are the value** — so the secret
itself never sits in the environment or the Nix store, only a path to it does. Every
variable in the table accepts this `_FILE` form. For any variable, the inline value and
the `_FILE` path are equivalent:

```sh
# value inline in the environment…
export PINBOARD_TOKEN='user:abcdef0123456789'
# …or the value read (and trimmed) from a file — how the NixOS service feeds sops-nix:
export PINBOARD_TOKEN_FILE=/run/secrets/pinboard-token

# the same applies to every variable below
export REDDIT_USERNAME=alice
export REDDIT_COOKIE_FILE=/run/secrets/reddit-cookie
export GITHUB_TOKEN_FILE=/run/secrets/github-token
export HN_USERNAME_FILE=/run/secrets/hn-username
```

| Variable | Sets | Secret |
| --- | --- | --- |
| `PINBOARD_TOKEN` | Pinboard API token (`user:TOKEN`), shared by every source | yes |
| `REDDIT_USERNAME` | Reddit user whose saved items to read | no |
| `REDDIT_COOKIE` | Reddit `reddit_session=…` cookie | yes |
| `GITHUB_TOKEN` | GitHub personal access token | yes |
| `HN_USERNAME` | HackerNews user whose favorites to read | no |

Two variables are **direct values, not `_FILE`-capable**:

- **`PINBOARD_SYNC_CONFIG`** — a direct path to the `--config` TOML file (equivalent to
  passing `--config`). The config is already a file, so it takes no `_FILE` indirection.
- **`PINBOARD_SYNC_ON_AUTH_FAILURE`** — a shell command run when a source needs
  re-authentication. It can also be set per-account or as `[hooks] on_auth_failure` in
  the config (flag/env → per-account → `[hooks]`).

## Usage

```sh
# Reddit (flags or env: REDDIT_USERNAME, REDDIT_COOKIE, PINBOARD_TOKEN)
pinboard-sync sync reddit --reddit-username you --reddit-cookie 'reddit_session=…'

# GitHub (GITHUB_TOKEN)
pinboard-sync sync github

# HackerNews (HN_USERNAME)
pinboard-sync sync hackernews --username you

# Preview without writing
pinboard-sync sync reddit --dry-run

# Normalize existing bookmarks
pinboard-sync cleanup reddit          # URLs → configured domain, tags, NSFW, titles
pinboard-sync cleanup github          # canonicalize repo URLs + refresh renamed repos
pinboard-sync cleanup hackernews      # rewrite HN item URLs to the linked article
```

`cleanup reddit` only contacts Reddit when marking NSFW or fixing placeholder titles
(`--no-nsfw --no-titles` skip both, and the cookie). Run
`pinboard-sync completions <bash|zsh|fish>` to print a completion script, and
`pinboard-sync config example` to print a fully-commented config template.

## Config file and multiple accounts

For multiple accounts of a source, or to keep settings out of flags, use a TOML
config passed via `--config <path>` (or `$PINBOARD_SYNC_CONFIG`). It holds
one `[pinboard]` destination, an optional `[hooks]` block, and arrays of accounts
per source. Print the annotated template with `pinboard-sync config example`; in
brief:

```toml
[pinboard]
token_file = "/run/secrets/pinboard-token"

[[reddit]]
name = "main"
username = "you"
cookie_file = "/run/secrets/reddit-cookie"
# tag_* keys (config-only) customize the tags; `tags` defaults to ["reddit"].

[[github]]
name = "personal"
token_file = "/run/secrets/github-token"

[[hackernews]]
username = "you"
```

Then select an account by name, or run them all:

```sh
pinboard-sync --config ./config.toml sync github personal   # one named account
pinboard-sync --config ./config.toml sync github --all      # every github account
pinboard-sync --config ./config.toml sync --all             # every account, every source
pinboard-sync --config ./config.toml cleanup --all          # reddit + github + hackernews
```

The selector is an account's `name`, falling back to its `username` for Reddit and
HackerNews — so an account with `username = "alice"` and no `name` is reachable as
`sync reddit alice`. GitHub has no username, so its accounts are selected by `name`
only. With no selector, a command uses the first account of that source. All tag
settings (prefixes, the base `tags` list, the Reddit media-type allowlist, the HN
special-type prefix) live in the config only — there are no tag CLI flags.

## Running as a NixOS service

The flake exports the module as `nixosModules.pinboard-sync` (with
`nixosModules.default` as an alias). It renders the non-secret config to the Nix
store and reads secrets from a systemd `environmentFile` (e.g. a sops-nix rendered
template, read as root — never in the store), running on a timer under a hardened
`DynamicUser`:

```nix
{
  imports = [ inputs.pinboard-sync.nixosModules.default ];

  # A sops-nix template that renders every credential into a root-only env file.
  sops.templates."pinboard-sync.env".content = ''
    PINBOARD_TOKEN=${config.sops.placeholder.pinboard-token}
    REDDIT_USERNAME=${config.sops.placeholder.reddit-username}
    REDDIT_COOKIE=${config.sops.placeholder.reddit-cookie}
  '';

  services.pinboard-sync = {
    enable = true;
    # Each account runs by default; set `enable = false` to keep it configured but
    # skip it (the flag is stripped before the config is rendered).
    settings.reddit = [
      { name = "main"; }
      { enable = false; name = "alt"; }        # configured but not synced
    ];
    environmentFile = config.sops.templates."pinboard-sync.env".path;
    # sync.schedule = "*:0/30";               # sync timer; default every 30 minutes
    cleanup.enable = true;                   # also normalize existing bookmarks…
    # cleanup.schedule = "weekly";           # …on its own timer; default weekly
  };
}
```

## Roadmap

Planned but not yet implemented:

- **Tag bundles.** Setting and updating Pinboard tag bundles. This depends on the
  Pinboard **API v2** — the v1 API has no bundle support, and v2 (which does) is a
  documented 2021 draft that hasn't been deployed, so this is on hold until v2
  ships.

## Why native-tls

Reddit's anti-bot edge rejects rustls's TLS ClientHello fingerprint but accepts
native-tls's, so the HTTP client is built with native-tls. On Linux that pulls in
OpenSSL (hence pkg-config + openssl at build time); on macOS it uses the Security
framework.

## Development

Run everything through the Nix dev shell so versions match the flake:

```sh
nix develop --command cargo test
nix develop --command cargo clippy --all-targets -- -D warnings
nix develop --command cargo fmt --check
nix flake check        # builds the package (runs the hermetic tests) + evaluates the module
```

See [CLAUDE.md](CLAUDE.md) for the architecture.
