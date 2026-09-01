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
native-tls, explained on the `reqwest` dependency in `Cargo.toml`):

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
  re-authentication, on both `sync` and `cleanup`. It can also be set per-account, as
  `[defaults.<source>] on_auth_failure`, or as `[hooks] on_auth_failure`, resolving
  **flag/env → account → `[defaults.<source>]` → `[hooks]`**. (`cleanup` takes no
  `--on-auth-failure` flag, so the env var is its top rung.) It does not run for a rate
  limit — waiting, not a new credential, is what clears one.

## Usage

**Every command needs your Pinboard token** (the destination), via `--pinboard-token`,
`$PINBOARD_TOKEN`, or `[pinboard]` in the config — the examples below assume it's in
the environment. Each source then adds its own auth on top.

```sh
# Reddit (PINBOARD_TOKEN + REDDIT_USERNAME, REDDIT_COOKIE)
pinboard-sync sync reddit --reddit-username you --reddit-cookie 'reddit_session=…'

# GitHub (PINBOARD_TOKEN + GITHUB_TOKEN)
pinboard-sync sync github

# HackerNews (PINBOARD_TOKEN + HN_USERNAME; favorites are public)
pinboard-sync sync hackernews --username you

# Preview without writing
pinboard-sync sync reddit --dry-run

# Normalize existing bookmarks
pinboard-sync cleanup reddit          # URLs → configured domain, tags, NSFW, titles
pinboard-sync cleanup github          # canonicalize repo URLs + refresh renamed repos
pinboard-sync cleanup hackernews      # rewrite HN item URLs to the linked article

# Validate the Pinboard token + every configured account's credentials
pinboard-sync doctor

# Snapshot every service to a directory (full API responses + normalized items)
pinboard-sync backup --all --out ~/snapshots/pinboard-sync
pinboard-sync backup pinboard --out DIR    # just the Pinboard account
pinboard-sync backup reddit main --out DIR # just one source account
```

`cleanup reddit` only contacts Reddit when marking NSFW or fixing placeholder titles
(`--no-nsfw --no-titles` skip both, and the cookie). Two utility subcommands —
`completions` and `config example` — are covered in
[Shell completions and example config](#shell-completions-and-example-config).

### Per-run setting flags

These per-account settings can also be set on the command line, where the flag tops
the resolution tier (**flag → account → `[defaults.<source>]` → `[pinboard]` global →
built-in default**). They are CLI- and config-only — there are no environment
variables for them.

| Flag | Commands | Setting |
| --- | --- | --- |
| `--limit <N>` | `sync` | Cap on new bookmarks written this run (0 / unset = no cap). |
| `--max-age-days <N>` | `sync`, `cleanup` | Only backdate posts newer than N days; older use "now". |
| `--toread[=BOOL]` | `sync` | Mark new bookmarks to-read. |
| `--public[=BOOL]` | `sync` | Create bookmarks public (default private). |
| `--use-post-date[=BOOL]` | `sync`, `cleanup` | Date bookmarks by the source post date. |
| `--stale-to-now[=BOOL]` | `cleanup` | Re-date too-old posts to now (default: keep existing). |

The boolean flags take an optional value: `--toread` means `true`, `--toread=false`
forces it off (so a config-set `true` can be overridden back to `false`). Bare
`--public` likewise means `--public=true`.

### Logging

Progress is logged to **stderr** at `info` level by default (the version on startup,
account and bookmark counts, per-source fetched/new counts, and a per-run summary), so
**stdout stays clean** for generated output like `--dry-run` listings. Raise the
verbosity with a repeatable `-v` (`-v` = debug, `-vv` = trace, `-vvv` also includes
dependency logs), or set `RUST_LOG` for full control (e.g. `RUST_LOG=pinboard_sync=debug`),
which overrides `-v`. Under the NixOS service these lines land in the journal
(`journalctl -u pinboard-sync`).

## What `cleanup` does

Where `sync` *adds* new bookmarks, `cleanup` *repairs the ones already on Pinboard* —
normalizing URLs, tags, and titles that drift over time or were saved in a messier
form. It only touches bookmarks it recognizes as belonging to that source, is
idempotent (safe to re-run — it re-writes a bookmark only when a field actually
changes, comparing creation dates by instant so an equivalently-formatted timestamp
isn't treated as a change), and supports `--dry-run` to preview every change first.
`cleanup --all` runs all three once over the shared bookmark set.

**It can combine two of your bookmarks into one.** Pinboard stores one record per URL,
so when cleanup rewrites a bookmark onto a URL you have saved separately — an HN story
whose article you also bookmarked, a GitHub repo starred under both its old and new
names — the two are **merged** rather than one overwriting the other: tags are unioned,
notes are concatenated, and the bookmark already at that URL keeps its title, date and
to-read state. **The absorbed bookmark is then deleted**, since its content now lives in
the merged record. `--dry-run` lists each merge with an `absorb <url>` line for every
bookmark that would be removed; it is worth reading before the first real run.

Some rewrites are **refused** and reported instead: cleanup will not write over a
bookmark it could not read this run, and it will not merge bookmarks that disagree about
being public or private, since that would either publish a private note or unshare
something you chose to share. A refusal leaves both records untouched, so the duplicate
stays until you resolve it yourself.

One case is not idempotent: if a merged note grows past what Pinboard's API accepts in a
request URL, it is stored truncated (logged when it happens), and the next run no longer
recognizes the truncated block — so that bookmark is rewritten every run and its note
grows. Rare, but if you see a note accumulating `… [truncated]` markers, shorten it.

- **Reddit** (`cleanup reddit`) — rewrites each Reddit bookmark's URL to your
  configured `reddit_domain` (default `old.reddit.com`), unwrapping `over18`
  interstitial redirects to the real post. It normalizes tags (ensures the base
  `reddit` tag and a correctly-cased `subreddit:<sub>` tag, dropping bare/legacy
  duplicates), and — using Reddit's `/api/info` — marks `over_18` posts `nsfw` and
  replaces generic placeholder titles with the real ones. The NSFW and title passes
  are the only ones that contact Reddit, so they need the `reddit_session` cookie;
  `--no-nsfw` / `--no-titles` skip them (and the cookie requirement).

- **GitHub** (`cleanup github`) — canonicalizes repo bookmark URLs to
  `https://github.com/<owner>/<repo>` (forcing https, lowercasing the host, dropping a
  `.git` suffix, trailing slash, and any query/fragment); deeper links like
  `/tree/...` or `/issues` are left alone. It then looks each repo up via the GitHub
  API, which follows **renames and transfers** — rewriting a moved repo's URL to its
  current location, refreshing the title to the current `owner/repo`, and refreshing
  the `lang:` tag. A repo that no longer exists (404) keeps just the URL
  canonicalization. Needs the GitHub token; existing notes and the bookmark's creation
  time are preserved.

- **HackerNews** (`cleanup hackernews`) — rewrites favorited *story* bookmarks whose
  URL is the HN item page (`news.ycombinator.com/item?id=…`) to the linked **article**,
  re-deriving the title, the `HN Link:` notes, and tags; favorited *comments* keep
  their HN permalink. Favorites are public, so no auth beyond the Pinboard token is
  needed. Optionally, `--link-discussions` (off by default) goes the other way: for
  article bookmarks carrying the marker tag (`find-hn` by default, override with
  `--link-tag`), it looks each up on HN by URL and adds the discussion link to the
  notes.

## What `backup` does

Where `sync` and `cleanup` *write to Pinboard*, `backup` only reads: it snapshots every
service to a directory, so nothing depends on Pinboard — or on the sources — still having
your data. It matters because the sync path is deliberately lossy. Each wire struct keeps
only the fields it needs, so a starred repo arrives with ~80 fields and `sync` uses five;
Reddit caps a saved listing near 1000 items; and Pinboard's own copy of a long note may
already be truncated to fit its API's URL budget.

```
<dir>/raw/pinboard.json                      # posts/all with meta=yes, every field
<dir>/raw/reddit-main.json                   # every saved.json page envelope
<dir>/raw/github-personal.json               # every /user/starred entry
<dir>/raw/hackernews-me.json                 # every Algolia response
<dir>/raw/hackernews-me-favorites.html       # the scraped favorites pages
<dir>/normalized/<same stems>.json           # the same items as domain bookmarks
<dir>/manifest.json                          # run timestamp, version, per-file counts
```

- **`raw/` keeps every field.** Bodies are captured as text *before* any parsing, during
  the same traversal that builds the normalized half — so the two halves always describe
  one instant, and nothing the typed read path drops is lost. It is not a byte-for-byte
  copy: merging a service's pages into one file means re-serializing, which sorts each
  object's keys. Same fields, same values, different bytes.
- **Pages are merged per service.** A response that is itself a JSON array flattens into
  one array of items (GitHub, Pinboard); anything else is kept whole as one element per
  page (Reddit's `Listing` envelope, Algolia's `{"hits": […]}`). HackerNews' favorites are
  scraped HTML, so they land as `.html` with the pages concatenated behind marker comments.
- **`normalized/` is one uniform shape** across every service — `url`, `title`, `note`,
  `tags`, `timestamp`, `public`, `read_later`, and each source's `dedup_key`.
- **Each run replaces the previous snapshot in place.** There are no timestamped run
  directories and nothing is pruned, so point a real backup tool (git, borg, Time Machine)
  at the directory if you want history. Every file is written atomically at mode 0600 —
  a snapshot holds all your private bookmarks — and a body that doesn't parse, or isn't a
  JSON array, is rejected rather than allowed to overwrite a good snapshot.
- **`manifest.json` is written last and says whether the run was complete.** Per-file
  atomicity isn't *run* atomicity, so the manifest carries `complete: false` and names the
  `failed` targets when any target failed. Each file also records **its own**
  `generated_at`, and a narrowed run merges into the existing manifest rather than
  replacing it — so after `backup pinboard`, the other targets' files are still described,
  with their older timestamps visible. Entries always describe what is on disk *now*, even
  for a target that failed after writing; a file that isn't a trustworthy snapshot carries
  its own `unusable` reason, which survives later runs of other targets; and entries are
  dropped when their file goes away. Each reports either `items` or `pages`, never both,
  because an envelope stream's element count is a page count (10 pages of Reddit saves is
  not 10 saves). One writer at a time per directory — concurrent runs race.
- **A truncated fetch is a failure, not a quiet overwrite.** Each source has a page cap as
  a runaway-pagination backstop; hitting it means the snapshot is only part of the account,
  so the target is reported failed and lands in the manifest's `failed` list rather than
  silently replacing a complete snapshot with a partial one.
- **The directory comes from `--out DIR` or `[backup].directory`**, with no built-in
  default. `backup <target>` narrows a run to one service (or one account); `--all` covers
  the Pinboard account plus every configured source account. Backing up a source never
  contacts Pinboard, so those targets need no Pinboard token.
- `--dry-run` prints the file plan without writing, but still performs the full fetch.

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

### Per-source defaults and dating by the source post date

A `[defaults.<source>]` table is a middle override tier between the `[pinboard]` /
`[hooks]` globals and a per-account value. `use_post_date`, `toread`, `public`,
`limit`, `on_auth_failure`, `post_date_max_age_days`, and `cleanup_stale_to_now`
resolve **CLI → account → `[defaults.<source>]` → global**:

```toml
[pinboard]
use_post_date = true        # global default
post_date_max_age_days = 30 # only backdate posts this recent (older use "now")

[defaults.reddit]
use_post_date = false       # ...but not for reddit accounts

[[reddit]]
name = "main"
use_post_date = true        # ...except this one
```

With `use_post_date`, a bookmark's creation date (Pinboard's `dt`) is set to the
**source post date** rather than the time of the sync. The exact logic:

- **Where the date comes from.** Reddit uses the post's `created_utc`, HackerNews the
  item's `created_at`, and GitHub the time you *starred* the repo (`starred_at`).
  `cleanup reddit` reads the date from `/api/info`, so it needs the `reddit_session`
  cookie when `use_post_date` is on; `cleanup github` reads the star list to map it.
- **The age cap.** `post_date_max_age_days` (default 30) bounds how far back a bookmark
  is dated. A post **within** the cap is dated to its source time. A post **older** than
  the cap is treated differently by the two commands:
  - `sync` falls back to **"now"** (it omits `dt`, letting Pinboard default it).
  - `cleanup` **keeps the bookmark's existing date** — unless `cleanup_stale_to_now =
    true`, which re-dates stale items to now.
- **When the date is unknown** (the source exposes none), the date is left untouched.
- **`cleanup` compares by instant.** A re-write only happens when a field a write would
  change actually differs. The date is compared as a moment in time, not as a string, so
  a stored timestamp and the source date that are the *same instant* formatted
  differently (e.g. a `+00:00` offset vs a trailing `Z`) are **not** counted as a change
  and don't trigger a needless re-write.

Dating is off by default; enable it per the resolution tiers above.

## Shell completions and example config

Two utility subcommands print to **stdout** so you can pipe or redirect them
wherever you like. (The Nix package runs both at build time and installs the results
for you — see below — so these are mainly for non-Nix installs.)

### `config example`

`pinboard-sync config example` prints a fully-commented config template: every key
with its built-in default and a short note on what it does. It's the canonical
reference for the config schema — start here rather than copying snippets. Pipe it to
a file and edit:

```sh
pinboard-sync config example > pinboard-sync.toml
$EDITOR pinboard-sync.toml
pinboard-sync --config pinboard-sync.toml sync --all --dry-run
```

The template is embedded in the binary, so it always matches that build's schema.

### `completions`

`pinboard-sync completions <shell>` prints a completion script for `<shell>` —
`bash`, `zsh`, and `fish` (plus `elvish` and `powershell`). Install it where your
shell looks for completions, for example:

```sh
# bash (current user)
pinboard-sync completions bash > ~/.local/share/bash-completion/completions/pinboard-sync

# zsh — anywhere on your $fpath, e.g.
pinboard-sync completions zsh > ~/.zfunc/_pinboard-sync

# fish
pinboard-sync completions fish > ~/.config/fish/completions/pinboard-sync.fish
```

Completions cover the subcommands, flags, and `<shell>`/source values; re-run after
upgrading so they track the installed version. Reload your shell (or `compinit` for
zsh) to pick them up.

`pinboard-sync man` prints a roff man page to stdout (`pinboard-sync man | man -l -`
to read it).

### Installed by the Nix package

`nix build` already generates and installs these, so you don't run the commands by
hand:

- bash/zsh/fish completions under the usual
  `share/{bash-completion,zsh/site-functions,fish/vendor_completions.d}` paths,
- the man page at `share/man/man1/pinboard-sync.1`, and
- the template at `share/pinboard-sync/config.example.toml`.

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
    backup.enable = true;                    # also snapshot every service to disk…
    # backup.schedule = "daily";             # …on its own timer; default daily
    # backup.directory = "/var/lib/pinboard-sync/backup";  # default location
  };
}
```

The `backup` timer writes into the service's `StateDirectory`
(`/var/lib/pinboard-sync`); `backup.path` must stay under it, since the hardened
`DynamicUser` can only write there. Each run replaces the file atomically, and the
snapshot is readable by root (retrieve it with a root-run job — the state dir lives
under the 0700 `/var/lib/private`).

Instead of rendering every credential into one `environmentFile`, you can point the
per-credential `*File` options at individual secret paths (e.g. one sops-nix secret
per credential). Each is loaded into the unit's credentials directory via systemd
`LoadCredential` and read through its `<VAR>_FILE` env var, so the value never lands in
the unit environment or the store, and the `DynamicUser` can read it even though the
source path is root-only:

```nix
services.pinboard-sync = {
  enable = true;
  pinboardTokenFile = config.sops.secrets.pinboard-token.path;
  redditUsernameFile = config.sops.secrets.reddit-username.path;
  # The Reddit cookie file must contain the full `reddit_session=<value>` form.
  redditCookieFile = config.sops.secrets.reddit-cookie.path;
};
```

The options are `pinboardTokenFile`, `redditUsernameFile`, `redditCookieFile`,
`githubTokenFile`, and `hnUsernameFile`. Set at least one of them or `environmentFile`;
the two can be combined, and credentials may also come from `*_file` paths inside
account tables.

## Roadmap

Planned but not yet implemented:

- **Tag bundles.** Setting and updating Pinboard tag bundles. This depends on the
  Pinboard **API v2** — the v1 API has no bundle support, and v2 (which does) is a
  documented 2021 draft that hasn't been deployed, so this is on hold until v2
  ships.
- **Incremental / early-stop fetch.** Stop paginating the page-based sources (Reddit,
  GitHub) once already-synced items are reached, to cut API calls on large accounts.
  Deferred: it needs an existing-keys parameter on the source `fetch`, and HackerNews
  can't fully participate (article dedup keys aren't known until after the Algolia
  lookup). Runs are already idempotent, so this is purely an efficiency win.
- **Prune / reconcile mode.** Optionally remove Pinboard bookmarks a source no longer
  has (opt-in, dry-run-first) — sync is additive-only today.
- **More sources.** Generic RSS/Atom, Mastodon bookmarks, Lobsters; and extending the
  existing ones (GitHub gists).
- **Machine-readable output and a post-run hook.** A `--format json` run summary, and
  a general post-run hook (not just the auth-failure one).

## Development

Run everything through the Nix dev shell so versions match the flake:

```sh
nix develop --command cargo test
nix develop --command cargo clippy --all-targets -- -D warnings
nix develop --command cargo fmt --check
nix flake check        # builds the package (runs the hermetic tests) + evaluates the module
```

See [CLAUDE.md](CLAUDE.md) for the architecture.
